/// Streaming audio player for YouTube Music.
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

struct UrlCacheEntry {
    url: String,
    fetched_at: std::time::Instant,
}

static URL_CACHE: OnceLock<Mutex<HashMap<String, UrlCacheEntry>>> = OnceLock::new();

fn url_cache() -> &'static Mutex<HashMap<String, UrlCacheEntry>> {
    URL_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns the cached CDN URL for `video_id` if it was fetched within the TTL.
pub fn get_cached_url(video_id: &str) -> Option<String> {
    let cache = url_cache().lock().ok()?;
    let entry = cache.get(video_id)?;
    if entry.fetched_at.elapsed().as_secs() < URL_CACHE_TTL_SECS {
        Some(entry.url.clone())
    } else {
        None
    }
}

fn store_cached_url(video_id: &str, url: String) {
    if let Ok(mut cache) = url_cache().lock() {
        cache.insert(video_id.to_string(), UrlCacheEntry {
            url,
            fetched_at: std::time::Instant::now(),
        });
    }
}

// ---------------------------------------------------------------------------
// HTTP streaming MediaSource — intentionally non-seekable for safe probing
// ---------------------------------------------------------------------------

struct HttpStream {
    reader: reqwest::blocking::Response,
    pos:    u64,
    content_len: Option<u64>,
}

impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.reader.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpStream {
    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        // Non-seekable — symphonia respects is_seekable() = false.
        // User-initiated seeks go through from_url_at_byte() instead.
        Err(io::Error::new(io::ErrorKind::Unsupported, "HttpStream is not seekable"))
    }
}

impl MediaSource for HttpStream {
    fn is_seekable(&self) -> bool { false }
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
}

impl StreamingAudioSource {
    /// Open `url` and set up symphonia decoding.
    /// Returns immediately once format probing succeeds (~50 ms for AAC/MP4).
    pub fn from_url(url: &str) -> Result<Self, String> {
        // YouTube innertube URLs often require &range=0- to avoid 403
        let url = if url.contains("googlevideo.com") && !url.contains("&range=") {
            format!("{}&range=0-", url)
        } else {
            url.to_string()
        };

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP client init failed: {e}"))?;

        let resp = client
            .get(&url)
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
                 AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/124.0.0.0 Safari/537.36",
            )
            .header("Referer", "https://www.youtube.com/")
            .header("Origin", "https://www.youtube.com")
            .send()
            .map_err(|e| format!("HTTP request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {} fetching audio stream", resp.status()));
        }

        let content_len = resp.content_length();
        let media_source = HttpStream { reader: resp, pos: 0, content_len };
        Self::probe_and_build(Box::new(media_source), content_len, false)
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
        })
    }

    /// Total byte size of the stream — used to compute Range offsets for seeking.
    pub fn content_len(&self) -> Option<u64> {
        self.content_len
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
                    let mut buf =
                        SampleBuffer::<i16>::new(decoded.capacity() as u64, spec);
                    buf.copy_interleaved_ref(decoded);
                    self.sample_buf = buf.samples().to_vec();
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
            .map_err(|_| rodio::source::SeekError::NotSupported {
                underlying_source: std::any::type_name::<Self>(),
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

/// Returns the best-available `--cookies-from-browser` args for yt-dlp,
/// based on which browser databases actually exist on this machine.
/// Used by both the streaming path and the cache download path.
pub fn cookie_args() -> Vec<String> {
    let edge_db = std::path::Path::new(&std::env::var("LOCALAPPDATA").unwrap_or_default())
        .join(r"Microsoft\Edge\User Data\Default\Cookies");
    let chrome_db = std::path::Path::new(&std::env::var("LOCALAPPDATA").unwrap_or_default())
        .join(r"Google\Chrome\User Data\Default\Cookies");
    let firefox_dir = std::path::Path::new(&std::env::var("APPDATA").unwrap_or_default())
        .join("Mozilla\\Firefox\\Profiles");
    // Return the first found — caller can iterate browsers if needed
    if edge_db.exists() {
        return vec!["--cookies-from-browser".to_string(), "edge".to_string()];
    }
    if chrome_db.exists() {
        return vec!["--cookies-from-browser".to_string(), "chrome".to_string()];
    }
    if firefox_dir.exists() {
        return vec!["--cookies-from-browser".to_string(), "firefox".to_string()];
    }
    vec![]
}

/// Fetches only the total byte size of a URL via a `Range: bytes=0-0` request.
/// YouTube CDN always returns `Content-Range: bytes 0-0/TOTAL` for this,
/// even when the full GET uses chunked encoding and has no Content-Length.
pub fn fetch_content_length(url: &str) -> Option<u64> {
    // YouTube innertube URLs need &range= parameter
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
    let edge_db = std::path::Path::new(&std::env::var("LOCALAPPDATA").unwrap_or_default())
        .join(r"Microsoft\Edge\User Data\Default\Cookies");
    let chrome_db = std::path::Path::new(&std::env::var("LOCALAPPDATA").unwrap_or_default())
        .join(r"Google\Chrome\User Data\Default\Cookies");
    let firefox_dir = std::path::Path::new(&std::env::var("APPDATA").unwrap_or_default())
        .join("Mozilla\\Firefox\\Profiles");

    let mut browser_attempts: Vec<Option<&str>> = Vec::new();
    if edge_db.exists()    { browser_attempts.push(Some("edge")); }
    if chrome_db.exists()  { browser_attempts.push(Some("chrome")); }
    if firefox_dir.exists(){ browser_attempts.push(Some("firefox")); }
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
