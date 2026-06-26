use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use reqwest::Client;
use rusqlite::{params, Connection};
use serde::Deserialize;

// --- Rate limiting ---
static LAST_REQUEST: Lazy<Mutex<Instant>> = Lazy::new(|| Mutex::new(Instant::now() - Duration::from_secs(2)));

// --- In-memory caches ---
static ARTIST_CACHE: Lazy<Mutex<HashMap<String, Vec<String>>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static RECORDING_CACHE: Lazy<Mutex<HashMap<String, Vec<String>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

// --- Response structs ---
#[derive(Deserialize)]
struct MBArtistSearchResponse {
    artists: Option<Vec<MBArtist>>,
}

#[derive(Deserialize)]
struct MBArtist {
    tags: Option<Vec<MBTag>>,
}

#[derive(Deserialize)]
struct MBRecordingSearchResponse {
    recordings: Option<Vec<MBRecording>>,
}

#[derive(Deserialize)]
struct MBRecording {
    tags: Option<Vec<MBTag>>,
}

#[derive(Deserialize, Clone)]
struct MBTag {
    name: String,
    count: Option<i64>,
}

fn http_client() -> Client {
    Client::builder()
        .user_agent("Euphonium/0.1.0 (github.com/user/euphonium)")
        .build()
        .unwrap_or_else(|_| Client::new())
}

async fn rate_limit() {
    let wait = {
        let mut last = LAST_REQUEST.lock().unwrap();
        let elapsed = last.elapsed();
        let wait = if elapsed < Duration::from_secs(1) {
            Duration::from_secs(1) - elapsed
        } else {
            Duration::ZERO
        };
        *last = Instant::now() + wait;
        wait
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

fn extract_top_tags(tags: Option<Vec<MBTag>>) -> Vec<String> {
    let Some(mut tags) = tags else { return vec![] };
    tags.sort_by(|a, b| b.count.unwrap_or(0).cmp(&a.count.unwrap_or(0)));
    tags.into_iter().take(5).map(|t| t.name).collect()
}

pub async fn lookup_artist_genres(artist_name: &str) -> Vec<String> {
    let key = artist_name.to_lowercase();
    if let Some(cached) = ARTIST_CACHE.lock().unwrap().get(&key) {
        return cached.clone();
    }

    rate_limit().await;

    let url = format!(
        "https://musicbrainz.org/ws/2/artist/?query=artist:{}&fmt=json&limit=1",
        urlencoding::encode(&key)
    );

    let result = match http_client().get(&url).send().await {
        Ok(resp) => match resp.json::<MBArtistSearchResponse>().await {
            Ok(data) => {
                let tags = data.artists.and_then(|a| a.into_iter().next()).and_then(|a| a.tags);
                extract_top_tags(tags)
            }
            Err(_) => vec![],
        },
        Err(_) => vec![],
    };

    ARTIST_CACHE.lock().unwrap().insert(key, result.clone());
    result
}

pub async fn lookup_recording_genres(title: &str, artist: &str) -> Vec<String> {
    let key = format!("{}:{}", title.to_lowercase(), artist.to_lowercase());
    if let Some(cached) = RECORDING_CACHE.lock().unwrap().get(&key) {
        return cached.clone();
    }

    rate_limit().await;

    let query = format!("recording:{} AND artist:{}", title, artist);
    let url = format!(
        "https://musicbrainz.org/ws/2/recording/?query={}&fmt=json&limit=1",
        urlencoding::encode(&query)
    );

    let result = match http_client().get(&url).send().await {
        Ok(resp) => match resp.json::<MBRecordingSearchResponse>().await {
            Ok(data) => {
                let tags = data.recordings.and_then(|r| r.into_iter().next()).and_then(|r| r.tags);
                extract_top_tags(tags)
            }
            Err(_) => vec![],
        },
        Err(_) => vec![],
    };

    RECORDING_CACHE.lock().unwrap().insert(key, result.clone());
    result
}

fn now_ts() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn store_genre_cache(conn: &Connection, entity_key: &str, genres: &[String]) {
    let genres_str = genres.join(",");
    let _ = conn.execute(
        "INSERT OR REPLACE INTO genre_cache (entity_key, genres, fetched_at) VALUES (?1, ?2, ?3)",
        params![entity_key, genres_str, now_ts()],
    );
}

fn load_genre_cache(conn: &Connection, entity_key: &str) -> Option<Vec<String>> {
    let cutoff = now_ts() - (30 * 24 * 60 * 60); // 30 days
    conn.query_row(
        "SELECT genres FROM genre_cache WHERE entity_key = ?1 AND fetched_at > ?2",
        params![entity_key, cutoff],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .map(|g| g.split(',').map(|s| s.to_string()).filter(|s| !s.is_empty()).collect())
}

pub async fn enrich_genre_data(db_path: std::path::PathBuf, from_artist: &str, to_artist: &str) {
    let from = from_artist.to_lowercase();
    let to = to_artist.to_lowercase();

    let conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Enrich from_artist
    let from_key = format!("artist:{}", from);
    if load_genre_cache(&conn, &from_key).is_none() {
        let genres = lookup_artist_genres(&from).await;
        if !genres.is_empty() {
            store_genre_cache(&conn, &from_key, &genres);
        }
    }

    // Enrich to_artist
    let to_key = format!("artist:{}", to);
    if load_genre_cache(&conn, &to_key).is_none() {
        let genres = lookup_artist_genres(&to).await;
        if !genres.is_empty() {
            store_genre_cache(&conn, &to_key, &genres);
        }
    }
}
