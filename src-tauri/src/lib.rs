use serde::{Deserialize, Serialize};
use rusqlite::{params, Connection};
use tauri::{Manager, Emitter};
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::OAuthToken;
use ytmapi_rs::query::{SearchQuery, search::SongsFilter, search::ArtistsFilter};
use ytmapi_rs::common::YoutubeID;

pub mod core;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, PlatformConfig};
use slint::{Model, ModelRc, SharedString, VecModel};



slint::include_modules!();

struct MediaControlsState {
    controls: Mutex<MediaControls>,
}

#[tauri::command]
fn update_media_metadata(state: tauri::State<MediaControlsState>, title: String, artist: String, album: String, cover_url: String) {
    let mut controls = state.controls.lock().unwrap();
    let _ = controls.set_metadata(MediaMetadata {
        title: Some(&title),
        artist: Some(&artist),
        album: Some(&album),
        cover_url: Some(&cover_url),
        duration: None,
    });
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Thumbnail {
    pub url: String,
    pub width: u64,
    pub height: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArtistRef {
    pub name: String,
    #[serde(rename = "browseId")]
    pub browse_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AlbumRef {
    pub name: String,
    #[serde(rename = "browseId")]
    pub browse_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Song {
    #[serde(rename = "videoId")]
    pub video_id: String,
    pub name: String,
    pub artist: ArtistRef,
    pub album: Option<AlbumRef>,
    pub duration: Option<u32>,
    pub thumbnails: Vec<Thumbnail>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Artist {
    pub name: String,
    #[serde(rename = "browseId")]
    pub browse_id: String,
    pub thumbnails: Vec<Thumbnail>,
}

enum ApiClient {
    Authenticated(YtMusic<OAuthToken>),
    Guest(YtMusic<ytmapi_rs::auth::noauth::NoAuthToken>),
}

impl ApiClient {
    async fn search_songs(&self, query: String) -> Result<Vec<Song>, String> {
        match self {
            Self::Authenticated(api) => {
                let results = api.query(SearchQuery::new(query).with_filter(SongsFilter)).await.map_err(|e| e.to_string())?;
                Ok(map_search_results(results))
            },
            Self::Guest(api) => {
                let results = api.query(SearchQuery::new(query).with_filter(SongsFilter)).await.map_err(|e| e.to_string())?;
                Ok(map_search_results(results))
            }
        }
    }

    async fn search_artists(&self, query: String) -> Result<Vec<Artist>, String> {
        match self {
            Self::Authenticated(api) => {
                let results = api.query(SearchQuery::new(query).with_filter(ArtistsFilter)).await.map_err(|e| e.to_string())?;
                Ok(results.into_iter().map(|r| Artist {
                    name: r.artist,
                    browse_id: r.browse_id.get_raw().to_string(),
                    thumbnails: Vec::new(),
                }).collect())
            },
            Self::Guest(api) => {
                let results = api.query(SearchQuery::new(query).with_filter(ArtistsFilter)).await.map_err(|e| e.to_string())?;
                Ok(results.into_iter().map(|r| Artist {
                    name: r.artist,
                    browse_id: r.browse_id.get_raw().to_string(),
                    thumbnails: Vec::new(),
                }).collect())
            }
        }
    }

    async fn get_playlist_tracks(&self, id: ytmapi_rs::common::PlaylistID<'static>) -> Result<Vec<Song>, String> {
        match self {
            Self::Authenticated(api) => {
                let results = api.get_playlist_tracks(id).await.map_err(|e| e.to_string())?;
                Ok(map_playlist_items(results))
            },
            Self::Guest(api) => {
                let results = api.get_playlist_tracks(id).await.map_err(|e| e.to_string())?;
                Ok(map_playlist_items(results))
            }
        }
    }

    async fn get_artist_data(&self, id: ytmapi_rs::common::ArtistChannelID<'static>) -> Result<ytmapi_rs::parse::GetArtist, String> {
        match self {
            Self::Authenticated(api) => api.get_artist(id).await.map_err(|e| e.to_string()),
            Self::Guest(api) => api.get_artist(id).await.map_err(|e| e.to_string()),
        }
    }

    async fn get_album_data(&self, id: ytmapi_rs::common::AlbumID<'static>) -> Result<ytmapi_rs::parse::GetAlbum, String> {
        match self {
            Self::Authenticated(api) => api.get_album(id).await.map_err(|e| e.to_string()),
            Self::Guest(api) => api.get_album(id).await.map_err(|e| e.to_string()),
        }
    }

    async fn get_watch_playlist(&self, video_id: String) -> Result<Vec<Song>, String> {
        let vid = ytmapi_rs::common::VideoID::from_raw(video_id);
        let tracks = match self {
            Self::Authenticated(api) => api.get_watch_playlist_from_video_id(vid).await.map_err(|e| e.to_string())?,
            Self::Guest(api) => api.get_watch_playlist_from_video_id(vid).await.map_err(|e| e.to_string())?,
        };
        Ok(tracks.into_iter().map(|r| Song {
            video_id: r.video_id.get_raw().to_string(),
            name: r.title,
            artist: ArtistRef { name: r.author, browse_id: String::new() },
            album: None,
            duration: None,
            thumbnails: r.thumbnails.into_iter().map(|t| Thumbnail {
                url: t.url, width: t.width as u64, height: t.height as u64,
            }).collect(),
        }).collect())
    }

    async fn get_library_playlists(&self) -> Result<Vec<ytmapi_rs::parse::LibraryPlaylist>, String> {
        match self {
            Self::Authenticated(api) => api.get_library_playlists().await.map_err(|e| e.to_string()),
            Self::Guest(_) => Ok(vec![]),
        }
    }
}

fn map_search_results(results: Vec<ytmapi_rs::parse::SearchResultSong>) -> Vec<Song> {
    results.into_iter().map(|r| Song {
        video_id: r.video_id.get_raw().to_string(),
        name: r.title,
        artist: ArtistRef {
            name: r.artist.clone(),
            browse_id: String::new(),
        },
        album: r.album.map(|a| AlbumRef {
            name: a.name,
            browse_id: a.id.get_raw().to_string(),
        }),
        duration: parse_duration(&r.duration),
        thumbnails: r.thumbnails.into_iter().map(|t| Thumbnail {
            url: t.url,
            width: t.width as u64,
            height: t.height as u64,
        }).collect(),
    }).collect()
}

fn map_playlist_items(results: Vec<ytmapi_rs::parse::PlaylistItem>) -> Vec<Song> {
    results.into_iter().filter_map(|p| {
        match p {
            ytmapi_rs::parse::PlaylistItem::Video(v) => {
                  Some(Song {
                    video_id: v.video_id.get_raw().to_string(),
                    name: v.title,
                    artist: ArtistRef {
                        name: v.channel_name,
                        browse_id: v.channel_id.get_raw().to_string(),
                    },
                    album: None,
                    duration: parse_duration(&v.duration),
                    thumbnails: v.thumbnails.into_iter().map(|t| Thumbnail {
                        url: t.url,
                        width: t.width as u64,
                        height: t.height as u64,
                    }).collect(),
                })
            },
            _ => None
        }
    }).collect()
}

fn parse_duration(d: &str) -> Option<u32> {
    let parts: Vec<&str> = d.split(':').collect();
    if parts.len() == 2 {
        let m: u32 = parts[0].parse().unwrap_or(0);
        let s: u32 = parts[1].parse().unwrap_or(0);
        Some(m * 60 + s)
    } else if parts.len() == 3 {
        let h: u32 = parts[0].parse().unwrap_or(0);
        let m: u32 = parts[1].parse().unwrap_or(0);
        let s: u32 = parts[2].parse().unwrap_or(0);
        Some(h * 3600 + m * 60 + s)
    } else {
        None
    }
}

async fn get_api(auth: Option<AuthTokens>) -> Result<ApiClient, String> {
    if let Some(tokens) = auth {
        let now = std::time::SystemTime::now();
        let token_json = serde_json::json!({
            "access_token": tokens.access_token,
            "refresh_token": tokens.refresh_token,
            "expires_at": now, 
            "expires_in": 3600,
            "token_type": "Bearer",
            "request_time": now,
            "client_id": GOOGLE_CLIENT_ID,
            "client_secret": ""
        });
        let oauth_token: OAuthToken = serde_json::from_value(token_json).map_err(|e| e.to_string())?;
        Ok(ApiClient::Authenticated(YtMusic::from_auth_token(oauth_token)))
    } else {
        let api = YtMusic::new_unauthenticated().await.map_err(|e| e.to_string())?;
        Ok(ApiClient::Guest(api))
    }
}

#[tauri::command]
async fn search_music(query: String, auth: Option<AuthTokens>) -> Result<Vec<Song>, String> {
    let api = get_api(auth).await?;
    api.search_songs(query).await
}

#[tauri::command]
async fn search_artists(query: String, auth: Option<AuthTokens>) -> Result<Vec<Artist>, String> {
    let api = get_api(auth).await?;
    api.search_artists(query).await
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct UserData {
    pub favourites: FavouritesData,
    pub history: Vec<Song>,
    #[serde(rename = "userPlaylists")]
    pub user_playlists: Vec<PlaylistData>,
    #[serde(rename = "dislikedSongs")]
    pub disliked_songs: Vec<String>,
    pub auth: Option<AuthTokens>,
    #[serde(default)]
    pub onboarding: OnboardingData,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct OnboardingData {
    #[serde(rename = "hasCompleted")]
    pub has_completed: bool,
    #[serde(rename = "startupMode")]
    pub startup_mode: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FavouritesData {
    pub songs: Vec<Song>,
    pub albums: Vec<AlbumRef>,
    pub artists: Vec<ArtistRef>,
    pub playlists: Vec<AlbumRef>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct PlaylistData {
    pub id: String,
    pub name: String,
    pub songs: Vec<Song>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AutoplayInteractionInput {
    pub from_song_id: String,
    pub to_song_id: String,
    pub from_artist: String,
    pub to_artist: String,
    pub interaction_type: String,
    pub timestamp: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AutoplayLearningSignal {
    pub song_id: String,
    pub score: f64,
    pub interactions: i64,
}

fn now_unix_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn autoplay_db(app: &tauri::AppHandle) -> Result<Connection, String> {
    let db_path = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("autoplay_learning.db");

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS autoplay_interactions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            from_song_id TEXT NOT NULL,
            to_song_id TEXT NOT NULL,
            from_artist TEXT NOT NULL,
            to_artist TEXT NOT NULL,
            interaction_type TEXT NOT NULL,
            occurred_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_autoplay_from_song ON autoplay_interactions(from_song_id, occurred_at DESC);
        CREATE INDEX IF NOT EXISTS idx_autoplay_from_artist ON autoplay_interactions(from_artist, occurred_at DESC);
        CREATE INDEX IF NOT EXISTS idx_autoplay_to_song ON autoplay_interactions(to_song_id);
        CREATE TABLE IF NOT EXISTS genre_cache (
            entity_key TEXT PRIMARY KEY,
            genres TEXT NOT NULL,
            fetched_at INTEGER NOT NULL
        );
        "#,
    )
    .map_err(|e| e.to_string())?;

    Ok(conn)
}

#[tauri::command]
async fn record_autoplay_interaction(app: tauri::AppHandle, interaction: AutoplayInteractionInput) -> Result<(), String> {
    let conn = autoplay_db(&app)?;
    let ts = interaction.timestamp.unwrap_or_else(now_unix_ts);

    conn.execute(
        "INSERT INTO autoplay_interactions (from_song_id, to_song_id, from_artist, to_artist, interaction_type, occurred_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            interaction.from_song_id,
            interaction.to_song_id,
            interaction.from_artist.to_lowercase(),
            interaction.to_artist.to_lowercase(),
            interaction.interaction_type,
            ts,
        ],
    )
    .map_err(|e| e.to_string())?;

    let cutoff = now_unix_ts() - (180 * 24 * 60 * 60);
    conn.execute(
        "DELETE FROM autoplay_interactions WHERE occurred_at < ?1",
        params![cutoff],
    )
    .map_err(|e| e.to_string())?;

    // Spawn background genre enrichment
    let db_path = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("autoplay_learning.db");
    let from_artist = interaction.from_artist.to_lowercase();
    let to_artist = interaction.to_artist.to_lowercase();
    tokio::spawn(async move {
        core::musicbrainz::enrich_genre_data(db_path, &from_artist, &to_artist).await;
    });

    Ok(())
}

#[tauri::command]
async fn get_autoplay_learning_signals(
    app: tauri::AppHandle,
    from_song_id: String,
    from_artist: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<AutoplayLearningSignal>, String> {
    let conn = autoplay_db(&app)?;
    let requested_limit = limit.unwrap_or(40).clamp(1, 100) as usize;
    let now = now_unix_ts() as f64;
    let mut scores = HashMap::<String, (f64, i64)>::new();

    {
        let mut stmt = conn
            .prepare(
                "SELECT to_song_id, interaction_type, occurred_at FROM autoplay_interactions WHERE from_song_id = ?1 ORDER BY occurred_at DESC LIMIT 300",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![from_song_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;

        for row in rows {
            let (song_id, interaction_type, occurred_at) = row.map_err(|e| e.to_string())?;
            let age_days = ((now - occurred_at as f64).max(0.0)) / 86_400.0;
            let decay = (-age_days / 30.0).exp();
            let base = match interaction_type.as_str() {
                "manual_pick" => 1.8,
                "skip" => -0.9,
                _ => 1.0,
            };
            let entry = scores.entry(song_id).or_insert((0.0, 0));
            entry.0 += base * decay;
            entry.1 += 1;
        }
    }

    if let Some(artist) = from_artist {
        let mut stmt = conn
            .prepare(
                "SELECT to_song_id, interaction_type, occurred_at FROM autoplay_interactions WHERE from_artist = ?1 ORDER BY occurred_at DESC LIMIT 300",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![artist.to_lowercase()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;

        for row in rows {
            let (song_id, interaction_type, occurred_at) = row.map_err(|e| e.to_string())?;
            let age_days = ((now - occurred_at as f64).max(0.0)) / 86_400.0;
            let decay = (-age_days / 45.0).exp();
            let base = match interaction_type.as_str() {
                "manual_pick" => 0.9,
                "skip" => -0.35,
                _ => 0.45,
            };
            let entry = scores.entry(song_id).or_insert((0.0, 0));
            entry.0 += base * decay;
            entry.1 += 1;
        }
    }

    let mut signals = scores
        .into_iter()
        .map(|(song_id, (score, interactions))| AutoplayLearningSignal {
            song_id,
            score,
            interactions,
        })
        .collect::<Vec<_>>();

    signals.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    signals.truncate(requested_limit);
    Ok(signals)
}

#[tauri::command]
async fn get_genre_affinity(app: tauri::AppHandle) -> Result<Vec<(String, f64)>, String> {
    let conn = autoplay_db(&app)?;

    // Get all artists with positive interactions and their counts
    let mut stmt = conn.prepare(
        "SELECT to_artist, COUNT(*) as cnt FROM autoplay_interactions WHERE interaction_type IN ('played', 'liked', 'completed') GROUP BY to_artist"
    ).map_err(|e| e.to_string())?;

    let artist_scores: Vec<(String, f64)> = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    }).map_err(|e| e.to_string())?
    .filter_map(|r| r.ok())
    .collect();

    // For each artist, look up their genres in genre_cache
    let mut genre_scores: HashMap<String, f64> = HashMap::new();
    let cutoff = now_unix_ts() - (30 * 24 * 60 * 60);

    for (artist, score) in &artist_scores {
        let key = format!("artist:{}", artist);
        if let Ok(genres_str) = conn.query_row(
            "SELECT genres FROM genre_cache WHERE entity_key = ?1 AND fetched_at > ?2",
            params![key, cutoff],
            |row| row.get::<_, String>(0),
        ) {
            for genre in genres_str.split(',').filter(|s| !s.is_empty()) {
                *genre_scores.entry(genre.to_string()).or_insert(0.0) += score;
            }
        }
    }

    let mut result: Vec<(String, f64)> = genre_scores.into_iter().collect();
    result.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    result.truncate(10);
    Ok(result)
}

#[tauri::command]
async fn save_user_data(app: tauri::AppHandle, data: UserData) -> Result<(), String> {
    let path = app.path().app_data_dir().map_err(|e| e.to_string())?.join("user_data.json");
    
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let json = serde_json::to_string_pretty(&data).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn load_user_data(app: tauri::AppHandle) -> Result<UserData, String> {
    let path = app.path().app_data_dir().map_err(|e| e.to_string())?.join("user_data.json");
    
    if !path.exists() {
        return Ok(UserData::default());
    }

    let json = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let data: UserData = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    Ok(data)
}

const GOOGLE_CLIENT_ID: &str = "363288825229-jlbiuir8ds26t7mt7jkdneqpq6o0s639.apps.googleusercontent.com";
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REDIRECT_URI: &str = "com.googleusercontent.apps.363288825229-jlbiuir8ds26t7mt7jkdneqpq6o0s639://auth";

#[tauri::command]
async fn exchange_google_code(code: String) -> Result<AuthTokens, String> {
    let client = reqwest::Client::new();
    let resp = client.post(GOOGLE_TOKEN_URL)
        .form(&[
            ("client_id", GOOGLE_CLIENT_ID),
            ("code", &code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        let err_body = resp.text().await.unwrap_or_default();
        return Err(format!("Token exchange failed: {}", err_body));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    
    let access_token = data["access_token"].as_str().ok_or("No access token")?.to_string();
    let refresh_token = data["refresh_token"].as_str().map(|s| s.to_string());
    let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() + expires_in;

    Ok(AuthTokens {
        access_token,
        refresh_token,
        expires_at,
    })
}

#[tauri::command]
async fn push_to_cloud(auth: AuthTokens, data: UserData) -> Result<(), String> {
    let client = reqwest::Client::new();
    
    // 1. Find if file exists in appDataFolder
    let list_url = "https://www.googleapis.com/drive/v3/files?spaces=appDataFolder&q=name='user_data.json'";
    let res = client.get(list_url)
        .bearer_auth(&auth.access_token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    
    let list_json: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    let files = list_json["files"].as_array().ok_or("Invalid response from Drive")?;
    
    let content = serde_json::to_string(&data).map_err(|e| e.to_string())?;
    
    if files.is_empty() {
        // Create new file
        let metadata = serde_json::json!({
            "name": "user_data.json",
            "parents": ["appDataFolder"]
        });
        
        let form = reqwest::multipart::Form::new()
            .part("metadata", reqwest::multipart::Part::text(metadata.to_string()).mime_str("application/json").unwrap())
            .part("file", reqwest::multipart::Part::text(content).mime_str("application/json").unwrap());
            
        client.post("https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart")
            .bearer_auth(&auth.access_token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| e.to_string())?;
    } else {
        // Update existing file
        let file_id = files[0]["id"].as_str().ok_or("No file ID found")?;
        let update_url = format!("https://www.googleapis.com/upload/drive/v3/files/{}?uploadType=media", file_id);
        
        client.patch(update_url)
            .bearer_auth(&auth.access_token)
            .body(content)
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
    }
    
    Ok(())
}

#[tauri::command]
async fn pull_from_cloud(auth: AuthTokens) -> Result<Option<UserData>, String> {
    let client = reqwest::Client::new();
    
    let list_url = "https://www.googleapis.com/drive/v3/files?spaces=appDataFolder&q=name='user_data.json'";
    let res = client.get(list_url)
        .bearer_auth(&auth.access_token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    
    let list_json: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    let files = list_json["files"].as_array().ok_or("Invalid response from Drive")?;
    
    if files.is_empty() {
        return Ok(None);
    }
    
    let file_id = files[0]["id"].as_str().ok_or("No file ID found")?;
    let download_url = format!("https://www.googleapis.com/drive/v3/files/{}?alt=media", file_id);
    
    let res = client.get(download_url)
        .bearer_auth(&auth.access_token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    
    let data: UserData = res.json().await.map_err(|e| e.to_string())?;
    Ok(Some(data))
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    pub thumbnails: Vec<Thumbnail>,
    pub count: Option<u32>,
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArtistDetails {
    pub name: String,
    pub description: Option<String>,
    pub thumbnails: Vec<Thumbnail>,
    pub songs: Vec<Song>,
    pub albums: Vec<AlbumRef>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AlbumDetails {
    pub name: String,
    pub artist: ArtistRef,
    pub year: Option<String>,
    pub thumbnails: Vec<Thumbnail>,
    pub songs: Vec<Song>,
}

#[tauri::command]
async fn get_library_playlists(auth: Option<AuthTokens>) -> Result<Vec<Playlist>, String> {
    let api = get_api(auth).await?;
    let results = api.get_library_playlists().await?;
    
    let playlists = results.into_iter().map(|p| Playlist {
        id: p.playlist_id.get_raw().to_string(),
        title: p.title,
        thumbnails: p.thumbnails.into_iter().map(|t| Thumbnail {
            url: t.url,
            width: t.width as u64,
            height: t.height as u64,
        }).collect(),
        count: None,
        description: None,
    }).collect();

    Ok(playlists)
}

#[tauri::command]
async fn get_playlist_tracks(playlist_id: String, auth: Option<AuthTokens>) -> Result<Vec<Song>, String> {
    let api = get_api(auth).await?;
    let id = ytmapi_rs::common::PlaylistID::from_raw(playlist_id);
    api.get_playlist_tracks(id).await
}

#[tauri::command]
async fn get_artist_details(browse_id: String, auth: Option<AuthTokens>) -> Result<ArtistDetails, String> {
    let api = get_api(auth).await?;
    let id = ytmapi_rs::common::ArtistChannelID::from_raw(browse_id);
    let results = api.get_artist_data(id).await?;
    
    let songs = results.top_releases.songs.map(|s| {
        s.results.into_iter().map(|r| Song {
            video_id: r.video_id.get_raw().to_string(),
            name: r.title,
            artist: ArtistRef {
                name: results.name.clone(),
                browse_id: "".to_string(),
            },
            album: None,
            duration: None,
            thumbnails: Vec::new(),
        }).collect()
    }).unwrap_or_default();

    let albums = results.top_releases.albums.map(|a| {
        a.results.into_iter().map(|r| AlbumRef {
            name: r.title,
            browse_id: r.album_id.get_raw().to_string(),
        }).collect()
    }).unwrap_or_default();

    Ok(ArtistDetails {
        name: results.name,
        description: results.description,
        thumbnails: results.thumbnails.into_iter().map(|t| Thumbnail {
            url: t.url,
            width: t.width as u64,
            height: t.height as u64,
        }).collect(),
        songs,
        albums,
    })
}

#[tauri::command]
async fn get_album_details(browse_id: String, auth: Option<AuthTokens>) -> Result<AlbumDetails, String> {
    let api = get_api(auth).await?;
    let id = ytmapi_rs::common::AlbumID::from_raw(browse_id);
    let results = api.get_album_data(id).await?;
    
    let songs = results.tracks.into_iter().map(|t| Song {
        video_id: t.video_id.get_raw().to_string(),
        name: t.title,
        artist: ArtistRef {
            name: results.artists.first().map(|a| a.name.clone()).unwrap_or_default(),
            browse_id: results.artists.first().and_then(|a| a.id.as_ref()).map(|id| id.get_raw().to_string()).unwrap_or_default(),
        },
        album: Some(AlbumRef {
            name: results.title.clone(),
            browse_id: "".to_string(),
        }),
        duration: parse_duration(&t.duration),
        thumbnails: Vec::new(),
    }).collect();

    Ok(AlbumDetails {
        name: results.title,
        artist: ArtistRef {
            name: results.artists.first().map(|a| a.name.clone()).unwrap_or_default(),
            browse_id: results.artists.first().and_then(|a| a.id.as_ref()).map(|id| id.get_raw().to_string()).unwrap_or_default(),
        },
        year: Some(results.year),
        thumbnails: results.thumbnails.into_iter().map(|t| Thumbnail {
            url: t.url,
            width: t.width as u64,
            height: t.height as u64,
        }).collect(),
        songs,
    })
}

#[tauri::command]
async fn generate_smart_mix(video_id: String, auth: Option<AuthTokens>) -> Result<Vec<Song>, String> {
    let api = get_api(auth).await?;
    api.get_watch_playlist(video_id).await
}

#[tauri::command]
fn get_google_auth_url() -> String {
    format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope=https://www.googleapis.com/auth/youtube.readonly%20https://www.googleapis.com/auth/drive.appdata&access_type=offline&prompt=consent",
        GOOGLE_AUTH_URL,
        GOOGLE_CLIENT_ID,
        urlencoding::encode(REDIRECT_URI)
    )
}

#[tauri::command]
fn minimize_window(window: tauri::Window, minimize_to_tray: Option<bool>) {
    if minimize_to_tray.unwrap_or(true) {
        let _ = window.hide();
    } else {
        let _ = window.minimize();
    }
}

#[tauri::command]
async fn get_stream_url(video_id: String, quality: Option<String>) -> Result<String, String> {
    core::streaming::resolve_stream_url(&video_id, quality.as_deref()).await
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct BridgeSong {
    #[serde(rename = "videoId")]
    video_id: String,
    title: String,
    artist: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct BridgePlaybackState {
    #[serde(rename = "isPlaying")]
    is_playing: bool,
    #[serde(rename = "nowPlaying")]
    now_playing: BridgeSong,
    #[serde(rename = "queuePreview")]
    queue_preview: Vec<BridgeSong>,
}

fn map_bridge_song(song: core::playback::NowPlaying) -> BridgeSong {
    BridgeSong {
        video_id: song.video_id,
        title: song.title,
        artist: song.artist,
    }
}

fn emit_bridge_playback_state(app_handle: &tauri::AppHandle) {
    let state = bridge_get_playback_state();
    let _ = app_handle.emit("bridge-playback-state", state);
}

#[tauri::command]
fn bridge_get_playback_state() -> BridgePlaybackState {
    let playback = core::bridge::playback_core();
    let now_playing = map_bridge_song(playback.now_playing());
    let queue_preview = playback
        .queue_preview(8)
        .into_iter()
        .map(map_bridge_song)
        .collect();

    BridgePlaybackState {
        is_playing: playback.is_playing(),
        now_playing,
        queue_preview,
    }
}

#[tauri::command]
fn bridge_set_now_playing(app_handle: tauri::AppHandle, video_id: String, title: String, artist: String) {
    core::bridge::playback_core().set_now_playing(video_id, title, artist, 0);
    emit_bridge_playback_state(&app_handle);
}

#[tauri::command]
fn bridge_set_playing(app_handle: tauri::AppHandle, is_playing: bool) {
    core::bridge::playback_core().set_playing(is_playing);
    emit_bridge_playback_state(&app_handle);
}

#[tauri::command]
fn bridge_toggle_play_pause(app_handle: tauri::AppHandle) -> bool {
    let is_playing = core::bridge::playback_core().toggle_play_pause();
    emit_bridge_playback_state(&app_handle);
    is_playing
}

#[tauri::command]
fn bridge_prev_track(app_handle: tauri::AppHandle) -> BridgeSong {
    let playback = core::bridge::playback_core();
    playback.prev_track();
    let song = map_bridge_song(playback.now_playing());
    emit_bridge_playback_state(&app_handle);
    song
}

#[tauri::command]
fn bridge_next_track(app_handle: tauri::AppHandle) -> BridgeSong {
    let playback = core::bridge::playback_core();
    playback.next_track();
    let song = map_bridge_song(playback.now_playing());
    emit_bridge_playback_state(&app_handle);
    song
}

#[tauri::command]
async fn bridge_seed_queue(app_handle: tauri::AppHandle, query: Option<String>, limit: Option<usize>) -> Result<(), String> {
    core::bridge::playback_core()
        .seed_queue_from_backend(query.as_deref().unwrap_or("gaming music mix"), limit.unwrap_or(12))
        .await?;
    emit_bridge_playback_state(&app_handle);
    Ok(())
}

fn make_song_item(t: &core::playback::NowPlaying) -> SongItem {
    let avatar = t.artist.chars().next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());
    let dur_str = if t.duration_secs > 0 {
        format!("{}:{:02}", t.duration_secs / 60, t.duration_secs % 60)
    } else {
        String::new()
    };
    let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", t.video_id));
    let (thumbnail, has_thumbnail) = if thumb_path.exists() {
        match slint::Image::load_from_path(&thumb_path) {
            Ok(img) => (img, true),
            Err(_) => (Default::default(), false),
        }
    } else {
        (Default::default(), false)
    };
    SongItem {
        video_id: SharedString::from(t.video_id.as_str()),
        title: SharedString::from(t.title.as_str()),
        artist: SharedString::from(t.artist.as_str()),
        album: SharedString::from(""),
        duration_str: SharedString::from(dur_str.as_str()),
        avatar_letter: SharedString::from(avatar.as_str()),
        duration_secs: t.duration_secs as i32,
        thumbnail,
        has_thumbnail,
    }
}

fn fetch_autoplay_queue(
    ui_weak: slint::Weak<NativeShellWindow>,
    autoplay_queue_data: Arc<Mutex<Vec<core::playback::NowPlaying>>>,
    video_id: String,
) {
    let seed_vid = video_id.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let songs = rt.block_on(async {
        let api = match ytmapi_rs::YtMusic::new_unauthenticated().await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[autoplay] failed to init api: {e}");
                return Vec::new();
            }
        };
        use ytmapi_rs::common::YoutubeID;
        let vid = ytmapi_rs::common::VideoID::from_raw(video_id);
        let tracks = match api.get_watch_playlist_from_video_id(vid).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[autoplay] watch playlist fetch failed: {e}");
                return Vec::new();
            }
        };
        tracks.into_iter().take(15).map(|r| {
            let dur = parse_duration(&r.duration).unwrap_or(0);
            core::playback::NowPlaying {
                video_id: r.video_id.get_raw().to_string(),
                title: r.title,
                artist: r.author,
                duration_secs: dur,
            }
        }).collect::<Vec<_>>()
    });
    // Filter out the seed song itself
    let songs: Vec<_> = songs.into_iter().filter(|s| s.video_id != seed_vid).collect();
    if songs.is_empty() {
        eprintln!("[autoplay] watch playlist returned 0 songs");
        return;
    }
    // Store in shared state
    {
        let mut aq = autoplay_queue_data.lock().unwrap();
        *aq = songs.clone();
    }
    // Collect video IDs that need thumbnails
    let need_thumbs: Vec<(usize, String)> = songs.iter().enumerate()
        .filter(|(_, s)| {
            !s.video_id.is_empty()
                && !std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", s.video_id)).exists()
        })
        .map(|(i, s)| (i, s.video_id.clone()))
        .collect();

    let ui_weak2 = ui_weak.clone();
    // Update UI model
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let items: Vec<SongItem> = songs.iter().map(|np| make_song_item(np)).collect();
            ui.set_autoplay_queue(ModelRc::new(VecModel::from(items)));
        }
    }).ok();

    // Fetch thumbnails in background using standard YouTube thumbnail URL
    if !need_thumbs.is_empty() {
        std::thread::spawn(move || {
            let http = match reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build() {
                Ok(c) => c,
                Err(_) => return,
            };
            for (idx, vid) in &need_thumbs {
                let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                if let Ok(resp) = http.get(&url).send() {
                    if let Ok(bytes) = resp.bytes() {
                        if let Ok(img) = image::load_from_memory(&bytes) {
                            let rgba = img.to_rgba8();
                            let (w, h) = (rgba.width(), rgba.height());
                            // Save to disk for future use
                            let _ = std::fs::write(&thumb_path, bytes.as_ref());
                            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                            let ui_w = ui_weak2.clone();
                            let idx_copy = *idx;
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    let slint_img = slint::Image::from_rgba8(buf);
                                    let model = ui.get_autoplay_queue();
                                    if let Some(row) = model.row_data(idx_copy) {
                                        let mut updated = row;
                                        updated.thumbnail = slint_img;
                                        updated.has_thumbnail = true;
                                        model.set_row_data(idx_copy, updated);
                                    }
                                }
                            }).ok();
                        }
                    }
                }
            }
        });
    }
}

fn refresh_native_shell_ui(ui: &NativeShellWindow, playback: &'static core::playback::PlaybackCore) {
    let track = playback.now_playing();
    ui.set_track_title(SharedString::from(track.title.as_str()));
    ui.set_track_artist(SharedString::from(track.artist.as_str()));
    ui.set_is_playing(playback.is_playing());
    ui.set_now_playing_video_id(SharedString::from(track.video_id.as_str()));
    let initial = track.title.chars().next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());
    ui.set_track_initial(SharedString::from(initial.as_str()));
    ui.set_is_liked(playback.is_liked(&track.video_id));

    let queue_items: Vec<SongItem> = playback.queue_upcoming().iter().map(make_song_item).collect();
    ui.set_queue(ModelRc::new(VecModel::from(queue_items)));

    let liked_items: Vec<SongItem> = playback.get_liked_songs().iter().map(make_song_item).collect();
    ui.set_liked_songs(ModelRc::new(VecModel::from(liked_items)));

    // Spawn background thumbnail fetch for items missing thumbnails
    let mut missing_vids: Vec<String> = Vec::new();
    for np in playback.queue_upcoming().iter() {
        let tp = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", np.video_id));
        if !tp.exists() && !np.video_id.is_empty() {
            missing_vids.push(np.video_id.clone());
        }
    }
    for np in playback.get_liked_songs().iter() {
        let tp = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", np.video_id));
        if !tp.exists() && !np.video_id.is_empty() && !missing_vids.contains(&np.video_id) {
            missing_vids.push(np.video_id.clone());
        }
    }
    if !missing_vids.is_empty() {
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .ok();
            if let Some(client) = client {
                for vid in &missing_vids {
                    let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                    if thumb_path.exists() { continue; }
                    let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                    if let Ok(resp) = client.get(&url).send() {
                        if resp.status().is_success() {
                            if let Ok(bytes) = resp.bytes() {
                                let _ = std::fs::write(&thumb_path, &bytes);
                            }
                        }
                    }
                }
                // Update UI with loaded thumbnails
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        // Refresh queue
                        let model = ui.get_queue();
                        let count = model.row_count();
                        let mut items: Vec<SongItem> = Vec::with_capacity(count);
                        let mut changed = false;
                        for i in 0..count {
                            let mut item = model.row_data(i).unwrap();
                            if !item.has_thumbnail {
                                let tp = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", item.video_id.as_str()));
                                if tp.exists() {
                                    if let Ok(img) = slint::Image::load_from_path(&tp) {
                                        item.thumbnail = img;
                                        item.has_thumbnail = true;
                                        changed = true;
                                    }
                                }
                            }
                            items.push(item);
                        }
                        if changed {
                            ui.set_queue(ModelRc::new(VecModel::from(items)));
                        }
                        // Refresh liked songs thumbnails
                        let model = ui.get_liked_songs();
                        let count = model.row_count();
                        let mut items: Vec<SongItem> = Vec::with_capacity(count);
                        let mut changed = false;
                        for i in 0..count {
                            let mut item = model.row_data(i).unwrap();
                            if !item.has_thumbnail {
                                let tp = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", item.video_id.as_str()));
                                if tp.exists() {
                                    if let Ok(img) = slint::Image::load_from_path(&tp) {
                                        item.thumbnail = img;
                                        item.has_thumbnail = true;
                                        changed = true;
                                    }
                                }
                            }
                            items.push(item);
                        }
                        if changed {
                            ui.set_liked_songs(ModelRc::new(VecModel::from(items)));
                        }
                    }
                }).ok();
            }
        });
    }
}

fn format_duration_secs(secs: u32) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn fetch_trending_songs(ui_weak: slint::Weak<NativeShellWindow>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let songs = rt.block_on(async {
            // Build a personalized query from the user's taste profile
            let query_string = {
                let core = crate::core::bridge::playback_core();
                let liked = core.get_liked_songs();
                let source = if !liked.is_empty() { liked } else { core.get_history() };

                if source.is_empty() {
                    "popular music new releases".to_string()
                } else {
                    // Count artist frequencies
                    let mut artist_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
                    for song in &source {
                        let artist = song.artist.trim().to_string();
                        if !artist.is_empty() {
                            *artist_counts.entry(artist).or_insert(0) += 1;
                        }
                    }
                    // Pick top 2-3 artists
                    let mut sorted: Vec<(String, usize)> = artist_counts.into_iter().collect();
                    sorted.sort_by(|a, b| b.1.cmp(&a.1));
                    let top_artists: Vec<String> = sorted.into_iter().take(3).map(|(name, _)| name).collect();
                    if top_artists.is_empty() {
                        "popular music new releases".to_string()
                    } else {
                        format!("{} new songs", top_artists.join(" "))
                    }
                }
            };

            let guest_api = ytmapi_rs::YtMusic::new_unauthenticated().await.ok()?;
            let results = guest_api.query(
                SearchQuery::new(query_string).with_filter(SongsFilter)
            ).await.ok()?;
            Some(map_search_results(results))
        });

        if let Some(songs) = songs {
            // Collect Send-safe raw data (include thumb URL)
            let raw: Vec<(String, String, String, u32, String)> = songs.into_iter().take(15).map(|s| {
                let dur = s.duration.unwrap_or(0);
                let thumb_url = s.thumbnails.last().map(|t| t.url.clone()).unwrap_or_default();
                (s.video_id, s.name, s.artist.name, dur, thumb_url)
            }).collect();

            // Extract thumb URLs before moving raw
            let thumb_urls: Vec<(usize, String)> = raw.iter().enumerate()
                .filter(|(_, (_, _, _, _, url))| !url.is_empty())
                .map(|(i, (_, _, _, _, url))| (i, url.clone()))
                .collect();

            let ui_weak2 = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let items: Vec<SongItem> = raw.iter().map(|(vid, title, artist, dur, _)| {
                        let avatar = title.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".to_string());
                        SongItem {
                            video_id: SharedString::from(vid.as_str()),
                            title: SharedString::from(title.as_str()),
                            artist: SharedString::from(artist.as_str()),
                            album: SharedString::default(),
                            duration_str: SharedString::from(format_duration_secs(*dur).as_str()),
                            avatar_letter: SharedString::from(avatar.as_str()),
                            duration_secs: *dur as i32,
                            thumbnail: slint::Image::default(),
                            has_thumbnail: false,
                        }
                    }).collect();
                    let model = std::rc::Rc::new(slint::VecModel::from(items));
                    ui.set_trending_songs(slint::ModelRc::from(model));
                }
            }).ok();

            // Fetch thumbnails in background
            if !thumb_urls.is_empty() {
                std::thread::spawn(move || {
                    let http = reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(8))
                        .build().ok();
                    let Some(http) = http else { return; };
                    for (idx, url) in thumb_urls {
                        if let Ok(resp) = http.get(&url).send() {
                            if let Ok(bytes) = resp.bytes() {
                                if let Ok(img) = image::load_from_memory(&bytes) {
                                    let rgba = img.to_rgba8();
                                    let (w, h) = (rgba.width(), rgba.height());
                                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                    let ui_w = ui_weak2.clone();
                                    slint::invoke_from_event_loop(move || {
                                        let slint_img = slint::Image::from_rgba8(buf);
                                        if let Some(ui) = ui_w.upgrade() {
                                            let model = ui.get_trending_songs();
                                            if let Some(row) = model.row_data(idx) {
                                                let mut updated = row;
                                                updated.thumbnail = slint_img;
                                                updated.has_thumbnail = true;
                                                model.set_row_data(idx, updated);
                                            }
                                        }
                                    }).ok();
                                }
                            }
                        }
                    }
                });
            }
        }
    });
}

fn fetch_personalized_songs(ui_weak: slint::Weak<NativeShellWindow>) {
    let playback = crate::core::bridge::playback_core();
    let liked = playback.get_liked_songs();
    if liked.is_empty() { return; }

    // Get most common artist from liked songs
    let mut artist_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for song in &liked {
        *artist_counts.entry(song.artist.clone()).or_default() += 1;
    }

    // Use the last liked song as seed for watch playlist
    let seed_vid = liked.last().unwrap().video_id.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let songs = rt.block_on(async {
            let guest_api = ytmapi_rs::YtMusic::new_unauthenticated().await.ok()?;
            let vid = ytmapi_rs::common::VideoID::from_raw(seed_vid);
            let tracks = guest_api.get_watch_playlist_from_video_id(vid).await.ok()?;
            Some(tracks.into_iter().take(15).map(|r| Song {
                video_id: r.video_id.get_raw().to_string(),
                name: r.title,
                artist: ArtistRef { name: r.author, browse_id: String::new() },
                album: None,
                duration: None,
                thumbnails: r.thumbnails.into_iter().map(|t| Thumbnail {
                    url: t.url, width: t.width as u64, height: t.height as u64,
                }).collect(),
            }).collect::<Vec<_>>())
        });

        if let Some(songs) = songs {
            let raw: Vec<(String, String, String, String)> = songs.into_iter().map(|s| {
                let thumb_url = s.thumbnails.last().map(|t| t.url.clone()).unwrap_or_default();
                (s.video_id, s.name, s.artist.name, thumb_url)
            }).collect();

            // Extract thumb URLs before moving raw
            let thumb_urls: Vec<(usize, String)> = raw.iter().enumerate()
                .filter(|(_, (_, _, _, url))| !url.is_empty())
                .map(|(i, (_, _, _, url))| (i, url.clone()))
                .collect();

            let ui_weak2 = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let items: Vec<SongItem> = raw.iter().map(|(vid, title, artist, _)| {
                        let avatar = title.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".to_string());
                        SongItem {
                            video_id: SharedString::from(vid.as_str()),
                            title: SharedString::from(title.as_str()),
                            artist: SharedString::from(artist.as_str()),
                            album: SharedString::default(),
                            duration_str: SharedString::default(),
                            avatar_letter: SharedString::from(avatar.as_str()),
                            duration_secs: 0,
                            thumbnail: slint::Image::default(),
                            has_thumbnail: false,
                        }
                    }).collect();
                    let model = std::rc::Rc::new(slint::VecModel::from(items));
                    ui.set_personalized_songs(slint::ModelRc::from(model));
                }
            }).ok();

            // Fetch thumbnails in background
            if !thumb_urls.is_empty() {
                std::thread::spawn(move || {
                    let http = reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(8))
                        .build().ok();
                    let Some(http) = http else { return; };
                    for (idx, url) in thumb_urls {
                        if let Ok(resp) = http.get(&url).send() {
                            if let Ok(bytes) = resp.bytes() {
                                if let Ok(img) = image::load_from_memory(&bytes) {
                                    let rgba = img.to_rgba8();
                                    let (w, h) = (rgba.width(), rgba.height());
                                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                    let ui_w = ui_weak2.clone();
                                    slint::invoke_from_event_loop(move || {
                                        let slint_img = slint::Image::from_rgba8(buf);
                                        if let Some(ui) = ui_w.upgrade() {
                                            let model = ui.get_personalized_songs();
                                            if let Some(row) = model.row_data(idx) {
                                                let mut updated = row;
                                                updated.thumbnail = slint_img;
                                                updated.has_thumbnail = true;
                                                model.set_row_data(idx, updated);
                                            }
                                        }
                                    }).ok();
                                }
                            }
                        }
                    }
                });
            }
        }
    });
}

/// Detects language from text by checking for non-Latin script characters.
/// Returns a language name suitable for searching (e.g., "Bengali", "Hindi", "Tamil").
fn detect_language_from_text(text: &str) -> Option<&'static str> {
    for ch in text.chars() {
        match ch {
            '\u{0980}'..='\u{09FF}' => return Some("Bengali"),
            '\u{0900}'..='\u{097F}' => return Some("Hindi"),
            '\u{0A00}'..='\u{0A7F}' => return Some("Punjabi"),
            '\u{0B80}'..='\u{0BFF}' => return Some("Tamil"),
            '\u{0C00}'..='\u{0C7F}' => return Some("Telugu"),
            '\u{0C80}'..='\u{0CFF}' => return Some("Kannada"),
            '\u{0D00}'..='\u{0D7F}' => return Some("Malayalam"),
            '\u{0A80}'..='\u{0AFF}' => return Some("Gujarati"),
            '\u{0B00}'..='\u{0B7F}' => return Some("Odia"),
            '\u{AC00}'..='\u{D7AF}' => return Some("Korean"),
            '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}' => return Some("Japanese"),
            '\u{0600}'..='\u{06FF}' => return Some("Arabic"),
            _ => {}
        }
    }
    None
}

/// Fetches enhanced home screen data: new releases, mixes, genre mix, and language section.
/// Runs entirely in background threads to avoid blocking the UI.
fn fetch_home_enhanced_data(ui_weak: slint::Weak<NativeShellWindow>) {
    let playback = crate::core::bridge::playback_core();
    let liked = playback.get_liked_songs();
    if liked.is_empty() {
        return;
    }

    // Collect unique artists from liked songs (up to top 5 by frequency)
    let mut artist_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for song in &liked {
        let artist = song.artist.trim().to_string();
        if !artist.is_empty() {
            *artist_counts.entry(artist).or_insert(0) += 1;
        }
    }
    let mut sorted_artists: Vec<(String, usize)> = artist_counts.into_iter().collect();
    sorted_artists.sort_by(|a, b| b.1.cmp(&a.1));
    let top_artists: Vec<String> = sorted_artists.iter().take(5).map(|(name, _)| name.clone()).collect();

    // Detect language from liked songs
    let detected_language = {
        let mut lang_counts: std::collections::HashMap<&'static str, usize> = std::collections::HashMap::new();
        for song in &liked {
            if let Some(lang) = detect_language_from_text(&song.title) {
                *lang_counts.entry(lang).or_insert(0) += 1;
            }
            if let Some(lang) = detect_language_from_text(&song.artist) {
                *lang_counts.entry(lang).or_insert(0) += 1;
            }
        }
        lang_counts.into_iter().max_by_key(|(_, count)| *count).map(|(lang, _)| lang)
    };

    // Determine genre keyword from liked songs (most common non-trivial word in titles)
    let genre_keyword = {
        let stop_words = ["the", "a", "an", "is", "it", "i", "you", "me", "my", "in", "on", "to", "of", "and", "or", "for", "with", "feat", "ft"];
        let mut word_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for song in &liked {
            for word in song.title.split_whitespace() {
                let w = word.to_lowercase().trim_matches(|c: char| !c.is_alphanumeric()).to_string();
                if w.len() > 3 && !stop_words.contains(&w.as_str()) {
                    *word_counts.entry(w).or_insert(0) += 1;
                }
            }
        }
        let mut sorted_words: Vec<(String, usize)> = word_counts.into_iter().filter(|(_, c)| *c > 1).collect();
        sorted_words.sort_by(|a, b| b.1.cmp(&a.1));
        sorted_words.first().map(|(word, _)| word.clone())
    };

    let top_artists_clone = top_artists.clone();

    // ── Thread 1: New releases (search latest albums from top artists) ────────
    {
        let ui_weak = ui_weak.clone();
        let artists = top_artists.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let albums = rt.block_on(async {
                use ytmapi_rs::YtMusic;
                use ytmapi_rs::query::{SearchQuery, search::AlbumsFilter};
                let api = match YtMusic::new_unauthenticated().await {
                    Ok(a) => a,
                    Err(_) => return Vec::new(),
                };
                let mut all_albums: Vec<(String, String, String, String, String)> = Vec::new(); // (title, browse_id, artist, year, thumb_url)
                for artist in artists.iter().take(3) {
                    let query = format!("{} new album", artist);
                    if let Ok(results) = api.query(SearchQuery::new(query).with_filter(AlbumsFilter)).await {
                        for alb in results.into_iter().take(3) {
                            let thumb_url = alb.thumbnails.last().map(|t| t.url.clone()).unwrap_or_default();
                            all_albums.push((
                                alb.title,
                                alb.album_id.get_raw().to_string(),
                                alb.artist,
                                alb.year,
                                thumb_url,
                            ));
                        }
                    }
                }
                // Deduplicate by browse_id
                let mut seen = std::collections::HashSet::new();
                all_albums.retain(|(_, id, _, _, _)| seen.insert(id.clone()));
                all_albums.truncate(8);
                all_albums
            });

            if albums.is_empty() { return; }

            let thumb_urls: Vec<(usize, String)> = albums.iter().enumerate()
                .filter(|(_, (_, _, _, _, url))| !url.is_empty())
                .map(|(i, (_, _, _, _, url))| (i, url.clone()))
                .collect();

            let ui_weak2 = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let items: Vec<AlbumItem> = albums.iter().map(|(title, browse_id, artist, year, _)| {
                        AlbumItem {
                            title: SharedString::from(title.as_str()),
                            browse_id: SharedString::from(browse_id.as_str()),
                            artist: SharedString::from(artist.as_str()),
                            year: SharedString::from(year.as_str()),
                            thumbnail: slint::Image::default(),
                            has_thumbnail: false,
                        }
                    }).collect();
                    ui.set_home_new_releases(ModelRc::new(VecModel::from(items)));
                }
            }).ok();

            // Fetch album thumbnails
            if !thumb_urls.is_empty() {
                std::thread::spawn(move || {
                    let http = match reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(8)).build() {
                        Ok(c) => c, Err(_) => return,
                    };
                    for (idx, url) in &thumb_urls {
                        if let Ok(resp) = http.get(url).send() {
                            if let Ok(bytes) = resp.bytes() {
                                if let Ok(img) = image::load_from_memory(&bytes) {
                                    let rgba = img.to_rgba8();
                                    let (w, h) = (rgba.width(), rgba.height());
                                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                    let ui_w = ui_weak2.clone();
                                    let idx_copy = *idx;
                                    slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_w.upgrade() {
                                            let slint_img = slint::Image::from_rgba8(buf);
                                            let model = ui.get_home_new_releases();
                                            if let Some(row) = model.row_data(idx_copy) {
                                                let mut updated = row;
                                                updated.thumbnail = slint_img;
                                                updated.has_thumbnail = true;
                                                model.set_row_data(idx_copy, updated);
                                            }
                                        }
                                    }).ok();
                                }
                            }
                        }
                    }
                });
            }
        });
    }

    // ── Thread 2: Mixes (3 personalized mixes from search) ────────────────────
    {
        let ui_weak = ui_weak.clone();
        let artists = top_artists_clone.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let (mix1, mix2, mix3, mix1_title, mix2_title, mix3_title) = rt.block_on(async {
                use ytmapi_rs::YtMusic;
                use ytmapi_rs::query::{SearchQuery, search::SongsFilter};
                let api = match YtMusic::new_unauthenticated().await {
                    Ok(a) => a,
                    Err(_) => return (Vec::new(), Vec::new(), Vec::new(), String::new(), String::new(), String::new()),
                };

                // Mix 1: "Your Mix" - songs similar to top artist
                let artist1 = artists.first().cloned().unwrap_or_else(|| "popular".to_string());
                let mix1_title = format!("{} Mix", artist1);
                let query1 = format!("{} mix songs", artist1);
                let mix1: Vec<(String, String, String, u32, String)> = api.query(SearchQuery::new(query1).with_filter(SongsFilter)).await
                    .unwrap_or_default()
                    .into_iter().take(10)
                    .map(|s| {
                        let dur = parse_duration(&s.duration).unwrap_or(0);
                        let thumb = s.thumbnails.last().map(|t| t.url.clone()).unwrap_or_default();
                        (s.video_id.get_raw().to_string(), s.title, s.artist, dur, thumb)
                    }).collect();

                // Mix 2: "Discover Mix" - songs from second artist or discovery
                let artist2 = artists.get(1).cloned().unwrap_or_else(|| "new music".to_string());
                let mix2_title = format!("Discover: {}", artist2);
                let query2 = format!("{} discover new songs", artist2);
                let mix2: Vec<(String, String, String, u32, String)> = api.query(SearchQuery::new(query2).with_filter(SongsFilter)).await
                    .unwrap_or_default()
                    .into_iter().take(10)
                    .map(|s| {
                        let dur = parse_duration(&s.duration).unwrap_or(0);
                        let thumb = s.thumbnails.last().map(|t| t.url.clone()).unwrap_or_default();
                        (s.video_id.get_raw().to_string(), s.title, s.artist, dur, thumb)
                    }).collect();

                // Mix 3: "Chill Mix" - relaxing songs
                let mix3_title = "Chill Mix".to_string();
                let query3 = "chill vibes relax lo-fi".to_string();
                let mix3: Vec<(String, String, String, u32, String)> = api.query(SearchQuery::new(query3).with_filter(SongsFilter)).await
                    .unwrap_or_default()
                    .into_iter().take(10)
                    .map(|s| {
                        let dur = parse_duration(&s.duration).unwrap_or(0);
                        let thumb = s.thumbnails.last().map(|t| t.url.clone()).unwrap_or_default();
                        (s.video_id.get_raw().to_string(), s.title, s.artist, dur, thumb)
                    }).collect();

                (mix1, mix2, mix3, mix1_title, mix2_title, mix3_title)
            });

            if mix1.is_empty() && mix2.is_empty() && mix3.is_empty() { return; }

            // Collect all thumb URLs for fetching
            let all_thumb_urls: Vec<(u8, usize, String)> = mix1.iter().enumerate()
                .filter(|(_, (_, _, _, _, url))| !url.is_empty())
                .map(|(i, (_, _, _, _, url))| (1u8, i, url.clone()))
                .chain(mix2.iter().enumerate()
                    .filter(|(_, (_, _, _, _, url))| !url.is_empty())
                    .map(|(i, (_, _, _, _, url))| (2u8, i, url.clone())))
                .chain(mix3.iter().enumerate()
                    .filter(|(_, (_, _, _, _, url))| !url.is_empty())
                    .map(|(i, (_, _, _, _, url))| (3u8, i, url.clone())))
                .collect();

            let ui_weak2 = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let make_song_items = |raw: &[(String, String, String, u32, String)]| -> Vec<SongItem> {
                        raw.iter().map(|(vid, title, artist, dur, _)| {
                            let avatar = title.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".to_string());
                            SongItem {
                                video_id: SharedString::from(vid.as_str()),
                                title: SharedString::from(title.as_str()),
                                artist: SharedString::from(artist.as_str()),
                                album: SharedString::default(),
                                duration_str: SharedString::from(format_duration_secs(*dur).as_str()),
                                avatar_letter: SharedString::from(avatar.as_str()),
                                duration_secs: *dur as i32,
                                thumbnail: slint::Image::default(),
                                has_thumbnail: false,
                            }
                        }).collect()
                    };
                    ui.set_home_mix_1(ModelRc::new(VecModel::from(make_song_items(&mix1))));
                    ui.set_home_mix_2(ModelRc::new(VecModel::from(make_song_items(&mix2))));
                    ui.set_home_mix_3(ModelRc::new(VecModel::from(make_song_items(&mix3))));
                    ui.set_home_mix_1_title(SharedString::from(mix1_title.as_str()));
                    ui.set_home_mix_2_title(SharedString::from(mix2_title.as_str()));
                    ui.set_home_mix_3_title(SharedString::from(mix3_title.as_str()));
                }
            }).ok();

            // Fetch thumbnails for mixes
            if !all_thumb_urls.is_empty() {
                std::thread::spawn(move || {
                    let http = match reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(8)).build() {
                        Ok(c) => c, Err(_) => return,
                    };
                    for (mix_num, idx, url) in &all_thumb_urls {
                        if let Ok(resp) = http.get(url).send() {
                            if let Ok(bytes) = resp.bytes() {
                                if let Ok(img) = image::load_from_memory(&bytes) {
                                    let rgba = img.to_rgba8();
                                    let (w, h) = (rgba.width(), rgba.height());
                                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                    let ui_w = ui_weak2.clone();
                                    let mix_num_copy = *mix_num;
                                    let idx_copy = *idx;
                                    slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_w.upgrade() {
                                            let slint_img = slint::Image::from_rgba8(buf);
                                            let model = match mix_num_copy {
                                                1 => ui.get_home_mix_1(),
                                                2 => ui.get_home_mix_2(),
                                                3 => ui.get_home_mix_3(),
                                                _ => return,
                                            };
                                            if let Some(row) = model.row_data(idx_copy) {
                                                let mut updated = row;
                                                updated.thumbnail = slint_img;
                                                updated.has_thumbnail = true;
                                                model.set_row_data(idx_copy, updated);
                                            }
                                        }
                                    }).ok();
                                }
                            }
                        }
                    }
                });
            }
        });
    }

    // ── Thread 3: Genre mix ───────────────────────────────────────────────────
    if let Some(genre_kw) = genre_keyword {
        let ui_weak = ui_weak.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let songs = rt.block_on(async {
                use ytmapi_rs::YtMusic;
                use ytmapi_rs::query::{SearchQuery, search::SongsFilter};
                let api = match YtMusic::new_unauthenticated().await {
                    Ok(a) => a,
                    Err(_) => return Vec::new(),
                };
                let query = format!("{} songs playlist", genre_kw);
                api.query(SearchQuery::new(query).with_filter(SongsFilter)).await
                    .unwrap_or_default()
                    .into_iter().take(10)
                    .map(|s| {
                        let dur = parse_duration(&s.duration).unwrap_or(0);
                        let thumb = s.thumbnails.last().map(|t| t.url.clone()).unwrap_or_default();
                        (s.video_id.get_raw().to_string(), s.title, s.artist, dur, thumb)
                    }).collect::<Vec<_>>()
            });

            if songs.is_empty() { return; }

            let thumb_urls: Vec<(usize, String)> = songs.iter().enumerate()
                .filter(|(_, (_, _, _, _, url))| !url.is_empty())
                .map(|(i, (_, _, _, _, url))| (i, url.clone()))
                .collect();

            let genre_title = format!("More \"{}\"", genre_kw);
            let ui_weak2 = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let items: Vec<SongItem> = songs.iter().map(|(vid, title, artist, dur, _)| {
                        let avatar = title.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".to_string());
                        SongItem {
                            video_id: SharedString::from(vid.as_str()),
                            title: SharedString::from(title.as_str()),
                            artist: SharedString::from(artist.as_str()),
                            album: SharedString::default(),
                            duration_str: SharedString::from(format_duration_secs(*dur).as_str()),
                            avatar_letter: SharedString::from(avatar.as_str()),
                            duration_secs: *dur as i32,
                            thumbnail: slint::Image::default(),
                            has_thumbnail: false,
                        }
                    }).collect();
                    ui.set_home_genre_mix(ModelRc::new(VecModel::from(items)));
                    ui.set_home_genre_mix_title(SharedString::from(genre_title.as_str()));
                }
            }).ok();

            // Fetch thumbnails for genre mix
            if !thumb_urls.is_empty() {
                std::thread::spawn(move || {
                    let http = match reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(8)).build() {
                        Ok(c) => c, Err(_) => return,
                    };
                    for (idx, url) in &thumb_urls {
                        if let Ok(resp) = http.get(url).send() {
                            if let Ok(bytes) = resp.bytes() {
                                if let Ok(img) = image::load_from_memory(&bytes) {
                                    let rgba = img.to_rgba8();
                                    let (w, h) = (rgba.width(), rgba.height());
                                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                    let ui_w = ui_weak2.clone();
                                    let idx_copy = *idx;
                                    slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_w.upgrade() {
                                            let slint_img = slint::Image::from_rgba8(buf);
                                            let model = ui.get_home_genre_mix();
                                            if let Some(row) = model.row_data(idx_copy) {
                                                let mut updated = row;
                                                updated.thumbnail = slint_img;
                                                updated.has_thumbnail = true;
                                                model.set_row_data(idx_copy, updated);
                                            }
                                        }
                                    }).ok();
                                }
                            }
                        }
                    }
                });
            }
        });
    }

    // ── Thread 4: Language-based music ────────────────────────────────────────
    if let Some(language) = detected_language {
        let ui_weak = ui_weak.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let songs = rt.block_on(async {
                use ytmapi_rs::YtMusic;
                use ytmapi_rs::query::{SearchQuery, search::SongsFilter};
                let api = match YtMusic::new_unauthenticated().await {
                    Ok(a) => a,
                    Err(_) => return Vec::new(),
                };
                let query = format!("{} music popular songs", language);
                api.query(SearchQuery::new(query).with_filter(SongsFilter)).await
                    .unwrap_or_default()
                    .into_iter().take(10)
                    .map(|s| {
                        let dur = parse_duration(&s.duration).unwrap_or(0);
                        let thumb = s.thumbnails.last().map(|t| t.url.clone()).unwrap_or_default();
                        (s.video_id.get_raw().to_string(), s.title, s.artist, dur, thumb)
                    }).collect::<Vec<_>>()
            });

            if songs.is_empty() { return; }

            let thumb_urls: Vec<(usize, String)> = songs.iter().enumerate()
                .filter(|(_, (_, _, _, _, url))| !url.is_empty())
                .map(|(i, (_, _, _, _, url))| (i, url.clone()))
                .collect();

            let lang_title = format!("{} Music", language);
            let ui_weak2 = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let items: Vec<SongItem> = songs.iter().map(|(vid, title, artist, dur, _)| {
                        let avatar = title.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".to_string());
                        SongItem {
                            video_id: SharedString::from(vid.as_str()),
                            title: SharedString::from(title.as_str()),
                            artist: SharedString::from(artist.as_str()),
                            album: SharedString::default(),
                            duration_str: SharedString::from(format_duration_secs(*dur).as_str()),
                            avatar_letter: SharedString::from(avatar.as_str()),
                            duration_secs: *dur as i32,
                            thumbnail: slint::Image::default(),
                            has_thumbnail: false,
                        }
                    }).collect();
                    ui.set_home_language_section(ModelRc::new(VecModel::from(items)));
                    ui.set_home_language_title(SharedString::from(lang_title.as_str()));
                }
            }).ok();

            // Fetch thumbnails for language section
            if !thumb_urls.is_empty() {
                std::thread::spawn(move || {
                    let http = match reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(8)).build() {
                        Ok(c) => c, Err(_) => return,
                    };
                    for (idx, url) in &thumb_urls {
                        if let Ok(resp) = http.get(url).send() {
                            if let Ok(bytes) = resp.bytes() {
                                if let Ok(img) = image::load_from_memory(&bytes) {
                                    let rgba = img.to_rgba8();
                                    let (w, h) = (rgba.width(), rgba.height());
                                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                    let ui_w = ui_weak2.clone();
                                    let idx_copy = *idx;
                                    slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_w.upgrade() {
                                            let slint_img = slint::Image::from_rgba8(buf);
                                            let model = ui.get_home_language_section();
                                            if let Some(row) = model.row_data(idx_copy) {
                                                let mut updated = row;
                                                updated.thumbnail = slint_img;
                                                updated.has_thumbnail = true;
                                                model.set_row_data(idx_copy, updated);
                                            }
                                        }
                                    }).ok();
                                }
                            }
                        }
                    }
                });
            }
        });
    }
}

pub fn run_native_shell() -> Result<(), slint::PlatformError> {
    let playback = core::bridge::playback_core();

    if let Err(err) = playback.enable_audio_output() {
        log::error!("Failed to enable native audio output: {err}");
    }

    // Seed queue in background thread, update UI when done
    {
        let ui_handle_seed: Option<slint::Weak<NativeShellWindow>> = None;
        let _ = ui_handle_seed; // will be set after ui creation
    }

    let ui = NativeShellWindow::new()?;

    // Load liked songs immediately on startup
    {
        let liked_items: Vec<SongItem> = playback.get_liked_songs().iter().map(make_song_item).collect();
        ui.set_liked_songs(ModelRc::new(VecModel::from(liked_items)));
    }


    // ── Toggle sidebar ────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_sidebar(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_sidebar_open(!ui.get_sidebar_open());
            }
        });
    }

    // ── Cache limit ───────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_set_cache_limit(move |limit_mb| {
            let limit_bytes = (limit_mb as u64).saturating_mul(1024 * 1024);
            if let Ok(mut cache) = crate::core::cache::AudioCache::global().lock() {
                cache.set_limit(limit_bytes);
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_cache_limit_mb(limit_mb);
            }
        });
    }

    // ── Essential add-ons (yt-dlp / ffmpeg) ───────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_refresh_addons(move || {
            let ui_weak2 = ui_weak.clone();
            std::thread::spawn(move || {
                let yt = crate::core::addons::ytdlp_installed();
                let ff = crate::core::addons::ffmpeg_installed();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_ytdlp_installed(yt);
                        ui.set_ffmpeg_installed(ff);
                    }
                }).ok();
            });
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_install_ytdlp(move || {
            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_addon_busy(true);
                ui.set_addon_status(SharedString::from("Installing yt-dlp…"));
            }
            std::thread::spawn(move || {
                let result = crate::core::addons::install_ytdlp();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_addon_busy(false);
                        match result {
                            Ok(_) => {
                                ui.set_ytdlp_installed(true);
                                ui.set_addon_status(SharedString::from("yt-dlp installed."));
                            }
                            Err(e) => {
                                ui.set_addon_status(SharedString::from(format!("yt-dlp install failed: {}", e)));
                            }
                        }
                    }
                }).ok();
            });
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_install_ffmpeg(move || {
            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_addon_busy(true);
                ui.set_addon_status(SharedString::from("Installing ffmpeg… this may take a moment."));
            }
            std::thread::spawn(move || {
                let result = crate::core::addons::install_ffmpeg();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_addon_busy(false);
                        match result {
                            Ok(_) => {
                                ui.set_ffmpeg_installed(true);
                                ui.set_addon_status(SharedString::from("ffmpeg installed."));
                            }
                            Err(e) => {
                                ui.set_addon_status(SharedString::from(format!("ffmpeg install failed: {}", e)));
                            }
                        }
                    }
                }).ok();
            });
        });
    }
    // Initial add-on detection at startup
    {
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            let yt = crate::core::addons::ytdlp_installed();
            let ff = crate::core::addons::ffmpeg_installed();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_ytdlp_installed(yt);
                    ui.set_ffmpeg_installed(ff);
                }
            }).ok();
        });
    }

    // ── Toggle queue pane ─────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_queue_pane(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_queue_pane_open(!ui.get_queue_pane_open());
            }
        });
    }

    // ── Remove from queue ─────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_remove_from_queue(move |idx| {
            playback.remove_from_queue(idx as usize);
            if let Some(ui) = ui_weak.upgrade() {
                let items: Vec<SongItem> = playback.queue_upcoming().iter().map(make_song_item).collect();
                ui.set_queue(ModelRc::new(VecModel::from(items)));
            }
        });
    }

    // ── Explore genre ─────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_explore_genre(move |genre_query| {
            let query = genre_query.to_string();
            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_loading(true);
                ui.set_current_view(SharedString::from("Search"));
            }
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                let result = rt.block_on(async {
                    let api = YtMusic::new_unauthenticated().await?;
                    api.query(SearchQuery::new(query).with_filter(SongsFilter)).await
                });
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_is_loading(false);
                        if let Ok(songs) = result {
                            let items: Vec<SongItem> = songs.into_iter().map(|s| {
                                let avatar = s.artist.chars().next()
                                    .map(|c| c.to_uppercase().to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                let dur_secs: u32 = s.duration.as_str().split(':')
                                    .collect::<Vec<_>>().iter()
                                    .fold(0u32, |acc, p| acc * 60 + p.parse::<u32>().unwrap_or(0));
                                SongItem {
                                    video_id: SharedString::from(s.video_id.get_raw()),
                                    title: SharedString::from(s.title.as_str()),
                                    artist: SharedString::from(s.artist.as_str()),
                                    album: SharedString::from(s.album.as_ref().map(|a| a.name.as_str()).unwrap_or("")),
                                    duration_str: SharedString::from(s.duration.as_str()),
                                    avatar_letter: SharedString::from(avatar.as_str()),
                                    duration_secs: dur_secs as i32,
                                    thumbnail: Default::default(),
                                    has_thumbnail: false,
                                }
                            }).collect();
                            ui.set_search_results(ModelRc::new(VecModel::from(items)));
                        }
                    }
                }).ok();
            });
        });
    }

    // ── Load explore data (Moods & Genres + New Releases) ─────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_load_explore_data(move || {
            let ui_weak2 = ui_weak.clone();
            let ui_weak3 = ui_weak.clone();
            // Fetch mood categories in background
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let result = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::query::GetMoodCategoriesQuery;
                    let api = YtMusic::new_unauthenticated().await.map_err(|e| e.to_string())?;
                    api.query(GetMoodCategoriesQuery).await.map_err(|e| e.to_string())
                });
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        if let Ok(sections) = result {
                            let mut moods: Vec<MoodItem> = Vec::new();
                            for section in sections {
                                for cat in section.mood_categories {
                                    use ytmapi_rs::common::YoutubeID;
                                    moods.push(MoodItem {
                                        title: SharedString::from(cat.title.as_str()),
                                        params: SharedString::from(cat.params.get_raw()),
                                    });
                                }
                            }
                            // Limit to first 20 moods for UI
                            moods.truncate(20);
                            ui.set_explore_moods(ModelRc::new(VecModel::from(moods)));
                        }
                    }
                }).ok();
            });
            // Fetch new releases in background
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let result = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    let api = YtMusic::new_unauthenticated().await.map_err(|e| e.to_string())?;
                    api.query(SearchQuery::new("new music 2026".to_string()).with_filter(SongsFilter)).await.map_err(|e| e.to_string())
                });
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak3.upgrade() {
                        if let Ok(songs) = result {
                            let items: Vec<SongItem> = songs.into_iter().take(10).map(|s| {
                                let avatar = s.artist.chars().next()
                                    .map(|c| c.to_uppercase().to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                let dur_secs: u32 = s.duration.as_str().split(':')
                                    .collect::<Vec<_>>().iter()
                                    .fold(0u32, |acc, p| acc * 60 + p.parse::<u32>().unwrap_or(0));
                                SongItem {
                                    video_id: SharedString::from(s.video_id.get_raw()),
                                    title: SharedString::from(s.title.as_str()),
                                    artist: SharedString::from(s.artist.as_str()),
                                    album: SharedString::from(s.album.as_ref().map(|a| a.name.as_str()).unwrap_or("")),
                                    duration_str: SharedString::from(s.duration.as_str()),
                                    avatar_letter: SharedString::from(avatar.as_str()),
                                    duration_secs: dur_secs as i32,
                                    thumbnail: Default::default(),
                                    has_thumbnail: false,
                                }
                            }).collect();
                            ui.set_explore_new_releases(ModelRc::new(VecModel::from(items)));
                        }
                    }
                }).ok();
            });
        });
    }

    // ── Explore mood (fetch playlists for a mood category) ────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_explore_mood(move |params_str| {
            let params_raw = params_str.to_string();
            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_loading(true);
                ui.set_current_view(SharedString::from("Search"));
            }
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let result = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::query::GetMoodPlaylistsQuery;
                    use ytmapi_rs::common::{MoodCategoryParams, YoutubeID};
                    let api = YtMusic::new_unauthenticated().await.map_err(|e| e.to_string())?;
                    let params = MoodCategoryParams::from_raw(params_raw);
                    let categories = api.query(GetMoodPlaylistsQuery::new(params)).await.map_err(|e| e.to_string())?;
                    // Get the first playlist from the first category and fetch its tracks
                    if let Some(cat) = categories.first() {
                        if let Some(playlist) = cat.playlists.first() {
                            let playlist_id = playlist.playlist_id.clone();
                            let tracks = api.get_playlist_tracks(playlist_id).await.map_err(|e| e.to_string())?;
                            return Ok(tracks);
                        }
                    }
                    Ok::<Vec<ytmapi_rs::parse::PlaylistItem>, String>(vec![])
                });
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_is_loading(false);
                        if let Ok(tracks) = result {
                            let items: Vec<SongItem> = map_playlist_items(tracks).into_iter().map(|s| {
                                let avatar = s.artist.name.chars().next()
                                    .map(|c| c.to_uppercase().to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                let dur_str = s.duration.map(|d| {
                                    let m = d / 60;
                                    let s_rem = d % 60;
                                    format!("{}:{:02}", m, s_rem)
                                }).unwrap_or_default();
                                SongItem {
                                    video_id: SharedString::from(s.video_id.as_str()),
                                    title: SharedString::from(s.name.as_str()),
                                    artist: SharedString::from(s.artist.name.as_str()),
                                    album: SharedString::from(s.album.as_ref().map(|a| a.name.as_str()).unwrap_or("")),
                                    duration_str: SharedString::from(dur_str.as_str()),
                                    avatar_letter: SharedString::from(avatar.as_str()),
                                    duration_secs: s.duration.unwrap_or(0) as i32,
                                    thumbnail: Default::default(),
                                    has_thumbnail: false,
                                }
                            }).collect();
                            ui.set_search_results(ModelRc::new(VecModel::from(items)));
                        }
                    }
                }).ok();
            });
        });
    }

    // ── Navigation History (Spotify-style back/forward) ───────────────────────

    #[derive(Clone, Debug)]
    struct NavEntry {
        view: String,
        context_id: String, // artist/album browse_id, or empty
    }

    let nav_history: Arc<Mutex<Vec<NavEntry>>> = Arc::new(Mutex::new(vec![
        NavEntry { view: "Home".to_string(), context_id: String::new() }
    ]));
    let nav_cursor: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let nav_restoring: Arc<std::sync::atomic::AtomicBool> = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Autoplay state
    let autoplay_enabled: Arc<std::sync::atomic::AtomicBool> = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let autoplay_seed_vid: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let autoplay_queue_data: Arc<Mutex<Vec<core::playback::NowPlaying>>> = Arc::new(Mutex::new(Vec::new()));

    fn push_nav_entry(nav_history: &Arc<Mutex<Vec<NavEntry>>>, nav_cursor: &Arc<Mutex<usize>>, view: String, context_id: String) {
        let mut hist = nav_history.lock().unwrap();
        let mut cur = nav_cursor.lock().unwrap();
        hist.truncate(*cur + 1);
        hist.push(NavEntry { view, context_id });
        *cur = hist.len() - 1;
    }

    fn update_nav_buttons(ui: &NativeShellWindow, hist: &[NavEntry], cursor: usize) {
        ui.set_can_go_back(cursor > 0);
        ui.set_can_go_forward(cursor < hist.len().saturating_sub(1));
    }

    // ── Navigate ──────────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        ui.on_navigate(move |view| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_current_view(view.clone());
                push_nav_entry(&nav_history, &nav_cursor, view.to_string(), String::new());
                let hist = nav_history.lock().unwrap();
                let cur = *nav_cursor.lock().unwrap();
                update_nav_buttons(&ui, &hist, cur);
                // Trigger explore data load when navigating to Explore
                if view.as_str() == "Explore" {
                    ui.invoke_load_explore_data();
                }
                // Trigger home data load when navigating to Home (if not yet loaded)
                if view.as_str() == "Home" {
                    let model = ui.get_home_new_releases();
                    if model.row_count() == 0 {
                        ui.invoke_load_home_data();
                    }
                }
            }
        });
    }

    // ── Nav Back ──────────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        let nav_restoring = nav_restoring.clone();
        ui.on_nav_back(move || {
            let hist = nav_history.lock().unwrap();
            let mut cur = nav_cursor.lock().unwrap();
            if *cur > 0 {
                *cur -= 1;
                let entry = hist[*cur].clone();
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_current_view(SharedString::from(entry.view.as_str()));
                    update_nav_buttons(&ui, &hist, *cur);
                    if entry.view == "Artist" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_artist(SharedString::from(entry.context_id.as_str()));
                        nav_restoring.store(false, std::sync::atomic::Ordering::Relaxed);
                    } else if entry.view == "Album" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_album(SharedString::from(entry.context_id.as_str()));
                        nav_restoring.store(false, std::sync::atomic::Ordering::Relaxed);
                    } else if entry.view == "Playlist" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_playlist(SharedString::from(entry.context_id.as_str()));
                        nav_restoring.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        });
    }

    // ── Nav Forward ───────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        let nav_restoring = nav_restoring.clone();
        ui.on_nav_forward(move || {
            let hist = nav_history.lock().unwrap();
            let mut cur = nav_cursor.lock().unwrap();
            if *cur < hist.len() - 1 {
                *cur += 1;
                let entry = hist[*cur].clone();
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_current_view(SharedString::from(entry.view.as_str()));
                    update_nav_buttons(&ui, &hist, *cur);
                    if entry.view == "Artist" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_artist(SharedString::from(entry.context_id.as_str()));
                        nav_restoring.store(false, std::sync::atomic::Ordering::Relaxed);
                    } else if entry.view == "Album" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_album(SharedString::from(entry.context_id.as_str()));
                        nav_restoring.store(false, std::sync::atomic::Ordering::Relaxed);
                    } else if entry.view == "Playlist" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_playlist(SharedString::from(entry.context_id.as_str()));
                        nav_restoring.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        });
    }

    // ── Toggle play/pause ─────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_play_pause(move || {
            playback.toggle_play_pause();
            let is_playing = playback.is_playing();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_playing(is_playing);
            }
        });
    }

    // ── Prev track ────────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_prev_track(move || {
            playback.prev_track();
            if let Some(ui) = ui_weak.upgrade() {
                refresh_native_shell_ui(&ui, playback);
            }
        });
    }

    // ── Next track ────────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_next_track(move || {
            playback.next_track();
            if let Some(ui) = ui_weak.upgrade() {
                refresh_native_shell_ui(&ui, playback);
            }
        });
    }

    // ── Play specific song ────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let autoplay_seed_vid = autoplay_seed_vid.clone();
        let autoplay_queue_data = autoplay_queue_data.clone();
        ui.on_play_song(move |video_id, title, artist, duration_secs| {
            // set_now_playing calls sync_audio_playback internally.
            // We set is_playing FIRST so sync_audio_playback actually starts audio.
            {
                let mut state = playback.state_lock();
                state.is_playing = true;
            }
            playback.set_now_playing(video_id.as_str(), title.as_str(), artist.as_str(), duration_secs as u32);
            if let Some(ui) = ui_weak.upgrade() {
                refresh_native_shell_ui(&ui, playback);
            }
            // Update autoplay seed (this is an "external play")
            autoplay_seed_vid.lock().unwrap().replace_range(.., video_id.as_str());
            // Clear old autoplay queue and fetch new one
            autoplay_queue_data.lock().unwrap().clear();
            let ui_w = ui_weak.clone();
            let aq_data = autoplay_queue_data.clone();
            let vid = video_id.to_string();
            std::thread::spawn(move || {
                fetch_autoplay_queue(ui_w, aq_data, vid);
            });
        });
    }

    // ── Seek — click or drag to a position on the progress bar ──────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_seek(move |fraction| {
            let fraction = fraction.clamp(0.0, 1.0) as f64;
            let dur = playback.track_duration_secs() as f64;
            let target_secs = fraction * dur;
            playback.seek_to_secs(target_secs);
            // Snap UI immediately to the clicked position.
            // Audio follows via sink.try_seek() (Range request for streams, file-seek for cached).
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_progress(fraction as f32);
                let m = target_secs as u64 / 60;
                let s = target_secs as u64 % 60;
                ui.set_current_time(SharedString::from(format!("{m}:{s:02}")));
            }
        });
    }

    // ── Volume ────────────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_set_volume(move |v| {
            let v = v.clamp(0.0, 1.0);
            playback.set_volume(v);
            // Update visual immediately so slider moves
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_volume(v);
            }
        });
    }

    // ── Queue song (add without playing) ─────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_queue_song(move |video_id, title, artist| {
            let dur = {
                let q = playback.full_queue();
                q.iter().find(|s| s.video_id == video_id.as_str()).map(|s| s.duration_secs).unwrap_or(0)
            };
            playback.add_to_queue(video_id.as_str(), title.as_str(), artist.as_str(), dur);
            if let Some(ui) = ui_weak.upgrade() {
                let items: Vec<SongItem> = playback.queue_upcoming().iter().map(make_song_item).collect();
                ui.set_queue(ModelRc::new(VecModel::from(items)));
            }
        });
    }

    // ── Play next (insert at top of queue) ───────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let autoplay_queue_data = autoplay_queue_data.clone();
        ui.on_play_next_song(move |video_id, title, artist, duration_secs| {
            playback.play_next(video_id.as_str(), title.as_str(), artist.as_str(), duration_secs as u32);
            // Also remove from autoplay queue if present
            {
                let mut aq = autoplay_queue_data.lock().unwrap();
                aq.retain(|s| s.video_id != video_id.as_str());
            }
            if let Some(ui) = ui_weak.upgrade() {
                let items: Vec<SongItem> = playback.queue_upcoming().iter().map(make_song_item).collect();
                ui.set_queue(ModelRc::new(VecModel::from(items)));
                let aq = autoplay_queue_data.lock().unwrap();
                let ap_items: Vec<SongItem> = aq.iter().map(|np| make_song_item(np)).collect();
                ui.set_autoplay_queue(ModelRc::new(VecModel::from(ap_items)));
            }
        });
    }

    // ── Remove from autoplay queue ───────────────────────────────────────────
    {
        let autoplay_queue_data = autoplay_queue_data.clone();
        let ui_weak = ui.as_weak();
        ui.on_remove_from_autoplay(move |index| {
            let songs = {
                let mut aq = autoplay_queue_data.lock().unwrap();
                if (index as usize) < aq.len() {
                    aq.remove(index as usize);
                }
                aq.clone()
            };
            if let Some(ui) = ui_weak.upgrade() {
                let items: Vec<SongItem> = songs.iter().map(|np| make_song_item(np)).collect();
                ui.set_autoplay_queue(ModelRc::new(VecModel::from(items)));
            }
        });
    }

    // ── Autoplay toggle ──────────────────────────────────────────────────────
    {
        let autoplay_enabled = autoplay_enabled.clone();
        let ui_weak = ui.as_weak();
        ui.on_set_autoplay_enabled(move |val| {
            autoplay_enabled.store(val, std::sync::atomic::Ordering::Relaxed);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_autoplay_enabled(val);
            }
        });
    }

    // ── Play from autoplay queue ─────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let autoplay_queue_data = autoplay_queue_data.clone();
        let autoplay_seed_vid = autoplay_seed_vid.clone();
        ui.on_play_autoplay_song(move |index| {
            let song = {
                let aq = autoplay_queue_data.lock().unwrap();
                aq.get(index as usize).cloned()
            };
            if let Some(song) = song {
                {
                    let mut state = playback.state_lock();
                    state.is_playing = true;
                }
                playback.set_now_playing(&song.video_id, &song.title, &song.artist, song.duration_secs);
                if let Some(ui) = ui_weak.upgrade() {
                    refresh_native_shell_ui(&ui, playback);
                }
                // Re-seed autoplay from the newly played song
                autoplay_seed_vid.lock().unwrap().replace_range(.., &song.video_id);
                autoplay_queue_data.lock().unwrap().clear();
                let ui_w = ui_weak.clone();
                let aq_data = autoplay_queue_data.clone();
                let vid = song.video_id.clone();
                std::thread::spawn(move || {
                    fetch_autoplay_queue(ui_w, aq_data, vid);
                });
            }
        });
    }

    // ── Like / unlike current track ───────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_like_current(move || {
            let video_id = playback.now_playing().video_id;
            let _was_liked = playback.is_liked(&video_id);
            let is_now_liked = playback.toggle_like(&video_id);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_liked(is_now_liked);
                let model = ui.get_liked_songs();
                if is_now_liked {
                    // Add new song to the end with thumbnail if available
                    let np = playback.now_playing();
                    let avatar = np.artist.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or("?".into());
                    let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", np.video_id));
                    let (thumbnail, has_thumbnail) = if thumb_path.exists() {
                        slint::Image::load_from_path(&thumb_path)
                            .map(|img| (img, true))
                            .unwrap_or((slint::Image::default(), false))
                    } else {
                        (slint::Image::default(), false)
                    };
                    let new_item = SongItem {
                        video_id: SharedString::from(np.video_id.as_str()),
                        title: SharedString::from(np.title.as_str()),
                        artist: SharedString::from(np.artist.as_str()),
                        album: SharedString::default(),
                        duration_str: SharedString::from(format!("{}:{:02}", np.duration_secs / 60, np.duration_secs % 60)),
                        avatar_letter: SharedString::from(avatar),
                        duration_secs: np.duration_secs as i32,
                        thumbnail,
                        has_thumbnail,
                    };
                    // Rebuild with existing items (preserving thumbnails) + new item
                    let count = model.row_count();
                    let mut items: Vec<SongItem> = (0..count).filter_map(|i| model.row_data(i)).collect();
                    items.push(new_item);
                    ui.set_liked_songs(ModelRc::new(VecModel::from(items)));
                } else {
                    // Remove the unliked song, preserving all other items' thumbnails
                    let count = model.row_count();
                    let items: Vec<SongItem> = (0..count)
                        .filter_map(|i| model.row_data(i))
                        .filter(|item| item.video_id.as_str() != video_id.as_str())
                        .collect();
                    ui.set_liked_songs(ModelRc::new(VecModel::from(items)));
                }
            }
        });
    }

    // ── Unlike a specific song ────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_unlike_song(move |video_id| {
            playback.unlike(video_id.as_str());
            if let Some(ui) = ui_weak.upgrade() {
                let model = ui.get_liked_songs();
                let count = model.row_count();
                let items: Vec<SongItem> = (0..count)
                    .filter_map(|i| model.row_data(i))
                    .filter(|item| item.video_id.as_str() != video_id.as_str())
                    .collect();
                ui.set_liked_songs(ModelRc::new(VecModel::from(items)));
                // Update is-liked if it's the current track
                if playback.now_playing().video_id == video_id.as_str() {
                    ui.set_is_liked(false);
                }
            }
        });
    }

    // ── Dislike (remove from taste profile) ───────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_dislike_song(move |video_id, title, artist, duration_secs| {
            playback.dislike(video_id.as_str(), title.as_str(), artist.as_str(), duration_secs as u32);
            if let Some(ui) = ui_weak.upgrade() {
                // Refresh liked songs in case it was removed
                let liked = playback.get_liked_songs();
                let items: Vec<SongItem> = liked.iter().map(|s| {
                    let avatar = s.artist.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or("?".into());
                    SongItem {
                        video_id: SharedString::from(s.video_id.as_str()),
                        title: SharedString::from(s.title.as_str()),
                        artist: SharedString::from(s.artist.as_str()),
                        album: SharedString::default(),
                        duration_str: SharedString::from(format!("{}:{:02}", s.duration_secs / 60, s.duration_secs % 60)),
                        avatar_letter: SharedString::from(avatar),
                        duration_secs: s.duration_secs as i32,
                        thumbnail: slint::Image::default(),
                        has_thumbnail: false,
                    }
                }).collect();
                ui.set_liked_songs(ModelRc::new(VecModel::from(items)));
                let current_vid = playback.now_playing().video_id;
                ui.set_is_liked(playback.is_liked(&current_vid));
            }
        });
    }

    // ── Clear Cache ───────────────────────────────────────────────────────────
    {
        ui.on_clear_cache(move || {
            // Clear thumbnail cache from temp
            let temp = std::env::temp_dir();
            if let Ok(entries) = std::fs::read_dir(&temp) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with("ytm_thumb_") || name.starts_with("ytm_stream_") {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
            // Clear audio cache at %LOCALAPPDATA%\ytm-native\cache\
            let cache = core::cache::AudioCache::global().lock().unwrap();
            let cache_path = cache.cache_dir().to_path_buf();
            drop(cache);
            if cache_path.exists() {
                let _ = std::fs::remove_dir_all(&cache_path);
                let _ = std::fs::create_dir_all(&cache_path);
            }
            // Re-init the singleton with empty index
            if let Ok(mut c) = core::cache::AudioCache::global().lock() {
                *c = core::cache::AudioCache::open(core::cache::DEFAULT_CACHE_LIMIT_BYTES);
            }
        });
    }

    // ── Minimize to tray setting ──────────────────────────────────────────────
    {
        let settings = core::persistence::load_settings();
        ui.set_minimize_to_tray(settings.minimize_to_tray);
    }
    {
        ui.on_set_minimize_to_tray(move |val| {
            let mut settings = core::persistence::load_settings();
            settings.minimize_to_tray = val;
            core::persistence::save_settings(&settings);
        });
    }

    // ── Search ────────────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_do_search(move |query| {
            let query = query.to_string();
            if query.trim().is_empty() {
                return;
            }

            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_loading(true);
                ui.set_current_view(SharedString::from("Search"));
            }

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let results = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::query::{SearchQuery, search::SongsFilter, search::ArtistsFilter, search::AlbumsFilter};

                    let api = YtMusic::new_unauthenticated().await.map_err(|e| e.to_string())?;
                    let songs = api.query(SearchQuery::new(query.clone()).with_filter(SongsFilter))
                        .await.map_err(|e| e.to_string())?;
                    let artists = api.query(SearchQuery::new(query.clone()).with_filter(ArtistsFilter))
                        .await.unwrap_or_default();
                    let albums = api.query(SearchQuery::new(query.clone()).with_filter(AlbumsFilter))
                        .await.unwrap_or_default();
                    Ok::<_, String>((songs, artists, albums))
                });

                // Collect video_ids for thumbnail fetching
                let video_ids_for_thumbs: Vec<String> = match &results {
                    Ok((songs, _, _)) => songs.iter().map(|s| s.video_id.get_raw().to_string()).collect(),
                    Err(_) => vec![],
                };

                let ui_weak3 = ui_weak2.clone();
                let ui_weak_for_song_thumbs = ui_weak3.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_is_loading(false);
                        match results {
                            Ok((songs, artists, albums)) => {
                                let items: Vec<SongItem> = songs.into_iter().map(|s| {
                                    let avatar = s.artist.chars().next()
                                        .map(|c| c.to_uppercase().to_string())
                                        .unwrap_or_else(|| "?".to_string());
                                    let dur_secs: u32 = s.duration.as_str().split(':')
                                        .collect::<Vec<_>>()
                                        .iter()
                                        .fold(0u32, |acc, p| acc * 60 + p.parse::<u32>().unwrap_or(0));
                                    // Check if thumbnail already cached in temp
                                    let vid_raw = s.video_id.get_raw().to_string();
                                    let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid_raw));
                                    let (thumbnail, has_thumbnail) = if thumb_path.exists() {
                                        match slint::Image::load_from_path(&thumb_path) {
                                            Ok(img) => (img, true),
                                            Err(_) => (Default::default(), false),
                                        }
                                    } else {
                                        (Default::default(), false)
                                    };
                                    SongItem {
                                        video_id: SharedString::from(s.video_id.get_raw()),
                                        title: SharedString::from(s.title.as_str()),
                                        artist: SharedString::from(s.artist.as_str()),
                                        album: SharedString::from(
                                            s.album.as_ref().map(|a| a.name.as_str()).unwrap_or(""),
                                        ),
                                        duration_str: SharedString::from(s.duration.as_str()),
                                        avatar_letter: SharedString::from(avatar.as_str()),
                                        duration_secs: dur_secs as i32,
                                        thumbnail,
                                        has_thumbnail,
                                    }
                                }).collect();
                                ui.set_search_results(ModelRc::new(VecModel::from(items)));

                                // Artist results
                                let mut artist_thumb_urls: Vec<(usize, String)> = vec![];
                                let artist_items: Vec<ArtistItem> = artists.into_iter().take(10).enumerate().map(|(i, a)| {
                                    if let Some(t) = a.thumbnails.last() {
                                        artist_thumb_urls.push((i, t.url.clone()));
                                    }
                                    ArtistItem {
                                        name: SharedString::from(a.artist.as_str()),
                                        browse_id: SharedString::from(a.browse_id.get_raw()),
                                        thumbnail: Default::default(),
                                        has_thumbnail: false,
                                        subscriber_count: SharedString::from(""),
                                    }
                                }).collect();
                                ui.set_search_artists(ModelRc::new(VecModel::from(artist_items)));

                                // Album results
                                let mut album_thumb_urls: Vec<(usize, String)> = vec![];
                                let album_items: Vec<AlbumItem> = albums.into_iter().take(10).enumerate().map(|(i, a)| {
                                    if let Some(t) = a.thumbnails.last() {
                                        album_thumb_urls.push((i, t.url.clone()));
                                    }
                                    AlbumItem {
                                        title: SharedString::from(a.title.as_str()),
                                        browse_id: SharedString::from(a.album_id.get_raw()),
                                        artist: SharedString::from(a.artist.as_str()),
                                        year: SharedString::from(a.year.as_str()),
                                        thumbnail: Default::default(),
                                        has_thumbnail: false,
                                    }
                                }).collect();
                                ui.set_search_albums(ModelRc::new(VecModel::from(album_items)));

                                // Fetch artist thumbnails in background
                                if !artist_thumb_urls.is_empty() {
                                    let ui_w = ui_weak3.clone();
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            for (idx, url) in &artist_thumb_urls {
                                                let path = std::env::temp_dir().join(format!("ytm_artist_search_{}.jpg", idx));
                                                if let Ok(resp) = client.get(url).send() {
                                                    if resp.status().is_success() {
                                                        if let Ok(bytes) = resp.bytes() {
                                                            let _ = std::fs::write(&path, &bytes);
                                                        }
                                                    }
                                                }
                                            }
                                            let indices: Vec<usize> = artist_thumb_urls.iter().map(|(i, _)| *i).collect();
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_w.upgrade() {
                                                    let model = ui.get_search_artists();
                                                    let mut items: Vec<ArtistItem> = (0..model.row_count()).map(|i| model.row_data(i).unwrap()).collect();
                                                    for idx in indices {
                                                        let path = std::env::temp_dir().join(format!("ytm_artist_search_{}.jpg", idx));
                                                        if path.exists() {
                                                            if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                items[idx].thumbnail = img;
                                                                items[idx].has_thumbnail = true;
                                                            }
                                                        }
                                                    }
                                                    ui.set_search_artists(ModelRc::new(VecModel::from(items)));
                                                }
                                            }).ok();
                                        }
                                    });
                                }

                                // Fetch album thumbnails in background
                                if !album_thumb_urls.is_empty() {
                                    let ui_w = ui_weak3.clone();
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            for (idx, url) in &album_thumb_urls {
                                                let path = std::env::temp_dir().join(format!("ytm_album_search_{}.jpg", idx));
                                                if let Ok(resp) = client.get(url).send() {
                                                    if resp.status().is_success() {
                                                        if let Ok(bytes) = resp.bytes() {
                                                            let _ = std::fs::write(&path, &bytes);
                                                        }
                                                    }
                                                }
                                            }
                                            let indices: Vec<usize> = album_thumb_urls.iter().map(|(i, _)| *i).collect();
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_w.upgrade() {
                                                    let model = ui.get_search_albums();
                                                    let mut items: Vec<AlbumItem> = (0..model.row_count()).map(|i| model.row_data(i).unwrap()).collect();
                                                    for idx in indices {
                                                        let path = std::env::temp_dir().join(format!("ytm_album_search_{}.jpg", idx));
                                                        if path.exists() {
                                                            if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                items[idx].thumbnail = img;
                                                                items[idx].has_thumbnail = true;
                                                            }
                                                        }
                                                    }
                                                    ui.set_search_albums(ModelRc::new(VecModel::from(items)));
                                                }
                                            }).ok();
                                        }
                                    });
                                }
                            }
                            Err(e) => {
                                log::error!("Search failed: {e}");
                            }
                        }
                    }
                }).ok();

                // Spawn background thumbnail fetch for search results
                if !video_ids_for_thumbs.is_empty() {
                    let ui_weak_thumb = ui_weak_for_song_thumbs;
                    std::thread::spawn(move || {
                        let client = reqwest::blocking::Client::builder()
                            .timeout(std::time::Duration::from_secs(5))
                            .build()
                            .ok();
                        if let Some(client) = client {
                            for vid in &video_ids_for_thumbs {
                                let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                                if thumb_path.exists() { continue; }
                                let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                                if let Ok(resp) = client.get(&url).send() {
                                    if resp.status().is_success() {
                                        if let Ok(bytes) = resp.bytes() {
                                            let _ = std::fs::write(&thumb_path, &bytes);
                                        }
                                    }
                                }
                            }
                            // After all thumbnails fetched, update the UI model
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak_thumb.upgrade() {
                                    let model = ui.get_search_results();
                                    let count = model.row_count();
                                    let mut new_items: Vec<SongItem> = Vec::with_capacity(count);
                                    for i in 0..count {
                                        let mut item = model.row_data(i).unwrap();
                                        if !item.has_thumbnail {
                                            let tp = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", item.video_id.as_str()));
                                            if tp.exists() {
                                                if let Ok(img) = slint::Image::load_from_path(&tp) {
                                                    item.thumbnail = img;
                                                    item.has_thumbnail = true;
                                                }
                                            }
                                        }
                                        new_items.push(item);
                                    }
                                    ui.set_search_results(ModelRc::new(VecModel::from(new_items)));
                                }
                            }).ok();
                        }
                    });
                }
            });
        });
    }

    // ── Navigate to Artist ─────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        let nav_restoring = nav_restoring.clone();
        ui.on_navigate_to_artist(move |browse_id| {
            let browse_id = browse_id.to_string();
            if browse_id.trim().is_empty() { return; }

            // Push to nav history unless restoring from back/forward
            if !nav_restoring.load(std::sync::atomic::Ordering::Relaxed) {
                push_nav_entry(&nav_history, &nav_cursor, "Artist".to_string(), browse_id.clone());
                if let Some(ui) = ui_weak.upgrade() {
                    let hist = nav_history.lock().unwrap();
                    let cur = *nav_cursor.lock().unwrap();
                    update_nav_buttons(&ui, &hist, cur);
                }
            }

            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_loading(true);
                ui.set_current_view(SharedString::from("Artist"));
                ui.set_artist_view_singles(ModelRc::new(VecModel::from(Vec::<AlbumItem>::new())));
                ui.set_artist_view_related(ModelRc::new(VecModel::from(Vec::<ArtistItem>::new())));
                ui.set_artist_view_videos(ModelRc::new(VecModel::from(Vec::<SongItem>::new())));
                ui.set_artist_view_has_latest(false);
            }

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let result = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::common::ArtistChannelID;

                    let api = YtMusic::new_unauthenticated().await.map_err(|e| e.to_string())?;
                    api.get_artist(ArtistChannelID::from_raw(&browse_id)).await.map_err(|e| e.to_string())
                });

                let ui_weak3 = ui_weak2.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_is_loading(false);
                        match result {
                            Ok(artist) => {
                                ui.set_artist_view_name(SharedString::from(artist.name.as_str()));
                                ui.set_artist_view_has_thumbnail(false);
                                ui.set_artist_view_subscribed(artist.subscribed);
                                ui.set_artist_view_description(SharedString::from(
                                    artist.description.as_deref().unwrap_or("")
                                ));

                                // Convert top songs
                                let song_vids: Vec<String> = artist.top_releases.songs.iter()
                                    .flat_map(|section| section.results.iter())
                                    .map(|s| s.video_id.get_raw().to_string())
                                    .collect();
                                let songs: Vec<SongItem> = artist.top_releases.songs.iter()
                                    .flat_map(|section| section.results.iter())
                                    .map(|s| {
                                        let avatar = s.title.chars().next()
                                            .map(|c| c.to_uppercase().to_string())
                                            .unwrap_or_else(|| "?".to_string());
                                        let vid = s.video_id.get_raw();
                                        let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                                        let (thumbnail, has_thumbnail) = if thumb_path.exists() {
                                            match slint::Image::load_from_path(&thumb_path) {
                                                Ok(img) => (img, true),
                                                Err(_) => (slint::Image::default(), false),
                                            }
                                        } else { (slint::Image::default(), false) };
                                        SongItem {
                                            video_id: SharedString::from(vid),
                                            title: SharedString::from(s.title.as_str()),
                                            artist: SharedString::from(artist.name.as_str()),
                                            album: SharedString::from(s.album.name.as_str()),
                                            duration_str: SharedString::from(""),
                                            avatar_letter: SharedString::from(avatar.as_str()),
                                            duration_secs: 0,
                                            thumbnail,
                                            has_thumbnail,
                                        }
                                    }).collect();
                                ui.set_artist_view_songs(ModelRc::new(VecModel::from(songs)));

                                // Fetch missing artist song thumbnails in background
                                let need_thumbs: Vec<(usize, String)> = song_vids.iter().enumerate()
                                    .filter(|(_, vid)| {
                                        !vid.is_empty() && !std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid)).exists()
                                    })
                                    .map(|(i, vid)| (i, vid.clone()))
                                    .collect();
                                if !need_thumbs.is_empty() {
                                    let ui_w_songs = ui_weak3.clone();
                                    std::thread::spawn(move || {
                                        let http = match reqwest::blocking::Client::builder()
                                            .timeout(std::time::Duration::from_secs(8)).build() {
                                            Ok(c) => c, Err(_) => return,
                                        };
                                        for (idx, vid) in &need_thumbs {
                                            let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                                            let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                                            if let Ok(resp) = http.get(&url).send() {
                                                if let Ok(bytes) = resp.bytes() {
                                                    if let Ok(img) = image::load_from_memory(&bytes) {
                                                        let rgba = img.to_rgba8();
                                                        let (w, h) = (rgba.width(), rgba.height());
                                                        let _ = std::fs::write(&thumb_path, bytes.as_ref());
                                                        let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                                        let ui_w2 = ui_w_songs.clone();
                                                        let idx_copy = *idx;
                                                        slint::invoke_from_event_loop(move || {
                                                            if let Some(ui) = ui_w2.upgrade() {
                                                                let slint_img = slint::Image::from_rgba8(buf);
                                                                let model = ui.get_artist_view_songs();
                                                                if let Some(row) = model.row_data(idx_copy) {
                                                                    let mut updated = row;
                                                                    updated.thumbnail = slint_img;
                                                                    updated.has_thumbnail = true;
                                                                    model.set_row_data(idx_copy, updated);
                                                                }
                                                            }
                                                        }).ok();
                                                    }
                                                }
                                            }
                                        }
                                    });
                                }

                                // Convert albums
                                let mut album_thumb_urls: Vec<(usize, String)> = vec![];
                                let albums: Vec<AlbumItem> = artist.top_releases.albums.iter()
                                    .flat_map(|section| section.results.iter())
                                    .enumerate()
                                    .map(|(i, a)| {
                                        if let Some(t) = a.thumbnails.last() {
                                            album_thumb_urls.push((i, t.url.clone()));
                                        }
                                        AlbumItem {
                                            title: SharedString::from(a.title.as_str()),
                                            browse_id: SharedString::from(a.album_id.get_raw()),
                                            artist: SharedString::from(artist.name.as_str()),
                                            year: SharedString::from(a.year.as_str()),
                                            thumbnail: Default::default(),
                                            has_thumbnail: false,
                                        }
                                    }).collect();
                                ui.set_artist_view_albums(ModelRc::new(VecModel::from(albums)));

                                // Convert singles
                                let mut single_thumb_urls: Vec<(usize, String)> = vec![];
                                let singles: Vec<AlbumItem> = artist.top_releases.singles.iter()
                                    .flat_map(|section| section.results.iter())
                                    .enumerate()
                                    .map(|(i, a)| {
                                        if let Some(t) = a.thumbnails.last() {
                                            single_thumb_urls.push((i, t.url.clone()));
                                        }
                                        AlbumItem {
                                            title: SharedString::from(a.title.as_str()),
                                            browse_id: SharedString::from(a.album_id.get_raw()),
                                            artist: SharedString::from(artist.name.as_str()),
                                            year: SharedString::from(a.year.as_str()),
                                            thumbnail: Default::default(),
                                            has_thumbnail: false,
                                        }
                                    }).collect();
                                ui.set_artist_view_singles(ModelRc::new(VecModel::from(singles)));

                                // Convert videos
                                let mut video_thumb_urls: Vec<(usize, String)> = vec![];
                                let videos: Vec<SongItem> = artist.top_releases.videos.iter()
                                    .flat_map(|section| section.results.iter())
                                    .filter_map(|v| {
                                        use ytmapi_rs::parse::SearchResultVideo;
                                        match v {
                                            SearchResultVideo::Video { title, video_id, views, thumbnails, channel_name, .. } => {
                                                Some((title.clone(), video_id.get_raw().to_string(), views.clone(), thumbnails.last().map(|t| t.url.clone()), channel_name.clone()))
                                            }
                                            _ => None,
                                        }
                                    })
                                    .enumerate()
                                    .map(|(i, (title, vid, views, thumb_url, _channel))| {
                                        if let Some(url) = thumb_url {
                                            video_thumb_urls.push((i, url));
                                        }
                                        let avatar = title.chars().next()
                                            .map(|c| c.to_uppercase().to_string())
                                            .unwrap_or_else(|| "▶".to_string());
                                        SongItem {
                                            video_id: SharedString::from(vid.as_str()),
                                            title: SharedString::from(title.as_str()),
                                            artist: SharedString::from(artist.name.as_str()),
                                            album: SharedString::from(""),
                                            duration_str: SharedString::from(views.as_str()),
                                            avatar_letter: SharedString::from(avatar.as_str()),
                                            duration_secs: 0,
                                            thumbnail: Default::default(),
                                            has_thumbnail: false,
                                        }
                                    }).collect();
                                ui.set_artist_view_videos(ModelRc::new(VecModel::from(videos)));

                                // Convert related artists (RelatedResult has no thumbnails)
                                let related_thumb_urls: Vec<(usize, String)> = vec![];
                                let related: Vec<ArtistItem> = artist.top_releases.related.iter()
                                    .flat_map(|section| section.results.iter())
                                    .enumerate()
                                    .map(|(_i, r)| {
                                        ArtistItem {
                                            name: SharedString::from(r.title.as_str()),
                                            browse_id: SharedString::from(r.browse_id.get_raw()),
                                            thumbnail: Default::default(),
                                            has_thumbnail: false,
                                            subscriber_count: SharedString::from(r.subscribers.as_str()),
                                        }
                                    }).collect();
                                ui.set_artist_view_related(ModelRc::new(VecModel::from(related)));

                                // Determine latest release (most recent year from albums + singles)
                                let mut latest: Option<AlbumItem> = None;
                                let mut latest_year: String = String::new();
                                let all_releases = artist.top_releases.albums.iter()
                                    .flat_map(|s| s.results.iter())
                                    .chain(artist.top_releases.singles.iter().flat_map(|s| s.results.iter()));
                                for a in all_releases {
                                    if a.year > latest_year {
                                        latest_year = a.year.clone();
                                        latest = Some(AlbumItem {
                                            title: SharedString::from(a.title.as_str()),
                                            browse_id: SharedString::from(a.album_id.get_raw()),
                                            artist: SharedString::from(artist.name.as_str()),
                                            year: SharedString::from(a.year.as_str()),
                                            thumbnail: Default::default(),
                                            has_thumbnail: false,
                                        });
                                    }
                                }
                                let latest_thumb_url: Option<String> = if latest.is_some() {
                                    // Find matching thumbnail URL
                                    let all_with_thumbs = artist.top_releases.albums.iter()
                                        .flat_map(|s| s.results.iter())
                                        .chain(artist.top_releases.singles.iter().flat_map(|s| s.results.iter()));
                                    all_with_thumbs
                                        .filter(|a| a.year == latest_year)
                                        .find_map(|a| a.thumbnails.last().map(|t| t.url.clone()))
                                } else { None };
                                if let Some(l) = latest {
                                    ui.set_artist_view_latest_release(l);
                                    ui.set_artist_view_has_latest(true);
                                } else {
                                    ui.set_artist_view_has_latest(false);
                                }

                                // Fetch artist thumbnail
                                let ui_w_albums = ui_weak3.clone();
                                let ui_w_singles = ui_weak3.clone();
                                let ui_w_videos = ui_weak3.clone();
                                let ui_w_related = ui_weak3.clone();
                                let ui_w_latest = ui_weak3.clone();
                                if let Some(thumb) = artist.thumbnails.last() {
                                    let thumb_url = thumb.url.clone();
                                    let ui_w = ui_weak3;
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            if let Ok(resp) = client.get(&thumb_url).send() {
                                                if resp.status().is_success() {
                                                    if let Ok(bytes) = resp.bytes() {
                                                        let path = std::env::temp_dir().join("ytm_artist_thumb.jpg");
                                                        let _ = std::fs::write(&path, &bytes);
                                                        slint::invoke_from_event_loop(move || {
                                                            if let Some(ui) = ui_w.upgrade() {
                                                                if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                    ui.set_artist_view_thumbnail(img);
                                                                    ui.set_artist_view_has_thumbnail(true);
                                                                }
                                                            }
                                                        }).ok();
                                                    }
                                                }
                                            }
                                        }
                                    });
                                }

                                // Fetch album thumbnails in artist view
                                if !album_thumb_urls.is_empty() {
                                    let ui_w = ui_w_albums;
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            for (idx, url) in &album_thumb_urls {
                                                let path = std::env::temp_dir().join(format!("ytm_artist_album_{}.jpg", idx));
                                                if let Ok(resp) = client.get(url).send() {
                                                    if resp.status().is_success() {
                                                        if let Ok(bytes) = resp.bytes() {
                                                            let _ = std::fs::write(&path, &bytes);
                                                        }
                                                    }
                                                }
                                            }
                                            let indices: Vec<usize> = album_thumb_urls.iter().map(|(i, _)| *i).collect();
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_w.upgrade() {
                                                    let model = ui.get_artist_view_albums();
                                                    let mut items: Vec<AlbumItem> = (0..model.row_count()).map(|i| model.row_data(i).unwrap()).collect();
                                                    for idx in indices {
                                                        let path = std::env::temp_dir().join(format!("ytm_artist_album_{}.jpg", idx));
                                                        if path.exists() {
                                                            if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                items[idx].thumbnail = img;
                                                                items[idx].has_thumbnail = true;
                                                            }
                                                        }
                                                    }
                                                    ui.set_artist_view_albums(ModelRc::new(VecModel::from(items)));
                                                }
                                            }).ok();
                                        }
                                    });
                                }

                                // Fetch single thumbnails
                                if !single_thumb_urls.is_empty() {
                                    let ui_w = ui_w_singles;
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            for (idx, url) in &single_thumb_urls {
                                                let path = std::env::temp_dir().join(format!("ytm_artist_single_{}.jpg", idx));
                                                if let Ok(resp) = client.get(url).send() {
                                                    if resp.status().is_success() {
                                                        if let Ok(bytes) = resp.bytes() {
                                                            let _ = std::fs::write(&path, &bytes);
                                                        }
                                                    }
                                                }
                                            }
                                            let indices: Vec<usize> = single_thumb_urls.iter().map(|(i, _)| *i).collect();
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_w.upgrade() {
                                                    let model = ui.get_artist_view_singles();
                                                    let mut items: Vec<AlbumItem> = (0..model.row_count()).map(|i| model.row_data(i).unwrap()).collect();
                                                    for idx in indices {
                                                        let path = std::env::temp_dir().join(format!("ytm_artist_single_{}.jpg", idx));
                                                        if path.exists() {
                                                            if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                items[idx].thumbnail = img;
                                                                items[idx].has_thumbnail = true;
                                                            }
                                                        }
                                                    }
                                                    ui.set_artist_view_singles(ModelRc::new(VecModel::from(items)));
                                                }
                                            }).ok();
                                        }
                                    });
                                }

                                // Fetch video thumbnails
                                if !video_thumb_urls.is_empty() {
                                    let ui_w = ui_w_videos;
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            for (idx, url) in &video_thumb_urls {
                                                let path = std::env::temp_dir().join(format!("ytm_artist_video_{}.jpg", idx));
                                                if let Ok(resp) = client.get(url).send() {
                                                    if resp.status().is_success() {
                                                        if let Ok(bytes) = resp.bytes() {
                                                            let _ = std::fs::write(&path, &bytes);
                                                        }
                                                    }
                                                }
                                            }
                                            let indices: Vec<usize> = video_thumb_urls.iter().map(|(i, _)| *i).collect();
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_w.upgrade() {
                                                    let model = ui.get_artist_view_videos();
                                                    let mut items: Vec<SongItem> = (0..model.row_count()).map(|i| model.row_data(i).unwrap()).collect();
                                                    for idx in indices {
                                                        let path = std::env::temp_dir().join(format!("ytm_artist_video_{}.jpg", idx));
                                                        if path.exists() {
                                                            if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                items[idx].thumbnail = img;
                                                                items[idx].has_thumbnail = true;
                                                            }
                                                        }
                                                    }
                                                    ui.set_artist_view_videos(ModelRc::new(VecModel::from(items)));
                                                }
                                            }).ok();
                                        }
                                    });
                                }

                                // Fetch related artist thumbnails
                                if !related_thumb_urls.is_empty() {
                                    let ui_w = ui_w_related;
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            for (idx, url) in &related_thumb_urls {
                                                let path = std::env::temp_dir().join(format!("ytm_artist_related_{}.jpg", idx));
                                                if let Ok(resp) = client.get(url).send() {
                                                    if resp.status().is_success() {
                                                        if let Ok(bytes) = resp.bytes() {
                                                            let _ = std::fs::write(&path, &bytes);
                                                        }
                                                    }
                                                }
                                            }
                                            let indices: Vec<usize> = related_thumb_urls.iter().map(|(i, _)| *i).collect();
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_w.upgrade() {
                                                    let model = ui.get_artist_view_related();
                                                    let mut items: Vec<ArtistItem> = (0..model.row_count()).map(|i| model.row_data(i).unwrap()).collect();
                                                    for idx in indices {
                                                        let path = std::env::temp_dir().join(format!("ytm_artist_related_{}.jpg", idx));
                                                        if path.exists() {
                                                            if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                items[idx].thumbnail = img;
                                                                items[idx].has_thumbnail = true;
                                                            }
                                                        }
                                                    }
                                                    ui.set_artist_view_related(ModelRc::new(VecModel::from(items)));
                                                }
                                            }).ok();
                                        }
                                    });
                                }

                                // Fetch latest release thumbnail
                                if let Some(latest_url) = latest_thumb_url {
                                    let ui_w = ui_w_latest;
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            let path = std::env::temp_dir().join("ytm_artist_latest.jpg");
                                            if let Ok(resp) = client.get(&latest_url).send() {
                                                if resp.status().is_success() {
                                                    if let Ok(bytes) = resp.bytes() {
                                                        let _ = std::fs::write(&path, &bytes);
                                                        slint::invoke_from_event_loop(move || {
                                                            if let Some(ui) = ui_w.upgrade() {
                                                                if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                    let mut item = ui.get_artist_view_latest_release();
                                                                    item.thumbnail = img;
                                                                    item.has_thumbnail = true;
                                                                    ui.set_artist_view_latest_release(item);
                                                                }
                                                            }
                                                        }).ok();
                                                    }
                                                }
                                            }
                                        }
                                    });
                                }
                            }
                            Err(e) => { log::error!("Get artist failed: {e}"); }
                        }
                    }
                }).ok();
            });
        });
    }

    // ── Navigate to Album ──────────────────────────────────────────────────────
    // ── Start Song Radio ───────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_start_song_radio(move |video_id| {
            let video_id = video_id.to_string();
            let ui_w = ui_weak.clone();
            // Show loading and navigate to Radio page
            if let Some(ui) = ui_w.upgrade() {
                ui.set_is_loading(true);
                ui.set_radio_title(SharedString::from(format!("Song Radio").as_str()));
                ui.set_radio_songs(ModelRc::new(VecModel::from(Vec::<SongItem>::new())));
                ui.set_current_view(SharedString::from("Radio"));
            }
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let result = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::common::VideoID;
                    let api = YtMusic::new_unauthenticated().await.ok()?;
                    let vid = VideoID::from_raw(&video_id);
                    api.get_watch_playlist_from_video_id(vid).await.ok()
                });
                let ui_w2 = ui_w.clone();
                if let Some(tracks) = result {
                    let raw: Vec<(String, String, String, u32)> = tracks.into_iter().map(|t| {
                        let dur = parse_duration(&t.duration).unwrap_or(0);
                        (t.video_id.get_raw().to_string(), t.title, t.author, dur)
                    }).collect();

                    // Collect missing thumbnails before raw is moved into closure
                    let need_thumbs: Vec<(usize, String)> = raw.iter().enumerate()
                        .filter(|(_, (vid, _, _, _))| {
                            !vid.is_empty() && !std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid)).exists()
                        })
                        .map(|(i, (vid, _, _, _))| (i, vid.clone()))
                        .collect();

                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            let items: Vec<SongItem> = raw.iter().map(|(vid, title, artist, dur)| {
                                let avatar = title.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".to_string());
                                let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                                let (thumbnail, has_thumbnail) = if thumb_path.exists() {
                                    match slint::Image::load_from_path(&thumb_path) {
                                        Ok(img) => (img, true),
                                        Err(_) => (slint::Image::default(), false),
                                    }
                                } else { (slint::Image::default(), false) };
                                SongItem {
                                    video_id: SharedString::from(vid.as_str()),
                                    title: SharedString::from(title.as_str()),
                                    artist: SharedString::from(artist.as_str()),
                                    album: SharedString::default(),
                                    duration_str: SharedString::from(format!("{}:{:02}", dur / 60, dur % 60).as_str()),
                                    avatar_letter: SharedString::from(avatar.as_str()),
                                    duration_secs: *dur as i32,
                                    thumbnail,
                                    has_thumbnail,
                                }
                            }).collect();
                            ui.set_radio_songs(ModelRc::new(VecModel::from(items)));
                            ui.set_is_loading(false);
                        }
                    }).ok();

                    // Fetch missing thumbnails
                    if !need_thumbs.is_empty() {
                        std::thread::spawn(move || {
                            let http = match reqwest::blocking::Client::builder()
                                .timeout(std::time::Duration::from_secs(8)).build() {
                                Ok(c) => c, Err(_) => return,
                            };
                            for (idx, vid) in &need_thumbs {
                                let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                                let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                                if let Ok(resp) = http.get(&url).send() {
                                    if let Ok(bytes) = resp.bytes() {
                                        if let Ok(img) = image::load_from_memory(&bytes) {
                                            let rgba = img.to_rgba8();
                                            let (w, h) = (rgba.width(), rgba.height());
                                            let _ = std::fs::write(&thumb_path, bytes.as_ref());
                                            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                            let ui_w3 = ui_w2.clone();
                                            let idx_copy = *idx;
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_w3.upgrade() {
                                                    let slint_img = slint::Image::from_rgba8(buf);
                                                    let model = ui.get_radio_songs();
                                                    if let Some(row) = model.row_data(idx_copy) {
                                                        let mut updated = row;
                                                        updated.thumbnail = slint_img;
                                                        updated.has_thumbnail = true;
                                                        model.set_row_data(idx_copy, updated);
                                                    }
                                                }
                                            }).ok();
                                        }
                                    }
                                }
                            }
                        });
                    }
                } else {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_is_loading(false);
                        }
                    }).ok();
                }
            });
        });
    }

    // ── Play Album (play all songs from the Album page) ──────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_play_album(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let model = ui.get_album_view_songs();
                let songs: Vec<core::playback::NowPlaying> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .map(|item| core::playback::NowPlaying {
                        video_id: item.video_id.to_string(),
                        title: item.title.to_string(),
                        artist: item.artist.to_string(),
                        duration_secs: item.duration_secs as u32,
                    })
                    .collect();
                if !songs.is_empty() {
                    let playback = crate::core::bridge::playback_core();
                    playback.set_queue(songs);
                    {
                        let mut state = playback.state_lock();
                        state.is_playing = true;
                    }
                    let first = playback.now_playing();
                    playback.set_now_playing(&first.video_id, &first.title, &first.artist, first.duration_secs);
                    refresh_native_shell_ui(&ui, playback);
                }
            }
        });
    }

    // ── Toggle Like Album ────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_like_album(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let currently_liked = ui.get_album_view_liked();
                ui.set_album_view_liked(!currently_liked);
                // Note: actual YouTube Music API rate_playlist/add_to_library calls
                // require authentication; for now we toggle the local UI state.
                // When auth is implemented, wire: api.rate_playlist(playlist_id, if liked INDIFFERENT else LIKE)
            }
        });
    }

    // ── Toggle Subscribe Artist ──────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_subscribe_artist(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let currently_subscribed = ui.get_artist_view_subscribed();
                ui.set_artist_view_subscribed(!currently_subscribed);
                // Note: actual YouTube Music subscribe/unsubscribe calls
                // require authentication; for now we toggle the local UI state.
            }
        });
    }

    // ── Toggle Like Playlist ─────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_like_playlist(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let currently_liked = ui.get_playlist_view_liked();
                ui.set_playlist_view_liked(!currently_liked);
                // Note: actual YouTube Music API calls require authentication;
                // for now we toggle the local UI state.
            }
        });
    }

    // ── Play Radio (play all songs from the Radio page) ──────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_play_radio(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let model = ui.get_radio_songs();
                let songs: Vec<core::playback::NowPlaying> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .map(|item| core::playback::NowPlaying {
                        video_id: item.video_id.to_string(),
                        title: item.title.to_string(),
                        artist: item.artist.to_string(),
                        duration_secs: item.duration_secs as u32,
                    })
                    .collect();
                if !songs.is_empty() {
                    let playback = crate::core::bridge::playback_core();
                    playback.set_queue(songs);
                    {
                        let mut state = playback.state_lock();
                        state.is_playing = true;
                    }
                    let first = playback.now_playing();
                    playback.set_now_playing(&first.video_id, &first.title, &first.artist, first.duration_secs);
                    refresh_native_shell_ui(&ui, playback);
                }
            }
        });
    }

    // ── Go to Song Artist ──────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_go_to_song_artist(move |artist_name| {
            let artist = artist_name.to_string();
            let ui_w = ui_weak.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let browse_id = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::query::{SearchQuery, search::ArtistsFilter};
                    use ytmapi_rs::common::YoutubeID;
                    let api = YtMusic::new_unauthenticated().await.ok()?;
                    let results = api.query(SearchQuery::new(artist).with_filter(ArtistsFilter)).await.ok()?;
                    results.into_iter().next().map(|a| a.browse_id.get_raw().to_string())
                });
                if let Some(id) = browse_id {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.invoke_navigate_to_artist(SharedString::from(id.as_str()));
                        }
                    }).ok();
                }
            });
        });
    }

    // ── Navigate to Album by name (from song context menu) ─────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_go_to_song_album(move |album_name| {
            let album = album_name.to_string();
            if album.trim().is_empty() { return; }
            let ui_w = ui_weak.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let browse_id = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::query::{SearchQuery, search::AlbumsFilter};
                    use ytmapi_rs::common::YoutubeID;
                    let api = YtMusic::new_unauthenticated().await.ok()?;
                    let results = api.query(SearchQuery::new(album).with_filter(AlbumsFilter)).await.ok()?;
                    results.into_iter().next().map(|a| a.album_id.get_raw().to_string())
                });
                if let Some(id) = browse_id {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.invoke_navigate_to_album(SharedString::from(id.as_str()));
                        }
                    }).ok();
                }
            });
        });
    }

    // ── Navigate to Album (continued) ──────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        let nav_restoring = nav_restoring.clone();
        ui.on_navigate_to_album(move |browse_id| {
            let browse_id = browse_id.to_string();
            if browse_id.trim().is_empty() { return; }

            if !nav_restoring.load(std::sync::atomic::Ordering::Relaxed) {
                push_nav_entry(&nav_history, &nav_cursor, "Album".to_string(), browse_id.clone());
                if let Some(ui) = ui_weak.upgrade() {
                    let hist = nav_history.lock().unwrap();
                    let cur = *nav_cursor.lock().unwrap();
                    update_nav_buttons(&ui, &hist, cur);
                }
            }

            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_loading(true);
                ui.set_current_view(SharedString::from("Album"));
            }

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let result = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::common::AlbumID;

                    let api = YtMusic::new_unauthenticated().await.map_err(|e| e.to_string())?;
                    api.get_album(AlbumID::from_raw(&browse_id)).await.map_err(|e| e.to_string())
                });

                let ui_weak3 = ui_weak2.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_is_loading(false);
                        match result {
                            Ok(album) => {
                                ui.set_album_view_title(SharedString::from(album.title.as_str()));
                                let artist_name = album.artists.first().map(|a| a.name.as_str()).unwrap_or("").to_string();
                                ui.set_album_view_artist(SharedString::from(artist_name.as_str()));
                                ui.set_album_view_year(SharedString::from(album.year.as_str()));
                                ui.set_album_view_has_thumbnail(false);
                                ui.set_album_view_liked(matches!(album.library_status, ytmapi_rs::common::LibraryStatus::InLibrary));
                                ui.set_album_view_duration(SharedString::from(album.duration.as_str()));
                                ui.set_album_view_description(SharedString::from(
                                    album.description.as_deref().unwrap_or("")
                                ));
                                // Clear previous recommendation sections
                                ui.set_album_view_more_by_artist(ModelRc::new(VecModel::from(Vec::<AlbumItem>::new())));
                                ui.set_album_view_similar(ModelRc::new(VecModel::from(Vec::<AlbumItem>::new())));

                                let tracks: Vec<SongItem> = album.tracks.iter().map(|t| {
                                    let avatar = t.title.chars().next()
                                        .map(|c| c.to_uppercase().to_string())
                                        .unwrap_or_else(|| "?".to_string());
                                    let dur_secs: i32 = t.duration.as_str().split(':')
                                        .collect::<Vec<_>>().iter()
                                        .fold(0i32, |acc, p| acc * 60 + p.parse::<i32>().unwrap_or(0));
                                    SongItem {
                                        video_id: SharedString::from(t.video_id.get_raw()),
                                        title: SharedString::from(t.title.as_str()),
                                        artist: SharedString::from(artist_name.as_str()),
                                        album: SharedString::from(album.title.as_str()),
                                        duration_str: SharedString::from(t.duration.as_str()),
                                        avatar_letter: SharedString::from(avatar.as_str()),
                                        duration_secs: dur_secs,
                                        thumbnail: Default::default(),
                                        has_thumbnail: false,
                                    }
                                }).collect();
                                ui.set_album_view_songs(ModelRc::new(VecModel::from(tracks)));

                                // Fetch album thumbnail
                                let ui_weak_recs = ui_weak3.clone();
                                if let Some(thumb) = album.thumbnails.last() {
                                    let thumb_url = thumb.url.clone();
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            if let Ok(resp) = client.get(&thumb_url).send() {
                                                if resp.status().is_success() {
                                                    if let Ok(bytes) = resp.bytes() {
                                                        let path = std::env::temp_dir().join("ytm_album_thumb.jpg");
                                                        let _ = std::fs::write(&path, &bytes);
                                                        slint::invoke_from_event_loop(move || {
                                                            if let Some(ui) = ui_weak3.upgrade() {
                                                                if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                    ui.set_album_view_thumbnail(img);
                                                                    ui.set_album_view_has_thumbnail(true);
                                                                }
                                                            }
                                                        }).ok();
                                                    }
                                                }
                                            }
                                        }
                                    });
                                }

                                // Fetch "More by this artist" and "You might also like" in background
                                let artist_id = album.artists.first().and_then(|a| a.id.as_ref()).map(|id| id.get_raw().to_string());
                                let album_title_for_search = album.title.clone();
                                let current_browse_id = browse_id.clone();
                                let artist_name_for_search = artist_name.clone();
                                std::thread::spawn(move || {
                                    let rt = tokio::runtime::Builder::new_current_thread()
                                        .enable_all()
                                        .build()
                                        .unwrap();
                                    rt.block_on(async {
                                        use ytmapi_rs::YtMusic;
                                        use ytmapi_rs::common::ArtistChannelID;
                                        use ytmapi_rs::query::{SearchQuery, search::AlbumsFilter};

                                        let api = match YtMusic::new_unauthenticated().await {
                                            Ok(a) => a,
                                            Err(_) => return,
                                        };

                                        // Collect raw data as simple Send-able tuples: (title, browse_id, artist, year, thumb_url)
                                        let mut more_raw: Vec<(String, String, String, String, Option<String>)> = vec![];
                                        if let Some(ref artist_browse_id) = artist_id {
                                            if let Ok(artist) = api.get_artist(ArtistChannelID::from_raw(artist_browse_id)).await {
                                                more_raw = artist.top_releases.albums.iter()
                                                    .flat_map(|section| section.results.iter())
                                                    .filter(|a| a.album_id.get_raw() != current_browse_id)
                                                    .take(10)
                                                    .map(|a| {
                                                        let thumb = a.thumbnails.last().map(|t| t.url.clone());
                                                        (a.title.clone(), a.album_id.get_raw().to_string(), artist.name.clone(), a.year.clone(), thumb)
                                                    }).collect();
                                            }
                                        }

                                        let mut similar_raw: Vec<(String, String, String, String, Option<String>)> = vec![];
                                        let search_query = format!("{} {}", artist_name_for_search, album_title_for_search);
                                        if let Ok(results) = api.query(SearchQuery::new(&search_query).with_filter(AlbumsFilter)).await {
                                            similar_raw = results.into_iter()
                                                .filter(|a| a.album_id.get_raw() != current_browse_id)
                                                .take(6)
                                                .map(|a| {
                                                    let thumb = a.thumbnails.last().map(|t| t.url.clone());
                                                    (a.title.clone(), a.album_id.get_raw().to_string(), a.artist.clone(), a.year.clone(), thumb)
                                                }).collect();
                                        }

                                        // Collect thumbnail URLs
                                        let more_thumb_urls: Vec<(usize, String)> = more_raw.iter().enumerate()
                                            .filter_map(|(i, (_, _, _, _, t))| t.as_ref().map(|url| (i, url.clone())))
                                            .collect();
                                        let similar_thumb_urls: Vec<(usize, String)> = similar_raw.iter().enumerate()
                                            .filter_map(|(i, (_, _, _, _, t))| t.as_ref().map(|url| (i, url.clone())))
                                            .collect();

                                        // Update UI with recommendation data (create AlbumItem inside event loop)
                                        let ui_weak_thumbs = ui_weak_recs.clone();
                                        slint::invoke_from_event_loop(move || {
                                            if let Some(ui) = ui_weak_recs.upgrade() {
                                                let more_items: Vec<AlbumItem> = more_raw.into_iter().map(|(title, bid, artist, year, _)| {
                                                    AlbumItem {
                                                        title: SharedString::from(title.as_str()),
                                                        browse_id: SharedString::from(bid.as_str()),
                                                        artist: SharedString::from(artist.as_str()),
                                                        year: SharedString::from(year.as_str()),
                                                        thumbnail: Default::default(),
                                                        has_thumbnail: false,
                                                    }
                                                }).collect();
                                                let similar_items: Vec<AlbumItem> = similar_raw.into_iter().map(|(title, bid, artist, year, _)| {
                                                    AlbumItem {
                                                        title: SharedString::from(title.as_str()),
                                                        browse_id: SharedString::from(bid.as_str()),
                                                        artist: SharedString::from(artist.as_str()),
                                                        year: SharedString::from(year.as_str()),
                                                        thumbnail: Default::default(),
                                                        has_thumbnail: false,
                                                    }
                                                }).collect();
                                                ui.set_album_view_more_by_artist(ModelRc::new(VecModel::from(more_items)));
                                                ui.set_album_view_similar(ModelRc::new(VecModel::from(similar_items)));
                                            }
                                        }).ok();

                                        // Fetch thumbnails for "More by this artist"
                                        if !more_thumb_urls.is_empty() {
                                            let ui_w = ui_weak_thumbs.clone();
                                            std::thread::spawn(move || {
                                                if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                                    for (idx, url) in &more_thumb_urls {
                                                        let path = std::env::temp_dir().join(format!("ytm_album_more_{}.jpg", idx));
                                                        if let Ok(resp) = client.get(url).send() {
                                                            if resp.status().is_success() {
                                                                if let Ok(bytes) = resp.bytes() {
                                                                    let _ = std::fs::write(&path, &bytes);
                                                                }
                                                            }
                                                        }
                                                    }
                                                    let indices: Vec<usize> = more_thumb_urls.iter().map(|(i, _)| *i).collect();
                                                    slint::invoke_from_event_loop(move || {
                                                        if let Some(ui) = ui_w.upgrade() {
                                                            let model = ui.get_album_view_more_by_artist();
                                                            let mut items: Vec<AlbumItem> = (0..model.row_count()).map(|i| model.row_data(i).unwrap()).collect();
                                                            for idx in indices {
                                                                if idx < items.len() {
                                                                    let path = std::env::temp_dir().join(format!("ytm_album_more_{}.jpg", idx));
                                                                    if path.exists() {
                                                                        if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                            items[idx].thumbnail = img;
                                                                            items[idx].has_thumbnail = true;
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            ui.set_album_view_more_by_artist(ModelRc::new(VecModel::from(items)));
                                                        }
                                                    }).ok();
                                                }
                                            });
                                        }

                                        // Fetch thumbnails for "You might also like"
                                        if !similar_thumb_urls.is_empty() {
                                            let ui_w = ui_weak_thumbs;
                                            std::thread::spawn(move || {
                                                if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                                    for (idx, url) in &similar_thumb_urls {
                                                        let path = std::env::temp_dir().join(format!("ytm_album_similar_{}.jpg", idx));
                                                        if let Ok(resp) = client.get(url).send() {
                                                            if resp.status().is_success() {
                                                                if let Ok(bytes) = resp.bytes() {
                                                                    let _ = std::fs::write(&path, &bytes);
                                                                }
                                                            }
                                                        }
                                                    }
                                                    let indices: Vec<usize> = similar_thumb_urls.iter().map(|(i, _)| *i).collect();
                                                    slint::invoke_from_event_loop(move || {
                                                        if let Some(ui) = ui_w.upgrade() {
                                                            let model = ui.get_album_view_similar();
                                                            let mut items: Vec<AlbumItem> = (0..model.row_count()).map(|i| model.row_data(i).unwrap()).collect();
                                                            for idx in indices {
                                                                if idx < items.len() {
                                                                    let path = std::env::temp_dir().join(format!("ytm_album_similar_{}.jpg", idx));
                                                                    if path.exists() {
                                                                        if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                            items[idx].thumbnail = img;
                                                                            items[idx].has_thumbnail = true;
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                            ui.set_album_view_similar(ModelRc::new(VecModel::from(items)));
                                                        }
                                                    }).ok();
                                                }
                                            });
                                        }
                                    });
                                });
                            }
                            Err(e) => { log::error!("Get album failed: {e}"); }
                        }
                    }
                }).ok();
            });
        });
    }

    // ── Live Search (suggestions as user types) ────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_live_search(move |query| {
            let query = query.to_string();
            if query.trim().is_empty() {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_search_suggestions(ModelRc::new(VecModel::from(Vec::<SongItem>::new())));
                }
                return;
            }

            let ui_weak2 = ui_weak.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let results = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::query::{SearchQuery, search::SongsFilter};

                    let api = YtMusic::new_unauthenticated().await.map_err(|e| e.to_string())?;
                    api.query(SearchQuery::new(query).with_filter(SongsFilter))
                        .await
                        .map_err(|e| e.to_string())
                });

                // Fetch thumbnails inline (parallel) before displaying
                if let Ok(ref songs) = results {
                    let client = reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(4))
                        .build()
                        .ok();
                    if let Some(client) = client {
                        use std::sync::Arc;
                        let client = Arc::new(client);
                        let handles: Vec<_> = songs.iter().take(6).filter_map(|s| {
                            let vid = s.video_id.get_raw().to_string();
                            let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", &vid));
                            if thumb_path.exists() { return None; }
                            let client = Arc::clone(&client);
                            Some(std::thread::spawn(move || {
                                let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                                if let Ok(resp) = client.get(&url).send() {
                                    if resp.status().is_success() {
                                        if let Ok(bytes) = resp.bytes() {
                                            let _ = std::fs::write(&thumb_path, &bytes);
                                        }
                                    }
                                }
                            }))
                        }).collect();
                        for h in handles { let _ = h.join(); }
                    }
                }

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        match results {
                            Ok(songs) => {
                                let items: Vec<SongItem> = songs.into_iter().take(6).map(|s| {
                                    let avatar = s.artist.chars().next()
                                        .map(|c| c.to_uppercase().to_string())
                                        .unwrap_or_else(|| "?".to_string());
                                    let dur_secs: u32 = s.duration.as_str().split(':')
                                        .collect::<Vec<_>>()
                                        .iter()
                                        .fold(0u32, |acc, p| acc * 60 + p.parse::<u32>().unwrap_or(0));
                                    let vid_raw = s.video_id.get_raw().to_string();
                                    let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", &vid_raw));
                                    let (thumbnail, has_thumbnail) = if thumb_path.exists() {
                                        match slint::Image::load_from_path(&thumb_path) {
                                            Ok(img) => (img, true),
                                            Err(_) => (slint::Image::default(), false),
                                        }
                                    } else {
                                        (slint::Image::default(), false)
                                    };
                                    SongItem {
                                        video_id: SharedString::from(s.video_id.get_raw()),
                                        title: SharedString::from(s.title.as_str()),
                                        artist: SharedString::from(s.artist.as_str()),
                                        album: SharedString::from(
                                            s.album.as_ref().map(|a| a.name.as_str()).unwrap_or(""),
                                        ),
                                        duration_str: SharedString::from(s.duration.as_str()),
                                        avatar_letter: SharedString::from(avatar.as_str()),
                                        duration_secs: dur_secs as i32,
                                        thumbnail,
                                        has_thumbnail,
                                    }
                                }).collect();
                                ui.set_search_suggestions(ModelRc::new(VecModel::from(items)));
                            }
                            Err(_) => {
                                ui.set_search_suggestions(ModelRc::new(VecModel::from(Vec::<SongItem>::new())));
                            }
                        }
                    }
                }).ok();
            });
        });
    }

    // HWND will be resolved lazily after the window is shown
    #[cfg(target_os = "windows")]
    let app_hwnd: Arc<std::sync::atomic::AtomicIsize> = Arc::new(std::sync::atomic::AtomicIsize::new(0));

    // ── Background state polling (500ms) ──────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        #[cfg(target_os = "windows")]
        let app_hwnd = app_hwnd.clone();
        let autoplay_enabled = autoplay_enabled.clone();
        let autoplay_queue_data = autoplay_queue_data.clone();
        let autoplay_seed_vid = autoplay_seed_vid.clone();
        std::thread::spawn(move || {
            let http = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .ok();
            let mut last_thumbnail_id = String::new();
            let mut last_precached_id = String::new(); // track which next-song we've pre-cached
            let mut thumb_refresh_counter: u32 = 0;
            let mut thumb_fetch_spawned = false;

            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));

                // Minimize-to-tray: if window is minimized and setting is on, hide it
                #[cfg(target_os = "windows")]
                {
                    use windows::Win32::Foundation::HWND;
                    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, ShowWindow, SW_HIDE, FindWindowW};
                    use windows::core::w;

                    // Lazily resolve HWND (window exists after ui.run() starts)
                    let mut hwnd_val = app_hwnd.load(std::sync::atomic::Ordering::Relaxed);
                    if hwnd_val == 0 {
                        let found = unsafe {
                            FindWindowW(None, w!("Auricle"))
                                .map(|h| h.0 as isize)
                                .unwrap_or(0)
                        };
                        if found != 0 {
                            app_hwnd.store(found, std::sync::atomic::Ordering::Relaxed);
                            hwnd_val = found;
                        }
                    }

                    let settings = core::persistence::load_settings();
                    if settings.minimize_to_tray && hwnd_val != 0 {
                        let hwnd = HWND(hwnd_val as *mut _);
                        unsafe {
                            if IsIconic(hwnd).as_bool() {
                                let _ = ShowWindow(hwnd, SW_HIDE);
                            }
                        }
                    }
                }

                // Auto-advance when track ends
                if playback.take_advance_pending() {
                    playback.next_track();
                    let ui_w = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            refresh_native_shell_ui(&ui, playback);
                        }
                    }).ok();
                }

                // Autoplay: play next from autoplay queue when user queue runs out
                if playback.take_autoplay_needed() {
                    if autoplay_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                        let next_song = {
                            let mut aq = autoplay_queue_data.lock().unwrap();
                            if !aq.is_empty() { Some(aq.remove(0)) } else { None }
                        };
                        if let Some(song) = next_song {
                            {
                                let mut state = playback.state_lock();
                                state.is_playing = true;
                            }
                            playback.set_now_playing(&song.video_id, &song.title, &song.artist, song.duration_secs);
                            let ui_w = ui_weak.clone();
                            let aq_data = autoplay_queue_data.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    refresh_native_shell_ui(&ui, playback);
                                    // Update autoplay queue model
                                    let items: Vec<SongItem> = aq_data.lock().unwrap().iter().map(make_song_item).collect();
                                    ui.set_autoplay_queue(ModelRc::new(VecModel::from(items)));
                                }
                            }).ok();
                        } else {
                            // Autoplay queue exhausted — fetch more based on seed
                            let seed = autoplay_seed_vid.lock().unwrap().clone();
                            if !seed.is_empty() {
                                let ui_w = ui_weak.clone();
                                let aq_data = autoplay_queue_data.clone();
                                std::thread::spawn(move || {
                                    fetch_autoplay_queue(ui_w, aq_data, seed);
                                });
                            }
                        }
                    }
                    // If autoplay disabled, do nothing — playback stops
                }

                // Start the playback timer the moment audio actually begins playing.
                if playback.take_audio_just_started() {
                    playback.on_audio_started();
                    // NOW kick off the cache download — after stream URL is resolved
                    // so yt-dlp cookie DB isn't locked by a competing process.
                    let np = playback.now_playing();
                    if !np.video_id.is_empty() && np.video_id != "native-prototype" {
                        crate::core::cache::spawn_prefetch(np.video_id.clone(), np.title.clone(), np.artist.clone());
                        // Persist as last played
                        let mut settings = core::persistence::load_settings();
                        settings.last_played = Some(core::persistence::StoredSong {
                            video_id: np.video_id,
                            title: np.title,
                            artist: np.artist,
                            duration_secs: np.duration_secs,
                        });
                        core::persistence::save_settings(&settings);
                    }
                }

                let track = playback.now_playing();
                let is_playing = playback.is_playing();
                let elapsed = playback.elapsed_secs();
                let dur = playback.track_duration_secs();
                let liked = playback.is_liked(&track.video_id);

                let progress = if dur > 0 { (elapsed / dur as f64).clamp(0.0, 1.0) as f32 } else { 0.0 };
                let current_time = format!("{}:{:02}", elapsed as u32 / 60, elapsed as u32 % 60);
                let total_time = if dur > 0 {
                    format!("{}:{:02}", dur / 60, dur % 60)
                } else {
                    "--:--".to_string()
                };
                let initial = track.title.chars().next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".to_string());

                // Fetch YouTube thumbnail when track changes (path only — load image on UI thread)
                let thumbnail_changed = track.video_id != last_thumbnail_id
                    && !track.video_id.is_empty()
                    && track.video_id != "native-prototype";

                let thumb_path: Option<std::path::PathBuf> = if thumbnail_changed {
                    if let Some(ref client) = http {
                        let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", track.video_id);
                        if let Ok(resp) = client.get(&url).send() {
                            if resp.status().is_success() {
                                if let Ok(bytes) = resp.bytes() {
                                    let tmp = std::env::temp_dir()
                                        .join(format!("ytm_thumb_{}.jpg", track.video_id));
                                    if std::fs::write(&tmp, &bytes).is_ok() { Some(tmp) } else { None }
                                } else { None }
                            } else { None }
                        } else { None }
                    } else { None }
                } else { None };

                if thumbnail_changed {
                    last_thumbnail_id = track.video_id.clone();
                }

                // ── Push cache stats to UI every poll tick ──
                let (cache_used_mb, cache_song_count, cache_limit_mb) = {
                    crate::core::cache::AudioCache::global().lock().ok()
                        .map(|c| (
                            (c.total_bytes() / (1024 * 1024)) as i32,
                            c.count() as i32,
                            (c.limit_bytes() / (1024 * 1024)) as i32,
                        ))
                        .unwrap_or((0, 0, 500))
                };

                // ── Pre-cache next song once current song is 25% through ──
                if is_playing && progress > 0.25 && last_precached_id != track.video_id {
                    last_precached_id = track.video_id.clone();
                    playback.cache_next_song_if_needed();
                }

                // ── Periodic thumbnail fetch for queue items (every 3s = 6 ticks) ──
                thumb_refresh_counter += 1;
                if thumb_refresh_counter >= 6 && !thumb_fetch_spawned {
                    thumb_refresh_counter = 0;
                    // Check if any queue items are missing thumbnails
                    let missing: Vec<String> = playback.full_queue().iter()
                        .filter(|np| {
                            !np.video_id.is_empty()
                                && !std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", np.video_id)).exists()
                        })
                        .map(|np| np.video_id.clone())
                        .collect();
                    if !missing.is_empty() {
                        thumb_fetch_spawned = true;
                        let ui_weak_t = ui_weak.clone();
                        std::thread::spawn(move || {
                            let client = reqwest::blocking::Client::builder()
                                .timeout(std::time::Duration::from_secs(5))
                                .build()
                                .ok();
                            if let Some(client) = client {
                                for vid in &missing {
                                    let tp = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                                    if tp.exists() { continue; }
                                    let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                                    if let Ok(resp) = client.get(&url).send() {
                                        if resp.status().is_success() {
                                            if let Ok(bytes) = resp.bytes() {
                                                let _ = std::fs::write(&tp, &bytes);
                                            }
                                        }
                                    }
                                }
                            }
                            // After fetching, refresh the queue model on UI thread
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak_t.upgrade() {
                                    let model = ui.get_queue();
                                    let count = model.row_count();
                                    let mut items: Vec<SongItem> = Vec::with_capacity(count);
                                    let mut changed = false;
                                    for i in 0..count {
                                        let mut item = model.row_data(i).unwrap();
                                        if !item.has_thumbnail {
                                            let tp = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", item.video_id.as_str()));
                                            if tp.exists() {
                                                if let Ok(img) = slint::Image::load_from_path(&tp) {
                                                    item.thumbnail = img;
                                                    item.has_thumbnail = true;
                                                    changed = true;
                                                }
                                            }
                                        }
                                        items.push(item);
                                    }
                                    if changed {
                                        ui.set_queue(ModelRc::new(VecModel::from(items)));
                                    }
                                }
                            }).ok();
                        });
                    } else {
                        // All thumbnails present, stop checking
                        thumb_fetch_spawned = false;
                    }
                }
                // Reset spawn flag once thread completes (approximate: after 10s)
                if thumb_fetch_spawned && thumb_refresh_counter == 0 {
                    thumb_fetch_spawned = false;
                }

                let ui_w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        ui.set_track_title(SharedString::from(track.title.as_str()));
                        ui.set_track_artist(SharedString::from(track.artist.as_str()));
                        ui.set_is_playing(is_playing);
                        ui.set_now_playing_video_id(SharedString::from(track.video_id.as_str()));
                        ui.set_progress(progress);
                        ui.set_current_time(SharedString::from(current_time.as_str()));
                        ui.set_total_time(SharedString::from(total_time.as_str()));
                        ui.set_track_initial(SharedString::from(initial.as_str()));
                        ui.set_is_liked(liked);
                        ui.set_cache_used_mb(cache_used_mb);
                        ui.set_cache_song_count(cache_song_count);
                        ui.set_cache_limit_mb(cache_limit_mb);
                        if let Some(path) = thumb_path {
                            if let Ok(img) = slint::Image::load_from_path(&path) {
                                ui.set_album_art(img);
                                ui.set_has_album_art(true);
                            }
                        } else if thumbnail_changed {
                            ui.set_has_album_art(false);
                        }
                    }
                }).ok();
            }
        });
    }

    // ── System tray + minimize-to-tray ────────────────────────────────────────

    let _tray_icon = {
        use tray_icon::{TrayIconBuilder, menu::{Menu, MenuItem}};
        use tray_icon::Icon;

        let icon = {
            let png = include_bytes!("../icons/32x32.png");
            let img = image::load_from_memory(png)
                .expect("tray icon decode")
                .to_rgba8();
            let (w, h) = img.dimensions();
            Icon::from_rgba(img.into_raw(), w, h).expect("tray icon rgba")
        };

        let show_item = MenuItem::new("Show Player", true, None);
        let hide_item = MenuItem::new("Hide Player", true, None);
        let quit_item = MenuItem::new("Quit", true, None);
        let show_id = show_item.id().clone();
        let hide_id = hide_item.id().clone();
        let quit_id = quit_item.id().clone();
        let menu = Menu::new();
        let _ = menu.append(&show_item);
        let _ = menu.append(&hide_item);
        let _ = menu.append(&tray_icon::menu::PredefinedMenuItem::separator());
        let _ = menu.append(&quit_item);

        let tray = TrayIconBuilder::new()
            .with_tooltip("Auricle")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .build()
            .ok();

        // Handle tray menu events
        #[cfg(target_os = "windows")]
        {
            let app_hwnd = app_hwnd.clone();
            let show_id2 = show_id.clone();
            let hide_id2 = hide_id.clone();
            let quit_id2 = quit_id.clone();
            tray_icon::menu::MenuEvent::set_event_handler(Some(move |event: tray_icon::menu::MenuEvent| {
                use windows::Win32::Foundation::HWND;
                use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SetForegroundWindow, SW_RESTORE, SW_HIDE};
                let hwnd_val = app_hwnd.load(std::sync::atomic::Ordering::Relaxed);
                if hwnd_val == 0 { return; }
                let hwnd = HWND(hwnd_val as *mut _);
                if event.id() == &show_id2 {
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                        let _ = SetForegroundWindow(hwnd);
                    }
                } else if event.id() == &hide_id2 {
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                    }
                } else if event.id() == &quit_id2 {
                    std::process::exit(0);
                }
            }));
        }

        // Handle tray icon double-click to restore
        #[cfg(target_os = "windows")]
        {
            let app_hwnd = app_hwnd.clone();
            tray_icon::TrayIconEvent::set_event_handler(Some(move |event: tray_icon::TrayIconEvent| {
                if let tray_icon::TrayIconEvent::DoubleClick { .. } = event {
                    use windows::Win32::Foundation::HWND;
                    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SetForegroundWindow, SW_RESTORE};
                    let hwnd_val = app_hwnd.load(std::sync::atomic::Ordering::Relaxed);
                    if hwnd_val == 0 { return; }
                    let hwnd = HWND(hwnd_val as *mut _);
                    unsafe {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                        let _ = SetForegroundWindow(hwnd);
                    }
                }
            }));
        }

        tray
    };

    // Close button works normally (exits the app)
    // Minimize-to-tray is handled in the 500ms polling loop below.

    // Restore last played song (paused)
    {
        let settings = core::persistence::load_settings();
        if let Some(last) = settings.last_played {
            let np = crate::core::playback::NowPlaying {
                video_id: last.video_id.clone(),
                title: last.title.clone(),
                artist: last.artist.clone(),
                duration_secs: last.duration_secs,
            };
            playback.set_now_playing_paused(np);
            ui.set_track_title(SharedString::from(last.title.as_str()));
            ui.set_track_artist(SharedString::from(last.artist.as_str()));
            ui.set_now_playing_video_id(SharedString::from(last.video_id.as_str()));
            ui.set_total_time(SharedString::from(format!("{}:{:02}", last.duration_secs / 60, last.duration_secs % 60).as_str()));
            ui.set_is_playing(false);
            let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", last.video_id));
            if thumb_path.exists() {
                if let Ok(img) = slint::Image::load_from_path(&thumb_path) {
                    ui.set_album_art(img);
                    ui.set_has_album_art(true);
                }
            }
            // Fetch autoplay queue for last played song
            {
                autoplay_seed_vid.lock().unwrap().replace_range(.., &last.video_id);
                let ui_w = ui.as_weak();
                let aq_data = autoplay_queue_data.clone();
                let vid = last.video_id.clone();
                std::thread::spawn(move || {
                    fetch_autoplay_queue(ui_w, aq_data, vid);
                });
            }
        }
    }

    // ── Load library data (Albums/Artists/Playlists tabs) ────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_load_library_data(move || {
            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                let tab = ui.get_library_tab().to_string();
                match tab.as_str() {
                    "Albums" => {
                        // Derive unique albums from liked songs
                        let liked = playback.get_liked_songs();
                        let mut seen = std::collections::HashSet::new();
                        let mut albums: Vec<AlbumItem> = Vec::new();
                        for song in &liked {
                            let album_name = String::new(); // NowPlaying has no album field
                            if !album_name.is_empty() && seen.insert(album_name.clone()) {
                                let avatar = album_name.chars().next()
                                    .map(|c| c.to_uppercase().to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                let _ = avatar;
                                albums.push(AlbumItem {
                                    title: SharedString::from(album_name.as_str()),
                                    browse_id: SharedString::default(), // no browse-id from liked songs
                                    artist: SharedString::from(song.artist.as_str()),
                                    year: SharedString::default(),
                                    thumbnail: Default::default(),
                                    has_thumbnail: false,
                                });
                            }
                        }
                        ui.set_library_albums(ModelRc::new(VecModel::from(albums)));
                    },
                    "Artists" => {
                        // Derive unique artists from liked songs
                        let liked = playback.get_liked_songs();
                        let mut seen = std::collections::HashSet::new();
                        let mut artists: Vec<ArtistItem> = Vec::new();
                        for song in &liked {
                            let artist_name = song.artist.clone();
                            if !artist_name.is_empty() && seen.insert(artist_name.clone()) {
                                artists.push(ArtistItem {
                                    name: SharedString::from(artist_name.as_str()),
                                    browse_id: SharedString::default(), // will be resolved on click
                                    thumbnail: Default::default(),
                                    has_thumbnail: false,
                                    subscriber_count: SharedString::default(),
                                });
                            }
                        }
                        ui.set_library_artists(ModelRc::new(VecModel::from(artists)));
                    },
                    "Playlists" => {
                        // Fetch library playlists from API in background
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .unwrap();
                            let result: Result<Vec<ytmapi_rs::parse::LibraryPlaylist>, String> = rt.block_on(async {
                                // Library playlists require authentication; return empty for now
                                Ok(vec![])
                            });
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak2.upgrade() {
                                    match result {
                                        Ok(playlists) => {
                                            let items: Vec<PlaylistItem> = playlists.into_iter().map(|p| {
                                                let count_text = SharedString::default();
                                                PlaylistItem {
                                                    title: SharedString::from(p.title.as_str()),
                                                    playlist_id: SharedString::from(p.playlist_id.get_raw()),
                                                    thumbnail: Default::default(),
                                                    has_thumbnail: false,
                                                    count_text,
                                                }
                                            }).collect();
                                            ui.set_library_playlists(ModelRc::new(VecModel::from(items)));
                                        },
                                        Err(_) => {
                                            // Leave empty on error
                                        }
                                    }
                                }
                            }).ok();
                        });
                    },
                    _ => {}
                }
            }
        });
    }

    // ── Sort library (liked songs) ────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_sort_library(move |sort_mode| {
            if let Some(ui) = ui_weak.upgrade() {
                let model = ui.get_liked_songs();
                let count = model.row_count();
                let mut items: Vec<SongItem> = (0..count).filter_map(|i| model.row_data(i)).collect();
                match sort_mode.as_str() {
                    "alpha" => {
                        items.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
                    },
                    "most-played" => {
                        // No play count tracked yet; sort by artist as fallback
                        items.sort_by(|a, b| a.artist.to_lowercase().cmp(&b.artist.to_lowercase()));
                    },
                    _ => {
                        // "recent" = reverse (most recently liked first = original order from playback)
                        let liked = playback.get_liked_songs();
                        let order_map: std::collections::HashMap<String, usize> = liked.iter().enumerate()
                            .map(|(i, s)| (s.video_id.clone(), i))
                            .collect();
                        items.sort_by_key(|item| order_map.get(item.video_id.as_str()).copied().unwrap_or(usize::MAX));
                    }
                }
                ui.set_liked_songs(ModelRc::new(VecModel::from(items)));
            }
        });
    }

    // ── Navigate to Playlist (full view) ────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        let nav_restoring = nav_restoring.clone();
        ui.on_navigate_to_playlist(move |playlist_id| {
            let playlist_id = playlist_id.to_string();
            if playlist_id.trim().is_empty() { return; }

            if !nav_restoring.load(std::sync::atomic::Ordering::Relaxed) {
                push_nav_entry(&nav_history, &nav_cursor, "Playlist".to_string(), playlist_id.clone());
                if let Some(ui) = ui_weak.upgrade() {
                    let hist = nav_history.lock().unwrap();
                    let cur = *nav_cursor.lock().unwrap();
                    update_nav_buttons(&ui, &hist, cur);
                }
            }

            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_loading(true);
                ui.set_current_view(SharedString::from("Playlist"));
            }

            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let result = rt.block_on(async {
                    use ytmapi_rs::YtMusic;
                    use ytmapi_rs::common::PlaylistID;

                    let api = YtMusic::new_unauthenticated().await.map_err(|e| e.to_string())?;
                    let id = PlaylistID::from_raw(&playlist_id);
                    let tracks = api.get_playlist_tracks(id).await.map_err(|e| e.to_string())?;
                    Ok::<Vec<ytmapi_rs::parse::PlaylistItem>, String>(tracks)
                });

                let ui_weak3 = ui_weak2.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_is_loading(false);
                        match result {
                            Ok(tracks) => {
                                let songs: Vec<Song> = map_playlist_items(tracks);
                                let mut total_secs: u32 = 0;
                                let items: Vec<SongItem> = songs.iter().map(|s| {
                                    let dur = s.duration.unwrap_or(0);
                                    total_secs += dur;
                                    let avatar = s.name.chars().next()
                                        .map(|c| c.to_uppercase().to_string())
                                        .unwrap_or_else(|| "?".to_string());
                                    let dur_str = if dur > 0 {
                                        format!("{}:{:02}", dur / 60, dur % 60)
                                    } else { String::new() };
                                    SongItem {
                                        video_id: SharedString::from(s.video_id.as_str()),
                                        title: SharedString::from(s.name.as_str()),
                                        artist: SharedString::from(s.artist.name.as_str()),
                                        album: SharedString::from(s.album.as_ref().map(|a| a.name.as_str()).unwrap_or("")),
                                        duration_str: SharedString::from(dur_str.as_str()),
                                        avatar_letter: SharedString::from(avatar.as_str()),
                                        duration_secs: dur as i32,
                                        thumbnail: Default::default(),
                                        has_thumbnail: false,
                                    }
                                }).collect();

                                let count = items.len();
                                let hours = total_secs / 3600;
                                let mins = (total_secs % 3600) / 60;
                                let duration_str = if hours > 0 {
                                    format!("{} hr {} min", hours, mins)
                                } else {
                                    format!("{} min", mins)
                                };

                                // Try to derive title from first track artist or use playlist_id
                                let title = if !items.is_empty() {
                                    // Use "Playlist" as fallback — the API doesn't return playlist title from get_playlist_tracks
                                    "Playlist".to_string()
                                } else {
                                    "Playlist".to_string()
                                };

                                ui.set_playlist_view_title(SharedString::from(title.as_str()));
                                ui.set_playlist_view_count(SharedString::from(format!("{}", count).as_str()));
                                ui.set_playlist_view_duration(SharedString::from(duration_str.as_str()));
                                ui.set_playlist_view_has_thumbnail(false);
                                ui.set_playlist_view_similar(ModelRc::new(VecModel::from(Vec::<PlaylistItem>::new())));
                                ui.set_playlist_view_songs(ModelRc::new(VecModel::from(items)));

                                // Fetch thumbnails for songs in background
                                let need_thumbs: Vec<(usize, String)> = songs.iter().enumerate()
                                    .filter_map(|(i, s)| {
                                        if s.video_id.is_empty() { return None; }
                                        let path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", &s.video_id));
                                        if path.exists() { None } else { Some((i, s.video_id.clone())) }
                                    })
                                    .collect();

                                // Fetch playlist thumbnail from first track
                                let thumb_url = songs.first()
                                    .and_then(|s| s.thumbnails.last())
                                    .map(|t| t.url.clone());

                                if let Some(url) = thumb_url {
                                    let ui_w_thumb = ui_weak3.clone();
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            if let Ok(resp) = client.get(&url).send() {
                                                if resp.status().is_success() {
                                                    if let Ok(bytes) = resp.bytes() {
                                                        let path = std::env::temp_dir().join("ytm_playlist_thumb.jpg");
                                                        let _ = std::fs::write(&path, &bytes);
                                                        slint::invoke_from_event_loop(move || {
                                                            if let Some(ui) = ui_w_thumb.upgrade() {
                                                                if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                    ui.set_playlist_view_thumbnail(img);
                                                                    ui.set_playlist_view_has_thumbnail(true);
                                                                }
                                                            }
                                                        }).ok();
                                                    }
                                                }
                                            }
                                        }
                                    });
                                }

                                // Fetch song thumbnails in background
                                if !need_thumbs.is_empty() {
                                    let ui_w_songs = ui_weak3.clone();
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(8)).build() {
                                            for (idx, vid) in &need_thumbs {
                                                let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                                                let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                                                if let Ok(resp) = client.get(&url).send() {
                                                    if let Ok(bytes) = resp.bytes() {
                                                        if let Ok(img) = image::load_from_memory(&bytes) {
                                                            let rgba = img.to_rgba8();
                                                            let (w, h) = (rgba.width(), rgba.height());
                                                            let _ = std::fs::write(&thumb_path, bytes.as_ref());
                                                            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                                            let ui_w4 = ui_w_songs.clone();
                                                            let idx_copy = *idx;
                                                            slint::invoke_from_event_loop(move || {
                                                                if let Some(ui) = ui_w4.upgrade() {
                                                                    let slint_img = slint::Image::from_rgba8(buf);
                                                                    let model = ui.get_playlist_view_songs();
                                                                    if let Some(row) = model.row_data(idx_copy) {
                                                                        let mut updated = row;
                                                                        updated.thumbnail = slint_img;
                                                                        updated.has_thumbnail = true;
                                                                        model.set_row_data(idx_copy, updated);
                                                                    }
                                                                }
                                                            }).ok();
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    });
                                }

                                // Search for similar playlists in background
                                let playlist_title = title.clone();
                                let first_artist = songs.first().map(|s| s.artist.name.clone()).unwrap_or_default();
                                let ui_w_similar = ui_weak3;
                                std::thread::spawn(move || {
                                    let rt2 = tokio::runtime::Builder::new_current_thread()
                                        .enable_all()
                                        .build()
                                        .unwrap();
                                    rt2.block_on(async {
                                        use ytmapi_rs::YtMusic;
                                        use ytmapi_rs::query::{SearchQuery, search::PlaylistsFilter};

                                        let api = match YtMusic::new_unauthenticated().await {
                                            Ok(a) => a,
                                            Err(_) => return,
                                        };
                                        let search_term = if first_artist.is_empty() { playlist_title } else { first_artist };
                                        if let Ok(results) = api.query(SearchQuery::new(&search_term).with_filter(PlaylistsFilter)).await {
                                            let similar_raw: Vec<(String, String, String, Option<String>)> = results.into_iter()
                                                .filter_map(|p| {
                                                    use ytmapi_rs::parse::SearchResultPlaylist;
                                                    match p {
                                                        SearchResultPlaylist::Featured(f) => {
                                                            let thumb = f.thumbnails.last().map(|t| t.url.clone());
                                                            Some((f.title.clone(), f.playlist_id.get_raw().to_string(), f.songs.clone(), thumb))
                                                        },
                                                        SearchResultPlaylist::Community(c) => {
                                                            let thumb = c.thumbnails.last().map(|t| t.url.clone());
                                                            Some((c.title.clone(), c.playlist_id.get_raw().to_string(), c.views.clone(), thumb))
                                                        },
                                                        _ => None,
                                                    }
                                                })
                                                .take(8)
                                                .collect();

                                            let thumb_urls: Vec<(usize, String)> = similar_raw.iter().enumerate()
                                                .filter_map(|(i, (_, _, _, t))| t.as_ref().map(|url| (i, url.clone())))
                                                .collect();

                                            let ui_w5 = ui_w_similar.clone();
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_w_similar.upgrade() {
                                                    let items: Vec<PlaylistItem> = similar_raw.into_iter().map(|(title, pid, count, _)| {
                                                        PlaylistItem {
                                                            title: SharedString::from(title.as_str()),
                                                            playlist_id: SharedString::from(pid.as_str()),
                                                            thumbnail: Default::default(),
                                                            has_thumbnail: false,
                                                            count_text: SharedString::from(count.as_str()),
                                                        }
                                                    }).collect();
                                                    ui.set_playlist_view_similar(ModelRc::new(VecModel::from(items)));
                                                }
                                            }).ok();

                                            // Fetch thumbnails for similar playlists
                                            if !thumb_urls.is_empty() {
                                                std::thread::spawn(move || {
                                                    if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                                        for (idx, url) in &thumb_urls {
                                                            let path = std::env::temp_dir().join(format!("ytm_pl_similar_{}.jpg", idx));
                                                            if let Ok(resp) = client.get(url).send() {
                                                                if resp.status().is_success() {
                                                                    if let Ok(bytes) = resp.bytes() {
                                                                        let _ = std::fs::write(&path, &bytes);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        let indices: Vec<usize> = thumb_urls.iter().map(|(i, _)| *i).collect();
                                                        slint::invoke_from_event_loop(move || {
                                                            if let Some(ui) = ui_w5.upgrade() {
                                                                let model = ui.get_playlist_view_similar();
                                                                let mut items: Vec<PlaylistItem> = (0..model.row_count()).map(|i| model.row_data(i).unwrap()).collect();
                                                                for idx in indices {
                                                                    if idx < items.len() {
                                                                        let path = std::env::temp_dir().join(format!("ytm_pl_similar_{}.jpg", idx));
                                                                        if path.exists() {
                                                                            if let Ok(img) = slint::Image::load_from_path(&path) {
                                                                                items[idx].thumbnail = img;
                                                                                items[idx].has_thumbnail = true;
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                ui.set_playlist_view_similar(ModelRc::new(VecModel::from(items)));
                                                            }
                                                        }).ok();
                                                    }
                                                });
                                            }
                                        }
                                    });
                                });
                            }
                            Err(e) => { log::error!("Get playlist tracks failed: {e}"); }
                        }
                    }
                }).ok();
            });
        });
    }

    // ── Play Playlist (play all songs from the Playlist page) ────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_play_playlist(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let model = ui.get_playlist_view_songs();
                let songs: Vec<core::playback::NowPlaying> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .map(|item| core::playback::NowPlaying {
                        video_id: item.video_id.to_string(),
                        title: item.title.to_string(),
                        artist: item.artist.to_string(),
                        duration_secs: item.duration_secs as u32,
                    })
                    .collect();
                if !songs.is_empty() {
                    let playback = crate::core::bridge::playback_core();
                    playback.set_queue(songs);
                    {
                        let mut state = playback.state_lock();
                        state.is_playing = true;
                    }
                    let first = playback.now_playing();
                    playback.set_now_playing(&first.video_id, &first.title, &first.artist, first.duration_secs);
                    refresh_native_shell_ui(&ui, playback);
                }
            }
        });
    }

    // ── Play mix (load mix songs into queue and start playing) ────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_play_mix(move |mix_index| {
            if let Some(ui) = ui_weak.upgrade() {
                let model = match mix_index {
                    1 => ui.get_home_mix_1(),
                    2 => ui.get_home_mix_2(),
                    3 => ui.get_home_mix_3(),
                    _ => return,
                };
                let songs: Vec<core::playback::NowPlaying> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .map(|item| core::playback::NowPlaying {
                        video_id: item.video_id.to_string(),
                        title: item.title.to_string(),
                        artist: item.artist.to_string(),
                        duration_secs: item.duration_secs as u32,
                    })
                    .collect();
                if !songs.is_empty() {
                    let playback = crate::core::bridge::playback_core();
                    playback.set_queue(songs);
                    {
                        let mut state = playback.state_lock();
                        state.is_playing = true;
                    }
                    let first = playback.now_playing();
                    playback.set_now_playing(&first.video_id, &first.title, &first.artist, first.duration_secs);
                    refresh_native_shell_ui(&ui, playback);
                }
            }
        });
    }

    // ── Load home data (new releases, mixes, genre, language sections) ────────
    {
        let ui_weak = ui.as_weak();
        ui.on_load_home_data(move || {
            fetch_home_enhanced_data(ui_weak.clone());
        });
    }

    // Fetch trending and personalized songs for the Home page
    fetch_trending_songs(ui.as_weak());
    fetch_personalized_songs(ui.as_weak());

    // Fetch enhanced home data (new releases, mixes, genre, language)
    fetch_home_enhanced_data(ui.as_weak());

    ui.run()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
      let _ = app.get_webview_window("main").expect("no main window").set_focus();
      // Handle deep links from CLI args (Windows/Linux)
      for arg in args {
        if arg.contains("com.googleusercontent.apps.363288825229-jlbiuir8ds26t7mt7jkdneqpq6o0s639") {
           let _ = app.emit("deep-link", arg).unwrap();
        }
      }
    }))
    .plugin(tauri_plugin_deep_link::init())
    .plugin(tauri_plugin_shell::init())
    .invoke_handler(tauri::generate_handler![
        search_music, 
        search_artists,
        save_user_data,
        load_user_data,
        get_google_auth_url,
        exchange_google_code,
        get_library_playlists,
        get_playlist_tracks,
        get_artist_details,
        get_album_details,
        generate_smart_mix,
        push_to_cloud,
        pull_from_cloud,
        record_autoplay_interaction,
        get_autoplay_learning_signals,
        get_genre_affinity,
        update_media_metadata,
        minimize_window,
        get_stream_url,
        bridge_get_playback_state,
        bridge_set_now_playing,
        bridge_set_playing,
        bridge_toggle_play_pause,
        bridge_prev_track,
        bridge_next_track,
        bridge_seed_queue,
    ])
    .setup(|app| {
        let window = app.get_webview_window("main").unwrap();

        let show_item = tauri::menu::MenuItem::with_id(app, "tray_show", "Show", true, None::<&str>)
            .map_err(|e| e.to_string())?;
        let separator = tauri::menu::PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
        let quit_item = tauri::menu::MenuItem::with_id(app, "tray_quit", "Quit", true, None::<&str>)
            .map_err(|e| e.to_string())?;
        let tray_menu = tauri::menu::Menu::with_items(app, &[&show_item, &separator, &quit_item])
            .map_err(|e| e.to_string())?;
        
        // Setup system tray icon with click-to-restore behavior
        let tray_builder = tauri::tray::TrayIconBuilder::with_id("main-tray")
            .tooltip("Auricle")
            .menu(&tray_menu)
            .on_menu_event(|app, event| {
                if event.id == "tray_show" {
                    if let Some(w) = app.get_webview_window("main") {
                        let _ = w.show();
                        let _ = w.unminimize();
                        let _ = w.set_focus();
                    }
                } else if event.id == "tray_quit" {
                    std::process::exit(0);
                }
            })
            .on_tray_icon_event(|tray, event| {
                use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};

                match event {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                    _ => {}
                }
            });

        let tray_builder = if let Some(icon) = app.default_window_icon().cloned() {
            tray_builder.icon(icon)
        } else {
            tray_builder
        };

        let _tray = tray_builder.build(app).map_err(|e| e.to_string())?;
        
        #[cfg(target_os = "windows")]
        let hwnd = Some(window.hwnd().unwrap().0 as *mut std::ffi::c_void);
        #[cfg(not(target_os = "windows"))]
        let hwnd = None;

        let config = PlatformConfig {
            dbus_name: "auricle-desktop",
            display_name: "Auricle",
            hwnd, 
        };
        
        let mut controls = MediaControls::new(config).map_err(|e| e.to_string())?;
        
        let handle = app.handle().clone();
        controls.attach(move |event| {
            match event {
                MediaControlEvent::Play => { let _ = handle.emit("media-play", ()).unwrap(); },
                MediaControlEvent::Pause => { let _ = handle.emit("media-pause", ()).unwrap(); },
                MediaControlEvent::Next => { let _ = handle.emit("media-next", ()).unwrap(); },
                MediaControlEvent::Previous => { let _ = handle.emit("media-prev", ()).unwrap(); },
                MediaControlEvent::Toggle => { let _ = handle.emit("media-toggle", ()).unwrap(); },
                _ => {}
            }
        }).map_err(|e| e.to_string())?;

        app.manage(MediaControlsState {
            controls: Mutex::new(controls),
        });

      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
