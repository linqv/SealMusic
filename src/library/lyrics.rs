use std::path::Path;

const LAST_LINE_SENTINEL_DURATION: f32 = 100.0;
const SAME_TIMESTAMP_EPSILON: f32 = 0.005;

#[derive(Clone, Debug, PartialEq)]
pub struct LyricWord {
    pub time: f32,
    pub duration: f32,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LyricLine {
    pub time: f32,
    pub duration: f32,
    pub text: String,
    pub words: Vec<LyricWord>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayWord {
    pub time: f32,
    pub duration: f32,
    pub text: String,
    pub space_after: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayLyricLine {
    pub time: f32,
    pub end_time: f32,
    pub text: String,
    pub translation: String,
    pub words: Vec<DisplayWord>,
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

fn parse_stamp_seconds(stamp: &str) -> Option<f32> {
    let (minutes, seconds) = stamp.split_once(':')?;
    let (Ok(m), Ok(s)) = (minutes.parse::<u32>(), seconds.parse::<f32>()) else {
        return None;
    };
    (0.0..60.0).contains(&s).then_some(m as f32 * 60.0 + s)
}

/// Parses enhanced LRC inline word timestamps (`<mm:ss.xx>word`), returning the
/// clean joined text and any extracted word spans (durations finalized later).
fn parse_inline_words(raw: &str, line_time: f32) -> (String, Vec<LyricWord>) {
    if !raw.contains('<') {
        return (raw.trim().into(), Vec::new());
    }

    let mut plain = String::with_capacity(raw.len());
    let mut words: Vec<LyricWord> = Vec::new();
    let mut cur_start: Option<f32> = None;
    let mut cur_text = String::new();
    let mut found_tag = false;
    let mut rest = raw;

    while let Some(open) = rest.find('<') {
        let before = &rest[..open];
        let after = &rest[open + 1..];
        if let Some((tag, tail)) = after.split_once('>')
            && let Some(ts) = parse_stamp_seconds(tag)
        {
            if !before.is_empty() {
                plain.push_str(before);
                if !found_tag {
                    cur_start = Some(line_time);
                }
                cur_text.push_str(before);
            }
            if let Some(start) = cur_start.take()
                && !cur_text.is_empty()
            {
                words.push(LyricWord {
                    time: start,
                    duration: (ts - start).max(0.0),
                    text: std::mem::take(&mut cur_text),
                });
            }
            found_tag = true;
            cur_start = Some(ts);
            rest = tail;
            continue;
        }
        plain.push_str(before);
        plain.push('<');
        cur_text.push_str(before);
        cur_text.push('<');
        rest = after;
    }

    if !rest.is_empty() {
        plain.push_str(rest);
        cur_text.push_str(rest);
    }

    if !found_tag {
        return (raw.trim().into(), Vec::new());
    }

    if let Some(start) = cur_start
        && !cur_text.is_empty()
    {
        words.push(LyricWord {
            time: start,
            duration: 0.0,
            text: cur_text,
        });
    }

    (plain.trim().into(), words)
}

pub fn parse_lyrics(text: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for line in text.trim_start_matches('\u{feff}').lines() {
        let mut times = Vec::new();
        let mut rest = line.trim();
        while let Some((stamp, tail)) = rest.strip_prefix('[').and_then(|s| s.split_once(']')) {
            let Some(secs) = parse_stamp_seconds(stamp) else {
                break;
            };
            times.push(secs);
            rest = tail;
        }
        if times.is_empty() {
            continue;
        }
        let trimmed = rest.trim();
        if trimmed.is_empty() {
            continue;
        }
        let first_time = times[0];
        let (clean, base_words) = parse_inline_words(trimmed, first_time);
        if clean.is_empty() {
            continue;
        }
        let multi = times.len() > 1;
        lines.extend(times.into_iter().map(|time| {
            let words = if base_words.is_empty() {
                Vec::new()
            } else if multi {
                let shift = time - first_time;
                base_words
                    .iter()
                    .map(|w| LyricWord {
                        time: (w.time + shift).max(0.0),
                        duration: w.duration,
                        text: w.text.clone(),
                    })
                    .collect()
            } else {
                base_words.clone()
            };
            LyricLine {
                time,
                duration: LAST_LINE_SENTINEL_DURATION,
                text: clean.clone(),
                words,
            }
        }));
    }
    lines.sort_by(|a, b| a.time.total_cmp(&b.time));
    for i in 1..lines.len() {
        lines[i - 1].duration = lines[i].time - lines[i - 1].time;
    }
    finalize_inline_word_durations(&mut lines);
    lines
}

fn finalize_inline_word_durations(lines: &mut [LyricLine]) {
    for line in lines {
        if let Some(last) = line.words.last_mut()
            && last.duration <= 0.0
        {
            let line_end = line.time + effective_sweep_duration(&line.text, line.duration);
            last.duration = (line_end - last.time).clamp(0.15, 2.5);
        }
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3040..=0x30FF // Hiragana & Katakana
            | 0x3400..=0x4DBF // CJK Ext A
            | 0x4E00..=0x9FFF // CJK Unified
            | 0xAC00..=0xD7AF // Hangul
            | 0xF900..=0xFAFF // CJK Compatibility
    )
}

fn is_attached_trailing_punct(ch: char) -> bool {
    matches!(
        ch,
        ',' | '.'
            | '!'
            | '?'
            | ';'
            | ':'
            | ')'
            | ']'
            | '}'
            | '"'
            | '\''
            | '’'
            | '”'
            | '，'
            | '。'
            | '！'
            | '？'
            | '、'
            | '；'
            | '：'
            | '）'
            | '】'
            | '》'
            | '」'
            | '』'
            | '…'
            | '—'
            | '~'
            | '～'
    )
}

fn char_weight(ch: char) -> f32 {
    if ch.is_whitespace() {
        0.35
    } else if is_cjk(ch) {
        1.6
    } else if ch.is_ascii_punctuation() || is_attached_trailing_punct(ch) {
        0.4
    } else {
        1.0
    }
}

fn text_weight(text: &str) -> f32 {
    text.chars().map(char_weight).sum::<f32>().max(0.5)
}

fn effective_sweep_duration(text: &str, raw_duration: f32) -> f32 {
    let estimated = (text_weight(text) * 0.24).clamp(1.8, 9.0);
    if !raw_duration.is_finite() || raw_duration <= 0.0 {
        return estimated;
    }
    if raw_duration >= LAST_LINE_SENTINEL_DURATION - 0.5 || raw_duration > 14.0 {
        return estimated.min(raw_duration);
    }
    (raw_duration * 0.94).max(0.25)
}

fn normalize_timed_words(words: &[LyricWord]) -> Vec<DisplayWord> {
    let mut out: Vec<DisplayWord> = Vec::new();
    for word in words {
        let has_leading = word.text.chars().next().is_some_and(char::is_whitespace);
        let has_trailing = word.text.chars().last().is_some_and(char::is_whitespace);
        let trimmed = word.text.trim();
        if has_leading && let Some(prev) = out.last_mut() {
            prev.space_after = true;
        }
        if trimmed.is_empty() {
            if let Some(prev) = out.last_mut() {
                prev.space_after = true;
            }
            continue;
        }
        out.push(DisplayWord {
            time: word.time,
            duration: word.duration.max(0.05),
            text: trimmed.to_owned(),
            space_after: has_trailing,
        });
    }
    if let Some(last) = out.last_mut() {
        last.space_after = false;
    }
    out
}

/// Groups consecutive lines that share a timestamp into primary + translation
/// pairs and keeps explicit word-level sweep timings intact while leaving plain
/// line-synced LRC unsplit.
pub fn group_for_display(lines: &[LyricLine], track_duration: f32) -> Vec<DisplayLyricLine> {
    struct RawGroup<'a> {
        time: f32,
        explicit_end: Option<f32>,
        text: &'a str,
        translation: String,
        words: &'a [LyricWord],
    }

    let mut groups: Vec<RawGroup<'_>> = Vec::new();
    for line in lines {
        let text = line.text.trim();
        if text.is_empty() || !line.time.is_finite() || line.time < 0.0 {
            continue;
        }
        if let Some(last) = groups.last_mut()
            && (line.time - last.time).abs() < SAME_TIMESTAMP_EPSILON
        {
            if text != last.text && !last.translation.split(" / ").any(|part| part == text) {
                if !last.translation.is_empty() {
                    last.translation.push_str(" / ");
                }
                last.translation.push_str(text);
            }
            if last.words.is_empty() && !line.words.is_empty() {
                last.words = &line.words;
            }
            continue;
        }
        let explicit_end = if !line.words.is_empty() {
            let word_end = line.words.iter().map(|w| w.time + w.duration).fold(line.time, f32::max);
            let line_end =
                if line.duration > 0.0 && line.duration < LAST_LINE_SENTINEL_DURATION - 0.5 {
                    line.time + line.duration
                } else {
                    word_end
                };
            Some(word_end.max(line_end))
        } else {
            None
        };
        groups.push(RawGroup {
            time: line.time,
            explicit_end,
            text,
            translation: String::new(),
            words: &line.words,
        });
    }

    let mut result = Vec::with_capacity(groups.len());
    for i in 0..groups.len() {
        let g = &groups[i];
        let next_start = groups.get(i + 1).map(|next| next.time);
        let gap = match next_start {
            Some(next) if next > g.time => next - g.time,
            _ if track_duration > g.time => track_duration - g.time,
            _ => LAST_LINE_SENTINEL_DURATION,
        };
        let end_time = match g.explicit_end {
            Some(exp) if exp > g.time => match next_start {
                Some(next) if next > g.time => exp.min(next),
                _ => exp,
            },
            _ => g.time + gap,
        };
        let words = if !g.words.is_empty() {
            normalize_timed_words(g.words)
        } else {
            Vec::new()
        };
        result.push(DisplayLyricLine {
            time: g.time,
            end_time: end_time.max(g.time + 0.1),
            text: g.text.to_owned(),
            translation: g.translation.clone(),
            words,
        });
    }
    result
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
    fn parses_enhanced_lrc_inline_word_timestamps_and_preserves_literal_angles() {
        let lines = parse_lyrics(
            "[00:01.00]<00:01.00>what <00:01.50>is <00:02.00>real?\n\
             [00:03.00]love <3 you\n\
             [00:05.00]lead <00:05.50>tail",
        );
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "what is real?");
        assert_eq!(lines[0].words.len(), 3);
        assert_eq!((lines[0].words[0].text.as_str(), lines[0].words[0].time), ("what ", 1.0));
        assert!((lines[0].words[0].duration - 0.5).abs() < 1e-4);
        assert_eq!((lines[0].words[2].text.as_str(), lines[0].words[2].time), ("real?", 2.0));
        assert!(lines[0].words[2].duration > 0.1);

        assert_eq!(lines[1].text, "love <3 you");
        assert!(lines[1].words.is_empty());

        assert_eq!(lines[2].text, "lead tail");
        assert_eq!(lines[2].words.len(), 2);
        assert_eq!((lines[2].words[0].text.as_str(), lines[2].words[0].time), ("lead ", 5.0));
        assert_eq!((lines[2].words[1].text.as_str(), lines[2].words[1].time), ("tail", 5.5));
    }

    #[test]
    fn groups_bilingual_lines_without_splitting_plain_lrc_and_keeps_timed_words() {
        let lines = parse_lyrics(
            "[00:01.54]夢ならば\n\
             [00:01.54]如果只是一场梦\n\
             [00:04.00]<00:04.00>was <00:04.50>heißt <00:05.00>Wirklichkeit?\n\
             [00:08.00]also,sehen wir vom Bildschirm auf",
        );
        let display = group_for_display(&lines, 180.0);
        assert_eq!(display.len(), 3);
        assert_eq!(display[0].text, "夢ならば");
        assert_eq!(display[0].translation, "如果只是一场梦");
        assert!((display[0].end_time - 4.0).abs() < 1e-3);
        assert!(display[0].words.is_empty());

        assert_eq!(display[1].text, "was heißt Wirklichkeit?");
        let tokens: Vec<_> =
            display[1].words.iter().map(|w| (w.text.as_str(), w.space_after)).collect();
        assert_eq!(tokens, vec![("was", true), ("heißt", true), ("Wirklichkeit?", false)]);
        for pair in display[1].words.windows(2) {
            assert!(pair[1].time >= pair[0].time);
        }

        // Plain last line keeps words empty and extends to track duration.
        assert!(display[2].words.is_empty());
        assert!((display[2].end_time - 180.0).abs() < 1e-3);
    }

    #[test]
    fn read_lyrics_prefers_embedded_and_falls_back_to_sidecar_lrc() {
        let base =
            std::env::temp_dir().join(format!("sealmusic_lyrics_test_{}", std::process::id()));
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
