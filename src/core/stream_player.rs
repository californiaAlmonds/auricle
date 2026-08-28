/// Streaming audio player.
///
/// Architecture:
///   1. `yt-dlp -g` extracts a signed CDN URL (~2s) — no download, no ffmpeg.
///   2. `StreamingAudioSource` opens an HTTP connection and feeds packets to symphonia.
///   3. Symphonia decodes AAC/Opus packets on-the-fly; samples are emitted to rodio.
///
/// Seeking strategy for streams:
///   `from_url` always uses `is_seekable: false` so symphonia probes safely without
///   trying to seek backward during initialization (which caused the unreachable!() panic
///   in rodio's symphonia wrapper).
///
///   User-initiated seeks are handled at the audio-worker level:
///   - Cached files (.m4a): `sink.try_seek()` works natively via rodio's file decoder.
///   - Streaming: `from_url_at_byte(url, byte_offset)` opens a new HTTP `Range: bytes=N-`
///     connection at the approximate byte offset (`content_len × fraction`) and creates a
///     fresh `StreamingAudioSource` from there.  For AAC at constant bitrate this gives
///     sub-second accuracy; symphonia syncs to the next keyframe automatically.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Serialize, Deserialize};
use reqwest::blocking::Client;
use rodio::Source;
use symphonia::core::{
    audio::SampleBuffer,
    codecs::{DecoderOptions, CODEC_TYPE_NULL},
    formats::{FormatOptions, FormatReader},
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
    probe::Hint,
};

// ---------------------------------------------------------------------------
// In-memory CDN URL cache — avoids re-running yt-dlp for recent songs
// ---------------------------------------------------------------------------

const URL_CACHE_TTL_SECS: u64 = 6 * 3600; // CDN URLs are valid for ~6h

#[derive(Clone, Serialize, Deserialize)]
struct UrlCacheEntry {
    url: String,
    /// Wall-clock unix seconds when the URL was resolved. Persisted so the cache
    /// survives restarts (an `Instant` can't be meaningfully serialized).
    fetched_at_unix: u64,
}

static URL_CACHE: OnceLock<Mutex<HashMap<String, UrlCacheEntry>>> = OnceLock::new();

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Disk location of the persisted URL cache: `%LOCALAPPDATA%\auricle\url_cache.json`.
fn url_cache_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
        base.join("auricle").join("url_cache.json")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
        home.join(".auricle").join("url_cache.json")
    }
}

/// Loads the persisted cache, dropping any entries already past the TTL.
fn load_url_cache_from_disk() -> HashMap<String, UrlCacheEntry> {
    let map: HashMap<String, UrlCacheEntry> = std::fs::read_to_string(url_cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let now = unix_now();
    map.into_iter()
        .filter(|(_, e)| now.saturating_sub(e.fetched_at_unix) < URL_CACHE_TTL_SECS)
        .collect()
}

/// Writes the whole cache to disk. Called on every mutation — the file is tiny
/// (one short line per recently-played song) so this is cheap.
fn save_url_cache(map: &HashMap<String, UrlCacheEntry>) {
    let path = url_cache_path();
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).ok(); }
    if let Ok(json) = serde_json::to_string(map) {
        std::fs::write(&path, json).ok();
    }
}

fn url_cache() -> &'static Mutex<HashMap<String, UrlCacheEntry>> {
    URL_CACHE.get_or_init(|| Mutex::new(load_url_cache_from_disk()))
}

/// Returns the cached CDN URL for `video_id` if it was fetched within the TTL.
pub fn get_cached_url(video_id: &str) -> Option<String> {
    let cache = url_cache().lock().ok()?;
    let entry = cache.get(video_id)?;
    if unix_now().saturating_sub(entry.fetched_at_unix) < URL_CACHE_TTL_SECS {
        Some(entry.url.clone())
    } else {
        None
    }
}

fn store_cached_url(video_id: &str, url: String) {
    if let Ok(mut cache) = url_cache().lock() {
        cache.insert(video_id.to_string(), UrlCacheEntry {
            url,
            fetched_at_unix: unix_now(),
        });
        save_url_cache(&cache);
    }
}

/// Drops any cached CDN URL for `video_id`, forcing the next `get_stream_url`
/// call to re-run yt-dlp for a freshly-signed URL. Called when a stream open
/// fails with an auth/expiry error (HTTP 401/403) so we don't keep reusing a
/// stale signed URL — the common cause of a prefetched radio song not starting.
pub fn invalidate_cached_url(video_id: &str) {
    if let Ok(mut cache) = url_cache().lock() {
        if cache.remove(video_id).is_some() {
            save_url_cache(&cache);
        }
    }
}

/// Drops every cached CDN URL. Called after yt-dlp is updated: YouTube binds a
/// signed URL to the player client that produced it, and URLs from an outdated
/// yt-dlp are served only in part (HTTP 403 past the first ~1 MiB). Without this
/// the 6h TTL would keep handing out those poisoned URLs after an update.
pub fn clear_url_cache() {
    if let Ok(mut cache) = url_cache().lock() {
        if !cache.is_empty() {
            cache.clear();
            save_url_cache(&cache);
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP request helpers
// ---------------------------------------------------------------------------

/// Realistic desktop User-Agent for googlevideo CDN requests.
const STREAM_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Bytes pulled from the network per read-ahead chunk.
const READ_CHUNK: usize = 64 * 1024;
/// Chunks the background reader may buffer ahead (bounds memory).
/// 96 × 64 KiB ≈ 6 MiB — enough to hold a whole ~4-min AAC track in RAM.
const READ_AHEAD_CHUNKS: usize = 96;
/// Max consecutive reconnect attempts (no forward progress) before giving up.
const MAX_RECONNECTS: u32 = 6;

/// Returns `url` with its googlevideo `range` query parameter set to `start-`.
/// googlevideo uses the `range` *query parameter* (not the HTTP Range header)
/// for partial content, so resuming a truncated stream means rewriting it.
fn set_range_param(url: &str, start: u64) -> String {
    match url.split_once('?') {
        Some((base, query)) => {
            let mut out = String::with_capacity(url.len() + 24);
            out.push_str(base);
            out.push('?');
            let mut sep = "";
            for pair in query.split('&') {
                if pair.split('=').next() == Some("range") { continue; }
                out.push_str(sep);
                out.push_str(pair);
                sep = "&";
            }
            if !sep.is_empty() { out.push('&'); }
            out.push_str(&format!("range={start}-"));
            out
        }
        None => format!("{url}?range={start}-"),
    }
}

/// Opens a GET for `url`, rewriting the googlevideo `range` param to resume at
/// `start` bytes when `start > 0`.
fn open_stream_response(client: &Client, url: &str, start: u64) -> io::Result<reqwest::blocking::Response> {
    let target = if start == 0 { url.to_string() } else { set_range_param(url, start) };
    let resp = client
        .get(&target)
        .header("User-Agent", STREAM_UA)
        .header("Referer", "https://www.youtube.com/")
        .header("Origin", "https://www.youtube.com")
        .send()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("stream GET failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(io::Error::new(io::ErrorKind::Other, format!("stream GET HTTP {}", resp.status())));
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Write-through cache tee
//
// As the streaming reader pulls bytes for playback it can also write them to the
// cache staging file, so a song played start-to-finish is cached WITHOUT a second
// download. Only the initial (byte-0) reader tees — seeks/reconnects at arbitrary
// offsets never tee (they'd produce a non-contiguous file). The tee commits to the
// cache on a clean, complete EOF and discards the partial file on any interruption.
// ---------------------------------------------------------------------------

struct TeeWriter {
    file: std::fs::File,
    staging_path: PathBuf,
    content_len: u64,
    write_pos: u64,
    video_id: String,
    title: String,
    artist: String,
}

impl TeeWriter {
    /// Creates the staging file for `video_id`. Returns `None` if the file already
    /// exists (a background cache download is already writing it) or the cache dir
    /// is unavailable — streaming then proceeds without teeing and the normal cache
    /// path handles the download.
    fn create(video_id: &str, title: &str, artist: &str, content_len: u64) -> Option<TeeWriter> {
        let staging = crate::core::cache::AudioCache::global().lock().ok()?.staging_path(video_id);
        // create_new: never clobber an in-progress download's staging file.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .ok()?;
        Some(TeeWriter {
            file,
            staging_path: staging,
            content_len,
            write_pos: 0,
            video_id: video_id.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
        })
    }

    fn write(&mut self, data: &[u8]) {
        use std::io::Write;
        if self.file.write_all(data).is_ok() {
            self.write_pos += data.len() as u64;
        }
    }

    /// Called on a clean end-of-stream: commit to the cache if the whole file was
    /// written, otherwise discard the partial staging file.
    fn finalize(mut self) {
        use std::io::Write;
        let _ = self.file.flush();
        drop(self.file); // close before commit renames the .part file (Windows)
        if self.content_len > 0 && self.write_pos >= self.content_len {
            if let Ok(mut cache) = crate::core::cache::AudioCache::global().lock() {
                if cache.commit(&self.video_id, &self.title, &self.artist).is_some() {
                    eprintln!("[write-through] cached {} ({} bytes) from stream", self.video_id, self.write_pos);
                }
            }
        } else {
            std::fs::remove_file(&self.staging_path).ok();
        }
    }

    /// Called when the stream was interrupted (seek/skip/error): discard the file.
    fn abort(self) {
        drop(self.file);
        std::fs::remove_file(&self.staging_path).ok();
    }
}

// ---------------------------------------------------------------------------
// Buffered, self-healing HTTP MediaSource
//
// googlevideo frequently truncates a single GET (delivering only a few MB before
// closing the connection). A background thread reads ahead into a bounded channel
// and transparently reconnects (`range=<pos>-`) whenever the CDN closes early.
//
// Seeking: is_seekable() returns FALSE during probe (linear box discovery, safe
// because YouTube M4A/AAC streams are always faststart — moov at byte 0).
// After probe_and_build returns, from_url flips probe_complete to true so
// is_seekable() returns true, enabling user-initiated seeks via HTTP range
// requests without causing CDN connection failures during the initial probe.
// ---------------------------------------------------------------------------

struct BufferedHttpStream {
    client: Client,
    url: String,
    content_len: Option<u64>,
    rx: Mutex<std::sync::mpsc::Receiver<io::Result<Vec<u8>>>>,
    chunk: Vec<u8>,
    chunk_pos: usize,
    pos: u64,
    finished: bool,
    /// False during probe (set by `from_url` after `probe_and_build` returns).
    probe_complete: std::sync::Arc<AtomicBool>,
}

impl BufferedHttpStream {
    /// Takes an already-opened initial response (so the caller can validate the
    /// initial HTTP status synchronously) plus the client + URL used to
    /// reconnect on premature EOF and to re-fetch after a seek. `tee`, when set,
    /// write-through-caches the streamed bytes (initial byte-0 reader only).
    fn new(client: Client, url: String, initial: reqwest::blocking::Response, content_len: Option<u64>, probe_complete: std::sync::Arc<AtomicBool>, tee: Option<TeeWriter>) -> Self {
        let rx = Self::spawn_reader(client.clone(), url.clone(), 0, content_len, Some(initial), tee);
        BufferedHttpStream {
            client,
            url,
            content_len,
            rx: Mutex::new(rx),
            chunk: Vec::new(),
            chunk_pos: 0,
            pos: 0,
            finished: false,
            probe_complete,
        }
    }

    /// Spawns a reader thread that streams bytes from `start` into a bounded
    /// channel, reconnecting (`range=<pos>-`) on premature EOF. When `initial`
    /// is provided it is used for the first connection (avoids a redundant GET).
    /// `tee` (initial reader only) write-through-caches bytes as they stream.
    fn spawn_reader(
        client: Client,
        url: String,
        start: u64,
        content_len: Option<u64>,
        initial: Option<reqwest::blocking::Response>,
        tee: Option<TeeWriter>,
    ) -> std::sync::mpsc::Receiver<io::Result<Vec<u8>>> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<io::Result<Vec<u8>>>(READ_AHEAD_CHUNKS);
        std::thread::Builder::new()
            .name("ytm-stream-reader".to_string())
            .spawn(move || {
                // Stay ahead of the decoder: if read-ahead falls behind under load
                // the decode thread starves no matter how high its own priority is.
                crate::core::audio_priority::raise_current_thread();
                let mut pos: u64 = start;
                let mut reconnects: u32 = 0;
                let mut tee = tee;
                let mut resp = match initial {
                    Some(r) => r,
                    None => match open_stream_response(&client, &url, start) {
                        Ok(r) => r,
                        Err(e) => { if let Some(t) = tee.take() { t.abort(); } let _ = tx.send(Err(e)); return; }
                    },
                };
                loop {
                    let mut chunk = vec![0u8; READ_CHUNK];
                    match resp.read(&mut chunk) {
                        Ok(0) => {
                            // EOF — if the CDN closed early, reconnect and continue.
                            // Unknown length (chunked) is treated as possibly truncated;
                            // the reconnect budget bounds probing at a genuine EOF.
                            let truncated = content_len.map_or(true, |total| pos < total);
                            if truncated && reconnects < MAX_RECONNECTS {
                                reconnects += 1;
                                match open_stream_response(&client, &url, pos) {
                                    Ok(r) => { resp = r; continue; }
                                    Err(e) => { if let Some(t) = tee.take() { t.abort(); } let _ = tx.send(Err(e)); break; }
                                }
                            }
                            // Genuine end of stream — finalize the write-through tee.
                            if let Some(t) = tee.take() { t.finalize(); }
                            break;
                        }
                        Ok(n) => {
                            reconnects = 0; // made progress — reset the budget
                            pos += n as u64;
                            chunk.truncate(n);
                            // Write-through BEFORE handing the chunk to playback; the
                            // read-ahead buffer absorbs the brief disk latency.
                            if let Some(t) = tee.as_mut() { t.write(&chunk); }
                            if tx.send(Ok(chunk)).is_err() {
                                // Receiver dropped (song skipped / seeked) — discard partial.
                                if let Some(t) = tee.take() { t.abort(); }
                                break;
                            }
                        }
                        Err(e) => {
                            // Transient network error — try to resume from pos.
                            if pos > start && reconnects < MAX_RECONNECTS {
                                reconnects += 1;
                                if let Ok(r) = open_stream_response(&client, &url, pos) {
                                    resp = r;
                                    continue;
                                }
                            }
                            if let Some(t) = tee.take() { t.abort(); }
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            })
            .ok();
        rx
    }
}

impl Read for BufferedHttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.chunk_pos >= self.chunk.len() {
            if self.finished { return Ok(0); }
            let next = self.rx.lock().unwrap().recv();
            match next {
                Ok(Ok(chunk)) => { self.chunk = chunk; self.chunk_pos = 0; }
                Ok(Err(e)) => { self.finished = true; return Err(e); }
                Err(_) => { self.finished = true; return Ok(0); } // reader done → EOF
            }
        }
        let avail = &self.chunk[self.chunk_pos..];
        let n = avail.len().min(buf.len());
        buf[..n].copy_from_slice(&avail[..n]);
        self.chunk_pos += n;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for BufferedHttpStream {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => (self.pos as i64).saturating_add(d).max(0) as u64,
            SeekFrom::End(d) => {
                let len = self.content_len.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "seek from end: unknown length")
                })?;
                (len as i64).saturating_add(d).max(0) as u64
            }
        };
        // Restart the reader at `target`. Dropping the old receiver makes the
        // previous reader thread exit on its next send. Seek readers never tee
        // (a mid-file start would produce a non-contiguous cache file).
        let rx = Self::spawn_reader(self.client.clone(), self.url.clone(), target, self.content_len, None, None);
        *self.rx.lock().unwrap() = rx;
        self.chunk.clear();
        self.chunk_pos = 0;
        self.pos = target;
        self.finished = false;
        Ok(target)
    }
}

impl MediaSource for BufferedHttpStream {
    /// False during probe (linear box discovery, safe for YouTube faststart M4A).
    /// True after probe — allows user-initiated seeks via HTTP range requests.
    fn is_seekable(&self) -> bool { self.probe_complete.load(Ordering::Relaxed) }
    fn byte_len(&self) -> Option<u64> { self.content_len }
}

// ---------------------------------------------------------------------------
// Seekable file-backed MediaSource — used for cached .m4a playback
// ---------------------------------------------------------------------------

struct FileSource {
    file: std::fs::File,
    len: u64,
}

impl Read for FileSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Seek for FileSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl MediaSource for FileSource {
    fn is_seekable(&self) -> bool { true }
    fn byte_len(&self) -> Option<u64> { Some(self.len) }
}



// ---------------------------------------------------------------------------
// Public streaming source (implements rodio::Source)
// ---------------------------------------------------------------------------

pub struct StreamingAudioSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    sample_buf: Vec<i16>,
    sample_pos: usize,
    channels: u16,
    sample_rate: u32,
    content_len: Option<u64>,
    /// True when the underlying source is a seekable file (not an HTTP stream).
    seekable: bool,
    /// Reused interleaved sample buffer — avoids a per-packet allocation.
    decode_buf: Option<SampleBuffer<i16>>,
    /// Frame capacity of `decode_buf` and the spec it was built for.
    decode_cap: u64,
    decode_spec: Option<(u32, usize)>,
}

impl StreamingAudioSource {
    /// Open `url` and set up symphonia decoding.
    /// Returns immediately once format probing succeeds (~50 ms for AAC/MP4).
    pub fn from_url(url: &str) -> Result<Self, String> {
        Self::from_url_inner(url, None)
    }

    /// Like [`from_url`], but write-through-caches the streamed bytes to the cache
    /// staging file, so a song played to completion is cached with no second
    /// download. Teeing is skipped automatically when the length is unknown or a
    /// cache download for this song is already in flight.
    pub fn from_url_teed(url: &str, video_id: &str, title: &str, artist: &str) -> Result<Self, String> {
        Self::from_url_inner(url, Some((video_id.to_string(), title.to_string(), artist.to_string())))
    }

    fn from_url_inner(url: &str, tee_meta: Option<(String, String, String)>) -> Result<Self, String> {
        // Signed CDN URLs often require &range=0- to avoid 403
        let url = if url.contains("googlevideo.com") && !url.contains("&range=") {
            format!("{}&range=0-", url)
        } else {
            url.to_string()
        };

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP client init failed: {e}"))?;

        let resp = open_stream_response(&client, &url, 0)
            .map_err(|e| format!("HTTP request failed: {e}"))?;

        let content_len = resp.content_length();
        // Set up write-through only when we know the total length (needed to tell a
        // complete download from a truncated one) and a staging file can be freshly
        // created (no competing download already writing it).
        let tee = match (tee_meta, content_len) {
            (Some((vid, title, artist)), Some(total)) => TeeWriter::create(&vid, &title, &artist, total),
            _ => None,
        };
        let probe_complete = std::sync::Arc::new(AtomicBool::new(false));
        let media_source = BufferedHttpStream::new(client, url, resp, content_len, probe_complete.clone(), tee);
        // seekable=true so the StreamingAudioSource will attempt range-based seeks
        // after probe; probe_complete starts false so probe itself is linear.
        let result = Self::probe_and_build(Box::new(media_source), content_len, true)?;
        // Flip the probe flag: is_seekable() now returns true for this stream,
        // enabling user-initiated seeks without re-running yt-dlp.
        probe_complete.store(true, Ordering::Relaxed);
        Ok(result)
    }

    /// Open a local cached .m4a file for playback.
    /// Uses our symphonia path (enable_gapless: false) to avoid rodio's panic.
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("Cache file open error: {e}"))?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let source = FileSource { file, len };
        Self::probe_and_build(Box::new(source), None, true)
    }

    fn probe_and_build(media_source: Box<dyn MediaSource>, fallback_content_len: Option<u64>, seekable: bool) -> Result<Self, String> {
        let content_len = media_source.byte_len().or(fallback_content_len);
        let mss = MediaSourceStream::new(media_source, Default::default());
        let hint = Hint::new();

        // CRITICAL: enable_gapless: false prevents the seek-during-init that
        // rodio's symphonia wrapper treats as unreachable!(), causing the panic.
        let format_opts = FormatOptions {
            enable_gapless: false,
            ..Default::default()
        };

        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &format_opts, &MetadataOptions::default())
            .map_err(|e| format!("Symphonia probe failed: {e}"))?;

        let format = probed.format;

        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| "No supported audio track in stream".to_string())?;

        let track_id = track.id;
        let channels = track
            .codec_params
            .channels
            .map(|c| c.count() as u16)
            .unwrap_or(2);
        let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);

        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| format!("Symphonia decoder init failed: {e}"))?;

        Ok(StreamingAudioSource {
            format,
            decoder,
            track_id,
            sample_buf: Vec::new(),
            sample_pos: 0,
            channels,
            sample_rate,
            content_len,
            seekable,
            decode_buf: None,
            decode_cap: 0,
            decode_spec: None,
        })
    }

    /// Total byte size of the stream — used to compute Range offsets for seeking.
    pub fn content_len(&self) -> Option<u64> {
        self.content_len
    }

    /// Total track duration in whole seconds, derived from the container's sample
    /// table. Used to backfill `duration_secs` for songs whose metadata omitted it
    /// (otherwise total-time shows "--:--" and seeking divides by zero → jumps to 0).
    pub fn duration_secs(&self) -> Option<u32> {
        let track = self.format.tracks().iter().find(|t| t.id == self.track_id)?;
        let params = &track.codec_params;
        let n_frames = params.n_frames?;
        if let Some(tb) = params.time_base {
            let t = tb.calc_time(n_frames);
            Some(t.seconds as u32 + if t.frac >= 0.5 { 1 } else { 0 })
        } else {
            let sr = params.sample_rate? as u64;
            if sr == 0 { None } else { Some((n_frames / sr) as u32) }
        }
    }

    /// Seek the symphonia reader to `secs` in-place.
    /// Only valid when `seekable == true` (i.e. created via `from_file`).
    pub fn seek_to(&mut self, secs: f64) -> Result<(), String> {
        if !self.seekable {
            return Err("source is not seekable (HTTP stream)".to_string());
        }
        use symphonia::core::formats::{SeekMode, SeekTo};
        use symphonia::core::units::Time;
        let seconds = secs.max(0.0) as u64;
        let frac = secs.max(0.0).fract();
        self.format
            .seek(SeekMode::Accurate, SeekTo::Time {
                time: Time { seconds, frac },
                track_id: Some(self.track_id),
            })
            .map_err(|e| format!("symphonia seek failed: {e}"))?;
        self.sample_buf.clear();
        self.sample_pos = 0;
        self.decoder.reset();
        Ok(())
    }

    fn fill_next_packet(&mut self) -> bool {
        // This runs on cpal's WASAPI stream thread, which cpal leaves at normal
        // priority. Claim real-time audio scheduling the first time we decode on
        // this thread so a busy foreground app (e.g. a game) can't preempt us
        // past the buffer deadline. Idempotent and near-free after the first call.
        crate::core::audio_priority::register_audio_thread();
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(_) => return false,
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let frames = decoded.capacity() as u64;
                    let spec_key = (spec.rate, spec.channels.count());
                    // Rebuild the reusable buffer only when the spec changes or a
                    // larger packet arrives; otherwise reuse it in place.
                    if self.decode_buf.is_none()
                        || self.decode_spec != Some(spec_key)
                        || self.decode_cap < frames
                    {
                        self.decode_buf = Some(SampleBuffer::<i16>::new(frames, spec));
                        self.decode_cap = frames;
                        self.decode_spec = Some(spec_key);
                    }
                    let buf = self.decode_buf.as_mut().unwrap();
                    buf.copy_interleaved_ref(decoded);
                    self.sample_buf.clear();
                    self.sample_buf.extend_from_slice(buf.samples());
                    self.sample_pos = 0;
                    return true;
                }
                Err(_) => continue, // decode errors are non-fatal in symphonia
            }
        }
    }
}

impl Iterator for StreamingAudioSource {
    type Item = i16;

    fn next(&mut self) -> Option<i16> {
        loop {
            if self.sample_pos < self.sample_buf.len() {
                let s = self.sample_buf[self.sample_pos];
                self.sample_pos += 1;
                return Some(s);
            }
            if !self.fill_next_packet() {
                return None;
            }
        }
    }
}

impl Source for StreamingAudioSource {
    fn current_frame_len(&self) -> Option<usize> {
        let remaining = self.sample_buf.len().saturating_sub(self.sample_pos);
        if remaining == 0 { None } else { Some(remaining) }
    }
    fn channels(&self) -> u16 { self.channels }
    fn sample_rate(&self) -> u32 { self.sample_rate }
    fn total_duration(&self) -> Option<Duration> { None }

    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        if !self.seekable {
            return Err(rodio::source::SeekError::NotSupported {
                underlying_source: std::any::type_name::<Self>(),
            });
        }
        use symphonia::core::formats::{SeekMode, SeekTo};
        use symphonia::core::units::Time;
        let seconds = pos.as_secs();
        let frac = pos.subsec_nanos() as f64 / 1_000_000_000.0;
        self.format
            .seek(SeekMode::Accurate, SeekTo::Time {
                time: Time { seconds, frac },
                track_id: Some(self.track_id),
            })
            .map_err(|e| {
                eprintln!("[stream-seek] format.seek({}s) failed: {e}", pos.as_secs());
                rodio::source::SeekError::NotSupported {
                    underlying_source: std::any::type_name::<Self>(),
                }
            })?;
        // Clear stale samples and reset decoder state after seek.
        self.sample_buf.clear();
        self.sample_pos = 0;
        self.decoder.reset();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// URL extraction via yt-dlp -g
// ---------------------------------------------------------------------------

fn resolve_ytdlp() -> PathBuf {
    crate::core::addons::resolve_tool("yt-dlp")
}

/// Names of browsers whose cookie database exists on this machine, in priority order.
/// Shared by the streaming path and the cache download path.
pub fn detected_cookie_browsers() -> Vec<&'static str> {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let mut out = Vec::new();
    if std::path::Path::new(&local).join(r"Microsoft\Edge\User Data\Default\Cookies").exists() {
        out.push("edge");
    }
    if std::path::Path::new(&local).join(r"Google\Chrome\User Data\Default\Cookies").exists() {
        out.push("chrome");
    }
    if std::path::Path::new(&appdata).join("Mozilla\\Firefox\\Profiles").exists() {
        out.push("firefox");
    }
    out
}

/// Returns the best-available `--cookies-from-browser` args for yt-dlp,
/// based on which browser databases actually exist on this machine.
/// Used by both the streaming path and the cache download path.
pub fn cookie_args() -> Vec<String> {
    match detected_cookie_browsers().first() {
        Some(browser) => vec!["--cookies-from-browser".to_string(), browser.to_string()],
        None => vec![],
    }
}

/// Fetches only the total byte size of a URL via a `Range: bytes=0-0` request.
/// The streaming CDN always returns `Content-Range: bytes 0-0/TOTAL` for this,
/// even when the full GET uses chunked encoding and has no Content-Length.
pub fn fetch_content_length(url: &str) -> Option<u64> {
    // Signed CDN URLs need &range= parameter
    let url = if url.contains("googlevideo.com") && !url.contains("&range=") {
        format!("{}&range=0-", url)
    } else {
        url.to_string()
    };
    let client = Client::builder()
        .timeout(Duration::from_secs(8))
        .build().ok()?;
    let resp = client.get(&url)
        .header("Range", "bytes=0-0")
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                 AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/124.0.0.0 Safari/537.36")
        .header("Referer", "https://www.youtube.com/")
        .send().ok()?;
    // Prefer explicit Content-Length (returned for 206 Partial Content)
    if let Some(n) = resp.content_length().filter(|&n| n > 1) {
        return Some(n);
    }
    // Parse from Content-Range: bytes 0-0/TOTAL
    resp.headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split('/').last())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// Resolves the signed CDN audio URL for `video_id`.
/// Order: cache → yt-dlp (with optimized flags).
pub fn get_stream_url(video_id: &str) -> Result<String, String> {
    // Fast path: URL already cached from a recent play
    if let Some(url) = get_cached_url(video_id) {
        eprintln!("[url-cache] hit for {video_id}");
        return Ok(url);
    }

    let ytdlp = resolve_ytdlp();
    let yt_url = format!("https://www.youtube.com/watch?v={video_id}");

    let start = std::time::Instant::now();

    // Build a list of browsers to try based on whether their cookie DB exists.
    let mut browser_attempts: Vec<Option<&str>> =
        detected_cookie_browsers().into_iter().map(Some).collect();
    browser_attempts.push(None); // bare fallback (no cookies)

    let mut last_err = format!("yt-dlp not available at {}", ytdlp.display());

    for &cookie_opt in &browser_attempts {
        let mut cmd = std::process::Command::new(&ytdlp);
        cmd.args([
            "-g", "-f", "140/bestaudio[ext=m4a]/bestaudio",
            "--no-playlist",
            "--no-check-certificates",
            "--no-warnings",
            "--extractor-retries", "2",
            "--socket-timeout", "10",
        ]);
        // Add addon dir to PATH so yt-dlp can find Deno/ffmpeg even when
        // they're not on the system PATH.
        crate::core::addons::set_addon_path_env(&mut cmd);

        if let Some(browser) = cookie_opt {
            cmd.arg("--cookies-from-browser").arg(browser);
        }
        cmd.arg(&yt_url);

        // Hide console window on Windows
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                last_err = format!("yt-dlp spawn error: {e}");
                break;
            }
        };

        if output.status.success() {
            let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !url.is_empty() {
                let elapsed = start.elapsed().as_millis();
                eprintln!("[yt-dlp] resolved {video_id} in {elapsed}ms");
                store_cached_url(video_id, url.clone());
                return Ok(url);
            }
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        last_err = format!("yt-dlp -g failed: {stderr}");

        // Only retry different cookie sources on auth/bot errors
        if !stderr.contains("Sign in") && !stderr.contains("bot") && !stderr.contains("cookies database") {
            break;
        }
    }

    Err(last_err)
}

/// Pre-fetches a stream URL in a background thread (populates the URL cache).
/// Non-blocking — returns immediately. If already cached, does nothing.
pub fn prefetch_stream_url(video_id: &str) {
    if video_id.is_empty() || video_id == "native-prototype" {
        return;
    }
    if get_cached_url(video_id).is_some() {
        return; // already cached
    }
    let video_id = video_id.to_string();
    std::thread::spawn(move || {
        let _ = get_stream_url(&video_id);
    });
}

/// Downloads the full audio file from an already-resolved CDN `url` straight to
/// `dest`, reconnecting (`range=<pos>-`) on premature EOF like the streaming
/// reader. This reuses a URL we already resolved via yt-dlp `-g`, so it avoids a
/// second yt-dlp invocation for the cache download. Returns bytes written.
///
/// Requires a known Content-Length so completeness can be verified; callers
/// should fall back to a full yt-dlp download on `Err` (e.g. CDN 403 on the
/// signed URL, or chunked responses with no length).
pub fn download_url_to_file(url: &str, dest: &std::path::Path) -> Result<u64, String> {
    use std::io::Write;

    let url = if url.contains("googlevideo.com") && !url.contains("&range=") {
        format!("{}&range=0-", url)
    } else {
        url.to_string()
    };

    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| format!("http client init: {e}"))?;

    let mut resp = open_stream_response(&client, &url, 0).map_err(|e| e.to_string())?;
    let total = resp
        .content_length()
        .ok_or_else(|| "no content-length".to_string())?;

    let mut file = std::fs::File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let mut pos: u64 = 0;
    let mut reconnects: u32 = 0;
    let mut buf = vec![0u8; READ_CHUNK];

    loop {
        match resp.read(&mut buf) {
            Ok(0) => {
                if pos < total && reconnects < MAX_RECONNECTS {
                    reconnects += 1;
                    resp = open_stream_response(&client, &url, pos).map_err(|e| e.to_string())?;
                    continue;
                }
                break; // done (or gave up)
            }
            Ok(n) => {
                reconnects = 0;
                file.write_all(&buf[..n]).map_err(|e| format!("write: {e}"))?;
                pos += n as u64;
            }
            Err(e) => {
                if pos > 0 && reconnects < MAX_RECONNECTS {
                    reconnects += 1;
                    if let Ok(r) = open_stream_response(&client, &url, pos) {
                        resp = r;
                        continue;
                    }
                }
                return Err(format!("read: {e}"));
            }
        }
    }
    file.flush().ok();

    if pos < total {
        return Err(format!("incomplete download: {pos}/{total} bytes"));
    }
    Ok(pos)
}
