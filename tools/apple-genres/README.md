# Apple genre names

Run `python tools/apple-genres/make_table.py` from the repo root; it needs network and rewrites `crates/octo-core/src/charts/apple_genre_names.rs`.
Run it when the server logs that it learned a new genre name, then commit the regenerated file.
