use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use encoding_rs::GBK;
use fuzzengine::{PreprocessingOptions, partial_ratio, partial_token_set_ratio};
use lrc::Lyrics;

mod providers;

#[derive(Clone, Default, Debug)]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
    pub secondary_text: Option<String>,
    timings: Vec<LyricTiming>,
}

#[derive(Clone, Debug)]
struct LyricTiming {
    start_time_ms: u64,
    end_time_ms: Option<u64>,
    end_byte: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LyricHighlight {
    pub start_byte: usize,
    pub end_byte: usize,
    pub progress: f32,
}

impl LyricLine {
    pub fn is_word_synced(&self) -> bool {
        !self.timings.is_empty()
    }

    pub fn replace_text_preserving_timings(&mut self, text: String) -> bool {
        if self.timings.is_empty() {
            self.text = text;
            return true;
        }

        let boundaries = self
            .timings
            .iter()
            .map(|timing| {
                self.text
                    .get(..timing.end_byte)
                    .map(|prefix| prefix.chars().count())
            })
            .collect::<Option<Vec<_>>>();
        let Some(boundaries) = boundaries else {
            return false;
        };
        let byte_offsets = text
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(text.len()))
            .collect::<Vec<_>>();
        if byte_offsets.len() != self.text.chars().count() + 1 {
            return false;
        }
        for (timing, boundary) in self.timings.iter_mut().zip(boundaries) {
            let Some(&end_byte) = byte_offsets.get(boundary) else {
                return false;
            };
            timing.end_byte = end_byte;
        }
        self.text = text;
        true
    }

    pub fn highlight_at(
        &self,
        position_ms: u64,
        next_line_time_ms: Option<u64>,
    ) -> Option<LyricHighlight> {
        if self.timings.is_empty() {
            return None;
        }

        let started = self
            .timings
            .partition_point(|timing| timing.start_time_ms <= position_ms);
        let index = started.saturating_sub(1);
        let timing = &self.timings[index];
        let start_byte = index
            .checked_sub(1)
            .map_or(0, |previous| self.timings[previous].end_byte);
        let end_time_ms = timing
            .end_time_ms
            .or_else(|| self.timings.get(index + 1).map(|next| next.start_time_ms))
            .unwrap_or_else(|| {
                let previous_duration = index
                    .checked_sub(1)
                    .map(|previous| {
                        timing
                            .start_time_ms
                            .saturating_sub(self.timings[previous].start_time_ms)
                    })
                    .filter(|duration| *duration > 0)
                    .unwrap_or(400)
                    .clamp(80, 1000);
                let estimated_end = timing.start_time_ms.saturating_add(previous_duration);
                next_line_time_ms
                    .filter(|next| *next > timing.start_time_ms)
                    .map_or(estimated_end, |next| next.min(estimated_end))
            })
            .max(timing.start_time_ms.saturating_add(1));
        let progress = if started == 0 {
            0.0
        } else {
            position_ms
                .saturating_sub(timing.start_time_ms)
                .min(end_time_ms - timing.start_time_ms) as f32
                / (end_time_ms - timing.start_time_ms) as f32
        };

        Some(LyricHighlight {
            start_byte,
            end_byte: timing.end_byte,
            progress,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LyricsMode {
    #[default]
    Online,
    Lrc,
}

impl From<&str> for LyricsMode {
    fn from(value: &str) -> Self {
        match value {
            "lrc" => Self::Lrc,
            _ => Self::Online,
        }
    }
}

const MAX_LOCAL_LRC_FILES: usize = 2048;

fn fetch_lyrics_local(title: &str, artist: &str, local_dir: &str) -> Option<Arc<Vec<LyricLine>>> {
    let title_key = MatchKey::new(title);
    if title_key.compact.is_empty() {
        return None;
    }
    let artist_key = MatchKey::new(artist);
    let local_dir = Path::new(local_dir);
    if !local_dir.is_dir() {
        return None;
    }

    let mut candidates = collect_lrc_candidates(local_dir, &title_key, &artist_key);
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (_, path) in candidates {
        if let Some(content) = read_lrc_text(&path) {
            let lines = parse_lyrics(&content, "");
            if !lines.is_empty() {
                return Some(Arc::new(lines));
            }
        }
    }
    None
}

#[derive(Clone)]
struct MatchKey {
    normalized: String,
    compact: String,
    simplified_compact: String,
}

impl MatchKey {
    fn new(value: &str) -> Self {
        let value = simplify_chinese(value);
        let normalized = normalize_match_text(&value, false);
        let simplified = normalize_match_text(&value, true);
        Self {
            compact: compact(&normalized),
            simplified_compact: compact(&simplified),
            normalized,
        }
    }

    fn variants(&self) -> impl Iterator<Item = &str> {
        [self.compact.as_str(), self.simplified_compact.as_str()]
            .into_iter()
            .filter(|value| !value.is_empty())
    }

    fn matches(&self, other: &Self) -> bool {
        self.variants()
            .any(|left| other.variants().any(|right| left == right))
    }
}

fn collect_lrc_candidates(
    local_dir: &Path,
    title: &MatchKey,
    artist: &MatchKey,
) -> Vec<(u16, PathBuf)> {
    let Ok(entries) = fs::read_dir(local_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .take(MAX_LOCAL_LRC_FILES)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file()
                || !path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("lrc"))
            {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            let filename_score = score_lrc_match(&MatchKey::new(stem), title, artist);
            let metadata_score = read_lrc_text_prefix(&path)
                .map(|content| score_lrc_metadata(&content, title, artist))
                .unwrap_or(Some(0))?;
            let score = filename_score.max(metadata_score);
            (score >= 80).then_some((score, path))
        })
        .collect()
}

fn score_lrc_match(candidate: &MatchKey, title: &MatchKey, artist: &MatchKey) -> u16 {
    if candidate.matches(title) {
        return 110;
    }
    if candidate.variants().any(|candidate| {
        title.variants().any(|title| {
            artist.variants().any(|artist| {
                !artist.is_empty()
                    && (candidate == format!("{artist}{title}")
                        || candidate == format!("{title}{artist}"))
            })
        })
    }) {
        return 120;
    }
    if let Some(score) = fuzzy_title_score(candidate, title, artist) {
        return score;
    }
    if title.simplified_compact.chars().count() < 3 {
        return 0;
    }
    let contains_title = candidate
        .variants()
        .any(|candidate| title.variants().any(|title| candidate.contains(title)));
    if !contains_title {
        return 0;
    }
    let contains_artist = artist.variants().any(|artist| {
        candidate
            .variants()
            .any(|candidate| candidate.contains(artist))
    });
    if contains_artist { 100 } else { 80 }
}

fn fuzzy_title_score(candidate: &MatchKey, title: &MatchKey, artist: &MatchKey) -> Option<u16> {
    let options = PreprocessingOptions {
        force_ascii: false,
        strip: true,
    };
    let mut queries = vec![title.simplified_compact.as_str()];
    let title_without_artist = remove_component(&title.simplified_compact, artist);
    if title_without_artist != title.simplified_compact && !title_without_artist.is_empty() {
        queries.push(&title_without_artist);
    }

    for query in queries {
        let query_len = query.chars().count();
        let score = partial_ratio(query, &candidate.simplified_compact, &options);
        if query_len >= 8 && score >= 0.90 {
            return Some(100);
        }
        if query_len >= 4 && score >= 0.98 {
            return Some(90);
        }
    }

    let common_len =
        longest_common_substring_len(&candidate.simplified_compact, &title.simplified_compact);
    let shorter_len = candidate
        .simplified_compact
        .chars()
        .count()
        .min(title.simplified_compact.chars().count());
    if common_len >= 8 && common_len * 100 >= shorter_len * 50 {
        return Some(95);
    }

    let (shared_tokens, shared_chars) =
        shared_token_signal(&candidate.normalized, &title.normalized);
    if shared_tokens >= 2
        && shared_chars >= 10
        && partial_token_set_ratio(&candidate.normalized, &title.normalized, &options) >= 0.90
    {
        return Some(90);
    }
    None
}

fn remove_component(value: &str, component: &MatchKey) -> String {
    component
        .variants()
        .filter(|component| component.chars().count() >= 3)
        .fold(value.to_string(), |value, component| {
            value.replace(component, "")
        })
}

fn longest_common_substring_len(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous = vec![0usize; right.len() + 1];
    let mut longest = 0;
    for left_char in left {
        let mut current = vec![0usize; right.len() + 1];
        for (index, right_char) in right.iter().enumerate() {
            if left_char == *right_char {
                current[index + 1] = previous[index] + 1;
                longest = longest.max(current[index + 1]);
            }
        }
        previous = current;
    }
    longest
}

fn shared_token_signal(left: &str, right: &str) -> (usize, usize) {
    let left_tokens: Vec<&str> = left.split_whitespace().collect();
    let mut shared_tokens = 0;
    let mut shared_chars = 0;
    for token in right.split_whitespace() {
        if token.chars().count() >= 3 && left_tokens.contains(&token) {
            shared_tokens += 1;
            shared_chars += token.chars().count();
        }
    }
    (shared_tokens, shared_chars)
}

fn score_lrc_metadata(content: &str, title: &MatchKey, artist: &MatchKey) -> Option<u16> {
    let mut metadata_title = None;
    let mut metadata_artist = None;
    for line in content.lines().take(80) {
        let line = line.trim().trim_start_matches('\u{feff}');
        if let Some(value) = lrc_metadata_value(line, "ti") {
            metadata_title = Some(MatchKey::new(value));
        } else if let Some(value) = lrc_metadata_value(line, "ar") {
            metadata_artist = Some(MatchKey::new(value));
        }
    }
    if metadata_title
        .as_ref()
        .is_some_and(|metadata| !metadata.matches(title))
        || (!artist.compact.is_empty()
            && metadata_artist
                .as_ref()
                .is_some_and(|metadata| !metadata.matches(artist)))
    {
        return None;
    }
    if metadata_title.is_none() {
        return Some(0);
    }
    Some(
        if metadata_artist
            .as_ref()
            .is_some_and(|metadata| metadata.matches(artist))
        {
            140
        } else {
            115
        },
    )
}

fn lrc_metadata_value<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let prefix = format!("[{tag}:");
    line.get(..prefix.len())?
        .eq_ignore_ascii_case(&prefix)
        .then(|| line.get(prefix.len()..)?.strip_suffix(']'))
        .flatten()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn normalize_match_text(value: &str, simplify: bool) -> String {
    let mut output = String::new();
    let mut bracket_depth = 0u32;
    let mut previous_space = true;
    for character in value.trim().to_lowercase().chars() {
        if simplify && matches!(character, '(' | '[' | '{' | '（' | '【') {
            bracket_depth += 1;
            continue;
        }
        if simplify && matches!(character, ')' | ']' | '}' | '）' | '】') {
            bracket_depth = bracket_depth.saturating_sub(1);
            continue;
        }
        if bracket_depth > 0 {
            continue;
        }
        if character.is_alphanumeric() {
            output.push(character);
            previous_space = false;
        } else if !previous_space {
            output.push(' ');
            previous_space = true;
        }
    }
    let normalized = output.trim().to_string();
    if !simplify {
        return normalized;
    }
    let words: Vec<&str> = normalized.split_whitespace().collect();
    let end = words
        .iter()
        .position(|word| matches!(*word, "feat" | "featuring" | "ft"))
        .unwrap_or(words.len());
    words[..end].join(" ")
}

fn compact(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn read_lrc_text_prefix(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(16 * 1024);
    file.by_ref().take(16 * 1024).read_to_end(&mut bytes).ok()?;
    Some(decode_lrc_text(&bytes))
}

fn read_lrc_text(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| decode_lrc_text(&bytes))
}

fn decode_lrc_text(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.trim_start_matches('\u{feff}').to_string();
    }
    if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let utf16 = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&utf16);
    }
    if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        let utf16 = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&utf16);
    }
    let (text, _, _) = GBK.decode(bytes);
    text.into_owned()
}

pub fn load_local_lyrics(
    title: &str,
    artist: &str,
    local_dir: &str,
) -> Option<Arc<Vec<LyricLine>>> {
    if title.is_empty() {
        return None;
    }
    fetch_lyrics_local(title, artist, local_dir).map(simplify_lyrics_for_chinese_ui)
}

/// The body handed out for the previous track. A source that has not caught up with a track change
/// serves the same body under the new song's name, which identifies it even when the body carries
/// no header naming its own song.
static LAST_ACCEPTED_BODY: Mutex<Option<AcceptedBody>> = Mutex::new(None);

struct AcceptedBody {
    title: MatchKey,
    artist: String,
    digest: u64,
}

/// A body may stop before the track does: lyric files often leave the outro out, and players
/// report a duration a few seconds off the file, so a body ending this close to the end still
/// belongs to the track.
const BODY_END_TOLERANCE_MS: u64 = 5_000;
/// A body whose last line falls further than this behind the reported duration belongs to a
/// shorter song. A missing outro stays well inside the window; a three minute body shown against a
/// six minute track does not.
const BODY_END_SHORTFALL_MS: u64 = 90_000;

pub async fn fetch_online_lyrics(
    title: &str,
    artist: &str,
    duration_secs: u64,
    source: &str,
    allow_fallback: bool,
) -> Option<Arc<Vec<LyricLine>>> {
    if title.is_empty() {
        return None;
    }

    log::info!("Lyrics: fetching '{title}' - '{artist}' from source '{source}'");
    if let Some(lyrics) = fetch_checked(source, title, artist, duration_secs).await {
        return Some(lyrics);
    }
    if !allow_fallback {
        // The caller asks the source again before the other providers are involved, so a source
        // that has not caught up with a track change is not abandoned on its first answer.
        log::debug!("Lyrics: source '{source}' has nothing for '{title}' - '{artist}' yet");
        return None;
    }
    log::warn!(
        "Lyrics: source '{source}' returned no lyrics for '{title}' - '{artist}', starting fallback"
    );
    for fallback_source in providers::fallback_sources(source) {
        log::info!("Lyrics: trying fallback source '{fallback_source}'");
        if let Some(lyrics) = fetch_checked(fallback_source, title, artist, duration_secs).await {
            log::warn!(
                "Lyrics: fallback source '{fallback_source}' succeeded ({} lines)",
                lyrics.len()
            );
            log::logger().flush();
            return Some(lyrics);
        }
    }
    None
}

/// Asks one source and only hands the answer on when it belongs to the requested track. Every
/// source goes through here: the configured one and each fallback alike.
async fn fetch_checked(
    source: &str,
    title: &str,
    artist: &str,
    duration_secs: u64,
) -> Option<Arc<Vec<LyricLine>>> {
    let mut lyrics = providers::fetch(source, title, artist, duration_secs).await?;
    // The island picks the line covering the current position with a binary search, so the lines
    // have to be ordered by time. `parse_lyrics` already collects them in a `BTreeMap`, but a
    // word-synced body is parsed in file order, which a source is free to write differently.
    Arc::make_mut(&mut lyrics).sort_by_key(|line| line.time_ms);
    if repeats_previous_track(&lyrics, title, artist) {
        log::warn!(
            "Lyrics: source '{source}' served the body already used for the previous track of '{title}' - '{artist}'"
        );
        log::logger().flush();
        return None;
    }
    if ends_far_from_duration(&lyrics, duration_secs) {
        log::warn!(
            "Lyrics: source '{source}' returned lyrics that do not fit the {duration_secs} s track '{title}' - '{artist}'"
        );
        log::logger().flush();
        return None;
    }
    let word_synced = lyrics.iter().filter(|line| line.is_word_synced()).count();
    log::info!(
        "Lyrics: source '{source}' succeeded ({} lines, {word_synced} word-synced)",
        lyrics.len()
    );
    log::logger().flush();
    remember_accepted_body(&lyrics, title, artist);
    Some(simplify_lyrics_for_chinese_ui(lyrics))
}

/// The same body cannot belong to two different tracks: when a source names another song but
/// hands out the body that was already accepted for the previous one, that body is stale.
fn repeats_previous_track(lyrics: &[LyricLine], title: &str, artist: &str) -> bool {
    let digest = body_digest(lyrics);
    let title = MatchKey::new(title);
    LAST_ACCEPTED_BODY
        .lock()
        .map(|slot| {
            slot.as_ref().is_some_and(|previous| {
                previous.digest == digest && names_another_song(previous, &title, artist)
            })
        })
        .unwrap_or(false)
}

/// Whether the request names a different track than the one the accepted body was fetched for.
///
/// A player refines a track's metadata while it starts: an artist that was still empty gets
/// filled in, and a featured artist can be appended. That is the same song, so the body is not the
/// previous track's body, and the check has to ignore it; only a different title, or two different
/// artists, means the source handed out another track's lyrics.
fn names_another_song(previous: &AcceptedBody, title: &MatchKey, artist: &str) -> bool {
    if !previous.title.matches(title) {
        return true;
    }
    !previous.artist.trim().is_empty()
        && !artist.trim().is_empty()
        && !providers::artist_matches(&previous.artist, artist)
}

/// Whether the last line of the body falls outside the span the reported duration allows.
fn ends_far_from_duration(lyrics: &[LyricLine], duration_secs: u64) -> bool {
    let Some(last_ms) = last_timed_line_ms(lyrics) else {
        return false;
    };
    let Some(duration_ms) = duration_secs.checked_mul(1_000).filter(|value| *value > 0) else {
        return false;
    };
    last_ms > duration_ms.saturating_add(BODY_END_TOLERANCE_MS)
        || last_ms.saturating_add(BODY_END_SHORTFALL_MS) < duration_ms
}

/// Latest timestamp that carries text. A lyric file often ends with an empty timed line that only
/// clears the display, and that line says nothing about how long the song is.
fn last_timed_line_ms(lyrics: &[LyricLine]) -> Option<u64> {
    lyrics
        .iter()
        .filter(|line| !line.text.trim().is_empty())
        .map(|line| line.time_ms)
        .max()
}

fn remember_accepted_body(lyrics: &[LyricLine], title: &str, artist: &str) {
    if let Ok(mut slot) = LAST_ACCEPTED_BODY.lock() {
        *slot = Some(AcceptedBody {
            title: MatchKey::new(title),
            artist: artist.to_string(),
            digest: body_digest(lyrics),
        });
    }
}

/// Identifies the timed text of a body so two answers can be compared without keeping the body
/// around.
fn body_digest(lyrics: &[LyricLine]) -> u64 {
    let mut hasher = DefaultHasher::new();
    lyrics.len().hash(&mut hasher);
    for line in lyrics {
        line.time_ms.hash(&mut hasher);
        line.text.hash(&mut hasher);
    }
    hasher.finish()
}

fn simplify_lyrics_for_chinese_ui(mut lyrics: Arc<Vec<LyricLine>>) -> Arc<Vec<LyricLine>> {
    if crate::i18n::current_lang() != "zh_cn" {
        return lyrics;
    }
    for line in Arc::make_mut(&mut lyrics) {
        let simplified = simplify_chinese(&line.text);
        // A rare one-to-many mapping cannot preserve per-word timing boundaries.
        line.replace_text_preserving_timings(simplified);
        if let Some(secondary) = line.secondary_text.as_mut() {
            *secondary = simplify_chinese(secondary);
        }
    }
    lyrics
}

static SIMPLIFY_HOOK: OnceLock<fn(&str) -> String> = OnceLock::new();

/// Registers the script conversion applied to traditional Chinese lyrics when the interface
/// language is simplified Chinese.
///
/// Until a hook is registered, lyric text is left unchanged.
pub fn set_simplify_hook(hook: fn(&str) -> String) {
    let _ = SIMPLIFY_HOOK.set(hook);
}

fn simplify_chinese(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    match SIMPLIFY_HOOK.get() {
        Some(hook) => hook(text),
        None => text.to_string(),
    }
}

fn parse_lyrics(lrc: &str, tlrc: &str) -> Vec<LyricLine> {
    let mut map: BTreeMap<u64, LyricLine> = BTreeMap::new();

    let mut process_content = |content: &str, keep_empty: bool| {
        let mut standard_lrc = String::with_capacity(content.len());
        for source_line in content.lines() {
            if let Some(line) = parse_word_synced_line(source_line) {
                merge_lyric_line(&mut map, line);
            } else {
                standard_lrc.push_str(source_line);
                standard_lrc.push('\n');
            }
        }

        let normalized = normalize_lrc_timestamps(&standard_lrc);
        let Ok(lyrics) = Lyrics::from_str(&normalized) else {
            return;
        };
        for (timestamp, text) in lyrics.get_timed_lines() {
            let text = text.trim();
            if text.is_empty() && !keep_empty {
                continue;
            }
            let timestamp = timestamp.get_timestamp();
            if timestamp < 0 {
                continue;
            }
            merge_lyric_line(
                &mut map,
                LyricLine {
                    time_ms: timestamp as u64,
                    text: text.to_string(),
                    secondary_text: None,
                    timings: Vec::new(),
                },
            );
        }
    };

    process_content(lrc, true);
    process_content(tlrc, false);

    map.into_values().collect()
}

fn merge_lyric_line(map: &mut BTreeMap<u64, LyricLine>, line: LyricLine) {
    let Some(current) = map.get_mut(&line.time_ms) else {
        map.insert(line.time_ms, line);
        return;
    };
    if line.text.is_empty() {
        return;
    }
    if current.text.is_empty() {
        current.text = line.text;
        current.timings = line.timings;
    } else if current.text != line.text && current.secondary_text.is_none() {
        current.secondary_text = Some(line.text);
    }
}

fn parse_word_synced_line(line: &str) -> Option<LyricLine> {
    let line = line.trim_start_matches('\u{feff}');
    let mut tags = Vec::new();
    let mut search_from = 0;
    while let Some(relative_start) = line.get(search_from..)?.find('[') {
        let start = search_from + relative_start;
        let Some(relative_end) = line.get(start + 1..)?.find(']') else {
            break;
        };
        let end = start + relative_end + 2;
        if let Some(time_ms) = parse_lrc_timestamp(line.get(start + 1..end - 1)?) {
            tags.push((start, end, time_ms));
        }
        search_from = end;
    }

    let mut text = String::new();
    let mut timings = Vec::new();
    for (index, &(_, segment_start, start_time_ms)) in tags.iter().enumerate() {
        let segment_end = tags
            .get(index + 1)
            .map_or(line.len(), |(start, _, _)| *start);
        let segment = line.get(segment_start..segment_end)?;
        if segment.is_empty() {
            continue;
        }
        if timings
            .last()
            .is_some_and(|previous: &LyricTiming| previous.start_time_ms > start_time_ms)
        {
            return None;
        }
        text.push_str(segment);
        timings.push(LyricTiming {
            start_time_ms,
            end_time_ms: None,
            end_byte: text.len(),
        });
    }

    if timings.len() < 2 || text.is_empty() {
        return None;
    }
    Some(LyricLine {
        time_ms: timings.first()?.start_time_ms,
        text,
        secondary_text: None,
        timings,
    })
}

fn parse_lrc_timestamp(timestamp: &str) -> Option<u64> {
    let (minutes, second_part) = timestamp.split_once(':')?;
    let (seconds, fraction) = second_part.split_once('.').unwrap_or((second_part, ""));
    if minutes.is_empty()
        || seconds.is_empty()
        || seconds.len() > 2
        || fraction.len() > 3
        || !minutes.bytes().all(|byte| byte.is_ascii_digit())
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let minutes = minutes.parse::<u64>().ok()?;
    let seconds = seconds.parse::<u64>().ok()?;
    if seconds >= 60 {
        return None;
    }
    let fraction = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 100,
        2 => fraction.parse::<u64>().ok()? * 10,
        3 => fraction.parse::<u64>().ok()?,
        _ => return None,
    };
    minutes
        .checked_mul(60_000)?
        .checked_add(seconds * 1000)?
        .checked_add(fraction)
}

fn normalize_lrc_timestamps(content: &str) -> String {
    let bytes = content.as_bytes();
    let mut output = String::with_capacity(content.len());
    let mut copied_until = 0;
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != b'[' && bytes[index] != b'<' {
            index += 1;
            continue;
        }
        let tag_start = index;
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == tag_start + 1 || bytes.get(index) != Some(&b':') {
            continue;
        }
        index += 1;
        let seconds_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == seconds_start || bytes.get(index) != Some(&b'.') {
            continue;
        }
        index += 1;
        let fraction_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index - fraction_start <= 2 || !matches!(bytes.get(index), Some(b']') | Some(b'>')) {
            continue;
        }

        output.push_str(&content[copied_until..fraction_start]);
        output.push(char::from(bytes[fraction_start]));
        output.push(char::from(bytes[fraction_start + 1]));
        copied_until = index;
    }

    if copied_until == 0 {
        content.to_string()
    } else {
        output.push_str(&content[copied_until..]);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AcceptedBody, LyricLine, MatchKey, body_digest, ends_far_from_duration, last_timed_line_ms,
        names_another_song,
    };

    fn line(time_ms: u64, text: &str) -> LyricLine {
        LyricLine {
            time_ms,
            text: text.to_owned(),
            secondary_text: None,
            timings: Vec::new(),
        }
    }

    fn accepted(title: &str, artist: &str) -> AcceptedBody {
        AcceptedBody {
            title: MatchKey::new(title),
            artist: artist.to_owned(),
            digest: 0,
        }
    }

    #[test]
    fn a_body_that_ends_near_the_duration_belongs_to_the_track() {
        let body = [line(12_000, "first"), line(220_000, "last")];
        assert!(!ends_far_from_duration(&body, 3 * 60 + 45));
    }

    #[test]
    fn a_missing_outro_is_still_accepted() {
        let body = [line(12_000, "first"), line(250_000, "last")];
        assert!(!ends_far_from_duration(&body, 5 * 60));
    }

    #[test]
    fn a_body_from_a_shorter_track_is_rejected() {
        let body = [line(12_000, "first"), line(190_000, "last")];
        assert!(ends_far_from_duration(&body, 6 * 60));
    }

    #[test]
    fn a_body_ending_after_the_track_is_rejected() {
        let body = [line(12_000, "first"), line(310_000, "last")];
        assert!(ends_far_from_duration(&body, 3 * 60));
    }

    #[test]
    fn an_empty_timed_line_does_not_extend_the_body() {
        let body = [
            line(12_000, "first"),
            line(200_000, "last"),
            line(400_000, ""),
        ];
        assert_eq!(last_timed_line_ms(&body), Some(200_000));
        assert!(!ends_far_from_duration(&body, 250));
    }

    #[test]
    fn a_body_without_a_known_duration_cannot_be_measured() {
        let body = [line(12_000, "first")];
        assert!(!ends_far_from_duration(&body, 0));
    }

    #[test]
    fn the_same_body_has_the_same_digest() {
        let first = [line(1_000, "a"), line(2_000, "b")];
        let second = [line(1_000, "a"), line(2_000, "b")];
        let other = [line(1_000, "a"), line(2_000, "c")];
        assert_eq!(body_digest(&first), body_digest(&second));
        assert_ne!(body_digest(&first), body_digest(&other));
    }

    #[test]
    fn a_filled_in_artist_still_names_the_same_song() {
        let previous = accepted("晴天", "");
        assert!(!names_another_song(
            &previous,
            &MatchKey::new("晴天"),
            "周杰伦"
        ));
    }

    #[test]
    fn an_appended_featured_artist_still_names_the_same_song() {
        let previous = accepted("Song", "Artist");
        assert!(!names_another_song(
            &previous,
            &MatchKey::new("Song"),
            "Artist & Guest"
        ));
    }

    #[test]
    fn a_different_title_names_another_song() {
        let previous = accepted("晴天", "周杰伦");
        assert!(names_another_song(
            &previous,
            &MatchKey::new("七里香"),
            "周杰伦"
        ));
    }

    #[test]
    fn a_different_artist_names_another_song() {
        let previous = accepted("Song", "Artist");
        assert!(names_another_song(
            &previous,
            &MatchKey::new("Song"),
            "Other"
        ));
    }
}
