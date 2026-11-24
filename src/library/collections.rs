use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};

use super::Track;
use crate::config::{config_path, save_toml};

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Collection {
    pub name: String,
    #[serde(with = "saved_tracks")]
    pub tracks: Vec<Arc<Track>>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Collections {
    pub likes: Collection,
    pub custom: Vec<Collection>,
    #[serde(skip)]
    path: Option<PathBuf>,
    #[serde(skip)]
    load_failed: bool,
}

impl Collections {
    pub fn load() -> Self {
        let path = config_path().with_file_name("collections.toml");
        let loaded = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|err| err.to_string()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.to_string()),
        };
        let mut collections = loaded.unwrap_or_else(|err| {
            log::warn!("Reading collections: {err}");
            Self {
                load_failed: true,
                ..Self::default()
            }
        });
        collections.path = Some(path);
        collections
    }

    pub fn save(&self) -> bool {
        if self.load_failed {
            return false;
        }
        if let Some(path) = &self.path
            && let Err(err) = save_toml(path, self)
        {
            log::warn!("Saving collections: {err}");
            return false;
        }
        true
    }

    pub fn get(&self, index: usize) -> Option<&Collection> {
        if index == 0 {
            Some(&self.likes)
        } else {
            self.custom.get(index - 1)
        }
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Collection> {
        if index == 0 {
            Some(&mut self.likes)
        } else {
            self.custom.get_mut(index - 1)
        }
    }
}

mod saved_tracks {
    use std::{ffi::OsString, time::Duration};

    use serde::{Deserializer, Serializer};

    use super::*;
    use crate::library::{TrackId, TrackOrigin};

    #[derive(Deserialize, Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum SavedTrack {
        Local {
            path: OsString,
            title: String,
            artist: String,
            duration: Duration,
        },
        #[cfg(target_os = "linux")]
        Online {
            reference: crate::online::OnlineTrack,
        },
    }

    pub fn serialize<S: Serializer>(
        tracks: &[Arc<Track>],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(tracks.iter().map(|track| match &track.origin {
            TrackOrigin::Local(path) => SavedTrack::Local {
                path: path.as_os_str().to_owned(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                duration: track.duration,
            },
            #[cfg(target_os = "linux")]
            TrackOrigin::Online(reference) => SavedTrack::Online {
                reference: reference.clone(),
            },
        }))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<Arc<Track>>, D::Error> {
        let mut ids = std::collections::HashSet::new();
        Ok(Vec::<SavedTrack>::deserialize(deserializer)?
            .into_iter()
            .filter_map(|track| match track {
                SavedTrack::Local {
                    path,
                    title,
                    artist,
                    duration,
                } => {
                    let path = PathBuf::from(path);
                    Some(Arc::new(Track::new(
                        TrackId::local(&path),
                        TrackOrigin::Local(path),
                        title,
                        artist,
                        duration,
                    )))
                }
                #[cfg(target_os = "linux")]
                SavedTrack::Online {
                    reference,
                } => reference.track().ok(),
            })
            .filter(|track| ids.insert(track.id.clone()))
            .collect())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt, time::Duration};

    use super::*;
    use crate::library::{TrackId, TrackOrigin};

    #[test]
    fn collections_preserve_local_paths_and_online_references() {
        let path = PathBuf::from(OsString::from_vec(b"/music/non-utf8-\xff.flac".to_vec()));
        let local = Arc::new(Track::new(
            TrackId::local(&path),
            TrackOrigin::Local(path),
            "Local".into(),
            "Artist".into(),
            Duration::from_secs(65),
        ));
        let mut link = url::Url::parse("lxmusic://music/play").unwrap();
        link.query_pairs_mut().append_pair(
            "data",
            r#"{"source":"kw","songmid":"42","name":"Online","singer":"Artist","types":[{"type":"128k"}]}"#,
        );
        let online = crate::online::OnlineTrack::parse(link.as_str()).unwrap().track().unwrap();
        let mut collections = Collections::default();
        collections.likes.tracks.push(local.clone());
        collections.custom.push(Collection {
            name: "夜间".into(),
            tracks: vec![local.clone(), online.clone(), online.clone()],
        });

        let saved = toml::to_string(&collections).unwrap();
        let restored: Collections = toml::from_str(&saved).unwrap();
        assert_eq!(restored.likes.tracks, std::slice::from_ref(&local));
        assert_eq!(restored.custom[0].name, "夜间");
        assert_eq!(restored.custom[0].tracks, [local, online]);
    }
}
