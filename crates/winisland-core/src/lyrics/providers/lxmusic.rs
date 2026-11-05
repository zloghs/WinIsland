use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::{SongQuery, get_json_with_timeout};
use crate::lyrics::{LyricLine, LyricTiming, parse_lrc_timestamp, parse_lyrics};

/// The LX Music open API listens on localhost only and answers for whatever that player is
/// playing, so the whole source is a single request with a timeout short enough to fall back
/// quickly when the feature is off or another player owns the session.
const STATUS_URL: &str =
    "http://127.0.0.1:23330/status?filter=name,singer,duration,lyric,tlyric,lxlyric";
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn fetch(
    title: &str,
    artist: &str,
    duration_secs: u64,
    fuzzy: bool,
) -> Option<Arc<Vec<LyricLine>>> {
    let status = get_json_with_timeout(STATUS_URL, STATUS_TIMEOUT).await?;
    let name = status.get("name").and_then(Value::as_str)?.trim();
    let singer = status
        .get("singer")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Bounded so the seconds-to-whole-seconds conversion cannot saturate on garbage input.
    let duration = status
        .get("duration")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && (1.0..86_400.0).contains(value))
        .map(|value| value.round() as u64);

    // The player reports what it holds, not what the media session plays: with another app in
    // front the answer belongs to a different song, so identity has to be confirmed.
    SongQuery::new(title, artist, duration_secs, fuzzy).score(name, [singer], duration)?;

    let translation = status
        .get("tlyric")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let word_lyrics = status
        .get("lxlyric")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if let Some(lines) = parse_word_lyrics(word_lyrics, translation) {
        return Some(Arc::new(lines));
    }

    let lyric = status
        .get("lyric")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?;
    let lines = parse_lyrics(lyric, translation);
    (!lines.is_empty()).then(|| Arc::new(lines))
}

/// Parses LX Music word-synced lyrics, where every word carries its offset and duration relative
/// to the line: `[mm:ss.mmm]<offset_ms,duration_ms>text<offset_ms,duration_ms>text`.
///
/// Returns `None` when the payload has no word timing at all, so the caller can fall back to the
/// line-synced lyric.
fn parse_word_lyrics(word_lyrics: &str, translation: &str) -> Option<Vec<LyricLine>> {
    if !word_lyrics.contains('<') {
        return None;
    }
    let translations = parse_translation_lines(translation);
    let lines = word_lyrics
        .lines()
        .filter_map(|line| parse_word_line(line, &translations))
        .collect::<Vec<_>>();
    (!lines.is_empty()).then_some(lines)
}

/// Builds the translation lookup for word-synced lines.
///
/// `parse_lyrics` normalizes timestamps to centiseconds, which then no longer matches the
/// millisecond times the word-synced lines carry, so the timestamps are read here directly.
/// `//` is what LX Music reports for a line it holds no translation for.
fn parse_translation_lines(translation: &str) -> BTreeMap<u64, String> {
    let mut translations = BTreeMap::new();
    for line in translation.lines() {
        let Some(line) = line.trim().strip_prefix('[') else {
            continue;
        };
        let Some(header_end) = line.find(']') else {
            continue;
        };
        let Some(time_ms) = line.get(..header_end).and_then(parse_lrc_timestamp) else {
            continue;
        };
        let Some(text) = line.get(header_end + 1..).map(str::trim) else {
            continue;
        };
        if text.is_empty() || text == "//" {
            continue;
        }
        translations
            .entry(time_ms)
            .or_insert_with(|| text.to_string());
    }
    translations
}

fn parse_word_line(line: &str, translations: &BTreeMap<u64, String>) -> Option<LyricLine> {
    let line = line.trim().strip_prefix('[')?;
    let header_end = line.find(']')?;
    let time_ms = parse_lrc_timestamp(line.get(..header_end)?)?;
    let content = line.get(header_end + 1..)?;

    let mut tags = Vec::new();
    let mut search_from = 0;
    while let Some(relative_start) = content.get(search_from..)?.find('<') {
        let start = search_from + relative_start;
        let Some(relative_end) = content.get(start + 1..)?.find('>') else {
            break;
        };
        let end = start + relative_end + 2;
        if let Some((offset_ms, duration_ms)) = parse_word_timing(content.get(start + 1..end - 1)?)
        {
            tags.push((start, end, offset_ms, duration_ms));
        }
        search_from = end;
    }

    if tags.is_empty() {
        // `lxlyric` can mix word-synced lines with plain ones (spoken intros, credits). A line
        // without any timing tag is kept on the line timeline instead of being dropped; a line
        // that carries an unparsable tag is skipped so its markup never reaches the island.
        if content.contains('<') {
            return None;
        }
        let text = content.trim();
        if text.is_empty() {
            return None;
        }
        return Some(LyricLine {
            time_ms,
            text: text.to_owned(),
            secondary_text: translations.get(&time_ms).cloned(),
            font_family: None,
            timings: Vec::new(),
        });
    }

    let mut text = String::new();
    let mut timings = Vec::new();
    for (index, &(_, segment_start, offset_ms, duration_ms)) in tags.iter().enumerate() {
        let segment_end = tags
            .get(index + 1)
            .map_or(content.len(), |(start, _, _, _)| *start);
        let segment = content.get(segment_start..segment_end)?;
        if segment.is_empty() {
            continue;
        }
        let start_time_ms = time_ms.saturating_add(offset_ms);
        if timings
            .last()
            .is_some_and(|previous: &LyricTiming| previous.start_time_ms > start_time_ms)
        {
            continue;
        }
        text.push_str(segment);
        timings.push(LyricTiming {
            start_time_ms,
            // A zero duration marks a filler word that should stay lit until the next one starts.
            end_time_ms: (duration_ms > 0).then(|| start_time_ms.saturating_add(duration_ms)),
            end_byte: text.len(),
        });
    }

    if text.is_empty() || timings.is_empty() {
        return None;
    }
    Some(LyricLine {
        time_ms,
        text,
        secondary_text: translations.get(&time_ms).cloned(),
        font_family: None,
        timings,
    })
}

fn parse_word_timing(value: &str) -> Option<(u64, u64)> {
    let (offset_ms, duration_ms) = value.split_once(',')?;
    Some((
        offset_ms.trim().parse().ok()?,
        duration_ms.trim().parse().ok()?,
    ))
}
