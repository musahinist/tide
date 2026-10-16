//! Torrent logic + torrent commands (WP1).
//!
//! Errors are `"CODE: message"` strings (see types.rs / PLAN.md).
//! `key` is always the 40-char lowercase hex info hash.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use librqbit::api::TorrentIdOrHash;
use librqbit::dht::Id20;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Magnet, ManagedTorrent, ManagedTorrentState,
    Session, TorrentStatsState,
};
use tauri::State;
use tokio::io::AsyncReadExt;

use crate::state::{AppState, PendingMagnet};
use crate::types::{AddMagnetResult, FileKind, FileRow, TorrentDetails, TorrentRow, TorrentState};

type St<'a> = State<'a, Arc<AppState>>;
type Handle = Arc<ManagedTorrent>;

/// Resolve failures written by the background resolve task (it only holds `'static`
/// data, so it can't reach `AppState.pending`). `list_rows`/`pending_snapshot` fold
/// these into the pending entries.
static RESOLVE_ERRORS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------- errors

fn invalid(msg: impl std::fmt::Display) -> String {
    format!("INVALID_INPUT: {msg}")
}
fn not_found(msg: impl std::fmt::Display) -> String {
    format!("NOT_FOUND: {msg}")
}
fn not_ready(msg: impl std::fmt::Display) -> String {
    format!("NOT_READY: {msg}")
}
fn internal(msg: impl std::fmt::Display) -> String {
    format!("INTERNAL: {msg}")
}
fn completed() -> String {
    "COMPLETED: download finished; completed torrents stay stopped (no seeding)".into()
}

// ---------------------------------------------------------------- media rules

const VIDEO_EXT: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "mov", "wmv", "flv", "webm", "mpg", "mpeg", "ts", "m2ts", "vob",
    "ogv", "3gp",
];
const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "wma", "aiff", "mka",
];
const SUBTITLE_EXT: &[&str] = &["srt", "vtt", "ass", "ssa"];
const BUILTIN_EXT: &[&str] = &["mp4", "m4v", "mov", "webm", "mp3", "m4a", "aac", "wav", "flac"];

fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn kind_of(name: &str) -> (FileKind, bool) {
    let ext = ext_of(name);
    let e = ext.as_str();
    let kind = if VIDEO_EXT.contains(&e) {
        FileKind::Video
    } else if AUDIO_EXT.contains(&e) {
        FileKind::Audio
    } else if SUBTITLE_EXT.contains(&e) {
        FileKind::Subtitle
    } else {
        FileKind::Other
    };
    (kind, BUILTIN_EXT.contains(&e))
}

// ---------------------------------------------------------------- helpers

fn parse_key(key: &str) -> Result<Id20, String> {
    let k = key.trim();
    if k.len() != 40 || !k.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(format!("bad key {key:?}")));
    }
    Id20::from_str(k).map_err(|e| invalid(format!("bad key {key:?}: {e}")))
}

/// Session handle for a key. Pending (resolving) keys → NOT_READY, unknown → NOT_FOUND.
pub(crate) fn handle(st: &AppState, key: &str) -> Result<Handle, String> {
    let id = parse_key(key)?;
    if let Some(h) = st.session.get(TorrentIdOrHash::Hash(id)) {
        return Ok(h);
    }
    let k = id.as_string();
    if st.pending.lock().unwrap().contains_key(&k) {
        return Err(not_ready(format!("torrent {k} is still resolving metadata")));
    }
    Err(not_found(format!("no torrent {k}")))
}

fn display_name(h: &ManagedTorrent) -> String {
    h.name()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| h.info_hash().as_string())
}

/// For single-file torrents the file itself, otherwise the torrent folder.
fn output_path(h: &ManagedTorrent) -> PathBuf {
    let folder = h.output_folder().to_path_buf();
    let single = h
        .with_metadata(|m| {
            let real: Vec<_> = m.file_infos.iter().filter(|f| !f.attrs.padding).collect();
            (real.len() == 1).then(|| real[0].relative_filename.clone())
        })
        .ok()
        .flatten();
    match single {
        Some(rel) => folder.join(rel),
        None => folder,
    }
}

fn map_state(s: &TorrentStatsState, finished: bool) -> TorrentState {
    match s {
        // A pause requested during the initial check leaves the torrent parked in
        // Initializing{paused:true} until resumed, so show it as paused.
        TorrentStatsState::Initializing { paused: true } => TorrentState::Paused,
        TorrentStatsState::Initializing { .. } => TorrentState::Initializing,
        // Finished torrents never seed: a Live+finished one is paused by the next tick.
        TorrentStatsState::Live | TorrentStatsState::Paused if finished => TorrentState::Completed,
        TorrentStatsState::Live => TorrentState::Downloading,
        TorrentStatsState::Paused => TorrentState::Paused,
        TorrentStatsState::Error => TorrentState::Error,
    }
}

fn row_for(id: usize, h: &ManagedTorrent) -> TorrentRow {
    let s = h.stats();
    let (down_bps, up_bps, peers) = match &s.live {
        Some(l) => (
            l.download_speed.as_bytes() as f64,
            l.upload_speed.as_bytes() as f64,
            l.snapshot.peer_stats.live,
        ),
        None => (0.0, 0.0, 0),
    };
    let state = map_state(&s.state, s.finished);
    let remaining = s.total_bytes.saturating_sub(s.progress_bytes);
    let eta_secs = if state == TorrentState::Downloading && down_bps >= 1.0 && remaining > 0 {
        Some((remaining as f64 / down_bps).ceil() as u64)
    } else {
        None
    };
    let progress = if s.total_bytes > 0 {
        (s.progress_bytes as f64 / s.total_bytes as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    TorrentRow {
        key: h.info_hash().as_string(),
        id: Some(id),
        name: display_name(h),
        state,
        error: s.error,
        total_bytes: s.total_bytes,
        progress_bytes: s.progress_bytes,
        progress,
        uploaded_bytes: s.uploaded_bytes,
        down_bps,
        up_bps,
        peers,
        eta_secs,
        output_path: Some(output_path(h).to_string_lossy().into_owned()),
    }
}

/// Fold finished resolve tasks into the pending map: drop entries whose torrent is now in
/// the session, record errors for failed ones.
fn reconcile_pending(st: &AppState) {
    let mut pending = st.pending.lock().unwrap();
    if pending.is_empty() {
        return;
    }
    let mut errors = RESOLVE_ERRORS.lock().unwrap();
    let before = pending.len();
    pending.retain(|k, p| {
        if let Some(e) = errors.remove(k) {
            p.error = Some(e);
            p.task = None;
            return true;
        }
        let in_session = Id20::from_str(k)
            .ok()
            .and_then(|id| st.session.get(TorrentIdOrHash::Hash(id)))
            .is_some();
        !in_session
    });
    if pending.len() != before {
        save_pending(st, &pending);
    }
}

// ---------------------------------------------------------------- pending.json

fn pending_file(st: &AppState) -> PathBuf {
    st.data_dir.join("pending.json")
}

/// Persist the magnets that are still resolving (or failed), so they survive a restart.
fn save_pending(st: &AppState, pending: &HashMap<String, PendingMagnet>) {
    let mut magnets: Vec<&str> = pending.values().map(|p| p.magnet.as_str()).collect();
    magnets.sort_unstable();
    let path = pending_file(st);
    let res = if magnets.is_empty() {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    } else {
        let json = serde_json::to_vec_pretty(&magnets).unwrap_or_default();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &path))
    };
    if let Err(e) = res {
        tracing::warn!("saving {} failed: {e}", path.display());
    }
}

/// Re-add magnets that were still resolving when the app last quit.
pub async fn restore_pending(st: &AppState) {
    let path = pending_file(st);
    let Ok(bytes) = std::fs::read(&path) else { return };
    let magnets: Vec<String> = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("ignoring bad {}: {e}", path.display());
            return;
        }
    };
    for m in magnets {
        match add_magnet_internal(st, &m).await {
            Ok(r) => tracing::info!(key = %r.key, "restored pending magnet"),
            Err(e) => tracing::warn!("restoring pending magnet failed: {e}"),
        }
    }
    // Drops entries that were already in the session (resolved before quit).
    let pending = st.pending.lock().unwrap();
    save_pending(st, &pending);
}

// ---------------------------------------------------------------- internal fns

pub fn list_rows(st: &AppState) -> Vec<TorrentRow> {
    reconcile_pending(st);

    let mut rows: Vec<TorrentRow> = st
        .session
        .with_torrents(|it| it.map(|(id, h)| row_for(id, h)).collect());
    // Newest first.
    rows.sort_by(|a, b| b.id.cmp(&a.id));

    let pending = st.pending.lock().unwrap();
    let mut prow: Vec<TorrentRow> = pending
        .iter()
        .filter(|(k, _)| !rows.iter().any(|r| &r.key == *k))
        .map(|(k, p)| TorrentRow {
            key: k.clone(),
            id: None,
            name: p.name.clone(),
            state: if p.error.is_some() {
                TorrentState::Error
            } else {
                TorrentState::Resolving
            },
            error: p.error.clone(),
            total_bytes: 0,
            progress_bytes: 0,
            progress: 0.0,
            uploaded_bytes: 0,
            down_bps: 0.0,
            up_bps: 0.0,
            peers: 0,
            eta_secs: None,
            output_path: None,
        })
        .collect();
    prow.sort_by(|a, b| a.name.cmp(&b.name));
    prow.extend(rows);
    prow
}

pub fn files(st: &AppState, key: &str) -> Result<Vec<FileRow>, String> {
    let h = handle(st, key)?;
    let s = h.stats();
    h.with_metadata(|m| {
        m.file_infos
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.attrs.padding)
            .map(|(index, f)| {
                let name = f
                    .relative_filename
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let progress_bytes = s.file_progress.get(index).copied().unwrap_or(0).min(f.len);
                let progress = if f.len > 0 {
                    progress_bytes as f64 / f.len as f64
                } else {
                    1.0
                };
                let (kind, playable_builtin) = kind_of(&name);
                FileRow {
                    index,
                    name,
                    size: f.len,
                    progress_bytes,
                    progress,
                    kind,
                    playable_builtin,
                }
            })
            .collect()
    })
    .map_err(|_| not_ready("metadata not resolved yet"))
}

fn file_rel_path(h: &ManagedTorrent, idx: usize) -> Result<PathBuf, String> {
    h.with_metadata(|m| m.file_infos.get(idx).map(|f| f.relative_filename.clone()))
        .map_err(|_| not_ready("metadata not resolved yet"))?
        .ok_or_else(|| not_found(format!("no file #{idx}")))
}

pub fn file_abs_path(st: &AppState, key: &str, idx: usize) -> Result<PathBuf, String> {
    let h = handle(st, key)?;
    let rel = file_rel_path(&h, idx)?;
    Ok(h.output_folder().join(rel))
}

/// http://127.0.0.1:{port}/torrents/{id}/stream/{idx}/{urlencoded basename}
pub fn stream_url(st: &AppState, key: &str, idx: usize) -> Result<String, String> {
    let h = handle(st, key)?;
    let rel = file_rel_path(&h, idx)?;
    let base = rel
        .file_name()
        .map(|b| b.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("file{idx}"));
    Ok(format!(
        "http://127.0.0.1:{}/torrents/{}/stream/{}/{}",
        st.stream_port,
        h.id(),
        idx,
        urlencoding::encode(&base)
    ))
}

/// How long play/resume wait for a file check ("Initializing") to finish.
const INIT_TIMEOUT_S: u64 = 30;

/// Wait (bounded) until the torrent is out of Initializing. librqbit's streams need
/// Paused or Live.
async fn wait_initialized(h: &ManagedTorrent) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(INIT_TIMEOUT_S), h.wait_until_initialized())
        .await
        .map_err(|_| not_ready("still checking files"))?
        .map_err(|e| internal(format!("torrent failed to initialize: {e:#}")))
}

/// Unpause if paused (also retries a torrent in error state) and wait until it is
/// initialized. Never wakes a completed torrent: returns `COMPLETED` instead.
pub async fn ensure_live(st: &AppState, key: &str) -> Result<(), String> {
    let h = handle(st, key)?;
    // Don't call h.is_paused() inside with_state: nested read lock.
    let paused_flag = h.is_paused();
    let checking = h.with_state(|s| matches!(s, ManagedTorrentState::Initializing(_)));
    if checking && !paused_flag {
        wait_initialized(&h).await?; // so `finished` is known
    }
    if h.stats().finished {
        return Err(completed());
    }
    let paused_flag = h.is_paused();
    let needs_start = h.with_state(|s| match s {
        ManagedTorrentState::Live(_) => false,
        ManagedTorrentState::Initializing(_) => paused_flag,
        _ => true,
    });
    if needs_start {
        st.session
            .unpause(&h)
            .await
            .map_err(|e| internal(format!("resume failed: {e:#}")))?;
    }
    wait_initialized(&h).await?;
    // A torrent parked mid-check can turn out complete only now.
    if h.stats().finished {
        let _ = st.session.pause(&h).await;
        return Err(completed());
    }
    Ok(())
}

/// Keys seen unfinished during this run: only those get a "download finished"
/// notification (a torrent restored already complete stays silent).
static SEEN_UNFINISHED: LazyLock<Mutex<std::collections::HashSet<String>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// Pause every Live torrent that has finished downloading (called by the 1 s ticker):
/// Tide never uploads after a download completes. Sends a completion notification.
pub async fn pause_finished(st: &AppState) {
    // (handle, finished+Live, unfinished-with-metadata)
    let all: Vec<(Handle, bool, bool)> = st.session.with_torrents(|it| {
        it.map(|(_, h)| {
            let s = h.stats();
            (h.clone(), s.finished && matches!(s.state, TorrentStatsState::Live), !s.finished && s.total_bytes > 0)
        })
        .collect()
    });
    let mut done: Vec<Handle> = Vec::new();
    {
        let mut seen = SEEN_UNFINISHED.lock().unwrap();
        for (h, live_done, unfinished) in all {
            if unfinished {
                seen.insert(h.info_hash().as_string());
            } else if live_done {
                done.push(h);
            }
        }
    }
    for h in done {
        let key = h.info_hash().as_string();
        match st.session.pause(&h).await {
            Ok(()) => tracing::info!(key = %key, "download complete; paused (no seeding)"),
            Err(e) => tracing::warn!("auto-pause of finished torrent failed: {e:#}"),
        }
        if SEEN_UNFINISHED.lock().unwrap().remove(&key) {
            crate::notify::notify("Download finished", &display_name(&h));
        }
    }
}

/// api_stream + read_to_end (prioritizes pieces). If the file is not complete yet the
/// torrent must be running: with `may_resume` it is unpaused, otherwise a paused/errored
/// torrent gives NOT_READY (never resume what the user paused for a background fetch).
pub async fn read_file(
    st: &AppState,
    key: &str,
    idx: usize,
    max_bytes: usize,
    timeout_s: u64,
    may_resume: bool,
) -> Result<Vec<u8>, String> {
    let h = handle(st, key)?;
    let len = h
        .with_metadata(|m| m.file_infos.get(idx).map(|f| f.len))
        .map_err(|_| not_ready("metadata not resolved yet"))?
        .ok_or_else(|| not_found(format!("no file #{idx}")))?;
    if len > max_bytes as u64 {
        return Err(invalid(format!(
            "file #{idx} is {len} bytes, over the {max_bytes} byte cap"
        )));
    }
    let have = h.stats().file_progress.get(idx).copied().unwrap_or(0);
    if have < len {
        if may_resume {
            ensure_live(st, key).await?;
        } else {
            let paused_flag = h.is_paused();
            let running = h.with_state(|s| match s {
                ManagedTorrentState::Live(_) => true,
                ManagedTorrentState::Initializing(_) => !paused_flag,
                _ => false,
            });
            if !running {
                return Err(not_ready("torrent is not running; file not downloaded yet"));
            }
        }
    }

    let fut = async {
        // The stream needs the torrent out of Initializing.
        h.wait_until_initialized()
            .await
            .map_err(|e| internal(format!("torrent failed to initialize: {e:#}")))?;
        let stream = st
            .api
            .api_stream(TorrentIdOrHash::Id(h.id()), idx)
            .await
            .map_err(|e| internal(format!("stream failed: {e}")))?;
        let mut buf = Vec::with_capacity(len as usize);
        stream
            .take(max_bytes as u64)
            .read_to_end(&mut buf)
            .await
            .map_err(|e| internal(format!("read failed: {e}")))?;
        Ok::<_, String>(buf)
    };
    tokio::time::timeout(Duration::from_secs(timeout_s), fut)
        .await
        .map_err(|_| not_ready(format!("timed out after {timeout_s}s reading file #{idx}")))?
}

/// Well-known public trackers appended to every magnet we add. Many magnets carry
/// few or no trackers; these make metadata/peer discovery much faster than DHT
/// alone. (Magnets can't be private-flagged, so this never leaks a private torrent.)
const EXTRA_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://explodie.org:6969/announce",
    "udp://tracker.dler.org:6969/announce",
    "udp://open.tracker.cl:1337/announce",
    "udp://tracker.qu.ax:6969/announce",
];

/// Return `magnet` (a `magnet:` URI or bare 40-hex info hash) as a `magnet:` URI
/// with `EXTRA_TRACKERS` appended as `tr=` params, skipping ones already present.
/// librqbit 9.0.1 ignores `AddTorrentOptions::trackers` for magnets (it only reads
/// the magnet's own `tr=`), hence the string append.
fn with_extra_trackers(magnet: &str, existing: &[String]) -> String {
    let norm = |t: &str| t.trim().trim_end_matches('/').to_ascii_lowercase();
    let have: std::collections::HashSet<String> = existing.iter().map(|t| norm(t)).collect();
    let mut out = if magnet.starts_with("magnet:") {
        magnet.to_string()
    } else {
        format!("magnet:?xt=urn:btih:{magnet}")
    };
    for tr in EXTRA_TRACKERS {
        if !have.contains(&norm(tr)) {
            out.push_str("&tr=");
            out.push_str(tr); // scheme/host/path chars are all query-safe
        }
    }
    out
}

// ---------------------------------------------------------------- same-name collisions

/// Names our metadata feature writes into a torrent folder (not "foreign" content).
const SIDECAR_NAMES: &[&str] = &["tide.json", ".tide.json.tmp", "poster.jpg", "fanart.jpg", "movie.nfo", "tvshow.nfo"];

/// "Sintel" -> first of "Sintel (2)", "Sintel (3)", … that is not `taken`.
pub fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    (2u32..)
        .map(|n| format!("{base} ({n})"))
        .find(|c| !taken(c))
        .expect("unbounded range")
}

/// Does the existing `target` look like an earlier download of this same torrent
/// (resume/seed it in place) rather than someone else's files?
fn same_content(target: &Path, single: bool, files: &[(PathBuf, u64)], info_hash: &str) -> bool {
    if single {
        return std::fs::metadata(target).is_ok_and(|m| m.is_file() && m.len() <= files[0].1);
    }
    if let Ok(b) = std::fs::read(target.join("tide.json")) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
            if v.get("key").and_then(|k| k.as_str()) == Some(info_hash) {
                return true;
            }
        }
    }
    let tops: std::collections::HashSet<String> = files
        .iter()
        .filter_map(|(p, _)| p.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()))
        .collect();
    let Ok(rd) = std::fs::read_dir(target) else { return false };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !(tops.contains(&n) || n.starts_with('.') || SIDECAR_NAMES.contains(&n.to_lowercase().as_str())) {
            return false;
        }
    }
    files.iter().all(|(p, len)| std::fs::metadata(target.join(p)).map_or(true, |m| m.len() <= *len))
}

/// Pick the output folder for a resolved torrent: every torrent gets its own folder
/// `Tidebox/<clean name>` (single files go inside it). If that folder is used by another
/// torrent or holds different content on disk, a unique sibling "Name (2)".
fn choose_output_folder(session: &Session, tidebox: &Path, base: &str, files: &[(PathBuf, u64)], info_hash: &str) -> PathBuf {
    let used_by_other = |p: &Path| {
        session.with_torrents(|it| {
            it.filter(|(_, h)| {
                h.info_hash().as_string() != info_hash && (output_path(h) == p || h.output_folder() == p)
            })
            .count()
                > 0
        })
    };
    let target = tidebox.join(base);
    let clash = used_by_other(&target) || (target.exists() && !same_content(&target, false, files, info_hash));
    if !clash {
        return target;
    }
    let name = unique_name(base, |c| {
        let p = tidebox.join(c);
        p.exists() || used_by_other(&p)
    });
    tracing::info!(target = %target.display(), chosen = %name, "output folder taken by other content; using a unique folder");
    tidebox.join(name)
}

/// Resolve the magnet (list_only), choose a non-clashing output folder, then add the
/// torrent from the resolved metadata (no second resolve; seen peers are reused).
async fn resolve_and_add(session: &Arc<Session>, tidebox: &Path, magnet: String) -> anyhow::Result<AddTorrentResponse> {
    let lo = session
        .add_torrent(
            AddTorrent::from_url(magnet),
            Some(AddTorrentOptions { list_only: true, ..Default::default() }),
        )
        .await?;
    let lo = match lo {
        AddTorrentResponse::ListOnly(lo) => lo,
        other => return Ok(other),
    };
    let info_hash = lo.info_hash.as_string();
    if let Some(h) = session.get(TorrentIdOrHash::Hash(lo.info_hash)) {
        return Ok(AddTorrentResponse::AlreadyManaged(h.id(), h));
    }
    let files: Vec<(PathBuf, u64)> = lo
        .info
        .iter_file_details()
        .filter(|d| !d.attrs().padding)
        .map(|d| (d.filename.to_pathbuf(), d.len))
        .collect();
    // Default folder of a multi-file torrent is <session dir>/<torrent name>.
    let single = files.len() == 1;
    let base = if single {
        folder_base(&files[0].0, true)
    } else {
        folder_base(&lo.output_folder, false)
    };
    let folder = choose_output_folder(session, tidebox, &base, &files, &info_hash);
    let _ = std::fs::create_dir_all(&folder);
    let opts = AddTorrentOptions {
        overwrite: true,
        output_folder: Some(folder.to_string_lossy().into_owned()),
        only_files: lo.only_files.clone(),
        initial_peers: Some(lo.seen_peers.clone()),
        ..Default::default()
    };
    session.add_torrent(AddTorrent::from_bytes(lo.torrent_bytes), Some(opts)).await
}

/// Shared by the `add_magnet` command and the magnet: deep-link handler.
/// Returns immediately; metadata is resolved by a background task tracked in
/// `AppState.pending`.
pub async fn add_magnet_internal(st: &AppState, magnet: &str) -> Result<AddMagnetResult, String> {
    let magnet = magnet.trim();
    let is_hex = magnet.len() == 40 && magnet.bytes().all(|b| b.is_ascii_hexdigit());
    let is_magnet = magnet
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("magnet:"));
    if !is_hex && !is_magnet {
        return Err(invalid("not a magnet link"));
    }
    // Normalise scheme case so librqbit's `starts_with("magnet:")` check matches.
    let magnet: String = if is_magnet {
        format!("magnet:{}", &magnet[7..])
    } else {
        magnet.to_ascii_lowercase()
    };
    let parsed = Magnet::parse(&magnet).map_err(|e| invalid(format!("bad magnet: {e:#}")))?;
    let id = parsed
        .as_id20()
        .ok_or_else(|| invalid("magnet has no BitTorrent v1 (btih) info hash"))?;
    let key = id.as_string();
    let dn = parsed.name.clone().filter(|n| !n.trim().is_empty());

    if let Some(h) = st.session.get(TorrentIdOrHash::Hash(id)) {
        return Ok(AddMagnetResult {
            key,
            name: display_name(&h),
            already_exists: true,
        });
    }

    reconcile_pending(st);
    let mut pending = st.pending.lock().unwrap();
    if let Some(p) = pending.get(&key) {
        if p.error.is_none() {
            return Ok(AddMagnetResult {
                key,
                name: p.name.clone(),
                already_exists: true,
            });
        }
        // Previous attempt failed: retry below (replaces the error row).
    }

    let name = dn.unwrap_or_else(|| key.clone());
    let session = st.session.clone();
    let task_key = key.clone();
    let stored_magnet = magnet.clone();
    let magnet = with_extra_trackers(&magnet, &parsed.trackers);
    // The user may have deleted ~/Downloads/Tidebox while the app runs.
    if let Err(e) = std::fs::create_dir_all(&st.download_dir) {
        return Err(internal(format!("create {}: {e}", st.download_dir.display())));
    }
    let tidebox = st.download_dir.clone();
    let task = tokio::spawn(async move {
        let res = resolve_and_add(&session, &tidebox, magnet).await;
        match res {
            Ok(AddTorrentResponse::Added(id, _)) => {
                tracing::info!(key = %task_key, id, "magnet resolved and added")
            }
            Ok(AddTorrentResponse::AlreadyManaged(id, _)) => {
                tracing::info!(key = %task_key, id, "magnet already managed")
            }
            Ok(AddTorrentResponse::ListOnly(_)) => {
                RESOLVE_ERRORS
                    .lock()
                    .unwrap()
                    .insert(task_key, "unexpected list-only response".into());
            }
            Err(e) => {
                tracing::warn!(key = %task_key, "resolving magnet failed: {e:#}");
                RESOLVE_ERRORS
                    .lock()
                    .unwrap()
                    .insert(task_key, format!("{e:#}"));
            }
        }
    });
    if let Some(old) = pending.insert(
        key.clone(),
        PendingMagnet {
            magnet: stored_magnet,
            name: name.clone(),
            error: None,
            task: Some(task),
        },
    ) {
        if let Some(t) = old.task {
            t.abort();
        }
    }
    save_pending(st, &pending);
    Ok(AddMagnetResult {
        key,
        name,
        already_exists: false,
    })
}

// ---------------------------------------------------------------- folder names

const TLDS: &str = "to|org|com|net|io|me|cc|tv|info|ws|ru|xyz|lol|se|is|in|co|nz|uk|ch|li|pw|vip|club|site|top|biz|eu|am|mx|st|sx|lt|ag|nu";

/// `www.UIndex.org    -    ` / `site.to - ` prefixes and ` - www.site.org` suffixes.
static SITE_AFFIX: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(&format!(
        r"(?i)^\s*(?:www\.[\w-]+(?:\.[\w-]+)*\.(?:{TLDS})\s*[-–—|:]?|[\w-]+(?:\.[\w-]+)*\.(?:{TLDS})\s*[-–—|])\s*|\s*[-–—|]\s*(?:www\.)?[\w-]+(?:\.[\w-]+)*\.(?:{TLDS})\s*$"
    ))
    .expect("valid regex")
});
/// `[EZTVx.to]`, `[ www.site.org ]`, `【site】`, `[eztv]`, `(RARBG)`… anywhere in the name.
static SITE_TAG: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(&format!(
        r"(?i)【[^】]*】|[\[(]\s*(?:(?:www\.)?[\w-]+(?:\.[\w-]+)*\.(?:{TLDS})|eztv\w*|ettv|rarbg|tgx|yts(?:\.\w+)?|1337x|torrentgalaxy|glodls)\s*[\])]"
    ))
    .expect("valid regex")
});

/// Folder name for a torrent inside Tidebox: site prefixes/suffixes/tags removed, runs of
/// whitespace collapsed, `:`/`/`/control chars replaced, no leading dot, ≤ 200 bytes.
/// Idempotent (so an already-clean folder is never renamed again).
pub fn clean_folder_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c == ':' || c == '/' || c == '\\' { '-' } else if c.is_control() { ' ' } else { c })
        .collect();
    loop {
        let before = s.clone();
        s = SITE_TAG.replace_all(&s, " ").into_owned();
        s = SITE_AFFIX.replace_all(&s, "").into_owned();
        s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        s = s.trim_matches(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '|').to_string();
        s = s.trim_start_matches('.').to_string();
        if s.len() > 200 {
            let mut end = 200;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
        }
        if s == before {
            break;
        }
    }
    if s.is_empty() || s == ".." {
        "Download".into()
    } else {
        s
    }
}

/// Base for a torrent's folder name: the torrent folder name, or a single file's stem.
fn folder_base(name: &Path, single: bool) -> String {
    let n = if single { name.file_stem() } else { name.file_name() };
    clean_folder_name(&n.map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
}

// ---------------------------------------------------------------- Tidebox migration

/// rename(2); on failure (e.g. another volume) copy recursively, then remove the source.
fn move_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} exists", dst.display())));
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    fn copy_rec(src: &Path, dst: &Path) -> std::io::Result<()> {
        if src.is_dir() {
            std::fs::create_dir(dst)?;
            for e in std::fs::read_dir(src)? {
                let e = e?;
                copy_rec(&e.path(), &dst.join(e.file_name()))?;
            }
            Ok(())
        } else {
            std::fs::copy(src, dst).map(|_| ())
        }
    }
    if let Err(e) = copy_rec(src, dst) {
        // Never leave a half copy behind; the source is untouched.
        let _ = if dst.is_dir() { std::fs::remove_dir_all(dst) } else { std::fs::remove_file(dst) };
        return Err(e);
    }
    if src.is_dir() { std::fs::remove_dir_all(src) } else { std::fs::remove_file(src) }
}

/// Is this torrent already laid out as `Tidebox/<clean folder>/…`?
fn in_own_folder(tidebox: &Path, folder: &Path, single: bool) -> bool {
    if folder.parent() != Some(tidebox) {
        return false;
    }
    // Single-file folders are never renamed once they exist; multi-file ones get the
    // cleaned name (clean_folder_name is idempotent, "Name (2)" stays).
    single || folder.file_name().is_some_and(|n| clean_folder_name(&n.to_string_lossy()) == n.to_string_lossy())
}

/// Startup: give every torrent its own clean folder in `~/Downloads/Tidebox` (older
/// versions put files in ~/Downloads, single files loose in Tidebox, folders with site
/// prefixes). Forget the torrent (files kept), move files + our sidecars, re-add it from
/// its stored .torrent bytes (same info hash: metadata cache / groups / watched still
/// match; librqbit re-verifies the data, so progress is kept). Any failure leaves (or
/// puts back) the torrent where it was.
pub async fn migrate_to_tidebox(st: &AppState) {
    let tidebox = st.download_dir.clone();
    let handles: Vec<Handle> = st.session.with_torrents(|it| it.map(|(_, h)| h.clone()).collect());
    for h in handles {
        let key = h.info_hash().as_string();
        match migrate_one(st, &tidebox, &h).await {
            Ok(Some(to)) => tracing::info!(%key, to = %to.display(), "migrated torrent into its Tidebox folder"),
            Ok(None) => {}
            Err(e) => tracing::warn!(%key, "Tidebox migration skipped; torrent left in place: {e}"),
        }
    }
}

/// `<stem>.tide.json` etc. next to a loose single file → folder-style names.
fn legacy_sidecars(dir: &Path, stem: &str) -> Vec<(PathBuf, String)> {
    let mut v = vec![];
    for (old, new) in [
        (format!("{stem}.tide.json"), "tide.json"),
        (format!("{stem}-poster.jpg"), "poster.jpg"),
        (format!("{stem}-fanart.jpg"), "fanart.jpg"),
    ] {
        v.push((dir.join(old), new.to_string()));
    }
    let nfo = dir.join(format!("{stem}.nfo"));
    let name = match std::fs::read_to_string(&nfo) {
        Ok(t) if t.contains("<movie") => "movie.nfo",
        _ => "tvshow.nfo",
    };
    v.push((nfo, name.to_string()));
    v.into_iter().filter(|(p, _)| p.is_file()).collect()
}

async fn migrate_one(st: &AppState, tidebox: &Path, h: &Handle) -> Result<Option<PathBuf>, String> {
    let key = h.info_hash().as_string();
    let old_folder = h.output_folder().to_path_buf();
    let (bytes, real) = h
        .with_metadata(|m| {
            let real: Vec<PathBuf> =
                m.file_infos.iter().filter(|f| !f.attrs.padding).map(|f| f.relative_filename.clone()).collect();
            (m.torrent_bytes.clone(), real)
        })
        .map_err(|e| format!("no metadata: {e:#}"))?;
    let single = real.len() == 1;
    if in_own_folder(tidebox, &old_folder, single) {
        return Ok(None);
    }
    if bytes.is_empty() {
        return Err("no stored torrent bytes".into());
    }
    let first = |p: &PathBuf| p.components().next().map(|c| PathBuf::from(c.as_os_str()));
    // What moves: a single file (or its top dir if nested) into the new folder; a
    // multi-file torrent's folder becomes the new folder.
    let (src, inner) = if single {
        let top = first(&real[0]).ok_or("empty file name")?;
        (old_folder.join(&top), Some(top))
    } else {
        // Never move a folder that is (or contains) Tidebox, ~/Downloads or $HOME.
        if tidebox.starts_with(&old_folder) {
            return Err(format!("output folder {} contains Tidebox", old_folder.display()));
        }
        (old_folder.clone(), None)
    };
    if !src.exists() {
        return Err(format!("{} does not exist", src.display()));
    }
    let base = if single { folder_base(&real[0], true) } else { folder_base(&old_folder, false) };
    let sidecars = match (&inner, real[0].file_stem()) {
        (Some(_), Some(stem)) => legacy_sidecars(&old_folder, &stem.to_string_lossy()),
        _ => vec![],
    };
    let taken_by_other = |p: &Path| {
        st.session.with_torrents(|it| {
            it.filter(|(_, o)| o.info_hash() != h.info_hash() && (output_path(o) == p || o.output_folder() == p)).count() > 0
        })
    };
    let mut new_folder = tidebox.join(&base);
    if new_folder.exists() || taken_by_other(&new_folder) {
        new_folder = tidebox.join(unique_name(&base, |c| {
            let p = tidebox.join(c);
            p.exists() || taken_by_other(&p)
        }));
    }
    let dst = match &inner {
        Some(top) => new_folder.join(top),
        None => new_folder.clone(),
    };
    let paused = h.is_paused() || h.stats().finished;
    let only_files = h.only_files();
    let trackers: Vec<String> = h.shared().trackers.iter().map(|u| u.to_string()).collect();
    let re_add = |folder: PathBuf| {
        let o = AddTorrentOptions {
            paused,
            overwrite: true,
            output_folder: Some(folder.to_string_lossy().into_owned()),
            only_files: only_files.clone(),
            trackers: (!trackers.is_empty()).then(|| trackers.clone()),
            ..Default::default()
        };
        let b = bytes.clone();
        async move { st.session.add_torrent(AddTorrent::from_bytes(b), Some(o)).await }
    };
    std::fs::create_dir_all(tidebox).map_err(|e| format!("create {}: {e}", tidebox.display()))?;
    if single {
        std::fs::create_dir(&new_folder).map_err(|e| format!("create {}: {e}", new_folder.display()))?;
    }

    st.session
        .delete(TorrentIdOrHash::Hash(h.info_hash()), false)
        .await
        .map_err(|e| format!("forget failed: {e:#}"))?;
    if let Err(e) = move_path(&src, &dst) {
        if single {
            let _ = std::fs::remove_dir(&new_folder);
        }
        let back = re_add(old_folder.clone()).await;
        return Err(format!("moving {} failed: {e}; re-added in place: {}", src.display(), back.is_ok()));
    }
    let mut moved_sidecars = vec![];
    for (s, name) in &sidecars {
        let to = new_folder.join(name);
        match move_path(s, &to) {
            Ok(()) => moved_sidecars.push((s.clone(), to)),
            Err(e) => tracing::warn!(%key, "moving sidecar {} failed: {e}", s.display()),
        }
    }
    match re_add(new_folder.clone()).await {
        Ok(_) => Ok(Some(new_folder)),
        Err(e) => {
            // Put everything back and re-add at the old place.
            let _ = move_path(&dst, &src);
            for (s, to) in &moved_sidecars {
                let _ = move_path(to, s);
            }
            if single {
                let _ = std::fs::remove_dir(&new_folder);
            }
            let back = re_add(old_folder).await;
            Err(format!("re-add failed: {e:#}; restored in place: {}", back.is_ok()))
        }
    }
}

// ---------------------------------------------------------------- archive (archive.rs)

/// What an archive move of a completed torrent sends.
pub(crate) struct TorrentLayout {
    pub name: String,
    pub torrent_bytes: Vec<u8>,
    /// The torrent's own output folder (every rel path is under it).
    pub base: PathBuf,
    /// (rel path with '/', size) of every selected, non-padding file.
    pub files: Vec<(String, u64)>,
    pub only_files: Option<Vec<usize>>,
}

pub(crate) fn archive_layout(st: &AppState, key: &str) -> Result<TorrentLayout, String> {
    let h = handle(st, key)?;
    if !h.stats().finished {
        return Err(not_ready("download is not finished"));
    }
    let only = h.only_files();
    let (bytes, files) = h
        .with_metadata(|m| {
            let files: Vec<(String, u64)> = m
                .file_infos
                .iter()
                .enumerate()
                .filter(|(i, f)| !f.attrs.padding && only.as_ref().is_none_or(|o| o.contains(i)))
                .map(|(_, f)| {
                    let rel = f
                        .relative_filename
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    (rel, f.len)
                })
                .collect();
            (m.torrent_bytes.to_vec(), files)
        })
        .map_err(|_| not_ready("metadata not resolved yet"))?;
    if bytes.is_empty() {
        return Err(internal("no stored torrent bytes"));
    }
    let base = h.output_folder().to_path_buf();
    let name = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| display_name(&h));
    Ok(TorrentLayout { name, torrent_bytes: bytes, base, files, only_files: only })
}

/// Is `key` in the session and fully downloaded?
pub(crate) fn has_complete(session: &Session, key: &str) -> bool {
    let Ok(id) = parse_key(key) else { return false };
    session.get(TorrentIdOrHash::Hash(id)).is_some_and(|h| {
        let s = h.stats();
        s.finished && s.total_bytes > 0
    })
}

/// Add a torrent whose data is already complete in `folder`: paused (never seeds, no
/// network), librqbit checks the files. Errors (and forgets it again, files kept) when
/// the check does not find it complete. Returns the key.
pub(crate) async fn add_archived(session: &Arc<Session>, bytes: Vec<u8>, folder: &Path, only_files: Option<Vec<usize>>) -> Result<String, String> {
    let opts = AddTorrentOptions {
        paused: true,
        overwrite: true,
        output_folder: Some(folder.to_string_lossy().into_owned()),
        only_files,
        ..Default::default()
    };
    let h = match session.add_torrent(AddTorrent::from_bytes(bytes), Some(opts)).await {
        Ok(AddTorrentResponse::Added(_, h)) => h,
        Ok(AddTorrentResponse::AlreadyManaged(_, h)) => {
            let key = h.info_hash().as_string();
            return if has_complete(session, &key) { Ok(key) } else { Err(invalid("torrent already managed here and incomplete")) };
        }
        Ok(AddTorrentResponse::ListOnly(_)) => return Err(internal("unexpected list-only answer")),
        Err(e) => return Err(internal(format!("add failed: {e:#}"))),
    };
    let key = h.info_hash().as_string();
    // A full file check of a big archive takes a while.
    let checked = tokio::time::timeout(Duration::from_secs(3 * 3600), h.wait_until_initialized()).await;
    let ok = matches!(checked, Ok(Ok(()))) && {
        let s = h.stats();
        s.finished && s.total_bytes > 0
    };
    if !ok {
        let s = h.stats();
        let _ = session.delete(TorrentIdOrHash::Hash(h.info_hash()), false).await;
        return Err(format!("VERIFY_FAILED: librqbit found {} of {} bytes", s.progress_bytes, s.total_bytes));
    }
    if !h.is_paused() {
        let _ = session.pause(&h).await;
    }
    Ok(key)
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub fn list_torrents(state: St<'_>) -> Result<Vec<TorrentRow>, String> {
    Ok(list_rows(&state))
}

#[tauri::command]
pub async fn add_magnet(state: St<'_>, magnet: String) -> Result<AddMagnetResult, String> {
    add_magnet_internal(&state, &magnet).await
}

#[tauri::command]
pub async fn pause_torrent(state: St<'_>, key: String) -> Result<(), String> {
    pause(&state, &key).await
}

pub(crate) async fn pause(state: &AppState, key: &str) -> Result<(), String> {
    let h = handle(state, key)?;
    let paused_flag = h.is_paused();
    let already = h.with_state(|s| match s {
        ManagedTorrentState::Paused(_) => true,
        ManagedTorrentState::Initializing(_) => paused_flag,
        _ => false,
    });
    if already {
        return Ok(());
    }
    state
        .session
        .pause(&h)
        .await
        .map_err(|e| internal(format!("pause failed: {e:#}")))
}

#[tauri::command]
pub async fn resume_torrent(state: St<'_>, key: String) -> Result<(), String> {
    ensure_live(&state, &key).await
}

/// JS: `{ key, deleteFiles }`.
#[tauri::command]
pub async fn remove_torrent(
    state: St<'_>,
    key: String,
    delete_files: bool,
) -> Result<(), String> {
    remove(&state, &key, delete_files).await
}

pub(crate) async fn remove(state: &AppState, key: &str, delete_files: bool) -> Result<(), String> {
    let id = parse_key(key)?;
    let k = id.as_string();
    let (was_pending, task) = {
        let mut pending = state.pending.lock().unwrap();
        match pending.remove(&k) {
            Some(p) => {
                save_pending(state, &pending);
                (true, p.task)
            }
            None => (false, None),
        }
    };
    // abort() only requests cancellation: wait for the task to really stop, so an add
    // that lands late is still caught by the session check below.
    if let Some(t) = task {
        t.abort();
        let _ = t.await;
    }
    RESOLVE_ERRORS.lock().unwrap().remove(&k);

    // Also covers a resolve task that finished right before the abort.
    if state.session.get(TorrentIdOrHash::Hash(id)).is_some() {
        // Collected before forgetting the torrent (needs its row + file list).
        let (mut sidecars, folder) = if delete_files {
            crate::metadata::sidecar_paths(state, &k)
        } else {
            (vec![], None)
        };
        if delete_files {
            // Online subtitles Tide saved next to the video (removed with the sidecars).
            sidecars.extend(crate::subfetch::owned_files(state, &k));
        }
        state
            .session
            .delete(TorrentIdOrHash::Hash(id), delete_files)
            .await
            .map_err(|e| internal(format!("remove failed: {e:#}")))?;
        for p in sidecars {
            if let Err(e) = std::fs::remove_file(&p) {
                tracing::warn!("removing sidecar {} failed: {e}", p.display());
            }
        }
        // librqbit only deletes torrent-owned files; drop the folder if it is now empty
        // (non-recursive: fails harmlessly when anything else is left).
        if let Some(f) = folder {
            let _ = std::fs::remove_file(f.join(".DS_Store"));
            let _ = std::fs::remove_dir(&f);
        }
        crate::metadata::forget(state, &k);
        if delete_files {
            crate::subfetch::forget(state, &k);
        }
        return Ok(());
    }
    if was_pending {
        Ok(())
    } else {
        Err(not_found(format!("no torrent {k}")))
    }
}

#[tauri::command]
pub async fn reveal_torrent(state: St<'_>, key: String) -> Result<(), String> {
    reveal(&state, &key).await
}

async fn reveal(state: &AppState, key: &str) -> Result<(), String> {
    let h = handle(state, key)?;
    let mut p = output_path(&h);
    if !p.exists() {
        p = h.output_folder().to_path_buf();
    }
    if !p.exists() {
        p = state.download_dir.clone();
    }
    let status = tokio::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(&p)
        .status()
        .await
        .map_err(|e| internal(format!("open -R failed: {e}")))?;
    if !status.success() {
        return Err(internal(format!("open -R exited with {status}")));
    }
    Ok(())
}

#[tauri::command]
pub fn list_files(state: St<'_>, key: String) -> Result<Vec<FileRow>, String> {
    files(&state, &key)
}

/// `~/Downloads/Tidebox` (re-created if the user deleted it).
#[tauri::command]
pub fn get_download_dir(state: St<'_>) -> Result<String, String> {
    let _ = std::fs::create_dir_all(&state.download_dir);
    Ok(state.download_dir.to_string_lossy().into_owned())
}

fn magnet_for(info_hash: &str, name: Option<&str>, trackers: &[String]) -> String {
    let mut m = format!("magnet:?xt=urn:btih:{info_hash}");
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        m.push_str("&dn=");
        m.push_str(&urlencoding::encode(n));
    }
    for t in trackers {
        m.push_str("&tr=");
        m.push_str(&urlencoding::encode(t));
    }
    m
}

/// Detail panel "More": info hash, magnet link, tracker count, save path.
#[tauri::command]
pub fn get_torrent_details(state: St<'_>, key: String) -> Result<TorrentDetails, String> {
    let id = parse_key(&key)?;
    let k = id.as_string();
    if let Some(h) = state.session.get(TorrentIdOrHash::Hash(id)) {
        let mut trackers: Vec<String> = h.shared().trackers.iter().map(|u| u.to_string()).collect();
        trackers.sort();
        return Ok(TorrentDetails {
            magnet: magnet_for(&k, h.name().as_deref(), &trackers),
            info_hash: k,
            trackers: trackers.len(),
            save_path: Some(output_path(&h).to_string_lossy().into_owned()),
        });
    }
    let pending = state.pending.lock().unwrap();
    let p = pending.get(&k).ok_or_else(|| not_found(format!("no torrent {k}")))?;
    let trackers = Magnet::parse(&p.magnet).map(|m| m.trackers.len()).unwrap_or(0);
    Ok(TorrentDetails { info_hash: k, magnet: p.magnet.clone(), trackers, save_path: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_of("a/B.MKV"), (FileKind::Video, false));
        assert_eq!(kind_of("x.mp4"), (FileKind::Video, true));
        assert_eq!(kind_of("x.flac"), (FileKind::Audio, true));
        assert_eq!(kind_of("x.ogg"), (FileKind::Audio, false));
        assert_eq!(kind_of("x.srt"), (FileKind::Subtitle, false));
        assert_eq!(kind_of("README"), (FileKind::Other, false));
    }

    #[test]
    fn unique_names() {
        let taken = ["Sintel (2)", "Sintel (3)"];
        assert_eq!(unique_name("Sintel", |c| taken.contains(&c)), "Sintel (4)");
        assert_eq!(unique_name("Sintel", |_| false), "Sintel (2)");
        assert_eq!(unique_name("Big Buck Bunny (2008)", |_| false), "Big Buck Bunny (2008) (2)");
    }

    #[test]
    fn same_content_rules() {
        let d = std::env::temp_dir().join(format!("tide-engine-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let t = d.join("Movie");
        std::fs::create_dir_all(t.join("Subs")).unwrap();
        std::fs::write(t.join("Movie.mkv"), vec![0u8; 10]).unwrap();
        std::fs::write(t.join("Subs/en.srt"), b"1").unwrap();
        std::fs::write(t.join("tide.json"), br#"{"key":"other"}"#).unwrap();
        std::fs::write(t.join(".DS_Store"), b"").unwrap();
        let files = vec![(PathBuf::from("Movie.mkv"), 100), (PathBuf::from("Subs/en.srt"), 5)];
        assert!(same_content(&t, false, &files, "abc"), "torrent files + sidecars + hidden");
        std::fs::write(t.join("notes.doc"), b"x").unwrap();
        assert!(!same_content(&t, false, &files, "abc"), "foreign file");
        std::fs::write(t.join("tide.json"), br#"{"key":"abc"}"#).unwrap();
        assert!(same_content(&t, false, &files, "abc"), "our tide.json says it's this torrent");
        std::fs::remove_file(t.join("tide.json")).unwrap();
        std::fs::remove_file(t.join("notes.doc")).unwrap();
        assert!(!same_content(&t, false, &[(PathBuf::from("Movie.mkv"), 5), (PathBuf::from("Subs/en.srt"), 5)], "abc"), "bigger than expected");
        let f = d.join("single.mkv");
        std::fs::write(&f, vec![0u8; 10]).unwrap();
        assert!(same_content(&f, true, &[(PathBuf::from("single.mkv"), 10)], "abc"));
        assert!(!same_content(&f, true, &[(PathBuf::from("single.mkv"), 9)], "abc"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn magnet_link() {
        let m = magnet_for("08ada5a7a6183aae1e09d831df6748d566095a10", Some("Sintel & co"), &["udp://a.b:1/announce".into()]);
        assert_eq!(m, "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel%20%26%20co&tr=udp%3A%2F%2Fa.b%3A1%2Fannounce");
        let p = Magnet::parse(&m).unwrap();
        assert_eq!(p.name.as_deref(), Some("Sintel & co"));
        assert_eq!(p.trackers, vec!["udp://a.b:1/announce".to_string()]);
    }

    #[test]
    fn keys() {
        assert!(parse_key("08ada5a7a6183aae1e09d831df6748d566095a10").is_ok());
        assert!(parse_key("08ADA5A7A6183AAE1E09D831DF6748D566095A10").is_ok());
        assert!(parse_key("nope").unwrap_err().starts_with("INVALID_INPUT"));
    }

    #[test]
    fn extra_trackers() {
        let ih = "08ada5a7a6183aae1e09d831df6748d566095a10";
        let m = format!("magnet:?xt=urn:btih:{ih}&dn=Sintel&tr=udp%3A%2F%2Fexplodie.org%3A6969");
        let parsed = Magnet::parse(&m).unwrap();
        let out = with_extra_trackers(&m, &parsed.trackers);
        let p2 = Magnet::parse(&out).unwrap();
        assert_eq!(p2.as_id20().unwrap().as_string(), ih);
        assert_eq!(p2.name.as_deref(), Some("Sintel"));
        // explodie.org already present (without /announce -> different URL, kept).
        assert_eq!(p2.trackers.len(), 1 + EXTRA_TRACKERS.len());
        assert!(p2.trackers.iter().all(|t| t.starts_with("udp://")));

        // Exact duplicate is skipped.
        let m = format!("magnet:?xt=urn:btih:{ih}&tr=udp://tracker.opentrackr.org:1337/announce");
        let parsed = Magnet::parse(&m).unwrap();
        let out = with_extra_trackers(&m, &parsed.trackers);
        assert_eq!(Magnet::parse(&out).unwrap().trackers.len(), EXTRA_TRACKERS.len());

        // Bare info hash becomes a magnet URI.
        let out = with_extra_trackers(ih, &[]);
        let p3 = Magnet::parse(&out).unwrap();
        assert_eq!(p3.as_id20().unwrap().as_string(), ih);
        assert_eq!(p3.trackers.len(), EXTRA_TRACKERS.len());
    }

    /// Live network check (not run by default): adds the tracker-less Sintel magnet
    /// and waits until it resolves, has peers and downloads some bytes. `state::init`
    /// uses `$HOME`, so run the prebuilt test binary with an isolated HOME and
    /// `TIDE_LIVE_TEST=1`, e.g.
    /// `HOME=/tmp/x TIDE_LIVE_TEST=1 target/debug/deps/tide-<hash> --ignored live_sintel --nocapture`.
    #[test]
    #[ignore]
    fn live_sintel() {
        assert_eq!(
            std::env::var("TIDE_LIVE_TEST").as_deref(),
            Ok("1"),
            "set TIDE_LIVE_TEST=1 and an isolated HOME"
        );
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(
                "info,librqbit_upnp=debug,librqbit_tracker_comms=debug",
            ))
            .try_init();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let st = crate::state::init().await.unwrap();
            let r = add_magnet_internal(
                &st,
                "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel",
            )
            .await
            .unwrap();
            let t0 = std::time::Instant::now();
            let mut resolved_at = None;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let rows = list_rows(&st);
                let row = rows.iter().find(|x| x.key == r.key).unwrap();
                eprintln!(
                    "t={:>3}s state={:?} peers={} down={:.0}B/s progress={}/{}",
                    t0.elapsed().as_secs(), row.state, row.peers, row.down_bps,
                    row.progress_bytes, row.total_bytes
                );
                if resolved_at.is_none() && row.id.is_some() {
                    resolved_at = Some(t0.elapsed());
                    eprintln!("metadata resolved after {:?}", t0.elapsed());
                }
                // Hold ~20s so the background UPnP forwarder gets to report too.
                if row.peers > 0 && row.progress_bytes > 5_000_000 && t0.elapsed().as_secs() >= 20 {
                    break;
                }
                assert!(t0.elapsed() < Duration::from_secs(240), "no progress in 240s");
            }
        });
    }
}


#[cfg(test)]
mod restart_tests {
    use super::*;

    /// Restart check (not run by default): HOME must hold a copied session dir with
    /// finished torrents. Prints row states for 40 s while running the ticker's auto-pause.
    #[test]
    #[ignore]
    fn restart_finished() {
        assert_eq!(std::env::var("TIDE_LIVE_TEST").as_deref(), Ok("1"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .try_init();
        // Same shape as main.rs: init inside block_on, then work on spawned tasks.
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let st = std::sync::Arc::new(rt.block_on(crate::state::init()).unwrap());
        rt.block_on(restore_pending(&st));
        let h = rt.spawn(async move {
            let secs: u32 = std::env::var("TIDE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
            for t in 0..secs {
                pause_finished(&st).await;
                for r in list_rows(&st) {
                    let _ = crate::artwork::pick_artwork(&st, &r.key).await;
                }
                let rows = list_rows(&st);
                let s: Vec<String> = rows
                    .iter()
                    .map(|r| format!("{}={:?}/{:.2}", &r.key[..6], r.state, r.progress))
                    .collect();
                eprintln!("t={t:>2}s {}", s.join(" "));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        while !h.is_finished() {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod migrate_tests {
    use super::*;

    #[test]
    fn clean_names() {
        let c = clean_folder_name;
        assert_eq!(c("www.UIndex.org    -    The.Diplomat.US.S04E06.MULTI.1080p.WEB.X264-HiggsBoson"), "The.Diplomat.US.S04E06.MULTI.1080p.WEB.X264-HiggsBoson");
        assert_eq!(c("The.Diplomat.US.S04E07.1080p.WEB.h264-ETHEL[EZTVx.to]"), "The.Diplomat.US.S04E07.1080p.WEB.h264-ETHEL");
        assert_eq!(c("[ www.Torrenting.com ] - Some  Show  S01 "), "Some Show S01");
        assert_eq!(c("【高清影视之家发布 www.HDBTHD.com】Film.2020"), "Film.2020");
        assert_eq!(c("Show S01E01 [eztv]"), "Show S01E01");
        assert_eq!(c("  Tëst  Ğüşİöç: Part/2  "), "Tëst Ğüşİöç- Part-2");
        assert_eq!(c("..hidden"), "hidden");
        assert_eq!(c("Name (2)"), "Name (2)");
        assert_eq!(c("Big Buck Bunny (2008) [1080p]"), "Big Buck Bunny (2008) [1080p]");
        assert_eq!(c("Animals.2026.1080p.WEB-DL"), "Animals.2026.1080p.WEB-DL");
        assert_eq!(c("[www.site.org]"), "Download");
        let long = "ş".repeat(300);
        assert!(c(&long).len() <= 200);
        for s in ["www.UIndex.org    -    X [EZTVx.to]", "  a  b  ", "Tëst [EZTVx.to]"] {
            assert_eq!(c(&c(s)), c(s), "idempotent");
        }
    }

    /// Isolated end-to-end check (not run by default). HOME must be a scratch dir.
    /// TIDE_MIG_PHASE=setup adds TIDE_MIG_TORRENTS ("file.torrent::folder under
    /// ~/Downloads::live|paused", ';' separated); =migrate runs the startup migration and
    /// checks the layout; =remove also removes everything with files.
    #[test]
    #[ignore]
    fn migrate_isolated() {
        assert_eq!(std::env::var("TIDE_LIVE_TEST").as_deref(), Ok("1"));
        let home = std::env::var("HOME").unwrap();
        assert!(home.contains("scratchpad"), "HOME must be isolated");
        let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new("info")).try_init();
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let st = std::sync::Arc::new(rt.block_on(crate::state::init()).unwrap());
        let dl = Path::new(&home).join("Downloads");
        let phase = std::env::var("TIDE_MIG_PHASE").unwrap();
        rt.block_on(async {
            if phase == "setup" {
                for t in std::env::var("TIDE_MIG_TORRENTS").unwrap().split(';') {
                    let mut it = t.split("::");
                    let (t, folder, mode) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
                    let bytes = std::fs::read(t).unwrap();
                    let r = st
                        .session
                        .add_torrent(
                            AddTorrent::from_bytes(bytes),
                            Some(AddTorrentOptions {
                                overwrite: true,
                                paused: mode == "paused",
                                output_folder: Some(dl.join(folder).to_string_lossy().into_owned()),
                                ..Default::default()
                            }),
                        )
                        .await
                        .unwrap();
                    let h = r.into_handle().unwrap();
                    wait_initialized(&h).await.unwrap();
                    let s = h.stats();
                    eprintln!("added {} at {:?} finished={} have={}", h.info_hash().as_string(), h.output_folder(), s.finished, s.progress_bytes);
                }
                return;
            }
            let before: HashMap<String, u64> = list_rows(&st)
                .into_iter()
                .map(|r| (r.key.clone(), handle(&st, &r.key).unwrap().stats().progress_bytes))
                .collect();
            migrate_to_tidebox(&st).await;
            let tb = st.download_dir.clone();
            for r in list_rows(&st) {
                let h = handle(&st, &r.key).unwrap();
                wait_initialized(&h).await.unwrap();
                let out = PathBuf::from(r.output_path.clone().unwrap());
                let s = h.stats();
                eprintln!("row {} {:?} paused={} finished={} have={} folder={:?} out={:?}", &r.key[..8], r.state, h.is_paused(), s.finished, s.progress_bytes, h.output_folder(), out);
                assert_eq!(h.output_folder().parent(), Some(tb.as_path()));
                assert!(out.exists(), "{out:?}");
                assert_eq!(s.progress_bytes, before[&r.key], "progress kept");
                assert_eq!(h.is_paused(), s.finished, "finished ones paused, live one live");
                for f in files(&st, &r.key).unwrap() {
                    let p = file_abs_path(&st, &r.key, f.index).unwrap();
                    assert!(p.starts_with(h.output_folder()));
                    let url = format!("asset://localhost/{}", urlencoding::encode(&p.to_string_lossy()));
                    let back = urlencoding::decode(url.strip_prefix("asset://localhost/").unwrap()).unwrap();
                    assert_eq!(Path::new(back.as_ref()), p);
                }
                let (side, folder) = crate::metadata::sidecar_paths(&st, &r.key);
                eprintln!("  sidecars {:?} folder={:?}", side.iter().map(|p| p.file_name().unwrap().to_owned()).collect::<Vec<_>>(), folder);
                assert!(side.iter().all(|s| s.parent() == Some(h.output_folder())));
                assert_eq!(folder.as_deref(), Some(h.output_folder()), "folder-style sidecars");
                let art = crate::artwork::pick_artwork(&st, &r.key).await.unwrap();
                eprintln!("  artwork found={}", art.poster.is_some());
            }
            let left: Vec<_> = std::fs::read_dir(&dl).unwrap().flatten().map(|e| e.file_name()).filter(|n| n != "Tidebox" && n != ".DS_Store").collect();
            assert!(left.is_empty(), "left behind in Downloads: {left:?}");
            let mut tops: Vec<_> = std::fs::read_dir(&tb).unwrap().flatten().map(|e| (e.file_name(), e.path().is_dir())).collect();
            tops.sort();
            eprintln!("Tidebox: {tops:#?}");
            // The only loose file allowed is the foreign one the setup planted.
            assert!(tops.iter().filter(|(_, d)| !*d).count() <= 1, "no loose torrent files in Tidebox");
            if phase == "remove" {
                for r in list_rows(&st) {
                    let out = PathBuf::from(r.output_path.clone().unwrap());
                    remove(&st, &r.key, true).await.unwrap();
                    assert!(!out.exists(), "{out:?}");
                }
                let left: Vec<_> = std::fs::read_dir(&tb).unwrap().flatten().map(|e| e.file_name()).collect();
                eprintln!("Tidebox after remove: {left:?}");
            }
        });
    }
}
