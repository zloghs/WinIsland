use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use winisland_utils::poll::Job;

use winisland_core::config::AppConfig;
use winisland_core::lyrics::{
    LyricHighlight, LyricLine, LyricsMode, fetch_online_lyrics, load_local_lyrics,
};

mod properties;
mod session;
pub mod sessions;
mod worker;

const SEEK_GUARD_DURATION: Duration = Duration::from_secs(4);
/// Every source is asked up to three times before the fallback providers are involved: one request
/// plus two retries. A source can still be holding the previous track's lyrics right after a track
/// change, and a request can fail for a momentary network reason.
const ONLINE_LYRICS_ATTEMPTS: usize = 3;
const ONLINE_LYRICS_RETRY_DELAY: Duration = Duration::from_millis(300);

pub(crate) fn detect_active_apps_async() -> Job<Vec<String>> {
    Job::spawn_named("winisland-smtc-settings", || {
        crate::platform::media().detect_active_apps()
    })
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub source_app_id: String,
    pub track_id: u64,
    pub is_playing: bool,
    pub thumbnail: Option<Arc<[u8]>>,
    pub thumbnail_hash: u64,
    pub spectrum: [f32; 6],
    pub position_ms: u64,
    pub last_update: Instant,
    pub last_thumbnail_fetch: Instant,
    pub lyrics: Option<Arc<Vec<LyricLine>>>,
    pub lyrics_fetch_id: u64,
    pub last_smtc_pos: u64,
    pub duration_secs: u64,
    pub duration_ms: u64,
    pub seek_target_ms: u64,
    pub seek_guard_until: Option<Instant>,
}

impl Default for MediaInfo {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            source_app_id: String::new(),
            track_id: 0,
            is_playing: false,
            thumbnail: None,
            thumbnail_hash: 0,
            spectrum: [0.0; 6],
            position_ms: 0,
            last_update: Instant::now(),
            last_thumbnail_fetch: Instant::now() - Duration::from_secs(10),
            lyrics: None,
            lyrics_fetch_id: 0,
            last_smtc_pos: 0,
            duration_secs: 0,
            duration_ms: 0,
            seek_target_ms: 0,
            seek_guard_until: None,
        }
    }
}

impl MediaInfo {
    pub fn effective_duration_ms(&self) -> u64 {
        if self.duration_ms > 0 {
            self.duration_ms
        } else if self.duration_secs > 0 {
            self.duration_secs * 1000
        } else {
            0
        }
    }

    pub(crate) fn apply_seek(&mut self, position_ms: u64) {
        let duration_ms = self.effective_duration_ms();
        let position_ms = if duration_ms > 0 {
            position_ms.min(duration_ms)
        } else {
            position_ms
        };
        let now = Instant::now();
        self.position_ms = position_ms;
        self.last_update = now;
        self.seek_target_ms = position_ms;
        self.seek_guard_until = Some(now + SEEK_GUARD_DURATION);
    }

    pub(crate) fn reject_seek(&mut self, position_ms: u64) -> bool {
        if self.seek_guard_until.is_none() || self.seek_target_ms != position_ms {
            return false;
        }
        self.position_ms = self.last_smtc_pos;
        self.last_update = Instant::now();
        self.seek_target_ms = 0;
        self.seek_guard_until = None;
        true
    }

    pub fn current_lyric(
        &self,
        delay_ms: i64,
        line_transition_lead_ms: u64,
    ) -> Option<CurrentLyric<'_>> {
        let lyrics = self.lyrics.as_ref()?;
        if lyrics.is_empty() {
            return None;
        }

        let raw_pos = if self.is_playing {
            self.position_ms
                .saturating_add(self.last_update.elapsed().as_millis() as u64)
        } else {
            self.position_ms
        };
        let current_pos = (raw_pos as i64 + delay_ms).max(0) as u64;
        let current_pos = if self.is_playing {
            let duration_ms = self.effective_duration_ms();
            if duration_ms > 0 && current_pos > duration_ms {
                duration_ms
            } else {
                current_pos
            }
        } else {
            current_pos
        };

        let line_selection_pos = current_pos.saturating_add(line_transition_lead_ms);
        let index = lyrics.partition_point(|line| line.time_ms <= line_selection_pos);
        let index = index.checked_sub(1)?;
        let line = &lyrics[index];
        Some(CurrentLyric {
            text: &line.text,
            font_family: line.font_family.as_deref(),
            secondary_text: line
                .secondary_text
                .as_deref()
                .filter(|secondary| *secondary != line.text),
            started: line.time_ms <= current_pos,
            highlight: line
                .highlight_at(current_pos, lyrics.get(index + 1).map(|next| next.time_ms)),
        })
    }
}

pub(crate) struct CurrentLyric<'a> {
    pub(crate) text: &'a str,
    pub(crate) font_family: Option<&'a str>,
    pub(crate) secondary_text: Option<&'a str>,
    pub(crate) started: bool,
    pub(crate) highlight: Option<LyricHighlight>,
}

#[derive(Debug, Clone)]
pub(super) enum PlaybackCommand {
    Toggle,
    Next,
    Prev,
}

pub struct SmtcListener {
    session_bridge: sessions::SessionBridge,
    info_tx: watch::Sender<MediaInfo>,
    info_rx: watch::Receiver<MediaInfo>,
    enabled_tx: watch::Sender<bool>,
    seek_tx: mpsc::UnboundedSender<u64>,
    playback_tx: mpsc::UnboundedSender<PlaybackCommand>,
    lyrics_mode_tx: mpsc::UnboundedSender<LyricsMode>,
    lyrics_source_tx: mpsc::UnboundedSender<String>,
    lyrics_fuzzy_search_tx: mpsc::UnboundedSender<bool>,
    lyrics_local_dir_tx: mpsc::UnboundedSender<Option<String>>,
    show_secondary_lyrics_tx: mpsc::UnboundedSender<bool>,
    allowed_apps_tx: mpsc::UnboundedSender<Vec<String>>,
    wake_tx: std::sync::mpsc::SyncSender<()>,
    cancel_token: CancellationToken,
}

impl SmtcListener {
    pub fn new(
        config: &AppConfig,
        enabled: bool,
        lyrics_bridge: Option<winisland_plugin_host::lifecycle::LyricsBridge>,
    ) -> Self {
        let known_apps = config.smtc_known_apps.clone();
        let session_bridge = sessions::SessionBridge::default();
        let worker_sessions = session_bridge.clone();
        let (info_tx, info_rx) = watch::channel(MediaInfo::default());
        let (enabled_tx, enabled_rx) = watch::channel(enabled);
        let (seek_tx, seek_rx) = mpsc::unbounded_channel();
        let (playback_tx, playback_rx) = mpsc::unbounded_channel();
        let (lyrics_mode_tx, lyrics_mode_rx) = mpsc::unbounded_channel();
        let (lyrics_source_tx, lyrics_source_rx) = mpsc::unbounded_channel();
        let (lyrics_fuzzy_search_tx, lyrics_fuzzy_search_rx) = mpsc::unbounded_channel();
        let (lyrics_local_dir_tx, lyrics_local_dir_rx) = mpsc::unbounded_channel();
        let (show_secondary_lyrics_tx, show_secondary_lyrics_rx) = mpsc::unbounded_channel();
        let (allowed_apps_tx, allowed_apps_rx) = mpsc::unbounded_channel();
        let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
        let cancel_token = CancellationToken::new();

        let _ = lyrics_mode_tx.send(config.lyrics_mode.as_str().into());
        let _ = lyrics_source_tx.send(config.lyrics_source.clone());
        let _ = lyrics_fuzzy_search_tx.send(config.lyrics_fuzzy_search);
        let _ = lyrics_local_dir_tx.send(config.lyrics_local_dir.clone());
        let _ = show_secondary_lyrics_tx.send(config.show_secondary_lyrics);
        let _ = allowed_apps_tx.send(config.smtc_apps.clone());

        let cancel = cancel_token.clone();
        let worker_info_tx = info_tx.clone();
        tokio::task::spawn_blocking(move || {
            worker::smtc_poll_loop(
                worker::WorkerChannels {
                    session_bridge: worker_sessions,
                    info_tx: worker_info_tx,
                    enabled_rx,
                    seek_rx,
                    playback_rx,
                    lyrics_mode_rx,
                    lyrics_source_rx,
                    lyrics_fuzzy_search_rx,
                    lyrics_local_dir_rx,
                    show_secondary_lyrics_rx,
                    allowed_apps_rx,
                    wake_rx,
                    known_apps,
                    lyrics_bridge,
                },
                cancel,
            );
        });

        Self {
            session_bridge,
            info_tx,
            info_rx,
            enabled_tx,
            seek_tx,
            playback_tx,
            lyrics_mode_tx,
            lyrics_source_tx,
            lyrics_fuzzy_search_tx,
            lyrics_local_dir_tx,
            show_secondary_lyrics_tx,
            allowed_apps_tx,
            wake_tx,
            cancel_token,
        }
    }

    pub fn plugin_sessions(&self) -> &sessions::SessionBridge {
        &self.session_bridge
    }

    pub fn request_session(
        &self,
        request: sessions::SessionRequest,
    ) -> winisland_plugin_api::PluginStatus {
        let status = self.session_bridge.request(request);
        self.wake_worker();
        status
    }

    fn wake_worker(&self) {
        let _ = self.wake_tx.try_send(());
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled_tx.send_if_modified(|current| {
            if *current == enabled {
                false
            } else {
                *current = enabled;
                true
            }
        });
    }

    pub fn set_allowed_apps(&self, apps: Vec<String>) {
        let _ = self.allowed_apps_tx.send(apps);
    }

    pub fn set_lyrics_source(&self, source: String) {
        let _ = self.lyrics_source_tx.send(source);
    }

    pub fn set_lyrics_fuzzy_search(&self, fuzzy: bool) {
        if self.lyrics_fuzzy_search_tx.send(fuzzy).is_ok() {
            self.wake_worker();
        }
    }

    pub fn set_lyrics_mode(&self, mode: String) {
        let _ = self.lyrics_mode_tx.send(mode.as_str().into());
    }

    pub fn set_lyrics_local_dir(&self, dir: Option<String>) {
        let _ = self.lyrics_local_dir_tx.send(dir);
    }

    pub fn set_show_secondary_lyrics(&self, show: bool) {
        let _ = self.show_secondary_lyrics_tx.send(show);
    }

    pub fn take_info_if_changed(&mut self) -> Option<MediaInfo> {
        self.info_rx
            .has_changed()
            .ok()
            .filter(|changed| *changed)
            .map(|_| self.info_rx.borrow_and_update().clone())
    }

    pub fn request_seek(&self, position_ms: u64) {
        let duration_ms = self.info_rx.borrow().effective_duration_ms();
        let position_ms = if duration_ms > 0 {
            position_ms.min(duration_ms)
        } else {
            position_ms
        };
        if self.seek_tx.send(position_ms).is_err() {
            return;
        }
        self.info_tx.send_if_modified(|info| {
            if info.title.is_empty() {
                return false;
            }
            info.apply_seek(position_ms);
            true
        });
        self.wake_worker();
    }

    pub fn request_toggle_play(&self) {
        if self.playback_tx.send(PlaybackCommand::Toggle).is_ok() {
            self.wake_worker();
        }
    }

    pub fn request_next(&self) {
        if self.playback_tx.send(PlaybackCommand::Next).is_ok() {
            self.wake_worker();
        }
    }

    pub fn request_prev(&self) {
        if self.playback_tx.send(PlaybackCommand::Prev).is_ok() {
            self.wake_worker();
        }
    }
}

#[derive(Clone)]
pub(super) struct LyricsFetchRequest {
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) duration_secs: u64,
    pub(super) mode: LyricsMode,
    pub(super) source: String,
    pub(super) local_dir: Option<String>,
    pub(super) show_secondary: bool,
    pub(super) fuzzy_search: bool,
    pub(super) request_id: u64,
    pub(super) track_id: u64,
}

#[derive(Clone, Copy)]
pub(super) struct LyricsFetchConfig<'a> {
    pub(super) mode: LyricsMode,
    pub(super) source: &'a str,
    pub(super) local_dir: Option<&'a str>,
    pub(super) show_secondary: bool,
    pub(super) fuzzy_search: bool,
    pub(super) bridge: Option<&'a winisland_plugin_host::lifecycle::LyricsBridge>,
}

impl LyricsFetchRequest {
    fn matches(&self, media: &MediaInfo) -> bool {
        media.title == self.title
            && media.artist == self.artist
            && media.lyrics_fetch_id == self.request_id
            && media.track_id == self.track_id
    }
}

/// Asks the configured source before the fallback providers are involved. The first answers after
/// a track change can still belong to the previous track, so the source is asked again before it is
/// given up on; the last attempt is the one allowed to start the fallback chain.
async fn fetch_online_with_retry(request: &LyricsFetchRequest) -> Option<Arc<Vec<LyricLine>>> {
    for attempt in 0..ONLINE_LYRICS_ATTEMPTS {
        let final_attempt = attempt + 1 == ONLINE_LYRICS_ATTEMPTS;
        if let Some(lyrics) = fetch_online_lyrics(
            &request.title,
            &request.artist,
            request.duration_secs,
            &request.source,
            final_attempt,
            request.fuzzy_search,
        )
        .await
        {
            return Some(lyrics);
        }
        if !final_attempt {
            tokio::time::sleep(ONLINE_LYRICS_RETRY_DELAY).await;
        }
    }
    None
}

/// Tells a running lyrics annotation to stop once the fetch that started it is dropped, for
/// example because the track changed.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub(super) fn spawn_lyrics_fetch(
    info_tx: &watch::Sender<MediaInfo>,
    request: LyricsFetchRequest,
    lyrics_bridge: Option<winisland_plugin_host::lifecycle::LyricsBridge>,
) {
    let info_tx = info_tx.clone();
    tokio::spawn(async move {
        let mut media_changes = info_tx.subscribe();
        if !request.matches(&media_changes.borrow()) {
            return;
        }
        let annotate_bridge = lyrics_bridge.clone();
        let fetch = async {
            let lyrics = if request.mode == LyricsMode::Lrc {
                let local_dir = request
                    .local_dir
                    .clone()
                    .filter(|dir| !dir.trim().is_empty());
                match local_dir {
                    Some(dir) => {
                        let title = request.title.clone();
                        let artist = request.artist.clone();
                        let fuzzy = request.fuzzy_search;
                        tokio::task::spawn_blocking(move || {
                            load_local_lyrics(&title, &artist, &dir, fuzzy)
                        })
                        .await
                        .ok()
                        .flatten()
                    }
                    None => None,
                }
            } else {
                fetch_online_with_retry(&request).await
            };
            let lyrics = match (lyrics, lyrics_bridge) {
                (Some(lyrics), Some(bridge)) => {
                    tokio::task::spawn_blocking(move || bridge.apply(lyrics))
                        .await
                        .ok()
                }
                (lyrics, _) => lyrics,
            };
            let lyrics = lyrics?;
            let Some(bridge) = annotate_bridge.filter(|bridge| bridge.has_secondary()) else {
                return Some(lyrics);
            };
            if !request.show_secondary {
                return Some(lyrics);
            }
            // A provider translation may be in another language than a secondary-text plugin
            // produces, so it stays hidden while plugins run and returns only if none answers.
            let mut stripped = lyrics.clone();
            for line in Arc::make_mut(&mut stripped) {
                line.secondary_text = None;
            }
            let publish = {
                let info_tx = info_tx.clone();
                let request = request.clone();
                move |lyrics: &Arc<Vec<LyricLine>>| {
                    info_tx.send_if_modified(|current| {
                        if !request.matches(current) {
                            return false;
                        }
                        current.lyrics = Some(lyrics.clone());
                        true
                    });
                }
            };
            // Show the lines at once; a plugin may need a while for the whole song.
            publish(&stripped);
            let cancel = CancelOnDrop(Arc::new(AtomicBool::new(false)));
            let cancelled = cancel.0.clone();
            match tokio::task::spawn_blocking(move || {
                bridge.annotate(stripped, &cancelled, publish)
            })
            .await
            {
                Ok(annotated) if annotated.iter().any(|line| line.secondary_text.is_some()) => {
                    Some(annotated)
                }
                _ => Some(lyrics),
            }
        };
        let lyrics = tokio::select! {
            result = fetch => result,
            _ = media_changes.wait_for(|media| !request.matches(media)) => return,
        };
        let applied = info_tx.send_if_modified(|current| {
            if !request.matches(current) {
                return false;
            }
            current.lyrics = lyrics.clone();
            true
        });
        if !applied {
            return;
        }
        if let Some(lyrics) = lyrics {
            log::info!("SMTC: lyrics fetched ({} lines)", lyrics.len());
        } else {
            log::warn!(
                "SMTC: lyrics fetch returned none for '{}' - '{}'",
                request.title,
                request.artist
            );
        }
    });
}

impl Drop for SmtcListener {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        self.wake_worker();
    }
}
