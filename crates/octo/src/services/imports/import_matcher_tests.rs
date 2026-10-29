//! Matching imported songs against the library.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::import_keys::ImportKeys;
use super::super::test_support::row;
use super::*;

fn r(id: &str, artist: &str, title: &str, seconds: i32) -> LibrarySongRow {
    row(id, artist, title, Some(seconds), None, "mp3", 320)
}

fn track(artist: &str, title: &str, seconds: Option<i32>, album: Option<&str>) -> ImportTrack {
    ImportTrack {
        key: ImportKeys::for_words(Some(artist), Some(title)),
        artist: artist.into(),
        title: title.into(),
        seconds,
        album: album.map(str::to_string),
        ..Default::default()
    }
}

fn found<'a>(index: &'a LibraryIndex, t: &ImportTrack) -> Option<&'a str> {
    index.find(t).map(|row| row.id.as_str())
}

#[test]
fn the_same_song_is_found_however_its_credit_and_version_words_are_written() {
    let index = LibraryIndex::new([
        r("1", "Drake", "Too Good (feat. Rihanna)", 263),
        r("2", "The Beatles", "Let It Be - Remastered 2009", 243),
        r("3", "Beyoncé", "Halo", 261),
        r("4", "Rihanna, Calvin Harris", "We Found Love", 215),
    ]);
    assert_eq!(
        found(&index, &track("Drake, Rihanna", "Too Good", Some(263), None)),
        Some("1")
    );
    assert_eq!(
        found(&index, &track("The Beatles", "Let It Be", Some(243), None)),
        Some("2")
    );
    assert_eq!(
        found(&index, &track("Beyonce", "Halo", Some(262), None)),
        Some("3")
    );
    // The other credit order still names the same artists.
    assert_eq!(
        found(
            &index,
            &track("Calvin Harris, Rihanna", "We Found Love", Some(215), None)
        ),
        Some("4")
    );
}

#[test]
fn a_live_take_or_a_far_length_is_another_song() {
    let index = LibraryIndex::new([
        r("1", "Nirvana", "Lithium (Live)", 260),
        r("2", "Nirvana", "Polly", 300),
    ]);
    assert_eq!(found(&index, &track("Nirvana", "Lithium", Some(257), None)), None);
    assert_eq!(found(&index, &track("Nirvana", "Polly", Some(177), None)), None);
    assert_eq!(
        found(&index, &track("Someone Else", "Polly", Some(300), None)),
        None
    );
}

#[test]
fn a_song_with_no_length_is_matched_on_its_words_and_the_closest_copy_wins() {
    let index = LibraryIndex::new([
        row(
            "mp3",
            "Massive Attack",
            "Teardrop",
            Some(330),
            Some("Mezzanine"),
            "mp3",
            320,
        ),
        row(
            "flac",
            "Massive Attack",
            "Teardrop",
            Some(331),
            Some("Mezzanine"),
            "flac",
            900,
        ),
        row(
            "edit",
            "Massive Attack",
            "Teardrop",
            Some(320),
            Some("Singles"),
            "mp3",
            320,
        ),
    ]);
    assert_eq!(
        found(
            &index,
            &track("Massive Attack", "Teardrop", None, Some("Mezzanine"))
        ),
        Some("flac")
    );
    assert_eq!(
        found(&index, &track("Massive Attack", "Teardrop", Some(320), None)),
        Some("edit")
    );
}

#[tokio::test]
async fn the_matcher_reads_the_whole_library_once_and_again_after_forget() {
    let reads = Arc::new(AtomicUsize::new(0));
    let library: Arc<Vec<LibrarySongRow>> = Arc::new(
        (0..1500)
            .map(|i| r(&i.to_string(), &format!("Artist {i}"), &format!("Song {i}"), 200))
            .collect(),
    );
    let counted = reads.clone();
    let matcher = ImportMatcher::with_pages(Arc::new(move |start, count| {
        counted.fetch_add(1, Ordering::SeqCst);
        let page: Vec<LibrarySongRow> = library
            .iter()
            .skip(start as usize)
            .take(count as usize)
            .cloned()
            .collect();
        let n = page.len();
        Box::pin(async move { Some((page, n)) })
    }));
    let mut tracks = vec![
        track("Artist 1499", "Song 1499", Some(200), None),
        track("Nobody", "Nothing", Some(200), None),
    ];
    assert!(matcher.match_tracks(&mut tracks).await);
    assert_eq!(tracks[0].library_id.as_deref(), Some("1499"));
    assert_eq!(tracks[1].library_id, None);
    assert_eq!(reads.load(Ordering::SeqCst), 2);

    matcher.match_tracks(&mut tracks).await;
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    matcher.forget();
    matcher.match_tracks(&mut tracks).await;
    assert_eq!(reads.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn a_library_that_cannot_be_read_keeps_what_the_last_match_found() {
    let matcher = ImportMatcher::with_pages(Arc::new(|_, _| Box::pin(async { None })));
    let mut tracks = vec![track("A", "B", Some(200), None)];
    tracks[0].library_id = Some("kept".into());
    assert!(!matcher.match_tracks(&mut tracks).await);
    assert_eq!(tracks[0].library_id.as_deref(), Some("kept"));
    assert_eq!(
        matcher.problem().as_deref(),
        Some(ImportMatcher::CANNOT_READ_LIBRARY)
    );
}

fn matcher_over(rows: Vec<LibrarySongRow>) -> ImportMatcher {
    let rows = Arc::new(rows);
    ImportMatcher::with_pages(Arc::new(move |start, count| {
        let page: Vec<LibrarySongRow> = rows
            .iter()
            .skip(start as usize)
            .take(count as usize)
            .cloned()
            .collect();
        let n = page.len();
        Box::pin(async move { Some((page, n)) })
    }))
}

fn at(mut song: LibrarySongRow, path: &str) -> LibrarySongRow {
    song.path = path.into();
    song
}

fn family_library() -> Vec<LibrarySongRow> {
    vec![
        at(
            r("shared", "Adele", "Hello", 295),
            "/music/Adele/25/01 Hello.flac",
        ),
        at(
            r("sam-own", "Lorde", "Royals", 190),
            "/members/sam/Lorde/Royals.flac",
        ),
        at(
            r("alex-own", "Sia", "Chandelier", 216),
            "/members/alex/Sia/Chandelier.flac",
        ),
    ]
}

fn family_tracks() -> Vec<ImportTrack> {
    vec![
        track("Adele", "Hello", Some(295), None),
        track("Lorde", "Royals", Some(190), None),
        track("Sia", "Chandelier", Some(216), None),
    ]
}

#[tokio::test]
async fn with_family_on_a_member_has_the_shared_library_and_their_own_songs_only() {
    use crate::services::family::library_index::scope_for_tests;

    let matcher =
        matcher_over(family_library()).with_library_scope(scope_for_tests(true, &[("alex", 3), ("sam", 4)]));
    let mut tracks = family_tracks();
    assert!(matcher.match_tracks_for("alex", &mut tracks).await);
    let ids: Vec<Option<&str>> = tracks.iter().map(|t| t.library_id.as_deref()).collect();
    // Sam's own copy of Royals is not Alex's, so Alex's list still fetches it.
    assert_eq!(ids, vec![Some("shared"), None, Some("alex-own")]);

    let mut tracks = family_tracks();
    assert!(matcher.match_tracks_for("Sam", &mut tracks).await);
    let ids: Vec<Option<&str>> = tracks.iter().map(|t| t.library_id.as_deref()).collect();
    assert_eq!(ids, vec![Some("shared"), Some("sam-own"), None]);
}

#[tokio::test]
async fn with_family_off_every_song_counts_as_before() {
    use crate::services::family::library_index::scope_for_tests;

    let matcher =
        matcher_over(family_library()).with_library_scope(scope_for_tests(false, &[("alex", 3), ("sam", 4)]));
    let mut scoped = family_tracks();
    assert!(matcher.match_tracks_for("alex", &mut scoped).await);
    let mut whole = family_tracks();
    assert!(matcher_over(family_library()).match_tracks(&mut whole).await);
    let ids = |tracks: &[ImportTrack]| -> Vec<Option<String>> {
        tracks.iter().map(|t| t.library_id.clone()).collect()
    };
    assert_eq!(ids(&scoped), ids(&whole));
    assert_eq!(
        ids(&scoped),
        vec![
            Some("shared".to_string()),
            Some("sam-own".to_string()),
            Some("alex-own".to_string())
        ]
    );
}

#[tokio::test]
async fn a_hidden_copy_never_stands_in_for_a_visible_one() {
    use crate::services::family::library_index::scope_for_tests;

    // Sam's own copy is the closer length, but Alex can only play the shared one.
    let library = vec![
        at(r("sam-own", "Adele", "Hello", 295), "/members/sam/Hello.flac"),
        at(r("shared", "Adele", "Hello", 299), "/music/Adele/Hello.flac"),
    ];
    let matcher = matcher_over(library).with_library_scope(scope_for_tests(true, &[("alex", 3), ("sam", 4)]));
    let mut tracks = vec![track("Adele", "Hello", Some(295), None)];
    assert!(matcher.match_tracks_for("alex", &mut tracks).await);
    assert_eq!(tracks[0].library_id.as_deref(), Some("shared"));
}

fn coded(mut song: LibrarySongRow, isrc: &str) -> LibrarySongRow {
    song.isrcs = vec![isrc.to_string()];
    song
}

fn with_isrc(mut t: ImportTrack, isrc: &str) -> ImportTrack {
    t.isrc = Some(isrc.to_string());
    t
}

#[test]
fn a_shared_isrc_wins_over_the_words_and_takes_the_closest_length() {
    let index = LibraryIndex::new([
        r("words", "Massive Attack", "Teardrop", 330),
        coded(
            r("coded-far", "Massive Attack", "Teardrop (Album Version)", 400),
            "GBAAA9800003",
        ),
        coded(
            r("coded-near", "Elizabeth Fraser", "Tear Drop", 331),
            "GB-AAA-98-00003",
        ),
    ]);
    let wanted = with_isrc(
        track("Massive Attack", "Teardrop", Some(330), None),
        "gbaaa9800003",
    );
    assert_eq!(found(&index, &wanted), Some("coded-near"));
    // An ISRC the library does not carry falls back to the words.
    let other = with_isrc(
        track("Massive Attack", "Teardrop", Some(330), None),
        "USRC17607839",
    );
    assert_eq!(found(&index, &other), Some("words"));
}

#[test]
fn a_title_in_another_script_is_the_same_song() {
    let index = LibraryIndex::new([r("cyr", "Эмин", "Камин", 210)]);
    assert_eq!(
        found(&index, &track("Emin", "Kamin", Some(212), None)),
        Some("cyr")
    );
    assert_eq!(found(&index, &track("Emin", "Kamin", None, None)), Some("cyr"));
    let latin = LibraryIndex::new([r("lat", "Emin", "Kamin", 210)]);
    assert_eq!(
        found(&latin, &track("Эмин", "Камин", Some(210), None)),
        Some("lat")
    );
    // Another length is another recording, in any script.
    assert_eq!(found(&index, &track("Emin", "Kamin", Some(260), None)), None);
}

#[test]
fn a_credit_written_last_name_first_is_the_same_artist() {
    let index = LibraryIndex::new([r("1", "Luis Fonsi", "Despacito", 229)]);
    assert_eq!(
        found(&index, &track("Fonsi, Luis", "Despacito", Some(229), None)),
        Some("1")
    );
    // A different singer of the same title is not.
    assert_eq!(
        found(&index, &track("Luis Miguel", "Despacito", Some(229), None)),
        None
    );
}

#[test]
fn a_typo_is_not_owned() {
    let index = LibraryIndex::new([r("1", "Imagine Dragons", "Believer", 204)]);
    assert_eq!(
        found(&index, &track("Imagine Dragons", "Beleiver", Some(204), None)),
        None
    );
}

#[test]
fn a_live_take_is_not_the_studio_song_by_any_lookup() {
    let index = LibraryIndex::new([
        r("live", "Nirvana", "Lithium (Live)", 257),
        r("cyr-live", "Нирвана", "Литиум (Live)", 257),
    ]);
    assert_eq!(found(&index, &track("Nirvana", "Lithium", Some(257), None)), None);
    assert_eq!(found(&index, &track("Nirvana", "Litium", Some(257), None)), None);
}

#[test]
fn a_tribute_band_is_not_the_artist() {
    let index = LibraryIndex::new([
        r("1", "Luis Fonsi Tribute Band", "Despacito", 229),
        r("2", "Fonsi, Luis (Karaoke)", "Despacito", 229),
    ]);
    assert_eq!(
        found(&index, &track("Luis Fonsi", "Despacito", Some(229), None)),
        None
    );
}

#[test]
fn a_title_with_stylized_letters_is_found_through_the_artist() {
    let index = LibraryIndex::new([r("1", "Kesha", "Ca$h Flow", 200)]);
    assert_eq!(
        found(&index, &track("Kesha", "Cash Flow", Some(201), None)),
        Some("1")
    );
}

#[test]
fn among_several_copies_the_best_fit_wins_first() {
    // Both are the song by its MatchKey; the copy with a known, equal length fits better than
    // one with no length, whatever their order.
    let index = LibraryIndex::new([
        row("unknown", "Adele", "Hello", None, None, "flac", 900),
        row("known", "Adele", "Hello", Some(295), None, "mp3", 320),
    ]);
    assert_eq!(
        found(&index, &track("Adele", "Hello", Some(295), None)),
        Some("known")
    );
}

#[tokio::test]
async fn every_lookup_keeps_to_the_members_scope() {
    use crate::services::family::library_index::scope_for_tests;

    let library = vec![
        at(
            coded(r("sam-isrc", "Massive Attack", "Teardrop", 330), "GBAAA9800003"),
            "/members/sam/Teardrop.flac",
        ),
        at(r("sam-cyr", "Эмин", "Камин", 210), "/members/sam/Kamin.flac"),
        at(
            r("sam-credit", "Luis Fonsi", "Despacito", 229),
            "/members/sam/Despacito.flac",
        ),
        at(
            r("sam-name", "Kesha", "Ca$h Flow", 200),
            "/members/sam/CashFlow.flac",
        ),
    ];
    let tracks = || {
        vec![
            with_isrc(track("Someone", "Something", Some(330), None), "GBAAA9800003"),
            track("Emin", "Kamin", Some(210), None),
            track("Fonsi, Luis", "Despacito", Some(229), None),
            track("Kesha", "Cash Flow", Some(200), None),
        ]
    };
    let matcher = matcher_over(library).with_library_scope(scope_for_tests(true, &[("alex", 3), ("sam", 4)]));
    let mut mine = tracks();
    assert!(matcher.match_tracks_for("sam", &mut mine).await);
    let ids: Vec<Option<&str>> = mine.iter().map(|t| t.library_id.as_deref()).collect();
    assert_eq!(
        ids,
        vec![
            Some("sam-isrc"),
            Some("sam-cyr"),
            Some("sam-credit"),
            Some("sam-name")
        ]
    );
    let mut theirs = tracks();
    assert!(matcher.match_tracks_for("alex", &mut theirs).await);
    assert!(theirs.iter().all(|t| t.library_id.is_none()), "{theirs:?}");
}

#[test]
fn a_name_on_thousands_of_songs_reads_only_the_first_two_thousand() {
    let mut rows: Vec<LibrarySongRow> = (0..2_500)
        .map(|i| r(&i.to_string(), "Traditional", &format!("Air {i}"), 200))
        .collect();
    rows.push(r("late", "Traditional", "Ca$h Air", 200));
    let index = LibraryIndex::new(rows);
    assert_eq!(
        found(&index, &track("Traditional", "Cash Air", Some(200), None)),
        None
    );
    let early = LibraryIndex::new([r("early", "Traditional", "Ca$h Air", 200)]);
    assert_eq!(
        found(&early, &track("Traditional", "Cash Air", Some(200), None)),
        Some("early")
    );
}

#[tokio::test]
async fn a_list_written_title_first_is_read_the_other_way_round() {
    use super::super::text_list::{Orientation, read_lines};

    let matcher = matcher_over(vec![
        r("1", "Adele", "Hello", 295),
        r("2", "Massive Attack", "Teardrop", 330),
    ]);
    let backward = read_lines(
        "Mine",
        "Hello - Adele\nTeardrop - Massive Attack\nNew Song - Nobody\n",
    )
    .expect("lines");
    assert_eq!(
        matcher.orientation_for("alex", &backward[0].lines).await,
        Orientation::TitleArtist
    );
    let forward = read_lines("Mine", "Adele - Hello\nMassive Attack - Teardrop\n").expect("lines");
    assert_eq!(
        matcher.orientation_for("alex", &forward[0].lines).await,
        Orientation::ArtistTitle
    );
}

// ---- the version a song names, the real case of G-Eazy's "Let's Get Lost" ---------------------

fn on(mut song: LibrarySongRow, album: &str) -> LibrarySongRow {
    song.album = Some(album.into());
    song
}

/// The library as it was on 2026-10-14: the original was in Recently removed, the Bear//Face
/// remix sat tagged as the plain song on the remix EP, and the Teklife remix carried the
/// Bear//Face title and ISRC.
fn lost_library(with_original: bool) -> Vec<LibrarySongRow> {
    let mut rows = vec![
        on(
            r("5usZcgFijfma1blnIYNjXB", "G-Eazy", "Let's Get Lost", 242),
            "Let's Get Lost Remixes",
        ),
        coded(
            on(
                r(
                    "1l7NVUVMjStbjNcPflszjx",
                    "G-Eazy",
                    "Let's Get Lost (feat. Devon Baldwin) [Bear//Face Remix]",
                    270,
                ),
                "Let's Get Lost Remixes (feat. Devon Baldwin) - EP",
            ),
            "USRC11501446",
        ),
    ];
    if with_original {
        rows.push(on(
            r(
                "6Ab7filWyAu4VzstkaDG3a",
                "G-Eazy",
                "Let's Get Lost (feat. Devon Baldwin)",
                241,
            ),
            "These Things Happen",
        ));
    }
    rows
}

fn original_row() -> ImportTrack {
    with_isrc(
        track(
            "G-Eazy",
            "Let's Get Lost (feat. Devon Baldwin)",
            None,
            Some("These Things Happen"),
        ),
        "USRC11400725",
    )
}

#[test]
fn the_original_never_maps_to_the_plain_titled_remix_on_the_remix_ep() {
    let index = LibraryIndex::new(lost_library(false));
    assert_eq!(
        found(&index, &original_row()),
        None,
        "fetched rather than a near match"
    );
    let index = LibraryIndex::new(lost_library(true));
    assert_eq!(found(&index, &original_row()), Some("6Ab7filWyAu4VzstkaDG3a"));
}

#[test]
fn the_remix_row_maps_by_its_isrc() {
    let index = LibraryIndex::new(lost_library(true));
    let remix = with_isrc(
        track(
            "G-Eazy",
            "Let's Get Lost (feat. Devon Baldwin) [Bear//Face Remix]",
            None,
            Some("Let's Get Lost Remixes (feat. Devon Baldwin) - EP"),
        ),
        "USRC11501446",
    );
    assert_eq!(found(&index, &remix), Some("1l7NVUVMjStbjNcPflszjx"));
}

#[test]
fn remixes_by_different_people_are_different_songs_and_keys() {
    let index = LibraryIndex::new([on(
        r("astro", "Pegboard Nerds", "Psychonaut (AstroLizard Remix)", 250),
        "Psychonaut (Remixes)",
    )]);
    for title in [
        "Psychonaut (12th Planet Remix)",
        "Psychonaut (Hydraulix Remix)",
        "Psychonaut (Krischvn Remix)",
    ] {
        let row = track("Pegboard Nerds", title, None, Some("Psychonaut (Remixes)"));
        assert_eq!(found(&index, &row), None, "{title}");
    }
    assert_eq!(
        found(
            &index,
            &track(
                "Pegboard Nerds",
                "Psychonaut (AstroLizard Remix)",
                None,
                Some("Psychonaut (Remixes)")
            )
        ),
        Some("astro")
    );
    let twelfth = ImportKeys::for_words(Some("Pegboard Nerds"), Some("Psychonaut (12th Planet Remix)"));
    let hydraulix = ImportKeys::for_words(Some("Pegboard Nerds"), Some("Psychonaut (Hydraulix Remix)"));
    assert_ne!(twelfth, hydraulix);
    assert!(twelfth.ends_with("|remix|by:12thplanet"), "{twelfth}");
    // A key saved before remixes kept their remixer's name reads as the new one.
    assert_eq!(
        ImportKeys::current(
            "song:pegboardnerds|psychonaut|remix",
            "Pegboard Nerds",
            "Psychonaut (12th Planet Remix)"
        ),
        twelfth
    );
    assert_eq!(
        ImportKeys::current("spotify:abc", "Pegboard Nerds", "Psychonaut (12th Planet Remix)"),
        "spotify:abc"
    );
}

#[test]
fn a_live_album_copy_titled_plainly_is_not_the_studio_song() {
    let index = LibraryIndex::new([on(
        r("live", "Garth Brooks", "Rodeo", 230),
        "Live in Germany 1995",
    )]);
    assert_eq!(
        found(
            &index,
            &track("Garth Brooks", "Rodeo", None, Some("The Ultimate Hits Disc: 2"))
        ),
        None
    );
    assert_eq!(
        found(
            &index,
            &track("Garth Brooks", "Rodeo", None, Some("Live in Germany 1995"))
        ),
        Some("live")
    );
}

#[tokio::test]
async fn a_file_tagged_with_the_recording_an_isrc_names_is_found_without_an_isrc_tag() {
    let mut tagged = on(
        r("tagged", "G-Eazy", "Let's Get Lost (feat. Devon Baldwin)", 241),
        "These Things Happen",
    );
    tagged.recording_id = Some("20CB620C-1B37-4E93-9B4B-C64B0A2FADDC".into());
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let matcher = matcher_over(vec![tagged]).with_isrc_recordings(Arc::new(move |isrc: String| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Some(if isrc == "USRC11400725" {
                vec![IsrcRecording {
                    id: "20cb620c-1b37-4e93-9b4b-c64b0a2faddc".into(),
                    title: "Let\u{2019}s Get Lost".into(),
                    seconds: Some(241),
                }]
            } else {
                Vec::new()
            })
        })
    }));
    let mut rows = vec![with_isrc(
        track("G-Eazy", "Let's Get Lost", None, Some("Somewhere Else")),
        "USRC11400725",
    )];
    matcher.learn_isrcs(&rows).await;
    assert!(matcher.match_tracks(&mut rows).await);
    assert_eq!(rows[0].library_id.as_deref(), Some("tagged"));
    // Asked once; the answer is kept.
    matcher.learn_isrcs(&rows).await;
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

#[test]
fn the_music_databases_isrc_answer_reads_into_recordings() {
    let doc = serde_json::json!({
        "recordings": [
            {"id": "20cb620c", "title": "Let\u{2019}s Get Lost", "length": 240680},
            {"id": "nolength", "title": "Let\u{2019}s Get Lost"}
        ]
    });
    assert_eq!(
        isrc_recordings_of(&doc),
        vec![
            IsrcRecording {
                id: "20cb620c".into(),
                title: "Let\u{2019}s Get Lost".into(),
                seconds: Some(241),
            },
            IsrcRecording {
                id: "nolength".into(),
                title: "Let\u{2019}s Get Lost".into(),
                seconds: None,
            },
        ]
    );
}

fn recorded_isrc(file: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/musicbrainz/").to_owned() + file,
    )
    .expect("a recorded MusicBrainz answer");
    serde_json::from_str(&text).expect("JSON")
}

fn tagged_with(mut song: LibrarySongRow, recording: &str) -> LibrarySongRow {
    song.recording_id = Some(recording.into());
    song
}

fn answering(recordings: Vec<IsrcRecording>) -> IsrcRecordings {
    Arc::new(move |_isrc: String| {
        let recordings = recordings.clone();
        Box::pin(async move { Some(recordings) })
    })
}

#[tokio::test]
async fn a_file_tagged_with_an_excerpt_the_isrc_also_names_is_not_the_song() {
    // MusicBrainz lists Stayin' Alive's code on two 93 s recordings and one of 189 s.
    let recordings = isrc_recordings_of(&recorded_isrc("isrc-search-GBA077700130.json"));
    assert_eq!(recordings.len(), 3);
    let excerpt = tagged_with(
        r("excerpt", "Bee Gees", "Night Fever Medley", 93),
        "8e13de06-8ada-4a68-91ad-171204ed59c2",
    );
    let matcher = matcher_over(vec![excerpt]).with_isrc_recordings(answering(recordings));
    let mut rows = vec![with_isrc(
        track("Bee Gees", "Stayin' Alive", Some(189), None),
        "GBA077700130",
    )];
    matcher.learn_isrcs(&rows).await;
    assert!(matcher.match_tracks(&mut rows).await);
    assert_eq!(rows[0].library_id, None);
}

#[tokio::test]
async fn a_row_without_a_length_finds_the_file_tagged_with_a_take_the_code_names_in_any_order() {
    let mut recordings = isrc_recordings_of(&recorded_isrc("isrc-search-GBA077700130.json"));
    for _ in 0..2 {
        let full = tagged_with(
            r("full", "Bee Gees", "Saturday Night Fever Part 2", 189),
            "b9f84ada-627e-4565-b9f7-686b440ed279",
        );
        let matcher = matcher_over(vec![full]).with_isrc_recordings(answering(recordings.clone()));
        let mut rows = vec![with_isrc(
            track("Bee Gees", "Stayin' Alive", None, None),
            "GBA077700130",
        )];
        matcher.learn_isrcs(&rows).await;
        assert!(matcher.match_tracks(&mut rows).await);
        assert_eq!(rows[0].library_id.as_deref(), Some("full"), "{recordings:?}");
        recordings.reverse();
    }
}

#[tokio::test]
async fn the_code_lookup_keeps_only_what_musicbrainz_really_said() {
    use crate::services::fingerprint::music_brainz_client::MusicBrainzClient;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBA077700130"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path("/ws/2/isrc/GBA077700130"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            recorded_isrc("isrc-lookup-GBA077700130.json").to_string(),
            "application/json",
        ))
        .mount(&server)
        .await;
    let base = url::Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    let ask = isrc_recordings_from(Arc::new(MusicBrainzClient::with_base_url(base)));

    // Busy: nothing kept, so the next match asks again.
    assert_eq!(ask("GBA077700130".into()).await, None);
    let recordings = ask("GBA077700130".into()).await.expect("answered");
    assert_eq!(recordings.len(), 3);
}

#[tokio::test]
async fn a_match_never_asks_the_music_database() {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let matcher = matcher_over(vec![r("teardrop", "Massive Attack", "Teardrop", 331)]).with_isrc_recordings(
        Arc::new(move |_isrc: String| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Some(Vec::new()) })
        }),
    );
    let mut rows = vec![with_isrc(
        track("Massive Attack", "Teardrop", Some(331), None),
        "GBAAA9800001",
    )];
    assert!(matcher.match_tracks(&mut rows).await);
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert_eq!(rows[0].library_id.as_deref(), Some("teardrop"));
}

#[tokio::test]
async fn a_learning_round_says_what_it_settled() {
    let matcher = matcher_over(vec![tagged_with(r("second", "B", "Second", 200), "rec-2")])
        .with_isrc_recordings(Arc::new(|isrc: String| {
            Box::pin(async move {
                Some(if isrc == "GBAAA0000002" {
                    vec![IsrcRecording {
                        id: "rec-2".into(),
                        title: "Second".into(),
                        seconds: Some(200),
                    }]
                } else {
                    Vec::new()
                })
            })
        }));
    let rows = vec![
        with_isrc(track("A", "First", Some(200), None), "GBAAA0000001"),
        with_isrc(track("B", "Second", Some(200), None), "GBAAA0000002"),
    ];
    assert_eq!(
        matcher.learn_isrcs(&rows).await,
        IsrcLearning {
            settled: 2,
            in_library: 1,
            failed: false
        }
    );
    assert_eq!(
        matcher.learn_isrcs(&rows).await,
        IsrcLearning::default(),
        "nothing is asked twice"
    );
}

#[tokio::test]
async fn codes_that_keep_failing_are_asked_after_the_others_oldest_failure_first() {
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = asked.clone();
    let matcher = matcher_over(vec![]).with_isrc_recordings(Arc::new(move |isrc: String| {
        log.lock().push(isrc.clone());
        Box::pin(async move { (isrc == "GBAAA0000002").then(Vec::new) })
    }));
    let rows = vec![
        with_isrc(track("A", "First", Some(200), None), "GBAAA0000001"),
        with_isrc(track("B", "Second", Some(200), None), "GBAAA0000002"),
        with_isrc(track("C", "Third", Some(200), None), "GBAAA0000003"),
    ];
    for _ in 0..4 {
        matcher.learn_isrcs(&rows).await;
    }
    assert_eq!(
        *asked.lock(),
        [
            "GBAAA0000001",
            "GBAAA0000002",
            "GBAAA0000003",
            "GBAAA0000001",
            "GBAAA0000003"
        ]
    );
}

#[tokio::test]
async fn a_refused_code_question_is_kept_and_the_next_isrc_is_still_asked() {
    use crate::services::fingerprint::music_brainz_client::MusicBrainzClient;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::builder().start().await;
    Mock::given(path("/ws/2/isrc/GBAAA0000001"))
        .respond_with(
            ResponseTemplate::new(400).set_body_raw(
                std::fs::read_to_string(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/musicbrainz/isrc-refused.json"
                ))
                .expect("fixture"),
                "application/json",
            ),
        )
        .mount(&server)
        .await;
    Mock::given(path("/ws/2/isrc/GBA077700130"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            recorded_isrc("isrc-lookup-GBA077700130.json").to_string(),
            "application/json",
        ))
        .mount(&server)
        .await;
    let base = url::Url::parse(&format!("{}/ws/2/", server.uri())).expect("parses");
    let ask = isrc_recordings_from(Arc::new(MusicBrainzClient::with_base_url(base)));
    assert_eq!(ask("GBAAA0000001".into()).await, Some(Vec::new()));

    let matcher = matcher_over(vec![]).with_isrc_recordings(ask);
    let rows = vec![
        with_isrc(track("A", "First", Some(200), None), "GBAAA0000001"),
        with_isrc(
            track("Bee Gees", "Stayin' Alive", Some(189), None),
            "GBA077700130",
        ),
    ];
    matcher.learn_isrcs(&rows).await;
    let asked: Vec<String> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.url.to_string())
        .collect();
    assert!(
        asked.iter().any(|url| url.contains("/ws/2/isrc/GBA077700130")),
        "{asked:?}"
    );
}

#[tokio::test]
async fn a_row_without_a_length_borrows_one_only_when_the_codes_takes_agree() {
    let agreeing = vec![
        IsrcRecording {
            id: "x".into(),
            title: "Song".into(),
            seconds: Some(189),
        },
        IsrcRecording {
            id: "y".into(),
            title: "Song".into(),
            seconds: Some(190),
        },
    ];
    let matcher =
        matcher_over(vec![r("short", "Band", "Song", 93)]).with_isrc_recordings(answering(agreeing.clone()));
    let mut rows = vec![with_isrc(track("Band", "Song", None, None), "GBAAA0000003")];
    matcher.learn_isrcs(&rows).await;
    assert!(matcher.match_tracks(&mut rows).await);
    assert_eq!(rows[0].library_id, None, "a 93 s copy is not a 189 s song");

    let matcher =
        matcher_over(vec![r("full", "Band", "Song", 189)]).with_isrc_recordings(answering(agreeing));
    let mut rows = vec![with_isrc(track("Band", "Song", None, None), "GBAAA0000003")];
    matcher.learn_isrcs(&rows).await;
    assert!(matcher.match_tracks(&mut rows).await);
    assert_eq!(rows[0].library_id.as_deref(), Some("full"));

    let disagreeing = vec![
        IsrcRecording {
            id: "x".into(),
            title: "Song".into(),
            seconds: Some(93),
        },
        IsrcRecording {
            id: "y".into(),
            title: "Song".into(),
            seconds: Some(189),
        },
    ];
    let matcher =
        matcher_over(vec![r("short", "Band", "Song", 93)]).with_isrc_recordings(answering(disagreeing));
    let mut rows = vec![with_isrc(track("Band", "Song", None, None), "GBAAA0000003")];
    matcher.learn_isrcs(&rows).await;
    assert!(matcher.match_tracks(&mut rows).await);
    assert_eq!(
        rows[0].library_id.as_deref(),
        Some("short"),
        "no agreed length: matched by name"
    );
}

#[test]
fn the_waiting_isrcs_of_a_long_list_are_read_quickly() {
    let matcher = matcher_over(vec![]).with_isrc_recordings(answering(Vec::new()));
    let rows: Vec<ImportTrack> = (0..50_000)
        .map(|i| {
            with_isrc(
                track("A", &format!("Song {i}"), Some(200), None),
                &format!("GBAAA{i:07}"),
            )
        })
        .collect();
    for i in 0..49_980 {
        matcher
            .isrc_failed
            .lock()
            .insert(format!("GBAAA{i:07}"), i as u64);
    }
    let started = std::time::Instant::now();
    let wanted = matcher.pending_isrcs(None, &rows, ImportMatcher::ISRC_ASKS_PER_MATCH);
    assert_eq!(wanted.len(), ImportMatcher::ISRC_ASKS_PER_MATCH);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    // Every song still waits, and that is read quickly too.
    for i in 49_980..50_000 {
        matcher
            .isrc_failed
            .lock()
            .insert(format!("GBAAA{i:07}"), i as u64);
    }
    let started = std::time::Instant::now();
    assert!(matcher.has_pending_isrcs(&rows));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn what_was_learned_survives_a_restart() {
    let dir = tempfile::tempdir().expect("a folder");
    let file = dir.path().join("imports-isrc.json");
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let tagged = tagged_with(r("roads", "Portishead", "Track 05", 305), "rec-roads");
    let first = matcher_over(vec![tagged.clone()])
        .with_isrc_recordings(Arc::new(move |_isrc: String| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Some(vec![IsrcRecording {
                    id: "rec-roads".into(),
                    title: "Roads".into(),
                    seconds: Some(305),
                }])
            })
        }))
        .with_isrc_file(Some(file.clone()));
    let mut rows = vec![with_isrc(
        track("Portishead", "Roads", Some(305), None),
        "GBAAA9400010",
    )];
    first.learn_isrcs(&rows).await;
    assert_eq!(asked.load(Ordering::SeqCst), 1);

    // A new matcher over the same file: nothing to ask, and the copy is still found.
    let again = Arc::new(AtomicUsize::new(0));
    let counted = again.clone();
    let second = matcher_over(vec![tagged])
        .with_isrc_recordings(Arc::new(move |_isrc: String| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Some(Vec::new()) })
        }))
        .with_isrc_file(Some(file));
    assert!(second.match_tracks(&mut rows).await);
    assert_eq!(rows[0].library_id.as_deref(), Some("roads"));
    rows[0].library_id = None;
    second.learn_isrcs(&rows).await;
    assert_eq!(again.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_last_look_before_a_fetch_finds_a_copy_only_the_isrc_names() {
    let tagged = tagged_with(r("roads", "Portishead", "Track 05", 305), "rec-roads");
    let matcher = matcher_over(vec![tagged]).with_isrc_recordings(answering(vec![IsrcRecording {
        id: "rec-roads".into(),
        title: "Roads".into(),
        seconds: Some(305),
    }]));
    let song = with_isrc(track("Portishead", "Roads", Some(305), None), "GBAAA9400010");
    assert_eq!(
        matcher.owned_by_isrc("alice", &song).await,
        IsrcOwnership::Owned("roads".into())
    );
    let plain = track("Portishead", "Roads", Some(305), None);
    assert_eq!(
        matcher.owned_by_isrc("alice", &plain).await,
        IsrcOwnership::Missing,
        "no ISRC, nothing to look by"
    );
    let busy = matcher_over(vec![]).with_isrc_recordings(Arc::new(|_isrc: String| Box::pin(async { None })));
    assert_eq!(
        busy.owned_by_isrc("alice", &song).await,
        IsrcOwnership::Unanswered,
        "the music database could not be asked"
    );
}

#[test]
fn the_answers_file_has_one_writer_at_a_time() {
    let dir = tempfile::tempdir().expect("a folder");
    let file = dir.path().join("imports-isrc.json");
    let matcher = Arc::new(matcher_over(vec![]).with_isrc_file(Some(file.clone())));
    matcher.isrc_recordings.lock().insert(
        "GBAAA0000001".into(),
        vec![IsrcRecording {
            id: "rec-1".into(),
            title: "Song".into(),
            seconds: Some(200),
        }],
    );
    let held = matcher.saving.lock();
    let (started, running) = std::sync::mpsc::channel();
    let writer = {
        let matcher = Arc::clone(&matcher);
        std::thread::spawn(move || {
            started.send(()).expect("the test is listening");
            matcher.keep_isrc_answers()
        })
    };
    running.recv().expect("the writer started");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!file.exists(), "the writer waits for the gate");
    drop(held);
    writer.join().expect("the writer");
    assert!(file.exists());
}

#[tokio::test]
async fn a_code_the_library_already_answers_holds_nothing() {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let live = coded(r("live", "Portishead", "Roads (Live)", 305), "GBAAA9400010");
    let matcher = matcher_over(vec![live]).with_isrc_recordings(Arc::new(move |_isrc: String| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { None })
    }));
    let song = with_isrc(track("Portishead", "Roads", Some(305), None), "GBAAA9400010");
    assert_eq!(
        matcher.owned_by_isrc("alice", &song).await,
        IsrcOwnership::Missing
    );
    assert_eq!(asked.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn answers_for_isrcs_no_list_holds_are_forgotten_and_empty_ones_are_not_kept_on_disk() {
    let dir = tempfile::tempdir().expect("a folder");
    let file = dir.path().join("imports-isrc.json");
    let matcher = matcher_over(vec![]).with_isrc_file(Some(file.clone()));
    matcher.isrc_recordings.lock().insert(
        "GBAAA0000001".into(),
        vec![IsrcRecording {
            id: "rec-1".into(),
            title: "Kept".into(),
            seconds: Some(200),
        }],
    );
    matcher.isrc_recordings.lock().insert(
        "GBAAA0000002".into(),
        vec![IsrcRecording {
            id: "rec-2".into(),
            title: "Gone".into(),
            seconds: Some(200),
        }],
    );
    matcher
        .isrc_recordings
        .lock()
        .insert("GBAAA0000003".into(), Vec::new());
    matcher.keep_only(&HashSet::from([
        "GBAAA0000001".to_string(),
        "GBAAA0000003".to_string(),
    ]));
    let reloaded = matcher_over(vec![]).with_isrc_file(Some(file));
    let kept = reloaded.isrc_recordings.lock();
    assert!(kept.contains_key("GBAAA0000001"));
    assert!(!kept.contains_key("GBAAA0000002"), "its list is gone");
    assert!(
        !kept.contains_key("GBAAA0000003"),
        "an empty answer stays in memory only"
    );
}

#[tokio::test]
async fn with_the_libraries_apart_a_code_another_members_file_carries_is_still_asked() {
    use crate::services::family::library_index::scope_for_tests;
    let carried = coded(r("theirs", "Portishead", "Roads", 305), "GBAAA9400010");
    let rows = vec![with_isrc(
        track("Portishead", "Roads", Some(305), None),
        "GBAAA9400010",
    )];

    let apart = matcher_over(vec![carried.clone()])
        .with_library_scope(scope_for_tests(true, &[("alex", 3), ("sam", 4)]))
        .with_isrc_recordings(answering(Vec::new()));
    assert_eq!(apart.learn_isrcs(&rows).await.settled, 1);

    let together = matcher_over(vec![carried])
        .with_library_scope(scope_for_tests(false, &[("alex", 3), ("sam", 4)]))
        .with_isrc_recordings(answering(Vec::new()));
    assert_eq!(
        together.learn_isrcs(&rows).await.settled,
        0,
        "the library's own copy answers it"
    );
}

#[tokio::test]
async fn musicbrainz_is_out_from_its_first_unanswered_question_to_its_next_answer() {
    let answers = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let on = answers.clone();
    let matcher = matcher_over(vec![]).with_isrc_recordings(Arc::new(move |_isrc: String| {
        let answer = on.load(Ordering::SeqCst).then(Vec::new);
        Box::pin(async move { answer })
    }));
    let waiting = with_isrc(track("Portishead", "Roads", Some(305), None), "GBAAA9400010");
    let plain = track("Portishead", "Glory Box", Some(301), None);
    let songs = vec![waiting.clone(), plain];
    assert!(!matcher.music_database_out());
    assert!(
        matcher.waiting_on_music_database(&songs).is_empty(),
        "nothing waits while it answers"
    );

    matcher.learn_isrcs(&songs).await;
    assert!(matcher.music_database_out());
    assert_eq!(
        matcher.waiting_on_music_database(&songs),
        HashSet::from([0]),
        "only the song with a code to check waits"
    );

    answers.store(true, Ordering::SeqCst);
    matcher.learn_isrcs(&songs).await;
    assert!(!matcher.music_database_out(), "its first answer ends the hold");
    assert!(matcher.waiting_on_music_database(&songs).is_empty());
}

#[tokio::test]
async fn holding_names_songs_by_place_so_a_shared_key_without_an_isrc_goes() {
    let matcher =
        matcher_over(vec![]).with_isrc_recordings(Arc::new(|_isrc: String| Box::pin(async { None })));
    let coded = with_isrc(track("Portishead", "Roads", Some(305), None), "GBAAA9400010");
    let plain = track("Portishead", "Roads", Some(305), None);
    assert_eq!(coded.key, plain.key, "text and CSV lists share word keys");
    matcher.learn_isrcs(std::slice::from_ref(&coded)).await;
    assert_eq!(
        matcher.waiting_on_music_database(&[coded, plain]),
        HashSet::from([0])
    );
}

#[tokio::test]
async fn working_out_the_hold_never_reads_the_library() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    let matcher = ImportMatcher::with_pages(Arc::new(move |_start, _count| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { None })
    }))
    .with_isrc_recordings(Arc::new(|_isrc: String| Box::pin(async { None })));
    let coded = with_isrc(track("Portishead", "Roads", Some(305), None), "GBAAA9400010");
    matcher.learn_isrcs(std::slice::from_ref(&coded)).await;
    assert!(matcher.music_database_out());
    let before = reads.load(Ordering::SeqCst);
    for _ in 0..2 {
        assert_eq!(
            matcher.waiting_on_music_database(std::slice::from_ref(&coded)),
            HashSet::from([0])
        );
    }
    assert_eq!(reads.load(Ordering::SeqCst), before, "no library read");
}

#[tokio::test]
async fn a_forget_keeps_an_answer_that_came_in_for_an_isrc_that_had_failed() {
    let answers = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let on = answers.clone();
    let matcher = matcher_over(vec![]).with_isrc_recordings(Arc::new(move |_isrc: String| {
        let answer = on.load(Ordering::SeqCst).then(Vec::new);
        Box::pin(async move { answer })
    }));
    let coded = with_isrc(track("Portishead", "Roads", Some(305), None), "GBAAA9400010");
    matcher.learn_isrcs(std::slice::from_ref(&coded)).await;
    let known = matcher.known_isrcs();
    assert!(known.failed.contains("GBAAA9400010"));
    answers.store(true, Ordering::SeqCst);
    matcher.learn_isrcs(std::slice::from_ref(&coded)).await;
    assert!(matcher.knows_isrc_for_tests("GBAAA9400010"));
    matcher.forget_unheld(&known, &HashSet::new());
    assert!(
        matcher.knows_isrc_for_tests("GBAAA9400010"),
        "the answer came in after the look"
    );
    matcher.forget_unheld(&matcher.known_isrcs(), &HashSet::new());
    assert!(
        !matcher.knows_isrc_for_tests("GBAAA9400010"),
        "a later look sees it and nothing holds it"
    );
}

#[tokio::test]
async fn learning_reads_an_unreadable_library_again_only_after_an_answer() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    let matcher = ImportMatcher::with_pages(Arc::new(move |_start, _count| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { None::<(Vec<LibrarySongRow>, usize)> })
    }))
    .with_isrc_recordings(answering(Vec::new()));
    let a = with_isrc(track("Portishead", "Roads", Some(305), None), "GBAAA9400010");
    let b = with_isrc(track("Portishead", "Glory Box", Some(301), None), "GBAAA9400011");
    matcher.learn_isrcs(std::slice::from_ref(&a)).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "the first round reads the library"
    );
    matcher.learn_isrcs(std::slice::from_ref(&b)).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        2,
        "a round that learned something reads again, once"
    );
    matcher.learn_isrcs(std::slice::from_ref(&b)).await;
    assert_eq!(reads.load(Ordering::SeqCst), 2, "nothing learned, no read");
}
