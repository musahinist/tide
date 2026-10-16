//! Online subtitles from Stremio's keyless OpenSubtitles v3 addon.
//!
//! `GET {BASE}/subtitles/{movie|series}/{imdb}[:{s}:{e}].json` -> `{subtitles:[{id,url,lang}]}`
//! (`lang` is ISO 639-2, e.g. "tur"/"eng"); each `url` serves a UTF-8 SRT.
//!
//! Only when the torrent's own subtitles lack a preferred language (default tur, eng;
//! `data_dir/settings.json` key `subtitle_langs`). At most `PER_LANG` files per language,
//! saved next to the video as `<video stem>.<lang2>[.N].srt` (never over an existing
//! file or a torrent file). Saved files are recorded in `data_dir/online_subs.json`
//! (Tide-owned, for remove-with-files); "nothing found" is cached there for 24 h.
//! Already downloaded subtitles are reused without any request.
//!
//! Track ids: `o:<key>:<16 hex hash of the absolute path>` (see `resolve_track`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::State;

use crate::state::AppState;
use crate::types::{FileKind, MediaRef, MediaType, SubtitleFormat, SubtitleTrack};
use crate::{engine, metadata, subtitles};

const BASE: &str = "https://opensubtitles-v3.strem.io";
const DEFAULT_LANGS: &[&str] = &["tur", "eng"];
const PER_LANG: usize = 2;
const NEG_TTL: u64 = 24 * 3600;
const MAX_SUB_BYTES: usize = 2 * 1024 * 1024;
const MAX_LIST_BYTES: usize = 4 * 1024 * 1024;
const REQ_TIMEOUT: Duration = Duration::from_secs(20);

/// Metered network (or the user's data-saver mode): reuse only, no downloads.
fn is_metered() -> bool {
    crate::netmode::is_metered()
}

/// Serializes all online lookups (no duplicate downloads for the same video).
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ------------------------------------------------------------------ pure helpers

/// (ISO 639-2, ISO 639-1)
const LANGS: &[(&str, &str)] = &[
    ("tur", "tr"), ("eng", "en"), ("spa", "es"), ("fre", "fr"), ("fra", "fr"), ("ger", "de"),
    ("deu", "de"), ("ita", "it"), ("por", "pt"), ("pob", "pt"), ("rus", "ru"), ("jpn", "ja"),
    ("kor", "ko"), ("chi", "zh"), ("zho", "zh"), ("ara", "ar"), ("dut", "nl"), ("nld", "nl"),
    ("swe", "sv"), ("nor", "no"), ("dan", "da"), ("fin", "fi"), ("pol", "pl"), ("cze", "cs"),
    ("ces", "cs"), ("hun", "hu"), ("rum", "ro"), ("ron", "ro"), ("gre", "el"), ("ell", "el"),
    ("heb", "he"), ("hin", "hi"), ("ind", "id"), ("vie", "vi"), ("tha", "th"), ("ukr", "uk"),
    ("per", "fa"), ("fas", "fa"), ("bul", "bg"), ("hrv", "hr"), ("srp", "sr"),
];

pub fn lang2(iso3: &str) -> Option<&'static str> {
    let l = iso3.to_ascii_lowercase();
    LANGS.iter().find(|(a, _)| *a == l).map(|(_, b)| *b)
}

/// "tr" or "tur" -> "tur".
fn lang3(code: &str) -> Option<&'static str> {
    let l = code.trim().to_ascii_lowercase();
    LANGS.iter().find(|(a, b)| *a == l || *b == l).map(|(a, _)| *a)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSub {
    pub url: String,
    pub lang: String,
}

/// Lenient parse of the addon's list; malformed entries are skipped.
pub fn parse_list(bytes: &[u8]) -> Vec<RemoteSub> {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else { return Vec::new() };
    v.get("subtitles")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    let url = s.get("url")?.as_str()?.trim().to_string();
                    let lang = s.get("lang")?.as_str()?.trim().to_ascii_lowercase();
                    (!url.is_empty() && !lang.is_empty()).then_some(RemoteSub { url, lang })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// https, or plain http to loopback (tests' mock server).
fn url_ok(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:")
}

/// First `per_lang` entries per wanted language (in `wanted` order), unique by url.
pub fn select(list: &[RemoteSub], wanted: &[String], per_lang: usize) -> Vec<RemoteSub> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for w in wanted {
        let mut n = 0;
        for s in list.iter().filter(|s| &s.lang == w && url_ok(&s.url)) {
            if n == per_lang {
                break;
            }
            if seen.insert(s.url.as_str()) {
                out.push(s.clone());
                n += 1;
            }
        }
    }
    out
}

/// `/subtitles/movie/tt1.json` or `/subtitles/series/tt1:2:3.json`.
pub fn query_path(imdb: &str, se: Option<(u32, u32)>) -> String {
    match se {
        Some((s, e)) => format!("/subtitles/series/{imdb}:{s}:{e}.json"),
        None => format!("/subtitles/movie/{imdb}.json"),
    }
}

/// `<dir>/<stem>.<l2>.srt`, else `.<l2>.2.srt` ... ; None when all are taken.
pub fn target_path(dir: &Path, stem: &str, l2: &str, taken: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    (1..=9).map(|n| {
        let name = if n == 1 { format!("{stem}.{l2}.srt") } else { format!("{stem}.{l2}.{n}.srt") };
        dir.join(name)
    })
    .find(|p| !taken(p))
}

/// FNV-1a 64 (stable across runs and Rust versions).
fn path_hash(p: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in p.to_string_lossy().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub fn track_id(key: &str, path: &Path) -> String {
    format!("o:{key}:{}", path_hash(path))
}

fn parse_track_id(id: &str) -> Option<(String, String)> {
    let (key, h) = id.strip_prefix("o:")?.split_once(':')?;
    (key.len() == 40 && key.bytes().all(|b| b.is_ascii_hexdigit()) && h.len() == 16)
        .then(|| (key.to_ascii_lowercase(), h.to_string()))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ------------------------------------------------------------------ store

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Owned {
    path: PathBuf,
    file_index: usize,
    /// ISO 639-1
    lang: String,
    url: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Store {
    /// torrent key -> Tide-owned subtitle files.
    owned: HashMap<String, Vec<Owned>>,
    /// "<query>|<langs>" -> unix time of an empty answer.
    misses: HashMap<String, u64>,
}

fn store_path(dd: &Path) -> PathBuf {
    dd.join("online_subs.json")
}

fn load(dd: &Path) -> Store {
    std::fs::read(store_path(dd)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(dd: &Path, s: &Store) {
    let p = store_path(dd);
    let tmp = dd.join(".online_subs.json.tmp");
    let ok = serde_json::to_vec_pretty(s)
        .map_err(|e| e.to_string())
        .and_then(|b| std::fs::write(&tmp, b).map_err(|e| e.to_string()))
        .and_then(|_| std::fs::rename(&tmp, &p).map_err(|e| e.to_string()));
    if let Err(e) = ok {
        tracing::warn!("saving {} failed: {e}", p.display());
    }
}

/// Preferred languages (ISO 639-2) from `data_dir/settings.json` `subtitle_langs`.
fn pref_langs(dd: &Path) -> Vec<String> {
    let from_file: Vec<String> = std::fs::read(dd.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("subtitle_langs").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|x| lang3(x.as_str()?))
        .map(str::to_string)
        .collect();
    let mut out: Vec<String> = Vec::new();
    for l in if from_file.is_empty() { DEFAULT_LANGS.iter().map(|s| s.to_string()).collect() } else { from_file } {
        if !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

/// Tide-owned online subtitle files of `key` (for remove-with-files).
pub fn owned_files(st: &AppState, key: &str) -> Vec<PathBuf> {
    let key = key.trim().to_ascii_lowercase();
    load(&st.data_dir).owned.get(&key).map(|v| v.iter().map(|o| o.path.clone()).collect()).unwrap_or_default()
}

/// Drop `key` from the owned list (after its files were removed).
pub fn forget(st: &AppState, key: &str) {
    let key = key.trim().to_ascii_lowercase();
    let mut s = load(&st.data_dir);
    if s.owned.remove(&key).is_some() {
        save(&st.data_dir, &s);
    }
}

/// Path of an online track id ("o:<key>:<hash>"); only Tide-owned files resolve.
pub fn resolve_track(st: &AppState, id: &str) -> Option<PathBuf> {
    let (key, h) = parse_track_id(id)?;
    load(&st.data_dir).owned.get(&key)?.iter().find(|o| path_hash(&o.path) == h).map(|o| o.path.clone())
}

/// WebVTT text of a downloaded SRT.
pub fn vtt_for_path(path: &Path) -> Result<String, String> {
    let mut b = std::fs::read(path).map_err(|e| format!("NOT_FOUND: {}: {e}", path.display()))?;
    b.truncate(MAX_SUB_BYTES);
    Ok(subtitles::to_vtt(&b, SubtitleFormat::Srt))
}

/// WebVTT for an online track id.
pub fn vtt_for_track(st: &AppState, id: &str) -> Result<String, String> {
    let p = resolve_track(st, id).ok_or("NOT_FOUND: unknown online subtitle")?;
    vtt_for_path(&p)
}

// ------------------------------------------------------------------ core

struct Job {
    key: String,
    file_index: usize,
    video: PathBuf,
    /// Absolute paths of every torrent file (never written to).
    torrent_paths: HashSet<PathBuf>,
    /// ISO 639-1 languages the torrent already has for this video.
    have: Vec<String>,
    /// None: no lookup possible (unmatched / missing S/E), only reuse.
    query: Option<String>,
    /// ISO 639-2, in preference order.
    langs: Vec<String>,
}

fn tracks_of(key: &str, owned: &[&Owned]) -> Vec<SubtitleTrack> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    owned
        .iter()
        .map(|o| {
            let base = format!("{} (online)", subtitles::lang_name(&o.lang).unwrap_or(&o.lang));
            let n = seen.entry(base.clone()).or_insert(0);
            *n += 1;
            let label = if *n == 1 { base } else { format!("{base} {n}") };
            SubtitleTrack { id: track_id(key, &o.path), label, lang: Some(o.lang.clone()), format: SubtitleFormat::Srt }
        })
        .collect()
}

async fn get_bytes(http: &reqwest::Client, url: &str, cap: usize) -> Result<Vec<u8>, String> {
    if !url_ok(url) {
        return Err(format!("refusing non-https url {url}"));
    }
    let r = http.get(url).timeout(REQ_TIMEOUT).send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("HTTP {}", r.status()));
    }
    if r.content_length().is_some_and(|n| n as usize > cap) {
        return Err("too large".into());
    }
    let b = r.bytes().await.map_err(|e| e.to_string())?;
    if b.len() > cap {
        return Err("too large".into());
    }
    Ok(b.to_vec())
}

async fn run(dd: &Path, http: &reqwest::Client, base: &str, job: &Job, metered: bool, now: u64) -> Vec<SubtitleTrack> {
    let mut store = load(dd);
    // Forget owned files that vanished from disk.
    let before = store.owned.get(&job.key).map_or(0, Vec::len);
    if let Some(v) = store.owned.get_mut(&job.key) {
        v.retain(|o| o.path.is_file());
    }
    let mut dirty = store.owned.get(&job.key).map_or(0, Vec::len) != before;

    let mine = |s: &Store| -> Vec<Owned> {
        s.owned.get(&job.key).map(|v| v.iter().filter(|o| o.file_index == job.file_index).cloned().collect()).unwrap_or_default()
    };
    let existing = mine(&store);
    let wanted: Vec<String> = job
        .langs
        .iter()
        .filter(|l| {
            let l2 = lang2(l).unwrap_or("");
            !job.have.iter().any(|h| h == l2) && !existing.iter().any(|o| o.lang == l2)
        })
        .cloned()
        .collect();

    'fetch: {
        let Some(q) = job.query.as_deref() else { break 'fetch };
        if wanted.is_empty() || metered {
            break 'fetch;
        }
        let miss_key = format!("{q}|{}", wanted.join(","));
        if store.misses.get(&miss_key).is_some_and(|t| now.saturating_sub(*t) < NEG_TTL) {
            break 'fetch;
        }
        let list = match get_bytes(http, &format!("{base}{q}"), MAX_LIST_BYTES).await {
            Ok(b) => parse_list(&b),
            Err(e) => {
                tracing::info!("online subtitle list failed: {e}");
                break 'fetch;
            }
        };
        let picks = select(&list, &wanted, PER_LANG);
        if picks.is_empty() {
            store.misses.insert(miss_key, now);
            dirty = true;
            break 'fetch;
        }
        let dir = job.video.parent().unwrap_or(Path::new("/"));
        let stem = job.video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        for p in picks {
            let Some(l2) = lang2(&p.lang) else { continue };
            let bytes = match get_bytes(http, &p.url, MAX_SUB_BYTES).await {
                Ok(b) if subtitles::decode(&b).contains("-->") => b,
                Ok(_) => {
                    tracing::info!("online subtitle {} is not SRT; skipped", p.url);
                    continue;
                }
                Err(e) => {
                    tracing::info!("online subtitle {} failed: {e}", p.url);
                    continue;
                }
            };
            let Some(path) = target_path(dir, &stem, l2, |c| c.exists() || job.torrent_paths.contains(c)) else { continue };
            // create_new: never overwrite, even if something appeared meanwhile.
            let written = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes));
            if let Err(e) = written {
                tracing::warn!("writing {} failed: {e}", path.display());
                continue;
            }
            store.owned.entry(job.key.clone()).or_default().push(Owned {
                path,
                file_index: job.file_index,
                lang: l2.to_string(),
                url: p.url.clone(),
            });
            dirty = true;
        }
    }
    if dirty {
        save(dd, &store);
    }
    let mut mine = mine(&store);
    // Preference order, then download order.
    let rank = |l: &str| job.langs.iter().position(|x| lang2(x) == Some(l)).unwrap_or(usize::MAX);
    mine.sort_by_key(|o| rank(&o.lang));
    tracks_of(&job.key, &mine.iter().collect::<Vec<_>>())
}

/// Online subtitle tracks for `media` (downloading missing preferred languages once).
/// Never fails: problems are logged and give fewer tracks.
pub async fn online_tracks(st: &AppState, media: &MediaRef) -> Vec<SubtitleTrack> {
    tracks(st, media, true).await
}

/// Online subtitles already saved for `media` (disk only, never a request).
pub async fn saved_tracks(st: &AppState, media: &MediaRef) -> Vec<SubtitleTrack> {
    tracks(st, media, false).await
}

async fn tracks(st: &AppState, media: &MediaRef, net: bool) -> Vec<SubtitleTrack> {
    let MediaRef::Torrent { key, file_index } = media else { return Vec::new() };
    let key = key.trim().to_ascii_lowercase();
    let Ok(files) = engine::files(st, &key) else { return Vec::new() };
    let Some(row) = files.iter().find(|f| f.index == *file_index) else { return Vec::new() };
    if row.kind != FileKind::Video {
        return Vec::new();
    }
    let Ok(video) = engine::file_abs_path(st, &key, *file_index) else { return Vec::new() };
    let videos: Vec<&str> = files.iter().filter(|f| f.kind == FileKind::Video).map(|f| f.name.as_str()).collect();
    let subs: Vec<(usize, &str)> =
        files.iter().filter(|f| f.kind == FileKind::Subtitle).map(|f| (f.index, f.name.as_str())).collect();
    let have: Vec<String> =
        subtitles::build_tracks(&key, &row.name, &videos, &subs).0.into_iter().filter_map(|t| t.lang).collect();
    let torrent_paths: HashSet<PathBuf> =
        files.iter().filter_map(|f| engine::file_abs_path(st, &key, f.index).ok()).collect();

    let query = metadata::offline_info(st, &key).filter(|i| net && !i.needs_match && metadata::valid_tt(&i.imdb_id)).and_then(|i| {
        match i.media_type {
            MediaType::Movie => Some(query_path(&i.imdb_id, None)),
            MediaType::Tv => {
                let name = engine::list_rows(st).into_iter().find(|r| r.key == key).map(|r| r.name).unwrap_or_default();
                let ep = metadata::episodes_of(&name, &files).into_iter().find(|e| e.file_index == *file_index);
                let s = ep.as_ref().and_then(|e| e.season).or(i.season)?;
                let e = ep.map(|e| e.episode).or(i.episode)?;
                Some(query_path(&i.imdb_id, Some((s, e))))
            }
        }
    });
    let job = Job { key, file_index: *file_index, video, torrent_paths, have, query, langs: pref_langs(&st.data_dir) };
    let _g = LOCK.lock().await;
    run(&st.data_dir, &metadata::http_client(st), BASE, &job, is_metered(), now_secs()).await
}

#[tauri::command]
pub async fn fetch_online_subtitles(state: State<'_, Arc<AppState>>, media: MediaRef) -> Result<Vec<SubtitleTrack>, String> {
    let st: Arc<AppState> = state.inner().clone();
    Ok(online_tracks(&st, &media).await)
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const KEY: &str = "0123456789abcdef0123456789abcdef01234567";
    const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\nMerhaba\n";

    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir()
            .join(format!("tide-subfetch-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Loopback-only HTTP server; "{BASE}" in a body becomes its own URL. Counts requests.
    async fn mock(routes: Vec<(&'static str, String)>) -> (String, Arc<AtomicUsize>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let (b2, h2) = (base.clone(), hits.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let (routes, base, hits) = (routes.clone(), b2.clone(), h2.clone());
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    hits.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = match routes.iter().find(|(p, _)| *p == path) {
                        Some((_, b)) => ("200 OK", b.replace("{BASE}", &base)),
                        None => ("404 Not Found", String::new()),
                    };
                    let resp = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        (base, hits)
    }

    const LIST: &str = r#"{"subtitles":[
        {"id":"1","url":"{BASE}/s/tr1","lang":"tur"},
        {"id":"2","url":"{BASE}/s/tr1","lang":"tur"},
        {"id":"3","url":"{BASE}/s/tr2","lang":"tur"},
        {"id":"4","url":"{BASE}/s/tr3","lang":"tur"},
        {"id":"5","url":"{BASE}/s/en1","lang":"eng"},
        {"id":"6","url":"{BASE}/s/fr1","lang":"fre"},
        {"id":"7","lang":"eng"}
    ]}"#;

    fn job(dir: &Path, have: &[&str], query: Option<&str>) -> Job {
        Job {
            key: KEY.into(),
            file_index: 0,
            video: dir.join("Movie.2020.1080p.mkv"),
            torrent_paths: [dir.join("Movie.2020.1080p.mkv")].into_iter().collect(),
            have: have.iter().map(|s| s.to_string()).collect(),
            query: query.map(str::to_string),
            langs: vec!["tur".into(), "eng".into()],
        }
    }

    #[test]
    fn parse_and_select() {
        let l = parse_list(LIST.replace("{BASE}", "https://x").as_bytes());
        assert_eq!(l.len(), 6); // entry without url skipped
        let sel = select(&l, &["tur".into(), "eng".into()], 2);
        let urls: Vec<&str> = sel.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, ["https://x/s/tr1", "https://x/s/tr2", "https://x/s/en1"]);
        assert!(parse_list(b"not json").is_empty());
        assert!(parse_list(br#"{"subtitles":[]}"#).is_empty());
        // Non-https urls are never selected.
        let bad = vec![RemoteSub { url: "http://evil/x".into(), lang: "tur".into() }];
        assert!(select(&bad, &["tur".into()], 2).is_empty());
    }

    #[test]
    fn langs_paths_ids() {
        assert_eq!(lang2("TUR"), Some("tr"));
        assert_eq!(lang3("en"), Some("eng"));
        assert_eq!(query_path("tt1", None), "/subtitles/movie/tt1.json");
        assert_eq!(query_path("tt1", Some((2, 3))), "/subtitles/series/tt1:2:3.json");
        let d = Path::new("/m");
        let taken: HashSet<PathBuf> = [d.join("V.tr.srt")].into_iter().collect();
        assert_eq!(target_path(d, "V", "tr", |p| taken.contains(p)), Some(d.join("V.tr.2.srt")));
        assert_eq!(target_path(d, "V", "en", |p| taken.contains(p)), Some(d.join("V.en.srt")));
        let id = track_id(KEY, &d.join("V.tr.srt"));
        assert_eq!(parse_track_id(&id).map(|(k, _)| k), Some(KEY.to_string()));
        assert_eq!(parse_track_id("t:x:1"), None);
        // Detected as Turkish by the local subtitle matcher too.
        assert_eq!(subtitles::detect_lang("V.mkv", "V.tr.2.srt").as_deref(), Some("tr"));
    }

    #[test]
    fn settings_langs() {
        let d = tmpdir("settings");
        assert_eq!(pref_langs(&d), ["tur", "eng"]);
        std::fs::write(d.join("settings.json"), r#"{"subtitle_langs":["en","eng","xx","de"]}"#).unwrap();
        assert_eq!(pref_langs(&d), ["eng", "ger"]);
    }

    #[tokio::test]
    async fn download_save_reuse_and_no_overwrite() {
        let (base, hits) = mock(vec![
            ("/subtitles/movie/tt0133093.json", LIST.to_string()),
            ("/s/tr1", SRT.to_string()),
            ("/s/tr2", SRT.to_string()),
            ("/s/en1", "<html>not a subtitle</html>".to_string()),
        ])
        .await;
        let dd = tmpdir("dd");
        let dir = tmpdir("movie");
        // A user file already sits at the first name: never overwritten.
        std::fs::write(dir.join("Movie.2020.1080p.tr.srt"), "mine").unwrap();
        let http = reqwest::Client::new();
        let j = job(&dir, &[], Some("/subtitles/movie/tt0133093.json"));

        let t = run(&dd, &http, &base, &j, false, 1000).await;
        let labels: Vec<&str> = t.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["Turkish (online)", "Turkish (online) 2"]);
        assert_eq!(std::fs::read_to_string(dir.join("Movie.2020.1080p.tr.srt")).unwrap(), "mine");
        assert_eq!(std::fs::read_to_string(dir.join("Movie.2020.1080p.tr.2.srt")).unwrap(), SRT);
        assert!(dir.join("Movie.2020.1080p.tr.3.srt").is_file());
        assert!(!dir.join("Movie.2020.1080p.en.srt").exists()); // non-SRT body skipped
        assert_eq!(hits.load(Ordering::SeqCst), 4);

        // Owned list + id resolution + VTT.
        let s = load(&dd);
        let owned: Vec<PathBuf> = s.owned[KEY].iter().map(|o| o.path.clone()).collect();
        assert_eq!(owned, [dir.join("Movie.2020.1080p.tr.2.srt"), dir.join("Movie.2020.1080p.tr.3.srt")]);
        assert_eq!(parse_track_id(&t[0].id).unwrap().1, path_hash(&owned[0]));
        assert!(vtt_for_path(&owned[0]).unwrap().starts_with("WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.000\nMerhaba"));

        // Turkish is now covered; English still missing -> one list request, nothing new.
        let t2 = run(&dd, &http, &base, &j, false, 1001).await;
        assert_eq!(t2.len(), 2);
        assert_eq!(hits.load(Ordering::SeqCst), 6); // list + en1 again (en1 is not SRT, no miss cached)
    }

    #[tokio::test]
    async fn negative_cache_metered_and_have() {
        let (base, hits) = mock(vec![("/subtitles/movie/tt1.json", r#"{"subtitles":[]}"#.to_string())]).await;
        let dd = tmpdir("dd");
        let dir = tmpdir("neg");
        let http = reqwest::Client::new();
        let j = job(&dir, &[], Some("/subtitles/movie/tt1.json"));
        assert!(run(&dd, &http, &base, &j, false, 1000).await.is_empty());
        assert!(run(&dd, &http, &base, &j, false, 1000 + 3600).await.is_empty());
        assert_eq!(hits.load(Ordering::SeqCst), 1); // cached miss
        assert!(run(&dd, &http, &base, &j, false, 1000 + NEG_TTL).await.is_empty());
        assert_eq!(hits.load(Ordering::SeqCst), 2); // expired

        // Metered, unmatched, or already-covered languages: no request at all.
        let dd2 = tmpdir("dd");
        run(&dd2, &http, &base, &j, true, 1).await;
        run(&dd2, &http, &base, &job(&dir, &[], None), false, 1).await;
        run(&dd2, &http, &base, &job(&dir, &["tr", "en"], Some("/subtitles/movie/tt1.json")), false, 1).await;
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(!dd2.join("online_subs.json").exists());
    }
}
