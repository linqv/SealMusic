use std::path::Path;

#[derive(Clone, Debug)]
pub struct LyricLine {
    pub time: f32,
    pub duration: f32,
    pub text: String,
}

pub fn read_lyrics(path: &Path, embedded: Option<&str>) -> Vec<LyricLine> {
    let lines = parse_lyrics(embedded.unwrap_or_default());
    if !lines.is_empty() {
        return lines;
    }
    std::fs::read_to_string(path.with_extension("lrc"))
        .map(|s| parse_lyrics(&s))
        .unwrap_or_default()
}

pub fn parse_lyrics(text: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for line in text.trim_start_matches('\u{feff}').lines() {
        let mut times = Vec::new();
        let mut rest = line.trim();
        while let Some((stamp, tail)) = rest.strip_prefix('[').and_then(|s| s.split_once(']')) {
            let Some((minutes, seconds)) = stamp.split_once(':') else {
                break;
            };
            let (Ok(m), Ok(s)) = (minutes.parse::<u32>(), seconds.parse::<f32>()) else {
                break;
            };
            if !(0.0..60.0).contains(&s) {
                break;
            }
            times.push(m as f32 * 60.0 + s);
            rest = tail;
        }
        if !rest.trim().is_empty() {
            lines.extend(times.into_iter().map(|time| LyricLine {
                time,
                duration: 100.0,
                text: rest.trim().into(),
            }));
        }
    }
    lines.sort_by(|a, b| a.time.total_cmp(&b.time));
    for i in 1..lines.len() {
        lines[i - 1].duration = lines[i].time - lines[i - 1].time;
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct TempCleanup(Vec<PathBuf>);

    impl Drop for TempCleanup {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    #[test]
    fn parses_bom_zero_and_repeated_stamps_in_time_order() {
        let lines = parse_lyrics("\u{feff}[00:03][00:00] First\n[ar:artist]\n[00:01.25]Middle");
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[0].time, lines[0].duration, lines[0].text.as_str()), (0., 1.25, "First"));
        assert_eq!(lines[2].time, 3.);
    }

    #[test]
    fn read_lyrics_prefers_embedded_and_falls_back_to_sidecar_lrc() {
        let base = std::env::temp_dir().join(format!("sealmusic_lyrics_test_{}", std::process::id()));
        let audio_path = base.with_extension("flac");
        let lrc_path = base.with_extension("lrc");
        let _cleanup = TempCleanup(vec![lrc_path.clone()]);

        std::fs::write(&lrc_path, "[00:05.00]Sidecar line\n").unwrap();

        let embedded_first = read_lyrics(&audio_path, Some("[00:01.00]Embedded line"));
        assert_eq!(embedded_first.len(), 1);
        assert_eq!(
            (embedded_first[0].time, embedded_first[0].text.as_str()),
            (1., "Embedded line")
        );

        let fallback_none = read_lyrics(&audio_path, None);
        assert_eq!(fallback_none.len(), 1);
        assert_eq!((fallback_none[0].time, fallback_none[0].text.as_str()), (5., "Sidecar line"));

        let fallback_untimed = read_lyrics(&audio_path, Some("Untimed lyrics text"));
        assert_eq!(fallback_untimed.len(), 1);
        assert_eq!(fallback_untimed[0].text, "Sidecar line");
    }
}
