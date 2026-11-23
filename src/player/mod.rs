pub mod command;
mod local;
pub mod state;

use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use command::{PlayerCommand as Cmd, Request};
use rodio::Source;
use state::{PlaybackEvent as Event, PlaybackState, Queue, SharedState, Status};

use crate::{
    config::Config,
    library::{Track, metadata, scan},
    slint_types::LoopMode,
};

struct PlayerCore<F> {
    cfg: Config,
    state: PlaybackState,
    shared: SharedState,
    tracks: Vec<Arc<Track>>,
    queue: Queue,
    query: String,
    pending_seek: Option<Duration>,
    mixer: rodio::mixer::Mixer,
    player: rodio::Player,
    publish: F,
    #[cfg(target_os = "linux")]
    artwork: Option<tempfile::NamedTempFile>,
}

pub fn start(
    cfg: Config,
    shared: SharedState,
    rx: mpsc::Receiver<Request>,
    mut publish: impl FnMut(Event) + Send + 'static,
) -> thread::JoinHandle<Config> {
    thread::spawn(move || {
        let mut device = match rodio::DeviceSinkBuilder::open_default_sink() {
            Ok(d) => d,
            Err(e) => {
                publish(Event::Error(format!("Audio output: {e}")));
                return cfg;
            }
        };
        device.log_on_drop(false);
        let mixer = device.mixer().clone();
        let player = rodio::Player::connect_new(&mixer);
        run_worker(cfg, shared, rx, mixer, player, publish)
    })
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn spawn_test_worker(
    cfg: Config,
    shared: SharedState,
    rx: mpsc::Receiver<Request>,
    publish: impl FnMut(Event) + Send + 'static,
) -> thread::JoinHandle<Config> {
    use std::sync::atomic::{AtomicBool, Ordering};

    thread::spawn(move || {
        let (mixer, mut output) =
            rodio::mixer::mixer(1.try_into().unwrap(), 8000.try_into().unwrap());
        let player = rodio::Player::connect_new(&mixer);
        let running = Arc::new(AtomicBool::new(true));
        let active = running.clone();
        let audio = thread::spawn(move || {
            while active.load(Ordering::Relaxed) {
                for _ in 0..80 {
                    output.next();
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let cfg = run_worker(cfg, shared, rx, mixer, player, publish);
        running.store(false, Ordering::Relaxed);
        let _ = audio.join();
        cfg
    })
}

fn run_worker(
    cfg: Config,
    shared: SharedState,
    rx: mpsc::Receiver<Request>,
    mixer: rodio::mixer::Mixer,
    player: rodio::Player,
    publish: impl FnMut(Event) + Send + 'static,
) -> Config {
    player.set_volume(cfg.volume);
    let state = PlaybackState {
        volume: cfg.volume,
        loop_mode: cfg.loop_mode,
        shuffle: cfg.shuffle,
        ..Default::default()
    };
    let mut core = PlayerCore {
        cfg,
        state,
        shared,
        tracks: Vec::new(),
        queue: Queue::default(),
        query: String::new(),
        pending_seek: None,
        mixer,
        player,
        publish,
        #[cfg(target_os = "linux")]
        artwork: None,
    };
    if let Err(e) = core.refresh(core.cfg.song_dir.clone(), true) {
        core.error(e);
    }
    core.broadcast();
    loop {
        let request = if core.state.status == Status::Playing {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(r) => r,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    core.tick();
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(r) => r,
                Err(_) => break,
            }
        };
        if matches!(request.command, Cmd::Shutdown) {
            break;
        }
        let result = core.handle(request.command);
        if let Err(e) = &result {
            core.error(e.clone());
        }
        #[cfg(target_os = "linux")]
        if let Some(reply) = request.reply {
            let _ = reply.send(result);
        }
        if core.state.status == Status::Playing && core.player.empty() {
            core.tick();
        }
    }
    core.cfg.current_song_path = core.state.track.as_ref().map(|t| t.path().to_owned());
    core.cfg.progress = core.player.get_pos().min(core.state.duration).as_secs_f32();
    if core.state.status == Status::Stopped {
        core.cfg.progress = 0.;
    }
    core.cfg.loop_mode = core.state.loop_mode;
    core.cfg.shuffle = core.state.shuffle;
    core.cfg.volume = core.state.volume;
    core.cfg
}

impl<F: FnMut(Event)> PlayerCore<F> {
    fn error(&mut self, msg: String) {
        log::warn!("{msg}");
        (self.publish)(Event::Error(msg));
    }
    fn tick(&mut self) {
        if self.player.empty() {
            let result = if self.state.loop_mode == LoopMode::Track {
                match self.state.track.clone() {
                    Some(t) => self.load(t, Status::Playing),
                    None => Ok(()),
                }
            } else {
                self.navigate(true, true)
            };
            if let Err(e) = result {
                self.stop();
                self.error(e);
            }
            self.broadcast();
        } else {
            self.sample_position();
            *self.shared.write().unwrap() = self.state.clone();
        }
    }
    fn sample_position(&mut self) {
        self.state.position = if self.state.status == Status::Stopped {
            Duration::ZERO
        } else {
            self.player.get_pos().min(self.state.duration)
        };
        self.state.sampled_at = Instant::now();
    }
    fn broadcast(&mut self) {
        self.sample_position();
        let current = self.state.track.as_ref().map(|t| &t.id);
        self.state.can_next = self.queue.step(current, true, self.state.loop_mode).is_some();
        self.state.can_previous = self.queue.step(current, false, self.state.loop_mode).is_some();
        self.state.can_play = !self.tracks.is_empty();
        *self.shared.write().unwrap() = self.state.clone();
        (self.publish)(Event::State(self.state.clone(), self.pending_seek.take()));
    }
    fn rebuild_order(&mut self) {
        self.queue.rebuild(
            &self.tracks,
            self.state.shuffle,
            self.state.track.as_ref().map(|t| &t.id),
        );
    }
    fn library_event(&mut self) {
        (self.publish)(Event::Library(
            self.cfg.song_dir.clone(),
            self.cfg.sort_key,
            self.cfg.sort_ascending,
            self.tracks.clone(),
        ));
        self.results();
    }
    fn results(&mut self) {
        (self.publish)(Event::Results(scan::search(&self.query, &self.tracks)));
    }
    fn refresh(&mut self, dir: PathBuf, startup: bool) -> Result<(), String> {
        let mut tracks = match scan::scan(&dir) {
            Ok(v) => v,
            Err(e) if startup && e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        };
        scan::sort(&mut tracks, self.cfg.sort_key, self.cfg.sort_ascending);
        self.cfg.song_dir = dir;
        self.tracks = tracks;
        self.rebuild_order();
        self.library_event();
        if let Some(current) = &self.state.track
            && let Some(updated) = self.tracks.iter().find(|t| t.id == current.id).cloned()
        {
            self.state.track = Some(updated.clone());
            self.details(&updated);
            return Ok(());
        }
        self.stop();
        self.state.track = None;
        self.state.duration = Duration::ZERO;
        #[cfg(target_os = "linux")]
        {
            self.artwork = None;
            self.state.art_url = None;
        }
        (self.publish)(Event::Details(Default::default()));
        let saved = self
            .cfg
            .current_song_path
            .as_ref()
            .filter(|_| startup)
            .and_then(|p| self.tracks.iter().find(|t| t.path() == p).cloned());
        if let Some(track) = saved.or_else(|| self.tracks.first().cloned()) {
            let resume = startup && self.cfg.current_song_path.as_deref() == Some(track.path());
            self.load(track, Status::Paused)?;
            if resume && self.cfg.progress > 0. {
                let pos = Duration::try_from_secs_f32(self.cfg.progress)
                    .unwrap_or_default()
                    .min(self.state.duration);
                self.seek(pos)?;
            }
        }
        Ok(())
    }
    fn details(&mut self, t: &Track) {
        let details = metadata::read_details(t.path());
        #[cfg(target_os = "linux")]
        {
            self.artwork =
                details.cover.as_ref().and_then(
                    |img| match crate::integrations::mpris::cache_cover(img) {
                        Ok(f) => Some(f),
                        Err(e) => {
                            log::warn!("Cover cache: {e}");
                            None
                        }
                    },
                );
            self.state.art_url = self
                .artwork
                .as_ref()
                .and_then(|f| url::Url::from_file_path(f.path()).ok())
                .map(Into::into);
        }
        (self.publish)(Event::Details(details));
    }
    fn load(&mut self, t: Arc<Track>, status: Status) -> Result<(), String> {
        let source = local::decode(t.path())?;
        let duration = source.total_duration().unwrap_or(t.duration);
        // A new sink starts at position zero, including while paused or stopped.
        let next = rodio::Player::connect_new(&self.mixer);
        next.pause();
        next.set_volume(self.state.volume);
        next.append(source);
        self.player = next;
        self.state.duration = duration;
        if self.state.track.as_ref().is_none_or(|old| old.id != t.id) {
            self.details(&t);
        }
        if self.state.track.as_ref().is_some_and(|old| old.id == t.id)
            && self.state.status != Status::Stopped
        {
            self.pending_seek = Some(Duration::ZERO);
        }
        self.state.track = Some(t);
        self.state.status = status;
        if status == Status::Playing {
            self.player.play();
        }
        Ok(())
    }
    fn stop(&mut self) {
        self.player.clear();
        self.state.status = Status::Stopped;
    }
    fn play(&mut self) -> Result<(), String> {
        if self.state.status == Status::Playing {
            return Ok(());
        }
        if self.player.empty() {
            if let Some(t) = self.state.track.clone().or_else(|| self.tracks.first().cloned()) {
                self.load(t, Status::Playing)?;
            }
        } else {
            self.player.play();
            self.state.status = Status::Playing;
        }
        Ok(())
    }
    fn navigate(&mut self, next: bool, finished: bool) -> Result<(), String> {
        let old = self.state.track.clone();
        if let Some(t) = self.queue.step(old.as_ref().map(|t| &t.id), next, self.state.loop_mode) {
            self.load(t.clone(), self.state.status)?;
            self.queue.record(old, &t.id, Some(next));
        } else if finished {
            self.stop();
        }
        Ok(())
    }
    fn seek(&mut self, pos: Duration) -> Result<(), String> {
        self.player.try_seek(pos).map_err(|e| format!("Seeking: {e}"))?;
        self.pending_seek = Some(pos);
        Ok(())
    }
    fn handle(&mut self, cmd: Cmd) -> Result<(), String> {
        match cmd {
            Cmd::TogglePlayPause => {
                return self.handle(if self.state.status == Status::Playing {
                    Cmd::Pause
                } else {
                    Cmd::Play
                });
            }
            Cmd::Play => self.play()?,
            Cmd::Pause => {
                if self.state.status == Status::Playing {
                    self.player.pause();
                    self.state.status = Status::Paused;
                }
            }
            Cmd::Stop => self.stop(),
            Cmd::PlayTrack(id) => {
                let t = self
                    .tracks
                    .iter()
                    .find(|t| t.id == id)
                    .cloned()
                    .ok_or("Track is no longer in the library")?;
                let old = self.state.track.clone();
                self.load(t, Status::Playing)?;
                self.queue.record(old, &id, None);
            }
            Cmd::Next => self.navigate(true, false)?,
            Cmd::Previous => self.navigate(false, false)?,
            Cmd::SeekTo(id, pos) => {
                if self.state.accepts_position(id.as_ref(), pos) {
                    self.seek(pos)?;
                }
            }
            Cmd::SeekRelative(offset) => {
                if self.state.can_seek() {
                    let delta = Duration::from_micros(offset.unsigned_abs());
                    let now = self.player.get_pos();
                    let pos = if offset < 0 {
                        now.saturating_sub(delta)
                    } else {
                        now.saturating_add(delta)
                    };
                    if pos > self.state.duration {
                        self.navigate(true, false)?;
                    } else {
                        self.seek(pos)?;
                    }
                }
            }
            Cmd::SetVolume(v) => {
                if v.is_finite() {
                    self.state.volume = v.clamp(0., 1.);
                    self.player.set_volume(self.state.volume);
                }
            }
            Cmd::SetLoopMode(m) => self.state.loop_mode = m,
            Cmd::SetShuffle(s) => {
                if self.state.shuffle != s {
                    self.state.shuffle = s;
                    self.rebuild_order();
                }
            }
            Cmd::Refresh(dir) => {
                let result = self.refresh(dir, false);
                self.broadcast();
                return result;
            }
            Cmd::Sort(key, asc) => {
                self.cfg.sort_key = key;
                self.cfg.sort_ascending = asc;
                scan::sort(&mut self.tracks, key, asc);
                if !self.state.shuffle {
                    self.rebuild_order();
                }
                self.library_event();
            }
            Cmd::Search(q) => {
                self.query = q;
                self.results();
                return Ok(());
            }
            #[cfg(target_os = "linux")]
            Cmd::Raise => {
                (self.publish)(Event::Raise);
                return Ok(());
            }
            #[cfg(target_os = "linux")]
            Cmd::Quit => {
                (self.publish)(Event::Quit);
                return Ok(());
            }
            Cmd::Shutdown => unreachable!(),
        }
        self.broadcast();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use super::*;
    use crate::library::metadata::read_meta_info;

    struct TestHarness {
        core: PlayerCore<Box<dyn FnMut(Event)>>,
        events: Arc<Mutex<Vec<Event>>>,
        tracks: Vec<Arc<Track>>,
        temp_dir: Option<PathBuf>,
        running: Arc<AtomicBool>,
        audio: Option<thread::JoinHandle<()>>,
    }

    impl Drop for TestHarness {
        fn drop(&mut self) {
            self.running.store(false, Ordering::Relaxed);
            if let Some(handle) = self.audio.take() {
                let _ = handle.join();
            }
            if let Some(dir) = &self.temp_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    impl TestHarness {
        fn new(track_count: usize) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let fixture =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tagged.flac");
            let (temp_dir, tracks) = if track_count <= 1 {
                (None, vec![Arc::new(read_meta_info(&fixture).unwrap())])
            } else {
                let dir = std::env::temp_dir()
                    .join(format!("sealmusic_player_test_{}_{seq}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                let tracks = (0..track_count)
                    .map(|idx| {
                        let copy = dir.join(format!("{idx:02}_fixture.flac"));
                        std::fs::copy(&fixture, &copy).unwrap();
                        Arc::new(read_meta_info(&copy).unwrap())
                    })
                    .collect();
                (Some(dir), tracks)
            };

            let (mixer, mut output) =
                rodio::mixer::mixer(1.try_into().unwrap(), 8000.try_into().unwrap());
            let player = rodio::Player::connect_new(&mixer);
            let running = Arc::new(AtomicBool::new(true));
            let active = running.clone();
            let audio = thread::spawn(move || {
                while active.load(Ordering::Relaxed) {
                    for _ in 0..80 {
                        output.next();
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            });

            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = events.clone();
            let publish: Box<dyn FnMut(Event)> = Box::new(move |ev| sink.lock().unwrap().push(ev));
            let mut core = PlayerCore {
                cfg: Config::default(),
                state: PlaybackState::default(),
                shared: SharedState::default(),
                tracks: tracks.clone(),
                queue: Queue::default(),
                query: String::new(),
                pending_seek: None,
                mixer,
                player,
                publish,
                #[cfg(target_os = "linux")]
                artwork: None,
            };
            core.rebuild_order();

            Self {
                core,
                events,
                tracks,
                temp_dir,
                running,
                audio: Some(audio),
            }
        }
    }

    #[test]
    fn playback_transitions_are_idempotent_and_failure_preserves_track() {
        let mut h = TestHarness::new(1);
        let track = h.tracks[0].clone();
        h.core.handle(Cmd::Play).unwrap();
        h.core.handle(Cmd::Play).unwrap();
        assert_eq!(h.core.state.status, Status::Playing);
        h.core.handle(Cmd::Pause).unwrap();
        h.core.handle(Cmd::Pause).unwrap();
        h.core.handle(Cmd::SeekTo(Some(track.id.clone()), Duration::from_millis(500))).unwrap();
        assert_eq!(h.core.state.position, Duration::from_millis(500));
        h.core
            .handle(Cmd::SeekTo(Some(crate::library::TrackId("stale".into())), Duration::ZERO))
            .unwrap();
        assert_eq!(h.core.state.position, Duration::from_millis(500));
        assert!(h.core.handle(Cmd::PlayTrack(crate::library::TrackId("missing".into()))).is_err());
        assert_eq!(h.core.state.track, Some(track));
        h.core.handle(Cmd::TogglePlayPause).unwrap();
        assert_eq!(h.core.state.status, Status::Playing);
        h.core.handle(Cmd::Stop).unwrap();
        h.core.handle(Cmd::Pause).unwrap();
        assert_eq!((h.core.state.status, h.core.state.position), (Status::Stopped, Duration::ZERO));
        h.core.handle(Cmd::Play).unwrap();
        assert_eq!(h.core.state.status, Status::Playing);
    }

    #[test]
    fn loop_mode_end_of_track_transitions_cover_none_track_and_playlist() {
        let mut h = TestHarness::new(2);
        let (first, second) = (h.tracks[0].clone(), h.tracks[1].clone());

        h.core.handle(Cmd::SetLoopMode(LoopMode::None)).unwrap();
        h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
        h.core.player.clear();
        h.core.tick();
        assert_eq!(h.core.state.status, Status::Playing);
        assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&second.id));

        h.core.player.clear();
        h.core.tick();
        assert_eq!(h.core.state.status, Status::Stopped);

        h.core.handle(Cmd::SetLoopMode(LoopMode::Track)).unwrap();
        h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
        h.events.lock().unwrap().clear();
        h.core.player.clear();
        h.core.tick();
        assert_eq!(h.core.state.status, Status::Playing);
        assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&first.id));
        let has_zero_seek = h
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|ev| matches!(ev, Event::State(_, Some(pos)) if *pos == Duration::ZERO));
        assert!(has_zero_seek);

        h.core.handle(Cmd::SetLoopMode(LoopMode::Playlist)).unwrap();
        h.core.handle(Cmd::PlayTrack(second.id.clone())).unwrap();
        h.core.player.clear();
        h.core.tick();
        assert_eq!(h.core.state.status, Status::Playing);
        assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&first.id));
    }

    #[test]
    fn seek_relative_clamps_backward_seeks_forward_and_advances_past_end() {
        let mut h = TestHarness::new(2);
        let (first, second) = (h.tracks[0].clone(), h.tracks[1].clone());

        h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
        h.core.handle(Cmd::Pause).unwrap();
        h.core.handle(Cmd::SeekTo(Some(first.id.clone()), Duration::from_millis(500))).unwrap();
        h.core.handle(Cmd::SeekRelative(-5_000_000)).unwrap();
        assert_eq!(h.core.state.position, Duration::ZERO);

        h.core.handle(Cmd::SeekRelative(500_000)).unwrap();
        assert_eq!(h.core.state.position, Duration::from_millis(500));

        h.core.handle(Cmd::SeekRelative(10_000_000)).unwrap();
        assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&second.id));
    }

    #[test]
    fn set_volume_clamps_range_and_ignores_non_finite_values() {
        let mut h = TestHarness::new(1);
        h.core.handle(Cmd::SetVolume(1.5)).unwrap();
        assert_eq!(h.core.state.volume, 1.0);
        h.core.handle(Cmd::SetVolume(-0.2)).unwrap();
        assert_eq!(h.core.state.volume, 0.0);
        h.core.handle(Cmd::SetVolume(0.45)).unwrap();
        assert_eq!(h.core.state.volume, 0.45);
        h.core.handle(Cmd::SetVolume(f32::NAN)).unwrap();
        assert_eq!(h.core.state.volume, 0.45);
        h.core.handle(Cmd::SetVolume(f32::INFINITY)).unwrap();
        assert_eq!(h.core.state.volume, 0.45);
    }

    #[test]
    fn set_shuffle_updates_state_and_supports_navigation() {
        let mut h = TestHarness::new(2);
        let first = h.tracks[0].clone();
        h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
        h.core.handle(Cmd::SetShuffle(true)).unwrap();
        assert!(h.core.state.shuffle);
        h.core.handle(Cmd::Next).unwrap();
        assert_ne!(h.core.state.track.as_ref().map(|t| &t.id), Some(&first.id));
        h.core.handle(Cmd::Previous).unwrap();
        assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&first.id));
        h.core.handle(Cmd::SetShuffle(false)).unwrap();
        assert!(!h.core.state.shuffle);
    }
}
