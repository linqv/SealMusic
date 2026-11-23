use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rayon::prelude::*;

use super::{Track, metadata::read_meta_info};
use crate::slint_types::SortKey;

fn audio_paths(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    Ok(dir
        .read_dir()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                ["mp3", "flac", "wav", "ogg"].iter().any(|format| e.eq_ignore_ascii_case(format))
            })
        })
        .collect())
}
fn read_track(path: &Path) -> Option<Arc<Track>> {
    match read_meta_info(path) {
        Ok(track) => Some(Arc::new(track)),
        Err(err) => {
            log::warn!("{}: {err}", path.display());
            None
        }
    }
}
pub fn scan(dir: &Path) -> std::io::Result<Vec<Arc<Track>>> {
    let paths = audio_paths(dir)?;
    if paths.len() < 32 {
        return Ok(paths.iter().filter_map(|p| read_track(p)).collect());
    }
    Ok(paths.par_iter().filter_map(|p| read_track(p)).collect())
}

pub fn sort(tracks: &mut [Arc<Track>], key: SortKey, ascending: bool) {
    tracks.sort_unstable_by(|a, b| {
        let order = match key {
            SortKey::BySongName => a.title.cmp(&b.title),
            SortKey::BySinger => a.artist.cmp(&b.artist),
            SortKey::ByDuration => a.duration.cmp(&b.duration),
        }
        .then_with(|| a.id.0.cmp(&b.id.0));
        if ascending {
            order
        } else {
            order.reverse()
        }
    });
}

pub fn search(query: &str, tracks: &[Arc<Track>]) -> Vec<Arc<Track>> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return vec![];
    }
    let score = |value: &str, exact, prefix, partial| {
        let value = value.to_lowercase();
        if value == query {
            exact
        } else if value.starts_with(&query) {
            prefix
        } else if value.contains(&query) {
            partial
        } else {
            0
        }
    };
    let mut hits: Vec<_> = tracks
        .iter()
        .filter_map(|t| {
            let rank = score(&t.title, 100, 80, 60) + score(&t.artist, 50, 40, 20);
            (rank > 0).then(|| (std::cmp::Reverse(rank), t.clone()))
        })
        .collect();
    hits.sort_by_key(|(rank, _)| *rank);
    hits.into_iter().map(|(_, t)| t).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::library::{TrackId, TrackOrigin};

    fn make_track(id: &str, title: &str, artist: &str, secs: u64) -> Arc<Track> {
        Arc::new(Track {
            id: TrackId(id.into()),
            origin: TrackOrigin::Local(PathBuf::from(format!("/music/{id}.flac"))),
            title: title.into(),
            artist: artist.into(),
            duration: Duration::from_secs(secs),
        })
    }

    #[test]
    fn sort_uses_plain_string_order_and_breaks_ties_by_track_id() {
        let mut tracks = vec![
            make_track("b", "Same", "Zed", 30),
            make_track("c", "啊", "Alpha", 10),
            make_track("d", "中", "Beta", 20),
            make_track("a", "Same", "Alpha", 40),
        ];

        sort(&mut tracks, SortKey::BySongName, true);
        let ids: Vec<_> = tracks.iter().map(|t| t.id.0.as_str()).collect();
        // Plain codepoint order: 'S' < '中' (U+4E2D) < '啊' (U+554A), never pinyin ('啊' before '中').
        assert_eq!(ids, ["a", "b", "d", "c"]);

        sort(&mut tracks, SortKey::BySongName, false);
        let ids_desc: Vec<_> = tracks.iter().map(|t| t.id.0.as_str()).collect();
        assert_eq!(ids_desc, ["c", "d", "b", "a"]);

        sort(&mut tracks, SortKey::BySinger, true);
        let artists: Vec<_> = tracks.iter().map(|t| t.id.0.as_str()).collect();
        assert_eq!(artists, ["a", "c", "d", "b"]);

        sort(&mut tracks, SortKey::ByDuration, true);
        let durations: Vec<_> = tracks.iter().map(|t| t.duration.as_secs()).collect();
        assert_eq!(durations, [10, 20, 30, 40]);
    }

    #[test]
    fn search_ranks_title_matches_above_artist_matches() {
        let tracks = vec![
            make_track("artist", "Other", "Alpha", 10),
            make_track("partial", "My Alpha Song", "Someone", 10),
            make_track("prefix", "Alphabet", "Someone", 10),
            make_track("exact", "alpha", "Someone", 10),
            make_track("miss", "Beta", "Gamma", 10),
        ];

        assert!(search("", &tracks).is_empty());

        let hits: Vec<_> = search("ALPHA", &tracks).into_iter().map(|t| t.id.0.clone()).collect();
        assert_eq!(hits, ["exact", "prefix", "partial", "artist"]);
    }
}

#[cfg(test)]
mod benchmarks {
    use std::time::Instant;

    use super::*;
    /// Run on an explicitly selected, read-only fixture library.
    #[test]
    #[ignore = "set SEALMUSIC_BENCH_DIR to a directory of audio files"]
    fn measure_library() {
        let dir =
            PathBuf::from(std::env::var_os("SEALMUSIC_BENCH_DIR").expect("set SEALMUSIC_BENCH_DIR"));
        let paths = audio_paths(&dir).unwrap();
        for run in 0..6 {
            for parallel in [run % 2 == 0, run % 2 != 0] {
                let start = Instant::now();
                let mut songs: Vec<_> = if parallel {
                    paths.par_iter().filter_map(|p| read_track(p)).collect()
                } else {
                    paths.iter().filter_map(|p| read_track(p)).collect()
                };
                let read = start.elapsed();
                sort(&mut songs, SortKey::BySongName, true);
                let sorted = start.elapsed();
                std::hint::black_box(search("0", &songs));
                println!(
                    "run={run} songs={} parallel={parallel} read={read:?} sort={:?} search={:?}",
                    songs.len(),
                    sorted - read,
                    start.elapsed() - sorted
                );
            }
        }
    }
}
