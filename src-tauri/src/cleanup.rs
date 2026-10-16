//! Cleanup of watched downloads. Watched flags come from groups.rs (`watched.json`,
//! `"<key>:<file_index>"`); there is no watched timestamp, so age is the files' mtime
//! on disk (≈ when the download finished).
//!
//! - A torrent whose media files are all watched (and which is complete) is offered as a
//!   whole (`file_index: null`) and removed with its files like "Remove + delete files".
//! - Otherwise single watched, fully downloaded media files of a multi-file torrent are
//!   offered one by one; applying deselects them in librqbit (so they are never fetched
//!   again) and deletes the file. If librqbit refuses the deselect, the file is skipped.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::state::AppState;
use crate::types::{FileKind, TorrentState};

type St<'a> = State<'a, Arc<AppState>>;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CleanupCandidate {
    pub key: String,
    /// null = the whole torrent.
    pub file_index: Option<usize>,
    pub name: String,
    pub size: u64,
    /// Unix seconds (newest file mtime).
    pub completed_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupList {
    pub items: Vec<CleanupCandidate>,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CleanupItem {
    pub key: String,
    #[serde(default, alias = "fileIndex")]
    pub file_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupSkip {
    pub key: String,
    pub file_index: Option<usize>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupResult {
    pub removed: Vec<CleanupCandidate>,
    pub skipped: Vec<CleanupSkip>,
    pub freed_bytes: u64,
}

// ---------------------------------------------------------------- pure selection

#[derive(Debug, Clone)]
pub(crate) struct FInfo {
    pub index: usize,
    pub name: String,
    pub size: u64,
    pub done: bool,
    pub media: bool,
    pub mtime: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct TInfo {
    pub key: String,
    pub name: String,
    pub completed: bool,
    pub files: Vec<FInfo>,
}

pub(crate) fn select(
    torrents: &[TInfo],
    watched: &BTreeMap<String, bool>,
    now: u64,
    min_age_days: u64,
) -> Vec<CleanupCandidate> {
    let cutoff = now.saturating_sub(min_age_days.saturating_mul(86_400));
    let is_watched = |key: &str, i: usize| watched.get(&format!("{key}:{i}")).copied().unwrap_or(false);
    let mut out = Vec::new();
    for t in torrents {
        let media: Vec<&FInfo> = t.files.iter().filter(|f| f.media).collect();
        if media.is_empty() {
            continue;
        }
        let all_watched = media.iter().all(|f| is_watched(&t.key, f.index));
        if t.completed && all_watched {
            let newest = t.files.iter().filter_map(|f| f.mtime).max();
            if newest.is_some_and(|m| m <= cutoff) {
                out.push(CleanupCandidate {
                    key: t.key.clone(),
                    file_index: None,
                    name: t.name.clone(),
                    size: t.files.iter().filter(|f| f.mtime.is_some()).map(|f| f.size).sum(),
                    completed_at: newest,
                });
            }
            continue;
        }
        if t.files.len() < 2 {
            continue;
        }
        for f in media {
            if f.done && is_watched(&t.key, f.index) && f.mtime.is_some_and(|m| m <= cutoff) {
                out.push(CleanupCandidate {
                    key: t.key.clone(),
                    file_index: Some(f.index),
                    name: f.name.clone(),
                    size: f.size,
                    completed_at: f.mtime,
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------- gathering

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn mtime(p: &PathBuf) -> Option<u64> {
    let m = std::fs::metadata(p).ok()?;
    m.is_file().then_some(())?;
    m.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn gather(st: &AppState) -> Vec<TInfo> {
    let mut out = Vec::new();
    for row in crate::engine::list_rows(st) {
        if row.id.is_none() {
            continue;
        }
        let Ok(files) = crate::engine::files(st, &row.key) else { continue };
        // Deselected files are not on disk (or not ours to judge).
        let only: Option<HashSet<usize>> = crate::engine::handle(st, &row.key)
            .ok()
            .and_then(|h| h.only_files())
            .map(|v| v.into_iter().collect());
        let files = files
            .into_iter()
            .filter(|f| only.as_ref().is_none_or(|o| o.contains(&f.index)))
            .map(|f| FInfo {
                index: f.index,
                done: f.progress_bytes >= f.size,
                media: matches!(f.kind, FileKind::Video | FileKind::Audio),
                mtime: crate::engine::file_abs_path(st, &row.key, f.index).ok().and_then(|p| mtime(&p)),
                name: f.name,
                size: f.size,
            })
            .collect();
        out.push(TInfo { key: row.key, name: row.name, completed: row.state == TorrentState::Completed, files });
    }
    out
}

pub(crate) fn candidates(st: &AppState, min_age_days: u64) -> Result<Vec<CleanupCandidate>, String> {
    let watched = crate::groups::get_watched()?;
    Ok(select(&gather(st), &watched, now_secs(), min_age_days))
}

// ---------------------------------------------------------------- commands

/// JS: `{ minAgeDays?: number }` (default 30).
#[tauri::command]
pub fn cleanup_candidates(state: St<'_>, min_age_days: Option<u64>) -> Result<CleanupList, String> {
    let items = candidates(&state, min_age_days.unwrap_or(30))?;
    let total_bytes = items.iter().map(|c| c.size).sum();
    Ok(CleanupList { items, total_bytes })
}

/// JS: `{ items: [{ key, file_index|fileIndex: number|null }] }`. Each item must still be
/// a candidate (watched; age not re-checked).
#[tauri::command]
pub async fn cleanup_apply(state: St<'_>, items: Vec<CleanupItem>) -> Result<CleanupResult, String> {
    apply(&state, items).await
}

pub(crate) async fn apply(st: &AppState, items: Vec<CleanupItem>) -> Result<CleanupResult, String> {
    let valid = candidates(st, 0)?;
    let mut res = CleanupResult { removed: vec![], skipped: vec![], freed_bytes: 0 };
    let mut per_torrent: BTreeMap<String, Vec<CleanupCandidate>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for it in items {
        let key = it.key.trim().to_ascii_lowercase();
        if !seen.insert((key.clone(), it.file_index)) {
            continue;
        }
        match valid.iter().find(|c| c.key == key && c.file_index == it.file_index) {
            None => res.skipped.push(CleanupSkip {
                key,
                file_index: it.file_index,
                reason: "not a cleanup candidate (not watched/complete)".into(),
            }),
            Some(c) if c.file_index.is_none() => match crate::engine::remove(st, &key, true).await {
                Ok(()) => {
                    res.freed_bytes += c.size;
                    res.removed.push(c.clone());
                }
                Err(e) => res.skipped.push(CleanupSkip { key, file_index: None, reason: e }),
            },
            Some(c) => per_torrent.entry(key).or_default().push(c.clone()),
        }
    }
    for (key, files) in per_torrent {
        // Whole torrent already removed above.
        if res.removed.iter().any(|r| r.key == key && r.file_index.is_none()) {
            continue;
        }
        let skip_all = |res: &mut CleanupResult, reason: String| {
            for c in &files {
                res.skipped.push(CleanupSkip { key: key.clone(), file_index: c.file_index, reason: reason.clone() });
            }
        };
        let h = match crate::engine::handle(st, &key) {
            Ok(h) => h,
            Err(e) => {
                skip_all(&mut res, e);
                continue;
            }
        };
        let all: Vec<usize> = match crate::engine::files(st, &key) {
            Ok(f) => f.into_iter().map(|f| f.index).collect(),
            Err(e) => {
                skip_all(&mut res, e);
                continue;
            }
        };
        let mut keep: HashSet<usize> = h.only_files().map(|v| v.into_iter().collect()).unwrap_or_else(|| all.into_iter().collect());
        for c in &files {
            keep.remove(&c.file_index.unwrap_or(usize::MAX));
        }
        if keep.is_empty() {
            skip_all(&mut res, "would deselect every file; remove the whole torrent instead".into());
            continue;
        }
        if let Err(e) = st.session.update_only_files(&h, &keep).await {
            skip_all(&mut res, format!("librqbit could not deselect the files: {e:#}"));
            continue;
        }
        let folder = h.output_folder().to_path_buf();
        for c in files {
            let idx = c.file_index.unwrap_or_default();
            let path = match crate::engine::file_abs_path(st, &key, idx) {
                Ok(p) => p,
                Err(e) => {
                    res.skipped.push(CleanupSkip { key: key.clone(), file_index: Some(idx), reason: e });
                    continue;
                }
            };
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    // Drop now-empty subfolders up to (not including) the torrent folder.
                    let mut dir = path.parent().map(|p| p.to_path_buf());
                    while let Some(d) = dir {
                        if d == folder || !d.starts_with(&folder) {
                            break;
                        }
                        let _ = std::fs::remove_file(d.join(".DS_Store"));
                        if std::fs::remove_dir(&d).is_err() {
                            break;
                        }
                        dir = d.parent().map(|p| p.to_path_buf());
                    }
                    res.freed_bytes += c.size;
                    res.removed.push(c);
                }
                Err(e) => res.skipped.push(CleanupSkip {
                    key: key.clone(),
                    file_index: Some(idx),
                    reason: format!("deselected, but deleting {} failed: {e}", path.display()),
                }),
            }
        }
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;
    const NOW: u64 = 1_000 * DAY;

    fn f(index: usize, media: bool, done: bool, age_days: Option<u64>) -> FInfo {
        FInfo { index, name: format!("f{index}"), size: 100, done, media, mtime: age_days.map(|d| NOW - d * DAY) }
    }

    fn w(keys: &[&str]) -> BTreeMap<String, bool> {
        keys.iter().map(|k| (k.to_string(), true)).collect()
    }

    #[test]
    fn whole_torrent_when_all_media_watched_and_old() {
        let t = vec![TInfo { key: "k".into(), name: "Movie".into(), completed: true, files: vec![f(0, true, true, Some(40)), f(1, false, true, Some(40))] }];
        let c = select(&t, &w(&["k:0"]), NOW, 30);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].file_index, None);
        assert_eq!(c[0].size, 200);
        assert_eq!(c[0].completed_at, Some(NOW - 40 * DAY));
        // Too young.
        assert!(select(&t, &w(&["k:0"]), NOW, 60).is_empty());
        // Not watched.
        assert!(select(&t, &w(&[]), NOW, 30).is_empty());
    }

    #[test]
    fn single_episodes_in_pack() {
        let t = vec![TInfo {
            key: "p".into(),
            name: "Show S01".into(),
            completed: false,
            files: vec![
                f(0, true, true, Some(45)),  // watched, old -> yes
                f(1, true, true, Some(5)),   // watched, young -> no
                f(2, true, false, Some(45)), // watched, incomplete -> no
                f(3, true, true, Some(45)),  // not watched -> no
                f(4, false, true, Some(45)), // subtitle -> no
            ],
        }];
        let c = select(&t, &w(&["p:0", "p:1", "p:2", "p:4"]), NOW, 30);
        assert_eq!(c.iter().map(|c| c.file_index).collect::<Vec<_>>(), vec![Some(0)]);
        // Everything watched but torrent incomplete: per-file, not whole.
        let c = select(&t, &w(&["p:0", "p:1", "p:2", "p:3"]), NOW, 30);
        assert_eq!(c.iter().map(|c| c.file_index).collect::<Vec<_>>(), vec![Some(0), Some(3)]);
    }

    #[test]
    fn missing_files_and_no_media() {
        let t = vec![
            TInfo { key: "a".into(), name: "A".into(), completed: true, files: vec![f(0, true, true, None)] },
            TInfo { key: "b".into(), name: "B".into(), completed: true, files: vec![f(0, false, true, Some(90))] },
        ];
        assert!(select(&t, &w(&["a:0", "b:0"]), NOW, 30).is_empty());
    }

    #[test]
    fn item_accepts_camel_case() {
        let i: CleanupItem = serde_json::from_str(r#"{"key":"k","fileIndex":3}"#).unwrap();
        assert_eq!(i.file_index, Some(3));
        let i: CleanupItem = serde_json::from_str(r#"{"key":"k","file_index":null}"#).unwrap();
        assert_eq!(i.file_index, None);
    }
}
