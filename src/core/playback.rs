use std::sync::{mpsc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
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
}

enum AudioCommand {
    SetTrack(String),
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
}

fn log_native_audio_error(stage: &str, code: &str, video_id: &str, detail: &str) {
    let message = format!(
        "[native-audio][stage={stage}][code={code}] video_id={video_id} detail={detail}"
    );
    log::error!("{message}");
    eprintln!("{message}");
}

fn create_audio_engine() -> Result<AudioEngine, String> {
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
    })
}

fn spawn_stream_fetch(events: mpsc::Sender<AudioEvent>, request_id: u64, video_id: String) {
    thread::spawn(move || {
        // 1. Cache hit?
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

fn load_stream_into_engine(engine: &mut AudioEngine, video_id: &str, url: &str, fallback_content_len: Option<u64>) -> Result<(), String> {
    let source = StreamingAudioSource::from_url(url).map_err(|e| {
        log_native_audio_error("decode", "stream-open-failed", video_id, &e);
        e
    })?;

    // Use content_len from the stream response, fall back to the HEAD-fetched value.
    let content_len = source.content_len().or(fallback_content_len);
    let replacement_sink = Sink::try_new(&engine.handle).map_err(|e| {
        let err = format!("Failed to replace native audio sink: {e}");
        log_native_audio_error("playback", "sink-replace-failed", video_id, &err);
        err
    })?;

    replacement_sink.set_volume(engine.volume);
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
) -> Result<AudioWorker, String> {
    let (sender, receiver) = mpsc::channel::<AudioCommand>();
    let (event_sender, event_receiver) = mpsc::channel::<AudioEvent>();

    thread::Builder::new()
        .name("ytm-native-audio".to_string())
        .spawn(move || {
            let mut engine = match create_audio_engine() {
                Ok(e) => e,
                Err(err) => {
                    log::error!("Native audio initialization failed: {err}");
                    return;
                }
            };

            let mut requested_video: Option<String> = None;
            let mut is_playing = false;
            let mut latest_request_id: u64 = 0;
            // Seek target deferred until cache upgrade completes (stream-only tracks).
            let mut pending_seek: Option<f64> = None;
            let mut sink_empty_count: u32 = 0;
            // Track elapsed time inside the worker for cache-fallback seeks
            let mut playback_started_at: Option<std::time::Instant> = None;
            let mut playback_base_secs: f64 = 0.0;

            loop {
                match receiver.recv_timeout(Duration::from_millis(80)) {
                    Ok(command) => match command {
                        AudioCommand::SetTrack(video_id) => {
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
                                latest_request_id = latest_request_id.saturating_add(1);
                                spawn_stream_fetch(event_sender.clone(), latest_request_id, video_id);
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
                                } else {
                                    engine.sink.stop();
                                    engine.current_video_id = None;
                                    audio_loading_flag.store(true, Ordering::Relaxed);
                                    audio_just_started_flag.store(false, Ordering::Relaxed);
                                    latest_request_id = latest_request_id.saturating_add(1);
                                    spawn_stream_fetch(event_sender.clone(), latest_request_id, video_id.clone());
                                }
                            }
                        }
                        AudioCommand::UpgradeToCache { path, elapsed_secs } => {
                            if engine.current_is_cached { continue; }
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
                            // Update worker-local elapsed tracking
                            playback_base_secs = secs;
                            playback_started_at = if is_playing { Some(std::time::Instant::now()) } else { None };
                            // Priority 1: file is in cache — load it and seek by baking
                            // position into the source before appending to sink.
                            let cached_path = AudioCache::global().lock().ok()
                                .and_then(|mut c| c.get(&video_id));
                            if let Some(path) = cached_path {
                                match load_file_into_engine(&mut engine, &video_id, &path, Some(secs)) {
                                    Ok(()) => {
                                        is_cached_flag.store(true, Ordering::Relaxed);
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
                                        if let Err(e) = load_file_into_engine(&mut engine, &cached_vid, &path, Some(secs)) {
                                            eprintln!("[seek-cached] reload+seek failed: {e}");
                                        } else if !is_playing {
                                            engine.sink.pause();
                                        }
                                    }
                                }
                            } else {
                                // Can't seek an HTTP stream (moov at byte 0). Store target
                                // so UpgradeToCache uses it when cache download finishes.
                                pending_seek = Some(secs);
                                eprintln!("[seek] stream-only — deferred to {secs:.1}s, awaiting cache");
                            }
                        }
                    },
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
                                    if let Err(err) = load_stream_into_engine(&mut engine, &video_id, &url, content_len) {
                                        log_native_audio_error("playback", "stream-load-failed", &video_id, &err);
                                    } else {
                                        is_cached_flag.store(false, Ordering::Relaxed);
                                        audio_loading_flag.store(false, Ordering::Relaxed);
                                        audio_just_started_flag.store(true, Ordering::Relaxed);
                                        sink_empty_count = 0;
                                        playback_base_secs = 0.0;
                                        playback_started_at = Some(std::time::Instant::now());
                                    }
                                }
                                Ok(AudioSource::CachedFile(path)) => {
                                    if let Err(err) = load_file_into_engine(&mut engine, &video_id, &path, None) {
                                        log_native_audio_error("playback", "cache-load-failed", &video_id, &err);
                                        // Fall back to streaming
                                        latest_request_id = latest_request_id.saturating_add(1);
                                        spawn_stream_fetch(event_sender.clone(), latest_request_id, video_id);
                                    } else {
                                        is_cached_flag.store(true, Ordering::Relaxed);
                                        audio_loading_flag.store(false, Ordering::Relaxed);
                                        audio_just_started_flag.store(true, Ordering::Relaxed);
                                        sink_empty_count = 0;
                                        playback_base_secs = 0.0;
                                        playback_started_at = Some(std::time::Instant::now());
                                    }
                                }
                                Err(err) => {
                                    log_native_audio_error("extract", "stream-url-failed", &video_id, &err);
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
                        if let Some(ref vid) = engine.current_video_id.clone() {
                            let cached_path = crate::core::cache::AudioCache::global().lock().ok()
                                .and_then(|mut c| c.get(vid));
                            if let Some(path) = cached_path {
                                let elapsed = playback_base_secs
                                    + playback_started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
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
        )?);
        state.audio_enabled = true;
        Ok(())
    }

    fn sync_audio_playback(&self) {
        let (audio_enabled, is_playing, video_id, sender) = {
            let state = self.state.lock().unwrap();
            (
                state.audio_enabled,
                state.is_playing,
                state.now_playing.video_id.clone(),
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

            if sender.send(AudioCommand::SetTrack(video_id)).is_err() {
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
        self.prefetch_next();
    }

    pub fn prev_track(&self) {
        let mut state = self.state.lock().unwrap();
        if state.queue.is_empty() { return; }
        if state.queue_index == 0 {
            state.queue_index = state.queue.len().saturating_sub(1);
        } else {
            state.queue_index -= 1;
        }
        state.now_playing = state.queue[state.queue_index].clone();
        // Reset timer so progress bar starts from 0
        state.paused_elapsed = std::time::Duration::ZERO;
        state.track_started_at = if state.is_playing { Some(std::time::Instant::now()) } else { None };
        drop(state);
        self.sync_audio_playback();
        self.prefetch_next();
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
        self.prefetch_next();
    }

    /// Pre-fetch the stream URL for the next track in queue (background thread).
    fn prefetch_next(&self) {
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
    /// For cached files: `sink.try_seek()` uses symphonia's exact sample index.
    /// For HTTP streams: `sink.try_seek()` works via Range-request re-connection
    /// (HttpStream::seek() makes a `Range: bytes=N-` GET — no new yt-dlp call needed).
    pub fn seek_to_secs(&self, target_secs: f64) {
        let mut state = self.state.lock().unwrap();
        let dur = state.now_playing.duration_secs as f64;
        let clamped = target_secs.max(0.0).min(if dur > 0.0 { dur } else { f64::MAX });
        let video_id = state.now_playing.video_id.clone();
        state.paused_elapsed = std::time::Duration::from_secs_f64(clamped);
        if state.is_playing {
            state.track_started_at = Some(std::time::Instant::now());
        }
        if let Some(ref worker) = state.audio_worker {
            let _ = worker.sender.send(AudioCommand::Seek { secs: clamped, video_id });
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

    /// Spawn background downloads for the next 2 queued songs if not yet cached.
    /// Call this once after the current song starts (e.g. when progress crosses 25%).
    pub fn cache_next_song_if_needed(&self) {
        let songs: Vec<NowPlaying> = {
            let state = self.state.lock().unwrap();
            (1..=2)
                .filter_map(|offset| state.queue.get(state.queue_index + offset).cloned())
                .collect()
        };
        for song in songs {
            crate::core::cache::spawn_prefetch(song.video_id, song.title, song.artist);
        }
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

    pub fn remove_from_queue(&self, index: usize) {
        let mut state = self.state.lock().unwrap();
        if index >= state.queue.len() { return; }
        let removed_id = state.queue[index].video_id.clone();
        state.queue.remove(index);
        // Adjust queue_index
        if state.queue.is_empty() {
            state.queue_index = 0;
        } else if removed_id == state.now_playing.video_id {
            state.queue_index = state.queue_index.min(state.queue.len() - 1);
        } else if index < state.queue_index {
            state.queue_index = state.queue_index.saturating_sub(1);
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
