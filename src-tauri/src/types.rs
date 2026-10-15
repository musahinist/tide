//! IPC contract (frozen). Returned JSON is snake_case; JS arg keys are camelCase
//! (Tauri converts command parameter names automatically).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TorrentState {
    Resolving,
    Initializing,
    Downloading,
    Seeding,
    Paused,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentRow {
    /// 40-char lowercase hex info hash.
    pub key: String,
    /// librqbit session id; null while resolving.
    pub id: Option<usize>,
    pub name: String,
    pub state: TorrentState,
    pub error: Option<String>,
    pub total_bytes: u64,
    pub progress_bytes: u64,
    /// 0..1
    pub progress: f64,
    pub uploaded_bytes: u64,
    pub down_bps: f64,
    pub up_bps: f64,
    pub peers: u32,
    pub eta_secs: Option<u64>,
    pub output_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRow {
    pub index: usize,
    /// Path components joined with "/".
    pub name: String,
    pub size: u64,
    pub progress_bytes: u64,
    /// 0..1
    pub progress: f64,
    pub kind: FileKind,
    pub playable_builtin: bool,
}

/// `{ kind: "torrent", key, fileIndex }`. v2 adds `{ kind: "yt", id }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MediaRef {
    Torrent {
        key: String,
        #[serde(rename = "fileIndex")]
        file_index: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubtitleFormat {
    Srt,
    Vtt,
    Ass,
    Ssa,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleTrack {
    /// Opaque, e.g. "t:<key>:<idx>".
    pub id: String,
    pub label: String,
    pub lang: Option<String>,
    pub format: SubtitleFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayMode {
    Builtin,
    Vlc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayResult {
    pub mode: PlayMode,
    pub title: String,
    pub media: MediaRef,
    pub stream_url: String,
    pub subtitles: Vec<SubtitleTrack>,
    pub default_subtitle_id: Option<String>,
}

/// Return value of `add_magnet`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMagnetResult {
    pub key: String,
    pub name: String,
    pub already_exists: bool,
}
