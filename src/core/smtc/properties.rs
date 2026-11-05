use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use tokio::sync::watch;
use winisland_platform::{
    MediaSessionHandle, PlatformError, ThumbnailError, Timeline, TimelineDurationSource, TrackInfo,
};

use super::{LyricsFetchConfig, LyricsFetchRequest, MediaInfo, spawn_lyrics_fetch};

const TIMELINE_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const THUMBNAIL_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const INITIAL_THUMBNAIL_DELAY: Duration = Duration::from_millis(800);
const THUMBNAIL_FETCH_ATTEMPTS: usize = 10;
const FAST_THUMBNAIL_RETRY_ATTEMPTS: usize = 3;
const FAST_THUMBNAIL_RETRY_DELAY: Duration = Duration::from_millis(300);
const THUMBNAIL_RETRY_DELAY: Duration = Duration::from_millis(500);
const SEEK_CONFIRM_TOLERANCE_MS: u64 = 1_500;
const TIMELINE_DRIFT_THRESHOLD_MS: u64 = 2_000;
const SEEK_RANGE_STABILITY: Duration = Duration::from_secs(1);
const MAX_OBSERVED_DURATIONS: usize = 64;
static NEXT_TRACK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

type MediaMetadata = TrackInfo;

fn next_track_id() -> u64 {
    NEXT_TRACK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone)]
struct ThumbnailFetchRequest {
    session: Arc<dyn MediaSessionHandle>,
    source_app_id: String,
    track_id: u64,
    title: String,
    artist: String,
    is_song_change: bool,
}

pub(super) struct ThumbnailFetcher {
    state: Arc<ThumbnailWorkerState>,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct ThumbnailWorkerState {
    request: Mutex<Option<ThumbnailFetchRequest>>,
    changed: Condvar,
    closed: std::sync::atomic::AtomicBool,
}

impl ThumbnailFetcher {
    pub(super) fn new(info_tx: watch::Sender<MediaInfo>) -> Option<Self> {
        let state = Arc::new(ThumbnailWorkerState {
            request: Mutex::new(None),
            changed: Condvar::new(),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        let worker_state = Arc::clone(&state);
        let spawn_result = std::thread::Builder::new()
            .name("winisland-smtc-thumbnail".to_string())
            .spawn(move || {
                loop {
                    let mut pending = worker_state.request.lock();
                    while pending.is_none()
                        && !worker_state
                            .closed
                            .load(std::sync::atomic::Ordering::Acquire)
                    {
                        worker_state.changed.wait(&mut pending);
                    }
                    if worker_state
                        .closed
                        .load(std::sync::atomic::Ordering::Acquire)
                    {
                        return;
                    }
                    let request = pending.take();
                    drop(pending);
                    if let Some(request) = request {
                        fetch_thumbnail(request, &info_tx);
                    }
                }
            });
        match spawn_result {
            Ok(handle) => Some(Self {
                state,
                handle: Some(handle),
            }),
            Err(error) => {
                log::warn!("SMTC: failed to start thumbnail worker: {error}");
                None
            }
        }
    }

    fn request(
        &self,
        session: &Arc<dyn MediaSessionHandle>,
        source_app_id: String,
        track_id: u64,
        title: String,
        artist: String,
        is_song_change: bool,
    ) {
        let mut request = self.state.request.lock();
        *request = Some(ThumbnailFetchRequest {
            session: Arc::clone(session),
            source_app_id,
            track_id,
            title,
            artist,
            is_song_change,
        });
        drop(request);
        self.state.changed.notify_one();
    }
}

impl Drop for ThumbnailFetcher {
    fn drop(&mut self) {
        self.state
            .closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.state.changed.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Default)]
pub(super) struct TimelineCache {
    source_app_id: String,
    last_fetch: Option<Instant>,
    position_ms: u64,
    duration_secs: u64,
    duration_ms: u64,
    duration_source: TimelineDurationSource,
    seek_range: Option<SeekRangeObservation>,
    observed: VecDeque<ObservedDuration>,
}

#[derive(PartialEq, Eq)]
struct DurationIdentity {
    source: String,
    title: String,
    artist: String,
    album: String,
}

impl DurationIdentity {
    fn new(source: &str, track: &TrackInfo) -> Self {
        Self {
            source: source.to_string(),
            title: track.title.trim().to_lowercase(),
            artist: track.artist.trim().to_lowercase(),
            album: track.album.trim().to_lowercase(),
        }
    }
}

struct ObservedDuration {
    identity: DurationIdentity,
    duration_ms: u64,
}

struct SeekRangeObservation {
    duration_ms: u64,
    position_ms: u64,
    since: Instant,
}

impl TimelineCache {
    pub(super) fn clear(&mut self) {
        let observed = std::mem::take(&mut self.observed);
        *self = Self {
            observed,
            ..Self::default()
        };
    }

    fn recall(&self, identity: &DurationIdentity) -> u64 {
        if let Some(entry) = self
            .observed
            .iter()
            .rev()
            .find(|entry| entry.identity == *identity)
        {
            return entry.duration_ms;
        }
        if identity.title.is_empty() || identity.artist.is_empty() || identity.album.is_empty() {
            return 0;
        }
        let mut matches = self.observed.iter().rev().filter(|entry| {
            entry.identity.title == identity.title
                && entry.identity.artist == identity.artist
                && entry.identity.album == identity.album
        });
        let Some(latest) = matches.next() else {
            return 0;
        };
        if matches
            .any(|entry| entry.duration_ms.abs_diff(latest.duration_ms) > SEEK_CONFIRM_TOLERANCE_MS)
        {
            return 0;
        }
        latest.duration_ms
    }

    fn remember(&mut self, identity: DurationIdentity, duration_ms: u64) {
        if identity.title.is_empty() || identity.artist.is_empty() || duration_ms == 0 {
            return;
        }
        self.observed.retain(|entry| entry.identity != identity);
        self.observed.push_back(ObservedDuration {
            identity,
            duration_ms,
        });
        while self.observed.len() > MAX_OBSERVED_DURATIONS {
            self.observed.pop_front();
        }
    }

    fn observe(
        &mut self,
        identity: DurationIdentity,
        timeline: Timeline,
        now: Instant,
    ) -> TimelineSnapshot {
        let position_ms = u64::try_from(timeline.position.as_millis()).unwrap_or(u64::MAX);
        let reported_duration = u64::try_from(timeline.duration.as_millis()).unwrap_or(u64::MAX);
        let known_duration = if self.duration_source == TimelineDurationSource::EndTime {
            self.duration_ms
        } else {
            self.recall(&identity)
        };
        let known_duration =
            if position_ms <= known_duration.saturating_add(SEEK_CONFIRM_TOLERANCE_MS) {
                known_duration
            } else {
                0
            };
        let (duration_ms, duration_source) = match timeline.duration_source {
            TimelineDurationSource::EndTime if reported_duration > 0 => {
                self.seek_range = None;
                self.remember(identity, reported_duration);
                (reported_duration, TimelineDurationSource::EndTime)
            }
            TimelineDurationSource::SeekRange if known_duration == 0 => {
                let stable = match &self.seek_range {
                    Some(previous)
                        if previous.duration_ms == reported_duration
                            && position_ms >= previous.position_ms
                            && position_ms - previous.position_ms
                                <= u64::try_from(
                                    now.duration_since(previous.since).as_millis(),
                                )
                                .unwrap_or(u64::MAX)
                                .saturating_mul(4)
                                .saturating_add(TIMELINE_DRIFT_THRESHOLD_MS) =>
                    {
                        now.duration_since(previous.since) >= SEEK_RANGE_STABILITY
                            && position_ms - previous.position_ms
                                >= TIMELINE_REFRESH_INTERVAL.as_millis() as u64
                    }
                    _ => {
                        self.seek_range = Some(SeekRangeObservation {
                            duration_ms: reported_duration,
                            position_ms,
                            since: now,
                        });
                        false
                    }
                };
                if stable {
                    (reported_duration, TimelineDurationSource::SeekRange)
                } else {
                    (0, TimelineDurationSource::Unknown)
                }
            }
            _ => {
                self.seek_range = None;
                (
                    known_duration,
                    if known_duration > 0 {
                        TimelineDurationSource::EndTime
                    } else {
                        TimelineDurationSource::Unknown
                    },
                )
            }
        };
        self.last_fetch = Some(now);
        self.position_ms = position_ms;
        self.duration_secs = duration_ms / 1000;
        self.duration_ms = duration_ms;
        self.duration_source = duration_source;
        TimelineSnapshot {
            position_ms,
            duration_secs: self.duration_secs,
            duration_ms,
        }
    }
}

#[derive(Clone, Copy)]
struct TimelineSnapshot {
    position_ms: u64,
    duration_secs: u64,
    duration_ms: u64,
}

struct FetchedMediaState<'a> {
    metadata: &'a MediaMetadata,
    source_app_id: &'a str,
    is_playing: bool,
    timeline: TimelineSnapshot,
}

struct QueuedThumbnailRequest {
    track_id: u64,
    title: String,
    artist: String,
    is_song_change: bool,
}

#[derive(Default)]
struct PendingMediaRequests {
    lyrics: Option<(String, String, u64, u64)>,
    thumbnail: Option<QueuedThumbnailRequest>,
}

pub(super) fn fetch_properties(
    session: &Arc<dyn MediaSessionHandle>,
    info_tx: &watch::Sender<MediaInfo>,
    lyrics: LyricsFetchConfig<'_>,
    thumbnail_fetcher: Option<&ThumbnailFetcher>,
    timeline_cache: &mut TimelineCache,
) -> Result<(), PlatformError> {
    if session.is_video() {
        let info = info_tx.borrow();
        if !info.title.is_empty() {
            drop(info);
            let _ = info_tx.send(MediaInfo::default());
        }
        return Ok(());
    }

    let metadata = session.track()?;
    let is_playing = session.playback()?;
    let source_app_id = session
        .source_app_id()
        .ok_or(PlatformError::Unavailable("media source"))?;
    let track_changed = {
        let info = info_tx.borrow();
        is_new_track(&info, &metadata, &source_app_id)
    };
    let should_fetch_timeline = track_changed
        || (info_tx.borrow().is_playing != is_playing)
        || timeline_cache.source_app_id != source_app_id
        || timeline_cache
            .last_fetch
            .is_none_or(|last| last.elapsed() >= TIMELINE_REFRESH_INTERVAL);
    let timeline = read_timeline(
        session,
        &source_app_id,
        &metadata,
        should_fetch_timeline,
        track_changed,
        timeline_cache,
    );
    let update = FetchedMediaState {
        metadata: &metadata,
        source_app_id: &source_app_id,
        is_playing,
        timeline,
    };
    let mut requests = PendingMediaRequests::default();
    info_tx.send_if_modified(|info| update_media_info(info, &update, &mut requests));

    if let Some(request) = requests.thumbnail
        && let Some(thumbnail_fetcher) = thumbnail_fetcher
    {
        thumbnail_fetcher.request(
            session,
            source_app_id.clone(),
            request.track_id,
            request.title,
            request.artist,
            request.is_song_change,
        );
    }
    if let Some((title, artist, request_id, track_id)) = requests.lyrics {
        spawn_lyrics_fetch(
            info_tx,
            LyricsFetchRequest {
                title,
                artist,
                duration_secs: timeline.duration_secs,
                mode: lyrics.mode,
                source: lyrics.source.to_string(),
                local_dir: lyrics.local_dir.map(str::to_string),
                request_id,
                track_id,
            },
            lyrics.bridge.cloned(),
        );
    }
    Ok(())
}

fn update_media_info(
    info: &mut MediaInfo,
    update: &FetchedMediaState<'_>,
    requests: &mut PendingMediaRequests,
) -> bool {
    let track_changed = is_new_track(info, update.metadata, update.source_app_id);
    let mut changed = track_changed;
    if track_changed {
        start_new_track(info, update, requests);
    } else if update.metadata.title.is_empty()
        && info.source_app_id != update.source_app_id
        && !info.title.is_empty()
    {
        *info = MediaInfo::default();
        return true;
    } else if !update.metadata.title.is_empty() {
        changed |= enrich_current_track(info, update, requests);
    }

    changed |= sync_timeline(info, update, track_changed);
    changed |= update_playback_state(info, update, track_changed);
    let duration_was_unknown = info.duration_ms == 0;
    changed |= update_duration(info, update.timeline);
    if duration_was_unknown
        && info.duration_ms > 0
        && info.lyrics.is_none()
        && requests.lyrics.is_none()
    {
        queue_lyrics(info, requests);
    }
    changed
}

fn start_new_track(
    info: &mut MediaInfo,
    update: &FetchedMediaState<'_>,
    requests: &mut PendingMediaRequests,
) {
    let metadata = update.metadata;
    log::info!(
        "SMTC: track changed -> {} - {} / {} ({})",
        metadata.title,
        metadata.artist,
        metadata.album,
        update.source_app_id
    );
    info.title.clone_from(&metadata.title);
    info.artist.clone_from(&metadata.artist);
    info.album.clone_from(&metadata.album);
    info.source_app_id.clear();
    info.source_app_id.push_str(update.source_app_id);
    info.track_id = next_track_id();
    info.duration_secs = update.timeline.duration_secs;
    info.duration_ms = update.timeline.duration_ms;
    info.thumbnail = None;
    info.thumbnail_hash = 0;
    info.position_ms = update.timeline.position_ms;
    info.last_smtc_pos = update.timeline.position_ms;
    info.last_update = Instant::now();
    info.seek_target_ms = 0;
    info.seek_guard_until = None;
    info.last_thumbnail_fetch = Instant::now();
    queue_lyrics(info, requests);
    requests.thumbnail = Some(QueuedThumbnailRequest {
        track_id: info.track_id,
        title: info.title.clone(),
        artist: info.artist.clone(),
        is_song_change: true,
    });
}

fn enrich_current_track(
    info: &mut MediaInfo,
    update: &FetchedMediaState<'_>,
    requests: &mut PendingMediaRequests,
) -> bool {
    let metadata = update.metadata;
    let mut changed = false;
    if info.artist.is_empty() && !metadata.artist.is_empty() {
        info.artist.clone_from(&metadata.artist);
        // The artist is only used to match the track, so a body that was already fetched for it
        // stays valid. Asking again would hand out the same body, which the freshness check would
        // then read as the previous track's.
        if info.lyrics.is_none() {
            queue_lyrics(info, requests);
        }
        changed = true;
    }
    if info.album.is_empty() && !metadata.album.is_empty() {
        info.album.clone_from(&metadata.album);
        changed = true;
    }
    if info.thumbnail.is_none()
        && (info.is_playing != update.is_playing
            || info.last_thumbnail_fetch.elapsed() >= THUMBNAIL_RETRY_INTERVAL)
    {
        info.last_thumbnail_fetch = Instant::now();
        requests.thumbnail = Some(QueuedThumbnailRequest {
            track_id: info.track_id,
            title: info.title.clone(),
            artist: info.artist.clone(),
            is_song_change: false,
        });
    }
    changed
}

fn sync_timeline(
    info: &mut MediaInfo,
    update: &FetchedMediaState<'_>,
    track_changed: bool,
) -> bool {
    let timeline = update.timeline;
    let elapsed_ms = u64::try_from(info.last_update.elapsed().as_millis()).unwrap_or(u64::MAX);
    let current_position = if info.is_playing {
        info.position_ms.saturating_add(elapsed_ms)
    } else {
        info.position_ms
    };
    let timeline_changed = timeline.position_ms != info.last_smtc_pos;
    let drift_ms = timeline.position_ms.abs_diff(current_position);
    let had_seek_guard = !track_changed && info.seek_guard_until.is_some();
    let mut seek_guard_active = had_seek_guard
        && info
            .seek_guard_until
            .is_some_and(|until| Instant::now() < until);
    let mut seek_guard_expired = had_seek_guard && !seek_guard_active;
    if !seek_guard_active {
        info.seek_guard_until = None;
    } else if timeline.position_ms > 0
        && timeline.position_ms.abs_diff(info.seek_target_ms) < SEEK_CONFIRM_TOLERANCE_MS
    {
        info.seek_guard_until = None;
        seek_guard_active = false;
        seek_guard_expired = false;
    }

    let should_sync = track_changed
        || info.is_playing != update.is_playing
        || (timeline.position_ms > 0 && info.position_ms == 0)
        || seek_guard_expired
        || (timeline_changed && (drift_ms > TIMELINE_DRIFT_THRESHOLD_MS || !update.is_playing));
    info.last_smtc_pos = timeline.position_ms;
    if !should_sync || seek_guard_active {
        return false;
    }
    if timeline.position_ms > 0 || !track_changed {
        info.position_ms = timeline.position_ms;
    }
    info.last_update = Instant::now();
    true
}

fn update_playback_state(
    info: &mut MediaInfo,
    update: &FetchedMediaState<'_>,
    track_changed: bool,
) -> bool {
    let playback_changed = info.is_playing != update.is_playing;
    info.is_playing = update.is_playing;
    let source_changed =
        !update.metadata.title.is_empty() && info.source_app_id != update.source_app_id;
    if source_changed {
        info.source_app_id.clear();
        info.source_app_id.push_str(update.source_app_id);
    }
    if playback_changed && !track_changed {
        log::info!(
            "SMTC: playback state -> {}",
            if update.is_playing {
                "Playing"
            } else {
                "Paused"
            }
        );
    }
    playback_changed || source_changed
}

fn queue_lyrics(info: &mut MediaInfo, requests: &mut PendingMediaRequests) {
    info.lyrics = None;
    info.lyrics_fetch_id = info.lyrics_fetch_id.wrapping_add(1);
    requests.lyrics = Some((
        info.title.clone(),
        info.artist.clone(),
        info.lyrics_fetch_id,
        info.track_id,
    ));
}

fn update_duration(info: &mut MediaInfo, timeline: TimelineSnapshot) -> bool {
    if info.duration_secs == timeline.duration_secs && info.duration_ms == timeline.duration_ms {
        return false;
    }
    info.duration_secs = timeline.duration_secs;
    info.duration_ms = timeline.duration_ms;
    true
}

fn is_new_track(info: &MediaInfo, metadata: &MediaMetadata, source_app_id: &str) -> bool {
    if metadata.title.is_empty() {
        return false;
    }
    info.title.is_empty()
        || info.source_app_id != source_app_id
        || info.title != metadata.title
        || (!info.artist.is_empty()
            && !metadata.artist.is_empty()
            && info.artist != metadata.artist)
        || (!info.album.is_empty() && !metadata.album.is_empty() && info.album != metadata.album)
}

fn read_timeline(
    session: &Arc<dyn MediaSessionHandle>,
    source_app_id: &str,
    metadata: &MediaMetadata,
    should_fetch: bool,
    reset_cache: bool,
    cache: &mut TimelineCache,
) -> TimelineSnapshot {
    if reset_cache || cache.source_app_id != source_app_id {
        cache.clear();
        cache.source_app_id = source_app_id.to_string();
    }
    if !should_fetch {
        return TimelineSnapshot {
            position_ms: cache.position_ms,
            duration_secs: cache.duration_secs,
            duration_ms: cache.duration_ms,
        };
    }

    let Some(timeline) = session.timeline() else {
        cache.last_fetch = Some(Instant::now());
        return TimelineSnapshot {
            position_ms: cache.position_ms,
            duration_secs: cache.duration_secs,
            duration_ms: cache.duration_ms,
        };
    };

    let identity = DurationIdentity::new(source_app_id, metadata);
    cache.observe(identity, timeline, Instant::now())
}

fn fetch_thumbnail(request: ThumbnailFetchRequest, info_tx: &watch::Sender<MediaInfo>) {
    if request.is_song_change {
        std::thread::sleep(INITIAL_THUMBNAIL_DELAY);
    }
    for attempt in 0..THUMBNAIL_FETCH_ATTEMPTS {
        if !thumbnail_request_is_current(info_tx, &request) {
            return;
        }
        match request.session.thumbnail(&request.title) {
            Ok(bytes) => {
                publish_thumbnail(info_tx, &request, bytes);
                return;
            }
            Err(ThumbnailError::Stale) => return,
            Err(ThumbnailError::Terminal) => {
                log::warn!(
                    "SMTC: could not safely load oversized thumbnail for '{}' - '{}'",
                    request.title,
                    request.artist
                );
                return;
            }
            Err(_) => {}
        }
        if attempt + 1 < THUMBNAIL_FETCH_ATTEMPTS {
            std::thread::sleep(thumbnail_retry_delay(attempt));
        }
    }
    log::warn!(
        "SMTC: thumbnail fetch failed for '{}' - '{}' after {} attempts",
        request.title,
        request.artist,
        THUMBNAIL_FETCH_ATTEMPTS
    );
}

fn publish_thumbnail(
    info_tx: &watch::Sender<MediaInfo>,
    request: &ThumbnailFetchRequest,
    bytes: Vec<u8>,
) {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    let hash = hasher.finish();
    let byte_len = bytes.len();
    let data: Arc<[u8]> = Arc::from(bytes);
    let applied = info_tx.send_if_modified(|current| {
        if current.track_id != request.track_id
            || current.source_app_id != request.source_app_id
            || current.thumbnail.is_some()
            || current.thumbnail_hash == hash
        {
            return false;
        }
        current.thumbnail = Some(data);
        current.thumbnail_hash = hash;
        true
    });
    if applied {
        log::info!("SMTC: thumbnail fetched ({byte_len} bytes, hash={hash:#x})");
        crate::platform::wake();
    }
}

fn thumbnail_retry_delay(attempt: usize) -> Duration {
    if attempt < FAST_THUMBNAIL_RETRY_ATTEMPTS {
        FAST_THUMBNAIL_RETRY_DELAY
    } else {
        THUMBNAIL_RETRY_DELAY
    }
}

fn thumbnail_request_is_current(
    info_tx: &watch::Sender<MediaInfo>,
    request: &ThumbnailFetchRequest,
) -> bool {
    let current = info_tx.borrow();
    current.track_id == request.track_id
        && current.source_app_id == request.source_app_id
        && current.thumbnail.is_none()
}
