//! Each listener's chart country and pinned countries, in `chart-countries.json`. A listener with
//! neither has no entry and follows the server's chart country.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// One listener's chart choices.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenerCharts {
    /// Their own chart country, lowercase; empty follows the server.
    #[serde(default)]
    pub country: String,
    /// Countries whose charts they get as well, at most `MAX_PINNED`.
    #[serde(default)]
    pub pinned: Vec<String>,
    /// Which change last touched this row; a saved choice carries the one it was based on.
    #[serde(default)]
    pub revision: u64,
    /// The person's Navidrome id, once a check of Navidrome's people matched the row by name; it
    /// finds the row again after Navidrome renames the person. Never part of the choices.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub navidrome_id: String,
}

impl ListenerCharts {
    /// Whether the choices are the same, whatever their revisions.
    fn same_choices(&self, other: &ListenerCharts) -> bool {
        self.country == other.country && self.pinned == other.pinned
    }

    fn is_empty(&self) -> bool {
        self.country.is_empty() && self.pinned.is_empty()
    }
}

/// What a check of Navidrome's people changed in the store.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// The keys of rows dropped because Navidrome no longer has the person.
    pub dropped: Vec<String>,
    /// Rows moved to the name Navidrome now gives the person: old key, new key.
    pub renamed: Vec<(String, String)>,
}

/// A save based on a row that has changed since: the row as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub current: ListenerCharts,
}

/// The file's current shape: the choices and the last revision handed out.
#[derive(Serialize)]
struct FileOut<'a> {
    stamp: u64,
    people: &'a BTreeMap<String, ListenerCharts>,
}

/// Reads either shape of the file: the choices alone, or `{"stamp", "people"}`.
fn parse_file(bytes: &[u8]) -> serde_json::Result<(u64, BTreeMap<String, ListenerCharts>)> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let current = value.as_object().and_then(|o| {
        let stamp = o.get("stamp")?.as_u64()?;
        let people = o.get("people")?.is_object().then(|| o.get("people").cloned())??;
        Some((stamp, people))
    });
    match current {
        Some((stamp, people)) => Ok((stamp, serde_json::from_value(people)?)),
        None => Ok((0, serde_json::from_value(value)?)),
    }
}

/// Every listener's chart choices, in `chart-countries.json`.
pub struct ChartCountryStore {
    path: Option<PathBuf>,
    users: Mutex<Users>,
    /// The generation last written to the file, held while writing, so a save never writes over
    /// a newer one.
    written: Mutex<u64>,
    /// The keys of the people the last complete check found in Navidrome; none before one.
    last_verified: Mutex<Option<HashSet<String>>>,
}

/// The choices, how many changes they have had, and the last revision handed to a row.
#[derive(Default)]
struct Users {
    by_name: BTreeMap<String, ListenerCharts>,
    generation: u64,
    stamp: u64,
}

impl ChartCountryStore {
    /// `path`: the file, or none to keep the choices in memory only. A file that cannot be read
    /// is moved aside to `chart-countries.json.broken-<unix time>` and the store starts empty.
    /// Names and countries are read in lowercase, whatever case the file holds; names that differ
    /// only by case keep the row with the higher revision.
    pub fn new(path: Option<PathBuf>) -> Self {
        let (saved_stamp, read): (u64, BTreeMap<String, ListenerCharts>) = match path
            .as_ref()
            .map(std::fs::read)
        {
            Some(Ok(bytes)) => match parse_file(&bytes) {
                Ok(read) => read,
                Err(e) => {
                    let path = path.as_ref().expect("read from a path");
                    let aside = broken_name(path);
                    match std::fs::rename(path, &aside) {
                        Ok(()) => warn!(
                            "Could not read the chart countries, starting without them; the file is kept as {}: {e}",
                            aside.display()
                        ),
                        Err(moved) => warn!(
                            "Could not read the chart countries, starting without them: {e} (and could not move the file aside: {moved})"
                        ),
                    }
                    (0, BTreeMap::new())
                }
            },
            _ => (0, BTreeMap::new()),
        };
        let mut stamp = saved_stamp;
        let mut by_name: BTreeMap<String, ListenerCharts> = BTreeMap::new();
        for (user, charts) in read {
            stamp = stamp.max(charts.revision);
            let mut row = ListenerCharts {
                country: charts.country.trim().to_ascii_lowercase(),
                pinned: Vec::new(),
                revision: charts.revision,
                navidrome_id: charts.navidrome_id.clone(),
            };
            for pin in charts.pinned {
                let pin = pin.trim().to_ascii_lowercase();
                if !pin.is_empty() && !row.pinned.contains(&pin) {
                    row.pinned.push(pin);
                }
            }
            match by_name.entry(user_key(&user)) {
                std::collections::btree_map::Entry::Occupied(mut kept) => {
                    if row.revision > kept.get().revision {
                        kept.insert(row);
                    }
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(row);
                }
            }
        }
        ChartCountryStore {
            path,
            users: Mutex::new(Users {
                by_name,
                generation: 0,
                stamp,
            }),
            written: Mutex::new(0),
            last_verified: Mutex::new(None),
        }
    }

    /// A listener's choices, none when they never chose.
    pub fn get(&self, user: &str) -> ListenerCharts {
        self.users
            .lock()
            .by_name
            .get(&user_key(user))
            .cloned()
            .unwrap_or_default()
    }

    /// Saves a listener's choices. Choosing nothing removes their entry.
    pub fn set(&self, user: &str, charts: ListenerCharts) {
        let _ = self.update(user, |mine| {
            *mine = charts;
            Ok::<(), ()>(())
        });
    }

    /// Changes a listener's choices in one step, so two changes at once never undo each other.
    /// An error from `change` leaves them as they were. Answers the choices as saved.
    pub fn update<E>(
        &self,
        user: &str,
        change: impl FnOnce(&mut ListenerCharts) -> Result<(), E>,
    ) -> Result<ListenerCharts, E> {
        let (mine, json, generation) = {
            let mut users = self.users.lock();
            let before = users.by_name.get(&user_key(user)).cloned().unwrap_or_default();
            let mut mine = before.clone();
            change(&mut mine)?;
            mine.revision = before.revision;
            mine.navidrome_id = before.navidrome_id.clone();
            if !mine.same_choices(&before) {
                users.stamp += 1;
                mine.revision = users.stamp;
            }
            if mine.is_empty() {
                users.by_name.remove(&user_key(user));
                mine.revision = 0;
            } else {
                users.by_name.insert(user_key(user), mine.clone());
            }
            users.generation += 1;
            let json = self.path.is_some().then(|| {
                serde_json::to_vec_pretty(&FileOut {
                    stamp: users.stamp,
                    people: &users.by_name,
                })
            });
            (mine, json, users.generation)
        };
        if let Some(json) = json {
            self.save(json, generation);
        }
        Ok(mine)
    }

    /// Changes a listener's choices only if their row is still at revision `expected` (0 for a
    /// row that does not exist). The check and the write are one step. A row that has changed
    /// since is answered as it is now and left alone.
    pub fn update_if(
        &self,
        user: &str,
        expected: u64,
        change: impl FnOnce(&mut ListenerCharts),
    ) -> Result<ListenerCharts, Conflict> {
        let mut current = None;
        let saved = self.update(user, |mine| {
            if mine.revision != expected {
                current = Some(mine.clone());
                return Err(());
            }
            change(mine);
            Ok(())
        });
        saved.map_err(|()| Conflict {
            current: current.unwrap_or_default(),
        })
    }

    /// Writes one generation of the choices, unless a newer one was written already. The file is
    /// written outside the choices' lock, so reading them never waits on the disk.
    fn save(&self, json: serde_json::Result<Vec<u8>>, generation: u64) {
        let Some(path) = &self.path else {
            return;
        };
        let mut written = self.written.lock();
        if generation <= *written {
            return;
        }
        match json {
            Ok(json) => {
                let temp = path.with_extension("json.tmp");
                match std::fs::write(&temp, json).and_then(|()| std::fs::rename(&temp, path)) {
                    Ok(()) => *written = generation,
                    Err(e) => warn!("Could not save the chart countries: {e}"),
                }
            }
            Err(e) => warn!("Could not save the chart countries: {e}"),
        }
    }

    /// The key and revision of every row now, to hand to [`Self::reconcile`] later.
    pub fn revisions(&self) -> BTreeMap<String, u64> {
        self.users
            .lock()
            .by_name
            .iter()
            .map(|(user, charts)| (user.clone(), charts.revision))
            .collect()
    }

    /// Brings the rows in line with Navidrome's people, given as (id, user name). A row whose
    /// name is a person's records their id. A row whose name is nobody's but whose recorded id
    /// is a person's moves to that person's new name. Any other row is dropped. A row that has
    /// changed since `seen` (from [`Self::revisions`]) is left alone, and a row not in `seen`
    /// was saved since and is kept. Also remembers who Navidrome has, for [`Self::is_known`].
    pub fn reconcile(&self, people: &[(String, String)], seen: &BTreeMap<String, u64>) -> Reconciled {
        let by_key: BTreeMap<String, &str> = people
            .iter()
            .map(|(id, name)| (user_key(name), id.as_str()))
            .collect();
        let by_id: BTreeMap<&str, &str> = people
            .iter()
            .filter(|(id, _)| !id.is_empty())
            .map(|(id, name)| (id.as_str(), name.as_str()))
            .collect();
        let mut done = Reconciled::default();
        let (json, generation) = {
            let mut users = self.users.lock();
            let mut changed = false;
            let keys: Vec<String> = users.by_name.keys().cloned().collect();
            for old in keys {
                let Some(row) = users.by_name.get(&old).cloned() else {
                    continue;
                };
                if let Some(id) = by_key.get(&old) {
                    if !id.is_empty() && row.navidrome_id != *id {
                        if let Some(mine) = users.by_name.get_mut(&old) {
                            mine.navidrome_id = (*id).to_string();
                        }
                        changed = true;
                    }
                    continue;
                }
                if seen.get(&old) != Some(&row.revision) {
                    continue;
                }
                let moved_to = by_id
                    .get(row.navidrome_id.as_str())
                    .filter(|_| !row.navidrome_id.is_empty())
                    .map(|name| user_key(name));
                users.by_name.remove(&old);
                changed = true;
                match moved_to {
                    Some(new) if !users.by_name.contains_key(&new) => {
                        users.by_name.insert(new.clone(), row);
                        done.renamed.push((old, new));
                    }
                    Some(_) => {}
                    None => done.dropped.push(old),
                }
            }
            if changed {
                users.generation += 1;
            }
            let json = (changed && self.path.is_some()).then(|| {
                serde_json::to_vec_pretty(&FileOut {
                    stamp: users.stamp,
                    people: &users.by_name,
                })
            });
            (json, users.generation)
        };
        *self.last_verified.lock() = Some(by_key.keys().cloned().collect());
        if let Some(json) = json {
            self.save(json, generation);
        }
        done
    }

    /// Whether Navidrome has the person, as far as the last complete check knows; everyone
    /// before one.
    pub fn is_known(&self, user: &str) -> bool {
        self.last_verified
            .lock()
            .as_ref()
            .is_none_or(|people: &HashSet<String>| people.contains(&user_key(user)))
    }

    /// Every listener with choices, by key.
    pub fn all(&self) -> Vec<(String, ListenerCharts)> {
        self.users
            .lock()
            .by_name
            .iter()
            .map(|(user, charts)| (user.clone(), charts.clone()))
            .collect()
    }

    /// Every country some listener chose or pinned, sorted, no repeats.
    pub fn countries(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .users
            .lock()
            .by_name
            .values()
            .flat_map(|c| std::iter::once(c.country.clone()).chain(c.pinned.iter().cloned()))
            .filter(|c| !c.is_empty())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// How a person's name is compared and stored: trimmed and lowercase.
pub(crate) fn user_key(user: &str) -> String {
    user.trim().to_lowercase()
}

/// Where a file that could not be read is kept: beside it, named for when it was moved.
fn broken_name(path: &std::path::Path) -> PathBuf {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".broken-{seconds}"));
    PathBuf::from(name)
}

#[cfg(test)]
#[path = "chart_country_store_tests.rs"]
mod tests;
