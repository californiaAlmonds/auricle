use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;
use ytmapi_rs::auth::noauth::NoAuthToken;
use ytmapi_rs::common::{AlbumID, VideoID, YoutubeID};
use ytmapi_rs::parse::{ParsedSongArtist, SearchResultAlbum, SearchResultSong};
use ytmapi_rs::query::search::{AlbumsFilter, SongsFilter};
use ytmapi_rs::query::{GetAlbumQuery, GetWatchPlaylistQuery, SearchQuery};
use ytmapi_rs::YtMusic;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtistCredit {
    pub name: String,
    pub browse_id: String,
}

const CACHE_LIMIT: usize = 128;
static ARTISTS: OnceLock<Mutex<ArtistCache>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Song,
    Album,
    Artist,
}

impl Kind {
    fn parse(kind: &str) -> Option<Self> {
        match kind.trim().to_ascii_lowercase().as_str() {
            "song" => Some(Self::Song),
            "album" => Some(Self::Album),
            "artist" => Some(Self::Artist),
            _ => None,
        }
    }
}

#[derive(Default)]
struct ArtistCache {
    entries: VecDeque<(Kind, String, Vec<ArtistCredit>)>,
}

impl ArtistCache {
    fn get(&mut self, kind: Kind, id: &str) -> Option<Vec<ArtistCredit>> {
        let position = self
            .entries
            .iter()
            .position(|entry| entry.0 == kind && entry.1 == id)?;
        let entry = self.entries.remove(position)?;
        let credits = entry.2.clone();
        self.entries.push_back(entry);
        Some(credits)
    }

    fn insert(&mut self, kind: Kind, id: &str, credits: Vec<ArtistCredit>) {
        let mut merged = self.get(kind, id).unwrap_or_default();
        merge_credits(&mut merged, credits);
        if merged.is_empty() {
            return;
        }
        if let Some(entry) = self
            .entries
            .back_mut()
            .filter(|entry| entry.0 == kind && entry.1 == id)
        {
            entry.2 = merged;
            return;
        }
        self.entries.push_back((kind, id.to_string(), merged));
        while self.entries.len() > CACHE_LIMIT {
            self.entries.pop_front();
        }
    }
}

fn artist_cache() -> &'static Mutex<ArtistCache> {
    ARTISTS.get_or_init(|| Mutex::new(ArtistCache::default()))
}

fn normalized_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn merge_credits(existing: &mut Vec<ArtistCredit>, credits: Vec<ArtistCredit>) {
    for credit in credits {
        let credit = ArtistCredit {
            name: credit.name.trim().to_string(),
            browse_id: credit.browse_id.trim().to_string(),
        };
        if credit.name.is_empty() && credit.browse_id.is_empty() {
            continue;
        }
        let matching = existing.iter_mut().find(|known| {
            if !credit.browse_id.is_empty() {
                known.browse_id == credit.browse_id
            } else {
                normalized_name(&known.name) == normalized_name(&credit.name)
            }
        });
        if let Some(known) = matching {
            if known.name.is_empty() && !credit.name.is_empty() {
                known.name = credit.name;
            }
        } else {
            existing.push(credit);
        }
    }
    let named_ids: Vec<String> = existing
        .iter()
        .filter(|credit| !credit.browse_id.is_empty() && !credit.name.is_empty())
        .map(|credit| normalized_name(&credit.name))
        .collect();
    existing.retain(|credit| {
        !credit.browse_id.is_empty() || !named_ids.contains(&normalized_name(&credit.name))
    });
}

fn parsed_credits(artists: &[ParsedSongArtist]) -> Vec<ArtistCredit> {
    let mut credits = Vec::new();
    merge_credits(
        &mut credits,
        artists
            .iter()
            .map(|artist| ArtistCredit {
                name: artist.name.clone(),
                browse_id: artist
                    .id
                    .as_ref()
                    .map(|id| id.get_raw().to_string())
                    .unwrap_or_default(),
            })
            .collect(),
    );
    credits
}

/// Merge structured API credits into the session-only, 128-entry LRU.
/// Kinds are Song, Album, or Artist (case-insensitive); IDs are case-sensitive.
pub fn register_artists(kind: &str, id: &str, credits: Vec<ArtistCredit>) {
    let Some(kind) = Kind::parse(kind) else {
        return;
    };
    let id = id.trim();
    if id.is_empty() || id == "native-prototype" {
        return;
    }
    if let Ok(mut cache) = artist_cache().lock() {
        cache.insert(kind, id, credits);
    }
}

/// Read and touch the LRU without network access. A blank browse_id is not navigable.
pub fn cached_artists(kind: &str, id: &str) -> Option<Vec<ArtistCredit>> {
    artist_cache()
        .lock()
        .ok()?
        .get(Kind::parse(kind)?, id.trim())
}

#[derive(Default)]
struct WatchMetadata {
    title: String,
    artists: Vec<ArtistCredit>,
    albums: Vec<(String, String)>,
}

fn exact_watch_rows<'response>(
    value: &'response Value,
    video_id: &str,
    rows: &mut Vec<&'response Value>,
) {
    match value {
        Value::Object(object) => {
            if let Some(row) = object.get("playlistPanelVideoRenderer") {
                if row.get("videoId").and_then(Value::as_str) == Some(video_id) {
                    rows.push(row);
                }
            }
            for child in object.values() {
                exact_watch_rows(child, video_id, rows);
            }
        }
        Value::Array(array) => {
            for child in array {
                exact_watch_rows(child, video_id, rows);
            }
        }
        _ => {}
    }
}

fn text_runs(value: &Value) -> String {
    if let Some(text) = value.get("simpleText").and_then(Value::as_str) {
        return text.to_string();
    }
    value
        .get("runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|run| run.get("text").and_then(Value::as_str))
        .collect()
}

fn parse_watch_metadata(response: &Value, video_id: &str) -> Result<WatchMetadata, String> {
    if video_id.is_empty() {
        return Err("Missing song video ID".into());
    }
    let mut rows = Vec::new();
    exact_watch_rows(response, video_id, &mut rows);
    if rows.is_empty() {
        return Err(format!(
            "Watch metadata did not contain requested video {video_id}"
        ));
    }
    let mut metadata = WatchMetadata::default();
    for row in rows {
        if metadata.title.is_empty() {
            metadata.title = text_runs(&row["title"]);
        }
        for field in ["longBylineText", "shortBylineText"] {
            let Some(runs) = row[field].get("runs").and_then(Value::as_array) else {
                continue;
            };
            let mut in_artist_section = true;
            for run in runs {
                let name = run.get("text").and_then(Value::as_str).unwrap_or("").trim();
                let endpoint = run.pointer("/navigationEndpoint/browseEndpoint");
                let browse_id = endpoint
                    .and_then(|endpoint| endpoint.get("browseId"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let page_type = endpoint
                    .and_then(|endpoint| endpoint.pointer(
                        "/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType",
                    ))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if page_type == "MUSIC_PAGE_TYPE_ALBUM"
                    || (page_type.is_empty() && browse_id.starts_with("MPRE"))
                {
                    metadata
                        .albums
                        .push((browse_id.to_string(), name.to_string()));
                    in_artist_section = false;
                } else if page_type == "MUSIC_PAGE_TYPE_ARTIST"
                    || (page_type.is_empty() && browse_id.starts_with("UC"))
                {
                    merge_credits(
                        &mut metadata.artists,
                        vec![ArtistCredit {
                            name: name.to_string(),
                            browse_id: browse_id.to_string(),
                        }],
                    );
                } else if matches!(name, "\u{2022}" | "\u{b7}") {
                    in_artist_section = false;
                } else if field == "longBylineText"
                    && in_artist_section
                    && endpoint.is_none()
                    && !matches!(name, "" | "," | "&" | ";" | "/")
                {
                    merge_credits(
                        &mut metadata.artists,
                        vec![ArtistCredit {
                            name: name.to_string(),
                            browse_id: String::new(),
                        }],
                    );
                }
            }
        }
    }
    Ok(metadata)
}

async fn watch_metadata(
    api: &YtMusic<NoAuthToken>,
    video_id: &str,
) -> Result<WatchMetadata, String> {
    let response = api
        .json_query(GetWatchPlaylistQuery::new_from_video_id(VideoID::from_raw(
            video_id,
        )))
        .await
        .map_err(|error| format!("Watch metadata: {error}"))?;
    let response = serde_json::to_value(response).map_err(|error| error.to_string())?;
    parse_watch_metadata(&response, video_id)
}

fn artist_result(
    kind: &str,
    id: &str,
    result: Result<Vec<ArtistCredit>, String>,
) -> Result<Vec<ArtistCredit>, String> {
    let error = match result {
        Ok(credits) => {
            register_artists(kind, id, credits);
            format!("No artist credits available for {kind} {id}")
        }
        Err(error) => error,
    };
    cached_artists(kind, id).ok_or(error)
}

/// Refresh from one API client, merging registrations; use cached credits on failure.
/// Artist requests identify the artist itself without network access. Callers own timeouts.
pub async fn resolve_artists(kind: &str, id: &str) -> Result<Vec<ArtistCredit>, String> {
    let parsed_kind =
        Kind::parse(kind).ok_or_else(|| format!("Unsupported metadata kind: {kind}"))?;
    let id = id.trim();
    if id.is_empty() || id == "native-prototype" {
        return Err(format!("Missing {kind} ID"));
    }
    if parsed_kind == Kind::Artist {
        let known = cached_artists(kind, id).unwrap_or_default();
        let name = known
            .iter()
            .find(|credit| credit.browse_id == id && !credit.name.is_empty())
            .or_else(|| known.iter().find(|credit| credit.browse_id.is_empty()))
            .map(|credit| credit.name.clone())
            .unwrap_or_default();
        let credits = vec![ArtistCredit {
            name,
            browse_id: id.to_string(),
        }];
        register_artists(kind, id, credits.clone());
        return Ok(credits);
    }
    let result = async {
        let api = YtMusic::new_unauthenticated()
            .await
            .map_err(|error| format!("Artist metadata client: {error}"))?;
        match parsed_kind {
            Kind::Song => Ok(watch_metadata(&api, id).await?.artists),
            Kind::Album => {
                let album = api
                    .query(GetAlbumQuery::new(AlbumID::from_raw(id)))
                    .await
                    .map_err(|error| format!("Album artist credits: {error}"))?;
                Ok(parsed_credits(&album.artists))
            }
            Kind::Artist => unreachable!(),
        }
    }
    .await;
    artist_result(kind, id, result)
}

fn unique_album<'metadata>(
    candidates: impl IntoIterator<Item = (&'metadata str, &'metadata str)>,
) -> Result<Option<(String, String)>, String> {
    let mut selected: Option<(String, String)> = None;
    for (id, name) in candidates {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        match selected.as_mut() {
            Some((known_id, _)) if known_id != id => {
                return Err("Album metadata is ambiguous; refusing to choose an edition".into());
            }
            Some((_, known_name)) => {
                if known_name.is_empty() {
                    *known_name = name.trim().to_string();
                }
            }
            None => selected = Some((id.to_string(), name.trim().to_string())),
        }
    }
    Ok(selected)
}

fn exact_song_album(
    songs: &[SearchResultSong],
    video_id: &str,
) -> Result<Option<(String, String)>, String> {
    unique_album(
        songs
            .iter()
            .filter(|song| song.video_id.get_raw() == video_id)
            .filter_map(|song| song.album.as_ref())
            .map(|album| (album.id.get_raw(), album.name.as_str())),
    )
}

fn matching_album(
    albums: &[SearchResultAlbum],
    title: &str,
    artist: &str,
) -> Result<(String, String), String> {
    let title = normalized_name(title);
    let artist = normalized_name(artist);
    if title.is_empty() || artist.is_empty() {
        return Err("Album lookup requires both an album name and artist".into());
    }
    unique_album(
        albums
            .iter()
            .filter(|album| {
                normalized_name(&album.title) == title && normalized_name(&album.artist) == artist
            })
            .map(|album| (album.album_id.get_raw(), album.title.as_str())),
    )?
    .ok_or_else(|| "No unambiguous album matched the exact album name and artist".into())
}

fn remember_album(video_id: &str, resolved: (String, String)) -> (String, String) {
    super::playback::register_song_meta(video_id, &resolved.1, &resolved.0, "");
    resolved
}

/// Return (album browse ID, album name). Uses playback metadata, exact video watch
/// metadata, exact video song search, then an unambiguous album-name/artist search.
/// Only this song is resolved; there is no backfill, persistence, or internal timeout.
pub async fn resolve_album(
    video_id: &str,
    title: &str,
    artist: &str,
    album: &str,
) -> Result<(String, String), String> {
    let video_id = video_id.trim();
    if video_id.is_empty() || video_id == "native-prototype" {
        return Err("Missing song video ID".into());
    }
    let known = super::playback::get_song_meta(video_id).unwrap_or_default();
    if !known.album_id.trim().is_empty() {
        return Ok((
            known.album_id.trim().to_string(),
            if known.album.trim().is_empty() {
                album.trim().to_string()
            } else {
                known.album
            },
        ));
    }
    let api = YtMusic::new_unauthenticated()
        .await
        .map_err(|error| format!("Album metadata client: {error}"))?;
    let mut errors = Vec::new();
    let watch = match watch_metadata(&api, video_id).await {
        Ok(watch) => {
            register_artists("Song", video_id, watch.artists.clone());
            if let Some(resolved) = unique_album(
                watch
                    .albums
                    .iter()
                    .map(|(id, name)| (id.as_str(), name.as_str())),
            )? {
                return Ok(remember_album(video_id, resolved));
            }
            watch
        }
        Err(error) => {
            errors.push(error);
            WatchMetadata::default()
        }
    };
    let title = if title.trim().is_empty() {
        watch.title.as_str()
    } else {
        title.trim()
    };
    let watch_artist = watch
        .artists
        .iter()
        .map(|credit| credit.name.as_str())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let artist = if artist.trim().is_empty() {
        watch_artist.as_str()
    } else {
        artist.trim()
    };
    if !title.is_empty() {
        match api
            .query(SearchQuery::new(format!("{title} {artist}")).with_filter(SongsFilter))
            .await
        {
            Ok(songs) => {
                if let Some(resolved) = exact_song_album(&songs, video_id)? {
                    return Ok(remember_album(video_id, resolved));
                }
                errors.push("Song search had no album for the exact requested video".into());
            }
            Err(error) => errors.push(format!("Song album search: {error}")),
        }
    }
    let album = if !album.trim().is_empty() {
        album.trim()
    } else if !known.album.trim().is_empty() {
        known.album.trim()
    } else {
        watch
            .albums
            .iter()
            .map(|(_, name)| name.as_str())
            .find(|name| !name.is_empty())
            .unwrap_or("")
    };
    if !album.is_empty() && !artist.is_empty() {
        match api
            .query(SearchQuery::new(format!("{album} {artist}")).with_filter(AlbumsFilter))
            .await
        {
            Ok(albums) => match matching_album(&albums, album, artist) {
                Ok(resolved) => return Ok(remember_album(video_id, resolved)),
                Err(error) => errors.push(error),
            },
            Err(error) => errors.push(format!("Album name search: {error}")),
        }
    } else {
        errors.push("Missing album name or artist for a reliable album search".into());
    }
    Err(format!(
        "Could not resolve album for {video_id}: {}",
        errors.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use ytmapi_rs::common::ArtistChannelID;

    fn credit(name: &str, id: &str) -> ArtistCredit {
        ArtistCredit {
            name: name.into(),
            browse_id: id.into(),
        }
    }

    fn run(name: &str, id: &str, page_type: &str) -> Value {
        json!({"text": name, "navigationEndpoint": {"browseEndpoint": {
            "browseId": id,
            "browseEndpointContextSupportedConfigs": {
                "browseEndpointContextMusicConfig": {"pageType": page_type}
            }
        }}})
    }

    fn watch_row(video_id: &str, runs: Vec<Value>) -> Value {
        json!({"playlistPanelVideoRenderer": {
            "videoId": video_id, "title": {"runs": [{"text": "Requested song"}]},
            "longBylineText": {"runs": runs}
        }})
    }

    fn song(video_id: &str, album_id: &str) -> SearchResultSong {
        serde_json::from_value(json!({
            "title": "Same title", "artist": "Same artist", "album": {"name": "Record", "id": album_id},
            "duration": "3:00", "plays": "1", "explicit": "NotExplicit",
            "video_id": video_id, "thumbnails": []
        })).unwrap()
    }

    fn album(title: &str, artist: &str, id: &str) -> SearchResultAlbum {
        serde_json::from_value(json!({
            "title": title, "artist": artist, "album_id": id, "year": "2022",
            "explicit": "NotExplicit", "album_type": "Album", "thumbnails": []
        }))
        .unwrap()
    }

    #[test]
    fn watch_selects_exact_video_not_first_related() {
        let response = json!({"contents": [
            watch_row("nearby", vec![run("Wrong artist", "UCwrong", "MUSIC_PAGE_TYPE_ARTIST")]),
            watch_row("wanted", vec![
                run("Right artist", "UCright", "MUSIC_PAGE_TYPE_ARTIST"),
                run("Right album", "MPREright", "MUSIC_PAGE_TYPE_ALBUM")
            ])
        ]});
        let metadata = parse_watch_metadata(&response, "wanted").unwrap();
        assert_eq!(metadata.artists, vec![credit("Right artist", "UCright")]);
        assert_eq!(
            metadata.albums,
            vec![("MPREright".into(), "Right album".into())]
        );
        assert!(parse_watch_metadata(&response, "absent").is_err());
        assert!(parse_watch_metadata(&response, "WANTED").is_err());
    }

    #[test]
    fn watch_keeps_all_structured_contributors_and_literal_commas() {
        let response = watch_row(
            "wanted",
            vec![
                run("Earth, Wind & Fire", "UCband", "MUSIC_PAGE_TYPE_ARTIST"),
                json!({"text": ", "}),
                run("Guest", "UCguest", "MUSIC_PAGE_TYPE_ARTIST"),
                json!({"text": " & "}),
                json!({"text": "Unlinked, Actual Name"}),
                json!({"text": " \u{2022} "}),
                run("Record", "MPRErecord", "MUSIC_PAGE_TYPE_ALBUM"),
                json!({"text": " \u{2022} "}),
                json!({"text": "2022"}),
            ],
        );
        assert_eq!(
            parse_watch_metadata(&response, "wanted").unwrap().artists,
            vec![
                credit("Earth, Wind & Fire", "UCband"),
                credit("Guest", "UCguest"),
                credit("Unlinked, Actual Name", "")
            ]
        );
    }

    #[test]
    fn short_display_byline_is_not_an_individual_credit() {
        let mut response = watch_row(
            "wanted",
            vec![
                run("First", "UCfirst", "MUSIC_PAGE_TYPE_ARTIST"),
                json!({"text": ", "}),
                run("Second", "UCsecond", "MUSIC_PAGE_TYPE_ARTIST"),
            ],
        );
        response["playlistPanelVideoRenderer"]["shortBylineText"] = json!({
            "runs": [{"text": "First, & Second"}]
        });
        assert_eq!(
            parse_watch_metadata(&response, "wanted").unwrap().artists,
            vec![credit("First", "UCfirst"), credit("Second", "UCsecond")]
        );
        response["playlistPanelVideoRenderer"]["longBylineText"] = Value::Null;
        assert!(parse_watch_metadata(&response, "wanted")
            .unwrap()
            .artists
            .is_empty());
    }

    #[test]
    fn borrowed_album_artist_mapping_preserves_optional_ids() {
        let artists = vec![
            ParsedSongArtist {
                name: "Earth, Wind & Fire".into(),
                id: Some(ArtistChannelID::from_raw("UCband")),
            },
            ParsedSongArtist {
                name: "Guest".into(),
                id: Some(ArtistChannelID::from_raw("UCguest")),
            },
            ParsedSongArtist {
                name: "Unlinked".into(),
                id: None,
            },
        ];
        assert_eq!(
            parsed_credits(&artists),
            vec![
                credit("Earth, Wind & Fire", "UCband"),
                credit("Guest", "UCguest"),
                credit("Unlinked", "")
            ]
        );
        assert_eq!(artists.len(), 3);
    }

    #[test]
    fn merging_deduplicates_without_losing_names_or_case_sensitive_ids() {
        let mut credits = Vec::new();
        merge_credits(
            &mut credits,
            vec![
                credit(" Guest ", ""),
                credit("", "UCguest"),
                credit("Guest", "UCguest"),
                credit("guest", ""),
                credit("", ""),
                credit("Guest", "ucguest"),
                credit("Other", "UCother"),
            ],
        );
        merge_credits(
            &mut credits,
            vec![credit("", "UCguest"), credit("OTHER", ""), credit("", "")],
        );
        assert_eq!(
            credits,
            vec![
                credit("Guest", "UCguest"),
                credit("Guest", "ucguest"),
                credit("Other", "UCother")
            ]
        );
        assert!(credits.iter().all(|credit| !credit.browse_id.is_empty()));
    }

    #[test]
    fn cache_merges_and_keeps_only_128_least_recently_used_entries() {
        let mut cache = ArtistCache::default();
        for index in 0..CACHE_LIMIT {
            cache.insert(
                Kind::Song,
                &index.to_string(),
                vec![credit("Artist", "UCartist")],
            );
        }
        assert!(cache.get(Kind::Song, "0").is_some());
        cache.insert(Kind::Song, "new", vec![credit("New", "UCnew")]);
        assert_eq!(cache.entries.len(), CACHE_LIMIT);
        assert!(cache.get(Kind::Song, "1").is_none());
        cache.insert(
            Kind::Song,
            "0",
            vec![credit("", "UCartist"), credit("Guest", "UCguest")],
        );
        assert_eq!(
            cache.get(Kind::Song, "0").unwrap(),
            vec![credit("Artist", "UCartist"), credit("Guest", "UCguest")]
        );
        assert!(cache.get(Kind::Album, "0").is_none());
        cache.insert(Kind::Song, "empty", vec![ArtistCredit::default()]);
        assert!(cache.get(Kind::Song, "empty").is_none());
        assert_eq!(cache.entries.len(), CACHE_LIMIT);
    }

    #[test]
    fn failed_resolution_uses_registered_credits_but_missing_is_an_error() {
        register_artists(
            "Song",
            "menu-metadata-test-fallback",
            vec![credit("Known", "UCknown")],
        );
        assert_eq!(
            artist_result(
                "song",
                "menu-metadata-test-fallback",
                Err("Network failure".into())
            )
            .unwrap(),
            vec![credit("Known", "UCknown")]
        );
        assert_eq!(
            artist_result(
                "Song",
                "menu-metadata-test-absent",
                Err("Network failure".into())
            ),
            Err("Network failure".into())
        );
        assert!(artist_result("Song", "menu-metadata-test-absent", Ok(Vec::new())).is_err());
    }

    #[test]
    fn song_search_requires_exact_video_and_nonempty_album_id() {
        let songs = vec![song("nearby", "MPREwrong"), song("wanted", "MPREright")];
        assert_eq!(
            exact_song_album(&songs, "wanted").unwrap(),
            Some(("MPREright".into(), "Record".into()))
        );
        assert_eq!(exact_song_album(&songs, "absent").unwrap(), None);
        assert_eq!(
            exact_song_album(&[song("wanted", "")], "wanted").unwrap(),
            None
        );
        assert!(exact_song_album(
            &[song("wanted", "MPREone"), song("wanted", "MPREtwo")],
            "wanted"
        )
        .is_err());
    }

    #[test]
    fn album_search_matches_title_and_artist_without_guessing_editions() {
        let albums = vec![
            album("Record (Deluxe)", "Artist", "MPREdeluxe"),
            album("Record", "Other artist", "MPREother"),
            album("Record", "Artist", "MPREright"),
        ];
        assert_eq!(
            matching_album(&albums, " record ", "ARTIST").unwrap(),
            ("MPREright".into(), "Record".into())
        );
        assert!(matching_album(&albums[..2], "Record", "Artist").is_err());
        assert!(matching_album(&albums, "Record", "").is_err());
        assert!(matching_album(&[album("Record", "Artist", "")], "Record", "Artist").is_err());
    }

    #[test]
    fn ambiguous_album_ids_error_but_duplicate_identity_is_allowed() {
        let mut albums = vec![
            album("Record", "Artist", "MPREone"),
            album("Record", "Artist", "MPREtwo"),
        ];
        assert!(matching_album(&albums, "Record", "Artist")
            .unwrap_err()
            .contains("ambiguous"));
        albums[1] = album("Record", "Artist", "MPREone");
        assert_eq!(
            matching_album(&albums, "Record", "Artist").unwrap().0,
            "MPREone"
        );
    }

    #[test]
    fn artist_self_and_playback_album_fast_paths_need_no_runtime() {
        register_artists(
            "Artist",
            "UC-menu-metadata-test-self",
            vec![credit("Known artist", "UC-menu-metadata-test-self")],
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let credits = runtime
            .block_on(resolve_artists("Artist", "UC-menu-metadata-test-self"))
            .unwrap();
        assert_eq!(
            credits,
            vec![credit("Known artist", "UC-menu-metadata-test-self")]
        );
        assert_eq!(
            runtime
                .block_on(resolve_artists("Artist", "UC-menu-metadata-test-unknown"))
                .unwrap(),
            vec![credit("", "UC-menu-metadata-test-unknown")]
        );
        super::super::playback::register_song_meta(
            "menu-metadata-test-album",
            "Record",
            "MPREknown",
            "",
        );
        assert_eq!(
            runtime
                .block_on(resolve_album("menu-metadata-test-album", "", "", ""))
                .unwrap(),
            ("MPREknown".into(), "Record".into())
        );
        assert!(runtime.block_on(resolve_artists("Playlist", "id")).is_err());
        assert!(runtime.block_on(resolve_artists("Song", " ")).is_err());
    }

    #[test]
    #[ignore = "Requires live YouTube Music; caller applies a network timeout"]
    fn live_requested_video_and_album_credits() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let credits = tokio::time::timeout(
                std::time::Duration::from_secs(45),
                resolve_artists("Song", "_xFoazo9bl8"),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(credits.len() >= 3);
            assert!(credits
                .iter()
                .all(|credit| !credit.name.is_empty() && !credit.browse_id.is_empty()));
            let resolved = tokio::time::timeout(
                std::time::Duration::from_secs(45),
                resolve_album("_xFoazo9bl8", "", "", ""),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(!resolved.0.is_empty());
            let album_credits = tokio::time::timeout(
                std::time::Duration::from_secs(45),
                resolve_artists("Album", "MPREb_E4GfUXfDfhy"),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(album_credits.len() >= 3);
            assert!(album_credits
                .iter()
                .any(|credit| !credit.browse_id.is_empty()));
            eprintln!(
                "Song credits: {credits:?}; album: {resolved:?}; album credits: {album_credits:?}"
            );
        });
    }
}
