/// Persistence layer for history, liked songs, and disliked songs.
/// Data is stored as JSON files in `%LOCALAPPDATA%\auricle\`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::playback::NowPlaying;

/// Stored song metadata (for liked songs and history).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSong {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub duration_secs: u32,
}

impl From<&NowPlaying> for StoredSong {
    fn from(np: &NowPlaying) -> Self {
        Self {
            video_id: np.video_id.clone(),
            title: np.title.clone(),
            artist: np.artist.clone(),
            duration_secs: np.duration_secs,
        }
    }
}

impl From<StoredSong> for NowPlaying {
    fn from(s: StoredSong) -> Self {
        Self {
            video_id: s.video_id,
            title: s.title,
            artist: s.artist,
            duration_secs: s.duration_secs,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct UserData {
    pub history: Vec<StoredSong>,
    pub liked: Vec<StoredSong>,
    pub disliked: Vec<StoredSong>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub minimize_to_tray: bool,
    pub last_played: Option<StoredSong>,
    #[serde(default)]
    pub onboarding_seen: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { minimize_to_tray: true, last_played: None, onboarding_seen: false }
    }
}

fn data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("auricle")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
        home.join(".auricle")
    }
}

/// Legacy data directory used by older builds (`ytm-native`). Its contents are
/// migrated into [`data_dir`] on startup, after which it is removed.
fn legacy_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("ytm-native")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
        home.join(".ytm-native")
    }
}

/// Recursively move `src` into `dest`, merging directories. Files already present
/// at `dest` are kept (the legacy copy is discarded). Empty source directories are
/// removed as they are drained.
fn merge_move(src: &Path, dest: &Path) {
    if src.is_dir() {
        let _ = std::fs::create_dir_all(dest);
        if let Ok(entries) = std::fs::read_dir(src) {
            for entry in entries.flatten() {
                let from = entry.path();
                let to = dest.join(entry.file_name());
                merge_move(&from, &to);
            }
        }
        let _ = std::fs::remove_dir(src);
    } else if !dest.exists() {
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Fast path: rename; fall back to copy+delete across volumes.
        if std::fs::rename(src, dest).is_err() && std::fs::copy(src, dest).is_ok() {
            let _ = std::fs::remove_file(src);
        }
    } else {
        // Destination already has this file — drop the legacy duplicate.
        let _ = std::fs::remove_file(src);
    }
}

/// Unify storage: migrate everything from the legacy `ytm-native` directory into
/// the canonical `auricle` directory, then delete the legacy directory. Existing
/// files in the new location take precedence. Safe (and cheap) to call on every
/// startup — a no-op once the legacy directory is gone.
pub fn migrate_legacy_dir() {
    let legacy = legacy_data_dir();
    if !legacy.exists() {
        return;
    }
    let new = data_dir();
    if legacy == new {
        return;
    }
    let _ = std::fs::create_dir_all(&new);
    merge_move(&legacy, &new);
    // Remove the legacy directory and anything left behind (e.g. duplicates).
    let _ = std::fs::remove_dir_all(&legacy);
}

fn data_path() -> PathBuf {
    data_dir().join("user_data.json")
}

pub fn load() -> UserData {
    std::fs::read_to_string(data_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(data: &UserData) {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(data) {
        let _ = std::fs::write(data_path(), json);
    }
}

/// Save just history + liked + disliked from current state.
pub fn save_history(history: &[NowPlaying], liked: &[(String, NowPlaying)], disliked: &HashSet<String>, all_songs: &[NowPlaying]) {
    // Previously-known disliked songs are cached in memory (seeded once from disk)
    // so we don't re-read and re-parse user_data.json on every save.
    let cell = DISLIKED_CACHE.get_or_init(|| Mutex::new(load().disliked));
    let disliked_songs: Vec<StoredSong> = {
        let known = cell.lock().map(|g| g.clone()).unwrap_or_default();
        disliked.iter().filter_map(|id| {
            all_songs.iter().find(|s| s.video_id == *id).map(StoredSong::from)
        }).chain(
            // Keep disliked entries that aren't in all_songs (preserve metadata).
            known.into_iter()
                .filter(|s| disliked.contains(&s.video_id) && !all_songs.iter().any(|a| a.video_id == s.video_id))
        ).collect()
    };
    if let Ok(mut g) = cell.lock() {
        *g = disliked_songs.clone();
    }

    let data = UserData {
        history: history.iter().map(StoredSong::from).collect(),
        liked: liked.iter().map(|(_, np)| StoredSong::from(np)).collect(),
        disliked: disliked_songs,
    };
    save(&data);
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

static SETTINGS_CACHE: OnceLock<Mutex<AppSettings>> = OnceLock::new();
static DISLIKED_CACHE: OnceLock<Mutex<Vec<StoredSong>>> = OnceLock::new();

fn settings_from_disk() -> AppSettings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn load_settings() -> AppSettings {
    SETTINGS_CACHE
        .get_or_init(|| Mutex::new(settings_from_disk()))
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default()
}

pub fn save_settings(settings: &AppSettings) {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(settings_path(), json);
    }
    // Keep the in-memory cache coherent with what we just wrote.
    match SETTINGS_CACHE.get() {
        Some(cell) => { if let Ok(mut g) = cell.lock() { *g = settings.clone(); } }
        None => { let _ = SETTINGS_CACHE.set(Mutex::new(settings.clone())); }
    }
}
