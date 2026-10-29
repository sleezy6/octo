//! Chart ids, the chart settings and the names, keys and sizes of the chart playlists.
//!
//! One id names every list, in the settings, the API and the apps: `34` the overall chart
//! (Popular right now), `new` Best New Songs, `trending` Trending Songs, and any other number
//! an Apple genre chart.

use super::apple_genre_names::APPLE_GENRE_NAMES;
use super::apple_pages::ChartGenre;

/// Apple's "All Genres", the overall chart.
pub const OVERALL: &str = "34";
pub const NEW_SONGS: &str = "new";
pub const TRENDING: &str = "trending";
/// The overall list's playlist key: the same as before charts had genres, so its id stays.
pub const OVERALL_KEY: &str = "chart:popular";
pub const OVERALL_NAME: &str = "Popular right now";
/// The words on Popular right now's cover, over the light line "Chart".
pub const OVERALL_COVER_TITLE: &str = "Popular";
pub const NEW_SONGS_NAME: &str = "Best New Songs";
pub const TRENDING_NAME: &str = "Trending Songs";
/// Songs a chart page holds without a developer token.
pub const CHART_SONGS: usize = 50;
/// Songs a room page holds without a developer token.
pub const ROOM_SONGS: usize = 100;
/// The most lists the `Charts` setting takes besides the overall.
pub const MAX_LISTS: usize = 14;
/// The most countries a listener pins besides their own.
pub const MAX_PINNED: usize = 3;
/// Between a chart id and a pinned country in a list id (`34@jp`) and a playlist key
/// (`chart:popular@jp`).
pub const PIN_MARK: char = '@';
/// Apple Music countries the country list has no English name for.
const APPLE_ONLY_NAMES: &[(&str, &str)] = &[("xk", "Kosovo")];

/// The US genre menu as read 2026-10-07: the English names Octo uses for these ids (Apple's
/// chart pages name genres in the country's own language), and the menu while Apple's own
/// cannot be read.
pub const ENGLISH_GENRES: &[(&str, &str)] = &[
    ("34", "All Genres"),
    ("20", "Alternative"),
    ("2", "Blues"),
    ("4", "Children's Music"),
    ("22", "Christian"),
    ("5", "Classical"),
    ("6", "Country"),
    ("17", "Dance"),
    ("7", "Electronic"),
    ("18", "Hip-Hop/Rap"),
    ("8", "Holiday"),
    ("11", "Jazz"),
    ("51", "K-Pop"),
    ("12", "Latin"),
    ("1153", "Metal"),
    ("14", "Pop"),
    ("15", "R&B/Soul"),
    ("24", "Reggae"),
    ("21", "Rock"),
    ("10", "Singer/Songwriter"),
    ("16", "Soundtrack"),
    ("19", "Worldwide"),
];

/// Apple Music's countries as its own country switcher listed them on 2026-10-07, used while
/// the switcher cannot be read.
pub const APPLE_COUNTRIES: &[&str] = &[
    "ae", "ag", "ai", "al", "am", "ao", "ar", "at", "au", "az", "ba", "bb", "be", "bf", "bg", "bh", "bj",
    "bm", "bn", "bo", "br", "bs", "bt", "bw", "by", "bz", "ca", "cd", "cg", "ch", "ci", "cl", "cm", "cn",
    "co", "cr", "cv", "cy", "cz", "de", "dk", "dm", "do", "dz", "ec", "ee", "eg", "es", "fi", "fj", "fm",
    "fr", "ga", "gb", "gd", "ge", "gh", "gm", "gr", "gt", "gw", "gy", "hk", "hn", "hr", "hu", "id", "ie",
    "il", "in", "iq", "is", "it", "jm", "jo", "jp", "ke", "kg", "kh", "kn", "kr", "kw", "ky", "kz", "la",
    "lb", "lc", "lk", "lr", "lt", "lu", "lv", "ly", "ma", "md", "me", "mg", "mk", "ml", "mm", "mn", "mo",
    "mr", "ms", "mt", "mu", "mv", "mw", "mx", "my", "mz", "na", "ne", "ng", "ni", "nl", "no", "np", "nz",
    "om", "pa", "pe", "pg", "ph", "pk", "pl", "pt", "py", "qa", "ro", "rs", "ru", "rw", "sa", "sb", "sc",
    "se", "sg", "si", "sk", "sl", "sn", "sr", "st", "sv", "sz", "tc", "td", "th", "tj", "tm", "tn", "to",
    "tr", "tt", "tw", "tz", "ua", "ug", "us", "uy", "uz", "vc", "ve", "vg", "vn", "vu", "xk", "ye", "za",
    "zm", "zw",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Overall,
    NewSongs,
    Trending,
    Genre,
}

/// What a chart id names, or `None` for an id that is no chart.
pub fn kind(id: &str) -> Option<ChartKind> {
    match id {
        OVERALL => Some(ChartKind::Overall),
        NEW_SONGS => Some(ChartKind::NewSongs),
        TRENDING => Some(ChartKind::Trending),
        _ if !id.is_empty() && id.len() <= 10 && id.bytes().all(|b| b.is_ascii_digit()) => {
            Some(ChartKind::Genre)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChartCountry {
    /// The server's own country, as Apple sees it.
    Auto,
    Fixed(String),
}

/// `auto`, empty or anything not two letters is automatic.
pub fn parse_country(value: &str) -> ChartCountry {
    let v = value.trim().to_ascii_lowercase();
    if v.len() == 2 && v.bytes().all(|b| b.is_ascii_lowercase()) {
        ChartCountry::Fixed(v)
    } else {
        ChartCountry::Auto
    }
}

/// What the settings API takes for `ChartCountry`.
pub fn is_valid_country(value: &str) -> bool {
    let v = value.trim();
    v.eq_ignore_ascii_case("auto") || (v.len() == 2 && v.bytes().all(|b| b.is_ascii_alphabetic()))
}

/// `"new, 18,trending,18,x,34"` -> `["new", "18", "trending"]`: known ids only, lowercase, no
/// repeats, never the overall (that one has its own switch), at most [`MAX_LISTS`].
pub fn parse_charts(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in value.split(',') {
        let id = part.trim().to_ascii_lowercase();
        if id == OVERALL || kind(&id).is_none() || out.contains(&id) {
            continue;
        }
        out.push(id);
        if out.len() == MAX_LISTS {
            break;
        }
    }
    out
}

/// What the settings API takes for `Charts`: comma-separated chart ids, empty parts ignored.
pub fn is_valid_charts(value: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .all(|p| kind(&p.to_ascii_lowercase()).is_some())
}

/// The key the list's playlist id is made from.
pub fn playlist_key(id: &str) -> String {
    match kind(id) {
        Some(ChartKind::Overall) => OVERALL_KEY.to_string(),
        Some(ChartKind::NewSongs) => "chart:new".to_string(),
        Some(ChartKind::Trending) => "chart:trending".to_string(),
        _ => format!("chart:genre:{id}"),
    }
}

/// The chart a playlist key names: the reverse of [`playlist_key`].
pub fn chart_from_key(key: &str) -> Option<String> {
    let id = match key {
        OVERALL_KEY => OVERALL,
        "chart:new" => NEW_SONGS,
        "chart:trending" => TRENDING,
        _ => key.strip_prefix("chart:genre:")?,
    };
    (kind(id).is_some()).then(|| id.to_string())
}

/// The word a chart's kind goes by in answers: `overall`, `new`, `trending` or `genre`.
pub fn kind_word(id: &str) -> Option<&'static str> {
    Some(match kind(id)? {
        ChartKind::Overall => "overall",
        ChartKind::NewSongs => "new",
        ChartKind::Trending => "trending",
        ChartKind::Genre => "genre",
    })
}

/// Whether every letter of `name` is Latin script (accents included); digits, spaces and
/// punctuation are fine.
pub fn is_latin_script(name: &str) -> bool {
    name.chars().all(|c| {
        let latin_letter = matches!(
            c,
            '\u{41}'..='\u{5A}'
                | '\u{61}'..='\u{7A}'
                | '\u{AA}'
                | '\u{BA}'
                | '\u{C0}'..='\u{24F}'
                | '\u{1E00}'..='\u{1EFF}'
                | '\u{2C60}'..='\u{2C7F}'
                | '\u{A720}'..='\u{A7FF}'
        );
        if c.is_alphabetic() {
            latin_letter
        } else {
            c < '\u{300}' || ('\u{2000}'..='\u{206F}').contains(&c)
        }
    })
}

/// A genre's English name from the tables: Octo's short names for the ids the apps show, then
/// Apple's full list.
pub fn table_genre_name(id: &str) -> Option<&'static str> {
    if let Some((_, name)) = ENGLISH_GENRES.iter().find(|(g, _)| *g == id) {
        return Some(name);
    }
    let number = |s: &str| s.parse::<u64>().ok();
    let wanted = number(id)?;
    APPLE_GENRE_NAMES
        .binary_search_by(|(g, _)| number(g).cmp(&Some(wanted)))
        .ok()
        .map(|at| APPLE_GENRE_NAMES[at].1)
}

/// A genre's name: Octo's English one when a table knows the id, else the country menu's own
/// name when it is in Latin script. None when only a foreign-script name is known.
pub fn genre_name(menu: &[ChartGenre], id: &str) -> Option<String> {
    if let Some(name) = table_genre_name(id) {
        return Some(name.to_string());
    }
    menu.iter()
        .find(|g| g.id == id)
        .filter(|g| is_latin_script(&g.name))
        .map(|g| g.name.clone())
}

/// The playlist's name: `Popular right now`, `Best New Songs`, `Trending Songs` or `Top {genre}`.
pub fn playlist_name(id: &str, genre: &str) -> String {
    match kind(id) {
        Some(ChartKind::Overall) => OVERALL_NAME.to_string(),
        Some(ChartKind::NewSongs) => NEW_SONGS_NAME.to_string(),
        Some(ChartKind::Trending) => TRENDING_NAME.to_string(),
        _ => format!("Top {}", genre.trim()),
    }
}

/// The most songs a list holds.
pub fn max_songs(id: &str) -> usize {
    match kind(id) {
        Some(ChartKind::NewSongs | ChartKind::Trending) => ROOM_SONGS,
        _ => CHART_SONGS,
    }
}

/// [`ENGLISH_GENRES`] as a menu.
pub fn english_menu() -> Vec<ChartGenre> {
    ENGLISH_GENRES
        .iter()
        .map(|(id, name)| ChartGenre {
            id: (*id).into(),
            name: (*name).into(),
        })
        .collect()
}

/// A list's id: the chart id for the listener's own country, `{chart}@{country}` for a pinned one.
pub fn list_id(chart: &str, pinned: Option<&str>) -> String {
    match pinned {
        Some(country) => format!("{chart}{PIN_MARK}{country}"),
        None => chart.to_string(),
    }
}

/// A list id's chart and pinned country: `"34@jp"` -> `("34", Some("jp"))`, `"34"` -> `("34", None)`.
pub fn split_list(list: &str) -> (&str, Option<&str>) {
    match list.split_once(PIN_MARK) {
        Some((chart, country)) if !country.is_empty() => (chart, Some(country)),
        _ => (list, None),
    }
}

/// The key a list's playlist id is made from: the chart's own key, with the pinned country after it.
pub fn list_key(list: &str) -> String {
    let (chart, pinned) = split_list(list);
    match pinned {
        Some(country) => format!("{}{PIN_MARK}{country}", playlist_key(chart)),
        None => playlist_key(chart),
    }
}

/// A list key's chart and pinned country: `chart:popular@jp` -> `("34", Some("jp"))`.
pub fn list_from_key(key: &str) -> Option<(String, Option<String>)> {
    let (base, pinned) = match key.split_once(PIN_MARK) {
        Some((base, country)) if !country.is_empty() => (base, Some(country.to_string())),
        _ => (key, None),
    };
    chart_from_key(base).map(|chart| (chart, pinned))
}

/// A country's English name, or its code in upper case when no name is known.
pub fn country_name(code: &str) -> String {
    let code = code.trim().to_ascii_lowercase();
    crate::common::countries::english_name(&code)
        .or_else(|| {
            APPLE_ONLY_NAMES
                .iter()
                .find(|(c, _)| *c == code)
                .map(|(_, name)| *name)
        })
        .map_or_else(|| code.to_ascii_uppercase(), str::to_string)
}

/// A pinned list's name: `Popular right now · Brazil`.
pub fn pinned_name(name: &str, country: &str) -> String {
    format!("{name} · {}", country_name(country))
}

/// Why pinned countries as sent cannot be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinsError {
    /// More than [`MAX_PINNED`] countries.
    TooMany,
    /// A part that is no two-letter country code, as sent.
    NotACountry(String),
}

/// Pinned countries as sent: comma-separated or repeated two-letter codes, lowercase, no
/// repeats, never `home`, at most [`MAX_PINNED`]. Anything else is the caller's mistake: the
/// first part that is no country, or too many countries.
pub fn parse_pins<'a>(
    parts: impl IntoIterator<Item = &'a str>,
    home: Option<&str>,
) -> Result<Vec<String>, PinsError> {
    let mut out: Vec<String> = Vec::new();
    for part in parts.into_iter().flat_map(|p| p.split(',')) {
        let code = part.trim().to_ascii_lowercase();
        if code.is_empty() {
            continue;
        }
        if !matches!(parse_country(&code), ChartCountry::Fixed(_)) {
            return Err(PinsError::NotACountry(part.trim().to_string()));
        }
        if Some(code.as_str()) == home || out.contains(&code) {
            continue;
        }
        out.push(code);
    }
    if out.len() > MAX_PINNED {
        return Err(PinsError::TooMany);
    }
    Ok(out)
}

#[cfg(test)]
#[path = "chart_settings_tests.rs"]
mod tests;
