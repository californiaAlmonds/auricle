/// Persistence layer for history, liked songs, and disliked songs.
/// Data is stored as JSON files in `%LOCALAPPDATA%\ytm-native\`.

use std::collections::HashSet;
use std::path::PathBuf;

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

#[derive(Serialize, Deserialize)]
pub struct AppSettings {
    pub minimize_to_tray: bool,
    pub last_played: Option<StoredSong>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { minimize_to_tray: true, last_played: None }
    }
}

fn data_dir() -> PathBuf {
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
    let data = UserData {
        history: history.iter().map(StoredSong::from).collect(),
        liked: liked.iter().map(|(_, np)| StoredSong::from(np)).collect(),
        disliked: disliked.iter().filter_map(|id| {
            all_songs.iter().find(|s| s.video_id == *id).map(StoredSong::from)
        }).chain(
            // Keep disliked entries that aren't in all_songs (preserve from previous saves)
            std::fs::read_to_string(data_path())
                .ok()
                .and_then(|s| serde_json::from_str::<UserData>(&s).ok())
                .unwrap_or_default()
                .disliked
                .into_iter()
                .filter(|s| disliked.contains(&s.video_id) && !all_songs.iter().any(|a| a.video_id == s.video_id))
        ).collect(),
    };
    save(&data);
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn load_settings() -> AppSettings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(settings: &AppSettings) {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(settings_path(), json);
    }
}
