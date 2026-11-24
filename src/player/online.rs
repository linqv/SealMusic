use std::{
    path::PathBuf,
    sync::{Arc, Weak, mpsc},
    time::Duration,
};

use tokio::{runtime::Runtime, task::JoinHandle};

use super::{
    Event, PlayerCore, Status,
    command::{PlayerCommand, Request},
};
use crate::{
    library::{Track, TrackId, TrackOrigin, metadata::TrackDetails},
    online::{self, OnlineTrack, Script},
    slint_types::OnlineIssue,
};

pub enum Completion {
    Search(u64, Result<online::SearchPage, ()>),
    Details(u64, TrackId, Option<TrackDetails>),
    Alert(u64, lx_probe::UpdateAlert),
    Imported(u64, online::Result<Script>),
    ImportDrained(u64),
    Prepared(u64, online::Result<online::PreparedAudio>),
    TransferEnded(u64),
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

#[derive(Default, Clone)]
pub struct CatalogView {
    pub source: String,
    pub query: String,
    pub page: u32,
    pub busy: bool,
    pub failed: bool,
    pub has_next: bool,
    pub rows: Vec<(Arc<Track>, Vec<String>)>,
}

pub(super) struct Online {
    search: Option<Job>,
    catalog: CatalogView,
    tx: Weak<mpsc::Sender<Request>>,
    seq: u64,
    import: Option<Job>,
    import_drain: Option<Job>,
    import_alert: Option<lx_probe::UpdateAlert>,
    shown_alert: bool,
    details: Option<Job>,
    pending: Option<Pending>,
    pub active_stream: Option<(u64, online::Stream)>,
    pub tracks: Vec<Arc<Track>>,
    // Keep last so every Job and Stream drops (setting cancelled and waking
    // Condvars) before Runtime::drop waits on blocking tasks.
    runtime: Runtime,
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
            search: None,
            catalog: CatalogView::default(),
            import: None,
            import_drain: None,
            import_alert: None,
            shown_alert: false,
            details: None,
            pending: None,
            active_stream: None,
            tracks: vec![],
        }
    }

    fn alert_handler(&self, id: u64) -> lx_probe::AlertHandler {
        let tx = self.tx.clone();
        Box::new(move |alert| Completion::Alert(id, alert).send(tx.clone()))
    }

    pub fn cancel_details(&mut self) {
        self.details.take();
    }
    #[cfg(test)]
    pub fn details_id(&self) -> Option<u64> {
        self.details.as_ref().map(|j| j.id)
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
    #[cfg(test)]
    pub(super) fn search_id(&self) -> Option<u64> {
        self.search.as_ref().map(|j| j.id)
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
        let alert = self.online.alert_handler(id);
        let handle = self.online.runtime.spawn(async move {
            match Script::import(path, alert).await {
                Ok((script, drain)) => {
                    Completion::Imported(id, Ok(script)).send(tx.clone());
                    if let Some(mut session) = drain
                        && let Err(err) = session.finish_import().await
                    {
                        log::debug!(
                            "LX initialization callbacks: {}",
                            online::sanitize_log(&err.to_string())
                        );
                    }
                    Completion::ImportDrained(id).send(tx);
                }
                Err(err) => Completion::Imported(id, Err(err)).send(tx),
            }
        });
        self.online.import_alert = None;
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
        self.online.import_drain.take();
        self.online.shown_alert = false;
        if let Some(track) = self.state.track.clone().filter(|t| t.local_path().is_some()) {
            self.details(&track);
        }
        self.rebuild_order();
        self.script_event();
        self.catalog_event();
        self.results();
    }

    pub(super) fn enhance_details(&mut self, track: &Arc<Track>, details: &TrackDetails) {
        self.online.details.take();
        match &track.origin {
            TrackOrigin::Local(_) => {
                let Some(script) = self.cfg.lx_script.as_ref() else {
                    return;
                };
                if !(details.lyrics.is_empty() && script.supports_local("lyric")
                    || details.cover.is_none() && script.supports_local("pic"))
                {
                    return;
                }
                self.online.seq += 1;
                let id = self.online.seq;
                let tx = self.online.tx.clone();
                let alert = self.online.alert_handler(id);
                let (script, track, details) = (script.clone(), track.clone(), details.clone());
                let handle = self.online.runtime.spawn(async move {
                    let result = online::enhance(script, &track, details, alert).await;
                    Completion::Details(id, track.id.clone(), result).send(tx);
                });
                self.online.details = Some(Job {
                    id,
                    handle,
                });
            }
            TrackOrigin::Online(reference) => {
                if self.state.deferred || (!details.lyrics.is_empty() && details.cover.is_some()) {
                    return;
                }
                self.online.seq += 1;
                let id = self.online.seq;
                let tx = self.online.tx.clone();
                let track_id = track.id.clone();
                let reference = reference.clone();
                let details = details.clone();
                let handle = self.online.runtime.spawn(async move {
                    let result = online::fetch_details(&reference, details).await;
                    Completion::Details(id, track_id, result).send(tx);
                });
                self.online.details = Some(Job {
                    id,
                    handle,
                });
            }
        }
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

    pub(super) fn search_catalog(&mut self, source: String, query: String, page: u32) {
        self.online.search.take();
        if page == 0
            || !online::is_search_source(&source)
            || query.trim().is_empty()
            || online::is_link(&query)
        {
            self.cancel_catalog();
            return;
        }
        self.online.catalog = CatalogView {
            source: source.clone(),
            query: query.clone(),
            page,
            busy: true,
            ..Default::default()
        };
        (self.publish)(Event::Catalog(self.online.catalog.clone()));
        self.online.seq += 1;
        let id = self.online.seq;
        let tx = self.online.tx.clone();
        let handle = self.online.runtime.spawn(async move {
            Completion::Search(id, online::search(&source, query.trim(), page).await).send(tx);
        });
        self.online.search = Some(Job {
            id,
            handle,
        });
    }

    pub(super) fn cancel_catalog(&mut self) {
        self.online.search.take();
        if self.online.catalog.page != 0 {
            self.online.catalog = CatalogView::default();
            (self.publish)(Event::Catalog(self.online.catalog.clone()));
        }
    }

    pub(super) fn abort_catalog_search(&mut self) {
        if self.online.search.take().is_some() && self.online.catalog.page != 0 {
            self.online.catalog = CatalogView::default();
            (self.publish)(Event::Catalog(self.online.catalog.clone()));
        }
    }

    fn catalog_event(&mut self) {
        if self.online.catalog.page == 0 {
            return;
        }
        for (track, qualities) in &mut self.online.catalog.rows {
            if let TrackOrigin::Online(reference) = &track.origin {
                *qualities = reference.qualities(self.cfg.lx_script.as_ref());
            }
        }
        (self.publish)(Event::Catalog(self.online.catalog.clone()));
    }

    pub(super) fn play_catalog(&mut self, id: TrackId, quality: String) -> online::Result<()> {
        let Some((track, _)) = self.online.catalog.rows.iter().find(|(t, _)| t.id == id) else {
            return Ok(());
        };
        let TrackOrigin::Online(mut reference) = track.origin.clone() else {
            return Ok(());
        };
        reference.quality = quality;
        self.begin_online(
            reference.track()?,
            Status::Playing,
            None,
            0,
            false,
            false,
            Duration::ZERO,
        )
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
        let script = self
            .cfg
            .lx_script
            .as_ref()
            .filter(|s| s.enabled)
            .ok_or((OnlineIssue::NoScript, track.title.clone()))?;
        let (action, label) = match &track.origin {
            TrackOrigin::Local(_) if script.supports_local("musicUrl") => {
                (online::local_action(&track, "musicUrl"), track.title.clone())
            }
            TrackOrigin::Online(reference)
                if reference.qualities(Some(script)).contains(&reference.quality) =>
            {
                (reference.action(), format!("{} · {}", track.title, reference.quality))
            }
            _ => return Err((OnlineIssue::NoQuality, track.title.clone())),
        };
        (self.publish)(Event::Loading(label.clone(), status != Status::Playing));
        let script = script.clone();
        self.online.seq += 1;
        let id = self.online.seq;
        let tx = self.online.tx.clone();
        let alert = self.online.alert_handler(id);
        let handle = self.online.runtime.spawn(async move {
            let transfer_tx = tx.clone();
            let result = online::prepare(script, action, alert, !position.is_zero(), move || {
                Completion::TransferEnded(id).send(transfer_tx);
            })
            .await;
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

    fn show_update(&mut self, alert: lx_probe::UpdateAlert) {
        if !self.online.shown_alert {
            self.online.shown_alert = true;
            (self.publish)(Event::UpdateAlert(alert));
        }
    }

    // The transfer can finish before the decoder is committed. Check the shared
    // outcome both at commit and when the completion message reaches the core.
    pub(super) fn sync_stream(&mut self) -> bool {
        let Some((_, stream)) = &self.online.active_stream else {
            return true;
        };
        let status = stream.status();
        let embedded = status.is_ok().then(|| stream.take_details()).flatten();
        match status {
            Ok((seekable, duration)) => {
                self.state.input_seekable = seekable;
                if let Some(duration) = duration {
                    self.state.duration = duration;
                }
                if let Some(details) = embedded
                    && let Some(track_id) = self.state.track.as_ref().map(|t| t.id.clone())
                {
                    self.merge_details(&track_id, details, false);
                }
                true
            }
            Err(err) => {
                self.online.active_stream.take();
                self.player.clear();
                self.state.status = Status::Stopped;
                self.state.input_seekable = false;
                self.online_failure(err);
                false
            }
        }
    }

    pub(super) fn complete_online(&mut self, done: Completion) {
        match done {
            Completion::TransferEnded(id)
                if self.online.active_stream.as_ref().is_some_and(|(active, _)| *active == id) =>
            {
                self.sync_stream();
            }
            Completion::Search(id, result)
                if self.online.search.as_ref().is_some_and(|j| j.id == id) =>
            {
                self.online.search.take();
                let view = &mut self.online.catalog;
                view.busy = false;
                match result {
                    Ok(page) => {
                        view.has_next = page.has_next;
                        view.rows = page.tracks.into_iter().map(|t| (t, vec![])).collect();
                    }
                    Err(()) => view.failed = true,
                }
                self.catalog_event();
                return;
            }
            Completion::Alert(id, alert)
                if self.online.import.as_ref().is_some_and(|j| j.id == id) =>
            {
                self.online.import_alert = Some(alert);
                return;
            }
            Completion::Alert(id, alert)
                if self.online.details.as_ref().is_some_and(|j| j.id == id)
                    || self.online.pending.as_ref().is_some_and(|p| p.job.id == id)
                    || self.online.import_drain.as_ref().is_some_and(|j| j.id == id) =>
            {
                self.show_update(alert);
                return;
            }
            Completion::ImportDrained(id)
                if self.online.import_drain.as_ref().is_some_and(|j| j.id == id) =>
            {
                self.online.import_drain.take();
                return;
            }
            Completion::Details(id, track_id, result)
                if self.online.details.as_ref().is_some_and(|j| j.id == id) =>
            {
                self.online.details.take();
                if let Some(details) = result {
                    if !self.merge_details(&track_id, details, true) {
                        return;
                    }
                } else {
                    return;
                }
            }
            Completion::Imported(id, result)
                if self.online.import.as_ref().is_some_and(|j| j.id == id) =>
            {
                let job = self.online.import.take().unwrap();
                match result {
                    Ok(script) => {
                        self.cancel_load();
                        self.stop_if_drained();
                        self.cfg.lx_script = Some(script);
                        self.script_changed();
                        if let Some(alert) = self.online.import_alert.take() {
                            self.show_update(alert);
                        } else {
                            self.online.import_drain = Some(job);
                        }
                    }
                    Err(err) => {
                        self.online.import_alert = None;
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
                if p.track.local_path().is_some() && self.queue.get(&p.track.id).is_none() {
                    self.stop_if_drained();
                    self.broadcast();
                    return;
                }
                let result = result.and_then(|prepared| {
                    if let Some(stream) = &prepared.stream {
                        stream.status()?;
                    }
                    Ok(prepared)
                });
                match result {
                    Ok(prepared) => {
                        let old = self.state.track.clone();
                        match self.commit(p.track.clone(), p.status, prepared.source, p.position) {
                            Ok(()) => {
                                self.online.active_stream =
                                    prepared.stream.map(|stream| (id, stream));
                                self.sync_stream();
                                if p.track.local_path().is_none() {
                                    self.online.upsert(p.track.clone());
                                    self.queue.upsert(p.track.clone());
                                }
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
        self.state.duration = if track.duration.is_zero() {
            Duration::ZERO
        } else {
            track.duration.max(self.state.position)
        };
        self.state.track = Some(track.clone());
        self.state.status = Status::Paused;
        self.state.deferred = true;
        self.rebuild_order();
        self.details(&track);
        true
    }
}
