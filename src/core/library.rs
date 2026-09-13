//! Local, account-free library: followed artists, saved albums, saved playlists,
//! and user-created local playlists. Persisted as JSON in
//! `%LOCALAPPDATA%\auricle\library.json`.
//!
//! This is intentionally *local-first* — the app runs against the unauthenticated
//! YouTube Music API, so there's no account to sync with. Saving an artist/album/
//! playlist stores a reference (browse-id + display metadata) locally; opening it
//! fetches live from the API. Local playlists store their tracks in full.

use std::sync::{Mutex, OnceLock};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::persistence::{data_dir, StoredSong};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedArtist {
    pub browse_id: String,
    pub name: String,
    #[serde(default)]
    pub thumbnail_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedAlbum {
    pub browse_id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub thumbnail_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedPlaylist {
    pub playlist_id: String,
    pub title: String,
    #[serde(default)]
    pub thumbnail_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalPlaylist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub songs: Vec<StoredSong>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LibraryData {
    #[serde(default)]
    pub followed_artists: Vec<SavedArtist>,
    #[serde(default)]
    pub saved_albums: Vec<SavedAlbum>,
    #[serde(default)]
    pub saved_playlists: Vec<SavedPlaylist>,
    #[serde(default)]
    pub local_playlists: Vec<LocalPlaylist>,
}

static LIBRARY: OnceLock<Mutex<LibraryData>> = OnceLock::new();

fn library_path() -> PathBuf {
    data_dir().join("library.json")
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load_from_disk() -> LibraryData {
    std::fs::read_to_string(library_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn library() -> &'static Mutex<LibraryData> {
    LIBRARY.get_or_init(|| Mutex::new(load_from_disk()))
}

fn save(data: &LibraryData) {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(data) {
        let _ = std::fs::write(library_path(), json);
    }
}

/// A short, unique-enough local id for a new playlist (time + counter based).
fn new_local_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("local-{}-{}", unix_now(), n)
}

// ── Followed artists ──────────────────────────────────────────────────────────

pub fn is_artist_followed(browse_id: &str) -> bool {
    if browse_id.is_empty() { return false; }
    library().lock().map(|l| l.followed_artists.iter().any(|a| a.browse_id == browse_id)).unwrap_or(false)
}

/// Toggles follow state for an artist. Returns the new state (true = now followed).
pub fn toggle_follow_artist(artist: SavedArtist) -> bool {
    let Ok(mut lib) = library().lock() else { return false };
    if let Some(pos) = lib.followed_artists.iter().position(|a| a.browse_id == artist.browse_id) {
        lib.followed_artists.remove(pos);
        save(&lib);
        false
    } else {
        lib.followed_artists.insert(0, artist);
        save(&lib);
        true
    }
}

pub fn followed_artists() -> Vec<SavedArtist> {
    library().lock().map(|l| l.followed_artists.clone()).unwrap_or_default()
}

/// Backfill an artist's thumbnail URL (learned when the artist page is opened).
pub fn set_artist_thumbnail(browse_id: &str, url: &str) {
    if browse_id.is_empty() || url.is_empty() { return; }
    if let Ok(mut lib) = library().lock() {
        if let Some(a) = lib.followed_artists.iter_mut().find(|a| a.browse_id == browse_id) {
            if a.thumbnail_url != url {
                a.thumbnail_url = url.to_string();
                save(&lib);
            }
        }
    }
}

// ── Saved albums ──────────────────────────────────────────────────────────────

pub fn is_album_saved(browse_id: &str) -> bool {
    if browse_id.is_empty() { return false; }
    library().lock().map(|l| l.saved_albums.iter().any(|a| a.browse_id == browse_id)).unwrap_or(false)
}

pub fn toggle_save_album(album: SavedAlbum) -> bool {
    let Ok(mut lib) = library().lock() else { return false };
    if let Some(pos) = lib.saved_albums.iter().position(|a| a.browse_id == album.browse_id) {
        lib.saved_albums.remove(pos);
        save(&lib);
        false
    } else {
        lib.saved_albums.insert(0, album);
        save(&lib);
        true
    }
}

pub fn saved_albums() -> Vec<SavedAlbum> {
    library().lock().map(|l| l.saved_albums.clone()).unwrap_or_default()
}

/// Backfill an album's thumbnail URL (learned when the album page is opened).
pub fn set_album_thumbnail(browse_id: &str, url: &str) {
    if browse_id.is_empty() || url.is_empty() { return; }
    if let Ok(mut lib) = library().lock() {
        if let Some(a) = lib.saved_albums.iter_mut().find(|a| a.browse_id == browse_id) {
            if a.thumbnail_url != url {
                a.thumbnail_url = url.to_string();
                save(&lib);
            }
        }
    }
}

// ── Saved playlists ───────────────────────────────────────────────────────────

pub fn is_playlist_saved(playlist_id: &str) -> bool {
    if playlist_id.is_empty() { return false; }
    library().lock().map(|l| l.saved_playlists.iter().any(|p| p.playlist_id == playlist_id)).unwrap_or(false)
}

pub fn toggle_save_playlist(playlist: SavedPlaylist) -> bool {
    let Ok(mut lib) = library().lock() else { return false };
    if let Some(pos) = lib.saved_playlists.iter().position(|p| p.playlist_id == playlist.playlist_id) {
        lib.saved_playlists.remove(pos);
        save(&lib);
        false
    } else {
        lib.saved_playlists.insert(0, playlist);
        save(&lib);
        true
    }
}

pub fn saved_playlists() -> Vec<SavedPlaylist> {
    library().lock().map(|l| l.saved_playlists.clone()).unwrap_or_default()
}

// ── Local (user-created) playlists ────────────────────────────────────────────

pub fn local_playlists() -> Vec<LocalPlaylist> {
    library().lock().map(|l| l.local_playlists.clone()).unwrap_or_default()
}

pub fn get_local_playlist(id: &str) -> Option<LocalPlaylist> {
    library().lock().ok()?.local_playlists.iter().find(|p| p.id == id).cloned()
}

/// Creates a new empty playlist and returns its generated id.
pub fn create_playlist(name: &str) -> String {
    let id = new_local_id();
    if let Ok(mut lib) = library().lock() {
        lib.local_playlists.insert(0, LocalPlaylist {
            id: id.clone(),
            name: name.trim().to_string(),
            created_at: unix_now(),
            songs: Vec::new(),
        });
        save(&lib);
    }
    id
}

pub fn rename_playlist(id: &str, name: &str) {
    if let Ok(mut lib) = library().lock() {
        if let Some(p) = lib.local_playlists.iter_mut().find(|p| p.id == id) {
            p.name = name.trim().to_string();
            save(&lib);
        }
    }
}

pub fn delete_playlist(id: &str) {
    if let Ok(mut lib) = library().lock() {
        let before = lib.local_playlists.len();
        lib.local_playlists.retain(|p| p.id != id);
        if lib.local_playlists.len() != before {
            save(&lib);
        }
    }
}

/// Adds a song to a playlist (deduped by video_id). Returns true if added.
pub fn add_song_to_playlist(id: &str, song: StoredSong) -> bool {
    let Ok(mut lib) = library().lock() else { return false };
    if let Some(p) = lib.local_playlists.iter_mut().find(|p| p.id == id) {
        if p.songs.iter().any(|s| s.video_id == song.video_id) {
            return false; // already present
        }
        p.songs.push(song);
        save(&lib);
        true
    } else {
        false
    }
}

pub fn remove_song_from_playlist(id: &str, video_id: &str) {
    if let Ok(mut lib) = library().lock() {
        if let Some(p) = lib.local_playlists.iter_mut().find(|p| p.id == id) {
            let before = p.songs.len();
            p.songs.retain(|s| s.video_id != video_id);
            if p.songs.len() != before {
                save(&lib);
            }
        }
    }
}
