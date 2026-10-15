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
    TorrentStatsState,
};
use tauri::State;
use tokio::io::AsyncReadExt;

use crate::state::{AppState, PendingMagnet};
use crate::types::{AddMagnetResult, FileKind, FileRow, TorrentRow, TorrentState};

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
fn handle(st: &AppState, key: &str) -> Result<Handle, String> {
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
        TorrentStatsState::Live if finished => TorrentState::Seeding,
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

/// Unpause if paused (also retries a torrent in error state).
pub async fn ensure_live(st: &AppState, key: &str) -> Result<(), String> {
    let h = handle(st, key)?;
    // Don't call h.is_paused() inside with_state: nested read lock.
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
    Ok(())
}

/// api_stream + read_to_end (prioritizes pieces). Unpauses the torrent if the file is
/// not complete yet (otherwise the read would never finish).
pub async fn read_file(
    st: &AppState,
    key: &str,
    idx: usize,
    max_bytes: usize,
    timeout_s: u64,
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
        ensure_live(st, key).await?;
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
    let task = tokio::spawn(async move {
        let opts = AddTorrentOptions {
            overwrite: true,
            ..Default::default()
        };
        let res = session
            .add_torrent(AddTorrent::from_url(magnet), Some(opts))
            .await;
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

async fn pause(state: &AppState, key: &str) -> Result<(), String> {
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

async fn remove(state: &AppState, key: &str, delete_files: bool) -> Result<(), String> {
    let id = parse_key(key)?;
    let k = id.as_string();
    let was_pending = {
        let mut pending = state.pending.lock().unwrap();
        match pending.remove(&k) {
            Some(p) => {
                if let Some(t) = p.task {
                    t.abort();
                }
                save_pending(state, &pending);
                true
            }
            None => false,
        }
    };
    RESOLVE_ERRORS.lock().unwrap().remove(&k);

    // Also covers a resolve task that finished right before the abort.
    if state.session.get(TorrentIdOrHash::Hash(id)).is_some() {
        state
            .session
            .delete(TorrentIdOrHash::Hash(id), delete_files)
            .await
            .map_err(|e| internal(format!("remove failed: {e:#}")))?;
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

#[tauri::command]
pub fn get_download_dir(state: St<'_>) -> Result<String, String> {
    Ok(state.download_dir.to_string_lossy().into_owned())
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
    fn keys() {
        assert!(parse_key("08ada5a7a6183aae1e09d831df6748d566095a10").is_ok());
        assert!(parse_key("08ADA5A7A6183AAE1E09D831DF6748D566095A10").is_ok());
        assert!(parse_key("nope").unwrap_err().starts_with("INVALID_INPUT"));
    }
}

