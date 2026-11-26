mod collections;
pub mod command;
mod jobs;
mod local;
#[cfg(target_os = "linux")]
mod online;
pub mod state;

#[cfg(all(test, target_os = "linux"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use command::{Inbox, PlayerCommand as Cmd};
use jobs::{Completion as MediaDone, MediaJobs};
use rodio::Source;
use state::{LoopMode, PlaybackEvent as Event, PlaybackState, Queue, SharedState, Status};

use crate::{
    config::Config,
    library::{Track, collections::Collections, metadata, scan},
};

const POSITION_SYNC_TICKS: u8 = 5;

struct PlayerCore<F> {
    cfg: Config,
    state: PlaybackState,
    shared: SharedState,
    tracks: Vec<Arc<Track>>,
    collections: Collections,
    queue: Queue,
    queue_visible: bool,
    #[cfg(target_os = "linux")]
    online: online::Online,
    query: String,
    pending_seek: Option<Duration>,
    tick_count: u8,
    mixer: rodio::mixer::Mixer,
    player: rodio::Player,
    publish: F,
    jobs: MediaJobs,
    scan_id: u64,
    initial_playback: Option<Status>,
    local_details_id: u64,
    pending_local_details: Option<u64>,
}

pub fn start(
    cfg: Config,
    shared: SharedState,
    inbox: Inbox,
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
        run_worker(cfg, Collections::load(), shared, inbox, mixer, publish)
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
        let cfg = run_worker(cfg, Collections::default(), shared, inbox, mixer, publish);
        running.store(false, Ordering::Relaxed);
        let _ = audio.join();
        cfg
    })
}

fn run_worker(
    mut cfg: Config,
    collections: Collections,
    shared: SharedState,
    inbox: Inbox,
    mixer: rodio::mixer::Mixer,
    publish: impl FnMut(Event) + Send + 'static,
) -> Config {
    if collections.get(cfg.selected_collection).is_none() {
        cfg.selected_collection = 0;
    }
    cfg.playing_collection =
        cfg.playing_collection.filter(|&index| collections.get(index).is_some());
    let player = rodio::Player::connect_new(&mixer);
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
        collections,
        queue: Queue::default(),
        queue_visible: false,
        #[cfg(target_os = "linux")]
        online: online::Online::new(inbox.sender.clone()),
        query: String::new(),
        pending_seek: None,
        tick_count: 0,
        mixer,
        player,
        publish,
        jobs: MediaJobs::new(inbox.sender),
        scan_id: 0,
        initial_playback: Some(Status::Paused),
        local_details_id: 0,
        pending_local_details: None,
    };
    core.collection_event();
    core.request_scan(core.cfg.song_dir.clone(), true);
    #[cfg(target_os = "linux")]
    core.script_event();
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
    core.sample_position();
    #[cfg(target_os = "linux")]
    {
        core.cancel_load();
        core.online.active_stream.take();
    }
    // A quit during initialization must keep the saved track and position.
    if core.initial_playback.is_none() || core.state.track.is_some() {
        core.cfg.current_song_path =
            core.state.track.as_ref().and_then(|track| track.local_path().map(ToOwned::to_owned));
        #[cfg(target_os = "linux")]
        {
            core.cfg.current_online =
                core.state.track.as_ref().and_then(|track| match &track.origin {
                    crate::library::TrackOrigin::Online(reference) => Some(reference.clone()),
                    _ => None,
                });
        }
        core.cfg.progress = if core.state.status == Status::Stopped {
            0.
        } else {
            core.state.position.as_secs_f32()
        };
    }
    core.player.clear();
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
            if !self.sync_stream() {
                self.broadcast();
                return;
            }
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
            if self.tick_count == 0 || !self.state.input_seekable {
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
        } else if self.player.empty() && !self.state.duration.is_zero() {
            self.state.duration
        } else if self.state.duration.is_zero() {
            self.player.get_pos().max(self.state.position)
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
        self.publish_upcoming();
    }
    fn publish_upcoming(&mut self) {
        let current = self.state.track.as_ref().map(|track| &track.id);
        if self.queue_visible
            && let Some(upcoming) = self.queue.upcoming_if_changed(current, self.state.loop_mode)
        {
            (self.publish)(Event::Upcoming(upcoming));
        }
    }
    fn rebuild_order(&mut self) {
        if let Some(index) = self.cfg.playing_collection {
            let mut tracks = self.sorted_collection(index);
            tracks.retain(|track| match &track.origin {
                crate::library::TrackOrigin::Local(path) => path.is_file(),
                #[cfg(target_os = "linux")]
                crate::library::TrackOrigin::Online(reference) => {
                    !reference.qualities(self.cfg.lx_script.as_ref()).is_empty()
                }
            });
            self.queue.rebuild(
                &tracks,
                self.state.shuffle,
                self.state.track.as_ref().map(|t| &t.id),
            );
            return;
        }
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
                        !r.qualities(self.cfg.lx_script.as_ref()).is_empty()
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
        #[cfg(target_os = "linux")]
        self.online.active_stream.take();
        self.state.deferred = false;
        self.player.clear();
        self.state.status = Status::Stopped;
        self.state.track = None;
        self.state.duration = Duration::ZERO;
        self.pending_seek = None;
        self.pending_local_details = None;
        #[cfg(target_os = "linux")]
        self.online.cancel_details();
        self.project_details(metadata::TrackDetails::default(), true, true);
    }
    fn reset_and_restore_local(&mut self, startup: bool) -> Result<(), String> {
        self.stop();
        self.clear_current_track();
        let saved = self.cfg.current_song_path.as_ref().filter(|_| startup).and_then(|p| {
            self.queue.get(&crate::library::TrackId::local(p)).or_else(|| {
                self.tracks.iter().find(|t| t.local_path() == Some(p.as_path())).cloned()
            })
        });
        let first = if self.cfg.playing_collection.is_some() {
            self.queue.step(None, true, LoopMode::None).filter(|track| track.local_path().is_some())
        } else {
            self.tracks.first().cloned()
        };
        if let Some(track) = saved.or(first) {
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
    fn request_scan(&mut self, dir: PathBuf, startup: bool) {
        self.scan_id += 1;
        let request = jobs::Scan {
            id: self.scan_id,
            dir,
            startup,
        };
        if self.jobs.scan.send(request).is_err() {
            log::error!("Library scan worker closed");
        }
    }

    fn apply_scan(
        &mut self,
        dir: PathBuf,
        mut tracks: Vec<Arc<Track>>,
        startup: bool,
    ) -> Result<(), String> {
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
            && let Some(updated) = self
                .tracks
                .iter()
                .find(|track| track.id == current.id)
                .cloned()
                .or_else(|| self.queue.get(&current.id))
        {
            self.state.track = Some(updated.clone());
            self.details(&updated, false);
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
    fn details(&mut self, track: &Arc<Track>, changed: bool) {
        self.pending_local_details = None;
        #[cfg(target_os = "linux")]
        self.online.cancel_details();
        match &track.origin {
            crate::library::TrackOrigin::Local(path) => {
                if changed {
                    self.project_details(metadata::TrackDetails::default(), true, true);
                }
                self.local_details_id += 1;
                let id = self.local_details_id;
                let request = jobs::Details::Read(id, track.id.clone(), path.clone());
                if self.jobs.details.send(request).is_ok() {
                    self.pending_local_details = Some(id);
                } else {
                    log::error!("Track details worker closed");
                }
            }
            #[cfg(target_os = "linux")]
            crate::library::TrackOrigin::Online(reference) => {
                let details = crate::online::inline_details(reference);
                self.enhance_details(track, details.cover.is_none(), details.lyrics.is_empty());
                self.project_details(details, true, true);
            }
        }
    }

    fn reload_details(&mut self) {
        if self.pending_local_details.is_some() {
            return;
        }
        if let Some((track, source)) =
            self.state.track.as_ref().zip(self.state.cover_source.as_ref())
        {
            if self
                .jobs
                .details
                .send(jobs::Details::Restore(track.id.clone(), source.clone()))
                .is_err()
            {
                log::error!("Track details worker closed");
            }
        } else {
            self.publish_details(
                metadata::TrackDetails::default(),
                Some(Arc::clone(&self.state.lyrics)),
                true,
            );
        }
    }

    fn waiting_for_startup_track(&self) -> bool {
        #[cfg(target_os = "linux")]
        if self.online.loading() {
            return false;
        }
        self.state.track.is_none()
    }

    fn complete_media(&mut self, done: MediaDone) {
        match done {
            MediaDone::Scan(request, tracks) if request.id == self.scan_id => {
                let waiting = self.waiting_for_startup_track();
                let initial_playback = self.initial_playback.take().filter(|_| waiting);
                let jobs::Scan {
                    dir,
                    startup,
                    ..
                } = request;
                let result = match tracks {
                    Ok(tracks) => self.apply_scan(dir, tracks, startup),
                    Err(err) if startup && err.kind() == std::io::ErrorKind::NotFound => {
                        self.apply_scan(dir, Vec::new(), true)
                    }
                    Err(err) => Err(format!("{}: {err}", dir.display())),
                };
                if let Err(err) = result {
                    if startup {
                        self.rebuild_order();
                    }
                    self.error(err);
                }
                #[cfg(target_os = "linux")]
                if startup
                    && self.state.track.is_none()
                    && !self.online.loading()
                    && self.cfg.current_online.is_some()
                    && !self.restore_online()
                {
                    self.cfg.current_online = None;
                    if let Err(err) = self.reset_and_restore_local(true) {
                        self.error(err);
                    }
                }
                match initial_playback {
                    Some(Status::Playing) => {
                        if let Err(err) = self.play() {
                            self.error(err);
                        }
                    }
                    Some(Status::Stopped) => self.stop(),
                    _ => {}
                }
                self.broadcast();
            }
            MediaDone::LocalDetails(id, track_id, details)
                if self.pending_local_details == Some(id)
                    && self.state.track.as_ref().is_some_and(|track| track.id == track_id) =>
            {
                self.pending_local_details = None;
                #[cfg(target_os = "linux")]
                {
                    let track = self.state.track.as_ref().unwrap().clone();
                    self.enhance_details(
                        &track,
                        details.cover.is_none(),
                        details.lyrics.is_empty(),
                    );
                }
                self.project_details(details, true, true);
                #[cfg(target_os = "linux")]
                self.sync_stream();
                self.broadcast();
            }
            MediaDone::RestoredCover(track, source, details)
                if self.state.track.as_ref().is_some_and(|current| current.id == track)
                    && self
                        .state
                        .cover_source
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &source)) =>
            {
                self.publish_details(details, Some(Arc::clone(&self.state.lyrics)), true);
            }
            _ => {}
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn merge_details(
        &mut self,
        track_id: &crate::library::TrackId,
        incoming: metadata::TrackDetails,
        prefer_incoming: bool,
    ) -> bool {
        if !self.state.track.as_ref().is_some_and(|t| &t.id == track_id) {
            return false;
        }
        let cover_changed =
            incoming.cover.is_some() && (prefer_incoming || self.state.cover_source.is_none());
        let lyrics_changed = !incoming.lyrics.is_empty()
            && (prefer_incoming || self.state.lyrics.is_empty())
            && incoming.lyrics.as_slice() != &*self.state.lyrics;
        if !cover_changed && !lyrics_changed {
            return false;
        }
        self.project_details(incoming, cover_changed, lyrics_changed);
        true
    }

    fn project_details(
        &mut self,
        mut details: metadata::TrackDetails,
        cover_changed: bool,
        lyrics_changed: bool,
    ) {
        let lyrics = if lyrics_changed {
            self.state.lyrics = Arc::from(std::mem::take(&mut details.lyrics));
            Some(Arc::clone(&self.state.lyrics))
        } else {
            details.lyrics.clear();
            None
        };
        let source = details.cover_source.take();
        if cover_changed {
            self.state.cover_source = source;
        } else {
            details.cover = None;
            details.ambient = None;
            details.palette = None;
        }
        self.publish_details(details, lyrics, cover_changed);
    }

    fn publish_details(
        &mut self,
        details: metadata::TrackDetails,
        lyrics: Option<Arc<[crate::library::lyrics::LyricLine]>>,
        cover_changed: bool,
    ) {
        self.sample_position();
        (self.publish)(Event::Details {
            details,
            lyrics,
            position: self.pending_seek.unwrap_or(self.state.position),
            cover_changed,
        });
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
        self.commit(t.clone(), status, source, position);
        self.queue.record_navigation(old, &t.id, direction, history_skip, from_history);
        Ok(())
    }

    fn commit(
        &mut self,
        t: Arc<Track>,
        status: Status,
        source: impl Source + Send + 'static,
        position: Duration,
    ) {
        let duration = source.total_duration().unwrap_or(t.duration);
        let changed = self.state.track.as_ref().is_none_or(|old| old.id != t.id);
        let was_deferred = self.state.deferred;
        // A new sink starts at position zero, including while paused or stopped.
        let next = rodio::Player::connect_new(&self.mixer);
        next.pause();
        next.set_volume(self.state.volume);
        #[cfg(target_os = "linux")]
        self.online.active_stream.take();
        next.append(source);
        if !position.is_zero() {
            let position = if duration.is_zero() {
                position
            } else {
                position.min(duration)
            };
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
        self.state.position = Duration::ZERO;
        self.state.sampled_at = Instant::now();
        self.state.input_seekable = true;
        self.state.deferred = false;
        self.state.duration = duration;
        self.state.track = Some(t.clone());
        self.state.status = status;
        if status == Status::Playing {
            self.player.play();
        }
        if changed || was_deferred {
            self.details(&t, changed);
        }
    }
    fn stop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            self.cancel_load();
            self.online.active_stream.take();
        }
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
        if self.initial_playback.is_some()
            && self.waiting_for_startup_track()
            && let Some(intent) = &mut self.initial_playback
        {
            let requested = match &cmd {
                Cmd::Play => Some(Status::Playing),
                Cmd::Pause => Some(Status::Paused),
                Cmd::Stop => Some(Status::Stopped),
                Cmd::TogglePlayPause if *intent == Status::Playing => Some(Status::Paused),
                Cmd::TogglePlayPause => Some(Status::Playing),
                _ => None,
            };
            if let Some(requested) = requested {
                *intent = requested;
                return Ok(());
            }
        }
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
                let track =
                    self.find_library_track(&id).ok_or("Track is no longer in the library")?;
                self.play_from(track, collections::QueueSource::Library, false)?;
            }
            Cmd::PlayQueuedTrack(id) => {
                if let Some(track) = self.queue.get(&id) {
                    self.load(track, Status::Playing, None, 0, false, false, Duration::ZERO)?;
                }
            }
            Cmd::PlayCollectionTrack(index, id, via_lx) => {
                if let Some(track) = self.find_collection_track(index, &id) {
                    self.play_from(track, collections::QueueSource::Collection(index), via_lx)?;
                }
            }
            Cmd::SetQueueVisible(visible) => {
                if self.queue_visible != visible {
                    self.queue_visible = visible;
                    self.queue.invalidate_upcoming();
                    self.publish_upcoming();
                }
                return Ok(());
            }
            Cmd::SelectCollection(index) => {
                if index != self.cfg.selected_collection && self.collections.get(index).is_some() {
                    self.cfg.selected_collection = index;
                    self.collection_event();
                }
                return Ok(());
            }
            Cmd::CreateCollection(name) => {
                self.create_collection(&name);
                return Ok(());
            }
            Cmd::ToggleLike(id) => {
                let liked = self.collections.likes.tracks.iter().any(|track| track.id == id);
                self.set_collection_member(0, &id, !liked);
            }
            Cmd::AddToCollection(index, id) => self.set_collection_member(index, &id, true),
            Cmd::RemoveFromCollection(index, id) => self.set_collection_member(index, &id, false),
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
                self.request_scan(dir, false);
                return Ok(());
            }
            Cmd::MediaDone(done) => {
                self.complete_media(done);
                return Ok(());
            }
            Cmd::Sort(key, asc) => {
                self.cfg.sort_key = key;
                self.cfg.sort_ascending = asc;
                scan::sort(&mut self.tracks, key, asc);
                if !self.state.shuffle {
                    self.rebuild_order();
                }
                self.library_event();
                self.collection_event();
            }
            #[cfg(target_os = "linux")]
            Cmd::SearchCatalog(source, query, page) => {
                self.search_catalog(source, query, page);
                return Ok(());
            }
            #[cfg(target_os = "linux")]
            Cmd::CancelCatalog => {
                self.cancel_catalog();
                return Ok(());
            }
            #[cfg(target_os = "linux")]
            Cmd::AbortCatalogSearch => {
                self.abort_catalog_search();
                return Ok(());
            }
            #[cfg(target_os = "linux")]
            Cmd::PlayCatalog(id) => {
                if let Err(err) = self.play_catalog(id) {
                    self.stop_if_drained();
                    self.online_failure(err);
                }
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
                if let Some(track) = self.find_library_track(&id) {
                    self.play_from(track, collections::QueueSource::Library, true)?;
                }
            }
            #[cfg(target_os = "linux")]
            Cmd::PlayLink(query) => {
                if let Err(err) = self.play_link(query) {
                    self.stop_if_drained();
                    self.online_failure(err);
                }
            }
            #[cfg(target_os = "linux")]
            Cmd::SetOnlineQuality(quality) => {
                if crate::online::is_quality(&quality) {
                    self.cfg.online_quality = quality;
                }
                return Ok(());
            }
            Cmd::Search(q) => {
                if q == self.query {
                    return Ok(());
                }
                self.query = q;
                self.results();
                return Ok(());
            }
            Cmd::ReloadDetails => {
                self.reload_details();
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
        _controls: command::Controls,
        media_rx: mpsc::Receiver<command::Request>,
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
            let (controls, inbox) = command::Controls::channel();
            let mut core = PlayerCore {
                cfg: Config::default(),
                state: PlaybackState::default(),
                shared: SharedState::default(),
                tracks: tracks.clone(),
                collections: Collections::default(),
                queue: Queue::default(),
                queue_visible: false,
                #[cfg(target_os = "linux")]
                online: online::Online::new(Default::default()),
                query: String::new(),
                pending_seek: None,
                tick_count: 0,
                mixer,
                player,
                publish,
                jobs: MediaJobs::new(inbox.sender),
                scan_id: 0,
                initial_playback: None,
                local_details_id: 0,
                pending_local_details: None,
            };
            core.rebuild_order();

            Self {
                core,
                _controls: controls,
                media_rx: inbox.rx,
                events,
                tracks,
                temp_dir,
                running,
                audio: Some(audio),
            }
        }
        fn finish_media(&mut self) -> bool {
            let request = self.media_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let scan = matches!(&request.command, Cmd::MediaDone(MediaDone::Scan(request, _)) if request.id == self.core.scan_id);
            self.core.handle(request.command).unwrap();
            scan
        }

        #[cfg(target_os = "linux")]
        fn refresh(&mut self, directory: PathBuf) {
            self.core.handle(Cmd::Refresh(directory)).unwrap();
            while !self.finish_media() {}
        }
    }

    #[test]
    fn startup_scan_preserves_play_intent_and_ignores_superseded_results() {
        let mut h = TestHarness::new(2);
        let dir = h.temp_dir.clone().unwrap();
        h.core.tracks.clear();
        h.core.rebuild_order();
        h.core.initial_playback = Some(Status::Paused);

        h.core.request_scan(dir.join("old"), true);
        let stale_id = h.core.scan_id;
        h.core.handle(Cmd::Refresh(dir.clone())).unwrap();
        h.core.handle(Cmd::TogglePlayPause).unwrap();
        h.core.handle(Cmd::Pause).unwrap();
        h.core.handle(Cmd::Play).unwrap();
        h.core.handle(Cmd::SetVolume(0.2)).unwrap();
        assert_eq!(h.core.state.volume, 0.2);

        h.core.complete_media(MediaDone::Scan(
            jobs::Scan {
                id: stale_id,
                dir: dir.join("old"),
                startup: true,
            },
            Ok(Vec::new()),
        ));
        assert!(h.core.state.track.is_none());
        assert_eq!(h.core.initial_playback, Some(Status::Playing));

        while !h.finish_media() {}
        assert_eq!(h.core.cfg.song_dir, dir);
        assert_eq!(h.core.state.status, Status::Playing);
        assert_eq!(h.core.state.track.as_ref().unwrap().id, h.core.tracks[0].id);
    }

    #[test]
    fn late_local_details_cannot_replace_the_new_track_details() {
        let mut h = TestHarness::new(2);
        let first = h.tracks[0].clone();
        let second = h.tracks[1].clone();
        h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
        let stale_id = h.core.pending_local_details.unwrap();
        h.core.handle(Cmd::PlayTrack(second.id.clone())).unwrap();
        let latest_id = h.core.pending_local_details.unwrap();

        h.events.lock().unwrap().clear();
        let old_details = metadata::read_details(first.local_path().unwrap());
        h.core.complete_media(MediaDone::LocalDetails(stale_id, first.id.clone(), old_details));
        assert!(h.events.lock().unwrap().is_empty());
        assert_eq!(h.core.pending_local_details, Some(latest_id));

        while h.core.pending_local_details.is_some() {
            h.finish_media();
        }
        assert_eq!(h.core.state.track.as_ref().unwrap().id, second.id);
        assert!(h.core.state.cover_source.is_some());
        assert!(!h.core.state.lyrics.is_empty());
    }

    #[test]
    fn upcoming_is_published_only_when_requested_and_reopens_at_the_current_track() {
        let mut h = TestHarness::new(3);
        h.core.handle(Cmd::PlayTrack(h.tracks[0].id.clone())).unwrap();
        assert!(!h.events.lock().unwrap().iter().any(|event| matches!(event, Event::Upcoming(_))));

        h.core.handle(Cmd::SetQueueVisible(true)).unwrap();
        assert!(matches!(
            h.events.lock().unwrap().last(),
            Some(Event::Upcoming(tracks)) if tracks.first() == Some(&h.tracks[1])
        ));

        h.core.handle(Cmd::SetQueueVisible(false)).unwrap();
        h.events.lock().unwrap().clear();
        h.core.handle(Cmd::Next).unwrap();
        assert!(!h.events.lock().unwrap().iter().any(|event| matches!(event, Event::Upcoming(_))));

        h.core.handle(Cmd::SetQueueVisible(true)).unwrap();
        assert!(matches!(
            h.events.lock().unwrap().last(),
            Some(Event::Upcoming(tracks)) if tracks.first() == Some(&h.tracks[2])
        ));
    }

    #[test]
    fn collection_playback_uses_its_members_while_browsing_other_lists() {
        let mut h = TestHarness::new(3);
        let [first, middle, last] = [0, 1, 2].map(|index| h.tracks[index].id.clone());

        h.core.handle(Cmd::ToggleLike(first.clone())).unwrap();
        h.core.handle(Cmd::CreateCollection("Road trip".into())).unwrap();
        h.core.handle(Cmd::AddToCollection(1, first.clone())).unwrap();
        h.core.handle(Cmd::AddToCollection(1, last.clone())).unwrap();
        h.core.handle(Cmd::AddToCollection(1, last.clone())).unwrap();
        assert_eq!(h.core.collections.get(1).unwrap().tracks.len(), 2);
        h.core.handle(Cmd::ToggleLike(first.clone())).unwrap();
        assert!(h.core.collections.likes.tracks.is_empty());

        h.core.handle(Cmd::PlayCollectionTrack(1, first.clone(), false)).unwrap();
        h.core.handle(Cmd::SelectCollection(0)).unwrap();
        h.core.handle(Cmd::PlayQueuedTrack(middle.clone())).unwrap();
        assert_eq!(h.core.state.track.as_ref().unwrap().id, first);
        h.core.handle(Cmd::PlayQueuedTrack(last.clone())).unwrap();
        assert_eq!(h.core.state.track.as_ref().unwrap().id, last);
        h.core.handle(Cmd::RemoveFromCollection(1, last.clone())).unwrap();
        assert_eq!(h.core.state.track.as_ref().unwrap().id, last);
        h.core.handle(Cmd::Next).unwrap();
        assert_eq!(h.core.state.track.as_ref().unwrap().id, first);

        h.core.handle(Cmd::PlayTrack(first)).unwrap();
        h.core.handle(Cmd::Next).unwrap();
        assert_eq!(h.core.state.track.as_ref().unwrap().id, middle);
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

        fn wy_link_qualities(songmid: u64, name: &str, qualities: &[&str]) -> String {
            let types: Vec<_> = qualities.iter().map(|q| serde_json::json!({"type": q})).collect();
            let payload = serde_json::json!({
                "source": "wy",
                "name": name,
                "singer": "Artist",
                "songmid": songmid,
                "interval": "00:02",
                "types": types,
            });
            let mut url = url::Url::parse("lxmusic://music/play").unwrap();
            url.query_pairs_mut().append_pair("data", &payload.to_string());
            url.into()
        }

        fn wy_link(songmid: u64, name: &str) -> String {
            wy_link_qualities(songmid, name, &["128k", "320k"])
        }

        fn pending_quality(core: &PlayerCore<Box<dyn FnMut(Event)>>) -> Option<String> {
            core.online.pending_track().and_then(|t| match &t.origin {
                crate::library::TrackOrigin::Online(r) => Some(r.quality.clone()),
                _ => None,
            })
        }

        fn fixture_audio() -> crate::online::PreparedAudio {
            let fixture =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tagged.flac");
            crate::online::PreparedAudio {
                source: Box::new(local::decode(&fixture).unwrap()),
                stream: None,
            }
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
            h.finish_media();
            // Both embedded fields exist, so no online enhancement is needed.
            assert!(h.core.online.details_id().is_none());
            h.core.enhance_details(&first, true, true);
            let stale = h.core.online.details_id().unwrap();
            h.core.enhance_details(&first, true, true);
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
            h.finish_media();
            assert!(!h.core.state.lyrics.is_empty());
            assert_eq!(h.core.state.status, Status::Playing);
        }

        #[test]
        fn failed_play_link_does_not_pollute_queue_or_online_tracks() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));
            let link = wy_link(101, "Broken");

            h.core.handle(Cmd::PlayLink(link)).unwrap();
            let mut attempts = 0;
            while let Some(id) = h.core.online.pending_id() {
                attempts += 1;
                assert!(h.core.online.tracks.is_empty());
                h.core.complete_online(online::Completion::Prepared(
                    id,
                    Err((OnlineIssue::Resolve, "wy".into())),
                ));
            }
            assert_eq!(attempts, 2);
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

            h.core.handle(Cmd::PlayLink(wy_link(102, "Online"))).unwrap();
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
            h.core.handle(Cmd::PlayLink(wy_link(103, "Online2"))).unwrap();
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
            h.core.handle(Cmd::PlayLink(wy_link(201, "A"))).unwrap();
            let id_a = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_a, Ok(fixture_audio())));
            let track_a = h.core.state.track.clone().unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(202, "B"))).unwrap();
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
            h.core.cfg.progress = 12.5;
            h.events.lock().unwrap().clear();

            assert!(h.core.restore_online());
            assert!(h.core.state.deferred);
            assert!(h.events.lock().unwrap().iter().any(
                |ev| matches!(ev, Event::Details { position, .. } if *position == Duration::from_secs_f32(12.5))
            ));

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
            h.core.handle(Cmd::PlayLink(wy_link(401, "Loading"))).unwrap();
            let pending_id = h.core.online.pending_id().unwrap();

            // Refresh to a fresh empty directory (so current local track disappears).
            let empty_dir = std::env::temp_dir()
                .join(format!("sealmusic_empty_refresh_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&empty_dir);
            h.refresh(empty_dir.clone());
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

            h.core.handle(Cmd::PlayLink(wy_link(501, "SlowA"))).unwrap();
            let id_a = h.core.online.pending_id().unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(502, "FastB"))).unwrap();
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
            h.core.handle(Cmd::SetOnlineQuality("128k".into())).unwrap();
            h.core.handle(Cmd::PlayLink(link.clone())).unwrap();
            let id_128 = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_128, Ok(fixture_audio())));
            let track_128_id = h.core.state.track.as_ref().unwrap().id.clone();
            assert!(matches!(
                &h.core.state.track.as_ref().unwrap().origin,
                crate::library::TrackOrigin::Online(r) if r.quality == "128k"
            ));

            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::SetOnlineQuality("320k".into())).unwrap();
            h.core.handle(Cmd::PlayLink(link)).unwrap();
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

            h.core.handle(Cmd::PlayLink(wy_link(504, "Stopped"))).unwrap();
            let id_stop = h.core.online.pending_id().unwrap();
            h.core.handle(Cmd::Stop).unwrap();
            assert!(!h.core.online.loading());
            assert_eq!(h.core.state.status, Status::Stopped);
            h.core.complete_online(online::Completion::Prepared(id_stop, Ok(fixture_audio())));
            assert_eq!(h.core.state.status, Status::Stopped);
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&local.id));

            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(wy_link(505, "Disabled"))).unwrap();
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
            h.core.handle(Cmd::PlayLink(wy_link(506, "InFlight"))).unwrap();
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
            h.core.handle(Cmd::PlayLink(wy_link(510, "Pending"))).unwrap();
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
            h.core.handle(Cmd::PlayLink(wy_link(507, "NextOnline"))).unwrap();
            let id_seed = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_seed, Ok(fixture_audio())));
            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            assert!(h.core.state.can_seek());

            // Simulate natural EOF of local -> tick() starts loading NextOnline.
            h.core.player.clear();
            h.core.tick();
            assert!(h.core.online.pending_id().is_some());
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

            // Failure across all fallback qualities on auto-advanced track enters Stopped and reports once.
            h.events.lock().unwrap().clear();
            let mut attempts = 0;
            while let Some(id_next) = h.core.online.pending_id() {
                attempts += 1;
                h.core.complete_online(online::Completion::Prepared(
                    id_next,
                    Err((OnlineIssue::Transfer, "HTTP 503".into())),
                ));
            }
            assert_eq!(attempts, 2);
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
            h.core.handle(Cmd::PlayLink(wy_link(508, "ManualB"))).unwrap();
            let id_b = h.core.online.pending_id().unwrap();
            h.core.player.clear();
            h.core.tick();
            assert_eq!(h.core.online.pending_id(), Some(id_b));
            assert!(h.core.state.sink_empty);
            assert!(!h.core.state.can_seek());

            // Exhausting fallback qualities after EOF must transition to Stopped (not keep Playing + empty sink).
            while let Some(id) = h.core.online.pending_id() {
                h.core.complete_online(online::Completion::Prepared(
                    id,
                    Err((OnlineIssue::Resolve, "wy".into())),
                ));
            }
            assert_eq!(h.core.state.status, Status::Stopped);
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&first.id));

            // Same scenario when user disables the active script after EOF during manual load.
            h.core.handle(Cmd::PlayTrack(first.id.clone())).unwrap();
            h.core.handle(Cmd::PlayLink(wy_link(509, "ManualC"))).unwrap();
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
            h.core.handle(Cmd::PlayLink(wy_link(601, "OnlineA"))).unwrap();
            let id_a = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_a, Ok(fixture_audio())));
            let online_a = h.core.state.track.clone().unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(602, "OnlineB"))).unwrap();
            let id_b = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_b, Ok(fixture_audio())));
            let online_b = h.core.state.track.clone().unwrap();

            // Sort descending and refresh local directory: online tracks, current track, and history stay intact.
            h.core.handle(Cmd::Sort(crate::library::SortKey::Title, false)).unwrap();
            h.refresh(dir);
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
        fn online_eviction_prunes_the_temporary_queue_but_keeps_liked_tracks_playable() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));
            let mut liked_id = None;
            let mut previous_id = None;
            let limit = online::MAX_SESSION_TRACKS as u64;

            for songmid in 0..=limit {
                h.core.handle(Cmd::PlayLink(wy_link(songmid, "Transient"))).unwrap();
                let id = h.core.online.pending_id().unwrap();
                h.core.complete_online(online::Completion::Prepared(id, Ok(fixture_audio())));
                let current = h.core.state.track.clone().unwrap();
                if songmid == 0 {
                    liked_id = Some(current.id.clone());
                    h.core.collections.likes.tracks.push(current);
                } else if songmid == limit - 1 {
                    previous_id = Some(current.id.clone());
                }
                h.events.lock().unwrap().clear();
            }

            let liked_id = liked_id.unwrap();
            assert_eq!(h.core.online.tracks.len(), online::MAX_SESSION_TRACKS);
            assert!(h.core.queue.get(&liked_id).is_none());
            h.core.handle(Cmd::Previous).unwrap();
            assert_eq!(h.core.online.pending_track().map(|track| &track.id), previous_id.as_ref());

            h.core.handle(Cmd::PlayCollectionTrack(0, liked_id.clone(), true)).unwrap();
            let id = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id, Ok(fixture_audio())));
            assert_eq!(h.core.state.track.as_ref().map(|track| &track.id), Some(&liked_id));
            assert!(h.core.queue.get(&liked_id).is_some());
        }

        #[test]
        fn refresh_clearing_current_track_still_truncates_history_on_pending_previous() {
            let mut h = TestHarness::new(2);
            h.core.cfg.lx_script = Some(fake_script(true));
            let local_0 = h.tracks[0].clone();

            // Commit online A -> online B -> local_0, so back = [A, B], current = local_0.
            h.core.handle(Cmd::PlayLink(wy_link(701, "HistA"))).unwrap();
            let id_a = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_a, Ok(fixture_audio())));
            let track_a = h.core.state.track.clone().unwrap();

            h.core.handle(Cmd::PlayLink(wy_link(702, "HistB"))).unwrap();
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
            h.refresh(empty_dir.clone());
            let _ = std::fs::remove_dir_all(&empty_dir);
            assert!(h.core.state.track.is_none());

            // Committing A must still truncate [A, B] from back so LoopMode::None has no previous.
            h.core.handle(Cmd::SetLoopMode(LoopMode::None)).unwrap();
            h.core.complete_online(online::Completion::Prepared(id_target_a, Ok(fixture_audio())));
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&track_a.id));
            assert!(!h.core.state.can_previous);
        }

        #[test]
        fn search_catalog_handles_abort_invalid_source_and_collection_playback() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));

            // Leaving the Search page aborts an in-flight search, but preserves completed results.
            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::SearchCatalog("wy".into(), "Fade".into(), 1)).unwrap();
            assert!(h.events.lock().unwrap().iter().any(
                |ev| matches!(ev, Event::Catalog(v) if v.source == "wy" && v.page == 1 && v.busy)
            ));
            let search_id = h.core.online.search_id().unwrap();
            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::AbortCatalogSearch).unwrap();
            assert!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|ev| matches!(ev, Event::Catalog(v) if v.page == 0 && !v.busy))
            );
            h.core.complete_online(online::Completion::Search(
                search_id,
                Ok(crate::online::SearchPage {
                    tracks: vec![],
                    has_next: false,
                    partial_failure: false,
                }),
            ));

            h.core.handle(Cmd::SearchCatalog("wy".into(), "Fade".into(), 2)).unwrap();
            let completed_id = h.core.online.search_id().unwrap();
            let catalog_track =
                OnlineTrack::parse(&wy_link(901, "CatalogSong")).unwrap().track().unwrap();
            h.core.complete_online(online::Completion::Search(
                completed_id,
                Ok(crate::online::SearchPage {
                    tracks: vec![catalog_track.clone()],
                    has_next: true,
                    partial_failure: false,
                }),
            ));
            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::AbortCatalogSearch).unwrap();
            assert!(h.events.lock().unwrap().is_empty());

            h.core.handle(Cmd::ToggleLike(catalog_track.id.clone())).unwrap();

            // Invalid source cancels catalog view.
            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::SearchCatalog("unknown".into(), "Fade".into(), 1)).unwrap();
            assert!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|ev| matches!(ev, Event::Catalog(v) if v.page == 0 && !v.busy))
            );
            h.core.handle(Cmd::PlayCollectionTrack(0, catalog_track.id.clone(), false)).unwrap();
            let pending = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(pending, Ok(fixture_audio())));
            assert_eq!(h.core.state.track.as_ref().unwrap().id, catalog_track.id);
            assert_eq!(h.core.cfg.playing_collection, Some(0));
            assert!(h.core.queue.get(&h.tracks[0].id).is_none());
        }

        #[test]
        fn online_details_project_merge_and_wait_past_deferred_restore() {
            let mut h = TestHarness::new(1);
            h.core.cfg.lx_script = Some(fake_script(true));

            // Deferred startup restore does NOT spawn network details until played.
            let mut restored = OnlineTrack::parse(&wy_link(801, "DeferredOnline")).unwrap();
            restored.quality = "128k".into();
            h.core.cfg.current_online = Some(restored);
            assert!(h.core.restore_online());
            assert!(h.core.online.details_id().is_none());

            // Pressing play and committing the previously deferred online track DOES schedule details.
            h.core.handle(Cmd::Play).unwrap();
            let pending_id = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(pending_id, Ok(fixture_audio())));
            let track_id = h.core.state.track.as_ref().unwrap().id.clone();
            let details_id = h.core.online.details_id().unwrap();

            // Simulate stream embedded cover arriving first, then API lyrics arriving via Completion::Details.
            let cover_img = image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 60, 40, 255]));
            let expected_palette = metadata::extract_palette(&cover_img.clone().into());
            assert!(expected_palette.is_some());
            let mut png = std::io::Cursor::new(Vec::new());
            cover_img.write_to(&mut png, image::ImageFormat::Png).unwrap();
            let embedded_cover = metadata::TrackDetails::from_cover_bytes(png.get_ref()).unwrap();
            assert!(h.core.merge_details(&track_id, embedded_cover, false));
            let cover_source = h.core.state.cover_source.clone().unwrap();
            assert_eq!(cover_source.palette, expected_palette);
            assert!(h.core.state.lyrics.is_empty());

            let api_lyrics = metadata::TrackDetails {
                lyrics: vec![crate::library::lyrics::LyricLine {
                    time: 1.5,
                    duration: 2.0,
                    text: "Online lyric".into(),
                    words: Vec::new(),
                }],
                ..Default::default()
            };
            h.core.complete_online(online::Completion::Details(
                details_id,
                track_id.clone(),
                Some(api_lyrics),
            ));
            // The lyrics update must preserve the embedded artwork.
            assert!(Arc::ptr_eq(h.core.state.cover_source.as_ref().unwrap(), &cover_source));
            assert!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find_map(|event| match event {
                        Event::Details {
                            cover_changed,
                            ..
                        } => Some(!cover_changed),
                        _ => None,
                    })
                    .unwrap_or(false)
            );
            assert_eq!(h.core.state.lyrics[0].text, "Online lyric");

            // A subsequent embedded fallback with different lyrics must NOT overwrite API lyrics.
            let late_embedded = metadata::TrackDetails {
                cover: Some(image::RgbaImage::new(2, 2).into()),
                lyrics: vec![crate::library::lyrics::LyricLine {
                    time: 0.0,
                    duration: 1.0,
                    text: "Fallback lyric".into(),
                    words: Vec::new(),
                }],
                ..Default::default()
            };
            assert!(!h.core.merge_details(&track_id, late_embedded, false));
            assert_eq!(h.core.state.lyrics[0].text, "Online lyric");

            // ReloadDetails restores artwork and lyrics without an MPRIS service.
            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::ReloadDetails).unwrap();
            h.finish_media();
            let reloaded = h.events.lock().unwrap().iter().find_map(|event| match event {
                Event::Details {
                    cover_changed: true,
                    details,
                    lyrics: Some(lyrics),
                    ..
                } => {
                    let cover = details.cover.as_ref()?;
                    Some((
                        cover.pixels.width(),
                        cover.pixels.height(),
                        details.palette,
                        lyrics.clone(),
                    ))
                }
                _ => None,
            });
            let (w, h_px, pal, reloaded_lyrics) =
                reloaded.expect("ReloadDetails emits cover and lyrics");
            assert_eq!((w, h_px), (4, 4));
            assert_eq!(pal, expected_palette);
            assert_eq!(reloaded_lyrics.len(), 1);
            assert_eq!(reloaded_lyrics[0].text, "Online lyric");

            // Switching from a sought track to another track must emit Details at 0s, not the old position.
            let local = h.tracks[0].clone();
            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            h.core.handle(Cmd::SeekTo(None, Duration::from_secs(1))).unwrap();
            h.events.lock().unwrap().clear();
            h.core.handle(Cmd::PlayLink(wy_link(802, "FreshStart"))).unwrap();
            let next_pending = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(next_pending, Ok(fixture_audio())));
            assert!(
                h.events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|ev| matches!(ev, Event::Details { position, .. } if position.is_zero()))
            );
        }

        #[test]
        fn preferred_quality_is_used_first_and_falls_back_when_missing_from_track_or_script() {
            let mut h = TestHarness::new(1);
            let mut script = fake_script(true);
            script.platforms.insert("wy".into(), vec!["128k".into(), "flac".into()]);
            h.core.cfg.lx_script = Some(script);

            // Track offers all 4, script offers 128k + flac. Preferred flac -> flac.
            let all_qs = wy_link_qualities(910, "AllQ", &["128k", "320k", "flac", "flac24bit"]);
            h.core.handle(Cmd::SetOnlineQuality("flac".into())).unwrap();
            h.core.handle(Cmd::PlayLink(all_qs.clone())).unwrap();
            assert_eq!(pending_quality(&h.core).as_deref(), Some("flac"));

            // Preferred 320k (unsupported by script) -> downgrades first to 128k.
            h.core.handle(Cmd::SetOnlineQuality("320k".into())).unwrap();
            h.core.handle(Cmd::PlayLink(all_qs)).unwrap();
            assert_eq!(pending_quality(&h.core).as_deref(), Some("128k"));

            // Preferred 128k when track only offers flac24bit -> upgrades to flac24bit.
            let mut hi_script = fake_script(true);
            hi_script.platforms.insert(
                "wy".into(),
                vec!["128k".into(), "320k".into(), "flac".into(), "flac24bit".into()],
            );
            h.core.cfg.lx_script = Some(hi_script);
            h.core.handle(Cmd::SetOnlineQuality("128k".into())).unwrap();
            h.core
                .handle(Cmd::PlayLink(wy_link_qualities(911, "OnlyHiRes", &["flac24bit"])))
                .unwrap();
            assert_eq!(pending_quality(&h.core).as_deref(), Some("flac24bit"));

            // Catalog selection honors preferred quality and falls back when absent from track.
            h.core.handle(Cmd::SearchCatalog("wy".into(), "AllQ".into(), 1)).unwrap();
            let search_id = h.core.online.search_id().unwrap();
            let cat_track = OnlineTrack::parse(&wy_link_qualities(912, "Cat", &["128k", "flac"]))
                .unwrap()
                .track()
                .unwrap();
            let cat_id = cat_track.id.clone();
            h.core.complete_online(online::Completion::Search(
                search_id,
                Ok(crate::online::SearchPage {
                    tracks: vec![cat_track],
                    has_next: false,
                    partial_failure: false,
                }),
            ));

            h.core.handle(Cmd::SetOnlineQuality("flac24bit".into())).unwrap();
            h.core.handle(Cmd::PlayCatalog(cat_id.clone())).unwrap();
            assert_eq!(pending_quality(&h.core).as_deref(), Some("flac"));

            h.core.handle(Cmd::SetOnlineQuality("320k".into())).unwrap();
            h.core.handle(Cmd::PlayCatalog(cat_id)).unwrap();
            assert_eq!(pending_quality(&h.core).as_deref(), Some("128k"));
        }

        #[test]
        fn runtime_failure_falls_back_through_qualities_preserving_pause_and_navigation() {
            let mut h = TestHarness::new(1);
            let mut script = fake_script(true);
            script.platforms.insert("wy".into(), vec!["128k".into(), "320k".into(), "flac".into()]);
            h.core.cfg.lx_script = Some(script);
            let local = h.tracks[0].clone();
            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();

            h.core.handle(Cmd::SetOnlineQuality("flac".into())).unwrap();
            h.core
                .handle(Cmd::PlayLink(wy_link_qualities(
                    920,
                    "Fallback",
                    &["128k", "320k", "flac"],
                )))
                .unwrap();
            let id_flac = h.core.online.pending_id().unwrap();
            assert_eq!(pending_quality(&h.core).as_deref(), Some("flac"));

            // Pause while loading: must carry over across fallback attempts.
            h.core.handle(Cmd::Pause).unwrap();
            assert_eq!(h.core.online.target_status(), Some(Status::Paused));
            h.events.lock().unwrap().clear();

            // Resolve failure on flac -> falls back to 320k without clearing loading or emitting failure.
            h.core.complete_online(online::Completion::Prepared(
                id_flac,
                Err((OnlineIssue::Resolve, "wy · flac".into())),
            ));
            let id_320 = h.core.online.pending_id().unwrap();
            assert_ne!(id_320, id_flac);
            assert_eq!(pending_quality(&h.core).as_deref(), Some("320k"));
            assert_eq!(h.core.online.target_status(), Some(Status::Paused));
            {
                let events = h.events.lock().unwrap();
                assert!(!events.iter().any(|ev| matches!(ev, Event::OnlineFailure(_))));
                assert!(
                    !events
                        .iter()
                        .any(|ev| matches!(ev, Event::Loading(label, _) if label.is_empty()))
                );
                assert!(events.iter().any(|ev| matches!(
                    ev,
                    Event::Loading(label, true) if label == "Fallback · 320k"
                )));
            }

            // Decode failure on 320k -> falls back to 128k.
            h.events.lock().unwrap().clear();
            h.core.complete_online(online::Completion::Prepared(
                id_320,
                Err((OnlineIssue::Decode, "wy · 320k".into())),
            ));
            let id_128 = h.core.online.pending_id().unwrap();
            assert_eq!(pending_quality(&h.core).as_deref(), Some("128k"));
            assert!(h.events.lock().unwrap().iter().any(|ev| matches!(
                ev,
                Event::Loading(label, true) if label == "Fallback · 128k"
            )));

            // Final quality succeeds: commits at 128k while paused and keeps Previous -> local.
            h.core.complete_online(online::Completion::Prepared(id_128, Ok(fixture_audio())));
            assert_eq!(h.core.state.status, Status::Paused);
            assert!(matches!(
                &h.core.state.track.as_ref().unwrap().origin,
                crate::library::TrackOrigin::Online(r) if r.quality == "128k"
            ));
            h.core.handle(Cmd::Previous).unwrap();
            assert_eq!(h.core.state.track.as_ref().map(|t| &t.id), Some(&local.id));
        }

        #[test]
        fn initialization_and_script_file_errors_do_not_retry_other_qualities() {
            for issue in [OnlineIssue::Initialization, OnlineIssue::ScriptFile] {
                let mut h = TestHarness::new(1);
                h.core.cfg.lx_script = Some(fake_script(true));
                h.core.handle(Cmd::SetOnlineQuality("320k".into())).unwrap();
                h.core.handle(Cmd::PlayLink(wy_link(930, "ScriptErr"))).unwrap();
                let id = h.core.online.pending_id().unwrap();

                h.events.lock().unwrap().clear();
                h.core
                    .complete_online(online::Completion::Prepared(id, Err((issue, String::new()))));
                assert!(!h.core.online.loading());
                assert_eq!(
                    h.events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|ev| matches!(ev, Event::OnlineFailure((got, _)) if *got == issue))
                        .count(),
                    1
                );
            }
        }

        #[test]
        fn queue_keeps_online_track_when_script_drops_previous_quality_and_replays_at_fallback() {
            let mut h = TestHarness::new(1);
            let mut script_v1 = fake_script(true);
            script_v1.platforms.insert("wy".into(), vec!["128k".into(), "flac".into()]);
            h.core.cfg.lx_script = Some(script_v1);
            let local = h.tracks[0].clone();

            h.core.handle(Cmd::SetOnlineQuality("flac".into())).unwrap();
            h.core
                .handle(Cmd::PlayLink(wy_link_qualities(940, "KeptOnline", &["128k", "flac"])))
                .unwrap();
            let id_flac = h.core.online.pending_id().unwrap();
            h.core.complete_online(online::Completion::Prepared(id_flac, Ok(fixture_audio())));
            let online_id = h.core.state.track.as_ref().unwrap().id.clone();

            // Return to local, then import a script that only allows 128k.
            h.core.handle(Cmd::PlayTrack(local.id.clone())).unwrap();
            let mut script_v2 = fake_script(true);
            script_v2.id = "2".repeat(64);
            script_v2.platforms.insert("wy".into(), vec!["128k".into()]);
            h.core.handle(Cmd::ImportScript(PathBuf::from("/nonexistent/v2.js"))).unwrap();
            let import_id = h.core.online.import_id().unwrap();
            h.core.complete_online(online::Completion::Imported(import_id, Ok(script_v2)));

            assert!(h.core.queue.get(&online_id).is_some());
            h.core.handle(Cmd::Next).unwrap();
            assert_eq!(h.core.online.pending_track().map(|t| &t.id), Some(&online_id));
            assert_eq!(pending_quality(&h.core).as_deref(), Some("128k"));
        }
    }
}
