# Last.fm geo chart names

Last.fm's `geo.gettoptracks` takes a country by name, and not every Apple Music country has a chart there.

Run from the repo root on the server host, with the Last.fm key in the environment:

```text
LASTFM_API_KEY=... python tools/lastfm-geo/probe.py
```

For each Apple Music country code it asks for one track, first with the ISO 3166-1 short name (when `pycountry` is installed), then with the common English name. It keeps the first name whose answer names that country and lists a track, rewrites `crates/octo/src/services/common/lastfm_country_names.rs`, and prints the codes Last.fm has no chart for.

The key is only read from the environment; it is never printed or written. Commit the regenerated file.
