"""How search candidates are ranked: the words they name, other takes, extra words, then length."""
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app_module = pytest.importorskip("app")


def ids(ranked):
    return [r["id"] for r in ranked]


def test_a_live_take_never_beats_the_studio_song():
    candidates = [
        {"id": "live", "title": "Silverstein - Smile in Your Sleep (Live at the El Mocambo)", "duration": 209},
        {"id": "studio", "title": "Silverstein - Smile in Your Sleep", "duration": 203},
    ]
    assert ids(app_module._rank_by_length(candidates, "Silverstein - Smile in Your Sleep", 209))[0] == "studio"


def test_a_query_that_asks_for_live_ranks_by_length_alone():
    candidates = [
        {"id": "studio", "title": "Nirvana - About a Girl", "duration": 168},
        {"id": "unplugged", "title": "Nirvana - About a Girl (MTV Unplugged)", "duration": 217},
    ]
    assert ids(app_module._rank_by_length(candidates, "Nirvana - About a Girl (MTV Unplugged)", 217))[0] == "unplugged"


def test_when_every_candidate_is_live_the_closest_still_plays():
    candidates = [
        {"id": "far", "title": "Song (Live)", "duration": 400},
        {"id": "near", "title": "Song (Live in Berlin)", "duration": 201},
    ]
    assert ids(app_module._rank_by_length(candidates, "Artist - Song", 200))[0] == "near"


def test_a_candidate_without_a_length_sorts_last():
    candidates = [
        {"id": "unknown", "title": "Song", "duration": None},
        {"id": "known", "title": "Song", "duration": 250},
    ]
    assert ids(app_module._rank_by_length(candidates, "Artist - Song", 200)) == ["known", "unknown"]


# Five candidates for one query: the artist's own video, three of another artist's song of a like
# title, and a talk about the song. The song's catalog length is 179 s and its video 161 s.
KILLAMONJARO = [
    {"id": "killy", "title": "KILLY - Killamonjaro (Official Music Video)", "duration": 161, "channel": "KILLY"},
    {"id": "summer-cem", "title": "SUMMER CEM x BILLA JOE - KILLY MANJARO (Official Video)",
     "duration": 153, "channel": "Summer Cem"},
    {"id": "genius", "title": "KILLY \"Killamonjaro\" Official Lyrics & Meaning | Verified",
     "duration": 238, "channel": "Genius"},
    {"id": "euromix", "title": "SUMMER CEM x BILLA JOE x DARDAN x EZHEL - KILLY MANJARO EUROMIX (Official Video)",
     "duration": 171, "channel": "Summer Cem and 2 more"},
    {"id": "lyrics", "title": "SUMMER CEM - KILLY MANJARO (Lyrics) ft. BILLA JOE", "duration": 147,
     "channel": "Luna Liebling"},
]


@pytest.mark.parametrize("hint", [179, 171, 161, 153, 147])
def test_another_artists_song_of_a_like_title_never_wins_on_length(hint):
    assert ids(app_module._rank_by_length(KILLAMONJARO, "KILLY - Killamonjaro", hint))[0] == "killy"


def test_among_videos_naming_the_song_the_closest_length_still_wins():
    candidates = [
        {"id": "short", "title": "KILLY - Killamonjaro", "duration": 161, "channel": "KILLY"},
        {"id": "long", "title": "KILLY - Killamonjaro (Audio)", "duration": 175, "channel": "KILLY"},
    ]
    assert ids(app_module._rank_by_length(candidates, "KILLY Killamonjaro", 179))[0] == "long"
    assert ids(app_module._rank_by_length(candidates, "KILLY Killamonjaro", 160))[0] == "short"


def test_the_channel_counts_as_naming_the_artist():
    candidates = [
        {"id": "other", "title": "Teardrop (Cover)", "duration": 330, "channel": "Some Singer"},
        {"id": "own", "title": "Teardrop", "duration": 320, "channel": "Massive Attack - Topic"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Massive Attack Teardrop", 330))[0] == "own"


def test_accents_and_case_do_not_hide_a_word():
    candidates = [
        {"id": "far", "title": "Bjork - Joga", "duration": 400, "channel": "x"},
        {"id": "other", "title": "Someone - Joga", "duration": 305, "channel": "y"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Björk Jóga", 305))[0] == "far"


def test_words_no_video_has_leave_the_length_to_decide():
    candidates = [
        {"id": "far", "title": "残酷な天使のテーゼ", "duration": 300, "channel": "アニメ"},
        {"id": "near", "title": "残酷な天使のテーゼ (TV size)", "duration": 241, "channel": "歌"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Yoko Takahashi A Cruel Angel's Thesis", 240))[0] == "near"


def test_a_featured_artist_asked_for_is_named():
    candidates = [
        {"id": "solo", "title": "Billie Eilish - bad guy", "duration": 194, "channel": "Billie Eilish"},
        {"id": "duet", "title": "Billie Eilish & Justin Bieber - bad guy", "duration": 197,
         "channel": "Billie Eilish"},
    ]
    ranked = app_module._rank_by_length(candidates, "Billie Eilish bad guy (with Justin Bieber)", 195)
    assert ids(ranked)[0] == "duet"


def test_an_edit_asked_for_never_loses_to_a_sped_up_upload():
    candidates = [
        {"id": "plain", "title": "Artist - Song", "duration": 206, "channel": "Artist"},
        {"id": "fast", "title": "Artist - Song - Radio Edit (sped up)", "duration": 200, "channel": "Artist"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Artist Song - Radio Edit", 200))[0] == "plain"


@pytest.mark.parametrize("words", [
    "(Sped-Up)", "(speed up)", "(Nightcore)", "(Karaoke)", "(Cover)", "(Acoustic)", "(Remixed)",
    "(X Remix)", "(Live)", "(10 Hours)",
])
def test_other_takes_never_beat_the_song(words):
    candidates = [
        {"id": "take", "title": f"Artist - Song {words}", "duration": 200, "channel": "Artist"},
        {"id": "song", "title": "Artist - Song", "duration": 206, "channel": "Artist"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Artist Song", 200))[0] == "song"


def test_a_take_word_in_the_title_is_not_held_against_it():
    candidates = [
        {"id": "studio", "title": "Oasis - Live Forever", "duration": 277, "channel": "Oasis"},
        {"id": "live", "title": "Oasis - Live Forever (Live at Knebworth)", "duration": 276, "channel": "Oasis"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Oasis Live Forever", 276))[0] == "studio"
    candidates = [
        {"id": "studio", "title": "Jason Isbell - Cover Me Up", "duration": 324, "channel": "Jason Isbell"},
        {"id": "cover", "title": "Cover Me Up (Cover by Someone)", "duration": 323, "channel": "Someone"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Jason Isbell Cover Me Up", 323))[0] == "studio"


def test_a_take_asked_for_wins():
    candidates = [
        {"id": "remix", "title": "Artist - Song (X Remix)", "duration": 210, "channel": "Artist"},
        {"id": "song", "title": "Artist - Song", "duration": 200, "channel": "Artist"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Artist Song (X Remix)", 200))[0] == "remix"


def test_artists_with_the():
    candidates = [
        {"id": "topic", "title": "Let It Be (Remastered 2009)", "duration": 243, "channel": "The Beatles - Topic"},
        {"id": "fan", "title": "Beatles - Let It Be", "duration": 250, "channel": "oldies"},
    ]
    assert ids(app_module._rank_by_length(candidates, "The Beatles Let It Be", 243))[0] == "topic"


def test_the_topic_channel_names_the_artist():
    candidates = [
        {"id": "topic", "title": "Teardrop", "duration": 320, "channel": "Massive Attack - Topic"},
        {"id": "reup", "title": "Teardrop (Lyrics)", "duration": 330, "channel": "lyricsworld"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Massive Attack Teardrop", 330))[0] == "topic"


def test_no_hint_keeps_youtubes_order():
    candidates = [
        {"id": "a", "title": "Artist - Song", "duration": 300, "channel": "Artist"},
        {"id": "b", "title": "Artist - Song", "duration": 100, "channel": "Artist"},
        {"id": "c", "title": "Artist - Song", "duration": 200, "channel": "Artist"},
    ]
    assert ids(app_module._rank_by_length(candidates, "Artist Song", None)) == ["a", "b", "c"]
