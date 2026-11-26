use std::{
    io,
    path::PathBuf,
    sync::{Arc, Weak, mpsc},
    thread,
};

use super::command::{PlayerCommand, Request};
use crate::library::{
    Track, TrackId,
    metadata::{self, CoverSource, TrackDetails},
    scan,
};

pub struct Scan {
    pub id: u64,
    pub dir: PathBuf,
    pub startup: bool,
}

pub enum Details {
    Read(u64, TrackId, PathBuf),
    Restore(TrackId, Arc<CoverSource>),
}

pub enum Completion {
    Scan(Scan, io::Result<Vec<Arc<Track>>>),
    LocalDetails(u64, TrackId, TrackDetails),
    RestoredCover(TrackId, Arc<CoverSource>, TrackDetails),
}

pub(super) struct MediaJobs {
    pub scan: mpsc::Sender<Scan>,
    pub details: mpsc::Sender<Details>,
}

impl MediaJobs {
    pub fn new(controls: Weak<mpsc::Sender<Request>>) -> Self {
        Self {
            scan: worker(controls.clone(), |scan: Scan| {
                let tracks = scan::scan(&scan.dir);
                crate::trim_allocator();
                Completion::Scan(scan, tracks)
            }),
            details: worker(controls, |details| match details {
                Details::Read(id, track, path) => {
                    Completion::LocalDetails(id, track, metadata::read_details(&path))
                }
                Details::Restore(track, source) => {
                    let details = source.render();
                    Completion::RestoredCover(track, source, details)
                }
            }),
        }
    }
}

fn worker<T: Send + 'static>(
    controls: Weak<mpsc::Sender<Request>>,
    run: impl Fn(T) -> Completion + Send + 'static,
) -> mpsc::Sender<T> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(task) = receiver.recv() {
            // Only the most recent request in this queue can still be useful.
            let task = receiver.try_iter().last().unwrap_or(task);
            let done = run(task);
            let Some(controls) = controls.upgrade() else {
                break;
            };
            if controls
                .send(Request {
                    command: PlayerCommand::MediaDone(done),
                    #[cfg(target_os = "linux")]
                    reply: None,
                })
                .is_err()
            {
                break;
            }
        }
    });
    sender
}
