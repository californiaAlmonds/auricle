use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const LIMIT: usize = 20;

#[derive(Clone)]
enum Change {
    Submit(String),
    Remove(String),
    Clear,
}

#[derive(Default)]
struct State {
    queries: Vec<String>,
    loaded: bool,
    pending: Vec<Change>,
}

#[derive(Clone)]
pub struct SearchHistory {
    state: Arc<Mutex<State>>,
    writer: Arc<Mutex<()>>,
    path: Option<PathBuf>,
}

fn spelling(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn apply(queries: &mut Vec<String>, change: &Change) {
    match change {
        Change::Submit(query) => {
            let query = spelling(query);
            if query.is_empty() {
                return;
            }
            queries.retain(|previous| previous.to_lowercase() != query.to_lowercase());
            queries.insert(0, query);
            queries.truncate(LIMIT);
        }
        Change::Remove(query) => {
            let key = spelling(query).to_lowercase();
            queries.retain(|previous| previous.to_lowercase() != key);
        }
        Change::Clear => queries.clear(),
    }
}

fn decode(bytes: &[u8]) -> Vec<String> {
    let decoded: Vec<String> = serde_json::from_slice(bytes).unwrap_or_default();
    let mut queries = Vec::new();
    for query in decoded.into_iter().rev() {
        apply(&mut queries, &Change::Submit(query));
    }
    queries
}

impl SearchHistory {
    pub fn new() -> Self {
        Self::at(std::env::var_os("LOCALAPPDATA").map(|base| {
            PathBuf::from(base).join("auricle").join("search_history.json")
        }))
    }

    fn at(path: Option<PathBuf>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            writer: Arc::new(Mutex::new(())),
            path,
        }
    }

    pub fn queries(&self) -> Vec<String> {
        self.state.lock().unwrap().queries.clone()
    }

    pub fn load(&self) {
        let _writer = self.writer.lock().unwrap();
        if self.state.lock().unwrap().loaded {
            return;
        }
        let mut queries = self.path.as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .map(|bytes| decode(&bytes))
            .unwrap_or_default();
        let changed = {
            let mut state = self.state.lock().unwrap();
            let changed = !state.pending.is_empty();
            for change in state.pending.drain(..) {
                apply(&mut queries, &change);
            }
            state.queries = queries;
            state.loaded = true;
            changed
        };
        if changed {
            self.write_current();
        }
    }

    fn update(&self, change: Change) {
        let mut state = self.state.lock().unwrap();
        apply(&mut state.queries, &change);
        if !state.loaded {
            state.pending.push(change);
        }
    }

    fn change(&self, change: Change) {
        self.update(change);
        let history = self.clone();
        super::net::spawn(move || history.save());
    }

    pub fn submit(&self, query: &str) {
        let query = spelling(query);
        if !query.is_empty() {
            self.change(Change::Submit(query));
        }
    }

    pub fn remove(&self, query: &str) {
        self.change(Change::Remove(query.into()));
    }

    pub fn clear(&self) {
        self.change(Change::Clear);
    }

    fn save(&self) {
        let _writer = self.writer.lock().unwrap();
        self.write_current();
    }

    fn write_current(&self) {
        let Some(path) = &self.path else { return; };
        let queries = {
            let state = self.state.lock().unwrap();
            if !state.loaded {
                return;
            }
            state.queries.clone()
        };
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, serde_json::to_vec(&queries)?)?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("[search-history] save failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temporary(PathBuf);

    impl Temporary {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Self(std::env::temp_dir().join(format!(
                "auricle-search-history-test-{}-{serial}", std::process::id()
            )))
        }

        fn history(&self) -> SearchHistory {
            SearchHistory::at(Some(self.0.join("search_history.json")))
        }
    }

    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bounded_recency_deduplicates_whitespace_case_and_keeps_newest_spelling() {
        let mut queries = Vec::new();
        for index in 0..25 {
            apply(&mut queries, &Change::Submit(format!("query {index}")));
        }
        assert_eq!(queries.len(), 20);
        assert_eq!(queries.last().unwrap(), "query 5");
        apply(&mut queries, &Change::Submit("  QUERY\t 10  ".into()));
        assert_eq!(queries[0], "QUERY 10");
        assert_eq!(queries.len(), 20);
        apply(&mut queries, &Change::Submit(" \n ".into()));
        assert_eq!(queries.len(), 20);
        apply(&mut queries, &Change::Remove(" query   10 ".into()));
        assert_eq!(queries.len(), 19);
        apply(&mut queries, &Change::Clear);
        assert!(queries.is_empty());
    }

    #[test]
    fn malformed_decode_is_empty_and_valid_decode_is_bounded_and_normalized() {
        for invalid in [b"not json".as_slice(), b"{}", b"[1]", b"null"] {
            assert!(decode(invalid).is_empty());
        }
        assert_eq!(decode(br#"["  NEW  spelling ", "new spelling", "", "Other"]"#),
            vec!["NEW spelling", "Other"]);
        let queries: Vec<_> = (0..30).map(|index| format!("query {index}")).collect();
        assert_eq!(decode(&serde_json::to_vec(&queries).unwrap()), queries[..20]);
    }

    #[test]
    fn pending_load_replays_submissions_removal_and_clear_without_resurrection() {
        let temporary = Temporary::new();
        let previous = temporary.history();
        previous.load();
        previous.update(Change::Submit("Old".into()));
        previous.save();
        let history = temporary.history();
        history.update(Change::Remove("old".into()));
        history.update(Change::Submit("Before load".into()));
        history.load();
        assert_eq!(history.queries(), vec!["Before load"]);
        let cleared = temporary.history();
        cleared.update(Change::Clear);
        cleared.load();
        assert!(cleared.queries().is_empty());
        assert!(decode(&std::fs::read(cleared.path.unwrap()).unwrap()).is_empty());
    }

    #[test]
    fn delayed_saves_take_current_snapshot_after_writer_lock() {
        let temporary = Temporary::new();
        let history = temporary.history();
        history.load();
        history.update(Change::Submit("Delete me".into()));
        history.update(Change::Submit("Keep me".into()));
        let writer = history.writer.lock().unwrap();
        let delayed = history.clone();
        let pending = std::thread::spawn(move || delayed.save());
        history.update(Change::Remove("delete ME".into()));
        drop(writer);
        history.save();
        pending.join().unwrap();
        assert_eq!(decode(&std::fs::read(history.path.as_ref().unwrap()).unwrap()), vec!["Keep me"]);
        let writer = history.writer.lock().unwrap();
        let delayed = history.clone();
        let pending = std::thread::spawn(move || delayed.save());
        history.update(Change::Clear);
        drop(writer);
        history.save();
        pending.join().unwrap();
        assert!(decode(&std::fs::read(history.path.as_ref().unwrap()).unwrap()).is_empty());
    }
}