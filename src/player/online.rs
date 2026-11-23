use std::{
    fs::File,
    io::BufReader,
    path::PathBuf,
    sync::{Arc, Weak, mpsc},
    time::Duration,
};

use rodio::Decoder;
use tokio::{runtime::Runtime, task::JoinHandle};

use super::{
    Event, PlayerCore, Status,
    command::{PlayerCommand, Request},
};
use crate::{
    library::{Track, TrackOrigin},
    online::{self, OnlineTrack, Script},
    slint_types::OnlineIssue,
};

type Audio = Decoder<BufReader<File>>;

pub enum Completion {
    Imported(u64, online::Result<Script>),
    Prepared(u64, online::Result<Box<Audio>>),
}

struct Job {
    id: u64,
    handle: JoinHandle<()>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct Pending {
    job: Job,
    track: Arc<Track>,
    label: String,
    status: Status,
    paused_current: bool,
    direction: Option<bool>,
    history_skip: usize,
    from_history: bool,
    finished: bool,
    position: Duration,
}

pub(super) struct Online {
    runtime: Runtime,
    tx: Weak<mpsc::Sender<Request>>,
    seq: u64,
    import: Option<Job>,
    pending: Option<Pending>,
    pub tracks: Vec<Arc<Track>>,
}

impl Online {
    pub fn new(tx: Weak<mpsc::Sender<Request>>) -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("LX runtime"),
            tx,
            seq: 0,
            import: None,
            pending: None,
            tracks: vec![],
        }
    }

    pub fn loading(&self) -> bool {
        self.pending.is_some()
    }
    #[cfg(test)]
    pub(super) fn pending_id(&self) -> Option<u64> {
        self.pending.as_ref().map(|p| p.job.id)
    }
    #[cfg(test)]
    pub(super) fn import_id(&self) -> Option<u64> {
        self.import.as_ref().map(|j| j.id)
    }
    pub fn nav_context(&self, next: bool) -> (Option<&Arc<Track>>, usize) {
        let Some(p) = &self.pending else {
            return (None, 0);
        };
        let skip = if p.direction == Some(next) {
            p.history_skip + usize::from(p.from_history)
        } else {
            0
        };
        (Some(&p.track), skip)
    }
    pub fn target_status(&self) -> Option<Status> {
        self.pending.as_ref().map(|p| p.status)
    }
    pub fn pause_pending(&mut self, paused_current: bool) -> Option<String> {
        let p = self.pending.as_mut()?;
        p.status = Status::Paused;
        if paused_current {
            p.paused_current = true;
        }
        Some(p.label.clone())
    }
    pub fn resume_pending(&mut self) -> Option<(String, bool)> {
        let p = self.pending.as_mut()?;
        p.status = Status::Playing;
        let resume_current = std::mem::take(&mut p.paused_current);
        Some((p.label.clone(), resume_current))
    }
    fn upsert(&mut self, t: Arc<Track>) {
        if let Some(slot) = self.tracks.iter_mut().find(|old| old.id == t.id) {
            *slot = t;
        } else {
            self.tracks.push(t);
        }
    }
}

impl Completion {
    fn send(self, tx: Weak<mpsc::Sender<Request>>) {
        if let Some(tx) = tx.upgrade() {
            let _ = tx.send(Request {
                command: PlayerCommand::OnlineDone(self),
                reply: None,
            });
        }
    }
}

impl<F: FnMut(Event)> PlayerCore<F> {
    pub(super) fn online_failure(&mut self, err: online::Failure) {
        log::warn!("Online playback failure ({:?}): {}", err.0, online::sanitize_log(&err.1));
        (self.publish)(Event::OnlineFailure(err));
    }

    pub(super) fn cancel_load(&mut self) {
        if self.online.pending.take().is_some() {
            (self.publish)(Event::Loading(String::new(), false));
        }
    }

    pub(super) fn script_event(&mut self) {
        (self.publish)(Event::Script(self.cfg.lx_script.clone(), self.online.import.is_some()));
    }

    pub(super) fn import_script(&mut self, path: PathBuf) {
        self.online.seq += 1;
        let id = self.online.seq;
        let tx = self.online.tx.clone();
        let handle = self.online.runtime.spawn(async move {
            let result = Script::import(path).await;
            Completion::Imported(id, result).send(tx);
        });
        self.online.import = Some(Job {
            id,
            handle,
        });
        self.script_event();
    }

    pub(super) fn enable_script(&mut self, enabled: bool) {
        self.online.import.take();
        if enabled {
            if let Some(script) = &self.cfg.lx_script {
                match script.path() {
                    Ok(path) => self.import_script(path),
                    Err(err) => {
                        self.script_event();
                        self.online_failure(err);
                    }
                }
            } else {
                self.script_event();
            }
        } else {
            self.cancel_load();
            self.stop_if_drained();
            if let Some(script) = &mut self.cfg.lx_script {
                script.enabled = false;
            }
            self.script_changed();
        }
    }

    fn script_changed(&mut self) {
        self.rebuild_order();
        self.script_event();
        self.results();
    }

    fn link_info(&self, query: &str) -> online::Result<(OnlineTrack, Arc<Track>, Vec<String>)> {
        let reference = OnlineTrack::parse(query)?;
        let track = reference.track()?;
        let script = self
            .cfg
            .lx_script
            .as_ref()
            .filter(|s| s.enabled)
            .ok_or((OnlineIssue::NoScript, track.title.clone()))?;
        let qualities = reference.qualities(Some(script));
        if qualities.is_empty() {
            return Err((OnlineIssue::NoQuality, track.title.clone()));
        }
        Ok((reference, track, qualities))
    }

    pub(super) fn preview_link(&mut self) {
        let (title, qualities, issue) = match self.link_info(&self.query) {
            Ok((_, t, qs)) => (format!("{} · {}", t.title, t.artist), qs, OnlineIssue::None),
            Err((issue, detail)) => (detail, vec![], issue),
        };
        (self.publish)(Event::Link(self.query.clone(), title, qualities, issue));
    }

    pub(super) fn play_link(&mut self, query: String, quality: String) -> online::Result<()> {
        let (mut reference, _, qualities) = self.link_info(&query)?;
        if !qualities.contains(&quality) {
            return Err((OnlineIssue::NoQuality, quality));
        }
        reference.quality = quality;
        let track = reference.track()?;
        self.begin_online(track, Status::Playing, None, 0, false, false, Duration::ZERO)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn begin_online(
        &mut self,
        track: Arc<Track>,
        status: Status,
        direction: Option<bool>,
        history_skip: usize,
        from_history: bool,
        finished: bool,
        position: Duration,
    ) -> online::Result<()> {
        self.cancel_load();
        let TrackOrigin::Online(reference) = &track.origin else {
            unreachable!()
        };
        let script = self
            .cfg
            .lx_script
            .as_ref()
            .filter(|s| s.enabled)
            .ok_or((OnlineIssue::NoScript, track.title.clone()))?;
        if !reference.qualities(Some(script)).contains(&reference.quality) {
            return Err((OnlineIssue::NoQuality, track.title.clone()));
        }
        let label = format!("{} · {}", track.title, reference.quality);
        (self.publish)(Event::Loading(label.clone(), status != Status::Playing));
        let script = script.clone();
        let reference = reference.clone();
        self.online.seq += 1;
        let id = self.online.seq;
        let tx = self.online.tx.clone();
        let handle = self.online.runtime.spawn(async move {
            let result = online::prepare(script, reference).await.map(Box::new);
            Completion::Prepared(id, result).send(tx);
        });
        self.online.pending = Some(Pending {
            job: Job {
                id,
                handle,
            },
            track,
            label,
            status,
            paused_current: false,
            direction,
            history_skip,
            from_history,
            finished,
            position,
        });
        Ok(())
    }

    pub(super) fn complete_online(&mut self, done: Completion) {
        match done {
            Completion::Imported(id, result)
                if self.online.import.as_ref().is_some_and(|j| j.id == id) =>
            {
                self.online.import.take();
                match result {
                    Ok(script) => {
                        self.cancel_load();
                        self.stop_if_drained();
                        self.cfg.lx_script = Some(script);
                        self.script_changed();
                    }
                    Err(err) => {
                        self.script_event();
                        self.online_failure(err);
                    }
                }
            }
            Completion::Prepared(id, result)
                if self.online.pending.as_ref().is_some_and(|p| p.job.id == id) =>
            {
                let p = self.online.pending.take().unwrap();
                (self.publish)(Event::Loading(String::new(), false));
                match result {
                    Ok(source) => {
                        let old = self.state.track.clone();
                        match self.commit(p.track.clone(), p.status, *source, p.position) {
                            Ok(()) => {
                                self.online.upsert(p.track.clone());
                                self.queue.upsert(p.track.clone());
                                self.queue.record_navigation(
                                    old,
                                    &p.track.id,
                                    p.direction,
                                    p.history_skip,
                                    p.from_history,
                                );
                            }
                            Err(_) => {
                                if p.finished {
                                    self.stop();
                                } else {
                                    self.stop_if_drained();
                                }
                                self.online_failure((OnlineIssue::Decode, p.track.title.clone()));
                            }
                        }
                    }
                    Err(err) => {
                        if p.finished {
                            self.stop();
                        } else {
                            self.stop_if_drained();
                        }
                        self.online_failure(err);
                    }
                }
            }
            _ => return,
        }
        self.broadcast();
    }

    pub(super) fn restore_online(&mut self) -> bool {
        let Some(reference) = self.cfg.current_online.as_ref() else {
            return false;
        };
        let Ok(track) = reference.track() else {
            return false;
        };
        self.online.upsert(track.clone());
        self.state.position = Duration::try_from_secs_f32(self.cfg.progress).unwrap_or_default();
        self.state.duration = track.duration.max(self.state.position);
        self.state.track = Some(track.clone());
        self.state.status = Status::Paused;
        self.state.deferred = true;
        self.rebuild_order();
        self.details(&track);
        true
    }
}
