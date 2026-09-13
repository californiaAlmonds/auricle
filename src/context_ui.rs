use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use ytmapi_rs::common::{AlbumID, YoutubeID};
use ytmapi_rs::parse::{GetAlbum, ParsedSongArtist};
use ytmapi_rs::query::{search::ArtistsFilter, GetAlbumQuery, SearchQuery};
use ytmapi_rs::YtMusic;

use crate::core::{bridge::playback_core, library, menu_metadata, net, playback};
use crate::NativeShellWindow;
use menu_metadata::ArtistCredit;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    generation: i32,
    kind: String,
    id: String,
    view: String,
}

impl Target {
    fn capture(ui: &NativeShellWindow) -> Self {
        Self {
            generation: ui.get_ctx_generation(),
            kind: ui.get_ctx_kind().to_string(),
            id: ui.get_ctx_id().to_string(),
            view: ui.get_current_view().to_string(),
        }
    }

    fn matches(&self, current: &Self) -> bool {
        self == current
    }
}

pub(crate) fn invalidate(ui: &NativeShellWindow) {
    ui.set_ctx_generation(ui.get_ctx_generation().wrapping_add(1));
    ui.set_ctx_artists_loading(false);
    ui.set_ctx_status("".into());
}

fn begin_action(ui: &NativeShellWindow) -> Target {
    invalidate(ui);
    Target::capture(ui)
}

fn background<Output, Work, Finish>(ui: &NativeShellWindow, target: Target, work: Work, finish: Finish)
where
    Output: Send + 'static,
    Work: Future<Output = Result<Output, &'static str>> + Send + 'static,
    Finish: FnOnce(&NativeShellWindow, Result<Output, &'static str>) + Send + 'static,
{
    let weak = ui.as_weak();
    let started = Instant::now();
    net::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread().enable_all().build()
            .map_err(|_| "Menu service unavailable. Please try again.")
            .and_then(|runtime| runtime.block_on(async {
                let remaining = Duration::from_secs(20).saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err("Request timed out. Please try again.");
                }
                tokio::time::timeout(remaining, work).await
                    .map_err(|_| "Request timed out. Please try again.")?
            }));
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                if target.matches(&Target::capture(&ui)) {
                    finish(&ui, result);
                }
            }
        });
    });
}

#[derive(Clone, Default)]
struct Entity {
    kind: String,
    id: String,
    title: String,
    artist: String,
    thumbnail_url: String,
}

static ENTITIES: OnceLock<Mutex<VecDeque<Entity>>> = OnceLock::new();

pub(crate) fn register_entity(kind: &str, id: &str, title: &str, artist: &str, thumbnail_url: &str) {
    if !matches!(kind, "Album" | "Artist") || id.trim().is_empty() {
        return;
    }
    let Ok(mut entries) = ENTITIES.get_or_init(Default::default).lock() else { return };
    let mut entity = entries.iter().position(|entry| entry.kind == kind && entry.id == id)
        .and_then(|index| entries.remove(index)).unwrap_or_default();
    entity.kind = kind.into();
    entity.id = id.into();
    if !title.is_empty() { entity.title = title.into(); }
    if !artist.is_empty() { entity.artist = artist.into(); }
    if !thumbnail_url.is_empty() { entity.thumbnail_url = thumbnail_url.into(); }
    entries.push_back(entity);
    while entries.len() > 128 { entries.pop_front(); }
}

fn known_entity(kind: &str, id: &str, title: &str, artist: &str) -> Entity {
    register_entity(kind, id, title, artist, "");
    ENTITIES.get_or_init(Default::default).lock().ok()
        .and_then(|entries| entries.iter().find(|entry| entry.kind == kind && entry.id == id).cloned())
        .unwrap_or_else(|| Entity { kind: kind.into(), id: id.into(), title: title.into(), artist: artist.into(), ..Default::default() })
}

pub(crate) fn credits(artists: &[ParsedSongArtist]) -> Vec<ArtistCredit> {
    artists.iter().map(|artist| ArtistCredit {
        name: artist.name.clone(),
        browse_id: artist.id.as_ref().map(|id| id.get_raw().to_string()).unwrap_or_default(),
    }).collect()
}

pub(crate) fn credit_names(credits: &[ArtistCredit]) -> String {
    credits.iter().map(|credit| credit.name.as_str()).filter(|name| !name.is_empty()).collect::<Vec<_>>().join(", ")
}

pub(crate) fn register_album(id: &str, album: &GetAlbum) {
    let artists = credits(&album.artists);
    let artist_id = artists.first().map(|artist| artist.browse_id.as_str()).unwrap_or("");
    menu_metadata::register_artists("Album", id, artists.clone());
    register_entity("Album", id, &album.title, &credit_names(&artists),
        crate::pick_thumb(&album.thumbnails).map(|thumb| thumb.url.as_str()).unwrap_or(""));
    for track in &album.tracks {
        playback::register_song_meta(track.video_id.get_raw(), &album.title, id, artist_id);
    }
}

fn show_credits(ui: &NativeShellWindow, artists: Vec<ArtistCredit>) {
    ui.set_ctx_artists(ModelRc::new(VecModel::from(artists.into_iter().map(|artist| crate::ArtistCredit {
        name: artist.name.into(), browse_id: artist.browse_id.into(),
    }).collect::<Vec<_>>())));
    ui.set_ctx_artists_loading(false);
    ui.set_ctx_artists_error("".into());
}

fn saved(kind: &str, id: &str) -> bool {
    match kind {
        "Artist" => library::is_artist_followed(id),
        "Album" => library::is_album_saved(id),
        _ => false,
    }
}

async fn resolve_artist(name: String) -> Result<String, &'static str> {
    let api = YtMusic::new_unauthenticated().await.map_err(|_| "Artist lookup unavailable. Please try again.")?;
    let results = api.query(SearchQuery::new(name.clone()).with_filter(ArtistsFilter)).await
        .map_err(|_| "Artist lookup unavailable. Please try again.")?;
    let id = unique_artist(&name, results.iter().map(|artist| (artist.artist.as_str(), artist.browse_id.get_raw())))?;
    if let Some(artist) = results.iter().find(|artist| artist.browse_id.get_raw() == id) {
        register_entity("Artist", &id, &artist.artist, "",
            crate::pick_thumb(&artist.thumbnails).map(|thumb| thumb.url.as_str()).unwrap_or(""));
    }
    Ok(id)
}

fn selected_artist_id(ui: &NativeShellWindow, id: &str, name: &str) -> String {
    if !id.trim().is_empty() { return id.to_string(); }
    let artists = ui.get_ctx_artists().iter().collect::<Vec<_>>();
    unique_artist(name, artists.iter().map(|artist| (artist.name.as_str(), artist.browse_id.as_str())))
        .unwrap_or_default()
}

#[derive(Clone)]
struct Views {
    artist: Arc<Mutex<library::SavedArtist>>,
    album: Arc<Mutex<library::SavedAlbum>>,
}

fn save_entity(ui: &NativeShellWindow, target: Target, mut entity: Entity, views: Views) {
    if entity.id.is_empty() { return; }
    let expected = saved(&entity.kind, &entity.id);
    ui.set_ctx_status("Updating library...".into());
    let weak = ui.as_weak();
    let started = Instant::now();
    net::spawn(move || {
        if started.elapsed() >= Duration::from_secs(20) {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if target.matches(&Target::capture(&ui)) {
                        ui.set_ctx_status("Request timed out. Please try again.".into());
                    }
                }
            });
            return;
        }
        if saved(&entity.kind, &entity.id) == expected {
            match entity.kind.as_str() {
                "Artist" => {
                    if entity.thumbnail_url.is_empty() {
                        entity.thumbnail_url = views.artist.lock().ok().filter(|artist| artist.browse_id == entity.id)
                            .map(|artist| artist.thumbnail_url.clone()).unwrap_or_default();
                    }
                    library::toggle_follow_artist(library::SavedArtist {
                        browse_id: entity.id.clone(), name: entity.title.clone(), thumbnail_url: entity.thumbnail_url,
                    });
                }
                "Album" => {
                    if entity.thumbnail_url.is_empty() {
                        entity.thumbnail_url = views.album.lock().ok().filter(|album| album.browse_id == entity.id)
                            .map(|album| album.thumbnail_url.clone()).unwrap_or_default();
                    }
                    library::toggle_save_album(library::SavedAlbum {
                        browse_id: entity.id.clone(), title: entity.title.clone(), artist: entity.artist.clone(), thumbnail_url: entity.thumbnail_url,
                    });
                }
                _ => return,
            }
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                crate::refresh_sidebar(&ui);
                ui.set_sidebar_albums(ModelRc::new(VecModel::from(crate::compute_sidebar_albums())));
                if ui.get_current_view() == "Library" { ui.invoke_load_library_data(); }
                let now_saved = saved(&entity.kind, &entity.id);
                if entity.kind == "Artist" && views.artist.lock().ok().is_some_and(|artist| artist.browse_id == entity.id) {
                    ui.set_artist_view_subscribed(now_saved);
                }
                if entity.kind == "Album" && views.album.lock().ok().is_some_and(|album| album.browse_id == entity.id) {
                    ui.set_album_view_liked(now_saved);
                }
                if target.matches(&Target::capture(&ui)) {
                    ui.set_ctx_saved(now_saved);
                    ui.set_ctx_status("".into());
                }
            }
        });
    });
}

fn album_tracks(album: &mut GetAlbum, id: &str) -> Vec<playback::NowPlaying> {
    register_album(id, album);
    album.tracks.sort_by_key(|track| track.track_no);
    let artist = credit_names(&credits(&album.artists));
    album.tracks.iter().filter(|track| !track.video_id.get_raw().is_empty()).map(|track| {
        playback::NowPlaying::enriched(track.video_id.get_raw().into(), track.title.clone(), artist.clone(),
            crate::parse_duration(&track.duration).unwrap_or(0))
    }).collect()
}

pub(crate) fn wire(ui: &NativeShellWindow, artist: Arc<Mutex<library::SavedArtist>>, album: Arc<Mutex<library::SavedAlbum>>) {
    let views = Views { artist, album };
    let weak = ui.as_weak();
    ui.on_prepare_context(move |kind, id, title, _artist, _album, generation| {
        let Some(ui) = weak.upgrade() else { return };
        let target = Target::capture(&ui);
        if target.generation != generation || target.kind != kind.as_str() || target.id != id.as_str() { return; }
        ui.set_ctx_status("".into());
        ui.set_ctx_artists_error("".into());
        ui.set_ctx_saved(saved(&kind, &id));
        if kind == "Artist" {
            show_credits(&ui, vec![ArtistCredit { name: title.to_string(), browse_id: id.to_string() }]);
            if !id.is_empty() {
                menu_metadata::register_artists("Artist", &id, vec![ArtistCredit { name: title.to_string(), browse_id: id.to_string() }]);
            } else {
                ui.set_ctx_artists_loading(true);
                background(&ui, target, resolve_artist(title.to_string()), move |ui, result| {
                    ui.set_ctx_artists_loading(false);
                    match result {
                        Ok(id) => {
                            let artists = vec![ArtistCredit { name: title.to_string(), browse_id: id.clone() }];
                            menu_metadata::register_artists("Artist", &id, artists.clone());
                            ui.set_ctx_saved(saved("Artist", &id));
                            show_credits(ui, artists);
                        }
                        Err(message) => {
                            ui.set_ctx_artists_error(message.into());
                            ui.set_ctx_status(message.into());
                        }
                    }
                });
            }
            return;
        }
        if let Some(artists) = menu_metadata::cached_artists(&kind, &id).filter(|artists| !artists.is_empty()) {
            show_credits(&ui, artists);
            return;
        }
        ui.set_ctx_artists_loading(true);
        background(&ui, target, async move {
            menu_metadata::resolve_artists(&kind, &id).await
                .map_err(|_| "Artist credits unavailable. Please try again.")
        }, |ui, result| {
            ui.set_ctx_artists_loading(false);
            match result {
                Ok(artists) if !artists.is_empty() => show_credits(ui, artists),
                Ok(_) => ui.set_ctx_artists_error("No artist credits found.".into()),
                Err(message) => ui.set_ctx_artists_error(message.into()),
            }
        });
    });

    let weak = ui.as_weak();
    ui.on_context_artist(move |id, name| {
        let Some(ui) = weak.upgrade() else { return };
        if !ui.get_ctx_artists().iter().any(|credit| credit.browse_id == id && credit.name == name) { return; }
        let target = begin_action(&ui);
        if !id.trim().is_empty() {
            ui.invoke_navigate_to_artist(id);
        } else {
            ui.set_ctx_status("Resolving artist...".into());
            background(&ui, target, resolve_artist(name.to_string()), |ui, result| match result {
                Ok(id) => { ui.set_ctx_status("".into()); ui.invoke_navigate_to_artist(id.into()); }
                Err(message) => ui.set_ctx_status(message.into()),
            });
        }
    });

    let weak = ui.as_weak();
    ui.on_context_album(move |video_id, title, artist, album| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_ctx_kind() != "Song" || ui.get_ctx_id() != video_id || ui.get_ctx_video_id() != video_id { return; }
        let target = begin_action(&ui);
        if let Some(meta) = playback::get_song_meta(&video_id).filter(|meta| !meta.album_id.is_empty()) {
            ui.invoke_navigate_to_album(meta.album_id.into());
            return;
        }
        ui.set_ctx_status("Resolving album...".into());
        background(&ui, target, async move {
            menu_metadata::resolve_album(&video_id, &title, &artist, &album).await
                .map_err(|_| "Album unavailable or could not be identified. Please try again.")
        }, |ui, result| match result {
            Ok((id, _)) => { ui.set_ctx_status("".into()); ui.invoke_navigate_to_album(id.into()); }
            Err(message) => ui.set_ctx_status(message.into()),
        });
    });

    let weak = ui.as_weak();
    ui.on_context_action(move |action, kind, id, title, artist| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_ctx_kind() != kind || ui.get_ctx_id() != id || !matches!(kind.as_str(), "Artist" | "Album") { return; }
        let id = if kind == "Artist" { selected_artist_id(&ui, &id, &title).into() } else { id };
        let target = begin_action(&ui);
        match action.as_str() {
            "open" if kind == "Artist" => {
                if !id.trim().is_empty() { ui.invoke_navigate_to_artist(id); }
                else {
                    ui.set_ctx_status("Resolving artist...".into());
                    background(&ui, target, resolve_artist(title.to_string()), |ui, result| match result {
                        Ok(id) => { ui.set_ctx_status("".into()); ui.invoke_navigate_to_artist(id.into()); }
                        Err(message) => ui.set_ctx_status(message.into()),
                    });
                }
            }
            "open" if !id.trim().is_empty() => ui.invoke_navigate_to_album(id),
            "save" => {
                if id.trim().is_empty() && kind == "Artist" {
                    ui.set_ctx_status("Resolving artist...".into());
                    let views = views.clone();
                    let save_target = target.clone();
                    background(&ui, target, resolve_artist(title.to_string()), move |ui, result| match result {
                        Ok(id) => save_entity(ui, save_target, known_entity("Artist", &id, &title, ""), views),
                        Err(message) => ui.set_ctx_status(message.into()),
                    });
                } else if !id.trim().is_empty() {
                    save_entity(&ui, target, known_entity(&kind, &id, &title, &artist), views.clone());
                } else { ui.set_ctx_status("Album ID unavailable.".into()); }
            }
            "play" | "queue" if kind == "Album" && !id.trim().is_empty() => {
                ui.set_ctx_status("Loading album...".into());
                background(&ui, target, async move {
                    let api = YtMusic::new_unauthenticated().await.map_err(|_| "Album unavailable. Please try again.")?;
                    let mut album = api.query(GetAlbumQuery::new(AlbumID::from_raw(id.as_str()))).await
                        .map_err(|_| "Album unavailable. Please try again.")?;
                    let songs = album_tracks(&mut album, &id);
                    if songs.is_empty() { return Err("No playable tracks found on this album."); }
                    Ok(songs)
                }, move |ui, result| match result {
                    Ok(songs) => {
                        let playback = playback_core();
                        if action == "queue" {
                            playback.extend_queue(songs);
                            ui.set_ctx_status("Album added to queue.".into());
                        } else {
                            playback.set_queue(songs);
                            playback.state_lock().is_playing = true;
                            let first = playback.now_playing();
                            playback.set_now_playing(&first.video_id, &first.title, &first.artist, first.duration_secs);
                            ui.set_ctx_status("".into());
                        }
                        crate::refresh_native_shell_ui(ui, playback);
                    }
                    Err(message) => ui.set_ctx_status(message.into()),
                });
            }
            _ => ui.set_ctx_status("This action is unavailable.".into()),
        }
    });
}

fn normalized_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn unique_artist<'entry>(
    name: &str,
    candidates: impl IntoIterator<Item = (&'entry str, &'entry str)>,
) -> Result<String, &'static str> {
    let name = normalized_name(name);
    if name.is_empty() {
        return Err("Artist name unavailable.");
    }
    let mut selected: Option<String> = None;
    for (candidate_name, id) in candidates {
        let id = id.trim();
        if id.is_empty() || normalized_name(candidate_name) != name {
            continue;
        }
        match selected.as_deref() {
            Some(known) if known != id => return Err("More than one artist matches this name."),
            Some(_) => {}
            None => selected = Some(id.to_string()),
        }
    }
    selected.ok_or("No exact artist match found.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target { generation: 4, kind: "Song".into(), id: "video".into(), view: "Search".into() }
    }

    #[test]
    fn accepts_only_the_original_action_target() {
        let original = target();
        assert!(original.matches(&target()));
        for changed in [
            Target { generation: 5, ..target() },
            Target { kind: "Album".into(), ..target() },
            Target { id: "other".into(), ..target() },
            Target { view: "Home".into(), ..target() },
        ] {
            assert!(!original.matches(&changed));
        }
    }

    #[test]
    fn navigation_away_and_back_invalidates_the_same_menu() {
        assert!(!target().matches(&Target { generation: 6, ..target() }));
    }

    #[test]
    fn exact_artist_match_ignores_wrong_first_result_and_duplicate_ids() {
        assert_eq!(unique_artist("  ALICE  Smith ", [
            ("Alice Jones", "wrong"), ("Alice Smith", "right"), ("alice  smith", "right"),
        ]), Ok("right".into()));
    }

    #[test]
    fn ambiguous_missing_and_combined_names_are_not_guessed() {
        assert!(unique_artist("Alice", [("Alice", "one"), ("ALICE", "two")]).is_err());
        assert!(unique_artist("Alice", [("Alice", "")]).is_err());
        assert!(unique_artist("", [("", "one")]).is_err());
        assert!(unique_artist("Alice, Bob", [("Alice", "one"), ("Bob", "two")]).is_err());
        assert_eq!(unique_artist("Earth, Wind & Fire", [("Earth, Wind & Fire", "band")]), Ok("band".into()));
    }
}