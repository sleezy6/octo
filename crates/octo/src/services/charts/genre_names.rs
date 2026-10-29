//! English names of Apple genres learned at run time, in `genre-names.json`: a map from genre
//! id to name, for ids the built-in tables do not know.

use std::collections::BTreeMap;
use std::path::PathBuf;

use parking_lot::Mutex;
use tracing::warn;

pub struct GenreNames {
    path: Option<PathBuf>,
    names: Mutex<BTreeMap<String, String>>,
}

impl GenreNames {
    /// `path`: the file, or none to keep the names in memory only. A file that cannot be read
    /// starts the store empty.
    pub fn new(path: Option<PathBuf>) -> Self {
        let names = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            names: Mutex::new(names),
        }
    }

    pub fn get(&self, id: &str) -> Option<String> {
        self.names.lock().get(id).cloned()
    }

    /// Remembers a name and saves the file.
    pub fn set(&self, id: &str, name: &str) {
        let json = {
            let mut names = self.names.lock();
            names.insert(id.to_string(), name.to_string());
            serde_json::to_vec_pretty(&*names)
        };
        let (Some(path), Ok(json)) = (&self.path, json) else {
            return;
        };
        let temp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&temp, json).and_then(|()| std::fs::rename(&temp, path)) {
            warn!("Could not save the learned genre names: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_learned_name_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("genre-names.json");
        GenreNames::new(Some(path.clone())).set("99999", "Rock Test");
        assert_eq!(
            GenreNames::new(Some(path)).get("99999").as_deref(),
            Some("Rock Test")
        );
    }
}
