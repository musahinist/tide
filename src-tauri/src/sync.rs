//! Mac↔Mac sync between paired Tides (pairing/transport: remote.rs). LAN only.
//!
//! What syncs, each entry last-writer-wins by `updated_at` (unix ms; ties go to the
//! larger device id so both Macs pick the same winner), deletes kept as tombstones:
//! - watched marks (`watched.json`, groups.rs) and resume positions (`resume.json`,
//!   here: `{"key:idx": {pos, dur, updated_at, device}}`, pos 0 = tombstone);
//! - groups (`groups.json`, per group + per excluded key);
//! - shared settings only: `settings.json` keys in `SETTINGS_KEYS`, `watcher.json`
//!   settings (+ `auto_add_device`: only the Mac where auto-add was last turned on
//!   auto-adds) and `unfollowed`. Their change times live in `sync.json` (local edits are
//!   noticed by comparing values every few seconds). Nothing else is ever accepted.
//!
//! Transport (Bearer + `X-Tide-Id`, served by remote.rs): `GET /v1/sync/state?since=<ms>`
//! returns a `Delta` of entries changed after `since` (0 = all) plus the server clock;
//! `POST /v1/sync/push` merges a `Delta`. A cycle with a peer = pull, merge, push.
//! Runs on startup, when a paired device comes online, every 60 s while online and 2 s
//! after local changes (`touch`). Event `sync-updated {what:[...]}` after applying
//! remote changes ("watched", "resume", "settings", "groups", "library").
//!
//! Commands: `resume_get {key, fileIndex}` -> {pos, dur, updated_at}|null,
//! `resume_set {key, fileIndex, pos, dur}`, `resume_list`, `sync_status`, `sync_now`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

use crate::groups::{self, newer, now_ms, ExcludedEntry, WatchedFile};
use crate::remote::{self, Device, RemoteState};
use crate::types::Group;

/// `settings.json` keys shared between Macs.
pub const SETTINGS_KEYS: &[&str] =
    &["netmode", "limit_kbps", "notifications_enabled", "metered_networks", "unmetered_networks", "subtitle_langs"];
/// `watcher.json` settings (+ auto_add_device) and unfollowed series.
const WATCHER_KEY: &str = "watcher";
const UNFOLLOWED_KEY: &str = "watcher.unfollowed";

/// `since` handed back to a peer is its clock minus this (clock jitter, in-flight writes).
const SINCE_MARGIN: u64 = 120_000;
const PERIOD: Duration = Duration::from_secs(60);
const DEBOUNCE_MS: u64 = 2_000;
const PROBE_EVERY: u64 = 15;
const FULL_REACH_EVERY: Duration = Duration::from_secs(300);
const TIMEOUT: Duration = Duration::from_secs(20);

pub fn is_shared_key(k: &str) -> bool {
    SETTINGS_KEYS.contains(&k) || k == WATCHER_KEY || k == UNFOLLOWED_KEY
}

static DEVICE: OnceLock<String> = OnceLock::new();
/// Unix ms of the last local change not yet synced (0 = none).
static DIRTY: AtomicU64 = AtomicU64::new(0);

/// This Mac's device id (remote.json `selfId`); empty before startup.
pub fn device_id() -> String {
    DEVICE.get().cloned().unwrap_or_default()
}

/// A local change happened: sync with online peers ~2 s from now.
pub fn touch() {
    DIRTY.store(now_ms().max(1), Ordering::SeqCst);
}

// ---------------------------------------------------------------- data

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeEntry {
    pub pos: f64,
    #[serde(default)]
    pub dur: f64,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub device: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingEntry {
    pub value: Value,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub device: String,
}

/// Wire format of `/v1/sync/state` and `/v1/sync/push`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Delta {
    pub device: String,
    /// Sender's clock (unix ms).
    pub now: u64,
    pub watched: WatchedFile,
    pub resume: BTreeMap<String, ResumeEntry>,
    pub settings: BTreeMap<String, SettingEntry>,
    pub groups: Vec<Group>,
    pub excluded: BTreeMap<String, ExcludedEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct SyncFile {
    settings: BTreeMap<String, SettingEntry>,
}

/// Pure LWW merge of resume entries; true if anything changed.
pub fn merge_resume(local: &mut BTreeMap<String, ResumeEntry>, remote: BTreeMap<String, ResumeEntry>) -> bool {
    let mut changed = false;
    for (k, r) in remote {
        if !valid_entry_key(&k) || !r.pos.is_finite() || !r.dur.is_finite() {
            continue;
        }
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

/// `"<40 hex>:<idx>"`.
fn valid_entry_key(k: &str) -> bool {
    k.split_once(':').is_some_and(|(h, i)| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()) && i.parse::<usize>().is_ok())
}

/// Keep only shared keys of incoming settings.
pub fn filter_settings(m: BTreeMap<String, SettingEntry>) -> BTreeMap<String, SettingEntry> {
    m.into_iter().filter(|(k, _)| is_shared_key(k)).collect()
}

fn read_json_value(path: &Path) -> Value {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

fn write_atomic(path: &Path, v: &impl Serialize) -> Result<(), String> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("INTERNAL: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v).map_err(|e| format!("INTERNAL: {e}"))?).map_err(|e| format!("INTERNAL: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("INTERNAL: {e}"))
}

fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Where `settings.json` values are read/written.
pub enum SettingsIo {
    /// Through netmode.rs (keeps its in-memory cache coherent). The app.
    App,
    /// Directly in `<dir>/settings.json` (tests).
    #[allow(dead_code)]
    Dir,
}

// ---------------------------------------------------------------- hub

#[derive(Debug, Clone, Default)]
struct Peer {
    online: bool,
    last_sync: Option<u64>,
    error: Option<String>,
    /// Next `since` for its `/v1/sync/state` (its clock).
    since: u64,
    /// Our clock at the start of the last successful push (0 = never: push all).
    last_push: u64,
    /// It contacted us while we thought it offline: sync soon.
    wanted: bool,
    last_full_reach: Option<Instant>,
}

pub struct SyncHub {
    dir: PathBuf,
    device: String,
    io: SettingsIo,
    resume: Mutex<BTreeMap<String, ResumeEntry>>,
    meta: Mutex<SyncFile>,
    peers: Mutex<HashMap<String, Peer>>,
    busy: tokio::sync::Mutex<()>,
    app: OnceLock<AppHandle>,
    #[cfg(test)]
    events: Mutex<Vec<Vec<String>>>,
}

impl SyncHub {
    pub fn new(dir: &Path, device: String, io: SettingsIo) -> Self {
        let resume: BTreeMap<String, ResumeEntry> =
            std::fs::read(dir.join("resume.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let meta: SyncFile = std::fs::read(dir.join("sync.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        SyncHub {
            dir: dir.to_path_buf(),
            device,
            io,
            resume: Mutex::new(resume),
            meta: Mutex::new(meta),
            peers: Mutex::new(HashMap::new()),
            busy: tokio::sync::Mutex::new(()),
            app: OnceLock::new(),
            #[cfg(test)]
            events: Mutex::new(Vec::new()),
        }
    }

    fn emit(&self, what: &[&str]) {
        if what.is_empty() {
            return;
        }
        #[cfg(test)]
        self.events.lock().unwrap().push(what.iter().map(|s| s.to_string()).collect());
        if let Some(app) = self.app.get() {
            if let Err(e) = app.emit("sync-updated", json!({ "what": what })) {
                tracing::debug!("emit sync-updated: {e}");
            }
        }
    }

    // ---- resume

    fn save_resume(&self, r: &BTreeMap<String, ResumeEntry>) -> Result<(), String> {
        write_atomic(&self.dir.join("resume.json"), r)
    }

    pub fn resume_get(&self, key: &str, idx: usize) -> Option<ResumeEntry> {
        let k = format!("{}:{idx}", key.to_ascii_lowercase());
        self.resume.lock().unwrap().get(&k).filter(|e| e.pos > 0.0).cloned()
    }

    /// `pos` None/0 clears (tombstone).
    pub fn resume_set(&self, key: &str, idx: usize, pos: Option<f64>, dur: Option<f64>) -> Result<(), String> {
        let k = format!("{}:{idx}", key.trim().to_ascii_lowercase());
        if !valid_entry_key(&k) {
            return Err("INVALID_INPUT: bad key".into());
        }
        let pos = pos.filter(|p| p.is_finite() && *p > 0.0).unwrap_or(0.0);
        let dur = dur.filter(|d| d.is_finite() && *d >= 0.0).unwrap_or(0.0);
        let mut r = self.resume.lock().unwrap();
        match r.get(&k) {
            None if pos == 0.0 => return Ok(()),
            Some(e) if e.pos == pos && (e.dur == dur || dur == 0.0) => return Ok(()),
            _ => {}
        }
        let dur = if dur == 0.0 { r.get(&k).map(|e| e.dur).unwrap_or(0.0) } else { dur };
        let mut next = r.clone();
        next.insert(k, ResumeEntry { pos, dur, updated_at: now_ms(), device: self.device.clone() });
        self.save_resume(&next)?;
        *r = next;
        Ok(())
    }

    pub fn resume_list(&self) -> Vec<Value> {
        self.resume
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| e.pos > 0.0)
            .filter_map(|(k, e)| {
                let (key, idx) = k.split_once(':')?;
                Some(json!({
                    "key": key, "file_index": idx.parse::<usize>().ok()?, "pos": e.pos, "dur": e.dur,
                    "updated_at": e.updated_at, "device": e.device,
                }))
            })
            .collect()
    }

    // ---- settings

    fn read_settings_obj(&self) -> serde_json::Map<String, Value> {
        let v = match self.io {
            SettingsIo::App => serde_json::to_value(crate::netmode::with_settings(|s| s.clone())).unwrap_or(Value::Null),
            SettingsIo::Dir => read_json_value(&self.dir.join("settings.json")),
        };
        match v {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        }
    }

    /// Current values of every shared key (missing = null).
    fn current(&self) -> BTreeMap<String, Value> {
        let s = self.read_settings_obj();
        let mut out: BTreeMap<String, Value> =
            SETTINGS_KEYS.iter().map(|k| (k.to_string(), s.get(*k).cloned().unwrap_or(Value::Null))).collect();
        let (w, unf) = crate::watcher::sync_get(&self.dir);
        out.insert(WATCHER_KEY.into(), w);
        out.insert(UNFOLLOWED_KEY.into(), json!(unf));
        out
    }

    fn write_setting(&self, key: &str, value: &Value) -> Result<(), String> {
        let set = |m: &mut serde_json::Map<String, Value>| {
            if value.is_null() {
                m.remove(key);
            } else {
                m.insert(key.to_string(), value.clone());
            }
        };
        match key {
            WATCHER_KEY => crate::watcher::sync_set(&self.dir, Some(value), None),
            UNFOLLOWED_KEY => {
                let list: Vec<String> = serde_json::from_value(value.clone()).map_err(|e| format!("INVALID_INPUT: {e}"))?;
                crate::watcher::sync_set(&self.dir, None, Some(list))
            }
            _ if SETTINGS_KEYS.contains(&key) => match self.io {
                SettingsIo::App => {
                    let mut err = None;
                    crate::netmode::update_settings(|s| {
                        let mut v = serde_json::to_value(&*s).unwrap_or(Value::Null);
                        if let Some(m) = v.as_object_mut() {
                            set(m);
                        }
                        match serde_json::from_value(v) {
                            Ok(n) => *s = n,
                            Err(e) => err = Some(format!("INVALID_INPUT: {key}: {e}")),
                        }
                    })?;
                    err.map_or(Ok(()), Err)
                }
                SettingsIo::Dir => {
                    let path = self.dir.join("settings.json");
                    let mut m = match read_json_value(&path) {
                        Value::Object(m) => m,
                        _ => serde_json::Map::new(),
                    };
                    set(&mut m);
                    write_atomic(&path, &Value::Object(m))
                }
            },
            _ => Err(format!("INVALID_INPUT: {key} is not shared")),
        }
    }

    fn source_mtime(&self, key: &str) -> u64 {
        let f = if SETTINGS_KEYS.contains(&key) { "settings.json" } else { "watcher.json" };
        mtime_ms(&self.dir.join(f))
    }

    /// Notice local settings edits: stamp keys whose value differs from the recorded one.
    /// Keys seen for the first time get the source file's mtime (an untouched default
    /// never beats a configured value on the other Mac). True if a key changed.
    pub fn capture_settings(&self) -> bool {
        let cur = self.current();
        let mut meta = self.meta.lock().unwrap();
        let mut dirty = false;
        let mut changed = false;
        for (k, v) in cur {
            match meta.settings.get(&k) {
                Some(e) if e.value == v => {}
                Some(_) => {
                    meta.settings.insert(k, SettingEntry { value: v, updated_at: now_ms(), device: self.device.clone() });
                    dirty = true;
                    changed = true;
                }
                None => {
                    let ts = self.source_mtime(&k);
                    meta.settings.insert(k, SettingEntry { value: v, updated_at: ts, device: self.device.clone() });
                    dirty = true;
                }
            }
        }
        if dirty {
            if let Err(e) = write_atomic(&self.dir.join("sync.json"), &*meta) {
                tracing::warn!("sync.json save failed: {e}");
            }
        }
        changed
    }

    // ---- export / apply

    /// Entries changed after `since` (0 = all).
    pub fn export(&self, since: u64) -> Result<Delta, String> {
        self.capture_settings();
        let ok = |ts: u64| since == 0 || ts > since;
        let (watched, groups, excluded) = groups::sync_export(&self.dir, since)?;
        let resume = self.resume.lock().unwrap().iter().filter(|(_, e)| ok(e.updated_at)).map(|(k, e)| (k.clone(), e.clone())).collect();
        let settings = self
            .meta
            .lock()
            .unwrap()
            .settings
            .iter()
            .filter(|(k, e)| is_shared_key(k) && ok(e.updated_at))
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        Ok(Delta { device: self.device.clone(), now: now_ms(), watched, resume, settings, groups, excluded })
    }

    /// Merge a remote delta. Returns what changed (and emits `sync-updated`).
    pub fn apply(&self, d: Delta) -> Result<Vec<&'static str>, String> {
        let mut what = Vec::new();
        let watched: WatchedFile = d.watched.into_iter().filter(|(k, _)| valid_entry_key(k)).collect();
        let (wc, gc) = groups::sync_apply(&self.dir, watched, d.groups, d.excluded)?;
        if wc {
            what.push("watched");
        }
        if gc {
            what.push("groups");
        }
        {
            let mut r = self.resume.lock().unwrap();
            let mut next = r.clone();
            if merge_resume(&mut next, d.resume) {
                self.save_resume(&next)?;
                *r = next;
                what.push("resume");
            }
        }
        // Settings: record local edits first so they compete with the remote ones.
        self.capture_settings();
        let mut settings_changed = false;
        for (k, r) in filter_settings(d.settings) {
            let take = {
                let meta = self.meta.lock().unwrap();
                match meta.settings.get(&k) {
                    None => true,
                    Some(l) => newer((l.updated_at, &l.device), (r.updated_at, &r.device)) && l.value != r.value,
                }
            };
            if !take {
                continue;
            }
            if let Err(e) = self.write_setting(&k, &r.value) {
                tracing::warn!("sync: setting {k} not applied: {e}");
                continue;
            }
            // Record what is actually stored now (normalised), with the remote stamp.
            let stored = self.current().remove(&k).unwrap_or(Value::Null);
            let mut meta = self.meta.lock().unwrap();
            meta.settings.insert(k, SettingEntry { value: stored, updated_at: r.updated_at, device: r.device });
            settings_changed = true;
        }
        if settings_changed {
            let meta = self.meta.lock().unwrap();
            write_atomic(&self.dir.join("sync.json"), &*meta)?;
            what.push("settings");
        }
        self.emit(&what);
        Ok(what)
    }

    fn mark_seen(&self, from: &str) {
        let mut p = self.peers.lock().unwrap();
        let e = p.entry(from.to_string()).or_default();
        if !e.online {
            e.online = true;
            e.wanted = true;
        }
    }

    /// Server side of `GET /v1/sync/state`.
    pub fn serve_state(&self, from: &str, since: u64) -> Result<Value, String> {
        self.mark_seen(from);
        serde_json::to_value(self.export(since)?).map_err(|e| format!("INTERNAL: {e}"))
    }

    /// Server side of `POST /v1/sync/push`.
    pub fn serve_push(&self, from: &str, body: Value) -> Result<Value, String> {
        self.mark_seen(from);
        let d: Delta = serde_json::from_value(body).map_err(|e| format!("INVALID_INPUT: {e}"))?;
        let what = self.apply(d)?;
        Ok(json!({ "ok": true, "applied": what }))
    }

    // ---- client

    /// One cycle with a reachable peer: pull, merge, push.
    pub async fn sync_peer(&self, rs: &RemoteState, dev: &Device, addr: std::net::SocketAddr) -> Result<Vec<&'static str>, String> {
        let me = rs.self_id();
        let (since, last_push) = {
            let p = self.peers.lock().unwrap();
            p.get(&dev.id).map(|x| (x.since, x.last_push)).unwrap_or((0, 0))
        };
        let v = remote::get_json(rs, addr, &format!("/v1/sync/state?since={since}"), (&me, &dev.secret), TIMEOUT).await?;
        let d: Delta = serde_json::from_value(v).map_err(|e| format!("REMOTE_ERROR: bad sync state: {e}"))?;
        let their_now = d.now;
        let what = self.apply(d)?;

        let started = now_ms();
        let out = self.export(if last_push == 0 { 0 } else { last_push.saturating_sub(SINCE_MARGIN).max(1) })?;
        let body = serde_json::to_value(&out).map_err(|e| format!("INTERNAL: {e}"))?;
        remote::post_json(rs, addr, "/v1/sync/push", Some((&me, &dev.secret)), body, TIMEOUT).await?;

        let mut p = self.peers.lock().unwrap();
        let e = p.entry(dev.id.clone()).or_default();
        e.since = their_now.saturating_sub(SINCE_MARGIN).max(1);
        e.last_push = started;
        e.last_sync = Some(now_ms());
        e.error = None;
        e.wanted = false;
        Ok(what)
    }

    fn set_online(&self, id: &str, online: bool, error: Option<String>) -> bool {
        let mut p = self.peers.lock().unwrap();
        let e = p.entry(id.to_string()).or_default();
        let flipped = e.online != online;
        e.online = online;
        if !online {
            // Next contact: full pull/push.
            e.since = 0;
            e.last_push = 0;
        }
        if error.is_some() || online {
            e.error = error;
        }
        flipped
    }

    /// Sync with paired devices. `full_reach`: try Bonjour for unreachable ones;
    /// `force`: ignore the 60 s period.
    async fn run(&self, rs: &RemoteState, full_reach: bool, force: bool) {
        let _g = self.busy.lock().await;
        let mut library_changed = false;
        for dev in rs.devices() {
            let (due, wanted, reach_due) = {
                let p = self.peers.lock().unwrap();
                let e = p.get(&dev.id).cloned().unwrap_or_default();
                let due = e.last_sync.is_none_or(|t| now_ms().saturating_sub(t) >= PERIOD.as_millis() as u64);
                let reach_due = e.last_full_reach.is_none_or(|t| t.elapsed() >= FULL_REACH_EVERY);
                (due || !e.online, e.wanted, reach_due)
            };
            if !(force || due || wanted) {
                continue;
            }
            let mut addr = remote::quick_addr(&dev, 800).await;
            if addr.is_none() && (full_reach || reach_due) {
                self.peers.lock().unwrap().entry(dev.id.clone()).or_default().last_full_reach = Some(Instant::now());
                addr = remote::reach(rs, &dev).await.ok();
            }
            let Some(addr) = addr else {
                library_changed |= self.set_online(&dev.id, false, None);
                continue;
            };
            library_changed |= self.set_online(&dev.id, true, None);
            match self.sync_peer(rs, &dev, addr).await {
                Ok(what) if !what.is_empty() => tracing::info!(device = %dev.name, ?what, "sync: applied remote changes"),
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(device = %dev.name, "sync failed: {e}");
                    let mut p = self.peers.lock().unwrap();
                    p.entry(dev.id.clone()).or_default().error = Some(e);
                }
            }
        }
        if library_changed {
            self.emit(&["library"]);
        }
    }

    /// Run due syncs (local changes debounced, wanted peers, 60 s period).
    async fn tick(&self, rs: &RemoteState, n: u64, handled: &mut u64) {
        if n % 3 == 0 && self.capture_settings() {
            touch();
        }
        let dirty = DIRTY.load(Ordering::SeqCst);
        let local = dirty > *handled && now_ms().saturating_sub(dirty) >= DEBOUNCE_MS;
        let wanted = self.peers.lock().unwrap().values().any(|p| p.wanted);
        if local {
            *handled = dirty;
            self.run(rs, false, true).await;
        } else if wanted || n % PROBE_EVERY == 0 {
            self.run(rs, false, false).await;
        }
    }

    pub fn status(&self, rs: &RemoteState) -> Value {
        let p = self.peers.lock().unwrap();
        let devices: Vec<Value> = rs
            .devices()
            .into_iter()
            .map(|d| {
                let e = p.get(&d.id).cloned().unwrap_or_default();
                json!({ "id": d.id, "name": d.name, "online": e.online, "last_sync": e.last_sync, "error": e.error })
            })
            .collect();
        json!({ "devices": devices })
    }
}

/// Startup: remember this Mac's id, then the background loop.
pub fn spawn(app: AppHandle, hub: Arc<SyncHub>, rs: Arc<RemoteState>) {
    let _ = DEVICE.set(hub.device.clone());
    let _ = hub.app.set(app);
    tauri::async_runtime::spawn(async move {
        // Let the LAN server come up first.
        tokio::time::sleep(Duration::from_secs(3)).await;
        hub.run(&rs, true, true).await;
        let mut iv = tokio::time::interval(Duration::from_secs(1));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut n = 0u64;
        let mut handled = DIRTY.load(Ordering::SeqCst);
        loop {
            iv.tick().await;
            n += 1;
            hub.tick(&rs, n, &mut handled).await;
        }
    });
}

/// Set the device id early (before any command can stamp entries).
pub fn init_device(id: &str) {
    let _ = DEVICE.set(id.to_string());
}

// ---------------------------------------------------------------- commands

type Hub<'a> = State<'a, Arc<SyncHub>>;
type Rs<'a> = State<'a, Arc<RemoteState>>;

/// JS: `{ key, fileIndex }` → `{pos, dur, updated_at}` | null.
#[tauri::command]
pub fn resume_get(hub: Hub<'_>, key: String, file_index: usize) -> Result<Option<Value>, String> {
    Ok(hub.resume_get(&key, file_index).map(|e| json!({ "pos": e.pos, "dur": e.dur, "updated_at": e.updated_at })))
}

/// JS: `{ key, fileIndex, pos, dur }` (pos 0/null clears).
#[tauri::command]
pub fn resume_set(hub: Hub<'_>, key: String, file_index: usize, pos: Option<f64>, dur: Option<f64>) -> Result<(), String> {
    hub.resume_set(&key, file_index, pos, dur)?;
    touch();
    Ok(())
}

/// `[{key, file_index, pos, dur, updated_at, device}]`.
#[tauri::command]
pub fn resume_list(hub: Hub<'_>) -> Result<Vec<Value>, String> {
    Ok(hub.resume_list())
}

/// `{devices: [{id, name, online, last_sync (unix ms)|null, error}]}`.
#[tauri::command]
pub fn sync_status(hub: Hub<'_>, rs: Rs<'_>) -> Result<Value, String> {
    Ok(hub.status(&rs))
}

/// Sync with every paired device now; returns `sync_status`.
#[tauri::command]
pub async fn sync_now(hub: Hub<'_>, rs: Rs<'_>) -> Result<Value, String> {
    let hub: Arc<SyncHub> = hub.inner().clone();
    let rs: Arc<RemoteState> = rs.inner().clone();
    hub.run(&rs, true, true).await;
    hub.emit(&["library"]);
    Ok(hub.status(&rs))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{BoxFut, Backend, Opened, PlayRequest};
    use crate::types::GroupsFile;

    const K1: &str = "1111111111111111111111111111111111111111";
    const K2: &str = "2222222222222222222222222222222222222222";

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tide-sync-{tag}-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn we(watched: bool, ts: u64, dev: &str) -> groups::WatchedEntry {
        groups::WatchedEntry { watched, updated_at: ts, device: dev.into() }
    }

    fn re(pos: f64, ts: u64, dev: &str) -> ResumeEntry {
        ResumeEntry { pos, dur: 100.0, updated_at: ts, device: dev.into() }
    }

    #[test]
    fn lww_merge_with_tombstones() {
        // Watched: newer unmark (tombstone) beats older mark; older remote ignored.
        let mut a: WatchedFile = [(format!("{K1}:0"), we(true, 100, "a")), (format!("{K1}:1"), we(true, 300, "a"))].into();
        let b: WatchedFile = [(format!("{K1}:0"), we(false, 200, "b")), (format!("{K1}:1"), we(false, 250, "b"))].into();
        assert!(groups::merge_watched(&mut a, b));
        assert!(!a[&format!("{K1}:0")].watched);
        assert!(a[&format!("{K1}:1")].watched);
        // Equal stamps: larger device id wins on both sides.
        let mut x: WatchedFile = [("k".into(), we(true, 5, "aaa"))].into();
        let y: WatchedFile = [("k".into(), we(false, 5, "bbb"))].into();
        assert!(groups::merge_watched(&mut x, y.clone()));
        assert!(!x["k"].watched);
        let mut y2 = y.clone();
        assert!(!groups::merge_watched(&mut y2, [("k".into(), we(true, 5, "aaa"))].into()));

        // Resume: tombstone (pos 0) newer than a position clears it; bad keys dropped.
        let mut r: BTreeMap<String, ResumeEntry> = [(format!("{K1}:0"), re(50.0, 100, "a"))].into();
        let incoming: BTreeMap<String, ResumeEntry> =
            [(format!("{K1}:0"), re(0.0, 200, "b")), (format!("{K2}:3"), re(10.0, 1, "b")), ("../x".into(), re(1.0, 9, "b"))].into();
        assert!(merge_resume(&mut r, incoming));
        assert_eq!(r[&format!("{K1}:0")].pos, 0.0);
        assert_eq!(r[&format!("{K2}:3")].pos, 10.0);
        assert!(!r.contains_key("../x"));
        assert!(!merge_resume(&mut r, [(format!("{K1}:0"), re(70.0, 150, "c"))].into()));

        // Groups: delete tombstone beats an older rename; a newer rename beats an older delete.
        let mut g = GroupsFile::default();
        let mk = |id: &str, name: &str, ts: u64, del: bool, m: &[&str]| Group {
            id: id.into(),
            name: name.into(),
            members: m.iter().map(|s| s.to_string()).collect(),
            updated_at: ts,
            deleted: del,
            ..Default::default()
        };
        g.groups = vec![mk("g1", "One", 100, false, &[K1]), mk("g2", "Two", 100, false, &[K2])];
        let changed = groups::merge_groups(
            &mut g,
            vec![mk("g1", "One!", 90, false, &[K1]), mk("g2", "", 200, true, &[]), mk("g3", "Three", 300, false, &[K1])],
            [(K2.to_string(), ExcludedEntry { excluded: true, updated_at: 5 })].into(),
        );
        assert!(changed);
        assert_eq!(g.groups[0].name, "One");
        assert!(g.groups[1].deleted);
        // K1 moved to the newer g3 (one live group per key).
        assert!(g.groups[0].members.is_empty());
        assert_eq!(g.groups[2].members, vec![K1.to_string()]);
        assert_eq!(g.excluded.get(K2), Some(&true));
        let changed = groups::merge_groups(&mut g, vec![mk("g2", "Two again", 300, false, &[])], [(K2.to_string(), ExcludedEntry { excluded: false, updated_at: 4 })].into());
        assert!(changed);
        assert!(!g.groups[1].deleted && g.groups[1].name == "Two again");
        assert_eq!(g.excluded.get(K2), Some(&true), "older include ignored");
    }

    #[test]
    fn local_group_ops_stamp_and_tombstone() {
        let dir = tmpdir("gstamp");
        groups::test_mutate_groups(&dir, |g| {
            groups::test_create(g, "a", "A", &[K1]);
            Ok(())
        })
        .unwrap();
        groups::test_mutate_groups(&dir, |g| {
            groups::test_delete(g, "a");
            Ok(())
        })
        .unwrap();
        let (_, exported, _) = groups::sync_export(&dir, 0).unwrap();
        assert_eq!(exported.len(), 1);
        assert!(exported[0].deleted && exported[0].updated_at > 0);
        let (visible, _) = groups::snapshot_in(&dir).unwrap();
        assert!(visible.groups.is_empty());
    }

    #[test]
    fn settings_key_filtering() {
        let dir = tmpdir("keys");
        std::fs::write(
            dir.join("settings.json"),
            r#"{"netmode":"limit","limit_kbps":300,"secret_thing":"x","subtitle_langs":["tur"]}"#,
        )
        .unwrap();
        let hub = SyncHub::new(&dir, "a".repeat(32), SettingsIo::Dir);
        let d = hub.export(0).unwrap();
        let keys: Vec<&str> = d.settings.keys().map(String::as_str).collect();
        assert!(keys.iter().all(|k| is_shared_key(k)), "{keys:?}");
        assert!(!keys.contains(&"secret_thing"));
        assert_eq!(d.settings["netmode"].value, json!("limit"));
        assert!(d.settings.contains_key(WATCHER_KEY) && d.settings.contains_key(UNFOLLOWED_KEY));

        let far = now_ms() + 10_000;
        let mut incoming = Delta::default();
        for (k, v) in [
            ("netmode", json!("off")),
            ("secret_thing", json!("evil")),
            ("devices", json!([{"id": "x"}])),
            ("self_id", json!("0")),
            ("remote.json", json!({})),
        ] {
            incoming.settings.insert(k.into(), SettingEntry { value: v, updated_at: far, device: "b".repeat(32) });
        }
        let what = hub.apply(incoming).unwrap();
        assert_eq!(what, vec!["settings"]);
        let s = read_json_value(&dir.join("settings.json"));
        assert_eq!(s["netmode"], json!("off"));
        assert_eq!(s["secret_thing"], json!("x"));
        assert!(s.get("devices").is_none() && s.get("self_id").is_none() && s.get("remote.json").is_none());
        // Remote auto-add on: this Mac must not auto-add.
        let mut w = Delta::default();
        w.settings.insert(
            WATCHER_KEY.into(),
            SettingEntry {
                value: json!({"torznab_url": "http://x/api", "api_key": "k", "auto_add": true, "quality": "720p", "auto_add_device": "b".repeat(32)}),
                updated_at: far,
                device: "b".repeat(32),
            },
        );
        hub.apply(w).unwrap();
        let (v, _) = crate::watcher::sync_get(&dir);
        assert_eq!(v["auto_add_device"], json!("b".repeat(32)));
        assert_eq!(v["quality"], json!("720p"));
        // A local edit after that is detected and newer.
        let mut m = match read_json_value(&dir.join("settings.json")) {
            Value::Object(m) => m,
            _ => unreachable!(),
        };
        m.insert("limit_kbps".into(), json!(999));
        write_atomic(&dir.join("settings.json"), &Value::Object(m)).unwrap();
        assert!(hub.capture_settings());
        assert!(hub.meta.lock().unwrap().settings["limit_kbps"].updated_at >= far - 10_000);
    }

    // ---- two in-process servers on loopback

    struct TestBe {
        hub: Arc<SyncHub>,
    }

    impl Backend for TestBe {
        fn open(&self, _k: String, _i: usize) -> BoxFut<'_, Result<Opened, String>> {
            Box::pin(async { Err("no".to_string()) })
        }
        fn sub_vtt(&self, _k: String, _i: usize) -> BoxFut<'_, Result<String, String>> {
            Box::pin(async { Err("no".to_string()) })
        }
        fn play(&self, _r: PlayRequest) -> BoxFut<'_, Result<(), String>> {
            Box::pin(async { Err("no".to_string()) })
        }
        fn emit(&self, _e: &str, _p: Value) {}
        fn sync_state(&self, from: &str, since: u64) -> Result<Value, String> {
            self.hub.serve_state(from, since)
        }
        fn sync_push(&self, from: &str, body: Value) -> Result<Value, String> {
            self.hub.serve_push(from, body)
        }
    }

    struct Node {
        dir: PathBuf,
        hub: Arc<SyncHub>,
        rs: Arc<RemoteState>,
        addr: std::net::SocketAddr,
    }

    async fn node(tag: &str) -> Node {
        let dir = tmpdir(tag);
        let rs = Arc::new(RemoteState::load(&dir, format!("Mac {tag}")));
        let hub = Arc::new(SyncHub::new(&dir, rs.self_id(), SettingsIo::Dir));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        remote::serve_on(l, rs.clone(), Arc::new(TestBe { hub: hub.clone() }));
        Node { dir, hub, rs, addr }
    }

    fn pair(a: &Node, b: &Node, secret: &str) {
        for (x, y) in [(a, b), (b, a)] {
            x.rs.upsert_device(Device {
                id: y.rs.self_id(),
                name: y.rs.name().into(),
                host: "127.0.0.1".into(),
                port: y.addr.port(),
                secret: secret.into(),
                last_ip: None,
            });
        }
    }

    #[tokio::test]
    async fn two_macs_converge() {
        let a = node("a").await;
        let b = node("b").await;
        let secret = "s".repeat(64);
        pair(&a, &b, &secret);
        let (ida, idb) = (a.rs.self_id(), b.rs.self_id());

        // Unauthenticated / wrong secret: refused.
        let dev_b = a.rs.device(&idb).unwrap();
        let e = remote::get_json(&a.rs, b.addr, "/v1/sync/state?since=0", (&ida, "wrong"), TIMEOUT).await.unwrap_err();
        assert!(e.starts_with("UNAUTHORIZED"), "{e}");
        let e = remote::post_json(&a.rs, b.addr, "/v1/sync/push", Some((&"f".repeat(32), &secret)), json!({}), TIMEOUT)
            .await
            .unwrap_err();
        assert!(e.starts_with("UNAUTHORIZED"), "{e}");

        // A: watched K1:0, resume K1:0 at 42 s, a group, netmode limit.
        groups::test_set_watched(&a.dir, K1, 0, true, &ida).unwrap();
        a.hub.resume_set(K1, 0, Some(42.0), Some(3000.0)).unwrap();
        groups::test_mutate_groups(&a.dir, |g| {
            groups::test_create(g, "ga", "Show", &[K1, K2]);
            Ok(())
        })
        .unwrap();
        write_atomic(&a.dir.join("settings.json"), &json!({"netmode": "limit", "limit_kbps": 128})).unwrap();
        a.hub.capture_settings();
        a.hub.meta.lock().unwrap().settings.get_mut("netmode").unwrap().updated_at = now_ms();
        // B: watched K2:1, an older resume for K1:0, its own private remote.json untouched.
        tokio::time::sleep(Duration::from_millis(5)).await;
        groups::test_set_watched(&b.dir, K2, 1, true, &idb).unwrap();
        b.hub.resume.lock().unwrap().insert(format!("{K1}:0"), re(5.0, 1, &idb));

        let what = a.hub.sync_peer(&a.rs, &dev_b, b.addr).await.unwrap();
        assert!(what.contains(&"watched"), "{what:?}");

        for n in [&a, &b] {
            let (g, w) = groups::snapshot_in(&n.dir).unwrap();
            assert_eq!(w.get(&format!("{K1}:0")), Some(&true));
            assert_eq!(w.get(&format!("{K2}:1")), Some(&true));
            assert_eq!(g.groups.len(), 1);
            assert_eq!(g.groups[0].members, vec![K1.to_string(), K2.to_string()]);
            assert_eq!(n.hub.resume_get(K1, 0).unwrap().pos, 42.0);
        }
        let sb = read_json_value(&b.dir.join("settings.json"));
        assert_eq!(sb["netmode"], json!("limit"));
        assert!(b.hub.events.lock().unwrap().iter().flatten().any(|w| w == "settings"));

        // B unmarks + clears resume + deletes the group; sync from B's side this time.
        tokio::time::sleep(Duration::from_millis(5)).await;
        groups::test_set_watched(&b.dir, K1, 0, false, &idb).unwrap();
        b.hub.resume_set(K1, 0, Some(0.0), None).unwrap();
        groups::test_mutate_groups(&b.dir, |g| {
            groups::test_delete(g, "ga");
            Ok(())
        })
        .unwrap();
        let dev_a = b.rs.device(&ida).unwrap();
        b.hub.sync_peer(&b.rs, &dev_a, a.addr).await.unwrap();
        let (g, w) = groups::snapshot_in(&a.dir).unwrap();
        assert!(!w.contains_key(&format!("{K1}:0")));
        assert!(g.groups.is_empty());
        assert!(a.hub.resume_get(K1, 0).is_none());
        assert!(a.hub.resume_list().is_empty());
        // Incremental second round is a no-op.
        assert!(b.hub.sync_peer(&b.rs, &dev_a, a.addr).await.unwrap().is_empty());
        // Pairing data never travelled.
        let ra = read_json_value(&a.dir.join("remote.json"));
        assert_eq!(ra["selfId"], json!(ida));
    }
}
