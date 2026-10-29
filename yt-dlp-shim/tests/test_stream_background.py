"""/stream resolves in the background lane only when the caller says bg=1."""
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

app = pytest.importorskip("app")


class FakeUpstream:
    status_code = 200
    headers = {"Content-Type": "audio/mp4", "Content-Range": "bytes 0-3/4"}

    def iter_content(self, chunk_size):
        yield b"abcd"

    def close(self):
        pass


def lanes_asked(monkeypatch, query):
    asked = []

    def resolve(video_id, bg=False):
        asked.append(bg)
        return "https://googlevideo.example/x"

    monkeypatch.setattr(app, "_resolve_url", resolve)
    monkeypatch.setattr(app, "_open_upstream", lambda url, headers, video_id: FakeUpstream())
    answer = app.app.test_client().get(f"/stream?id=vid-lane{query}")
    answer.get_data()
    return asked


def test_a_play_resolves_in_the_foreground(monkeypatch):
    assert lanes_asked(monkeypatch, "") == [False]


def test_bg_resolves_in_the_background(monkeypatch):
    assert lanes_asked(monkeypatch, "&bg=1") == [True]
