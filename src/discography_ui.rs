use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::sync::mpsc;
use ytmapi_rs::auth::{noauth::NoAuthToken, AuthToken};
use ytmapi_rs::common::{AlbumID, ArtistChannelID, BrowseParams, PlaylistID, YoutubeID};
use ytmapi_rs::parse::{ParseFrom, PlaylistItem, ProcessedResult};
use ytmapi_rs::query::{GetArtistAlbumsQuery, GetArtistQuery, PostMethod, PostQuery, Query};
use ytmapi_rs::YtMusic;

use crate::{context_ui, core, AlbumItem, NativeShellWindow};

const CACHE_CAPACITY: usize = 8;
const CACHE_TTL: Duration = Duration::from_secs(120);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const ALBUM_STATS_LIMIT: usize = 2000;
const ALBUM_FETCH_DELAY: Duration = Duration::from_millis(150);
const ALBUM_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const SONGS_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const SECTIONS: &str = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents";

static CATALOGUES: OnceLock<Mutex<CatalogueCache>> = OnceLock::new();
static ALBUM_STATS: OnceLock<Mutex<HashMap<String, (u64, usize)>>> = OnceLock::new();
static ROW_GENERATION: AtomicU64 = AtomicU64::new(0);
static CUR_ARTIST: OnceLock<Arc<Mutex<core::library::SavedArtist>>> = OnceLock::new();

fn catalogues() -> &'static Mutex<CatalogueCache> {
    CATALOGUES.get_or_init(|| Mutex::new(CatalogueCache::default()))
}

fn album_stats() -> &'static Mutex<HashMap<String, (u64, usize)>> {
    ALBUM_STATS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn parse_plays(text: &str) -> u64 {
    let Some(token) = text.split_whitespace().next() else {
        return 0;
    };
    let cleaned = token.replace(',', "");
    let (number, multiplier) = match cleaned.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&cleaned[..cleaned.len() - 1], 1e3_f64),
        Some(b'm' | b'M') => (&cleaned[..cleaned.len() - 1], 1e6_f64),
        Some(b'b' | b'B') => (&cleaned[..cleaned.len() - 1], 1e9_f64),
        _ => (cleaned.as_str(), 1.0_f64),
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return 0;
    }
    let Ok(value) = number.parse::<f64>() else {
        return 0;
    };
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    (value * multiplier).round() as u64
}

pub(crate) fn order_by_popularity<T>(
    releases: Vec<(String, T)>,
    plays_by_release: &std::collections::HashMap<String, u64>,
) -> Vec<T> {
    let mut seen = HashSet::new();
    let mut deduped: Vec<(u64, T)> = releases
        .into_iter()
        .filter_map(|(id, item)| {
            seen.insert(id.clone())
                .then(|| (plays_by_release.get(&id).copied().unwrap_or(0), item))
        })
        .collect();
    deduped.sort_by(|(a, _), (b, _)| b.cmp(a));
    deduped.into_iter().map(|(_, item)| item).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    Albums,
    Singles,
}

impl Category {
    fn from_section(title: &str) -> Option<Self> {
        match title.to_ascii_lowercase().as_str() {
            "albums" => Some(Self::Albums),
            "singles" | "singles & eps" | "singles and eps" => Some(Self::Singles),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Endpoint {
    browse_id: String,
    params: String,
}

impl Endpoint {
    fn query(&self) -> GetArtistAlbumsQuery<'_> {
        GetArtistAlbumsQuery::new(
            ArtistChannelID::from_raw(&self.browse_id),
            BrowseParams::from_raw(&self.params),
        )
    }
}

fn select_endpoint(
    browse_id: Option<&str>,
    params: Option<&str>,
) -> Result<Option<Endpoint>, String> {
    match (browse_id, params) {
        (None, None) => Ok(None),
        (Some(browse_id), Some(params)) if !browse_id.is_empty() && !params.is_empty() => {
            Ok(Some(Endpoint {
                browse_id: browse_id.into(),
                params: params.into(),
            }))
        }
        _ => Err("The artist catalogue endpoint is incomplete. Please retry.".into()),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Release {
    id: String,
    title: String,
    year: String,
    thumbnail_url: String,
    albums: bool,
    singles: bool,
}

fn text(value: &Value) -> String {
    value
        .get("simpleText")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            value["runs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|run| run["text"].as_str())
                .collect()
        })
}

fn map_release(row: &Value, category: Category) -> Result<Release, String> {
    let id = row
        .pointer("/title/runs/0/navigationEndpoint/browseEndpoint/browseId")
        .or_else(|| row.pointer("/navigationEndpoint/browseEndpoint/browseId"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or("An artist release has no browse ID. Please retry.")?;
    let title = text(&row["title"]);
    if title.trim().is_empty() {
        return Err("An artist release has no title. Please retry.".into());
    }
    let thumbnails: Vec<crate::Thumbnail> = row
        .pointer("/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let year = ["subtitle", "subtitle2"]
        .into_iter()
        .flat_map(|key| row[key]["runs"].as_array().into_iter().flatten())
        .filter_map(|run| run["text"].as_str())
        .find(|part| part.len() == 4 && part.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or_default()
        .to_string();
    Ok(Release {
        id: id.into(),
        title,
        year,
        thumbnail_url: crate::pick_thumb(&thumbnails)
            .map(|thumb| thumb.url.clone())
            .unwrap_or_default(),
        albums: category == Category::Albums,
        singles: category == Category::Singles,
    })
}

fn deduplicate(releases: impl IntoIterator<Item = Release>) -> Vec<Release> {
    let mut positions = HashMap::<String, usize>::new();
    let mut unique = Vec::<Release>::new();
    for release in releases {
        if release.id.trim().is_empty() {
            continue;
        }
        if let Some(&position) = positions.get(&release.id) {
            unique[position].albums |= release.albums;
            unique[position].singles |= release.singles;
        } else {
            positions.insert(release.id.clone(), unique.len());
            unique.push(release);
        }
    }
    unique
}

#[derive(Debug)]
struct Section {
    category: Category,
    endpoint: Option<Endpoint>,
    preview: Vec<Release>,
}

fn artist_sections(json: &Value) -> Result<(String, Vec<Section>), String> {
    let sections = json
        .pointer(SECTIONS)
        .and_then(Value::as_array)
        .ok_or("The artist catalogue response was not recognized. Please retry.")?;
    let name = json["header"]
        .as_object()
        .into_iter()
        .flat_map(|header| header.values())
        .map(|header| text(&header["title"]))
        .find(|name| !name.is_empty())
        .ok_or("The artist name is missing. Please retry.")?;
    let mut selected = Vec::new();
    for shelf in sections
        .iter()
        .filter_map(|section| section.get("musicCarouselShelfRenderer"))
    {
        let header = &shelf["header"]["musicCarouselShelfBasicHeaderRenderer"];
        let Some(category) = Category::from_section(&text(&header["title"])) else {
            continue;
        };
        let endpoint = header
            .pointer("/title/runs/0/navigationEndpoint/browseEndpoint")
            .or_else(|| {
                header
                    .pointer("/moreContentButton/buttonRenderer/navigationEndpoint/browseEndpoint")
            });
        let endpoint = select_endpoint(
            endpoint.and_then(|endpoint| endpoint["browseId"].as_str()),
            endpoint.and_then(|endpoint| endpoint["params"].as_str()),
        )?;
        let preview = shelf["contents"]
            .as_array()
            .ok_or("The artist release shelf is missing.")?
            .iter()
            .filter_map(|item| item.get("musicTwoRowItemRenderer"))
            .map(|row| map_release(row, category))
            .collect::<Result<Vec<_>, _>>()?;
        selected.push(Section {
            category,
            endpoint,
            preview,
        });
    }
    Ok((name, selected))
}

#[derive(Clone, Debug, Default)]
struct Catalogue {
    artist_id: String,
    artist_name: String,
    releases: Vec<Release>,
}

struct ContinuationQuery<'a> {
    endpoint: &'a Endpoint,
    token: &'a str,
}

#[derive(Debug)]
struct RawPage;

impl ParseFrom<ContinuationQuery<'_>> for RawPage {
    fn parse_from(_: ProcessedResult<ContinuationQuery<'_>>) -> ytmapi_rs::Result<Self> {
        Ok(Self)
    }
}

impl<Auth: AuthToken> Query<Auth> for ContinuationQuery<'_> {
    type Output = RawPage;
    type Method = PostMethod;
}

impl PostQuery for ContinuationQuery<'_> {
    fn header(&self) -> serde_json::Map<String, Value> {
        self.endpoint.query().header()
    }

    fn path(&self) -> &str {
        "browse"
    }

    fn params(&self) -> Vec<(&str, std::borrow::Cow<'_, str>)> {
        vec![
            ("ctoken", self.token.into()),
            ("continuation", self.token.into()),
        ]
    }
}

fn page_releases(
    json: &Value,
    category: Category,
) -> Result<(Vec<Release>, Option<String>), String> {
    let grid = json
        .pointer(SECTIONS)
        .and_then(Value::as_array)
        .and_then(|sections| {
            sections
                .iter()
                .find_map(|section| section.get("gridRenderer"))
        })
        .or_else(|| json.pointer("/continuationContents/gridContinuation"));
    let appended = ["onResponseReceivedActions", "onResponseReceivedEndpoints"]
        .into_iter()
        .flat_map(|key| json[key].as_array().into_iter().flatten())
        .find_map(|action| action.get("appendContinuationItemsAction"));
    let container = grid
        .or(appended)
        .ok_or("The catalogue page format changed. Please retry.")?;
    let items = container
        .get("items")
        .or_else(|| container.get("continuationItems"))
        .and_then(Value::as_array)
        .ok_or("The catalogue page has no release list. Please retry.")?;
    let mut tokens = HashSet::new();
    if let Some(continuations) = container.get("continuations") {
        for continuation in continuations
            .as_array()
            .ok_or("Unrecognized catalogue pagination.")?
        {
            let token = continuation
                .pointer("/nextContinuationData/continuation")
                .or_else(|| continuation.pointer("/reloadContinuationData/continuation"))
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .ok_or("The catalogue continuation is unsupported. Please retry.")?;
            tokens.insert(token.to_string());
        }
    }
    let mut releases = Vec::new();
    for item in items {
        if let Some(row) = item.get("musicTwoRowItemRenderer") {
            releases.push(map_release(row, category)?);
        } else if let Some(continuation) = item.get("continuationItemRenderer") {
            let token = continuation
                .pointer("/continuationEndpoint/continuationCommand/token")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
                .ok_or("The catalogue continuation is unsupported. Please retry.")?;
            tokens.insert(token.to_string());
        } else {
            return Err("The catalogue contains an unsupported release. Please retry.".into());
        }
    }
    if tokens.len() > 1 {
        return Err("The catalogue has ambiguous continuation pages. Please retry.".into());
    }
    Ok((releases, tokens.into_iter().next()))
}

async fn query_json<QueryType: Query<NoAuthToken>>(
    api: &YtMusic<NoAuthToken>,
    query: QueryType,
) -> Result<Value, String> {
    let json = api
        .json_query(query)
        .await
        .map_err(|error| error.to_string())?;
    ytmapi_rs::json::from_json(json).map_err(|error| error.to_string())
}

async fn fetch_section(
    api: &YtMusic<NoAuthToken>,
    section: Section,
) -> Result<Vec<Release>, String> {
    let Some(endpoint) = section.endpoint else {
        return Ok(section.preview);
    };
    let mut json = query_json(api, endpoint.query()).await?;
    let mut releases = Vec::new();
    let mut seen_tokens = HashSet::new();
    loop {
        let (page, next) = page_releases(&json, section.category)?;
        releases.extend(page);
        let Some(token) = next else {
            return Ok(releases);
        };
        if !seen_tokens.insert(token.clone()) || seen_tokens.len() > 100 {
            return Err(
                "The provider did not finish the catalogue pagination. Please retry.".into(),
            );
        }
        json = query_json(
            api,
            ContinuationQuery {
                endpoint: &endpoint,
                token: &token,
            },
        )
        .await?;
    }
}

async fn fetch_catalogue(api: &YtMusic<NoAuthToken>, artist_id: &str) -> Result<Catalogue, String> {
    let json = query_json(
        api,
        GetArtistQuery::new(ArtistChannelID::from_raw(artist_id)),
    )
    .await?;
    let (artist_name, sections) = artist_sections(&json)?;
    let mut releases = Vec::new();
    for section in sections {
        releases.extend(fetch_section(api, section).await?);
    }
    Ok(Catalogue {
        artist_id: artist_id.into(),
        artist_name,
        releases: deduplicate(releases),
    })
}

#[derive(Default)]
struct CatalogueCache(VecDeque<(Instant, Arc<Catalogue>)>);

impl CatalogueCache {
    fn get(&mut self, artist_id: &str, now: Instant) -> Option<Arc<Catalogue>> {
        self.0
            .retain(|(inserted, _)| now.saturating_duration_since(*inserted) < CACHE_TTL);
        let position = self
            .0
            .iter()
            .position(|(_, catalogue)| catalogue.artist_id == artist_id)?;
        let entry = self.0.remove(position)?;
        let catalogue = entry.1.clone();
        self.0.push_back(entry);
        Some(catalogue)
    }

    fn insert(&mut self, catalogue: Arc<Catalogue>, now: Instant) {
        self.0
            .retain(|(_, cached)| cached.artist_id != catalogue.artist_id);
        self.0.push_back((now, catalogue));
        while self.0.len() > CACHE_CAPACITY {
            self.0.pop_front();
        }
    }
}

#[derive(Clone, Debug)]
struct Request {
    generation: u64,
    artist_id: String,
}

impl Request {
    fn matches(&self, generation: u64, artist_id: &str, view: &str) -> bool {
        self.generation == generation && self.artist_id == artist_id && view == "Discography"
    }
}

#[derive(Default)]
struct State {
    generation: AtomicU64,
    catalogue: Mutex<Option<Arc<Catalogue>>>,
    thumbnails: Mutex<ThumbnailRequests>,
}

impl State {
    fn matches(&self, request: &Request, ui: &NativeShellWindow) -> bool {
        request.matches(
            self.generation.load(Ordering::SeqCst),
            &ui.get_discography_artist_id(),
            &ui.get_current_view(),
        )
    }
}

#[derive(Clone, Debug)]
struct ThumbnailRequest {
    view: Request,
    id: String,
    row_index: usize,
    release_index: usize,
}

#[derive(Default)]
struct ThumbnailRequests {
    generation: u64,
    artist_id: String,
    rows: HashMap<String, (usize, usize)>,
    requested: HashSet<String>,
}

fn to_candidate(release: &Release) -> core::release_rank::Candidate {
    core::release_rank::Candidate {
        id: release.id.clone(),
        title: release.title.clone(),
        year: release.year.clone(),
        from_albums: release.albums,
        from_singles: release.singles,
    }
}

fn visible_indices(
    catalogue: &Catalogue,
    filter: &str,
    stats: &HashMap<String, (u64, usize)>,
) -> Vec<usize> {
    use core::release_rank::{classify, dedupe, ReleaseKind};
    let mut first_index = HashMap::<&str, usize>::with_capacity(catalogue.releases.len());
    for (idx, release) in catalogue.releases.iter().enumerate() {
        first_index.entry(release.id.as_str()).or_insert(idx);
    }
    let candidates = catalogue.releases.iter().map(to_candidate).collect();
    dedupe(candidates)
        .into_iter()
        .filter(|candidate| {
            let track_count = stats.get(&candidate.id).map(|s| s.1);
            let kind = classify(candidate, &catalogue.artist_name, track_count);
            match filter {
                "All" => true,
                "Albums" => kind == ReleaseKind::Album,
                "Singles" => kind == ReleaseKind::Single,
                "Compilations" => kind == ReleaseKind::Compilation,
                _ => false,
            }
        })
        .filter_map(|candidate| first_index.get(candidate.id.as_str()).copied())
        .collect()
}

impl ThumbnailRequests {
    fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.artist_id.clear();
        self.rows.clear();
        self.requested.clear();
    }

    fn reset(&mut self, catalogue: &Catalogue, indices: &[usize]) {
        self.clear();
        self.artist_id.clone_from(&catalogue.artist_id);
        self.rows.extend(
            indices
                .iter()
                .copied()
                .enumerate()
                .map(|(row_index, release_index)| {
                    (
                        catalogue.releases[release_index].id.clone(),
                        (row_index, release_index),
                    )
                }),
        );
    }

    fn claim(&mut self, id: &str, artist_id: &str, view: &str) -> Option<ThumbnailRequest> {
        if view != "Discography" || self.artist_id != artist_id {
            return None;
        }
        let &(row_index, release_index) = self.rows.get(id)?;
        if !self.requested.insert(id.to_string()) {
            return None;
        }
        Some(ThumbnailRequest {
            view: Request {
                generation: self.generation,
                artist_id: artist_id.into(),
            },
            id: id.into(),
            row_index,
            release_index,
        })
    }

    fn matches(&self, request: &ThumbnailRequest, artist_id: &str, view: &str) -> bool {
        request.view.matches(self.generation, artist_id, view)
            && self.artist_id == artist_id
            && self.requested.contains(&request.id)
            && self.rows.get(&request.id) == Some(&(request.row_index, request.release_index))
    }

    fn is_current(&self, request: &ThumbnailRequest) -> bool {
        self.matches(request, &self.artist_id, "Discography")
    }
}

fn album_item(release: &Release, artist_name: &str) -> AlbumItem {
    let (thumbnail, has_thumbnail) =
        crate::load_cached_thumb(&crate::library_thumb_path("album", &release.id));
    AlbumItem {
        browse_id: release.id.as_str().into(),
        title: release.title.as_str().into(),
        artist: artist_name.into(),
        year: release.year.as_str().into(),
        thumbnail,
        has_thumbnail,
    }
}

fn render(ui: &NativeShellWindow, state: &State, catalogue: &Catalogue) {
    let filter = ui.get_discography_filter();
    let indices = {
        let stats = album_stats().lock().unwrap_or_else(|e| e.into_inner());
        visible_indices(catalogue, &filter, &stats)
    };
    let items = indices
        .iter()
        .map(|&release_index| {
            let release = &catalogue.releases[release_index];
            context_ui::register_entity(
                "Album",
                &release.id,
                &release.title,
                &catalogue.artist_name,
                &release.thumbnail_url,
            );
            album_item(release, &catalogue.artist_name)
        })
        .collect::<Vec<_>>();
    state.thumbnails.lock().unwrap().reset(catalogue, &indices);
    ui.set_discography_items(ModelRc::new(VecModel::from(items)));
}

fn request_thumbnail(ui: &NativeShellWindow, state: &Arc<State>, id: &str) {
    let Some(catalogue) = state.catalogue.lock().unwrap().clone() else {
        return;
    };
    if catalogue.artist_id != ui.get_discography_artist_id().as_str() {
        return;
    }
    let Some(request) = state.thumbnails.lock().unwrap().claim(
        id,
        &ui.get_discography_artist_id(),
        &ui.get_current_view(),
    ) else {
        return;
    };
    let Some(release) = catalogue
        .releases
        .get(request.release_index)
        .filter(|release| release.id == request.id && !release.thumbnail_url.is_empty())
    else {
        return;
    };
    let Some(item) = ui.get_discography_items().row_data(request.row_index) else {
        return;
    };
    if item.browse_id.as_str() != request.id || item.has_thumbnail {
        return;
    }
    let url = release.thumbnail_url.clone();
    let weak = ui.as_weak();
    let state = state.clone();
    core::net::spawn(move || {
        if !state.thumbnails.lock().unwrap().is_current(&request) {
            return;
        }
        let path = crate::library_thumb_path("album", &request.id);
        if !path.exists() {
            let Ok(bytes) = core::net::http()
                .get(&url)
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::bytes)
            else {
                return;
            };
            if !state.thumbnails.lock().unwrap().is_current(&request)
                || std::fs::write(&path, bytes).is_err()
            {
                return;
            }
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            if !state.thumbnails.lock().unwrap().matches(
                &request,
                &ui.get_discography_artist_id(),
                &ui.get_current_view(),
            ) {
                return;
            }
            let model = ui.get_discography_items();
            let Some(mut item) = model.row_data(request.row_index) else {
                return;
            };
            if item.browse_id.as_str() != request.id {
                return;
            }
            (item.thumbnail, item.has_thumbnail) = crate::load_cached_thumb(&path);
            model.set_row_data(request.row_index, item);
        });
    });
}

async fn fetch_artist_song_album_ids(
    api: &YtMusic<NoAuthToken>,
    artist_id: &str,
    songs_playlist_id: Option<&str>,
) -> Vec<String> {
    let Some(raw_id) = songs_playlist_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return Vec::new();
    };
    let first = api.get_playlist_tracks(PlaylistID::from_raw(raw_id)).await;
    let items = match first {
        Ok(tracks) if !tracks.is_empty() => tracks,
        first_res => {
            if let Some(stripped) = raw_id.strip_prefix("VL").filter(|s| !s.is_empty()) {
                match api.get_playlist_tracks(PlaylistID::from_raw(stripped)).await {
                    Ok(tracks) => tracks,
                    Err(error) => {
                        log::warn!(
                            "Artist songs playlist fetch failed for {artist_id} ({raw_id}): {error}"
                        );
                        Vec::new()
                    }
                }
            } else {
                if let Err(error) = first_res {
                    log::warn!(
                        "Artist songs playlist fetch failed for {artist_id} ({raw_id}): {error}"
                    );
                }
                Vec::new()
            }
        }
    };
    items
        .into_iter()
        .filter_map(|item| match item {
            PlaylistItem::Song(track) => Some(track.album.id.get_raw().to_string()),
            _ => None,
        })
        .collect()
}

pub(crate) async fn compute_artist_row(
    api: &YtMusic<NoAuthToken>,
    artist_id: &str,
    artist_name: &str,
    songs_playlist_id: Option<&str>,
    limit: usize,
    should_continue: &dyn Fn() -> bool,
) -> Result<(Vec<Release>, String), String> {
    if !should_continue() {
        return Err("cancelled".into());
    }
    // Bind first: an `if let` on the lock would hold the guard into the else branch and deadlock.
    let cached = catalogues()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(artist_id, Instant::now());
    let catalogue = if let Some(cached) = cached {
        cached
    } else {
        let fetched = Arc::new(fetch_catalogue(api, artist_id).await?);
        catalogues()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(fetched.clone(), Instant::now());
        fetched
    };
    if !should_continue() {
        return Err("cancelled".into());
    }
    let resolved_artist_name = if artist_name.trim().is_empty() {
        catalogue.artist_name.as_str()
    } else {
        artist_name
    };

    let song_album_ids = match tokio::time::timeout(
        SONGS_FETCH_TIMEOUT,
        fetch_artist_song_album_ids(api, artist_id, songs_playlist_id),
    )
    .await
    {
        Ok(ids) => ids,
        Err(_) => {
            log::warn!("Artist songs playlist fetch timed out for {artist_id}");
            Vec::new()
        }
    };
    if !should_continue() {
        return Err("cancelled".into());
    }
    let song_rank = core::release_rank::song_ranks(&song_album_ids);
    let deduped = core::release_rank::dedupe(catalogue.releases.iter().map(to_candidate).collect());

    let mut stats_snapshot = album_stats()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let non_compilations: Vec<core::release_rank::Candidate> = deduped
        .iter()
        .filter(|c| {
            let track_count = stats_snapshot.get(&c.id).map(|s| s.1);
            core::release_rank::classify(c, resolved_artist_name, track_count)
                != core::release_rank::ReleaseKind::Compilation
        })
        .cloned()
        .collect();
    let cached_plays: HashMap<String, u64> = stats_snapshot
        .iter()
        .map(|(k, &(plays, _))| (k.clone(), plays))
        .collect();
    let initially_ranked = core::release_rank::rank(&non_compilations, &song_rank, &cached_plays);
    let to_fetch: Vec<String> = initially_ranked
        .into_iter()
        .take(limit)
        .filter(|c| !stats_snapshot.contains_key(&c.id))
        .map(|c| c.id.clone())
        .collect();

    for (idx, album_id) in to_fetch.iter().enumerate() {
        if !should_continue() {
            return Err("cancelled".into());
        }
        if idx > 0 {
            tokio::time::sleep(ALBUM_FETCH_DELAY).await;
            if !should_continue() {
                return Err("cancelled".into());
            }
        }
        match tokio::time::timeout(ALBUM_FETCH_TIMEOUT, api.get_album(AlbumID::from_raw(album_id.as_str())))
            .await
            .map_err(|_| "timed out".to_string())
            .and_then(|result| result.map_err(|error| error.to_string()))
        {
            Ok(album) => {
                let plays_sum = album
                    .tracks
                    .iter()
                    .map(|track| parse_plays(&track.plays))
                    .fold(0u64, u64::saturating_add);
                let track_count = album.tracks.len();
                stats_snapshot.insert(album_id.clone(), (plays_sum, track_count));
                let mut cache = album_stats().lock().unwrap_or_else(|e| e.into_inner());
                if cache.len() >= ALBUM_STATS_LIMIT {
                    cache.clear();
                }
                cache.insert(album_id.clone(), (plays_sum, track_count));
            }
            Err(error) => {
                log::warn!("Album stats fetch failed for {album_id}: {error}");
            }
        }
    }

    if !should_continue() {
        return Err("cancelled".into());
    }
    let final_candidates: Vec<core::release_rank::Candidate> = deduped
        .into_iter()
        .filter(|c| {
            let track_count = stats_snapshot.get(&c.id).map(|s| s.1);
            core::release_rank::classify(c, resolved_artist_name, track_count)
                != core::release_rank::ReleaseKind::Compilation
        })
        .collect();
    let plays: HashMap<String, u64> = stats_snapshot
        .iter()
        .map(|(k, &(p, _))| (k.clone(), p))
        .collect();
    let ranked = core::release_rank::rank(&final_candidates, &song_rank, &plays);

    let mut release_by_id = HashMap::<&str, &Release>::with_capacity(catalogue.releases.len());
    for release in &catalogue.releases {
        release_by_id.entry(release.id.as_str()).or_insert(release);
    }
    let releases = ranked
        .into_iter()
        .take(limit)
        .filter_map(|candidate| {
            let mut release = (*release_by_id.get(candidate.id.as_str())?).clone();
            release.albums = candidate.from_albums;
            release.singles = candidate.from_singles;
            Some(release)
        })
        .collect();
    Ok((releases, resolved_artist_name.to_string()))
}

fn current_artist_matches(artist_id: &str) -> bool {
    CUR_ARTIST
        .get()
        .and_then(|cur| cur.lock().ok())
        .is_some_and(|artist| artist.browse_id == artist_id)
}

/// Invalidates any artist-row ranking still running for a previous page.
pub(crate) fn cancel_artist_row() {
    ROW_GENERATION.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn rank_artist_row(
    ui: &NativeShellWindow,
    artist_id: &str,
    artist_name: &str,
    songs_playlist_id: Option<String>,
) {
    let generation = ROW_GENERATION.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    let weak = ui.as_weak();
    let artist_id = artist_id.to_string();
    let artist_name = artist_name.to_string();
    let _ = std::thread::Builder::new()
        .name("auricle-artist-row".into())
        .spawn(move || {
            core::audio_priority::lower_current_thread();
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    if ROW_GENERATION.load(Ordering::SeqCst) == generation {
                        ui.set_artist_row_sorting(false);
                        ui.set_artist_view_albums(ui.get_artist_row_fallback());
                    }
                });
                return;
            };
            let result = runtime.block_on(async {
                tokio::time::timeout(FETCH_TIMEOUT, async {
                    let api = YtMusic::new_unauthenticated()
                        .await
                        .map_err(|error| error.to_string())?;
                    compute_artist_row(
                        &api,
                        &artist_id,
                        &artist_name,
                        songs_playlist_id.as_deref(),
                        20,
                        &|| ROW_GENERATION.load(Ordering::SeqCst) == generation,
                    )
                    .await
                })
                .await
                .unwrap_or_else(|_| Err("Artist row ranking timed out.".into()))
            });
            match result {
                Ok((releases, resolved_name)) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        if ROW_GENERATION.load(Ordering::SeqCst) != generation
                            || ui.get_current_view() != "Artist"
                            || !current_artist_matches(&artist_id)
                        {
                            return;
                        }
                        let mut missing_thumbs = Vec::new();
                        let items: Vec<AlbumItem> = releases
                            .iter()
                            .map(|release| {
                                context_ui::register_entity(
                                    "Album",
                                    &release.id,
                                    &release.title,
                                    &resolved_name,
                                    &release.thumbnail_url,
                                );
                                let item = album_item(release, &resolved_name);
                                if !item.has_thumbnail && !release.thumbnail_url.is_empty() {
                                    missing_thumbs.push((
                                        release.id.clone(),
                                        release.thumbnail_url.clone(),
                                    ));
                                }
                                item
                            })
                            .collect();
                        ui.set_artist_row_sorting(false);
                        if items.is_empty() {
                            ui.set_artist_view_albums(ui.get_artist_row_fallback());
                            return;
                        }
                        ui.set_artist_view_albums(ModelRc::new(VecModel::from(items)));
                        for (id, url) in missing_thumbs {
                            let weak = ui.as_weak();
                            let artist_id = artist_id.clone();
                            core::net::spawn(move || {
                                if ROW_GENERATION.load(Ordering::SeqCst) != generation {
                                    return;
                                }
                                let path = crate::library_thumb_path("album", &id);
                                if !path.exists() {
                                    let Ok(bytes) = core::net::http()
                                        .get(&url)
                                        .send()
                                        .and_then(reqwest::blocking::Response::error_for_status)
                                        .and_then(reqwest::blocking::Response::bytes)
                                    else {
                                        return;
                                    };
                                    if ROW_GENERATION.load(Ordering::SeqCst) != generation
                                        || std::fs::write(&path, bytes).is_err()
                                    {
                                        return;
                                    }
                                }
                                let _ = slint::invoke_from_event_loop(move || {
                                    let Some(ui) = weak.upgrade() else { return };
                                    if ROW_GENERATION.load(Ordering::SeqCst) != generation
                                        || ui.get_current_view() != "Artist"
                                        || !current_artist_matches(&artist_id)
                                    {
                                        return;
                                    }
                                    let (thumbnail, has_thumbnail) = crate::load_cached_thumb(&path);
                                    if !has_thumbnail {
                                        return;
                                    }
                                    let model = ui.get_artist_view_albums();
                                    for row in 0..model.row_count() {
                                        let Some(mut item) = model.row_data(row) else {
                                            continue;
                                        };
                                        if item.browse_id.as_str() == id {
                                            item.thumbnail = thumbnail;
                                            item.has_thumbnail = true;
                                            model.set_row_data(row, item);
                                            break;
                                        }
                                    }
                                });
                            });
                        }
                    });
                }
                Err(error) if error == "cancelled" => {}
                Err(error) => {
                    log::warn!("Artist row ranking failed for {artist_id}: {error}");
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        if ROW_GENERATION.load(Ordering::SeqCst) != generation
                            || !current_artist_matches(&artist_id)
                        {
                            return;
                        }
                        ui.set_artist_row_sorting(false);
                        ui.set_artist_view_albums(ui.get_artist_row_fallback());
                    });
                }
            }
        });
}

enum Command {
    Load(Request, bool),
    Cancel,
}

fn publish(
    weak: slint::Weak<NativeShellWindow>,
    state: Arc<State>,
    request: Request,
    result: Result<Arc<Catalogue>, String>,
) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        if !state.matches(&request, &ui) {
            return;
        }
        ui.set_discography_busy(false);
        match result {
            Ok(catalogue) => {
                ui.set_discography_title(catalogue.artist_name.as_str().into());
                ui.set_discography_error("".into());
                *state.catalogue.lock().unwrap() = Some(catalogue.clone());
                render(&ui, &state, &catalogue);
            }
            Err(error) => {
                log::warn!("Discography fetch failed: {error}");
                ui.set_discography_error(
                    format!("Could not load the artist catalogue. {error}").into(),
                );
            }
        }
    });
}

fn start_worker(ui: &NativeShellWindow, state: Arc<State>) -> mpsc::UnboundedSender<Command> {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let weak = ui.as_weak();
    let _ = std::thread::Builder::new().name("auricle-discography".into()).spawn(move || {
        core::audio_priority::lower_current_thread();
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        runtime.block_on(async move {
            let mut api = None;
            let mut command = receiver.recv().await;
            while let Some(mut latest) = command {
                while let Ok(newer) = receiver.try_recv() { latest = newer; }
                let Command::Load(request, force) = latest else {
                    command = receiver.recv().await;
                    continue;
                };
                if state.generation.load(Ordering::SeqCst) != request.generation {
                    command = receiver.recv().await;
                    continue;
                }
                let cached = (!force)
                    .then(|| {
                        catalogues()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&request.artist_id, Instant::now())
                    })
                    .flatten();
                if let Some(catalogue) = cached {
                    publish(weak.clone(), state.clone(), request, Ok(catalogue));
                } else {
                    let work = tokio::time::timeout(FETCH_TIMEOUT, async {
                        let client = match &api {
                            Some(client) => client,
                            None => {
                                let created = YtMusic::new_unauthenticated().await.map_err(|error| error.to_string())?;
                                api.insert(created)
                            }
                        };
                        fetch_catalogue(client, &request.artist_id).await.map(Arc::new)
                    });
                    let result = tokio::select! {
                        biased;
                        newer = receiver.recv() => { command = newer; continue; }
                        result = work => result.unwrap_or_else(|_| Err("The request timed out. Please retry.".into())),
                    };
                    if state.generation.load(Ordering::SeqCst) == request.generation {
                        if let Ok(catalogue) = &result {
                            catalogues()
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(catalogue.clone(), Instant::now());
                        }
                        publish(weak.clone(), state.clone(), request, result);
                    }
                }
                command = receiver.recv().await;
            }
        });
    });
    sender
}

fn begin(
    ui: &NativeShellWindow,
    state: &State,
    sender: &mpsc::UnboundedSender<Command>,
    force: bool,
) {
    let request = Request {
        generation: state
            .generation
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1),
        artist_id: ui.get_discography_artist_id().to_string(),
    };
    state.thumbnails.lock().unwrap().clear();
    ui.set_discography_busy(true);
    ui.set_discography_error("".into());
    if sender.send(Command::Load(request, force)).is_err() {
        ui.set_discography_busy(false);
        ui.set_discography_error(
            "The catalogue service is unavailable. Please restart Auricle.".into(),
        );
    }
}

pub(crate) fn wire(
    ui: &NativeShellWindow,
    cur_artist: Arc<Mutex<core::library::SavedArtist>>,
    record_navigation: impl Fn(&NativeShellWindow, &str) + 'static,
) {
    let _ = CUR_ARTIST.set(cur_artist.clone());
    let state = Arc::new(State::default());
    let sender = start_worker(ui, state.clone());
    {
        let weak = ui.as_weak();
        let cur_artist = cur_artist.clone();
        ui.on_open_discography(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_current_view() != "Artist" || ui.get_is_loading() {
                return;
            }
            let artist_id = cur_artist.lock().unwrap().browse_id.clone();
            if !artist_id.trim().is_empty() {
                ui.invoke_navigate_to_discography(artist_id.into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let sender = sender.clone();
        ui.on_navigate_to_discography(move |artist_id| {
            if artist_id.trim().is_empty() {
                return;
            }
            let Some(ui) = weak.upgrade() else { return };
            context_ui::invalidate(&ui);
            record_navigation(&ui, &artist_id);
            let title = cur_artist
                .lock()
                .ok()
                .filter(|artist| artist.browse_id == artist_id.as_str())
                .map(|artist| artist.name.clone())
                .unwrap_or_default();
            ui.set_discography_artist_id(artist_id);
            ui.set_discography_title(title.into());
            ui.set_discography_filter("All".into());
            ui.set_discography_items(ModelRc::new(VecModel::from(Vec::<AlbumItem>::new())));
            *state.catalogue.lock().unwrap() = None;
            ui.set_current_view("Discography".into());
            begin(&ui, &state, &sender, false);
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_discography_category(move |category| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_current_view() != "Discography"
                || ui.get_discography_filter() != category
                || !matches!(
                    category.as_str(),
                    "All" | "Albums" | "Singles" | "Compilations"
                )
            {
                return;
            }
            let catalogue = state.catalogue.lock().unwrap().clone();
            if let Some(catalogue) = catalogue
                .filter(|catalogue| catalogue.artist_id == ui.get_discography_artist_id().as_str())
            {
                render(&ui, &state, &catalogue);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_request_discography_thumbnail(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            request_thumbnail(&ui, &state, &id);
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let sender = sender.clone();
        ui.on_retry_discography(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_current_view() == "Discography" && !ui.get_discography_artist_id().is_empty()
            {
                begin(&ui, &state, &sender, true);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_discography_view_changed(move |view| {
            if view != "Discography" {
                state.generation.fetch_add(1, Ordering::SeqCst);
                state.thumbnails.lock().unwrap().clear();
                let _ = sender.send(Command::Cancel);
                if let Some(ui) = weak.upgrade() {
                    ui.set_discography_busy(false);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use ytmapi_rs::query::PostQuery;

    fn row(id: &str, title: &str) -> Value {
        json!({
            "title": {"runs": [{"text": title, "navigationEndpoint": {"browseEndpoint": {"browseId": id}}}]},
            "subtitle": {"runs": [{"text": "Single"}, {"text": "2026"}]},
            "thumbnailRenderer": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                {"url": "small", "width": 120, "height": 120},
                {"url": "card", "width": 320, "height": 320},
                {"url": "large", "width": 1200, "height": 1200}
            ]}}}
        })
    }

    fn artist_json(contents: Value) -> Value {
        json!({
            "header": {"musicImmersiveHeaderRenderer": {"title": {"runs": [{"text": "Artist name"}]}}},
            "contents": {"singleColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"content": {
                "sectionListRenderer": {"contents": contents}
            }}}]}}
        })
    }

    #[test]
    fn raw_artist_recovers_singles_endpoint_ignored_by_provider_parser() {
        let shelf = |title, id, params, release_id| {
            json!({"musicCarouselShelfRenderer": {
                "header": {"musicCarouselShelfBasicHeaderRenderer": {"title": {"runs": [{
                    "text": title, "navigationEndpoint": {"browseEndpoint": {"browseId": id, "params": params}}
                }]}}},
                "contents": [{"musicTwoRowItemRenderer": row(release_id, "Release")}]
            }})
        };
        let json = artist_json(json!([
            shelf("Albums", "UC-albums", "albums", "album"),
            shelf("Singles & EPs", "UC-singles", "singles", "single")
        ]));
        let (name, sections) = artist_sections(&json).unwrap();
        assert_eq!(name, "Artist name");
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].category, Category::Albums);
        assert_eq!(sections[1].category, Category::Singles);
        assert_eq!(
            sections[1].endpoint.as_ref().unwrap().browse_id,
            "UC-singles"
        );
        assert_eq!(sections[1].preview[0].id, "single");
        assert!(sections[1].preview[0].singles);
    }

    #[test]
    fn pagination_preserves_query_endpoint_and_parses_grid_and_append_pages() {
        let endpoint = select_endpoint(Some("UC-endpoint"), Some("real-params"))
            .unwrap()
            .unwrap();
        let query = ContinuationQuery {
            endpoint: &endpoint,
            token: "next-token",
        };
        assert_eq!(query.header(), endpoint.query().header());
        assert_eq!(
            query.params(),
            [
                ("ctoken", "next-token".into()),
                ("continuation", "next-token".into())
            ]
        );
        let initial = artist_json(json!([{"gridRenderer": {
            "items": [{"musicTwoRowItemRenderer": row("first", "First")}],
            "continuations": [{"nextContinuationData": {"continuation": "next-token"}}]
        }}]));
        let (first, token) = page_releases(&initial, Category::Albums).unwrap();
        assert_eq!(first[0].id, "first");
        assert_eq!(token.as_deref(), Some("next-token"));
        let continued = json!({"continuationContents": {"gridContinuation": {
            "items": [{"musicTwoRowItemRenderer": row("second", "Second")}]
        }}});
        let (second, token) = page_releases(&continued, Category::Singles).unwrap();
        assert_eq!(second[0].id, "second");
        assert!(second[0].singles);
        assert!(token.is_none());
        let appended = json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {
            "continuationItems": [{"continuationItemRenderer": {"continuationEndpoint": {
                "continuationCommand": {"token": "append-token"}
            }}}]
        }}]});
        assert_eq!(
            page_releases(&appended, Category::Albums)
                .unwrap()
                .1
                .as_deref(),
            Some("append-token")
        );
        assert!(page_releases(&json!({"contents": {}}), Category::Albums).is_err());
        let unsupported = artist_json(
            json!([{"gridRenderer": {"items": [], "continuations": [{"unknown": "token"}]}}]),
        );
        assert!(page_releases(&unsupported, Category::Albums).is_err());
    }

    #[test]
    #[ignore = "Requires live YouTube Music access; provider catalogue may change"]
    fn live_arijit_catalogue_exceeds_artist_preview() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(120), async {
                let artist_id = "UCDxKh1gFWeYsqePvgVzmPoQ";
                let api = YtMusic::new_unauthenticated().await.unwrap();
                let json = query_json(
                    &api,
                    GetArtistQuery::new(ArtistChannelID::from_raw(artist_id)),
                )
                .await
                .unwrap();
                let (_, sections) = artist_sections(&json).unwrap();
                assert!(sections
                    .iter()
                    .any(|section| section.category == Category::Singles
                        && section.endpoint.is_some()));
                let preview_count =
                    deduplicate(sections.into_iter().flat_map(|section| section.preview)).len();
                let catalogue = fetch_catalogue(&api, artist_id).await.unwrap();
                assert_eq!(catalogue.artist_id, artist_id);
                assert!(
                    catalogue.releases.len() > preview_count,
                    "full catalogue {} must exceed preview {}",
                    catalogue.releases.len(),
                    preview_count
                );
                assert!(catalogue.releases.iter().any(|release| release.singles));
                assert!(catalogue.releases.iter().any(|release| release.albums));
                assert_eq!(
                    catalogue
                        .releases
                        .iter()
                        .map(|release| &release.id)
                        .collect::<HashSet<_>>()
                        .len(),
                    catalogue.releases.len()
                );
                println!(
                    "Arijit preview: {preview_count}; catalogue: {}",
                    catalogue.releases.len()
                );
            })
            .await
            .expect("live catalogue timed out");
        });
    }

    #[test]
    fn endpoint_selection_preserves_real_id_and_category_params() {
        let albums = select_endpoint(Some("UC-real-albums"), Some("albums-token"))
            .unwrap()
            .unwrap();
        let singles = select_endpoint(Some("UC-real-singles"), Some("singles-token"))
            .unwrap()
            .unwrap();
        assert_eq!(albums.query().header()["browseId"], "UC-real-albums");
        assert_eq!(singles.query().header()["params"], "singles-token");
        assert_ne!(albums, singles);
        assert_eq!(select_endpoint(None, None).unwrap(), None);
        assert!(select_endpoint(Some("UC-real"), None).is_err());
        assert!(select_endpoint(None, Some("params")).is_err());
        assert!(select_endpoint(Some(""), Some("params")).is_err());
    }

    #[test]
    fn mapping_dedup_and_filters_keep_provider_order_and_shared_membership() {
        let releases = deduplicate([
            map_release(&row("album-first", "Same title"), Category::Albums).unwrap(),
            map_release(&row("shared", "Shared"), Category::Albums).unwrap(),
            map_release(&row("single-first", "Same title"), Category::Singles).unwrap(),
            map_release(&row("shared", "Shared"), Category::Singles).unwrap(),
        ]);
        assert_eq!(
            releases.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["album-first", "shared", "single-first"]
        );
        assert!(releases[0].albums && !releases[0].singles);
        assert!(releases[1].albums && releases[1].singles);
        assert!(!releases[2].albums && releases[2].singles);
        assert_eq!(releases[0].year, "2026");
        assert_eq!(releases[0].thumbnail_url, "card");
        assert!(map_release(&row("", "Invalid"), Category::Albums).is_err());
    }

    #[test]
    fn visible_indices_collapses_duplicates_and_filters_by_kind_and_stats() {
        let rel = |id: &str, title: &str, albums: bool, singles: bool| Release {
            id: id.into(),
            title: title.into(),
            year: "2024".into(),
            thumbnail_url: "card".into(),
            albums,
            singles,
        };
        let catalogue = Catalogue {
            artist_id: "artist-a".into(),
            artist_name: "Arijit Singh".into(),
            releases: vec![
                rel("alb-1", "Aashiqui 2", true, false),
                rel("alb-1", "Aashiqui 2", false, true),
                rel("sng-1", "Kesariya (From \"Brahmastra\")", false, true),
                rel("sng-2", "Kesariya (From \u{201C}Brahmastra\u{201D})", false, true),
                rel("comp-title", "Best of Arijit Singh", true, false),
                rel("alb-25", "Mega Pack", true, false),
                rel("ost-25", "Epic (Original Motion Picture Soundtrack)", true, false),
                rel("merged-first", "Tum Jab Paas - Single", false, true),
                rel("merged-second", "Tum Jab Paas", true, false),
            ],
        };

        let empty_stats = HashMap::new();
        assert_eq!(
            visible_indices(&catalogue, "All", &empty_stats),
            vec![0, 2, 4, 5, 6, 7]
        );
        assert_eq!(
            visible_indices(&catalogue, "Albums", &empty_stats),
            vec![0, 5, 6, 7]
        );
        assert_eq!(
            visible_indices(&catalogue, "Singles", &empty_stats),
            vec![2]
        );
        assert_eq!(
            visible_indices(&catalogue, "Compilations", &empty_stats),
            vec![4]
        );
        assert!(visible_indices(&catalogue, "Videos", &empty_stats).is_empty());

        let stats = HashMap::from([
            ("alb-25".to_string(), (1_000_000, 25)),
            ("ost-25".to_string(), (500_000, 25)),
        ]);
        assert_eq!(
            visible_indices(&catalogue, "All", &stats),
            vec![0, 2, 4, 5, 6, 7]
        );
        assert_eq!(
            visible_indices(&catalogue, "Albums", &stats),
            vec![0, 6, 7]
        );
        assert_eq!(visible_indices(&catalogue, "Singles", &stats), vec![2]);
        assert_eq!(
            visible_indices(&catalogue, "Compilations", &stats),
            vec![4, 5]
        );
    }

    #[test]
    #[ignore = "Requires live YouTube Music access; provider catalogue may change"]
    fn live_arijit_artist_row() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let artist_id = "UCDxKh1gFWeYsqePvgVzmPoQ";
            let api = YtMusic::new_unauthenticated().await.unwrap();
            let artist = api
                .get_artist(ArtistChannelID::from_raw(artist_id))
                .await
                .unwrap();
            let songs_playlist_id = artist
                .top_releases
                .songs
                .as_ref()
                .map(|songs| songs.browse_id.get_raw().to_string());
            let (releases, artist_name) = compute_artist_row(
                &api,
                artist_id,
                &artist.name,
                songs_playlist_id.as_deref(),
                20,
                &|| true,
            )
            .await
            .unwrap();
            for (idx, release) in releases.iter().enumerate() {
                eprintln!("{}: {} ({})", idx + 1, release.title, release.year);
            }
            assert!((1..=20).contains(&releases.len()));
            let stats = album_stats().lock().unwrap_or_else(|e| e.into_inner());
            for release in &releases {
                let candidate = to_candidate(release);
                let track_count = stats.get(&release.id).map(|s| s.1);
                assert_ne!(
                    core::release_rank::classify(&candidate, &artist_name, track_count),
                    core::release_rank::ReleaseKind::Compilation,
                    "unexpected compilation in artist row: {}",
                    release.title
                );
            }
        });
    }

    #[test]
    fn stale_guard_rejects_leave_return_retry_and_different_artist() {
        let request = Request {
            generation: 7,
            artist_id: "artist-a".into(),
        };
        assert!(request.matches(7, "artist-a", "Discography"));
        assert!(!request.matches(8, "artist-a", "Discography"));
        assert!(!request.matches(7, "artist-b", "Discography"));
        assert!(!request.matches(7, "artist-a", "Artist"));
    }

    fn thumbnail_catalogue() -> Catalogue {
        Catalogue {
            artist_id: "artist-a".into(),
            releases: ["album", "single"]
                .into_iter()
                .map(|id| Release {
                    id: id.into(),
                    title: id.into(),
                    year: "2026".into(),
                    thumbnail_url: "card".into(),
                    albums: id == "album",
                    singles: id == "single",
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn thumbnail_requests_are_lazy_deduplicated_and_reject_invalid_ids() {
        let mut requests = ThumbnailRequests::default();
        let catalogue = thumbnail_catalogue();
        requests.reset(&catalogue, &visible_indices(&catalogue, "All", &HashMap::new()));
        assert!(requests.requested.is_empty());
        assert!(requests.claim("missing", "artist-a", "Discography").is_none());
        assert!(requests.claim("", "artist-a", "Discography").is_none());
        assert!(requests.claim("album", "artist-b", "Discography").is_none());
        assert!(requests.claim("album", "artist-a", "Artist").is_none());
        assert!(requests.requested.is_empty());
        let request = requests.claim("single", "artist-a", "Discography").unwrap();
        assert_eq!((request.row_index, request.release_index), (1, 1));
        assert!(requests.is_current(&request));
        assert!(requests.claim("single", "artist-a", "Discography").is_none());
        assert_eq!(requests.requested.len(), 1);
    }

    #[test]
    fn thumbnail_filter_reset_invalidates_jobs_and_reindexes_rows() {
        let catalogue = thumbnail_catalogue();
        let mut requests = ThumbnailRequests::default();
        requests.reset(&catalogue, &visible_indices(&catalogue, "All", &HashMap::new()));
        let previous = requests.claim("single", "artist-a", "Discography").unwrap();
        requests.reset(&catalogue, &visible_indices(&catalogue, "Singles", &HashMap::new()));
        assert!(!requests.is_current(&previous));
        assert!(requests.claim("album", "artist-a", "Discography").is_none());
        let current = requests.claim("single", "artist-a", "Discography").unwrap();
        assert_eq!((current.row_index, current.release_index), (0, 1));
        assert!(requests.is_current(&current));
        assert!(!requests.is_current(&previous));
    }

    #[test]
    fn thumbnail_view_cancel_and_return_reject_stale_completions() {
        let catalogue = thumbnail_catalogue();
        let mut requests = ThumbnailRequests::default();
        requests.reset(&catalogue, &visible_indices(&catalogue, "All", &HashMap::new()));
        let previous = requests.claim("album", "artist-a", "Discography").unwrap();
        assert!(!requests.matches(&previous, "artist-a", "Artist"));
        assert!(!requests.matches(&previous, "artist-b", "Discography"));
        requests.clear();
        assert!(!requests.is_current(&previous));
        assert!(requests.claim("album", "artist-a", "Discography").is_none());
        requests.reset(&catalogue, &visible_indices(&catalogue, "All", &HashMap::new()));
        let current = requests.claim("album", "artist-a", "Discography").unwrap();
        assert!(requests.is_current(&current));
        assert!(!requests.is_current(&previous));
    }

    #[test]
    fn cache_is_bounded_lru_and_reads_do_not_extend_ttl() {
        let now = Instant::now();
        let mut cache = CatalogueCache::default();
        for index in 0..CACHE_CAPACITY {
            cache.insert(
                Arc::new(Catalogue {
                    artist_id: index.to_string(),
                    ..Default::default()
                }),
                now,
            );
        }
        assert!(cache.get("0", now + Duration::from_secs(60)).is_some());
        cache.insert(
            Arc::new(Catalogue {
                artist_id: "new".into(),
                ..Default::default()
            }),
            now,
        );
        assert_eq!(cache.0.len(), CACHE_CAPACITY);
        assert!(cache.get("1", now).is_none());
        assert!(cache.get("0", now + CACHE_TTL).is_none());
    }

    #[test]
    fn parse_plays_handles_suffixes_commas_views_and_invalid_inputs() {
        assert_eq!(parse_plays("1.2B plays"), 1_200_000_000);
        assert_eq!(parse_plays("1.2b plays"), 1_200_000_000);
        assert_eq!(parse_plays("345M plays"), 345_000_000);
        assert_eq!(parse_plays("2.5M views"), 2_500_000);
        assert_eq!(parse_plays("2.5m views"), 2_500_000);
        assert_eq!(parse_plays("12K plays"), 12_000);
        assert_eq!(parse_plays("12k plays"), 12_000);
        assert_eq!(parse_plays("999 plays"), 999);
        assert_eq!(parse_plays("1,234 plays"), 1_234);
        assert_eq!(parse_plays("1,234,567 views"), 1_234_567);
        assert_eq!(parse_plays(""), 0);
        assert_eq!(parse_plays("plays"), 0);
        assert_eq!(parse_plays("N/A"), 0);
        assert_eq!(parse_plays("1.2.3M plays"), 0);
        assert_eq!(parse_plays("-5K plays"), 0);
    }

    #[test]
    fn order_by_popularity_deduplicates_orders_scored_first_and_preserves_ties_and_unscored() {
        let releases = vec![
            ("unscored-1".to_string(), "Unscored 1"),
            ("mid-a".to_string(), "Mid A (first)"),
            ("high".to_string(), "High"),
            ("mid-b".to_string(), "Mid B (tied)"),
            ("mid-a".to_string(), "Mid A (duplicate)"),
            ("unscored-2".to_string(), "Unscored 2"),
            ("zero".to_string(), "Zero score"),
            ("low".to_string(), "Low"),
            ("unscored-1".to_string(), "Unscored 1 (duplicate)"),
        ];
        let plays = HashMap::from([
            ("high".to_string(), 5_000_000),
            ("mid-a".to_string(), 200_000),
            ("mid-b".to_string(), 200_000),
            ("low".to_string(), 1_500),
            ("zero".to_string(), 0),
        ]);
        let ordered = order_by_popularity(releases, &plays);
        assert_eq!(
            ordered,
            vec![
                "High",
                "Mid A (first)",
                "Mid B (tied)",
                "Low",
                "Unscored 1",
                "Unscored 2",
                "Zero score",
            ]
        );
    }
}
