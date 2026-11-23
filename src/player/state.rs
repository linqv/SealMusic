use std::{
    collections::VecDeque,
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
    #[cfg(target_os = "linux")]
    pub art_url: Option<String>,
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
            #[cfg(target_os = "linux")]
            art_url: None,
        }
    }
}
impl PlaybackState {
    pub fn position_now(&self) -> Duration {
        if self.status == Status::Playing {
            self.position.saturating_add(self.sampled_at.elapsed()).min(self.duration)
        } else {
            self.position
        }
    }
    pub fn can_seek(&self) -> bool {
        self.status != Status::Stopped && !self.duration.is_zero()
    }
    pub fn accepts_position(&self, id: Option<&TrackId>, position: Duration) -> bool {
        self.can_seek()
            && position <= self.duration
            && id.is_none_or(|id| self.track.as_ref().is_some_and(|t| &t.id == id))
    }
}

pub type SharedState = Arc<RwLock<PlaybackState>>;

pub enum PlaybackEvent {
    State(PlaybackState, Option<Duration>),
    Details(TrackDetails),
    Library(PathBuf, SortKey, bool, Vec<Arc<Track>>),
    Results(Vec<Arc<Track>>),
    Error(String),
    #[cfg(target_os = "linux")]
    Raise,
    #[cfg(target_os = "linux")]
    Quit,
}

#[derive(Default)]
pub struct Queue {
    order: Vec<Arc<Track>>,
    back: VecDeque<Arc<Track>>,
    forward: VecDeque<Arc<Track>>,
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
        // Resolve history against the new library to drop missing tracks and refresh metadata.
        let by_id: std::collections::HashMap<_, _> = tracks.iter().map(|t| (&t.id, t)).collect();
        for stack in [&mut self.back, &mut self.forward] {
            *stack = stack.iter().filter_map(|t| by_id.get(&t.id).map(|t| Arc::clone(t))).collect();
        }
    }
    pub fn step(
        &self,
        current: Option<&TrackId>,
        next: bool,
        looping: LoopMode,
    ) -> Option<Arc<Track>> {
        let history = if next {
            &self.forward
        } else {
            &self.back
        };
        if let Some(t) = history.back() {
            return Some(t.clone());
        }
        let Some(i) = self.order.iter().position(|t| Some(&t.id) == current) else {
            return self.order.first().cloned();
        };
        let neighbor = if next {
            self.order.get(i + 1)
        } else {
            i.checked_sub(1).and_then(|p| self.order.get(p))
        };
        neighbor
            .or_else(|| {
                if looping == LoopMode::None {
                    None
                } else if next {
                    self.order.first()
                } else {
                    self.order.last()
                }
            })
            .cloned()
    }
    /// Commit navigation only after the destination has successfully loaded.
    pub fn record(&mut self, old: Option<Arc<Track>>, new: &TrackId, next: Option<bool>) {
        let Some(old) = old.filter(|t| t.id != *new) else {
            return;
        };
        let (source, dest) = if next == Some(false) {
            (&mut self.back, &mut self.forward)
        } else {
            (&mut self.forward, &mut self.back)
        };
        if next.is_none() {
            source.clear();
        } else if source.back().is_some_and(|t| t.id == *new) {
            source.pop_back();
        }
        if dest.len() >= 256 {
            dest.pop_front();
        }
        dest.push_back(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::TrackOrigin;
    fn track(s: &str) -> Arc<Track> {
        Arc::new(Track {
            id: TrackId(s.into()),
            origin: TrackOrigin::Local(s.into()),
            title: s.into(),
            artist: String::new(),
            duration: Duration::from_secs(2),
        })
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
}
