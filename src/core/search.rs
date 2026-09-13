use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};
use ytmapi_rs::auth::noauth::NoAuthToken;
use ytmapi_rs::common::{AlbumID, ArtistChannelID, SearchSuggestion, Thumbnail, YoutubeID};
use ytmapi_rs::parse::{
    BasicSearchResultCommunityPlaylist, GetAlbum, GetArtist, ParsedSongArtist, SearchResultAlbum,
    SearchResultArtist, SearchResultCommunityPlaylist, SearchResultFeaturedPlaylist,
    SearchResultPlaylist, SearchResultSong, SearchResults, TopResult, TopResultType,
};
use ytmapi_rs::query::search::{AlbumsFilter, ArtistsFilter, PlaylistsFilter, SongsFilter};
use ytmapi_rs::query::{GetAlbumQuery, GetArtistQuery, GetSearchSuggestionsQuery, SearchQuery};
use ytmapi_rs::YtMusic;

#[derive(Clone, Debug, Default)]
pub struct SearchEntry {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub artist: String,
    pub album: String,
    pub album_id: String,
    pub artist_id: String,
    pub duration_secs: u32,
    pub thumbnail_url: String,
    pub year: String,
}

#[derive(Clone, Debug, Default)]
pub struct SearchPage {
    pub top: Option<SearchEntry>,
    pub intent: String,
    pub songs: Vec<SearchEntry>,
    pub artists: Vec<SearchEntry>,
    pub albums: Vec<SearchEntry>,
    pub playlists: Vec<SearchEntry>,
}

const CACHE_LIMIT: usize = 16;
const CACHE_TTL: Duration = Duration::from_secs(120);

#[derive(Clone)]
enum CachedValue {
    Page(SearchPage),
    Catalogue(SearchPage),
    Completions(Vec<String>),
}

pub struct SearchClient {
    api: YtMusic<NoAuthToken>,
    cache: Cache<CachedValue>,
}

impl SearchClient {
    pub async fn new() -> Result<Self, String> {
        let api = YtMusic::new_unauthenticated()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self {
            api,
            cache: Cache::new(),
        })
    }

    pub async fn search(&mut self, query: &str, category: &str) -> Result<SearchPage, String> {
        let key = search_key(query, category)?;
        if query.trim().is_empty() {
            return Ok(SearchPage::default());
        }
        if let Some(CachedValue::Page(page)) = self.cache.get(&key, Instant::now()) {
            return Ok(page);
        }
        let category = normalize(category);
        if category != "all" {
            return self.filtered(query, &category).await;
        }
        let json = self
            .api
            .json_query(SearchQuery::new(query))
            .await
            .map_err(|error| error.to_string())?;
        let (results, mut identity) = parse_unfiltered_results(
            query,
            serde_json::to_value(json).map_err(|error| error.to_string())?,
        )?;
        let mut page = unfiltered_page(results);
        if needs_music_video_songs(identity.as_ref(), &page) {
            if let Ok(songs) = self.filtered(query, "songs").await {
                page.songs = songs.songs;
            }
        }
        let spaced_query = space_title_number(query);
        if needs_spaced_query(identity.as_ref(), query, &spaced_query)
            && resolve_top(None, &page, query).is_none()
        {
            let json = self
                .api
                .json_query(SearchQuery::new(&spaced_query))
                .await
                .map_err(|error| error.to_string())?;
            let (targeted, targeted_identity) = parse_unfiltered_results(
                &spaced_query,
                serde_json::to_value(json).map_err(|error| error.to_string())?,
            )?;
            if let Some(targeted_identity) = targeted_identity.filter(|identity| {
                matches!(identity.kind.as_str(), "Artist" | "Album")
                    && normalize(&identity.title) == normalize(&spaced_query)
            }) {
                add_candidates(&mut page, unfiltered_page(targeted), |entry| {
                    matches_identity(&targeted_identity, entry)
                });
                identity = Some(targeted_identity);
            }
        }
        if let Some(identity) = identity.as_ref() {
            if let Some(category) = targeted_category(Some(identity), &page) {
                let targeted_query = label(
                    &[
                        &identity.title,
                        &identity.artist,
                        &identity.album,
                        &identity.year,
                    ],
                    " ",
                );
                if let Ok(targeted) = self.filtered(&targeted_query, category).await {
                    add_candidates(&mut page, targeted, |entry| {
                        matches_identity(identity, entry)
                    });
                }
            }
        }
        page.top = resolve_top(identity.as_ref(), &page, query);
        page.intent = page
            .top
            .as_ref()
            .map(|entry| entry.kind.clone())
            .unwrap_or_default();
        self.cache
            .insert(key, CachedValue::Page(page.clone()), Instant::now());
        Ok(page)
    }

    pub async fn preview(&mut self, query: &str) -> Result<SearchPage, String> {
        let mut page = self.search(query, "All").await?;
        if page.playlists.is_empty() && !query.trim().is_empty() {
            merge_playlist_supplement(&mut page, self.filtered(query, "playlists").await);
        }
        Ok(page)
    }

    async fn filtered(&mut self, query: &str, category: &str) -> Result<SearchPage, String> {
        let key = search_key(query, category)?;
        if let Some(CachedValue::Page(page)) = self.cache.get(&key, Instant::now()) {
            return Ok(page);
        }
        let mut page = SearchPage::default();
        match category {
            "songs" => {
                page.songs = self
                    .api
                    .query(SearchQuery::new(query).with_filter(SongsFilter))
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .map(song_entry)
                    .collect();
            }
            "artists" => {
                page.artists = self
                    .api
                    .query(SearchQuery::new(query).with_filter(ArtistsFilter))
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .map(artist_entry)
                    .collect();
            }
            "albums" => {
                page.albums = self
                    .api
                    .query(SearchQuery::new(query).with_filter(AlbumsFilter))
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .map(album_entry)
                    .collect();
            }
            "playlists" => {
                page.playlists = self
                    .api
                    .query(SearchQuery::new(query).with_filter(PlaylistsFilter))
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .filter_map(|playlist| match playlist {
                        SearchResultPlaylist::Featured(playlist) => {
                            Some(featured_playlist_entry(playlist))
                        }
                        SearchResultPlaylist::Community(playlist) => {
                            Some(community_playlist_entry(playlist))
                        }
                        _ => None,
                    })
                    .collect();
            }
            _ => return Err(format!("Unknown search category: {category}")),
        }
        clean_page(&mut page);
        self.cache
            .insert(key, CachedValue::Page(page.clone()), Instant::now());
        Ok(page)
    }

    pub async fn enrich(&mut self, page: &SearchPage) -> Result<SearchPage, String> {
        let Some(top) = page.top.as_ref().filter(|top| !top.id.trim().is_empty()) else {
            return Ok(page.clone());
        };
        if !matches!(top.kind.as_str(), "Artist" | "Album") {
            return Ok(page.clone());
        }
        let key = CacheKey::Enrich {
            kind: top.kind.clone(),
            id: top.id.clone(),
        };
        let catalogue = if let Some(CachedValue::Catalogue(catalogue)) = self.cache.get(&key, Instant::now()) {
            catalogue
        } else {
            let mut catalogue = if top.kind == "Artist" {
                let artist = self
                    .api
                    .query(GetArtistQuery::new(ArtistChannelID::from_raw(
                        top.id.as_str(),
                    )))
                    .await
                    .map_err(|error| format!("Artist catalogue: {error}"))?;
                artist_catalogue(artist)
            } else {
                let album = self
                    .api
                    .query(GetAlbumQuery::new(AlbumID::from_raw(top.id.as_str())))
                    .await
                    .map_err(|error| format!("Album tracks: {error}"))?;
                album_catalogue(album, &top.id)
            };
            clean_page(&mut catalogue);
            self.cache
                .insert(key, CachedValue::Catalogue(catalogue.clone()), Instant::now());
            catalogue
        };
        let mut enriched = merge_catalogue(page, &catalogue);
        if top.kind == "Artist" && page.playlists.is_empty() {
            merge_playlist_supplement(&mut enriched, self.filtered(&top.title, "playlists").await);
        }
        Ok(enriched)
    }

    pub async fn completions(&mut self, query: &str) -> Result<Vec<String>, String> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let key = CacheKey::Completions(normalize(query));
        if let Some(CachedValue::Completions(completions)) = self.cache.get(&key, Instant::now()) {
            return Ok(completions);
        }
        let suggestions = self
            .api
            .query(GetSearchSuggestionsQuery::new(query))
            .await
            .map_err(|error| error.to_string())?;
        let completions = completion_texts(suggestions);
        self.cache.insert(
            key,
            CachedValue::Completions(completions.clone()),
            Instant::now(),
        );
        Ok(completions)
    }
}

fn parse_unfiltered_results(
    query: &str,
    mut json: serde_json::Value,
) -> Result<(SearchResults, Option<TopIdentity>), String> {
    let mut card_metadata = None;
    let mut previews = SearchResults::default();
    if let Some(sections) = json.pointer_mut(
        "/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents",
    ).and_then(serde_json::Value::as_array_mut) {
        for section in sections {
            if let Some(card) = section.get_mut("musicCardShelfRenderer") {
                extract_card_previews(card, &mut previews);
                if card_metadata.is_none() {
                    card_metadata = Some(card.clone());
                }
                if let Some(card) = card.as_object_mut() {
                    card.remove("contents");
                }
            }
        }
    }
    let search_query = SearchQuery::new(query);
    let mut results = ytmapi_rs::parse::ProcessedResult {
        query: &search_query,
        source: String::new(),
        json: serde_json::from_value(json).map_err(|error| error.to_string())?,
    }
    .parse_into::<SearchResults>()
    .map_err(|error| error.to_string())?;
    results.songs.extend(previews.songs);
    results.albums.extend(previews.albums);
    let identity = card_identity(&results, card_metadata.as_ref());
    Ok((results, identity))
}

fn extract_card_previews(card: &serde_json::Value, results: &mut SearchResults) {
    for child in card["contents"].as_array().into_iter().flatten() {
        let Some(row) = child.get("musicResponsiveListItemRenderer") else { continue };
        let Some(title_runs) = row.pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs")
            .and_then(serde_json::Value::as_array) else { continue };
        let Some(metadata) = row.pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs")
            .and_then(serde_json::Value::as_array) else { continue };
        let kind = metadata.first().and_then(|run| run["text"].as_str()).unwrap_or_default();
        if !matches!(kind, "Song" | "Album" | "Single" | "EP") { continue; }
        let title = title_runs.iter().filter_map(|run| run["text"].as_str()).collect::<String>();
        if title.trim().is_empty() { continue; }
        let endpoints: Vec<_> = title_runs.iter().filter_map(|run| run.get("navigationEndpoint"))
            .chain([
                row.get("navigationEndpoint"), row.get("onTap"),
                row.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint"),
            ].into_iter().flatten()).collect();
        let browse_run = |page_type: &str| {
            metadata.iter().find(|run| run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                .and_then(serde_json::Value::as_str) == Some(page_type))
        };
        let mut artist = metadata.iter().filter(|run| run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
            .and_then(serde_json::Value::as_str) == Some("MUSIC_PAGE_TYPE_ARTIST"))
            .filter_map(|run| run["text"].as_str()).collect::<Vec<_>>().join(", ");
        if artist.is_empty() && card.pointer("/subtitle/runs/0/text").and_then(serde_json::Value::as_str) == Some("Artist") {
            artist = card.pointer("/title/runs").and_then(serde_json::Value::as_array)
                .into_iter().flatten().filter_map(|run| run["text"].as_str()).collect();
        }
        let thumbnails = row.pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
            .cloned().unwrap_or_else(|| serde_json::json!([]));
        if kind == "Song" {
            if endpoints.iter().any(|endpoint| matches!(endpoint.pointer("/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType")
                .and_then(serde_json::Value::as_str), Some(video_type) if video_type != "MUSIC_VIDEO_TYPE_ATV")) {
                continue;
            }
            let id = row.pointer("/playlistItemData/videoId").and_then(serde_json::Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .or_else(|| endpoints.iter().find_map(|endpoint| endpoint.pointer("/watchEndpoint/videoId")
                    .and_then(serde_json::Value::as_str).filter(|id| !id.trim().is_empty())));
            let Some(id) = id else { continue };
            let album = browse_run("MUSIC_PAGE_TYPE_ALBUM").and_then(|run| Some(serde_json::json!({
                "name": run["text"].as_str()?,
                "id": run.pointer("/navigationEndpoint/browseEndpoint/browseId")?.as_str()?,
            })));
            let duration = metadata.iter().filter_map(|run| run["text"].as_str())
                .find(|text| duration_secs(text) > 0).unwrap_or_default();
            if let Ok(song) = serde_json::from_value(serde_json::json!({
                "title": title, "artist": artist, "album": album, "duration": duration,
                "plays": "", "explicit": "NotExplicit", "video_id": id, "thumbnails": thumbnails,
            })) {
                results.songs.push(song);
            }
        } else {
            let id = endpoints.iter().find_map(|endpoint| {
                let browse = endpoint.get("browseEndpoint")?;
                if browse.pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                    .and_then(serde_json::Value::as_str).is_some_and(|kind| kind != "MUSIC_PAGE_TYPE_ALBUM") {
                    return None;
                }
                browse["browseId"].as_str().filter(|id| !id.trim().is_empty())
            });
            let Some(id) = id else { continue };
            let year = metadata.iter().filter_map(|run| run["text"].as_str())
                .find(|text| text.len() == 4 && text.bytes().all(|byte| byte.is_ascii_digit())).unwrap_or_default();
            if let Ok(album) = serde_json::from_value(serde_json::json!({
                "title": title, "artist": artist, "year": year, "explicit": "NotExplicit",
                "album_id": id, "album_type": kind, "thumbnails": thumbnails,
            })) {
                results.albums.push(album);
            }
        }
    }
}

fn card_endpoint<'card>(card: &'card serde_json::Value, kind: &str) -> Option<&'card str> {
    let paths: &[&str] = match kind {
        "Song" | "Video" => &[
            "/title/runs/0/navigationEndpoint/watchEndpoint/videoId",
            "/onTap/watchEndpoint/videoId",
            "/playButton/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId",
        ],
        "Playlist" => &[
            "/title/runs/0/navigationEndpoint/browseEndpoint/browseId",
            "/onTap/browseEndpoint/browseId",
            "/title/runs/0/navigationEndpoint/watchPlaylistEndpoint/playlistId",
            "/title/runs/0/navigationEndpoint/watchEndpoint/playlistId",
            "/onTap/watchPlaylistEndpoint/playlistId",
            "/onTap/watchEndpoint/playlistId",
            "/playButton/musicPlayButtonRenderer/playNavigationEndpoint/watchPlaylistEndpoint/playlistId",
            "/playButton/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/playlistId",
        ],
        _ => &[
            "/title/runs/0/navigationEndpoint/browseEndpoint/browseId",
            "/onTap/browseEndpoint/browseId",
        ],
    };
    paths.iter().find_map(|path| {
        card.pointer(path)
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
    })
}

fn is_music_video_card(card: &serde_json::Value) -> bool {
    [
        "/title/runs/0/navigationEndpoint",
        "/onTap",
        "/playButton/musicPlayButtonRenderer/playNavigationEndpoint",
    ].iter().any(|path| {
        matches!(
            card.pointer(path).and_then(|endpoint| endpoint.pointer(
                "/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType"
            )).and_then(serde_json::Value::as_str),
            Some("MUSIC_VIDEO_TYPE_OMV" | "MUSIC_VIDEO_TYPE_ATV")
        )
    })
}

fn needs_music_video_songs(identity: Option<&TopIdentity>, page: &SearchPage) -> bool {
    page.songs.is_empty()
        && identity.is_some_and(|identity| identity.kind == "Video" && identity.music_video)
}

fn card_identity(results: &SearchResults, card: Option<&serde_json::Value>) -> Option<TopIdentity> {
    let top = results.top_results.first()?;
    let card = card.filter(|card| {
        card.pointer("/title/runs/0/text")
            .and_then(serde_json::Value::as_str)
            .map(|title| normalize(title) == normalize(&top.result_name))
            .unwrap_or(false)
    });
    let mut identity = top_identity(top).or_else(|| {
        if !matches!(top.result_type, None | Some(TopResultType::Video)) {
            return None;
        }
        let card = card?;
        let video_id = card_endpoint(card, "Video")?;
        if top.result_type.is_none() {
            if let Some(song) = results.songs.iter().find(|song| song.video_id.get_raw() == video_id) {
                return Some(TopIdentity {
                    kind: "Song".into(),
                    id: video_id.into(),
                    title: song.title.clone(),
                    artist: song.artist.clone(),
                    ..Default::default()
                });
            }
        }
        Some(TopIdentity {
            kind: "Video".into(),
            id: video_id.into(),
            title: top.result_name.clone(),
            artist: top.artist.clone().unwrap_or_default(),
            music_video: is_music_video_card(card),
            ..Default::default()
        })
    });
    if let (Some(identity), Some(card)) = (identity.as_mut(), card) {
        let title = card
            .pointer("/title/runs/0/text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if normalize(title) == normalize(&identity.title) {
            identity.id = card_endpoint(card, &identity.kind)
                .unwrap_or_default()
                .to_string();
            if let Some(runs) = card.pointer("/subtitle/runs").and_then(serde_json::Value::as_array) {
                let metadata = runs.iter().skip(2).filter_map(|run| run["text"].as_str()).collect::<String>();
                if !metadata.trim().is_empty() {
                    identity.subtitle = metadata.trim().into();
                }
                if matches!(identity.kind.as_str(), "Album" | "Song" | "Video") {
                    identity.artist_credits = runs.iter().filter(|run| {
                        run.pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                            .and_then(serde_json::Value::as_str) == Some("MUSIC_PAGE_TYPE_ARTIST")
                    }).filter_map(|run| run["text"].as_str()).map(str::to_string).collect();
                    if identity.artist.is_empty() {
                        identity.artist = identity.artist_credits.join(", ");
                    }
                }
                if identity.kind == "Album" {
                    if identity.year.is_empty() {
                        identity.year = runs.iter().rev().filter_map(|run| run["text"].as_str())
                            .find(|text| text.len() == 4 && text.bytes().all(|byte| byte.is_ascii_digit()))
                            .unwrap_or_default().into();
                    }
                }
            }
        }
    }
    identity
}

fn add_candidates(
    page: &mut SearchPage,
    candidates: SearchPage,
    accepts: impl Fn(&SearchEntry) -> bool,
) {
    for (current, incoming) in [
        (&mut page.artists, candidates.artists),
        (&mut page.albums, candidates.albums),
        (&mut page.songs, candidates.songs),
        (&mut page.playlists, candidates.playlists),
    ] {
        current.extend(incoming.into_iter().filter(|entry| accepts(entry)));
        deduplicate(current);
    }
}

fn completion_texts(suggestions: Vec<SearchSuggestion>) -> Vec<String> {
    let mut seen = HashSet::new();
    suggestions
        .into_iter()
        .map(|suggestion| {
            suggestion
                .runs
                .into_iter()
                .map(|run| run.take_text())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|text| !text.is_empty() && seen.insert(normalize(text)))
        .collect()
}

fn label(parts: &[&str], separator: &str) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(separator)
}

fn thumbnail_url(thumbnails: &[Thumbnail]) -> String {
    crate::pick_thumb(thumbnails)
        .map(|thumbnail| thumbnail.url.clone())
        .unwrap_or_default()
}

fn duration_secs(duration: &str) -> u32 {
    let parts: Vec<_> = duration.trim().split(':').collect();
    if !matches!(parts.len(), 2 | 3) {
        return 0;
    }
    parts
        .iter()
        .enumerate()
        .try_fold(0u32, |total, (index, part)| {
            let value = part.parse::<u32>().ok()?;
            if index > 0 && value >= 60 {
                return None;
            }
            total.checked_mul(60)?.checked_add(value)
        })
        .unwrap_or(0)
}

fn song_entry(song: SearchResultSong) -> SearchEntry {
    let (album, album_id) = song
        .album
        .map(|album| (album.name, album.id.get_raw().to_string()))
        .unwrap_or_default();
    SearchEntry {
        kind: "Song".into(),
        id: song.video_id.get_raw().into(),
        subtitle: label(&[&song.artist, &album], " - "),
        title: song.title,
        artist: song.artist,
        album,
        album_id,
        duration_secs: duration_secs(&song.duration),
        thumbnail_url: thumbnail_url(&song.thumbnails),
        ..Default::default()
    }
}

fn artist_entry(artist: SearchResultArtist) -> SearchEntry {
    SearchEntry {
        kind: "Artist".into(),
        id: artist.browse_id.get_raw().into(),
        title: artist.artist.clone(),
        artist: artist.artist,
        artist_id: artist.browse_id.get_raw().into(),
        subtitle: artist.subscribers.unwrap_or_default(),
        thumbnail_url: thumbnail_url(&artist.thumbnails),
        ..Default::default()
    }
}

fn album_entry(album: SearchResultAlbum) -> SearchEntry {
    SearchEntry {
        kind: "Album".into(),
        id: album.album_id.get_raw().into(),
        album_id: album.album_id.get_raw().into(),
        album: album.title.clone(),
        subtitle: label(&[&album.artist, &album.year], " - "),
        title: album.title,
        artist: album.artist,
        year: album.year,
        thumbnail_url: thumbnail_url(&album.thumbnails),
        ..Default::default()
    }
}

fn featured_playlist_entry(playlist: SearchResultFeaturedPlaylist) -> SearchEntry {
    SearchEntry {
        kind: "Playlist".into(),
        id: playlist.playlist_id.get_raw().into(),
        title: playlist.title,
        subtitle: label(&[&playlist.author, &playlist.songs], " - "),
        thumbnail_url: thumbnail_url(&playlist.thumbnails),
        ..Default::default()
    }
}

fn community_playlist_entry(playlist: SearchResultCommunityPlaylist) -> SearchEntry {
    SearchEntry {
        kind: "Playlist".into(),
        id: playlist.playlist_id.get_raw().into(),
        title: playlist.title,
        subtitle: label(&[&playlist.author, &playlist.views], " - "),
        thumbnail_url: thumbnail_url(&playlist.thumbnails),
        ..Default::default()
    }
}

fn unfiltered_page(results: SearchResults) -> SearchPage {
    let mut page =
        SearchPage {
            songs: results.songs.into_iter().map(song_entry).collect(),
            artists: results.artists.into_iter().map(artist_entry).collect(),
            albums: results.albums.into_iter().map(album_entry).collect(),
            playlists: results
                .featured_playlists
                .into_iter()
                .map(featured_playlist_entry)
                .chain(results.community_playlists.into_iter().filter_map(
                    |playlist| match playlist {
                        BasicSearchResultCommunityPlaylist::Playlist(playlist) => {
                            Some(community_playlist_entry(playlist))
                        }
                        _ => None,
                    },
                ))
                .collect(),
            ..Default::default()
        };
    clean_page(&mut page);
    page
}

fn clean_page(page: &mut SearchPage) {
    deduplicate(&mut page.songs);
    deduplicate(&mut page.artists);
    deduplicate(&mut page.albums);
    deduplicate(&mut page.playlists);
}

fn top_identity(top: &TopResult) -> Option<TopIdentity> {
    let kind = match top.result_type.as_ref()? {
        TopResultType::Artist => "Artist",
        TopResultType::Album(_) => "Album",
        TopResultType::Song => "Song",
        TopResultType::Playlist => "Playlist",
        _ => return None,
    };
    if normalize(&top.result_name).is_empty() {
        return None;
    }
    Some(TopIdentity {
        kind: kind.into(),
        id: String::new(),
        title: top.result_name.clone(),
        artist: top.artist.clone().unwrap_or_default(),
        album: top.album.clone().unwrap_or_default(),
        year: top.year.clone().unwrap_or_default(),
        subtitle: if kind == "Artist" {
            top.subscribers.clone().unwrap_or_default()
        } else {
            label(&[top.artist.as_deref().unwrap_or_default(), top.year.as_deref().unwrap_or_default()], " - ")
        },
        thumbnail_url: thumbnail_url(&top.thumbnails),
        music_video: false,
        artist_credits: Vec::new(),
    })
}

fn credit_names(artists: &[ParsedSongArtist]) -> String {
    artists
        .iter()
        .map(|artist| artist.name.as_str())
        .filter(|name| !name.trim().is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn first_artist_id(artists: &[ParsedSongArtist]) -> String {
    artists
        .first()
        .and_then(|artist| artist.id.as_ref())
        .map(|id| id.get_raw().to_string())
        .unwrap_or_default()
}

fn artist_catalogue(artist: GetArtist) -> SearchPage {
    let songs = artist
        .top_releases
        .songs
        .into_iter()
        .flat_map(|songs| songs.results)
        .map(|song| {
            let artists = credit_names(&song.artists);
            SearchEntry {
                kind: "Song".into(),
                id: song.video_id.get_raw().into(),
                thumbnail_url: format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", song.video_id.get_raw()),
                title: song.title,
                subtitle: label(&[&artists, &song.album.name], " - "),
                artist: artists,
                artist_id: first_artist_id(&song.artists),
                album: song.album.name,
                album_id: song.album.id.get_raw().into(),
                ..Default::default()
            }
        })
        .collect();
    let albums = artist
        .top_releases
        .albums
        .into_iter()
        .chain(artist.top_releases.singles)
        .flat_map(|albums| albums.results)
        .map(|album| SearchEntry {
            kind: "Album".into(),
            id: album.album_id.get_raw().into(),
            album_id: album.album_id.get_raw().into(),
            album: album.title.clone(),
            title: album.title,
            subtitle: label(&[&artist.name, &album.year], " - "),
            artist: artist.name.clone(),
            artist_id: artist.channel_id.get_raw().into(),
            year: album.year,
            thumbnail_url: thumbnail_url(&album.thumbnails),
            ..Default::default()
        })
        .collect();
    let artists = artist
        .top_releases
        .related
        .into_iter()
        .flat_map(|related| related.results)
        .map(|related| SearchEntry {
            kind: "Artist".into(),
            id: related.browse_id.get_raw().into(),
            artist_id: related.browse_id.get_raw().into(),
            artist: related.title.clone(),
            title: related.title,
            subtitle: related.subscribers,
            ..Default::default()
        })
        .collect();
    SearchPage {
        songs,
        albums,
        artists,
        ..Default::default()
    }
}

fn album_catalogue(mut album: GetAlbum, album_id: &str) -> SearchPage {
    album.tracks.sort_by_key(|track| track.track_no);
    let artist_names = credit_names(&album.artists);
    let artist_id = first_artist_id(&album.artists);
    let album_thumbnail = thumbnail_url(&album.thumbnails);
    let songs = album
        .tracks
        .into_iter()
        .map(|track| SearchEntry {
            kind: "Song".into(),
            id: track.video_id.get_raw().into(),
            title: track.title,
            subtitle: label(&[&artist_names, &album.title], " - "),
            artist: artist_names.clone(),
            artist_id: artist_id.clone(),
            album: album.title.clone(),
            album_id: album_id.into(),
            duration_secs: duration_secs(&track.duration),
            thumbnail_url: album_thumbnail.clone(),
            year: album.year.clone(),
        })
        .collect();
    let artists = album
        .artists
        .into_iter()
        .filter_map(|artist| {
            let id = artist.id?.get_raw().to_string();
            if id.trim().is_empty() {
                return None;
            }
            Some(SearchEntry {
                kind: "Artist".into(),
                id: id.clone(),
                artist_id: id,
                artist: artist.name.clone(),
                title: artist.name,
                ..Default::default()
            })
        })
        .collect();
    SearchPage {
        songs,
        artists,
        ..Default::default()
    }
}

fn merge_playlist_supplement(page: &mut SearchPage, supplement: Result<SearchPage, String>) {
    if page.playlists.is_empty() {
        if let Ok(supplement) = supplement {
            page.playlists = supplement.playlists;
            deduplicate(&mut page.playlists);
        }
    }
}

fn merge_catalogue(page: &SearchPage, catalogue: &SearchPage) -> SearchPage {
    SearchPage {
        top: page.top.clone(),
        intent: page.intent.clone(),
        songs: catalogue.songs.clone(),
        artists: catalogue.artists.clone(),
        albums: catalogue.albums.clone(),
        playlists: page.playlists.clone(),
    }
}

fn needs_spaced_query(identity: Option<&TopIdentity>, query: &str, spaced_query: &str) -> bool {
    spaced_query != query
        && identity.map_or(true, |identity| {
            let title = song_title(&identity.title);
            matches!(identity.kind.as_str(), "Video" | "Song")
                && title != song_title(query)
                && title != song_title(spaced_query)
        })
}

fn space_title_number(query: &str) -> String {
    let mut spaced = String::with_capacity(query.len());
    let mut previous_letter = false;
    for character in query.chars() {
        if previous_letter && character.is_ascii_digit() {
            spaced.push(' ');
        }
        spaced.push(character);
        previous_letter = character.is_alphabetic();
    }
    spaced
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CacheKey {
    Search { category: String, query: String },
    Enrich { kind: String, id: String },
    Completions(String),
}

fn search_key(query: &str, category: &str) -> Result<CacheKey, String> {
    let category = normalize(category);
    if !matches!(
        category.as_str(),
        "all" | "songs" | "artists" | "albums" | "playlists"
    ) {
        return Err(format!("Unknown search category: {category}"));
    }
    Ok(CacheKey::Search {
        category,
        query: normalize(query),
    })
}

struct CacheItem<Value> {
    key: CacheKey,
    inserted: Instant,
    value: Value,
}

struct Cache<Value> {
    entries: VecDeque<CacheItem<Value>>,
}

impl<Value: Clone> Cache<Value> {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &CacheKey, now: Instant) -> Option<Value> {
        self.entries
            .retain(|entry| now.saturating_duration_since(entry.inserted) < CACHE_TTL);
        let position = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(position)?;
        let value = entry.value.clone();
        self.entries.push_back(entry);
        Some(value)
    }

    fn insert(&mut self, key: CacheKey, value: Value, now: Instant) {
        self.entries.retain(|entry| {
            entry.key != key && now.saturating_duration_since(entry.inserted) < CACHE_TTL
        });
        self.entries.push_back(CacheItem {
            key,
            value,
            inserted: now,
        });
        while self.entries.len() > CACHE_LIMIT {
            self.entries.pop_front();
        }
    }
}

#[derive(Default)]
struct TopIdentity {
    kind: String,
    id: String,
    title: String,
    artist: String,
    album: String,
    year: String,
    subtitle: String,
    thumbnail_url: String,
    music_video: bool,
    artist_credits: Vec<String>,
}

enum Resolution {
    Missing,
    Unique(SearchEntry),
    Ambiguous,
}

fn entries(page: &SearchPage) -> impl Iterator<Item = &SearchEntry> {
    page.artists.iter().chain(&page.albums).chain(&page.songs).chain(&page.playlists)
}

fn endpoint_id<'id>(kind: &str, id: &'id str) -> &'id str {
    if kind == "Playlist" {
        id.strip_prefix("VL").unwrap_or(id)
    } else {
        id
    }
}

fn unique_match<'entry>(candidates: impl Iterator<Item = &'entry SearchEntry>) -> Resolution {
    let mut identities = HashSet::new();
    let mut result = None;
    for entry in candidates.filter(|entry| !entry.id.trim().is_empty()) {
        if identities.insert((&entry.kind, endpoint_id(&entry.kind, &entry.id))) {
            if result.is_some() {
                return Resolution::Ambiguous;
            }
            result = Some(entry.clone());
        }
    }
    result
        .map(Resolution::Unique)
        .unwrap_or(Resolution::Missing)
}

fn matches_identity(identity: &TopIdentity, entry: &SearchEntry) -> bool {
    if entry.kind != identity.kind {
        return false;
    }
    if !identity.id.is_empty() {
        return endpoint_id(&entry.kind, &entry.id) == endpoint_id(&identity.kind, &identity.id);
    }
    !normalize(&identity.title).is_empty()
        && normalize(&entry.title) == normalize(&identity.title)
        && (identity.artist.trim().is_empty()
            || normalize(&entry.artist) == normalize(&identity.artist))
        && (identity.album.trim().is_empty()
            || normalize(&entry.album) == normalize(&identity.album))
        && (identity.year.trim().is_empty() || normalize(&entry.year) == normalize(&identity.year))
}

fn resolve_identity(identity: &TopIdentity, page: &SearchPage) -> Resolution {
    unique_match(entries(page).filter(|entry| matches_identity(identity, entry)))
}

fn provider_entry(identity: &TopIdentity) -> Option<SearchEntry> {
    if identity.id.trim().is_empty() || identity.title.trim().is_empty()
        || !matches!(identity.kind.as_str(), "Artist" | "Album" | "Playlist")
    {
        return None;
    }
    Some(SearchEntry {
        kind: identity.kind.clone(),
        id: identity.id.clone(),
        title: identity.title.clone(),
        subtitle: identity.subtitle.clone(),
        thumbnail_url: identity.thumbnail_url.clone(),
        artist: if identity.kind == "Artist" { identity.title.clone() } else { identity.artist.clone() },
        artist_id: if identity.kind == "Artist" { identity.id.clone() } else { String::new() },
        album: if identity.kind == "Album" { identity.title.clone() } else { identity.album.clone() },
        album_id: if identity.kind == "Album" { identity.id.clone() } else { String::new() },
        year: identity.year.clone(),
        ..Default::default()
    })
}

fn targeted_category(identity: Option<&TopIdentity>, page: &SearchPage) -> Option<&'static str> {
    let identity = identity?;
    if !matches!(resolve_identity(identity, page), Resolution::Missing) || provider_entry(identity).is_some() {
        return None;
    }
    match identity.kind.as_str() {
        "Artist" => Some("artists"),
        "Album" => Some("albums"),
        _ => None,
    }
}

fn song_title(value: &str) -> String {
    let value = value.trim();
    for (opening, closing) in [('(', ')'), ('[', ']')] {
        if let Some(body) = value.strip_suffix(closing) {
            if let Some((title, suffix)) = body.rsplit_once(opening) {
                if matches!(normalize(suffix).as_str(),
                    "official video" | "official music video" | "official audio" | "music video" | "audio")
                {
                    return normalize(title);
                }
            }
        }
    }
    normalize(value)
}

fn has_artist_credit(credits: &str, artist: &str) -> bool {
    let credits = normalize(credits);
    let artist = normalize(artist);
    !artist.is_empty() && (credits == artist
        || credits.split(',').flat_map(|credit| credit.split(" & ")).any(|credit| credit.trim() == artist))
}

fn resolve_music_video(identity: &TopIdentity, page: &SearchPage, query: &str) -> Option<SearchEntry> {
    if !identity.music_video {
        return None;
    }
    let songs = || page.songs.iter().filter(|song| song.kind == "Song" && !song.id.trim().is_empty());
    if let Resolution::Unique(song) = unique_match(songs().filter(|song| song.id == identity.id)) {
        return Some(song);
    }
    let provider_title = song_title(&identity.title);
    let artist = normalize(&identity.artist);
    let query_title = song_title(query);
    let query = normalize(query);
    if !artist.is_empty() {
        let artist_credits: Vec<_> = if identity.artist_credits.is_empty() {
            vec![artist]
        } else {
            identity.artist_credits.iter().map(|artist| normalize(artist)).collect()
        };
        if let Some(song) = songs().find(|song| {
            let title = song_title(&song.title);
            !title.is_empty() && artist_credits.iter().any(|artist| {
                has_artist_credit(&song.artist, artist)
                    && (query == format!("{title} {artist}") || query == format!("{artist} {title}"))
            })
        }) {
            return Some(song.clone());
        }
        let matching: Vec<_> = songs().filter(|song| {
            let title = song_title(&song.title);
            !title.is_empty() && (title == provider_title || title == query_title)
                && artist_credits.iter().any(|artist| has_artist_credit(&song.artist, artist))
        }).collect();
        let artists: HashSet<_> = matching.iter().map(|song| normalize(&song.artist)).collect();
        return if artists.len() == 1 { matching.first().map(|song| (*song).clone()) } else { None };
    }
    if query_title.is_empty() {
        return None;
    }
    match unique_match(songs().filter(|song| song_title(&song.title) == query_title)) {
        Resolution::Unique(song) => Some(song),
        _ => None,
    }
}

fn resolve_top(
    identity: Option<&TopIdentity>,
    page: &SearchPage,
    query: &str,
) -> Option<SearchEntry> {
    if let Some(identity) = identity {
        if identity.kind == "Video" {
            return resolve_music_video(identity, page, query);
        }
        match resolve_identity(identity, page) {
            Resolution::Unique(entry) => return Some(entry),
            Resolution::Ambiguous => return None,
            Resolution::Missing => {}
        }
        if let Some(entry) = provider_entry(identity) {
            return Some(entry);
        }
    }
    let query = normalize(query);
    if query.is_empty() {
        return None;
    }
    match unique_match(entries(page).filter(|entry| normalize(&entry.title) == query)) {
        Resolution::Unique(entry) => {
            let contradicts_top = identity
                .map(|identity| {
                    identity.kind == entry.kind
                        && normalize(&identity.title) == normalize(&entry.title)
                        && !matches_identity(identity, &entry)
                })
                .unwrap_or(false);
            if contradicts_top {
                None
            } else {
                Some(entry)
            }
        }
        _ => None,
    }
}

fn deduplicate(entries: &mut Vec<SearchEntry>) {
    let mut seen = HashSet::new();
    entries.retain(|entry| !entry.id.trim().is_empty() && seen.insert(entry.id.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, id: &str, title: &str) -> SearchEntry {
        SearchEntry {
            kind: kind.into(),
            id: id.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    #[test]
    fn lazy_fallback_only_requests_one_missing_provider_artist_or_album() {
        let page = SearchPage::default();
        assert_eq!(targeted_category(None, &page), None);
        for (kind, category) in [
            ("Artist", Some("artists")),
            ("Album", Some("albums")),
            ("Song", None),
            ("Playlist", None),
            ("Video", None),
        ] {
            let identity = TopIdentity {
                kind: kind.into(),
                title: "Provider title".into(),
                ..Default::default()
            };
            assert_eq!(targeted_category(Some(&identity), &page), category);
        }
        let identity = TopIdentity {
            kind: "Artist".into(),
            title: "Provider title".into(),
            ..Default::default()
        };
        let mut page = SearchPage {
            artists: vec![entry("Artist", "first", "Provider title")],
            ..Default::default()
        };
        assert_eq!(targeted_category(Some(&identity), &page), None);
        page.artists.push(entry("Artist", "second", "Provider title"));
        assert_eq!(targeted_category(Some(&identity), &page), None);
    }

    #[test]
    fn top_identity_uses_kind_artist_and_year() {
        let mut correct = entry("Album", "album-2013", "Aashiqui 2");
        correct.artist = "Mithoon, Ankit Tiwari, Jeet Gannguli".into();
        correct.year = "2013".into();
        let mut other = correct.clone();
        other.id = "album-2020".into();
        other.year = "2020".into();
        let page = SearchPage {
            albums: vec![other, correct.clone()],
            songs: vec![entry("Song", "song", "Aashiqui 2")],
            ..Default::default()
        };
        let identity = TopIdentity {
            kind: "Album".into(),
            title: "Aashiqui 2".into(),
            artist: correct.artist,
            year: "2013".into(),
            ..Default::default()
        };
        assert_eq!(
            resolve_top(Some(&identity), &page, "soundtrack")
                .unwrap()
                .id,
            "album-2013"
        );
    }

    #[test]
    fn duplicate_names_are_ambiguous_but_duplicate_ids_are_not() {
        let artist = entry("Artist", "artist-1", "Arijit Singh");
        let identity = TopIdentity {
            kind: "Artist".into(),
            title: "Arijit Singh".into(),
            ..Default::default()
        };
        let mut page = SearchPage {
            artists: vec![artist.clone(), artist],
            ..Default::default()
        };
        assert!(resolve_top(Some(&identity), &page, "Arijit Singh").is_some());
        page.artists
            .push(entry("Artist", "artist-2", "Arijit Singh"));
        assert!(resolve_top(Some(&identity), &page, "Arijit Singh").is_none());
    }

    #[test]
    fn fallback_requires_unique_exact_match_across_types() {
        let mut page = SearchPage {
            songs: vec![
                entry("Song", "first", "Different title"),
                entry("Song", "song", "Hello"),
            ],
            ..Default::default()
        };
        assert_eq!(resolve_top(None, &page, " hello ").unwrap().id, "song");
        assert!(resolve_top(None, &page, "hell").is_none());
        page.albums.push(entry("Album", "album", "Hello"));
        assert!(resolve_top(None, &page, "hello").is_none());
    }

    #[test]
    fn navigation_entries_require_real_ids() {
        let mut items = vec![
            entry("Playlist", "", "Missing"),
            entry("Playlist", "playlist", "Public"),
            entry("Playlist", "playlist", "Public"),
        ];
        deduplicate(&mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "playlist");
    }

    #[test]
    fn normalization_preserves_punctuation_and_category_separation() {
        assert_eq!(
            search_key("  Arijit\tSINGH ", " All ").unwrap(),
            search_key("arijit singh", "all").unwrap()
        );
        assert_ne!(
            search_key("AC/DC", "all").unwrap(),
            search_key("AC DC", "all").unwrap()
        );
        assert_ne!(
            search_key("Hello", "all").unwrap(),
            search_key("Hello", "songs").unwrap()
        );
        assert!(search_key("Hello", "unsupported").is_err());
    }

    #[test]
    fn shared_cache_is_bounded_lru_and_expires_without_sliding_ttl() {
        let now = Instant::now();
        let mut cache = Cache::new();
        let oldest = search_key("0", "all").unwrap();
        for index in 0..CACHE_LIMIT {
            cache.insert(search_key(&index.to_string(), "all").unwrap(), index, now);
        }
        assert_eq!(cache.get(&oldest, now + Duration::from_secs(60)), Some(0));
        cache.insert(CacheKey::Completions("hello".into()), 100, now);
        assert_eq!(cache.entries.len(), CACHE_LIMIT);
        assert!(cache.get(&search_key("1", "all").unwrap(), now).is_none());
        cache.insert(
            CacheKey::Enrich {
                kind: "Artist".into(),
                id: "artist".into(),
            },
            101,
            now,
        );
        assert_eq!(cache.entries.len(), CACHE_LIMIT);
        assert!(cache.get(&oldest, now + CACHE_TTL).is_none());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn crate_top_fixture_resolves_artist_without_inventing_endpoint() {
        let top: TopResult = serde_json::from_value(serde_json::json!({
            "result_name": "Arijit Singh", "result_type": "Artist", "thumbnails": []
        }))
        .unwrap();
        let artist: SearchResultArtist = serde_json::from_value(serde_json::json!({
            "artist": "Arijit Singh", "browse_id": "UC-real-artist", "subscribers": "10M subscribers", "thumbnails": []
        })).unwrap();
        let identity = top_identity(&top).unwrap();
        let mut page = SearchPage::default();
        assert!(matches!(
            resolve_identity(&identity, &page),
            Resolution::Missing
        ));
        page.artists.push(artist_entry(artist));
        let resolved = resolve_top(Some(&identity), &page, "arijit").unwrap();
        assert_eq!(resolved.id, "UC-real-artist");
        assert_eq!(resolved.subtitle, "10M subscribers");
        assert_eq!(resolved.artist_id, resolved.id);
    }

    #[test]
    fn known_top_metadata_cannot_be_overridden_by_fallback() {
        let mut album = entry("Album", "wrong-album", "Aashiqui 2");
        album.artist = "Different artist".into();
        album.year = "2020".into();
        let page = SearchPage {
            albums: vec![album],
            ..Default::default()
        };
        let identity = TopIdentity {
            kind: "Album".into(),
            title: "Aashiqui 2".into(),
            artist: "Mithoon".into(),
            year: "2013".into(),
            ..Default::default()
        };
        assert!(resolve_top(Some(&identity), &page, "Aashiqui 2").is_none());
    }

    #[test]
    fn unusable_top_allows_an_independent_unique_exact_query_match() {
        let identity = TopIdentity {
            kind: "Artist".into(),
            title: "Other artist".into(),
            ..Default::default()
        };
        let page = SearchPage {
            songs: vec![entry("Song", "song", "Tum Hi Ho")],
            ..Default::default()
        };
        assert_eq!(
            resolve_top(Some(&identity), &page, "tum hi ho").unwrap().id,
            "song"
        );
    }

    #[test]
    fn song_conversion_preserves_multi_name_credit_without_guessing_artist_id() {
        let song: SearchResultSong = serde_json::from_value(serde_json::json!({
            "title": "Song", "artist": "First, Second", "album": { "name": "Album", "id": "album-id" },
            "duration": "4:22", "plays": "1000", "explicit": "NotExplicit", "video_id": "video-id",
            "thumbnails": [
                { "url": "https://example.test/small.jpg", "width": 120, "height": 120 },
                { "url": "https://example.test/medium.jpg", "width": 320, "height": 320 },
                { "url": "https://example.test/large.jpg", "width": 1200, "height": 1200 }
            ]
        })).unwrap();
        let entry = song_entry(song);
        assert_eq!(entry.artist, "First, Second");
        assert!(entry.artist_id.is_empty());
        assert_eq!(entry.album_id, "album-id");
        assert_eq!(entry.duration_secs, 262);
        assert_eq!(entry.subtitle, "First, Second - Album");
        assert_eq!(entry.thumbnail_url, "https://example.test/medium.jpg");
        assert_eq!(duration_secs("1:02:03"), 3723);
        assert_eq!(duration_secs("4:99"), 0);
        assert_eq!(duration_secs("unknown"), 0);
    }

    #[test]
    fn basic_playlists_exclude_podcasts_and_empty_or_duplicate_ids() {
        let mut fixture = serde_json::to_value(SearchResults::default()).unwrap();
        fixture["featured_playlists"] = serde_json::json!([
            { "title": "Public", "author": "YouTube Music", "songs": "20 songs", "playlist_id": "public-id", "thumbnails": [] }
        ]);
        fixture["community_playlists"] = serde_json::json!([
            { "Playlist": { "title": "Duplicate", "author": "Curator", "views": "10 views", "playlist_id": "public-id", "thumbnails": [] } },
            { "Playlist": { "title": "Community", "author": "Curator", "views": "20 views", "playlist_id": "community-id", "thumbnails": [] } },
            { "Playlist": { "title": "Missing", "author": "Curator", "views": "0", "playlist_id": "", "thumbnails": [] } },
            { "Podcast": { "title": "Podcast", "publisher": "Publisher", "podcast_id": "podcast-id", "thumbnails": [] } }
        ]);
        let page = unfiltered_page(serde_json::from_value(fixture).unwrap());
        assert_eq!(
            page.playlists
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["public-id", "community-id"]
        );
        assert!(page
            .playlists
            .iter()
            .all(|entry| entry.kind == "Playlist" && entry.subtitle != "Playlist"));
    }

    #[test]
    fn album_catalogue_uses_ordered_tracks_and_only_real_credit_ids() {
        let album: GetAlbum = serde_json::from_value(serde_json::json!({
            "title": "Aashiqui 2", "category": "Album", "thumbnails": [], "artist_thumbnails": [],
            "artists": [{ "name": "First", "id": "UC-first" }, { "name": "Second", "id": null }],
            "year": "2013", "duration": "10 minutes", "library_status": "LIBRARY_ADD",
            "tracks": [
                { "video_id": "second", "track_no": 2, "duration": "4:00", "plays": "10", "title": "Second track", "like_status": "INDIFFERENT", "explicit": "NotExplicit" },
                { "video_id": "first", "track_no": 1, "duration": "3:00", "plays": "20", "title": "First track", "like_status": "INDIFFERENT", "explicit": "NotExplicit" }
            ]
        })).unwrap();
        let catalogue = album_catalogue(album, "album-id");
        assert_eq!(
            catalogue
                .songs
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(catalogue.songs[0].artist, "First, Second");
        assert_eq!(catalogue.songs[0].album_id, "album-id");
        assert_eq!(catalogue.songs[0].duration_secs, 180);
        assert_eq!(catalogue.artists.len(), 1);
        assert_eq!(catalogue.artists[0].id, "UC-first");
    }

    #[test]
    fn artist_catalogue_maps_popular_songs_releases_and_related() {
        let artist: GetArtist = serde_json::from_value(serde_json::json!({
            "name": "Arijit Singh", "channel_id": "UC-arijit", "subscribed": false, "thumbnails": [],
            "top_releases": {
                "songs": { "browse_id": "popular-list", "results": [
                    { "video_id": "popular-song", "plays": "100", "album": { "name": "Album", "id": "album-id" },
                      "artists": [{ "name": "Arijit Singh", "id": "UC-arijit" }], "title": "Popular song", "like_status": "INDIFFERENT", "explicit": "NotExplicit" }
                ] },
                "albums": { "results": [
                    { "title": "Release", "year": "2013", "album_id": "release-id", "library_status": "LIBRARY_ADD", "thumbnails": [], "explicit": "NotExplicit" }
                ] },
                "related": { "results": [{ "browse_id": "UC-related", "title": "Related", "subscribers": "1M subscribers" }] }
            }
        })).unwrap();
        let catalogue = artist_catalogue(artist);
        assert_eq!(catalogue.songs[0].id, "popular-song");
        assert_eq!(catalogue.songs[0].artist_id, "UC-arijit");
        assert_eq!(catalogue.albums[0].id, "release-id");
        assert_eq!(catalogue.artists[0].id, "UC-related");
        assert!(catalogue.playlists.is_empty());
    }

    #[test]
    fn enrichment_cache_does_not_replace_top_or_query_playlists() {
        let catalogue = SearchPage {
            songs: vec![entry("Song", "catalogue-song", "Catalogue")],
            ..Default::default()
        };
        let mut cache = Cache::new();
        let now = Instant::now();
        let key = CacheKey::Enrich {
            kind: "Artist".into(),
            id: "UC-CaseSensitive".into(),
        };
        cache.insert(key.clone(), CachedValue::Catalogue(catalogue), now);
        for playlist_id in ["query-one-playlist", "query-two-playlist"] {
            let base = SearchPage {
                top: Some(entry("Artist", "UC-CaseSensitive", "Artist name")),
                intent: "Artist".into(),
                playlists: vec![entry("Playlist", playlist_id, "Public")],
                ..Default::default()
            };
            let Some(CachedValue::Catalogue(catalogue)) = cache.get(&key, now) else {
                panic!("missing catalogue cache");
            };
            let enriched = merge_catalogue(&base, &catalogue);
            assert_eq!(enriched.top.unwrap().id, base.top.unwrap().id);
            assert_eq!(enriched.playlists[0].id, playlist_id);
            assert_eq!(enriched.songs[0].id, "catalogue-song");
        }
        assert!(cache
            .get(
                &CacheKey::Enrich {
                    kind: "Artist".into(),
                    id: "uc-casesensitive".into()
                },
                now
            )
            .is_none());
    }

    #[test]
    fn optional_playlist_supplement_preserves_entities_and_existing_playlists() {
        let mut page = SearchPage {
            top: Some(entry("Artist", "UC-arijit", "Arijit Singh")),
            intent: "Artist".into(),
            songs: vec![entry("Song", "tum-audio", "Tum Hi Ho")],
            albums: vec![entry("Album", "MPRE-forever", "Arijit Forever")],
            artists: vec![entry("Artist", "UC-arijit", "Arijit Singh")],
            ..Default::default()
        };
        merge_playlist_supplement(&mut page, Err("optional request failed".into()));
        assert!(page.playlists.is_empty());
        merge_playlist_supplement(&mut page, Ok(SearchPage {
            top: Some(entry("Playlist", "unexpected-top", "Not a replacement")),
            songs: vec![entry("Song", "unexpected-song", "Not a replacement")],
            playlists: vec![entry("Playlist", "public-list", "Public"), entry("Playlist", "public-list", "Duplicate")],
            ..Default::default()
        }));
        merge_playlist_supplement(&mut page, Ok(SearchPage {
            playlists: vec![entry("Playlist", "other-list", "Other")],
            ..Default::default()
        }));
        assert_eq!(page.top.unwrap().id, "UC-arijit");
        assert_eq!(page.intent, "Artist");
        assert_eq!(page.songs[0].id, "tum-audio");
        assert_eq!(page.albums[0].id, "MPRE-forever");
        assert_eq!(page.artists[0].id, "UC-arijit");
        assert_eq!(page.playlists.len(), 1);
        assert_eq!(page.playlists[0].id, "public-list");
    }

    #[test]
    fn completion_runs_are_joined_trimmed_and_deduplicated() {
        let suggestions: Vec<SearchSuggestion> = serde_json::from_value(serde_json::json!([
            { "runs": [{ "Normal": "Arijit " }, { "Bold": "Singh" }], "suggestion_type": "Prediction" },
            { "runs": [{ "Normal": " arijit singh " }], "suggestion_type": "Prediction" },
            { "runs": [{ "Normal": "   " }], "suggestion_type": "Prediction" }
        ])).unwrap();
        assert_eq!(completion_texts(suggestions), ["Arijit Singh"]);
    }

    fn card_response(kind: &str, title: &str, endpoint: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "contents": { "tabbedSearchResultsRenderer": { "tabs": [{ "tabRenderer": {
                "content": { "sectionListRenderer": { "contents": [{ "musicCardShelfRenderer": {
                    "title": { "runs": [{ "text": title, "navigationEndpoint": endpoint }] },
                    "subtitle": { "runs": [{ "text": kind }] },
                    "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [] } } },
                    "contents": [{ "musicResponsiveListItemRenderer": {
                        "flexColumns": [
                            { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Tum Hi Ho" }] } } },
                            { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Song" }, { "text": " - " }, { "text": "4:22" }] } } }
                        ]
                    } }]
                } }] } }
            } }] } }
        })
    }

    #[test]
    fn compact_card_preserves_provider_endpoint_and_resolves_duplicate_names() {
        for (kind, title, id) in [
            ("Artist", "Arijit Singh", "UC-real-artist"),
            ("Album", "Aashiqui 2", "MPRE-real-album"),
            ("Song", "Tum Hi Ho", "real-song"),
        ] {
            let endpoint = if kind == "Song" {
                serde_json::json!({ "watchEndpoint": { "videoId": id } })
            } else {
                serde_json::json!({ "browseEndpoint": { "browseId": id } })
            };
            let (results, identity) =
                parse_unfiltered_results(title, card_response(kind, title, endpoint)).unwrap();
            assert_eq!(results.top_results.len(), 1);
            let identity = identity.unwrap();
            assert_eq!(identity.id, id);
            let mut page = unfiltered_page(results);
            if kind == "Song" {
                assert!(resolve_top(Some(&identity), &page, title).is_none());
            } else {
                assert_eq!(resolve_top(Some(&identity), &page, title).unwrap().id, id);
                assert_eq!(targeted_category(Some(&identity), &page), None);
            }
            let candidates = vec![entry(kind, "wrong-endpoint", title), entry(kind, id, title)];
            match kind {
                "Artist" => page.artists = candidates,
                "Album" => page.albums = candidates,
                _ => page.songs = candidates,
            }
            assert_eq!(resolve_top(Some(&identity), &page, title).unwrap().id, id);
            let mut wrong_case = identity;
            wrong_case.id = id.to_uppercase();
            assert!(matches!(resolve_identity(&wrong_case, &page), Resolution::Missing));
        }
    }

    #[test]
    fn compact_arijit_card_preserves_song_and_album_previews() {
        let mut raw = card_response("Artist", "Arijit Singh", serde_json::json!({
            "browseEndpoint": { "browseId": "UC-arijit" }
        }));
        let mut rows: Vec<_> = [
            ("Song", "Sukoon Mila", "sukoon-audio", "3:19"),
            ("Song", "Tum Hi Ho", "tum-audio", "4:22"),
            ("Album", "Arijit Forever", "MPRE-forever", "2023"),
            ("Video", "Tum Hi Ho", "tum-video", "4:22"),
            ("Song", "Missing endpoint", "", "3:00"),
        ].into_iter().map(|(kind, title, id, duration)| {
            let endpoint = if kind == "Album" {
                serde_json::json!({ "browseEndpoint": { "browseId": id,
                    "browseEndpointContextSupportedConfigs": { "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ALBUM" } }
                } })
            } else {
                serde_json::json!({ "watchEndpoint": { "videoId": id,
                    "watchEndpointMusicSupportedConfigs": { "watchEndpointMusicConfig": {
                        "musicVideoType": if kind == "Video" { "MUSIC_VIDEO_TYPE_OMV" } else { "MUSIC_VIDEO_TYPE_ATV" }
                    } }
                } })
            };
            serde_json::json!({ "musicResponsiveListItemRenderer": {
                "navigationEndpoint": endpoint,
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": title }] } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                        { "text": kind }, { "text": " \u{2022} " }, { "text": duration }
                    ] } } }
                ],
                "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [
                    { "url": "small", "width": 60, "height": 60 },
                    { "url": "target", "width": 320, "height": 320 },
                    { "url": "large", "width": 640, "height": 640 }
                ] } } }
            } })
        }).collect();
        let mut mislabeled_video = rows[3].clone();
        mislabeled_video.pointer_mut("/musicResponsiveListItemRenderer/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text")
            .unwrap().clone_from(&serde_json::json!("Song"));
        rows.push(mislabeled_video);
        rows.push(rows[0].clone());
        raw.pointer_mut("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicCardShelfRenderer").unwrap()["contents"] = serde_json::json!(rows);
        let (results, identity) = parse_unfiltered_results("Arijit Singh", raw).unwrap();
        let page = unfiltered_page(results);
        assert_eq!(page.songs.iter().map(|song| (song.kind.as_str(), song.id.as_str(), song.title.as_str(), song.duration_secs))
            .collect::<Vec<_>>(), [("Song", "sukoon-audio", "Sukoon Mila", 199), ("Song", "tum-audio", "Tum Hi Ho", 262)]);
        assert_eq!(page.albums.len(), 1);
        assert_eq!((&*page.albums[0].kind, &*page.albums[0].id, &*page.albums[0].title, &*page.albums[0].year),
            ("Album", "MPRE-forever", "Arijit Forever", "2023"));
        assert!(page.songs.iter().chain(&page.albums).all(|entry| entry.thumbnail_url == "target" && entry.artist == "Arijit Singh"));
        assert_eq!(resolve_top(identity.as_ref(), &page, "Arijit Singh").unwrap().id, "UC-arijit");
    }

    fn typed_song(id: &str, title: &str) -> SearchResultSong {
        serde_json::from_value(serde_json::json!({
            "title": title, "artist": "Sabrina Carpenter", "album": { "name": "Short n' Sweet", "id": "album-id" },
            "duration": "2:56", "plays": "1000", "explicit": "NotExplicit", "video_id": id,
            "thumbnails": []
        })).unwrap()
    }

    fn music_video_response(title: &str, artist: &str, video_type: &str) -> serde_json::Value {
        let mut raw = card_response("Unrecognized", title, serde_json::json!({
            "watchEndpoint": {
                "videoId": "official-video",
                "watchEndpointMusicSupportedConfigs": {
                    "watchEndpointMusicConfig": { "musicVideoType": video_type }
                }
            }
        }));
        let card = raw.pointer_mut("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicCardShelfRenderer").unwrap();
        if !artist.is_empty() {
            card["subtitle"]["runs"] = serde_json::json!([
                { "text": artist, "navigationEndpoint": { "browseEndpoint": {
                    "browseId": "UC-artist", "browseEndpointContextSupportedConfigs": {
                        "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ARTIST" }
                    }
                } } },
                { "text": " - " }, { "text": "2026" }
            ]);
        }
        raw
    }

    #[test]
    fn empty_song_shelf_fallback_requires_known_music_video() {
        assert!(!needs_music_video_songs(None, &SearchPage::default()));
        for video_type in ["MUSIC_VIDEO_TYPE_OMV", "MUSIC_VIDEO_TYPE_ATV", "MUSIC_VIDEO_TYPE_UGC", ""] {
            let raw = music_video_response("Mera Yaar (Official Video)", "B Praak", video_type);
            let (results, identity) = parse_unfiltered_results("Mera Yaar B Praak", raw).unwrap();
            let identity = identity.unwrap();
            assert_eq!(identity.kind, "Video");
            assert_eq!(identity.artist, "B Praak");
            let mut page = unfiltered_page(results);
            assert!(page.songs.is_empty());
            assert_eq!(needs_music_video_songs(Some(&identity), &page),
                matches!(video_type, "MUSIC_VIDEO_TYPE_OMV" | "MUSIC_VIDEO_TYPE_ATV"));
            assert_eq!(targeted_category(Some(&identity), &page), None);
            page.songs.push(entry("Song", "audio", "Mera Yaar"));
            assert!(!needs_music_video_songs(Some(&identity), &page));
        }
        for kind in ["Song", "Artist", "Album", "Playlist"] {
            let identity = TopIdentity { kind: kind.into(), music_video: true, ..Default::default() };
            assert!(!needs_music_video_songs(Some(&identity), &SearchPage::default()));
        }
    }

    #[test]
    fn raw_song_recovers_artist_endpoint_when_parser_omits_artist() {
        let mut raw = music_video_response("Mera Yaar", "B Praak", "MUSIC_VIDEO_TYPE_ATV");
        let card = raw.pointer_mut("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicCardShelfRenderer").unwrap().clone();
        let mut results = parse_unfiltered_results("Mera Yaar B Praak", raw.take()).unwrap().0;
        results.top_results[0].result_type = Some(TopResultType::Song);
        results.top_results[0].artist = None;
        let identity = card_identity(&results, Some(&card)).unwrap();
        assert_eq!(identity.kind, "Song");
        assert_eq!(identity.artist, "B Praak");
    }

    fn music_identity(title: &str, artist: &str) -> TopIdentity {
        let raw = music_video_response(title, artist, "MUSIC_VIDEO_TYPE_OMV");
        parse_unfiltered_results(title, raw).unwrap().1.unwrap()
    }

    fn credited_song(id: &str, title: &str, artist: &str) -> SearchEntry {
        SearchEntry { artist: artist.into(), ..entry("Song", id, title) }
    }

    #[test]
    fn music_video_resolves_different_audio_id_using_endpoint_artist() {
        for video_type in ["MUSIC_VIDEO_TYPE_OMV", "MUSIC_VIDEO_TYPE_ATV"] {
            let raw = music_video_response("Mera Yaar (Official Video)", "B Praak", video_type);
            let (results, identity) = parse_unfiltered_results("Mera Yaar B Praak", raw).unwrap();
            let identity = identity.unwrap();
            let mut page = unfiltered_page(results);
            assert!(needs_music_video_songs(Some(&identity), &page));
            let mut original = credited_song("original-audio", "Mera Yaar", "B Praak");
            original.album_id = "original-album".into();
            original.duration_secs = 257;
            page.songs = vec![
                credited_song("other-artist", "Mera Yaar", "Other Artist"),
                credited_song("remix", "Mera Yaar (Remix)", "B Praak"),
                original,
                credited_song("other-song", "Other Title", "B Praak"),
            ];
            let order: Vec<_> = page.songs.iter().map(|song| song.id.clone()).collect();
            let top = resolve_top(Some(&identity), &page, "Mera Yaar B Praak").unwrap();
            assert_eq!((top.kind.as_str(), top.id.as_str(), top.title.as_str()), ("Song", "original-audio", "Mera Yaar"));
            assert_ne!(top.id, identity.id);
            assert_eq!(top.album_id, "original-album");
            assert_eq!(top.duration_secs, 257);
            assert_eq!(page.songs.iter().map(|song| song.id.clone()).collect::<Vec<_>>(), order);
            assert!(!needs_music_video_songs(Some(&identity), &page));
        }
    }

    #[test]
    fn official_suffix_normalization_is_terminal_and_narrow() {
        for suffix in ["(Official Video)", "[OFFICIAL MUSIC VIDEO]", "(Music Video)", "[Audio]", "(official audio)"] {
            assert_eq!(song_title(&format!("  Mera Yaar {suffix}  ")), "mera yaar");
        }
        for title in [
            "Mera Yaar (Live)", "Mera Yaar (Remix)", "Mera Yaar [Cover]", "Mera Yaar (Lyrics)",
            "Mera Yaar (Official Video) Live", "Mera Yaar | Official Video", "Mera Yaar (Official Video 4K)",
            "Mera Yaar [Video]", "Mera Yaar (Audio]", "AC/DC",
        ] {
            assert_eq!(song_title(title), normalize(title));
        }
        let page = SearchPage { songs: vec![credited_song("original", "Mera Yaar", "B Praak")], ..Default::default() };
        for title in ["Mera Yaar (Live)", "Mera Yaar (Remix)", "Mera Yaar (Official Video) Live"] {
            let identity = music_identity(title, "B Praak");
            assert!(resolve_top(Some(&identity), &page, "unrelated query").is_none());
        }
    }

    #[test]
    fn exact_combined_query_requires_endpoint_artist_for_verbose_video_title() {
        let page = SearchPage {
            songs: vec![credited_song("original", "Mera Yaar", "B Praak")],
            ..Default::default()
        };
        for title in ["Mera Yaar (Official Video)", "Mera Yaar | B Praak | Official Video"] {
            let identity = music_identity(title, "B Praak");
            let top = resolve_top(Some(&identity), &page, "  mera YAAR b praak ").unwrap();
            assert_eq!(top.id, "original");
            let unknown_artist = music_identity(title, "");
            assert!(resolve_top(Some(&unknown_artist), &page, "Mera Yaar B Praak").is_none());
            let wrong_artist = music_identity(title, "Other Artist");
            assert!(resolve_top(Some(&wrong_artist), &page, "Mera Yaar B Praak").is_none());
        }
    }

    #[test]
    fn music_video_without_artist_needs_unique_exact_query_title() {
        let identity = music_identity("Verbose music video title", "");
        let mut page = SearchPage {
            songs: vec![credited_song("audio", "Chiggy Wiggy", "Kylie Minogue, Sonu Nigam")],
            ..Default::default()
        };
        assert_eq!(resolve_top(Some(&identity), &page, " chiggy WIGGY ").unwrap().id, "audio");
        assert!(resolve_top(Some(&identity), &page, "Chiggy").is_none());
        assert!(resolve_top(Some(&identity), &page, "Chiggy Wiggy Kylie Minogue").is_none());
        page.songs.push(credited_song("other-recording", "Chiggy Wiggy", "Kylie Minogue, Sonu Nigam"));
        for _ in 0..2 {
            assert!(resolve_top(Some(&identity), &page, "Chiggy Wiggy").is_none());
            page.songs.reverse();
        }
        page.songs.clear();
        assert!(resolve_top(Some(&identity), &page, "Chiggy Wiggy").is_none());
    }

    #[test]
    fn ambiguous_song_artists_stay_unresolved_regardless_of_order() {
        let unknown_artist = music_identity("Mera Yaar (Official Video)", "");
        let known_artist = music_identity("Mera Yaar (Official Video)", "B Praak");
        let mut page = SearchPage {
            songs: vec![
                credited_song("cover", "Mera Yaar", "Cover Artist"),
                credited_song("original", "Mera Yaar", "B Praak"),
            ],
            ..Default::default()
        };
        for _ in 0..2 {
            assert!(resolve_top(Some(&unknown_artist), &page, "Mera Yaar").is_none());
            assert_eq!(resolve_top(Some(&known_artist), &page, "Mera Yaar").unwrap().id, "original");
            page.songs.reverse();
        }
    }

    #[test]
    fn provider_rank_only_breaks_ties_with_strong_title_and_artist() {
        let identity = music_identity("Mera Yaar (Official Video)", "B Praak");
        let mut page = SearchPage {
            songs: vec![
                credited_song("wrong-title", "Other Song", "B Praak"),
                credited_song("wrong-artist", "Mera Yaar", "Other Artist"),
                credited_song("ranked-first", "Mera Yaar", "B Praak"),
                credited_song("ranked-second", "Mera Yaar", "B Praak"),
            ],
            ..Default::default()
        };
        assert_eq!(resolve_top(Some(&identity), &page, "Mera Yaar B Praak").unwrap().id, "ranked-first");
        page.songs.swap(2, 3);
        assert_eq!(resolve_top(Some(&identity), &page, "Mera Yaar B Praak").unwrap().id, "ranked-second");
        page.songs.truncate(2);
        assert!(resolve_top(Some(&identity), &page, "Mera Yaar B Praak").is_none());
        assert_eq!(page.songs.len(), 2);
    }

    #[test]
    fn combined_query_uses_one_real_endpoint_credit_despite_different_collaborators() {
        let mut identity = music_identity("Mera Yaar (Official Video)", "B Praak");
        identity.artist = "B Praak, Jaani".into();
        identity.artist_credits.push("Jaani".into());
        let mut page = SearchPage {
            songs: vec![
                credited_song("soundtrack", "Mera Yaar (From \"Lekh\")", "Gurnam Bhullar & B Praak"),
                credited_song("other-credit", "Mera Yaar", "Jaani"),
                credited_song("original", "Mera Yaar", "B Praak & Dilnoor"),
                credited_song("other-title", "Mere Yaara Ve", "B Praak"),
            ],
            ..Default::default()
        };
        for _ in 0..2 {
            assert_eq!(resolve_top(Some(&identity), &page, "Mera Yaar B Praak").unwrap().id, "original");
            assert!(resolve_top(Some(&identity), &page, "Mera Yaar").is_none());
            page.songs.reverse();
        }
        assert!(has_artist_credit("B Praak & Dilnoor", "B Praak"));
        assert!(has_artist_credit("B Praak, Jaani", "Jaani"));
        assert!(has_artist_credit("AC/DC", "AC/DC"));
        assert!(!has_artist_credit("B Praakish & Dilnoor", "B Praak"));
        assert!(!has_artist_credit("Not B Praak", "B Praak"));
        assert!(!has_artist_credit("B Praak", ""));
    }

    fn response_with_songs(mut raw: serde_json::Value, songs: &[(&str, &str)]) -> serde_json::Value {
        let rows: Vec<_> = songs.iter().map(|(id, title)| serde_json::json!({
            "musicResponsiveListItemRenderer": {
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": title }] } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [
                        { "text": "Sabrina Carpenter" }, { "text": " \u{2022} " },
                        { "text": "Short n' Sweet", "navigationEndpoint": { "browseEndpoint": { "browseId": "album-id" } } },
                        { "text": " \u{2022} " }, { "text": "2:56" }
                    ] } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "1000 plays" }] } } }
                ],
                "playlistItemData": { "videoId": id },
                "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [] } } }
            }
        })).collect();
        raw.pointer_mut("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
            .unwrap().as_array_mut().unwrap().push(serde_json::json!({
                "musicShelfRenderer": { "title": { "runs": [{ "text": "Songs" }] }, "contents": rows }
            }));
        raw
    }

    #[test]
    fn raw_untyped_card_promotes_only_the_same_typed_song_id() {
        let raw = response_with_songs(
            card_response("Sabrina Carpenter", "Espresso (Audio)", serde_json::Value::Null),
            &[("other-audio", "Espresso"), ("real-audio", "Espresso")],
        );
        for pointer in [
            "/title/runs/0/navigationEndpoint",
            "/onTap",
            "/playButton/musicPlayButtonRenderer/playNavigationEndpoint",
        ] {
            let mut raw = raw.clone();
            let card = raw.pointer_mut("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicCardShelfRenderer").unwrap();
            let endpoint = serde_json::json!({ "watchEndpoint": { "videoId": "real-audio" } });
            match pointer {
                "/onTap" => card["onTap"] = endpoint,
                "/title/runs/0/navigationEndpoint" => card["title"]["runs"][0]["navigationEndpoint"] = endpoint,
                _ => card["playButton"] = serde_json::json!({ "musicPlayButtonRenderer": { "playNavigationEndpoint": endpoint } }),
            }
            let (results, identity) = parse_unfiltered_results("Espresso Sabrina Carpenter", raw).unwrap();
            let identity = identity.unwrap();
            let page = unfiltered_page(results);
            let top = resolve_top(Some(&identity), &page, "Espresso Sabrina Carpenter").unwrap();
            assert_eq!((top.kind.as_str(), top.id.as_str()), ("Song", "real-audio"));
            assert_eq!(top.title, "Espresso");
            assert_eq!(top.album_id, "album-id");
            assert_eq!(top.duration_secs, 176);
            assert_eq!(page.songs.len(), 2);
            assert_eq!(targeted_category(Some(&identity), &page), None);
        }
    }

    #[test]
    fn unsupported_video_keeps_ordinary_songs_without_title_promotion() {
        for (kind, song_id) in [("Sabrina Carpenter", "different-audio"), ("Sabrina Carpenter", "VIDEO-ONLY"), ("Video", "video-only")] {
            let raw = response_with_songs(
                card_response(kind, "Espresso", serde_json::json!({ "watchEndpoint": { "videoId": "video-only" } })),
                &[(song_id, "Espresso")],
            );
            let (results, identity) = parse_unfiltered_results("Espresso", raw).unwrap();
            let identity = identity.unwrap();
            assert_eq!(identity.kind, "Video");
            let page = unfiltered_page(results);
            assert!(resolve_top(Some(&identity), &page, "Espresso").is_none());
            assert_eq!(page.songs.len(), 1);
            assert_eq!(targeted_category(Some(&identity), &page), None);
        }
    }

    #[test]
    fn no_provider_identity_uses_only_unique_already_returned_titles() {
        let raw = response_with_songs(
            card_response("Unrecognized", "Other title", serde_json::Value::Null),
            &[("real-audio", "Espresso")],
        );
        let (results, identity) = parse_unfiltered_results("Espresso", raw).unwrap();
        assert!(identity.is_none());
        let mut page = unfiltered_page(results);
        assert_eq!(targeted_category(None, &page), None);
        assert_eq!(resolve_top(None, &page, "Espresso").unwrap().id, "real-audio");
        page.playlists.push(entry("Playlist", "VLPL-real", "Espresso"));
        assert!(resolve_top(None, &page, "Espresso").is_none());
        page.songs.clear();
        page.playlists.push(entry("Playlist", "PL-real", "Espresso"));
        assert_eq!(resolve_top(None, &page, "Espresso").unwrap().kind, "Playlist");
        assert_eq!(targeted_category(None, &page), None);
    }

    #[test]
    fn playlist_card_resolves_typed_browse_or_play_navigation_without_song_lookup() {
        for (endpoint, play) in [
            (serde_json::json!({ "browseEndpoint": { "browseId": "VLPL-real" } }), false),
            (serde_json::json!({ "watchPlaylistEndpoint": { "playlistId": "PL-real" } }), true),
            (serde_json::json!({ "watchEndpoint": { "playlistId": "PL-real", "videoId": "song-id" } }), true),
        ] {
            let mut raw = card_response("Playlist", "Daily Mix", endpoint.clone());
            if play {
                let card = raw.pointer_mut("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicCardShelfRenderer").unwrap();
                card["title"]["runs"][0]["navigationEndpoint"] = serde_json::Value::Null;
                card["playButton"] = serde_json::json!({ "musicPlayButtonRenderer": { "playNavigationEndpoint": endpoint } });
            }
            let (mut results, identity) = parse_unfiltered_results("Daily Mix", raw).unwrap();
            results.featured_playlists.push(serde_json::from_value(serde_json::json!({
                "title": "Daily Mix", "author": "Curator", "songs": "20 songs", "playlist_id": "VLPL-real", "thumbnails": []
            })).unwrap());
            results.songs.push(typed_song("song-id", "Daily Mix"));
            let identity = identity.unwrap();
            assert_eq!(identity.kind, "Playlist");
            let page = unfiltered_page(results);
            let top = resolve_top(Some(&identity), &page, "Daily Mix").unwrap();
            assert_eq!((top.kind.as_str(), top.id.as_str()), ("Playlist", "VLPL-real"));
            assert_eq!(top.subtitle, "Curator - 20 songs");
            assert_eq!(targeted_category(Some(&identity), &page), None);
            assert!(resolve_top(None, &page, "Daily Mix").is_none());
        }
    }

    #[test]
    fn raw_album_metadata_preserves_omitted_provider_entity_without_extra_query() {
        let mut raw = card_response("Album", "Aashiqui 2", serde_json::json!({ "browseEndpoint": { "browseId": "MPRE-real" } }));
        let card = raw.pointer_mut("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicCardShelfRenderer").unwrap();
        card["subtitle"]["runs"] = serde_json::json!([
            { "text": "Album" }, { "text": " - " },
            { "text": "Mithoon", "navigationEndpoint": { "browseEndpoint": {
                "browseId": "UC-real", "browseEndpointContextSupportedConfigs": {
                    "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ARTIST" }
                }
            } } }, { "text": " - " }, { "text": "2013" }
        ]);
        card["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"] = serde_json::json!([
            { "url": "https://example.test/album.jpg", "width": 320, "height": 320 }
        ]);
        let (results, identity) = parse_unfiltered_results("Aashiqui 2", raw).unwrap();
        let identity = identity.unwrap();
        let page = unfiltered_page(results);
        assert!(page.albums.is_empty());
        assert_eq!(targeted_category(Some(&identity), &page), None);
        let top = resolve_top(Some(&identity), &page, "Aashiqui 2").unwrap();
        assert_eq!(top.id, "MPRE-real");
        assert_eq!(top.album_id, top.id);
        assert_eq!(top.artist, "Mithoon");
        assert_eq!(top.year, "2013");
        assert_eq!(top.subtitle, "Mithoon - 2013");
        assert_eq!(top.thumbnail_url, "https://example.test/album.jpg");
    }

    #[test]
    fn endpointless_card_requires_unique_targeted_metadata_match() {
        let (results, identity) = parse_unfiltered_results(
            "arijit",
            card_response("Artist", "Arijit Singh", serde_json::Value::Null),
        )
        .unwrap();
        let identity = identity.unwrap();
        assert!(identity.id.is_empty());
        let mut page = unfiltered_page(results);
        assert!(resolve_top(Some(&identity), &page, "arijit").is_none());
        add_candidates(
            &mut page,
            SearchPage {
                artists: vec![
                    entry("Artist", "unrelated", "Other artist"),
                    entry("Artist", "real", "Arijit Singh"),
                ],
                ..Default::default()
            },
            |entry| matches_identity(&identity, entry),
        );
        assert_eq!(page.artists.len(), 1);
        assert_eq!(
            resolve_top(Some(&identity), &page, "arijit").unwrap().id,
            "real"
        );
        page.artists
            .push(entry("Artist", "duplicate-name", "Arijit Singh"));
        assert!(resolve_top(Some(&identity), &page, "arijit").is_none());
    }

    #[test]
    fn untyped_video_card_does_not_invent_a_song_identity() {
        let (results, identity) = parse_unfiltered_results(
            "Tum Hi Ho",
            card_response(
                "Mar 23, 2013",
                "Tum Hi Ho",
                serde_json::json!({ "browseEndpoint": { "browseId": "MPEDNUo8CKI34o4" } }),
            ),
        )
        .unwrap();
        assert!(identity.is_none());
        assert!(resolve_top(None, &unfiltered_page(results), "Tum Hi Ho").is_none());
    }

    #[test]
    fn targeted_cache_keeps_original_query_and_category_separate() {
        let mut cache = Cache::new();
        let now = Instant::now();
        cache.insert(search_key("Aashiqui2", "All").unwrap(), "original all", now);
        cache.insert(
            search_key("Aashiqui 2", "Albums").unwrap(),
            "targeted albums",
            now,
        );
        assert!(cache
            .get(&search_key("Aashiqui2", "Albums").unwrap(), now)
            .is_none());
        cache.insert(
            search_key("Aashiqui2", "Albums").unwrap(),
            "original albums",
            now,
        );
        assert_eq!(
            cache.get(&search_key("Aashiqui2", "All").unwrap(), now),
            Some("original all")
        );
        assert_eq!(
            cache.get(&search_key("Aashiqui 2", "Albums").unwrap(), now),
            Some("targeted albums")
        );
        assert_eq!(
            cache.get(&search_key("Aashiqui2", "Albums").unwrap(), now),
            Some("original albums")
        );
    }

    #[test]
    fn number_spacing_only_expands_letter_digit_boundaries() {
        assert_eq!(space_title_number("Aashiqui2"), "Aashiqui 2");
        assert_eq!(space_title_number("Aashiqui 2"), "Aashiqui 2");
        assert_eq!(space_title_number("Album2013"), "Album 2013");
        assert_eq!(space_title_number("AC/DC 2:30"), "AC/DC 2:30");
        assert_eq!(space_title_number("Arijit Singh"), "Arijit Singh");
    }

    #[test]
    fn spaced_query_retries_missing_or_weak_song_and_video_identity() {
        let query = "Aashiqui2";
        let spaced = space_title_number(query);
        assert!(needs_spaced_query(None, query, &spaced));
        assert!(!needs_spaced_query(None, &spaced, &spaced));
        for kind in ["Video", "Song"] {
            for music_video in [false, true] {
                let identity = TopIdentity {
                    kind: kind.into(),
                    title: "Tum Hi Ho (Official Video)".into(),
                    music_video,
                    ..Default::default()
                };
                assert!(needs_spaced_query(Some(&identity), query, &spaced));
            }
        }
    }

    #[test]
    fn spaced_query_preserves_strong_titles_and_non_song_identities() {
        let query = "Blink182";
        let spaced = space_title_number(query);
        for kind in ["Video", "Song"] {
            for title in ["  BLINK182  ", "Blink 182", "Blink182 (Official Audio)"] {
                let identity = TopIdentity {
                    kind: kind.into(), title: title.into(), music_video: true,
                    ..Default::default()
                };
                assert!(!needs_spaced_query(Some(&identity), query, &spaced));
            }
        }
        for kind in ["Artist", "Album", "Playlist"] {
            let identity = TopIdentity {
                kind: kind.into(), title: "Blink-182".into(), ..Default::default()
            };
            assert!(!needs_spaced_query(Some(&identity), query, &spaced));
        }
    }

    #[test]
    #[ignore = "Requires live YouTube Music access; run manually with --ignored --nocapture"]
    fn live_music_video_top_populates_songs() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(90), async {
                let mut client = SearchClient::new().await.expect("unauthenticated client");
                let mut verified = Vec::new();
                let queries = std::env::var("AURICLE_TEST_AUDIO_QUERY")
                    .map(|query| vec![query])
                    .unwrap_or_else(|_| vec!["Time in a Bottle Jim Croce".into(), "Mera Yaar B Praak".into(), "Chiggy Wiggy".into()]);
                for query in &queries {
                    let query = query.as_str();
                    let raw = serde_json::to_value(client.api.json_query(SearchQuery::new(query)).await.expect("raw search")).unwrap();
                    let card = raw.pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
                        .and_then(serde_json::Value::as_array)
                        .and_then(|sections| sections.iter().find_map(|section| section.get("musicCardShelfRenderer")));
                    let video_id = card.and_then(|card| card_endpoint(card, "Video")).unwrap_or_default().to_string();
                    let (results, identity) = parse_unfiltered_results(query, raw.clone()).expect("compatible raw parser");
                    eprintln!("query={query:?} raw_video_id={video_id:?} provider_type={:?} provider_title={:?} parsed_artist={:?} identity_artist={:?} returned_songs={} known_music_video={}",
                        results.top_results.first().map(|top| &top.result_type), results.top_results.first().map(|top| &top.result_name),
                        results.top_results.first().and_then(|top| top.artist.as_ref()), identity.as_ref().map(|identity| &identity.artist),
                        results.songs.len(), card.is_some_and(is_music_video_card));
                    let candidates = unfiltered_page(results);
                    let fallback = needs_music_video_songs(identity.as_ref(), &candidates);
                    let page = client.search(query, "All").await.expect("production All search");
                    for song in page.songs.iter().take(6) {
                        eprintln!("query={query:?} song id={} title={:?} artist={:?}", song.id, song.title, song.artist);
                    }
                    if fallback {
                        assert!(!page.songs.is_empty(), "known music video must populate the missing Songs shelf");
                        let Some(CachedValue::Page(filtered)) = client.cache.get(&search_key(query, "songs").unwrap(), Instant::now()) else {
                            panic!("missing original-query SongsFilter cache entry");
                        };
                        assert_eq!(page.songs.iter().map(|song| &song.id).collect::<Vec<_>>(), filtered.songs.iter().map(|song| &song.id).collect::<Vec<_>>());
                    }
                    if let Some(top) = page.top.as_ref() {
                        assert_eq!(top.kind, "Song");
                        assert_eq!(page.intent, "Song");
                        assert!(page.songs.iter().any(|song| song.id == top.id));
                        eprintln!("query={query:?} verified Song top={} title={:?} artist={:?} songs={} different_video_id={}",
                            top.id, top.title, top.artist, page.songs.len(), top.id != video_id);
                        verified.push(query);
                    } else {
                        eprintln!("query={query:?} ordinary songs={}; insufficient metadata or ambiguous title; no Song top claimed", page.songs.len());
                    }
                    if normalize(query) == "mera yaar b praak" {
                        let top = page.top.as_ref().expect("Mera Yaar B Praak must resolve the original Song");
                        assert_eq!(song_title(&top.title), "mera yaar");
                        assert!(has_artist_credit(&top.artist, "B Praak"));
                    }
                    assert!(client.cache.entries.iter().all(|cached| matches!(&cached.key,
                        CacheKey::Search { category, query } if (category == "all" || category == "songs") && queries.iter().any(|original| normalize(original) == *query))),
                        "All must only request original-query Songs, never fan out into categories");
                }
                assert!(!verified.is_empty(), "no live Song top verified; inspect provider metadata before widening matching");
            }).await.expect("live audio card check timed out");
        });
    }

    #[test]
    #[ignore = "Requires live YouTube Music access; run manually with --ignored --nocapture"]
    fn live_filtered_playlists_and_completions() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(60), async {
                let mut client = SearchClient::new().await.expect("unauthenticated client");
                let query = "Arijit Singh";
                let page = client.search(query, "Playlists").await.expect("filtered playlists parser");
                assert!(!page.playlists.is_empty());
                assert!(page.top.is_none() && page.songs.is_empty() && page.artists.is_empty() && page.albums.is_empty());
                let mut ids = HashSet::new();
                assert!(page.playlists.iter().all(|entry| entry.kind == "Playlist" && !entry.id.is_empty() && ids.insert(&entry.id)));
                for entry in page.playlists.iter().take(5) {
                    eprintln!("query={query:?} playlist id={} title={:?} subtitle={:?}", entry.id, entry.title, entry.subtitle);
                }
                assert!(matches!(client.cache.get(&search_key(query, "Playlists").unwrap(), Instant::now()), Some(CachedValue::Page(_))));
                assert!(client.cache.get(&search_key(query, "All").unwrap(), Instant::now()).is_none());
                let completions = client.completions("arijit").await.expect("live completions parser");
                assert!(!completions.is_empty());
                let mut seen = HashSet::new();
                assert!(completions.iter().all(|text| !text.is_empty() && text.trim() == text && seen.insert(normalize(text))));
                assert_eq!(client.completions("arijit").await.unwrap(), completions);
                eprintln!("query=arijit completions={completions:?}; playlists={} category cache uses original query", page.playlists.len());
            }).await.expect("live playlists/completions timed out");
        });
    }

    #[test]
    #[ignore = "Requires live YouTube Music access; run manually with --ignored --nocapture"]
    fn live_arijit_preview_and_catalogue() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(90), async {
                let query = "Arijit Singh";
                let mut client = SearchClient::new().await.expect("unauthenticated client");
                let base = client.search(query, "All").await.expect("Arijit All");
                let top = base.top.as_ref().expect("Arijit artist identity");
                assert_eq!(top.kind, "Artist");
                assert!(!base.songs.is_empty() && !base.albums.is_empty(), "compact card previews must survive parsing");
                let preview = client.preview(query).await.expect("Arijit mixed preview");
                assert!(!preview.playlists.is_empty(), "mixed preview includes public playlists");
                assert_eq!(preview.top.as_ref().unwrap().id, top.id);
                assert_eq!(preview.intent, base.intent);
                assert_eq!(preview.songs.iter().map(|entry| &entry.id).collect::<Vec<_>>(), base.songs.iter().map(|entry| &entry.id).collect::<Vec<_>>());
                assert_eq!(preview.albums.iter().map(|entry| &entry.id).collect::<Vec<_>>(), base.albums.iter().map(|entry| &entry.id).collect::<Vec<_>>());
                let artist = client.api.query(GetArtistQuery::new(ArtistChannelID::from_raw(top.id.as_str())))
                    .await.expect("provider artist catalogue");
                let expected = artist_catalogue(artist);
                let enriched = client.enrich(&base).await.expect("artist catalogue and public playlists");
                assert_eq!(enriched.top.as_ref().unwrap().id, top.id);
                assert_eq!(enriched.songs.len(), expected.songs.len());
                assert_eq!(enriched.albums.len(), expected.albums.len());
                assert_eq!(enriched.artists.len(), expected.artists.len());
                assert!(!enriched.songs.is_empty() && !enriched.albums.is_empty());
                assert_eq!(enriched.playlists.iter().map(|entry| &entry.id).collect::<Vec<_>>(), preview.playlists.iter().map(|entry| &entry.id).collect::<Vec<_>>());
                assert!(entries(&enriched).all(|entry| !entry.id.trim().is_empty()));
                let stamps: Vec<_> = client.cache.entries.iter().map(|entry| entry.inserted).collect();
                let repeated = client.preview(query).await.unwrap();
                let cached = client.enrich(&base).await.unwrap();
                let with_playlists = client.enrich(&preview).await.unwrap();
                assert_eq!(cached.playlists.len(), enriched.playlists.len());
                assert_eq!(with_playlists.playlists.len(), preview.playlists.len());
                assert_eq!(repeated.playlists.len(), preview.playlists.len());
                assert_eq!(stamps.len(), client.cache.entries.len());
                assert!(client.cache.entries.iter().all(|entry| stamps.contains(&entry.inserted)));
                assert!(client.cache.entries.iter().all(|entry| match &entry.key {
                    CacheKey::Search { category, query: cached_query } =>
                        matches!(category.as_str(), "all" | "playlists") && *cached_query == normalize(query),
                    CacheKey::Enrich { .. } => true,
                    _ => false,
                }), "no upfront category fanout");
                eprintln!("Arijit base songs={} albums={} playlists={}; preview playlists={}; enrich songs={} albums={} playlists={} optional_related={}",
                    base.songs.len(), base.albums.len(), base.playlists.len(), preview.playlists.len(),
                    enriched.songs.len(), enriched.albums.len(), enriched.playlists.len(), enriched.artists.len());
            }).await.expect("live Arijit check timed out");
        });
    }

    #[test]
    #[ignore = "Requires live YouTube Music access; run manually with --ignored --nocapture"]
    fn live_metadata_artist_album_and_song() {
        fn diagnose_cards(query: &str, value: &serde_json::Value) {
            if let Some(card) = value.get("musicCardShelfRenderer") {
                let texts = |runs: &serde_json::Value| -> Vec<String> {
                    runs.as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|run| run.get("text").and_then(|text| text.as_str()))
                        .map(str::to_string)
                        .collect()
                };
                eprintln!(
                    "query={query:?} card title={:?} subtitle={:?} browse_id={} video_id={}",
                    texts(&card["title"]["runs"]),
                    texts(&card["subtitle"]["runs"]),
                    card.pointer("/title/runs/0/navigationEndpoint/browseEndpoint/browseId")
                        .unwrap_or(&serde_json::Value::Null),
                    card.pointer("/onTap/watchEndpoint/videoId")
                        .unwrap_or(&serde_json::Value::Null)
                );
                for child in card["contents"].as_array().into_iter().flatten() {
                    let columns = &child["musicResponsiveListItemRenderer"]["flexColumns"];
                    let columns: Vec<_> = columns
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|column| {
                            texts(
                                &column["musicResponsiveListItemFlexColumnRenderer"]["text"]
                                    ["runs"],
                            )
                        })
                        .collect();
                    eprintln!("query={query:?} card_child_columns={columns:?}");
                }
            }
            match value {
                serde_json::Value::Object(object) => {
                    for child in object.values() {
                        diagnose_cards(query, child);
                    }
                }
                serde_json::Value::Array(array) => {
                    for child in array {
                        diagnose_cards(query, child);
                    }
                }
                _ => {}
            }
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(90), async {
                    let mut client = SearchClient::new().await.expect("unauthenticated client");
                    let mut failed_queries = Vec::new();
                    for (query, expected_kind) in [
                        ("Arijit Singh", "Artist"),
                        ("Aashiqui 2", "Album"),
                        ("Aashiqui2", "Album"),
                        ("Tum Hi Ho", "Song"),
                        ("Blinding Lights", "Song"),
                    ] {
                        let search_query = SearchQuery::new(query);
                        let json = client
                            .api
                            .json_query(SearchQuery::new(query))
                            .await
                            .expect("diagnostic unfiltered search");
                        let raw = serde_json::to_value(&json).unwrap();
                        diagnose_cards(query, &raw);
                        let parsed = ytmapi_rs::parse::ProcessedResult {
                            query: &search_query,
                            source: String::new(),
                            json,
                        }.parse_into::<SearchResults>();
                        if parsed.is_err() {
                            eprintln!("query={query:?} upstream_unfiltered_parse_failed=true");
                        }
                        let (results, identity) = parse_unfiltered_results(query, raw)
                            .expect("compatible unfiltered parser");
                        for top in &results.top_results {
                            eprintln!("query={query:?} provider_top type={:?} title={:?} artist={:?} album={:?} year={:?} byline={:?} endpoint={:?}",
                                top.result_type, top.result_name, top.artist, top.album, top.year, top.byline,
                                identity.as_ref().map(|identity| identity.id.as_str()));
                        }
                        let candidates = unfiltered_page(results);
                        for entry in candidates.artists.iter()
                            .chain(&candidates.albums)
                            .chain(candidates.songs.iter().take(8))
                        {
                            eprintln!(
                                "query={query:?} candidate kind={} id={} title={:?} artist={:?} album={:?} year={:?}",
                                entry.kind, entry.id, entry.title, entry.artist, entry.album, entry.year
                            );
                        }
                        let category = match expected_kind {
                            "Artist" => "artists",
                            "Album" => "albums",
                            _ => "songs",
                        };
                        let filtered = client.filtered(query, category).await.expect("typed category parser");
                        for entry in entries(&filtered).take(10) {
                            eprintln!("query={query:?} filtered={category} kind={} id={} title={:?} artist={:?} album={:?} year={:?}",
                                entry.kind, entry.id, entry.title, entry.artist, entry.album, entry.year);
                        }
                        let page = client
                            .search(query, "All")
                            .await
                            .expect("unfiltered search");
                        let Some(CachedValue::Page(cached)) = client.cache.get(
                            &search_key(query, category).unwrap(), Instant::now(),
                        ) else { panic!("query={query:?}: original category cache missing"); };
                        assert_eq!(
                            entries(&cached).map(|entry| (&entry.kind, &entry.id)).collect::<Vec<_>>(),
                            entries(&filtered).map(|entry| (&entry.kind, &entry.id)).collect::<Vec<_>>(),
                            "query={query:?}: All resolution must not overwrite original category results",
                        );
                        let Some(top) = page.top.as_ref() else {
                            if expected_kind == "Song" && identity.as_ref().map(|identity| identity.kind.as_str()) != Some("Song") {
                                eprintln!("query={query:?}: provider has no resolved song top; ambiguous candidates remain unselected");
                                continue;
                            }
                            eprintln!("query={query:?}: no confidence-resolved top result");
                            failed_queries.push(query);
                            continue;
                        };
                        assert_eq!(top.kind, expected_kind, "query: {query}");
                        assert!(!top.id.is_empty());
                        if let Some(identity) = identity.as_ref().filter(|identity| !identity.id.is_empty()) {
                            if identity.kind == "Video" && expected_kind == "Song" {
                                assert!(page.songs.iter().any(|song| song.kind == "Song" && song.id == top.id));
                                assert!(filtered.songs.iter().any(|song| song.id == top.id), "query: {query}; selected typed audio");
                                let matched = resolve_music_video(identity, &filtered, query).expect("strong typed Song match");
                                assert_eq!(top.id, matched.id, "query: {query}; matched audio, not provider Video endpoint");
                            } else if matches!(top.kind.as_str(), "Artist" | "Album")
                                && needs_spaced_query(Some(identity), query, &space_title_number(query))
                            {
                                let spaced_query = space_title_number(query);
                                assert_eq!(normalize(&top.title), normalize(&spaced_query));
                                let json = client.api.json_query(SearchQuery::new(&spaced_query))
                                    .await.expect("spaced provider search");
                                let (_, spaced_identity) = parse_unfiltered_results(
                                    &spaced_query, serde_json::to_value(json).unwrap(),
                                ).expect("spaced provider identity");
                                let spaced_identity = spaced_identity.expect("exact spaced provider top");
                                assert_eq!(spaced_identity.kind, expected_kind);
                                assert_eq!(normalize(&spaced_identity.title), normalize(&spaced_query));
                                assert_eq!(top.id, spaced_identity.id, "query: {query}; selected exact spaced provider endpoint");
                            } else {
                                assert_eq!(top.id, identity.id, "query: {query}; selected provider endpoint");
                            }
                        }
                        let enriched = client.enrich(&page).await.expect("real catalogue metadata");
                        assert_eq!(enriched.top.as_ref().unwrap().id, top.id);
                        assert!(!enriched.songs.is_empty(), "query: {query}");
                        assert!(enriched.songs.iter().all(|entry| !entry.id.is_empty()));
                        if expected_kind == "Album" {
                            assert!(!enriched.artists.is_empty());
                            assert!(enriched.songs.iter().all(|entry| entry.album_id == top.id));
                        }
                        eprintln!(
                            "{query}: {} ({}) songs={} artists={} albums={} playlists={}",
                            top.title,
                            top.id,
                            enriched.songs.len(),
                            enriched.artists.len(),
                            enriched.albums.len(),
                            enriched.playlists.len()
                        );
                    }
                    assert!(!client
                        .completions("arijit")
                        .await
                        .expect("completions")
                        .is_empty());
                    assert!(failed_queries.is_empty(), "failed queries: {failed_queries:?}");
                })
                .await
                .expect("live metadata check timed out");
            });
    }
}
