use std::{path::PathBuf, sync::mpsc, time::Duration};

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
pub struct Controls(pub mpsc::Sender<Request>);

impl Controls {
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
