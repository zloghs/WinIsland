mod thumbnail;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession, GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use winisland_platform::{
    MediaCommand, MediaContext, MediaProvider, MediaSessionHandle, PlatformError, ThumbnailError,
    Timeline, TrackInfo,
};

pub struct WindowsMedia;

struct WinRtGuard;

impl WinRtGuard {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: Each successful initialization is balanced by this guard on the same thread.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(PlatformError::backend)?;
        Ok(Self)
    }
}

impl Drop for WinRtGuard {
    fn drop(&mut self) {
        // SAFETY: The guard exists only after a successful RoInitialize on this thread.
        unsafe { RoUninitialize() };
    }
}

struct WindowsMediaContext {
    manager: GlobalSystemMediaTransportControlsSessionManager,
    token: Option<i64>,
    event_rx: mpsc::Receiver<()>,
    /// The manager returns the same sessions on every poll but not always the same Rust wrapper,
    /// so wrappers are reused to keep their timeline estimate alive across polls.
    sessions: Mutex<HashMap<u64, Arc<WindowsMediaSession>>>,
    /// Timeline estimates keyed by source app. `GetCurrentSession` and `GetSessions` hand out
    /// different interface pointers for the same logical session, so the estimate cannot live on
    /// one wrapper alone: both paths have to continue the same position.
    timelines: TimelineStates,
    _guard: WinRtGuard,
}

type TimelineStates = Arc<Mutex<HashMap<String, TimelineEstimate>>>;

impl WindowsMediaContext {
    fn new() -> Result<Self, PlatformError> {
        let guard = WinRtGuard::new()?;
        let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .and_then(|operation| operation.join())
            .map_err(PlatformError::backend)?;
        let (event_tx, event_rx) = mpsc::sync_channel(1);
        let handler = TypedEventHandler::new(move |_manager, _args| {
            let _ = event_tx.try_send(());
            Ok(())
        });
        let token = manager.SessionsChanged(&handler).ok();
        Ok(Self {
            manager,
            token,
            event_rx,
            sessions: Mutex::new(HashMap::new()),
            timelines: Arc::new(Mutex::new(HashMap::new())),
            _guard: guard,
        })
    }

    fn session(
        &self,
        session: GlobalSystemMediaTransportControlsSession,
    ) -> Arc<WindowsMediaSession> {
        let identity = session_identity(&session);
        let timelines = Arc::clone(&self.timelines);
        let mut sessions = lock_sessions(self);
        Arc::clone(
            sessions
                .entry(identity)
                .or_insert_with(|| Arc::new(WindowsMediaSession::new(session, timelines))),
        )
    }
}

fn lock_sessions(
    context: &WindowsMediaContext,
) -> MutexGuard<'_, HashMap<u64, Arc<WindowsMediaSession>>> {
    context
        .sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl MediaContext for WindowsMediaContext {
    fn sessions(&self) -> Vec<Arc<dyn MediaSessionHandle>> {
        let Ok(sessions) = self.manager.GetSessions() else {
            return Vec::new();
        };
        let Ok(count) = sessions.Size() else {
            return Vec::new();
        };
        let timelines = Arc::clone(&self.timelines);
        let mut cached = lock_sessions(self);
        let mut live = HashMap::with_capacity(count as usize);
        let mut handles = Vec::with_capacity(count as usize);
        for index in 0..count {
            let Ok(session) = sessions.GetAt(index) else {
                continue;
            };
            let identity = session_identity(&session);
            let handle = cached.remove(&identity).unwrap_or_else(|| {
                Arc::new(WindowsMediaSession::new(session, Arc::clone(&timelines)))
            });
            live.insert(identity, Arc::clone(&handle));
            handles.push(handle as Arc<dyn MediaSessionHandle>);
        }
        // Sessions that left are dropped so their timeline state cannot leak into a later one.
        *cached = live;
        handles
    }

    fn current(&self) -> Option<Arc<dyn MediaSessionHandle>> {
        self.manager
            .GetCurrentSession()
            .ok()
            .map(|session| self.session(session) as Arc<dyn MediaSessionHandle>)
    }

    fn poll_events(&self) -> bool {
        self.event_rx.try_recv().is_ok()
    }
}

impl Drop for WindowsMediaContext {
    fn drop(&mut self) {
        if let Some(token) = self.token.take()
            && let Err(error) = self.manager.RemoveSessionsChanged(token)
        {
            log::warn!("SMTC: failed to remove session change handler: {error}");
        }
    }
}

struct WindowsMediaSession {
    session: GlobalSystemMediaTransportControlsSession,
    /// Source app the session belongs to, used as the key of its timeline estimate.
    app_id: String,
    /// Position estimates shared by every wrapper of the same app.
    timelines: TimelineStates,
}

/// Position the last poll settled on, together with the published value it came from.
struct TimelineEstimate {
    observed_at: Instant,
    published_position: i64,
    reported_position: i64,
    playing: bool,
    start: i64,
    end: i64,
}

impl WindowsMediaSession {
    fn new(session: GlobalSystemMediaTransportControlsSession, timelines: TimelineStates) -> Self {
        let app_id = read_string(session.SourceAppUserModelId());
        Self {
            session,
            app_id,
            timelines,
        }
    }

    fn media_type(&self) -> Option<windows::Media::MediaPlaybackType> {
        self.session
            .GetPlaybackInfo()
            .ok()?
            .PlaybackType()
            .ok()?
            .Value()
            .ok()
    }

    /// Extrapolates the position a player published at `LastUpdatedTime` to the current poll.
    ///
    /// While the player keeps reporting the same position, the time since the previous poll is
    /// added; a gap longer than [`MAX_EXTRAPOLATION_STEP`] is cut to that bound so a suspended
    /// worker does not add the whole pause.
    ///
    /// A fresh estimate (the application just started, or the player published a new position)
    /// is seeded with the time elapsed since `LastUpdatedTime`: `Position` describes that
    /// moment, and some players do not publish again until the next seek or track. Reading it
    /// as the present would leave the lyrics behind for the rest of the track. A timestamp
    /// older than [`MAX_PUBLISH_AGE`] is ignored, and resuming from pause does not seed at all,
    /// because both values stay untouched until the player publishes again.
    fn advance_position(&self, published: i64, updated: i64, start: i64, end: i64) -> i64 {
        let playing = self.is_playing();
        let now = Instant::now();
        let key = self.timeline_key();
        let mut timelines = self
            .timelines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = timelines.get(&key);
        let same_update = previous.is_some_and(|previous| {
            previous.published_position == published
                && previous.start == start
                && previous.end == end
        });
        let position = estimate_position(
            previous,
            published,
            updated,
            playing,
            same_update,
            now,
            wall_clock_ticks(),
        );
        // Clamping keeps the estimate monotonic: falling back to the stale published position
        // would make the bar jump backwards near the end of a track.
        let position = if playing && end > start {
            position.min(end)
        } else {
            position
        };
        timelines.insert(
            key,
            TimelineEstimate {
                observed_at: now,
                published_position: published,
                reported_position: position,
                playing,
                start,
                end,
            },
        );
        position
    }

    /// Key the estimate is stored under. The source app is the same whichever API handed the
    /// session out; a session that reports no app falls back to its own pointer.
    fn timeline_key(&self) -> String {
        if self.app_id.is_empty() {
            format!("#{}", session_identity(&self.session))
        } else {
            self.app_id.clone()
        }
    }
}

impl MediaSessionHandle for WindowsMediaSession {
    fn identity(&self) -> u64 {
        session_identity(&self.session)
    }

    fn controls(&self) -> winisland_platform::MediaCapabilities {
        let Some(controls) = self
            .session
            .GetPlaybackInfo()
            .ok()
            .and_then(|info| info.Controls().ok())
        else {
            return Default::default();
        };
        winisland_platform::MediaCapabilities {
            play: controls.IsPlayEnabled().unwrap_or(false),
            pause: controls.IsPauseEnabled().unwrap_or(false),
            next: controls.IsNextEnabled().unwrap_or(false),
            previous: controls.IsPreviousEnabled().unwrap_or(false),
            seek: controls.IsPlaybackPositionEnabled().unwrap_or(false),
        }
    }
    fn source_app_id(&self) -> Option<String> {
        self.session
            .SourceAppUserModelId()
            .ok()
            .map(|id| id.to_string())
    }

    fn is_music(&self) -> bool {
        self.media_type() == Some(windows::Media::MediaPlaybackType::Music)
    }

    fn is_video(&self) -> bool {
        self.media_type() == Some(windows::Media::MediaPlaybackType::Video)
    }

    fn is_playing(&self) -> bool {
        self.playback().unwrap_or(false)
    }

    fn playback(&self) -> Result<bool, PlatformError> {
        let info = self
            .session
            .GetPlaybackInfo()
            .map_err(PlatformError::backend)?;
        let status = info.PlaybackStatus().map_err(PlatformError::backend)?;
        Ok(status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
    }

    fn track(&self) -> Result<TrackInfo, PlatformError> {
        let props = self
            .session
            .TryGetMediaPropertiesAsync()
            .and_then(|operation| operation.join())
            .map_err(PlatformError::backend)?;
        Ok(read_track(&props))
    }

    fn timeline(&self) -> Option<Timeline> {
        let props = self.session.GetTimelineProperties().ok()?;
        let start = props.StartTime().map_or(0, |value| value.Duration);
        let end = props.EndTime().map_or(0, |value| value.Duration);
        let published = props.Position().map_or(0, |value| value.Duration);
        let updated = props
            .LastUpdatedTime()
            .map_or(0, |value| value.UniversalTime);
        let position = self.advance_position(published, updated, start, end);
        Some(timeline_from_ticks(
            start,
            end,
            props.MinSeekTime().map_or(0, |value| value.Duration),
            props.MaxSeekTime().map_or(0, |value| value.Duration),
            position,
        ))
    }

    fn send(&self, command: MediaCommand) -> Result<bool, PlatformError> {
        let operation = match command {
            MediaCommand::Toggle => {
                let Ok(is_playing) = self.playback() else {
                    return Ok(false);
                };
                if is_playing {
                    self.session.TryPauseAsync()
                } else {
                    self.session.TryPlayAsync()
                }
            }
            MediaCommand::Play => self.session.TryPlayAsync(),
            MediaCommand::Pause => self.session.TryPauseAsync(),
            MediaCommand::Next => self.session.TrySkipNextAsync(),
            MediaCommand::Previous => self.session.TrySkipPreviousAsync(),
            MediaCommand::Seek(position) => {
                let start = self
                    .session
                    .GetTimelineProperties()
                    .ok()
                    .and_then(|timeline| timeline.StartTime().ok())
                    .map_or(0, |value| value.Duration.max(0));
                let ticks = start.saturating_add(
                    position
                        .as_millis()
                        .min((i64::MAX - start) as u128 / 10_000) as i64
                        * 10_000,
                );
                return self
                    .session
                    .TryChangePlaybackPositionAsync(ticks)
                    .and_then(|operation| operation.join())
                    .map_err(PlatformError::backend);
            }
        };
        operation
            .and_then(|operation| operation.join())
            .map_err(PlatformError::backend)
    }

    fn thumbnail(&self, expected_title: &str) -> Result<Vec<u8>, ThumbnailError> {
        let _guard =
            WinRtGuard::new().map_err(|error| ThumbnailError::Backend(error.to_string()))?;
        thumbnail::load(&self.session, expected_title)
    }
}

fn timeline_from_ticks(
    start: i64,
    end: i64,
    min_seek: i64,
    max_seek: i64,
    position: i64,
) -> Timeline {
    use winisland_platform::TimelineDurationSource;

    const POSITION_TOLERANCE_TICKS: i64 = 15_000_000;
    const MAX_INFERRED_DURATION_TICKS: i64 = 7 * 24 * 60 * 60 * 10_000_000;

    let start = start.max(0);
    let position = position.max(start);
    let (boundary, duration_source) = if end > start
        && end < i64::MAX - POSITION_TOLERANCE_TICKS
        && position <= end.saturating_add(POSITION_TOLERANCE_TICKS)
    {
        (end, TimelineDurationSource::EndTime)
    } else if min_seek <= start
        && max_seek > start
        && max_seek >= position
        && max_seek - start <= MAX_INFERRED_DURATION_TICKS
    {
        (max_seek, TimelineDurationSource::SeekRange)
    } else {
        (start, TimelineDurationSource::Unknown)
    };
    Timeline {
        position: Duration::from_millis(((position - start) / 10_000) as u64),
        duration: Duration::from_millis(((boundary - start) / 10_000) as u64),
        duration_source,
    }
}

/// Longest gap between two polls that is counted as continuous playback. The worker polls every
/// 500 ms; a larger gap (the worker was suspended) contributes at most this much.
const MAX_EXTRAPOLATION_STEP: Duration = Duration::from_secs(2);

/// Longest time between a player's last publish and now that is still counted as playback. A
/// track that keeps playing publishes every few seconds; a much older timestamp means playback
/// stopped without the status changing (system sleep, a player that leaves it behind) and adding
/// the whole span would jump near the end of the track.
const MAX_PUBLISH_AGE: Duration = Duration::from_secs(30);

/// Converts a duration to 100-nanosecond ticks, saturating on absurd values.
fn ticks(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos() / 100).unwrap_or(i64::MAX)
}

/// Ticks from the Windows epoch (1601) to now, the scale `LastUpdatedTime` uses.
fn wall_clock_ticks() -> i64 {
    const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    i64::try_from(now.as_nanos() / 100)
        .unwrap_or(i64::MAX)
        .saturating_add(UNIX_EPOCH_TICKS)
}

/// How long before `now_ticks` a player published at `updated`.
///
/// A timestamp that is missing, in the future, or older than [`MAX_PUBLISH_AGE`] adds nothing:
/// such a value means playback stopped without the status moving (a long pause, system sleep),
/// and counting the whole span as playback would jump forward.
fn publish_delay_ticks(updated: i64, now_ticks: i64) -> i64 {
    if updated <= 0 || now_ticks <= updated {
        return 0;
    }
    let delay = now_ticks - updated;
    if delay > ticks(MAX_PUBLISH_AGE) {
        return 0;
    }
    delay
}

/// Position to report for this poll, before clamping to the track end.
///
/// `same_update` says the player still reports the position it reported last time. While that
/// holds, the estimate advances with the poll interval. A missing or new update is seeded with
/// the time since `LastUpdatedTime`; resuming from pause is not seeded because the player leaves
/// both values untouched until it publishes again.
fn estimate_position(
    previous: Option<&TimelineEstimate>,
    published: i64,
    updated: i64,
    playing: bool,
    same_update: bool,
    now: Instant,
    now_ticks: i64,
) -> i64 {
    match previous {
        Some(previous) if playing && previous.playing && same_update => {
            let step = now
                .saturating_duration_since(previous.observed_at)
                .min(MAX_EXTRAPOLATION_STEP);
            previous.reported_position.saturating_add(ticks(step))
        }
        _ if playing && !same_update => {
            published.saturating_add(publish_delay_ticks(updated, now_ticks))
        }
        _ => published,
    }
}

/// Identity the host uses for a session; also keys the cached wrappers.
fn session_identity(session: &GlobalSystemMediaTransportControlsSession) -> u64 {
    use windows::core::Interface;
    session
        .cast::<windows::core::IUnknown>()
        .map(|object| object.as_raw() as usize as u64)
        .unwrap_or(0)
}

fn read_string(value: windows::core::Result<windows::core::HSTRING>) -> String {
    value
        .map(|value| value.to_string().trim().to_string())
        .unwrap_or_default()
}

fn read_track(props: &GlobalSystemMediaTransportControlsSessionMediaProperties) -> TrackInfo {
    let raw_title = read_string(props.Title());
    let subtitle = read_string(props.Subtitle());
    let album = read_string(props.AlbumTitle());
    let raw_artist = read_string(props.Artist());
    let album_artist = read_string(props.AlbumArtist());
    let title = if !raw_title.is_empty() {
        raw_title
    } else if !album.is_empty() {
        album.clone()
    } else {
        subtitle.clone()
    };
    let artist = if !raw_artist.is_empty() {
        raw_artist
    } else if !album_artist.is_empty() {
        album_artist
    } else if subtitle != title {
        subtitle
    } else {
        String::new()
    };
    TrackInfo {
        title,
        artist,
        album,
        thumbnail: None,
    }
}

impl MediaProvider for WindowsMedia {
    fn open_context(&self) -> Result<Box<dyn MediaContext>, PlatformError> {
        Ok(Box::new(WindowsMediaContext::new()?))
    }

    fn detect_active_apps(&self) -> Vec<String> {
        let Ok(context) = WindowsMediaContext::new() else {
            return Vec::new();
        };
        let mut apps = Vec::new();
        for session in context.sessions() {
            if let Some(id) = session.source_app_id()
                && !apps.contains(&id)
            {
                apps.push(id);
            }
        }
        apps
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{MAX_PUBLISH_AGE, TimelineEstimate, estimate_position, publish_delay_ticks, ticks};

    const END: i64 = 600 * 10_000_000;

    fn previous(
        reported: i64,
        published: i64,
        playing: bool,
        observed_at: Instant,
    ) -> TimelineEstimate {
        TimelineEstimate {
            observed_at,
            published_position: published,
            reported_position: reported,
            playing,
            start: 0,
            end: END,
        }
    }

    #[test]
    fn a_fresh_estimate_starts_from_the_time_the_position_was_published() {
        let now = Instant::now();
        let now_ticks = 133_000_000_000_000_000;
        let published = ticks(Duration::from_secs(108));
        let updated = now_ticks - ticks(Duration::from_secs(6));
        assert_eq!(
            estimate_position(None, published, updated, true, false, now, now_ticks),
            published + ticks(Duration::from_secs(6))
        );
    }

    #[test]
    fn an_unchanged_publish_continues_with_the_poll_interval() {
        let now = Instant::now();
        let published = ticks(Duration::from_secs(108));
        let last = previous(
            published + ticks(Duration::from_secs(6)),
            published,
            true,
            now - Duration::from_millis(300),
        );
        assert_eq!(
            estimate_position(Some(&last), published, 0, true, true, now, 0),
            published + ticks(Duration::from_millis(6_300))
        );
    }

    #[test]
    fn resuming_from_pause_does_not_count_the_pause() {
        let now = Instant::now();
        let now_ticks = 133_000_000_000_000_000;
        let published = ticks(Duration::from_secs(108));
        let last = previous(published, published, false, now - Duration::from_secs(60));
        assert_eq!(
            estimate_position(
                Some(&last),
                published,
                now_ticks - ticks(Duration::from_secs(60)),
                true,
                true,
                now,
                now_ticks,
            ),
            published
        );
    }

    #[test]
    fn a_new_publish_is_seeded_from_its_own_timestamp() {
        let now = Instant::now();
        let now_ticks = 133_000_000_000_000_000;
        let last_published = ticks(Duration::from_secs(50));
        let last = previous(
            last_published,
            last_published,
            true,
            now - Duration::from_millis(300),
        );
        let published = ticks(Duration::from_secs(60));
        let updated = now_ticks - ticks(Duration::from_millis(800));
        assert_eq!(
            estimate_position(Some(&last), published, updated, true, false, now, now_ticks,),
            published + ticks(Duration::from_millis(800))
        );
    }

    #[test]
    fn a_paused_estimate_stays_on_the_published_position() {
        let now = Instant::now();
        let now_ticks = 133_000_000_000_000_000;
        let published = ticks(Duration::from_secs(108));
        assert_eq!(
            estimate_position(None, published, now_ticks, false, false, now, now_ticks),
            published
        );
    }

    #[test]
    fn a_publish_that_just_happened_adds_nothing() {
        let now = 133_000_000_000_000_000;
        assert_eq!(publish_delay_ticks(now, now), 0);
    }

    #[test]
    fn the_delay_is_the_time_since_the_publish() {
        let now = 133_000_000_000_000_000;
        let six_seconds = ticks(Duration::from_secs(6));
        assert_eq!(publish_delay_ticks(now - six_seconds, now), six_seconds);
    }

    #[test]
    fn a_stale_publish_adds_nothing() {
        let now = 133_000_000_000_000_000;
        let hour = ticks(Duration::from_secs(3600));
        assert_eq!(publish_delay_ticks(now - hour, now), 0);
        assert_eq!(
            publish_delay_ticks(now - ticks(MAX_PUBLISH_AGE) - 1, now),
            0
        );
    }

    #[test]
    fn a_missing_or_future_timestamp_adds_nothing() {
        assert_eq!(publish_delay_ticks(0, 133_000_000_000_000_000), 0);
        assert_eq!(
            publish_delay_ticks(133_000_000_000_000_000, 132_000_000_000_000_000),
            0
        );
    }
}
