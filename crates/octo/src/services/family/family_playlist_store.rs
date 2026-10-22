//! `family-playlists.json`: playlists every member can add to. Each entry keeps a library song
//! id and an outside id where it has them, so a member who cannot see a song in someone's own
//! library still gets its outside version.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use octo_core::json::datetime;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::family_playlist_covers::{self, CoverRefused};
use super::store_file;

/// One song in a family playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FamilyPlaylistEntry {
    /// The Navidrome song id, when the song is in a library.
    #[serde(default)]
    pub library_id: Option<String>,
    /// Which library the song is in: `Shared`, or `Personal:<username>`.
    #[serde(default)]
    pub scope: String,
    /// The outside id (`ext-...`), when Octo knows one.
    #[serde(default)]
    pub outside_id: Option<String>,
    #[serde(default)]
    pub added_by: String,
    #[serde(with = "datetime::utc")]
    pub added: DateTime<Utc>,
    /// Waiting for an approver (playlists that need approval for additions).
    #[serde(default)]
    pub pending: bool,
    /// The outside version's length in seconds, as the catalog gave it when it was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<i64>,
    /// The outside version's explicit mark (`explicit`, `clean`, or empty for none), as the
    /// catalog gave it when it was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit_status: Option<String>,
}

/// One family playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FamilyPlaylist {
    /// `of` and 20 letters or digits: 22 characters, like Octo's other playlist ids.
    pub id: String,
    pub name: String,
    #[serde(with = "datetime::utc")]
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub approve_additions: bool,
    #[serde(default)]
    pub entries: Vec<FamilyPlaylistEntry>,
    /// The members it is shared with, besides its maker and the owner, who always see it.
    /// Empty: the whole family sees it. Left out of the file while empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<String>,
}

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `of` + 20 base62 characters of SHA-256(name + created).
pub fn family_playlist_id(name: &str, created: &DateTime<Utc>) -> String {
    let digest = Sha256::digest(format!("{name}{}", datetime::format_utc(created)).as_bytes());
    let tail: String = digest
        .iter()
        .take(20)
        .map(|b| char::from(BASE62[usize::from(*b) % BASE62.len()]))
        .collect();
    format!("of{tail}")
}

/// The store behind `family-playlists.json`.
pub struct FamilyPlaylistStore {
    playlists: Mutex<Vec<FamilyPlaylist>>,
    path: Option<PathBuf>,
    /// The covers folder beside the file, where a playlist's own picture is kept.
    covers: Option<PathBuf>,
    save_lock: Mutex<()>,
    load_warning: Option<String>,
}

impl FamilyPlaylistStore {
    pub fn in_memory() -> Self {
        Self::new(None::<PathBuf>)
    }

    pub fn new(path: Option<impl AsRef<Path>>) -> Self {
        let path = store_file::clean_path(path);
        let (playlists, load_warning) = store_file::load::<Vec<FamilyPlaylist>>(path.as_deref());
        let covers = path
            .as_deref()
            .and_then(Path::parent)
            .map(|folder| folder.join("covers"));
        FamilyPlaylistStore {
            playlists: Mutex::new(playlists),
            path,
            covers,
            save_lock: Mutex::new(()),
            load_warning,
        }
    }

    pub fn load_warning(&self) -> Option<&str> {
        self.load_warning.as_deref()
    }

    /// A new, empty playlist.
    pub fn create(&self, name: &str, by: &str, now: DateTime<Utc>) -> FamilyPlaylist {
        let playlist = {
            let mut playlists = self.playlists.lock();
            let mut id = family_playlist_id(name, &now);
            let mut n = 0u32;
            while playlists.iter().any(|p| p.id == id) {
                n += 1;
                id = family_playlist_id(&format!("{name}#{n}"), &now);
            }
            let playlist = FamilyPlaylist {
                id,
                name: name.trim().to_string(),
                created: now,
                created_by: by.to_string(),
                approve_additions: false,
                entries: Vec::new(),
                shared_with: Vec::new(),
            };
            playlists.push(playlist.clone());
            playlist
        };
        self.save();
        playlist
    }

    pub fn get(&self, id: &str) -> Option<FamilyPlaylist> {
        self.playlists.lock().iter().find(|p| p.id == id).cloned()
    }

    pub fn list(&self) -> Vec<FamilyPlaylist> {
        self.playlists.lock().clone()
    }

    pub fn rename(&self, id: &str, name: &str) -> bool {
        self.change(id, |p| p.name = name.trim().to_string())
    }

    /// Who the playlist is shared with; empty shares it with the whole family.
    pub fn set_shared_with(&self, id: &str, people: Vec<String>) -> bool {
        self.change(id, |p| p.shared_with = people)
    }

    pub fn set_approve(&self, id: &str, approve: bool) -> bool {
        self.change(id, |p| p.approve_additions = approve)
    }

    /// Adds entries at the end.
    pub fn add_entries(&self, id: &str, entries: Vec<FamilyPlaylistEntry>) -> bool {
        self.change(id, |p| p.entries.extend(entries))
    }

    /// Removes the entries at these positions (positions past the end are ignored).
    pub fn remove_indexes(&self, id: &str, indexes: &[usize]) -> bool {
        self.change(id, |p| {
            let mut at = 0usize;
            p.entries.retain(|_| {
                let keep = !indexes.contains(&at);
                at += 1;
                keep
            });
        })
    }

    /// Lets a waiting entry in. False when there is no such playlist or entry.
    pub fn approve_entry(&self, id: &str, index: usize) -> bool {
        let mut found = false;
        let changed = self.change(id, |p| {
            if let Some(entry) = p.entries.get_mut(index) {
                entry.pending = false;
                found = true;
            }
        });
        changed && found
    }

    pub fn delete(&self, id: &str) -> bool {
        let removed = {
            let mut playlists = self.playlists.lock();
            let before = playlists.len();
            playlists.retain(|p| p.id != id);
            before != playlists.len()
        };
        if removed {
            self.save();
            self.remove_cover(id);
        }
        removed
    }

    /// Where the playlist's own picture is kept, whether or not it has one: the covers folder,
    /// under its id. None for an id that is not a playlist id, or with no folder.
    pub fn cover_path(&self, id: &str) -> Option<PathBuf> {
        let well_formed =
            id.len() == 22 && id.starts_with("of") && id.bytes().all(|b| b.is_ascii_alphanumeric());
        if !well_formed {
            return None;
        }
        Some(self.covers.as_ref()?.join(format!("{id}.jpg")))
    }

    /// When the playlist's own picture was last written (nanoseconds since 1970), so a new
    /// one gets a new cover id; None while it has none.
    pub fn cover_stamp(&self, id: &str) -> Option<u128> {
        let modified = std::fs::metadata(self.cover_path(id)?).ok()?.modified().ok()?;
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since| since.as_nanos())
    }

    /// Keeps `picture` as the playlist's own cover, in place of the painted one or the picture
    /// before it. Slow (it reads and redraws the picture): not on an async thread.
    pub fn set_cover(&self, id: &str, picture: &[u8]) -> Result<(), CoverRefused> {
        if self.get(id).is_none() {
            return Err(CoverRefused::NotFound);
        }
        let path = self.cover_path(id).ok_or(CoverRefused::NowhereToKeep)?;
        let kept = family_playlist_covers::prepared(picture)?;
        family_playlist_covers::write_whole(&path, &kept)
    }

    /// Removes the playlist's own picture: its painted cover shows again. True when there was
    /// one.
    pub fn remove_cover(&self, id: &str) -> bool {
        self.cover_path(id)
            .is_some_and(|path| std::fs::remove_file(path).is_ok())
    }

    /// Points every entry of every playlist that names library song `old` at `new` instead, in
    /// `scope` (`Shared` or `Personal:<user>`), for a song whose file moved and whose id
    /// changed with it. Answers how many entries changed.
    pub fn remap_library_id(&self, old: &str, new: &str, scope: &str) -> usize {
        let changed = {
            let mut playlists = self.playlists.lock();
            let mut changed = 0;
            for entry in playlists.iter_mut().flat_map(|p| p.entries.iter_mut()) {
                if entry.library_id.as_deref() == Some(old) {
                    entry.library_id = Some(new.to_string());
                    entry.scope = scope.to_string();
                    changed += 1;
                }
            }
            changed
        };
        if changed > 0 {
            self.save();
        }
        changed
    }

    fn change(&self, id: &str, change: impl FnOnce(&mut FamilyPlaylist)) -> bool {
        let found = {
            let mut playlists = self.playlists.lock();
            match playlists.iter_mut().find(|p| p.id == id) {
                Some(playlist) => {
                    change(playlist);
                    true
                }
                None => false,
            }
        };
        if found {
            self.save();
        }
        found
    }

    fn save(&self) {
        store_file::save_with(self.path.as_deref(), &self.save_lock, || {
            self.playlists.lock().clone()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 18, 0, 0).unwrap()
    }

    fn entry(id: &str, by: &str) -> FamilyPlaylistEntry {
        FamilyPlaylistEntry {
            library_id: None,
            scope: String::new(),
            outside_id: Some(id.to_string()),
            added_by: by.to_string(),
            added: now(),
            pending: false,
            duration: None,
            explicit_status: None,
        }
    }

    #[test]
    fn of_ids_are_octo_playlist_ids() {
        let id = family_playlist_id("Road trip", &now());
        assert_eq!(id.len(), 22, "{id}");
        assert!(id.starts_with("of"));
        assert!(id[2..].bytes().all(|b| b.is_ascii_alphanumeric()), "{id}");
        assert_eq!(
            id,
            family_playlist_id("Road trip", &now()),
            "the same name and time"
        );
        assert_ne!(id, family_playlist_id("Road trip 2", &now()));
        // Two made at the same instant with one name still get two ids.
        let store = FamilyPlaylistStore::in_memory();
        let a = store.create("Road trip", "winters", now());
        let b = store.create("Road trip", "alex", now());
        assert_ne!(a.id, b.id);
        assert_eq!(b.id.len(), 22);
    }

    #[test]
    fn create_get_rename_and_approval() {
        let store = FamilyPlaylistStore::in_memory();
        let made = store.create(" Road trip ", "winters", now());
        assert_eq!(made.name, "Road trip");
        assert_eq!(store.get(&made.id), Some(made.clone()));
        assert!(store.rename(&made.id, "Summer"));
        assert!(store.set_approve(&made.id, true));
        let got = store.get(&made.id).expect("there");
        assert_eq!(got.name, "Summer");
        assert!(got.approve_additions);
        assert!(!store.rename("of_missing", "x"));
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn entries_add_remove_and_approve() {
        let store = FamilyPlaylistStore::in_memory();
        let made = store.create("Road trip", "winters", now());
        let mut waiting = entry("e2", "alex");
        waiting.pending = true;
        assert!(store.add_entries(&made.id, vec![entry("e1", "alex"), waiting, entry("e3", "sam")]));
        assert!(store.approve_entry(&made.id, 1));
        assert!(!store.approve_entry(&made.id, 9));
        assert!(!store.get(&made.id).expect("there").entries[1].pending);
        assert!(store.remove_indexes(&made.id, &[0, 2, 7]));
        let left = store.get(&made.id).expect("there").entries;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].outside_id.as_deref(), Some("e2"));
    }

    #[test]
    fn delete_and_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("family-playlists.json");
        let store = FamilyPlaylistStore::new(Some(&path));
        let keep = store.create("Keep", "winters", now());
        let gone = store.create("Gone", "winters", now());
        store.add_entries(&keep.id, vec![entry("e1", "alex")]);
        assert!(store.delete(&gone.id));
        assert!(!store.delete(&gone.id));
        let reloaded = FamilyPlaylistStore::new(Some(&path));
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(
            reloaded.get(&keep.id).expect("there").entries,
            vec![entry("e1", "alex")]
        );
    }

    #[test]
    fn a_playlist_keeps_its_own_picture_in_the_covers_folder_until_it_goes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = FamilyPlaylistStore::new(Some(dir.path().join("family-playlists.json")));
        let made = store.create("Road Trip", "winters", now());
        let path = store.cover_path(&made.id).expect("a place");
        assert_eq!(path, dir.path().join("covers").join(format!("{}.jpg", made.id)));
        assert_eq!(store.cover_stamp(&made.id), None);

        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(400, 300)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encodes");
        assert_eq!(store.set_cover(&made.id, png.get_ref()), Ok(()));
        assert!(store.cover_stamp(&made.id).is_some());
        assert_eq!(image::image_dimensions(&path).expect("a picture"), (300, 300));
        assert_eq!(
            store.set_cover("of00000000000000000000", png.get_ref()),
            Err(CoverRefused::NotFound)
        );
        assert_eq!(store.cover_path("../../etc/passwd"), None);

        assert!(store.delete(&made.id));
        assert!(!path.exists());
        assert!(!FamilyPlaylistStore::in_memory().remove_cover(&made.id));
    }
}
