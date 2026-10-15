//! Local poster artwork: picks a poster/cover image shipped inside a torrent (or a
//! sidecar next to a single-file torrent) and returns it as a `data:` URL.
//! No network access beyond the torrent itself.
//!
//! Command: `get_artwork { key }` -> `{ poster: string|null, kind: "poster"|"backdrop"|null }`.
//! Errors: `NOT_READY` while metadata resolves / the image is still downloading.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::State;

use crate::engine;
use crate::state::AppState;
use crate::types::TorrentState;

type St<'a> = State<'a, Arc<AppState>>;

/// Images larger than this are ignored (also the `read_file` cap).
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
/// How long a "no artwork" answer is cached.
const NEGATIVE_TTL: Duration = Duration::from_secs(60);
const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "webp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtworkKind {
    Poster,
    Backdrop,
}

#[derive(Debug, Clone, Serialize)]
pub struct Artwork {
    /// `data:image/...;base64,...`
    pub poster: Option<String>,
    pub kind: Option<ArtworkKind>,
}

impl Artwork {
    fn none() -> Self {
        Artwork { poster: None, kind: None }
    }
}

enum Cached {
    Found(Artwork),
    Missing(Instant),
}

static CACHE: LazyLock<Mutex<HashMap<String, Cached>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------- ranking

/// A file candidate for ranking: (torrent file index, path with "/" separators, size).
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub index: usize,
    pub name: &'a str,
    pub size: u64,
}

fn ext_lower(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_image(name: &str) -> bool {
    IMAGE_EXT.contains(&ext_lower(name).as_str())
}

/// Lowercase alphanumerics only ("The.Movie (2020)" -> "themovie2020").
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn stem_lower(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    Path::new(base)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Normalized title vs image stem: equal, or one a prefix of the other
/// ("themovie2020" vs "themovie2020poster…", "themovie") — min 4 chars to avoid "the".
fn title_match(title: &str, stem: &str) -> bool {
    if title.is_empty() || stem.is_empty() {
        return false;
    }
    title == stem
        || (stem.len() >= 4 && title.len() >= 4 && (title.starts_with(stem) || stem.starts_with(title)))
}

/// Score an image path; higher is better. None = not usable.
/// `titles` are normalized movie titles (torrent name, video stems).
fn score(name: &str, size: u64, titles: &[String]) -> Option<(u32, ArtworkKind)> {
    if !is_image(name) || size == 0 || size > MAX_IMAGE_BYTES {
        return None;
    }
    let lower = name.to_lowercase();
    // macOS resource forks / junk folders.
    if lower.split('/').any(|c| c.starts_with("._") || c == "__macosx") {
        return None;
    }
    let stem = stem_lower(name);
    let has = |w: &str| stem.contains(w);
    let low_value = ["screen", "sample", "thumb", "snapshot", "capture"]
        .iter()
        .any(|w| has(w));

    let (s, kind) = if has("poster") {
        (100, ArtworkKind::Poster)
    } else if has("cover") {
        (90, ArtworkKind::Poster)
    } else if stem == "folder" {
        (80, ArtworkKind::Poster)
    } else if stem == "movie"
        || (!low_value && {
            let n = norm(&stem);
            titles.iter().any(|t| title_match(t, &n))
        })
    {
        (70, ArtworkKind::Poster)
    } else if has("fanart") || has("backdrop") || has("background") {
        (40, ArtworkKind::Backdrop)
    } else if low_value {
        (5, ArtworkKind::Poster)
    } else {
        (20, ArtworkKind::Poster)
    };
    Some((s, kind))
}

/// Pick the best artwork among `files`. `torrent_name` helps recognise title-named images.
/// Ties: shallower path first, then larger file.
pub fn pick_best(files: &[Candidate<'_>], torrent_name: &str) -> Option<(usize, ArtworkKind)> {
    let mut titles: Vec<String> = vec![norm(torrent_name)];
    for f in files {
        let e = ext_lower(f.name);
        if ["mkv", "mp4", "m4v", "avi", "mov", "webm", "wmv", "ts"].contains(&e.as_str()) {
            titles.push(norm(&stem_lower(f.name)));
        }
    }
    files
        .iter()
        .filter_map(|f| score(f.name, f.size, &titles).map(|(s, k)| (f, s, k)))
        .max_by(|(a, sa, _), (b, sb, _)| {
            sa.cmp(sb)
                .then_with(|| b.name.matches('/').count().cmp(&a.name.matches('/').count()))
                .then_with(|| a.size.cmp(&b.size))
                // Deterministic: lower index wins on a full tie.
                .then_with(|| b.index.cmp(&a.index))
        })
        .map(|(f, _, k)| (f.index, k))
}

/// Sidecar images next to a single-file torrent: `<stem>-poster.jpg`, `<stem>.jpg`, …
/// and — only when the file lives in its own folder (not the shared download dir) —
/// `poster.jpg` / `cover.jpg` / `folder.jpg`. (Tide's own `<stem>-poster.jpg` sidecar is
/// preferred by the UI via `get_media_image` anyway.)
fn sidecar_candidates(file: &Path, download_dir: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (file.parent(), file.file_stem()) else {
        return vec![];
    };
    let stem = stem.to_string_lossy();
    let mut names: Vec<String> = Vec::new();
    for ext in IMAGE_EXT {
        names.push(format!("{stem}-poster.{ext}"));
        names.push(format!("{stem}.poster.{ext}"));
        names.push(format!("{stem}.{ext}"));
    }
    // Shared folders: ~/Downloads/Tidebox, and ~/Downloads (torrents added before Tidebox).
    if dir != download_dir && Some(dir) != download_dir.parent() {
        for base in ["poster", "cover", "folder"] {
            for ext in IMAGE_EXT {
                names.push(format!("{base}.{ext}"));
            }
        }
    }
    names.into_iter().map(|n| dir.join(n)).collect()
}

fn mime(name: &str) -> &'static str {
    match ext_lower(name).as_str() {
        "png" => "image/png",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
}

pub(crate) fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn data_url(name: &str, bytes: &[u8]) -> String {
    format!("data:{};base64,{}", mime(name), base64(bytes))
}

// ---------------------------------------------------------------- lookup

pub async fn pick_artwork(st: &AppState, key: &str) -> Result<Artwork, String> {
    let files = engine::files(st, key)?; // NOT_READY while resolving
    let row = engine::list_rows(st).into_iter().find(|r| r.key == key);
    let torrent_name = row.as_ref().map(|r| r.name.clone()).unwrap_or_default();

    let cands: Vec<Candidate> = files
        .iter()
        .map(|f| Candidate { index: f.index, name: &f.name, size: f.size })
        .collect();

    if let Some((idx, kind)) = pick_best(&cands, &torrent_name) {
        let f = files.iter().find(|f| f.index == idx).expect("picked from files");
        let bytes = if f.progress_bytes >= f.size {
            let p = engine::file_abs_path(st, key, idx)?;
            tokio::fs::read(&p)
                .await
                .map_err(|e| format!("INTERNAL: read {}: {e}", p.display()))?
        } else {
            // Don't resume a torrent the user paused just to fetch artwork.
            if row.as_ref().is_some_and(|r| r.state == TorrentState::Paused) {
                return Err("NOT_READY: torrent is paused; artwork not downloaded yet".into());
            }
            engine::read_file(st, key, idx, MAX_IMAGE_BYTES as usize, 30).await?
        };
        return Ok(Artwork { poster: Some(data_url(&f.name, &bytes)), kind: Some(kind) });
    }

    // Single-file torrent: look for sidecar images on disk.
    if files.len() == 1 {
        let file = engine::file_abs_path(st, key, files[0].index)?;
        for p in sidecar_candidates(&file, &st.download_dir) {
            let Ok(meta) = tokio::fs::metadata(&p).await else { continue };
            if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_IMAGE_BYTES {
                continue;
            }
            if let Ok(bytes) = tokio::fs::read(&p).await {
                let name = p.to_string_lossy();
                return Ok(Artwork { poster: Some(data_url(&name, &bytes)), kind: Some(ArtworkKind::Poster) });
            }
        }
    }
    Ok(Artwork::none())
}

#[tauri::command]
pub async fn get_artwork(state: St<'_>, key: String) -> Result<Artwork, String> {
    let key = key.trim().to_ascii_lowercase();
    {
        let cache = CACHE.lock().unwrap();
        match cache.get(&key) {
            Some(Cached::Found(a)) => return Ok(a.clone()),
            Some(Cached::Missing(t)) if t.elapsed() < NEGATIVE_TTL => return Ok(Artwork::none()),
            _ => {}
        }
    }
    let st: Arc<AppState> = state.inner().clone();
    let art = pick_artwork(&st, &key).await?;
    let entry = if art.poster.is_some() {
        Cached::Found(art.clone())
    } else {
        Cached::Missing(Instant::now())
    };
    CACHE.lock().unwrap().insert(key, entry);
    Ok(art)
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(list: &[(&str, u64)], name: &str) -> Option<(String, ArtworkKind)> {
        let c: Vec<Candidate> = list
            .iter()
            .enumerate()
            .map(|(i, (n, s))| Candidate { index: i, name: n, size: *s })
            .collect();
        pick_best(&c, name).map(|(i, k)| (list[i].0.to_string(), k))
    }

    #[test]
    fn sintel_picks_poster() {
        let files = [
            ("Sintel/Sintel.de.srt", 1652),
            ("Sintel/Sintel.en.srt", 1514),
            ("Sintel/Sintel.es.srt", 1554),
            ("Sintel/Sintel.fr.srt", 1618),
            ("Sintel/Sintel.it.srt", 1546),
            ("Sintel/Sintel.mp4", 129_241_752),
            ("Sintel/Sintel.nl.srt", 1537),
            ("Sintel/Sintel.pl.srt", 1536),
            ("Sintel/Sintel.pt.srt", 1551),
            ("Sintel/Sintel.ru.srt", 2016),
            ("Sintel/poster.jpg", 46_115),
        ];
        let (n, k) = pick(&files, "Sintel").unwrap();
        assert_eq!(n, "Sintel/poster.jpg");
        assert_eq!(k, ArtworkKind::Poster);
    }

    #[test]
    fn priority_order() {
        let base = [
            ("M/screenshot1.png", 900_000),
            ("M/random.jpg", 100_000),
            ("M/fanart.jpg", 800_000),
            ("M/The.Movie.2020.jpg", 300_000),
            ("M/The.Movie.2020.mkv", 2_000_000_000),
            ("M/folder.jpg", 200_000),
            ("M/Cover.JPEG", 200_000),
            ("M/POSTER.png", 200_000),
        ];
        let order = [
            "M/POSTER.png",
            "M/Cover.JPEG",
            "M/folder.jpg",
            "M/The.Movie.2020.jpg",
            "M/fanart.jpg",
            "M/random.jpg",
            "M/screenshot1.png",
        ];
        let mut list: Vec<(&str, u64)> = base.to_vec();
        for want in order {
            assert_eq!(pick(&list, "The Movie (2020)").unwrap().0, want);
            list.retain(|(n, _)| *n != want);
        }
        assert!(pick(&list, "The Movie (2020)").is_none());
    }

    #[test]
    fn backdrop_kind_and_exclusions() {
        let files = [("X/backdrop.jpg", 500_000), ("X/movie.mkv", 1 << 30)];
        assert_eq!(pick(&files, "X").unwrap().1, ArtworkKind::Backdrop);

        // Too large, junk, non-images -> none.
        let files = [
            ("X/huge.jpg", 6 * 1024 * 1024),
            ("X/__MACOSX/._poster.jpg", 4096),
            ("X/._poster.jpg", 4096),
            ("X/poster.gif", 1000),
            ("X/notes.txt", 10),
            ("X/empty.png", 0),
        ];
        assert!(pick(&files, "X").is_none());
    }

    #[test]
    fn shallower_wins_tie() {
        let files = [("A/extras/poster.jpg", 100), ("A/poster.jpg", 100)];
        assert_eq!(pick(&files, "A").unwrap().0, "A/poster.jpg");
        let files = [("A/thumb.jpg", 10), ("A/sample.png", 20)];
        assert_eq!(pick(&files, "A").unwrap().0, "A/sample.png");
    }

    #[test]
    fn sidecars() {
        let dl = Path::new("/dl");
        let own = sidecar_candidates(Path::new("/dl/Film.mkv"), dl);
        assert!(own.contains(&PathBuf::from("/dl/Film-poster.jpg")));
        assert!(!own.contains(&PathBuf::from("/dl/poster.jpg")));
        let sub = sidecar_candidates(Path::new("/dl/Film/Film.mkv"), dl);
        assert!(sub.contains(&PathBuf::from("/dl/Film/poster.jpg")));
    }

    #[test]
    fn base64_matches() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(data_url("a.PNG", b"hi"), "data:image/png;base64,aGk=");
    }
}
