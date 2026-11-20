use serde::{Deserialize, Serialize};
use ytmapi_rs::query::{SearchQuery, search::SongsFilter};
use ytmapi_rs::common::YoutubeID;

pub mod core;
mod context_ui;
mod discography_ui;
mod now_playing_ui;
mod search_ui;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use slint::{Model, ModelRc, SharedString, VecModel};



slint::include_modules!();

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

fn map_search_results(results: Vec<ytmapi_rs::parse::SearchResultSong>) -> Vec<Song> {
    results.into_iter().map(|r| {
        let song = Song {
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
        };
        // Cache album/artist ids so playback + context menus can resolve
        // "go to album/artist" for this video later, by id.
        let (album_name, album_id) = song.album.as_ref()
            .map(|a| (a.name.as_str(), a.browse_id.as_str()))
            .unwrap_or(("", ""));
        crate::core::playback::register_song_meta(&song.video_id, album_name, album_id, &song.artist.browse_id);
        song
    }).collect()
}

fn map_playlist_items(results: Vec<ytmapi_rs::parse::PlaylistItem>) -> Vec<Song> {
    results.into_iter().filter_map(|p| {
        match p {
            ytmapi_rs::parse::PlaylistItem::Song(track) => {
                let credits = context_ui::credits(&track.artists);
                let video_id = track.video_id.get_raw().to_string();
                let artist_id = credits.first().map(|artist| artist.browse_id.clone()).unwrap_or_default();
                core::menu_metadata::register_artists("Song", &video_id, credits.clone());
                core::playback::register_song_meta(&video_id, &track.album.name, track.album.id.get_raw(), &artist_id);
                Some(Song {
                    video_id,
                    name: track.title,
                    artist: ArtistRef { name: context_ui::credit_names(&credits), browse_id: artist_id },
                    album: Some(AlbumRef { name: track.album.name, browse_id: track.album.id.get_raw().into() }),
                    duration: parse_duration(&track.duration),
                    thumbnails: track.thumbnails.into_iter().map(|thumb| Thumbnail {
                        url: thumb.url, width: thumb.width as u64, height: thumb.height as u64,
                    }).collect(),
                })
            }
            ytmapi_rs::parse::PlaylistItem::Video(v) => {
                  let song = Song {
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
                };
                  crate::core::playback::register_song_meta(&song.video_id, "", "", &song.artist.browse_id);
                  Some(song)
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
    let (thumbnail, has_thumbnail) = load_cached_thumb(&thumb_path);
    SongItem {
        video_id: SharedString::from(t.video_id.as_str()),
        title: SharedString::from(t.title.as_str()),
        artist: SharedString::from(t.artist.as_str()),
        album: SharedString::from(t.album.as_str()),
        duration_str: SharedString::from(dur_str.as_str()),
        avatar_letter: SharedString::from(avatar.as_str()),
        duration_secs: t.duration_secs as i32,
        thumbnail,
        has_thumbnail,
    }
}

/// Build a `StoredSong` from the scalar fields carried by a context-menu action,
/// backfilling album/browse-ids from the session metadata registry.
fn stored_song_from_parts(vid: &str, title: &str, artist: &str, dur: u32) -> core::persistence::StoredSong {
    let meta = core::playback::get_song_meta(vid).unwrap_or_default();
    core::persistence::StoredSong {
        video_id: vid.to_string(),
        title: title.to_string(),
        artist: artist.to_string(),
        duration_secs: dur,
        album: meta.album,
        album_id: meta.album_id,
        artist_id: meta.artist_id,
    }
}

/// In-place Fisher-Yates shuffle using a time-seeded LCG (no extra crate needed).
fn shuffle_vec<T>(v: &mut [T]) {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1;
    let n = v.len();
    for i in (1..n).rev() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = ((seed >> 33) as usize) % (i + 1);
        v.swap(i, j);
    }
}

/// Push the current local playlists into the add-to-playlist modal's list.
fn refresh_modal_playlists(ui: &NativeShellWindow) {
    let items: Vec<PlaylistItem> = core::library::local_playlists().into_iter().map(|p| {
        let n = p.songs.len();
        PlaylistItem {
            title: SharedString::from(p.name.as_str()),
            playlist_id: SharedString::from(p.id.as_str()),
            thumbnail: Default::default(),
            has_thumbnail: false,
            count_text: SharedString::from(format!("{} song{}", n, if n == 1 { "" } else { "s" }).as_str()),
        }
    }).collect();
    ui.set_modal_playlists(ModelRc::new(VecModel::from(items)));
}

/// Compute the "your artists" list for the sidebar: followed artists first, then
/// the most-listened artists from your liked songs (dedup by name).
fn compute_sidebar_artists() -> Vec<ArtistItem> {
    use std::collections::{HashMap, HashSet};
    let mut out: Vec<ArtistItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for a in core::library::followed_artists() {
        if !a.name.is_empty() && seen.insert(a.name.to_lowercase()) {
            out.push(ArtistItem {
                name: SharedString::from(a.name.as_str()),
                browse_id: SharedString::from(a.browse_id.as_str()),
                thumbnail: Default::default(),
                has_thumbnail: false,
                subscriber_count: SharedString::default(),
            });
        }
    }
    let liked = crate::core::bridge::playback_core().get_liked_songs();
    let mut counts: HashMap<String, (usize, String, String)> = HashMap::new();
    for s in &liked {
        if s.artist.is_empty() { continue; }
        let e = counts.entry(s.artist.to_lowercase()).or_insert((0, s.artist.clone(), String::new()));
        e.0 += 1;
        if e.2.is_empty() && !s.artist_id.is_empty() { e.2 = s.artist_id.clone(); }
    }
    let mut ranked: Vec<(usize, String, String)> = counts.into_values().collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, name, aid) in ranked {
        if out.len() >= 8 { break; }
        if seen.insert(name.to_lowercase()) {
            out.push(ArtistItem {
                name: SharedString::from(name.as_str()),
                browse_id: SharedString::from(aid.as_str()),
                thumbnail: Default::default(),
                has_thumbnail: false,
                subscriber_count: SharedString::default(),
            });
        }
    }
    out
}

/// Refresh the sidebar "your artists" + "your playlists" shortcut lists.
fn refresh_sidebar(ui: &NativeShellWindow) {
    ui.set_sidebar_artists(ModelRc::new(VecModel::from(compute_sidebar_artists())));
    let pls: Vec<PlaylistItem> = core::library::local_playlists().into_iter().map(|p| {
        let n = p.songs.len();
        // Use the first song's cached thumbnail as the playlist cover.
        let (thumb, has) = p.songs.first()
            .map(|s| load_cached_thumb(&std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", s.video_id))))
            .unwrap_or_else(|| (Default::default(), false));
        PlaylistItem {
            title: SharedString::from(p.name.as_str()),
            playlist_id: SharedString::from(p.id.as_str()),
            thumbnail: thumb,
            has_thumbnail: has,
            count_text: SharedString::from(format!("{} song{}", n, if n == 1 { "" } else { "s" }).as_str()),
        }
    }).collect();
    ui.set_sidebar_playlists(ModelRc::new(VecModel::from(pls)));
    ui.set_sidebar_albums(ModelRc::new(VecModel::from(compute_sidebar_albums())));
    // Load artist + album images for the sidebar rows in the background.
    spawn_sidebar_artist_thumbs(ui.as_weak());
    spawn_sidebar_album_thumbs(ui.as_weak());
}

/// Background: fetch artist images for the sidebar "your artists" rows, cache
/// them, and fill the rows. Followed artists carry a channel id; artists derived
/// from liked songs often do not, so we resolve those by name via search (whose
/// result already carries a usable thumbnail). Rows are matched by name so the
/// name-only artists get their image too.
fn spawn_sidebar_artist_thumbs(ui_weak: slint::Weak<NativeShellWindow>) {
    // (name, browse_id) — ArtistItem holds a non-Send Image, so extract plain data.
    let seeds: Vec<(String, String)> = compute_sidebar_artists().iter()
        .map(|a| (a.name.to_string(), a.browse_id.to_string()))
        .filter(|(n, _)| !n.is_empty())
        .collect();
    if seeds.is_empty() { return; }
    core::net::spawn(move || {
        let http = core::net::http();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok();
        // Build the API handle ONCE for the whole batch. `new_unauthenticated`
        // sets up a TLS stack and does a token round-trip, so creating it per
        // artist (as this loop used to) multiplied startup cost by the number of
        // sidebar artists. Reusing it across `block_on` calls is safe because
        // they all run on this same, still-alive runtime.
        let api = rt.as_ref().and_then(|rt| {
            rt.block_on(async { crate::core::net::ytmusic().await.ok() })
        });
        for (name, bid) in seeds {
            // Cache key: channel id when known, else the (lowercased) name.
            let key = if bid.is_empty() { format!("name_{}", name.to_lowercase()) } else { bid.clone() };
            let path = library_thumb_path("artist", &key);
            if !path.exists() {
                let mut url = String::new();
                if let (Some(rt), Some(api)) = (rt.as_ref(), api.as_ref()) {
                    let name_q = name.clone();
                    let bid_q = bid.clone();
                    url = rt.block_on(async {
                        use ytmapi_rs::common::ArtistChannelID;
                        use ytmapi_rs::query::{SearchQuery, search::ArtistsFilter};
                        // Have a channel id → fetch the artist page thumbnail.
                        if !bid_q.is_empty() {
                            if let Ok(art) = api.get_artist(ArtistChannelID::from_raw(&bid_q)).await {
                                return pick_thumb(&art.thumbnails).map(|t| t.url.clone());
                            }
                        }
                        // Otherwise resolve by name; the search result carries a thumbnail.
                        let res = api.query(SearchQuery::new(name_q).with_filter(ArtistsFilter)).await.ok()?;
                        let first = res.into_iter().next()?;
                        if let Some(t) = pick_thumb(&first.thumbnails) { return Some(t.url.clone()); }
                        let art = api.get_artist(first.browse_id).await.ok()?;
                        pick_thumb(&art.thumbnails).map(|t| t.url.clone())
                    }).unwrap_or_default();
                }
                if url.is_empty() { continue; }
                if let Ok(resp) = http.get(&url).send() {
                    if let Ok(bytes) = resp.bytes() { let _ = std::fs::write(&path, &bytes); }
                }
            }
            if path.exists() {
                let name_key = name.to_lowercase();
                let uiw = ui_weak.clone();
                let p = path.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uiw.upgrade() {
                        if let Ok(img) = slint::Image::load_from_path(&p) {
                            let sb = ui.get_sidebar_artists();
                            for i in 0..sb.row_count() {
                                if let Some(mut row) = sb.row_data(i) {
                                    if row.name.as_str().to_lowercase() == name_key {
                                        row.thumbnail = img;
                                        row.has_thumbnail = true;
                                        sb.set_row_data(i, row);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }).ok();
            }
        }
    });
}

/// Albums from recently-listened songs (currently playing first, then history),
/// de-duplicated and newest-first, for the sidebar "recently listened" shortcut.
/// Recently-listened albums for the sidebar, as plain data.
///
/// Deliberately does NOT touch images: the background thumbnail fetcher only
/// needs ids, and calling [`compute_sidebar_albums`] there decoded every cover a
/// second time on the UI thread.
fn sidebar_album_seeds() -> Vec<(String, String, String)> {
    use std::collections::HashSet;
    let pb = crate::core::bridge::playback_core();
    let mut entries: Vec<core::playback::NowPlaying> = Vec::new();
    let np = pb.now_playing();
    if !np.album_id.is_empty() && !np.album.is_empty() { entries.push(np); }
    entries.extend(pb.get_history());
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for e in entries {
        if e.album_id.is_empty() || e.album.is_empty() { continue; }
        if !seen.insert(e.album_id.clone()) { continue; }
        out.push((e.album_id, e.album, e.artist));
        if out.len() >= 8 { break; }
    }
    out
}

fn compute_sidebar_albums() -> Vec<AlbumItem> {
    sidebar_album_seeds().into_iter().map(|(album_id, album, artist)| {
        let (thumb, has) = load_cached_thumb(&library_thumb_path("album", &album_id));
        AlbumItem {
            title: SharedString::from(album.as_str()),
            browse_id: SharedString::from(album_id.as_str()),
            artist: SharedString::from(artist.as_str()),
            year: SharedString::default(),
            thumbnail: thumb,
            has_thumbnail: has,
        }
    }).collect()
}

/// Background: fetch album art for the sidebar "recently listened" albums.
fn spawn_sidebar_album_thumbs(ui_weak: slint::Weak<NativeShellWindow>) {
    let ids: Vec<String> = sidebar_album_seeds().into_iter()
        .map(|(id, _, _)| id)
        .filter(|id| !id.is_empty() && !library_thumb_path("album", id).exists())
        .collect();
    if ids.is_empty() { return; }
    core::net::spawn(move || {
        let http = core::net::http();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok();
        // One API handle for the whole batch — see spawn_sidebar_artist_thumbs.
        let api = rt.as_ref().and_then(|rt| {
            rt.block_on(async { crate::core::net::ytmusic().await.ok() })
        });
        for id in ids {
            let path = library_thumb_path("album", &id);
            if !path.exists() {
                let mut url = String::new();
                if let (Some(rt), Some(api)) = (rt.as_ref(), api.as_ref()) {
                    let id_q = id.clone();
                    url = rt.block_on(async {
                        use ytmapi_rs::common::AlbumID;
                        let alb = api.get_album(AlbumID::from_raw(&id_q)).await.ok()?;
                        pick_thumb(&alb.thumbnails).map(|t| t.url.clone())
                    }).unwrap_or_default();
                }
                if url.is_empty() { continue; }
                if let Ok(resp) = http.get(&url).send() {
                    if let Ok(bytes) = resp.bytes() { let _ = std::fs::write(&path, &bytes); }
                }
            }
            if path.exists() {
                let id2 = id.clone();
                let uiw = ui_weak.clone();
                let p = path.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uiw.upgrade() {
                        if let Ok(img) = slint::Image::load_from_path(&p) {
                            let sb = ui.get_sidebar_albums();
                            for i in 0..sb.row_count() {
                                if let Some(mut row) = sb.row_data(i) {
                                    if row.browse_id == id2 {
                                        row.thumbnail = img;
                                        row.has_thumbnail = true;
                                        sb.set_row_data(i, row);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }).ok();
            }
        }
    });
}

/// Temp-cache path for a library artist/album thumbnail.
fn library_thumb_path(kind: &str, id: &str) -> std::path::PathBuf {
    let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    std::env::temp_dir().join(format!("ytm_lib_{}_{}.jpg", kind, safe))
}

/// Deletes thumbnails cached by older builds, which stored whatever the API
/// listed last — for artists that could be a 2880x1200 banner.
///
/// These files are keyed by id, so the usual `path.exists()` check would keep
/// reusing them forever and the UI thread would keep paying to decode them.
/// Anything far larger than a right-sized JPEG is dropped so it gets refetched at
/// a sane resolution. Runs once ever, tracked in settings.
fn purge_oversized_thumb_cache() {
    // A 320px JPEG lands well under this; the banners ran 200 KB to 1 MB.
    const MAX_CACHED_THUMB_BYTES: u64 = 100 * 1024;

    let mut settings = core::persistence::load_settings();
    if settings.thumb_cache_purged {
        return;
    }

    let mut removed = 0usize;
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Only this app's own thumbnail files.
            if !name.starts_with("ytm_") || !name.ends_with(".jpg") {
                continue;
            }
            let oversized = entry.metadata().map(|m| m.len() > MAX_CACHED_THUMB_BYTES).unwrap_or(false);
            if oversized && std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    if removed > 0 {
        eprintln!("[thumb-cache] dropped {removed} oversized cached thumbnails");
    }

    settings.thumb_cache_purged = true;
    core::persistence::save_settings(&settings);
}

// Decoded-thumbnail cache for `load_cached_thumb` (below).
// `thread_local!` is a macro invocation, so this cannot be a `///` doc comment.
thread_local! {
    // Decoded-thumbnail cache, keyed by path.
    //
    // `refresh_native_shell_ui` rebuilds the queue AND the whole liked-songs
    // model on every track change, and each row used to re-read and re-decode
    // its JPEG from disk. With a few dozen liked songs that was tens of
    // synchronous image decodes on the UI thread per song change — the visible
    // "app not responding" blip between tracks.
    //
    // `slint::Image` is refcounted, so returning a cached clone is essentially
    // free. `Image` is also `!Send`, which is exactly why this is thread-local
    // rather than a global mutex.
    //
    // Only successful loads are memoised: a miss stays uncached so a thumbnail
    // that finishes downloading later is still picked up on the next refresh.
    static THUMB_CACHE: std::cell::RefCell<
        std::collections::HashMap<std::path::PathBuf, slint::Image>
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Upper bound on [`THUMB_CACHE`] entries before it is dropped wholesale.
/// These are small (~320px) covers, so a few hundred is cheap to hold.
const THUMB_CACHE_MAX: usize = 512;

/// Loads a cached thumbnail from disk, memoising the decoded image.
fn load_cached_thumb(path: &std::path::Path) -> (slint::Image, bool) {
    if let Some(img) = THUMB_CACHE.with(|c| c.borrow().get(path).cloned()) {
        return (img, true);
    }
    if path.exists() {
        if let Ok(img) = slint::Image::load_from_path(path) {
            THUMB_CACHE.with(|c| {
                let mut map = c.borrow_mut();
                if map.len() >= THUMB_CACHE_MAX {
                    map.clear();
                }
                map.insert(path.to_path_buf(), img.clone());
            });
            return (img, true);
        }
    }
    (Default::default(), false)
}

/// Longest edge, in pixels, we ever want a cached thumbnail to be.
///
/// The largest place any of these images is drawn is a ~176px card, so 320
/// still leaves headroom for high-DPI displays.
const THUMB_TARGET_PX: u64 = 320;

/// Picks the smallest thumbnail variant that still covers [`THUMB_TARGET_PX`] on
/// its longest edge, falling back to the largest available.
///
/// The API offers everything from 60px squares up to 2880x1200 artist banners.
/// The previous code took the last (i.e. always the biggest) variant, so the
/// on-disk cache ended up holding multi-megapixel banners that then had to be
/// decoded on the UI thread just to be drawn at 48px in the sidebar.
fn pick_thumb<T: ThumbSize>(thumbs: &[T]) -> Option<&T> {
    thumbs
        .iter()
        .filter(|t| t.longest_edge() >= THUMB_TARGET_PX)
        .min_by_key(|t| t.longest_edge())
        .or_else(|| thumbs.iter().max_by_key(|t| t.longest_edge()))
}

/// Tidies an artist bio for display.
///
/// YouTube Music appends a Wikipedia attribution like
/// `From Wikipedia (https://en.wikipedia.org/wiki/Some_Artist)`. Two things go
/// wrong with it:
///   * the API's plain-text extraction drops the link run, leaving a dangling
///     `From Wikipedia (` — which is what actually rendered in the UI; and
///   * when the URL *is* present it is one unbreakable token, so a word-wrapping
///     `Text` cannot fit it on a line and silently clips it.
/// Either way, keep the credit and drop the link.
fn clean_artist_description(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;

    // Remove any "(http…)" / "(https…)" parenthetical, wherever it appears.
    while let Some(open) = rest.find("(http") {
        match rest[open..].find(')') {
            Some(rel_close) => {
                out.push_str(&rest[..open]);
                rest = &rest[open + rel_close + 1..];
            }
            // Unbalanced parenthesis: drop the remainder rather than emit a
            // half-open link.
            None => {
                out.push_str(&rest[..open]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);

    // Tidy the whitespace the removal leaves behind, then drop any trailing
    // empty parenthesis left by the API's own link stripping.
    let mut cleaned = out
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    while cleaned.ends_with('(') || cleaned.ends_with("()") {
        cleaned.truncate(cleaned.trim_end_matches(['(', ')']).len());
        cleaned = cleaned.trim_end().to_string();
    }

    cleaned
}

/// Lets [`pick_thumb`] work over both our own [`Thumbnail`] and the one the
/// `ytmapi_rs` responses carry.
trait ThumbSize {
    fn longest_edge(&self) -> u64;
}

impl ThumbSize for Thumbnail {
    fn longest_edge(&self) -> u64 { self.width.max(self.height) }
}

impl ThumbSize for ytmapi_rs::common::Thumbnail {
    fn longest_edge(&self) -> u64 { self.width.max(self.height) }
}

/// Background: fetch missing album-art for the saved-albums list (from the stored
/// thumbnail URL, or via the API if none), cache it, and update the model rows.
fn spawn_album_thumb_loader(ui_weak: slint::Weak<NativeShellWindow>) {
    core::net::spawn(move || {
        let albums = core::library::saved_albums();
        let http = core::net::http();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok();
        // One API handle for the whole batch, built lazily: most albums already
        // have a stored thumbnail URL, so often it is never needed at all.
        let mut api = None;
        for a in albums {
            if a.browse_id.is_empty() { continue; }
            let path = library_thumb_path("album", &a.browse_id);
            if !path.exists() {
                let mut url = a.thumbnail_url.clone();
                if url.is_empty() {
                    if let Some(ref rt) = rt {
                        if api.is_none() {
                            api = rt.block_on(async { crate::core::net::ytmusic().await.ok() });
                        }
                        if let Some(api) = api.as_ref() {
                            url = rt.block_on(async {
                                use ytmapi_rs::common::AlbumID;
                                let alb = api.get_album(AlbumID::from_raw(&a.browse_id)).await.ok()?;
                                pick_thumb(&alb.thumbnails).map(|t| t.url.clone())
                            }).unwrap_or_default();
                        }
                        if !url.is_empty() { core::library::set_album_thumbnail(&a.browse_id, &url); }
                    }
                }
                if url.is_empty() { continue; }
                if let Ok(resp) = http.get(&url).send() {
                    if let Ok(bytes) = resp.bytes() { let _ = std::fs::write(&path, &bytes); }
                }
            }
            if path.exists() {
                let bid = a.browse_id.clone();
                let uiw = ui_weak.clone();
                let p = path.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uiw.upgrade() {
                        if let Ok(img) = slint::Image::load_from_path(&p) {
                            let model = ui.get_library_albums();
                            for i in 0..model.row_count() {
                                if let Some(mut row) = model.row_data(i) {
                                    if row.browse_id == bid {
                                        row.thumbnail = img;
                                        row.has_thumbnail = true;
                                        model.set_row_data(i, row);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }).ok();
            }
        }
    });
}

/// Background: fetch missing artist images for the followed-artists list, cache
/// them, and update the model rows (and the sidebar).
fn spawn_artist_thumb_loader(ui_weak: slint::Weak<NativeShellWindow>) {
    core::net::spawn(move || {
        let artists = core::library::followed_artists();
        let http = core::net::http();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok();
        // One API handle for the whole batch, built lazily — see the album loader.
        let mut api = None;
        for a in artists {
            if a.browse_id.is_empty() { continue; }
            let path = library_thumb_path("artist", &a.browse_id);
            if !path.exists() {
                let mut url = a.thumbnail_url.clone();
                if url.is_empty() {
                    if let Some(ref rt) = rt {
                        if api.is_none() {
                            api = rt.block_on(async { crate::core::net::ytmusic().await.ok() });
                        }
                        if let Some(api) = api.as_ref() {
                            url = rt.block_on(async {
                                use ytmapi_rs::common::ArtistChannelID;
                                let art = api.get_artist(ArtistChannelID::from_raw(&a.browse_id)).await.ok()?;
                                pick_thumb(&art.thumbnails).map(|t| t.url.clone())
                            }).unwrap_or_default();
                        }
                        if !url.is_empty() { core::library::set_artist_thumbnail(&a.browse_id, &url); }
                    }
                }
                if url.is_empty() { continue; }
                if let Ok(resp) = http.get(&url).send() {
                    if let Ok(bytes) = resp.bytes() { let _ = std::fs::write(&path, &bytes); }
                }
            }
            if path.exists() {
                let bid = a.browse_id.clone();
                let uiw = ui_weak.clone();
                let p = path.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = uiw.upgrade() {
                        if let Ok(img) = slint::Image::load_from_path(&p) {
                            let model = ui.get_library_artists();
                            for i in 0..model.row_count() {
                                if let Some(mut row) = model.row_data(i) {
                                    if row.browse_id == bid {
                                        row.thumbnail = img.clone();
                                        row.has_thumbnail = true;
                                        model.set_row_data(i, row);
                                        break;
                                    }
                                }
                            }
                            // Also update the sidebar artists row if present.
                            let sb = ui.get_sidebar_artists();
                            for i in 0..sb.row_count() {
                                if let Some(mut row) = sb.row_data(i) {
                                    if row.browse_id == bid {
                                        row.thumbnail = img.clone();
                                        row.has_thumbnail = true;
                                        sb.set_row_data(i, row);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }).ok();
            }
        }
    });
}

fn fetch_radio_songs(seed: &str) -> Vec<core::playback::NowPlaying> {
    if seed.is_empty() {
        return Vec::new();
    }
    let seed_vid = seed.to_string();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[autoplay] failed to build runtime: {e}");
            return Vec::new();
        }
    };
    let songs = rt.block_on(async {
        let api = match crate::core::net::ytmusic().await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[autoplay] failed to init api: {e}");
                return Vec::new();
            }
        };
        use ytmapi_rs::common::YoutubeID;
        let vid = ytmapi_rs::common::VideoID::from_raw(seed_vid.clone());
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
                ..Default::default()
            }
        }).collect::<Vec<_>>()
    });
    // Filter out the seed song itself
    let songs: Vec<_> = songs.into_iter().filter(|s| s.video_id != seed_vid).collect();

    // Drop tracks played very recently so radio keeps moving forward instead of
    // replaying the same batch. Fall back to the seed-filtered list if dedup
    // would leave nothing to play.
    let songs: Vec<_> = {
        let pb = crate::core::bridge::playback_core();
        let mut recent: std::collections::HashSet<String> =
            pb.get_history().into_iter().take(30).map(|h| h.video_id).collect();
        recent.insert(pb.now_playing().video_id);
        let deduped: Vec<_> = songs.iter().filter(|s| !recent.contains(&s.video_id)).cloned().collect();
        if deduped.is_empty() { songs } else { deduped }
    };

    // Gently bias the radio toward the user's taste profile (followed artists,
    // liked-song artists, saved-album artists) WITHOUT replacing YouTube's
    // ordering: a familiar artist only nudges a song a few spots earlier, while
    // most of the queue stays whatever radio returned — so unfamiliar music keeps
    // coming through and discovery is preserved. `BIAS` is the one subtlety knob.
    let taste = TasteAffinity::build();
    if taste.is_empty() {
        songs
    } else {
        const BIAS: f32 = 2.5; // max spots a full-taste match can climb
        let mut ranked: Vec<(f32, usize, core::playback::NowPlaying)> = songs
            .into_iter()
            .enumerate()
            .map(|(i, s)| (i as f32 - BIAS * taste.score(&s.artist), i, s))
            .collect();
        ranked.sort_by(|a, b| a.0
            .partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1)));
        ranked.into_iter().map(|(_, _, s)| s).collect()
    }
}

fn spawn_autoplay_thumbnails(
    ui_weak: slint::Weak<NativeShellWindow>,
    songs: &[core::playback::NowPlaying],
) {
    let need_thumbs: Vec<String> = songs
        .iter()
        .filter(|s| {
            !s.video_id.is_empty()
                && !std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", s.video_id)).exists()
        })
        .map(|s| s.video_id.clone())
        .collect();
    if need_thumbs.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let http = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        for vid in need_thumbs {
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
                        let ui_w = ui_weak.clone();
                        let vid_copy = vid.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                let slint_img = slint::Image::from_rgba8(buf);
                                let model = ui.get_autoplay_queue();
                                for i in 0..model.row_count() {
                                    if let Some(mut row) = model.row_data(i) {
                                        if row.video_id.as_str() == vid_copy {
                                            row.thumbnail = slint_img;
                                            row.has_thumbnail = true;
                                            model.set_row_data(i, row);
                                            break;
                                        }
                                    }
                                }
                            }
                        }).ok();
                    }
                }
            }
        }
    });
}

fn fetch_autoplay_queue(
    ui_weak: slint::Weak<NativeShellWindow>,
    video_id: String,
    resume: bool,
) {
    let songs = fetch_radio_songs(&video_id);
    if songs.is_empty() {
        eprintln!("[autoplay] watch playlist returned 0 songs");
        if resume {
            let pb = crate::core::bridge::playback_core();
            if pb.is_awaiting_autoplay_after_end() {
                pb.stop_at_queue_end();
                let ui_w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        refresh_native_shell_ui(&ui, crate::core::bridge::playback_core());
                    }
                }).ok();
            }
        }
        return;
    }
    // Store in shared state
    {
        crate::core::bridge::playback_core().set_autoplay_queue(songs.clone());
    }

    // Triggered by an exhausted queue — resume playback now that fresh songs
    // are available, otherwise playback would dead-stop at the end of radio.
    if resume {
        crate::core::bridge::playback_core().request_autoplay();
    }

    let ui_weak2 = ui_weak.clone();
    // Update UI model
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let current = crate::core::bridge::playback_core().autoplay_queue();
            let items: Vec<SongItem> = current.iter().map(make_song_item).collect();
            ui.set_autoplay_queue(ModelRc::new(VecModel::from(items)));
        }
    }).ok();

    spawn_autoplay_thumbnails(ui_weak2, &songs);
}

fn refill_autoplay_queue(ui_weak: slint::Weak<NativeShellWindow>) {
    let pb = crate::core::bridge::playback_core();
    if !pb.try_begin_autoplay_refill() {
        return;
    }
    std::thread::spawn(move || {
        struct RefillGuard(&'static core::playback::PlaybackCore);
        impl Drop for RefillGuard {
            fn drop(&mut self) {
                self.0.end_autoplay_refill();
            }
        }
        let pb = crate::core::bridge::playback_core();
        let _guard = RefillGuard(pb);
        let generation = pb.autoplay_generation();
        let seed = pb.autoplay_refill_seed();
        let added = if seed.is_empty() {
            0
        } else {
            let songs = fetch_radio_songs(&seed);
            pb.append_autoplay(songs, generation)
        };
        pb.autoplay_refill_empty
            .store(added == 0, std::sync::atomic::Ordering::Release);
        drop(_guard);
        if added > 0 {
            let full = pb.autoplay_queue();
            let ui_w = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    let current = crate::core::bridge::playback_core().autoplay_queue();
                    let items: Vec<SongItem> = current.iter().map(make_song_item).collect();
                    ui.set_autoplay_queue(ModelRc::new(VecModel::from(items)));
                }
            }).ok();
            spawn_autoplay_thumbnails(ui_weak, &full);
        }
    });
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

    let can_shuffle = playback.has_upcoming_user_queue();
    if ui.get_can_shuffle_queue() != can_shuffle {
        ui.set_can_shuffle_queue(can_shuffle);
    }
    let repeat_str = playback.repeat_mode().as_str();
    if ui.get_repeat_mode().as_str() != repeat_str {
        ui.set_repeat_mode(SharedString::from(repeat_str));
    }

    // Fetch each list once under lock, then reuse for both the models and the
    // missing-thumbnail scan below.
    let upcoming = playback.queue_upcoming();
    let liked = playback.get_liked_songs();
    let mut history: Vec<core::playback::NowPlaying> = playback
        .get_history()
        .into_iter()
        .take(20)
        .filter(|h| !h.video_id.is_empty() && h.video_id != track.video_id && h.video_id != "native-prototype")
        .collect();
    history.reverse();

    let queue_items: Vec<SongItem> = upcoming.iter().map(make_song_item).collect();
    ui.set_queue(ModelRc::new(VecModel::from(queue_items)));

    let history_items: Vec<SongItem> = history.iter().map(make_song_item).collect();
    ui.set_queue_history(ModelRc::new(VecModel::from(history_items)));

    let liked_items: Vec<SongItem> = liked.iter().map(make_song_item).collect();
    ui.set_liked_songs(ModelRc::new(VecModel::from(liked_items)));

    // Spawn background thumbnail fetch for items missing thumbnails
    let mut missing_vids: Vec<String> = Vec::new();
    for np in upcoming.iter().chain(history.iter()).chain(liked.iter()) {
        if np.video_id.is_empty() || missing_vids.contains(&np.video_id) {
            continue;
        }
        let tp = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", np.video_id));
        if !tp.exists() {
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
                        // Refresh queue history thumbnails
                        let model = ui.get_queue_history();
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
                            ui.set_queue_history(ModelRc::new(VecModel::from(items)));
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

struct HeroState {
    window_seed: usize,
    video_id: String,
    pick_offset: usize,
    is_taste_seed: bool,
}

static HERO_STATE: std::sync::OnceLock<Mutex<HeroState>> = std::sync::OnceLock::new();

fn get_hero_state() -> &'static Mutex<HeroState> {
    HERO_STATE.get_or_init(|| Mutex::new(HeroState {
        window_seed: 0,
        video_id: String::new(),
        pick_offset: 0,
        is_taste_seed: false,
    }))
}

fn current_time_window_and_eyebrow() -> (usize, &'static str) {
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let window = (unix_secs / (4 * 3600)) as usize;
    let eyebrows = [
        "FEATURED FOR YOU",
        "MADE FOR YOUR TASTE",
        "FRESH PICK FOR YOU",
        "CURATED FROM YOUR TASTE",
        "YOUR DAILY SPOTLIGHT",
        "RECOMMENDED FOR YOU",
    ];
    let eyebrow = eyebrows[window % eyebrows.len()];
    (window, eyebrow)
}

fn maybe_seed_home_hero(ui: &NativeShellWindow, force_next: bool) {
    let extract = |model: ModelRc<SongItem>| -> Vec<SongItem> {
        (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .filter(|s| !s.video_id.is_empty())
            .collect()
    };

    let personalized = extract(ui.get_personalized_songs());
    let followed = extract(ui.get_home_followed_songs());
    let liked = extract(ui.get_liked_songs());
    let trending = extract(ui.get_trending_songs());

    let is_taste = !personalized.is_empty() || !followed.is_empty() || !liked.is_empty();

    let mut candidates = Vec::new();
    candidates.extend(personalized);
    candidates.extend(followed);
    candidates.extend(liked);
    candidates.extend(trending);

    if candidates.is_empty() {
        return;
    }

    let (current_window, eyebrow) = current_time_window_and_eyebrow();
    let mut state = get_hero_state().lock().unwrap();

    if force_next {
        state.pick_offset = state.pick_offset.wrapping_add(1);
    } else if !state.video_id.is_empty() && state.window_seed == current_window {
        if !state.is_taste_seed && is_taste {
            // Upgrade from fallback to taste once
        } else {
            return;
        }
    }

    state.window_seed = current_window;
    state.is_taste_seed = is_taste;

    let idx = (current_window + state.pick_offset) % candidates.len();
    let pick = candidates[idx].clone();
    state.video_id = pick.video_id.to_string();
    drop(state);

    ui.set_hero_video_id(pick.video_id.clone());
    ui.set_hero_title(pick.title.clone());
    ui.set_hero_artist(pick.artist.clone());
    ui.set_hero_duration_secs(pick.duration_secs);
    ui.set_hero_eyebrow(SharedString::from(eyebrow));

    let vid = pick.video_id.to_string();
    let ui_weak = ui.as_weak();
    core::net::spawn(move || {
        let hero_cache_path = std::env::temp_dir().join(format!("ytm_hero_{}.jpg", vid));
        let thumb_cache_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));

        let mut bytes_opt: Option<Vec<u8>> = None;
        if hero_cache_path.exists() {
            if let Ok(b) = std::fs::read(&hero_cache_path) {
                bytes_opt = Some(b);
            }
        } else if thumb_cache_path.exists() {
            if let Ok(b) = std::fs::read(&thumb_cache_path) {
                bytes_opt = Some(b);
            }
        }

        if bytes_opt.is_none() {
            let http = core::net::http();
            let url_hq = format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", vid);
            if let Ok(resp) = http.get(&url_hq).send() {
                if resp.status().is_success() {
                    if let Ok(bytes) = resp.bytes() {
                        let b = bytes.to_vec();
                        let _ = std::fs::write(&hero_cache_path, &b);
                        bytes_opt = Some(b);
                    }
                }
            }
            if bytes_opt.is_none() {
                let url_mq = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                if let Ok(resp) = http.get(&url_mq).send() {
                    if resp.status().is_success() {
                        if let Ok(bytes) = resp.bytes() {
                            let b = bytes.to_vec();
                            let _ = std::fs::write(&hero_cache_path, &b);
                            bytes_opt = Some(b);
                        }
                    }
                }
            }
        }

        if let Some(bytes) = bytes_opt {
            if let Ok(img) = image::load_from_memory(&bytes) {
                let accent_color = extract_vibrant_accent_from_image(&img);
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());
                let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        if ui.get_hero_video_id().as_str() == vid.as_str() {
                            let slint_img = slint::Image::from_rgba8(buf);
                            ui.set_hero_thumbnail(slint_img);
                            ui.set_hero_has_thumbnail(true);
                            let theme = ui.global::<Theme>();
                            if !theme.get_has_hero_accent() {
                                theme.set_hero_accent(theme.get_effective_accent());
                            }
                            theme.set_has_hero_accent(true);
                            animate_theme_targets(&ui, None, None, Some(accent_color), false);
                        }
                    }
                }).ok();
            }
        }
    });
}

fn fetch_trending_songs(ui_weak: slint::Weak<NativeShellWindow>) {
    core::net::spawn(move || {
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

            let guest_api = crate::core::net::ytmusic().await.ok()?;
            let results = guest_api.query(
                SearchQuery::new(query_string).with_filter(SongsFilter)
            ).await.ok()?;
            Some(map_search_results(results))
        });

        if let Some(songs) = songs {
            // Collect Send-safe raw data (include thumb URL)
            let raw: Vec<(String, String, String, u32, String)> = songs.into_iter().take(15).map(|s| {
                let dur = s.duration.unwrap_or(0);
                let thumb_url = pick_thumb(&s.thumbnails).map(|t| t.url.clone()).unwrap_or_default();
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
                    maybe_seed_home_hero(&ui, false);
                }
            }).ok();

            // Fetch thumbnails in background
            if !thumb_urls.is_empty() {
                core::net::spawn(move || {
                    let http = core::net::http();
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

/// Ranked "taste profile" artist names, combining followed artists (strong
/// signal) + liked-song artists (by frequency) + saved-album artists. This is the
/// shared taste signal Home uses to bias what it surfaces. (H4)
fn taste_profile_top_artists(limit: usize) -> Vec<String> {
    use std::collections::HashMap;
    let mut scores: HashMap<String, (f32, String)> = HashMap::new();
    for a in core::library::followed_artists() {
        let n = a.name.trim();
        if !n.is_empty() { scores.entry(n.to_lowercase()).or_insert((0.0, n.to_string())).0 += 5.0; }
    }
    for s in crate::core::bridge::playback_core().get_liked_songs() {
        let n = s.artist.trim();
        if !n.is_empty() { scores.entry(n.to_lowercase()).or_insert((0.0, n.to_string())).0 += 1.0; }
    }
    for al in core::library::saved_albums() {
        let n = al.artist.trim();
        if !n.is_empty() { scores.entry(n.to_lowercase()).or_insert((0.0, n.to_string())).0 += 2.0; }
    }
    let mut ranked: Vec<(f32, String)> = scores.into_values().collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().take(limit).map(|(_, n)| n).collect()
}

/// Lightweight taste-affinity lookup by artist name, blending followed artists,
/// liked-song artists (by frequency), and saved-album artists. Used to *gently
/// bias* (never replace) the autoplay radio toward the user's taste.
struct TasteAffinity {
    followed: std::collections::HashSet<String>,
    liked_counts: std::collections::HashMap<String, u32>,
    saved_artists: std::collections::HashSet<String>,
    max_liked: u32,
}

impl TasteAffinity {
    fn build() -> Self {
        use std::collections::{HashMap, HashSet};
        let mut followed = HashSet::new();
        for a in core::library::followed_artists() {
            let n = a.name.trim().to_lowercase();
            if !n.is_empty() { followed.insert(n); }
        }
        let mut saved_artists = HashSet::new();
        for al in core::library::saved_albums() {
            let n = al.artist.trim().to_lowercase();
            if !n.is_empty() { saved_artists.insert(n); }
        }
        let mut liked_counts: HashMap<String, u32> = HashMap::new();
        for s in crate::core::bridge::playback_core().get_liked_songs() {
            let n = s.artist.trim().to_lowercase();
            if !n.is_empty() { *liked_counts.entry(n).or_insert(0) += 1; }
        }
        let max_liked = liked_counts.values().copied().max().unwrap_or(1).max(1);
        TasteAffinity { followed, liked_counts, saved_artists, max_liked }
    }

    /// 0.0 (unfamiliar) .. ~1.5 (strongly in taste). Weights: followed 1.0,
    /// liked-frequency up to 0.6, saved-album artist 0.4.
    fn score(&self, artist: &str) -> f32 {
        let n = artist.trim().to_lowercase();
        if n.is_empty() { return 0.0; }
        let mut w = 0.0f32;
        if self.followed.contains(&n) { w += 1.0; }
        if let Some(&c) = self.liked_counts.get(&n) {
            w += 0.6 * (c as f32 / self.max_liked as f32);
        }
        if self.saved_artists.contains(&n) { w += 0.4; }
        w.min(1.5)
    }

    fn is_empty(&self) -> bool {
        self.followed.is_empty() && self.liked_counts.is_empty() && self.saved_artists.is_empty()
    }
}

/// H1: "From artists you follow" — top songs from each followed artist.
fn fetch_followed_songs(ui_weak: slint::Weak<NativeShellWindow>) {
    let followed = core::library::followed_artists();
    if followed.is_empty() { return; }
    core::net::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let raw: Vec<(String, String, String, u32, String)> = rt.block_on(async {
            use ytmapi_rs::query::{SearchQuery, search::SongsFilter};
            let api = match crate::core::net::ytmusic().await { Ok(a) => a, Err(_) => return Vec::new() };
            let mut out: Vec<(String, String, String, u32, String)> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for a in followed.iter().take(8) {
                if a.name.trim().is_empty() { continue; }
                if let Ok(res) = api.query(SearchQuery::new(a.name.clone()).with_filter(SongsFilter)).await {
                    let songs = map_search_results(res);
                    for s in songs.into_iter().take(3) {
                        if seen.insert(s.video_id.clone()) {
                            let thumb = pick_thumb(&s.thumbnails).map(|t| t.url.clone()).unwrap_or_default();
                            out.push((s.video_id, s.name, s.artist.name, s.duration.unwrap_or(0), thumb));
                        }
                    }
                }
            }
            out.truncate(20);
            out
        });
        if raw.is_empty() { return; }
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
                ui.set_home_followed_songs(ModelRc::new(VecModel::from(items)));
                maybe_seed_home_hero(&ui, false);
            }
        }).ok();
        if !thumb_urls.is_empty() {
            core::net::spawn(move || {
                let http = core::net::http();
                for (idx, url) in thumb_urls {
                    if let Ok(resp) = http.get(&url).send() {
                        if let Ok(bytes) = resp.bytes() {
                            if let Ok(img) = image::load_from_memory(&bytes) {
                                let rgba = img.to_rgba8();
                                let (w, h) = (rgba.width(), rgba.height());
                                let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
                                let ui_w = ui_weak2.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_w.upgrade() {
                                        let slint_img = slint::Image::from_rgba8(buf);
                                        let model = ui.get_home_followed_songs();
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

    core::net::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let songs = rt.block_on(async {
            let guest_api = crate::core::net::ytmusic().await.ok()?;
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
                let thumb_url = pick_thumb(&s.thumbnails).map(|t| t.url.clone()).unwrap_or_default();
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
                    maybe_seed_home_hero(&ui, false);
                }
            }).ok();

            // Fetch thumbnails in background
            if !thumb_urls.is_empty() {
                core::net::spawn(move || {
                    let http = core::net::http();
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
/// Current calendar year, derived from the system clock without pulling in a
/// date crate. Uses the mean tropical-year length, which is accurate to the year
/// for our purposes (filtering "recent" releases).
fn current_year_approx() -> i32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    1970 + (secs / 31_556_952) as i32
}

fn fetch_home_enhanced_data(ui_weak: slint::Weak<NativeShellWindow>) {
    let playback = crate::core::bridge::playback_core();
    let liked = playback.get_liked_songs();

    // Top artists for personalization — blends followed artists (strong signal),
    // liked-song artists (by frequency), and saved-album artists. New Releases and
    // the (hidden) mixes are seeded from this, so following an artist surfaces
    // their new music too. (H2 + H4)
    let top_artists: Vec<String> = taste_profile_top_artists(5);
    if top_artists.is_empty() {
        return;
    }

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

    // Seed artists for New Releases: followed artists carry real channel ids (so we
    // can pull their actual discography); taste names fill in behind them.
    let new_release_seeds: Vec<(String, String)> = {
        let mut seeds: Vec<(String, String)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for a in core::library::followed_artists() {
            let key = a.name.trim().to_lowercase();
            if key.is_empty() || !seen.insert(key) { continue; }
            seeds.push((a.name, a.browse_id));
        }
        for name in &top_artists {
            let key = name.trim().to_lowercase();
            if key.is_empty() || !seen.insert(key) { continue; }
            seeds.push((name.clone(), String::new()));
        }
        seeds
    };

    // ── Thread 1: New releases — genuinely recent releases from each seed artist's
    // own discography (get_artist → albums + singles), sorted newest-first and
    // filtered to the last ~2 years. Replaces the old "{artist} new album" search
    // that returned popularity-ranked (often decades-old) catalog entries. ───────
    {
        let ui_weak = ui_weak.clone();
        let seeds = new_release_seeds;
        core::net::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let albums = rt.block_on(async {
                use ytmapi_rs::common::ArtistChannelID;
                use ytmapi_rs::query::{SearchQuery, search::ArtistsFilter};
                let api = match crate::core::net::ytmusic().await {
                    Ok(a) => a,
                    Err(_) => return Vec::new(),
                };
                let current_year = current_year_approx();
                // (title, browse_id, artist, year_i32, thumb_url)
                let mut cand: Vec<(String, String, String, i32, String)> = Vec::new();
                for (name, bid) in seeds.iter().take(6) {
                    // Resolve the artist's channel id (followed artists already have one).
                    let channel = if !bid.is_empty() {
                        bid.clone()
                    } else {
                        match api.query(SearchQuery::new(name.clone()).with_filter(ArtistsFilter)).await {
                            Ok(res) => res.into_iter().next()
                                .map(|a| a.browse_id.get_raw().to_string())
                                .unwrap_or_default(),
                            Err(_) => String::new(),
                        }
                    };
                    if channel.is_empty() { continue; }
                    if let Ok(art) = api.get_artist(ArtistChannelID::from_raw(&channel)).await {
                        let disp = art.name.clone();
                        let releases = art.top_releases.albums.iter()
                            .flat_map(|s| s.results.iter())
                            .chain(art.top_releases.singles.iter().flat_map(|s| s.results.iter()));
                        for a in releases {
                            let year: i32 = a.year.trim().parse().unwrap_or(0);
                            let thumb = pick_thumb(&a.thumbnails).map(|t| t.url.clone()).unwrap_or_default();
                            cand.push((a.title.clone(), a.album_id.get_raw().to_string(), disp.clone(), year, thumb));
                        }
                    }
                }
                // Dedup by browse_id.
                let mut seen = std::collections::HashSet::new();
                cand.retain(|(_, id, _, _, _)| !id.is_empty() && seen.insert(id.clone()));
                // Prefer genuinely recent releases; relax the window if too sparse.
                let mut recent: Vec<_> = cand.iter().filter(|c| c.3 >= current_year - 1).cloned().collect();
                if recent.len() < 4 {
                    recent = cand.iter().filter(|c| c.3 >= current_year - 2).cloned().collect();
                }
                if recent.len() < 4 {
                    recent = cand.clone();
                }
                // Newest first (unknown years sink to the bottom).
                recent.sort_by(|a, b| b.3.cmp(&a.3));
                recent.truncate(10);
                recent.into_iter()
                    .map(|(t, id, ar, y, th)| (t, id, ar, if y > 0 { y.to_string() } else { String::new() }, th))
                    .collect::<Vec<(String, String, String, String, String)>>()
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
                core::net::spawn(move || {
                    let http = core::net::http();
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

    // ── Thread 3: Genre mix ───────────────────────────────────────────────────
    if let Some(genre_kw) = genre_keyword {
        let ui_weak = ui_weak.clone();
        core::net::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let songs = rt.block_on(async {
                use ytmapi_rs::query::{SearchQuery, search::SongsFilter};
                let api = match crate::core::net::ytmusic().await {
                    Ok(a) => a,
                    Err(_) => return Vec::new(),
                };
                let query = format!("{} songs playlist", genre_kw);
                api.query(SearchQuery::new(query).with_filter(SongsFilter)).await
                    .unwrap_or_default()
                    .into_iter().take(10)
                    .map(|s| {
                        let dur = parse_duration(&s.duration).unwrap_or(0);
                        let thumb = pick_thumb(&s.thumbnails).map(|t| t.url.clone()).unwrap_or_default();
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
                core::net::spawn(move || {
                    let http = core::net::http();
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
        core::net::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let songs = rt.block_on(async {
                use ytmapi_rs::query::{SearchQuery, search::SongsFilter};
                let api = match crate::core::net::ytmusic().await {
                    Ok(a) => a,
                    Err(_) => return Vec::new(),
                };
                let query = format!("{} music popular songs", language);
                api.query(SearchQuery::new(query).with_filter(SongsFilter)).await
                    .unwrap_or_default()
                    .into_iter().take(10)
                    .map(|s| {
                        let dur = parse_duration(&s.duration).unwrap_or(0);
                        let thumb = pick_thumb(&s.thumbnails).map(|t| t.url.clone()).unwrap_or_default();
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
                core::net::spawn(move || {
                    let http = core::net::http();
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

fn normalize_theme_name(name: &str) -> &'static str {
    match name {
        "Dynamic" => "Dynamic",
        "Solar Red" => "Solar Red",
        "Ember" => "Ember",
        "Earth" => "Earth",
        "Forest" => "Forest",
        "Ocean" => "Ocean",
        "Sky" => "Sky",
        "Twilight" => "Twilight",
        "Rose" => "Rose",
        "Mono" => "Mono",
        _ => "Default Blue",
    }
}

fn theme_accent(name: &str) -> slint::Color {
    let (r, g, b) = match normalize_theme_name(name) {
        "Dynamic" => (0x7c, 0x8c, 0xf8),
        "Solar Red" => (0xfa, 0x2d, 0x48),
        "Ember" => (0xf9, 0x73, 0x16),
        "Earth" => (0xc2, 0x78, 0x49),
        "Forest" => (0x24, 0xc4, 0x66),
        "Ocean" => (0x14, 0xb8, 0xc4),
        "Sky" => (0x38, 0xbd, 0xf8),
        "Twilight" => (0x9b, 0x6d, 0xff),
        "Rose" => (0xec, 0x48, 0x99),
        "Mono" => (0x8b, 0x95, 0xa5),
        _ => (0x5b, 0x8d, 0xef),
    };
    slint::Color::from_rgb_u8(r, g, b)
}

fn normalize_theme_mode(mode: &str) -> &'static str {
    match mode {
        "Light" => "Light",
        "Dark" => "Dark",
        _ => "System",
    }
}

#[cfg(target_os = "windows")]
fn windows_apps_use_light_theme() -> Option<bool> {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
    };

    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(value != 0)
}

#[cfg(not(target_os = "windows"))]
fn windows_apps_use_light_theme() -> Option<bool> { None }

fn theme_is_light(mode: &str) -> bool {
    match normalize_theme_mode(mode) {
        "Light" => true,
        "Dark" => false,
        _ => windows_apps_use_light_theme().unwrap_or(false),
    }
}

fn srgb_u8_to_linear(c: u8) -> f32 {
    let v = c as f32 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb_u8(x: f32) -> u8 {
    let x = x.clamp(0.0, 1.0);
    let v = if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    };
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn color_to_oklab(c: slint::Color) -> (f32, f32, f32) {
    let r = srgb_u8_to_linear(c.red());
    let g = srgb_u8_to_linear(c.green());
    let b = srgb_u8_to_linear(c.blue());

    let l_cone = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
    let m_cone = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
    let s_cone = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;

    let l_ = l_cone.max(0.0).cbrt();
    let m_ = m_cone.max(0.0).cbrt();
    let s_ = s_cone.max(0.0).cbrt();

    let l = 0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_;
    let a = 1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_;
    let b_ok = 0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_;

    (l, a, b_ok)
}

fn oklab_to_color(l: f32, a: f32, b: f32) -> slint::Color {
    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;

    let l_cone = l_ * l_ * l_;
    let m_cone = m_ * m_ * m_;
    let s_cone = s_ * s_ * s_;

    let r_lin =  4.0767416621 * l_cone - 3.3077115913 * m_cone + 0.2309699292 * s_cone;
    let g_lin = -1.2684380046 * l_cone + 2.6097574011 * m_cone - 0.3413193965 * s_cone;
    let b_lin = -0.0041960863 * l_cone - 0.7034186147 * m_cone + 1.7076147010 * s_cone;

    let r = linear_to_srgb_u8(r_lin);
    let g = linear_to_srgb_u8(g_lin);
    let b_u8 = linear_to_srgb_u8(b_lin);

    slint::Color::from_rgb_u8(r, g, b_u8)
}

pub(crate) fn oklab_lerp_color(from: slint::Color, to: slint::Color, t: f32) -> slint::Color {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        return from;
    }
    if t >= 1.0 {
        return to;
    }
    let (l1, a1, b1) = color_to_oklab(from);
    let (l2, a2, b2) = color_to_oklab(to);
    let l = l1 + (l2 - l1) * t;
    let a = a1 + (a2 - a1) * t;
    let b = b1 + (b2 - b1) * t;
    oklab_to_color(l, a, b)
}

fn colors_close(a: slint::Color, b: slint::Color) -> bool {
    (a.red() as i16 - b.red() as i16).abs() <= 1
        && (a.green() as i16 - b.green() as i16).abs() <= 1
        && (a.blue() as i16 - b.blue() as i16).abs() <= 1
}

struct AccentAnimState {
    timer: slint::Timer,
    start: Instant,
    accent_from: slint::Color,
    accent_to: slint::Color,
    art_from: slint::Color,
    art_to: slint::Color,
    hero_from: slint::Color,
    hero_to: slint::Color,
    running: bool,
}

impl Default for AccentAnimState {
    fn default() -> Self {
        Self {
            timer: slint::Timer::default(),
            start: Instant::now(),
            accent_from: slint::Color::default(),
            accent_to: slint::Color::default(),
            art_from: slint::Color::default(),
            art_to: slint::Color::default(),
            hero_from: slint::Color::default(),
            hero_to: slint::Color::default(),
            running: false,
        }
    }
}

thread_local! {
    static ACCENT_ANIM: std::cell::RefCell<AccentAnimState> = std::cell::RefCell::new(AccentAnimState::default());
}

fn animate_theme_targets(
    ui: &NativeShellWindow,
    accent_to: Option<slint::Color>,
    art_to: Option<slint::Color>,
    hero_to: Option<slint::Color>,
    immediate: bool,
) {
    let theme = ui.global::<Theme>();
    let cur_accent = theme.get_accent();
    let cur_art = theme.get_art_accent();
    let cur_hero = theme.get_hero_accent();

    ACCENT_ANIM.with(|anim_cell| {
        let mut anim = anim_cell.borrow_mut();
        let final_accent_to = accent_to.unwrap_or(if anim.running { anim.accent_to } else { cur_accent });
        let final_art_to = art_to.unwrap_or(if anim.running { anim.art_to } else { cur_art });
        let final_hero_to = hero_to.unwrap_or(if anim.running { anim.hero_to } else { cur_hero });

        if immediate {
            anim.timer.stop();
            anim.running = false;
            anim.accent_from = final_accent_to;
            anim.accent_to = final_accent_to;
            anim.art_from = final_art_to;
            anim.art_to = final_art_to;
            anim.hero_from = final_hero_to;
            anim.hero_to = final_hero_to;
            theme.set_accent(final_accent_to);
            theme.set_art_accent(final_art_to);
            theme.set_hero_accent(final_hero_to);
            return;
        }

        if colors_close(cur_accent, final_accent_to)
            && colors_close(cur_art, final_art_to)
            && colors_close(cur_hero, final_hero_to)
            && (!anim.running
                || (colors_close(anim.accent_to, final_accent_to)
                    && colors_close(anim.art_to, final_art_to)
                    && colors_close(anim.hero_to, final_hero_to)))
        {
            if !anim.running {
                return;
            }
        }

        anim.accent_from = cur_accent;
        anim.accent_to = final_accent_to;
        anim.art_from = cur_art;
        anim.art_to = final_art_to;
        anim.hero_from = cur_hero;
        anim.hero_to = final_hero_to;
        anim.start = Instant::now();
        anim.running = true;

        let ui_weak = ui.as_weak();
        anim.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                ACCENT_ANIM.with(|anim_cell| {
                    let mut anim = anim_cell.borrow_mut();
                    if !anim.running {
                        anim.timer.stop();
                        return;
                    }
                    let raw_t = (anim.start.elapsed().as_secs_f32() / 1.10).clamp(0.0, 1.0);
                    let Some(ui) = ui_weak.upgrade() else {
                        anim.running = false;
                        anim.timer.stop();
                        return;
                    };
                    let theme = ui.global::<Theme>();
                    if raw_t >= 1.0 {
                        theme.set_accent(anim.accent_to);
                        theme.set_art_accent(anim.art_to);
                        theme.set_hero_accent(anim.hero_to);
                        anim.running = false;
                        anim.timer.stop();
                    } else {
                        let eased = raw_t * raw_t * (3.0 - 2.0 * raw_t);
                        theme.set_accent(oklab_lerp_color(anim.accent_from, anim.accent_to, eased));
                        theme.set_art_accent(oklab_lerp_color(anim.art_from, anim.art_to, eased));
                        theme.set_hero_accent(oklab_lerp_color(anim.hero_from, anim.hero_to, eased));
                    }
                });
            },
        );
    });
}

fn apply_theme_with_mode(ui: &NativeShellWindow, name: &str, mode: &str, immediate: bool) {
    let norm = normalize_theme_name(name);
    let is_dyn = norm == "Dynamic";
    let theme = ui.global::<Theme>();
    theme.set_is_dynamic(is_dyn);
    theme.set_is_light(theme_is_light(mode));

    let art_target = ACCENT_ANIM.with(|anim_cell| {
        let anim = anim_cell.borrow();
        if anim.running {
            anim.art_to
        } else {
            theme.get_art_accent()
        }
    });
    let target_accent = if is_dyn && theme.get_has_art_accent() {
        art_target
    } else {
        theme_accent(norm)
    };
    animate_theme_targets(ui, Some(target_accent), None, None, immediate);
}

fn apply_theme(ui: &NativeShellWindow, name: &str, mode: &str) {
    apply_theme_with_mode(ui, name, mode, false);
}

fn apply_art_accent(ui: &NativeShellWindow, art_color: Option<slint::Color>) {
    let theme = ui.global::<Theme>();
    match art_color {
        Some(c) => {
            theme.set_has_art_accent(true);
            let accent_target = if theme.get_is_dynamic() {
                Some(c)
            } else {
                None
            };
            animate_theme_targets(ui, accent_target, Some(c), None, false);
        }
        None => {
            theme.set_has_art_accent(false);
            if theme.get_is_dynamic() {
                animate_theme_targets(ui, Some(theme_accent("Dynamic")), None, None, false);
            }
        }
    }
}

fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let v = max;
    let s = if max > 0.0 { delta / max } else { 0.0 };
    let h = if delta <= 1e-6 {
        0.0
    } else if (max - r).abs() < 1e-6 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < 1e-6 {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };
    (h.rem_euclid(360.0), s, v)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let h_prime = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - ((h_prime % 2.0) - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = if h_prime < 1.0 {
        (c, x, 0.0)
    } else if h_prime < 2.0 {
        (x, c, 0.0)
    } else if h_prime < 3.0 {
        (0.0, c, x)
    } else if h_prime < 4.0 {
        (0.0, x, c)
    } else if h_prime < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let r = ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    let g = ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    let b = ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (r, g, b)
}

pub fn extract_vibrant_accent_from_image(img: &image::DynamicImage) -> slint::Color {
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    if w == 0 || h == 0 {
        return slint::Color::from_rgb_u8(0x8b, 0x95, 0xa5);
    }

    let x_step = (w / 24).max(1);
    let y_step = (h / 24).max(1);

    // Skip top/bottom 12% letterbox bars (e.g. YouTube mqdefault.jpg 16:9 black bars)
    let mut y_start = (h as f32 * 0.12) as u32;
    let mut y_end = (h as f32 * 0.88).ceil() as u32;
    if y_start >= y_end || y_end > h {
        y_start = 0;
        y_end = h;
    }

    let mut bin_weights = [0.0f32; 12];
    let mut bin_r = [0.0f32; 12];
    let mut bin_g = [0.0f32; 12];
    let mut bin_b = [0.0f32; 12];
    let mut total_vivid_weight = 0.0f32;

    let mut y = y_start;
    while y < y_end {
        let mut x = 0;
        while x < w {
            let px = rgba.get_pixel(x, y);
            let alpha = px[3];
            if alpha >= 128 {
                let r = px[0] as f32 / 255.0;
                let g = px[1] as f32 / 255.0;
                let b = px[2] as f32 / 255.0;
                let (h_val, s, v) = rgb_to_hsv(r, g, b);

                let is_near_black = v < 0.12;
                let is_near_white = v > 0.96 && s < 0.12;
                if !is_near_black && !is_near_white {
                    let weight = s.powf(1.6) * (1.0 - ((v - 0.62).abs() * 1.35).min(0.9));
                    if weight > 0.001 {
                        let bin = ((h_val / 30.0).floor() as usize).min(11);
                        bin_weights[bin] += weight;
                        bin_r[bin] += r * weight;
                        bin_g[bin] += g * weight;
                        bin_b[bin] += b * weight;
                        total_vivid_weight += weight;
                    }
                }
            }
            x += x_step;
        }
        y += y_step;
    }

    if total_vivid_weight < 1e-3 {
        return slint::Color::from_rgb_u8(0x8b, 0x95, 0xa5);
    }

    let mut best_bin = 0;
    let mut best_weight = 0.0f32;
    for (i, &bw) in bin_weights.iter().enumerate() {
        if bw > best_weight {
            best_weight = bw;
            best_bin = i;
        }
    }

    if best_weight < 1e-4 {
        return slint::Color::from_rgb_u8(0x8b, 0x95, 0xa5);
    }

    let avg_r = bin_r[best_bin] / best_weight;
    let avg_g = bin_g[best_bin] / best_weight;
    let avg_b = bin_b[best_bin] / best_weight;

    let (h_val, s, v) = rgb_to_hsv(avg_r, avg_g, avg_b);
    let norm_s = s.clamp(0.52, 0.82);
    let norm_v = v.clamp(0.68, 0.88);
    let (final_r, final_g, final_b) = hsv_to_rgb(h_val, norm_s, norm_v);

    slint::Color::from_rgb_u8(final_r, final_g, final_b)
}

pub fn extract_vibrant_accent_from_bytes(bytes: &[u8]) -> Option<slint::Color> {
    image::load_from_memory(bytes).ok().map(|img| extract_vibrant_accent_from_image(&img))
}

pub fn extract_vibrant_accent_from_path(path: &std::path::Path) -> Option<slint::Color> {
    image::open(path).ok().map(|img| extract_vibrant_accent_from_image(&img))
}

#[cfg(target_os = "windows")]
fn set_clipboard_text(text: &str) -> bool {
    use std::os::raw::c_void;
    type HANDLE = *mut c_void;
    type HGLOBAL = *mut c_void;
    type HWND = *mut c_void;
    type UINT = u32;
    type BOOL = i32;

    const CF_UNICODETEXT: UINT = 13;
    const GMEM_MOVEABLE: UINT = 0x0002;

    extern "system" {
        fn OpenClipboard(hwnd: HWND) -> BOOL;
        fn CloseClipboard() -> BOOL;
        fn EmptyClipboard() -> BOOL;
        fn SetClipboardData(uFormat: UINT, hMem: HANDLE) -> HANDLE;
        fn GlobalAlloc(uFlags: UINT, dwBytes: usize) -> HGLOBAL;
        fn GlobalFree(hMem: HGLOBAL) -> HGLOBAL;
        fn GlobalLock(hMem: HGLOBAL) -> *mut c_void;
        fn GlobalUnlock(hMem: HGLOBAL) -> BOOL;
    }

    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let size = utf16.len() * std::mem::size_of::<u16>();

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let mut ok = false;
        let h_mem = GlobalAlloc(GMEM_MOVEABLE, size);
        if !h_mem.is_null() {
            let ptr = GlobalLock(h_mem) as *mut u16;
            if !ptr.is_null() {
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr, utf16.len());
                GlobalUnlock(h_mem);
                if !SetClipboardData(CF_UNICODETEXT, h_mem).is_null() {
                    ok = true;
                } else {
                    GlobalFree(h_mem);
                }
            } else {
                GlobalFree(h_mem);
            }
        }
        CloseClipboard();
        ok
    }
}

#[cfg(not(target_os = "windows"))]
fn set_clipboard_text(_text: &str) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn get_clipboard_text() -> Option<String> {
    use std::os::raw::c_void;
    type HANDLE = *mut c_void;
    type HGLOBAL = *mut c_void;
    type HWND = *mut c_void;
    type UINT = u32;
    type BOOL = i32;

    const CF_UNICODETEXT: UINT = 13;

    extern "system" {
        fn OpenClipboard(hwnd: HWND) -> BOOL;
        fn CloseClipboard() -> BOOL;
        fn GetClipboardData(uFormat: UINT) -> HANDLE;
        fn GlobalLock(hMem: HGLOBAL) -> *mut c_void;
        fn GlobalSize(hMem: HGLOBAL) -> usize;
        fn GlobalUnlock(hMem: HGLOBAL) -> BOOL;
    }

    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let handle = GetClipboardData(CF_UNICODETEXT);
        let mut result = None;
        if !handle.is_null() {
            let max_u16 = GlobalSize(handle) / std::mem::size_of::<u16>();
            let ptr = GlobalLock(handle) as *const u16;
            if !ptr.is_null() && max_u16 > 0 {
                let mut len = 0;
                while len < max_u16 && *ptr.add(len) != 0 {
                    len += 1;
                }
                let slice = std::slice::from_raw_parts(ptr, len);
                result = String::from_utf16(slice).ok();
                GlobalUnlock(handle);
            }
        }
        CloseClipboard();
        result
    }
}

#[cfg(not(target_os = "windows"))]
fn get_clipboard_text() -> Option<String> {
    None
}

fn now_playing_to_party_track(np: &core::playback::NowPlaying, added_by: &str) -> crate::core::party::PartyTrack {
    let thumbnail_url = if !np.video_id.is_empty() && np.video_id != "native-prototype" {
        format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", np.video_id)
    } else {
        String::new()
    };
    crate::core::party::PartyTrack {
        video_id: np.video_id.clone(),
        title: np.title.clone(),
        artist: np.artist.clone(),
        album: np.album.clone(),
        album_id: np.album_id.clone(),
        artist_id: np.artist_id.clone(),
        duration_secs: np.duration_secs,
        thumbnail_url,
        added_by: added_by.to_string(),
    }
}

fn build_party_seed_from_playback(playback: &core::playback::PlaybackCore, local_name: &str) -> crate::core::party::PartySeedState {
    let full_queue = playback.full_queue();
    let queue: Vec<crate::core::party::PartyTrack> = full_queue.iter()
        .filter(|np| !np.video_id.is_empty() && np.video_id != "native-prototype")
        .map(|np| now_playing_to_party_track(np, local_name))
        .collect();

    let np = playback.now_playing();
    let current_track = if !np.video_id.is_empty() && np.video_id != "native-prototype" {
        Some(now_playing_to_party_track(&np, local_name))
    } else {
        None
    };

    let is_playing = playback.is_playing();
    let position_ms = (playback.elapsed_secs() * 1000.0) as u64;
    let repeat_mode = playback.repeat_mode().as_str().to_string();
    let current_index = playback.queue_index();

    crate::core::party::PartySeedState {
        queue,
        current_index,
        current_track,
        is_playing,
        position_ms,
        repeat_mode,
    }
}

fn refresh_party_ui(ui: &NativeShellWindow) {
    let snap = crate::core::party::party_manager().snapshot();
    ui.set_party_status(SharedString::from(snap.status.as_str()));
    ui.set_party_active(snap.is_active);
    ui.set_party_is_host(snap.is_host);
    ui.set_party_code(SharedString::from(snap.party_code.as_str()));
    ui.set_party_invite_link(SharedString::from(snap.invite_link.as_str()));
    ui.set_party_host_name(SharedString::from(snap.host_name.as_str()));
    ui.set_party_member_count(snap.participant_count as i32);
    ui.set_party_rtt_ms(snap.rtt_ms as i32);
    ui.set_party_activity_text(SharedString::from(snap.last_action_desc.as_str()));
    ui.set_party_error_text(SharedString::from(snap.error_text.as_str()));

    let members: Vec<PartyMemberItem> = snap.participants.iter().map(|p| {
        let initial = p.name.chars().next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());
        PartyMemberItem {
            name: SharedString::from(p.name.as_str()),
            platform: SharedString::from(p.platform.as_str()),
            is_host: p.is_host,
            initial: SharedString::from(initial.as_str()),
        }
    }).collect();
    ui.set_party_members(ModelRc::new(VecModel::from(members)));
}

fn apply_pending_party_updates(ui: &NativeShellWindow, playback: &'static core::playback::PlaybackCore) {
    let upd = crate::core::party::party_manager().poll_remote_update();
    if upd.has_update {
        if let Some(pstate) = upd.state {
            let queue_np: Vec<core::playback::NowPlaying> = pstate.queue.iter().map(|t| {
                core::playback::NowPlaying {
                    video_id: t.video_id.clone(),
                    title: t.title.clone(),
                    artist: t.artist.clone(),
                    album: t.album.clone(),
                    album_id: t.album_id.clone(),
                    artist_id: t.artist_id.clone(),
                    duration_secs: t.duration_secs,
                }
            }).collect();

            let target_repeat = match pstate.repeat_mode.as_str() {
                "all" => core::playback::RepeatMode::All,
                "one" => core::playback::RepeatMode::One,
                _ => core::playback::RepeatMode::Off,
            };

            let cur_np = playback.now_playing();
            let new_track_opt = pstate.current_track.as_ref();
            let new_vid = new_track_opt.map(|t| t.video_id.as_str()).unwrap_or("");

            if !new_vid.is_empty() && new_vid != cur_np.video_id {
                let new_track = new_track_opt.unwrap();
                {
                    let mut state = playback.state_lock();
                    state.queue = queue_np;
                    state.queue_index = pstate.current_index.min(state.queue.len().saturating_sub(1));
                    state.is_playing = pstate.is_playing;
                    state.repeat_mode = target_repeat;
                }
                playback.set_now_playing(&new_track.video_id, &new_track.title, &new_track.artist, new_track.duration_secs);
                {
                    let mut state = playback.state_lock();
                    state.queue_index = pstate.current_index.min(state.queue.len().saturating_sub(1));
                    state.is_playing = pstate.is_playing;
                }
                if upd.target_position_ms > 500 {
                    playback.seek_to_secs(upd.target_position_ms as f64 / 1000.0);
                }
                now_playing_ui::notify_track_changed(
                    ui.as_weak(),
                    new_track.video_id.clone(),
                    new_track.title.clone(),
                    new_track.artist.clone(),
                    new_track.duration_secs,
                );
            } else {
                {
                    let mut state = playback.state_lock();
                    state.queue = queue_np;
                    state.queue_index = pstate.current_index.min(state.queue.len().saturating_sub(1));
                    state.repeat_mode = target_repeat;
                }
                if playback.is_playing() != pstate.is_playing {
                    playback.set_playing(pstate.is_playing);
                }
                let local_ms = (playback.elapsed_secs() * 1000.0) as i64;
                let delta = (local_ms - upd.target_position_ms as i64).abs();
                if (!playback.is_audio_loading() || !pstate.is_playing) && delta > 600 {
                    playback.seek_to_secs(upd.target_position_ms as f64 / 1000.0);
                }
            }
            refresh_native_shell_ui(ui, playback);
        }
    }
}

pub fn run_native_shell() -> Result<(), slint::PlatformError> {
    // Unify storage: fold any legacy `ytm-native` data into `auricle` before
    // anything reads settings or user data.
    core::persistence::migrate_legacy_dir();
    log::info!("[startup] data folder ready");

    // Keep yt-dlp current in the background. A stale yt-dlp still resolves a CDN
    // URL but that URL gets 403'd past the first ~1 MiB, which shows up as every
    // song failing to play. Checked at most weekly; silent and non-blocking.
    core::addons::spawn_ytdlp_auto_update();

    // One-time sweep of thumbnails cached at their source resolution by older
    // builds. Done before any thumbnail loader starts so nothing races it.
    purge_oversized_thumb_cache();

    let playback = core::bridge::playback_core();
    log::info!("[startup] playback core ready");

    if let Err(err) = playback.enable_audio_output() {
        log::error!("Failed to enable native audio output: {err}");
    } else {
        log::info!("[startup] audio output ready");
    }

    // Seed queue in background thread, update UI when done
    {
        let ui_handle_seed: Option<slint::Weak<NativeShellWindow>> = None;
        let _ = ui_handle_seed; // will be set after ui creation
    }

    log::info!("[startup] creating main window");
    let ui = NativeShellWindow::new()?;
    log::info!("[startup] main window created");

    // Apply Appearance before the window is shown so startup never flashes the
    // default palette. Unknown/legacy values normalize safely.
    {
        let settings = core::persistence::load_settings();
        let name = normalize_theme_name(&settings.theme_name);
        let mode = normalize_theme_mode(&settings.theme_mode);
        ui.set_theme_name(SharedString::from(name));
        ui.set_theme_mode(SharedString::from(mode));
        apply_theme_with_mode(&ui, name, mode, true);
    }

    // Open at a comfortable desktop size instead of the content's (short) preferred
    // height, so the window starts large and the layout fills it.
    ui.window().set_size(slint::LogicalSize::new(1200.0, 760.0));

    // Load liked songs immediately on startup
    {
        let liked_items: Vec<SongItem> = playback.get_liked_songs().iter().map(make_song_item).collect();
        ui.set_liked_songs(ModelRc::new(VecModel::from(liked_items)));
        maybe_seed_home_hero(&ui, false);
    }

    {
        let np = playback.now_playing();
        if !np.video_id.is_empty() {
            now_playing_ui::notify_track_changed(
                ui.as_weak(),
                np.video_id,
                np.title,
                np.artist,
                np.duration_secs,
            );
        }
    }

    // ── Auricle Party setup & deep-link ──────────────────────────────────────
    {
        let display_name = {
            let settings = core::persistence::load_settings();
            if !settings.party_display_name.is_empty() {
                settings.party_display_name
            } else {
                "Desktop Listener".to_string()
            }
        };
        ui.set_party_display_name(SharedString::from(display_name.as_str()));

        // Wakeup callback for party background thread events
        {
            let ui_w = ui.as_weak();
            crate::core::party::party_manager().set_wakeup_callback(Box::new(move || {
                let ui_w2 = ui_w.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w2.upgrade() {
                        let playback = crate::core::bridge::playback_core();
                        apply_pending_party_updates(&ui, playback);
                        refresh_party_ui(&ui);
                    }
                }).ok();
            }));
        }

        for arg in std::env::args().skip(1) {
            if arg.contains("auricle://party/") {
                ui.set_party_join_input(SharedString::from(arg.trim()));
                ui.set_show_party_modal(true);
                refresh_party_ui(&ui);
                break;
            }
        }
    }

    // ── Auricle Party callbacks ───────────────────────────────────────────────
    {
        ui.on_save_party_display_name(move |name| {
            let trimmed = name.trim().to_string();
            let mut settings = core::persistence::load_settings();
            settings.party_display_name = trimmed;
            core::persistence::save_settings(&settings);
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_start_party(move |display_name| {
            let name = if !display_name.trim().is_empty() {
                display_name.trim().to_string()
            } else {
                let settings = core::persistence::load_settings();
                if !settings.party_display_name.is_empty() {
                    settings.party_display_name
                } else {
                    std::env::var("USERNAME").unwrap_or_else(|_| "Windows PC".to_string())
                }
            };
            let playback = crate::core::bridge::playback_core();
            let seed = build_party_seed_from_playback(playback, &name);
            crate::core::party::party_manager().start_party(name, seed);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_party_link_copied(false);
                refresh_party_ui(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_join_party(move |invite_input, display_name| {
            let name = if !display_name.trim().is_empty() {
                display_name.trim().to_string()
            } else {
                let settings = core::persistence::load_settings();
                if !settings.party_display_name.is_empty() {
                    settings.party_display_name
                } else {
                    std::env::var("USERNAME").unwrap_or_else(|_| "Windows PC".to_string())
                }
            };
            crate::core::party::party_manager().join_party(invite_input.trim().to_string(), name);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_party_link_copied(false);
                refresh_party_ui(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_leave_party(move || {
            crate::core::party::party_manager().leave_party();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_party_link_copied(false);
                refresh_party_ui(&ui);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_copy_party_link(move || {
            let snap = crate::core::party::party_manager().snapshot();
            if !snap.invite_link.is_empty() {
                set_clipboard_text(&snap.invite_link);
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_party_link_copied(true);
                }
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_paste_party_link(move || {
            if let Some(text) = get_clipboard_text() {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_party_join_input(SharedString::from(text.trim()));
                }
            }
        });
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

    // ── Prefetch depth (shared live with the background poll loop) ─────────────
    let prefetch_depth: Arc<std::sync::atomic::AtomicU32> = Arc::new(
        std::sync::atomic::AtomicU32::new(
            core::persistence::load_settings().prefetch_depth.clamp(1, 3),
        ),
    );
    {
        let ui_weak = ui.as_weak();
        let prefetch_depth = prefetch_depth.clone();
        ui.on_set_prefetch_depth(move |depth| {
            let depth = depth.clamp(1, 3);
            prefetch_depth.store(depth as u32, std::sync::atomic::Ordering::Relaxed);
            let mut settings = core::persistence::load_settings();
            settings.prefetch_depth = depth as u32;
            core::persistence::save_settings(&settings);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_prefetch_depth(depth);
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
                let dn = crate::core::addons::deno_installed();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_ytdlp_installed(yt);
                        ui.set_ffmpeg_installed(ff);
                        ui.set_deno_installed(dn);
                    }
                }).ok();
            });
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_install_ytdlp(move || {
            let ui_weak2 = ui_weak.clone();
            let ui_weak_prog = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_addon_busy(true);
                ui.set_addon_installing(SharedString::from("ytdlp"));
                ui.set_addon_progress(0.0);
                ui.set_addon_status(SharedString::from("Installing yt-dlp…"));
            }
            std::thread::spawn(move || {
                let last = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(-2));
                let on_progress = {
                    let last = last.clone();
                    move |p: f32| {
                        let permille = (p * 1000.0) as i32;
                        if last.swap(permille, std::sync::atomic::Ordering::Relaxed) != permille {
                            let w = ui_weak_prog.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = w.upgrade() { ui.set_addon_progress(p); }
                            }).ok();
                        }
                    }
                };
                let result = crate::core::addons::install_ytdlp(on_progress);
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_addon_busy(false);
                        ui.set_addon_installing(SharedString::from(""));
                        match result {
                            Ok(_) => {
                                ui.set_ytdlp_installed(true);
                                ui.set_addon_status(SharedString::from("yt-dlp installed."));
                                // A successful install completes onboarding for good.
                                let mut s = crate::core::persistence::load_settings();
                                if !s.onboarding_seen {
                                    s.onboarding_seen = true;
                                    crate::core::persistence::save_settings(&s);
                                }
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
            let ui_weak_prog = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_addon_busy(true);
                ui.set_addon_installing(SharedString::from("ffmpeg"));
                ui.set_addon_progress(0.0);
                ui.set_addon_status(SharedString::from("Installing ffmpeg… this may take a moment."));
            }
            std::thread::spawn(move || {
                let last = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(-2));
                let on_progress = {
                    let last = last.clone();
                    move |p: f32| {
                        let permille = (p * 1000.0) as i32;
                        if last.swap(permille, std::sync::atomic::Ordering::Relaxed) != permille {
                            let w = ui_weak_prog.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = w.upgrade() { ui.set_addon_progress(p); }
                            }).ok();
                        }
                    }
                };
                let result = crate::core::addons::install_ffmpeg(on_progress);
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_addon_busy(false);
                        ui.set_addon_installing(SharedString::from(""));
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
    // ── Update yt-dlp ─────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_update_ytdlp(move || {
            let ui_weak2 = ui_weak.clone();
            let ui_weak_prog = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_addon_busy(true);
                ui.set_addon_installing(SharedString::from("ytdlp-update"));
                ui.set_addon_progress(-1.0);
                ui.set_addon_status(SharedString::from("Updating yt-dlp…"));
            }
            std::thread::spawn(move || {
                let last = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(-2));
                let on_progress = {
                    let last = last.clone();
                    move |p: f32| {
                        let permille = (p * 1000.0) as i32;
                        if last.swap(permille, std::sync::atomic::Ordering::Relaxed) != permille {
                            let w = ui_weak_prog.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = w.upgrade() { ui.set_addon_progress(p); }
                            }).ok();
                        }
                    }
                };
                let result = crate::core::addons::update_ytdlp(on_progress);
                if result.is_ok() {
                    // URLs signed by the previous yt-dlp client are partly blocked
                    // (403 past ~1 MiB); drop them so the next play re-resolves.
                    crate::core::stream_player::clear_url_cache();
                    let mut s = core::persistence::load_settings();
                    s.last_ytdlp_update_check = crate::core::addons::unix_now();
                    core::persistence::save_settings(&s);
                }
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_addon_busy(false);
                        ui.set_addon_installing(SharedString::from(""));
                        match result {
                            Ok(_) => ui.set_addon_status(SharedString::from("yt-dlp updated to latest.")),
                            Err(e) => ui.set_addon_status(SharedString::from(format!("yt-dlp update failed: {}", e))),
                        }
                    }
                }).ok();
            });
        });
    }
    // ── Install Deno ──────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_install_deno(move || {
            let ui_weak2 = ui_weak.clone();
            let ui_weak_prog = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_addon_busy(true);
                ui.set_addon_installing(SharedString::from("deno"));
                ui.set_addon_progress(0.0);
                ui.set_addon_status(SharedString::from("Installing Deno…"));
            }
            std::thread::spawn(move || {
                let last = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(-2));
                let on_progress = {
                    let last = last.clone();
                    move |p: f32| {
                        let permille = (p * 1000.0) as i32;
                        if last.swap(permille, std::sync::atomic::Ordering::Relaxed) != permille {
                            let w = ui_weak_prog.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = w.upgrade() { ui.set_addon_progress(p); }
                            }).ok();
                        }
                    }
                };
                let result = crate::core::addons::install_deno(on_progress);
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_addon_busy(false);
                        ui.set_addon_installing(SharedString::from(""));
                        match result {
                            Ok(_) => {
                                ui.set_deno_installed(true);
                                ui.set_addon_status(SharedString::from("Deno installed. yt-dlp can now handle YouTube signature extraction."));
                            }
                            Err(e) => {
                                ui.set_addon_status(SharedString::from(format!("Deno install failed: {}", e)));
                            }
                        }
                    }
                }).ok();
            });
        });
    }
    // ── Updates (portable build only) ─────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_check_updates(move || {
            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_update_busy(true);
                ui.set_update_status(SharedString::from("Checking for updates…"));
            }
            std::thread::spawn(move || {
                let result = crate::core::updater::check_latest();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_update_busy(false);
                        match result {
                            Ok(c) => {
                                ui.set_update_available(c.update_available);
                                ui.set_update_status(SharedString::from(if c.update_available {
                                    format!("Update available: v{}", c.latest)
                                } else {
                                    "You're up to date.".to_string()
                                }));
                            }
                            Err(e) => ui.set_update_status(SharedString::from(format!("Update check failed: {e}"))),
                        }
                    }
                }).ok();
            });
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_apply_update(move || {
            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_update_busy(true);
                ui.set_update_status(SharedString::from("Downloading update…"));
            }
            std::thread::spawn(move || {
                let result = crate::core::updater::apply_update();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_update_busy(false);
                        match result {
                            Ok(v) => {
                                ui.set_update_available(false);
                                ui.set_update_status(SharedString::from(format!("Updated to v{v}. Restart to apply.")));
                            }
                            Err(e) => ui.set_update_status(SharedString::from(format!("Update failed: {e}"))),
                        }
                    }
                }).ok();
            });
        });
    }

    // Dismiss the first-run onboarding popup (and remember the choice).
    {
        let ui_weak = ui.as_weak();
        ui.on_dismiss_onboarding(move || {
            let mut s = crate::core::persistence::load_settings();
            s.onboarding_seen = true;
            crate::core::persistence::save_settings(&s);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_onboarding(false);
            }
        });
    }
    // Initial add-on detection at startup
    {
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            let yt = crate::core::addons::ytdlp_installed();
            let ff = crate::core::addons::ffmpeg_installed();
            let dn = crate::core::addons::deno_installed();
            let seen = crate::core::persistence::load_settings().onboarding_seen;
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_ytdlp_installed(yt);
                    ui.set_ffmpeg_installed(ff);
                    ui.set_deno_installed(dn);
                    // Show onboarding only on first run when yt-dlp is missing.
                    ui.set_show_onboarding(!seen && !yt);
                }
            }).ok();
        });
    }

    // Version + update capability (portable build self-updates; installer/Store hidden)
    ui.set_app_version(SharedString::from(crate::core::updater::current_version()));
    ui.set_self_update_enabled(crate::core::updater::self_update_supported());
    if crate::core::updater::self_update_supported() {
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            if let Ok(c) = crate::core::updater::check_latest() {
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_update_available(c.update_available);
                        if c.update_available {
                            ui.set_update_status(SharedString::from(format!("Update available: v{}", c.latest)));
                        }
                    }
                }).ok();
            }
        });
    }

    // ── Toggle queue pane ─────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_queue_pane(move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_current_view().as_str() == "NowPlaying" {
                    ui.set_queue_pane_open(false);
                    return;
                }
                ui.set_queue_pane_open(!ui.get_queue_pane_open());
            }
        });
    }

    // ── Remove from queue ─────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_remove_from_queue(move |idx| {
            playback.remove_from_queue(idx as usize);
            if crate::core::party::party_manager().snapshot().is_active {
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::RemoveFromQueue { upcoming_index: idx as usize },
                    false,
                );
            }
            if let Some(ui) = ui_weak.upgrade() {
                let items: Vec<SongItem> = playback.queue_upcoming().iter().map(make_song_item).collect();
                ui.set_queue(ModelRc::new(VecModel::from(items)));
            }
        });
    }

    // ── Explore genre ─────────────────────────────────────────────────────────
    let search_controller = search_ui::wire(&ui);
    {
        let ui_weak = ui.as_weak();
        ui.on_explore_genre(move |genre_query| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_do_search(genre_query);
            }
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
                    use ytmapi_rs::query::GetMoodCategoriesQuery;
                    let api = crate::core::net::ytmusic().await.map_err(|e| e.to_string())?;
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
                    let api = crate::core::net::ytmusic().await.map_err(|e| e.to_string())?;
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
            let Some(ui) = ui_weak.upgrade() else { return; };
            let query = ui.get_explore_moods().iter()
                .find(|mood| mood.params == params_str)
                .map(|mood| mood.title.to_string())
                .unwrap_or_else(|| "Mood".to_string());
            let request = search_controller.begin_external(&ui, query);
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let result = rt.block_on(async {
                    use ytmapi_rs::query::GetMoodPlaylistsQuery;
                    use ytmapi_rs::common::{MoodCategoryParams, YoutubeID};
                    let api = crate::core::net::ytmusic().await.map_err(|e| e.to_string())?;
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
                request.finish(result.map(|tracks| {
                    map_playlist_items(tracks).into_iter().map(|song| core::search::SearchEntry {
                        kind: "Song".to_string(),
                        id: song.video_id,
                        title: song.name,
                        artist: song.artist.name,
                        artist_id: song.artist.browse_id,
                        album_id: song.album.as_ref().map(|album| album.browse_id.clone()).unwrap_or_default(),
                        album: song.album.map(|album| album.name).unwrap_or_default(),
                        duration_secs: song.duration.unwrap_or(0),
                        ..core::search::SearchEntry::default()
                    }).collect()
                }));
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
    // The autoplay/radio queue and seed now live inside PlaybackCore
    // (playback.set_autoplay_queue / pop_autoplay / autoplay_seed / …).

    // Current detail-view descriptors — captured when navigating to an artist/
    // album/playlist so the Follow / Save buttons know what to persist locally.
    let cur_artist: Arc<Mutex<core::library::SavedArtist>> = Arc::new(Mutex::new(Default::default()));
    let cur_album: Arc<Mutex<core::library::SavedAlbum>> = Arc::new(Mutex::new(Default::default()));
    let cur_playlist: Arc<Mutex<core::library::SavedPlaylist>> = Arc::new(Mutex::new(Default::default()));
    context_ui::wire(&ui, cur_artist.clone(), cur_album.clone());

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

    {
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        let nav_restoring = nav_restoring.clone();
        discography_ui::wire(&ui, cur_artist.clone(), move |ui, artist_id| {
            if !nav_restoring.load(std::sync::atomic::Ordering::Relaxed) {
                push_nav_entry(&nav_history, &nav_cursor, "Discography".to_string(), artist_id.to_string());
                let hist = nav_history.lock().unwrap();
                let cur = *nav_cursor.lock().unwrap();
                update_nav_buttons(ui, &hist, cur);
            }
        });
    }
    now_playing_ui::wire(&ui);

    // ── Navigate ──────────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        ui.on_navigate(move |view| {
            if let Some(ui) = ui_weak.upgrade() {
                if view == "Discography" {
                    let artist_id = ui.get_discography_artist_id();
                    if !artist_id.is_empty() { ui.invoke_navigate_to_discography(artist_id); }
                    return;
                }
                context_ui::invalidate(&ui);
                ui.set_current_view(view.clone());
                push_nav_entry(&nav_history, &nav_cursor, view.to_string(), String::new());
                let hist = nav_history.lock().unwrap();
                let cur = *nav_cursor.lock().unwrap();
                update_nav_buttons(&ui, &hist, cur);
                if view.as_str() == "NowPlaying" {
                    ui.set_queue_pane_open(false);
                    let playback = crate::core::bridge::playback_core();
                    let np = playback.now_playing();
                    if !np.video_id.is_empty() {
                        now_playing_ui::notify_track_changed(
                            ui.as_weak(),
                            np.video_id,
                            np.title,
                            np.artist,
                            np.duration_secs,
                        );
                    }
                }
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
                    context_ui::invalidate(&ui);
                    ui.set_current_view(SharedString::from(entry.view.as_str()));
                    update_nav_buttons(&ui, &hist, *cur);
                    if entry.view == "Artist" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_artist(SharedString::from(entry.context_id.as_str()));
                        nav_restoring.store(false, std::sync::atomic::Ordering::Relaxed);
                    } else if entry.view == "Discography" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_discography(SharedString::from(entry.context_id.as_str()));
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
                    } else if entry.view == "NowPlaying" {
                        ui.set_queue_pane_open(false);
                        let playback = crate::core::bridge::playback_core();
                        let np = playback.now_playing();
                        if !np.video_id.is_empty() {
                            now_playing_ui::notify_track_changed(
                                ui.as_weak(),
                                np.video_id,
                                np.title,
                                np.artist,
                                np.duration_secs,
                            );
                        }
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
                    context_ui::invalidate(&ui);
                    ui.set_current_view(SharedString::from(entry.view.as_str()));
                    update_nav_buttons(&ui, &hist, *cur);
                    if entry.view == "Artist" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_artist(SharedString::from(entry.context_id.as_str()));
                        nav_restoring.store(false, std::sync::atomic::Ordering::Relaxed);
                    } else if entry.view == "Discography" && !entry.context_id.is_empty() {
                        nav_restoring.store(true, std::sync::atomic::Ordering::Relaxed);
                        drop(cur);
                        drop(hist);
                        ui.invoke_navigate_to_discography(SharedString::from(entry.context_id.as_str()));
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
                    } else if entry.view == "NowPlaying" {
                        ui.set_queue_pane_open(false);
                        let playback = crate::core::bridge::playback_core();
                        let np = playback.now_playing();
                        if !np.video_id.is_empty() {
                            now_playing_ui::notify_track_changed(
                                ui.as_weak(),
                                np.video_id,
                                np.title,
                                np.artist,
                                np.duration_secs,
                            );
                        }
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
            if crate::core::party::party_manager().snapshot().is_active {
                let position_ms = (playback.elapsed_secs() * 1000.0) as u64;
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::PlayPause { is_playing, position_ms },
                    false,
                );
            }
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
            if crate::core::party::party_manager().snapshot().is_active {
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::Prev,
                    false,
                );
            }
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
            if crate::core::party::party_manager().snapshot().is_active {
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::Next,
                    false,
                );
            }
            if let Some(ui) = ui_weak.upgrade() {
                refresh_native_shell_ui(&ui, playback);
            }
        });
    }

    // ── Play specific song ────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_play_song(move |video_id, title, artist, duration_secs| {
            if playback.is_currently_active_track(video_id.as_str()) {
                return;
            }
            // set_now_playing calls sync_audio_playback internally.
            // We set is_playing FIRST so sync_audio_playback actually starts audio.
            {
                let mut state = playback.state_lock();
                state.is_playing = true;
            }
            playback.set_now_playing(video_id.as_str(), title.as_str(), artist.as_str(), duration_secs as u32);
            if crate::core::party::party_manager().snapshot().is_active {
                let track = crate::core::party::PartyTrack {
                    video_id: video_id.to_string(),
                    title: title.to_string(),
                    artist: artist.to_string(),
                    duration_secs: duration_secs as u32,
                    thumbnail_url: format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", video_id),
                    ..Default::default()
                };
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::PlayTrackNow { track },
                    true,
                );
            }
            if let Some(ui) = ui_weak.upgrade() {
                refresh_native_shell_ui(&ui, playback);
            }
            // Update autoplay seed (this is an "external play") + reset radio queue
            playback.set_autoplay_seed(video_id.as_str());
            playback.clear_autoplay_queue();
            let ui_w = ui_weak.clone();
            let vid = video_id.to_string();
            std::thread::spawn(move || {
                fetch_autoplay_queue(ui_w, vid, false);
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
            let applied = playback.seek_to_secs(target_secs);
            if crate::core::party::party_manager().snapshot().is_active {
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::Seek { position_ms: (target_secs * 1000.0) as u64 },
                    false,
                );
            }
            // Only snap the UI when the audio actually moved. While streaming and
            // not yet cached, an arbitrary seek can't be honored — leave the bar
            // where playback really is so the timeline can't desync.
            if applied {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_progress(fraction as f32);
                    let m = target_secs as u64 / 60;
                    let s = target_secs as u64 % 60;
                    ui.set_current_time(SharedString::from(format!("{m}:{s:02}")));
                }
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
            if crate::core::party::party_manager().snapshot().is_active {
                let track = crate::core::party::PartyTrack {
                    video_id: video_id.to_string(),
                    title: title.to_string(),
                    artist: artist.to_string(),
                    duration_secs: dur,
                    thumbnail_url: format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", video_id),
                    ..Default::default()
                };
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::AddToQueue { track, play_next: false },
                    false,
                );
            }
            if let Some(ui) = ui_weak.upgrade() {
                let items: Vec<SongItem> = playback.queue_upcoming().iter().map(make_song_item).collect();
                ui.set_queue(ModelRc::new(VecModel::from(items)));
            }
        });
    }

    // ── Play next (insert at top of queue) ───────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_play_next_song(move |video_id, title, artist, duration_secs| {
            playback.play_next(video_id.as_str(), title.as_str(), artist.as_str(), duration_secs as u32);
            // Also remove from autoplay queue if present
            playback.autoplay_remove_id(video_id.as_str());
            if crate::core::party::party_manager().snapshot().is_active {
                let track = crate::core::party::PartyTrack {
                    video_id: video_id.to_string(),
                    title: title.to_string(),
                    artist: artist.to_string(),
                    duration_secs: duration_secs as u32,
                    thumbnail_url: format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", video_id),
                    ..Default::default()
                };
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::AddToQueue { track, play_next: true },
                    false,
                );
            }
            if let Some(ui) = ui_weak.upgrade() {
                let items: Vec<SongItem> = playback.queue_upcoming().iter().map(make_song_item).collect();
                ui.set_queue(ModelRc::new(VecModel::from(items)));
                let ap_items: Vec<SongItem> = playback.autoplay_queue().iter().map(make_song_item).collect();
                ui.set_autoplay_queue(ModelRc::new(VecModel::from(ap_items)));
            }
        });
    }

    // ── Remove from autoplay queue ───────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_remove_from_autoplay(move |index| {
            let songs = playback.remove_autoplay(index as usize);
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
        ui.on_play_autoplay_song(move |index| {
            if let Some(song) = playback.play_autoplay_at_preserving_queues(index as usize) {
                if crate::core::party::party_manager().snapshot().is_active {
                    let track = now_playing_to_party_track(&song, "");
                    crate::core::party::party_manager().send_action(
                        crate::core::party::PartyAction::PlayTrackNow { track },
                        false,
                    );
                }
                if let Some(ui) = ui_weak.upgrade() {
                    refresh_native_shell_ui(&ui, playback);
                    let items: Vec<SongItem> = playback.autoplay_queue().iter().map(make_song_item).collect();
                    ui.set_autoplay_queue(ModelRc::new(VecModel::from(items)));
                    now_playing_ui::notify_track_changed(
                        ui.as_weak(),
                        song.video_id.clone(),
                        song.title.clone(),
                        song.artist.clone(),
                        song.duration_secs,
                    );
                }
            }
        });
    }

    // ── Shuffle queue ────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_shuffle_queue(move || {
            let playback = crate::core::bridge::playback_core();
            if playback.shuffle_upcoming_user_queue() {
                if crate::core::party::party_manager().snapshot().is_active {
                    crate::core::party::party_manager().send_action(
                        crate::core::party::PartyAction::ShuffleUpcoming,
                        true,
                    );
                }
                if let Some(ui) = ui_weak.upgrade() {
                    refresh_native_shell_ui(&ui, playback);
                }
            }
        });
    }

    // ── Cycle repeat mode ────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_cycle_repeat(move || {
            let playback = crate::core::bridge::playback_core();
            let mode = playback.cycle_repeat_mode();
            if crate::core::party::party_manager().snapshot().is_active {
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::SetRepeatMode { mode: mode.as_str().to_string() },
                    false,
                );
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_repeat_mode(SharedString::from(mode.as_str()));
            }
        });
    }

    // ── Open Now Playing view ────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        ui.on_open_now_playing(move || {
            let Some(ui) = ui_weak.upgrade() else { return; };
            if ui.get_now_playing_video_id().as_str().is_empty() {
                return;
            }
            if ui.get_current_view().as_str() == "NowPlaying" {
                if ui.get_can_go_back() {
                    ui.invoke_nav_back();
                } else {
                    ui.set_current_view(SharedString::from("Home"));
                }
            } else {
                ui.set_is_loading(false);
                ui.set_queue_pane_open(false);
                context_ui::invalidate(&ui);
                push_nav_entry(&nav_history, &nav_cursor, "NowPlaying".to_string(), String::new());
                let hist = nav_history.lock().unwrap();
                let cur = *nav_cursor.lock().unwrap();
                update_nav_buttons(&ui, &hist, cur);
                ui.set_current_view(SharedString::from("NowPlaying"));
                let playback = crate::core::bridge::playback_core();
                let np = playback.now_playing();
                now_playing_ui::notify_track_changed(
                    ui.as_weak(),
                    np.video_id,
                    np.title,
                    np.artist,
                    np.duration_secs,
                );
            }
        });
    }

    // ── Refresh hero pick ────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_refresh_hero_pick(move || {
            if let Some(ui) = ui_weak.upgrade() {
                maybe_seed_home_hero(&ui, true);
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
                        if name.starts_with("ytm_thumb_") {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
            // Clear audio cache at %LOCALAPPDATA%\auricle\cache\
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
        ui.set_prefetch_depth(settings.prefetch_depth.clamp(1, 3) as i32);
    }
    {
        ui.on_set_minimize_to_tray(move |val| {
            let mut settings = core::persistence::load_settings();
            settings.minimize_to_tray = val;
            core::persistence::save_settings(&settings);
        });
    }
    // ── Privacy & diagnostics settings ───────────────────────────────────────
    ui.set_use_browser_cookies(core::persistence::load_settings().use_browser_cookies);
    ui.on_set_browser_cookies(move |enabled| {
        let mut settings = core::persistence::load_settings();
        settings.use_browser_cookies = enabled;
        core::persistence::save_settings(&settings);
        if !enabled {
            // Stream links resolved with cookies are dropped along with the consent.
            core::stream_player::revoke_browser_cookies();
        }
        log::info!("[privacy] browser cookies {}", if enabled { "enabled by user" } else { "disabled by user" });
    });
    ui.on_open_log_folder(core::diagnostics::open_log_folder);
    ui.on_report_problem(|| core::diagnostics::open_report(""));
    // ── Appearance settings ──────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_set_theme_name(move |requested| {
            let name = normalize_theme_name(requested.as_str()).to_string();
            let mut settings = core::persistence::load_settings();
            settings.theme_name = name.clone();
            let mode = normalize_theme_mode(&settings.theme_mode).to_string();
            core::persistence::save_settings(&settings);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_theme_name(SharedString::from(name.as_str()));
                ui.set_theme_mode(SharedString::from(mode.as_str()));
                apply_theme(&ui, &name, &mode);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_set_theme_mode(move |requested| {
            let mode = normalize_theme_mode(requested.as_str()).to_string();
            let mut settings = core::persistence::load_settings();
            settings.theme_mode = mode.clone();
            let name = normalize_theme_name(&settings.theme_name).to_string();
            core::persistence::save_settings(&settings);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_theme_name(SharedString::from(name.as_str()));
                ui.set_theme_mode(SharedString::from(mode.as_str()));
                apply_theme(&ui, &name, &mode);
            }
        });
    }

    // ── Navigate to Artist ─────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let nav_history = nav_history.clone();
        let nav_cursor = nav_cursor.clone();
        let nav_restoring = nav_restoring.clone();
        let cur_artist = cur_artist.clone();
        ui.on_navigate_to_artist(move |browse_id| {
            let browse_id = browse_id.to_string();
            if browse_id.trim().is_empty() { return; }
            if let Some(ui) = ui_weak.upgrade() { context_ui::invalidate(&ui); }

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
                ui.set_artist_row_sorting(false);
                ui.set_artist_row_fallback(ModelRc::new(VecModel::from(Vec::<AlbumItem>::new())));
                discography_ui::cancel_artist_row();
            }

            let cur_artist = cur_artist.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let result = rt.block_on(async {
                    use ytmapi_rs::common::ArtistChannelID;

                    let api = crate::core::net::ytmusic().await.map_err(|e| e.to_string())?;
                    api.get_artist(ArtistChannelID::from_raw(&browse_id)).await.map_err(|e| e.to_string())
                });

                let ui_weak3 = ui_weak2.clone();
                let browse_id_c = browse_id.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_is_loading(false);
                        match result {
                            Ok(artist) => {
                                ui.set_artist_view_name(SharedString::from(artist.name.as_str()));
                                ui.set_artist_view_has_thumbnail(false);
                                let art_thumb = pick_thumb(&artist.thumbnails).map(|t| t.url.clone()).unwrap_or_default();
                                context_ui::register_entity("Artist", &browse_id_c, &artist.name, "", &art_thumb);
                                core::menu_metadata::register_artists("Artist", &browse_id_c, vec![core::menu_metadata::ArtistCredit {
                                    name: artist.name.clone(), browse_id: browse_id_c.clone(),
                                }]);
                                for release in artist.top_releases.albums.iter().chain(artist.top_releases.singles.iter())
                                    .flat_map(|section| section.results.iter()) {
                                    context_ui::register_entity("Album", release.album_id.get_raw(), &release.title, &artist.name,
                                        pick_thumb(&release.thumbnails).map(|thumb| thumb.url.as_str()).unwrap_or(""));
                                }
                                if let Ok(mut c) = cur_artist.lock() {
                                    *c = core::library::SavedArtist {
                                        browse_id: browse_id_c.clone(),
                                        name: artist.name.clone(),
                                        thumbnail_url: art_thumb.clone(),
                                    };
                                }
                                core::library::set_artist_thumbnail(&browse_id_c, &art_thumb);
                                ui.set_artist_view_subscribed(core::library::is_artist_followed(&browse_id_c));
                                ui.set_artist_view_description(SharedString::from(
                                    clean_artist_description(artist.description.as_deref().unwrap_or(""))
                                        .as_str()
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
                                        let credits = context_ui::credits(&s.artists);
                                        let artist_names = context_ui::credit_names(&credits);
                                        core::playback::register_song_meta(
                                            vid, s.album.name.as_str(),
                                            ytmapi_rs::common::YoutubeID::get_raw(&s.album.id),
                                            credits.first().map(|artist| artist.browse_id.as_str()).unwrap_or(""));
                                        core::menu_metadata::register_artists("Song", vid, credits);
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
                                            artist: SharedString::from(artist_names.as_str()),
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

                                // Convert releases (albums + singles in native order)
                                let combined_releases: Vec<(String, (AlbumItem, Option<String>))> = artist.top_releases.albums.iter()
                                    .chain(artist.top_releases.singles.iter())
                                    .flat_map(|section| section.results.iter())
                                    .map(|a| {
                                        let browse_id = a.album_id.get_raw().to_string();
                                        let thumb_url = pick_thumb(&a.thumbnails).map(|t| t.url.clone());
                                        let item = AlbumItem {
                                            title: SharedString::from(a.title.as_str()),
                                            browse_id: SharedString::from(browse_id.as_str()),
                                            artist: SharedString::from(artist.name.as_str()),
                                            year: SharedString::from(a.year.as_str()),
                                            thumbnail: Default::default(),
                                            has_thumbnail: false,
                                        };
                                        (browse_id, (item, thumb_url))
                                    })
                                    .collect();
                                let mut album_thumb_urls: Vec<(String, String)> = vec![];
                                let albums: Vec<AlbumItem> = discography_ui::filter_preview_releases(combined_releases)
                                    .into_iter()
                                    .map(|(item, thumb_url)| {
                                        if let Some(url) = thumb_url {
                                            album_thumb_urls.push((item.browse_id.to_string(), url));
                                        }
                                        item
                                    })
                                    .collect();
                                ui.set_artist_row_sorting(false);
                                ui.set_artist_view_albums(ModelRc::new(VecModel::from(albums)));
                                ui.set_artist_view_singles(ModelRc::new(VecModel::from(Vec::<AlbumItem>::new())));

                                // Convert videos
                                let mut video_thumb_urls: Vec<(usize, String)> = vec![];
                                let videos: Vec<SongItem> = artist.top_releases.videos.iter()
                                    .flat_map(|section| section.results.iter())
                                    .filter_map(|v| {
                                        use ytmapi_rs::parse::SearchResultVideo;
                                        match v {
                                            SearchResultVideo::Video { title, video_id, views, thumbnails, channel_name, .. } => {
                                                Some((title.clone(), video_id.get_raw().to_string(), views.clone(), pick_thumb(&thumbnails).map(|t| t.url.clone()), channel_name.clone()))
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
                                        .find_map(|a| pick_thumb(&a.thumbnails).map(|t| t.url.clone()))
                                } else { None };
                                if let Some(l) = latest {
                                    ui.set_artist_view_latest_release(l);
                                    ui.set_artist_view_has_latest(true);
                                } else {
                                    ui.set_artist_view_has_latest(false);
                                }

                                // Fetch artist thumbnail
                                let ui_w_albums = ui_weak3.clone();
                                let ui_w_videos = ui_weak3.clone();
                                let ui_w_related = ui_weak3.clone();
                                let ui_w_latest = ui_weak3.clone();
                                if let Some(thumb) = pick_thumb(&artist.thumbnails) {
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
                                    let thumb_path = |id: &str| {
                                        let safe: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
                                        std::env::temp_dir().join(format!("ytm_artist_release_{safe}.jpg"))
                                    };
                                    std::thread::spawn(move || {
                                        if let Ok(client) = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5)).build() {
                                            let mut loaded: Vec<String> = Vec::new();
                                            for (id, url) in &album_thumb_urls {
                                                let path = thumb_path(id);
                                                if !path.exists() {
                                                    let Ok(resp) = client.get(url).send() else { continue };
                                                    if !resp.status().is_success() { continue; }
                                                    let Ok(bytes) = resp.bytes() else { continue };
                                                    if std::fs::write(&path, &bytes).is_err() { continue; }
                                                }
                                                loaded.push(id.clone());
                                            }
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_w.upgrade() {
                                                    let model = ui.get_artist_view_albums();
                                                    for row in 0..model.row_count() {
                                                        let Some(mut item) = model.row_data(row) else { continue };
                                                        if item.has_thumbnail || !loaded.iter().any(|id| id.as_str() == item.browse_id.as_str()) { continue; }
                                                        if let Ok(img) = slint::Image::load_from_path(&thumb_path(item.browse_id.as_str())) {
                                                            item.thumbnail = img;
                                                            item.has_thumbnail = true;
                                                            model.set_row_data(row, item);
                                                        }
                                                    }
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
                context_ui::invalidate(&ui);
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
                    use ytmapi_rs::common::VideoID;
                    let api = crate::core::net::ytmusic().await.ok()?;
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
                    .map(|item| core::playback::NowPlaying::enriched(
                        item.video_id.to_string(),
                        item.title.to_string(),
                        item.artist.to_string(),
                        item.duration_secs as u32,
                    ))
                    .collect();
                if !songs.is_empty() {
                    let playback = crate::core::bridge::playback_core();
                    playback.set_queue(songs.clone());
                    {
                        let mut state = playback.state_lock();
                        state.is_playing = true;
                    }
                    let first = playback.now_playing();
                    playback.set_now_playing(&first.video_id, &first.title, &first.artist, first.duration_secs);
                    if crate::core::party::party_manager().snapshot().is_active {
                        let tracks: Vec<crate::core::party::PartyTrack> = songs.iter().map(|s| now_playing_to_party_track(s, "")).collect();
                        crate::core::party::party_manager().send_action(
                            crate::core::party::PartyAction::ReplaceQueue { tracks, start_index: 0, is_playing: true },
                            false,
                        );
                    }
                    refresh_native_shell_ui(&ui, playback);
                }
            }
        });
    }

    // ── Shuffle Album ────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_shuffle_album(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let model = ui.get_album_view_songs();
                let mut songs: Vec<core::playback::NowPlaying> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .map(|item| core::playback::NowPlaying::enriched(
                        item.video_id.to_string(),
                        item.title.to_string(),
                        item.artist.to_string(),
                        item.duration_secs as u32,
                    ))
                    .collect();
                if !songs.is_empty() {
                    shuffle_vec(&mut songs);
                    let playback = crate::core::bridge::playback_core();
                    playback.set_queue(songs.clone());
                    {
                        let mut state = playback.state_lock();
                        state.is_playing = true;
                    }
                    let first = playback.now_playing();
                    playback.set_now_playing(&first.video_id, &first.title, &first.artist, first.duration_secs);
                    if crate::core::party::party_manager().snapshot().is_active {
                        let tracks: Vec<crate::core::party::PartyTrack> = songs.iter().map(|s| now_playing_to_party_track(s, "")).collect();
                        crate::core::party::party_manager().send_action(
                            crate::core::party::PartyAction::ReplaceQueue { tracks, start_index: 0, is_playing: true },
                            false,
                        );
                    }
                    refresh_native_shell_ui(&ui, playback);
                }
            }
        });
    }

    // ── Toggle Like Album (persist to local library) ─────────────────────────
    {
        let ui_weak = ui.as_weak();
        let cur_album = cur_album.clone();
        ui.on_toggle_like_album(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let album = cur_album.lock().map(|a| a.clone()).unwrap_or_default();
                if album.browse_id.is_empty() {
                    // No descriptor yet — fall back to a plain UI toggle.
                    ui.set_album_view_liked(!ui.get_album_view_liked());
                    return;
                }
                let now_saved = core::library::toggle_save_album(album);
                ui.set_album_view_liked(now_saved);
            }
        });
    }

    // ── Toggle Follow Artist (persist to local library) ──────────────────────
    {
        let ui_weak = ui.as_weak();
        let cur_artist = cur_artist.clone();
        ui.on_toggle_subscribe_artist(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let artist = cur_artist.lock().map(|a| a.clone()).unwrap_or_default();
                if artist.browse_id.is_empty() {
                    ui.set_artist_view_subscribed(!ui.get_artist_view_subscribed());
                    return;
                }
                let now_followed = core::library::toggle_follow_artist(artist);
                ui.set_artist_view_subscribed(now_followed);
                refresh_sidebar(&ui);
            }
        });
    }

    // ── Toggle Save Playlist (persist to local library) ──────────────────────
    {
        let ui_weak = ui.as_weak();
        let cur_playlist = cur_playlist.clone();
        ui.on_toggle_like_playlist(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let playlist = cur_playlist.lock().map(|p| p.clone()).unwrap_or_default();
                if playlist.playlist_id.is_empty() {
                    ui.set_playlist_view_liked(!ui.get_playlist_view_liked());
                    return;
                }
                let now_saved = core::library::toggle_save_playlist(playlist);
                ui.set_playlist_view_liked(now_saved);
            }
        });
    }

    // ── Add to playlist: open the picker modal for a song ────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_open_add_to_playlist(move |vid, title, artist, dur| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_pending_song_vid(vid);
                ui.set_pending_song_title(title);
                ui.set_pending_song_artist(artist);
                ui.set_pending_song_dur(dur);
                refresh_modal_playlists(&ui);
                ui.set_show_playlist_modal(true);
            }
        });
    }

    // ── New playlist (create-only modal, no pending song) ────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_new_playlist(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_pending_song_vid(SharedString::default());
                ui.set_pending_song_title(SharedString::default());
                ui.set_pending_song_artist(SharedString::default());
                ui.set_pending_song_dur(0);
                refresh_modal_playlists(&ui);
                ui.set_show_playlist_modal(true);
            }
        });
    }

    // ── Submit a new playlist (create + optionally add the pending song) ─────
    {
        let ui_weak = ui.as_weak();
        ui.on_submit_new_playlist(move |name| {
            if name.trim().is_empty() { return; }
            if let Some(ui) = ui_weak.upgrade() {
                let id = core::library::create_playlist(&name);
                let vid = ui.get_pending_song_vid().to_string();
                if !vid.is_empty() {
                    let song = stored_song_from_parts(
                        &vid,
                        &ui.get_pending_song_title().to_string(),
                        &ui.get_pending_song_artist().to_string(),
                        ui.get_pending_song_dur() as u32,
                    );
                    core::library::add_song_to_playlist(&id, song);
                }
                ui.set_show_playlist_modal(false);
                ui.invoke_load_library_data();
                refresh_sidebar(&ui);
            }
        });
    }

    // ── Add the pending song to an existing local playlist ───────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_add_to_existing_playlist(move |pid| {
            if let Some(ui) = ui_weak.upgrade() {
                let vid = ui.get_pending_song_vid().to_string();
                if !vid.is_empty() {
                    let song = stored_song_from_parts(
                        &vid,
                        &ui.get_pending_song_title().to_string(),
                        &ui.get_pending_song_artist().to_string(),
                        ui.get_pending_song_dur() as u32,
                    );
                    core::library::add_song_to_playlist(&pid.to_string(), song);
                }
                ui.set_show_playlist_modal(false);
                refresh_sidebar(&ui);
            }
        });
    }

    // ── Delete a local playlist ──────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_delete_local_playlist(move |id| {
            core::library::delete_playlist(&id.to_string());
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_current_view(SharedString::from("Library"));
                ui.set_library_tab(SharedString::from("Playlists"));
                ui.invoke_load_library_data();
                refresh_sidebar(&ui);
            }
        });
    }

    // ── Remove a song from a local playlist (reloads the playlist view) ──────
    {
        let ui_weak = ui.as_weak();
        ui.on_remove_from_local_playlist(move |pid, vid| {
            core::library::remove_song_from_playlist(&pid.to_string(), &vid.to_string());
            if let Some(ui) = ui_weak.upgrade() {
                if let Some(pl) = core::library::get_local_playlist(&pid.to_string()) {
                    let items: Vec<SongItem> = pl.songs.iter().map(|s| {
                        let np: core::playback::NowPlaying = s.clone().into();
                        make_song_item(&np)
                    }).collect();
                    let n = items.len();
                    ui.set_playlist_view_songs(ModelRc::new(VecModel::from(items)));
                    ui.set_playlist_view_count(SharedString::from(n.to_string().as_str()));
                }
            }
        });
    }

    // ── Artist radio (the artist's top tracks; falls back to song radio) ─────
    {
        let ui_weak = ui.as_weak();
        ui.on_start_artist_radio(move |video_id, artist_name| {
            let vid = video_id.to_string();
            let artist = artist_name.to_string();
            if let Some(ui) = ui_weak.upgrade() {
                context_ui::invalidate(&ui);
                ui.set_is_loading(true);
                ui.set_radio_title(SharedString::from(format!("{} Radio", artist).as_str()));
                ui.set_radio_songs(ModelRc::new(VecModel::from(Vec::<SongItem>::new())));
                ui.set_current_view(SharedString::from("Radio"));
            }
            let ui_w = ui_weak.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                let raw: Vec<(String, String, String, u32)> = rt.block_on(async {
                    use ytmapi_rs::common::{VideoID, ArtistChannelID, YoutubeID};
                    let api = match crate::core::net::ytmusic().await { Ok(a) => a, Err(_) => return vec![] };
                    // Resolve the artist's browse id: session metadata, else search by name.
                    let mut artist_id = core::playback::get_song_meta(&vid).map(|m| m.artist_id).unwrap_or_default();
                    if artist_id.is_empty() {
                        use ytmapi_rs::query::{SearchQuery, search::ArtistsFilter};
                        if let Ok(res) = api.query(SearchQuery::new(artist.clone()).with_filter(ArtistsFilter)).await {
                            if let Some(a) = res.into_iter().next() { artist_id = a.browse_id.get_raw().to_string(); }
                        }
                    }
                    // Prefer the artist's own top tracks (a real artist-centric radio).
                    if !artist_id.is_empty() {
                        if let Ok(art) = api.get_artist(ArtistChannelID::from_raw(&artist_id)).await {
                            let songs: Vec<(String, String, String, u32)> = art.top_releases.songs.iter()
                                .flat_map(|section| section.results.iter())
                                .map(|s| (s.video_id.get_raw().to_string(), s.title.clone(), artist.clone(), 0u32))
                                .filter(|(v, _, _, _)| !v.is_empty())
                                .collect();
                            if songs.len() >= 3 { return songs; }
                        }
                    }
                    // Fallback: watch-playlist radio seeded from the clicked track.
                    if let Ok(tracks) = api.get_watch_playlist_from_video_id(VideoID::from_raw(&vid)).await {
                        return tracks.into_iter()
                            .map(|t| (t.video_id.get_raw().to_string(), t.title, t.author, parse_duration(&t.duration).unwrap_or(0)))
                            .filter(|(v, _, _, _)| !v.is_empty())
                            .collect();
                    }
                    vec![]
                });
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        let items: Vec<SongItem> = raw.iter().map(|(v, t, a, d)| {
                            make_song_item(&core::playback::NowPlaying::enriched(v.clone(), t.clone(), a.clone(), *d))
                        }).collect();
                        ui.set_radio_songs(ModelRc::new(VecModel::from(items)));
                        ui.set_is_loading(false);
                    }
                }).ok();
            });
        });
    }

    // ── Queue-row "Play next": move an upcoming song right after the current ─
    {
        let ui_weak = ui.as_weak();
        ui.on_move_queue_song_next(move |idx| {
            playback.move_queue_song_next(idx.max(0) as usize);
            if crate::core::party::party_manager().snapshot().is_active {
                crate::core::party::party_manager().send_action(
                    crate::core::party::PartyAction::MoveQueueNext { upcoming_index: idx.max(0) as usize },
                    false,
                );
            }
            if let Some(ui) = ui_weak.upgrade() {
                let items: Vec<SongItem> = playback.queue_upcoming().iter().map(make_song_item).collect();
                ui.set_queue(ModelRc::new(VecModel::from(items)));
            }
        });
    }

    // ── "Go to artist" resolved by video id (session meta), else by name ─────
    {
        let ui_weak = ui.as_weak();
        ui.on_open_artist_for(move |vid, name| {
            if let Some(ui) = ui_weak.upgrade() {
                let aid = core::playback::get_song_meta(&vid.to_string()).map(|m| m.artist_id).unwrap_or_default();
                if !aid.is_empty() {
                    ui.invoke_navigate_to_artist(SharedString::from(aid.as_str()));
                } else if !name.is_empty() {
                    ui.invoke_go_to_song_artist(name);
                }
            }
        });
    }

    // ── "Go to album" resolved by video id (session meta) ────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_open_album_for(move |vid, album_name| {
            if let Some(ui) = ui_weak.upgrade() {
                let m = core::playback::get_song_meta(&vid.to_string()).unwrap_or_default();
                if !m.album_id.is_empty() {
                    ui.invoke_navigate_to_album(SharedString::from(m.album_id.as_str()));
                    return;
                }
                // Fall back to the album NAME. Prefer the cached metadata, but a
                // song we've never played has no meta at all — hence the name the
                // row itself is displaying, passed in by the caller. Without this
                // fallback "Go to album" silently did nothing for search results.
                let name = if !m.album.is_empty() { m.album } else { album_name.to_string() };
                if !name.trim().is_empty() {
                    ui.invoke_go_to_song_album(SharedString::from(name.as_str()));
                }
            }
        });
        ui.on_album_resolvable(move |vid, album_name| {
            if !album_name.trim().is_empty() { return true; }
            let m = core::playback::get_song_meta(&vid.to_string()).unwrap_or_default();
            !m.album_id.is_empty() || !m.album.is_empty()
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
                    .map(|item| core::playback::NowPlaying::enriched(
                        item.video_id.to_string(),
                        item.title.to_string(),
                        item.artist.to_string(),
                        item.duration_secs as u32,
                    ))
                    .collect();
                if !songs.is_empty() {
                    let playback = crate::core::bridge::playback_core();
                    playback.set_queue(songs.clone());
                    {
                        let mut state = playback.state_lock();
                        state.is_playing = true;
                    }
                    let first = playback.now_playing();
                    playback.set_now_playing(&first.video_id, &first.title, &first.artist, first.duration_secs);
                    if crate::core::party::party_manager().snapshot().is_active {
                        let tracks: Vec<crate::core::party::PartyTrack> = songs.iter().map(|s| now_playing_to_party_track(s, "")).collect();
                        crate::core::party::party_manager().send_action(
                            crate::core::party::PartyAction::ReplaceQueue { tracks, start_index: 0, is_playing: true },
                            false,
                        );
                    }
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
                    use ytmapi_rs::query::{SearchQuery, search::ArtistsFilter};
                    use ytmapi_rs::common::YoutubeID;
                    let api = crate::core::net::ytmusic().await.ok()?;
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
                    use ytmapi_rs::query::{SearchQuery, search::AlbumsFilter};
                    use ytmapi_rs::common::YoutubeID;
                    let api = crate::core::net::ytmusic().await.ok()?;
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
        let cur_album = cur_album.clone();
        ui.on_navigate_to_album(move |browse_id| {
            let browse_id = browse_id.to_string();
            if browse_id.trim().is_empty() { return; }
            if let Some(ui) = ui_weak.upgrade() { context_ui::invalidate(&ui); }

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

            let cur_album = cur_album.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let result = rt.block_on(async {
                    use ytmapi_rs::common::AlbumID;

                    let api = crate::core::net::ytmusic().await.map_err(|e| e.to_string())?;
                    api.get_album(AlbumID::from_raw(&browse_id)).await.map_err(|e| e.to_string())
                });

                let ui_weak3 = ui_weak2.clone();
                let browse_id_c = browse_id.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_is_loading(false);
                        match result {
                            Ok(album) => {
                                context_ui::register_album(&browse_id_c, &album);
                                ui.set_album_view_title(SharedString::from(album.title.as_str()));
                                let artist_name = context_ui::credit_names(&context_ui::credits(&album.artists));
                                ui.set_album_view_artist(SharedString::from(artist_name.as_str()));
                                ui.set_album_view_year(SharedString::from(album.year.as_str()));
                                ui.set_album_view_has_thumbnail(false);
                                let alb_thumb = pick_thumb(&album.thumbnails).map(|t| t.url.clone()).unwrap_or_default();
                                if let Ok(mut c) = cur_album.lock() {
                                    *c = core::library::SavedAlbum {
                                        browse_id: browse_id_c.clone(),
                                        title: album.title.clone(),
                                        artist: artist_name.clone(),
                                        thumbnail_url: alb_thumb.clone(),
                                    };
                                }
                                core::library::set_album_thumbnail(&browse_id_c, &alb_thumb);
                                ui.set_album_view_liked(core::library::is_album_saved(&browse_id_c));
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
                                if let Some(thumb) = pick_thumb(&album.thumbnails) {
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
                                        use ytmapi_rs::common::ArtistChannelID;
                                        use ytmapi_rs::query::{SearchQuery, search::AlbumsFilter};

                                        let api = match crate::core::net::ytmusic().await {
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
                                                        let thumb = pick_thumb(&a.thumbnails).map(|t| t.url.clone());
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
                                                    let thumb = pick_thumb(&a.thumbnails).map(|t| t.url.clone());
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

    // HWND will be resolved lazily after the window is shown
    #[cfg(target_os = "windows")]
    let app_hwnd: Arc<std::sync::atomic::AtomicIsize> = Arc::new(std::sync::atomic::AtomicIsize::new(0));

    // ── Background state polling (500ms) ──────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        #[cfg(target_os = "windows")]
        let app_hwnd = app_hwnd.clone();
        let autoplay_enabled = autoplay_enabled.clone();
        let prefetch_depth = prefetch_depth.clone();
        std::thread::spawn(move || {
            let http = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .ok();
            let mut last_thumbnail_id = String::new();
            let mut last_precached_id = String::new(); // next-song we've queued a full cache download for
            let mut last_upgraded_id = String::new();  // song we've upgraded stream→cache
            let mut last_sidebar_album_sig = String::new(); // dedupes the sidebar album rebuild
            let mut thumb_refresh_counter: u32 = 0;
            let mut thumb_fetch_done: Option<std::sync::mpsc::Receiver<()>> = None;
            let mut last_autoplay_refill: Option<std::time::Instant> = None;
            let mut last_notified_np_id = String::new();
            let mut party_poll_counter: u32 = 0;
            let mut last_party_drift_seek = std::time::Instant::now();
            #[cfg(target_os = "windows")]
            let mut smtc_attached = false;

            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));

                // Minimize-to-tray: if window is minimized and setting is on, hide it
                #[cfg(target_os = "windows")]
                {
                    use windows::Win32::Foundation::HWND;
                    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, ShowWindow, SW_HIDE, FindWindowExW, GetWindowThreadProcessId};
                    use windows::core::w;

                    // Lazily resolve the app window (exists after ui.run() starts).
                    // In debug builds a console window can share the "Auricle" title,
                    // so pick the top-level "Auricle" window owned by THIS process —
                    // otherwise we'd hide/restore the console and minimize-to-tray
                    // would silently do nothing.
                    let mut hwnd_val = app_hwnd.load(std::sync::atomic::Ordering::Relaxed);
                    if hwnd_val == 0 {
                        let found = unsafe {
                            let our_pid = std::process::id();
                            let mut child = HWND::default();
                            let mut result: isize = 0;
                            loop {
                                match FindWindowExW(HWND::default(), child, None, w!("Auricle")) {
                                    Ok(h) if !h.0.is_null() => {
                                        child = h;
                                        let mut pid: u32 = 0;
                                        GetWindowThreadProcessId(h, Some(&mut pid));
                                        if pid == our_pid { result = h.0 as isize; break; }
                                    }
                                    _ => break,
                                }
                            }
                            result
                        };
                        if found != 0 {
                            app_hwnd.store(found, std::sync::atomic::Ordering::Relaxed);
                            hwnd_val = found;
                        }
                    }

                    if !smtc_attached && hwnd_val != 0 {
                        smtc_attached = true;
                        let ui_w_smtc = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            let ui_w_act = ui_w_smtc.clone();
                            crate::core::media_controls::attach(hwnd_val, move |action| {
                                let ui_w = ui_w_act.clone();
                                slint::invoke_from_event_loop(move || {
                                    let Some(ui) = ui_w.upgrade() else { return; };
                                    let playback = crate::core::bridge::playback_core();
                                    match action {
                                        crate::core::media_controls::MediaAction::PlayPause => {
                                            ui.invoke_toggle_play_pause();
                                        }
                                        crate::core::media_controls::MediaAction::Play => {
                                            if !playback.is_playing() {
                                                ui.invoke_toggle_play_pause();
                                            }
                                        }
                                        crate::core::media_controls::MediaAction::Pause => {
                                            if playback.is_playing() {
                                                ui.invoke_toggle_play_pause();
                                            }
                                        }
                                        crate::core::media_controls::MediaAction::Next => {
                                            ui.invoke_next_track();
                                        }
                                        crate::core::media_controls::MediaAction::Previous => {
                                            ui.invoke_prev_track();
                                        }
                                        crate::core::media_controls::MediaAction::SeekTo(secs) => {
                                            let dur = playback.track_duration_secs() as f64;
                                            if dur > 0.0 {
                                                let fraction = (secs / dur).clamp(0.0, 1.0) as f32;
                                                ui.invoke_seek(fraction);
                                            } else {
                                                playback.seek_to_secs(secs);
                                            }
                                        }
                                        crate::core::media_controls::MediaAction::SeekBy(delta_secs) => {
                                            let target = (playback.elapsed_secs() + delta_secs).max(0.0);
                                            let dur = playback.track_duration_secs() as f64;
                                            if dur > 0.0 {
                                                let fraction = (target / dur).clamp(0.0, 1.0) as f32;
                                                ui.invoke_seek(fraction);
                                            } else {
                                                playback.seek_to_secs(target);
                                            }
                                        }
                                    }
                                    let np = playback.now_playing();
                                    let album = if !np.album.is_empty() {
                                        np.album
                                    } else {
                                        crate::core::playback::get_song_meta(&np.video_id)
                                            .map(|m| m.album)
                                            .unwrap_or_default()
                                    };
                                    crate::core::media_controls::update(
                                        crate::core::media_controls::MediaSnapshot {
                                            video_id: np.video_id,
                                            title: np.title,
                                            artist: np.artist,
                                            album,
                                            duration_secs: playback.track_duration_secs(),
                                            elapsed_secs: playback.elapsed_secs(),
                                            is_playing: playback.is_playing(),
                                        },
                                    );
                                }).ok();
                            });
                        }).ok();
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

                let party_snap = crate::core::party::party_manager().snapshot();

                // Auto-advance when track ends
                if party_snap.is_active && !party_snap.is_host {
                    // Guest pauses at track end and waits for Host's state broadcast
                    playback.take_advance_pending();
                    playback.take_autoplay_needed();
                } else {
                    if playback.take_advance_pending() {
                        playback.advance_after_track_end();
                        if party_snap.is_active && party_snap.is_host {
                            let full = playback.full_queue();
                            let tracks: Vec<crate::core::party::PartyTrack> = full.iter().map(|s| now_playing_to_party_track(s, "")).collect();
                            crate::core::party::party_manager().send_action(
                                crate::core::party::PartyAction::ReplaceQueue {
                                    tracks,
                                    start_index: playback.queue_index(),
                                    is_playing: playback.is_playing(),
                                },
                                false,
                            );
                        }
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
                            let next_song = playback.pop_autoplay();
                            if let Some(song) = next_song {
                                {
                                    let mut state = playback.state_lock();
                                    state.is_playing = true;
                                }
                                playback.set_now_playing(&song.video_id, &song.title, &song.artist, song.duration_secs);
                                if party_snap.is_active && party_snap.is_host {
                                    let full = playback.full_queue();
                                    let tracks: Vec<crate::core::party::PartyTrack> = full.iter().map(|s| now_playing_to_party_track(s, "")).collect();
                                    crate::core::party::party_manager().send_action(
                                        crate::core::party::PartyAction::ReplaceQueue {
                                            tracks,
                                            start_index: playback.queue_index(),
                                            is_playing: true,
                                        },
                                        false,
                                    );
                                }
                                let ui_w = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_w.upgrade() {
                                        refresh_native_shell_ui(&ui, playback);
                                        // Update autoplay queue model
                                        let items: Vec<SongItem> = playback.autoplay_queue().iter().map(make_song_item).collect();
                                        ui.set_autoplay_queue(ModelRc::new(VecModel::from(items)));
                                    }
                                }).ok();
                            } else {
                                // Autoplay queue exhausted — continue radio from the most
                                // recently played track so we get a fresh continuation
                                // instead of replaying the original seed's batch.
                                let seed = {
                                    let np = playback.now_playing().video_id;
                                    if !np.is_empty() && np != "native-prototype" {
                                        np
                                    } else {
                                        playback.autoplay_seed()
                                    }
                                };
                                if !seed.is_empty() {
                                    // Keep the seed fresh for the next exhaustion cycle.
                                    playback.set_autoplay_seed(&seed);
                                    let ui_w = ui_weak.clone();
                                    std::thread::spawn(move || {
                                        fetch_autoplay_queue(ui_w, seed, true);
                                    });
                                } else {
                                    eprintln!("[autoplay] queue exhausted but no seed available — stopping");
                                    if playback.is_awaiting_autoplay_after_end() {
                                        playback.stop_at_queue_end();
                                        let ui_w = ui_weak.clone();
                                        slint::invoke_from_event_loop(move || {
                                            if let Some(ui) = ui_w.upgrade() {
                                                refresh_native_shell_ui(&ui, playback);
                                            }
                                        }).ok();
                                    }
                                }
                            }
                        } else if playback.is_awaiting_autoplay_after_end() {
                            playback.stop_at_queue_end();
                            let ui_w = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_w.upgrade() {
                                    refresh_native_shell_ui(&ui, playback);
                                }
                            }).ok();
                        }
                    }
                }

                if autoplay_enabled.load(std::sync::atomic::Ordering::Relaxed)
                    && playback.is_playing()
                {
                    let np_id = playback.now_playing().video_id;
                    let cooldown = if playback.last_autoplay_refill_empty() {
                        std::time::Duration::from_secs(60)
                    } else {
                        std::time::Duration::from_secs(10)
                    };
                    if !np_id.is_empty()
                        && np_id != "native-prototype"
                        && playback.autoplay_queue().len() < core::playback::AUTOPLAY_TARGET
                        && last_autoplay_refill.map_or(true, |t| t.elapsed() >= cooldown)
                    {
                        last_autoplay_refill = Some(std::time::Instant::now());
                        refill_autoplay_queue(ui_weak.clone());
                    }
                }

                // Start the playback timer the moment audio actually begins playing.
                if playback.take_audio_just_started() {
                    playback.on_audio_started();
                    let np = playback.now_playing();
                    if let Some(target_ms) = crate::core::party::party_manager().target_position_ms(&np.video_id) {
                        if target_ms > 800 {
                            playback.seek_to_secs(target_ms as f64 / 1000.0);
                        }
                    }
                    // NOW kick off the cache download — after stream URL is resolved
                    // so yt-dlp cookie DB isn't locked by a competing process.
                    if !np.video_id.is_empty() && np.video_id != "native-prototype" {
                        crate::core::cache::spawn_cache_download(np.video_id.clone(), np.title.clone(), np.artist.clone());
                        // Persist as last played
                        let mut settings = core::persistence::load_settings();
                        settings.last_played = Some(core::persistence::StoredSong::from(&np));
                        core::persistence::save_settings(&settings);
                    }
                    // History changed — refresh the "recently listened" album shortcuts.
                    //
                    // Rebuilding this model decodes up to 8 covers ON THE UI THREAD, so
                    // only do it when the album set actually changed. Most track changes
                    // (same album, or an album already in the recent list) leave it
                    // identical and can be skipped entirely.
                    let sig = sidebar_album_seeds().into_iter()
                        .map(|(id, _, _)| id)
                        .collect::<Vec<_>>()
                        .join("\u{1}");
                    if sig != last_sidebar_album_sig {
                        last_sidebar_album_sig = sig;
                        let ui_w = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                ui.set_sidebar_albums(ModelRc::new(VecModel::from(compute_sidebar_albums())));
                                spawn_sidebar_album_thumbs(ui.as_weak());
                            }
                        }).ok();
                    }
                }

                let track = playback.now_playing();
                let is_playing = playback.is_playing();
                let elapsed = playback.elapsed_secs();
                // Backfill duration detected from the audio stream (for songs whose
                // metadata lacked one) so total-time, the progress bar, and seeking work.
                let detected = playback.take_detected_duration();
                if detected > 0 { playback.set_current_duration(detected); }
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

                // Album art when the track changes. If the thumbnail is already on
                // disk, use it immediately; otherwise download it on a background
                // thread so this 500ms poll loop never blocks on the network.
                let thumbnail_changed = track.video_id != last_thumbnail_id
                    && !track.video_id.is_empty()
                    && track.video_id != "native-prototype";

                let mut thumb_path: Option<std::path::PathBuf> = None;
                if thumbnail_changed {
                    last_thumbnail_id = track.video_id.clone();
                    let cached = std::env::temp_dir()
                        .join(format!("ytm_thumb_{}.jpg", track.video_id));
                    if cached.exists() {
                        thumb_path = Some(cached);
                    } else if let Some(ref client) = http {
                        let client = client.clone();
                        let vid = track.video_id.clone();
                        let ui_weak_art = ui_weak.clone();
                        std::thread::spawn(move || {
                            let url = format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", vid);
                            let tmp = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", vid));
                            let bytes = client.get(&url).send().ok()
                                .filter(|r| r.status().is_success())
                                .and_then(|r| r.bytes().ok());
                            if let Some(b) = bytes {
                                let _ = std::fs::write(&tmp, &b);
                                let art_color = extract_vibrant_accent_from_bytes(&b);
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_art.upgrade() {
                                        // Only apply if this is still the current track.
                                        if ui.get_now_playing_video_id().as_str() == vid {
                                            if let Ok(img) = slint::Image::load_from_path(&tmp) {
                                                ui.set_album_art(img);
                                                ui.set_has_album_art(true);
                                                if art_color.is_some() {
                                                    apply_art_accent(&ui, art_color);
                                                }
                                            }
                                        }
                                    }
                                }).ok();
                            }
                        });
                    }
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

                // ── Look-ahead prewarm once the current song is 25% through ──
                // Combine the user queue's upcoming songs with the autoplay queue
                // (playback flows seamlessly from either), then fully cache the
                // next `prefetch_depth` songs and URL-prewarm the one right after.
                // This is what makes queue/autoplay transitions instant instead of
                // paying the ~3s yt-dlp resolve on every track change.
                if is_playing && progress > 0.25 && last_precached_id != track.video_id {
                    last_precached_id = track.video_id.clone();
                    let depth = prefetch_depth.load(std::sync::atomic::Ordering::Relaxed).clamp(1, 3) as usize;
                    let want = depth + 1; // cache `depth`, plus URL-warm the next one
                    let mut next: Vec<core::playback::NowPlaying> = playback.queue_upcoming();
                    if next.len() < want {
                        for s in playback.autoplay_queue() {
                            if next.len() >= want { break; }
                            if s.video_id != track.video_id
                                && next.iter().all(|n| n.video_id != s.video_id)
                            {
                                next.push(s);
                            }
                        }
                    }
                    // Fully cache the next `depth` songs so they start instantly.
                    for song in next.iter().take(depth) {
                        crate::core::cache::spawn_cache_download(
                            song.video_id.clone(), song.title.clone(), song.artist.clone());
                    }
                    // Warm just the stream URL of the song right after the cached ones.
                    if let Some(after) = next.get(depth) {
                        crate::core::stream_player::prefetch_stream_url(&after.video_id);
                    }
                    // Protect the current song + everything we just looked ahead to
                    // from LRU eviction, so we never drop a song we're about to play.
                    if let Ok(mut cache) = crate::core::cache::AudioCache::global().lock() {
                        let mut protect: Vec<String> = Vec::with_capacity(next.len() + 1);
                        protect.push(track.video_id.clone());
                        protect.extend(next.iter().map(|n| n.video_id.clone()));
                        cache.set_protected(protect);
                    }
                }

                // ── Silent stream→cache upgrade (only while paused) ──
                // When the current streaming song's cache download has finished
                // AND playback is paused, swap the source to the local file now.
                // Doing it while paused makes the sink swap inaudible (no samples
                // are flowing), so there's no mid-song hiccup. On resume it plays
                // from the cached file (instant seeks, immune to the stream
                // dropping). If the user never pauses, the song simply streams to
                // the end with no swap — and seeks still load the cache on demand
                // via the worker's Seek handler. Fires once per song.
                if !is_playing
                    && !playback.is_current_cached()
                    && track.video_id != last_upgraded_id
                    && !track.video_id.is_empty()
                    && track.video_id != "native-prototype"
                {
                    let cached_path = crate::core::cache::AudioCache::global().lock().ok()
                        .and_then(|mut c| c.get(&track.video_id));
                    if let Some(path) = cached_path {
                        last_upgraded_id = track.video_id.clone();
                        playback.upgrade_current_to_cache(path);
                    }
                }

                // ── Periodic thumbnail fetch for queue items (every 3s = 6 ticks) ──
                if let Some(done) = &thumb_fetch_done {
                    if !matches!(done.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                        thumb_fetch_done = None;
                    }
                }
                thumb_refresh_counter += 1;
                if thumb_refresh_counter >= 6 && thumb_fetch_done.is_none() {
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
                        let (done_tx, done_rx) = std::sync::mpsc::channel();
                        thumb_fetch_done = Some(done_rx);
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
                            drop(done_tx);
                        });
                    }
                }

                let thumb_art_color = thumb_path.as_deref().and_then(extract_vibrant_accent_from_path);

                #[cfg(target_os = "windows")]
                let media_snap = {
                    let album = if !track.album.is_empty() {
                        track.album.clone()
                    } else {
                        crate::core::playback::get_song_meta(&track.video_id)
                            .map(|m| m.album)
                            .unwrap_or_default()
                    };
                    crate::core::media_controls::MediaSnapshot {
                        video_id: track.video_id.clone(),
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        album,
                        duration_secs: dur,
                        elapsed_secs: elapsed,
                        is_playing,
                    }
                };

                if track.video_id != last_notified_np_id {
                    last_notified_np_id = track.video_id.clone();
                    if !track.video_id.is_empty() {
                        now_playing_ui::notify_track_changed(
                            ui_weak.clone(),
                            track.video_id.clone(),
                            track.title.clone(),
                            track.artist.clone(),
                            dur,
                        );
                    }
                }

                let can_shuffle = playback.has_upcoming_user_queue();
                let repeat_str = playback.repeat_mode().as_str();

                let ui_w = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        if ui.get_track_title().as_str() != track.title.as_str() {
                            ui.set_track_title(SharedString::from(track.title.as_str()));
                        }
                        if ui.get_track_artist().as_str() != track.artist.as_str() {
                            ui.set_track_artist(SharedString::from(track.artist.as_str()));
                        }
                        if ui.get_is_playing() != is_playing {
                            ui.set_is_playing(is_playing);
                        }
                        if ui.get_can_shuffle_queue() != can_shuffle {
                            ui.set_can_shuffle_queue(can_shuffle);
                        }
                        if ui.get_repeat_mode().as_str() != repeat_str {
                            ui.set_repeat_mode(SharedString::from(repeat_str));
                        }
                        if ui.get_now_playing_video_id().as_str() != track.video_id.as_str() {
                            ui.set_now_playing_video_id(SharedString::from(track.video_id.as_str()));
                        }
                        if (ui.get_progress() - progress).abs() >= 0.0005 {
                            ui.set_progress(progress);
                        }
                        if ui.get_current_time().as_str() != current_time.as_str() {
                            ui.set_current_time(SharedString::from(current_time.as_str()));
                        }
                        if ui.get_total_time().as_str() != total_time.as_str() {
                            ui.set_total_time(SharedString::from(total_time.as_str()));
                        }
                        if ui.get_track_initial().as_str() != initial.as_str() {
                            ui.set_track_initial(SharedString::from(initial.as_str()));
                        }
                        if ui.get_is_liked() != liked {
                            ui.set_is_liked(liked);
                        }
                        if ui.get_cache_used_mb() != cache_used_mb {
                            ui.set_cache_used_mb(cache_used_mb);
                        }
                        if ui.get_cache_song_count() != cache_song_count {
                            ui.set_cache_song_count(cache_song_count);
                        }
                        if ui.get_cache_limit_mb() != cache_limit_mb {
                            ui.set_cache_limit_mb(cache_limit_mb);
                        }
                        if let Some(path) = thumb_path {
                            if let Ok(img) = slint::Image::load_from_path(&path) {
                                ui.set_album_art(img);
                                ui.set_has_album_art(true);
                                if thumb_art_color.is_some() {
                                    apply_art_accent(&ui, thumb_art_color);
                                }
                            }
                        } else if thumbnail_changed {
                            ui.set_has_album_art(false);
                            apply_art_accent(&ui, None);
                        }
                    }
                    #[cfg(target_os = "windows")]
                    crate::core::media_controls::update(media_snap);
                }).ok();

                // ── Auricle Party periodic polling & drift correction ─────────
                party_poll_counter += 1;
                if party_poll_counter >= 2 {
                    party_poll_counter = 0;
                    let psnap = crate::core::party::party_manager().snapshot();
                    if psnap.is_active || psnap.status != "off" {
                        let ui_w_party = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w_party.upgrade() {
                                apply_pending_party_updates(&ui, playback);
                                refresh_party_ui(&ui);
                            }
                        }).ok();

                        if is_playing && !playback.is_audio_loading() {
                            let cur_vid = playback.now_playing().video_id;
                            if let Some(target_ms) = crate::core::party::party_manager().target_position_ms(&cur_vid) {
                                let current_ms = playback.elapsed_secs() * 1000.0;
                                let delta = (current_ms - target_ms as f64).abs();
                                if delta > 750.0 && last_party_drift_seek.elapsed() >= Duration::from_secs(4) {
                                    last_party_drift_seek = Instant::now();
                                    playback.seek_to_secs(target_ms as f64 / 1000.0);
                                }
                            }
                        }
                    }
                }
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
                    // Quit via the event loop so the TrayIcon's Drop removes the icon.
                    // (std::process::exit skips destructors, leaving a ghost tray icon.)
                    slint::invoke_from_event_loop(|| { let _ = slint::quit_event_loop(); }).ok();
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
            core::playback::register_song_meta(&last.video_id, &last.album, &last.album_id, &last.artist_id);
            let np = crate::core::playback::NowPlaying {
                video_id: last.video_id.clone(),
                title: last.title.clone(),
                artist: last.artist.clone(),
                duration_secs: last.duration_secs,
                album: last.album.clone(),
                album_id: last.album_id.clone(),
                artist_id: last.artist_id.clone(),
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
                    let art_color = extract_vibrant_accent_from_path(&thumb_path);
                    if art_color.is_some() {
                        apply_art_accent(&ui, art_color);
                    }
                }
            }
            // Fetch autoplay queue for last played song
            {
                playback.set_autoplay_seed(&last.video_id);
                let ui_w = ui.as_weak();
                let vid = last.video_id.clone();
                std::thread::spawn(move || {
                    fetch_autoplay_queue(ui_w, vid, false);
                });
            }
            // Warmup: pre-fetch the stream URL (and cache if not already cached)
            // in the background so the first play starts without the usual ~3s
            // yt-dlp delay. For a cached song this is a no-op; for a streaming
            // song it populates the in-memory URL cache (6h TTL) so the worker's
            // spawn_stream_fetch gets an instant cache hit.
            {
                let vid = last.video_id.clone();
                let title = last.title.clone();
                let artist = last.artist.clone();
                std::thread::spawn(move || {
                    // If already cached on disk, no network work needed.
                    let already_cached = crate::core::cache::AudioCache::global()
                        .lock().ok()
                        .and_then(|mut c| c.get(&vid))
                        .is_some();
                    if !already_cached {
                        // Resolve and cache the stream URL (~3s) so the first
                        // play is instant, then kick off the full cache download
                        // so seeks and offline playback work immediately.
                        let _ = crate::core::stream_player::get_stream_url(&vid);
                        crate::core::cache::spawn_cache_download(vid, title, artist);
                    }
                });
            }
        }
    }

    // ── Load library data (Albums/Artists/Playlists tabs) ────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_load_library_data(move || {
            let _ = &ui_weak;
            if let Some(ui) = ui_weak.upgrade() {
                let tab = ui.get_library_tab().to_string();
                match tab.as_str() {
                    "Albums" => {
                        // Real saved albums from the local library.
                        let albums: Vec<AlbumItem> = core::library::saved_albums().into_iter().map(|a| {
                            let (thumb, has) = load_cached_thumb(&library_thumb_path("album", &a.browse_id));
                            AlbumItem {
                                title: SharedString::from(a.title.as_str()),
                                browse_id: SharedString::from(a.browse_id.as_str()),
                                artist: SharedString::from(a.artist.as_str()),
                                year: SharedString::default(),
                                thumbnail: thumb,
                                has_thumbnail: has,
                            }
                        }).collect();
                        ui.set_library_albums(ModelRc::new(VecModel::from(albums)));
                        spawn_album_thumb_loader(ui_weak.clone());
                    },
                    "Artists" => {
                        // Real followed artists from the local library.
                        let artists: Vec<ArtistItem> = core::library::followed_artists().into_iter().map(|a| {
                            let (thumb, has) = load_cached_thumb(&library_thumb_path("artist", &a.browse_id));
                            ArtistItem {
                                name: SharedString::from(a.name.as_str()),
                                browse_id: SharedString::from(a.browse_id.as_str()),
                                thumbnail: thumb,
                                has_thumbnail: has,
                                subscriber_count: SharedString::default(),
                            }
                        }).collect();
                        ui.set_library_artists(ModelRc::new(VecModel::from(artists)));
                        spawn_artist_thumb_loader(ui_weak.clone());
                    },
                    "Playlists" => {
                        // Local (user-created) playlists first, then saved public ones.
                        let mut items: Vec<PlaylistItem> = core::library::local_playlists().into_iter().map(|p| {
                            let n = p.songs.len();
                            PlaylistItem {
                                title: SharedString::from(p.name.as_str()),
                                playlist_id: SharedString::from(p.id.as_str()),
                                thumbnail: Default::default(),
                                has_thumbnail: false,
                                count_text: SharedString::from(format!("{} song{}", n, if n == 1 { "" } else { "s" }).as_str()),
                            }
                        }).collect();
                        items.extend(core::library::saved_playlists().into_iter().map(|p| {
                            PlaylistItem {
                                title: SharedString::from(p.title.as_str()),
                                playlist_id: SharedString::from(p.playlist_id.as_str()),
                                thumbnail: Default::default(),
                                has_thumbnail: false,
                                count_text: SharedString::from("Saved"),
                            }
                        }));
                        ui.set_library_playlists(ModelRc::new(VecModel::from(items)));
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
        let cur_playlist = cur_playlist.clone();
        ui.on_navigate_to_playlist(move |playlist_id| {
            let playlist_id = playlist_id.to_string();
            if playlist_id.trim().is_empty() { return; }
            if let Some(ui) = ui_weak.upgrade() { context_ui::invalidate(&ui); }

            if !nav_restoring.load(std::sync::atomic::Ordering::Relaxed) {
                push_nav_entry(&nav_history, &nav_cursor, "Playlist".to_string(), playlist_id.clone());
                if let Some(ui) = ui_weak.upgrade() {
                    let hist = nav_history.lock().unwrap();
                    let cur = *nav_cursor.lock().unwrap();
                    update_nav_buttons(&ui, &hist, cur);
                }
            }

            // Local (user-created) playlists are stored on disk — render directly,
            // no API call. Their ids are prefixed "local-".
            if playlist_id.starts_with("local-") {
                if let Some(ui) = ui_weak.upgrade() {
                    if let Some(pl) = core::library::get_local_playlist(&playlist_id) {
                        let items: Vec<SongItem> = pl.songs.iter()
                            .map(|s| make_song_item(&s.clone().into()))
                            .collect();
                        let n = items.len();
                        ui.set_playlist_view_title(SharedString::from(pl.name.as_str()));
                        ui.set_playlist_view_songs(ModelRc::new(VecModel::from(items)));
                        ui.set_playlist_view_count(SharedString::from(n.to_string().as_str()));
                        ui.set_playlist_view_duration(SharedString::default());
                        ui.set_playlist_view_similar(ModelRc::new(VecModel::from(Vec::<PlaylistItem>::new())));
                        ui.set_playlist_view_has_thumbnail(false);
                        ui.set_playlist_view_liked(false);
                        ui.set_playlist_view_is_local(true);
                        ui.set_playlist_view_id(SharedString::from(playlist_id.as_str()));
                        ui.set_current_view(SharedString::from("Playlist"));
                        ui.set_is_loading(false);
                    }
                }
                return;
            }

            let ui_weak2 = ui_weak.clone();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_loading(true);
                ui.set_current_view(SharedString::from("Playlist"));
                ui.set_playlist_view_is_local(false);
                ui.set_playlist_view_id(SharedString::from(playlist_id.as_str()));
            }

            let cur_playlist = cur_playlist.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                let result = rt.block_on(async {
                    use ytmapi_rs::common::PlaylistID;

                    let api = crate::core::net::ytmusic().await.map_err(|e| e.to_string())?;
                    let id = PlaylistID::from_raw(&playlist_id);
                    let tracks = api.get_playlist_tracks(id).await.map_err(|e| e.to_string())?;
                    Ok::<Vec<ytmapi_rs::parse::PlaylistItem>, String>(tracks)
                });

                let ui_weak3 = ui_weak2.clone();
                let playlist_id_c = playlist_id.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak2.upgrade() {
                        ui.set_is_loading(false);
                        if let Ok(mut c) = cur_playlist.lock() {
                            *c = core::library::SavedPlaylist {
                                playlist_id: playlist_id_c.clone(),
                                title: ui.get_playlist_view_title().to_string(),
                                thumbnail_url: String::new(),
                            };
                        }
                        ui.set_playlist_view_liked(core::library::is_playlist_saved(&playlist_id_c));
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
                                    .and_then(|s| pick_thumb(&s.thumbnails))
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
                                        use ytmapi_rs::query::{SearchQuery, search::PlaylistsFilter};

                                        let api = match crate::core::net::ytmusic().await {
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
                                                            let thumb = pick_thumb(&f.thumbnails).map(|t| t.url.clone());
                                                            Some((f.title.clone(), f.playlist_id.get_raw().to_string(), f.songs.clone(), thumb))
                                                        },
                                                        SearchResultPlaylist::Community(c) => {
                                                            let thumb = pick_thumb(&c.thumbnails).map(|t| t.url.clone());
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
                    .map(|item| core::playback::NowPlaying::enriched(
                        item.video_id.to_string(),
                        item.title.to_string(),
                        item.artist.to_string(),
                        item.duration_secs as u32,
                    ))
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

    // ── Shuffle Playlist ─────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_shuffle_playlist(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let model = ui.get_playlist_view_songs();
                let mut songs: Vec<core::playback::NowPlaying> = (0..model.row_count())
                    .filter_map(|i| model.row_data(i))
                    .map(|item| core::playback::NowPlaying::enriched(
                        item.video_id.to_string(),
                        item.title.to_string(),
                        item.artist.to_string(),
                        item.duration_secs as u32,
                    ))
                    .collect();
                if !songs.is_empty() {
                    shuffle_vec(&mut songs);
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
                    .map(|item| core::playback::NowPlaying::enriched(
                        item.video_id.to_string(),
                        item.title.to_string(),
                        item.artist.to_string(),
                        item.duration_secs as u32,
                    ))
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
            fetch_followed_songs(ui_weak.clone());
        });
    }

    // Fetch trending and personalized songs for the Home page
    fetch_trending_songs(ui.as_weak());
    fetch_personalized_songs(ui.as_weak());

    // Fetch enhanced home data (new releases, mixes, genre, language)
    fetch_home_enhanced_data(ui.as_weak());

    // H1: "From artists you follow" — top songs from followed artists.
    fetch_followed_songs(ui.as_weak());

    // Populate the sidebar shortcuts (your artists + your playlists).
    refresh_sidebar(&ui);

    // End the event loop explicitly on close; otherwise the process could outlive its window.
    ui.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });

    // Confirm the window actually appeared; only warn if it never does, so later
    // hiding to the tray is not mistaken for a startup failure.
    let startup_check = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        let ticks = std::cell::Cell::new(0u32);
        let done = std::cell::Cell::new(false);
        startup_check.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), move || {
            if done.get() {
                return;
            }
            let Some(ui) = ui_weak.upgrade() else { return };
            ticks.set(ticks.get() + 1);
            if core::diagnostics::window_visible() {
                done.set(true);
                core::diagnostics::log_window_state(ui.window());
            } else if ticks.get() >= 10 {
                done.set(true);
                core::diagnostics::window_never_shown(ui.window());
            }
        });
    }
    log::info!("[startup] entering event loop");
    let result = ui.run();
    log::info!("[startup] event loop ended: {result:?}");
    // Remove the tray icon, then exit without waiting on background work or destructors.
    drop(_tray_icon);
    if result.is_ok() {
        std::process::exit(0);
    }
    result
}

/// Entry point for the executables: CLI flags, local diagnostics, then the UI.
pub fn launch() {
    if core::diagnostics::handle_cli_args() {
        return;
    }
    core::diagnostics::init();
    if let Err(err) = run_native_shell() {
        log::error!("[startup] UI failed: {err}");
        core::diagnostics::show_error_dialog(&format!("Auricle couldn't open its window: {err}"));
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_theme_name() {
        assert_eq!(normalize_theme_name("Dynamic"), "Dynamic");
        assert_eq!(normalize_theme_name("Default Blue"), "Default Blue");
        assert_eq!(normalize_theme_name("Solar Red"), "Solar Red");
        assert_eq!(normalize_theme_name("Ember"), "Ember");
        assert_eq!(normalize_theme_name("Earth"), "Earth");
        assert_eq!(normalize_theme_name("Forest"), "Forest");
        assert_eq!(normalize_theme_name("Ocean"), "Ocean");
        assert_eq!(normalize_theme_name("Sky"), "Sky");
        assert_eq!(normalize_theme_name("Twilight"), "Twilight");
        assert_eq!(normalize_theme_name("Rose"), "Rose");
        assert_eq!(normalize_theme_name("Mono"), "Mono");

        // Backwards compatibility for unknown / legacy theme names
        assert_eq!(normalize_theme_name("Legacy Theme"), "Default Blue");
        assert_eq!(normalize_theme_name(""), "Default Blue");
    }

    #[test]
    fn test_theme_accent_values() {
        assert_eq!(theme_accent("Dynamic"), slint::Color::from_rgb_u8(0x7c, 0x8c, 0xf8));
        assert_eq!(theme_accent("Default Blue"), slint::Color::from_rgb_u8(0x5b, 0x8d, 0xef));
        assert_eq!(theme_accent("Solar Red"), slint::Color::from_rgb_u8(0xfa, 0x2d, 0x48));
        assert_eq!(theme_accent("Ember"), slint::Color::from_rgb_u8(0xf9, 0x73, 0x16));
        assert_eq!(theme_accent("Earth"), slint::Color::from_rgb_u8(0xc2, 0x78, 0x49));
        assert_eq!(theme_accent("Forest"), slint::Color::from_rgb_u8(0x24, 0xc4, 0x66));
        assert_eq!(theme_accent("Ocean"), slint::Color::from_rgb_u8(0x14, 0xb8, 0xc4));
        assert_eq!(theme_accent("Sky"), slint::Color::from_rgb_u8(0x38, 0xbd, 0xf8));
        assert_eq!(theme_accent("Twilight"), slint::Color::from_rgb_u8(0x9b, 0x6d, 0xff));
        assert_eq!(theme_accent("Rose"), slint::Color::from_rgb_u8(0xec, 0x48, 0x99));
        assert_eq!(theme_accent("Mono"), slint::Color::from_rgb_u8(0x8b, 0x95, 0xa5));
        assert_eq!(theme_accent("Invalid"), slint::Color::from_rgb_u8(0x5b, 0x8d, 0xef));
    }

    #[test]
    fn test_extract_vibrant_accent_grayscale_fallback() {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            64,
            64,
            image::Rgba([128, 128, 128, 255]),
        ));
        let accent = extract_vibrant_accent_from_image(&img);
        assert_eq!(accent, slint::Color::from_rgb_u8(0x8b, 0x95, 0xa5));
    }

    #[test]
    fn test_extract_vibrant_accent_letterbox_rejection() {
        let mut raw = image::RgbaImage::new(100, 100);
        for y in 0..100 {
            for x in 0..100 {
                if y < 12 || y >= 88 {
                    // Top and bottom 12% letterbox bars: neon red
                    raw.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
                } else {
                    // Content area: pure vibrant cyan
                    raw.put_pixel(x, y, image::Rgba([0, 220, 220, 255]));
                }
            }
        }
        let img = image::DynamicImage::ImageRgba8(raw);
        let accent = extract_vibrant_accent_from_image(&img);
        // Letterbox red was ignored, cyan content won
        assert!(accent.blue() > 150);
        assert!(accent.green() > 150);
        assert!(accent.red() < 100);
    }

    #[test]
    fn test_extract_vibrant_accent_complementary_hue_non_muddying() {
        let mut raw = image::RgbaImage::new(100, 100);
        for y in 0..100 {
            for x in 0..100 {
                if y < 60 {
                    // 60% orange
                    raw.put_pixel(x, y, image::Rgba([255, 120, 20, 255]));
                } else {
                    // 40% blue
                    raw.put_pixel(x, y, image::Rgba([20, 120, 255, 255]));
                }
            }
        }
        let img = image::DynamicImage::ImageRgba8(raw);
        let accent = extract_vibrant_accent_from_image(&img);
        // The winning hue bin is orange; hue binning prevents muddy brown averaging
        assert!(accent.red() > accent.blue() + 50);
        assert!(accent.red() > 180);
    }

    #[test]
    fn test_oklab_lerp_endpoints_and_chroma() {
        let solar_red = slint::Color::from_rgb_u8(0xfa, 0x2d, 0x48); // #fa2d48
        let ocean_teal = slint::Color::from_rgb_u8(0x14, 0xb8, 0xc4); // #14b8c4

        // Test t = 0.0 and t = 1.0 round-trip within <= 1 channel unit
        let at_0 = oklab_lerp_color(solar_red, ocean_teal, 0.0);
        let at_1 = oklab_lerp_color(solar_red, ocean_teal, 1.0);
        assert!(colors_close(at_0, solar_red));
        assert!(colors_close(at_1, ocean_teal));

        // Test midpoint (t = 0.5) preserves perceptual chroma/saturation rather than collapsing to muddy mid-gray
        let mid = oklab_lerp_color(solar_red, ocean_teal, 0.5);
        let max_c = mid.red().max(mid.green()).max(mid.blue());
        let min_c = mid.red().min(mid.green()).min(mid.blue());
        assert!(max_c - min_c > 30, "Midpoint must preserve chroma/saturation (spread: {}), not collapse to gray", max_c - min_c);
    }
}
