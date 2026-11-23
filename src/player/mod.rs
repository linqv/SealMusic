pub mod command;
mod local;
#[cfg(target_os = "linux")]
mod online;
pub mod state;

#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use command::{Inbox, PlayerCommand as Cmd};
use rodio::Source;
use state::{PlaybackEvent as Event, PlaybackState, Queue, SharedState, Status};

use crate::{
    config::Config,
    library::{Track, metadata, scan},
    slint_types::LoopMode,
};

const POSITION_SYNC_TICKS: u8 = 5;

struct PlayerCore<F> {
    cfg: Config,
    state: PlaybackState,
    shared: SharedState,
    tracks: Vec<Arc<Track>>,
    queue: Queue,
    #[cfg(target_os = "linux")]
    online: online::Online,
    query: String,
    pending_seek: Option<Duration>,
    tick_count: u8,
    mixer: rodio::mixer::Mixer,
    player: rodio::Player,
    publish: F,
    #[cfg(target_os = "linux")]
    mpris_available: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    artwork: Option<tempfile::NamedTempFile>,
}

pub fn start(
    cfg: Config,
    shared: SharedState,
    inbox: Inbox,
    #[cfg(target_os = "linux")] mpris_available: Arc<AtomicBool>,
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
        run_worker(
            cfg,
            shared,
            inbox,
            mixer,
            player,
            #[cfg(target_os = "linux")]
            mpris_available,
            publish,
        )
    })
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn spawn_test_worker(
    cfg: Config,
    shared: SharedState,
    inbox: Inbox,
    publish: impl FnMut(Event) + Send + 'static,
) -> thread::JoinHandle<Config> {
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
        let cfg =
            run_worker(cfg, shared, inbox, mixer, player, Arc::new(AtomicBool::new(true)), publish);
        running.store(false, Ordering::Relaxed);
        let _ = audio.join();
        cfg
    })
}

fn run_worker(
    cfg: Config,
    shared: SharedState,
    inbox: Inbox,
    mixer: rodio::mixer::Mixer,
    player: rodio::Player,
    #[cfg(target_os = "linux")] mpris_available: Arc<AtomicBool>,
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
        #[cfg(target_os = "linux")]
        online: online::Online::new(inbox.sender),
        query: String::new(),
        pending_seek: None,
        tick_count: 0,
        mixer,
        player,
        publish,
        #[cfg(target_os = "linux")]
        mpris_available,
        #[cfg(target_os = "linux")]
        artwork: None,
    };
    if let Err(e) = core.refresh(core.cfg.song_dir.clone(), true) {
        core.error(e);
    }
    #[cfg(target_os = "linux")]
    {
        core.script_event();
        if core.cfg.current_online.is_some() && !core.restore_online() {
            core.cfg.current_online = None;
            if let Err(e) = core.reset_and_restore_local(true) {
                core.error(e);
            }
        }
    }
    core.broadcast();
    let rx = inbox.rx;
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
    core.cfg.current_song_path =
        core.state.track.as_ref().and_then(|t| t.local_path().map(ToOwned::to_owned));
    #[cfg(target_os = "linux")]
    {
        core.cancel_load();
        core.cfg.current_online = core.state.track.as_ref().and_then(|t| match &t.origin {
            crate::library::TrackOrigin::Online(r) => Some(r.clone()),
            _ => None,
        });
    }
    core.sample_position();
    core.cfg.progress = core.state.position.as_secs_f32();
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
            #[cfg(target_os = "linux")]
            if self.online.loading() {
                if !self.state.sink_empty {
                    self.broadcast();
                }
                return;
            }
            let result = if self.state.loop_mode == LoopMode::Track {
                match self.state.track.clone() {
                    Some(t) => self.load(t, Status::Playing, None, 0, false, true, Duration::ZERO),
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
            self.tick_count = (self.tick_count + 1) % POSITION_SYNC_TICKS;
            if self.tick_count == 0 {
                self.sample_position();
                let mut shared = self.shared.write().unwrap();
                shared.position = self.state.position;
                shared.sampled_at = self.state.sampled_at;
            }
        }
    }
    fn sample_position(&mut self) {
        if self.state.deferred {
            return;
        }
        self.state.position = if self.state.status == Status::Stopped {
            Duration::ZERO
        } else if self.player.empty() {
            self.state.duration
        } else {
            self.player.get_pos().min(self.state.duration)
        };
        self.state.sampled_at = Instant::now();
    }
    fn broadcast(&mut self) {
        self.tick_count = 0;
        self.state.sink_empty = self.player.empty();
        self.sample_position();
        let current = self.state.track.as_ref().map(|t| &t.id);
        self.state.can_next = self.queue.step(current, true, self.state.loop_mode).is_some();
        self.state.can_previous = self.queue.step(current, false, self.state.loop_mode).is_some();
        self.state.can_play =
            !self.player.empty() || self.queue.step(None, true, LoopMode::None).is_some();
        let snapshot = Arc::new(self.state.clone());
        *self.shared.write().unwrap() = (*snapshot).clone();
        (self.publish)(Event::State(snapshot, self.pending_seek.take()));
    }
    fn rebuild_order(&mut self) {
        #[cfg(target_os = "linux")]
        let tracks = {
            let mut tracks = self.tracks.clone();
            tracks.extend(
                self.online
                    .tracks
                    .iter()
                    .filter(|t| {
                        let crate::library::TrackOrigin::Online(r) = &t.origin else {
                            return false;
                        };
                        r.qualities(self.cfg.lx_script.as_ref()).contains(&r.quality)
                    })
                    .cloned(),
            );
            tracks
        };
        #[cfg(not(target_os = "linux"))]
        let tracks = &self.tracks;
        self.queue.rebuild(&tracks, self.state.shuffle, self.state.track.as_ref().map(|t| &t.id));
    }
    fn library_event(&mut self) {
        (self.publish)(Event::Library(
            self.cfg.song_dir.clone(),
            self.cfg.sort_key,
            self.cfg.sort_ascending,
            self.tracks.clone(),
        ));
        if !self.query.is_empty() {
            self.results();
        }
    }
    fn results(&mut self) {
        #[cfg(target_os = "linux")]
        if crate::online::is_link(&self.query) {
            self.preview_link();
            return;
        }
        (self.publish)(Event::Results(scan::search(&self.query, &self.tracks)));
    }
    fn clear_current_track(&mut self) {
        self.state.deferred = false;
        self.player.clear();
        self.state.status = Status::Stopped;
        self.state.track = None;
        self.state.duration = Duration::ZERO;
        #[cfg(target_os = "linux")]
        {
            self.online.cancel_details();
            self.artwork = None;
            self.state.art_url = None;
            self.state.synced_lyrics = None;
        }
        (self.publish)(Event::Details(Default::default()));
    }
    fn reset_and_restore_local(&mut self, startup: bool) -> Result<(), String> {
        self.stop();
        self.clear_current_track();
        let saved = self.cfg.current_song_path.as_ref().filter(|_| startup).and_then(|p| {
            self.tracks.iter().find(|t| t.local_path() == Some(p.as_path())).cloned()
        });
        if let Some(track) = saved.or_else(|| self.tracks.first().cloned()) {
            let resume = startup && self.cfg.current_song_path.as_deref() == track.local_path();
            self.load(track, Status::Paused, None, 0, false, false, Duration::ZERO)?;
            if resume && self.cfg.progress > 0. {
                let pos = Duration::try_from_secs_f32(self.cfg.progress)
                    .unwrap_or_default()
                    .min(self.state.duration);
                self.seek(pos)?;
            }
        }
        Ok(())
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
        #[cfg(target_os = "linux")]
        if (startup && self.cfg.current_online.is_some())
            || self.state.track.as_ref().is_some_and(|t| t.local_path().is_none())
        {
            return Ok(());
        }
        if let Some(current) = &self.state.track
            && let Some(updated) = self.queue.get(&current.id)
        {
            self.state.track = Some(updated.clone());
            self.details(&updated);
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if self.online.loading() {
            if self.state.track.is_some() {
                self.clear_current_track();
            }
            return Ok(());
        }
        self.reset_and_restore_local(startup)
    }
    fn details(&mut self, t: &Arc<Track>) {
        let details = t.local_path().map(metadata::read_details).unwrap_or_default();
        #[cfg(target_os = "linux")]
        self.enhance_details(t, &details);
        self.project_details(&t.id, details);
    }

    fn project_details(&mut self, _id: &crate::library::TrackId, details: metadata::TrackDetails) {
        #[cfg(target_os = "linux")]
        if self.mpris_available.load(Ordering::Acquire) {
            self.artwork = if let Some(raw) = &details.raw_cover {
                match crate::integrations::mpris::cache_cover_raw(&raw.bytes, raw.extension) {
                    Ok(f) => Some(f),
                    Err(e) => {
                        log::warn!("Cover cache: {e}");
                        None
                    }
                }
            } else {
                details.cover.as_ref().and_then(
                    |img| match crate::integrations::mpris::cache_cover(img) {
                        Ok(f) => Some(f),
                        Err(e) => {
                            log::warn!("Cover cache: {e}");
                            None
                        }
                    },
                )
            };
            self.state.art_url = self
                .artwork
                .as_ref()
                .and_then(|f| url::Url::from_file_path(f.path()).ok())
                .map(|u| Arc::<str>::from(u.as_str()));
            self.state.synced_lyrics =
                crate::integrations::mpris::encode_synced_lyrics(_id, &details.lyrics);
        } else {
            self.artwork = None;
            self.state.art_url = None;
            self.state.synced_lyrics = None;
        }
        (self.publish)(Event::Details(details));
    }
    #[allow(clippy::too_many_arguments)]
    fn load(
        &mut self,
        t: Arc<Track>,
        status: Status,
        direction: Option<bool>,
        history_skip: usize,
        from_history: bool,
        finished: bool,
        position: Duration,
    ) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            self.cancel_load();
            if t.local_path().is_none() {
                if let Err(err) = self.begin_online(
                    t,
                    status,
                    direction,
                    history_skip,
                    from_history,
                    finished,
                    position,
                ) {
                    if finished {
                        self.stop();
                    } else {
                        self.stop_if_drained();
                    }
                    self.online_failure(err);
                }
                return Ok(());
            }
        }
        let source = match local::decode(t.local_path().ok_or("Missing local path")?) {
            Ok(source) => source,
            Err(e) => {
                if finished {
                    self.stop();
                } else {
                    self.stop_if_drained();
                }
                return Err(e);
            }
        };
        let old = self.state.track.clone();
        self.commit(t.clone(), status, source, position)?;
        self.queue.record_navigation(old, &t.id, direction, history_skip, from_history);
        Ok(())
    }

    fn commit(
        &mut self,
        t: Arc<Track>,
        status: Status,
        source: rodio::Decoder<std::io::BufReader<std::fs::File>>,
        position: Duration,
    ) -> Result<(), String> {
        let duration = source.total_duration().unwrap_or(t.duration);
        let changed = self.state.track.as_ref().is_none_or(|old| old.id != t.id);
        // A new sink starts at position zero, including while paused or stopped.
        let next = rodio::Player::connect_new(&self.mixer);
        next.pause();
        next.set_volume(self.state.volume);
        next.append(source);
        if !position.is_zero() {
            let position = position.min(duration);
            if let Err(e) = next.try_seek(position) {
                log::warn!("Restoring position for '{}': {e}", t.title);
                if !changed && self.state.status != Status::Stopped {
                    self.pending_seek = Some(Duration::ZERO);
                }
            } else {
                self.pending_seek = Some(position);
            }
        } else if !changed && self.state.status != Status::Stopped {
            self.pending_seek = Some(Duration::ZERO);
        }
        self.player = next;
        self.state.deferred = false;
        self.state.duration = duration;
        self.state.track = Some(t.clone());
        self.state.status = status;
        if status == Status::Playing {
            self.player.play();
        }
        if changed {
            self.details(&t);
        }
        Ok(())
    }
    fn stop(&mut self) {
        #[cfg(target_os = "linux")]
        self.cancel_load();
        self.state.deferred = false;
        self.player.clear();
        self.state.status = Status::Stopped;
    }
    pub(super) fn stop_if_drained(&mut self) {
        #[cfg(target_os = "linux")]
        if self.online.loading() {
            return;
        }
        if !self.state.deferred && self.state.status != Status::Stopped && self.player.empty() {
            self.stop();
        }
    }
    fn play(&mut self) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if let Some((label, resume_current)) = self.online.resume_pending() {
            (self.publish)(Event::Loading(label, false));
            if resume_current && !self.player.empty() {
                self.player.play();
                self.state.status = Status::Playing;
            }
            return Ok(());
        }
        if self.state.status == Status::Playing {
            return Ok(());
        }
        if self.player.empty() {
            if let Some(t) = self.state.track.as_ref().filter(|_| self.state.deferred).cloned() {
                self.load(t, Status::Playing, None, 0, false, false, self.state.position)?;
            } else if let Some(t) = self
                .state
                .track
                .as_ref()
                .and_then(|t| self.queue.get(&t.id))
                .or_else(|| self.queue.step(None, true, LoopMode::None))
            {
                self.load(t, Status::Playing, None, 0, false, false, Duration::ZERO)?;
            }
        } else {
            self.player.play();
            self.state.status = Status::Playing;
        }
        Ok(())
    }
    fn navigate(&mut self, next: bool, finished: bool) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        let (pending_target, skip) = self.online.nav_context(next);
        #[cfg(not(target_os = "linux"))]
        let (pending_target, skip): (Option<&Arc<Track>>, usize) = (None, 0);
        let current = pending_target
            .filter(|t| self.queue.get(&t.id).is_some())
            .or(self.state.track.as_ref());
        let status = self.state.status;
        #[cfg(target_os = "linux")]
        let status = self.online.target_status().unwrap_or(status);
        let (target, from_history) =
            self.queue.step_skip(current.map(|t| &t.id), next, self.state.loop_mode, skip);
        if let Some(t) = target {
            self.load(t, status, Some(next), skip, from_history, finished, Duration::ZERO)?;
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
                let status = self.state.status;
                #[cfg(target_os = "linux")]
                let status = self.online.target_status().unwrap_or(status);
                return self.handle(if status == Status::Playing {
                    Cmd::Pause
                } else {
                    Cmd::Play
                });
            }
            Cmd::Play => self.play()?,
            Cmd::Pause => {
                #[cfg(target_os = "linux")]
                if let Some(label) = self.online.pause_pending(self.state.status == Status::Playing)
                {
                    (self.publish)(Event::Loading(label, true));
                }
                if self.state.status == Status::Playing {
                    self.player.pause();
                    self.state.status = Status::Paused;
                }
            }
            Cmd::Stop => self.stop(),
            Cmd::PlayTrack(id) => {
                let t = self.queue.get(&id).ok_or("Track is no longer in the library")?;
                self.load(t, Status::Playing, None, 0, false, false, Duration::ZERO)?;
            }
            Cmd::Next => self.navigate(true, false)?,
            Cmd::Previous => self.navigate(false, false)?,
            Cmd::SeekTo(id, pos) => {
                self.state.sink_empty = self.player.empty();
                if self.state.accepts_position(id.as_ref(), pos) {
                    self.seek(pos)?;
                }
            }
            Cmd::SeekRelative(offset) => {
                self.state.sink_empty = self.player.empty();
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
            #[cfg(target_os = "linux")]
            Cmd::OnlineDone(done) => {
                self.complete_online(done);
                return Ok(());
            }
            #[cfg(target_os = "linux")]
            Cmd::ImportScript(path) => self.import_script(path),
            #[cfg(target_os = "linux")]
            Cmd::EnableScript(enabled) => self.enable_script(enabled),
            #[cfg(target_os = "linux")]
            Cmd::PlayLocalSource(id) => {
                if let Some(track) = self.queue.get(&id).filter(|t| t.local_path().is_some())
                    && let Err(err) = self.begin_online(
                        track,
                        Status::Playing,
                        None,
                        0,
                        false,
                        false,
                        Duration::ZERO,
                    )
                {
                    self.stop_if_drained();
                    self.online_failure(err);
                }
            }
            #[cfg(target_os = "linux")]
            Cmd::PlayLink(query, quality) => {
                if let Err(err) = self.play_link(query, quality) {
                    self.stop_if_drained();
                    self.online_failure(err);
                }
            }
            Cmd::Search(q) => {
                if q == self.query {
                    return Ok(());
                }
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
                #[cfg(target_os = "linux")]
                online: online::Online::new(Default::default()),
                query: String::new(),
                pending_seek: None,
                tick_count: 0,
                mixer,
                player,
                publish,
                #[cfg(target_os = "linux")]
                mpris_available: Arc::new(AtomicBool::new(true)),
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

    #[cfg(target_os = "linux")]
    mod online_state_tests {
        use std::collections::BTreeMap;

        use super::*;
        use crate::{
            online::{OnlineTrack, Script},
            slint_types::OnlineIssue,
        };

        fn fake_script(enabled: bool) -> Script {
            let mut platforms = BTreeMap::new();
            platforms.insert("wy".into(), vec!["128k".into(), "320k".into()]);
            Script {
                id: "0".repeat(64),
                enabled,
                metadata: BTreeMap::new(),
                platforms,
                local_actions: vec![],
            }
        }

        fn wy_link(songmid: u64, name: &str) -> String {
            let payload = serde_json::json!({
                "source": "wy",
                "name": name,
                "singer": "Artist",
                "songmid": songmid,
                "interval": "00:02",
                "types": [{"type": "128k"}, {"type": "320k"}],
            });
            let mut url = url::Url::parse("lxmusic://music/play").unwrap();
            url.query_pairs_mut().append_pair("data", &payload.to_string());
            url.into()
        }

        fn fixture_audio() -> Box<rodio::Decoder<std::io::BufReader<std::fs::File>>> {
            let fixture =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tagged.flac");
            Box::new(local::decode(&fixture).unwrap())
        }

        #[test]
        fn local_details_require_the_current_request_and_track() {
            let mut h = TestHarness::new(2);
            let first = h.tracks[0].clone();
            let second = h.tracks[1].clone();
            let mut script = fake_script(true);
            script.local_actions.push("lyric".into());
            h.core.cfg.lx_script = Some(script);
            h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
            // Both embedded fields exist, so this commit needed no worker.
            assert!(h.core.online.details_id().is_none());
            let missing = metadata::TrackDetails::default();
            h.core.enhance_details(&first, &missing);
            let stale = h.core.online.details_id().unwrap();
            h.core.enhance_details(&first, &missing);
            let latest = h.core.online.details_id().unwrap();
            let reply = |id, track: &Track| {
                online::Completion::Details(id, track.id.clone(), Some(Default::default()))
            };
            h.events.lock().unwrap().clear();
            h.core.complete_online(reply(stale, &first));
            assert!(h.events.lock().unwrap().is_empty());
            h.core.handle(Cmd::PlayTrack(second.id.clone())).unwrap();
            h.events.lock().unwrap().clear();
            h.core.complete_online(reply(latest, &first));
            assert!(h.events.lock().unwrap().is_empty());
            assert!(h.core.state.synced_lyrics.is_some());
            assert_eq!(h.core.state.status, Status::Playing);
        }

        #[test]
        fn failed_play_link_does_not_pollute_queue_or_online_tracks() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));
            let link = wy_link(101, "Broken");

            h.core.handle(Cmd::PlayLink(link, "128k".into())).unwrap();
            let id = h.core.online.pending_id().unwrap();
            assert!(h.core.online.tracks.is_empty());

            h.core.complete_online(online::Completion::Prepared(
                id,
                Err((OnlineIssue::Resolve, "wy · 128k".into())),
            ));
            assert!(h.core.online.tracks.is_empty());
            assert_eq!(h.core.queue.step(None, true, LoopMode::None), Some(h.tracks[0].clone()));
        }

        #[test]
        fn pause_and_resume_while_loading_respects_prior_paused_state() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));
            let local = h.tracks[0].clone();

            // Start paused on local track, then trigger online load.
            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            h.core.handle(Cmd::Pause).unwrap();
            assert_eq!(h.core.state.status, Status::Paused);

            h.core.handle(Cmd::PlayLink(wy_link(102, "Online"), "128k".into())).unwrap();
            assert_eq!(h.core.online.target_status(), Some(Status::Playing));

            // Pause then resume while loading must NOT unpause the old paused local track.
            h.core.handle(Cmd::Pause).unwrap();
            assert_eq!(h.core.online.target_status(), Some(Status::Paused));
            h.core.handle(Cmd::Play).unwrap();
            assert_eq!(h.core.online.target_status(), Some(Status::Playing));
            assert_eq!(h.core.state.status, Status::Paused);

            // When old track WAS playing, pause pauses both and resume resumes both.
            h.core.cancel_load();
            h.core.handle(Cmd::Play).unwrap();
            assert_eq!(h.core.state.status, Status::Playing);
            h.core.handle(Cmd::PlayLink(wy_link(103, "Online2"), "128k".into())).unwrap();
            h.core.handle(Cmd::Pause).unwrap();
            assert_eq!(h.core.state.status, Status::Paused);
            assert_eq!(h.core.online.target_status(), Some(Status::Paused));
            h.core.handle(Cmd::Play).unwrap();
            assert_eq!(h.core.state.status, Status::Playing);
            assert_eq!(h.core.online.target_status(), Some(Status::Playing));
        }

        #[test]
        fn repeated_previous_advances_past_pending_online_history_track() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));
            let local = h.tracks[0].clone();

            // Play local -> commit online A -> commit online B, so back = [local, A], current = B.
            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(wy_link(201, "A"), "128k".into())).unwrap();
            let id_a = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_a, Ok(fixture_audio())));
            let track_a = h.core.state.track.clone().unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(202, "B"), "128k".into())).unwrap();
            let id_b = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_b, Ok(fixture_audio())));

            // First Previous begins loading A; second Previous skips A and loads local immediately.
            h.core.handle(Cmd::Previous).unwrap();
            assert!(h.core.online.loading());
            assert_eq!(h.core.online.nav_context(false).0.map(|t| &t.id), Some(&track_a.id));

            h.core.handle(Cmd::Previous).unwrap();
            assert!(!h.core.online.loading());
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&local.id));
        }

        #[test]
        fn restore_online_emits_details_and_reports_disabled_script_on_play() {
            let mut h = TestHarness::new(1);
            let mut reference = OnlineTrack::parse(&wy_link(301, "Restored")).unwrap();
            reference.quality = "128k".into();
            h.core.cfg.current_online = Some(reference);
            h.core.cfg.lx_script = Some(fake_script(false));
            h.events.lock().unwrap().clear();

            assert!(h.core.restore_online());
            assert!(h.core.state.deferred);
            assert!(h.events.lock().unwrap().iter().any(|ev| matches!(ev, Event::Details(_))));

            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::Play).unwrap();
            assert!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|ev| matches!(ev, Event::OnlineFailure((OnlineIssue::NoScript, _))))
            );
            assert_eq!(h.core.state.status, Status::Paused);
        }

        #[test]
        fn refresh_deleting_current_local_track_preserves_pending_online_load() {
            let mut h = TestHarness::new(2);
            h.core.cfg.lx_script = Some(fake_script(true));
            let first = h.tracks[0].clone();
            h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(wy_link(401, "Loading"), "128k".into())).unwrap();
            let pending_id = h.core.online.pending_id().unwrap();

            // Refresh to a fresh empty directory (so current local track disappears).
            let empty_dir = std::env::temp_dir()
                .join(format!("sealmusic_empty_refresh_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&empty_dir);
            h.core.handle(Cmd::Refresh(empty_dir.clone())).unwrap();
            let _ = std::fs::remove_dir_all(&empty_dir);

            assert_eq!(h.core.online.pending_id(), Some(pending_id));
            assert!(h.core.state.track.is_none());
            h.core.complete_online(online::Completion::Prepared(pending_id, Ok(fixture_audio())));
            assert_eq!(h.core.state.status, Status::Playing);
            assert!(h.core.state.track.as_ref().is_some_and(|t| t.title == "Loading"));
        }

        #[test]
        fn stale_completion_from_replaced_load_is_discarded() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));
            let local = h.tracks[0].clone();
            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(501, "SlowA"), "128k".into())).unwrap();
            let id_a = h.core.online.pending_id().unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(502, "FastB"), "320k".into())).unwrap();
            let id_b = h.core.online.pending_id().unwrap();
            assert_ne!(id_a, id_b);

            // Stale completion for A must be ignored without replacing current track or queue.
            h.core.complete_online(online::Completion::Prepared(id_a, Ok(fixture_audio())));
            assert_eq!(h.core.online.pending_id(), Some(id_b));
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&local.id));
            assert!(h.core.online.tracks.is_empty());

            h.core.complete_online(online::Completion::Prepared(id_b, Ok(fixture_audio())));
            assert!(!h.core.online.loading());
            assert_eq!(h.core.state.track.as_ref().map(|t| t.title.as_str()), Some("FastB"));
            assert_eq!(h.core.online.tracks.len(), 1);
        }

        #[test]
        fn same_track_quality_switch_updates_track_without_polluting_history() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));
            let local = h.tracks[0].clone();
            let link = wy_link(503, "SwitchQuality");

            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(link.clone(), "128k".into())).unwrap();
            let id_128 = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_128, Ok(fixture_audio())));
            let track_128_id = h.core.state.track.as_ref().unwrap().id.clone();

            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::PlayLink(link, "320k".into())).unwrap();
            let id_320 = h.core.online.pending_id().unwrap();
            assert_ne!(id_128, id_320);
            h.core.complete_online(online::Completion::Prepared(id_320, Ok(fixture_audio())));

            let current = h.core.state.track.as_ref().unwrap();
            assert_eq!(current.id, track_128_id);
            assert!(matches!(
                &current.origin,
                crate::library::TrackOrigin::Online(r) if r.quality == "320k"
            ));
            assert_eq!(h.core.online.tracks.len(), 1);
            assert!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|ev| matches!(ev, Event::State(_, Some(pos)) if *pos == Duration::ZERO))
            );
            // History must still step straight back to local rather than duplicating the same track.
            h.core.handle(Cmd::Previous).unwrap();
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&local.id));
        }

        #[test]
        fn stop_and_disable_script_cancel_pending_load_and_discard_stale_result() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));
            let local = h.tracks[0].clone();
            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(504, "Stopped"), "128k".into())).unwrap();
            let id_stop = h.core.online.pending_id().unwrap();
            h.core.handle(Cmd::Stop).unwrap();
            assert!(!h.core.online.loading());
            assert_eq!(h.core.state.status, Status::Stopped);
            h.core.complete_online(online::Completion::Prepared(id_stop, Ok(fixture_audio())));
            assert_eq!(h.core.state.status, Status::Stopped);
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&local.id));

            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(wy_link(505, "Disabled"), "128k".into())).unwrap();
            let id_disable = h.core.online.pending_id().unwrap();
            h.core.handle(Cmd::EnableScript(false)).unwrap();
            assert!(!h.core.online.loading());
            assert!(h.core.cfg.lx_script.as_ref().is_some_and(|s| !s.enabled));
            h.core.complete_online(online::Completion::Prepared(id_disable, Ok(fixture_audio())));
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&local.id));
            assert!(h.core.online.tracks.is_empty());
        }

        #[test]
        fn failed_script_import_preserves_existing_script_and_pending_load() {
            let mut h = TestHarness::new(1);
            let old = fake_script(true);
            h.core.cfg.lx_script = Some(old.clone());
            h.core.handle(Cmd::PlayLink(wy_link(506, "InFlight"), "128k".into())).unwrap();
            let pending_id = h.core.online.pending_id().unwrap();

            h.core.handle(Cmd::ImportScript(PathBuf::from("/nonexistent/bad.js"))).unwrap();
            let import_id = h.core.online.import_id().unwrap();
            h.events.lock().unwrap().clear();
            h.core.complete_online(online::Completion::Imported(
                import_id,
                Err((OnlineIssue::Initialization, String::new())),
            ));
            assert_eq!(h.core.cfg.lx_script.as_ref().map(|s| &s.id), Some(&old.id));
            assert!(h.core.cfg.lx_script.as_ref().is_some_and(|s| s.enabled));
            assert_eq!(h.core.online.pending_id(), Some(pending_id));
            assert!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|ev| matches!(ev, Event::OnlineFailure((OnlineIssue::Initialization, _))))
            );

            // Successful replacement cancels the old script's pending load and swaps active script.
            let mut replacement = fake_script(true);
            replacement.id = "1".repeat(64);
            h.core.handle(Cmd::ImportScript(PathBuf::from("/nonexistent/good.js"))).unwrap();
            let import_id_2 = h.core.online.import_id().unwrap();
            h.core.complete_online(online::Completion::Imported(import_id_2, Ok(replacement)));
            assert!(!h.core.online.loading());
            assert_eq!(
                h.core.cfg.lx_script.as_ref().map(|s| s.id.as_str()),
                Some("1".repeat(64).as_str())
            );
        }

        #[test]
        fn import_drain_delivers_late_alert_once_and_discards_after_disable() {
            let mut h = TestHarness::new(1);
            h.core.handle(Cmd::ImportScript(PathBuf::from("/nonexistent/good.js"))).unwrap();
            let import_id = h.core.online.import_id().unwrap();
            h.events.lock().unwrap().clear();

            h.core.complete_online(online::Completion::Imported(import_id, Ok(fake_script(true))));
            // Import completed immediately without blocking for background drain.
            assert!(h.core.online.import_id().is_none());
            assert!(h.core.cfg.lx_script.as_ref().is_some_and(|s| s.enabled));

            let alert = lx_probe::UpdateAlert {
                log: "New version".into(),
                url: Some("https://example.invalid/update".into()),
            };
            h.core.complete_online(online::Completion::Alert(import_id, alert.clone()));
            h.core.complete_online(online::Completion::Alert(import_id, alert.clone()));
            assert_eq!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|ev| matches!(ev, Event::UpdateAlert(_)))
                    .count(),
                1
            );

            // Disabling the script cancels background drain and drops stale alerts.
            h.core.handle(Cmd::ImportScript(PathBuf::from("/nonexistent/second.js"))).unwrap();
            let second_id = h.core.online.import_id().unwrap();
            h.core.complete_online(online::Completion::Imported(second_id, Ok(fake_script(true))));
            h.core.handle(Cmd::EnableScript(false)).unwrap();
            h.events.lock().unwrap().clear();
            h.core.complete_online(online::Completion::Alert(second_id, alert));
            assert!(!h.events.lock().unwrap().iter().any(|ev| matches!(ev, Event::UpdateAlert(_))));
        }

        #[test]
        fn play_local_source_error_stops_when_sink_already_drained() {
            let mut h = TestHarness::new(1);
            // Script enabled but does NOT declare local.musicUrl.
            h.core.cfg.lx_script = Some(fake_script(true));
            let local = h.tracks[0].clone();

            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(wy_link(510, "Pending"), "128k".into())).unwrap();
            h.core.player.clear();
            h.core.tick();
            assert!(h.core.state.sink_empty);
            assert_eq!(h.core.state.status, Status::Playing);

            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::PlayLocalSource(local.id.clone())).unwrap();
            assert!(!h.core.online.loading());
            assert_eq!(h.core.state.status, Status::Stopped);
            assert!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|ev| matches!(ev, Event::OnlineFailure((OnlineIssue::NoQuality, _))))
            );
        }

        #[test]
        fn drained_sink_while_auto_advancing_disables_seek_and_stops_on_failure() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));
            let local = h.tracks[0].clone();

            // Put an online track after local in queue, then play local.
            h.core.handle(Cmd::PlayLink(wy_link(507, "NextOnline"), "128k".into())).unwrap();
            let id_seed = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_seed, Ok(fixture_audio())));
            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            assert!(h.core.state.can_seek());

            // Simulate natural EOF of local -> tick() starts loading NextOnline.
            h.core.player.clear();
            h.core.tick();
            let id_next = h.core.online.pending_id().unwrap();
            assert!(h.core.state.sink_empty);
            assert!(!h.core.state.can_seek());
            assert_eq!(h.core.state.position, h.core.state.duration);

            // Seek commands while sink is drained must be ignored cleanly without error or Seeked.
            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::SeekTo(Some(local.id.clone()), Duration::from_millis(500))).unwrap();
            h.core.handle(Cmd::SeekRelative(1_000_000)).unwrap();
            assert!(
                !h.events.lock().unwrap().iter().any(|ev| matches!(ev, Event::State(_, Some(_))))
            );

            // Failure on auto-advanced track enters Stopped and reports once.
            h.events.lock().unwrap().clear();
            h.core.complete_online(online::Completion::Prepared(
                id_next,
                Err((OnlineIssue::Transfer, "HTTP 503".into())),
            ));
            assert_eq!(h.core.state.status, Status::Stopped);
            assert_eq!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|ev| matches!(ev, Event::OnlineFailure((OnlineIssue::Transfer, _))))
                    .count(),
                1
            );
        }

        #[test]
        fn eof_during_manual_load_stops_on_failure_or_script_disable() {
            let mut h = TestHarness::new(2);
            h.core.cfg.lx_script = Some(fake_script(true));
            let first = h.tracks[0].clone();

            // Play first, trigger manual load of B, then first reaches EOF before B finishes.
            h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(wy_link(508, "ManualB"), "128k".into())).unwrap();
            let id_b = h.core.online.pending_id().unwrap();
            h.core.player.clear();
            h.core.tick();
            assert_eq!(h.core.online.pending_id(), Some(id_b));
            assert!(h.core.state.sink_empty);
            assert!(!h.core.state.can_seek());

            // Manual load failure after EOF must transition to Stopped (not keep Playing + empty sink).
            h.core.complete_online(online::Completion::Prepared(
                id_b,
                Err((OnlineIssue::Resolve, "wy · 128k".into())),
            ));
            assert_eq!(h.core.state.status, Status::Stopped);
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&first.id));

            // Same scenario when user disables the active script after EOF during manual load.
            h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(wy_link(509, "ManualC"), "128k".into())).unwrap();
            h.core.player.clear();
            h.core.tick();
            h.core.handle(Cmd::EnableScript(false)).unwrap();
            assert!(!h.core.online.loading());
            assert_eq!(h.core.state.status, Status::Stopped);
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&first.id));
        }

        #[test]
        fn mixed_queue_preserves_navigation_across_sort_shuffle_refresh_and_loop_modes() {
            let mut h = TestHarness::new(2);
            h.core.cfg.lx_script = Some(fake_script(true));
            let dir = h.temp_dir.clone().unwrap();
            let local_0 = h.tracks[0].clone();

            // Commit two online tracks into mixed queue: [local_0, local_1, online_a, online_b].
            h.core.handle(Cmd::PlayTrack(local_0.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(wy_link(601, "OnlineA"), "128k".into())).unwrap();
            let id_a = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_a, Ok(fixture_audio())));
            let online_a = h.core.state.track.clone().unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(602, "OnlineB"), "128k".into())).unwrap();
            let id_b = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_b, Ok(fixture_audio())));
            let online_b = h.core.state.track.clone().unwrap();

            // Sort descending and refresh local directory: online tracks, current track, and history stay intact.
            h.core.handle(Cmd::Sort(crate::slint_types::SortKey::BySongName, false)).unwrap();
            h.core.handle(Cmd::Refresh(dir)).unwrap();
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&online_b.id));
            assert!(h.core.queue.get(&online_a.id).is_some());
            assert!(h.core.queue.get(&online_b.id).is_some());

            // LoopMode::None at tail of non-shuffled order stops next; LoopMode::Playlist wraps.
            h.core.handle(Cmd::SetLoopMode(LoopMode::None)).unwrap();
            assert!(!h.core.state.can_next);
            h.core.handle(Cmd::SetLoopMode(LoopMode::Playlist)).unwrap();
            assert!(h.core.state.can_next);

            // Shuffle keeps current online_b playable and Previous returns to online_a via history.
            h.core.handle(Cmd::SetShuffle(true)).unwrap();
            h.core.handle(Cmd::Previous).unwrap();
            assert_eq!(h.core.online.nav_context(false).0.map(|t| &t.id), Some(&online_a.id));
            let id_back = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_back, Ok(fixture_audio())));
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&online_a.id));

            // LoopMode::Track on EOF re-requests online_a.
            h.core.handle(Cmd::SetLoopMode(LoopMode::Track)).unwrap();
            h.core.player.clear();
            h.core.tick();
            assert_eq!(h.core.online.nav_context(true).0.map(|t| &t.id), Some(&online_a.id));
        }

        #[test]
        fn refresh_clearing_current_track_still_truncates_history_on_pending_previous() {
            let mut h = TestHarness::new(2);
            h.core.cfg.lx_script = Some(fake_script(true));
            let local_0 = h.tracks[0].clone();

            // Commit online A -> online B -> local_0, so back = [A, B], current = local_0.
            h.core.handle(Cmd::PlayLink(wy_link(701, "HistA"), "128k".into())).unwrap();
            let id_a = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_a, Ok(fixture_audio())));
            let track_a = h.core.state.track.clone().unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(702, "HistB"), "128k".into())).unwrap();
            let id_b = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_b, Ok(fixture_audio())));

            h.core.handle(Cmd::PlayTrack(local_0.id.clone())).unwrap();

            // Previous twice to skip B and target A.
            h.core.handle(Cmd::Previous).unwrap();
            h.core.handle(Cmd::Previous).unwrap();
            let id_target_a = h.core.online.pending_id().unwrap();

            // Refresh to empty dir deletes local_0 (state.track becomes None) while A is pending.
            let empty_dir =
                std::env::temp_dir().join(format!("sealmusic_empty_hist_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&empty_dir);
            h.core.handle(Cmd::Refresh(empty_dir.clone())).unwrap();
            let _ = std::fs::remove_dir_all(&empty_dir);
            assert!(h.core.state.track.is_none());

            // Committing A must still truncate [A, B] from back so LoopMode::None has no previous.
            h.core.handle(Cmd::SetLoopMode(LoopMode::None)).unwrap();
            h.core.complete_online(online::Completion::Prepared(id_target_a, Ok(fixture_audio())));
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&track_a.id));
            assert!(!h.core.state.can_previous);
        }
    }
}
