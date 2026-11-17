use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use ytmapi_rs::common::{AlbumID, ArtistChannelID, VideoID, YoutubeID};
use ytmapi_rs::query::{GetAlbumQuery, GetArtistQuery, GetLyricsIDQuery, GetLyricsQuery};

use crate::core::{bridge::playback_core, menu_metadata, net, playback};
use crate::{ArtistCredit, ArtistItem, NativeShellWindow, SongItem};

static GENERATION: AtomicU64 = AtomicU64::new(0);
static LAST_VIDEO_ID: OnceLock<Mutex<String>> = OnceLock::new();

fn last_video_id() -> &'static Mutex<String> {
    LAST_VIDEO_ID.get_or_init(|| Mutex::new(String::new()))
}

#[derive(Clone, Debug, Default)]
struct CachedLyrics {
    available: bool,
    text: String,
    source: String,
}

static LYRICS_CACHE: OnceLock<Mutex<HashMap<String, CachedLyrics>>> = OnceLock::new();

fn lyrics_cache() -> &'static Mutex<HashMap<String, CachedLyrics>> {
    LYRICS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlainSong {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_str: String,
    pub avatar_letter: String,
    pub duration_secs: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlainArtist {
    pub name: String,
    pub browse_id: String,
    pub subscriber_count: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlainCredit {
    pub name: String,
    pub browse_id: String,
}

#[derive(Clone, Debug, Default)]
struct CachedRelated {
    credits_artists: Vec<PlainCredit>,
    album_title: String,
    album_id: String,
    album_year: String,
    duration_text: String,
    album_songs: Vec<PlainSong>,
    related_songs: Vec<PlainSong>,
    related_artists: Vec<PlainArtist>,
}

static RELATED_CACHE: OnceLock<Mutex<HashMap<String, CachedRelated>>> = OnceLock::new();

fn related_cache() -> &'static Mutex<HashMap<String, CachedRelated>> {
    RELATED_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn format_duration_text(duration_secs: u64) -> String {
    if duration_secs > 0 {
        format!("{}:{:02}", duration_secs / 60, duration_secs % 60)
    } else {
        String::new()
    }
}

pub(crate) fn deduplicate_related_songs(
    current_video_id: &str,
    album_songs: &[PlainSong],
    candidate_songs: Vec<PlainSong>,
    limit: usize,
) -> Vec<PlainSong> {
    let mut out = Vec::new();
    for song in candidate_songs {
        if song.video_id.is_empty()
            || song.video_id == current_video_id
            || album_songs.iter().any(|a| a.video_id == song.video_id)
            || out.iter().any(|s: &PlainSong| s.video_id == song.video_id)
        {
            continue;
        }
        out.push(song);
        if out.len() >= limit {
            break;
        }
    }
    out
}

fn safe_file_stem(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn pick_thumbnail<'a>(
    thumbs: &'a [ytmapi_rs::common::Thumbnail],
) -> Option<&'a ytmapi_rs::common::Thumbnail> {
    const TARGET_PX: u64 = 320;
    thumbs
        .iter()
        .filter(|t| t.width.max(t.height) >= TARGET_PX)
        .min_by_key(|t| t.width.max(t.height))
        .or_else(|| thumbs.iter().max_by_key(|t| t.width.max(t.height)))
}

fn make_song_item_from_plain(s: &PlainSong) -> SongItem {
    let thumb_path = std::env::temp_dir().join(format!("ytm_thumb_{}.jpg", s.video_id));
    let (thumbnail, has_thumbnail) = if thumb_path.exists() {
        slint::Image::load_from_path(&thumb_path)
            .map(|img| (img, true))
            .unwrap_or((slint::Image::default(), false))
    } else {
        (slint::Image::default(), false)
    };

    SongItem {
        video_id: SharedString::from(s.video_id.as_str()),
        title: SharedString::from(s.title.as_str()),
        artist: SharedString::from(s.artist.as_str()),
        album: SharedString::from(s.album.as_str()),
        duration_str: SharedString::from(s.duration_str.as_str()),
        avatar_letter: SharedString::from(s.avatar_letter.as_str()),
        duration_secs: s.duration_secs,
        thumbnail,
        has_thumbnail,
    }
}

fn make_artist_item_from_plain(a: &PlainArtist) -> ArtistItem {
    let safe = safe_file_stem(&a.browse_id);
    let thumb_path = std::env::temp_dir().join(format!("ytm_lib_artist_{}.jpg", safe));
    let (thumbnail, has_thumbnail) = if thumb_path.exists() {
        slint::Image::load_from_path(&thumb_path)
            .map(|img| (img, true))
            .unwrap_or((slint::Image::default(), false))
    } else {
        (slint::Image::default(), false)
    };

    ArtistItem {
        name: SharedString::from(a.name.as_str()),
        browse_id: SharedString::from(a.browse_id.as_str()),
        thumbnail,
        has_thumbnail,
        subscriber_count: SharedString::from(a.subscriber_count.as_str()),
    }
}

fn make_credit_from_plain(c: &PlainCredit) -> ArtistCredit {
    ArtistCredit {
        name: SharedString::from(c.name.as_str()),
        browse_id: SharedString::from(c.browse_id.as_str()),
    }
}

fn apply_lyrics_to_ui(ui: &NativeShellWindow, lyrics: &CachedLyrics) {
    ui.set_np_lyrics_busy(false);
    ui.set_np_lyrics_available(lyrics.available);
    ui.set_np_lyrics_text(SharedString::from(lyrics.text.as_str()));
    ui.set_np_lyrics_source(SharedString::from(lyrics.source.as_str()));
}

fn apply_related_to_ui(ui: &NativeShellWindow, rel: &CachedRelated) {
    let credits: Vec<ArtistCredit> = rel
        .credits_artists
        .iter()
        .map(make_credit_from_plain)
        .collect();
    ui.set_np_credits_artists(ModelRc::new(VecModel::from(credits)));
    ui.set_np_album_title(SharedString::from(rel.album_title.as_str()));
    ui.set_np_album_id(SharedString::from(rel.album_id.as_str()));
    ui.set_np_album_year(SharedString::from(rel.album_year.as_str()));
    ui.set_np_duration_text(SharedString::from(rel.duration_text.as_str()));

    let album_songs: Vec<SongItem> = rel
        .album_songs
        .iter()
        .map(make_song_item_from_plain)
        .collect();
    ui.set_np_album_songs(ModelRc::new(VecModel::from(album_songs)));

    let related_songs: Vec<SongItem> = rel
        .related_songs
        .iter()
        .map(make_song_item_from_plain)
        .collect();
    ui.set_np_related_songs(ModelRc::new(VecModel::from(related_songs)));

    let related_artists: Vec<ArtistItem> = rel
        .related_artists
        .iter()
        .map(make_artist_item_from_plain)
        .collect();
    ui.set_np_related_artists(ModelRc::new(VecModel::from(related_artists)));

    ui.set_np_related_busy(false);
}

fn spawn_thumbnail_download(
    ui_weak: slint::Weak<NativeShellWindow>,
    video_id: String,
    gen: u64,
) {
    let safe_vid = safe_file_stem(&video_id);
    let hq_path = std::env::temp_dir().join(format!("ytm_hqthumb_{}.jpg", safe_vid));
    if hq_path.exists() {
        let ui_w = ui_weak.clone();
        let vid = video_id.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w.upgrade() {
                if GENERATION.load(Ordering::SeqCst) == gen
                    && ui.get_now_playing_video_id().as_str() == vid.as_str()
                {
                    if let Ok(img) = slint::Image::load_from_path(&hq_path) {
                        ui.set_np_highres_art(img);
                        ui.set_np_has_highres_art(true);
                    } else {
                        ui.set_np_has_highres_art(false);
                    }
                }
            }
        })
        .ok();
        return;
    }

    net::spawn(move || {
        let http = net::http();
        let maxres_url = format!("https://i.ytimg.com/vi/{}/maxresdefault.jpg", video_id);
        let hq_url = format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", video_id);

        let mut downloaded_bytes = None;
        if let Ok(resp) = http.get(&maxres_url).send() {
            if resp.status().is_success() {
                if let Ok(bytes) = resp.bytes() {
                    if bytes.len() > 1024 {
                        downloaded_bytes = Some(bytes);
                    }
                }
            }
        }

        if downloaded_bytes.is_none() {
            if let Ok(resp) = http.get(&hq_url).send() {
                if resp.status().is_success() {
                    if let Ok(bytes) = resp.bytes() {
                        if !bytes.is_empty() {
                            downloaded_bytes = Some(bytes);
                        }
                    }
                }
            }
        }

        if let Some(bytes) = downloaded_bytes {
            let _ = std::fs::write(&hq_path, &bytes);
            let ui_w = ui_weak.clone();
            let vid = video_id.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    if GENERATION.load(Ordering::SeqCst) == gen
                        && ui.get_now_playing_video_id().as_str() == vid.as_str()
                    {
                        if let Ok(img) = slint::Image::load_from_path(&hq_path) {
                            ui.set_np_highres_art(img);
                            ui.set_np_has_highres_art(true);
                        } else {
                            ui.set_np_has_highres_art(false);
                        }
                    }
                }
            })
            .ok();
        } else {
            let ui_w = ui_weak.clone();
            let vid = video_id.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_w.upgrade() {
                    if GENERATION.load(Ordering::SeqCst) == gen
                        && ui.get_now_playing_video_id().as_str() == vid.as_str()
                    {
                        ui.set_np_has_highres_art(false);
                    }
                }
            })
            .ok();
        }
    });
}

fn spawn_lyrics_fetch(
    ui_weak: slint::Weak<NativeShellWindow>,
    video_id: String,
    gen: u64,
) {
    if let Some(cached) = lyrics_cache().lock().unwrap().get(&video_id).cloned() {
        let ui_w = ui_weak.clone();
        let vid = video_id.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w.upgrade() {
                if GENERATION.load(Ordering::SeqCst) == gen
                    && ui.get_now_playing_video_id().as_str() == vid.as_str()
                {
                    apply_lyrics_to_ui(&ui, &cached);
                }
            }
        })
        .ok();
        return;
    }

    {
        let ui_w = ui_weak.clone();
        let vid = video_id.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w.upgrade() {
                if GENERATION.load(Ordering::SeqCst) == gen
                    && ui.get_now_playing_video_id().as_str() == vid.as_str()
                {
                    ui.set_np_lyrics_busy(true);
                    ui.set_np_lyrics_available(false);
                    ui.set_np_lyrics_text(SharedString::default());
                    ui.set_np_lyrics_source(SharedString::default());
                }
            }
        })
        .ok();
    }

    net::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };

        let vid_query = video_id.clone();
        let lyrics_result: CachedLyrics = rt.block_on(async {
            let api = match net::ytmusic().await {
                Ok(a) => a,
                Err(_) => {
                    return CachedLyrics {
                        available: false,
                        text: String::new(),
                        source: String::new(),
                    }
                }
            };

            let lyrics_id = match api
                .query(GetLyricsIDQuery::new(VideoID::from_raw(&vid_query)))
                .await
            {
                Ok(id) => id,
                Err(_) => {
                    return CachedLyrics {
                        available: false,
                        text: String::new(),
                        source: String::new(),
                    }
                }
            };

            if lyrics_id.get_raw().is_empty() {
                return CachedLyrics {
                    available: false,
                    text: String::new(),
                    source: String::new(),
                };
            }

            match api.query(GetLyricsQuery::new(lyrics_id)).await {
                Ok(lyrics) => {
                    let text = lyrics.lyrics.trim().to_string();
                    if !text.is_empty() {
                        CachedLyrics {
                            available: true,
                            text: lyrics.lyrics,
                            source: lyrics.source,
                        }
                    } else {
                        CachedLyrics {
                            available: false,
                            text: String::new(),
                            source: String::new(),
                        }
                    }
                }
                Err(_) => CachedLyrics {
                    available: false,
                    text: String::new(),
                    source: String::new(),
                },
            }
        });

        if let Ok(mut cache) = lyrics_cache().lock() {
            cache.insert(video_id.clone(), lyrics_result.clone());
        }

        let ui_w = ui_weak.clone();
        let vid = video_id.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w.upgrade() {
                if GENERATION.load(Ordering::SeqCst) == gen
                    && ui.get_now_playing_video_id().as_str() == vid.as_str()
                {
                    apply_lyrics_to_ui(&ui, &lyrics_result);
                }
            }
        })
        .ok();
    });
}

fn spawn_related_fetch(
    ui_weak: slint::Weak<NativeShellWindow>,
    video_id: String,
    title: String,
    artist: String,
    duration_secs: u64,
    gen: u64,
) {
    if let Some(cached) = related_cache().lock().unwrap().get(&video_id).cloned() {
        let ui_w = ui_weak.clone();
        let vid = video_id.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w.upgrade() {
                if GENERATION.load(Ordering::SeqCst) == gen
                    && ui.get_now_playing_video_id().as_str() == vid.as_str()
                {
                    apply_related_to_ui(&ui, &cached);
                }
            }
        })
        .ok();
        return;
    }

    {
        let ui_w = ui_weak.clone();
        let vid = video_id.clone();
        let dur_text = format_duration_text(duration_secs);
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w.upgrade() {
                if GENERATION.load(Ordering::SeqCst) == gen
                    && ui.get_now_playing_video_id().as_str() == vid.as_str()
                {
                    ui.set_np_related_busy(true);
                    ui.set_np_credits_artists(ModelRc::new(VecModel::default()));
                    ui.set_np_album_title(SharedString::default());
                    ui.set_np_album_id(SharedString::default());
                    ui.set_np_album_year(SharedString::default());
                    ui.set_np_duration_text(SharedString::from(dur_text.as_str()));
                    ui.set_np_album_songs(ModelRc::new(VecModel::default()));
                    ui.set_np_related_songs(ModelRc::new(VecModel::default()));
                    ui.set_np_related_artists(ModelRc::new(VecModel::default()));
                }
            }
        })
        .ok();
    }

    net::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };

        let dur_text = format_duration_text(duration_secs);
        let vid = video_id.clone();
        let ttl = title.clone();
        let art = artist.clone();

        let related_result: CachedRelated = rt.block_on(async {
            let credits = match menu_metadata::resolve_artists("Song", &vid).await {
                Ok(c) if !c.is_empty() => c,
                _ => vec![menu_metadata::ArtistCredit {
                    name: art.clone(),
                    browse_id: String::new(),
                }],
            };

            let plain_credits: Vec<PlainCredit> = credits
                .iter()
                .map(|c| PlainCredit {
                    name: c.name.clone(),
                    browse_id: c.browse_id.clone(),
                })
                .collect();

            let (album_id, album_name) = {
                let known = playback::get_song_meta(&vid).unwrap_or_default();
                if !known.album_id.trim().is_empty() {
                    (known.album_id.trim().to_string(), known.album)
                } else {
                    menu_metadata::resolve_album(&vid, &ttl, &art, "")
                        .await
                        .unwrap_or_default()
                }
            };

            let mut album_title = album_name;
            let mut album_year = String::new();
            let mut album_songs = Vec::new();
            let mut related_songs = Vec::new();
            let mut related_artists = Vec::new();

            let api = net::ytmusic().await.ok();

            if let Some(ref api) = api {
                if !album_id.is_empty() {
                    if let Ok(album) = api
                        .query(GetAlbumQuery::new(AlbumID::from_raw(&album_id)))
                        .await
                    {
                        if album_title.is_empty() {
                            album_title = album.title.clone();
                        }
                        album_year = album.year.clone();

                        let album_artist = album
                            .artists
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");

                        let album_thumb_url = pick_thumbnail(&album.thumbnails)
                            .map(|t| t.url.clone())
                            .unwrap_or_default();

                        if !album_thumb_url.is_empty() {
                            let alb_safe = safe_file_stem(&album_id);
                            let alb_path = std::env::temp_dir()
                                .join(format!("ytm_lib_album_{}.jpg", alb_safe));
                            if !alb_path.exists() {
                                if let Ok(resp) = net::http().get(&album_thumb_url).send() {
                                    if let Ok(bytes) = resp.bytes() {
                                        let _ = std::fs::write(&alb_path, &bytes);
                                    }
                                }
                            }
                        }

                        for track in album.tracks.iter().take(15) {
                            let tr_vid = track.video_id.get_raw().to_string();
                            if tr_vid.is_empty() || tr_vid == vid {
                                continue;
                            }
                            if album_songs.len() >= 12 {
                                break;
                            }
                            let avatar = track
                                .title
                                .chars()
                                .next()
                                .map(|c| c.to_uppercase().to_string())
                                .unwrap_or_else(|| "?".to_string());
                            let dur_secs = parse_duration_str(&track.duration).unwrap_or(0);
                            let dur_str = format_duration_text(dur_secs as u64);

                            let track_thumb = std::env::temp_dir()
                                .join(format!("ytm_thumb_{}.jpg", tr_vid));
                            if !track_thumb.exists() && !album_thumb_url.is_empty() {
                                let alb_safe = safe_file_stem(&album_id);
                                let alb_path = std::env::temp_dir()
                                    .join(format!("ytm_lib_album_{}.jpg", alb_safe));
                                if alb_path.exists() {
                                    let _ = std::fs::copy(&alb_path, &track_thumb);
                                }
                            }

                            album_songs.push(PlainSong {
                                video_id: tr_vid,
                                title: track.title.clone(),
                                artist: if album_artist.is_empty() {
                                    art.clone()
                                } else {
                                    album_artist.clone()
                                },
                                album: album.title.clone(),
                                duration_str: dur_str,
                                avatar_letter: avatar,
                                duration_secs: dur_secs as i32,
                            });
                        }
                    }
                }

                let artist_channel_id = plain_credits
                    .iter()
                    .find(|c| c.browse_id.starts_with("UC"))
                    .map(|c| c.browse_id.clone())
                    .or_else(|| {
                        let meta = playback::get_song_meta(&vid).unwrap_or_default();
                        if meta.artist_id.starts_with("UC") {
                            Some(meta.artist_id)
                        } else {
                            None
                        }
                    });

                if let Some(channel_id) = artist_channel_id {
                    if let Ok(artist_res) = api
                        .query(GetArtistQuery::new(ArtistChannelID::from_raw(&channel_id)))
                        .await
                    {
                        let mut candidate_songs = Vec::new();
                        if let Some(section) = &artist_res.top_releases.songs {
                            for s in &section.results {
                                let s_vid = s.video_id.get_raw().to_string();
                                let avatar = s
                                    .title
                                    .chars()
                                    .next()
                                    .map(|c| c.to_uppercase().to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                let s_artist = s
                                    .artists
                                    .iter()
                                    .map(|a| a.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ");

                                candidate_songs.push(PlainSong {
                                    video_id: s_vid,
                                    title: s.title.clone(),
                                    artist: if s_artist.is_empty() {
                                        artist_res.name.clone()
                                    } else {
                                        s_artist
                                    },
                                    album: s.album.name.clone(),
                                    duration_str: String::new(),
                                    avatar_letter: avatar,
                                    duration_secs: 0,
                                });
                            }
                        }

                        related_songs = deduplicate_related_songs(
                            &vid,
                            &album_songs,
                            candidate_songs,
                            8,
                        );

                        if let Some(section) = &artist_res.top_releases.related {
                            for r in &section.results {
                                if related_artists.len() >= 8 {
                                    break;
                                }
                                related_artists.push(PlainArtist {
                                    name: r.title.clone(),
                                    browse_id: r.browse_id.get_raw().to_string(),
                                    subscriber_count: r.subscribers.clone(),
                                });
                            }
                        }

                        let http = net::http();
                        for song in &related_songs {
                            let thumb_path = std::env::temp_dir()
                                .join(format!("ytm_thumb_{}.jpg", song.video_id));
                            if !thumb_path.exists() {
                                let url = format!(
                                    "https://i.ytimg.com/vi/{}/mqdefault.jpg",
                                    song.video_id
                                );
                                if let Ok(resp) = http.get(&url).send() {
                                    if resp.status().is_success() {
                                        if let Ok(bytes) = resp.bytes() {
                                            let _ = std::fs::write(&thumb_path, &bytes);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            CachedRelated {
                credits_artists: plain_credits,
                album_title,
                album_id,
                album_year,
                duration_text: dur_text,
                album_songs,
                related_songs,
                related_artists,
            }
        });

        if let Ok(mut cache) = related_cache().lock() {
            cache.insert(video_id.clone(), related_result.clone());
        }

        let ui_w = ui_weak.clone();
        let vid = video_id.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w.upgrade() {
                if GENERATION.load(Ordering::SeqCst) == gen
                    && ui.get_now_playing_video_id().as_str() == vid.as_str()
                {
                    apply_related_to_ui(&ui, &related_result);
                }
            }
        })
        .ok();
    });
}

fn parse_duration_str(d: &str) -> Option<u32> {
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

pub fn wire(ui: &NativeShellWindow) {
    let ui_weak = ui.as_weak();
    ui.on_np_tab_changed(move |tab| {
        let Some(ui) = ui_weak.upgrade() else { return };
        ui.set_np_tab(tab);
        let vid = ui.get_now_playing_video_id().to_string();
        if !vid.is_empty() && vid != "native-prototype" {
            let title = ui.get_track_title().to_string();
            let artist = ui.get_track_artist().to_string();
            let duration_secs = playback_core().now_playing().duration_secs;
            let already_loaded = last_video_id().lock().map(|l| l.as_str() == vid.as_str()).unwrap_or(false);
            if !already_loaded {
                notify_track_changed(ui.as_weak(), vid, title, artist, duration_secs);
            }
        }
    });
}

pub fn notify_track_changed(
    ui_weak: slint::Weak<NativeShellWindow>,
    video_id: String,
    title: String,
    artist: String,
    duration_secs: u32,
) {
    let duration_secs = duration_secs as u64;
    if video_id.is_empty() || video_id == "native-prototype" {
        if let Ok(mut last) = last_video_id().lock() {
            *last = String::new();
        }
        let ui_w = ui_weak.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_w.upgrade() {
                ui.set_np_has_highres_art(false);
                ui.set_np_lyrics_busy(false);
                ui.set_np_lyrics_available(false);
                ui.set_np_lyrics_text(SharedString::default());
                ui.set_np_lyrics_source(SharedString::default());
                ui.set_np_related_busy(false);
                ui.set_np_credits_artists(ModelRc::new(VecModel::default()));
                ui.set_np_album_title(SharedString::default());
                ui.set_np_album_id(SharedString::default());
                ui.set_np_album_year(SharedString::default());
                ui.set_np_duration_text(SharedString::default());
                ui.set_np_album_songs(ModelRc::new(VecModel::default()));
                ui.set_np_related_songs(ModelRc::new(VecModel::default()));
                ui.set_np_related_artists(ModelRc::new(VecModel::default()));
            }
        })
        .ok();
        return;
    }

    let gen = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut last) = last_video_id().lock() {
        *last = video_id.clone();
    }

    spawn_thumbnail_download(ui_weak.clone(), video_id.clone(), gen);
    spawn_lyrics_fetch(ui_weak.clone(), video_id.clone(), gen);
    spawn_related_fetch(
        ui_weak,
        video_id,
        title,
        artist,
        duration_secs,
        gen,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_duration_text() {
        assert_eq!(format_duration_text(0), "");
        assert_eq!(format_duration_text(45), "0:45");
        assert_eq!(format_duration_text(75), "1:15");
        assert_eq!(format_duration_text(3600), "60:00");
    }

    #[test]
    fn test_deduplicate_related_songs() {
        let current_vid = "song1";
        let album_songs = vec![
            PlainSong {
                video_id: "song2".into(),
                title: "Track 2".into(),
                ..Default::default()
            },
        ];
        let candidates = vec![
            PlainSong {
                video_id: "song1".into(), // matches current
                title: "Track 1".into(),
                ..Default::default()
            },
            PlainSong {
                video_id: "song2".into(), // matches album song
                title: "Track 2".into(),
                ..Default::default()
            },
            PlainSong {
                video_id: "song3".into(),
                title: "Track 3".into(),
                ..Default::default()
            },
            PlainSong {
                video_id: "song3".into(), // duplicate in candidates
                title: "Track 3 duplicate".into(),
                ..Default::default()
            },
            PlainSong {
                video_id: "song4".into(),
                title: "Track 4".into(),
                ..Default::default()
            },
        ];

        let result = deduplicate_related_songs(current_vid, &album_songs, candidates, 8);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].video_id, "song3");
        assert_eq!(result[1].video_id, "song4");
    }
}
