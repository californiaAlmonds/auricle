/// Local audio file cache.
///
/// Audio is downloaded to  `%APPDATA%\..\Local\auricle\cache\<video_id>.m4a`
/// (or `~/.auricle/cache/` on other OSes).
///
/// An index file `cache_index.json` records metadata per entry:
///   { video_id, file_size_bytes, added_at_unix_secs, last_played_unix_secs, title, artist }
///
/// Eviction: LRU by `last_played_unix_secs` when total size > limit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use std::thread;

use serde::{Deserialize, Serialize};

// ── Public limit type ─────────────────────────────────────────────────────────

/// Cache limit in bytes (default 500 MB).
pub const DEFAULT_CACHE_LIMIT_BYTES: u64 = 500 * 1024 * 1024;

// ── Index entry ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheEntry {
    pub video_id: String,
    pub file_size_bytes: u64,
    pub added_at: u64,        // unix secs
    pub last_played: u64,     // unix secs
    pub title: String,
    pub artist: String,
}

/// Global singleton — call `global()` from anywhere.
static CACHE: OnceLock<Mutex<AudioCache>> = OnceLock::new();

impl AudioCache {
    /// Get the process-wide cache singleton.
    pub fn global() -> &'static Mutex<AudioCache> {
        CACHE.get_or_init(|| Mutex::new(AudioCache::open(DEFAULT_CACHE_LIMIT_BYTES)))
    }
}

// ── Open / init ────────────────────────────────────────────────────────────────

pub struct AudioCache {
    dir: PathBuf,
    index_path: PathBuf,
    index: HashMap<String, CacheEntry>,
    limit_bytes: u64,
    /// In-memory `last_played` touches that haven't been flushed to disk yet.
    dirty: bool,
    /// Last time the index was flushed — used to throttle read-path writes.
    last_flush: std::time::Instant,
    /// Video IDs that must not be evicted (current + upcoming songs). Set each
    /// poll from the playback look-ahead so we never drop a song we're about to
    /// play and then have to re-download it. Not persisted.
    protected: HashSet<String>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl AudioCache {
    /// Open (or create) the cache at the platform cache directory.
    pub fn open(limit_bytes: u64) -> Self {
        let dir = cache_dir();
        std::fs::create_dir_all(&dir).ok();
        // Sweep leftover `.part` staging files from a previous run — any download
        // interrupted by exit/crash left a partial file that would otherwise block
        // a fresh download (the dedup check treats an existing `.part` as "in
        // progress"). At open time no download is running, so all are stale.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("part") {
                    std::fs::remove_file(&path).ok();
                }
            }
        }
        let index_path = dir.join("cache_index.json");
        let index: HashMap<String, CacheEntry> = std::fs::read_to_string(&index_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { dir, index_path, index, limit_bytes, dirty: false, last_flush: std::time::Instant::now(), protected: HashSet::new() }
    }

    /// Returns the cached file path if it exists on disk.
    pub fn get(&mut self, video_id: &str) -> Option<PathBuf> {
        let entry = self.index.get_mut(video_id)?;
        let path = self.dir.join(format!("{}.m4a", video_id));
        if path.exists() {
            // Update LRU timestamp in memory only; flush is throttled to keep
            // synchronous disk I/O off the playback hot path (seek / track change).
            entry.last_played = unix_now();
            self.dirty = true;
            self.maybe_flush();
            Some(path)
        } else {
            // stale index entry — remove it (structural change, flush immediately)
            self.index.remove(video_id);
            self.save_index();
            None
        }
    }

    /// Returns the path where a new download should be written.
    /// Call `commit()` after a successful download.
    pub fn staging_path(&self, video_id: &str) -> PathBuf {
        self.dir.join(format!("{}.part", video_id))
    }

    /// Finalise a download: move `.part` → `.m4a`, add to index, evict if needed.
    pub fn commit(&mut self, video_id: &str, title: &str, artist: &str) -> Option<PathBuf> {
        let staging = self.dir.join(format!("{}.part", video_id));
        let final_path = self.dir.join(format!("{}.m4a", video_id));
        if !staging.exists() { return None; }
        let size = staging.metadata().ok()?.len();
        std::fs::rename(&staging, &final_path).ok()?;
        let now = unix_now();
        self.index.insert(video_id.to_string(), CacheEntry {
            video_id: video_id.to_string(),
            file_size_bytes: size,
            added_at: now,
            last_played: now,
            title: title.to_string(),
            artist: artist.to_string(),
        });
        self.save_index();
        self.evict_if_needed();
        Some(final_path)
    }

    /// Total bytes used by all cache entries.
    pub fn total_bytes(&self) -> u64 {
        self.index.values().map(|e| e.file_size_bytes).sum()
    }

    /// Number of cached songs.
    pub fn count(&self) -> usize {
        self.index.len()
    }

    /// Evict LRU entries until total size ≤ limit.
    fn evict_if_needed(&mut self) {
        let mut total = self.total_bytes();
        if total <= self.limit_bytes { return; }
        // Sort by last_played ascending (oldest first)
        let mut entries: Vec<_> = self.index.values().cloned().collect();
        entries.sort_by_key(|e| e.last_played);
        for entry in entries {
            if total <= self.limit_bytes { break; }
            // Never evict the current/upcoming songs — dropping one we're about to
            // play would force a wasteful re-download moments later.
            if self.protected.contains(&entry.video_id) { continue; }
            let path = self.dir.join(format!("{}.m4a", entry.video_id));
            std::fs::remove_file(&path).ok();
            if self.index.remove(&entry.video_id).is_some() {
                total = total.saturating_sub(entry.file_size_bytes);
            }
        }
        self.save_index();
    }

    /// Mark a set of video IDs as protected from eviction (current + upcoming).
    /// Replaces the previous protected set. Cheap; called from the poll loop.
    pub fn set_protected(&mut self, ids: impl IntoIterator<Item = String>) {
        self.protected = ids.into_iter().collect();
    }

    /// Flush the index to disk only if it's dirty and the throttle window elapsed.
    /// Keeps `last_played` updates off the synchronous playback path.
    fn maybe_flush(&mut self) {
        if self.dirty && self.last_flush.elapsed().as_secs() >= 30 {
            self.save_index();
        }
    }

    fn save_index(&mut self) {
        if let Ok(json) = serde_json::to_string(&self.index) {
            std::fs::write(&self.index_path, json).ok();
        }
        self.dirty = false;
        self.last_flush = std::time::Instant::now();
    }

    /// Set a new limit and immediately evict if over.
    pub fn set_limit(&mut self, limit_bytes: u64) {
        self.limit_bytes = limit_bytes;
        self.evict_if_needed();
    }

    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    pub fn cache_dir(&self) -> &Path {
        &self.dir
    }
}

/// Platform cache directory: `%LOCALAPPDATA%\auricle\cache\` on Windows.
fn cache_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("auricle").join("cache")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
        home.join(".auricle").join("cache")
    }
}

// ── Download helper ────────────────────────────────────────────────────────────

/// Download a video's audio into `staging`, reusing the already-resolved stream
/// URL for a plain HTTP download (no extra yt-dlp) when possible, and falling
/// back to a full yt-dlp download (robust against CDN 403 on the signed URL).
fn download_audio(video_id: &str, staging: &Path) -> Result<(), String> {
    let staging_str = staging.to_str().unwrap_or("").to_string();

    // 1. Fast path: HTTP download from the resolved signed URL. `get_stream_url`
    //    hits the in-memory URL cache for the current song, so this avoids a
    //    second yt-dlp process entirely in the common case. For prefetch-ahead
    //    songs it also warms the URL cache so their playback starts instantly.
    if let Ok(url) = crate::core::stream_player::get_stream_url(video_id) {
        std::fs::remove_file(staging).ok();
        match crate::core::stream_player::download_url_to_file(&url, staging) {
            Ok(_) => return Ok(()),
            Err(e) => {
                eprintln!("[cache] {video_id}: http download failed ({e}); trying yt-dlp");
                std::fs::remove_file(staging).ok();
            }
        }
    }

    // 2. Fallback: full yt-dlp download (handles a signed-URL 403 that the direct
    //    HTTP GET can't, so un-streamable songs still get cached for next time).
    let yt_dlp = find_yt_dlp();
    let watch_url = format!("https://www.youtube.com/watch?v={}", video_id);
    let mut args = crate::core::stream_player::cookie_args();
    args.extend([
        "-f".to_string(), "bestaudio[ext=m4a]/bestaudio/best".to_string(),
        "--no-playlist".to_string(),
        "--fixup".to_string(), "never".to_string(),
        "-o".to_string(), staging_str,
        watch_url,
    ]);
    let mut cmd = std::process::Command::new(&yt_dlp);
    cmd.args(&args);
    crate::core::addons::set_addon_path_env(&mut cmd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let output = cmd.output().map_err(|e| format!("yt-dlp spawn error: {e}"))?;
    if !output.status.success() {
        return Err(format!("yt-dlp download failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(())
}

/// Download a video's audio to the cache. Returns the final cached path.
pub fn download_to_cache(
    cache: &mut AudioCache,
    video_id: &str,
    title: &str,
    artist: &str,
) -> Result<PathBuf, String> {
    let staging = cache.staging_path(video_id);
    download_audio(video_id, &staging)?;
    cache.commit(video_id, title, artist)
        .ok_or_else(|| "Failed to commit cache entry".to_string())
}

fn find_yt_dlp() -> std::path::PathBuf {
    crate::core::addons::resolve_tool("yt-dlp")
}

/// Spawn a background thread to download `video_id` to cache (full file).
/// Returns immediately. Does nothing if already cached or a download is running.
pub fn spawn_cache_download(video_id: String, title: String, artist: String) {
    // Quick check — already cached or in-progress staging file?
    {
        let Ok(mut cache) = AudioCache::global().lock() else { return };
        if cache.get(&video_id).is_some() { return; }
        // If a .part file already exists, another download is already running
        if cache.staging_path(&video_id).exists() { return; }
    }

    thread::spawn(move || {
        // Get staging path without holding the lock during download
        let staging = {
            let Ok(cache) = AudioCache::global().lock() else { return };
            cache.staging_path(&video_id)
        };

        eprintln!("[cache] Starting background download: {video_id}");
        match download_audio(&video_id, &staging) {
            Ok(()) => {
                // Commit (brief lock)
                if let Ok(mut cache) = AudioCache::global().lock() {
                    match cache.commit(&video_id, &title, &artist) {
                        Some(path) => eprintln!("[cache] ✓ {video_id} → {}", path.display()),
                        None => eprintln!("[cache] ✗ {video_id}: commit failed"),
                    }
                }
            }
            Err(e) => {
                eprintln!("[cache] ✗ {video_id}: {e}");
                std::fs::remove_file(&staging).ok();
            }
        }
    });
}
