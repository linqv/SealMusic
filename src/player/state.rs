use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use rand::seq::SliceRandom;

use crate::{
    library::{Track, TrackId, metadata::TrackDetails},
    slint_types::{LoopMode, SortKey},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    Playing,
    Paused,
    #[default]
    Stopped,
}

#[derive(Clone, Debug)]
pub struct PlaybackState {
    pub track: Option<Arc<Track>>,
    pub duration: Duration,
    pub position: Duration,
    pub sampled_at: Instant,
    pub status: Status,
    pub volume: f32,
    pub loop_mode: LoopMode,
    pub shuffle: bool,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_play: bool,
    pub deferred: bool,
    pub sink_empty: bool,
    pub input_seekable: bool,
    #[cfg(target_os = "linux")]
    pub art_url: Option<Arc<str>>,
    #[cfg(target_os = "linux")]
    pub synced_lyrics: Option<Arc<str>>,
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            track: None,
            duration: Duration::ZERO,
            position: Duration::ZERO,
            sampled_at: Instant::now(),
            status: Status::Stopped,
            volume: 1.,
            loop_mode: LoopMode::Playlist,
            shuffle: false,
            can_next: false,
            can_previous: false,
            can_play: false,
            deferred: false,
            sink_empty: false,
            input_seekable: true,
            #[cfg(target_os = "linux")]
            art_url: None,
            #[cfg(target_os = "linux")]
            synced_lyrics: None,
        }
    }
}
impl PlaybackState {
    pub fn position_now(&self) -> Duration {
        // Incomplete streams may stall between samples. Use Rodio's measured
        // position there so UI and MPRIS don't count buffering time as playback.
        if self.status == Status::Playing && !self.sink_empty && self.input_seekable {
            let position = self.position.saturating_add(self.sampled_at.elapsed());
            if self.duration.is_zero() {
                position
            } else {
                position.min(self.duration)
            }
        } else {
            self.position
        }
    }
    pub fn can_seek(&self) -> bool {
        self.input_seekable
            && !self.deferred
            && !self.sink_empty
            && self.status != Status::Stopped
            && !self.duration.is_zero()
    }
    pub fn accepts_position(&self, id: Option<&TrackId>, position: Duration) -> bool {
        self.can_seek()
            && position <= self.duration
            && id.is_none_or(|id| self.track.as_ref().is_some_and(|t| &t.id == id))
    }
}

pub type SharedState = Arc<RwLock<PlaybackState>>;

pub enum PlaybackEvent {
    State(Arc<PlaybackState>, Option<Duration>),
    Details(TrackDetails, Duration),
    Library(PathBuf, SortKey, bool, Vec<Arc<Track>>),
    Collections(usize, Vec<String>, Vec<Arc<Track>>, Vec<TrackId>),
    CollectionSaveFailed,
    Results(Vec<Arc<Track>>),
    Error(String),
    #[cfg(target_os = "linux")]
    Catalog(super::online::CatalogView),
    #[cfg(target_os = "linux")]
    Script(Option<crate::online::Script>, bool),
    #[cfg(target_os = "linux")]
    Link(String, String, crate::slint_types::OnlineIssue, Option<Arc<Track>>),
    #[cfg(target_os = "linux")]
    UpdateAlert(lx_probe::UpdateAlert),
    #[cfg(target_os = "linux")]
    OnlineFailure(crate::online::Failure),
    #[cfg(target_os = "linux")]
    Loading(String, bool),
    #[cfg(target_os = "linux")]
    Raise,
    #[cfg(target_os = "linux")]
    Quit,
}

#[derive(Default)]
pub struct Queue {
    order: Vec<Arc<Track>>,
    index_by_id: HashMap<TrackId, usize>,
    back: VecDeque<TrackId>,
    forward: VecDeque<TrackId>,
}

impl Queue {
    pub fn rebuild(&mut self, tracks: &[Arc<Track>], shuffle: bool, current: Option<&TrackId>) {
        self.order = tracks.to_vec();
        if shuffle {
            self.order.shuffle(&mut rand::rng());
            if let Some(index) = self.order.iter().position(|t| Some(&t.id) == current) {
                self.order.swap(0, index);
            }
        }
        self.index_by_id.clear();
        self.index_by_id.reserve(self.order.len());
        for (idx, track) in self.order.iter().enumerate() {
            self.index_by_id.insert(track.id.clone(), idx);
        }
        self.back.retain(|id| self.index_by_id.contains_key(id));
        self.forward.retain(|id| self.index_by_id.contains_key(id));
    }
    #[cfg(target_os = "linux")]
    pub fn upsert(&mut self, track: Arc<Track>) {
        if let Some(&i) = self.index_by_id.get(&track.id) {
            self.order[i] = track;
        } else {
            self.index_by_id.insert(track.id.clone(), self.order.len());
            self.order.push(track);
        }
    }
    pub fn get(&self, id: &TrackId) -> Option<Arc<Track>> {
        self.index_by_id.get(id).map(|&idx| Arc::clone(&self.order[idx]))
    }
    pub fn step(
        &self,
        current: Option<&TrackId>,
        next: bool,
        looping: LoopMode,
    ) -> Option<Arc<Track>> {
        self.step_skip(current, next, looping, 0).0
    }
    pub fn step_skip(
        &self,
        current: Option<&TrackId>,
        next: bool,
        looping: LoopMode,
        skip: usize,
    ) -> (Option<Arc<Track>>, bool) {
        let history = if next {
            &self.forward
        } else {
            &self.back
        };
        if let Some(track) = history
            .len()
            .checked_sub(skip + 1)
            .and_then(|idx| history.get(idx))
            .and_then(|id| self.get(id))
        {
            return (Some(track), true);
        }
        let Some(i) = current.and_then(|id| self.index_by_id.get(id).copied()) else {
            return (self.order.first().cloned(), false);
        };
        let neighbor = if next {
            self.order.get(i + 1)
        } else {
            i.checked_sub(1).and_then(|p| self.order.get(p))
        };
        let track = neighbor
            .or_else(|| {
                if looping == LoopMode::None {
                    None
                } else if next {
                    self.order.first()
                } else {
                    self.order.last()
                }
            })
            .cloned();
        (track, false)
    }
    /// Commit navigation only after the destination has successfully loaded.
    #[cfg(test)]
    fn record(&mut self, old: Option<Arc<Track>>, new: &TrackId, next: Option<bool>) {
        self.record_navigation(old, new, next, 0, next.is_some());
    }
    pub fn record_navigation(
        &mut self,
        old: Option<Arc<Track>>,
        new: &TrackId,
        next: Option<bool>,
        history_skip: usize,
        from_history: bool,
    ) {
        if old.as_ref().is_some_and(|t| t.id == *new) {
            return;
        }
        let (source, dest) = if next == Some(false) {
            (&mut self.back, &mut self.forward)
        } else {
            (&mut self.forward, &mut self.back)
        };
        if next.is_none() {
            source.clear();
        } else if from_history {
            if let Some(idx) = source.len().checked_sub(history_skip + 1)
                && source.get(idx) == Some(new)
            {
                source.truncate(idx);
            }
        } else if history_skip > 0 {
            source.truncate(source.len().saturating_sub(history_skip));
        }
        if let Some(old) = old.filter(|t| self.index_by_id.contains_key(&t.id)) {
            if dest.len() >= 256 {
                dest.pop_front();
            }
            dest.push_back(old.id.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::TrackOrigin;
    fn track(s: &str) -> Arc<Track> {
        Arc::new(Track::new(
            TrackId(s.into()),
            TrackOrigin::Local(s.into()),
            s.into(),
            String::new(),
            Duration::from_secs(2),
        ))
    }
    #[test]
    fn history_survives_reorder_and_drops_removed_tracks() {
        let (a, b, c) = (track("a"), track("b"), track("c"));
        let mut q = Queue::default();
        q.rebuild(&[a.clone(), b.clone(), c.clone()], false, None);
        q.record(Some(a.clone()), &c.id, None);
        q.rebuild(&[c.clone(), b.clone(), a.clone()], false, Some(&c.id));
        assert_eq!(q.step(Some(&c.id), false, LoopMode::None), Some(a.clone()));
        q.record(Some(c.clone()), &a.id, Some(false));
        assert_eq!(q.step(Some(&a.id), true, LoopMode::None), Some(c));
        q.rebuild(std::slice::from_ref(&a), false, Some(&a.id));
        assert!(q.step(Some(&a.id), true, LoopMode::None).is_none());
        assert_eq!(q.step(Some(&a.id), true, LoopMode::Playlist), Some(a));
    }
    #[test]
    fn seek_rejects_stale_track_and_stopped_state() {
        let t = track("a");
        let mut s = PlaybackState {
            track: Some(t.clone()),
            duration: t.duration,
            ..Default::default()
        };
        assert!(!s.accepts_position(Some(&t.id), Duration::ZERO));
        s.status = Status::Paused;
        assert!(!s.accepts_position(Some(&track("b").id), Duration::ZERO));
        assert!(!s.accepts_position(None, Duration::from_secs(3)));
        assert!(s.accepts_position(Some(&t.id), Duration::from_micros(12345)));
    }
    #[test]
    fn shuffle_rebuild_keeps_current_track_at_front() {
        let tracks: Vec<_> = ["a", "b", "c", "d", "e"].into_iter().map(track).collect();
        let mut q = Queue::default();
        q.rebuild(&tracks, true, Some(&tracks[3].id));
        assert_eq!(q.order.len(), tracks.len());
        assert_eq!(q.order[0].id, tracks[3].id);
    }
    #[test]
    fn step_skip_and_record_navigation_advance_past_pending_history_entries() {
        let (a, b, c, d) = (track("a"), track("b"), track("c"), track("d"));
        let mut q = Queue::default();
        q.rebuild(&[a.clone(), b.clone(), c.clone(), d.clone()], false, None);
        // Play a -> b -> d, so back = [a, b], current = d.
        q.record(Some(a.clone()), &b.id, None);
        q.record(Some(b.clone()), &d.id, None);

        // First Previous picks top of back (b); second skips b and picks a; third falls into order.
        assert_eq!(q.step_skip(Some(&d.id), false, LoopMode::Playlist, 0), (Some(b.clone()), true));
        assert_eq!(q.step_skip(Some(&b.id), false, LoopMode::Playlist, 1), (Some(a.clone()), true));
        assert_eq!(
            q.step_skip(Some(&a.id), false, LoopMode::Playlist, 2),
            (Some(d.clone()), false)
        );

        // Committing the second step (a) drops both b and a from back and pushes d to forward.
        q.record_navigation(Some(d.clone()), &a.id, Some(false), 1, true);
        assert_eq!(q.step(Some(&a.id), true, LoopMode::None), Some(d.clone()));
        assert_eq!(q.step(Some(&a.id), false, LoopMode::None), None);
    }
    #[test]
    fn record_navigation_updates_source_when_old_is_missing_or_removed() {
        let (a, b, c, gone) = (track("a"), track("b"), track("c"), track("gone"));
        let mut q = Queue::default();
        q.rebuild(&[a.clone(), b.clone(), c.clone()], false, None);
        q.record(Some(a.clone()), &b.id, None);
        q.record(Some(b.clone()), &c.id, None);

        // With old == None (e.g. current track disappeared during refresh), back still truncates.
        q.record_navigation(None, &a.id, Some(false), 1, true);
        assert_eq!(q.step(Some(&a.id), false, LoopMode::None), None);
        assert_eq!(q.step(Some(&a.id), true, LoopMode::None), Some(b.clone()));

        // Establish forward = [a], then jump manually from a removed track `gone` to `c`.
        q.record(Some(a.clone()), &b.id, Some(false));
        assert_eq!(q.step(Some(&b.id), true, LoopMode::None), Some(a));
        q.record_navigation(Some(gone), &c.id, None, 0, false);
        assert_eq!(q.step(Some(&c.id), true, LoopMode::None), None);
        assert_eq!(q.step(Some(&c.id), false, LoopMode::None), Some(b));
    }
    #[test]
    fn unknown_duration_advances_without_exposing_seek() {
        let state = PlaybackState {
            status: Status::Playing,
            position: Duration::from_secs(4),
            sampled_at: Instant::now() - Duration::from_secs(1),
            ..Default::default()
        };
        assert!(state.position_now() >= Duration::from_secs(5));
        assert!(!state.can_seek());
        let buffering = PlaybackState {
            duration: Duration::from_secs(8),
            input_seekable: false,
            ..state
        };
        assert!(!buffering.can_seek());
        assert_eq!(buffering.position_now(), buffering.position);
    }

    #[test]
    fn sink_empty_disables_seek_and_freezes_position() {
        let t = track("a");
        let s = PlaybackState {
            track: Some(t.clone()),
            duration: Duration::from_secs(10),
            position: Duration::from_secs(10),
            sampled_at: Instant::now() - Duration::from_secs(3),
            status: Status::Playing,
            sink_empty: true,
            ..Default::default()
        };
        assert!(!s.can_seek());
        assert!(!s.accepts_position(Some(&t.id), Duration::from_secs(2)));
        assert_eq!(s.position_now(), Duration::from_secs(10));
    }
}
