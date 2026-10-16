//! Archive moves (docs/FEATURE-ARCHIVE.md): finished downloads go from this Mac (the
//! sender, usually the MacBook) to a paired "archive" Mac (the receiver: a Mac mini with
//! an external SSD) over the authenticated LAN server of remote.rs. LAN only, so it also
//! runs on metered networks.
//!
//! Receiver (`settings.json` `archive_dir`; default `/Volumes/NEO Storage/Tidebox` when
//! that volume is mounted, "" = none). Endpoints (Bearer + `X-Tide-Id`):
//! - `GET /v1/archive/info` -> `{archive_dir, available, free_bytes, error}`
//! - `POST /v1/archive/begin` `BeginReq` -> `{upload_id, have: {rel_path: bytes}}` (or
//!   `{already: true}` when this Mac already has the complete torrent). Staging lives in
//!   `<archive_dir>/.tide-incoming/<upload_id>/` (same volume: the final move is a rename);
//!   the upload id is derived from (sender, key), so a move resumes after restarts.
//! - `PUT /v1/archive/<upload_id>/<rel_path>` with `Content-Range: bytes a-b/size`:
//!   append-only into `<rel>.partial` (a must equal what is there; 409 + `have` otherwise),
//!   renamed to `<rel>` once complete.
//! - `POST /v1/archive/<upload_id>/commit`: verifies every size + SHA-256, moves the folder
//!   to `<archive_dir>/<unique clean name>`, re-adds the torrent from its bytes (paused,
//!   checked complete, never seeds) or registers a done yt-dlp job. Ok only after that.
//!
//! Sender: setting `archive_auto` (default on; per device, never synced). A background
//! loop moves completed torrents and done yt-dlp jobs one at a time while the archive Mac
//! is online, skipping what is playing or has resume progress from the last minute.
//! `archive_move {key|ytId}` queues one item by hand. Local files are deleted (remove with
//! files, incl. sidecars + online subtitles) only after the receiver's commit said ok.
//! Event `archive-progress` (= `archive_status`).
//!
//! Auto-clean (archive Mac): `autoclean_days` (default 30 when an archive location is set,
//! 0 = off) applies cleanup.rs on start and daily without asking, then notifies.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::remote::{self, BoxFut, Device, RemoteState};
use crate::state::AppState;
use crate::types::{MediaRef, YtPreset};

/// Largest accepted chunk body (remote.rs body limit).
pub const MAX_CHUNK: usize = 8 * 1024 * 1024;
const CHUNK: usize = 4 * 1024 * 1024;
const INCOMING: &str = ".tide-incoming";
pub const DEFAULT_VOLUME: &str = "/Volumes/NEO Storage";
const RECENT_RESUME_MS: u64 = 60_000;
const LOOP_EVERY: Duration = Duration::from_secs(30);
const RETRY_AFTER: Duration = Duration::from_secs(600);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(120);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(3 * 3600);
const CLEAN_EVERY: Duration = Duration::from_secs(24 * 3600);

fn internal<E: std::fmt::Display>(e: E) -> String {
    format!("INTERNAL: {e}")
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha256_bytes(data: &[u8]) -> String {
    use librqbit_sha1_wrapper::{ISha256, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finish())
}

/// SHA-256 (hex) of a file.
pub(crate) fn sha256_file(p: &Path) -> std::io::Result<String> {
    use librqbit_sha1_wrapper::{ISha256, Sha256};
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finish()))
}

/// "6.4 GB" (decimal, like the UI).
fn gb(bytes: u64) -> String {
    let g = bytes as f64 / 1e9;
    if g >= 10.0 {
        format!("{g:.0} GB")
    } else if g >= 0.1 {
        format!("{g:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

pub(crate) fn freed_text(bytes: u64, items: usize) -> String {
    format!("Freed {} ({items} item{})", gb(bytes), if items == 1 { "" } else { "s" })
}

// ---------------------------------------------------------------- settings (per Mac)

fn setting(key: &str) -> Option<Value> {
    crate::netmode::with_settings(|s| s.extra.get(key).cloned())
}

fn set_setting(key: &str, v: Value) -> Result<(), String> {
    crate::netmode::update_settings(|s| {
        s.extra.insert(key.to_string(), v);
    })
    .map(|_| ())
}

pub fn default_archive_dir() -> Option<PathBuf> {
    Path::new(DEFAULT_VOLUME).is_dir().then(|| Path::new(DEFAULT_VOLUME).join("Tidebox"))
}

/// Configured archive location: missing key = default, "" = none.
pub fn archive_dir_setting() -> Option<PathBuf> {
    match setting("archive_dir") {
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) => Some(PathBuf::from(s.trim())),
        _ => default_archive_dir(),
    }
}

fn archive_auto_setting() -> bool {
    setting("archive_auto").and_then(|v| v.as_bool()).unwrap_or(true)
}

fn autoclean_setting() -> Option<u64> {
    setting("autoclean_days").and_then(|v| v.as_u64())
}

/// Days for auto-clean, None = off. Only an archive Mac auto-cleans (default 30).
pub(crate) fn effective_autoclean(setting: Option<u64>, has_archive: bool) -> Option<u64> {
    if !has_archive {
        return None;
    }
    match setting.unwrap_or(30) {
        0 => None,
        n => Some(n),
    }
}

/// `/Volumes/<name>` of a path on an external volume, else its parent.
fn volume_root(dir: &Path) -> PathBuf {
    let mut c = dir.components();
    let first: Vec<_> = c.by_ref().take(3).collect();
    if first.len() == 3 && first[1].as_os_str() == "Volumes" {
        return first.iter().collect();
    }
    dir.parent().map(Path::to_path_buf).unwrap_or_else(|| dir.to_path_buf())
}

/// The archive folder, created if its volume is there. Never creates a mount point.
pub fn check_dir(dir: &Path) -> Result<PathBuf, String> {
    let vol = volume_root(dir);
    if !vol.is_dir() {
        let name = vol.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| vol.display().to_string());
        return Err(format!("ARCHIVE_UNAVAILABLE: {name} not connected"));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("ARCHIVE_UNAVAILABLE: cannot create {}: {e}", dir.display()))?;
    Ok(dir.to_path_buf())
}

/// Free bytes on the volume of `dir` (`df -Pk`).
pub fn free_bytes(dir: &Path) -> Option<u64> {
    let o = std::process::Command::new("/bin/df").arg("-Pk").arg(dir).output().ok()?;
    let out = String::from_utf8_lossy(&o.stdout);
    let line = out.lines().nth(1)?;
    let kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb * 1024)
}

// ---------------------------------------------------------------- wire format

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireFile {
    pub rel_path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnlineSub {
    pub rel_path: String,
    pub file_index: usize,
    pub lang: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YtMeta {
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub thumbnail: Option<String>,
    pub preset: YtPreset,
    /// The media file among `files`.
    pub file_rel: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeginReq {
    /// Torrent info hash; for yt items a hash of url + file name (40 hex).
    pub key: String,
    /// "torrent" | "yt"
    pub kind: String,
    pub name: String,
    pub files: Vec<WireFile>,
    #[serde(default)]
    pub torrent_bytes_b64: Option<String>,
    #[serde(default)]
    pub only_files: Option<Vec<usize>>,
    /// Our sidecars among `files` (tide.json, poster.jpg, …).
    #[serde(default)]
    pub sidecars: Vec<String>,
    #[serde(default)]
    pub online_subs: Vec<OnlineSub>,
    #[serde(default)]
    pub yt: Option<YtMeta>,
}

/// A relative path that stays inside its folder.
pub fn valid_rel(rel: &str) -> bool {
    !rel.is_empty()
        && rel.len() <= 1024
        && !rel.starts_with('/')
        && !rel.contains('\0')
        && !rel.contains('\\')
        && !rel.ends_with(".partial")
        && rel.split('/').all(|c| !c.is_empty() && c != "." && c != ".." && c.len() <= 255)
}

/// `bytes a-b/total` -> (a, b, total).
pub fn parse_content_range(h: &str) -> Option<(u64, u64, u64)> {
    let r = h.trim().strip_prefix("bytes ")?;
    let (span, total) = r.split_once('/')?;
    let (a, b) = span.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?, total.trim().parse().ok()?))
}

impl BeginReq {
    fn validate(&self) -> Result<(), String> {
        let bad = |m: &str| Err(format!("INVALID_INPUT: {m}"));
        if !is_hex(&self.key, 40) {
            return bad("bad key");
        }
        if self.name.trim().is_empty() || self.name.len() > 1024 {
            return bad("bad name");
        }
        if self.files.is_empty() || self.files.len() > 100_000 {
            return bad("no files");
        }
        let mut seen = HashSet::new();
        for f in &self.files {
            if !valid_rel(&f.rel_path) {
                return Err(format!("INVALID_INPUT: bad path {:?}", f.rel_path));
            }
            if !is_hex(&f.sha256, 64) {
                return bad("bad sha256");
            }
            if !seen.insert(f.rel_path.as_str()) {
                return bad("duplicate path");
            }
        }
        // A file must not also be a folder of another one ("a" and "a/b").
        for f in &self.files {
            let mut p = f.rel_path.as_str();
            while let Some((parent, _)) = p.rsplit_once('/') {
                if seen.contains(parent) {
                    return bad("path is both a file and a folder");
                }
                p = parent;
            }
        }
        let has = |r: &str| seen.contains(r);
        if !self.sidecars.iter().all(|s| has(s)) || !self.online_subs.iter().all(|s| has(&s.rel_path)) {
            return bad("sidecar not among files");
        }
        match self.kind.as_str() {
            "torrent" => {
                let ok = self.torrent_bytes_b64.as_deref().and_then(remote::base64_decode).is_some_and(|b| !b.is_empty());
                if !ok {
                    return bad("missing torrent bytes");
                }
            }
            "yt" => match &self.yt {
                Some(y) if has(&y.file_rel) => {}
                _ => return bad("missing yt file"),
            },
            _ => return bad("bad kind"),
        }
        Ok(())
    }

    fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

// ---------------------------------------------------------------- receiver

/// Puts a verified folder into this Mac's library (mocked in tests).
pub(crate) trait Placer: Send + Sync + 'static {
    /// Does this Mac already hold the complete item?
    fn has_complete(&self, m: &BeginReq) -> bool;
    fn place<'a>(&'a self, m: &'a BeginReq, dest: &'a Path) -> BoxFut<'a, Result<(), String>>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    from: String,
    req: BeginReq,
}

pub struct Receiver {
    dir: Mutex<Option<PathBuf>>,
    placer: Arc<dyn Placer>,
    /// begin/commit one at a time.
    gate: tokio::sync::Mutex<()>,
    /// (upload, rel) being written.
    writing: Arc<Mutex<HashSet<(String, String)>>>,
}

struct WriteGuard {
    set: Arc<Mutex<HashSet<(String, String)>>>,
    k: (String, String),
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.k);
    }
}

fn upload_id(from: &str, key: &str) -> String {
    sha256_bytes(format!("{from}\n{key}").as_bytes())[..32].to_string()
}

fn partial_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

/// Bytes of `f` already in `stage` (stale or oversized leftovers are dropped).
fn have_of(stage: &Path, f: &WireFile) -> u64 {
    let fin = stage.join("data").join(&f.rel_path);
    if let Ok(m) = std::fs::metadata(&fin) {
        if m.is_file() && m.len() == f.size {
            return f.size;
        }
        let _ = std::fs::remove_file(&fin);
    }
    let part = partial_of(&fin);
    match std::fs::metadata(&part) {
        Ok(m) if m.len() <= f.size => m.len(),
        Ok(_) => {
            let _ = std::fs::remove_file(&part);
            0
        }
        Err(_) => 0,
    }
}

fn load_manifest(stage: &Path) -> Option<Manifest> {
    std::fs::read(stage.join("manifest.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

impl Receiver {
    pub(crate) fn new(dir: Option<PathBuf>, placer: Arc<dyn Placer>) -> Self {
        Receiver {
            dir: Mutex::new(dir),
            placer,
            gate: tokio::sync::Mutex::new(()),
            writing: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn set_dir(&self, d: Option<PathBuf>) {
        *self.dir.lock().unwrap() = d;
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.dir.lock().unwrap().clone()
    }

    fn root(&self) -> Result<PathBuf, String> {
        let d = self.dir().ok_or("NOT_ARCHIVE: this Mac has no archive location")?;
        check_dir(&d)
    }

    pub fn info(&self) -> Value {
        match self.dir() {
            None => json!({ "archive_dir": null, "available": false }),
            Some(d) => match check_dir(&d) {
                Ok(d) => json!({ "archive_dir": d, "available": true, "free_bytes": free_bytes(&d) }),
                Err(e) => json!({ "archive_dir": d, "available": false, "error": e }),
            },
        }
    }

    pub async fn begin(&self, from: &str, mut req: BeginReq) -> Result<Value, String> {
        req.key = req.key.to_ascii_lowercase();
        req.validate()?;
        let root = self.root()?;
        let _g = self.gate.lock().await;
        let id = upload_id(from, &req.key);
        if self.placer.has_complete(&req) {
            return Ok(json!({ "upload_id": id, "have": {}, "already": true }));
        }
        let stage = root.join(INCOMING).join(&id);
        if let Some(old) = load_manifest(&stage) {
            if old.from != from || old.req.files != req.files {
                let _ = std::fs::remove_dir_all(&stage);
            }
        }
        let mut have = BTreeMap::new();
        let mut present = 0u64;
        for f in &req.files {
            let h = have_of(&stage, f);
            present += h;
            have.insert(f.rel_path.clone(), h);
        }
        if let Some(free) = free_bytes(&root) {
            let need = req.total().saturating_sub(present);
            if free < need.saturating_add(256 * 1024 * 1024) {
                return Err(format!("ARCHIVE_FULL: needs {}, {} free", gb(need), gb(free)));
            }
        }
        std::fs::create_dir_all(stage.join("data")).map_err(internal)?;
        let m = Manifest { from: from.to_string(), req };
        std::fs::write(stage.join("manifest.json"), serde_json::to_vec(&m).map_err(internal)?).map_err(internal)?;
        Ok(json!({ "upload_id": id, "have": have }))
    }

    /// Append one chunk. Ok = bytes of the file now present; Err = (error, have).
    pub async fn put(&self, from: &str, upload: &str, rel: &str, range: (u64, u64, u64), body: &[u8]) -> Result<u64, (String, u64)> {
        let e0 = |m: String| (m, 0u64);
        if !is_hex(upload, 32) {
            return Err(e0("NOT_FOUND: no such upload".into()));
        }
        if !valid_rel(rel) {
            return Err(e0(format!("INVALID_INPUT: bad path {rel:?}")));
        }
        let root = self.root().map_err(e0)?;
        let stage = root.join(INCOMING).join(upload);
        let m = load_manifest(&stage).ok_or_else(|| e0("NOT_FOUND: no such upload".into()))?;
        if m.from != from {
            return Err(e0("UNAUTHORIZED: not your upload".into()));
        }
        let f = m.req.files.iter().find(|f| f.rel_path == rel).ok_or_else(|| e0("NOT_FOUND: not part of this upload".into()))?.clone();
        let (start, end, total) = range;
        if total != f.size || end < start || end >= total || body.len() as u64 != end - start + 1 {
            return Err(e0("INVALID_INPUT: bad range".into()));
        }
        let k = (upload.to_string(), rel.to_string());
        if !self.writing.lock().unwrap().insert(k.clone()) {
            return Err(e0("BUSY: chunk already being written".into()));
        }
        let _guard = WriteGuard { set: self.writing.clone(), k };
        let have = have_of(&stage, &f);
        if have == f.size {
            return Ok(have);
        }
        if start != have {
            return Err((format!("RANGE_MISMATCH: expected offset {have}"), have));
        }
        let fin = stage.join("data").join(&f.rel_path);
        let body = body.to_vec();
        let size = f.size;
        tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
            if let Some(p) = fin.parent() {
                std::fs::create_dir_all(p)?;
            }
            let part = partial_of(&fin);
            let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&part)?;
            file.write_all(&body)?;
            let len = file.metadata()?.len();
            if len == size {
                file.sync_all()?;
                drop(file);
                std::fs::rename(&part, &fin)?;
            }
            Ok(len)
        })
        .await
        .map_err(|e| (internal(e), have))?
        .map_err(|e| (internal(e), have))
    }

    pub async fn commit(&self, from: &str, upload: &str) -> Result<Value, String> {
        if !is_hex(upload, 32) {
            return Err("NOT_FOUND: no such upload".into());
        }
        let root = self.root()?;
        let _g = self.gate.lock().await;
        let stage = root.join(INCOMING).join(upload);
        let marker = root.join(INCOMING).join(format!("{upload}.done"));
        let Some(m) = load_manifest(&stage) else {
            // Committed before (the answer got lost): same answer again.
            if let Some(v) = std::fs::read(&marker).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                if v.get("from").and_then(|x| x.as_str()) == Some(from) {
                    return Ok(v["result"].clone());
                }
            }
            return Err("NOT_FOUND: no such upload".into());
        };
        if m.from != from {
            return Err("UNAUTHORIZED: not your upload".into());
        }
        let data = stage.join("data");
        // Verify every file (size, then SHA-256). A bad file is dropped so it is re-sent.
        for f in &m.req.files {
            let p = data.join(&f.rel_path);
            if f.size == 0 && !p.exists() {
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d).map_err(internal)?;
                }
                std::fs::write(&p, b"").map_err(internal)?;
            }
            let len = std::fs::metadata(&p).map(|x| x.len()).ok();
            if len != Some(f.size) {
                return Err(format!("INCOMPLETE: {} has {} of {} bytes", f.rel_path, len.unwrap_or(0), f.size));
            }
            let pp = p.clone();
            let sum = tokio::task::spawn_blocking(move || sha256_file(&pp)).await.map_err(internal)?.map_err(internal)?;
            if !sum.eq_ignore_ascii_case(&f.sha256) {
                let _ = std::fs::remove_file(&p);
                return Err(format!("VERIFY_FAILED: SHA-256 mismatch for {}", f.rel_path));
            }
        }
        let base = crate::engine::clean_folder_name(&m.req.name);
        let dest = if root.join(&base).exists() {
            root.join(crate::engine::unique_name(&base, |c| root.join(c).exists()))
        } else {
            root.join(&base)
        };
        std::fs::rename(&data, &dest).map_err(|e| internal(format!("move into {}: {e}", dest.display())))?;
        if let Err(e) = self.placer.place(&m.req, &dest).await {
            let _ = std::fs::rename(&dest, &data);
            return Err(e);
        }
        let result = json!({ "ok": true, "path": dest });
        let rec = json!({ "from": from, "at": now_ms(), "result": result });
        let _ = std::fs::write(&marker, rec.to_string());
        let _ = std::fs::remove_dir_all(&stage);
        tracing::info!(name = %m.req.name, to = %dest.display(), "archive: item received");
        Ok(result)
    }
}

/// The app's placer: librqbit session / yt-dlp job list.
pub(crate) struct AppPlacer {
    pub st: Arc<AppState>,
    pub app: OnceLock<AppHandle>,
}

impl Placer for AppPlacer {
    fn has_complete(&self, m: &BeginReq) -> bool {
        m.kind == "torrent" && crate::engine::has_complete(&self.st.session, &m.key)
    }

    fn place<'a>(&'a self, m: &'a BeginReq, dest: &'a Path) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            if m.kind == "yt" {
                let y = m.yt.as_ref().ok_or("INVALID_INPUT: missing yt")?;
                crate::ytdlp::register_archived(
                    self.app.get(),
                    &self.st,
                    &y.url,
                    &y.title,
                    y.thumbnail.clone(),
                    y.preset,
                    dest.to_path_buf(),
                    dest.join(&y.file_rel),
                );
                return Ok(());
            }
            let bytes = m.torrent_bytes_b64.as_deref().and_then(remote::base64_decode).ok_or("INVALID_INPUT: bad torrent bytes")?;
            let key = crate::engine::add_archived(&self.st.session, bytes, dest, m.only_files.clone()).await?;
            if key != m.key {
                tracing::warn!(sent = %m.key, got = %key, "archive: torrent key differs");
            }
            let subs = m.online_subs.iter().map(|s| (dest.join(&s.rel_path), s.file_index, s.lang.clone(), s.url.clone())).collect();
            crate::subfetch::adopt(&self.st.data_dir, &key, subs);
            Ok(())
        })
    }
}

// ---------------------------------------------------------------- sender: transfer

/// One item to move.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Item {
    Torrent { key: String },
    Yt { id: String },
}

/// A begin request + the local folder its rel paths are under.
pub(crate) struct Prepared {
    pub req: BeginReq,
    pub base: PathBuf,
}

pub(crate) trait Source: Send + Sync {
    fn prepare<'a>(&'a self, item: &'a Item) -> BoxFut<'a, Result<Prepared, String>>;
    /// Remove the local copy (only called after the receiver's commit said ok).
    fn delete<'a>(&'a self, item: &'a Item) -> BoxFut<'a, Result<(), String>>;
}

/// Where to send + how to authenticate.
pub(crate) struct Peer {
    pub addr: SocketAddr,
    pub id: String,
    pub secret: String,
}

impl Peer {
    fn url(&self, path: &str) -> String {
        format!("http://{}:{}{path}", remote::url_host(self.addr.ip()), self.addr.port())
    }
}

pub(crate) fn http_client() -> reqwest::Client {
    // No read timeout: a commit verifies big files before it answers.
    reqwest::Client::builder().no_proxy().connect_timeout(Duration::from_secs(3)).build().unwrap_or_default()
}

/// Send a request; (status, JSON body).
async fn send(rb: reqwest::RequestBuilder, peer: &Peer) -> Result<(u16, Value), String> {
    let resp = rb
        .header("X-Tide-Id", &peer.id)
        .header("Authorization", format!("Bearer {}", peer.secret))
        .send()
        .await
        .map_err(|e| format!("UNREACHABLE: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await.map_err(|e| format!("UNREACHABLE: {e}"))?;
    Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

/// Non-2xx answer -> its "CODE: message" error.
fn err_of(status: u16, v: &Value) -> String {
    match v.get("error").and_then(|e| e.as_str()) {
        Some(e) if e.contains(':') => e.to_string(),
        Some(e) => remote::answer(status, json!({ "error": e }).to_string().as_bytes()).err().unwrap_or_default(),
        None => format!("REMOTE_ERROR: HTTP {status}"),
    }
}

async fn post(http: &reqwest::Client, peer: &Peer, path: &str, body: &Value, timeout: Duration) -> Result<Value, String> {
    let rb = http.post(peer.url(path)).timeout(timeout).header("Content-Type", "application/json").body(body.to_string());
    let (status, v) = send(rb, peer).await?;
    if (200..300).contains(&status) {
        Ok(v)
    } else {
        Err(err_of(status, &v))
    }
}

/// GET `/v1/archive/info` of a paired device.
pub(crate) async fn remote_info(http: &reqwest::Client, peer: &Peer) -> Result<Value, String> {
    let (status, v) = send(http.get(peer.url("/v1/archive/info")).timeout(Duration::from_secs(5)), peer).await?;
    if status == 200 {
        Ok(v)
    } else {
        Err(err_of(status, &v))
    }
}

fn enc_rel(rel: &str) -> String {
    rel.split('/').map(|s| urlencoding::encode(s).into_owned()).collect::<Vec<_>>().join("/")
}

/// Upload one file from `off` on, chunk by chunk.
async fn send_file(
    http: &reqwest::Client,
    peer: &Peer,
    upload: &str,
    base: &Path,
    f: &WireFile,
    mut off: u64,
    done: &AtomicU64,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<(), String> {
    let path = base.join(&f.rel_path);
    let mut file = tokio::fs::File::open(&path).await.map_err(|e| format!("INTERNAL: open {}: {e}", path.display()))?;
    let url = peer.url(&format!("/v1/archive/{upload}/{}", enc_rel(&f.rel_path)));
    let mut failures = 0u32;
    while off < f.size {
        let n = (f.size - off).min(CHUNK as u64) as usize;
        let mut buf = vec![0u8; n];
        file.seek(std::io::SeekFrom::Start(off)).await.map_err(internal)?;
        file.read_exact(&mut buf).await.map_err(|e| format!("INTERNAL: read {}: {e}", path.display()))?;
        let rb = http
            .put(&url)
            .timeout(CHUNK_TIMEOUT)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Range", format!("bytes {off}-{}/{}", off + n as u64 - 1, f.size))
            .body(buf);
        let (status, v) = match send(rb, peer).await {
            Ok(x) => x,
            Err(e) => {
                failures += 1;
                if failures > 3 {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_secs(2 * failures as u64)).await;
                continue;
            }
        };
        let have = v.get("have").and_then(|h| h.as_u64());
        match (status, have) {
            (200, Some(h)) | (409, Some(h)) if h <= f.size => {
                if status == 409 {
                    failures += 1;
                    if failures > 5 {
                        return Err(err_of(status, &v));
                    }
                } else {
                    failures = 0;
                }
                if h >= off {
                    done.fetch_add(h - off, Ordering::SeqCst);
                } else {
                    done.fetch_sub(off - h, Ordering::SeqCst);
                }
                off = h;
                progress(done.load(Ordering::SeqCst));
            }
            _ => return Err(err_of(status, &v)),
        }
    }
    Ok(())
}

/// begin -> missing chunks (≤ 2 files at once, one chunk each) -> commit. Ok only when the
/// receiver verified and placed everything.
pub(crate) async fn transfer(http: &reqwest::Client, peer: &Peer, p: &Prepared, progress: &(dyn Fn(u64) + Send + Sync)) -> Result<(), String> {
    let body = serde_json::to_value(&p.req).map_err(internal)?;
    let b = post(http, peer, "/v1/archive/begin", &body, Duration::from_secs(120)).await?;
    if b.get("already").and_then(|x| x.as_bool()) == Some(true) {
        progress(p.req.total());
        return Ok(());
    }
    let upload = b.get("upload_id").and_then(|x| x.as_str()).filter(|s| is_hex(s, 32)).ok_or("REMOTE_ERROR: bad begin answer")?.to_string();
    let have = |rel: &str| b.get("have").and_then(|h| h.get(rel)).and_then(|x| x.as_u64()).unwrap_or(0);
    let done = AtomicU64::new(p.req.files.iter().map(|f| have(&f.rel_path).min(f.size)).sum());
    progress(done.load(Ordering::SeqCst));
    let todo: Vec<(&WireFile, u64)> = p.req.files.iter().map(|f| (f, have(&f.rel_path))).filter(|(f, h)| *h < f.size).collect();
    let queue = Mutex::new(todo.into_iter());
    let worker = || async {
        loop {
            let next = queue.lock().unwrap().next();
            let Some((f, off)) = next else { return Ok::<(), String>(()) };
            send_file(http, peer, &upload, &p.base, f, off, &done, progress).await?;
        }
    };
    // Two workers pulling from one queue: at most two chunks in flight.
    let (a, b2) = tokio::join!(worker(), worker());
    a?;
    b2?;
    let c = post(http, peer, &format!("/v1/archive/{upload}/commit"), &json!({}), COMMIT_TIMEOUT).await?;
    if c.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        return Err("REMOTE_ERROR: commit not confirmed".into());
    }
    Ok(())
}

/// Prepare, transfer, and only after a confirmed commit delete the local copy.
pub(crate) async fn move_one(
    http: &reqwest::Client,
    peer: &Peer,
    src: &dyn Source,
    item: &Item,
    on_start: &(dyn Fn(&str, u64) + Send + Sync),
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<(), String> {
    let p = src.prepare(item).await?;
    on_start(&p.req.name, p.req.total());
    transfer(http, peer, &p, progress).await?;
    src.delete(item).await
}

// ---------------------------------------------------------------- sender: the app side

#[derive(Debug, Clone, Serialize)]
pub struct QEntry {
    #[serde(flatten)]
    pub item: Item,
    pub name: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// "queued" | "moving" | "verifying" | "done" | "error". The list is in move order:
    /// the in-flight item first, then what comes next.
    pub state: String,
    pub error: Option<String>,
    /// Picked by hand (`archive_move`), not by auto mode.
    #[serde(skip)]
    manual: bool,
    /// When it reached "done" (shown briefly, then dropped).
    #[serde(skip)]
    done_at: Option<Instant>,
}

const DONE_SHOW: Duration = Duration::from_secs(10);
const ACTIVE: [&str; 2] = ["moving", "verifying"];

fn q_entry(item: Item, name: String, manual: bool) -> QEntry {
    QEntry { item, name, bytes_done: 0, bytes_total: 0, state: "queued".into(), error: None, manual, done_at: None }
}

/// Queue an item. `front`: right after the in-flight one (a manual pick jumps the line,
/// without aborting the current transfer). An errored or done entry is re-queued.
fn q_enqueue(q: &mut Vec<QEntry>, item: Item, name: String, front: bool) {
    let pos = q.iter().position(|e| e.item == item);
    if let Some(i) = pos {
        if ACTIVE.contains(&q[i].state.as_str()) {
            return;
        }
    }
    let mut e = match pos {
        Some(i) => q.remove(i),
        None => q_entry(item, name, front),
    };
    e.state = "queued".into();
    e.error = None;
    e.done_at = None;
    e.bytes_done = 0;
    if front {
        e.manual = true;
        let at = q.iter().take_while(|x| ACTIVE.contains(&x.state.as_str())).count();
        q.insert(at, e);
    } else {
        q.push(e);
    }
}

/// Make the queue list every auto candidate (in order, after what is already queued) and
/// drop auto entries that are no longer candidates. `retry_ok(item)`: an errored auto
/// entry may be queued again.
fn q_sync_auto(q: &mut Vec<QEntry>, cands: &[(Item, String)], retry_ok: impl Fn(&Item) -> bool) {
    q.retain(|e| e.manual || e.state != "queued" || cands.iter().any(|(i, _)| i == &e.item));
    for (item, name) in cands {
        match q.iter_mut().find(|e| &e.item == item) {
            Some(e) if e.state == "error" && retry_ok(item) => {
                e.state = "queued".into();
                e.error = None;
            }
            Some(_) => {}
            None if retry_ok(item) => q.push(q_entry(item.clone(), name.clone(), false)),
            None => {}
        }
    }
}

/// Drop the auto entries still waiting (auto mode turned off) and stale "done" ones.
fn q_prune(q: &mut Vec<QEntry>, auto: bool) {
    q.retain(|e| {
        (auto || e.manual || e.state != "queued") && !(e.state == "done" && e.done_at.is_none_or(|t| t.elapsed() >= DONE_SHOW))
    });
}

/// The first queued item (queue order) that is not busy.
fn q_next(q: &[QEntry], busy: impl Fn(&Item) -> bool) -> Option<Item> {
    q.iter().filter(|e| e.state == "queued").map(|e| e.item.clone()).find(|i| !busy(i))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DevInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub archive_dir: Option<String>,
    #[serde(default)]
    pub free_bytes: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

pub struct Archiver {
    http: reqwest::Client,
    queue: Mutex<Vec<QEntry>>,
    device: Mutex<Option<DevInfo>>,
    failed: Mutex<HashMap<Item, Instant>>,
    /// (path, size, mtime) -> sha256
    hashes: Arc<Mutex<HashMap<(PathBuf, u64, u64), String>>>,
    wake: tokio::sync::Notify,
    last_emit: Mutex<Option<Instant>>,
    app: OnceLock<AppHandle>,
}

impl Default for Archiver {
    fn default() -> Self {
        Self::new()
    }
}

impl Archiver {
    pub fn new() -> Self {
        // The last archive device seen (shown while it is offline).
        let device = setting("archive_device").and_then(|v| serde_json::from_value::<DevInfo>(v).ok()).map(|mut d| {
            d.online = false;
            d.available = false;
            d
        });
        Archiver {
            http: http_client(),
            queue: Mutex::new(Vec::new()),
            device: Mutex::new(device),
            failed: Mutex::new(HashMap::new()),
            hashes: Arc::new(Mutex::new(HashMap::new())),
            wake: tokio::sync::Notify::new(),
            last_emit: Mutex::new(None),
            app: OnceLock::new(),
        }
    }

    pub fn status(&self) -> Value {
        let q: Vec<QEntry> = {
            let mut q = self.queue.lock().unwrap();
            q.retain(|e| !(e.state == "done" && e.done_at.is_some_and(|t| t.elapsed() >= DONE_SHOW)));
            q.clone()
        };
        let device = self.device.lock().unwrap().clone();
        json!({ "queue": q, "device": device, "auto": archive_auto_setting() })
    }

    fn emit(&self, force: bool) {
        {
            let mut l = self.last_emit.lock().unwrap();
            if !force && l.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
                return;
            }
            *l = Some(Instant::now());
        }
        if let Some(app) = self.app.get() {
            let _ = app.emit("archive-progress", self.status());
        }
    }

    fn update(&self, item: &Item, f: impl FnOnce(&mut QEntry)) {
        if let Some(e) = self.queue.lock().unwrap().iter_mut().find(|e| &e.item == item) {
            f(e);
        }
    }

    /// Manual pick: next after the in-flight item.
    fn enqueue_front(&self, item: Item, name: String) {
        q_enqueue(&mut self.queue.lock().unwrap(), item, name, true);
    }
}

struct AppSource {
    app: AppHandle,
    st: Arc<AppState>,
    hashes: Arc<Mutex<HashMap<(PathBuf, u64, u64), String>>>,
}

fn mtime_s(m: &std::fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

impl AppSource {
    /// size + SHA-256 of `base/rel` (cached by path/size/mtime).
    async fn wire(&self, base: &Path, rel: &str, expect: Option<u64>) -> Result<WireFile, String> {
        let p = base.join(rel);
        let m = std::fs::metadata(&p).map_err(|e| format!("NOT_FOUND: {}: {e}", p.display()))?;
        if expect.is_some_and(|s| s != m.len()) {
            return Err(format!("NOT_READY: {} is incomplete on disk", p.display()));
        }
        let k = (p.clone(), m.len(), mtime_s(&m));
        let cached = self.hashes.lock().unwrap().get(&k).cloned();
        let sha = match cached {
            Some(s) => s,
            None => {
                let pp = p.clone();
                let s = tokio::task::spawn_blocking(move || sha256_file(&pp)).await.map_err(internal)?.map_err(internal)?;
                self.hashes.lock().unwrap().insert(k, s.clone());
                s
            }
        };
        Ok(WireFile { rel_path: rel.to_string(), size: m.len(), sha256: sha })
    }

    async fn prepare_torrent(&self, key: &str) -> Result<Prepared, String> {
        let l = crate::engine::archive_layout(&self.st, key)?;
        let mut files = Vec::new();
        for (rel, size) in &l.files {
            files.push(self.wire(&l.base, rel, Some(*size)).await?);
        }
        let rel_of = |p: &Path| -> Option<String> {
            let r = p.strip_prefix(&l.base).ok()?;
            let s = r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
            valid_rel(&s).then_some(s)
        };
        let mut sidecars = Vec::new();
        for p in crate::metadata::sidecar_paths(&self.st, key).0 {
            if p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".tmp")) {
                continue;
            }
            if let Some(rel) = rel_of(&p) {
                if !files.iter().any(|f| f.rel_path == rel) {
                    files.push(self.wire(&l.base, &rel, None).await?);
                    sidecars.push(rel);
                }
            }
        }
        let mut online_subs = Vec::new();
        for (p, file_index, lang, url) in crate::subfetch::owned_entries(&self.st, key) {
            if let Some(rel) = rel_of(&p).filter(|_| p.is_file()) {
                if !files.iter().any(|f| f.rel_path == rel) {
                    files.push(self.wire(&l.base, &rel, None).await?);
                }
                online_subs.push(OnlineSub { rel_path: rel, file_index, lang, url });
            }
        }
        Ok(Prepared {
            req: BeginReq {
                key: key.to_string(),
                kind: "torrent".into(),
                name: l.name,
                files,
                torrent_bytes_b64: Some(crate::artwork::base64(&l.torrent_bytes)),
                only_files: l.only_files,
                sidecars,
                online_subs,
                yt: None,
            },
            base: l.base,
        })
    }

    async fn prepare_yt(&self, id: &str) -> Result<Prepared, String> {
        let (job, dir) = crate::ytdlp::done_jobs(&self.st).into_iter().find(|(j, _)| j.id == id).ok_or("NOT_FOUND: no finished download")?;
        let file = PathBuf::from(job.file_path.clone().unwrap_or_default());
        if !file.is_file() {
            return Err(format!("NOT_FOUND: {} is missing", file.display()));
        }
        let tidebox = &self.st.download_dir;
        let own = dir.filter(|d| d.parent() == Some(tidebox.as_path()) && file.starts_with(d));
        let (base, rels) = match own {
            Some(d) => {
                let mut rels = Vec::new();
                walk(&d, &d, &mut rels);
                (d, rels)
            }
            None => {
                let base = file.parent().map(Path::to_path_buf).unwrap_or_default();
                (base, vec![file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()])
            }
        };
        let file_rel = file.strip_prefix(&base).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
        let mut files = Vec::new();
        for rel in rels.iter().filter(|r| valid_rel(r)) {
            files.push(self.wire(&base, rel, None).await?);
        }
        let name = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| job.title.clone());
        let fname = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Prepared {
            req: BeginReq {
                key: sha256_bytes(format!("{}\n{fname}", job.url).as_bytes())[..40].to_string(),
                kind: "yt".into(),
                name,
                files,
                torrent_bytes_b64: None,
                only_files: None,
                sidecars: vec![],
                online_subs: vec![],
                yt: Some(YtMeta { url: job.url, title: job.title, thumbnail: job.thumbnail, preset: job.preset, file_rel }),
            },
            base,
        })
    }
}

/// Regular files under `dir` as rel paths ('/'), hidden files skipped.
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with('.') {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out);
        } else if p.is_file() {
            if let Ok(r) = p.strip_prefix(root) {
                out.push(r.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/"));
            }
        }
    }
}

impl Source for AppSource {
    fn prepare<'a>(&'a self, item: &'a Item) -> BoxFut<'a, Result<Prepared, String>> {
        Box::pin(async move {
            match item {
                Item::Torrent { key } => self.prepare_torrent(key).await,
                Item::Yt { id } => self.prepare_yt(id).await,
            }
        })
    }

    fn delete<'a>(&'a self, item: &'a Item) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            match item {
                Item::Torrent { key } => crate::engine::remove(&self.st, key, true).await,
                Item::Yt { id } => crate::ytdlp::remove_job(&self.app, &self.st, id, true).await,
            }
        })
    }
}

/// Playing right now, or resume progress saved within the last minute.
fn busy(app: &AppHandle, st: &AppState, item: &Item) -> bool {
    let visible = app.get_webview_window("player").is_some_and(|w| w.is_visible().unwrap_or(false));
    if visible {
        let cur = st.player.current.lock().ok().and_then(|c| c.as_ref().map(|r| r.media.clone()));
        let playing = match (cur, item) {
            (Some(MediaRef::Torrent { key: k, .. }), Item::Torrent { key }) => &k == key,
            (Some(MediaRef::Yt { id: a }), Item::Yt { id }) => &a == id,
            _ => false,
        };
        if playing {
            return true;
        }
    }
    if let Item::Torrent { key } = item {
        let hub = app.state::<Arc<crate::sync::SyncHub>>();
        let now = now_ms();
        return hub.resume_list().iter().any(|v| {
            v.get("key").and_then(|k| k.as_str()) == Some(key.as_str())
                && v.get("updated_at").and_then(|u| u.as_u64()).is_some_and(|u| now.saturating_sub(u) < RECENT_RESUME_MS)
        });
    }
    false
}

fn display_name(st: &AppState, item: &Item) -> String {
    match item {
        Item::Torrent { key } => crate::engine::list_rows(st).into_iter().find(|r| &r.key == key).map(|r| r.name).unwrap_or_else(|| key.clone()),
        Item::Yt { id } => crate::ytdlp::done_jobs(st).into_iter().find(|(j, _)| &j.id == id).map(|(j, _)| j.title).unwrap_or_else(|| id.clone()),
    }
}

/// Completed torrents + done yt-dlp jobs (auto mode).
fn auto_candidates(st: &AppState) -> Vec<Item> {
    let mut v: Vec<Item> = crate::engine::list_rows(st)
        .into_iter()
        .filter(|r| r.id.is_some() && r.state == crate::types::TorrentState::Completed)
        .map(|r| Item::Torrent { key: r.key })
        .collect();
    v.extend(crate::ytdlp::done_jobs(st).into_iter().map(|(j, _)| Item::Yt { id: j.id }));
    v
}

/// The paired device that reports an archive location (probes online ones).
async fn find_device(rs: &RemoteState, http: &reqwest::Client, prev: Option<DevInfo>) -> Option<(DevInfo, Option<Peer>)> {
    let devices: Vec<Device> = rs.devices();
    // The known archive device first.
    let mut order = devices.clone();
    if let Some(p) = &prev {
        order.sort_by_key(|d| d.id != p.id);
    }
    for d in order {
        let Some(addr) = remote::quick_addr(&d, 800).await else { continue };
        let peer = Peer { addr, id: rs.self_id(), secret: d.secret.clone() };
        let Ok(info) = remote_info(http, &peer).await else { continue };
        let Some(dir) = info.get("archive_dir").and_then(|x| x.as_str()) else { continue };
        let dev = DevInfo {
            id: d.id.clone(),
            name: d.name.clone(),
            online: true,
            available: info.get("available").and_then(|x| x.as_bool()).unwrap_or(false),
            archive_dir: Some(dir.to_string()),
            free_bytes: info.get("free_bytes").and_then(|x| x.as_u64()),
            error: info.get("error").and_then(|x| x.as_str()).map(str::to_string),
        };
        return Some((dev, Some(peer)));
    }
    // Offline: keep showing the last one (if still paired).
    prev.filter(|p| devices.iter().any(|d| d.id == p.id)).map(|mut p| {
        p.online = false;
        p.available = false;
        (p, None)
    })
}

/// One pass: refresh the device, then move at most one item. True = moved something.
async fn pass(ar: &Archiver, app: &AppHandle, st: &Arc<AppState>, rs: &RemoteState) -> bool {
    // The default location follows the volume being plugged in / out.
    let dir = archive_dir_setting();
    if let Some(rc) = app.try_state::<Arc<Receiver>>() {
        rc.set_dir(dir.clone());
    }
    if dir.is_some() {
        // The archive Mac itself never sends.
        return false;
    }
    let prev = ar.device.lock().unwrap().clone();
    let found = find_device(rs, &ar.http, prev.clone()).await;
    let peer = match found {
        Some((dev, peer)) => {
            if prev.as_ref().is_none_or(|p| p.id != dev.id || p.name != dev.name) {
                let _ = set_setting("archive_device", json!({ "id": dev.id, "name": dev.name }));
            }
            *ar.device.lock().unwrap() = Some(dev.clone());
            if dev.available {
                peer
            } else {
                None
            }
        }
        None => {
            *ar.device.lock().unwrap() = None;
            None
        }
    };
    ar.emit(true);
    let Some(peer) = peer else { return false };
    // Manual queue first, then (auto) candidates; skip busy and recently failed ones.
    let auto = archive_auto_setting();
    q_prune(&mut ar.queue.lock().unwrap(), auto);
    if auto {
        let retry_ok = |i: &Item| ar.failed.lock().unwrap().get(i).is_none_or(|t| t.elapsed() >= RETRY_AFTER);
        let cands: Vec<(Item, String)> = auto_candidates(st).into_iter().map(|i| (i.clone(), display_name(st, &i))).collect();
        q_sync_auto(&mut ar.queue.lock().unwrap(), &cands, retry_ok);
    }
    let next = {
        let q = ar.queue.lock().unwrap().clone();
        q_next(&q, |i| busy(app, st, i))
    };
    let Some(item) = next else {
        ar.emit(true);
        return false;
    };
    // The in-flight item leads the list.
    {
        let mut q = ar.queue.lock().unwrap();
        if let Some(i) = q.iter().position(|e| e.item == item) {
            let e = q.remove(i);
            q.insert(0, e);
        }
    }
    ar.update(&item, |e| {
        e.state = "moving".into();
        e.error = None;
    });
    ar.emit(true);
    let src = AppSource { app: app.clone(), st: st.clone(), hashes: ar.hashes.clone() };
    let on_start = |name: &str, total: u64| {
        ar.update(&item, |e| {
            e.name = name.to_string();
            e.bytes_total = total;
        });
        ar.emit(true);
    };
    let progress = |done: u64| {
        ar.update(&item, |e| {
            e.bytes_done = done;
            // All bytes sent: the receiver now checks sizes + SHA-256 and places them.
            if e.bytes_total > 0 && done >= e.bytes_total {
                e.state = "verifying".into();
            }
        });
        ar.emit(false);
    };
    let r = move_one(&ar.http, &peer, &src, &item, &on_start, &progress).await;
    match r {
        Ok(()) => {
            tracing::info!(?item, "archive: moved");
            ar.update(&item, |e| {
                e.state = "done".into();
                e.done_at = Some(Instant::now());
            });
            ar.failed.lock().unwrap().remove(&item);
            crate::sync::touch();
            let _ = app.emit("sync-updated", json!({ "what": ["library"] }));
            ar.emit(true);
            true
        }
        Err(e) => {
            tracing::warn!(?item, "archive move failed: {e}");
            ar.failed.lock().unwrap().insert(item.clone(), Instant::now());
            ar.update(&item, |q| {
                q.state = "error".into();
                q.error = Some(e);
            });
            // Out of the way of what is still queued.
            {
                let mut q = ar.queue.lock().unwrap();
                if let Some(i) = q.iter().position(|x| x.item == item) {
                    let x = q.remove(i);
                    q.push(x);
                }
            }
            ar.emit(true);
            false
        }
    }
}

/// Background mover (every device; the archive Mac idles).
pub fn spawn(app: AppHandle, st: Arc<AppState>, rs: Arc<RemoteState>, ar: Arc<Archiver>) {
    let _ = ar.app.set(app.clone());
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        loop {
            let moved = pass(&ar, &app, &st, &rs).await;
            if !moved {
                tokio::select! {
                    _ = tokio::time::sleep(LOOP_EVERY) => {}
                    _ = ar.wake.notified() => {}
                }
            }
        }
    });
}

// ---------------------------------------------------------------- auto-clean

/// Cleanup items for the candidates (all of them: no confirmation on the archive Mac).
pub(crate) fn autoclean_items(c: &[crate::cleanup::CleanupCandidate]) -> Vec<crate::cleanup::CleanupItem> {
    c.iter().map(|c| crate::cleanup::CleanupItem { key: c.key.clone(), file_index: c.file_index }).collect()
}

async fn autoclean_once(st: &AppState) {
    let has_archive = archive_dir_setting().is_some();
    let Some(days) = effective_autoclean(autoclean_setting(), has_archive) else { return };
    let items = match crate::cleanup::candidates(st, days) {
        Ok(c) => autoclean_items(&c),
        Err(e) => return tracing::warn!("auto-clean: {e}"),
    };
    if items.is_empty() {
        return;
    }
    match crate::cleanup::apply(st, items).await {
        Ok(r) if !r.removed.is_empty() => {
            let msg = freed_text(r.freed_bytes, r.removed.len());
            tracing::info!("auto-clean: {msg}");
            crate::notify::notify("Tide", &msg);
            crate::sync::touch();
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("auto-clean: {e}"),
    }
}

pub fn spawn_autoclean(st: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            autoclean_once(&st).await;
            tokio::time::sleep(CLEAN_EVERY).await;
        }
    });
}

// ---------------------------------------------------------------- commands

type Ar<'a> = State<'a, Arc<Archiver>>;
type Rc<'a> = State<'a, Arc<Receiver>>;

fn settings_value(ar: &Archiver, rc: &Receiver) -> Value {
    let dir = archive_dir_setting();
    let info = rc.info();
    json!({
        "archive_dir": dir,
        "available": info.get("available").cloned().unwrap_or(json!(false)),
        "free_bytes": info.get("free_bytes").cloned().unwrap_or(Value::Null),
        "error": info.get("error").cloned().unwrap_or(Value::Null),
        "autoclean_days": effective_autoclean(autoclean_setting(), dir.is_some()),
        "archive_auto": archive_auto_setting(),
        "device": ar.device.lock().unwrap().clone(),
    })
}

/// `{archive_dir|null, available, free_bytes, error, autoclean_days|null, archive_auto, device|null}`.
#[tauri::command]
pub fn archive_settings(ar: Ar<'_>, rc: Rc<'_>) -> Result<Value, String> {
    Ok(settings_value(&ar, &rc))
}

/// JS: `{ archiveDir?: string ("" = none), autocleanDays?: number (0 = off), archiveAuto?: bool }`.
#[tauri::command]
pub fn archive_set_settings(
    ar: Ar<'_>,
    rc: Rc<'_>,
    archive_dir: Option<String>,
    autoclean_days: Option<u64>,
    archive_auto: Option<bool>,
) -> Result<Value, String> {
    if let Some(d) = archive_dir {
        let d = d.trim().to_string();
        if !d.is_empty() && !d.starts_with('/') {
            return Err("INVALID_INPUT: the archive location must be an absolute path".into());
        }
        set_setting("archive_dir", json!(d))?;
        rc.set_dir(archive_dir_setting());
    }
    if let Some(n) = autoclean_days {
        set_setting("autoclean_days", json!(n.min(3650)))?;
    }
    if let Some(a) = archive_auto {
        set_setting("archive_auto", json!(a))?;
        if a {
            ar.wake.notify_one();
        }
    }
    Ok(settings_value(&ar, &rc))
}

/// JS: `{ key }` or `{ ytId }`: move one finished item to the archive Mac.
#[tauri::command]
pub fn archive_move(app: AppHandle, ar: Ar<'_>, key: Option<String>, yt_id: Option<String>) -> Result<Value, String> {
    let st = app.state::<Arc<AppState>>().inner().clone();
    let item = match (key, yt_id) {
        (Some(k), _) if is_hex(k.trim(), 40) => Item::Torrent { key: k.trim().to_ascii_lowercase() },
        (_, Some(id)) if !id.trim().is_empty() => Item::Yt { id: id.trim().to_string() },
        _ => return Err("INVALID_INPUT: key or ytId required".into()),
    };
    if archive_dir_setting().is_some() {
        return Err("INVALID_INPUT: this Mac is the archive".into());
    }
    if ar.device.lock().unwrap().is_none() {
        return Err("NOT_FOUND: no paired Mac has an archive location".into());
    }
    match &item {
        Item::Torrent { key } => {
            let row = crate::engine::list_rows(&st).into_iter().find(|r| &r.key == key).ok_or("NOT_FOUND: no such torrent")?;
            if row.state != crate::types::TorrentState::Completed {
                return Err("NOT_READY: download is not finished".into());
            }
        }
        Item::Yt { id } => {
            if !crate::ytdlp::done_jobs(&st).iter().any(|(j, _)| &j.id == id) {
                return Err("NOT_READY: download is not finished".into());
            }
        }
    }
    ar.failed.lock().unwrap().remove(&item);
    ar.enqueue_front(item.clone(), display_name(&st, &item));
    ar.wake.notify_one();
    ar.emit(true);
    Ok(ar.status())
}

/// `{queue: [{kind, key|id, name, bytes_done, bytes_total, state, error}], device, auto}`;
/// queue in move order (in-flight first), state queued|moving|verifying|done|error.
#[tauri::command]
pub fn archive_status(ar: Ar<'_>) -> Result<Value, String> {
    Ok(ar.status())
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{Backend, Opened, PlayRequest};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tide-archive-{tag}-{}-{}", std::process::id(), now_ms() % 1_000_000));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[derive(Default)]
    struct MockPlacer {
        placed: Mutex<Vec<(String, PathBuf)>>,
    }

    impl Placer for MockPlacer {
        fn has_complete(&self, _m: &BeginReq) -> bool {
            false
        }
        fn place<'a>(&'a self, m: &'a BeginReq, dest: &'a Path) -> BoxFut<'a, Result<(), String>> {
            Box::pin(async move {
                self.placed.lock().unwrap().push((m.key.clone(), dest.to_path_buf()));
                Ok(())
            })
        }
    }

    /// Re-adds into a real (offline) librqbit session.
    struct SessionPlacer {
        session: Arc<librqbit::Session>,
    }

    impl Placer for SessionPlacer {
        fn has_complete(&self, m: &BeginReq) -> bool {
            crate::engine::has_complete(&self.session, &m.key)
        }
        fn place<'a>(&'a self, m: &'a BeginReq, dest: &'a Path) -> BoxFut<'a, Result<(), String>> {
            Box::pin(async move {
                let bytes = remote::base64_decode(m.torrent_bytes_b64.as_deref().unwrap()).unwrap();
                crate::engine::add_archived(&self.session, bytes, dest, m.only_files.clone()).await.map(|_| ())
            })
        }
    }

    struct Be {
        rcv: Arc<Receiver>,
    }

    impl Backend for Be {
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
        fn archive(&self) -> Option<Arc<Receiver>> {
            Some(self.rcv.clone())
        }
    }

    const SECRET: &str = "5555555555555555555555555555555555555555555555555555555555555555";

    struct Rig {
        rcv: Arc<Receiver>,
        peer: Peer,
        root: PathBuf,
        src_dir: PathBuf,
    }

    /// Receiver server on loopback (temp data dir) + a sender identity paired with it.
    async fn rig(tag: &str, placer: Arc<dyn Placer>, root: Option<PathBuf>) -> Rig {
        let base = tmpdir(tag);
        let root = root.unwrap_or_else(|| base.join("vol").join("Tidebox"));
        if root.starts_with(&base) {
            std::fs::create_dir_all(base.join("vol")).unwrap();
        }
        let rdata = base.join("recv-data");
        let sdata = base.join("send-data");
        std::fs::create_dir_all(&rdata).unwrap();
        std::fs::create_dir_all(&sdata).unwrap();
        let rs = Arc::new(RemoteState::load(&rdata, "Mini".into()));
        let sender = RemoteState::load(&sdata, "Book".into());
        rs.upsert_device(Device {
            id: sender.self_id(),
            name: "Book".into(),
            host: "127.0.0.1".into(),
            port: 47900,
            secret: SECRET.into(),
            last_ip: None,
        });
        let rcv = Arc::new(Receiver::new(Some(root.clone()), placer));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        remote::serve_on(l, rs, Arc::new(Be { rcv: rcv.clone() }));
        let src_dir = base.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        Rig { rcv, peer: Peer { addr, id: sender.self_id(), secret: SECRET.into() }, root, src_dir }
    }

    fn data(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| ((i as u64 * 31 + seed as u64) % 251) as u8).collect()
    }

    fn wf(rel: &str, d: &[u8]) -> WireFile {
        WireFile { rel_path: rel.into(), size: d.len() as u64, sha256: sha256_bytes(d) }
    }

    fn yt_req(key: &str, files: Vec<WireFile>, file_rel: &str) -> BeginReq {
        BeginReq {
            key: key.into(),
            kind: "yt".into(),
            name: "Clip [x]".into(),
            files,
            torrent_bytes_b64: None,
            only_files: None,
            sidecars: vec![],
            online_subs: vec![],
            yt: Some(YtMeta { url: "https://e/x".into(), title: "Clip".into(), thumbnail: None, preset: YtPreset::P720, file_rel: file_rel.into() }),
        }
    }

    const K: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[tokio::test]
    async fn chunked_upload_resumes_after_interruption() {
        let placer = Arc::new(MockPlacer::default());
        let r = rig("resume", placer.clone(), None).await;
        let big = data(CHUNK * 2 + 12345, 1);
        let small = data(1000, 2);
        std::fs::create_dir_all(r.src_dir.join("sub")).unwrap();
        std::fs::write(r.src_dir.join("clip.mp4"), &big).unwrap();
        std::fs::write(r.src_dir.join("sub/clip.en.srt"), &small).unwrap();
        std::fs::write(r.src_dir.join("empty.txt"), b"").unwrap();
        let req = yt_req(K, vec![wf("clip.mp4", &big), wf("sub/clip.en.srt", &small), wf("empty.txt", b"")], "clip.mp4");
        let p = Prepared { req: req.clone(), base: r.src_dir.clone() };
        let http = http_client();

        // First attempt: begin + one chunk, then the "connection drops".
        let b = post(&http, &r.peer, "/v1/archive/begin", &serde_json::to_value(&req).unwrap(), Duration::from_secs(10)).await.unwrap();
        let upload = b["upload_id"].as_str().unwrap().to_string();
        assert_eq!(b["have"]["clip.mp4"], json!(0));
        let url = r.peer.url(&format!("/v1/archive/{upload}/clip.mp4"));
        let rb = http.put(&url).header("Content-Range", format!("bytes 0-{}/{}", CHUNK - 1, big.len())).body(big[..CHUNK].to_vec());
        let (st, v) = send(rb, &r.peer).await.unwrap();
        assert_eq!((st, v["have"].as_u64()), (200, Some(CHUNK as u64)));
        // A chunk at the wrong offset is refused with what is there.
        let rb = http.put(&url).header("Content-Range", format!("bytes 0-9/{}", big.len())).body(big[..10].to_vec());
        let (st, v) = send(rb, &r.peer).await.unwrap();
        assert_eq!((st, v["have"].as_u64()), (409, Some(CHUNK as u64)));
        assert!(placer.placed.lock().unwrap().is_empty());

        // Second attempt resumes: the first reported progress is the chunk already there.
        let seen = Mutex::new(Vec::new());
        transfer(&http, &r.peer, &p, &|d| seen.lock().unwrap().push(d)).await.unwrap();
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen[0], CHUNK as u64, "resumed from the received chunk");
        assert_eq!(*seen.last().unwrap(), (big.len() + small.len()) as u64);
        let placed = placer.placed.lock().unwrap().clone();
        assert_eq!(placed.len(), 1);
        let dest = &placed[0].1;
        assert_eq!(dest, &r.root.join("Clip [x]"));
        assert_eq!(std::fs::read(dest.join("clip.mp4")).unwrap(), big);
        assert_eq!(std::fs::read(dest.join("sub/clip.en.srt")).unwrap(), small);
        assert_eq!(std::fs::read(dest.join("empty.txt")).unwrap(), b"");
        assert!(!r.root.join(INCOMING).join(&upload).exists(), "staging cleaned up");
        // A repeated commit (lost answer) is answered ok again.
        let c = post(&http, &r.peer, &format!("/v1/archive/{upload}/commit"), &json!({}), Duration::from_secs(10)).await.unwrap();
        assert_eq!(c["ok"], json!(true));
        // Same name again: unique folder.
        let req2 = yt_req(&"b".repeat(40), vec![wf("clip.mp4", &big)], "clip.mp4");
        transfer(&http, &r.peer, &Prepared { req: req2, base: r.src_dir.clone() }, &|_| {}).await.unwrap();
        assert_eq!(placer.placed.lock().unwrap()[1].1, r.root.join("Clip [x] (2)"));
    }

    #[tokio::test]
    async fn sha256_mismatch_is_rejected() {
        let placer = Arc::new(MockPlacer::default());
        let r = rig("sha", placer.clone(), None).await;
        let d = data(5000, 3);
        std::fs::write(r.src_dir.join("a.mp4"), &d).unwrap();
        let mut f = wf("a.mp4", &d);
        f.sha256 = sha256_bytes(b"something else");
        let p = Prepared { req: yt_req(K, vec![f], "a.mp4"), base: r.src_dir.clone() };
        let e = transfer(&http_client(), &r.peer, &p, &|_| {}).await.unwrap_err();
        assert!(e.starts_with("VERIFY_FAILED"), "{e}");
        assert!(placer.placed.lock().unwrap().is_empty());
        // The bad file was dropped from staging (will be re-sent).
        let upload = upload_id(&r.peer.id, K);
        assert!(!r.root.join(INCOMING).join(&upload).join("data/a.mp4").exists());
        assert!(std::fs::read_dir(&r.root).unwrap().flatten().all(|e| e.file_name() == INCOMING));
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        for bad in ["../evil", "/etc/passwd", "a/../../x", "a//b", "./a", "x.partial", "a\\..\\b", ""] {
            assert!(!valid_rel(bad), "{bad:?}");
        }
        assert!(valid_rel("Season 1/Show S01E01.mkv"));
        let placer = Arc::new(MockPlacer::default());
        let r = rig("trav", placer.clone(), None).await;
        let http = http_client();
        let d = data(10, 4);
        for bad in ["../evil.mp4", "/tmp/evil.mp4", "sub/../../evil.mp4"] {
            let req = yt_req(K, vec![wf(bad, &d)], bad);
            let e = post(&http, &r.peer, "/v1/archive/begin", &serde_json::to_value(&req).unwrap(), Duration::from_secs(10)).await.unwrap_err();
            assert!(e.starts_with("INVALID_INPUT"), "{bad}: {e}");
        }
        // A good upload; PUT outside its file list (encoded "..") is refused.
        let req = yt_req(K, vec![wf("ok.mp4", &d)], "ok.mp4");
        let b = post(&http, &r.peer, "/v1/archive/begin", &serde_json::to_value(&req).unwrap(), Duration::from_secs(10)).await.unwrap();
        let upload = b["upload_id"].as_str().unwrap();
        // Raw requests: an HTTP client would normalise the dot segments itself.
        for path in ["../../evil.mp4", "%2E%2E/%2E%2E/evil.mp4", "..%2Fevil.mp4", "ok.mp4.partial", "%2Ftmp%2Fevil.mp4"] {
            use tokio::io::AsyncWriteExt;
            let mut sock = tokio::net::TcpStream::connect(r.peer.addr).await.unwrap();
            let head = format!(
                "PUT /v1/archive/{upload}/{path} HTTP/1.1\r\nHost: x\r\nX-Tide-Id: {}\r\nAuthorization: Bearer {SECRET}\r\nContent-Range: bytes 0-9/10\r\nContent-Length: 10\r\n\r\n",
                r.peer.id
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&d).await.unwrap();
            let mut out = String::new();
            sock.read_to_string(&mut out).await.unwrap();
            let code: u16 = out.split(' ').nth(1).unwrap().parse().unwrap();
            assert!(code == 400 || code == 404, "{path}: {out}");
        }
        assert!(!r.root.join("..").join("evil.mp4").exists());
        assert!(!r.root.parent().unwrap().join("evil.mp4").exists());
        // Unpaired caller.
        let stranger = Peer { addr: r.peer.addr, id: "f".repeat(32), secret: SECRET.into() };
        let e = post(&http, &stranger, "/v1/archive/begin", &serde_json::to_value(&req).unwrap(), Duration::from_secs(10)).await.unwrap_err();
        assert!(e.starts_with("UNAUTHORIZED"), "{e}");
    }

    #[tokio::test]
    async fn unavailable_when_volume_missing() {
        let base = tmpdir("unavail");
        let missing = base.join("NEO Storage").join("Tidebox");
        let placer = Arc::new(MockPlacer::default());
        let r = rig("unavail-r", placer, Some(missing.clone())).await;
        let info = remote_info(&http_client(), &r.peer).await.unwrap();
        assert_eq!(info["available"], json!(false));
        assert_eq!(info["error"], json!("ARCHIVE_UNAVAILABLE: NEO Storage not connected"));
        let d = data(10, 5);
        std::fs::write(r.src_dir.join("a.mp4"), &d).unwrap();
        let p = Prepared { req: yt_req(K, vec![wf("a.mp4", &d)], "a.mp4"), base: r.src_dir.clone() };
        let e = transfer(&http_client(), &r.peer, &p, &|_| {}).await.unwrap_err();
        assert_eq!(e, "ARCHIVE_UNAVAILABLE: NEO Storage not connected");
        assert!(!missing.exists(), "never creates the mount point");
        assert_eq!(volume_root(Path::new("/Volumes/NEO Storage/Tidebox")), PathBuf::from("/Volumes/NEO Storage"));
        // No archive location at all.
        r.rcv.set_dir(None);
        let info = remote_info(&http_client(), &r.peer).await.unwrap();
        assert_eq!(info["archive_dir"], Value::Null);
    }

    struct MockSource {
        p: Mutex<Option<Prepared>>,
        deleted: Mutex<Vec<Item>>,
    }

    impl Source for MockSource {
        fn prepare<'a>(&'a self, _item: &'a Item) -> BoxFut<'a, Result<Prepared, String>> {
            Box::pin(async move {
                let g = self.p.lock().unwrap();
                let p = g.as_ref().unwrap();
                Ok(Prepared { req: p.req.clone(), base: p.base.clone() })
            })
        }
        fn delete<'a>(&'a self, item: &'a Item) -> BoxFut<'a, Result<(), String>> {
            Box::pin(async move {
                self.deleted.lock().unwrap().push(item.clone());
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn sender_deletes_only_after_commit_ok() {
        let placer = Arc::new(MockPlacer::default());
        let r = rig("del", placer.clone(), None).await;
        let d = data(3 * 1024 * 1024, 6);
        std::fs::write(r.src_dir.join("a.mp4"), &d).unwrap();
        let item = Item::Yt { id: "yt-1".into() };
        let http = http_client();
        let noop_start = |_: &str, _: u64| {};
        // Bad hash -> commit fails -> nothing deleted.
        let mut bad = wf("a.mp4", &d);
        bad.sha256 = "0".repeat(64);
        let src = MockSource { p: Mutex::new(Some(Prepared { req: yt_req(K, vec![bad], "a.mp4"), base: r.src_dir.clone() })), deleted: Mutex::new(vec![]) };
        assert!(move_one(&http, &r.peer, &src, &item, &noop_start, &|_| {}).await.is_err());
        assert!(src.deleted.lock().unwrap().is_empty());
        // Archive volume gone -> nothing deleted.
        r.rcv.set_dir(Some(tmpdir("del-gone").join("Gone").join("Tidebox")));
        *src.p.lock().unwrap() = Some(Prepared { req: yt_req(K, vec![wf("a.mp4", &d)], "a.mp4"), base: r.src_dir.clone() });
        let e = move_one(&http, &r.peer, &src, &item, &noop_start, &|_| {}).await.unwrap_err();
        assert!(e.starts_with("ARCHIVE_UNAVAILABLE"), "{e}");
        assert!(src.deleted.lock().unwrap().is_empty());
        // Receiver unreachable -> nothing deleted.
        let gone = Peer { addr: "127.0.0.1:9".parse().unwrap(), id: r.peer.id.clone(), secret: SECRET.into() };
        assert!(move_one(&http, &gone, &src, &item, &noop_start, &|_| {}).await.is_err());
        assert!(src.deleted.lock().unwrap().is_empty());
        // Good -> placed, then deleted.
        r.rcv.set_dir(Some(r.root.clone()));
        let total = Mutex::new(0u64);
        move_one(&http, &r.peer, &src, &item, &|_, t| *total.lock().unwrap() = t, &|_| {}).await.unwrap();
        assert_eq!(*total.lock().unwrap(), d.len() as u64);
        assert_eq!(placer.placed.lock().unwrap().len(), 1);
        assert_eq!(src.deleted.lock().unwrap().as_slice(), &[item]);
    }

    // ---- real librqbit re-add

    fn sha1(d: &[u8]) -> [u8; 20] {
        use librqbit_sha1_wrapper::{ISha1, Sha1};
        let mut h = Sha1::new();
        h.update(d);
        h.finish()
    }

    /// Single-file .torrent (bencoded by hand).
    fn make_torrent(name: &str, content: &[u8]) -> Vec<u8> {
        let plen = 16384usize;
        let pieces: Vec<u8> = content.chunks(plen).flat_map(sha1).collect();
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi{}e4:name{}:{name}12:piece lengthi{plen}e6:pieces{}:", content.len(), name.len(), pieces.len()).as_bytes());
        info.extend_from_slice(&pieces);
        info.push(b'e');
        let mut t = b"d4:info".to_vec();
        t.extend_from_slice(&info);
        t.push(b'e');
        t
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn commit_re_adds_paused_and_completed() {
        let base = tmpdir("readd");
        let session = librqbit::Session::new_with_opts(
            base.join("session-dl"),
            librqbit::SessionOptions {
                dht: None,
                persistence: None,
                listen: None,
                disable_trackers: true,
                disable_local_service_discovery: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let r = rig("readd-r", Arc::new(SessionPlacer { session: session.clone() }), None).await;
        let content = data(100_000, 7);
        let name = "Animals.2026.1080p.mp4";
        let tbytes = make_torrent(name, &content);
        std::fs::write(r.src_dir.join(name), &content).unwrap();
        let poster = data(500, 8);
        std::fs::write(r.src_dir.join("poster.jpg"), &poster).unwrap();
        // Info hash = SHA-1 of the info dict.
        let info_start = 7;
        let key = hex(&sha1(&tbytes[info_start..tbytes.len() - 1]));
        let req = BeginReq {
            key: key.clone(),
            kind: "torrent".into(),
            name: "Animals (2026)".into(),
            files: vec![wf(name, &content), wf("poster.jpg", &poster)],
            torrent_bytes_b64: Some(crate::artwork::base64(&tbytes)),
            only_files: None,
            sidecars: vec!["poster.jpg".into()],
            online_subs: vec![],
            yt: None,
        };
        let p = Prepared { req, base: r.src_dir.clone() };
        transfer(&http_client(), &r.peer, &p, &|_| {}).await.unwrap();
        let id = key.parse::<librqbit::dht::Id20>().unwrap();
        let h = session.get(librqbit::api::TorrentIdOrHash::Hash(id)).expect("re-added");
        let s = h.stats();
        assert!(s.finished, "complete after the check: {} of {}", s.progress_bytes, s.total_bytes);
        assert_eq!(s.progress_bytes, content.len() as u64);
        assert!(h.is_paused(), "paused: never seeds");
        assert_eq!(h.output_folder(), r.root.join("Animals (2026)"));
        assert!(r.root.join("Animals (2026)/poster.jpg").is_file());
        assert!(crate::engine::has_complete(&session, &key));
        // Moving it again is a no-op ("already").
        let b = post(&http_client(), &r.peer, "/v1/archive/begin", &serde_json::to_value(&p.req).unwrap(), Duration::from_secs(10)).await.unwrap();
        assert_eq!(b["already"], json!(true));
    }

    #[test]
    fn archive_queue_manual_pick_goes_next() {
        let t = |k: &str| Item::Torrent { key: k.repeat(40) };
        let names = |q: &[QEntry]| q.iter().map(|e| (e.item.clone(), e.state.clone())).collect::<Vec<_>>();
        let mut q = Vec::new();
        let cands: Vec<(Item, String)> = ["a", "b", "c", "d"].iter().map(|k| (t(k), k.to_string())).collect();
        q_sync_auto(&mut q, &cands, |_| true);
        assert_eq!(q.len(), 4);
        q[0].state = "moving".into();
        // Manual pick of "d": right after the in-flight "a", ahead of b and c.
        q_enqueue(&mut q, t("d"), "d".into(), true);
        let order: Vec<Item> = q.iter().map(|e| e.item.clone()).collect();
        assert_eq!(order, vec![t("a"), t("d"), t("b"), t("c")]);
        assert_eq!(q[0].state, "moving", "in-flight transfer untouched");
        assert_eq!(q_next(&q, |_| false), Some(t("d")));
        // Re-syncing auto keeps the manual position; a vanished candidate is dropped.
        q_sync_auto(&mut q, &cands[..2], |_| true);
        assert_eq!(names(&q).into_iter().map(|x| x.0).collect::<Vec<_>>(), vec![t("a"), t("d"), t("b")]);
        // Verifying also counts as in flight; an errored item re-queued by hand goes next.
        q[0].state = "verifying".into();
        q.push(q_entry(t("e"), "e".into(), false));
        q[3].state = "error".into();
        q_enqueue(&mut q, t("e"), "e".into(), true);
        assert_eq!(q[1].item, t("e"));
        assert_eq!(q[1].state, "queued");
        // Busy items are skipped; enqueueing the in-flight item is a no-op.
        assert_eq!(q_next(&q, |i| i == &t("e")), Some(t("d")));
        q_enqueue(&mut q, t("a"), "a".into(), true);
        assert_eq!(q[0].item, t("a"));
        assert_eq!(q[0].state, "verifying");
        // Auto off: waiting auto entries go, manual ones stay; stale done entries go.
        q[0].state = "done".into();
        q[0].done_at = Some(Instant::now() - DONE_SHOW);
        q_prune(&mut q, false);
        assert_eq!(names(&q), vec![(t("e"), "queued".into()), (t("d"), "queued".into())]);
        let s = serde_json::to_value(&q[0]).unwrap();
        assert!(s.get("manual").is_none() && s.get("done_at").is_none());
    }

    #[test]
    fn autoclean_selection() {
        use crate::cleanup::{select, FInfo, TInfo};
        assert_eq!(effective_autoclean(None, true), Some(30));
        assert_eq!(effective_autoclean(None, false), None);
        assert_eq!(effective_autoclean(Some(0), true), None);
        assert_eq!(effective_autoclean(Some(7), true), Some(7));
        assert_eq!(effective_autoclean(Some(7), false), None);
        const DAY: u64 = 86_400;
        let now = 1000 * DAY;
        let f = |index, age: u64| FInfo { index, name: format!("e{index}"), size: 1_000_000_000, done: true, media: true, mtime: Some(now - age * DAY) };
        let ts = vec![
            TInfo { key: "a".repeat(40), name: "Old watched".into(), completed: true, files: vec![f(0, 40)] },
            TInfo { key: "b".repeat(40), name: "New watched".into(), completed: true, files: vec![f(0, 5)] },
            TInfo { key: "c".repeat(40), name: "Old unwatched".into(), completed: true, files: vec![f(0, 90)] },
            TInfo { key: "d".repeat(40), name: "Pack".into(), completed: true, files: vec![f(0, 60), f(1, 60)] },
        ];
        let mut w = BTreeMap::new();
        for k in [format!("{}:0", "a".repeat(40)), format!("{}:0", "b".repeat(40)), format!("{}:1", "d".repeat(40))] {
            w.insert(k, true);
        }
        let days = effective_autoclean(None, true).unwrap();
        let items = autoclean_items(&select(&ts, &w, now, days));
        let got: Vec<(String, Option<usize>)> = items.iter().map(|i| (i.key[..1].to_string(), i.file_index)).collect();
        assert_eq!(got, vec![("a".to_string(), None), ("d".to_string(), Some(1))]);
        assert_eq!(freed_text(6_400_000_000, 3), "Freed 6.4 GB (3 items)");
        assert_eq!(freed_text(12_000_000_000, 1), "Freed 12 GB (1 item)");
    }

    #[test]
    fn content_range_and_begin_validation() {
        assert_eq!(parse_content_range("bytes 0-99/100"), Some((0, 99, 100)));
        assert_eq!(parse_content_range("bytes x-1/2"), None);
        let d = data(4, 9);
        let mut r = yt_req(K, vec![wf("a", &d), wf("a/b", &d)], "a");
        assert!(r.validate().is_err(), "file and folder at once");
        r.files.pop();
        assert!(r.validate().is_ok());
        r.kind = "torrent".into();
        assert!(r.validate().is_err(), "torrent without bytes");
        r.kind = "yt".into();
        r.key = "xyz".into();
        assert!(r.validate().is_err());
    }
}
