//! New-episode watcher.
//!
//! Followed series = TV series (by IMDb id, from the `tide.json` sidecars metadata.rs
//! writes) with at least one episode in Tide (single-episode torrents or season-pack
//! files), minus the ones the user unfollowed. At most once per 24 h (plus a manual
//! "Check now") each followed series' Cinemeta meta is fetched and aired episodes after
//! the newest (season, episode) the user has are reported (specials ignored).
//!
//! Searching goes ONLY through a user-configured Torznab endpoint (Prowlarr or Jackett).
//! Without one the watcher just reports "new episode available".
//!
//! Persistence: `data_dir/watcher.json`
//! `{unfollowed:[tt…], last_check, settings:{torznab_url, api_key, auto_add, quality},
//!   seen:{"tt…:S:E": "dismissed"|"added"}, pending:[NewEpisode…]}`
//!
//! Commands (JS args camelCase, JSON snake_case, errors "CODE: msg"):
//! - `watcher_get_settings` -> WatcherInfo
//! - `watcher_set_settings {torznabUrl, apiKey, autoAdd, quality}` -> WatcherInfo
//!   (a `t=caps` request validates a changed URL/key)
//! - `watcher_check {force}` -> NewEpisode[]
//! - `watcher_follow {imdbId}`, `watcher_unfollow {imdbId}`, `watcher_dismiss {imdbId, season, episode}`
//!
//! Event `new-episodes` (NewEpisode[]) after a background check.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::engine;
use crate::metadata::{self, Cinemeta, NetErr};
use crate::nameparse;
use crate::state::AppState;
use crate::types::{FileRow, MediaInfo, MediaType, NewEpisode, TorrentState, TorznabResult, WatcherInfo, WatcherSettings};

type St<'a> = State<'a, Arc<AppState>>;

const CHECK_EVERY: u64 = 24 * 3600;
/// Report (and search) at most this many missing episodes per series.
const MAX_NEW_PER_SERIES: usize = 3;
const MAX_RESULTS: usize = 5;
const TORZNAB_TIMEOUT: Duration = Duration::from_secs(25);
const FIRST_DELAY: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_secs(3600);
const USER_AGENT: &str = "Tide/0.1 (+https://github.com/musahinist/tide)";
const QUALITIES: &[&str] = &["1080p", "720p", "any"];

fn invalid(m: impl std::fmt::Display) -> String {
    format!("INVALID_INPUT: {m}")
}
fn internal(m: impl std::fmt::Display) -> String {
    format!("INTERNAL: {m}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "YYYY-MM-DDTHH:MM:SS" (UTC): compares lexicographically with Cinemeta's `released`.
fn iso(secs: u64) -> String {
    let (y, m, d) = nameparse::civil_from_days((secs / 86400) as i64);
    let r = secs % 86400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", r / 3600, r / 60 % 60, r % 60)
}

// ---------------------------------------------------------------- persistence

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct WatcherFile {
    unfollowed: Vec<String>,
    last_check: u64,
    settings: WatcherSettings,
    seen: BTreeMap<String, String>,
    pending: Vec<NewEpisode>,
}

impl WatcherFile {
    fn info(&self) -> WatcherInfo {
        WatcherInfo { settings: self.settings.clone(), unfollowed: self.unfollowed.clone(), last_check: self.last_check }
    }
}

fn seen_key(imdb: &str, s: u32, e: u32) -> String {
    format!("{imdb}:{s}:{e}")
}

struct Store {
    path: PathBuf,
    file: WatcherFile,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);
/// Serialises checks (startup task vs. UI): a second caller sees the fresh `last_check`.
static CHECK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn load(path: &Path) -> WatcherFile {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
            tracing::warn!("{}: invalid JSON, starting empty: {e}", path.display());
            WatcherFile::default()
        }),
        Err(_) => WatcherFile::default(),
    }
}

fn save(path: &Path, f: &WatcherFile) -> Result<(), String> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(internal)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(f).map_err(internal)?).map_err(internal)?;
    std::fs::rename(&tmp, path).map_err(internal)
}

fn with_file<R>(dd: &Path, f: impl FnOnce(&WatcherFile) -> R) -> R {
    let mut g = STORE.lock().unwrap();
    let s = g.get_or_insert_with(|| {
        let path = dd.join("watcher.json");
        Store { file: load(&path), path }
    });
    f(&s.file)
}

/// Mutate a copy, persist it, then swap it in.
fn mutate<R>(dd: &Path, f: impl FnOnce(&mut WatcherFile) -> Result<R, String>) -> Result<R, String> {
    with_file(dd, |_| ());
    let mut g = STORE.lock().unwrap();
    let s = g.as_mut().expect("store loaded");
    let mut next = s.file.clone();
    let out = f(&mut next)?;
    save(&s.path, &next)?;
    s.file = next;
    Ok(out)
}

// ---------------------------------------------------------------- library

/// A followed series and the newest (season, episode) the user has.
#[derive(Debug, Clone, PartialEq)]
pub struct Followed {
    pub imdb_id: String,
    pub show: String,
    pub have: (u32, u32),
}

/// One torrent's view: its MediaInfo plus the (season, episode) pairs it holds.
pub struct LibEntry {
    pub info: MediaInfo,
    pub eps: Vec<(Option<u32>, u32)>,
}

/// Merge torrents into followed series (specials and unmatched entries ignored).
pub fn collect_followed(entries: &[LibEntry], unfollowed: &[String]) -> Vec<Followed> {
    let mut by: BTreeMap<String, Followed> = BTreeMap::new();
    for en in entries {
        let i = &en.info;
        if i.media_type != MediaType::Tv || i.needs_match || !metadata::valid_tt(&i.imdb_id) || unfollowed.contains(&i.imdb_id) {
            continue;
        }
        let mut pairs: Vec<(u32, u32)> = en.eps.iter().filter_map(|(s, e)| Some((s.or(i.season)?, *e))).collect();
        if let (Some(s), Some(e)) = (i.season, i.episode) {
            pairs.push((s, e));
        }
        let Some(max) = pairs.into_iter().filter(|(s, _)| *s > 0).max() else { continue };
        let f = by.entry(i.imdb_id.clone()).or_insert_with(|| Followed { imdb_id: i.imdb_id.clone(), show: i.title.clone(), have: max });
        f.have = f.have.max(max);
    }
    by.into_values().collect()
}

/// `tide.json` sidecar of a torrent, read directly (no network, ever).
fn sidecar_info(output_path: &Path, files: Vec<FileRow>, name: &str, download_dir: &Path) -> Option<MediaInfo> {
    let loc = metadata::Loc::from_parts_in(output_path, files, name, false, Some(download_dir));
    let json = match &loc.stem {
        Some(s) => format!("{s}.tide.json"),
        None => "tide.json".into(),
    };
    let v: Value = serde_json::from_slice(&std::fs::read(loc.dir.join(json)).ok()?).ok()?;
    serde_json::from_value(v.get("info")?.clone()).ok()
}

fn library(st: &AppState) -> Vec<LibEntry> {
    engine::list_rows(st)
        .into_iter()
        .filter(|r| r.state != TorrentState::Resolving)
        .filter_map(|r| {
            let out = r.output_path.as_ref()?;
            let files = engine::files(st, &r.key).ok()?;
            let eps = metadata::episodes_of(&r.name, &files).into_iter().map(|e| (e.season, e.episode)).collect();
            let info = sidecar_info(Path::new(out), files, &r.name, &st.download_dir)?;
            Some(LibEntry { info, eps })
        })
        .collect()
}

// ---------------------------------------------------------------- episode diffing

fn num(v: &Value, k: &str) -> Option<u32> {
    let x = v.get(k)?;
    x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())).map(|n| n as u32)
}

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// Aired regular episodes of a Cinemeta series `meta` after `have`, oldest first.
pub fn new_episodes(meta: &Value, imdb: &str, show: &str, have: (u32, u32), now_iso: &str) -> Vec<NewEpisode> {
    let show = text(meta, "name").unwrap_or_else(|| show.to_string());
    let mut out: Vec<NewEpisode> = meta
        .get("videos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let s = num(v, "season").filter(|s| *s > 0)?;
            let e = num(v, "episode").or_else(|| num(v, "number"))?;
            let released = text(v, "released").or_else(|| text(v, "firstAired"))?;
            if (s, e) <= have || released.as_str() > now_iso {
                return None;
            }
            Some(NewEpisode {
                imdb_id: imdb.to_string(),
                show: show.clone(),
                season: s,
                episode: e,
                episode_title: text(v, "name").or_else(|| text(v, "title")),
                released: Some(released),
                results: vec![],
            })
        })
        .collect();
    out.sort_by_key(|n| (n.season, n.episode));
    out.dedup_by_key(|n| (n.season, n.episode));
    out
}

// ---------------------------------------------------------------- Torznab

/// A parsed Torznab `<item>`.
#[derive(Debug, Clone, PartialEq)]
pub struct RawItem {
    pub title: String,
    pub size: u64,
    pub seeders: u32,
    pub magnet: Option<String>,
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).expect("valid regex"))
}

/// Decode XML text content (CDATA or entities).
fn xml_text(s: &str) -> String {
    let t = s.trim();
    if let Some(raw) = t.strip_prefix("<![CDATA[").and_then(|x| x.strip_suffix("]]>")) {
        return raw.trim().to_string();
    }
    static ENT: OnceLock<Regex> = OnceLock::new();
    re(&ENT, r"&(#x[0-9a-fA-F]+|#[0-9]+|amp|lt|gt|quot|apos);")
        .replace_all(t, |c: &regex::Captures| {
            let n = &c[1];
            let ch = match n {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if n.starts_with("#x") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32),
                _ => n[1..].parse().ok().and_then(char::from_u32),
            };
            ch.map_or_else(|| c[0].to_string(), String::from)
        })
        .into_owned()
}

fn tag_text(item: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = item.find(&open)?;
    let rest = &item[start + open.len()..];
    // `<link>` must not match `<linkfoo>`; self-closing tags have no text.
    if !rest.starts_with('>') && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let gt = rest.find('>')?;
    if rest[..gt].ends_with('/') {
        return None;
    }
    let body = &rest[gt + 1..];
    let end = body.find(&format!("</{tag}>"))?;
    Some(xml_text(&body[..end])).filter(|s| !s.is_empty())
}

fn attr(tag: &str, name: &str) -> Option<String> {
    static A: OnceLock<Regex> = OnceLock::new();
    re(&A, r#"([A-Za-z_:][-\w:.]*)\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
        .captures_iter(tag)
        .find(|c| c[1].eq_ignore_ascii_case(name))
        .map(|c| xml_text(c.get(2).or(c.get(3)).map_or("", |m| m.as_str())))
}

fn magnet_from_hash(hash: &str, title: &str) -> Option<String> {
    let h = hash.trim();
    let ok = (h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())) || (h.len() == 32 && h.bytes().all(|b| b.is_ascii_alphanumeric()));
    ok.then(|| format!("magnet:?xt=urn:btih:{}&dn={}", h.to_ascii_lowercase(), urlencoding::encode(title)))
}

fn is_magnet(s: &str) -> bool {
    s.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("magnet:"))
}

/// Parse a Torznab RSS response. `<error code=… description=…/>` becomes an Err.
pub fn parse_torznab(xml: &str) -> Result<Vec<RawItem>, String> {
    static ERR: OnceLock<Regex> = OnceLock::new();
    static ITEM: OnceLock<Regex> = OnceLock::new();
    static ATTR: OnceLock<Regex> = OnceLock::new();
    static ENC: OnceLock<Regex> = OnceLock::new();
    if let Some(m) = re(&ERR, r"<error\b[^>]*>").find(xml) {
        let d = attr(m.as_str(), "description").unwrap_or_else(|| "unknown error".into());
        let code = attr(m.as_str(), "code").unwrap_or_default();
        return Err(format!("Torznab error {code}: {d}"));
    }
    let mut out = vec![];
    for m in re(&ITEM, r"(?s)<item\b.*?</item>").find_iter(xml) {
        let item = m.as_str();
        let Some(title) = tag_text(item, "title") else { continue };
        let mut attrs: HashMap<String, String> = HashMap::new();
        for a in re(&ATTR, r"<(?:torznab|newznab):attr\b[^>]*>").find_iter(item) {
            if let (Some(n), Some(v)) = (attr(a.as_str(), "name"), attr(a.as_str(), "value")) {
                attrs.entry(n.to_ascii_lowercase()).or_insert(v);
            }
        }
        let enclosure = re(&ENC, r"<enclosure\b[^>]*>").find(item).map(|e| e.as_str().to_string());
        let enc_url = enclosure.as_deref().and_then(|e| attr(e, "url"));
        let size = tag_text(item, "size")
            .or_else(|| attrs.get("size").cloned())
            .or_else(|| enclosure.as_deref().and_then(|e| attr(e, "length")))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let seeders = attrs.get("seeders").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let magnet = attrs
            .get("magneturl")
            .cloned()
            .filter(|m| is_magnet(m))
            .or_else(|| tag_text(item, "link").filter(|l| is_magnet(l)))
            .or_else(|| enc_url.filter(|l| is_magnet(l)))
            .or_else(|| tag_text(item, "guid").filter(|l| is_magnet(l)))
            .or_else(|| attrs.get("infohash").and_then(|h| magnet_from_hash(h, &title)));
        out.push(RawItem { title, size, seeders, magnet });
    }
    Ok(out)
}

pub fn quality_of(title: &str) -> Option<&'static str> {
    static Q: OnceLock<Regex> = OnceLock::new();
    let c = re(&Q, r"(?i)\b(2160p|4k|uhd|1080p|1080i|720p|576p|480p)\b").captures(title)?;
    Some(match c[1].to_ascii_lowercase().as_str() {
        "2160p" | "4k" | "uhd" => "2160p",
        "1080p" | "1080i" => "1080p",
        "720p" => "720p",
        _ => "480p",
    })
}

fn btih(m: &str) -> Option<String> {
    let i = m.to_ascii_lowercase().find("btih:")?;
    Some(m[i + 5..].split('&').next()?.to_ascii_lowercase())
}

/// Keep items for exactly S/E (a multi-episode release covering E counts) with a magnet,
/// dedupe by info hash, rank: preferred quality first, then seeders. Top 5.
pub fn rank(items: Vec<RawItem>, season: u32, episode: u32, quality: &str) -> Vec<TorznabResult> {
    let mut seen = HashSet::new();
    let mut out: Vec<TorznabResult> = items
        .into_iter()
        .filter(|it| {
            let p = nameparse::parse(&it.title);
            p.season == Some(season) && p.episode.is_some_and(|e| e <= episode && episode <= p.episode_end.unwrap_or(e))
        })
        .filter_map(|it| {
            let magnet = it.magnet?;
            if !seen.insert(btih(&magnet).unwrap_or_else(|| magnet.clone())) {
                return None;
            }
            Some(TorznabResult { quality: quality_of(&it.title).map(String::from), title: it.title, size: it.size, seeders: it.seeders, magnet: Some(magnet) })
        })
        .collect();
    let pref = |r: &TorznabResult| quality != "any" && r.quality.as_deref() == Some(quality);
    out.sort_by(|a, b| pref(b).cmp(&pref(a)).then(b.seeders.cmp(&a.seeders)));
    out.truncate(MAX_RESULTS);
    out
}

pub struct Torznab {
    api: String,
    key: String,
    http: reqwest::Client,
}

impl Torznab {
    /// `None` when no endpoint is configured.
    pub fn new(s: &WatcherSettings, http: reqwest::Client) -> Option<Torznab> {
        let base = s.torznab_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return None;
        }
        let api = if base.ends_with("/api") { base.to_string() } else { format!("{base}/api") };
        Some(Torznab { api, key: s.api_key.trim().to_string(), http })
    }

    async fn get(&self, params: &[(&str, String)]) -> Result<String, String> {
        let mut url = format!("{}?apikey={}", self.api, urlencoding::encode(&self.key));
        for (k, v) in params {
            url.push_str(&format!("&{k}={}", urlencoding::encode(v)));
        }
        // `without_url`: never leak the API key into logs / error toasts.
        let resp = self.http.get(&url).timeout(TORZNAB_TIMEOUT).send().await.map_err(|e| format!("request failed: {}", e.without_url()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| format!("read failed: {}", e.without_url()))?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        Ok(body)
    }

    pub async fn caps(&self) -> Result<(), String> {
        let body = self.get(&[("t", "caps".into())]).await?;
        parse_torznab(&body)?;
        if !body.contains("<caps") {
            return Err("not a Torznab endpoint (no <caps>)".into());
        }
        Ok(())
    }

    /// `t=tvsearch` by IMDb id + S/E; falls back to `t=search&q=<Show> SxxEyy` when that
    /// fails or finds nothing usable.
    pub async fn search_episode(&self, ep: &NewEpisode, quality: &str) -> Result<Vec<TorznabResult>, String> {
        let tv = self
            .get(&[
                ("t", "tvsearch".into()),
                ("imdbid", ep.imdb_id.clone()),
                ("season", ep.season.to_string()),
                ("ep", ep.episode.to_string()),
            ])
            .await
            .and_then(|b| parse_torznab(&b));
        if let Ok(items) = &tv {
            let r = rank(items.clone(), ep.season, ep.episode, quality);
            if !r.is_empty() {
                return Ok(r);
            }
        }
        let q = format!("{} S{:02}E{:02}", ep.show, ep.season, ep.episode);
        match self.get(&[("t", "search".into()), ("q", q)]).await.and_then(|b| parse_torznab(&b)) {
            Ok(items) => Ok(rank(items, ep.season, ep.episode, quality)),
            Err(e) => tv.map(|_| vec![]).map_err(|_| e),
        }
    }
}

// ---------------------------------------------------------------- check

fn http() -> reqwest::Client {
    static H: OnceLock<reqwest::Client> = OnceLock::new();
    H.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest client")
    })
    .clone()
}

/// Network part of a check (testable without AppState).
pub async fn check_followed(followed: &[Followed], cm: &Cinemeta, tz: Option<&Torznab>, quality: &str, now_iso: &str) -> Vec<NewEpisode> {
    let mut out = vec![];
    for f in followed {
        let meta = match cm.meta(MediaType::Tv, &f.imdb_id).await {
            Ok(m) => m,
            Err(NetErr::NotFound) => continue,
            Err(NetErr::Net(e)) => {
                tracing::warn!("watcher: Cinemeta {} failed: {e}", f.imdb_id);
                continue;
            }
        };
        for mut ep in new_episodes(&meta, &f.imdb_id, &f.show, f.have, now_iso).into_iter().take(MAX_NEW_PER_SERIES) {
            if let Some(tz) = tz {
                match tz.search_episode(&ep, quality).await {
                    Ok(r) => ep.results = r,
                    Err(e) => tracing::warn!("watcher: Torznab search for {} S{}E{} failed: {e}", ep.show, ep.season, ep.episode),
                }
            }
            out.push(ep);
        }
    }
    out
}

/// What the UI should show: not dismissed/added, still followed, still newer than
/// what the library holds (an episode added since then drops out).
fn visible(file: &WatcherFile, followed: &[Followed]) -> Vec<NewEpisode> {
    let have: HashMap<&str, (u32, u32)> = followed.iter().map(|f| (f.imdb_id.as_str(), f.have)).collect();
    file.pending
        .iter()
        .filter(|n| !file.unfollowed.contains(&n.imdb_id) && !file.seen.contains_key(&seen_key(&n.imdb_id, n.season, n.episode)))
        .filter(|n| have.get(n.imdb_id.as_str()).is_some_and(|h| (n.season, n.episode) > *h))
        .cloned()
        .collect()
}

/// Returns (episodes to show, whether the network was used).
async fn run_check(st: &AppState, force: bool) -> Result<(Vec<NewEpisode>, bool), String> {
    let _g = CHECK.lock().await;
    let dd = &st.data_dir;
    let file = with_file(dd, Clone::clone);
    let followed = collect_followed(&library(st), &file.unfollowed);
    let now = now_secs();
    if !force && now.saturating_sub(file.last_check) < CHECK_EVERY {
        return Ok((visible(&file, &followed), false));
    }
    let cm = Cinemeta::new(metadata::CINEMETA, http());
    let tz = Torznab::new(&file.settings, http());
    let found = check_followed(&followed, &cm, tz.as_ref(), &file.settings.quality, &iso(now)).await;

    let mut added = vec![];
    if file.settings.auto_add {
        for n in &found {
            let key = seen_key(&n.imdb_id, n.season, n.episode);
            if file.seen.contains_key(&key) || file.unfollowed.contains(&n.imdb_id) {
                continue;
            }
            let Some(m) = n.results.first().and_then(|r| r.magnet.as_deref()) else { continue };
            match engine::add_magnet_internal(st, m).await {
                Ok(r) => {
                    tracing::info!(key = %r.key, "watcher: auto-added {} S{:02}E{:02}", n.show, n.season, n.episode);
                    added.push(key);
                }
                Err(e) => tracing::warn!("watcher: auto-add failed: {e}"),
            }
        }
    }
    let next = mutate(dd, |f| {
        f.pending = found;
        f.last_check = now;
        for k in added {
            f.seen.insert(k, "added".into());
        }
        Ok(f.clone())
    })?;
    Ok((visible(&next, &followed), true))
}

/// Startup hook: first check ~20 s after launch, then re-evaluate hourly (a check only
/// hits the network when the last one is ≥ 24 h old). Emits `new-episodes`.
pub fn spawn_background(app: AppHandle, st: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_DELAY).await;
        loop {
            match run_check(&st, false).await {
                Ok((list, true)) => {
                    if let Err(e) = app.emit("new-episodes", list) {
                        tracing::debug!("emit new-episodes failed: {e}");
                    }
                }
                Ok((_, false)) => {}
                Err(e) => tracing::warn!("watcher: check failed: {e}"),
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub fn watcher_get_settings(state: St<'_>) -> WatcherInfo {
    with_file(&state.data_dir, WatcherFile::info)
}

#[tauri::command]
pub async fn watcher_set_settings(
    state: St<'_>,
    torznab_url: String,
    api_key: String,
    auto_add: bool,
    quality: String,
) -> Result<WatcherInfo, String> {
    let url = torznab_url.trim().to_string();
    if !url.is_empty() && !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(invalid("Torznab URL must start with http:// or https://"));
    }
    if !QUALITIES.contains(&quality.as_str()) {
        return Err(invalid(format!("unknown quality {quality}")));
    }
    let next = WatcherSettings { torznab_url: url, api_key: api_key.trim().to_string(), auto_add, quality };
    let cur = with_file(&state.data_dir, |f| f.settings.clone());
    // Validate only a changed endpoint/key (a user action: Save).
    if next.torznab_url != cur.torznab_url || next.api_key != cur.api_key {
        if let Some(tz) = Torznab::new(&next, http()) {
            tz.caps().await.map_err(|e| invalid(format!("Torznab check failed: {e}")))?;
        }
    }
    mutate(&state.data_dir, |f| {
        f.settings = next;
        Ok(f.info())
    })
}

#[tauri::command]
pub async fn watcher_check(state: St<'_>, force: bool) -> Result<Vec<NewEpisode>, String> {
    let st: Arc<AppState> = state.inner().clone();
    run_check(&st, force).await.map(|(v, _)| v)
}

fn check_tt(imdb_id: &str) -> Result<(), String> {
    if metadata::valid_tt(imdb_id) { Ok(()) } else { Err(invalid("bad IMDb id")) }
}

#[tauri::command]
pub fn watcher_unfollow(state: St<'_>, imdb_id: String) -> Result<(), String> {
    check_tt(&imdb_id)?;
    mutate(&state.data_dir, |f| {
        if !f.unfollowed.contains(&imdb_id) {
            f.unfollowed.push(imdb_id);
        }
        Ok(())
    })
}

#[tauri::command]
pub fn watcher_follow(state: St<'_>, imdb_id: String) -> Result<(), String> {
    check_tt(&imdb_id)?;
    mutate(&state.data_dir, |f| {
        f.unfollowed.retain(|x| *x != imdb_id);
        Ok(())
    })
}

#[tauri::command]
pub fn watcher_dismiss(state: St<'_>, imdb_id: String, season: u32, episode: u32) -> Result<(), String> {
    check_tt(&imdb_id)?;
    mutate(&state.data_dir, |f| {
        f.seen.insert(seen_key(&imdb_id, season, episode), "dismissed".into());
        Ok(())
    })
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const META: &str = r#"{"meta":{"id":"tt14582950","type":"series","name":"The Diplomat","videos":[
      {"season":0,"episode":1,"name":"Special","released":"2024-01-01T00:00:00.000Z"},
      {"season":4,"episode":7,"name":"Seven","released":"2026-10-01T05:00:00.000Z"},
      {"season":4,"episode":9,"name":"Nine","released":"2026-10-15T05:00:00.000Z"},
      {"season":4,"episode":8,"name":"Eight","released":"2026-10-08T05:00:00.000Z"},
      {"season":4,"episode":10,"name":"Ten","released":"2026-10-22T05:00:00.000Z"},
      {"season":5,"episode":1,"name":"TBA"},
      {"season":3,"episode":6,"name":"Old","released":"2025-11-01T05:00:00.000Z"}]}}"#;

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel><title>Indexer</title>
<item>
  <title>The.Diplomat.S04E08.720p.WEB.h264-GRP</title>
  <guid>https://indexer.invalid/details/1</guid>
  <link>https://indexer.invalid/dl/1</link>
  <size>1100000000</size>
  <torznab:attr name="seeders" value="900"/>
  <torznab:attr name="infohash" value="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"/>
</item>
<item>
  <title><![CDATA[The Diplomat S04E08 1080p WEB H264 & More]]></title>
  <link>magnet:?xt=urn:btih:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&amp;dn=x</link>
  <enclosure url="magnet:?xt=urn:btih:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" length="2100000000" type="application/x-bittorrent"/>
  <torznab:attr value="340" name="seeders"/>
</item>
<item>
  <title>The.Diplomat.S04E08.1080p.x265-OTHER</title>
  <size>900000000</size>
  <torznab:attr name="seeders" value="12"/>
  <torznab:attr name="magneturl" value="magnet:?xt=urn:btih:cccccccccccccccccccccccccccccccccccccccc&amp;dn=y"/>
</item>
<item>
  <title>The.Diplomat.S04E08.1080p.DUPE</title>
  <torznab:attr name="seeders" value="5"/>
  <torznab:attr name="magneturl" value="magnet:?xt=urn:btih:CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC"/>
</item>
<item>
  <title>The.Diplomat.S04E07.1080p.WEB</title>
  <torznab:attr name="seeders" value="999"/>
  <torznab:attr name="infohash" value="dddddddddddddddddddddddddddddddddddddddd"/>
</item>
<item>
  <title>The.Diplomat.S04.COMPLETE.1080p</title>
  <torznab:attr name="seeders" value="999"/>
  <torznab:attr name="infohash" value="eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"/>
</item>
<item>
  <title>The.Diplomat.S04E08.2160p.torrent.only</title>
  <link>https://indexer.invalid/dl/9</link>
  <torznab:attr name="seeders" value="5000"/>
</item>
</channel></rss>"#;

    fn tv(imdb: &str, s: Option<u32>, e: Option<u32>) -> MediaInfo {
        MediaInfo { imdb_id: imdb.into(), media_type: MediaType::Tv, title: "The Diplomat".into(), season: s, episode: e, ..Default::default() }
    }

    #[test]
    fn followed_from_library() {
        let lib = vec![
            LibEntry { info: tv("tt14582950", Some(4), Some(5)), eps: vec![] },
            // Season pack: files carry the episodes; season from the info.
            LibEntry { info: tv("tt14582950", Some(4), None), eps: vec![(None, 6), (Some(4), 7), (Some(0), 99)] },
            LibEntry { info: tv("tt0903747", Some(0), Some(3)), eps: vec![] }, // specials only
            LibEntry { info: MediaInfo { media_type: MediaType::Movie, ..tv("tt0133093", None, None) }, eps: vec![] },
            LibEntry { info: tv("tt0386676", Some(2), Some(1)), eps: vec![] },
            LibEntry { info: MediaInfo { needs_match: true, ..tv("tt1111111", Some(1), Some(1)) }, eps: vec![] },
        ];
        let f = collect_followed(&lib, &[]);
        assert_eq!(f.len(), 2);
        assert_eq!(f[1], Followed { imdb_id: "tt14582950".into(), show: "The Diplomat".into(), have: (4, 7) });
        let f = collect_followed(&lib, &["tt0386676".into()]);
        assert_eq!(f.iter().map(|x| x.imdb_id.as_str()).collect::<Vec<_>>(), vec!["tt14582950"]);
    }

    #[test]
    fn diffing_aired_after_have() {
        let meta: Value = serde_json::from_str::<Value>(META).unwrap()["meta"].clone();
        let now = iso(1_760_600_000); // 2025-10-16
        assert!(now.starts_with("2025-10-16T"));
        let now = "2026-10-16T12:00:00";
        let n = new_episodes(&meta, "tt14582950", "x", (4, 7), now);
        let got: Vec<_> = n.iter().map(|e| (e.season, e.episode, e.episode_title.clone().unwrap())).collect();
        assert_eq!(got, vec![(4, 8, "Eight".into()), (4, 9, "Nine".into())]);
        assert_eq!(n[0].show, "The Diplomat");
        assert_eq!(n[0].released.as_deref(), Some("2026-10-08T05:00:00.000Z"));
        // Up to date / ahead: nothing. Specials never.
        assert!(new_episodes(&meta, "tt", "x", (4, 9), now).is_empty());
        assert_eq!(new_episodes(&meta, "tt", "x", (3, 6), now).len(), 3);
    }

    #[test]
    fn torznab_parse_and_rank() {
        let items = parse_torznab(RSS).unwrap();
        assert_eq!(items.len(), 7);
        assert_eq!(items[1].title, "The Diplomat S04E08 1080p WEB H264 & More");
        assert_eq!(items[1].size, 2_100_000_000);
        assert_eq!(items[1].seeders, 340);
        assert_eq!(items[1].magnet.as_deref(), Some("magnet:?xt=urn:btih:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&dn=x"));
        assert!(items[0].magnet.as_deref().unwrap().starts_with("magnet:?xt=urn:btih:aaaaaaaa"));
        assert_eq!(items[6].magnet, None);

        let r = rank(items.clone(), 4, 8, "1080p");
        let t: Vec<_> = r.iter().map(|x| (x.quality.as_deref(), x.seeders)).collect();
        // 1080p first (by seeders), dupes / other episodes / packs / no-magnet dropped.
        assert_eq!(t, vec![(Some("1080p"), 340), (Some("1080p"), 12), (Some("720p"), 900)]);
        let r = rank(items.clone(), 4, 8, "any");
        assert_eq!(r[0].seeders, 900);
        let r = rank(items, 4, 8, "720p");
        assert_eq!(r[0].quality.as_deref(), Some("720p"));

        let e = parse_torznab(r#"<?xml version="1.0"?><error code="100" description="Invalid API Key"/>"#).unwrap_err();
        assert!(e.contains("Invalid API Key"), "{e}");
        assert_eq!(xml_text("a &amp; b &#39;c&#x27; &bogus;"), "a & b 'c' &bogus;");
    }

    // ------------------------------------------------ mock HTTP server (loopback only)

    /// Answers the first route whose needle occurs in the request target; 404 otherwise.
    async fn mock_server(routes: Vec<(&'static str, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(vec![]));
        let h2 = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let (routes, hits) = (routes.clone(), h2.clone());
                tokio::spawn(async move {
                    let mut buf = vec![];
                    let mut tmp = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let req = String::from_utf8_lossy(&buf).to_string();
                    let target = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    hits.lock().unwrap().push(target.clone());
                    let (status, body) = match routes.iter().find(|(n, _)| target.contains(n)) {
                        Some((_, b)) => ("200 OK", b.clone()),
                        None => ("404 Not Found", "nope".to_string()),
                    };
                    let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(body.as_bytes()).await;
                });
            }
        });
        (base, hits)
    }

    #[tokio::test]
    async fn check_end_to_end_with_mock_servers() {
        let (cbase, chits) = mock_server(vec![("/meta/series/tt14582950.json", META.to_string())]).await;
        let empty_rss = r#"<rss><channel></channel></rss>"#.to_string();
        let (tbase, thits) = mock_server(vec![
            ("t=caps", r#"<?xml version="1.0"?><caps><searching><tv-search available="yes"/></searching></caps>"#.into()),
            // S04E08: tvsearch hits. S04E09: tvsearch empty -> q= fallback.
            ("ep=8", RSS.to_string()),
            ("ep=9", empty_rss),
            ("q=The%20Diplomat%20S04E09", RSS.replace("S04E08", "S04E09")),
        ])
        .await;
        let settings = WatcherSettings { torznab_url: format!("{tbase}/"), api_key: "k&y".into(), ..Default::default() };
        let tz = Torznab::new(&settings, http()).unwrap();
        tz.caps().await.unwrap();
        let cm = Cinemeta::new(&cbase, http());
        let followed = vec![
            Followed { imdb_id: "tt14582950".into(), show: "Diplomat".into(), have: (4, 7) },
            Followed { imdb_id: "tt0000404".into(), show: "Gone".into(), have: (1, 1) },
        ];
        let out = check_followed(&followed, &cm, Some(&tz), "1080p", "2026-10-16T12:00:00").await;
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].season, out[0].episode, out[0].results.len()), (4, 8, 3));
        assert_eq!(out[0].results[0].seeders, 340);
        assert_eq!(out[1].episode, 9);
        assert_eq!(out[1].results[0].title, "The Diplomat S04E09 1080p WEB H264 & More");
        let th = thits.lock().unwrap().clone();
        assert!(th[0].starts_with("/api?apikey=k%26y&t=caps"), "{th:?}");
        assert!(th.iter().any(|h| h.contains("t=tvsearch&imdbid=tt14582950&season=4&ep=8")), "{th:?}");
        assert!(chits.lock().unwrap().iter().any(|h| h == "/meta/series/tt0000404.json"));

        // Without Torznab: episodes only.
        let out = check_followed(&followed[..1], &cm, None, "1080p", "2026-10-16T12:00:00").await;
        assert!(out.iter().all(|n| n.results.is_empty()) && out.len() == 2);
    }

    #[test]
    fn visibility_and_persistence() {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("tide-watcher-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let ep = |e| NewEpisode { imdb_id: "tt14582950".into(), show: "D".into(), season: 4, episode: e, episode_title: None, released: None, results: vec![] };
        let mut f = WatcherFile { pending: vec![ep(8), ep(9), ep(10)], ..Default::default() };
        f.seen.insert(seen_key("tt14582950", 4, 9), "dismissed".into());
        let fol = vec![Followed { imdb_id: "tt14582950".into(), show: "D".into(), have: (4, 8) }];
        assert_eq!(visible(&f, &fol).iter().map(|n| n.episode).collect::<Vec<_>>(), vec![10]);
        f.unfollowed.push("tt14582950".into());
        assert!(visible(&f, &fol).is_empty());

        save(&dir.join("watcher.json"), &f).unwrap();
        assert_eq!(load(&dir.join("watcher.json")), f);
        let min: WatcherFile = serde_json::from_str(r#"{"settings":{"torznab_url":"http://x"}}"#).unwrap();
        assert_eq!(min.settings.quality, "1080p");
        let _ = std::fs::remove_dir_all(dir);
    }
}
