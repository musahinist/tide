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

// ---------------------------------------------------------------- metadata (WP6)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    #[default]
    Movie,
    Tv,
}

/// Movie / TV info for a torrent (source: Cinemeta, keyed by IMDb id). Persisted in the
/// `tide.json` sidecar, so every field has a serde default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaInfo {
    /// IMDb id ("tt0133093"); empty when `needs_match`.
    pub imdb_id: String,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<u16>,
    /// As shown by the source, e.g. "1999" or "2008–2013".
    pub release_info: Option<String>,
    pub overview: String,
    /// Remote poster (search tiles only; matched rows use the sidecar via `get_media_image`).
    pub poster_url: Option<String>,
    pub genres: Vec<String>,
    pub runtime_min: Option<u32>,
    /// https://www.imdb.com/title/{imdb_id}/
    pub imdb_url: Option<String>,
    pub imdb_rating: Option<f32>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub episode_title: Option<String>,
    pub episode_overview: Option<String>,
    /// Daily shows: YYYY-MM-DD parsed from the name.
    pub air_date: Option<String>,
    /// The user picked this match (never auto-overwritten).
    pub manual: bool,
    /// No confident automatic match: `title`/`year`/`season`/`episode` hold the parsed
    /// name (search prefill) and the UI offers "Eşleştir".
    pub needs_match: bool,
    /// Why it was (not) auto-matched, e.g. "similarity 0.72; year 2010 vs 2014".
    pub match_note: Option<String>,
}

/// Return value of `get_torrent_details` (detail panel "More" section).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentDetails {
    pub info_hash: String,
    pub magnet: String,
    pub trackers: usize,
    pub save_path: Option<String>,
}
