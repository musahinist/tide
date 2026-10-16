//! IPC contract (frozen). Returned JSON is snake_case; JS arg keys are camelCase
//! (Tauri converts command parameter names automatically).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TorrentState {
    Resolving,
    Initializing,
    Downloading,
    /// Finished downloading; kept paused (Tide never seeds). Resume is refused.
    Completed,
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
    /// v2: a finished yt-dlp download ("yt-<n>").
    Yt { id: String },
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
    /// IMDb vote count (from IMDb's title.ratings dataset; absent in older sidecars).
    #[serde(default)]
    pub imdb_votes: Option<u32>,
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

/// A user-visible episode group (`data_dir/groups.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    /// Torrent keys (info hashes).
    #[serde(default)]
    pub members: Vec<String>,
    /// Set when the group was materialised from an automatic group key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_key: Option<String>,
}

/// Contents of `data_dir/groups.json`; also the return value of `list_groups`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupsFile {
    #[serde(default)]
    pub groups: Vec<Group>,
    /// Keys the user removed from a group: never auto-grouped again.
    #[serde(default)]
    pub excluded: std::collections::BTreeMap<String, bool>,
}

// ---------------------------------------------------------------- yt-dlp (v2)

/// Return value of `yt_probe`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YtProbe {
    pub url: String,
    pub title: String,
    pub uploader: Option<String>,
    pub duration_secs: Option<f64>,
    pub thumbnail: Option<String>,
    pub is_live: bool,
    pub max_height: Option<u32>,
}

/// Download presets (never above 720p).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum YtPreset {
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "480p")]
    P480,
    #[serde(rename = "360p")]
    P360,
    #[serde(rename = "audio_mp3")]
    AudioMp3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum YtJobState {
    Queued,
    Downloading,
    Postprocessing,
    Done,
    Error,
    Cancelled,
}

/// A yt-dlp download (`yt_list`, event `yt-jobs-updated`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct YtJob {
    /// "yt-<n>"
    pub id: String,
    pub url: String,
    pub title: String,
    pub thumbnail: Option<String>,
    pub preset: YtPreset,
    pub state: YtJobState,
    /// 0..1
    pub progress: f64,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: Option<f64>,
    pub eta_secs: Option<u64>,
    pub file_path: Option<String>,
    pub error: Option<String>,
}

/// Return value of `parse_names` (offline name parse; used for instant grouping).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParsedName {
    pub title: String,
    /// `nameparse::norm_title(title)`: the grouping key.
    pub norm: String,
    pub year: Option<u16>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub is_tv: bool,
}

/// One episode file inside a torrent (`list_episodes`; season packs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EpisodeFile {
    pub file_index: usize,
    pub season: Option<u32>,
    pub episode: u32,
    /// Show title parsed from the file (or torrent) name.
    pub title_hint: String,
    /// `nameparse::norm_title(title_hint)`.
    pub norm: String,
    pub size: u64,
    /// 0..1
    pub progress: f64,
}

// ---------------------------------------------------------------- new-episode watcher

/// Watcher settings (`data_dir/watcher.json` `settings`). Search goes only through a
/// user-configured Torznab endpoint (Prowlarr / Jackett); empty URL = no search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatcherSettings {
    pub torznab_url: String,
    pub api_key: String,
    pub auto_add: bool,
    /// "1080p" | "720p" | "any"
    pub quality: String,
}

impl Default for WatcherSettings {
    fn default() -> Self {
        WatcherSettings { torznab_url: String::new(), api_key: String::new(), auto_add: false, quality: "1080p".into() }
    }
}

/// Return value of `watcher_get_settings` / `watcher_set_settings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatcherInfo {
    #[serde(flatten)]
    pub settings: WatcherSettings,
    /// Series (IMDb ids) the user stopped following.
    pub unfollowed: Vec<String>,
    /// Unix seconds of the last completed check (0 = never).
    pub last_check: u64,
}

/// One Torznab search result for a new episode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TorznabResult {
    pub title: String,
    pub size: u64,
    pub seeders: u32,
    /// Always set (results without a magnet / info hash are dropped).
    pub magnet: Option<String>,
    /// "2160p" | "1080p" | "720p" | "480p" | null
    pub quality: Option<String>,
}

/// An aired episode newer than the newest one in Tide (`watcher_check`, event `new-episodes`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEpisode {
    pub imdb_id: String,
    pub show: String,
    pub season: u32,
    pub episode: u32,
    pub episode_title: Option<String>,
    /// Cinemeta `released` (ISO 8601).
    pub released: Option<String>,
    /// Best first (preferred quality, then seeders); at most 5. Empty without Torznab.
    #[serde(default)]
    pub results: Vec<TorznabResult>,
}
