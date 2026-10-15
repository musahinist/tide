//! Play / VLC / subtitle commands (WP3).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

use crate::engine;
use crate::state::AppState;
use crate::subtitles;
use crate::types::{FileKind, FileRow, MediaRef, PlayMode, PlayResult, SubtitleTrack};

type St<'a> = State<'a, Arc<AppState>>;

/// Subtitle files are tiny; cap reads at 2 MB.
const SUB_MAX_BYTES: usize = 2 * 1024 * 1024;
const SUB_TIMEOUT_S: u64 = 60;
/// VLC launch must not wait long for an incomplete subtitle; it starts without one.
const VLC_SUB_TIMEOUT_S: u64 = 5;
/// How many top-ranked subtitle tracks are prefetched for the builtin player.
const PREFETCH_SUBS: usize = 4;

/// WP3 may add fields, but only fields with Default.
#[derive(Default)]
pub struct PlayerState {
    /// What the `player` window is (or should be) showing.
    pub current: Mutex<Option<PlayResult>>,
    /// Converted WebVTT text by track id ("t:<key>:<idx>").
    pub vtt_cache: Mutex<HashMap<String, Arc<String>>>,
}

fn internal<E: std::fmt::Display>(e: E) -> String {
    format!("INTERNAL: {e}")
}

fn valid_key(key: &str) -> Result<(), String> {
    if key.len() == 40 && key.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("INVALID_INPUT: bad torrent key".into())
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn find_file(st: &AppState, key: &str, idx: usize) -> Result<(Vec<FileRow>, FileRow), String> {
    let files = engine::files(st, key)?;
    let row = files
        .iter()
        .find(|f| f.index == idx)
        .cloned()
        .ok_or_else(|| format!("NOT_FOUND: file {idx} not in torrent"))?;
    Ok((files, row))
}

/// Ranked subtitle tracks for a video file (empty for audio).
fn subtitle_tracks(key: &str, files: &[FileRow], row: &FileRow) -> (Vec<SubtitleTrack>, Option<String>) {
    if row.kind != FileKind::Video {
        return (Vec::new(), None);
    }
    let videos: Vec<&str> = files.iter().filter(|f| f.kind == FileKind::Video).map(|f| f.name.as_str()).collect();
    let subs: Vec<(usize, &str)> = files
        .iter()
        .filter(|f| f.kind == FileKind::Subtitle)
        .map(|f| (f.index, f.name.as_str()))
        .collect();
    subtitles::build_tracks(key, &row.name, &videos, &subs)
}

/// Raw bytes of a torrent file: from disk when complete, else through the stream.
async fn fetch_file_bytes(st: &AppState, key: &str, row: &FileRow, timeout_s: u64) -> Result<Vec<u8>, String> {
    if row.progress >= 1.0 {
        if let Ok(p) = engine::file_abs_path(st, key, row.index) {
            if let Ok(mut b) = tokio::fs::read(&p).await {
                b.truncate(SUB_MAX_BYTES);
                return Ok(b);
            }
        }
    }
    // Picking a subtitle must not resume a torrent the user paused.
    engine::read_file(st, key, row.index, SUB_MAX_BYTES, timeout_s, false).await
}

/// Where to play a file from.
enum Source {
    /// Complete on disk: played without touching the torrent (completed torrents stay
    /// stopped; Tide never seeds).
    Disk(PathBuf),
    /// librqbit stream URL (torrent made live and initialized first).
    Stream(String),
}

async fn media_source(st: &AppState, key: &str, row: &FileRow) -> Result<Source, String> {
    if row.progress >= 1.0 {
        if let Ok(p) = engine::file_abs_path(st, key, row.index) {
            if p.is_file() {
                return Ok(Source::Disk(p));
            }
        }
    }
    engine::ensure_live(st, key).await?;
    Ok(Source::Stream(engine::stream_url(st, key, row.index)?))
}

/// Builtin player URL for a file on disk (Tauri asset protocol; the file is added to
/// the asset scope here, nothing else is reachable).
fn asset_url(app: &AppHandle, path: &Path) -> Result<String, String> {
    app.asset_protocol_scope().allow_file(path).map_err(internal)?;
    Ok(format!("asset://localhost/{}", urlencoding::encode(&path.to_string_lossy())))
}

async fn vtt_for(st: &AppState, sub_id: &str) -> Result<Arc<String>, String> {
    if let Some(v) = st.player.vtt_cache.lock().map_err(internal)?.get(sub_id) {
        return Ok(v.clone());
    }
    let (key, idx) = subtitles::parse_track_id(sub_id).ok_or("INVALID_INPUT: bad subtitle id")?;
    let (_, row) = find_file(st, &key, idx)?;
    let format = subtitles::format_from_name(&row.name).ok_or("INVALID_INPUT: not a subtitle file")?;
    let bytes = fetch_file_bytes(st, &key, &row, SUB_TIMEOUT_S).await?;
    let vtt = Arc::new(subtitles::to_vtt(&bytes, format));
    st.player.vtt_cache.lock().map_err(internal)?.insert(sub_id.to_string(), vtt.clone());
    Ok(vtt)
}

/// Write a subtitle to `data_dir/subs/<key>_<idx>.<ext>` for VLC. Waits at most
/// `VLC_SUB_TIMEOUT_S` for an incomplete file.
async fn subtitle_to_disk(st: &AppState, sub_id: &str) -> Result<PathBuf, String> {
    let (key, idx) = subtitles::parse_track_id(sub_id).ok_or("INVALID_INPUT: bad subtitle id")?;
    let (_, row) = find_file(st, &key, idx)?;
    let ext = row.name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_else(|| "srt".into());
    let bytes = fetch_file_bytes(st, &key, &row, VLC_SUB_TIMEOUT_S).await?;
    let dir = st.data_dir.join("subs");
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    let path = dir.join(format!("{key}_{idx}.{ext}"));
    tokio::fs::write(&path, bytes).await.map_err(internal)?;
    Ok(path)
}

// ------------------------------------------------------------------ VLC

fn vlc_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/Applications/VLC.app/Contents/MacOS/VLC")];
    if let Some(h) = home {
        v.push(h.join("Applications/VLC.app/Contents/MacOS/VLC"));
    }
    v.push("/usr/local/bin/vlc".into());
    v.push("/opt/homebrew/bin/vlc".into());
    v
}

pub fn find_vlc() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    vlc_candidates(home.as_deref()).into_iter().find(|p| p.is_file())
}

fn spawn_vlc(vlc: &Path, title: &str, url: &str, sub: Option<&Path>) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(vlc);
    cmd.arg("--play-and-exit").arg("--quiet").arg(format!("--meta-title={title}"));
    if let Some(s) = sub {
        cmd.arg(format!("--sub-file={}", s.display()));
    }
    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    #[cfg(unix)]
    cmd.process_group(0); // detach from our process group
    let mut child = cmd.spawn().map_err(|e| format!("INTERNAL: failed to start VLC: {e}"))?;
    // Reap it so it does not linger as a zombie.
    tauri::async_runtime::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// Resolve stream URL/title and launch VLC, optionally with a subtitle track.
async fn launch_vlc(st: &AppState, key: &str, idx: usize, sub_id: Option<&str>) -> Result<PlayResult, String> {
    let vlc = find_vlc().ok_or("VLC_NOT_FOUND: VLC is not installed")?;
    let (files, row) = find_file(st, key, idx)?;
    let url = match media_source(st, key, &row).await? {
        Source::Disk(p) => p.to_string_lossy().into_owned(),
        Source::Stream(u) => u,
    };
    let title = basename(&row.name).to_string();
    let (subtitles, default_sub) = subtitle_tracks(key, &files, &row);
    let sub_path = match sub_id {
        Some(id) => match subtitle_to_disk(st, id).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!("subtitle for VLC failed: {e}");
                None
            }
        },
        None => None,
    };
    spawn_vlc(&vlc, &title, &url, sub_path.as_deref())?;
    Ok(PlayResult {
        mode: PlayMode::Vlc,
        title,
        media: MediaRef::Torrent { key: key.to_string(), file_index: idx },
        stream_url: url,
        subtitles,
        default_subtitle_id: sub_id.map(str::to_string).or(default_sub),
    })
}

// ------------------------------------------------------------------ commands

/// Creates/focuses the player window, so it must be async.
#[tauri::command]
pub async fn play_media(app: AppHandle, state: St<'_>, media: MediaRef) -> Result<PlayResult, String> {
    let st: Arc<AppState> = state.inner().clone();
    let MediaRef::Torrent { key, file_index } = media;
    let key = key.to_ascii_lowercase();
    valid_key(&key)?;

    let (files, row) = find_file(&st, &key, file_index)?;
    if !matches!(row.kind, FileKind::Video | FileKind::Audio) {
        return Err("INVALID_INPUT: not a media file".into());
    }

    if !row.playable_builtin {
        let (_, default_sub) = subtitle_tracks(&key, &files, &row);
        return launch_vlc(&st, &key, file_index, default_sub.as_deref()).await;
    }

    let stream_url = match media_source(&st, &key, &row).await? {
        Source::Disk(p) => asset_url(&app, &p)?,
        Source::Stream(u) => u,
    };
    let (subtitles, default_subtitle_id) = subtitle_tracks(&key, &files, &row);
    let result = PlayResult {
        mode: PlayMode::Builtin,
        title: basename(&row.name).to_string(),
        media: MediaRef::Torrent { key: key.clone(), file_index },
        stream_url,
        subtitles,
        default_subtitle_id,
    };

    // Prefetch the top tracks in the background (pieces get prioritised).
    for t in result.subtitles.iter().take(PREFETCH_SUBS) {
        let st = st.clone();
        let id = t.id.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = vtt_for(&st, &id).await {
                tracing::debug!("subtitle prefetch {id} failed: {e}");
            }
        });
    }

    *st.player.current.lock().map_err(internal)? = Some(result.clone());

    if let Some(w) = app.get_webview_window("player") {
        let _ = w.set_title(&result.title);
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        app.emit_to("player", "player-load", &result).map_err(internal)?;
    } else {
        // The new window calls `player_current` on load.
        WebviewWindowBuilder::new(&app, "player", WebviewUrl::App("player.html".into()))
            .title(&result.title)
            .inner_size(960., 540.)
            .min_inner_size(320., 180.)
            .background_color(tauri::window::Color(0, 0, 0, 255))
            .theme(Some(tauri::Theme::Dark))
            .build()
            .map_err(internal)?;
    }
    Ok(result)
}

#[tauri::command]
pub fn player_current(state: St<'_>) -> Result<Option<PlayResult>, String> {
    Ok(state.player.current.lock().map_err(internal)?.clone())
}

/// JS: `{ subId }`.
#[tauri::command]
pub async fn get_subtitle_vtt(state: St<'_>, sub_id: String) -> Result<String, String> {
    let st: Arc<AppState> = state.inner().clone();
    Ok(vtt_for(&st, &sub_id).await?.as_str().to_string())
}

/// JS: `{ media, subId }`. Launches VLC and closes the player window.
#[tauri::command]
pub async fn open_in_vlc(
    app: AppHandle,
    state: St<'_>,
    media: MediaRef,
    sub_id: Option<String>,
) -> Result<(), String> {
    let st: Arc<AppState> = state.inner().clone();
    let MediaRef::Torrent { key, file_index } = media;
    let key = key.to_ascii_lowercase();
    valid_key(&key)?;
    launch_vlc(&st, &key, file_index, sub_id.as_deref()).await?;
    if let Some(w) = app.get_webview_window("player") {
        let _ = w.close();
    }
    Ok(())
}

fn valid_external_url(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() > "https://".len()
        && url.len() < 2048
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// https URLs only.
#[tauri::command]
pub async fn open_external(url: String) -> Result<(), String> {
    if !valid_external_url(&url) {
        return Err("INVALID_INPUT: only https URLs can be opened".into());
    }
    let status = tokio::process::Command::new("/usr/bin/open")
        .arg(&url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(internal)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("INTERNAL: open exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlc_candidate_order() {
        let c = vlc_candidates(Some(Path::new("/Users/x")));
        assert_eq!(c[0], PathBuf::from("/Applications/VLC.app/Contents/MacOS/VLC"));
        assert_eq!(c[1], PathBuf::from("/Users/x/Applications/VLC.app/Contents/MacOS/VLC"));
        assert_eq!(c[2], PathBuf::from("/usr/local/bin/vlc"));
        assert_eq!(c[3], PathBuf::from("/opt/homebrew/bin/vlc"));
    }

    #[test]
    fn finds_installed_vlc() {
        // This machine has VLC in /Applications.
        if Path::new("/Applications/VLC.app/Contents/MacOS/VLC").is_file() {
            assert_eq!(find_vlc(), Some(PathBuf::from("/Applications/VLC.app/Contents/MacOS/VLC")));
        }
    }

    #[test]
    fn external_url_rules() {
        assert!(valid_external_url("https://www.videolan.org/vlc/"));
        assert!(!valid_external_url("http://www.videolan.org/"));
        assert!(!valid_external_url("file:///etc/passwd"));
        assert!(!valid_external_url("https://"));
        assert!(!valid_external_url("https://a b"));
        assert!(!valid_external_url("-a https://x"));
    }
}
