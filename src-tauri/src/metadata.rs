//! Movie / TV metadata (WP6). Source: Cinemeta (Stremio's public, keyless metadata API,
//! keyed by IMDb id; images from metahub.space). English only, no API key, no settings.
//!
//! Each torrent is looked up ONCE; the result is written next to the content as
//! Kodi/Plex/Infuse-style sidecars (`tide.json`, `poster.jpg`, `fanart.jpg`,
//! `movie.nfo`/`tvshow.nfo`; single-file torrents use `<stem>.tide.json`,
//! `<stem>-poster.jpg`, ...). From then on only the sidecars are read; app start never
//! re-queries. "No match" is cached for 24 h in `data_dir/metadata.json`, as is
//! "needs_match" (results exist, none confident enough to auto-accept).
//!
//! Commands (JS args camelCase, JSON snake_case, errors "CODE: msg"):
//! - `get_media_info {key}` -> `MediaInfo | null`
//! - `get_media_image {key, kind: "poster"|"backdrop"}` -> data URL
//! - `search_media {query, mediaType: "movie"|"tv"|null}` -> `MediaInfo[]`
//! - `set_media_match {key, imdbId, mediaType}` -> `MediaInfo` (manual, rewrites sidecars)
//! - `clear_media_match {key}` -> null

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::State;

use crate::engine;
use crate::nameparse::{self, Confidence, Parsed};
use crate::state::AppState;
use crate::types::{EpisodeFile, FileKind, FileRow, MediaInfo, MediaType, ParsedName, TorrentState};

type St<'a> = State<'a, Arc<AppState>>;

pub const CINEMETA: &str = "https://v3-cinemeta.strem.io";
const METAHUB: &str = "https://images.metahub.space";
const IMDB_RATINGS: &str = "https://datasets.imdbws.com/title.ratings.tsv.gz";
/// Re-download the IMDb ratings dataset at most this often.
const RATINGS_MAX_AGE: u64 = 7 * 24 * 3600;
/// After a failed dataset download, don't queue new lookups for this long.
const RATINGS_RETRY: u64 = 600;
const USER_AGENT: &str = "Tide/0.1 (+https://github.com/musahinist/tide)";
const MISS_TTL: u64 = 24 * 3600;
const MIN_VIDEO_BYTES: u64 = 50_000_000;
const MAX_TEXT_BYTES: u64 = 256 * 1024;
const MAX_SEARCHES: usize = 8;
const LRU_CAP: usize = 16;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

fn internal(msg: impl std::fmt::Display) -> String {
    format!("INTERNAL: {msg}")
}
fn not_ready(msg: impl std::fmt::Display) -> String {
    format!("NOT_READY: {msg}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- state

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NeedsRec {
    at: u64,
    info: MediaInfo,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexFile {
    #[serde(default)]
    misses: HashMap<String, u64>,
    #[serde(default)]
    needs: HashMap<String, NeedsRec>,
    /// Failed sidecar-image self-heals: key -> time (retried after MISS_TTL).
    #[serde(default)]
    img_fail: HashMap<String, u64>,
}

#[derive(Default)]
struct Inner {
    loaded: bool,
    index: IndexFile,
    infos: HashMap<String, MediaInfo>,
}

pub struct MetaState {
    inner: Mutex<Inner>,
    /// Be polite to Cinemeta: at most 2 lookups (hence requests) at a time.
    gate: tokio::sync::Semaphore,
    images: Mutex<VecDeque<(String, Arc<String>)>>,
    http: OnceLock<reqwest::Client>,
    /// Background IMDb-dataset rating fill-ins (Cinemeta lacks ratings for new titles).
    ratings: Arc<RatingQueue>,
    /// Keys whose missing poster/fanart were re-fetched this run (at most once each).
    healed: Mutex<HashSet<String>>,
}

impl Default for MetaState {
    fn default() -> Self {
        MetaState {
            inner: Mutex::new(Inner::default()),
            gate: tokio::sync::Semaphore::new(2),
            images: Mutex::new(VecDeque::new()),
            http: OnceLock::new(),
            ratings: Arc::default(),
            healed: Mutex::default(),
        }
    }
}

/// metahub answers new titles with 307 -> live.metahub.space: follow up to 5 hops,
/// https only (plain http only to loopback, for the tests' mock server).
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|a| {
        let u = a.url();
        let ok_scheme = u.scheme() == "https"
            || (u.scheme() == "http" && matches!(u.host_str(), Some("127.0.0.1" | "localhost")));
        if a.previous().len() > 5 {
            a.error("too many redirects")
        } else if !ok_scheme {
            a.error("redirect to non-https URL")
        } else {
            a.follow()
        }
    })
}

fn index_path(dd: &Path) -> PathBuf {
    dd.join("metadata.json")
}

impl MetaState {
    fn http(&self) -> reqwest::Client {
        self.http
            .get_or_init(|| {
                reqwest::Client::builder()
                    .connect_timeout(REQUEST_TIMEOUT)
                    .user_agent(USER_AGENT)
                    .redirect(redirect_policy())
                    .build()
                    .expect("reqwest client")
            })
            .clone()
    }

    /// In-memory info, with a rating the background worker found since it was cached.
    fn cached(&self, dd: &Path, key: &str) -> Option<MediaInfo> {
        let mut i = self.with(dd, |g| g.infos.get(key).cloned())?;
        if i.imdb_rating.is_none() {
            let hit = self.ratings.q.lock().unwrap().done.get(key).copied();
            if let Some((r, v)) = hit {
                (i.imdb_rating, i.imdb_votes) = (Some(r), Some(v));
                self.with(dd, |g| g.infos.insert(key.to_string(), i.clone()));
            }
        }
        Some(i)
    }

    /// Queue an IMDb-dataset rating lookup for a matched title without one. Off the
    /// request path: the worker patches the sidecars and the next poll picks it up.
    fn want_rating(&self, dd: &Path, cm: &Cinemeta, key: &str, info: &MediaInfo, loc: &Loc) {
        if info.imdb_rating.is_some() || info.needs_match || !valid_tt(&info.imdb_id) {
            return;
        }
        let rq = self.ratings.clone();
        let start = {
            let mut g = rq.q.lock().unwrap();
            if now_secs() < g.retry_after || !g.tried.insert(key.to_string()) {
                return;
            }
            g.pending.insert(key.to_string(), (info.imdb_id.clone(), loc.clone()));
            !std::mem::replace(&mut g.running, true)
        };
        if start {
            tokio::spawn(rating_worker(rq, dd.to_path_buf(), cm.http.clone(), cm.ratings_url.clone()));
        }
    }

    fn with<R>(&self, dd: &Path, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut g = self.inner.lock().unwrap();
        if !g.loaded {
            g.loaded = true;
            if let Ok(b) = std::fs::read(index_path(dd)) {
                match serde_json::from_slice(&b) {
                    Ok(ix) => g.index = ix,
                    Err(e) => tracing::warn!("ignoring bad metadata.json: {e}"),
                }
            }
        }
        f(&mut g)
    }

    /// Mutate the index and persist it (atomically).
    fn with_index(&self, dd: &Path, f: impl FnOnce(&mut IndexFile)) {
        let json = self.with(dd, |g| {
            f(&mut g.index);
            serde_json::to_vec_pretty(&g.index).unwrap_or_default()
        });
        let p = index_path(dd);
        if let Err(e) = write_atomic(&p, &json) {
            tracing::warn!("saving {} failed: {e}", p.display());
        }
    }

    fn drop_images(&self, key: &str) {
        self.images.lock().unwrap().retain(|(k, _)| !k.starts_with(&format!("{key}:")));
    }
}

// ---------------------------------------------------------------- Cinemeta client

#[derive(Debug)]
pub enum NetErr {
    NotFound,
    Net(String),
}

impl NetErr {
    fn to_ipc(&self) -> String {
        match self {
            NetErr::NotFound => "NOT_FOUND: no such title".into(),
            NetErr::Net(m) => internal(m),
        }
    }
}

pub struct Cinemeta {
    pub base: String,
    /// metahub image base (poster fallback when a meta has none).
    pub images: String,
    /// IMDb's title.ratings.tsv.gz (rating fallback).
    pub ratings_url: String,
    pub http: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cand {
    pub id: String,
    pub kind: MediaType,
    pub name: String,
    pub year: Option<u16>,
    pub release_info: Option<String>,
    pub poster: Option<String>,
}

fn kind_seg(k: MediaType) -> &'static str {
    match k {
        MediaType::Movie => "movie",
        MediaType::Tv => "series",
    }
}

fn year_of(s: &str) -> Option<u16> {
    s.get(..4)?.parse().ok().filter(|y| *y > 1850)
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn valid_tt(s: &str) -> bool {
    s.len() >= 9 && s.len() <= 12 && s.starts_with("tt") && s[2..].bytes().all(|b| b.is_ascii_digit())
}

/// First `tt\d{7,8}` in a text (also inside imdb.com URLs).
pub fn find_tt(text: &str) -> Option<String> {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    let r = R.get_or_init(|| regex::Regex::new(r"\btt(\d{7,8})\b").unwrap());
    r.captures(text).map(|c| format!("tt{}", &c[1]))
}

/// `{metas:[{id, imdb_id, name, releaseInfo, type, poster}]}` -> candidates.
pub fn cands_from_search(v: &Value, kind: MediaType) -> Vec<Cand> {
    let Some(arr) = v.get("metas").and_then(|m| m.as_array()) else {
        return vec![];
    };
    arr.iter()
        .filter_map(|m| {
            let id = str_field(m, "imdb_id").or_else(|| str_field(m, "id"))?;
            if !valid_tt(&id) {
                return None;
            }
            let name = str_field(m, "name")?;
            let release_info = str_field(m, "releaseInfo").or_else(|| str_field(m, "year"));
            Some(Cand {
                year: release_info.as_deref().and_then(year_of),
                id,
                kind,
                name,
                release_info,
                poster: str_field(m, "poster"),
            })
        })
        .collect()
}

fn cand_info(c: &Cand) -> MediaInfo {
    MediaInfo {
        imdb_id: c.id.clone(),
        media_type: c.kind,
        title: c.name.clone(),
        year: c.year,
        release_info: c.release_info.clone(),
        poster_url: c.poster.clone(),
        imdb_url: Some(format!("https://www.imdb.com/title/{}/", c.id)),
        ..Default::default()
    }
}

/// "136 min", "2h 16min", "49" -> minutes.
fn runtime_of(s: &str) -> Option<u32> {
    let l = s.to_lowercase();
    let nums: Vec<u32> = l
        .split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty())
        .filter_map(|x| x.parse().ok())
        .collect();
    match (l.contains('h'), nums.as_slice()) {
        (true, [h, m, ..]) => Some(h * 60 + m),
        (true, [h]) => Some(h * 60),
        (false, [m, ..]) => Some(*m),
        _ => None,
    }
    .filter(|m| *m > 0)
}

fn video_num(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
}

/// Pick the episode from a series meta's `videos`.
fn find_episode<'a>(meta: &'a Value, s: Option<u32>, e: Option<u32>, air_date: Option<&str>) -> Option<&'a Value> {
    let vids = meta.get("videos")?.as_array()?;
    let ep_of = |v: &Value| video_num(v, "episode").or_else(|| video_num(v, "number"));
    if let Some(d) = air_date {
        return vids.iter().find(|v| {
            str_field(v, "released").or_else(|| str_field(v, "firstAired")).is_some_and(|r| r.starts_with(d))
        });
    }
    let e = e? as u64;
    if let Some(s) = s {
        return vids.iter().find(|v| video_num(v, "season") == Some(s as u64) && ep_of(v) == Some(e));
    }
    // Absolute numbering (anime): S1 first, else the e-th regular episode overall.
    if let Some(v) = vids.iter().find(|v| video_num(v, "season") == Some(1) && ep_of(v) == Some(e)) {
        return Some(v);
    }
    let mut regular: Vec<&Value> = vids.iter().filter(|v| video_num(v, "season").unwrap_or(0) > 0).collect();
    regular.sort_by_key(|v| (video_num(v, "season").unwrap_or(0), ep_of(v).unwrap_or(0)));
    regular.get(e.checked_sub(1)? as usize).copied()
}

/// `/meta/{type}/{tt}.json` `meta` object -> MediaInfo (+ episode details).
pub fn info_from_meta(kind: MediaType, meta: &Value, p: &Parsed) -> MediaInfo {
    let id = str_field(meta, "imdb_id").or_else(|| str_field(meta, "id")).unwrap_or_default();
    let release_info = str_field(meta, "releaseInfo").or_else(|| str_field(meta, "year"));
    let year = str_field(meta, "year")
        .as_deref()
        .and_then(year_of)
        .or_else(|| release_info.as_deref().and_then(year_of))
        .or_else(|| str_field(meta, "released").as_deref().and_then(year_of));
    let genres: Vec<String> = meta
        .get("genres")
        .or_else(|| meta.get("genre"))
        .and_then(|g| g.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let mut info = MediaInfo {
        imdb_url: valid_tt(&id).then(|| format!("https://www.imdb.com/title/{id}/")),
        imdb_id: id,
        media_type: kind,
        title: str_field(meta, "name").unwrap_or_default(),
        year,
        release_info,
        overview: str_field(meta, "description").unwrap_or_default(),
        poster_url: str_field(meta, "poster"),
        genres,
        runtime_min: str_field(meta, "runtime").as_deref().and_then(runtime_of),
        imdb_rating: str_field(meta, "imdbRating").and_then(|r| r.parse::<f32>().ok()).filter(|r| *r > 0.0),
        ..Default::default()
    };
    if kind == MediaType::Tv {
        info.season = p.season;
        info.episode = p.episode;
        info.air_date = p.air_date.clone();
        if let Some(v) = find_episode(meta, p.season, p.episode, p.air_date.as_deref()) {
            info.episode_title = str_field(v, "name").or_else(|| str_field(v, "title"));
            info.episode_overview = str_field(v, "overview").or_else(|| str_field(v, "description"));
            if p.season.is_none() {
                info.season = video_num(v, "season").map(|x| x as u32);
            }
            info.episode = video_num(v, "episode").or_else(|| video_num(v, "number")).map(|x| x as u32).or(info.episode);
        }
    }
    info
}

/// Sidecar-quality image URLs: (poster, background).
fn image_urls(meta: &Value, id: &str, images: &str) -> (Option<String>, Option<String>) {
    let poster = str_field(meta, "poster")
        .map(|u| u.replace("/poster/small/", "/poster/medium/"))
        .or_else(|| valid_tt(id).then(|| format!("{images}/poster/medium/{id}/img")));
    let bg = str_field(meta, "background");
    (poster, bg)
}

fn image_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"\x89PNG") {
        Some("image/png")
    } else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

impl Cinemeta {
    pub fn new(base: &str, http: reqwest::Client) -> Self {
        let base = base.trim_end_matches('/').to_string();
        // Tests point both at a local mock server.
        let (images, ratings_url) = if base == CINEMETA {
            (METAHUB.to_string(), IMDB_RATINGS.to_string())
        } else {
            (base.clone(), format!("{base}/title.ratings.tsv.gz"))
        };
        Cinemeta { base, images, ratings_url, http }
    }

    /// One polite retry (after 1.5 s) on timeouts / 5xx: Cinemeta sporadically answers 500.
    async fn get_bytes(&self, url: &str, timeout: Duration) -> Result<Vec<u8>, NetErr> {
        match self.get_once(url, timeout).await {
            Err(NetErr::Net(e)) => {
                tracing::debug!("retrying after: {e}");
                tokio::time::sleep(Duration::from_millis(1500)).await;
                self.get_once(url, timeout).await
            }
            r => r,
        }
    }

    async fn get_once(&self, url: &str, timeout: Duration) -> Result<Vec<u8>, NetErr> {
        let resp = self
            .http
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| NetErr::Net(format!("request failed: {e}")))?;
        match resp.status().as_u16() {
            404 => return Err(NetErr::NotFound),
            s if !(200..300).contains(&s) => return Err(NetErr::Net(format!("HTTP {s} from {url}"))),
            _ => {}
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| NetErr::Net(format!("read failed: {e}")))
    }

    async fn get_json(&self, path: &str) -> Result<Value, NetErr> {
        // Uncached catalog searches take ~10-12 s on Cinemeta's side (cached: ~0.1 s).
        let t = if path.starts_with("/catalog/") { SEARCH_TIMEOUT } else { REQUEST_TIMEOUT };
        let b = self.get_bytes(&format!("{}{}", self.base, path), t).await?;
        serde_json::from_slice(&b).map_err(|e| NetErr::Net(format!("bad JSON from {path}: {e}")))
    }

    pub async fn search(&self, kind: MediaType, query: &str) -> Result<Vec<Cand>, NetErr> {
        // The query is a path segment: encode everything (incl. '/').
        let path = format!("/catalog/{}/top/search={}.json", kind_seg(kind), urlencoding::encode(query.trim()));
        match self.get_json(&path).await {
            Ok(v) => Ok(cands_from_search(&v, kind)),
            Err(NetErr::NotFound) => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    pub async fn meta(&self, kind: MediaType, id: &str) -> Result<Value, NetErr> {
        if !valid_tt(id) {
            return Err(NetErr::NotFound);
        }
        let v = self.get_json(&format!("/meta/{}/{id}.json", kind_seg(kind))).await?;
        match v.get("meta") {
            Some(m) if m.is_object() => Ok(m.clone()),
            _ => Err(NetErr::NotFound),
        }
    }

    /// Unknown type (an id from an .nfo): try the preferred type, then the other.
    pub async fn meta_any(&self, id: &str, prefer: MediaType) -> Result<(MediaType, Value), NetErr> {
        let other = if prefer == MediaType::Movie { MediaType::Tv } else { MediaType::Movie };
        match self.meta(prefer, id).await {
            Ok(m) => Ok((prefer, m)),
            Err(NetErr::NotFound) => self.meta(other, id).await.map(|m| (other, m)),
            Err(e) => Err(e),
        }
    }

    pub async fn image(&self, url: &str) -> Result<Vec<u8>, NetErr> {
        let b = self.get_bytes(url, REQUEST_TIMEOUT).await?;
        if image_mime(&b).is_none() {
            return Err(NetErr::Net(format!("not an image: {url}")));
        }
        Ok(b)
    }
}

/// Kodi/Infuse expect JPEG: metahub posters are WebP, backgrounds 1920px JPEG.
/// Convert / downscale with macOS `sips`; a JPEG small enough is kept as is.
async fn to_jpeg(bytes: Vec<u8>, max_px: u32, max_keep: usize) -> Option<Vec<u8>> {
    let is_jpeg = image_mime(&bytes) == Some("image/jpeg");
    if is_jpeg && bytes.len() <= max_keep {
        return Some(bytes);
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir();
    let src = dir.join(format!("tide-img-{}-{n}.src", std::process::id()));
    let dst = dir.join(format!("tide-img-{}-{n}.jpg", std::process::id()));
    let out = async {
        tokio::fs::write(&src, &bytes).await.ok()?;
        let st = tokio::process::Command::new("/usr/bin/sips")
            .args(["-s", "format", "jpeg", "-s", "formatOptions", "85", "-Z", &max_px.to_string()])
            .arg(&src)
            .arg("--out")
            .arg(&dst)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .ok()?;
        if !st.success() {
            return None;
        }
        tokio::fs::read(&dst).await.ok().filter(|b| image_mime(b) == Some("image/jpeg"))
    }
    .await;
    let _ = tokio::fs::remove_file(&src).await;
    let _ = tokio::fs::remove_file(&dst).await;
    match out {
        Some(b) => Some(b),
        None if is_jpeg => Some(bytes),
        None => {
            tracing::warn!("image conversion to JPEG failed; skipping sidecar image");
            None
        }
    }
}

// ---------------------------------------------------------------- matching

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Matched { kind: MediaType, id: String, note: String },
    Needs { note: String },
    NoMatch,
}

/// Best candidate for `titles`/`year` plus its confidence.
pub fn evaluate(cands: &[Cand], titles: &[String], year: Option<u16>, tv: bool) -> Option<(usize, Confidence)> {
    let sims: Vec<f64> = cands
        .iter()
        .map(|c| titles.iter().map(|t| nameparse::similarity(t, &c.name)).fold(0.0, f64::max))
        .collect();
    let year_ok = |c: &Cand| year.is_none_or(|q| c.year.is_some_and(|y| (y as i32 - q as i32).abs() <= 1));
    let mut best: Option<(usize, Confidence)> = None;
    for (i, c) in cands.iter().enumerate() {
        let rivals = cands
            .iter()
            .enumerate()
            .filter(|(j, o)| *j != i && o.id != c.id && sims[*j] >= nameparse::SIM_NO_YEAR && year_ok(o))
            .count();
        let conf = nameparse::confidence(sims[i], year, c.year, rivals, tv);
        let key = |x: &Confidence| (x.accepted, x.year_ok == Some(true), (x.similarity * 1000.0) as i64);
        if best.as_ref().is_none_or(|(_, b)| key(&conf) > key(b)) {
            best = Some((i, conf));
        }
    }
    best
}

const COUNTRY_NAMES: &[(&str, &[&str])] = &[
    ("US", &["united states", "usa"]),
    ("UK", &["united kingdom", "uk", "england"]),
    ("AU", &["australia"]),
    ("CA", &["canada"]),
    ("NZ", &["new zealand"]),
    ("IE", &["ireland"]),
];

fn country_matches(meta: &Value, code: &str) -> bool {
    let c = str_field(meta, "country").unwrap_or_default().to_lowercase();
    COUNTRY_NAMES
        .iter()
        .find(|(k, _)| *k == code)
        .is_some_and(|(_, names)| names.iter().any(|n| c.contains(n)))
}

/// Sort key: open-ended ("2023-") first, then by latest year in releaseInfo.
fn recency(c: &Cand) -> (bool, u16) {
    let ri = c.release_info.as_deref().unwrap_or("").trim();
    let open = ri.ends_with(['-', '–']);
    let last = ri
        .split(|ch: char| !ch.is_ascii_digit())
        .filter_map(|t| t.parse::<u16>().ok().filter(|y| *y > 1850))
        .last()
        .or(c.year)
        .unwrap_or(0);
    (open, last)
}

/// Merge the parses (torrent name first, then main video, then its folder).
fn merged(parses: &[Parsed]) -> Parsed {
    let mut base = parses.iter().find(|p| !p.title.is_empty()).cloned().unwrap_or_default();
    for p in parses {
        base.year = base.year.or(p.year);
        base.season = base.season.or(p.season);
        base.episode = base.episode.or(p.episode);
        base.air_date = base.air_date.clone().or(p.air_date.clone());
        base.country = base.country.clone().or(p.country.clone());
        base.anime |= p.anime;
        base.tv_hint |= p.tv_hint;
    }
    base
}

/// Search ladder over the candidate names; auto-accept only above the thresholds.
pub async fn auto_match(cm: &Cinemeta, parses: &[Parsed]) -> Result<Outcome, NetErr> {
    let q = merged(parses);
    if q.collection || parses.first().is_some_and(|p| p.collection) {
        return Ok(Outcome::Needs { note: "collection / box set: no single title".into() });
    }
    // Query variants: each distinct title, its AKA side, then minus its last word.
    let mut variants: Vec<String> = Vec::new();
    let push = |t: &str, v: &mut Vec<String>| {
        let t = t.trim();
        if !t.is_empty() && !v.iter().any(|x| nameparse::norm_title(x) == nameparse::norm_title(t)) {
            v.push(t.to_string());
        }
    };
    for p in parses.iter().filter(|p| !p.title.is_empty()) {
        push(&p.title, &mut variants);
        if let Some(a) = &p.alt_title {
            push(a, &mut variants);
        }
    }
    for p in parses.iter().filter(|p| !p.title.is_empty()) {
        let words: Vec<&str> = p.title.split_whitespace().collect();
        if words.len() >= 3 {
            push(&words[..words.len() - 1].join(" "), &mut variants);
        }
    }
    if variants.is_empty() {
        return Ok(Outcome::NoMatch);
    }
    let tv = q.is_tv();
    let kinds = if tv { [MediaType::Tv, MediaType::Movie] } else { [MediaType::Movie, MediaType::Tv] };

    let mut searches = 0usize;
    let mut any_results = false;
    let mut best_note: Option<(f64, String)> = None;
    let mut net_err: Option<String> = None;
    for v in &variants {
        for &kind in &kinds {
            if searches >= MAX_SEARCHES {
                break;
            }
            searches += 1;
            let cands = match cm.search(kind, v).await {
                Ok(c) => c,
                Err(NetErr::Net(e)) => {
                    // Keep going (another variant may match); never cache an incomplete answer.
                    net_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            };
            if cands.is_empty() {
                continue;
            }
            any_results = true;
            let top: Vec<Cand> = cands.into_iter().take(10).collect();
            let titles = vec![v.clone()];
            let Some((i, conf)) = evaluate(&top, &titles, q.year, kind == MediaType::Tv) else { continue };
            if conf.accepted {
                return Ok(Outcome::Matched {
                    kind,
                    id: top[i].id.clone(),
                    note: format!("{} (query {v:?})", conf.reason),
                });
            }
            // Same-name titles: the requested episode, then the country hint, break the tie.
            let ep = if kind == MediaType::Tv { q.season.zip(q.episode) } else { None };
            if conf.ambiguous && (ep.is_some() || q.country.is_some()) {
                let mut strong: Vec<&Cand> = top
                    .iter()
                    .filter(|c| nameparse::similarity(v, &c.name) >= nameparse::SIM_NO_YEAR)
                    .collect();
                // Running / recent shows first: the 3-meta budget goes to the likely ones.
                strong.sort_by_key(|c| std::cmp::Reverse(recency(c)));
                let mut metas: Vec<(&Cand, Value)> = Vec::new();
                for c in strong.into_iter().take(3) {
                    if let Ok(m) = cm.meta(kind, &c.id).await {
                        metas.push((c, m));
                    }
                }
                if let Some((s, e)) = ep {
                    let has: Vec<(&Cand, Value)> =
                        metas.iter().filter(|(_, m)| find_episode(m, Some(s), Some(e), None).is_some()).cloned().collect();
                    if has.len() == 1 {
                        return Ok(Outcome::Matched {
                            kind,
                            id: has[0].0.id.clone(),
                            note: format!("{}; only {} has S{s:02}E{e:02}", conf.reason, has[0].0.id),
                        });
                    }
                    if !has.is_empty() {
                        metas = has;
                    }
                }
                if let Some(code) = q.country.as_deref() {
                    let hits: Vec<&Cand> = metas.iter().filter(|(_, m)| country_matches(m, code)).map(|(c, _)| *c).collect();
                    if hits.len() == 1 {
                        return Ok(Outcome::Matched {
                            kind,
                            id: hits[0].id.clone(),
                            note: format!("{}; country {code} decides", conf.reason),
                        });
                    }
                }
            }
            if best_note.as_ref().is_none_or(|(s, _)| conf.similarity > *s) {
                best_note = Some((conf.similarity, format!("best {:?}: {}", top[i].name, conf.reason)));
            }
        }
    }
    if let Some(e) = net_err {
        return Err(NetErr::Net(e));
    }
    if !any_results {
        return Ok(Outcome::NoMatch);
    }
    Ok(Outcome::Needs { note: best_note.map(|(_, n)| n).unwrap_or_else(|| "no confident result".into()) })
}

// ---------------------------------------------------------------- locating a torrent

#[derive(Debug, Clone)]
pub struct Loc {
    /// Folder the sidecars go in.
    pub dir: PathBuf,
    /// Single-file torrents: the file stem (sidecars are `<stem>-poster.jpg` ...).
    pub stem: Option<String>,
    pub exists: bool,
    pub files: Vec<FileRow>,
    /// Lowercased relative names of the torrent's own files (never overwritten).
    pub owned: HashSet<String>,
    pub name: String,
    pub paused: bool,
}

impl Loc {
    #[cfg(test)]
    pub fn from_parts(output_path: &Path, files: Vec<FileRow>, name: &str, paused: bool) -> Loc {
        Self::from_parts_in(output_path, files, name, paused, None)
    }

    /// `shared`: the download dir. A single file in its own folder (not directly in
    /// `shared` or its parent) uses folder-style sidecars (`tide.json`, `poster.jpg`…),
    /// unless old `<stem>.tide.json` sidecars are still next to it.
    pub fn from_parts_in(output_path: &Path, files: Vec<FileRow>, name: &str, paused: bool, shared: Option<&Path>) -> Loc {
        let mut single = files.len() == 1;
        if single {
            if let (Some(shared), Some(dir), Some(stem)) = (shared, output_path.parent(), output_path.file_stem()) {
                let own = dir != shared && Some(dir) != shared.parent();
                let legacy = dir.join(format!("{}.tide.json", stem.to_string_lossy())).is_file();
                if own && !legacy {
                    single = false;
                }
            }
        }
        let output_path = if single || files.len() != 1 {
            output_path.to_path_buf()
        } else {
            // Folder-style single: the folder holds the file under its relative name.
            output_path.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        let output_path = output_path.as_path();
        let (dir, stem, exists) = if single {
            (
                output_path.parent().map(Path::to_path_buf).unwrap_or_default(),
                output_path.file_stem().map(|s| s.to_string_lossy().into_owned()),
                output_path.exists(),
            )
        } else {
            (output_path.to_path_buf(), None, output_path.is_dir())
        };
        let owned = if single {
            output_path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .into_iter()
                .collect()
        } else {
            files.iter().map(|f| f.name.to_lowercase()).collect()
        };
        Loc { dir, stem, exists, files, owned, name: name.to_string(), paused }
    }

    fn is_owned(&self, rel: &str) -> bool {
        self.owned.contains(&rel.to_lowercase())
    }

    /// Biggest non-extra video file.
    fn main_video(&self) -> Option<&FileRow> {
        self.files
            .iter()
            .filter(|f| f.kind == FileKind::Video && !nameparse::is_extra(&f.name))
            .max_by_key(|f| f.size)
    }

    /// No lookup for non-video torrents (music, ISOs, ebooks) or < 50 MB of video.
    fn worth_lookup(&self) -> bool {
        let total: u64 = self
            .files
            .iter()
            .filter(|f| f.kind == FileKind::Video && !nameparse::is_extra(&f.name))
            .map(|f| f.size)
            .sum();
        total >= MIN_VIDEO_BYTES
    }

    /// Torrent name, main video basename, its parent folder.
    fn parses(&self) -> Vec<Parsed> {
        let mut out = vec![nameparse::parse(&self.name)];
        if let Some(v) = self.main_video() {
            let mut parts: Vec<&str> = v.name.split('/').collect();
            if let Some(base) = parts.pop() {
                out.push(nameparse::parse(base));
            }
            if let Some(dir) = parts.pop() {
                if dir != self.name {
                    out.push(nameparse::parse(dir));
                }
            }
        }
        out
    }
}

fn locate(st: &AppState, key: &str) -> Result<Loc, String> {
    let row = engine::list_rows(st)
        .into_iter()
        .find(|r| r.key == key)
        .ok_or_else(|| format!("NOT_FOUND: no torrent {key}"))?;
    let out = row.output_path.clone().ok_or_else(|| not_ready("metadata not resolved yet"))?;
    let files = engine::files(st, key)?;
    Ok(Loc::from_parts_in(Path::new(&out), files, &row.name, row.state == TorrentState::Paused, Some(&st.download_dir)))
}

// ---------------------------------------------------------------- sidecars

struct Names {
    json: String,
    poster: String,
    fanart: String,
    nfo_movie: String,
    nfo_tv: String,
}

fn names(stem: Option<&str>) -> Names {
    match stem {
        Some(s) => Names {
            json: format!("{s}.tide.json"),
            poster: format!("{s}-poster.jpg"),
            fanart: format!("{s}-fanart.jpg"),
            nfo_movie: format!("{s}.nfo"),
            nfo_tv: format!("{s}.nfo"),
        },
        None => Names {
            json: "tide.json".into(),
            poster: "poster.jpg".into(),
            fanart: "fanart.jpg".into(),
            nfo_movie: "movie.nfo".into(),
            nfo_tv: "tvshow.nfo".into(),
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TideJson {
    #[serde(default)]
    tide: u32,
    #[serde(default)]
    key: String,
    #[serde(default)]
    torrent_name: String,
    #[serde(default)]
    manual: bool,
    #[serde(default)]
    fetched_at: u64,
    #[serde(default)]
    source: String,
    #[serde(default)]
    info: MediaInfo,
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

fn xml_esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            _ => o.push(c),
        }
    }
    o
}

/// Minimal Kodi nfo. Single-file TV episodes get `<episodedetails>`.
fn nfo_xml(info: &MediaInfo, single_file: bool) -> String {
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n");
    let tag = |x: &mut String, t: &str, v: &str| x.push_str(&format!("  <{t}>{}</{t}>\n", xml_esc(v)));
    let episode = info.media_type == MediaType::Tv && single_file && info.episode.is_some();
    let root = if episode {
        "episodedetails"
    } else if info.media_type == MediaType::Tv {
        "tvshow"
    } else {
        "movie"
    };
    x.push_str(&format!("<{root}>\n"));
    if episode {
        let e = info.episode.unwrap_or(0);
        tag(&mut x, "title", info.episode_title.as_deref().unwrap_or(&format!("Episode {e}")));
        tag(&mut x, "showtitle", &info.title);
        if let Some(s) = info.season {
            tag(&mut x, "season", &s.to_string());
        }
        tag(&mut x, "episode", &e.to_string());
        tag(&mut x, "plot", info.episode_overview.as_deref().unwrap_or(&info.overview));
    } else {
        tag(&mut x, "title", &info.title);
        if let Some(y) = info.year {
            tag(&mut x, "year", &y.to_string());
        }
        tag(&mut x, "plot", &info.overview);
        if let Some(r) = info.runtime_min {
            tag(&mut x, "runtime", &r.to_string());
        }
        for g in &info.genres {
            tag(&mut x, "genre", g);
        }
    }
    if let Some(r) = info.imdb_rating {
        let votes = info.imdb_votes.map(|v| format!("<votes>{v}</votes>")).unwrap_or_default();
        x.push_str(&format!(
            "  <ratings><rating name=\"imdb\" max=\"10\" default=\"true\"><value>{r:.1}</value>{votes}</rating></ratings>\n"
        ));
    }
    if !episode && !info.imdb_id.is_empty() {
        x.push_str(&format!("  <uniqueid type=\"imdb\" default=\"true\">{}</uniqueid>\n", xml_esc(&info.imdb_id)));
    }
    x.push_str(&format!("</{root}>\n"));
    x
}

fn read_sidecar(loc: &Loc) -> Option<TideJson> {
    let n = names(loc.stem.as_deref());
    if loc.is_owned(&n.json) {
        return None;
    }
    let b = std::fs::read(loc.dir.join(&n.json)).ok()?;
    match serde_json::from_slice::<TideJson>(&b) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!("ignoring bad {}: {e}", n.json);
            None
        }
    }
}

/// Images and nfo first, `tide.json` last (atomic): tide.json present = complete.
/// Never writes a name the torrent itself owns. Failures are logged, not fatal.
fn write_sidecars(loc: &Loc, key: &str, info: &MediaInfo, poster: Option<&[u8]>, fanart: Option<&[u8]>) {
    let n = names(loc.stem.as_deref());
    put_sidecar(loc, &n.poster, poster);
    put_sidecar(loc, &n.fanart, fanart);
    let tj = TideJson {
        tide: 1,
        key: key.to_string(),
        torrent_name: loc.name.clone(),
        manual: info.manual,
        fetched_at: now_secs(),
        source: "cinemeta".into(),
        info: info.clone(),
    };
    write_text_sidecars(loc, &tj);
}

/// Write (`Some`) or remove (`None`) one sidecar; never a torrent-owned name.
fn put_sidecar(loc: &Loc, rel: &str, data: Option<&[u8]>) {
    if loc.is_owned(rel) {
        return;
    }
    let p = loc.dir.join(rel);
    let res = match data {
        Some(d) => write_atomic(&p, d),
        // A previous match's image would be wrong now.
        None => match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    };
    if let Err(e) = res {
        tracing::warn!("sidecar {} failed: {e}", p.display());
    }
}

/// The .nfo, then `tide.json` (last: its presence means complete).
fn write_text_sidecars(loc: &Loc, tj: &TideJson) {
    let n = names(loc.stem.as_deref());
    let single = loc.stem.is_some();
    let (nfo, other) = match tj.info.media_type {
        MediaType::Movie => (&n.nfo_movie, &n.nfo_tv),
        MediaType::Tv => (&n.nfo_tv, &n.nfo_movie),
    };
    if other != nfo {
        put_sidecar(loc, other, None);
    }
    put_sidecar(loc, nfo, Some(nfo_xml(&tj.info, single).as_bytes()));
    put_sidecar(loc, &n.json, Some(&serde_json::to_vec_pretty(tj).unwrap_or_default()));
}

/// Our sidecars that exist on disk (never torrent-owned files).
pub fn sidecar_paths_loc(loc: &Loc) -> Vec<PathBuf> {
    let n = names(loc.stem.as_deref());
    let mut v: Vec<String> = vec![n.json.clone(), n.poster, n.fanart, n.nfo_movie, n.nfo_tv, format!(".{}.tmp", n.json)];
    v.dedup();
    v.into_iter()
        .filter(|rel| !loc.is_owned(rel))
        .map(|rel| loc.dir.join(rel))
        .filter(|p| p.is_file())
        .collect()
}

/// For `remove_torrent` with deleteFiles: every sidecar of `key` that exists on disk.
/// Works without anything in memory. Returns (files, folder to try to remove).
pub fn sidecar_paths(st: &AppState, key: &str) -> (Vec<PathBuf>, Option<PathBuf>) {
    match locate(st, key) {
        Ok(loc) => {
            let folder = loc.stem.is_none().then(|| loc.dir.clone());
            (sidecar_paths_loc(&loc), folder)
        }
        Err(_) => (vec![], None),
    }
}

/// Drop memory / image cache / negative-cache entries for `key`.
pub fn forget(st: &AppState, key: &str) {
    let m = &st.meta;
    let had = m.with(&st.data_dir, |g| {
        g.infos.remove(key);
        g.index.misses.contains_key(key) || g.index.needs.contains_key(key)
    });
    if had {
        m.with_index(&st.data_dir, |ix| {
            ix.misses.remove(key);
            ix.needs.remove(key);
        });
    }
    m.drop_images(key);
}

// ---------------------------------------------------------------- lookup

/// IMDb id from the torrent's small .nfo/.txt files (exact match).
async fn scan_tt(st: Option<&AppState>, key: &str, loc: &Loc) -> Option<String> {
    let mut texts: Vec<&FileRow> = loc
        .files
        .iter()
        .filter(|f| {
            let l = f.name.to_lowercase();
            (l.ends_with(".nfo") || l.ends_with(".txt")) && f.size > 0 && f.size <= MAX_TEXT_BYTES
        })
        .collect();
    texts.sort_by_key(|f| (!f.name.to_lowercase().ends_with(".nfo"), f.size));
    for f in texts.into_iter().take(3) {
        let path = if loc.stem.is_some() { loc.dir.join(f.name.rsplit('/').next().unwrap_or(&f.name)) } else { loc.dir.join(&f.name) };
        let bytes = if f.progress_bytes >= f.size {
            tokio::fs::read(&path).await.ok()
        } else if let (Some(st), false) = (st, loc.paused) {
            engine::read_file(st, key, f.index, MAX_TEXT_BYTES as usize, 20, false).await.ok()
        } else {
            None
        };
        if let Some(tt) = bytes.and_then(|b| find_tt(&String::from_utf8_lossy(&b))) {
            return Some(tt);
        }
    }
    None
}

async fn fetch_image(cm: &Cinemeta, url: Option<String>, max_px: u32, max_keep: usize) -> Option<Vec<u8>> {
    let url = url?;
    match cm.image(&url).await {
        Ok(b) => to_jpeg(b, max_px, max_keep).await,
        Err(e) => {
            tracing::warn!("image download failed: {e:?}");
            None
        }
    }
}

/// Fetch details + images for a chosen title, write sidecars, update memory.
async fn apply_match(
    meta: &MetaState,
    dd: &Path,
    cm: &Cinemeta,
    key: &str,
    loc: &Loc,
    kind: MediaType,
    id: &str,
    manual: bool,
    note: Option<String>,
) -> Result<MediaInfo, NetErr> {
    let p = nameparse::parse(&loc.name);
    let parsed = merged(&[p, loc.parses().into_iter().nth(1).unwrap_or_default()]);
    let m = cm.meta(kind, id).await?;
    let mut info = info_from_meta(kind, &m, &parsed);
    info.manual = manual;
    info.match_note = note;
    let (pu, bu) = image_urls(&m, &info.imdb_id, &cm.images);
    let poster = fetch_image(cm, pu, 750, 1_000_000).await;
    let fanart = fetch_image(cm, bu, 1280, 400_000).await;
    write_sidecars(loc, key, &info, poster.as_deref(), fanart.as_deref());
    meta.with(dd, |g| {
        g.infos.insert(key.to_string(), info.clone());
    });
    meta.want_rating(dd, cm, key, &info, loc);
    meta.with_index(dd, |ix| {
        ix.misses.remove(key);
        ix.needs.remove(key);
    });
    meta.drop_images(key);
    Ok(info)
}

/// Self-heal for a matched title whose poster/fanart sidecar is missing (image not yet
/// published when the match ran, network hiccup...): re-download just the images from
/// metahub by IMDb id. No Cinemeta query. At most once per run per key; a failure
/// backs off for MISS_TTL (persisted in the index). Returns true if anything was written.
pub async fn heal_images(meta: &MetaState, dd: &Path, cm: &Cinemeta, key: &str, loc: &Loc, info: &MediaInfo, now: u64) -> bool {
    if info.needs_match || !valid_tt(&info.imdb_id) || !loc.exists {
        return false;
    }
    let n = names(loc.stem.as_deref());
    let missing = |rel: &str| !loc.is_owned(rel) && !loc.dir.join(rel).exists();
    let (need_p, need_f) = (missing(&n.poster), missing(&n.fanart));
    if !(need_p || need_f) || read_sidecar(loc).is_none() {
        return false;
    }
    if meta.with(dd, |g| g.index.img_fail.get(key).is_some_and(|t| now.saturating_sub(*t) < MISS_TTL)) {
        return false;
    }
    if !meta.healed.lock().unwrap().insert(key.to_string()) {
        return false;
    }
    let id = &info.imdb_id;
    let (mut ok, mut wrote) = (true, false);
    for (need, rel, url, px, keep) in [
        (need_p, &n.poster, format!("{}/poster/medium/{id}/img", cm.images), 750, 1_000_000),
        (need_f, &n.fanart, format!("{}/background/medium/{id}/img", cm.images), 1280, 400_000),
    ] {
        if !need {
            continue;
        }
        match fetch_image(cm, Some(url), px, keep).await {
            Some(b) => {
                put_sidecar(loc, rel, Some(&b));
                wrote = true;
            }
            None => ok = false,
        }
    }
    let had_fail = meta.with(dd, |g| g.index.img_fail.contains_key(key));
    if !ok {
        meta.with_index(dd, |ix| {
            ix.img_fail.insert(key.to_string(), now);
        });
    } else if had_fail {
        meta.with_index(dd, |ix| {
            ix.img_fail.remove(key);
        });
    }
    if wrote {
        meta.drop_images(key);
        tracing::info!("re-fetched missing sidecar images for {key}");
    }
    wrote
}

/// Steps 3..8 of the lookup order (after `locate`). `st` is only used to priority-fetch
/// .nfo files that are not downloaded yet (None in tests).
pub async fn lookup_loc(
    meta: &MetaState,
    dd: &Path,
    cm: &Cinemeta,
    st: Option<&AppState>,
    key: &str,
    loc: &Loc,
    now: u64,
) -> Result<Option<MediaInfo>, String> {
    if let Some(i) = meta.cached(dd, key) {
        return Ok(Some(i));
    }
    // Sidecar: no network, ever.
    if let Some(tj) = read_sidecar(loc) {
        if tj.key != key {
            tracing::info!("tide.json key {} differs from {key}; using it anyway", tj.key);
        }
        let mut info = tj.info;
        info.manual = tj.manual;
        meta.with(dd, |g| g.infos.insert(key.to_string(), info.clone()));
        meta.want_rating(dd, cm, key, &info, loc);
        return Ok(Some(info));
    }
    if !loc.worth_lookup() {
        return Ok(None);
    }
    if !loc.exists {
        return Err(not_ready("waiting for files"));
    }
    let fresh = |t: u64| now.saturating_sub(t) < MISS_TTL;
    let cached = meta.with(dd, |g| {
        if g.index.misses.get(key).is_some_and(|t| fresh(*t)) {
            return Some(None);
        }
        g.index.needs.get(key).filter(|n| fresh(n.at)).map(|n| Some(n.info.clone()))
    });
    if let Some(c) = cached {
        return Ok(c);
    }

    let _permit = meta.gate.acquire().await.map_err(internal)?;
    if let Some(i) = meta.cached(dd, key) {
        return Ok(Some(i));
    }
    let parses = loc.parses();
    let prefer = if merged(&parses).is_tv() { MediaType::Tv } else { MediaType::Movie };

    if let Some(tt) = scan_tt(st, key, loc).await {
        match cm.meta_any(&tt, prefer).await {
            Ok((kind, _)) => {
                let info = apply_match(meta, dd, cm, key, loc, kind, &tt, false, Some(format!("IMDb id {tt} from .nfo"))).await;
                return info.map(Some).map_err(|e| e.to_ipc());
            }
            Err(NetErr::NotFound) => tracing::info!("{tt} from .nfo unknown to Cinemeta; searching by name"),
            Err(e) => return Err(e.to_ipc()),
        }
    }

    match auto_match(cm, &parses).await.map_err(|e| e.to_ipc())? {
        Outcome::Matched { kind, id, note } => {
            apply_match(meta, dd, cm, key, loc, kind, &id, false, Some(note)).await.map(Some).map_err(|e| e.to_ipc())
        }
        Outcome::Needs { note } => {
            let q = merged(&parses);
            let info = MediaInfo {
                media_type: prefer,
                title: q.title.clone(),
                year: q.year,
                season: q.season,
                episode: q.episode,
                air_date: q.air_date.clone(),
                needs_match: true,
                match_note: Some(note),
                ..Default::default()
            };
            meta.with_index(dd, |ix| {
                ix.needs.insert(key.to_string(), NeedsRec { at: now, info: info.clone() });
            });
            Ok(Some(info))
        }
        Outcome::NoMatch => {
            meta.with_index(dd, |ix| {
                ix.misses.insert(key.to_string(), now);
            });
            Ok(None)
        }
    }
}

fn client(st: &AppState) -> Cinemeta {
    Cinemeta::new(CINEMETA, st.meta.http())
}

fn norm_key(key: &str) -> String {
    key.trim().to_ascii_lowercase()
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub async fn get_media_info(state: St<'_>, key: String) -> Result<Option<MediaInfo>, String> {
    let key = norm_key(&key);
    let st: Arc<AppState> = state.inner().clone();
    if let Some(i) = st.meta.cached(&st.data_dir, &key) {
        return Ok(Some(i));
    }
    let loc = locate(&st, &key)?;
    let r = lookup_loc(&st.meta, &st.data_dir, &client(&st), Some(&st), &key, &loc, now_secs()).await;
    if let Ok(Some(info)) = &r {
        let (st2, info, key2) = (st.clone(), info.clone(), key.clone());
        tokio::spawn(async move {
            heal_images(&st2.meta, &st2.data_dir, &client(&st2), &key2, &loc, &info, now_secs()).await;
        });
    }
    r
}

/// Sidecar image as a data URL (small LRU).
#[tauri::command]
pub async fn get_media_image(state: St<'_>, key: String, kind: String) -> Result<String, String> {
    let key = norm_key(&key);
    let st: Arc<AppState> = state.inner().clone();
    let ck = format!("{key}:{kind}");
    if let Some(hit) = st.meta.images.lock().unwrap().iter().find(|(k, _)| *k == ck).map(|(_, v)| v.clone()) {
        return Ok((*hit).clone());
    }
    let loc = locate(&st, &key)?;
    let n = names(loc.stem.as_deref());
    let rel = match kind.as_str() {
        "poster" => n.poster,
        "backdrop" => n.fanart,
        _ => return Err(format!("INVALID_INPUT: bad image kind {kind:?}")),
    };
    if loc.is_owned(&rel) {
        return Err("NOT_FOUND: no sidecar image".into());
    }
    let bytes = tokio::fs::read(loc.dir.join(&rel)).await.map_err(|_| "NOT_FOUND: no sidecar image".to_string())?;
    let mime = image_mime(&bytes).ok_or_else(|| "NOT_FOUND: sidecar is not an image".to_string())?;
    let url = Arc::new(format!("data:{mime};base64,{}", crate::artwork::base64(&bytes)));
    {
        let mut lru = st.meta.images.lock().unwrap();
        lru.push_back((ck, url.clone()));
        while lru.len() > LRU_CAP {
            lru.pop_front();
        }
    }
    Ok((*url).clone())
}

/// JS: `{ query, mediaType }` (mediaType "movie" | "tv" | null). An IMDb id ("tt…") or
/// imdb.com URL resolves directly.
#[tauri::command]
pub async fn search_media(state: St<'_>, query: String, media_type: Option<MediaType>) -> Result<Vec<MediaInfo>, String> {
    let q = query.trim().to_string();
    if q.is_empty() {
        return Err("INVALID_INPUT: empty query".into());
    }
    let st: Arc<AppState> = state.inner().clone();
    let cm = client(&st);
    let _permit = st.meta.gate.acquire().await.map_err(internal)?;
    if let Some(tt) = find_tt(&q).filter(|_| q.len() < 80) {
        return match cm.meta_any(&tt, media_type.unwrap_or_default()).await {
            Ok((kind, m)) => {
                let mut i = info_from_meta(kind, &m, &Parsed::default());
                i.poster_url = image_urls(&m, &tt, &cm.images).0;
                Ok(vec![i])
            }
            Err(NetErr::NotFound) => Ok(vec![]),
            Err(e) => Err(e.to_ipc()),
        };
    }
    let p = nameparse::parse(&q);
    let text = if p.title.is_empty() { q.clone() } else { p.title.clone() };
    let kinds: Vec<MediaType> = match media_type {
        Some(k) => vec![k],
        None => vec![MediaType::Movie, MediaType::Tv],
    };
    let mut all: Vec<(f64, usize, Cand)> = Vec::new();
    for k in kinds {
        for (i, c) in cm.search(k, &text).await.map_err(|e| e.to_ipc())?.into_iter().enumerate() {
            let mut s = nameparse::similarity(&text, &c.name);
            if p.year.is_some() && c.year == p.year {
                s += 0.2;
            }
            all.push((s, i, c));
        }
    }
    all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    Ok(all.into_iter().take(8).map(|(_, _, c)| cand_info(&c)).collect())
}

/// JS: `{ key, imdbId, mediaType }`. Manual fix: rewrites all sidecars with manual=true.
#[tauri::command]
pub async fn set_media_match(state: St<'_>, key: String, imdb_id: String, media_type: MediaType) -> Result<MediaInfo, String> {
    let key = norm_key(&key);
    let id = imdb_id.trim().to_string();
    if !valid_tt(&id) {
        return Err("INVALID_INPUT: not an IMDb id".into());
    }
    let st: Arc<AppState> = state.inner().clone();
    let loc = locate(&st, &key)?;
    if !loc.exists {
        return Err(not_ready("waiting for files"));
    }
    let cm = client(&st);
    let _permit = st.meta.gate.acquire().await.map_err(internal)?;
    apply_match(&st.meta, &st.data_dir, &cm, &key, &loc, media_type, &id, true, Some("chosen by you".into()))
        .await
        .map_err(|e| e.to_ipc())
}

/// Delete the sidecars and forget everything; the next `get_media_info` matches again.
#[tauri::command]
pub async fn clear_media_match(state: St<'_>, key: String) -> Result<(), String> {
    let key = norm_key(&key);
    let st: Arc<AppState> = state.inner().clone();
    for p in sidecar_paths(&st, &key).0 {
        if let Err(e) = std::fs::remove_file(&p) {
            tracing::warn!("removing {} failed: {e}", p.display());
        }
    }
    forget(&st, &key);
    Ok(())
}

// ---------------------------------------------------------------- IMDb rating fallback

#[derive(Default)]
struct RatingQueue {
    q: Mutex<RqInner>,
}

#[derive(Default)]
struct RqInner {
    /// key -> (IMDb id, where its sidecars are).
    pending: HashMap<String, (String, Loc)>,
    /// Keys queued this session (each is looked up at most once per run).
    tried: HashSet<String>,
    /// Ratings found: key -> (rating, votes), merged into memory by `MetaState::cached`.
    done: HashMap<String, (f32, u32)>,
    running: bool,
    retry_after: u64,
}

/// Drains the queue in batches: one dataset scan serves every pending title.
async fn rating_worker(rq: Arc<RatingQueue>, dd: PathBuf, http: reqwest::Client, url: String) {
    // Let the burst of polls (one per torrent row) land in the same batch.
    tokio::time::sleep(Duration::from_millis(300)).await;
    loop {
        let batch: Vec<(String, (String, Loc))> = {
            let mut g = rq.q.lock().unwrap();
            if g.pending.is_empty() {
                g.running = false;
                return;
            }
            g.pending.drain().collect()
        };
        let Some(path) = ensure_ratings(&http, &url, &dd).await else {
            let mut g = rq.q.lock().unwrap();
            g.retry_after = now_secs() + RATINGS_RETRY;
            for (k, _) in &batch {
                g.tried.remove(k);
            }
            continue;
        };
        let ids: HashSet<String> = batch.iter().map(|(_, (id, _))| id.clone()).collect();
        let p2 = path.clone();
        let found = match tokio::task::spawn_blocking(move || std::fs::File::open(&p2).and_then(|f| scan_ratings(f, &ids))).await {
            Ok(Ok(f)) => f,
            r => {
                tracing::warn!("IMDb ratings scan failed ({r:?}); dropping the file");
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        for (key, (id, loc)) in batch {
            let Some(&(r, v)) = found.get(&id) else {
                tracing::debug!("{id}: not in the IMDb ratings dataset");
                continue;
            };
            if let Some(mut tj) = read_sidecar(&loc).filter(|t| t.info.imdb_id == id && t.info.imdb_rating.is_none()) {
                (tj.info.imdb_rating, tj.info.imdb_votes) = (Some(r), Some(v));
                write_text_sidecars(&loc, &tj);
            }
            rq.q.lock().unwrap().done.insert(key, (r, v));
        }
    }
}

/// `data_dir/imdb/title.ratings.tsv.gz`, refreshed at most once per `RATINGS_MAX_AGE`.
/// A stale copy is used when the refresh fails.
async fn ensure_ratings(http: &reqwest::Client, url: &str, dd: &Path) -> Option<PathBuf> {
    let path = dd.join("imdb").join("title.ratings.tsv.gz");
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    if mtime.and_then(|t| t.elapsed().ok()).is_some_and(|a| a.as_secs() < RATINGS_MAX_AGE) {
        return Some(path);
    }
    match download_ratings(http, url, &path, mtime).await {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("IMDb ratings download failed: {e}");
            mtime.map(|_| path)
        }
    }
}

async fn download_ratings(http: &reqwest::Client, url: &str, path: &Path, since: Option<std::time::SystemTime>) -> Result<(), String> {
    use tokio::io::AsyncWriteExt as _;
    let dir = path.parent().ok_or("no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut req = http.get(url).timeout(Duration::from_secs(300));
    if let Some(t) = since {
        req = req.header(reqwest::header::IF_MODIFIED_SINCE, http_date(t));
    }
    let mut resp = req.send().await.map_err(|e| e.to_string())?;
    match resp.status().as_u16() {
        304 => {
            // Unchanged: restart the 7-day clock.
            return std::fs::File::options()
                .append(true)
                .open(path)
                .and_then(|f| f.set_modified(std::time::SystemTime::now()))
                .map_err(|e| e.to_string());
        }
        s if !(200..300).contains(&s) => return Err(format!("HTTP {s} from {url}")),
        _ => {}
    }
    let tmp = dir.join(".title.ratings.tsv.gz.tmp");
    let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
    while let Some(c) = resp.chunk().await.map_err(|e| e.to_string())? {
        f.write_all(&c).await.map_err(|e| e.to_string())?;
    }
    f.flush().await.map_err(|e| e.to_string())?;
    drop(f);
    tokio::fs::rename(&tmp, path).await.map_err(|e| e.to_string())
}

/// Stream-decompress `tconst\taverageRating\tnumVotes` and keep only `ids`.
fn scan_ratings(r: impl std::io::Read, ids: &HashSet<String>) -> std::io::Result<HashMap<String, (f32, u32)>> {
    use std::io::BufRead;
    let mut out = HashMap::new();
    let mut rd = std::io::BufReader::with_capacity(1 << 16, flate2::read::GzDecoder::new(r));
    let mut line = String::new();
    while out.len() < ids.len() {
        line.clear();
        if rd.read_line(&mut line)? == 0 {
            break;
        }
        let mut it = line.trim_end().split('\t');
        let Some(id) = it.next().filter(|id| ids.contains(*id)) else { continue };
        let rating = it.next().and_then(|x| x.parse::<f32>().ok()).filter(|r| *r > 0.0);
        let votes = it.next().and_then(|x| x.parse::<u32>().ok());
        if let (Some(r), Some(v)) = (rating, votes) {
            out.insert(id.to_string(), (r, v));
        }
    }
    Ok(out)
}

/// RFC 7231 IMF-fixdate ("Sun, 06 Nov 1994 08:49:37 GMT").
fn http_date(t: std::time::SystemTime) -> String {
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // Civil-from-days (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{}, {d:02} {} {y} {:02}:{:02}:{:02} GMT",
        WD[(days % 7) as usize],
        MON[(m - 1) as usize],
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

// ---------------------------------------------------------------- grouping (offline)

/// Offline name parse for instant grouping (no network).
pub fn parsed_name(name: &str) -> ParsedName {
    let p = nameparse::parse(name);
    ParsedName {
        norm: nameparse::norm_title(&p.title),
        is_tv: p.is_tv(),
        title: p.title,
        year: p.year,
        season: p.season,
        episode: p.episode,
    }
}

/// Episode files of a torrent: non-extra videos whose name (or "folder file") parses to an
/// episode number. Season falls back to the folder, then the torrent name. Sorted by S/E.
pub fn episodes_of(torrent_name: &str, files: &[FileRow]) -> Vec<EpisodeFile> {
    let tp = nameparse::parse(torrent_name);
    let mut out: Vec<EpisodeFile> = files
        .iter()
        .filter(|f| f.kind == FileKind::Video && !nameparse::is_extra(&f.name))
        .filter_map(|f| {
            let mut parts: Vec<&str> = f.name.split('/').collect();
            let base = parts.pop()?;
            let stem = base.rsplit_once('.').map_or(base, |(a, _)| a);
            let dir_name = parts.pop();
            let dir = dir_name.map(nameparse::parse);
            let mut p = nameparse::parse(stem);
            if p.episode.is_none() {
                if let Some(d) = dir_name {
                    let q = nameparse::parse(&format!("{d} {stem}"));
                    if q.episode.is_some() {
                        p = q;
                    }
                }
            }
            let episode = p.episode?;
            let season = p.season.or(dir.and_then(|d| d.season)).or(tp.season);
            let title = if p.title.trim().is_empty() || p.title.eq_ignore_ascii_case("episode") { tp.title.clone() } else { p.title };
            Some(EpisodeFile {
                file_index: f.index,
                season,
                episode,
                norm: nameparse::norm_title(&title),
                title_hint: title,
                size: f.size,
                progress: f.progress,
            })
        })
        .collect();
    out.sort_by_key(|e| (e.season.unwrap_or(u32::MAX), e.episode, e.file_index));
    out
}

#[tauri::command]
pub fn parse_names(names: Vec<String>) -> Vec<ParsedName> {
    names.iter().map(|n| parsed_name(n)).collect()
}

#[tauri::command]
pub fn list_episodes(state: St<'_>, key: String) -> Result<Vec<EpisodeFile>, String> {
    let key = norm_key(&key);
    let name = engine::list_rows(&state)
        .into_iter()
        .find(|r| r.key == key)
        .map(|r| r.name)
        .ok_or_else(|| format!("NOT_FOUND: no torrent {key}"))?;
    Ok(episodes_of(&name, &engine::files(&state, &key)?))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const SEARCH_MATRIX: &str = r#"{"query":"The Matrix","metas":[
      {"id":"tt0133093","imdb_id":"tt0133093","type":"movie","name":"The Matrix","releaseInfo":"1999","poster":"https://m.media-amazon.com/x.jpg"},
      {"id":"tt0234215","imdb_id":"tt0234215","type":"movie","name":"The Matrix Reloaded","releaseInfo":"2003"},
      {"id":"tt10838180","imdb_id":"tt10838180","type":"movie","name":"The Matrix Resurrections","releaseInfo":"2021"},
      {"id":"bad","type":"movie","name":"No id"}]}"#;
    const SEARCH_OFFICE: &str = r#"{"metas":[
      {"id":"tt0386676","type":"series","name":"The Office","releaseInfo":"2005-2013"},
      {"id":"tt0290978","type":"series","name":"The Office","releaseInfo":"2001-2003"},
      {"id":"tt0840149","type":"series","name":"The Office: The Accountants","releaseInfo":"2006"}]}"#;
    const SEARCH_JUNK: &str = r#"{"metas":[
      {"id":"tt2245084","type":"movie","name":"Big Hero 6","releaseInfo":"2014"},
      {"id":"tt0319061","type":"movie","name":"Big Fish","releaseInfo":"2003"}]}"#;

    fn meta_matrix(base: &str) -> String {
        format!(
            r#"{{"meta":{{"id":"tt0133093","imdb_id":"tt0133093","type":"movie","name":"The Matrix","year":"1999",
            "releaseInfo":"1999","imdbRating":"8.7","genres":["Action","Sci-Fi"],"runtime":"136 min",
            "description":"A hacker learns the truth.","poster":"{base}/poster/small/tt0133093/img",
            "background":"{base}/background/medium/tt0133093/img","country":"United States"}}}}"#
        )
    }
    const META_BB: &str = r#"{"meta":{"id":"tt0903747","imdb_id":"tt0903747","type":"series","name":"Breaking Bad",
      "releaseInfo":"2008–2013","year":"2008–2013","imdbRating":"9.5","genres":["Crime","Drama"],"runtime":"49 min",
      "description":"A teacher turns to crime.","videos":[
        {"season":5,"episode":13,"name":"To'hajiilee","released":"2013-09-08T05:00:00.000Z"},
        {"season":5,"episode":14,"name":"Ozymandias","overview":"Everyone copes.","released":"2013-09-15T05:00:00.000Z"},
        {"season":0,"episode":1,"name":"Special"},
        {"season":1,"episode":1,"name":"Pilot","released":"2008-01-20T05:00:00.000Z"},
        {"season":1,"episode":2,"name":"Cat's in the Bag...","released":"2008-01-27T05:00:00.000Z"}]}}"#;

    #[test]
    fn mappers() {
        let c = cands_from_search(&serde_json::from_str(SEARCH_MATRIX).unwrap(), MediaType::Movie);
        assert_eq!(c.len(), 3, "entry without a tt id is dropped");
        assert_eq!(c[0].year, Some(1999));
        let c = cands_from_search(&serde_json::from_str(SEARCH_OFFICE).unwrap(), MediaType::Tv);
        assert_eq!((c[1].year, c[1].release_info.as_deref()), (Some(2001), Some("2001-2003")));

        let m: Value = serde_json::from_str::<Value>(&meta_matrix("https://images.metahub.space")).unwrap()["meta"].clone();
        let i = info_from_meta(MediaType::Movie, &m, &Parsed::default());
        assert_eq!((i.title.as_str(), i.year, i.runtime_min, i.imdb_rating), ("The Matrix", Some(1999), Some(136), Some(8.7)));
        assert_eq!(i.genres, vec!["Action", "Sci-Fi"]);
        assert_eq!(i.imdb_url.as_deref(), Some("https://www.imdb.com/title/tt0133093/"));
        let (p, b) = image_urls(&m, "tt0133093", METAHUB);
        assert_eq!(p.as_deref(), Some("https://images.metahub.space/poster/medium/tt0133093/img"));
        assert!(b.unwrap().contains("/background/medium/"));

        let m: Value = serde_json::from_str::<Value>(META_BB).unwrap()["meta"].clone();
        let p = nameparse::parse("Breaking.Bad.S05E14.720p");
        let i = info_from_meta(MediaType::Tv, &m, &p);
        assert_eq!((i.year, i.season, i.episode), (Some(2008), Some(5), Some(14)));
        assert_eq!(i.episode_title.as_deref(), Some("Ozymandias"));
        assert_eq!(i.episode_overview.as_deref(), Some("Everyone copes."));
        // Absolute numbering (anime-style): E2 -> S1E2.
        let i = info_from_meta(MediaType::Tv, &m, &nameparse::parse("[Grp] Breaking Bad - 02 [1080p]"));
        assert_eq!(i.episode_title.as_deref(), Some("Cat's in the Bag..."));
        assert_eq!(i.season, Some(1));
        // Daily-show style air date.
        let i = info_from_meta(MediaType::Tv, &m, &nameparse::parse("Breaking.Bad.2013.09.08.720p"));
        assert_eq!(i.episode_title.as_deref(), Some("To'hajiilee"));
        // Missing fields -> None.
        let i = info_from_meta(MediaType::Movie, &serde_json::json!({"id":"tt1","name":"X","imdbRating":"0"}), &Parsed::default());
        assert_eq!((i.imdb_rating, i.poster_url, i.runtime_min), (None, None, None));
        assert_eq!(runtime_of("2h 16min"), Some(136));
        assert_eq!(find_tt("see https://www.imdb.com/title/tt0133093/ ok"), Some("tt0133093".into()));
        assert_eq!(find_tt("tt123"), None);
    }

    #[test]
    fn selection() {
        let cands = cands_from_search(&serde_json::from_str(SEARCH_MATRIX).unwrap(), MediaType::Movie);
        let (i, c) = evaluate(&cands, &["The Matrix".into()], Some(1999), false).unwrap();
        assert_eq!((i, c.accepted), (0, true));
        // Year picks the right remake even when it's not first.
        let lk = vec![
            Cand { id: "tt6105098".into(), kind: MediaType::Movie, name: "The Lion King".into(), year: Some(2019), release_info: None, poster: None },
            Cand { id: "tt0110357".into(), kind: MediaType::Movie, name: "The Lion King".into(), year: Some(1994), release_info: None, poster: None },
        ];
        let (i, c) = evaluate(&lk, &["The Lion King".into()], Some(1994), false).unwrap();
        assert_eq!((i, c.accepted), (1, true));
        // No year + two strong candidates -> needs_match.
        let (_, c) = evaluate(&lk, &["The Lion King".into()], None, false).unwrap();
        assert!(!c.accepted && c.ambiguous);
        // Fuzzy junk results are never accepted.
        let junk = cands_from_search(&serde_json::from_str(SEARCH_JUNK).unwrap(), MediaType::Movie);
        let (_, c) = evaluate(&junk, &["Big Buck Bunny".into()], Some(2008), false).unwrap();
        assert!(!c.accepted);
    }

    // ------------------------------------------------ mock HTTP server

    struct Mock {
        base: String,
        hits: Arc<Mutex<Vec<String>>>,
    }

    /// Answers by path with canned JSON; 404 otherwise. Records request lines + UA.
    async fn mock_server(routes: Vec<(String, Vec<u8>)>) -> Mock {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let h2 = hits.clone();
        let base2 = base.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { break };
                let routes = routes.clone();
                let hits = h2.clone();
                let base = base2.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let Ok(n) = s.read(&mut tmp).await else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let req = String::from_utf8_lossy(&buf).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    let ua = req
                        .lines()
                        .find(|l| l.to_lowercase().starts_with("user-agent:"))
                        .unwrap_or("")
                        .to_string();
                    hits.lock().unwrap().push(format!("{path} | {ua}"));
                    // A body "REDIRECT:<url>" answers 307 to <url> ("{BASE}" expanded).
                    let redirect = routes.iter().find(|(p, _)| *p == path).and_then(|(_, b)| b.strip_prefix(&b"REDIRECT:"[..]));
                    if let Some(to) = redirect {
                        let to = String::from_utf8_lossy(to).replace("{BASE}", &base);
                        let head = format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        let _ = s.write_all(head.as_bytes()).await;
                        return;
                    }
                    let (status, body) = match routes.iter().find(|(p, _)| *p == path) {
                        // "{BASE}" in a body becomes this server's own URL.
                        Some((_, b)) => ("200 OK", match std::str::from_utf8(b) {
                            Ok(t) => t.replace("{BASE}", &base).into_bytes(),
                            Err(_) => b.clone(),
                        }),
                        None => ("404 Not Found", b"Cannot GET".to_vec()),
                    };
                    let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                });
            }
        });
        Mock { base, hits }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!("tide-meta-test-{}-{tag}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn frow(index: usize, name: &str, size: u64, kind: FileKind) -> FileRow {
        FileRow { index, name: name.into(), size, progress_bytes: size, progress: 1.0, kind, playable_builtin: false }
    }

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 2, 3];

    fn http() -> reqwest::Client {
        MetaState::default().http()
    }

    #[tokio::test]
    async fn end_to_end_movie_with_nfo_and_sidecars() {
        let dd = tmpdir("dd");
        let dl = tmpdir("dl");
        let folder = dl.join("The.Matrix.1999.1080p.BluRay.x264-GRP");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("info.nfo"), "Movie: https://www.imdb.com/title/tt0133093/").unwrap();
        // A torrent-owned poster.jpg must never be overwritten.
        std::fs::write(folder.join("poster.jpg"), b"torrent-owned").unwrap();

        let mock = mock_server(vec![
            ("/meta/movie/tt0133093.json".to_string(), meta_matrix("{BASE}").into_bytes()),
            ("/poster/medium/tt0133093/img".to_string(), JPEG.to_vec()),
            ("/background/medium/tt0133093/img".to_string(), JPEG.to_vec()),
        ])
        .await;
        let cm = Cinemeta::new(&mock.base, http());
        let files = vec![
            frow(0, "The.Matrix.1999.1080p.BluRay.x264-GRP.mkv", 2_000_000_000, FileKind::Video),
            frow(1, "info.nfo", 60, FileKind::Other),
            frow(2, "poster.jpg", 13, FileKind::Other),
            frow(3, "Sample/sample.mkv", 60_000_000, FileKind::Video),
        ];
        let loc = Loc::from_parts(&folder, files, "The.Matrix.1999.1080p.BluRay.x264-GRP", false);
        let meta = MetaState::default();
        let key = "a".repeat(40);
        let info = lookup_loc(&meta, &dd, &cm, None, &key, &loc, 1000).await.unwrap().unwrap();
        assert_eq!((info.imdb_id.as_str(), info.imdb_rating), ("tt0133093", Some(8.7)));
        assert!(info.match_note.unwrap().contains(".nfo"));
        assert_eq!(std::fs::read(folder.join("poster.jpg")).unwrap(), b"torrent-owned");
        assert_eq!(std::fs::read(folder.join("fanart.jpg")).unwrap(), JPEG);
        let nfo = std::fs::read_to_string(folder.join("movie.nfo")).unwrap();
        assert!(nfo.contains("<uniqueid type=\"imdb\" default=\"true\">tt0133093</uniqueid>"), "{nfo}");
        assert!(nfo.contains("<value>8.7</value>"));
        let tj: TideJson = serde_json::from_slice(&std::fs::read(folder.join("tide.json")).unwrap()).unwrap();
        assert_eq!((tj.key.as_str(), tj.manual, tj.info.title.as_str()), (key.as_str(), false, "The Matrix"));
        assert!(!folder.join(".tide.json.tmp").exists());
        // Polite User-Agent.
        assert!(mock.hits.lock().unwrap().iter().all(|h| h.contains("Tide/0.1 (+https://github.com/musahinist/tide)")));

        // Sidecar paths: ours only (not the torrent's poster.jpg).
        let sp = sidecar_paths_loc(&loc);
        let names: HashSet<String> = sp.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["tide.json", "fanart.jpg", "movie.nfo"].iter().map(|s| s.to_string()).collect());

        // A fresh process: served from the sidecar without any request.
        let n = mock.hits.lock().unwrap().len();
        let meta2 = MetaState::default();
        let again = lookup_loc(&meta2, &dd, &cm, None, &key, &loc, 2000).await.unwrap().unwrap();
        assert_eq!(again.title, "The Matrix");
        assert_eq!(mock.hits.lock().unwrap().len(), n);
        let _ = std::fs::remove_dir_all(&dd);
        let _ = std::fs::remove_dir_all(&dl);
    }

    #[tokio::test]
    async fn search_ladder_single_file_tv_and_switch() {
        let dd = tmpdir("dd");
        let dl = tmpdir("dl");
        let file = dl.join("Breaking.Bad.S05E14.720p.HDTV.x264-CTU.mkv");
        std::fs::write(&file, b"x").unwrap();
        let routes = vec![
            ("/catalog/series/top/search=Breaking%20Bad.json".to_string(),
             br#"{"metas":[{"id":"tt0903747","name":"Breaking Bad","releaseInfo":"2008-2013"},{"id":"tt1","name":"Breaking Point","releaseInfo":"2010"}]}"#.to_vec()),
            ("/meta/series/tt0903747.json".to_string(), META_BB.as_bytes().to_vec()),
            ("/meta/movie/tt0133093.json".to_string(), meta_matrix("http://127.0.0.1:9").into_bytes()),
        ];
        let mock = mock_server(routes).await;
        let cm = Cinemeta::new(&mock.base, http());
        let files = vec![frow(0, "Breaking.Bad.S05E14.720p.HDTV.x264-CTU.mkv", 900_000_000, FileKind::Video)];
        let loc = Loc::from_parts(&file, files, "Breaking.Bad.S05E14.720p.HDTV.x264-CTU.mkv", false);
        let meta = MetaState::default();
        let key = "b".repeat(40);
        let info = lookup_loc(&meta, &dd, &cm, None, &key, &loc, 1000).await.unwrap().unwrap();
        assert_eq!((info.media_type, info.episode_title.as_deref()), (MediaType::Tv, Some("Ozymandias")));
        let stem = "Breaking.Bad.S05E14.720p.HDTV.x264-CTU";
        assert!(dl.join(format!("{stem}.tide.json")).is_file());
        let nfo = std::fs::read_to_string(dl.join(format!("{stem}.nfo"))).unwrap();
        assert!(nfo.contains("<episodedetails>") && nfo.contains("<showtitle>Breaking Bad</showtitle>"), "{nfo}");
        // No image URLs in this meta -> poster via metahub fallback fails (404) -> no image file.
        assert!(!dl.join(format!("{stem}-poster.jpg")).exists());

        // Manual fix to a movie rewrites the sidecars (manual=true, movie nfo).
        let i2 = apply_match(&meta, &dd, &cm, &key, &loc, MediaType::Movie, "tt0133093", true, None).await.unwrap();
        assert!(i2.manual);
        let nfo = std::fs::read_to_string(dl.join(format!("{stem}.nfo"))).unwrap();
        assert!(nfo.contains("<movie>"));
        let tj: TideJson = serde_json::from_slice(&std::fs::read(dl.join(format!("{stem}.tide.json"))).unwrap()).unwrap();
        assert!(tj.manual && tj.info.imdb_id == "tt0133093");

        // Multi-file: movie -> tv switch removes movie.nfo.
        let folder = dl.join("Pack");
        std::fs::create_dir_all(&folder).unwrap();
        let loc2 = Loc::from_parts(&folder, vec![frow(0, "a.mkv", 1, FileKind::Video), frow(1, "b.mkv", 1, FileKind::Video)], "Pack", false);
        let mut info = i2.clone();
        write_sidecars(&loc2, &key, &info, None, None);
        assert!(folder.join("movie.nfo").exists());
        info.media_type = MediaType::Tv;
        info.title = "Tom & Jerry <3>".into();
        write_sidecars(&loc2, &key, &info, None, None);
        assert!(!folder.join("movie.nfo").exists());
        let x = std::fs::read_to_string(folder.join("tvshow.nfo")).unwrap();
        assert!(x.contains("<title>Tom &amp; Jerry &lt;3&gt;</title>"), "{x}");
        let _ = std::fs::remove_dir_all(&dd);
        let _ = std::fs::remove_dir_all(&dl);
    }

    #[tokio::test]
    async fn needs_match_and_negative_cache() {
        let dd = tmpdir("dd");
        let dl = tmpdir("dl");
        let routes = vec![
            ("/catalog/series/top/search=The%20Office%20US.json".to_string(), br#"{"metas":[]}"#.to_vec()),
            ("/catalog/movie/top/search=The%20Office%20US.json".to_string(), br#"{"metas":[]}"#.to_vec()),
            ("/catalog/series/top/search=The%20Office.json".to_string(), SEARCH_OFFICE.as_bytes().to_vec()),
            ("/catalog/movie/top/search=The%20Office.json".to_string(), br#"{"metas":[]}"#.to_vec()),
            ("/meta/series/tt0386676.json".to_string(), br#"{"meta":{"id":"tt0386676","name":"The Office","country":"United States"}}"#.to_vec()),
            ("/meta/series/tt0290978.json".to_string(), br#"{"meta":{"id":"tt0290978","name":"The Office","country":"United Kingdom"}}"#.to_vec()),
            ("/catalog/movie/top/search=Big%20Buck%20Bunny.json".to_string(), SEARCH_JUNK.as_bytes().to_vec()),
            ("/catalog/series/top/search=Big%20Buck%20Bunny.json".to_string(), br#"{"metas":[]}"#.to_vec()),
            ("/catalog/movie/top/search=Big%20Buck.json".to_string(), br#"{"metas":[]}"#.to_vec()),
            ("/catalog/series/top/search=Big%20Buck.json".to_string(), br#"{"metas":[]}"#.to_vec()),
        ];
        let mock = mock_server(routes).await;
        let cm = Cinemeta::new(&mock.base, http());

        // Country hint breaks the US/UK tie.
        let p = vec![nameparse::parse("The Office US S05 1080p")];
        match auto_match(&cm, &p).await.unwrap() {
            Outcome::Matched { id, note, .. } => assert_eq!(id, "tt0386676", "{note}"),
            o => panic!("{o:?}"),
        }
        // Same without a hint: ambiguous -> needs_match.
        let p = vec![nameparse::parse("The.Office.S05.1080p")];
        assert!(matches!(auto_match(&cm, &p).await.unwrap(), Outcome::Needs { .. }));

        // Fuzzy junk results -> needs_match, recorded for 24 h without re-querying.
        let folder = dl.join("Big Buck Bunny (2008) 1080p");
        std::fs::create_dir_all(&folder).unwrap();
        let loc = Loc::from_parts(&folder, vec![frow(0, "Big Buck Bunny.mp4", 400_000_000, FileKind::Video), frow(1, "x.srt", 1, FileKind::Subtitle)], "Big Buck Bunny (2008) 1080p", false);
        let meta = MetaState::default();
        let key = "c".repeat(40);
        let i = lookup_loc(&meta, &dd, &cm, None, &key, &loc, 1000).await.unwrap().unwrap();
        assert!(i.needs_match && i.title == "Big Buck Bunny" && i.year == Some(2008), "{i:?}");
        assert!(!folder.join("tide.json").exists(), "no sidecars for needs_match");
        let n = mock.hits.lock().unwrap().len();
        let i = lookup_loc(&MetaState::default(), &dd, &cm, None, &key, &loc, 1000 + 3600).await.unwrap().unwrap();
        assert!(i.needs_match);
        assert_eq!(mock.hits.lock().unwrap().len(), n, "cached in metadata.json");

        // A real miss (zero results anywhere) -> None; retried after 24 h only.
        let folder2 = dl.join("Zzqx Nothing 2011");
        std::fs::create_dir_all(&folder2).unwrap();
        let loc2 = Loc::from_parts(&folder2, vec![frow(0, "a.mkv", 400_000_000, FileKind::Video), frow(1, "b.srt", 1, FileKind::Subtitle)], "Zzqx Nothing 2011", false);
        let key2 = "d".repeat(40);
        assert_eq!(lookup_loc(&meta, &dd, &cm, None, &key2, &loc2, 1000).await.unwrap(), None);
        let n = mock.hits.lock().unwrap().len();
        assert_eq!(lookup_loc(&MetaState::default(), &dd, &cm, None, &key2, &loc2, 1000 + 23 * 3600).await.unwrap(), None);
        assert_eq!(mock.hits.lock().unwrap().len(), n);
        assert_eq!(lookup_loc(&MetaState::default(), &dd, &cm, None, &key2, &loc2, 1000 + 25 * 3600).await.unwrap(), None);
        assert!(mock.hits.lock().unwrap().len() > n, "retried after 24 h");

        // Network errors are never recorded as misses.
        let dead = Cinemeta::new("http://127.0.0.1:9", http());
        let key3 = "e".repeat(40);
        assert!(lookup_loc(&MetaState::default(), &dd, &dead, None, &key3, &loc2, 1000).await.unwrap_err().starts_with("INTERNAL"));
        let ix: IndexFile = serde_json::from_slice(&std::fs::read(dd.join("metadata.json")).unwrap()).unwrap();
        assert!(!ix.misses.contains_key(&key3) && ix.misses.contains_key(&key2) && ix.needs.contains_key(&key));

        // Not worth a lookup: no video / under 50 MB / only samples.
        let small = Loc::from_parts(&folder2, vec![frow(0, "a.mkv", 10_000_000, FileKind::Video), frow(1, "b.txt", 1, FileKind::Other)], "x", false);
        assert_eq!(lookup_loc(&MetaState::default(), &dd, &dead, None, &key3, &small, 1).await.unwrap(), None);
        let samples = Loc::from_parts(&folder2, vec![frow(0, "Sample/a.mkv", 900_000_000, FileKind::Video), frow(1, "b.mp3", 9_000_000, FileKind::Audio)], "x", false);
        assert_eq!(lookup_loc(&MetaState::default(), &dd, &dead, None, &key3, &samples, 1).await.unwrap(), None);
        // Files not there yet -> NOT_READY.
        let gone = Loc::from_parts(&dl.join("nope"), loc2.files.clone(), "Zzqx Nothing 2011", false);
        assert!(lookup_loc(&MetaState::default(), &dd, &dead, None, &key3, &gone, 1).await.unwrap_err().starts_with("NOT_READY"));
        let _ = std::fs::remove_dir_all(&dd);
        let _ = std::fs::remove_dir_all(&dl);
    }

    /// Live check against the real Cinemeta (not run by default):
    /// `cargo test live_cinemeta -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_cinemeta() {
        let cm = Cinemeta::new(CINEMETA, http());
        for (name, want) in [
            ("The.Matrix.1999.1080p.BluRay.x264-GRP", Some("tt0133093")),
            ("Breaking.Bad.S03E07.720p.HDTV.x264-CTU", Some("tt0903747")),
            ("[YTS.MX] Inception (2010) [1080p]", Some("tt1375666")),
            ("Blade.Runner.2049.2017.1080p", Some("tt1856101")),
            ("The.Office.S05.1080p", None),
            ("Big Buck Bunny (2008) 1080p", None),
        ] {
            // Cinemeta answers uncached searches slowly and sometimes 5xx: retry a few times.
            let mut o = auto_match(&cm, &[nameparse::parse(name)]).await;
            for _ in 0..3 {
                if o.is_ok() {
                    break;
                }
                eprintln!("{name}: transient {o:?}; retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
                o = auto_match(&cm, &[nameparse::parse(name)]).await;
            }
            let o = o.unwrap();
            eprintln!("{name:50} -> {o:?}");
            match (want, &o) {
                (Some(w), Outcome::Matched { id, .. }) => assert_eq!(id, w),
                (None, Outcome::Matched { .. }) => panic!("{name}: unexpected auto-match {o:?}"),
                (Some(_), _) => panic!("{name}: expected a match, got {o:?}"),
                _ => {}
            }
        }
        let m = cm.meta(MediaType::Movie, "tt0133093").await.unwrap();
        let (p, b) = image_urls(&m, "tt0133093", &cm.images);
        let pj = to_jpeg(cm.image(&p.unwrap()).await.unwrap(), 750, 1_000_000).await.unwrap();
        let bj = to_jpeg(cm.image(&b.unwrap()).await.unwrap(), 1280, 400_000).await.unwrap();
        eprintln!("poster {} B, fanart {} B (JPEG)", pj.len(), bj.len());
        assert_eq!(image_mime(&pj), Some("image/jpeg"));
        assert!(bj.len() < 700_000);
    }

    #[test]
    fn ratings_scan_and_http_date() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"tconst\taverageRating\tnumVotes\ntt0000001\t5.7\t2000\ntt31049299\t5.7\t15428\ntt17491088\t8.0\t102371\n").unwrap();
        let bytes = gz.finish().unwrap();
        let ids: HashSet<String> = ["tt31049299".to_string(), "tt9999999".to_string()].into();
        let f = scan_ratings(&bytes[..], &ids).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f["tt31049299"], (5.7, 15428));
        let t = std::time::UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(http_date(t), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(http_date(std::time::UNIX_EPOCH + Duration::from_secs(951_782_400)), "Tue, 29 Feb 2000 00:00:00 GMT");
    }

    #[tokio::test]
    async fn rating_fallback_patches_old_sidecar() {
        use std::io::Write;
        let dd = tmpdir("dd");
        let dl = tmpdir("dl");
        let folder = dl.join("Animals.2026.1080p");
        std::fs::create_dir_all(&folder).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"tconst\taverageRating\tnumVotes\ntt31049299\t5.7\t15428\n").unwrap();
        let mock = mock_server(vec![("/title.ratings.tsv.gz".to_string(), gz.finish().unwrap())]).await;
        let cm = Cinemeta::new(&mock.base, http());
        let files = vec![frow(0, "Animals.2026.1080p/a.mkv", 900_000_000, FileKind::Video), frow(1, "Animals.2026.1080p/b.srt", 1, FileKind::Other)];
        let loc = Loc::from_parts(&folder, files, "Animals.2026.1080p", false);
        let key = "c".repeat(40);
        // An old sidecar: no rating, no imdb_votes field at all.
        let old = format!(r#"{{"tide":1,"key":"{key}","info":{{"imdb_id":"tt31049299","media_type":"movie","title":"Animals","imdb_rating":null}}}}"#);
        std::fs::write(folder.join("tide.json"), old).unwrap();
        let meta = MetaState::default();
        let i = lookup_loc(&meta, &dd, &cm, None, &key, &loc, 1000).await.unwrap().unwrap();
        assert_eq!((i.imdb_rating, i.imdb_votes), (None, None), "request path does not wait");
        let mut got = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let i = lookup_loc(&meta, &dd, &cm, None, &key, &loc, 1000).await.unwrap().unwrap();
            if i.imdb_rating.is_some() {
                got = Some(i);
                break;
            }
        }
        let i = got.expect("rating filled in");
        assert_eq!((i.imdb_rating, i.imdb_votes), (Some(5.7), Some(15428)));
        let tj: TideJson = serde_json::from_slice(&std::fs::read(folder.join("tide.json")).unwrap()).unwrap();
        assert_eq!((tj.info.imdb_rating, tj.info.imdb_votes, tj.key.as_str()), (Some(5.7), Some(15428), key.as_str()));
        let nfo = std::fs::read_to_string(folder.join("movie.nfo")).unwrap();
        assert!(nfo.contains("<value>5.7</value><votes>15428</votes>"), "{nfo}");
        assert!(dd.join("imdb/title.ratings.tsv.gz").is_file());
        // Fresh file (< 7 days): no second download.
        let n = mock.hits.lock().unwrap().iter().filter(|h| h.starts_with("/title.ratings")).count();
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(&dd);
        let _ = std::fs::remove_dir_all(&dl);
    }

    #[tokio::test]
    async fn same_name_series_tie_broken_by_episode() {
        let search = br#"{"metas":[
            {"id":"tt13944906","name":"The Diplomat","releaseInfo":"2019","type":"series"},
            {"id":"tt17006090","name":"The Diplomat","releaseInfo":"2015","type":"series"},
            {"id":"tt17491088","name":"The Diplomat","releaseInfo":"2023-","type":"series"}]}"#.to_vec();
        let meta = |id: &str, vids: &str| {
            format!(r#"{{"meta":{{"id":"{id}","type":"series","name":"The Diplomat","country":"United States","videos":[{vids}]}}}}"#).into_bytes()
        };
        let mut routes = vec![
            ("/meta/series/tt17491088.json".to_string(), meta("tt17491088", r#"{"season":4,"episode":6},{"season":4,"episode":7}"#)),
            ("/meta/series/tt13944906.json".to_string(), meta("tt13944906", r#"{"season":1,"episode":1},{"season":1,"episode":7}"#)),
            ("/meta/series/tt17006090.json".to_string(), meta("tt17006090", r#"{"season":1,"episode":1}"#)),
        ];
        for q in ["The%20Diplomat", "The%20Diplomat%20US", "the%20diplomat", "the%20diplomat%20us"] {
            routes.push((format!("/catalog/series/top/search={q}.json"), search.clone()));
        }
        let mock = mock_server(routes).await;
        let cm = Cinemeta::new(&mock.base, http());
        let p = nameparse::parse("the.diplomat.us.s04e07.1080p.web.h264.mkv");
        match auto_match(&cm, &[p.clone()]).await.unwrap() {
            Outcome::Matched { kind, id, note } => {
                assert_eq!((kind, id.as_str()), (MediaType::Tv, "tt17491088"), "{note}");
                assert!(note.contains("S04E07"), "{note}");
            }
            o => panic!("expected a match for {p:?}, got {o:?}; hits {:?}", mock.hits.lock().unwrap()),
        }
        // Two candidates with the episode -> still needs_match (only one may win).
        let mut p2 = p.clone();
        (p2.season, p2.episode, p2.country) = (Some(1), Some(1), None);
        let o = auto_match(&cm, &[p2]).await.unwrap();
        assert!(matches!(o, Outcome::Needs { .. } | Outcome::NoMatch), "{o:?}");
    }

    #[tokio::test]
    async fn heal_missing_images_follows_307() {
        let dd = tmpdir("dd");
        let dl = tmpdir("dl");
        let folder = dl.join("Animals.2026.1080p.WEBRip.x265-GRP");
        std::fs::create_dir_all(&folder).unwrap();
        let m = mock_server(vec![
            ("/poster/medium/tt31049299/img".to_string(), b"REDIRECT:{BASE}/live/poster".to_vec()),
            ("/live/poster".to_string(), JPEG.to_vec()),
            ("/background/medium/tt31049299/img".to_string(), b"REDIRECT:{BASE}/live/bg".to_vec()),
            ("/live/bg".to_string(), JPEG.to_vec()),
        ])
        .await;
        let cm = Cinemeta::new(&m.base, http());
        // 307 -> 200 is followed.
        assert_eq!(cm.image(&format!("{}/poster/medium/tt31049299/img", m.base)).await.unwrap(), JPEG);

        let files = vec![
            frow(0, "a.mkv", 2_000_000_000, FileKind::Video),
            frow(1, "a.srt", 1_000, FileKind::Other),
        ];
        let loc = Loc::from_parts(&folder, files, "Animals.2026.1080p.WEBRip.x265-GRP", false);
        let info = MediaInfo { imdb_id: "tt31049299".into(), title: "Animals".into(), year: Some(2026), ..Default::default() };
        let ms = MetaState::default();
        // No tide.json yet: nothing to heal.
        assert!(!heal_images(&ms, &dd, &cm, "k", &loc, &info, 1000).await);
        write_sidecars(&loc, "k", &info, None, None);
        assert!(folder.join("tide.json").exists() && !folder.join("poster.jpg").exists());
        let before = m.hits.lock().unwrap().len();
        assert!(heal_images(&ms, &dd, &cm, "k", &loc, &info, 1000).await);
        assert_eq!(std::fs::read(folder.join("poster.jpg")).unwrap(), JPEG);
        assert_eq!(std::fs::read(folder.join("fanart.jpg")).unwrap(), JPEG);
        // Images only: no Cinemeta meta request.
        assert!(m.hits.lock().unwrap()[before..].iter().all(|h| !h.contains("/meta/")));
        // At most once per run per key.
        std::fs::remove_file(folder.join("poster.jpg")).unwrap();
        assert!(!heal_images(&ms, &dd, &cm, "k", &loc, &info, 1000).await);

        // Failure (404) -> 24 h backoff persisted in metadata.json, honoured by a new run.
        let info2 = MediaInfo { imdb_id: "tt0000001".into(), ..info.clone() };
        let ms2 = MetaState::default();
        assert!(!heal_images(&ms2, &dd, &cm, "k2", &loc, &info2, 1000).await);
        assert!(ms2.with(&dd, |g| g.index.img_fail.contains_key("k2")));
        let ms3 = MetaState::default();
        assert!(!heal_images(&ms3, &dd, &cm, "k2", &loc, &info2, 2000).await);
        assert!(!ms3.healed.lock().unwrap().contains("k2"));
        // After the backoff it retries.
        assert!(!heal_images(&ms3, &dd, &cm, "k2", &loc, &info2, 1000 + MISS_TTL).await);
        assert!(ms3.healed.lock().unwrap().contains("k2"));
    }

    #[test]
    fn pack_episodes() {
        let v = |i: usize, n: &str| frow(i, n, 1_000_000_000, FileKind::Video);
        let files = vec![
            v(0, "Northern.Lights.S02.1080p/Northern.Lights.S02E02.1080p.WEB-DL.mkv"),
            v(1, "Northern.Lights.S02.1080p/Northern.Lights.S02E01.1080p.WEB-DL.mkv"),
            frow(2, "Northern.Lights.S02.1080p/Northern.Lights.S02E01.en.srt", 40_000, FileKind::Subtitle),
            v(3, "Northern.Lights.S02.1080p/Sample/northern.lights.s02e01.sample.mkv"),
            v(4, "Northern.Lights.S02.1080p/Northern.Lights.S02E03.1080p.WEB-DL.mkv"),
            v(5, "Northern.Lights.S02.1080p/Featurettes/Making.Of.mkv"),
        ];
        let eps = episodes_of("Northern.Lights.S02.1080p.WEB-DL", &files);
        let se: Vec<_> = eps.iter().map(|e| (e.file_index, e.season, e.episode)).collect();
        assert_eq!(se, vec![(1, Some(2), 1), (0, Some(2), 2), (4, Some(2), 3)]);
        assert!(eps.iter().all(|e| e.norm == "northern lights" && e.title_hint == "Northern Lights"));

        // "Season 3/Episode 05.mkv": season from the folder, show from the torrent name.
        let files = vec![v(0, "Show Name/Season 3/Episode 05.mkv"), v(1, "Show Name/Season 3/Episode 06.mkv")];
        let eps = episodes_of("Show Name Season 3 Complete", &files);
        let se: Vec<_> = eps.iter().map(|e| (e.season, e.episode, e.norm.as_str())).collect();
        assert_eq!(se, vec![(Some(3), 5, "show name"), (Some(3), 6, "show name")]);

        // A movie with a sample and a single-episode torrent are not packs.
        let files = vec![v(0, "Movie.2010.1080p/Movie.2010.1080p.mkv"), v(1, "Movie.2010.1080p/Sample.mkv")];
        assert!(episodes_of("Movie.2010.1080p", &files).is_empty());
        let eps = episodes_of("Show.S01E04.720p", &[v(0, "Show.S01E04.720p.mkv")]);
        assert_eq!(eps.len(), 1);

        let p = parsed_name("The.Lighthouse.Keepers.S01E02.1080p.WEB-DL");
        assert_eq!((p.norm.as_str(), p.season, p.episode, p.is_tv), ("lighthouse keepers", Some(1), Some(2), true));
        let p = parsed_name("The Lighthouse Keepers S01 1080p");
        assert_eq!((p.norm.as_str(), p.season, p.episode, p.is_tv), ("lighthouse keepers", Some(1), None, true));
        assert!(!parsed_name("Sintel.2010.2160p.mkv").is_tv);
    }
}

// ---------------------------------------------------------------- accessors (subfetch)

/// Media info for `key` without any network: memory, else the `tide.json` sidecar.
pub fn offline_info(st: &AppState, key: &str) -> Option<MediaInfo> {
    let key = norm_key(key);
    if let Some(i) = st.meta.cached(&st.data_dir, &key) {
        return Some(i);
    }
    let tj = read_sidecar(&locate(st, &key).ok()?)?;
    let mut info = tj.info;
    info.manual = tj.manual;
    Some(info)
}

/// The shared metadata HTTP client (timeouts, user agent, https-only redirects).
pub fn http_client(st: &AppState) -> reqwest::Client {
    st.meta.http()
}
