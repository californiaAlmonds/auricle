use std::sync::{mpsc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rodio::{OutputStream, OutputStreamHandle, Sink};
use ytmapi_rs::common::YoutubeID;
use ytmapi_rs::YtMusic;
use ytmapi_rs::query::{search::SongsFilter, SearchQuery};

use crate::core::stream_player::{get_stream_url, StreamingAudioSource};

use crate::core::cache::AudioCache;
use crate::core::persistence;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NowPlaying {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub duration_secs: u32,
}

pub struct PlaybackState {
    pub queue: Vec<NowPlaying>,
    pub queue_index: usize,
    pub now_playing: NowPlaying,
    pub is_playing: bool,
    pub audio_enabled: bool,
    audio_worker: Option<AudioWorker>,
    pub track_started_at: Option<std::time::Instant>,
    pub paused_elapsed: std::time::Duration,
    pub history: std::collections::VecDeque<NowPlaying>,
}

struct AudioWorker {
    sender: mpsc::Sender<AudioCommand>,
}

struct AudioEngine {
    _stream: OutputStream,
    handle: OutputStreamHandle,
    sink: Sink,
    current_video_id: Option<String>,
    current_is_cached: bool,
    /// CDN URL of the currently streaming track (for Range-request seeks)
    stream_url: Option<String>,
    /// Total file size in bytes — used to compute byte offset for seeks
    stream_content_len: Option<u64>,
    /// Current volume level (0.0–1.0), persisted across sink replacements
    volume: f32,
    /// Duration (secs) of the loaded source, published so the polling loop can
    /// backfill songs whose metadata omitted a duration.
    duration_out: Arc<AtomicU32>,
}

enum AudioCommand {
    SetTrack { video_id: String, title: String, artist: String, duration_secs: u32 },
    SetPlaying(bool),
    SetVolume(f32),
    /// Seek to absolute seconds. Instant for cached files, deferred until upgrade for streams.
    Seek { secs: f64, video_id: String },
    /// Swap a currently-streaming track to its now-finished cached file.
    /// elapsed_secs is the current UI timer value — reflects user seeks.
    UpgradeToCache { path: std::path::PathBuf, elapsed_secs: f64 },
}

enum AudioEvent {
    StreamReady {
        request_id: u64,
        video_id: String,
        result: Result<AudioSource, String>,
    },
}

enum AudioSource {
    /// CDN streaming URL + total byte length (for Range seeks)
    Stream(String, Option<u64>),
    CachedFile(std::path::PathBuf),
}

pub struct PlaybackCore {
    state: Mutex<PlaybackState>,
    advance_pending: Arc<AtomicBool>,
    liked_ids: Mutex<std::collections::HashSet<String>>,
    liked_songs: Mutex<Vec<NowPlaying>>,
    disliked_ids: Mutex<std::collections::HashSet<String>>,
    current_is_cached: Arc<AtomicBool>,
    /// True while yt-dlp/cache-lookup is in progress — timer stays at 0.
    audio_loading: Arc<AtomicBool>,
    /// Flipped true by the audio worker the moment a track actually starts playing.
    audio_just_started: Arc<AtomicBool>,
    /// Set when queue reaches the end and autoplay radio fetch is needed.
    autoplay_needed: Arc<AtomicBool>,
    /// Real track duration (secs) detected from the audio stream; 0 = none pending.
    detected_duration: Arc<AtomicU32>,
    /// Autoplay/radio queue (what to continue with once the user queue ends) and
    /// its seed video id. Previously separate `Arc<Mutex<…>>` values juggled in the
    /// UI layer; folded in here so callers use methods instead of raw shared locks.
    autoplay_queue: Mutex<Vec<NowPlaying>>,
    autoplay_seed: Mutex<String>,
}

fn log_native_audio_error(stage: &str, code: &str, video_id: &str, detail: &str) {
    let message = format!(
        "[native-audio][stage={stage}][code={code}] video_id={video_id} detail={detail}"
    );
    log::error!("{message}");
    eprintln!("{message}");
}

fn create_audio_engine(duration_out: Arc<AtomicU32>) -> Result<AudioEngine, String> {
    let (_stream, handle) = OutputStream::try_default()
        .map_err(|err| format!("Failed to open native audio output: {err}"))?;
    let sink = Sink::try_new(&handle)
        .map_err(|err| format!("Failed to create native audio sink: {err}"))?;
    Ok(AudioEngine {
        _stream,
        handle,
        sink,
        current_video_id: None,
        current_is_cached: false,
        stream_url: None,
        stream_content_len: None,
        volume: 1.0,
        duration_out,
    })
}

fn spawn_stream_fetch(events: mpsc::Sender<AudioEvent>, request_id: u64, video_id: String, force_refresh: bool) {
    thread::spawn(move || {
        // 1. Cache hit? (a local .m4a can never 401 — always safe to use)
        let cached = AudioCache::global().lock().ok()
            .and_then(|mut c| c.get(&video_id));
        if let Some(path) = cached {
            let _ = events.send(AudioEvent::StreamReady {
                request_id,
                video_id,
                result: Ok(AudioSource::CachedFile(path)),
            });
            return;
        }
        // Recovering from an expired/unauthorized signed URL — drop the stale
        // cached URL so get_stream_url re-runs yt-dlp for a freshly-signed one.
        if force_refresh {
            crate::core::stream_player::invalidate_cached_url(&video_id);
        }
        // 2. Stream URL via yt-dlp
        let url = match get_stream_url(&video_id) {
            Ok(u) => u,
            Err(e) => {
                let _ = events.send(AudioEvent::StreamReady { request_id, video_id, result: Err(e) });
                return;
            }
        };
        // 3. Get total byte length for seeking (Range:bytes=0-0 — very fast, <200ms)
        let content_len = crate::core::stream_player::fetch_content_length(&url);
        eprintln!("[stream-fetch] {video_id} content_len={content_len:?}");
        let _ = events.send(AudioEvent::StreamReady {
            request_id,
            video_id,
            result: Ok(AudioSource::Stream(url, content_len)),
        });
    });
}

fn load_stream_into_engine(engine: &mut AudioEngine, video_id: &str, url: &str, fallback_content_len: Option<u64>, seek_secs: Option<f64>, tee_meta: Option<(&str, &str)>) -> Result<(), String> {
    // Write-through-cache only a fresh full load (no seek). A seek starts mid-file
    // and would produce a non-contiguous cache file, so stream without teeing then.
    let mut source = match (tee_meta, seek_secs) {
        (Some((title, artist)), None) => StreamingAudioSource::from_url_teed(url, video_id, title, artist),
        _ => StreamingAudioSource::from_url(url),
    }.map_err(|e| {
        log_native_audio_error("decode", "stream-open-failed", video_id, &e);
        e
    })?;

    // Pre-seek on the raw source (HTTP range request) BEFORE appending — the same
    // reliable path cached files use, which sidesteps rodio's sink.try_seek chain.
    if let Some(secs) = seek_secs.filter(|&s| s > 0.5) {
        source.seek_to(secs)?;
    }

    // Use content_len from the stream response, fall back to the HEAD-fetched value.
    let content_len = source.content_len().or(fallback_content_len);
    let replacement_sink = Sink::try_new(&engine.handle).map_err(|e| {
        let err = format!("Failed to replace native audio sink: {e}");
        log_native_audio_error("playback", "sink-replace-failed", video_id, &err);
        err
    })?;

    replacement_sink.set_volume(engine.volume);
    engine.duration_out.store(source.duration_secs().unwrap_or(0), Ordering::Relaxed);
    replacement_sink.append(source);
    replacement_sink.play();

    engine.sink.stop();
    engine.sink = replacement_sink;
    engine.current_video_id = Some(video_id.to_string());
    engine.current_is_cached = false;
    engine.stream_url = Some(url.to_string());
    engine.stream_content_len = content_len;
    Ok(())
}

/// Load a cached file into the engine, optionally pre-seeking to `seek_secs`.
/// Seeking happens on the source BEFORE appending to the sink — avoids
/// relying on sink.try_seek() which doesn't forward cleanly through rodio's pipeline.
fn load_file_into_engine(engine: &mut AudioEngine, video_id: &str, path: &std::path::Path, seek_secs: Option<f64>) -> Result<(), String> {
    let mut source = StreamingAudioSource::from_file(path)
        .map_err(|e| format!("Cache file decode error: {e}"))?;

    if let Some(secs) = seek_secs.filter(|&s| s > 0.1) {
        if let Err(e) = source.seek_to(secs) {
            eprintln!("[file-seek] seek_to({secs:.1}s) failed: {e}");
            // Non-fatal: play from beginning rather than crash
        }
    }

    let replacement_sink = Sink::try_new(&engine.handle)
        .map_err(|e| format!("Failed to replace sink: {e}"))?;
    replacement_sink.set_volume(engine.volume);
    engine.duration_out.store(source.duration_secs().unwrap_or(0), Ordering::Relaxed);
    replacement_sink.append(source);
    replacement_sink.play();
    engine.sink.stop();
    engine.sink = replacement_sink;
    engine.current_video_id = Some(video_id.to_string());
    engine.current_is_cached = true;
    engine.stream_url = None;
    engine.stream_content_len = None;
    Ok(())
}

fn spawn_audio_worker(
    advance_pending: Arc<AtomicBool>,
    is_cached_flag: Arc<AtomicBool>,
    audio_loading_flag: Arc<AtomicBool>,
    audio_just_started_flag: Arc<AtomicBool>,
    detected_duration: Arc<AtomicU32>,
) -> Result<AudioWorker, String> {
    let (sender, receiver) = mpsc::channel::<AudioCommand>();
    let (event_sender, event_receiver) = mpsc::channel::<AudioEvent>();

    thread::Builder::new()
        .name("ytm-native-audio".to_string())
        .spawn(move || {
            let mut engine = match create_audio_engine(detected_duration) {
                Ok(e) => e,
                Err(err) => {
                    log::error!("Native audio initialization failed: {err}");
                    return;
                }
            };

            let mut requested_video: Option<String> = None;
            // Title/artist of the requested track, learned from SetTrack. Used to
            // label a write-through cache commit for the currently streaming song.
            let mut requested_title = String::new();
            let mut requested_artist = String::new();
            let mut is_playing = false;
            let mut latest_request_id: u64 = 0;
            // Seek target deferred until cache upgrade completes (stream-only tracks).
            let mut pending_seek: Option<f64> = None;
            let mut sink_empty_count: u32 = 0;
            // Track elapsed time inside the worker for cache-fallback seeks
            let mut playback_started_at: Option<std::time::Instant> = None;
            let mut playback_base_secs: f64 = 0.0;
            // Consecutive failed load attempts for the current track. Reset on
            // every new-track request and on a successful load. Bounds the
            // fresh-URL retry loop so a broken track skips instead of stalling.
            let mut stream_retries: u32 = 0;
            const MAX_STREAM_RETRIES: u32 = 2;
            // Duration of the current track (secs), learned from SetTrack. Lets us
            // tell a mid-song stall (recover from cache) apart from the natural end
            // of the track (just advance).
            let mut current_duration_secs: f64 = 0.0;

            loop {
                // Collect all immediately-available commands, then process them.
                // A scrubber drag emits a burst of Seeks; keep only the final one
                // so we don't flood the worker with per-pixel range requests.
                let mut commands: Vec<AudioCommand> = Vec::new();
                match receiver.recv_timeout(Duration::from_millis(80)) {
                    Ok(command) => commands.push(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                while let Ok(command) = receiver.try_recv() {
                    commands.push(command);
                }
                let last_seek_idx = commands.iter()
                    .rposition(|c| matches!(c, AudioCommand::Seek { .. }));
                for (cmd_idx, command) in commands.into_iter().enumerate() {
                    // Skip superseded scrub positions — only the last Seek matters.
                    if matches!(command, AudioCommand::Seek { .. }) && Some(cmd_idx) != last_seek_idx {
                        continue;
                    }
                    match command {
                        AudioCommand::SetTrack { video_id, title, artist, duration_secs } => {
                            current_duration_secs = duration_secs as f64;
                            requested_title = title;
                            requested_artist = artist;
                            let same_loaded = engine.current_video_id.as_deref() == Some(&video_id);
                            let same_requested = requested_video.as_deref() == Some(&video_id);
                            requested_video = Some(video_id.clone());

                            if same_loaded && same_requested {
                                if is_playing { engine.sink.play(); }
                                audio_loading_flag.store(false, Ordering::Relaxed);
                                continue;
                            }

                            // Reset elapsed tracking for new track
                            playback_base_secs = 0.0;
                            playback_started_at = None;

                            // If already requested but not yet loaded, don't spawn again
                            if same_requested && !same_loaded {
                                continue;
                            }

                            engine.sink.stop();
                            engine.current_video_id = None;
                            pending_seek = None;

                            if is_playing {
                                // New track needs fetching — freeze timer until StreamReady.
                                audio_loading_flag.store(true, Ordering::Relaxed);
                                audio_just_started_flag.store(false, Ordering::Relaxed);
                                stream_retries = 0;
                                latest_request_id = latest_request_id.saturating_add(1);
                                spawn_stream_fetch(event_sender.clone(), latest_request_id, video_id, false);
                            }
                        }
                        AudioCommand::SetPlaying(playing) => {
                            is_playing = playing;
                            if !playing {
                                // Freeze elapsed tracking
                                if let Some(started) = playback_started_at.take() {
                                    playback_base_secs += started.elapsed().as_secs_f64();
                                }
                                engine.sink.pause();
                                continue;
                            } else {
                                playback_started_at = Some(std::time::Instant::now());
                            }
                            if let Some(ref video_id) = requested_video.clone() {
                                if engine.current_video_id.as_deref() == Some(video_id) {
                                    engine.sink.play();
                                    audio_loading_flag.store(false, Ordering::Relaxed);
                                    sink_empty_count = 0;
                                } else if audio_loading_flag.load(Ordering::Relaxed) {
                                    // SetTrack already kicked off a fetch for this video in
                                    // the same command batch; avoid spawning a second one
                                    // (which wastes a yt-dlp call and can interfere with CDN
                                    // rate-limiting). Just wait for the existing StreamReady.
                                    audio_just_started_flag.store(false, Ordering::Relaxed);
                                } else {
                                    engine.sink.stop();
                                    engine.current_video_id = None;
                                    audio_loading_flag.store(true, Ordering::Relaxed);
                                    audio_just_started_flag.store(false, Ordering::Relaxed);
                                    stream_retries = 0;
                                    latest_request_id = latest_request_id.saturating_add(1);
                                    spawn_stream_fetch(event_sender.clone(), latest_request_id, video_id.clone(), false);
                                }
                            }
                        }
                        AudioCommand::UpgradeToCache { path, elapsed_secs } => {
                            if engine.current_is_cached { continue; }
                            // Only swap while paused — a sink swap during playback
                            // is audible (brief gap/overlap). Paused = silent swap.
                            // If playback resumed between the poll and here, skip;
                            // the song keeps streaming and seeks still use the cache.
                            if is_playing { continue; }
                            let vid = match engine.current_video_id.clone() {
                                Some(v) => v,
                                None => continue,
                            };
                            // If user sought while streaming, honour that exact target.
                            // Otherwise use elapsed_secs (real playback position).
                            let seek_target = pending_seek.take().unwrap_or(elapsed_secs);
                            match load_file_into_engine(&mut engine, &vid, &path, Some(seek_target)) {
                                Ok(()) => {
                                    is_cached_flag.store(true, Ordering::Relaxed);
                                    if !is_playing { engine.sink.pause(); }
                                    playback_base_secs = seek_target;
                                    playback_started_at = if is_playing { Some(std::time::Instant::now()) } else { None };
                                    eprintln!("[upgrade] {vid} → cached file, seeked to {seek_target:.1}s");
                                }
                                Err(e) => eprintln!("[upgrade] cache load failed: {e}"),
                            }
                        }
                        AudioCommand::SetVolume(v) => {
                            engine.volume = v;
                            engine.sink.set_volume(v);
                        }
                        AudioCommand::Seek { secs, video_id } => {
                            // Priority 1: file is in cache — load it and seek by baking
                            // position into the source before appending to sink.
                            let cached_path = AudioCache::global().lock().ok()
                                .and_then(|mut c| c.get(&video_id));
                            if let Some(path) = cached_path {
                                match load_file_into_engine(&mut engine, &video_id, &path, Some(secs)) {
                                    Ok(()) => {
                                        is_cached_flag.store(true, Ordering::Relaxed);
                                        playback_base_secs = secs;
                                        playback_started_at = if is_playing { Some(std::time::Instant::now()) } else { None };
                                        if !is_playing { engine.sink.pause(); }
                                    }
                                    Err(e) => eprintln!("[seek] cache load failed: {e}"),
                                }
                            } else if engine.current_is_cached {
                                // Priority 2: already on cached file — reload at target position.
                                // We need the path again; try cache lookup by current_video_id.
                                if let Some(cached_vid) = engine.current_video_id.clone() {
                                    let path2 = AudioCache::global().lock().ok()
                                        .and_then(|mut c| c.get(&cached_vid));
                                    if let Some(path) = path2 {
                                        match load_file_into_engine(&mut engine, &cached_vid, &path, Some(secs)) {
                                            Ok(()) => {
                                                playback_base_secs = secs;
                                                playback_started_at = if is_playing { Some(std::time::Instant::now()) } else { None };
                                                if !is_playing { engine.sink.pause(); }
                                            }
                                            Err(e) => eprintln!("[seek-cached] reload+seek failed: {e}"),
                                        }
                                    }
                                }
                            } else {
                                // Streaming: try a light in-place seek first (reuses the
                                // parsed MP4 moov via rodio's sink.try_seek). If rodio's
                                // source chain doesn't forward the seek, reload a fresh
                                // source pre-seeked to the target — the same reliable path
                                // cached files use. Defer to the cache only as a last resort.
                                let url = engine.stream_url.clone();
                                let clen = engine.stream_content_len;
                                let in_place = engine.sink.try_seek(Duration::from_secs_f64(secs.max(0.0)));
                                if in_place.is_ok() {
                                    eprintln!("[seek] stream in-place seek → {secs:.1}s");
                                    playback_base_secs = secs;
                                    playback_started_at = if is_playing { Some(std::time::Instant::now()) } else { None };
                                } else if let Some(url) = url {
                                    match load_stream_into_engine(&mut engine, &video_id, &url, clen, Some(secs), None) {
                                        Ok(()) => {
                                            eprintln!("[seek] stream reloaded at {secs:.1}s");
                                            playback_base_secs = secs;
                                            playback_started_at = if is_playing { Some(std::time::Instant::now()) } else { None };
                                            if !is_playing { engine.sink.pause(); }
                                        }
                                        Err(e) => {
                                            pending_seek = Some(secs);
                                            eprintln!("[seek] stream seek failed ({e}) — deferred to {secs:.1}s, awaiting cache");
                                        }
                                    }
                                } else {
                                    pending_seek = Some(secs);
                                }
                            }
                        }
                    }
                }

                while let Ok(event) = event_receiver.try_recv() {
                    match event {
                        AudioEvent::StreamReady { request_id, video_id, result } => {
                            if request_id != latest_request_id {
                                continue;
                            }
                            if !is_playing || requested_video.as_deref() != Some(&video_id) {
                                continue;
                            }
                            match result {
                                Ok(AudioSource::Stream(url, content_len)) => {
                                    if let Err(err) = load_stream_into_engine(&mut engine, &video_id, &url, content_len, None, Some((&requested_title, &requested_artist))) {
                                        // Most common cause: expired/unauthorized signed CDN
                                        // URL (HTTP 401/403) from a prefetched radio track.
                                        if stream_retries < MAX_STREAM_RETRIES {
                                            stream_retries += 1;
                                            log_native_audio_error("playback", "stream-load-retry", &video_id, &err);
                                            eprintln!("[stream-retry] {video_id} attempt {stream_retries}/{MAX_STREAM_RETRIES} (fresh URL)");
                                            latest_request_id = latest_request_id.saturating_add(1);
                                            spawn_stream_fetch(event_sender.clone(), latest_request_id, video_id.clone(), true);
                                        } else {
                                            // Exhausted retries — unfreeze the UI and skip to the
                                            // next track instead of stalling on "loading" forever.
                                            log_native_audio_error("playback", "stream-load-giveup", &video_id, &err);
                                            audio_loading_flag.store(false, Ordering::Relaxed);
                                            engine.current_video_id = None;
                                            stream_retries = 0;
                                            advance_pending.store(true, Ordering::Relaxed);
                                        }
                                    } else {
                                        is_cached_flag.store(false, Ordering::Relaxed);
                                        audio_loading_flag.store(false, Ordering::Relaxed);
                                        audio_just_started_flag.store(true, Ordering::Relaxed);
                                        sink_empty_count = 0;
                                        stream_retries = 0;
                                        playback_base_secs = 0.0;
                                        playback_started_at = Some(std::time::Instant::now());
                                    }
                                }
                                Ok(AudioSource::CachedFile(path)) => {
                                    if let Err(err) = load_file_into_engine(&mut engine, &video_id, &path, None) {
                                        log_native_audio_error("playback", "cache-load-failed", &video_id, &err);
                                        // Fall back to streaming (fresh stream attempt)
                                        stream_retries = 0;
                                        latest_request_id = latest_request_id.saturating_add(1);
                                        spawn_stream_fetch(event_sender.clone(), latest_request_id, video_id, false);
                                    } else {
                                        is_cached_flag.store(true, Ordering::Relaxed);
                                        audio_loading_flag.store(false, Ordering::Relaxed);
                                        audio_just_started_flag.store(true, Ordering::Relaxed);
                                        sink_empty_count = 0;
                                        stream_retries = 0;
                                        playback_base_secs = 0.0;
                                        playback_started_at = Some(std::time::Instant::now());
                                    }
                                }
                                Err(err) => {
                                    // yt-dlp couldn't resolve a URL at all.
                                    if stream_retries < MAX_STREAM_RETRIES {
                                        stream_retries += 1;
                                        log_native_audio_error("extract", "stream-url-retry", &video_id, &err);
                                        eprintln!("[stream-retry] {video_id} url-resolve attempt {stream_retries}/{MAX_STREAM_RETRIES}");
                                        latest_request_id = latest_request_id.saturating_add(1);
                                        spawn_stream_fetch(event_sender.clone(), latest_request_id, video_id.clone(), true);
                                    } else {
                                        // Give up — unfreeze the UI and skip to the next track.
                                        log_native_audio_error("extract", "stream-url-giveup", &video_id, &err);
                                        audio_loading_flag.store(false, Ordering::Relaxed);
                                        engine.current_video_id = None;
                                        stream_retries = 0;
                                        advance_pending.store(true, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                    }
                }

                // Auto-advance: signal when the current track finishes
                // Debounce: sink must be empty for 15+ consecutive checks (~1.2s)
                // to avoid false triggers from HTTP stream buffering gaps or sink replacements.
                if is_playing
                    && engine.current_video_id.is_some()
                    && !audio_loading_flag.load(Ordering::Relaxed)
                    && engine.sink.empty()
                {
                    sink_empty_count += 1;

                    // At ~400ms of empty sink on a streaming track, try cache fallback
                    // before declaring the song finished.
                    if sink_empty_count == 5 && !engine.current_is_cached {
                        let elapsed = playback_base_secs
                            + playback_started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                        // Only recover genuine mid-song stalls. If the stream simply
                        // reached its natural end, skip the cache load (it would seek
                        // past the end and could restart the track) and let the
                        // advance debounce fire.
                        let near_end = current_duration_secs > 0.0
                            && elapsed >= current_duration_secs - 5.0;
                        if !near_end {
                            if let Some(ref vid) = engine.current_video_id.clone() {
                                let cached_path = crate::core::cache::AudioCache::global().lock().ok()
                                    .and_then(|mut c| c.get(vid));
                                if let Some(path) = cached_path {
                                    match load_file_into_engine(&mut engine, vid, &path, Some(elapsed)) {
                                        Ok(()) => {
                                            is_cached_flag.store(true, Ordering::Relaxed);
                                            sink_empty_count = 0;
                                            playback_base_secs = elapsed;
                                            playback_started_at = Some(std::time::Instant::now());
                                            eprintln!("[cache-fallback] {vid} → resumed from cache at {elapsed:.1}s");
                                        }
                                        Err(e) => {
                                            eprintln!("[cache-fallback] {vid} failed: {e}");
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if sink_empty_count >= 15 {
                        engine.current_video_id = None;
                        advance_pending.store(true, Ordering::Relaxed);
                        sink_empty_count = 0;
                    }
                } else {
                    sink_empty_count = 0;
                }
            }
        })
        .map_err(|err| format!("Failed to spawn native audio worker thread: {err}"))?;

    Ok(AudioWorker { sender })
}


fn parse_duration_str(s: &str) -> u32 {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        2 => parts[0].parse::<u32>().unwrap_or(0) * 60 + parts[1].parse::<u32>().unwrap_or(0),
        3 => {
            parts[0].parse::<u32>().unwrap_or(0) * 3600
                + parts[1].parse::<u32>().unwrap_or(0) * 60
                + parts[2].parse::<u32>().unwrap_or(0)
        }
        _ => 0,
    }
}

impl PlaybackCore {
    pub fn new() -> Self {
        let user_data = persistence::load();
        let history: std::collections::VecDeque<NowPlaying> = user_data.history.into_iter().map(NowPlaying::from).collect();
        let liked_songs: Vec<NowPlaying> = user_data.liked.iter().map(|s| NowPlaying::from(s.clone())).collect();
        let liked_ids: std::collections::HashSet<String> = user_data.liked.iter().map(|s| s.video_id.clone()).collect();
        let disliked_ids: std::collections::HashSet<String> = user_data.disliked.iter().map(|s| s.video_id.clone()).collect();

        Self {
            state: Mutex::new(PlaybackState {
                queue: vec![NowPlaying {
                    video_id: "native-prototype".to_string(),
                    title: "Native Shell Prototype".to_string(),
                    artist: "Auricle".to_string(),
                    duration_secs: 0,
                }],
                queue_index: 0,
                now_playing: NowPlaying {
                    video_id: "native-prototype".to_string(),
                    title: "Native Shell Prototype".to_string(),
                    artist: "Auricle".to_string(),
                    duration_secs: 0,
                },
                is_playing: false,
                audio_enabled: false,
                audio_worker: None,
                track_started_at: None,
                paused_elapsed: std::time::Duration::ZERO,
                history,
            }),
            advance_pending: Arc::new(AtomicBool::new(false)),
            liked_ids: Mutex::new(liked_ids),
            liked_songs: Mutex::new(liked_songs),
            disliked_ids: Mutex::new(disliked_ids),
            current_is_cached: Arc::new(AtomicBool::new(false)),
            audio_loading: Arc::new(AtomicBool::new(false)),
            audio_just_started: Arc::new(AtomicBool::new(false)),
            autoplay_needed: Arc::new(AtomicBool::new(false)),
            detected_duration: Arc::new(AtomicU32::new(0)),
            autoplay_queue: Mutex::new(Vec::new()),
            autoplay_seed: Mutex::new(String::new()),
        }
    }

    pub fn enable_audio_output(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if state.audio_enabled {
            return Ok(());
        }

        state.audio_worker = Some(spawn_audio_worker(
            Arc::clone(&self.advance_pending),
            Arc::clone(&self.current_is_cached),
            Arc::clone(&self.audio_loading),
            Arc::clone(&self.audio_just_started),
            Arc::clone(&self.detected_duration),
        )?);
        state.audio_enabled = true;
        Ok(())
    }

    fn sync_audio_playback(&self) {
        let (audio_enabled, is_playing, video_id, title, artist, duration_secs, sender) = {
            let state = self.state.lock().unwrap();
            (
                state.audio_enabled,
                state.is_playing,
                state.now_playing.video_id.clone(),
                state.now_playing.title.clone(),
                state.now_playing.artist.clone(),
                state.now_playing.duration_secs,
                state.audio_worker.as_ref().map(|w| w.sender.clone()),
            )
        };

        if !audio_enabled {
            return;
        }

        if let Some(sender) = sender {
            if !is_playing {
                if sender.send(AudioCommand::SetPlaying(false)).is_err() {
                    log::error!("Failed to send play state to native audio worker");
                }
                return;
            }

            if sender.send(AudioCommand::SetTrack { video_id, title, artist, duration_secs }).is_err() {
                log::error!("Failed to send track to native audio worker");
                return;
            }
            if sender.send(AudioCommand::SetPlaying(true)).is_err() {
                log::error!("Failed to send play state to native audio worker");
            }
        }
    }

    pub fn toggle_play_pause(&self) -> bool {
        let is_playing = {
            let mut state = self.state.lock().unwrap();
            let now = std::time::Instant::now();
            if state.is_playing {
                // Pausing: accumulate elapsed time
                if let Some(started) = state.track_started_at {
                    state.paused_elapsed += now.duration_since(started);
                }
                state.track_started_at = None;
            } else {
                // Resuming: start timer
                state.track_started_at = Some(now);
            }
            state.is_playing = !state.is_playing;
            state.is_playing
        };

        self.sync_audio_playback();

        is_playing
    }

    pub fn set_playing(&self, is_playing: bool) {
        {
            let mut state = self.state.lock().unwrap();
            let now = std::time::Instant::now();
            if is_playing && !state.is_playing {
                state.track_started_at = Some(now);
            } else if !is_playing && state.is_playing {
                if let Some(started) = state.track_started_at {
                    state.paused_elapsed += now.duration_since(started);
                }
                state.track_started_at = None;
            }
            state.is_playing = is_playing;
        }

        self.sync_audio_playback();
    }

    pub fn is_playing(&self) -> bool {
        self.state.lock().unwrap().is_playing
    }

    /// Expose direct state lock for advanced callers that need atomic multi-field updates.
    pub fn state_lock(&self) -> std::sync::MutexGuard<'_, PlaybackState> {
        self.state.lock().unwrap()
    }

    pub fn now_playing(&self) -> NowPlaying {
        self.state.lock().unwrap().now_playing.clone()
    }

    pub fn set_now_playing(
        &self,
        video_id: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
        duration_secs: u32,
    ) {
        let mut state = self.state.lock().unwrap();

        // Save previous track BEFORE overwriting
        let prev = state.now_playing.clone();

        state.now_playing = NowPlaying {
            video_id: video_id.into(),
            title: title.into(),
            artist: artist.into(),
            duration_secs,
        };
        // Reset playback timer for the new track
        state.paused_elapsed = std::time::Duration::ZERO;
        state.track_started_at = if state.is_playing { Some(std::time::Instant::now()) } else { None };

        // Add previous track to history
        if !prev.video_id.is_empty() && prev.video_id != "native-prototype" {
            // Dedup: don't add same track twice in a row
            if state.history.front().map(|h| h.video_id.as_str()) != Some(&prev.video_id) {
                state.history.push_front(prev);
                if state.history.len() > 200 {
                    state.history.pop_back();
                }
            }
        }
        // Persist history
        // (must be after drop(state) to avoid deadlock)

        if let Some(existing_index) = state
            .queue
            .iter()
            .position(|song| song.video_id == state.now_playing.video_id)
        {
            state.queue_index = existing_index;
        } else {
            // Song not in queue (e.g. from autoplay or external play).
            // Clear already-played items — history handles "previous" navigation.
            let now_playing = state.now_playing.clone();
            state.queue.clear();
            state.queue.push(now_playing);
            state.queue_index = 0;
        }

        // Remove the placeholder entry if still present
        state.queue.retain(|s| s.video_id != "native-prototype");
        state.queue_index = state.queue_index.min(state.queue.len().saturating_sub(1));

        drop(state);
        self.persist();
        self.sync_audio_playback();
        self.prewarm_next_queue_url();
    }

    pub fn prev_track(&self) {
        // Standard player behavior: if we're more than 3s into the track,
        // restart it rather than jumping to the previous song.
        if self.elapsed_secs() > 3.0 {
            self.seek_to_secs(0.0);
            return;
        }

        let mut state = self.state.lock().unwrap();
        if state.queue.is_empty() { return; }

        // Prefer a genuine previous entry when the queue holds multiple songs.
        if state.queue.len() > 1 && state.queue_index > 0 {
            state.queue_index -= 1;
            state.now_playing = state.queue[state.queue_index].clone();
            state.paused_elapsed = std::time::Duration::ZERO;
            state.track_started_at = if state.is_playing { Some(std::time::Instant::now()) } else { None };
            drop(state);
            self.sync_audio_playback();
            self.prewarm_next_queue_url();
            return;
        }

        // Otherwise (autoplay / single-item queue) walk back through play history.
        // `history.front()` is the most recently played track; skip the current one.
        let cur = state.now_playing.video_id.clone();
        let back_idx = state.history.iter()
            .position(|h| h.video_id != cur && h.video_id != "native-prototype");
        match back_idx {
            Some(i) => {
                // Consume the entry so repeated prev walks further back instead of
                // toggling between two tracks.
                let song = state.history.remove(i).expect("history index valid");
                state.now_playing = song.clone();
                state.queue.clear();
                state.queue.push(song);
                state.queue_index = 0;
                state.paused_elapsed = std::time::Duration::ZERO;
                state.track_started_at = if state.is_playing { Some(std::time::Instant::now()) } else { None };
                drop(state);
                self.persist();
                self.sync_audio_playback();
                self.prewarm_next_queue_url();
            }
            None => {
                // Nothing earlier to return to — restart the current track.
                drop(state);
                self.seek_to_secs(0.0);
            }
        }
    }

    pub fn next_track(&self) {
        let mut state = self.state.lock().unwrap();
        if state.queue.is_empty() { return; }
        if state.queue_index + 1 >= state.queue.len() {
            // Reached end of queue — request autoplay radio fetch
            self.autoplay_needed.store(true, Ordering::Relaxed);
            return;
        }
        state.queue_index += 1;
        state.now_playing = state.queue[state.queue_index].clone();
        // Reset timer so progress bar starts from 0
        state.paused_elapsed = std::time::Duration::ZERO;
        state.track_started_at = if state.is_playing { Some(std::time::Instant::now()) } else { None };
        drop(state);
        self.sync_audio_playback();
        self.prewarm_next_queue_url();
    }

    /// Pre-warm the CDN stream URL for the next track in the *user* queue
    /// (background yt-dlp `-g`). Autoplay look-ahead is handled in the UI layer
    /// (`prewarm_upcoming` in lib.rs) because it spans the separate autoplay queue.
    fn prewarm_next_queue_url(&self) {
        let state = self.state.lock().unwrap();
        if state.queue.len() < 2 { return; }
        let next_idx = (state.queue_index + 1) % state.queue.len();
        let next_vid = state.queue[next_idx].video_id.clone();
        drop(state);
        crate::core::stream_player::prefetch_stream_url(&next_vid);
    }

    pub fn queue_preview(&self, limit: usize) -> Vec<NowPlaying> {
        let state = self.state.lock().unwrap();
        if state.queue.is_empty() {
            return vec![];
        }

        let mut preview = Vec::new();
        let max_items = limit.min(state.queue.len());

        for step in 0..max_items {
            let idx = (state.queue_index + step) % state.queue.len();
            let track = &state.queue[idx];
            preview.push(track.clone());
        }

        preview
    }

    pub async fn seed_queue_from_backend(&self, query: &str, limit: usize) -> Result<(), String> {
        let api = YtMusic::new_unauthenticated()
            .await
            .map_err(|e| format!("Failed to initialize backend music client: {e}"))?;

        let results = api
            .query(SearchQuery::new(query.to_string()).with_filter(SongsFilter))
            .await
            .map_err(|e| format!("Failed to fetch backend song queue: {e}"))?;

        let queue: Vec<NowPlaying> = results
            .into_iter()
            .take(limit)
            .map(|song| {
                let duration_secs = parse_duration_str(&song.duration);
                NowPlaying {
                    video_id: song.video_id.get_raw().to_string(),
                    title: song.title,
                    artist: song.artist,
                    duration_secs,
                }
            })
            .collect();

        if queue.is_empty() {
            return Err("Backend queue seed returned zero songs".to_string());
        }

        let mut state = self.state.lock().unwrap();
        state.queue = queue;
        state.queue_index = 0;
        state.now_playing = state.queue[0].clone();
        drop(state);
        self.sync_audio_playback();

        Ok(())
    }

    pub fn set_queue(&self, songs: Vec<NowPlaying>) {
        if songs.is_empty() { return; }
        let mut state = self.state.lock().unwrap();
        state.queue = songs;
        state.queue_index = 0;
        state.now_playing = state.queue[0].clone();
        state.paused_elapsed = std::time::Duration::ZERO;
        state.track_started_at = if state.is_playing { Some(std::time::Instant::now()) } else { None };
        drop(state);
        self.sync_audio_playback();
    }

    pub fn full_queue(&self) -> Vec<NowPlaying> {
        self.state.lock().unwrap().queue.clone()
    }

    /// Returns only songs AFTER the current queue_index (i.e. upcoming, not including now-playing).
    pub fn queue_upcoming(&self) -> Vec<NowPlaying> {
        let state = self.state.lock().unwrap();
        if state.queue.is_empty() {
            return vec![];
        }
        state.queue.iter().skip(state.queue_index + 1)
            .filter(|np| np.video_id != "native-prototype")
            .cloned().collect()
    }

    pub fn get_history(&self) -> Vec<NowPlaying> {
        self.state.lock().unwrap().history.iter().cloned().collect()
    }

    pub fn elapsed_secs(&self) -> f64 {
        // While yt-dlp/cache-lookup is running, don't advance the timer.
        if self.audio_loading.load(Ordering::Relaxed) {
            return 0.0;
        }
        let state = self.state.lock().unwrap();
        let base = state.paused_elapsed.as_secs_f64();
        if let Some(started) = state.track_started_at {
            base + started.elapsed().as_secs_f64()
        } else {
            base
        }
    }

    pub fn track_duration_secs(&self) -> u32 {
        self.state.lock().unwrap().now_playing.duration_secs
    }

    /// Seek to `target_secs` within the current track.
    ///
    /// Both cached files and HTTP streams are seekable now: cached files seek via
    /// symphonia's sample index, streams via on-demand HTTP range requests
    /// (`BufferedHttpStream`). The seek is honored either way, so the UI clock is
    /// moved to match. Returns `true` when a command was dispatched to the worker.
    pub fn seek_to_secs(&self, target_secs: f64) -> bool {
        let mut state = self.state.lock().unwrap();
        let dur = state.now_playing.duration_secs as f64;
        let clamped = target_secs.max(0.0).min(if dur > 0.0 { dur } else { f64::MAX });
        let video_id = state.now_playing.video_id.clone();

        state.paused_elapsed = std::time::Duration::from_secs_f64(clamped);
        state.track_started_at = if state.is_playing { Some(std::time::Instant::now()) } else { None };

        if let Some(ref worker) = state.audio_worker {
            let _ = worker.sender.send(AudioCommand::Seek { secs: clamped, video_id });
            true
        } else {
            false
        }
    }

    pub fn set_volume(&self, v: f32) {
        let state = self.state.lock().unwrap();
        if let Some(ref worker) = state.audio_worker {
            let _ = worker.sender.send(AudioCommand::SetVolume(v.clamp(0.0, 1.0)));
        }
    }

    /// Called by the polling loop when the current song's cache download finishes.
    /// Swaps the audio worker from HTTP stream to the local file (instant seeks).
    pub fn upgrade_current_to_cache(&self, path: std::path::PathBuf) {
        let state = self.state.lock().unwrap();
        // Timer elapsed already reflects any user seeks (seek_to_secs updates it).
        let elapsed_secs = {
            let base = state.paused_elapsed.as_secs_f64();
            if let Some(started) = state.track_started_at {
                base + started.elapsed().as_secs_f64()
            } else {
                base
            }
        };
        if let Some(ref worker) = state.audio_worker {
            let _ = worker.sender.send(AudioCommand::UpgradeToCache { path, elapsed_secs });
        }
    }

    /// True when the currently-playing track is served from the local cached
    /// file rather than the HTTP stream. Used by the polling loop to decide
    /// whether a finished cache download should trigger a proactive upgrade.
    pub fn is_current_cached(&self) -> bool {
        self.current_is_cached.load(Ordering::Relaxed)
    }

    /// Returns true (and clears the flag) if the audio worker detected a track completion.
    pub fn take_advance_pending(&self) -> bool {
        self.advance_pending.swap(false, Ordering::Relaxed)
    }

    /// Returns true (and clears the flag) if the audio worker just started a new track.
    /// The polling loop calls on_audio_started() when this is true.
    pub fn take_audio_just_started(&self) -> bool {
        self.audio_just_started.swap(false, Ordering::Relaxed)
    }

    /// Returns true (and clears the flag) if the queue reached its end and autoplay songs are needed.
    pub fn take_autoplay_needed(&self) -> bool {
        self.autoplay_needed.swap(false, Ordering::Relaxed)
    }

    /// Re-arm autoplay so the polling loop pulls the next song from the autoplay
    /// queue. Called after a fresh radio batch is fetched for an exhausted queue,
    /// so playback continues instead of dead-stopping at the end of radio.
    pub fn request_autoplay(&self) {
        self.autoplay_needed.store(true, Ordering::Relaxed);
    }

    // ── Autoplay/radio queue ──────────────────────────────────────────────────
    // The autoplay queue + seed used to live as separate Arc<Mutex<…>> in the UI
    // layer. They're owned here now; the UI/poll loop go through these methods.

    /// Replace the entire autoplay queue (e.g. after fetching a fresh radio batch).
    pub fn set_autoplay_queue(&self, songs: Vec<NowPlaying>) {
        *self.autoplay_queue.lock().unwrap() = songs;
    }

    /// Snapshot of the current autoplay queue.
    pub fn autoplay_queue(&self) -> Vec<NowPlaying> {
        self.autoplay_queue.lock().unwrap().clone()
    }

    /// Empty the autoplay queue.
    pub fn clear_autoplay_queue(&self) {
        self.autoplay_queue.lock().unwrap().clear();
    }

    /// Remove and return the next autoplay song (front of the queue).
    pub fn pop_autoplay(&self) -> Option<NowPlaying> {
        let mut q = self.autoplay_queue.lock().unwrap();
        if q.is_empty() { None } else { Some(q.remove(0)) }
    }

    /// Autoplay song at `index`, if any.
    pub fn autoplay_get(&self, index: usize) -> Option<NowPlaying> {
        self.autoplay_queue.lock().unwrap().get(index).cloned()
    }

    /// Remove the autoplay song at `index` and return the resulting queue.
    pub fn remove_autoplay(&self, index: usize) -> Vec<NowPlaying> {
        let mut q = self.autoplay_queue.lock().unwrap();
        if index < q.len() { q.remove(index); }
        q.clone()
    }

    /// Drop any autoplay entry matching `video_id` (e.g. it was queued explicitly).
    pub fn autoplay_remove_id(&self, video_id: &str) {
        self.autoplay_queue.lock().unwrap().retain(|s| s.video_id != video_id);
    }

    /// Set the autoplay seed video id (the track radio continues from).
    pub fn set_autoplay_seed(&self, seed: impl Into<String>) {
        *self.autoplay_seed.lock().unwrap() = seed.into();
    }

    /// Current autoplay seed video id.
    pub fn autoplay_seed(&self) -> String {
        self.autoplay_seed.lock().unwrap().clone()
    }

    /// Returns (and clears) a stream-detected duration in seconds, or 0 if none.
    pub fn take_detected_duration(&self) -> u32 {
        self.detected_duration.swap(0, Ordering::Relaxed)
    }

    /// Backfill the current track's duration when its metadata lacked one.
    /// Updates now_playing and the matching queue entry so total-time, the
    /// progress bar, and seeking all use the real length (a 0 duration made
    /// every seek compute `fraction * 0` and jump back to the start).
    pub fn set_current_duration(&self, secs: u32) {
        if secs == 0 { return; }
        let mut state = self.state.lock().unwrap();
        if state.now_playing.duration_secs == secs { return; }
        state.now_playing.duration_secs = secs;
        let vid = state.now_playing.video_id.clone();
        for s in state.queue.iter_mut() {
            if s.video_id == vid { s.duration_secs = secs; }
        }
    }

    /// Append songs to the queue, skipping duplicates and disliked tracks.
    pub fn extend_queue(&self, songs: Vec<NowPlaying>) {
        let disliked = self.disliked_ids.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let existing_ids: std::collections::HashSet<String> = state.queue.iter().map(|s| s.video_id.clone()).collect();
        for song in songs {
            if existing_ids.contains(&song.video_id) || disliked.contains(&song.video_id) {
                continue;
            }
            state.queue.push(song);
        }
    }

    /// Set now_playing without triggering audio playback (for restoring last session).
    pub fn set_now_playing_paused(&self, np: NowPlaying) {
        let mut state = self.state.lock().unwrap();
        state.now_playing = np;
        state.is_playing = false;
        state.paused_elapsed = std::time::Duration::ZERO;
        state.track_started_at = None;
    }

    /// Called by the polling loop when audio_just_started fires.
    /// Starts the playback timer from zero at the moment audio actually begins.
    pub fn on_audio_started(&self) {
        let mut state = self.state.lock().unwrap();
        if state.is_playing {
            // Don't touch paused_elapsed — it was already set correctly by
            // next_track/prev_track/set_now_playing (to 0 for new tracks) or
            // by seek_to_secs (to the seek target). Just start the clock.
            state.track_started_at = Some(std::time::Instant::now());
        }
    }

    pub fn add_to_queue(&self, video_id: impl Into<String>, title: impl Into<String>, artist: impl Into<String>, duration_secs: u32) {
        let song = NowPlaying {
            video_id: video_id.into(),
            title: title.into(),
            artist: artist.into(),
            duration_secs,
        };
        let vid = song.video_id.clone();
        let mut state = self.state.lock().unwrap();
        // Don't add duplicates
        if !state.queue.iter().any(|s| s.video_id == song.video_id) {
            state.queue.push(song);
        }
        drop(state);
        // Pre-fetch URL for the newly queued song
        crate::core::stream_player::prefetch_stream_url(&vid);
    }

    /// Insert a song right after the currently-playing track (play next).
    pub fn play_next(&self, video_id: impl Into<String>, title: impl Into<String>, artist: impl Into<String>, duration_secs: u32) {
        let song = NowPlaying {
            video_id: video_id.into(),
            title: title.into(),
            artist: artist.into(),
            duration_secs,
        };
        let vid = song.video_id.clone();
        let mut state = self.state.lock().unwrap();
        // Remove existing duplicate if present
        if let Some(pos) = state.queue.iter().position(|s| s.video_id == song.video_id) {
            state.queue.remove(pos);
            if pos <= state.queue_index && state.queue_index > 0 {
                state.queue_index -= 1;
            }
        }
        // Insert right after current
        let insert_pos = state.queue_index + 1;
        state.queue.insert(insert_pos, song);
        drop(state);
        crate::core::stream_player::prefetch_stream_url(&vid);
    }

    /// Removes the `upcoming_index`-th song from the *upcoming* part of the queue
    /// (the list shown in the queue pane, which mirrors `queue_upcoming()`).
    /// The UI hands us an index into that upcoming list — not an absolute queue
    /// index — so we translate it, skipping the placeholder and already-played
    /// songs the same way `queue_upcoming()` does.
    pub fn remove_from_queue(&self, upcoming_index: usize) {
        let mut state = self.state.lock().unwrap();
        let start = state.queue_index + 1;
        let abs = state.queue.iter().enumerate()
            .skip(start)
            .filter(|(_, np)| np.video_id != "native-prototype")
            .nth(upcoming_index)
            .map(|(idx, _)| idx);
        if let Some(idx) = abs {
            // Upcoming songs are always after queue_index, so it stays valid.
            state.queue.remove(idx);
        }
    }

    /// Toggles the liked state of video_id, returns the new liked state.
    pub fn toggle_like(&self, video_id: &str) -> bool {
        let mut liked_ids = self.liked_ids.lock().unwrap();
        let mut liked_songs = self.liked_songs.lock().unwrap();
        if liked_ids.contains(video_id) {
            liked_ids.remove(video_id);
            liked_songs.retain(|s| s.video_id != video_id);
            drop(liked_ids);
            drop(liked_songs);
            self.persist();
            false
        } else {
            liked_ids.insert(video_id.to_string());
            // Get metadata from state
            let state = self.state.lock().unwrap();
            let song = state.queue.iter()
                .chain(state.history.iter())
                .find(|s| s.video_id == video_id)
                .cloned()
                .unwrap_or(NowPlaying {
                    video_id: video_id.to_string(),
                    title: String::new(),
                    artist: String::new(),
                    duration_secs: 0,
                });
            drop(state);
            liked_songs.push(song);
            drop(liked_ids);
            drop(liked_songs);
            self.persist();
            true
        }
    }

    /// Unlike a specific song (remove from liked).
    pub fn unlike(&self, video_id: &str) {
        self.liked_ids.lock().unwrap().remove(video_id);
        self.liked_songs.lock().unwrap().retain(|s| s.video_id != video_id);
        self.persist();
    }

    pub fn is_liked(&self, video_id: &str) -> bool {
        self.liked_ids.lock().unwrap().contains(video_id)
    }

    /// Returns liked songs with full metadata (persisted).
    pub fn get_liked_songs(&self) -> Vec<NowPlaying> {
        self.liked_songs.lock().unwrap().clone()
    }

    /// Add a song to the disliked list (taste profile exclusion).
    pub fn dislike(&self, video_id: &str, title: &str, artist: &str, duration_secs: u32) {
        let mut disliked = self.disliked_ids.lock().unwrap();
        disliked.insert(video_id.to_string());
        drop(disliked);
        // Also unlike if it was liked
        self.liked_ids.lock().unwrap().remove(video_id);
        self.liked_songs.lock().unwrap().retain(|s| s.video_id != video_id);
        // Store the metadata for persistence
        let song = NowPlaying { video_id: video_id.to_string(), title: title.to_string(), artist: artist.to_string(), duration_secs };
        // We pass it through persist
        let _ = song; // metadata stored via persist's all_songs scan
        self.persist();
    }

    pub fn is_disliked(&self, video_id: &str) -> bool {
        self.disliked_ids.lock().unwrap().contains(video_id)
    }

    /// Persist history, liked, disliked to disk.
    fn persist(&self) {
        let state = self.state.lock().unwrap();
        let history: Vec<NowPlaying> = state.history.iter().cloned().collect();
        let all_songs: Vec<NowPlaying> = state.queue.iter()
            .chain(state.history.iter())
            .cloned()
            .collect();
        drop(state);
        let liked_songs = self.liked_songs.lock().unwrap();
        let liked: Vec<(String, NowPlaying)> = liked_songs.iter().map(|s| (s.video_id.clone(), s.clone())).collect();
        drop(liked_songs);
        let disliked = self.disliked_ids.lock().unwrap().clone();
        // Save in background to not block
        std::thread::spawn(move || {
            persistence::save_history(&history, &liked, &disliked, &all_songs);
        });
    }

    pub fn status_label(&self) -> String {
        if self.is_playing() {
            "Playing".to_string()
        } else {
            "Paused".to_string()
        }
    }

    pub fn play_pause_label(&self) -> String {
        if self.is_playing() {
            "Pause".to_string()
        } else {
            "Play".to_string()
        }
    }
}
