use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Weak, mpsc},
    time::Duration,
};

use tokio::{runtime::Runtime, task::JoinHandle};

use super::{
    Event, PlayerCore, Status,
    collections::QueueSource,
    command::{PlayerCommand, Request},
};
use crate::{
    library::{Track, TrackId, TrackOrigin, metadata::TrackDetails},
    online::{self, OnlineTrack, Script},
    slint_types::OnlineIssue,
};

pub(super) const MAX_SESSION_TRACKS: usize = 512;
const MAX_SESSION_METADATA_BYTES: usize = 8 * 1024 * 1024;

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

struct LoadContext {
    status: Status,
    paused_current: bool,
    direction: Option<bool>,
    history_skip: usize,
    from_history: bool,
    finished: bool,
    position: Duration,
    remaining_qualities: VecDeque<String>,
    queue_source: Option<QueueSource>,
}

struct Pending {
    job: Job,
    track: Arc<Track>,
    label: String,
    ctx: LoadContext,
}

#[derive(Default, Clone)]
pub struct CatalogView {
    pub source: String,
    pub query: String,
    pub page: u32,
    pub busy: bool,
    pub failed: bool,
    pub has_next: bool,
    pub partial_failure: bool,
    pub rows: Vec<Arc<Track>>,
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
    pub tracks: VecDeque<Arc<Track>>,
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
            tracks: VecDeque::new(),
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

    pub(super) fn find_track(&self, id: &TrackId) -> Option<Arc<Track>> {
        self.catalog.rows.iter().chain(&self.tracks).find(|track| &track.id == id).cloned()
    }

    pub(super) fn set_pending_queue(&mut self, source: QueueSource) {
        if let Some(pending) = &mut self.pending {
            pending.ctx.queue_source = Some(source);
        }
    }
    #[cfg(test)]
    pub(super) fn pending_id(&self) -> Option<u64> {
        self.pending.as_ref().map(|p| p.job.id)
    }
    #[cfg(test)]
    pub(super) fn pending_track(&self) -> Option<&Arc<Track>> {
        self.pending.as_ref().map(|p| &p.track)
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
        let skip = if p.ctx.direction == Some(next) {
            p.ctx.history_skip + usize::from(p.ctx.from_history)
        } else {
            0
        };
        (Some(&p.track), skip)
    }
    pub fn target_status(&self) -> Option<Status> {
        self.pending.as_ref().map(|p| p.ctx.status)
    }
    pub fn pause_pending(&mut self, paused_current: bool) -> Option<String> {
        let p = self.pending.as_mut()?;
        p.ctx.status = Status::Paused;
        if paused_current {
            p.ctx.paused_current = true;
        }
        Some(p.label.clone())
    }
    pub fn resume_pending(&mut self) -> Option<(String, bool)> {
        let p = self.pending.as_mut()?;
        p.ctx.status = Status::Playing;
        let resume_current = std::mem::take(&mut p.ctx.paused_current);
        Some((p.label.clone(), resume_current))
    }
    fn track_metadata_bytes(track: &Track) -> usize {
        let TrackOrigin::Online(reference) = &track.origin else {
            return 0;
        };
        reference.metadata_bytes()
            + track.id.0.capacity()
            + track.title.capacity()
            + track.artist.capacity()
            + track.album.capacity()
            + track.title_lower.capacity()
            + track.artist_lower.capacity()
    }

    fn upsert(&mut self, track: Arc<Track>) -> Vec<TrackId> {
        let id = track.id.clone();
        let existing = self.tracks.iter().position(|old| old.id == id);
        if Self::track_metadata_bytes(&track) > MAX_SESSION_METADATA_BYTES {
            if let Some(index) = existing {
                self.tracks.remove(index);
            }
            return vec![id];
        }
        if let Some(index) = existing {
            self.tracks[index] = track;
        } else {
            self.tracks.push_back(track);
        }

        let mut bytes =
            self.tracks.iter().map(|track| Self::track_metadata_bytes(track)).sum::<usize>();
        let mut evicted = Vec::new();
        while self.tracks.len() > MAX_SESSION_TRACKS || bytes > MAX_SESSION_METADATA_BYTES {
            // Keep the just-committed track and preserve the playback order of survivors.
            let oldest = usize::from(self.tracks[0].id == id);
            let removed = self.tracks.remove(oldest).unwrap();
            bytes -= Self::track_metadata_bytes(&removed);
            evicted.push(removed.id.clone());
        }
        evicted
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

    pub(super) fn link_info(&self, query: &str) -> online::Result<(OnlineTrack, Arc<Track>)> {
        let reference = OnlineTrack::parse(query)?;
        let track = reference.track()?;
        let script = self
            .cfg
            .lx_script
            .as_ref()
            .filter(|s| s.enabled)
            .ok_or((OnlineIssue::NoScript, track.title.clone()))?;
        if reference.qualities(Some(script)).is_empty() {
            return Err((OnlineIssue::NoQuality, track.title.clone()));
        }
        Ok((reference, track))
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

    pub(super) fn play_catalog(&mut self, id: TrackId) -> online::Result<()> {
        let Some(track) = self.online.catalog.rows.iter().find(|t| t.id == id).cloned() else {
            return Ok(());
        };
        self.begin_online(track, Status::Playing, None, 0, false, false, Duration::ZERO)?;
        self.online.set_pending_queue(QueueSource::Library);
        Ok(())
    }

    pub(super) fn preview_link(&mut self) {
        let (title, issue, track) = match self.link_info(&self.query) {
            Ok((_, t)) => (format!("{} · {}", t.title, t.artist), OnlineIssue::None, Some(t)),
            Err((issue, detail)) => (detail, issue, None),
        };
        (self.publish)(Event::Link(self.query.clone(), title, issue, track));
    }

    pub(super) fn play_link(&mut self, query: String) -> online::Result<()> {
        let (_, track) = self.link_info(&query)?;
        self.begin_online(track, Status::Playing, None, 0, false, false, Duration::ZERO)?;
        self.online.set_pending_queue(QueueSource::Library);
        Ok(())
    }

    fn spawn_prepare(
        &mut self,
        script: Script,
        action: serde_json::Value,
        track: Arc<Track>,
        label: String,
        ctx: LoadContext,
    ) {
        self.online.seq += 1;
        let id = self.online.seq;
        let tx = self.online.tx.clone();
        let alert = self.online.alert_handler(id);
        let resume = !ctx.position.is_zero();
        let handle = self.online.runtime.spawn(async move {
            let transfer_tx = tx.clone();
            let result = online::prepare(script, action, alert, resume, move || {
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
            ctx,
        });
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
        let (track, action, label, remaining_qualities) = match &track.origin {
            TrackOrigin::Local(_) if script.supports_local("musicUrl") => (
                track.clone(),
                online::local_action(&track, "musicUrl"),
                track.title.clone(),
                VecDeque::new(),
            ),
            TrackOrigin::Online(reference) => {
                let mut candidates = VecDeque::from(
                    reference.ordered_qualities(Some(script), &self.cfg.online_quality),
                );
                let Some(quality) = candidates.pop_front() else {
                    return Err((OnlineIssue::NoQuality, track.title.clone()));
                };
                let mut chosen = reference.clone();
                chosen.quality = quality;
                let track = chosen.track()?;
                let action = chosen.action();
                let label = format!("{} · {}", track.title, chosen.quality);
                (track, action, label, candidates)
            }
            _ => return Err((OnlineIssue::NoQuality, track.title.clone())),
        };
        (self.publish)(Event::Loading(label.clone(), status != Status::Playing));
        let script = script.clone();
        self.spawn_prepare(
            script,
            action,
            track,
            label,
            LoadContext {
                status,
                paused_current: false,
                direction,
                history_skip,
                from_history,
                finished,
                position,
                remaining_qualities,
                queue_source: None,
            },
        );
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
                        view.partial_failure = page.partial_failure;
                        view.rows = page.tracks;
                    }
                    Err(()) => view.failed = true,
                }
                (self.publish)(Event::Catalog(self.online.catalog.clone()));
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
                let mut p = self.online.pending.take().unwrap();
                if p.track.local_path().is_some()
                    && match p.ctx.queue_source {
                        Some(QueueSource::Library) => {
                            self.find_library_track(&p.track.id).is_none()
                        }
                        Some(QueueSource::Collection(index)) => {
                            self.find_collection_track(index, &p.track.id).is_none()
                        }
                        None => self.queue.get(&p.track.id).is_none(),
                    }
                {
                    (self.publish)(Event::Loading(String::new(), false));
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
                        (self.publish)(Event::Loading(String::new(), false));
                        let old = self.state.track.clone();
                        self.commit(p.track.clone(), p.ctx.status, prepared.source, p.ctx.position);
                        self.online.active_stream = prepared.stream.map(|stream| (id, stream));
                        self.sync_stream();
                        let evicted = if p.track.local_path().is_none() {
                            self.online.upsert(p.track.clone())
                        } else {
                            Vec::new()
                        };
                        let changed_queue =
                            p.ctx.queue_source.is_some_and(|source| self.switch_queue(source));
                        let temporary_queue = self.cfg.playing_collection.is_none();
                        if p.track.local_path().is_none()
                            && if temporary_queue {
                                !evicted.contains(&p.track.id)
                            } else {
                                self.queue.get(&p.track.id).is_some()
                            }
                        {
                            self.queue.upsert(p.track.clone());
                        }
                        self.queue.record_navigation(
                            old.filter(|_| !changed_queue),
                            &p.track.id,
                            p.ctx.direction,
                            p.ctx.history_skip,
                            p.ctx.from_history,
                        );
                        if temporary_queue {
                            self.queue.remove_tracks(&evicted);
                        }
                    }
                    Err(err) => {
                        if matches!(
                            err.0,
                            OnlineIssue::Resolve | OnlineIssue::Transfer | OnlineIssue::Decode
                        ) && let Some(next_quality) = p.ctx.remaining_qualities.pop_front()
                            && let Some(script) =
                                self.cfg.lx_script.as_ref().filter(|s| s.enabled).cloned()
                            && let TrackOrigin::Online(reference) = &p.track.origin
                        {
                            let mut next_ref = reference.clone();
                            next_ref.quality = next_quality;
                            if let Ok(next_track) = next_ref.track() {
                                log::info!(
                                    "Online quality '{}' failed for '{}' ({:?}); falling back to '{}'",
                                    reference.quality,
                                    p.track.title,
                                    err.0,
                                    next_ref.quality
                                );
                                let label = format!("{} · {}", next_track.title, next_ref.quality);
                                (self.publish)(Event::Loading(
                                    label.clone(),
                                    p.ctx.status != Status::Playing,
                                ));
                                let action = next_ref.action();
                                self.spawn_prepare(script, action, next_track, label, p.ctx);
                                return;
                            }
                        }
                        (self.publish)(Event::Loading(String::new(), false));
                        if p.ctx.finished {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_budget_evicts_metadata_before_reaching_the_track_limit() {
        let mut url = url::Url::parse("lxmusic://music/play").unwrap();
        url.query_pairs_mut().append_pair(
            "data",
            r#"{"source":"wy","songmid":1,"name":"Title","singer":"Artist","types":[]}"#,
        );
        let reference = OnlineTrack::parse(url.as_str()).unwrap();
        let mut online = Online::new(Weak::new());

        for index in 0..3 {
            let mut track = reference.track().unwrap();
            let data = Arc::get_mut(&mut track).unwrap();
            data.id.0 = index.to_string();
            data.album = "x".repeat(MAX_SESSION_METADATA_BYTES / 3);
            let evicted = online.upsert(track);
            if index == 2 {
                assert_eq!(evicted, vec![TrackId("0".into())]);
            } else {
                assert!(evicted.is_empty());
            }
        }
        assert_eq!(online.tracks.len(), 2);
    }
}
