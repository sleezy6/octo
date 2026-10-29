"""/search can leave videos out, and /meta reads the channel and re-asks lookups an older ranking answered."""
import json
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app_module = pytest.importorskip("app")

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

TEARDROP = [
    {"id": "topic", "title": "Teardrop", "duration": 320, "channel": "Massive Attack - Topic"},
    {"id": "reup", "title": "Teardrop (Lyrics)", "duration": 330, "channel": "lyricsworld"},
]

client = app_module.app.test_client()


def _clear_caches():
    with app_module._SEARCH_CACHE_LOCK:
        app_module._SEARCH_CACHE.clear()
    with app_module._META_CACHE_LOCK:
        app_module._META_CACHE.clear()


@pytest.fixture
def runs(monkeypatch):
    """Answers every yt-dlp search with `rows` and records the arguments it was run with."""
    _clear_caches()
    state = {"rows": list(KILLAMONJARO), "args": []}

    def fake_run(args, *a, **k):
        state["args"].append(list(args))
        return "\n".join(json.dumps(r) for r in state["rows"])

    monkeypatch.setattr(app_module, "_run", fake_run)
    monkeypatch.setattr(app_module, "_resolve_url", lambda *a, **k: None)
    monkeypatch.setattr(app_module, "_with_facts", lambda p: p)
    yield state
    _clear_caches()


def test_a_skipped_video_is_left_out(runs):
    assert client.get("/search?q=KILLY Killamonjaro&duration=179").get_json()["video_id"] == "killy"
    skipped = client.get("/search?q=KILLY Killamonjaro&duration=179&skip=killy").get_json()
    assert skipped["video_id"] != "killy"
    assert client.get("/search?q=KILLY Killamonjaro&duration=179").get_json()["video_id"] == "killy"


def test_skip_without_a_length_still_ranks(runs):
    answer = client.get("/search?q=KILLY Killamonjaro&skip=summer-cem").get_json()
    assert answer["video_id"] == "killy"
    assert any("ytsearch5:KILLY Killamonjaro" in args for args in runs["args"])


def test_meta_reads_the_channel(runs):
    runs["rows"] = list(TEARDROP)
    answer = client.get("/meta?q=Massive Attack Teardrop&duration=330").get_json()
    assert answer["video_id"] == "topic"
    assert any("%(.{id,title,duration,channel})j" in args for args in runs["args"])


def test_meta_searches_again_for_a_lookup_an_older_ranking_answered(runs):
    runs["rows"] = list(TEARDROP)
    app_module._meta_db_put("v2|massive attack teardrop", {"video_id": "old", "title": "x", "duration": 1})
    answer = client.get("/meta?q=Massive Attack Teardrop&duration=330").get_json()
    assert answer["video_id"] == "topic"
