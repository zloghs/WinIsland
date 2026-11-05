mod amll;
mod kugou;
mod lrclib;
mod lxmusic;
mod matching;
mod netease;
mod qq;

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde_json::Value;

use super::LyricLine;
use crate::config::{APP_HOMEPAGE, APP_VERSION};
use matching::{SongQuery, ranked};

pub(super) const MOZILLA_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
});

const MAX_LYRICS_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(super) async fn fetch(
    source: &str,
    title: &str,
    artist: &str,
    duration_secs: u64,
    fuzzy: bool,
) -> Option<Arc<Vec<LyricLine>>> {
    let duration_secs = if fuzzy { 0 } else { duration_secs };
    match source {
        "amll" => amll::fetch(title, artist, fuzzy).await,
        "qq" => qq::fetch(title, artist, duration_secs, fuzzy).await,
        "kugou" => kugou::fetch(title, artist, duration_secs, fuzzy).await,
        "lrclib" => lrclib::fetch(title, artist, duration_secs, fuzzy).await,
        "lxmusic" => lxmusic::fetch(title, artist, duration_secs, fuzzy).await,
        _ => netease::fetch(title, artist, duration_secs, fuzzy).await,
    }
}

pub(super) fn fallback_sources(source: &str) -> &'static [&'static str] {
    match source {
        "amll" => &["163", "lrclib", "kugou", "qq"],
        "qq" => &["163", "kugou", "lrclib", "amll"],
        "kugou" => &["163", "lrclib", "qq", "amll"],
        "lrclib" => &["163", "kugou", "qq", "amll"],
        "lxmusic" => &["163", "lrclib", "kugou", "qq", "amll"],
        _ => &["lrclib", "kugou", "qq", "amll"],
    }
}

pub(super) async fn get_json(url: &str, user_agent: &str) -> Option<Value> {
    get_json_request(HTTP_CLIENT.get(url).header("User-Agent", user_agent)).await
}

pub(super) async fn get_json_with_timeout(url: &str, timeout: Duration) -> Option<Value> {
    get_json_request(HTTP_CLIENT.get(url).timeout(timeout)).await
}

pub(super) async fn get_json_with_referer(
    url: &str,
    user_agent: &str,
    referer: &str,
) -> Option<Value> {
    get_json_request(
        HTTP_CLIENT
            .get(url)
            .header("User-Agent", user_agent)
            .header("Referer", referer),
    )
    .await
}

async fn get_json_request(request: reqwest::RequestBuilder) -> Option<Value> {
    let mut response = request.send().await.ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_LYRICS_RESPONSE_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len().saturating_add(chunk.len()) > MAX_LYRICS_RESPONSE_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn winisland_ua() -> String {
    format!("WinIsland/{APP_VERSION} ({APP_HOMEPAGE})")
}

pub(super) fn url_encode(input: &str) -> String {
    let mut output = String::new();
    for byte in input.bytes() {
        match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char);
            }
            b' ' => output.push_str("%20"),
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}
