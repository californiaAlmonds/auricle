use crate::core::search::{SearchClient, SearchEntry as Entry, SearchPage};
use crate::{AlbumItem, ArtistItem, NativeShellWindow, PlaylistItem, SearchEntry, SongItem};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

const DEBOUNCE: Duration = Duration::from_millis(300);
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const INIT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Generations {
    main: AtomicU64,
    preview: AtomicU64,
}

fn advance(counter: &AtomicU64) -> u64 {
    counter.fetch_add(1, Ordering::SeqCst) + 1
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    generation: u64,
    query: String,
    category: String,
}

impl Request {
    fn matches(&self, generation: u64, view: &str, query: &str, category: &str) -> bool {
        self.generation == generation
            && view == "Search"
            && self.query == query
            && self.category == category
    }
}

enum Command {
    Main(Request),
    CancelMain,
    Preview {
        generation: u64,
        query: String,
        deadline: Instant,
    },
}

#[derive(Clone)]
struct MainWork {
    request: Request,
    base: Option<SearchPage>,
}

#[derive(Clone)]
struct PreviewWork {
    generation: u64,
    query: String,
    deadline: Instant,
    completions: Option<Result<Vec<String>, String>>,
}

#[derive(Clone)]
enum Work {
    Main(MainWork),
    Preview(PreviewWork),
}

#[derive(Default)]
struct Pending {
    main: Option<MainWork>,
    preview: Option<PreviewWork>,
}

impl Pending {
    fn apply(&mut self, command: Command) {
        match command {
            Command::Main(request) => {
                self.preview = None;
                self.main = Some(MainWork {
                    request,
                    base: None,
                });
            }
            Command::CancelMain => self.main = None,
            Command::Preview {
                generation,
                query,
                deadline,
            } => {
                self.preview = (!query.trim().is_empty()).then_some(PreviewWork {
                    generation,
                    query,
                    deadline,
                    completions: None,
                });
            }
        }
    }

    fn drain(&mut self, receiver: &mut UnboundedReceiver<Command>) {
        while let Ok(command) = receiver.try_recv() {
            self.apply(command);
        }
    }

    fn next(&self, now: Instant) -> Option<Work> {
        if let Some(preview) = self
            .preview
            .as_ref()
            .filter(|preview| preview.deadline <= now)
        {
            return Some(Work::Preview(preview.clone()));
        }
        self.main.clone().map(Work::Main)
    }

    fn interrupted(&self, work: &Work, now: Instant) -> bool {
        match work {
            Work::Main(main) => {
                self.main.as_ref().map(|next| &next.request) != Some(&main.request)
                    || self
                        .preview
                        .as_ref()
                        .is_some_and(|preview| preview.deadline <= now)
            }
            Work::Preview(preview) => {
                self.preview.as_ref().map(|next| next.generation) != Some(preview.generation)
            }
        }
    }
}

async fn deadline_wait(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending::<()>().await,
    }
}

async fn interruptible<T>(
    future: impl Future<Output = Result<T, String>>,
    timeout: Duration,
    receiver: &mut UnboundedReceiver<Command>,
    pending: &mut Pending,
    work: &Work,
) -> Option<Result<T, String>> {
    let operation = tokio::time::timeout(timeout, future);
    tokio::pin!(operation);
    loop {
        if pending.interrupted(work, Instant::now()) {
            return None;
        }
        let deadline = match work {
            Work::Main(_) => pending.preview.as_ref().map(|preview| preview.deadline),
            Work::Preview(_) => None,
        };
        tokio::select! {
            biased;
            command = receiver.recv() => {
                let Some(command) = command else { return None; };
                pending.apply(command);
                pending.drain(receiver);
            }
            _ = deadline_wait(deadline) => return None,
            result = &mut operation => {
                return Some(result.unwrap_or_else(|_| Err("Search request timed out. Try again.".into())));
            }
        }
    }
}

fn navigable(entry: &Entry) -> bool {
    !entry.id.trim().is_empty()
        && matches!(
            entry.kind.as_str(),
            "Song" | "Artist" | "Album" | "Playlist"
        )
}

fn preview_entries(completions: &[String], page: &SearchPage) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for title in completions {
        let title = title.trim();
        if !title.is_empty() && seen.insert(("Completion".into(), title.to_lowercase())) {
            entries.push(Entry {
                kind: "Completion".into(),
                title: title.into(),
                ..Entry::default()
            });
            if entries.len() == 3 {
                break;
            }
        }
    }
    let mut add = |entry: &Entry| {
        if entries.len() < 10
            && navigable(entry)
            && seen.insert((entry.kind.clone(), entry.id.clone()))
        {
            entries.push(entry.clone());
        }
    };
    if let Some(top) = &page.top {
        add(top);
    }
    let sections = [&page.artists, &page.albums, &page.songs, &page.playlists];
    let longest = sections
        .iter()
        .map(|section| section.len())
        .max()
        .unwrap_or(0);
    for index in 0..longest.min(10) {
        for section in sections {
            if let Some(entry) = section.get(index) {
                add(entry);
            }
        }
    }
    entries
}

fn preview_error(
    entries: &[Entry],
    completions_error: Option<&str>,
    search_error: Option<&str>,
) -> String {
    if entries.is_empty() {
        completions_error
            .or(search_error)
            .unwrap_or_default()
            .to_string()
    } else {
        String::new()
    }
}

fn selection_after_update(
    previous: Option<(u64, &[Entry])>,
    selected: i32,
    generation: u64,
    entries: &[Entry],
) -> i32 {
    let Some((previous_generation, previous_entries)) = previous else {
        return -1;
    };
    if previous_generation != generation {
        return -1;
    }
    let Some(selected) = usize::try_from(selected)
        .ok()
        .and_then(|index| previous_entries.get(index))
    else {
        return -1;
    };
    entries
        .iter()
        .position(|entry| {
            entry.kind == selected.kind
                && if entry.kind == "Completion" {
                    entry.title == selected.title
                } else {
                    entry.id == selected.id
                }
        })
        .map(|index| index as i32)
        .unwrap_or(-1)
}

#[derive(Default)]
struct Displayed {
    main: Option<(Request, SearchPage)>,
    unfinished: Option<Request>,
    preview: Option<(u64, Vec<Entry>)>,
    main_thumbnails: ThumbnailBudget,
    preview_thumbnails: ThumbnailBudget,
}

#[derive(Default)]
struct ThumbnailBudget {
    generation: u64,
    paths: HashSet<std::path::PathBuf>,
}

impl ThumbnailBudget {
    fn reserve(&mut self, generation: u64, path: std::path::PathBuf, limit: usize) -> bool {
        if self.generation != generation {
            self.generation = generation;
            self.paths.clear();
        }
        self.paths.len() < limit && self.paths.insert(path)
    }
}

#[derive(Clone)]
enum ImageScope {
    Main(Request),
    Preview(u64),
}

impl ImageScope {
    fn current(&self, generations: &Generations) -> bool {
        match self {
            Self::Main(request) => request.generation == generations.main.load(Ordering::SeqCst),
            Self::Preview(generation) => *generation == generations.preview.load(Ordering::SeqCst),
        }
    }
}

fn same_image(current: &Entry, downloaded: &Entry) -> bool {
    current.kind == downloaded.kind
        && current.id == downloaded.id
        && current.thumbnail_url == downloaded.thumbnail_url
}

fn image_in(entries: &[Entry], downloaded: &Entry) -> bool {
    entries.iter().any(|entry| same_image(entry, downloaded))
}

impl Displayed {
    fn contains_image(&self, scope: &ImageScope, entry: &Entry) -> bool {
        match scope {
            ImageScope::Main(request) => self.main.as_ref().is_some_and(|(current, page)| {
                current == request
                    && page_entries(page).any(|candidate| same_image(candidate, entry))
            }),
            ImageScope::Preview(generation) => {
                self.preview.as_ref().is_some_and(|(current, entries)| {
                    current == generation
                        && entries.iter().any(|candidate| same_image(candidate, entry))
                })
            }
        }
    }
}

#[derive(Clone)]
struct Bridge {
    ui: slint::Weak<NativeShellWindow>,
    generations: Arc<Generations>,
    displayed: Arc<Mutex<Displayed>>,
}

impl Bridge {
    fn main_current(&self, ui: &NativeShellWindow, request: &Request) -> bool {
        request.matches(
            self.generations.main.load(Ordering::SeqCst),
            ui.get_current_view().as_str(),
            ui.get_search_query().as_str(),
            ui.get_search_filter().as_str(),
        )
    }

    fn preview_current(&self, ui: &NativeShellWindow, generation: u64) -> bool {
        self.generations.preview.load(Ordering::SeqCst) == generation
            && ui.get_search_dropdown_open()
    }

    fn page(&self, request: Request, page: SearchPage, detail_busy: bool, detail_error: String) {
        let bridge = self.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = bridge.ui.upgrade() else {
                return;
            };
            if !bridge.main_current(&ui, &request) {
                return;
            }
            register_page(&page);
            ui.set_search_top(page.top.as_ref().map(map_entry).unwrap_or_default());
            ui.set_search_intent(page.intent.as_str().into());
            ui.set_search_results(model(page.songs.iter().map(map_song).collect()));
            ui.set_search_artists(model(page.artists.iter().map(map_artist).collect()));
            ui.set_search_albums(model(page.albums.iter().map(map_album).collect()));
            ui.set_search_playlists(model(page.playlists.iter().map(map_playlist).collect()));
            ui.set_search_busy(false);
            ui.set_search_error("".into());
            ui.set_search_detail_busy(detail_busy);
            ui.set_search_detail_error(detail_error.into());
            let scope = ImageScope::Main(request.clone());
            {
                let mut displayed = bridge.displayed.lock().unwrap();
                displayed.unfinished = None;
                displayed.main = Some((request, page));
            }
            bridge.schedule_thumbnails(scope);
        });
    }

    fn main_error(&self, request: Request, error: String) {
        let bridge = self.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = bridge.ui.upgrade() else {
                return;
            };
            if !bridge.main_current(&ui, &request) {
                return;
            }
            ui.set_search_busy(false);
            ui.set_search_detail_busy(false);
            ui.set_search_error(error.into());
            bridge.displayed.lock().unwrap().unfinished = None;
        });
    }

    fn preview(&self, generation: u64, entries: Vec<Entry>, error: String, busy: bool) {
        let bridge = self.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = bridge.ui.upgrade() else {
                return;
            };
            if !bridge.preview_current(&ui, generation) {
                return;
            }
            for entry in &entries {
                register_entry(entry);
            }
            let selected = {
                let displayed = bridge.displayed.lock().unwrap();
                selection_after_update(
                    displayed
                        .preview
                        .as_ref()
                        .map(|(generation, entries)| (*generation, entries.as_slice())),
                    ui.get_search_preview_selected(),
                    generation,
                    &entries,
                )
            };
            ui.set_search_preview(model(entries.iter().map(map_entry).collect()));
            ui.set_search_preview_selected(selected);
            ui.set_search_preview_error(error.into());
            ui.set_search_preview_busy(busy);
            bridge.displayed.lock().unwrap().preview = Some((generation, entries));
            bridge.schedule_thumbnails(ImageScope::Preview(generation));
        });
    }

    fn schedule_thumbnails(&self, scope: ImageScope) {
        let entries = {
            let mut displayed = self.displayed.lock().unwrap();
            let (entries, budget, generation, limit) = match &scope {
                ImageScope::Main(request) => {
                    let Some((current, page)) = &displayed.main else {
                        return;
                    };
                    if current != request {
                        return;
                    }
                    let mut entries: Vec<Entry> = page.top.iter().cloned().collect();
                    let sections = [&page.songs, &page.artists, &page.albums, &page.playlists];
                    for index in 0..40 {
                        for section in sections {
                            if let Some(entry) = section.get(index) {
                                entries.push(entry.clone());
                            }
                        }
                    }
                    (
                        entries,
                        &mut displayed.main_thumbnails,
                        request.generation,
                        40,
                    )
                }
                ImageScope::Preview(generation) => {
                    let Some((current, entries)) = &displayed.preview else {
                        return;
                    };
                    if current != generation {
                        return;
                    }
                    (
                        entries.clone(),
                        &mut displayed.preview_thumbnails,
                        *generation,
                        10,
                    )
                }
            };
            entries
                .into_iter()
                .filter(|entry| {
                    navigable(entry)
                        && !entry.thumbnail_url.is_empty()
                        && !thumbnail_path(entry).exists()
                        && budget.reserve(generation, thumbnail_path(entry), limit)
                })
                .collect::<Vec<_>>()
        };
        for entry in entries {
            let bridge = self.clone();
            let scope = scope.clone();
            crate::core::net::spawn(move || {
                if !scope.current(&bridge.generations)
                    || !bridge
                        .displayed
                        .lock()
                        .unwrap()
                        .contains_image(&scope, &entry)
                {
                    return;
                }
                let path = thumbnail_path(&entry);
                if !path.exists() && download_thumbnail(&entry, &path).is_err() {
                    return;
                }
                if !scope.current(&bridge.generations) {
                    return;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = bridge.ui.upgrade() else {
                        return;
                    };
                    let current = match &scope {
                        ImageScope::Main(request) => bridge.main_current(&ui, request),
                        ImageScope::Preview(generation) => bridge.preview_current(&ui, *generation),
                    };
                    let displayed = bridge.displayed.lock().unwrap();
                    if !current || !displayed.contains_image(&scope, &entry) {
                        return;
                    }
                    let (image, loaded) = crate::load_cached_thumb(&path);
                    if loaded {
                        repaint_thumbnail(&ui, &scope, &entry, image, &displayed);
                    }
                });
            });
        }
    }
}

fn download_thumbnail(entry: &Entry, path: &std::path::Path) -> Result<(), String> {
    use std::io::{Read, Write};
    let response = crate::core::net::http()
        .get(&entry.thumbnail_url)
        .timeout(Duration::from_secs(8))
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    response
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
        return Err("Invalid thumbnail size".into());
    }
    static NEXT_DOWNLOAD: AtomicU64 = AtomicU64::new(0);
    let temporary = path.with_extension(format!(
        "{}.{}.part",
        std::process::id(),
        advance(&NEXT_DOWNLOAD)
    ));
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
    {
        Ok(file) => file,
        Err(error) => return Err(error.to_string()),
    };
    let write_result = file.write_all(&bytes);
    drop(file);
    let result = write_result.and_then(|_| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result.map_err(|error| error.to_string())
}

fn repaint_rows<T: Clone + 'static>(model: ModelRc<T>, mut update: impl FnMut(&mut T) -> bool) {
    for index in 0..model.row_count() {
        if let Some(mut row) = model.row_data(index) {
            if update(&mut row) {
                model.set_row_data(index, row);
            }
        }
    }
}

fn repaint_thumbnail(
    ui: &NativeShellWindow,
    scope: &ImageScope,
    entry: &Entry,
    image: slint::Image,
    displayed: &Displayed,
) {
    if matches!(scope, ImageScope::Preview(_)) {
        repaint_rows(ui.get_search_preview(), |row| {
            if row.kind != entry.kind.as_str() || row.id != entry.id.as_str() {
                return false;
            }
            row.thumbnail = image.clone();
            row.has_thumbnail = true;
            true
        });
        return;
    }
    let Some((_, page)) = &displayed.main else {
        return;
    };
    let mut top = ui.get_search_top();
    if page.top.as_ref().is_some_and(|top| same_image(top, entry))
        && top.kind == entry.kind.as_str()
        && top.id == entry.id.as_str()
    {
        top.thumbnail = image.clone();
        top.has_thumbnail = true;
        ui.set_search_top(top);
    }
    match entry.kind.as_str() {
        "Song" if image_in(&page.songs, entry) => repaint_rows(ui.get_search_results(), |row| {
            if row.video_id != entry.id.as_str() {
                return false;
            }
            row.thumbnail = image.clone();
            row.has_thumbnail = true;
            true
        }),
        "Artist" if image_in(&page.artists, entry) => {
            repaint_rows(ui.get_search_artists(), |row| {
                if row.browse_id != entry.id.as_str() {
                    return false;
                }
                row.thumbnail = image.clone();
                row.has_thumbnail = true;
                true
            })
        }
        "Album" if image_in(&page.albums, entry) => repaint_rows(ui.get_search_albums(), |row| {
            if row.browse_id != entry.id.as_str() {
                return false;
            }
            row.thumbnail = image.clone();
            row.has_thumbnail = true;
            true
        }),
        "Playlist" if image_in(&page.playlists, entry) => {
            repaint_rows(ui.get_search_playlists(), |row| {
                if row.playlist_id != entry.id.as_str() {
                    return false;
                }
                row.thumbnail = image.clone();
                row.has_thumbnail = true;
                true
            })
        }
        _ => {}
    }
}

fn model<T: Clone + 'static>(entries: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(entries))
}

fn register_entry(entry: &Entry) {
    crate::context_ui::register_entity(&entry.kind, &entry.id, &entry.title, &entry.artist, &entry.thumbnail_url);
    if entry.kind == "Artist" && navigable(entry) {
        crate::core::menu_metadata::register_artists("Artist", &entry.id, vec![crate::core::menu_metadata::ArtistCredit {
            name: entry.title.clone(), browse_id: entry.id.clone(),
        }]);
    }
    if entry.kind == "Song" && navigable(entry) {
        crate::core::playback::register_song_meta(
            &entry.id,
            &entry.album,
            &entry.album_id,
            &entry.artist_id,
        );
    }
}

fn page_entries(page: &SearchPage) -> impl Iterator<Item = &Entry> {
    page.top
        .iter()
        .chain(&page.songs)
        .chain(&page.artists)
        .chain(&page.albums)
        .chain(&page.playlists)
}

fn register_page(page: &SearchPage) {
    for entry in page_entries(page) {
        register_entry(entry);
    }
}

fn thumbnail_path(entry: &Entry) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (&entry.kind, &entry.id, &entry.thumbnail_url).hash(&mut hash);
    std::env::temp_dir().join(format!("ytm_search_{:016x}.jpg", hash.finish()))
}

fn thumbnail(entry: &Entry) -> (slint::Image, bool) {
    if entry.kind == "Completion" {
        return (Default::default(), false);
    }
    crate::load_cached_thumb(&thumbnail_path(entry))
}

fn map_entry(entry: &Entry) -> SearchEntry {
    let (thumbnail, has_thumbnail) = thumbnail(entry);
    SearchEntry {
        kind: entry.kind.as_str().into(),
        id: entry.id.as_str().into(),
        title: entry.title.as_str().into(),
        subtitle: entry.subtitle.as_str().into(),
        artist: entry.artist.as_str().into(),
        album: entry.album.as_str().into(),
        duration_secs: entry.duration_secs.min(i32::MAX as u32) as i32,
        thumbnail,
        has_thumbnail,
    }
}

fn map_song(entry: &Entry) -> SongItem {
    let (thumbnail, has_thumbnail) = thumbnail(entry);
    SongItem {
        video_id: entry.id.as_str().into(),
        title: entry.title.as_str().into(),
        artist: entry.artist.as_str().into(),
        album: entry.album.as_str().into(),
        duration_str: if entry.duration_secs == 0 {
            String::new()
        } else {
            format!(
                "{}:{:02}",
                entry.duration_secs / 60,
                entry.duration_secs % 60
            )
        }
        .into(),
        avatar_letter: entry
            .artist
            .chars()
            .next()
            .map(|letter| letter.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into(),
        duration_secs: entry.duration_secs.min(i32::MAX as u32) as i32,
        thumbnail,
        has_thumbnail,
    }
}

fn map_artist(entry: &Entry) -> ArtistItem {
    let (thumbnail, has_thumbnail) = thumbnail(entry);
    ArtistItem {
        browse_id: entry.id.as_str().into(),
        name: entry.title.as_str().into(),
        subscriber_count: entry.subtitle.as_str().into(),
        thumbnail,
        has_thumbnail,
    }
}

fn map_album(entry: &Entry) -> AlbumItem {
    let (thumbnail, has_thumbnail) = thumbnail(entry);
    AlbumItem {
        browse_id: entry.id.as_str().into(),
        title: entry.title.as_str().into(),
        artist: entry.artist.as_str().into(),
        year: entry.year.as_str().into(),
        thumbnail,
        has_thumbnail,
    }
}

fn map_playlist(entry: &Entry) -> PlaylistItem {
    let (thumbnail, has_thumbnail) = thumbnail(entry);
    PlaylistItem {
        playlist_id: entry.id.as_str().into(),
        title: entry.title.as_str().into(),
        count_text: entry.subtitle.as_str().into(),
        thumbnail,
        has_thumbnail,
    }
}

fn needs_detail(request: &Request, page: &SearchPage) -> bool {
    request.category == "All"
        && page
            .top
            .as_ref()
            .is_some_and(|top| matches!(top.kind.as_str(), "Artist" | "Album"))
}

fn detail_result(base: &SearchPage, result: Result<SearchPage, String>) -> (SearchPage, String) {
    match result {
        Ok(page) => (page, String::new()),
        Err(error) => (base.clone(), error),
    }
}

async fn worker(mut receiver: UnboundedReceiver<Command>, bridge: Bridge) {
    let mut client = None;
    let mut pending = Pending::default();
    loop {
        pending.drain(&mut receiver);
        if receiver.is_closed() {
            return;
        }
        let Some(work) = pending.next(Instant::now()) else {
            let deadline = pending.preview.as_ref().map(|preview| preview.deadline);
            tokio::select! {
                command = receiver.recv() => {
                    let Some(command) = command else { return; };
                    pending.apply(command);
                }
                _ = deadline_wait(deadline) => {}
            }
            continue;
        };
        if client.is_none() {
            let Some(result) = interruptible(
                SearchClient::new(),
                INIT_TIMEOUT,
                &mut receiver,
                &mut pending,
                &work,
            )
            .await
            else {
                continue;
            };
            match result {
                Ok(initialized) => client = Some(initialized),
                Err(error) => {
                    match work {
                        Work::Main(main) => {
                            bridge.main_error(main.request, error);
                            pending.main = None;
                        }
                        Work::Preview(preview) => {
                            bridge.preview(preview.generation, Vec::new(), error, false);
                            pending.preview = None;
                        }
                    }
                    continue;
                }
            }
        }
        let client = client.as_mut().unwrap();
        match &work {
            Work::Main(main) => {
                let future = async {
                    match &main.base {
                        Some(base) => client.enrich(base).await,
                        None => {
                            client
                                .search(&main.request.query, &main.request.category)
                                .await
                        }
                    }
                };
                let Some(result) =
                    interruptible(future, FETCH_TIMEOUT, &mut receiver, &mut pending, &work).await
                else {
                    continue;
                };
                if let Some(base) = &main.base {
                    let (page, error) = detail_result(base, result);
                    bridge.page(main.request.clone(), page, false, error);
                    pending.main = None;
                    continue;
                }
                match result {
                    Ok(page) if main.base.is_none() && needs_detail(&main.request, &page) => {
                        bridge.page(
                            main.request.clone(),
                            page.clone(),
                            true,
                            String::new(),
                        );
                        pending.main = Some(MainWork {
                            request: main.request.clone(),
                            base: Some(page),
                        });
                    }
                    Ok(page) => {
                        bridge.page(main.request.clone(), page, false, String::new());
                        pending.main = None;
                    }
                    Err(error) => {
                        bridge.main_error(main.request.clone(), error);
                        pending.main = None;
                    }
                }
            }
            Work::Preview(preview) => {
                if let Some(completions) = &preview.completions {
                    let Some(result) = interruptible(
                        client.preview(&preview.query),
                        FETCH_TIMEOUT,
                        &mut receiver,
                        &mut pending,
                        &work,
                    )
                    .await
                    else {
                        continue;
                    };
                    let entries = preview_entries(
                        completions.as_ref().map(Vec::as_slice).unwrap_or_default(),
                        result.as_ref().unwrap_or(&SearchPage::default()),
                    );
                    let error = preview_error(
                        &entries,
                        completions.as_ref().err().map(String::as_str),
                        result.as_ref().err().map(String::as_str),
                    );
                    bridge.preview(preview.generation, entries, error, false);
                    pending.preview = None;
                } else {
                    let Some(result) = interruptible(
                        client.completions(&preview.query),
                        FETCH_TIMEOUT,
                        &mut receiver,
                        &mut pending,
                        &work,
                    )
                    .await
                    else {
                        continue;
                    };
                    if let Ok(completions) = &result {
                        let entries = preview_entries(completions, &SearchPage::default());
                        if !entries.is_empty() {
                            bridge.preview(preview.generation, entries, String::new(), false);
                        }
                    }
                    if let Some(preview) = &mut pending.preview {
                        preview.completions = Some(result);
                    }
                }
            }
        }
    }
}

fn cancel_preview(ui: &NativeShellWindow, bridge: &Bridge, sender: &UnboundedSender<Command>) {
    let generation = advance(&bridge.generations.preview);
    ui.set_search_preview_busy(false);
    ui.set_search_preview_error("".into());
    ui.set_search_preview(model(Vec::new()));
    ui.set_search_preview_selected(-1);
    ui.set_search_dropdown_open(false);
    bridge.displayed.lock().unwrap().preview = None;
    let _ = sender.send(Command::Preview {
        generation,
        query: String::new(),
        deadline: Instant::now(),
    });
}

fn prepare_request(
    ui: &NativeShellWindow,
    bridge: &Bridge,
    sender: &UnboundedSender<Command>,
    query: String,
    category: String,
) -> Request {
    crate::context_ui::invalidate(ui);
    if ui.get_current_view() != "Search" {
        ui.invoke_navigate("Search".into());
    }
    cancel_preview(ui, bridge, sender);
    let request = Request {
        generation: advance(&bridge.generations.main),
        query,
        category,
    };
    ui.set_is_loading(false);
    ui.set_search_query(request.query.as_str().into());
    ui.set_search_filter(request.category.as_str().into());
    ui.set_search_busy(true);
    ui.set_search_detail_busy(false);
    ui.set_search_error("".into());
    ui.set_search_detail_error("".into());
    ui.set_search_top(SearchEntry::default());
    ui.set_search_intent("".into());
    ui.set_search_results(model(Vec::new()));
    ui.set_search_artists(model(Vec::new()));
    ui.set_search_albums(model(Vec::new()));
    ui.set_search_playlists(model(Vec::new()));
    {
        let mut displayed = bridge.displayed.lock().unwrap();
        displayed.main = None;
        displayed.unfinished = None;
    }
    request
}

fn submit(
    ui: &NativeShellWindow,
    bridge: &Bridge,
    sender: &UnboundedSender<Command>,
    query: String,
    category: String,
) {
    let request = prepare_request(ui, bridge, sender, query, category);
    bridge.displayed.lock().unwrap().unfinished = Some(request.clone());
    if sender.send(Command::Main(request)).is_err() {
        bridge.displayed.lock().unwrap().unfinished = None;
        ui.set_search_busy(false);
        ui.set_search_error("Search worker unavailable. Restart Auricle.".into());
    }
}

pub(crate) struct Controller {
    bridge: Bridge,
    sender: UnboundedSender<Command>,
}

pub(crate) struct ExternalRequest {
    bridge: Bridge,
    request: Request,
}

impl Controller {
    pub(crate) fn begin_external(&self, ui: &NativeShellWindow, query: String) -> ExternalRequest {
        let request = prepare_request(ui, &self.bridge, &self.sender, query, "Songs".into());
        let _ = self.sender.send(Command::CancelMain);
        ExternalRequest {
            bridge: self.bridge.clone(),
            request,
        }
    }
}

impl ExternalRequest {
    pub(crate) fn finish(self, result: Result<Vec<Entry>, String>) {
        match result {
            Ok(songs) => self.bridge.page(
                self.request,
                SearchPage { songs, ..SearchPage::default() },
                false,
                String::new(),
            ),
            Err(error) => self.bridge.main_error(self.request, error),
        }
    }
}

pub(crate) fn wire(ui: &NativeShellWindow) -> Controller {
    let (sender, receiver) = mpsc::unbounded_channel();
    let bridge = Bridge {
        ui: ui.as_weak(),
        generations: Arc::new(Generations::default()),
        displayed: Arc::new(Mutex::new(Displayed::default())),
    };
    let controller = Controller { bridge: bridge.clone(), sender: sender.clone() };
    let worker_bridge = bridge.clone();
    let _ = std::thread::Builder::new()
        .name("ytm-search".into())
        .spawn(move || {
            crate::core::audio_priority::lower_current_thread();
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(worker(receiver, worker_bridge)),
                Err(error) => log::error!("Search runtime: {error}"),
            }
        });
    {
        let bridge = bridge.clone();
        let sender = sender.clone();
        ui.on_search_view_changed(move |view| {
            let Some(ui) = bridge.ui.upgrade() else {
                return;
            };
            crate::context_ui::invalidate(&ui);
            cancel_preview(&ui, &bridge, &sender);
            if view != "Search" {
                advance(&bridge.generations.main);
                let _ = sender.send(Command::CancelMain);
                if ui.get_search_detail_busy() {
                    ui.set_search_detail_error("Details interrupted. Showing search results.".into());
                } else if ui.get_search_busy()
                    && bridge.displayed.lock().unwrap().unfinished.is_none()
                {
                    ui.set_search_error("Mood search interrupted. Choose the mood again to retry.".into());
                }
                ui.set_search_busy(false);
                ui.set_search_detail_busy(false);
            } else if !ui.get_search_busy() {
                let unfinished = bridge.displayed.lock().unwrap().unfinished.take();
                if let Some(request) = unfinished {
                    submit(&ui, &bridge, &sender, request.query, request.category);
                }
            }
        });
    }
    {
        let bridge = bridge.clone();
        let sender = sender.clone();
        ui.on_do_search(move |query| {
            if let Some(ui) = bridge.ui.upgrade() {
                submit(&ui, &bridge, &sender, query.to_string(), "All".into());
            }
        });
    }
    {
        let bridge = bridge.clone();
        let sender = sender.clone();
        ui.on_search_category(move |category| {
            if let Some(ui) = bridge.ui.upgrade() {
                if ui.get_current_view() != "Search" {
                    return;
                }
                if !matches!(
                    category.as_str(),
                    "All" | "Songs" | "Artists" | "Albums" | "Playlists"
                ) {
                    return;
                }
                submit(
                    &ui,
                    &bridge,
                    &sender,
                    ui.get_search_query().to_string(),
                    category.to_string(),
                );
            }
        });
    }
    {
        let bridge = bridge.clone();
        let sender = sender.clone();
        ui.on_live_search(move |query| {
            let Some(ui) = bridge.ui.upgrade() else {
                return;
            };
            if query.trim().is_empty() {
                cancel_preview(&ui, &bridge, &sender);
                return;
            }
            let generation = advance(&bridge.generations.preview);
            ui.set_search_preview(model(Vec::new()));
            ui.set_search_preview_selected(-1);
            ui.set_search_preview_error("".into());
            ui.set_search_preview_busy(true);
            bridge.displayed.lock().unwrap().preview = None;
            if sender
                .send(Command::Preview {
                    generation,
                    query: query.to_string(),
                    deadline: Instant::now() + DEBOUNCE,
                })
                .is_err()
            {
                ui.set_search_preview_busy(false);
                ui.set_search_preview_error("Search worker unavailable. Restart Auricle.".into());
            }
        });
    }
    ui.on_activate_search_entry(move |entry| {
        let Some(ui) = bridge.ui.upgrade() else {
            return;
        };
        cancel_preview(&ui, &bridge, &sender);
        if entry.kind == "Completion" {
            ui.invoke_do_search(entry.title);
            return;
        }
        if entry.id.trim().is_empty() {
            return;
        }
        match entry.kind.as_str() {
            "Song" => ui.invoke_play_song(entry.id, entry.title, entry.artist, entry.duration_secs),
            "Artist" => ui.invoke_navigate_to_artist(entry.id),
            "Album" => ui.invoke_navigate_to_album(entry.id),
            "Playlist" => ui.invoke_navigate_to_playlist(entry.id),
            _ => {}
        }
    });
    controller
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, id: &str) -> Entry {
        Entry {
            kind: kind.into(),
            id: id.into(),
            title: id.into(),
            ..Entry::default()
        }
    }

    fn request(generation: u64) -> Request {
        Request {
            generation,
            query: "Original Query".into(),
            category: "All".into(),
        }
    }

    #[test]
    fn preview_mixes_kinds_and_deduplicates_top_and_completions() {
        let page = SearchPage {
            top: Some(entry("Song", "song-0")),
            songs: (0..30)
                .map(|index| entry("Song", &format!("song-{index}")))
                .collect(),
            artists: vec![entry("Artist", "artist")],
            albums: vec![entry("Album", "album")],
            playlists: vec![entry("Playlist", "playlist")],
            ..SearchPage::default()
        };
        let completions = ["one", "ONE", "two", "three", "four"].map(str::to_string);
        let entries = preview_entries(&completions, &page);
        assert_eq!(entries.len(), 10);
        assert!(entries[..3].iter().all(|entry| entry.kind == "Completion"));
        assert_eq!(entries[3].id, "song-0");
        for kind in ["Artist", "Album", "Song", "Playlist"] {
            assert!(entries.iter().any(|entry| entry.kind == kind));
        }
        assert_eq!(
            entries.iter().filter(|entry| entry.id == "song-0").count(),
            1
        );
    }

    #[test]
    fn preview_rejects_missing_navigation_ids_and_keeps_partial_success() {
        let page = SearchPage {
            songs: vec![entry("Song", ""), entry("Song", "ok")],
            ..SearchPage::default()
        };
        let entries = preview_entries(&[], &page);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            preview_error(&entries, Some("completions failed"), None),
            ""
        );
        assert_eq!(
            preview_error(&[], Some("offline"), Some("failed")),
            "offline"
        );
        assert_eq!(preview_error(&[], None, None), "");
    }

    #[test]
    fn bursts_coalesce_and_debounce_uses_latest_deadline() {
        let now = Instant::now();
        let mut pending = Pending::default();
        for generation in 1..=3 {
            pending.apply(Command::Preview {
                generation,
                query: generation.to_string(),
                deadline: now + DEBOUNCE,
            });
        }
        assert!(pending
            .next(now + DEBOUNCE - Duration::from_millis(1))
            .is_none());
        let Some(Work::Preview(preview)) = pending.next(now + DEBOUNCE) else {
            panic!("preview not ready")
        };
        assert_eq!(preview.generation, 3);
        pending.apply(Command::Preview {
            generation: 4,
            query: "latest".into(),
            deadline: now + DEBOUNCE * 2,
        });
        assert!(pending.interrupted(&Work::Preview(preview), now));
        assert!(pending.next(now + DEBOUNCE).is_none());
    }

    #[test]
    fn enter_and_preview_close_do_not_cancel_main() {
        let now = Instant::now();
        let mut pending = Pending::default();
        pending.apply(Command::Main(request(1)));
        let main = pending.next(now).unwrap();
        pending.apply(Command::Preview {
            generation: 2,
            query: String::new(),
            deadline: now,
        });
        assert!(!pending.interrupted(&main, now));
        pending.apply(Command::Main(request(2)));
        assert!(pending.interrupted(&main, now));
    }

    #[test]
    fn stale_success_errors_and_loading_share_the_same_guard() {
        let generations = Generations::default();
        let old = request(advance(&generations.main));
        let latest = request(advance(&generations.main));
        let current = generations.main.load(Ordering::SeqCst);
        assert!(!old.matches(current, "Search", &old.query, "All"));
        assert!(latest.matches(current, "Search", &latest.query, "All"));
        assert!(!latest.matches(current, "Home", &latest.query, "All"));
        assert!(!latest.matches(current, "Search", "other query", "All"));
        assert!(!latest.matches(current, "Search", &latest.query, "Songs"));
        advance(&generations.preview);
        assert!(latest.matches(current, "Search", &latest.query, "All"));
    }

    #[test]
    fn cancel_main_invalidates_queued_work_and_late_results_after_returning() {
        let generations = Generations::default();
        let old = request(advance(&generations.main));
        let mut pending = Pending::default();
        pending.apply(Command::Main(old.clone()));
        let work = pending.next(Instant::now()).unwrap();
        advance(&generations.main);
        pending.apply(Command::CancelMain);
        assert!(pending.main.is_none());
        assert!(pending.next(Instant::now()).is_none());
        assert!(pending.interrupted(&work, Instant::now()));
        assert!(!old.matches(
            generations.main.load(Ordering::SeqCst),
            "Search",
            &old.query,
            &old.category,
        ));
        pending.apply(Command::Main(request(advance(&generations.main))));
        assert_eq!(pending.main.unwrap().request.generation, 3);
    }

    #[test]
    fn preview_selection_follows_identity_only_within_the_same_generation() {
        let completion = Entry {
            kind: "Completion".into(),
            title: "query".into(),
            ..Entry::default()
        };
        let previous = vec![completion.clone(), entry("Song", "same-id")];
        let updated = vec![
            entry("Artist", "same-id"),
            completion,
            Entry {
                title: "Updated title".into(),
                thumbnail_url: "https://example.invalid/updated.jpg".into(),
                ..entry("Song", "same-id")
            },
        ];
        let old = Some((1, previous.as_slice()));
        assert_eq!(selection_after_update(old, 0, 1, &updated), 1);
        assert_eq!(selection_after_update(old, 1, 1, &updated), 2);
        assert_eq!(selection_after_update(old, 1, 2, &updated), -1);
        assert_eq!(selection_after_update(old, -1, 1, &updated), -1);
        assert_eq!(selection_after_update(old, 2, 1, &updated), -1);
        assert_eq!(selection_after_update(old, 1, 1, &updated[..2]), -1);
        assert_eq!(selection_after_update(None, 0, 1, &updated), -1);
        let other_completion = Entry {
            kind: "Completion".into(),
            title: "different".into(),
            ..Entry::default()
        };
        assert_eq!(selection_after_update(old, 0, 1, &[other_completion]), -1);
    }

    #[test]
    fn detail_failure_keeps_successful_query_sections_and_reports_the_error() {
        let base = SearchPage {
            top: Some(entry("Artist", "top")),
            intent: "Artist".into(),
            songs: vec![entry("Song", "query-song")],
            artists: vec![entry("Artist", "query-artist")],
            albums: vec![entry("Album", "query-album")],
            playlists: vec![entry("Playlist", "query-playlist")],
        };
        let (page, error) = detail_result(&base, Err("Detail unavailable".into()));
        assert_eq!(error, "Detail unavailable");
        assert_eq!(page.top.unwrap().id, "top");
        assert_eq!(page.intent, "Artist");
        assert_eq!(page.songs[0].id, "query-song");
        assert_eq!(page.artists[0].id, "query-artist");
        assert_eq!(page.albums[0].id, "query-album");
        assert_eq!(page.playlists[0].id, "query-playlist");
        let (page, error) = detail_result(&base, Ok(SearchPage::default()));
        assert!(error.is_empty());
        assert!(page_entries(&page).next().is_none());
    }

    #[test]
    fn detail_is_only_requested_for_all_artist_and_album_results() {
        let page = SearchPage {
            top: Some(entry("Artist", "artist")),
            intent: "Artist".into(),
            songs: vec![entry("Song", "query-song")],
            artists: vec![entry("Artist", "query-artist")],
            albums: vec![entry("Album", "query-album")],
            playlists: vec![entry("Playlist", "query-playlist")],
        };
        assert!(needs_detail(&request(1), &page));
        assert!(!needs_detail(
            &Request {
                category: "Songs".into(),
                ..request(1)
            },
            &page
        ));
    }

    #[test]
    fn thumbnails_are_identity_url_and_stage_scoped_with_a_per_request_cap() {
        let mut top = entry("Artist", "../artist");
        top.thumbnail_url = "https://example.invalid/old.jpg".into();
        let old_path = thumbnail_path(&top);
        assert_eq!(old_path.parent(), Some(std::env::temp_dir().as_path()));
        let mut updated = top.clone();
        updated.thumbnail_url = "https://example.invalid/new.jpg".into();
        assert_ne!(old_path, thumbnail_path(&updated));
        assert!(!image_in(&[updated.clone()], &top));
        assert!(image_in(&[updated.clone()], &updated));
        let mut displayed = Displayed {
            main: Some((
                request(1),
                SearchPage {
                    top: Some(updated.clone()),
                    ..SearchPage::default()
                },
            )),
            ..Displayed::default()
        };
        let scope = ImageScope::Main(request(1));
        assert!(!displayed.contains_image(&scope, &top));
        assert!(displayed.contains_image(&scope, &updated));
        displayed.main.as_mut().unwrap().1.top = None;
        assert!(!displayed.contains_image(&scope, &updated));
        let mut budget = ThumbnailBudget::default();
        for index in 0..40 {
            assert!(budget.reserve(1, format!("{index}").into(), 40));
        }
        assert!(!budget.reserve(1, "detail-extra".into(), 40));
        assert!(budget.reserve(2, "new-request".into(), 40));
        assert!(!budget.reserve(2, "new-request".into(), 40));
    }

    #[test]
    fn command_channel_drains_a_burst_to_the_latest_main_and_preview() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut pending = Pending::default();
        sender.send(Command::Main(request(1))).unwrap();
        sender.send(Command::Main(request(2))).unwrap();
        for generation in 1..=4 {
            sender
                .send(Command::Preview {
                    generation,
                    query: generation.to_string(),
                    deadline: Instant::now() + DEBOUNCE,
                })
                .unwrap();
        }
        pending.drain(&mut receiver);
        assert_eq!(pending.main.as_ref().unwrap().request.generation, 2);
        assert_eq!(pending.preview.as_ref().unwrap().generation, 4);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn cancellation_drops_inflight_detail_and_preview_futures() {
        use std::cell::Cell;
        use std::rc::Rc;
        struct DropProbe(Rc<Cell<bool>>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        for preview in [false, true] {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let mut pending = Pending::default();
            if preview {
                pending.apply(Command::Preview {
                    generation: 1,
                    query: "old".into(),
                    deadline: Instant::now(),
                });
            } else {
                pending.main = Some(MainWork {
                    request: request(1),
                    base: Some(SearchPage::default()),
                });
            }
            let work = pending.next(Instant::now()).unwrap();
            let dropped = Rc::new(Cell::new(false));
            let probe = DropProbe(dropped.clone());
            let operation = std::future::poll_fn(move |context| {
                let _ = &probe;
                sender.send(Command::Main(request(2))).unwrap();
                context.waker().wake_by_ref();
                std::task::Poll::<Result<(), String>>::Pending
            });
            let result = runtime.block_on(interruptible(
                operation,
                FETCH_TIMEOUT,
                &mut receiver,
                &mut pending,
                &work,
            ));
            assert!(result.is_none());
            assert!(dropped.get());
            assert_eq!(pending.main.unwrap().request.generation, 2);
        }
    }

    #[test]
    fn cancel_main_drops_inflight_search_and_detail_futures() {
        use std::cell::Cell;
        use std::rc::Rc;
        struct DropProbe(Rc<Cell<bool>>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        for base in [None, Some(SearchPage::default())] {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let mut pending = Pending {
                main: Some(MainWork { request: request(1), base }),
                preview: None,
            };
            let work = pending.next(Instant::now()).unwrap();
            let dropped = Rc::new(Cell::new(false));
            let probe = DropProbe(dropped.clone());
            let operation = std::future::poll_fn(move |context| {
                let _ = &probe;
                sender.send(Command::CancelMain).unwrap();
                context.waker().wake_by_ref();
                std::task::Poll::<Result<(), String>>::Pending
            });
            let result = runtime.block_on(interruptible(
                operation,
                FETCH_TIMEOUT,
                &mut receiver,
                &mut pending,
                &work,
            ));
            assert!(result.is_none());
            assert!(dropped.get());
            assert!(pending.main.is_none());
        }
    }

    #[test]
    fn preview_close_preserves_the_inflight_main_future() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut pending = Pending::default();
        pending.apply(Command::Main(request(1)));
        let work = pending.next(Instant::now()).unwrap();
        let mut polls = 0;
        let operation = std::future::poll_fn(move |context| {
            polls += 1;
            if polls == 1 {
                sender
                    .send(Command::Preview {
                        generation: 2,
                        query: String::new(),
                        deadline: Instant::now(),
                    })
                    .unwrap();
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(Ok(polls))
            }
        });
        let result = runtime.block_on(interruptible(
            operation,
            FETCH_TIMEOUT,
            &mut receiver,
            &mut pending,
            &work,
        ));
        assert_eq!(result, Some(Ok(2)));
    }

    #[test]
    fn ready_preview_preempts_detail_but_keeps_it_available_to_resume() {
        let now = Instant::now();
        let mut pending = Pending {
            main: Some(MainWork {
                request: request(1),
                base: Some(SearchPage::default()),
            }),
            preview: None,
        };
        let detail = pending.next(now).unwrap();
        pending.apply(Command::Preview {
            generation: 1,
            query: "new preview".into(),
            deadline: now + DEBOUNCE,
        });
        assert!(!pending.interrupted(&detail, now));
        assert!(pending.interrupted(&detail, now + DEBOUNCE));
        assert!(matches!(
            pending.next(now + DEBOUNCE),
            Some(Work::Preview(_))
        ));
        pending.apply(Command::Preview {
            generation: 2,
            query: String::new(),
            deadline: now,
        });
        let Some(Work::Main(resumed)) = pending.next(now) else {
            panic!("main was lost")
        };
        assert!(resumed.base.is_some());
        assert_eq!(resumed.request, request(1));
    }

    #[test]
    fn hung_operation_returns_a_timeout_without_a_network_request() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let (_sender, mut receiver) = mpsc::unbounded_channel();
        let mut pending = Pending::default();
        pending.apply(Command::Main(request(1)));
        let work = pending.next(Instant::now()).unwrap();
        let result = runtime.block_on(interruptible(
            std::future::pending::<Result<(), String>>(),
            Duration::ZERO,
            &mut receiver,
            &mut pending,
            &work,
        ));
        assert!(result.unwrap().unwrap_err().contains("timed out"));
    }
}
