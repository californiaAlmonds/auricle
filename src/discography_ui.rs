use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tokio::sync::mpsc;
use ytmapi_rs::auth::{noauth::NoAuthToken, AuthToken};
use ytmapi_rs::common::{ArtistChannelID, BrowseParams, YoutubeID};
use ytmapi_rs::parse::{ParseFrom, ProcessedResult};
use ytmapi_rs::query::{GetArtistAlbumsQuery, GetArtistQuery, PostMethod, PostQuery, Query};
use ytmapi_rs::YtMusic;

use crate::{context_ui, core, AlbumItem, NativeShellWindow};

const CACHE_CAPACITY: usize = 8;
const CACHE_TTL: Duration = Duration::from_secs(120);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
const SECTIONS: &str = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents";

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

    fn included(self, filter: &str) -> bool {
        filter == "All"
            || matches!(
                (self, filter),
                (Self::Albums, "Albums") | (Self::Singles, "Singles")
            )
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
struct Release {
    id: String,
    title: String,
    year: String,
    thumbnail_url: String,
    albums: bool,
    singles: bool,
}

impl Release {
    fn included(&self, filter: &str) -> bool {
        (self.albums && Category::Albums.included(filter))
            || (self.singles && Category::Singles.included(filter))
    }
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

impl ThumbnailRequests {
    fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.artist_id.clear();
        self.rows.clear();
        self.requested.clear();
    }

    fn reset(&mut self, catalogue: &Catalogue, filter: &str) {
        self.clear();
        self.artist_id.clone_from(&catalogue.artist_id);
        self.rows.extend(
            catalogue
                .releases
                .iter()
                .enumerate()
                .filter(|(_, release)| release.included(filter))
                .enumerate()
                .map(|(row_index, (release_index, release))| {
                    (release.id.clone(), (row_index, release_index))
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
    let items = catalogue
        .releases
        .iter()
        .filter(|release| release.included(&filter))
        .map(|release| {
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
    state.thumbnails.lock().unwrap().reset(catalogue, &filter);
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
            let mut cache = CatalogueCache::default();
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
                if let Some(catalogue) = (!force).then(|| cache.get(&request.artist_id, Instant::now())).flatten() {
                    publish(weak.clone(), state.clone(), request, Ok(catalogue));
                } else {
                    let work = tokio::time::timeout(FETCH_TIMEOUT, async {
                        if api.is_none() {
                            api = Some(YtMusic::new_unauthenticated().await.map_err(|error| error.to_string())?);
                        }
                        fetch_catalogue(api.as_ref().unwrap(), &request.artist_id).await.map(Arc::new)
                    });
                    let result = tokio::select! {
                        biased;
                        newer = receiver.recv() => { command = newer; continue; }
                        result = work => result.unwrap_or_else(|_| Err("The request timed out. Please retry.".into())),
                    };
                    if state.generation.load(Ordering::SeqCst) == request.generation {
                        if let Ok(catalogue) = &result { cache.insert(catalogue.clone(), Instant::now()); }
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
                || !matches!(category.as_str(), "All" | "Albums" | "Singles")
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
        let ids = |filter| {
            releases
                .iter()
                .filter(|release| release.included(filter))
                .map(|release| release.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("All"), ["album-first", "shared", "single-first"]);
        assert_eq!(ids("Albums"), ["album-first", "shared"]);
        assert_eq!(ids("Singles"), ["shared", "single-first"]);
        assert!(ids("Videos").is_empty());
        assert_eq!(releases[0].year, "2026");
        assert_eq!(releases[0].thumbnail_url, "card");
        assert!(map_release(&row("", "Invalid"), Category::Albums).is_err());
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
        requests.reset(&thumbnail_catalogue(), "All");
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
        requests.reset(&catalogue, "All");
        let previous = requests.claim("single", "artist-a", "Discography").unwrap();
        requests.reset(&catalogue, "Singles");
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
        requests.reset(&catalogue, "All");
        let previous = requests.claim("album", "artist-a", "Discography").unwrap();
        assert!(!requests.matches(&previous, "artist-a", "Artist"));
        assert!(!requests.matches(&previous, "artist-b", "Discography"));
        requests.clear();
        assert!(!requests.is_current(&previous));
        assert!(requests.claim("album", "artist-a", "Discography").is_none());
        requests.reset(&catalogue, "All");
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
}
