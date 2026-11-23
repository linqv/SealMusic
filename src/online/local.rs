use std::time::Duration;

use serde_json::{Value, json};

use super::Script;
use crate::library::{
    Track,
    lyrics::parse_lyrics,
    metadata::{self, TrackDetails},
};

pub fn local_action(track: &Track, name: &str) -> Value {
    let path = track.local_path().expect("local source action");
    let seconds = track.duration.as_secs();
    let info = json!({
        "name": track.title, "singer": track.artist, "source": "local",
        "songmid": path.to_string_lossy(), "filePath": path.to_string_lossy(),
        "ext": path.extension().unwrap_or_default().to_string_lossy(),
        "interval": format!("{:02}:{:02}", seconds / 60, seconds % 60),
        "albumName": track.album, "albumId": "", "img": "", "types": [], "_types": {}, "typeUrl": {}
    });
    json!({"source": "local", "action": name, "info": {"type": (name != "musicUrl").then_some("music"), "musicInfo": info}})
}

async fn fetch_cover(url: &str) -> Option<image::RgbaImage> {
    const LIMIT: usize = 4 * 1024 * 1024;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(20))
        .build()
        .ok()?;
    let mut response = client.get(url).send().await.ok()?.error_for_status().ok()?;
    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > LIMIT - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    tokio::task::spawn_blocking(move || metadata::decode_network_cover(&bytes)).await.ok()?
}

pub async fn enhance(
    script: Script,
    track: &Track,
    mut details: TrackDetails,
    alert: lx_probe::AlertHandler,
) -> Option<TrackDetails> {
    let mut session = script.session(alert).await.ok()?;
    let mut changed = false;
    if details.lyrics.is_empty()
        && script.supports_local("lyric")
        && let Ok(result) = session.action(&local_action(track, "lyric")).await
    {
        // The existing timeline accepts multiple lines at the same timestamp.
        let lrc = ["lyric", "tlyric", "rlyric"]
            .iter()
            .filter_map(|key| result[key].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        details.lyrics = parse_lyrics(&lrc);
        changed = !details.lyrics.is_empty();
    }
    if details.cover.is_none()
        && script.supports_local("pic")
        && let Ok(url) = session.resolve(&local_action(track, "pic")).await
    {
        details.cover = fetch_cover(&url).await;
        changed |= details.cover.is_some();
    }
    changed.then_some(details)
}
