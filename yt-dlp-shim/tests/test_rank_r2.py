"""Candidates are judged script by script, a wrong first hit is ranked again, answers carry the
ranking version, and background callers can rank without resolving or caching."""
import json
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app_module = pytest.importorskip("app")

client = app_module.app.test_client()

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

YOASOBI_QUERY = "YOASOBI Racing Into The Night"


def first(candidates, q, hint):
    return app_module._rank_by_length(candidates, q, hint)[0]["id"]


def _clear_caches():
    with app_module._SEARCH_CACHE_LOCK:
        app_module._SEARCH_CACHE.clear()
    with app_module._META_CACHE_LOCK:
        app_module._META_CACHE.clear()


@pytest.fixture
def runs(monkeypatch):
    """Answers a yt-dlp search by its first argument's prefix and records every run's arguments."""
    _clear_caches()
    monkeypatch.setattr(app_module, "_PLAYER_CLIENT", "")
    state = {"one": [], "five": list(KILLAMONJARO), "args": []}

    def fake_run(args, *a, **k):
        state["args"].append(list(args))
        rows = state["one"] if str(args[0]).startswith("ytsearch1:") else state["five"]
        return "\n".join(json.dumps(r) for r in rows)

    monkeypatch.setattr(app_module, "_run", fake_run)
    monkeypatch.setattr(app_module, "_resolve_url", lambda *a, **k: None)
    monkeypatch.setattr(app_module, "_with_facts", lambda p: p)
    yield state
    _clear_caches()


def test_a_native_title_on_the_artists_channel_beats_a_translated_upload():
    candidates = [
        {"id": "cover", "title": "YOASOBI - Racing Into The Night (English Cover)", "duration": 262,
         "channel": "Someone"},
        {"id": "lyrics", "title": "YOASOBI - Racing Into The Night (Lyrics)", "duration": 262,
         "channel": "lyricsworld"},
        {"id": "official", "title": "YOASOBI「夜に駆ける」 Official Music Video", "duration": 261,
         "channel": "Ayase / YOASOBI"},
    ]
    assert first(candidates, YOASOBI_QUERY, 261) == "official"


def test_a_topic_upload_with_a_native_title_wins():
    candidates = [
        {"id": "lyrics", "title": "YOASOBI - Racing Into The Night (Lyrics)", "duration": 262,
         "channel": "lyricsworld"},
        {"id": "topic", "title": "夜に駆ける", "duration": 261, "channel": "YOASOBI - Topic"},
    ]
    assert first(candidates, YOASOBI_QUERY, 261) == "topic"


def test_digits_are_always_judged():
    candidates = [
        {"id": "corvette", "title": "Prince - Little Red Corvette", "duration": 303, "channel": "Prince"},
        {"id": "1999", "title": "Prince - 1999", "duration": 379, "channel": "Prince"},
    ]
    assert first(candidates, "Prince 1999", 303) == "1999"


def test_a_bengali_query_judges_its_words():
    candidates = [
        {"id": "other", "title": "আরফিন রুমি - আমি", "duration": 180, "channel": "আরফিন রুমি"},
        {"id": "match", "title": "আরফিন রুমি - তুমি", "duration": 200, "channel": "আরফিন রুমি"},
    ]
    assert first(candidates, "আরফিন রুমি তুমি", 180) == "match"


@pytest.mark.parametrize("hint", [179, 171, 161, 153, 147])
def test_an_annotated_fan_upload_never_ties_the_artists_video(hint):
    candidates = KILLAMONJARO + [
        {"id": "fan", "title": "SUMMER CEM x BILLA JOE - KILLY MANJARO (перевод)", "duration": hint,
         "channel": "fan"},
    ]
    assert first(candidates, "KILLY Killamonjaro", hint) == "killy"


def test_a_wrong_first_hit_falls_through(runs):
    runs["one"] = [KILLAMONJARO[1]]
    answer = client.get("/search?q=KILLY Killamonjaro").get_json()
    assert answer["video_id"] == "killy"
    assert answer["rank"] == 3
    assert [a[0] for a in runs["args"]] == ["ytsearch1:KILLY Killamonjaro", "ytsearch5:KILLY Killamonjaro"]


def test_a_right_first_hit_stays_one_run(runs):
    runs["one"] = [KILLAMONJARO[0]]
    answer = client.get("/search?q=KILLY Killamonjaro").get_json()
    assert answer["video_id"] == "killy"
    assert len(runs["args"]) == 1


def test_rank_only_background_search_resolves_nothing_and_caches_nothing(runs, monkeypatch):
    def fail(*a, **k):
        pytest.fail("a rank-only search resolved a URL")

    monkeypatch.setattr(app_module, "_resolve_url", fail)
    before = dict(app_module._SEARCH_CACHE)
    answer = client.get("/search?q=KILLY Killamonjaro&bg=1&resolve=0").get_json()
    assert answer["video_id"] == "killy"
    assert answer["rank"] == 3
    assert dict(app_module._SEARCH_CACHE) == before


def test_meta_v2_rows_are_searched_again(runs):
    app_module._meta_db_put("v2|massive attack angel", {"video_id": "old", "title": "x", "duration": 1})
    runs["one"] = [{"id": "topic", "title": "Angel", "duration": 379, "channel": "Massive Attack - Topic"}]
    answer = client.get("/meta?q=Massive Attack Angel").get_json()
    assert answer["video_id"] == "topic"
    assert answer["rank"] == 3


def test_background_ytm_search_never_fills_the_cache(monkeypatch):
    monkeypatch.setattr(app_module.ytm, "search", lambda q, f, n: [{"videoId": "x"}])
    before = dict(app_module._YTM_CACHE["search"])
    answer = client.get("/ytm/search?q=bg-only-query&bg=1")
    assert answer.status_code == 200
    assert dict(app_module._YTM_CACHE["search"]) == before
    assert app_module._YTM_BG_GATE.acquire(blocking=False)
    try:
        busy = client.get("/ytm/search?q=another-bg-query&bg=1")
        assert busy.status_code == 503
        assert busy.get_json() == {"error": "busy"}
    finally:
        app_module._YTM_BG_GATE.release()
