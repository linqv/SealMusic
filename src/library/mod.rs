pub mod lyrics;
pub mod metadata;
pub mod scan;

use std::{
    fmt::Write,
    path::{Path, PathBuf},
    time::Duration,
};

/// Independent of queue order and of the lossy display spelling of an OS path.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TrackId(pub String);

impl TrackId {
    pub fn local(path: &Path) -> Self {
        let mut id = String::from("/dev/sealmusic/track/local_");
        for byte in path.as_os_str().as_encoded_bytes() {
            write!(id, "{byte:02x}").unwrap();
        }
        Self(id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TrackOrigin {
    Local(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub id: TrackId,
    pub origin: TrackOrigin,
    pub title: String,
    pub artist: String,
    pub duration: Duration,
}

impl Track {
    pub fn path(&self) -> &Path {
        match &self.origin {
            TrackOrigin::Local(path) => path,
        }
    }
}
