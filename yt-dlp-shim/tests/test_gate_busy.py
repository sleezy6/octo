"""A request the yt-dlp gate dropped is answered 503 {"error":"busy"}; any other failed run keeps 502."""
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app = pytest.importorskip("app")


@pytest.fixture
def full_background_gate(monkeypatch):
    """Every background slot held, and the wait for one cut to 0.05 s."""
    monkeypatch.setattr(app, "_BG_GATE_WAIT_SEC", 0.05)
    held = 0
    while app._GATE.acquire(True, 0.05):
        held += 1
    try:
        yield held
    finally:
        for _ in range(held):
            app._GATE.release(True)


def test_a_full_gate_answers_stream_busy(full_background_gate):
    assert full_background_gate > 0
    answer = app.app.test_client().get("/stream?id=busyvid0001&bg=1")
    assert answer.status_code == 503
    assert answer.get_json() == {"error": "busy"}


def test_a_full_gate_answers_facts_busy(full_background_gate):
    answer = app.app.test_client().get("/facts?id=busyvid0002&bg=1")
    assert answer.status_code == 503
    assert answer.get_json() == {"error": "busy"}


def test_a_full_gate_answers_search_busy(full_background_gate):
    answer = app.app.test_client().get("/search?q=busy+gate+song+one&bg=1")
    assert answer.status_code == 503
    assert answer.get_json() == {"error": "busy"}


def test_a_failed_run_is_still_a_502(monkeypatch):
    monkeypatch.setattr(app, "_run", lambda *a, **k: None)
    answer = app.app.test_client().get("/stream?id=busyvid0003&bg=1")
    assert answer.status_code == 502
    assert answer.get_json() == {"error": "no_audio_url"}
