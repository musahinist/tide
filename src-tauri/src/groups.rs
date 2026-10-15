//! Persistence for manual groups (`groups.json`) and watched flags (`watched.json`).
//! Grouping itself is computed in the frontend; this module only stores the data.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{de::DeserializeOwned, Serialize};

use crate::types::{Group, GroupsFile};

pub type Watched = BTreeMap<String, bool>;

struct Store {
    dir: PathBuf,
    groups: GroupsFile,
    watched: Watched,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

/// Same directory `state.rs` uses.
fn data_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/com.musahinist.tide"))
}

fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!("{}: invalid JSON, starting empty: {e}", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

/// Write to a temp file then rename, so a crash never leaves a half-written file.
fn save<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

fn with_store<R>(f: impl FnOnce(&mut Store) -> Result<R, String>) -> Result<R, String> {
    let mut guard = STORE.lock().map_err(|_| "groups store poisoned".to_string())?;
    if guard.is_none() {
        let dir = data_dir()?;
        *guard = Some(Store {
            groups: load(&dir.join("groups.json")),
            watched: load(&dir.join("watched.json")),
            dir,
        });
    }
    f(guard.as_mut().expect("store initialised"))
}

/// Apply a mutation to the groups file and persist it.
fn mutate_groups<R>(f: impl FnOnce(&mut GroupsFile) -> Result<R, String>) -> Result<R, String> {
    with_store(|s| {
        let mut next = s.groups.clone();
        let out = f(&mut next)?;
        save(&s.dir.join("groups.json"), &next)?;
        s.groups = next;
        Ok(out)
    })
}

fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("g{nanos:x}{:x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

// ---- pure logic (unit-tested) ----

fn detach(g: &mut GroupsFile, key: &str) {
    for group in &mut g.groups {
        group.members.retain(|k| k != key);
    }
}

fn find<'a>(g: &'a mut GroupsFile, id: &str) -> Result<&'a mut Group, String> {
    g.groups
        .iter_mut()
        .find(|x| x.id == id)
        .ok_or_else(|| format!("no group {id}"))
}

fn op_create(g: &mut GroupsFile, id: String, name: String, keys: Vec<String>) -> Group {
    let mut members = Vec::new();
    for key in keys {
        detach(g, &key);
        g.excluded.remove(&key);
        if !members.contains(&key) {
            members.push(key);
        }
    }
    let group = Group { id, name, members, auto_key: None };
    g.groups.push(group.clone());
    group
}

fn op_add(g: &mut GroupsFile, id: &str, key: &str) -> Result<(), String> {
    find(g, id)?;
    detach(g, key);
    g.excluded.remove(key);
    find(g, id)?.members.push(key.to_string());
    Ok(())
}

fn op_remove(g: &mut GroupsFile, key: &str) {
    detach(g, key);
    g.excluded.insert(key.to_string(), true);
}

fn op_rename(g: &mut GroupsFile, id: &str, name: String) -> Result<(), String> {
    find(g, id)?.name = name;
    Ok(())
}

fn op_delete(g: &mut GroupsFile, id: &str) {
    g.groups.retain(|x| x.id != id);
}

fn watched_key(key: &str, file_index: usize) -> String {
    format!("{key}:{file_index}")
}

// ---- commands ----

#[tauri::command]
pub fn list_groups() -> Result<GroupsFile, String> {
    with_store(|s| Ok(s.groups.clone()))
}

#[tauri::command]
pub fn create_group(name: String, keys: Vec<String>) -> Result<Group, String> {
    mutate_groups(|g| Ok(op_create(g, new_id(), name, keys)))
}

#[tauri::command]
pub fn rename_group(id: String, name: String) -> Result<(), String> {
    mutate_groups(|g| op_rename(g, &id, name))
}

#[tauri::command]
pub fn add_to_group(id: String, key: String) -> Result<(), String> {
    mutate_groups(|g| op_add(g, &id, &key))
}

#[tauri::command]
pub fn remove_from_group(key: String) -> Result<(), String> {
    mutate_groups(|g| {
        op_remove(g, &key);
        Ok(())
    })
}

#[tauri::command]
pub fn delete_group(id: String) -> Result<(), String> {
    mutate_groups(|g| {
        op_delete(g, &id);
        Ok(())
    })
}

#[tauri::command]
pub fn get_watched() -> Result<Watched, String> {
    with_store(|s| Ok(s.watched.clone()))
}

#[tauri::command]
pub fn set_watched(key: String, file_index: usize, watched: bool) -> Result<(), String> {
    with_store(|s| {
        let mut next = s.watched.clone();
        let k = watched_key(&key, file_index);
        if watched {
            next.insert(k, true);
        } else {
            next.remove(&k);
        }
        save(&s.dir.join("watched.json"), &next)?;
        s.watched = next;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GroupsFile {
        let mut g = GroupsFile::default();
        op_create(&mut g, "a".into(), "A".into(), vec!["k1".into(), "k2".into(), "k1".into()]);
        op_create(&mut g, "b".into(), "B".into(), vec!["k3".into()]);
        g
    }

    #[test]
    fn create_dedups_and_steals() {
        let mut g = sample();
        assert_eq!(g.groups[0].members, vec!["k1", "k2"]);
        op_create(&mut g, "c".into(), "C".into(), vec!["k2".into()]);
        assert_eq!(g.groups[0].members, vec!["k1"]);
        assert_eq!(g.groups[2].members, vec!["k2"]);
    }

    #[test]
    fn add_moves_between_groups_and_clears_exclusion() {
        let mut g = sample();
        op_remove(&mut g, "k3");
        assert_eq!(g.excluded.get("k3"), Some(&true));
        op_add(&mut g, "a", "k3").unwrap();
        assert!(!g.excluded.contains_key("k3"));
        op_add(&mut g, "b", "k1").unwrap();
        assert_eq!(g.groups[0].members, vec!["k2", "k3"]);
        assert_eq!(g.groups[1].members, vec!["k1"]);
        assert!(op_add(&mut g, "nope", "k1").is_err());
        assert_eq!(g.groups[1].members, vec!["k1"]);
    }

    #[test]
    fn remove_excludes_and_rename_delete() {
        let mut g = sample();
        op_remove(&mut g, "k1");
        assert_eq!(g.groups[0].members, vec!["k2"]);
        assert_eq!(g.excluded.get("k1"), Some(&true));
        op_rename(&mut g, "a", "Renamed".into()).unwrap();
        assert_eq!(g.groups[0].name, "Renamed");
        assert!(op_rename(&mut g, "zz", "x".into()).is_err());
        op_delete(&mut g, "a");
        assert_eq!(g.groups.len(), 1);
        assert_eq!(g.excluded.get("k1"), Some(&true));
    }

    #[test]
    fn json_round_trip_and_atomic_save() {
        let mut g = sample();
        op_remove(&mut g, "k2");
        let dir = std::env::temp_dir().join(format!("tide-groups-test-{}", new_id()));
        let path = dir.join("groups.json");
        save(&path, &g).unwrap();
        assert!(!dir.join("groups.json.tmp").exists());
        let back: GroupsFile = load(&path);
        assert_eq!(back, g);

        let mut w = Watched::new();
        w.insert(watched_key("abc", 3), true);
        save(&dir.join("watched.json"), &w).unwrap();
        let wb: Watched = load(&dir.join("watched.json"));
        assert_eq!(wb.get("abc:3"), Some(&true));

        // Minimal/legacy JSON still parses.
        let min: GroupsFile = serde_json::from_str(r#"{"groups":[{"id":"x","name":"X"}]}"#).unwrap();
        assert!(min.groups[0].members.is_empty() && min.excluded.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
