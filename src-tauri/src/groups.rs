//! Persistence for manual groups (`groups.json`) and watched flags (`watched.json`).
//! Grouping itself is computed in the frontend; this module only stores the data.
//!
//! Mac↔Mac sync (see sync.rs): every group carries `updated_at` (unix ms) and deletes
//! stay as tombstones (`deleted`); excluded keys carry `excluded_at`; watched entries are
//! `{watched, updated_at, device}` (legacy `{"k": true}` still loads). Merge is last
//! writer wins per entry. Stores are keyed by data dir (tests run several at once).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::types::{Group, GroupsFile};

/// What `get_watched` returns: only watched entries, `"key:idx" -> true`.
pub type Watched = BTreeMap<String, bool>;

/// One watched mark (false = tombstone of an unmark).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchedEntry {
    pub watched: bool,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub device: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WatchedVal {
    Legacy(bool),
    Entry(WatchedEntry),
}

pub type WatchedFile = BTreeMap<String, WatchedEntry>;

fn parse_watched(bytes: &[u8]) -> Result<WatchedFile, serde_json::Error> {
    let raw: BTreeMap<String, WatchedVal> = serde_json::from_slice(bytes)?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| {
            let e = match v {
                WatchedVal::Legacy(b) => WatchedEntry { watched: b, updated_at: 0, device: String::new() },
                WatchedVal::Entry(e) => e,
            };
            (k, e)
        })
        .collect())
}

/// Exclusion state of one key for sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExcludedEntry {
    pub excluded: bool,
    pub updated_at: u64,
}

struct Store {
    groups: GroupsFile,
    watched: WatchedFile,
}

static STORES: Mutex<Option<HashMap<PathBuf, Store>>> = Mutex::new(None);

/// Same directory `state.rs` uses.
fn data_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/com.musahinist.tide"))
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
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

fn load_watched(path: &Path) -> WatchedFile {
    match std::fs::read(path) {
        Ok(bytes) => parse_watched(&bytes).unwrap_or_else(|e| {
            tracing::warn!("{}: invalid JSON, starting empty: {e}", path.display());
            WatchedFile::default()
        }),
        Err(_) => WatchedFile::default(),
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

fn with_store_in<R>(dir: &Path, f: impl FnOnce(&mut Store) -> Result<R, String>) -> Result<R, String> {
    let mut guard = STORES.lock().map_err(|_| "groups store poisoned".to_string())?;
    let map = guard.get_or_insert_with(HashMap::new);
    let s = map.entry(dir.to_path_buf()).or_insert_with(|| Store {
        groups: load(&dir.join("groups.json")),
        watched: load_watched(&dir.join("watched.json")),
    });
    f(s)
}

fn with_store<R>(f: impl FnOnce(&mut Store) -> Result<R, String>) -> Result<R, String> {
    with_store_in(&data_dir()?, f)
}

/// Stamp what a local mutation changed: edited groups get `updated_at`, removed ones
/// become tombstones, exclusion flips get `excluded_at`.
fn stamp_changes(old: &GroupsFile, next: &mut GroupsFile, now: u64) {
    let strip = |g: &Group| Group { updated_at: 0, ..g.clone() };
    for g in &mut next.groups {
        match old.groups.iter().find(|o| o.id == g.id) {
            Some(o) if strip(o) == strip(g) => {}
            _ => g.updated_at = now,
        }
    }
    for o in &old.groups {
        if !next.groups.iter().any(|g| g.id == o.id) {
            next.groups.push(Group { id: o.id.clone(), name: o.name.clone(), deleted: true, updated_at: now, ..Default::default() });
        }
    }
    let keys: std::collections::BTreeSet<String> = old.excluded.keys().chain(next.excluded.keys()).cloned().collect();
    for k in keys {
        if old.excluded.contains_key(&k) != next.excluded.contains_key(&k) {
            next.excluded_at.insert(k, now);
        }
    }
}

/// Apply a mutation to the groups file and persist it.
fn mutate_groups<R>(f: impl FnOnce(&mut GroupsFile) -> Result<R, String>) -> Result<R, String> {
    let dir = data_dir()?;
    let out = with_store_in(&dir, |s| mutate_groups_store(s, &dir, f))?;
    crate::sync::touch();
    Ok(out)
}

fn mutate_groups_store<R>(s: &mut Store, dir: &Path, f: impl FnOnce(&mut GroupsFile) -> Result<R, String>) -> Result<R, String> {
    let mut next = s.groups.clone();
    let out = f(&mut next)?;
    stamp_changes(&s.groups, &mut next, now_ms());
    save(&dir.join("groups.json"), &next)?;
    s.groups = next;
    Ok(out)
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
        .find(|x| x.id == id && !x.deleted)
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
    let group = Group { id, name, members, ..Default::default() };
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
    with_store(|s| Ok(visible_groups(&s.groups)))
}

/// `groups.json` without sync bookkeeping (tombstones, timestamps).
fn visible_groups(g: &GroupsFile) -> GroupsFile {
    GroupsFile {
        groups: g.groups.iter().filter(|x| !x.deleted).map(|x| Group { updated_at: 0, ..x.clone() }).collect(),
        excluded: g.excluded.clone(),
        excluded_at: BTreeMap::new(),
    }
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

fn visible_watched(w: &WatchedFile) -> Watched {
    w.iter().filter(|(_, e)| e.watched).map(|(k, _)| (k.clone(), true)).collect()
}

#[tauri::command]
pub fn get_watched() -> Result<Watched, String> {
    with_store(|s| Ok(visible_watched(&s.watched)))
}

fn set_watched_in(dir: &Path, key: &str, file_index: usize, watched: bool, device: &str) -> Result<(), String> {
    with_store_in(dir, |s| {
        let mut next = s.watched.clone();
        let k = watched_key(key, file_index);
        if next.get(&k).map(|e| e.watched) == Some(watched) || (!watched && !next.contains_key(&k)) {
            return Ok(());
        }
        next.insert(k, WatchedEntry { watched, updated_at: now_ms(), device: device.to_string() });
        save(&dir.join("watched.json"), &next)?;
        s.watched = next;
        Ok(())
    })
}

#[tauri::command]
pub fn set_watched(key: String, file_index: usize, watched: bool) -> Result<(), String> {
    set_watched_in(&data_dir()?, &key, file_index, watched, &crate::sync::device_id())?;
    crate::sync::touch();
    Ok(())
}

// ---- sync (sync.rs) ----

/// Is `b` (ts, device) newer than `a`? Ties go to the larger device id so both Macs
/// converge on the same winner.
pub fn newer(a: (u64, &str), b: (u64, &str)) -> bool {
    b.0 > a.0 || (b.0 == a.0 && b.1 > a.1)
}

fn since_ok(ts: u64, since: u64) -> bool {
    since == 0 || ts > since
}

/// Entries changed after `since` (0 = everything).
pub fn sync_export(dir: &Path, since: u64) -> Result<(WatchedFile, Vec<Group>, BTreeMap<String, ExcludedEntry>), String> {
    with_store_in(dir, |s| {
        let w = s.watched.iter().filter(|(_, e)| since_ok(e.updated_at, since)).map(|(k, e)| (k.clone(), e.clone())).collect();
        let g = s.groups.groups.iter().filter(|g| since_ok(g.updated_at, since)).cloned().collect();
        let x = s
            .groups
            .excluded_at
            .iter()
            .filter(|(_, ts)| since_ok(**ts, since))
            .map(|(k, ts)| (k.clone(), ExcludedEntry { excluded: s.groups.excluded.contains_key(k), updated_at: *ts }))
            .collect();
        Ok((w, g, x))
    })
}

/// Pure LWW merge of watched entries; true if anything changed.
pub fn merge_watched(local: &mut WatchedFile, remote: WatchedFile) -> bool {
    let mut changed = false;
    for (k, r) in remote {
        let take = match local.get(&k) {
            None => true,
            Some(l) => newer((l.updated_at, &l.device), (r.updated_at, &r.device)),
        };
        if take && local.get(&k) != Some(&r) {
            local.insert(k, r);
            changed = true;
        }
    }
    changed
}

/// Pure LWW merge of groups (per group) and exclusions (per key); true if changed.
pub fn merge_groups(local: &mut GroupsFile, groups: Vec<Group>, excluded: BTreeMap<String, ExcludedEntry>) -> bool {
    let mut changed = false;
    for r in groups {
        if r.id.is_empty() {
            continue;
        }
        match local.groups.iter_mut().find(|g| g.id == r.id) {
            Some(l) => {
                if r.updated_at > l.updated_at && *l != r {
                    *l = r;
                    changed = true;
                }
            }
            None => {
                local.groups.push(r);
                changed = true;
            }
        }
    }
    for (k, r) in excluded {
        let lt = local.excluded_at.get(&k).copied();
        if lt.is_none_or(|t| r.updated_at > t) {
            let was = local.excluded.contains_key(&k);
            if r.excluded {
                local.excluded.insert(k.clone(), true);
            } else {
                local.excluded.remove(&k);
            }
            local.excluded_at.insert(k, r.updated_at);
            changed |= was != r.excluded || lt.is_none();
        }
    }
    if changed {
        normalize_members(local);
    }
    changed
}

/// A key belongs to at most one live group: the most recently changed one keeps it
/// (deterministic, so both Macs end up identical without restamping).
fn normalize_members(g: &mut GroupsFile) {
    let mut owner: HashMap<String, (u64, String)> = HashMap::new();
    for grp in g.groups.iter().filter(|x| !x.deleted) {
        for k in &grp.members {
            let cand = (grp.updated_at, grp.id.clone());
            match owner.get(k) {
                Some(cur) if *cur >= cand => {}
                _ => {
                    owner.insert(k.clone(), cand);
                }
            }
        }
    }
    for grp in g.groups.iter_mut() {
        if grp.deleted {
            grp.members.clear();
            continue;
        }
        let id = grp.id.clone();
        grp.members.retain(|k| owner.get(k).is_some_and(|(_, o)| *o == id));
    }
}

/// Merge remote entries into `dir`'s stores. Returns (watched changed, groups changed).
pub fn sync_apply(
    dir: &Path,
    watched: WatchedFile,
    groups: Vec<Group>,
    excluded: BTreeMap<String, ExcludedEntry>,
) -> Result<(bool, bool), String> {
    with_store_in(dir, |s| {
        let mut w = s.watched.clone();
        let wc = merge_watched(&mut w, watched);
        if wc {
            save(&dir.join("watched.json"), &w)?;
            s.watched = w;
        }
        let mut g = s.groups.clone();
        let gc = merge_groups(&mut g, groups, excluded);
        if gc {
            save(&dir.join("groups.json"), &g)?;
            s.groups = g;
        }
        Ok((wc, gc))
    })
}

/// Visible groups / watched of a data dir (tests, sync).
#[allow(dead_code)]
pub fn snapshot_in(dir: &Path) -> Result<(GroupsFile, Watched), String> {
    with_store_in(dir, |s| Ok((visible_groups(&s.groups), visible_watched(&s.watched))))
}

/// Local mutations against an explicit data dir (tests).
#[cfg(test)]
pub fn test_set_watched(dir: &Path, key: &str, idx: usize, watched: bool, device: &str) -> Result<(), String> {
    set_watched_in(dir, key, idx, watched, device)
}

#[cfg(test)]
pub fn test_mutate_groups<R>(dir: &Path, f: impl FnOnce(&mut GroupsFile) -> Result<R, String>) -> Result<R, String> {
    with_store_in(dir, |s| mutate_groups_store(s, dir, f))
}

#[cfg(test)]
pub fn test_create(g: &mut GroupsFile, id: &str, name: &str, keys: &[&str]) -> Group {
    op_create(g, id.into(), name.into(), keys.iter().map(|k| k.to_string()).collect())
}

#[cfg(test)]
pub fn test_delete(g: &mut GroupsFile, id: &str) {
    op_delete(g, id)
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
