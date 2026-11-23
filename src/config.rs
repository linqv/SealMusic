use std::path::PathBuf;

use crate::slint_types::{LoopMode, SortKey};

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home::home_dir().unwrap_or_default().join(".config"))
        .join("sealmusic/config.toml")
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct Config {
    pub song_dir: PathBuf,
    pub current_song_path: Option<PathBuf>,
    pub progress: f32,
    pub loop_mode: LoopMode,
    pub shuffle: bool,
    pub sort_key: SortKey,
    pub sort_ascending: bool,
    pub lang: String,
    pub light_ui: bool,
    pub follow_system_theme: bool,
    pub volume: f32,
    pub current_page: i32,
    pub sidebar_expanded: bool,
    pub gallery_viewport_y: f32,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            song_dir: home::home_dir().unwrap_or_default().join("Music"),
            current_song_path: None,
            progress: 0.,
            loop_mode: LoopMode::Playlist,
            shuffle: false,
            sort_key: SortKey::BySongName,
            sort_ascending: true,
            lang: String::new(),
            light_ui: false,
            follow_system_theme: true,
            volume: 1.,
            current_page: 0,
            sidebar_expanded: false,
            gallery_viewport_y: 0.,
        }
    }
}

impl Config {
    fn parse(text: &str) -> Result<Self, toml::de::Error> {
        let table: toml::Table = toml::from_str(text)?;
        let mut cfg: Self = table.clone().try_into()?;
        if let Some(mode) = table.get("play_mode").and_then(|v| v.as_str()) {
            if !table.contains_key("loop_mode") {
                cfg.loop_mode = if mode == "Recursive" {
                    LoopMode::Track
                } else {
                    LoopMode::Playlist
                };
            }
            if !table.contains_key("shuffle") {
                cfg.shuffle = mode == "Random";
            }
        }
        if cfg.lang != "zh_CN" {
            cfg.lang.clear();
        }
        if !cfg.volume.is_finite() {
            cfg.volume = 1.;
        }
        cfg.volume = cfg.volume.clamp(0., 1.);
        if !cfg.progress.is_finite() || cfg.progress < 0. {
            cfg.progress = 0.;
        }
        cfg.current_page = cfg.current_page.clamp(0, 3);
        Ok(cfg)
    }
    pub fn load() -> Self {
        match std::fs::read_to_string(config_path()) {
            Ok(s) => Self::parse(&s).unwrap_or_else(|e| {
                log::warn!("Invalid configuration: {e}");
                Self::default()
            }),
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("Reading configuration: {e}");
                }
                Self::default()
            }
        }
    }
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = config_path();
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir)?;
        let temp = dir.join("config.tmp");
        std::fs::write(&temp, toml::to_string_pretty(self)?)?;
        std::fs::rename(temp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn imports_legacy_modes_and_about_without_overwriting_other_fields() {
        for (mode, looping, shuffle) in [
            ("Recursive", LoopMode::Track, false),
            ("Random", LoopMode::Playlist, true),
            ("InOrder", LoopMode::Playlist, false),
        ] {
            let cfg = Config::parse(&format!(
                "play_mode='{mode}'\ncurrent_page=4\nvolume=0.42\nlang='fr'\nshow_spectrum=true"
            ))
            .unwrap();
            assert_eq!(
                (cfg.loop_mode, cfg.shuffle, cfg.current_page, cfg.volume, cfg.lang.as_str()),
                (looping, shuffle, 3, 0.42, "")
            );
        }
    }
}
