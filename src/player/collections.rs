use std::{sync::Arc, time::Duration};

use super::{Event, PlayerCore, Queue, Status};
use crate::library::{Track, TrackId, collections::Collection, scan};

#[derive(Clone, Copy)]
pub(super) enum QueueSource {
    Library,
    Collection(usize),
}

impl<F: FnMut(Event)> PlayerCore<F> {
    pub(super) fn collection_event(&mut self) {
        let selected = self.cfg.selected_collection;
        let tracks = self.sorted_collection(selected);
        (self.publish)(Event::Collections(
            selected,
            self.collections.custom.iter().map(|list| list.name.clone()).collect(),
            tracks,
            self.collections.likes.tracks.iter().map(|track| track.id.clone()).collect(),
        ));
    }

    pub(super) fn sorted_collection(&self, index: usize) -> Vec<Arc<Track>> {
        let mut tracks =
            self.collections.get(index).map_or_else(Vec::new, |list| list.tracks.clone());
        scan::sort(&mut tracks, self.cfg.sort_key, self.cfg.sort_ascending);
        tracks
    }

    pub(super) fn find_library_track(&self, id: &TrackId) -> Option<Arc<Track>> {
        self.tracks.iter().find(|t| &t.id == id).cloned()
    }

    pub(super) fn find_collection_track(&self, index: usize, id: &TrackId) -> Option<Arc<Track>> {
        self.collections.get(index)?.tracks.iter().find(|t| &t.id == id).cloned()
    }

    fn find_collectible(&self, id: &TrackId) -> Option<Arc<Track>> {
        self.state
            .track
            .as_ref()
            .filter(|t| &t.id == id)
            .cloned()
            .or_else(|| self.find_library_track(id))
            .or_else(|| {
                #[cfg(target_os = "linux")]
                {
                    self.online.find_track(id)
                }
                #[cfg(not(target_os = "linux"))]
                None
            })
            .or_else(|| self.find_collection_track(self.cfg.selected_collection, id))
    }

    pub(super) fn create_collection(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let index = if let Some(index) =
            self.collections.custom.iter().position(|list| list.name == name)
        {
            index + 1
        } else {
            self.collections.custom.push(Collection {
                name: name.into(),
                tracks: Vec::new(),
            });
            if !self.collections.save() {
                self.collections.custom.pop();
                (self.publish)(Event::CollectionSaveFailed);
                return;
            }
            self.collections.custom.len()
        };
        self.cfg.selected_collection = index;
        self.collection_event();
    }

    pub(super) fn set_collection_member(&mut self, index: usize, id: &TrackId, add: bool) {
        let Some(list) = self.collections.get(index) else {
            return;
        };
        let mut tracks = list.tracks.clone();
        if let Some(position) = tracks.iter().position(|t| &t.id == id) {
            if add {
                return;
            }
            tracks.remove(position);
        } else if add && let Some(track) = self.find_collectible(id) {
            tracks.push(track);
        } else {
            return;
        }
        let old = std::mem::replace(&mut self.collections.get_mut(index).unwrap().tracks, tracks);
        if !self.collections.save() {
            self.collections.get_mut(index).unwrap().tracks = old;
            (self.publish)(Event::CollectionSaveFailed);
            return;
        }
        if self.cfg.playing_collection == Some(index) {
            self.rebuild_order();
        }
        self.collection_event();
    }

    pub(super) fn switch_queue(&mut self, source: QueueSource) -> bool {
        let index = match source {
            QueueSource::Library => None,
            QueueSource::Collection(index) => Some(index),
        };
        if self.cfg.playing_collection == index {
            return false;
        }
        self.cfg.playing_collection = index;
        self.queue = Queue::default();
        self.rebuild_order();
        true
    }

    pub(super) fn play_from(
        &mut self,
        track: Arc<Track>,
        source: QueueSource,
        via_lx: bool,
    ) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if via_lx || track.local_path().is_none() {
            match self.begin_online(track, Status::Playing, None, 0, false, false, Duration::ZERO) {
                Ok(()) => self.online.set_pending_queue(source),
                Err(err) => {
                    self.stop_if_drained();
                    self.online_failure(err);
                }
            }
            return Ok(());
        }
        #[cfg(not(target_os = "linux"))]
        let _ = via_lx;
        let generation = self.playback_gen;
        let result = self.load(track, Status::Playing, None, 0, false, false, Duration::ZERO);
        // A track prepared while the output is unavailable still belongs to its selected queue.
        if result.is_ok() || self.playback_gen != generation && self.state.status == Status::Paused
        {
            self.switch_queue(source);
        }
        result
    }
}
