"""Writes crates/octo/src/services/common/lastfm_country_names.rs from Last.fm's geo charts.

For every Apple Music country code it asks geo.gettoptracks for one track, first with the ISO 3166-1
short name and then with the common English name. It keeps the first name whose answer names that
same country and lists a track. The key is read from LASTFM_API_KEY and is never printed or
written. Prints the codes Last.fm has no chart for.
"""

import json
import os
import pathlib
import re
import sys
import time
import urllib.parse
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = ROOT / "crates/octo/src/services/common/lastfm_country_names.rs"
SETTINGS = ROOT / "crates/octo-core/src/charts/chart_settings.rs"
NAMES = ROOT / "crates/octo-core/src/common/countries.rs"
API = "https://ws.audioscrobbler.com/2.0/"


def apple_countries():
    text = SETTINGS.read_text(encoding="utf-8")
    block = re.search(r"pub const APPLE_COUNTRIES: &\[&str\] = &\[(.*?)\];", text, re.S)
    return re.findall(r'"([a-z]{2})"', block.group(1))


def english_names():
    """Code to the names to try for it: the ISO 3166-1 short name when pycountry is installed,
    then the common English name this repo uses."""
    common = {
        code.lower(): name
        for code, name in re.findall(r'\("([A-Z]{2})", "([^"]+)"\)', NAMES.read_text(encoding="utf-8"))
    }
    try:
        import pycountry
    except ImportError:
        pycountry = None
    names = {}
    for code in apple_countries():
        tried = []
        if pycountry is not None:
            entry = pycountry.countries.get(alpha_2=code.upper())
            if entry is not None:
                tried.append(entry.name)
        if code in common:
            tried.append(common[code])
        names[code] = list(dict.fromkeys(tried))
    return names


def ask(key, name):
    query = urllib.parse.urlencode(
        {
            "method": "geo.gettoptracks",
            "country": name,
            "limit": 1,
            "format": "json",
            "api_key": key,
        }
    )
    try:
        with urllib.request.urlopen(f"{API}?{query}", timeout=30) as response:
            doc = json.load(response)
    except Exception:
        return None
    tracks = doc.get("tracks") or {}
    answered = (tracks.get("@attr") or {}).get("country", "")
    listed = tracks.get("track")
    if not listed or answered.lower() != name.lower():
        return None
    return name


def main():
    key = os.environ.get("LASTFM_API_KEY", "").strip()
    if not key:
        sys.exit("Set LASTFM_API_KEY first.")
    known = english_names()
    found = []
    missing = []
    for code in apple_countries():
        chosen = None
        for name in known.get(code, []):
            chosen = ask(key, name)
            time.sleep(0.25)
            if chosen:
                break
        if chosen:
            found.append((code, chosen))
        else:
            missing.append(code)
    found.sort()
    lines = [
        "//! Last.fm's own name for each Apple Music country whose geo chart it has, by code.",
        "//! Written by tools/lastfm-geo/probe.py.",
        "",
        "pub const LASTFM_COUNTRY_NAMES: &[(&str, &str)] = &[",
    ]
    lines += [f'    ("{code}", "{name}"),' for code, name in found]
    lines.append("];")
    OUT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"{len(found)} countries have a Last.fm chart.")
    print("No chart: " + (", ".join(missing) if missing else "none"))


if __name__ == "__main__":
    main()
