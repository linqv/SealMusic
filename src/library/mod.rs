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
        const PREFIX: &str = "/dev/sealmusic/track/local_";
        const FNV_OFFSET_BASIS: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
        const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;

        let mut hash = FNV_OFFSET_BASIS;
        for &byte in path.as_os_str().as_encoded_bytes() {
            hash ^= u128::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        let mut id = String::with_capacity(PREFIX.len() + 32);
        id.push_str(PREFIX);
        write!(id, "{hash:032x}").unwrap();
        Self(id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TrackOrigin {
    Local(PathBuf),
    #[cfg(target_os = "linux")]
    Online(crate::online::OnlineTrack),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub id: TrackId,
    pub origin: TrackOrigin,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub title_lower: String,
    pub artist_lower: String,
    pub duration: Duration,
}

impl Track {
    pub fn new(
        id: TrackId,
        origin: TrackOrigin,
        title: String,
        artist: String,
        duration: Duration,
    ) -> Self {
        let title_lower = title.to_lowercase();
        let artist_lower = artist.to_lowercase();
        Self {
            id,
            origin,
            title,
            artist,
            title_lower,
            artist_lower,
            album: String::new(),
            duration,
        }
    }

    pub fn local_path(&self) -> Option<&Path> {
        match &self.origin {
            TrackOrigin::Local(path) => Some(path),
            #[cfg(target_os = "linux")]
            TrackOrigin::Online(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_track_id_is_deterministic_distinct_and_compact() {
        let long_path = PathBuf::from(format!("/music/{}/测试歌曲.flac", "目录".repeat(40)));
        let id_a = TrackId::local(&long_path);
        let id_b = TrackId::local(&long_path);
        let id_c = TrackId::local(Path::new("/music/other.flac"));

        assert_eq!(id_a, id_b);
        assert_ne!(id_a, id_c);
        assert!(id_a.0.starts_with("/dev/sealmusic/track/local_"));
        assert_eq!(id_a.0.len(), "/dev/sealmusic/track/local_".len() + 32);
    }
}
