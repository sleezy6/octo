//! Kid profiles (`CleanOnly`): explicit songs are left out of their answers, and lists Octo
//! builds for them are built clean.
//!
//! A library song's mark is read from the [`LibraryIndex`], never from the answer: Navidrome
//! leaves `explicitStatus` out of its answers to older clients (DSub), and the index has it
//! for every client. A song the index does not know (an outside song, or one scanned since the
//! index was last read) is judged by the `explicitStatus` Octo or Navidrome wrote, which for
//! outside songs is the catalog's own mark. A song with no mark stays unless
//! `Family:KidUnratedIsExplicit` is on. Album lists are left alone: an album with one explicit
//! song still holds clean ones.

use bytes::Bytes;
use octo_core::models::domain::explicit_status::{CLEAN_WORD, EXPLICIT_WORD};
use octo_core::settings::{AppSettings, ExplicitFilter};

use super::caller::FamilyCaller;
use super::family_store::FamilyStore;
use super::library_index::{LibraryIndex, LibraryScope, ScopeResolver};
use super::list_rules::listed;
use super::rules::AnswerContext;
use crate::app::AppState;

/// The endpoints whose songs a kid profile never sees explicit. `getSong` of an explicit song
/// answers error 70.
pub const KID_ENDPOINTS: &[&str] = &[
    "search2",
    "search3",
    "getAlbum",
    "getMusicDirectory",
    "getPlaylist",
    "getRandomSongs",
    "getSongsByGenre",
    "getSimilarSongs",
    "getSimilarSongs2",
    "getTopSongs",
    "getStarred",
    "getStarred2",
    "getPlayQueue",
    "getSong",
];

/// The message of the error 70 a kid profile gets for an explicit song.
pub const NOT_FOUND: &str = "Song not found";

/// Whether the kid filter applies to this caller's answer from `endpoint`.
pub fn applies(caller: &FamilyCaller, endpoint: &str) -> bool {
    is_kid(caller) && listed(KID_ENDPOINTS, endpoint)
}

/// Whether the caller is a member who hears only clean songs.
pub fn is_kid(caller: &FamilyCaller) -> bool {
    caller.managed && caller.abilities.clean_only
}

/// Whether a song is explicit for a kid profile: its mark in the index when the index knows
/// it, else the mark the answer wrote. An unmarked song counts as explicit only when
/// `unrated_is_explicit`.
pub fn is_explicit(indexed: Option<&str>, answered: Option<&str>, unrated_is_explicit: bool) -> bool {
    let mark = indexed.or(answered).unwrap_or("").trim();
    if mark.eq_ignore_ascii_case(EXPLICIT_WORD) {
        return true;
    }
    if mark.eq_ignore_ascii_case(CLEAN_WORD) {
        return false;
    }
    unrated_is_explicit
}

/// [`is_explicit`] for a song id, reading its mark from `index`.
pub fn song_is_explicit(
    index: &LibraryIndex,
    id: &str,
    answered: Option<&str>,
    unrated_is_explicit: bool,
) -> bool {
    let indexed = index.song(id);
    is_explicit(
        indexed.as_ref().map(|song| song.explicit_status.as_str()),
        answered,
        unrated_is_explicit,
    )
}

/// The error 70 that stands for an explicit song, in the format nswer is written in.
pub(crate) fn not_found(state: &AppState, context: &AnswerContext<'_>, answer: &[u8]) -> Bytes {
    let shape = super::list_rules::shape_of(answer, context.content_type, context.parameters);
    super::list_rules::error_in(state, &shape, 70, NOT_FOUND)
}

/// The explicit filter for lists Octo builds for `user` (radio, stations, mixes, Made for you,
/// charts): clean only for a kid profile while Family is on, else the listener's own choice
/// when they made one, else the server's setting.
pub fn explicit_filter_for(
    settings: &AppSettings,
    family: Option<&FamilyStore>,
    user: Option<&str>,
) -> ExplicitFilter {
    let global = settings.subsonic.explicit_filter;
    let (Some(store), Some(user)) = (family, user.filter(|u| !u.trim().is_empty())) else {
        return global;
    };
    let kid = settings.family.enabled
        && store
            .member(user)
            .is_some_and(|member| member.abilities().clean_only);
    if kid {
        return ExplicitFilter::CleanOnly;
    }
    store.explicit_choice(user).unwrap_or(global)
}

/// What a caller's answers (search, albums, playlists and the rest of [`KID_ENDPOINTS`]) leave
/// out, while Family is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerFilter {
    /// Explicit songs: a kid profile, or a listener who chose clean only. An unmarked song
    /// counts as explicit when `unrated_is_explicit` (kid profiles only, by
    /// `Family:KidUnratedIsExplicit`).
    Clean { unrated_is_explicit: bool },
    /// Clean edits: a listener who chose explicit versions.
    NoEdits,
}

impl AnswerFilter {
    /// Whether a song with these marks leaves the answer.
    pub fn drops(self, indexed: Option<&str>, answered: Option<&str>) -> bool {
        match self {
            AnswerFilter::Clean { unrated_is_explicit } => {
                is_explicit(indexed, answered, unrated_is_explicit)
            }
            AnswerFilter::NoEdits => indexed
                .or(answered)
                .is_some_and(|mark| mark.trim().eq_ignore_ascii_case(CLEAN_WORD)),
        }
    }
}

/// The answer filter for this caller's answer from `endpoint`, if any: a kid profile's, else
/// the listener's own choice.
pub fn answer_filter(
    store: &FamilyStore,
    caller: &FamilyCaller,
    endpoint: &str,
    kid_unrated_is_explicit: bool,
) -> Option<AnswerFilter> {
    if !listed(KID_ENDPOINTS, endpoint) {
        return None;
    }
    if is_kid(caller) {
        return Some(AnswerFilter::Clean {
            unrated_is_explicit: kid_unrated_is_explicit,
        });
    }
    match store.explicit_choice(&caller.user)? {
        ExplicitFilter::CleanOnly => Some(AnswerFilter::Clean {
            unrated_is_explicit: false,
        }),
        ExplicitFilter::ExplicitOnly => Some(AnswerFilter::NoEdits),
        ExplicitFilter::All => None,
    }
}

/// The explicit filter for lists Octo builds for the caller of a request.
pub fn caller_explicit_filter(caller: Option<&FamilyCaller>, global: ExplicitFilter) -> ExplicitFilter {
    match caller {
        Some(caller) if is_kid(caller) => ExplicitFilter::CleanOnly,
        _ => global,
    }
}

/// How many shared-library songs have no explicit mark, for the Family page's "Mark them now".
/// Members' own libraries are not counted. Zero until the index has been read.
pub fn unrated_count(index: &LibraryIndex, scope: &ScopeResolver) -> usize {
    let view = scope.shared_view();
    let mut count = 0;
    index.each_song(|song| {
        let shared = song
            .library_id
            .is_none_or(|library| view.allows_library(&LibraryScope::Shared, library));
        if shared && song.explicit_status.trim().is_empty() {
            count += 1;
        }
    });
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use octo_core::family::abilities::{Abilities, Preset};

    #[test]
    fn a_listeners_own_choice_shapes_their_lists_and_answers_but_a_kid_stays_clean() {
        let store = FamilyStore::in_memory();
        let mut settings = AppSettings::default();
        settings.family.enabled = true;
        settings.subsonic.explicit_filter = ExplicitFilter::All;
        let mut kid = crate::services::family::member_credentials::tests::member("kid", String::new());
        kid.preset = Preset::Kid;
        store.upsert_member(kid);
        store.set_explicit_choice("sam", Some(ExplicitFilter::CleanOnly));
        store.set_explicit_choice("KID", Some(ExplicitFilter::ExplicitOnly));
        store.set_explicit_choice("lee", Some(ExplicitFilter::ExplicitOnly));
        assert_eq!(
            explicit_filter_for(&settings, Some(&store), Some("Sam")),
            ExplicitFilter::CleanOnly
        );
        assert_eq!(
            explicit_filter_for(&settings, Some(&store), Some("kid")),
            ExplicitFilter::CleanOnly
        );
        assert_eq!(
            explicit_filter_for(&settings, Some(&store), Some("lee")),
            ExplicitFilter::ExplicitOnly
        );
        assert_eq!(
            explicit_filter_for(&settings, Some(&store), Some("pat")),
            ExplicitFilter::All
        );
        store.set_explicit_choice("sam", None);
        assert_eq!(
            explicit_filter_for(&settings, Some(&store), Some("sam")),
            ExplicitFilter::All
        );

        let lee = FamilyCaller::unmanaged("lee", false);
        let filter = answer_filter(&store, &lee, "search3", false).expect("lee chose");
        assert_eq!(filter, AnswerFilter::NoEdits);
        assert!(filter.drops(None, Some("clean")));
        assert!(!filter.drops(None, Some("explicit")));
        assert!(!filter.drops(None, None));
        assert_eq!(
            answer_filter(&store, &lee, "getArtists", false),
            None,
            "not a song list"
        );
        store.set_explicit_choice("lee", Some(ExplicitFilter::CleanOnly));
        let clean = answer_filter(&store, &lee, "getAlbum", true).expect("lee chose");
        assert_eq!(
            clean,
            AnswerFilter::Clean {
                unrated_is_explicit: false
            },
            "only kids lose unmarked songs"
        );
        assert!(clean.drops(Some("explicit"), None));
    }

    #[test]
    fn the_index_mark_wins_over_the_answer() {
        assert!(is_explicit(Some("explicit"), Some(""), false));
        assert!(!is_explicit(Some("clean"), Some("explicit"), false));
        // A legacy client's answer has no mark; the index still has it.
        assert!(is_explicit(Some("explicit"), None, false));
        // Outside songs carry the catalog's mark.
        assert!(is_explicit(None, Some("explicit"), false));
    }

    #[test]
    fn unrated_kept_by_default() {
        assert!(!is_explicit(Some(""), None, false));
        assert!(!is_explicit(None, None, false));
        assert!(is_explicit(Some(""), None, true));
        assert!(!is_explicit(Some("clean"), None, true));
    }

    #[test]
    fn only_kids_get_the_filter() {
        let kid = FamilyCaller::member("kid", Preset::Kid, Abilities::preset(Preset::Kid));
        let listener = FamilyCaller::member("sam", Preset::Listener, Abilities::preset(Preset::Listener));
        let owner = FamilyCaller::unmanaged("winters", true);
        assert!(applies(&kid, "search3"));
        assert!(applies(&kid, "GETSONG"));
        assert!(!applies(&kid, "getAlbumList2"), "album lists are left alone");
        assert!(!applies(&listener, "search3"));
        assert!(!applies(&owner, "search3"));
        assert_eq!(
            caller_explicit_filter(Some(&kid), ExplicitFilter::All),
            ExplicitFilter::CleanOnly
        );
        assert_eq!(
            caller_explicit_filter(Some(&listener), ExplicitFilter::All),
            ExplicitFilter::All
        );
        assert_eq!(
            caller_explicit_filter(None, ExplicitFilter::ExplicitOnly),
            ExplicitFilter::ExplicitOnly
        );
    }

    #[test]
    fn kid_lists_are_built_clean_only_while_family_is_on() {
        let store = FamilyStore::in_memory();
        let mut kid = crate::services::family::member_credentials::tests::member("kid", String::new());
        kid.preset = Preset::Kid;
        store.upsert_member(kid);
        store.upsert_member(crate::services::family::member_credentials::tests::member(
            "sam",
            String::new(),
        ));
        let mut settings = AppSettings::default();
        settings.subsonic.explicit_filter = ExplicitFilter::ExplicitOnly;
        assert_eq!(
            explicit_filter_for(&settings, Some(&store), Some("kid")),
            ExplicitFilter::ExplicitOnly,
            "Family off"
        );
        settings.family.enabled = true;
        assert_eq!(
            explicit_filter_for(&settings, Some(&store), Some("Kid")),
            ExplicitFilter::CleanOnly
        );
        assert_eq!(
            explicit_filter_for(&settings, Some(&store), Some("sam")),
            ExplicitFilter::ExplicitOnly
        );
        assert_eq!(
            explicit_filter_for(&settings, Some(&store), None),
            ExplicitFilter::ExplicitOnly
        );
        assert_eq!(
            explicit_filter_for(&settings, None, Some("kid")),
            ExplicitFilter::ExplicitOnly
        );
    }
}
