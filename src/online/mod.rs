mod local;
mod media;
use std::{collections::BTreeMap, io::Write, path::PathBuf, sync::Arc, time::Duration};

pub use local::{enhance, local_action};
pub use media::prepare;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    library::{Track, TrackId, TrackOrigin},
    slint_types::OnlineIssue,
};

pub type Failure = (OnlineIssue, String);
pub type Result<T> = std::result::Result<T, Failure>;

pub fn is_link(query: &str) -> bool {
    query.trim_start().get(..10).is_some_and(|prefix| prefix.eq_ignore_ascii_case("lxmusic://"))
}

pub(crate) fn sanitize_log(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(pos) = find_http_scheme(rest) {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '>' | ','))
            .unwrap_or(after.len());
        let token = &after[..end];
        if let Ok(url) = Url::parse(token) {
            out.push_str(&url.origin().ascii_serialization());
        } else {
            out.push_str("<redacted-url>");
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn find_http_scheme(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    bytes.windows(7).enumerate().find_map(|(i, w)| {
        if !s.is_char_boundary(i) {
            return None;
        }
        if w.eq_ignore_ascii_case(b"http://")
            || bytes.get(i..i + 8).is_some_and(|w8| w8.eq_ignore_ascii_case(b"https://"))
        {
            Some(i)
        } else {
            None
        }
    })
}

fn field_error(platform: &str, field: &str) -> Failure {
    (OnlineIssue::InvalidLink, format!("{platform} · {field}"))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OnlineTrack {
    // A JSON string keeps nulls, platform extensions and numeric IDs intact in TOML.
    music_info: String,
    pub quality: String,
}

impl OnlineTrack {
    pub fn parse(link: &str) -> Result<Self> {
        let invalid = || field_error("LX", "music/play?data");
        if link.len() > 256 * 1024 {
            return Err(invalid());
        }
        let url = Url::parse(link.trim()).map_err(|_| invalid())?;
        if url.scheme() != "lxmusic" || url.host_str() != Some("music") || url.path() != "/play" {
            return Err(invalid());
        }
        let (_, data) = url.query_pairs().find(|(k, _)| k == "data").ok_or_else(invalid)?;
        let info = serde_json::from_str(&data).map_err(|_| invalid())?;
        Self::from_info(info)
    }

    fn from_info(mut info: Value) -> Result<Self> {
        let source = info["source"].as_str().unwrap_or("");
        if !matches!(source, "kw" | "kg" | "tx" | "wy" | "mg") {
            return Err(field_error("LX", "source"));
        }
        let err = |key: &str| field_error(source, key);
        for key in ["name", "singer"] {
            if !info[key].is_string() || (key == "name" && info[key] == "") {
                return Err(err(key));
            }
        }
        if id_string(&info["songmid"]).is_none() {
            return Err(err("songmid"));
        }
        let required = match source {
            "kg" => Some("hash"),
            "tx" => Some("strMediaMid"),
            "mg" => Some("copyrightId"),
            _ => None,
        };
        if let Some(key) = required
            && info[key].as_str().is_none_or(str::is_empty)
        {
            return Err(err(key));
        }
        let mut by_quality = serde_json::Map::new();
        for entry in info["types"].as_array().ok_or_else(|| err("types"))? {
            let ty = entry["type"].as_str().ok_or_else(|| err("types[].type"))?;
            if source == "kg" && entry["hash"].as_str().is_none_or(str::is_empty) {
                return Err(err("types[].hash"));
            }
            by_quality.insert(ty.into(), entry.clone());
        }
        info["_types"] = Value::Object(by_quality);
        Ok(Self {
            music_info: info.to_string(),
            quality: String::new(),
        })
    }

    pub fn track(&self) -> Result<Arc<Track>> {
        let info: Value =
            serde_json::from_str(&self.music_info).map_err(|_| field_error("LX", "data"))?;
        let source = info["source"].as_str().ok_or_else(|| field_error("LX", "source"))?;
        let key = match source {
            "kg" => "hash",
            "mg" => "copyrightId",
            _ => "songmid",
        };
        let mut id = id_string(&info[key]).ok_or_else(|| field_error(source, key))?;
        if source == "kg" {
            id.make_ascii_lowercase();
        }
        let hash = Sha256::digest(format!("{source}\0{id}").as_bytes());
        let duration = info["interval"]
            .as_str()
            .and_then(|s| s.split_once(':'))
            .and_then(|(m, s)| m.parse::<u64>().ok()?.checked_mul(60)?.checked_add(s.parse().ok()?))
            .unwrap_or_default();
        Ok(Arc::new(Track::new(
            TrackId(format!("/dev/sealmusic/track/online_{hash:x}")),
            TrackOrigin::Online(self.clone()),
            info["name"].as_str().unwrap_or_default().into(),
            info["singer"].as_str().unwrap_or_default().into(),
            Duration::from_secs(duration),
        )))
    }

    fn info(&self) -> Value {
        serde_json::from_str(&self.music_info).unwrap_or(Value::Null)
    }

    pub fn qualities(&self, script: Option<&Script>) -> Vec<String> {
        let info = self.info();
        script
            .filter(|s| s.enabled)
            .and_then(|s| s.platforms.get(info["source"].as_str()?))
            .into_iter()
            .flatten()
            .filter(|q| info["_types"].get(q.as_str()).is_some())
            .cloned()
            .collect()
    }

    pub fn action(&self) -> Value {
        let info = self.info();
        json!({"source":info["source"],"action":"musicUrl","info":{"type":self.quality,"musicInfo":info}})
    }
}

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().into()),
        Value::Number(n) if n.is_u64() || n.is_i64() => Some(n.to_string()),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Script {
    pub id: String,
    pub enabled: bool,
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub local_actions: Vec<String>,
    pub platforms: BTreeMap<String, Vec<String>>,
}

fn scripts_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home::home_dir().unwrap_or_default().join(".local/share"))
        .join("sealmusic/sources")
}

impl Script {
    pub fn supports_local(&self, action: &str) -> bool {
        self.enabled && self.local_actions.iter().any(|a| a == action)
    }

    pub async fn session(&self, alert: lx_probe::AlertHandler) -> Result<lx_probe::Session> {
        let path = self.path()?;
        let raw = tokio::task::spawn_blocking(move || read_script(&path))
            .await
            .map_err(|_| (OnlineIssue::ScriptFile, String::new()))??;
        Self::initialize(raw, alert).await
    }

    async fn initialize(raw: String, alert: lx_probe::AlertHandler) -> Result<lx_probe::Session> {
        lx_probe::Session::initialize_with_alert(raw, "--lx-worker", alert).await.map_err(|e| {
            log::warn!("LX source initialization failed: {}", sanitize_log(&e.to_string()));
            (OnlineIssue::Initialization, String::new())
        })
    }

    pub fn path(&self) -> Result<PathBuf> {
        if self.id.len() != 64 || !self.id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err((OnlineIssue::ScriptFile, String::new()));
        }
        Ok(scripts_dir().join(format!("{}.js", self.id)))
    }

    pub fn summary(&self) -> String {
        let mut lines = vec![
            ["name", "version", "author"]
                .iter()
                .filter_map(|k| self.metadata.get(*k))
                .filter(|v| !v.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" · "),
        ];
        lines.extend(
            ["description", "homepage"]
                .iter()
                .filter_map(|k| self.metadata.get(*k))
                .filter(|v| !v.is_empty())
                .cloned(),
        );
        lines.extend(
            self.platforms.iter().map(|(platform, qs)| format!("{platform} · {}", qs.join(", "))),
        );
        if !self.local_actions.is_empty() {
            lines.push(format!("local · {}", self.local_actions.join(", ")));
        }
        lines.join("\n")
    }

    pub async fn import(
        path: PathBuf,
        alert: lx_probe::AlertHandler,
    ) -> Result<(Self, Option<lx_probe::Session>)> {
        let raw =
            tokio::task::spawn_blocking(move || read_script(&path)).await.map_err(|e| {
                log::warn!("LX source read task failed: {e}");
                (OnlineIssue::ScriptFile, String::new())
            })??;
        let id = format!("{:x}", Sha256::digest(raw.as_bytes()));
        let session = Self::initialize(raw.clone(), alert).await?;
        let local_actions: Vec<String> =
            serde_json::from_value(session.sources["local"]["actions"].clone()).unwrap_or_default();
        let metadata = serde_json::from_value(session.metadata.clone()).unwrap_or_default();
        let mut platforms = BTreeMap::new();
        if let Some(sources) = session.sources.as_object() {
            for (key, value) in sources {
                if value["actions"].as_array().is_some_and(|v| v.iter().any(|a| a == "musicUrl"))
                    && let Ok(qs) = serde_json::from_value::<Vec<String>>(value["qualitys"].clone())
                    && !qs.is_empty()
                {
                    platforms.insert(key.clone(), qs);
                }
            }
        }
        if platforms.is_empty() && local_actions.is_empty() {
            log::warn!("LX source declared no supported musicUrl platforms/qualities");
            return Err((OnlineIssue::NoQuality, String::new()));
        }
        let script = Self {
            id,
            enabled: true,
            metadata,
            platforms,
            local_actions,
        };
        let target = script.path()?;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let dir = scripts_dir();
            std::fs::create_dir_all(&dir)?;
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            file.write_all(raw.as_bytes())?;
            file.persist(target).map_err(|e| e.error)?;
            Ok(())
        })
        .await
        .map_err(|e| {
            log::warn!("LX source save task failed: {e}");
            (OnlineIssue::ScriptFile, String::new())
        })?
        .map_err(|e| {
            log::warn!("Could not save LX source: {e}");
            (OnlineIssue::ScriptFile, String::new())
        })?;
        let drain = session.has_pending().then_some(session);
        Ok((script, drain))
    }
}

fn read_script(path: &std::path::Path) -> Result<String> {
    if !path.is_file() {
        log::warn!("LX source path is not a regular file");
        return Err((OnlineIssue::ScriptFile, String::new()));
    }
    lx_probe::read_input(path).map_err(|e| {
        log::warn!("Could not read LX source: {e}");
        (OnlineIssue::ScriptFile, String::new())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link_for(payload: Value) -> String {
        let mut url = Url::parse("lxmusic://music/play").unwrap();
        url.query_pairs_mut().append_pair("data", &payload.to_string());
        url.into()
    }

    #[test]
    fn is_link_handles_whitespace_and_case() {
        assert!(is_link("lxmusic://music/play?data={}"));
        assert!(is_link("  \n\tLXMUSIC://music/play?data={}"));
        assert!(!is_link("https://example.com"));
        assert!(!is_link("lxmusic"));
        assert!(!is_link("中文搜索"));
    }

    #[test]
    fn sanitize_log_redacts_credentials_and_signed_queries() {
        let raw = "错误: https://user:secret@cdn.example.com/a/b.flac?token=xyz&expires=123, ok http://127.0.0.1:8080/song.mp3?sig=abc";
        let clean = sanitize_log(raw);
        assert!(!clean.contains("secret"));
        assert!(!clean.contains("token=xyz"));
        assert!(!clean.contains("sig=abc"));
        assert!(clean.contains("https://cdn.example.com"));
        assert!(clean.contains("http://127.0.0.1:8080"));
        assert!(clean.starts_with("错误: "));
    }

    #[test]
    fn parse_validates_platforms_and_normalizes_stable_ids() {
        let kg_upper = OnlineTrack::parse(&format!(
            "  {}  ",
            link_for(json!({
                "source": "kg",
                "name": "Song",
                "singer": "Artist",
                "songmid": 1002,
                "hash": "AABBCC",
                "types": [{"type": "128k", "hash": "112233"}],
            }))
        ))
        .unwrap();
        let kg_lower = OnlineTrack::parse(&link_for(json!({
            "source": "kg",
            "name": "Song Renamed",
            "singer": "Artist",
            "songmid": 9999,
            "hash": "aabbcc",
            "types": [{"type": "320k", "hash": "445566"}],
        })))
        .unwrap();
        assert_eq!(kg_upper.track().unwrap().id, kg_lower.track().unwrap().id);
        assert!(kg_upper.track().unwrap().id.0.starts_with("/dev/sealmusic/track/online_"));

        // Missing platform-specific required fields report platform · field.
        let missing_tx = OnlineTrack::parse(&link_for(json!({
            "source": "tx",
            "name": "Song",
            "singer": "Artist",
            "songmid": "mid",
            "types": [{"type": "128k"}],
        })))
        .unwrap_err();
        assert_eq!(missing_tx, (OnlineIssue::InvalidLink, "tx · strMediaMid".into()));

        let missing_mg = OnlineTrack::parse(&link_for(json!({
            "source": "mg",
            "name": "Song",
            "singer": "Artist",
            "songmid": "mid",
            "types": [{"type": "128k"}],
        })))
        .unwrap_err();
        assert_eq!(missing_mg, (OnlineIssue::InvalidLink, "mg · copyrightId".into()));
    }
}
