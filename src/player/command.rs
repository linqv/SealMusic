use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Duration,
};

use crate::{
    library::TrackId,
    slint_types::{LoopMode, SortKey},
};

pub enum PlayerCommand {
    Play,
    Pause,
    TogglePlayPause,
    Stop,
    PlayTrack(TrackId),
    PlayQueuedTrack(TrackId),
    PlayCollectionTrack(usize, TrackId, bool),
    SelectCollection(usize),
    CreateCollection(String),
    ToggleLike(TrackId),
    AddToCollection(usize, TrackId),
    RemoveFromCollection(usize, TrackId),
    Next,
    Previous,
    SeekTo(Option<TrackId>, Duration),
    SeekRelative(i64),
    SetVolume(f32),
    SetLoopMode(LoopMode),
    SetShuffle(bool),
    Refresh(PathBuf),
    Sort(SortKey, bool),
    Search(String),
    #[cfg(target_os = "linux")]
    SearchCatalog(String, String, u32),
    #[cfg(target_os = "linux")]
    CancelCatalog,
    #[cfg(target_os = "linux")]
    AbortCatalogSearch,
    #[cfg(target_os = "linux")]
    PlayCatalog(TrackId),
    #[cfg(target_os = "linux")]
    ImportScript(PathBuf),
    #[cfg(target_os = "linux")]
    EnableScript(bool),
    #[cfg(target_os = "linux")]
    OnlineDone(super::online::Completion),
    #[cfg(target_os = "linux")]
    PlayLocalSource(TrackId),
    #[cfg(target_os = "linux")]
    PlayLink(String),
    #[cfg(target_os = "linux")]
    SetOnlineQuality(String),
    #[cfg(target_os = "linux")]
    Raise,
    #[cfg(target_os = "linux")]
    Quit,
    Shutdown,
}

pub struct Request {
    pub command: PlayerCommand,
    #[cfg(target_os = "linux")]
    pub reply: Option<futures_channel::oneshot::Sender<Result<(), String>>>,
}

#[derive(Clone)]
pub struct Controls(Arc<mpsc::Sender<Request>>);

pub struct Inbox {
    pub rx: mpsc::Receiver<Request>,
    #[cfg(target_os = "linux")]
    pub sender: std::sync::Weak<mpsc::Sender<Request>>,
}

impl Controls {
    pub fn channel() -> (Self, Inbox) {
        let (tx, rx) = mpsc::channel();
        let tx = Arc::new(tx);
        let inbox = Inbox {
            rx,
            #[cfg(target_os = "linux")]
            sender: Arc::downgrade(&tx),
        };
        (Self(tx), inbox)
    }

    pub fn send(&self, command: PlayerCommand) {
        if self
            .0
            .send(Request {
                command,
                #[cfg(target_os = "linux")]
                reply: None,
            })
            .is_err()
        {
            log::debug!("Playback worker has closed");
        }
    }
    #[cfg(target_os = "linux")]
    pub async fn execute(&self, command: PlayerCommand) -> Result<(), String> {
        let (tx, rx) = futures_channel::oneshot::channel();
        self.0
            .send(Request {
                command,
                reply: Some(tx),
            })
            .map_err(|_| "Playback worker closed")?;
        rx.await.map_err(|_| "Playback worker closed")?
    }
}
