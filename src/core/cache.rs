/// Local audio file cache.
///
/// Audio is downloaded to  `%APPDATA%\..\Local\auricle\cache\<video_id>.m4a`
/// (or `~/.auricle/cache/` on other OSes).
///
/// An index file `cache_index.json` records metadata per entry:
///   { video_id, file_size_bytes, added_at_unix_secs, last_played_unix_secs, title, artist }
///
/// Eviction: LRU by `last_played_unix_secs` when total size > limit.

use std::collections::HashMap;
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
        let index_path = dir.join("cache_index.json");
        let index: HashMap<String, CacheEntry> = std::fs::read_to_string(&index_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { dir, index_path, index, limit_bytes }
    }

    /// Returns the cached file path if it exists on disk.
    pub fn get(&mut self, video_id: &str) -> Option<PathBuf> {
        let entry = self.index.get_mut(video_id)?;
        let path = self.dir.join(format!("{}.m4a", video_id));
        if path.exists() {
            entry.last_played = unix_now();
            self.save_index();
            Some(path)
        } else {
            // stale index entry — remove it
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
        if self.total_bytes() <= self.limit_bytes { return; }
        // Sort by last_played ascending (oldest first)
        let mut entries: Vec<_> = self.index.values().cloned().collect();
        entries.sort_by_key(|e| e.last_played);
        for entry in entries {
            if self.total_bytes() <= self.limit_bytes { break; }
            let path = self.dir.join(format!("{}.m4a", entry.video_id));
            std::fs::remove_file(&path).ok();
            self.index.remove(&entry.video_id);
        }
        self.save_index();
    }

    fn save_index(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.index) {
            std::fs::write(&self.index_path, json).ok();
        }
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

/// Download a video's audio to the cache using yt-dlp.
/// Returns the final cached path on success.
pub fn download_to_cache(
    cache: &mut AudioCache,
    video_id: &str,
    title: &str,
    artist: &str,
) -> Result<PathBuf, String> {
    let staging = cache.staging_path(video_id);
    std::fs::remove_file(&staging).ok();

    let yt_dlp = find_yt_dlp();
    let url = format!("https://www.youtube.com/watch?v={}", video_id);
    let staging_str = staging.to_str().unwrap_or("").to_string();

    let mut args = crate::core::stream_player::cookie_args();
    args.extend([
        "-f".to_string(), "bestaudio[ext=m4a]/bestaudio/best".to_string(),
        "--no-playlist".to_string(),
        "-o".to_string(), staging_str,
        url,
    ]);

    let mut cmd = std::process::Command::new(&yt_dlp);
    cmd.args(&args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let output = cmd
        .output()
        .map_err(|e| format!("yt-dlp spawn error: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("yt-dlp download failed: {stderr}"));
    }

    cache.commit(video_id, title, artist)
        .ok_or_else(|| "Failed to commit cache entry".to_string())
}

fn find_yt_dlp() -> std::path::PathBuf {
    crate::core::addons::resolve_tool("yt-dlp")
}

/// Spawn a background thread to download `video_id` to cache.
/// Returns immediately. Does nothing if the file is already cached or staging.
pub fn spawn_prefetch(video_id: String, title: String, artist: String) {
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

        let yt_dlp = find_yt_dlp();
        let url = format!("https://www.youtube.com/watch?v={}", video_id);
        let staging_str = staging.to_str().unwrap_or("").to_string();

        let mut args = crate::core::stream_player::cookie_args();
        args.extend([
            "-f".to_string(), "bestaudio[ext=m4a]/bestaudio/best".to_string(),
            "--no-playlist".to_string(),
            "-o".to_string(), staging_str.clone(),
            url,
        ]);

        eprintln!("[cache-prefetch] Starting background download: {video_id}");
        let mut cmd = std::process::Command::new(&yt_dlp);
        cmd.args(&args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        match cmd.output() {
            Ok(out) if out.status.success() => {
                // Commit (brief lock)
                if let Ok(mut cache) = AudioCache::global().lock() {
                    match cache.commit(&video_id, &title, &artist) {
                        Some(path) => eprintln!("[cache-prefetch] ✓ {video_id} → {}", path.display()),
                        None => eprintln!("[cache-prefetch] ✗ {video_id}: commit failed"),
                    }
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                eprintln!("[cache-prefetch] ✗ {video_id}: {stderr}");
                std::fs::remove_file(&staging_str).ok();
            }
            Err(e) => {
                eprintln!("[cache-prefetch] ✗ {video_id}: spawn error: {e}");
            }
        }
    });
}
