use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReleaseKind {
    Album,
    Single,
    Compilation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub title: String,
    pub year: String,
    pub from_albums: bool,
    pub from_singles: bool,
}

const COMPILATION_PHRASES: &[&[&str]] = &[
    &["best", "of"],
    &["greatest", "hits"],
    &["hits"],
    &["collection"],
    &["essentials"],
    &["jukebox"],
    &["non", "stop"],
    &["nonstop"],
    &["mashup"],
    &["lofi"],
    &["lo", "fi"],
    &["vibes"],
    &["vibe"],
    &["chill"],
    &["chillout"],
    &["unwind"],
    &["spotlight"],
    &["motivation"],
    &["workout"],
    &["gym"],
    &["anthems"],
    &["reloaded"],
    &["revisited"],
    &["tribute"],
    &["remembering"],
    &["evergreen"],
    &["retro"],
    &["70s"],
    &["80s"],
    &["90s"],
    &["night", "drive"],
    &["long", "drive"],
    &["trending"],
    &["playlist"],
    &["mania"],
];

const SOUNDTRACK_PHRASES: &[&[&str]] = &[
    &["soundtrack"],
    &["original", "motion", "picture"],
    &["original", "background", "score"],
];

pub fn title_key(title: &str) -> String {
    let lowered: String = title
        .chars()
        .map(|c| match c {
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2018}' | '\u{2019}' => '\'',
            _ => c,
        })
        .flat_map(|c| c.to_lowercase())
        .collect();
    let trimmed = lowered.trim_end();
    let stripped = trimmed
        .strip_suffix(" - single")
        .or_else(|| trimmed.strip_suffix(" - ep"))
        .unwrap_or(trimmed);
    collapse_alnum(stripped)
}

pub fn is_compilation_title(title: &str, artist_name: &str) -> bool {
    let norm_title = title_key(title);
    if norm_title.is_empty() || is_soundtrack(title, &norm_title) {
        return false;
    }

    let norm_artist = title_key(artist_name);
    let artist_alnum_count = norm_artist.chars().filter(|c| c.is_alphanumeric()).count();
    if artist_alnum_count >= 5 && norm_title != norm_artist {
        let stripped_title = title_key(&strip_feat_brackets(title));
        let stripped_words: Vec<&str> = stripped_title.split_whitespace().collect();
        let artist_words: Vec<&str> = norm_artist.split_whitespace().collect();
        if contains_phrase(&stripped_words, &artist_words) {
            return true;
        }
    }

    let words: Vec<&str> = norm_title.split_whitespace().collect();
    if COMPILATION_PHRASES
        .iter()
        .any(|phrase| contains_phrase(&words, phrase))
    {
        return true;
    }

    if words.windows(2).any(|w| {
        w[0] == "top" && !w[1].is_empty() && w[1].chars().all(|c| c.is_ascii_digit())
    }) {
        return true;
    }

    words.last() == Some(&"songs")
}

pub fn classify(
    candidate: &Candidate,
    artist_name: &str,
    track_count: Option<usize>,
) -> ReleaseKind {
    if is_compilation_title(&candidate.title, artist_name) {
        return ReleaseKind::Compilation;
    }
    let norm_title = title_key(&candidate.title);
    if track_count.is_some_and(|n| n >= 20) && !is_soundtrack(&candidate.title, &norm_title) {
        return ReleaseKind::Compilation;
    }
    if candidate.from_albums {
        ReleaseKind::Album
    } else {
        ReleaseKind::Single
    }
}

pub fn dedupe(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen_ids: HashMap<String, usize> = HashMap::new();
    let mut seen_keys: HashMap<String, usize> = HashMap::new();

    for c in candidates {
        if c.id.is_empty() {
            continue;
        }
        let key = title_key(&c.title);
        if let Some(&idx) = seen_ids.get(&c.id) {
            out[idx].from_albums |= c.from_albums;
            out[idx].from_singles |= c.from_singles;
            continue;
        }
        if !key.is_empty() {
            if let Some(&idx) = seen_keys.get(&key) {
                seen_ids.insert(c.id, idx);
                out[idx].from_albums |= c.from_albums;
                out[idx].from_singles |= c.from_singles;
                continue;
            }
        }
        let idx = out.len();
        seen_ids.insert(c.id.clone(), idx);
        if !key.is_empty() {
            seen_keys.insert(key, idx);
        }
        out.push(c);
    }
    out
}

pub fn song_ranks(album_ids_in_popularity_order: &[String]) -> HashMap<String, usize> {
    let mut ranks = HashMap::new();
    for (idx, id) in album_ids_in_popularity_order.iter().enumerate() {
        if !id.is_empty() {
            ranks.entry(id.clone()).or_insert(idx);
        }
    }
    ranks
}

pub fn rank<'a>(
    candidates: &'a [Candidate],
    song_rank: &HashMap<String, usize>,
    plays: &HashMap<String, u64>,
) -> Vec<&'a Candidate> {
    let mut refs: Vec<&'a Candidate> = candidates.iter().collect();
    refs.sort_by(|a, b| {
        let a_plays = plays.get(&a.id).copied().unwrap_or(0);
        let b_plays = plays.get(&b.id).copied().unwrap_or(0);
        match (a_plays > 0, b_plays > 0) {
            (true, true) => return b_plays.cmp(&a_plays),
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            (false, false) => {}
        }

        match (song_rank.get(&a.id), song_rank.get(&b.id)) {
            (Some(a_rank), Some(b_rank)) => return a_rank.cmp(b_rank),
            (Some(_), None) => return std::cmp::Ordering::Less,
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (None, None) => {}
        }

        parse_year(&b.year).cmp(&parse_year(&a.year))
    });
    refs
}

fn collapse_alnum(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.push(c);
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn contains_phrase(words: &[&str], phrase: &[&str]) -> bool {
    !phrase.is_empty()
        && words.len() >= phrase.len()
        && words.windows(phrase.len()).any(|w| w == phrase)
}

fn is_soundtrack(raw_title: &str, norm_title: &str) -> bool {
    if raw_title.to_lowercase().contains("(from ") {
        return true;
    }
    let words: Vec<&str> = norm_title.split_whitespace().collect();
    SOUNDTRACK_PHRASES
        .iter()
        .any(|phrase| contains_phrase(&words, phrase))
}

fn strip_feat_brackets(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let chars: Vec<char> = title.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let open = chars[i];
        let close = match open {
            '(' => Some(')'),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(closing) = close {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != closing {
                j += 1;
            }
            let inner: String = chars[i + 1..j].iter().collect();
            let norm_inner = title_key(&inner);
            let is_feat = norm_inner
                .split_whitespace()
                .any(|w| matches!(w, "feat" | "ft" | "with"));
            if is_feat {
                out.push(' ');
                i = if j < chars.len() { j + 1 } else { j };
                continue;
            }
        }
        out.push(open);
        i += 1;
    }
    out
}

fn parse_year(year: &str) -> Option<u16> {
    let trimmed = year.trim();
    if trimmed.len() == 4 && trimmed.chars().all(|c| c.is_ascii_digit()) {
        trimmed.parse::<u16>().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: &str, title: &str, year: &str, from_albums: bool, from_singles: bool) -> Candidate {
        Candidate {
            id: id.to_string(),
            title: title.to_string(),
            year: year.to_string(),
            from_albums,
            from_singles,
        }
    }

    #[test]
    fn title_key_normalizes_quotes_suffixes_and_punctuation() {
        assert_eq!(
            title_key("Satranga (From \"ANIMAL\")"),
            title_key("Satranga (From \u{201C}ANIMAL\u{201D})")
        );
        assert_eq!(
            title_key("Don\u{2019}t Mind"),
            title_key("Don't Mind")
        );
        assert_eq!(
            title_key("Tum Jab Paas - Single"),
            title_key("Tum Jab Paas")
        );
        assert_eq!(
            title_key("Midnight Dreams - EP"),
            title_key("Midnight Dreams")
        );
        assert_eq!(title_key("cold/mess"), title_key("cold / mess"));
        assert_eq!(title_key("cold/mess"), "cold mess");
    }

    #[test]
    fn compilation_positive_artist_named_examples() {
        let cases = [
            ("Love Dose Arijit Singh", "Arijit Singh"),
            ("Groove With Arijit Singh", "Arijit Singh"),
            ("The Arijit Singh Collection", "Arijit Singh"),
            ("2016 Shining Star - Arijit Singh", "Arijit Singh"),
            ("With Love - Arijit Singh", "Arijit Singh"),
            ("Bollywood Romantic Songs With Armaan Malik", "Armaan Malik"),
            ("Sonu Nigam Odia Songs", "Sonu Nigam"),
            ("Lata Mangeshkar Romantic Songs", "Lata Mangeshkar"),
        ];
        for (title, artist) in cases {
            assert!(
                is_compilation_title(title, artist),
                "expected compilation for title={title:?}, artist={artist:?}"
            );
        }
    }

    #[test]
    fn compilation_positive_keyword_examples() {
        let cases = [
            ("Artist Spotlight - Akshay Kumar", "Akshay Kumar"),
            ("Gym Motivation", "Various Artists"),
            ("Love Mania 3", "Arijit Singh"),
            ("Bollywood Chillout", "Various Artists"),
            ("Old 70s Songs", "Kishore Kumar"),
            ("Night Drive Chill", "Arijit Singh"),
            ("Trending Telugu Vibes", "Sid Sriram"),
            ("Hindi Unwind", "Arijit Singh"),
            ("Romance Reloaded", "Atif Aslam"),
            ("Love Anthems", "Pritam"),
            ("Desh Bhakti Songs", "Lata Mangeshkar"),
            ("Non Stop Arijit Singh Hits", "Arijit Singh"),
            ("Top 50 Bollywood", "Pritam"),
        ];
        for (title, artist) in cases {
            assert!(
                is_compilation_title(title, artist),
                "expected compilation for title={title:?}, artist={artist:?}"
            );
        }
    }

    #[test]
    fn compilation_negative_examples() {
        let cases = [
            ("Aashiqui 2", "Arijit Singh"),
            ("Rockstar", "A. R. Rahman"),
            ("The Way That Lovers Do", "Prateek Kuhad"),
            ("Moosetape", "Sidhu Moose Wala"),
            ("Husn", "Anuv Jain"),
            ("Kesariya (From \"Brahmastra\")", "Arijit Singh"),
            (
                "Laila Majnu (Original Motion Picture Soundtrack)",
                "Niladri Kumar",
            ),
            ("Heeriye (feat. Arijit Singh)", "Arijit Singh"),
            ("Armaan", "Armaan Malik"),
            ("Aalas Ka Pedh", "The Local Train"),
            ("P-POP CULTURE", "Karan Aujla"),
            ("Arijit Singh", "Arijit Singh"),
        ];
        for (title, artist) in cases {
            assert!(
                !is_compilation_title(title, artist),
                "expected non-compilation for title={title:?}, artist={artist:?}"
            );
        }
    }

    #[test]
    fn classify_respects_title_track_count_and_source_flags() {
        let comp = cand("1", "Love Anthems", "2023", true, false);
        assert_eq!(classify(&comp, "Arijit Singh", Some(8)), ReleaseKind::Compilation);

        let many_tracks = cand("2", "Mega Pack", "2022", true, false);
        assert_eq!(
            classify(&many_tracks, "Arijit Singh", Some(20)),
            ReleaseKind::Compilation
        );

        let soundtrack_many = cand(
            "3",
            "Epic Movie (Original Motion Picture Soundtrack)",
            "2022",
            true,
            false,
        );
        assert_eq!(
            classify(&soundtrack_many, "Arijit Singh", Some(25)),
            ReleaseKind::Album
        );

        let from_descriptor_many = cand("4", "Satranga (From \"ANIMAL\")", "2023", false, true);
        assert_eq!(
            classify(&from_descriptor_many, "Arijit Singh", Some(22)),
            ReleaseKind::Single
        );

        let album = cand("5", "Rockstar", "2011", true, false);
        assert_eq!(classify(&album, "A. R. Rahman", Some(14)), ReleaseKind::Album);

        let single = cand("6", "Husn", "2023", false, true);
        assert_eq!(classify(&single, "Anuv Jain", Some(1)), ReleaseKind::Single);
    }

    #[test]
    fn dedupe_merges_flags_and_drops_empty_or_duplicate_entries() {
        let input = vec![
            cand("", "Ignored Empty", "2024", true, false),
            cand("id1", "Satranga (From \"ANIMAL\")", "2023", true, false),
            cand("id2", "Satranga (From \u{201C}ANIMAL\u{201D})", "2023", false, true),
            cand("id1", "Satranga Different Title", "2023", false, true),
            cand("id3", "Tum Jab Paas - Single", "2020", false, true),
            cand("id4", "Tum Jab Paas", "2020", true, false),
        ];

        let out = dedupe(input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "id1");
        assert!(out[0].from_albums);
        assert!(out[0].from_singles);
        assert_eq!(out[1].id, "id3");
        assert!(out[1].from_albums);
        assert!(out[1].from_singles);
    }

    #[test]
    fn song_ranks_keeps_lowest_index_and_skips_empty() {
        let ids = vec![
            "".to_string(),
            "alb_a".to_string(),
            "alb_b".to_string(),
            "alb_a".to_string(),
            "alb_c".to_string(),
        ];
        let ranks = song_ranks(&ids);
        assert_eq!(ranks.len(), 3);
        assert_eq!(ranks.get("alb_a"), Some(&1));
        assert_eq!(ranks.get("alb_b"), Some(&2));
        assert_eq!(ranks.get("alb_c"), Some(&4));
    }

    #[test]
    fn rank_orders_by_tiers_and_preserves_ties() {
        let candidates = vec![
            cand("unranked_empty_1", "No Year 1", "", true, false),
            cand("unranked_2019_a", "Old A", "2019", true, false),
            cand("unranked_2023", "New", "2023", true, false),
            cand("unranked_2019_b", "Old B", "2019", true, false),
            cand("unranked_empty_2", "No Year 2", "", true, false),
            cand("song_rank_5", "Popular 5", "2018", true, false),
            cand("song_rank_1", "Popular 1", "2015", true, false),
            cand("played_10_a", "Played 10 A", "2010", true, false),
            cand("played_50", "Played 50", "2012", true, false),
            cand("played_10_b", "Played 10 B", "2024", true, false),
        ];

        let mut plays = HashMap::new();
        plays.insert("played_50".to_string(), 50);
        plays.insert("played_10_a".to_string(), 10);
        plays.insert("played_10_b".to_string(), 10);
        plays.insert("song_rank_1".to_string(), 0);

        let mut sr = HashMap::new();
        sr.insert("song_rank_5".to_string(), 5);
        sr.insert("song_rank_1".to_string(), 1);
        sr.insert("played_10_a".to_string(), 0);

        let ranked = rank(&candidates, &sr, &plays);
        let ids: Vec<&str> = ranked.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "played_50",
                "played_10_a",
                "played_10_b",
                "song_rank_1",
                "song_rank_5",
                "unranked_2023",
                "unranked_2019_a",
                "unranked_2019_b",
                "unranked_empty_1",
                "unranked_empty_2",
            ]
        );
    }
}
