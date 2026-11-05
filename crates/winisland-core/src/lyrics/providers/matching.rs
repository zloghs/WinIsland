use std::cmp::Reverse;

use serde_json::Value;

use crate::lyrics::{
    MatchKey, fuzzy_title_score, normalize_match_text, simplify_chinese, split_artists,
};

pub(super) struct SongQuery {
    raw_title: String,
    title: MatchKey,
    artist: String,
    edition: u16,
    duration_secs: u64,
    fuzzy: bool,
}

impl SongQuery {
    pub(super) fn new(title: &str, artist: &str, duration_secs: u64, fuzzy: bool) -> Self {
        Self {
            raw_title: title.trim().to_string(),
            title: MatchKey::new(title),
            artist: artist.to_string(),
            edition: edition_flags(title),
            duration_secs: if fuzzy { 0 } else { duration_secs },
            fuzzy,
        }
    }

    pub(super) fn search_terms(&self) -> Vec<String> {
        let mut terms = Vec::new();
        if !self.artist.trim().is_empty() {
            terms.push(format!("{} {}", self.raw_title, self.artist.trim()));
        }
        terms.push(self.raw_title.clone());
        for title in self.title_search_terms().into_iter().skip(1) {
            if !self.artist.trim().is_empty() {
                terms.push(format!("{title} {}", self.artist.trim()));
            }
            terms.push(title);
        }
        terms
    }

    pub(super) fn title_search_terms(&self) -> Vec<String> {
        let mut terms = vec![self.raw_title.clone()];
        let simplified = normalize_match_text(&simplify_chinese(&self.raw_title), true);
        if !simplified.is_empty() && MatchKey::new(&simplified).compact != self.title.compact {
            terms.push(simplified);
        }
        terms
    }

    pub(super) fn accepts_edition(&self, title: &str) -> bool {
        self.fuzzy || self.edition == edition_flags(title)
    }

    pub(super) fn score<'a>(
        &self,
        title: &str,
        artists: impl IntoIterator<Item = &'a str>,
        duration_secs: Option<u64>,
    ) -> Option<u16> {
        let candidate = MatchKey::new(title);
        if self.title.compact.is_empty() || candidate.compact.is_empty() {
            return None;
        }
        if !self.accepts_edition(title) {
            return None;
        }
        let title_score = if self.title.compact == candidate.compact {
            300
        } else if !self.title.simplified_compact.is_empty()
            && self.title.simplified_compact == candidate.simplified_compact
        {
            280
        } else if self.title.normalized.split_whitespace().count() >= 2
            && token_sort_ratio(&self.title.normalized, &candidate.normalized) >= 0.999
        {
            260
        } else if self
            .title
            .compact
            .chars()
            .count()
            .min(candidate.compact.chars().count())
            >= 8
            && ratio(&self.title.compact, &candidate.compact) >= 0.94
        {
            220
        } else if self.fuzzy {
            100 + fuzzy_title_score(&candidate, &self.title, &MatchKey::new(&self.artist), true)?
        } else {
            return None;
        };
        let artists: Vec<_> = artists
            .into_iter()
            .filter(|value| !value.trim().is_empty())
            .collect();
        let artist_match = artist_matches(&self.artist, &artists.join(" & "));
        let duration_secs = duration_secs.filter(|duration| *duration > 0);
        if self.duration_secs > 0
            && duration_secs.is_some_and(|duration| {
                duration.abs_diff(self.duration_secs) > 15.max(self.duration_secs / 10)
            })
        {
            return None;
        }
        let duration_match = self.duration_secs > 0
            && duration_secs.is_some_and(|duration| duration.abs_diff(self.duration_secs) <= 5);
        if !self.fuzzy
            && !self.artist.trim().is_empty()
            && !artist_match
            && (!artists.is_empty() || !duration_match || title_score < 280)
        {
            return None;
        }
        if !self.fuzzy && title_score < 260 && !artist_match && !duration_match {
            return None;
        }
        let duration_score = if duration_match { 30 } else { 0 };
        let edition_score = u16::from(self.fuzzy && self.edition == edition_flags(title)) * 20;
        Some(title_score + u16::from(artist_match) * 80 + duration_score + edition_score)
    }
}

pub(super) fn ranked<'a>(
    items: &'a [Value],
    mut score: impl FnMut(&'a Value) -> Option<u16>,
) -> Vec<&'a Value> {
    let mut candidates: Vec<_> = items
        .iter()
        .filter_map(|item| score(item).map(|score| (score, item)))
        .collect();
    candidates.sort_by_key(|(score, _)| Reverse(*score));
    candidates
        .into_iter()
        .take(3)
        .map(|(_, item)| item)
        .collect()
}

pub(crate) fn artist_matches(expected: &str, candidate: &str) -> bool {
    let expected_key = MatchKey::new(expected);
    let candidate_key = MatchKey::new(candidate);
    if expected_key.compact.is_empty() || candidate_key.compact.is_empty() {
        return false;
    }
    if expected_key.compact == candidate_key.compact {
        return true;
    }
    let components = |value: &str| {
        split_artists(value)
            .into_iter()
            .map(|key| key.compact)
            .collect::<Vec<_>>()
    };
    let expected_parts = components(expected);
    let candidate_parts = components(candidate);
    (!expected_parts.is_empty()
        && expected_parts
            .iter()
            .all(|part| candidate_parts.contains(part)))
        || (!candidate_parts.is_empty()
            && candidate_parts
                .iter()
                .all(|part| expected_parts.contains(part)))
}

fn ratio(left: &str, right: &str) -> f64 {
    let left: Vec<char> = left.trim().chars().collect();
    let right: Vec<char> = right.trim().chars().collect();
    let total = left.len() + right.len();
    if total == 0 {
        return 1.0;
    }
    let mut row: Vec<usize> = (0..=right.len()).collect();
    for (index, left_char) in left.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = index + 1;
        for (column, right_char) in right.iter().enumerate() {
            let above = row[column + 1];
            row[column + 1] = if left_char == right_char {
                diagonal
            } else {
                1 + diagonal.min(above).min(row[column])
            };
            diagonal = above;
        }
    }
    1.0 - row[right.len()] as f64 / total as f64
}

fn token_sort_ratio(left: &str, right: &str) -> f64 {
    let sorted = |value: &str| {
        let mut tokens: Vec<_> = value.split(' ').collect();
        tokens.sort_unstable();
        tokens.join(" ")
    };
    ratio(&sorted(left), &sorted(right))
}

fn edition_flags(title: &str) -> u16 {
    let mut qualifiers = String::new();
    let mut depth = 0u32;
    for character in title.chars() {
        if matches!(character, '(' | '[' | '{' | '（' | '【') {
            depth += 1;
        } else if matches!(character, ')' | ']' | '}' | '）' | '】') {
            depth = depth.saturating_sub(1);
            qualifiers.push(' ');
        } else if depth > 0 {
            qualifiers.push(character);
        }
    }
    if let Some((_, suffix)) = title.rsplit_once(" - ") {
        qualifiers.push(' ');
        qualifiers.push_str(suffix);
    }
    let normalized = normalize_match_text(&simplify_chinese(&qualifiers), false);
    let mut flags = 0;
    for word in normalized.split_whitespace() {
        flags |= match word {
            "live" | "现场" | "现场版" | "演唱会" | "live版" => 1,
            "remix" | "混音" | "混音版" => 2,
            "instrumental" | "伴奏" | "伴奏版" | "纯音乐" => 4,
            "cover" | "翻唱" | "翻唱版" => 8,
            "acoustic" | "unplugged" | "不插电" => 16,
            "slowed" | "慢速" | "降速" => 32,
            "sped" | "speedup" | "加速" | "加速版" => 64,
            _ => 0,
        };
    }
    for (markers, flag) in [
        (&["现场", "演唱会"][..], 1),
        (&["混音"][..], 2),
        (&["伴奏", "纯音乐"][..], 4),
        (&["翻唱"][..], 8),
        (&["不插电"][..], 16),
        (&["慢速", "降速"][..], 32),
        (&["加速"][..], 64),
    ] {
        if markers.iter().any(|marker| normalized.contains(marker)) {
            flags |= flag;
        }
    }
    flags
}
