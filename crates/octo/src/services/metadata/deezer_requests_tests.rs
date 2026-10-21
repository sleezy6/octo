use super::*;

#[test]
fn an_unmarked_request_counts_by_what_it_asked_for() {
    assert_eq!(DeezerUse::of_path("/artist/27/top"), DeezerUse::ArtistPages);
    assert_eq!(DeezerUse::of_path("/search/artist"), DeezerUse::ArtistPages);
    assert_eq!(DeezerUse::of_path("/album/302127"), DeezerUse::AlbumPages);
    assert_eq!(DeezerUse::of_path("/search/album"), DeezerUse::AlbumPages);
    assert_eq!(DeezerUse::of_path("/search/track"), DeezerUse::SongLookups);
    assert_eq!(DeezerUse::of_path("/search"), DeezerUse::SongLookups);
    assert_eq!(
        DeezerUse::of_path("/track/isrc:USUM71703861"),
        DeezerUse::SongLookups
    );
    assert_eq!(DeezerUse::of_path("/chart/0/tracks"), DeezerUse::Other);
}

#[tokio::test]
async fn a_mark_holds_inside_its_work_and_is_carried_into_a_spawned_task() {
    assert_eq!(DeezerUse::current(), None);
    let seen = DeezerUse::Search
        .scope(async {
            let inner = DeezerUse::Lengths.scope(async { DeezerUse::current() }).await;
            let spawned = tokio::spawn(DeezerUse::carry(async { DeezerUse::current() }))
                .await
                .expect("the task ends");
            let bare = tokio::spawn(async { DeezerUse::current() }).await.expect("ends");
            (DeezerUse::current(), inner, spawned, bare)
        })
        .await;
    assert_eq!(
        seen,
        (
            Some(DeezerUse::Search),
            Some(DeezerUse::Lengths),
            Some(DeezerUse::Search),
            None
        )
    );
}

#[test]
fn counts_add_up_by_purpose_and_lane() {
    let counter = DeezerRequestCounter::new();
    counter.count(DeezerUse::Search, false);
    counter.count(DeezerUse::Search, false);
    counter.count(DeezerUse::Lengths, true);
    counter.turned_away();
    counter.search_skipped();
    let report = counter.report();
    assert_eq!(report.last_hour, report.since_start);
    let hour = report.last_hour;
    assert_eq!(hour.total, 3);
    assert_eq!(hour.turned_away, 1);
    assert_eq!(hour.searches_skipped, 1);
    let search = hour
        .uses
        .iter()
        .find(|u| u.purpose == DeezerUse::Search)
        .expect("listed");
    assert_eq!((search.interactive, search.background), (2, 0));
    let lengths = hour
        .uses
        .iter()
        .find(|u| u.purpose == DeezerUse::Lengths)
        .expect("listed");
    assert_eq!((lengths.interactive, lengths.background), (0, 1));
    assert_eq!(
        log_line(&hour),
        "Deezer requests in the last hour: 3 (search 2, lengths 1 (1 in the background)); \
         1 turned away by the rate limit; 1 plays and hearts needed no search"
    );
}

#[test]
fn the_last_hour_leaves_out_older_minutes() {
    let mut ring = [Minute::default(); MINUTES_KEPT];
    ring[5] = Minute {
        at: 5,
        turned_away: 1,
        ..Minute::default()
    };
    ring[6] = Minute {
        at: 66,
        turned_away: 2,
        ..Minute::default()
    };
    assert_eq!(hour_of(&ring, 66).turned_away, 2);
    assert_eq!(hour_of(&ring, 60).turned_away, 1);
}
