//! "Next episode" for the player: offline, from the torrents already in Tide.
//!
//! The next (season, episode) after the playing file, for the same show: inside the
//! same torrent (season packs, via `metadata::episodes_of`) and in other torrents of the
//! same show (same IMDb id from the `tide.json` sidecar, else the same normalised parsed
//! name). Only files that can play now: complete on disk, or in a torrent that is
//! downloading (streamable).

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::engine;
use crate::metadata;
use crate::state::AppState;
use crate::types::{EpisodeFile, FileRow, MediaInfo, MediaRef, TorrentState};

type St<'a> = State<'a, Arc<AppState>>;

/// Return value of `next_episode`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NextEpisode {
    pub media: MediaRef,
    /// File base name (what the player shows as its title).
    pub title: String,
    /// "S04E08 · Title".
    pub label: String,
    /// Plays in the builtin player (else only a VLC button is offered).
    pub builtin: bool,
}

/// One torrent of the library, as far as next-episode selection cares.
#[derive(Debug, Clone)]
struct Lib {
    key: String,
    name: String,
    state: TorrentState,
    files: Vec<FileRow>,
    /// Matched IMDb id (None when unmatched / no sidecar).
    imdb: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SidecarInfo {
    info: MediaInfo,
}

/// Matched media info from the torrent's `tide.json` (disk only, never the network).
fn sidecar_info(st: &AppState, key: &str) -> Option<MediaInfo> {
    let (paths, _) = metadata::sidecar_paths(st, key);
    let p = paths.into_iter().find(|p| p.to_string_lossy().ends_with("tide.json"))?;
    let b = std::fs::read(p).ok()?;
    serde_json::from_slice::<SidecarInfo>(&b).ok().map(|s| s.info)
}

fn imdb_of(info: Option<&MediaInfo>) -> Option<String> {
    info.filter(|i| !i.needs_match && metadata::valid_tt(&i.imdb_id)).map(|i| i.imdb_id.clone())
}

fn se(e: &EpisodeFile) -> (u32, u32) {
    (e.season.unwrap_or(1), e.episode)
}

fn norms(lib: &Lib, eps: &[EpisodeFile]) -> HashSet<String> {
    let mut s: HashSet<String> = eps.iter().map(|e| e.norm.clone()).collect();
    s.insert(metadata::parsed_name(&lib.name).norm);
    s.remove("");
    s
}

fn playable(lib: &Lib, ep: &EpisodeFile) -> bool {
    ep.progress >= 1.0 || lib.state == TorrentState::Downloading
}

/// Pick (index into `libs`, episode) of the next episode after `cur_key`/`cur_idx`.
/// Smallest (season, episode) greater than the current one; ties prefer a complete
/// file, then the same torrent.
fn pick(libs: &[Lib], cur_key: &str, cur_idx: usize) -> Option<(usize, EpisodeFile)> {
    let cur = libs.iter().find(|l| l.key == cur_key)?;
    let cur_eps = metadata::episodes_of(&cur.name, &cur.files);
    let cur_ep = cur_eps.iter().find(|e| e.file_index == cur_idx)?;
    let cur_se = se(cur_ep);
    let mut cur_norms = norms(cur, &cur_eps);
    if !cur_ep.norm.is_empty() {
        cur_norms.insert(cur_ep.norm.clone());
    }

    let mut best: Option<((u32, u32, bool, bool), usize, EpisodeFile)> = None;
    for (i, lib) in libs.iter().enumerate() {
        let same = lib.key == cur.key;
        let eps = if same { cur_eps.clone() } else { metadata::episodes_of(&lib.name, &lib.files) };
        if eps.is_empty() {
            continue;
        }
        if !same {
            let show = match (&cur.imdb, &lib.imdb) {
                (Some(a), Some(b)) => a == b,
                _ => !norms(lib, &eps).is_disjoint(&cur_norms),
            };
            if !show {
                continue;
            }
        }
        for ep in eps {
            if se(&ep) <= cur_se || !playable(lib, &ep) {
                continue;
            }
            let (s, e) = se(&ep);
            let rank = (s, e, ep.progress < 1.0, !same);
            if best.as_ref().is_none_or(|(r, _, _)| rank < *r) {
                best = Some((rank, i, ep));
            }
        }
    }
    best.map(|(_, i, ep)| (i, ep))
}

fn label_for(ep: &EpisodeFile, info: Option<&MediaInfo>) -> String {
    let code = match ep.season {
        Some(s) => format!("S{s:02}E{:02}", ep.episode),
        None => format!("E{:02}", ep.episode),
    };
    let ep_title = info
        .filter(|i| i.episode == Some(ep.episode) && (ep.season.is_none() || i.season == ep.season))
        .and_then(|i| i.episode_title.clone())
        .filter(|t| !t.trim().is_empty());
    let show = info
        .filter(|i| !i.needs_match && !i.title.trim().is_empty())
        .map(|i| i.title.clone())
        .unwrap_or_else(|| ep.title_hint.clone());
    let name = ep_title.unwrap_or(show);
    if name.trim().is_empty() {
        code
    } else {
        format!("{code} · {name}")
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// JS: `{ media }` -> `NextEpisode | null`. Offline (engine state + sidecars on disk).
#[tauri::command]
pub fn next_episode(state: St<'_>, media: MediaRef) -> Result<Option<NextEpisode>, String> {
    let MediaRef::Torrent { key, file_index } = media else {
        return Ok(None);
    };
    let key = key.trim().to_ascii_lowercase();
    let st: &AppState = &state;
    let mut libs: Vec<Lib> = engine::list_rows(st)
        .into_iter()
        .filter(|r| r.state != TorrentState::Error || r.key == key)
        .filter_map(|r| {
            let files = engine::files(st, &r.key).ok()?;
            Some(Lib { key: r.key, name: r.name, state: r.state, files, imdb: None })
        })
        .collect();
    // Sidecars only for torrents that contain episodes at all.
    for l in &mut libs {
        if !metadata::episodes_of(&l.name, &l.files).is_empty() {
            l.imdb = imdb_of(sidecar_info(st, &l.key).as_ref());
        }
    }
    let Some((i, ep)) = pick(&libs, &key, file_index) else {
        return Ok(None);
    };
    let lib = &libs[i];
    let Some(row) = lib.files.iter().find(|f| f.index == ep.file_index) else {
        return Ok(None);
    };
    let info = sidecar_info(st, &lib.key);
    Ok(Some(NextEpisode {
        media: MediaRef::Torrent { key: lib.key.clone(), file_index: ep.file_index },
        title: basename(&row.name).to_string(),
        label: label_for(&ep, info.as_ref()),
        builtin: row.playable_builtin,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FileKind;

    fn f(index: usize, name: &str, progress: f64) -> FileRow {
        FileRow {
            index,
            name: name.into(),
            size: 100,
            progress_bytes: (progress * 100.0) as u64,
            progress,
            kind: FileKind::Video,
            playable_builtin: true,
        }
    }

    fn lib(key: &str, name: &str, state: TorrentState, files: Vec<FileRow>, imdb: Option<&str>) -> Lib {
        Lib { key: key.into(), name: name.into(), state, files, imdb: imdb.map(Into::into) }
    }

    fn pack() -> Lib {
        lib(
            "a",
            "Breaking.Bad.S04.1080p.BluRay.x264-GRP",
            TorrentState::Completed,
            vec![
                f(0, "Breaking.Bad.S04.1080p/Breaking.Bad.S04E07.1080p.mkv", 1.0),
                f(1, "Breaking.Bad.S04.1080p/Breaking.Bad.S04E08.1080p.mkv", 1.0),
                f(2, "Breaking.Bad.S04.1080p/Breaking.Bad.S04E09.1080p.mkv", 1.0),
                f(3, "Breaking.Bad.S04.1080p/Sample/sample.mkv", 1.0),
            ],
            Some("tt0903747"),
        )
    }

    #[test]
    fn next_inside_season_pack() {
        let libs = vec![pack()];
        let (i, ep) = pick(&libs, "a", 0).unwrap();
        assert_eq!((i, ep.file_index, ep.season, ep.episode), (0, 1, Some(4), 8));
        assert_eq!(pick(&libs, "a", 2), None, "last episode has no next");
        assert_eq!(pick(&libs, "a", 3), None, "sample is not an episode");
    }

    #[test]
    fn next_from_other_torrent_by_imdb_or_name() {
        let mut p = pack();
        p.files.truncate(1); // only E07
        let other = lib(
            "b",
            "Breaking Bad S04E08 720p WEB",
            TorrentState::Completed,
            vec![f(0, "Breaking Bad S04E08 720p WEB.mp4", 1.0)],
            Some("tt0903747"),
        );
        let unrelated = lib(
            "c",
            "Better.Call.Saul.S04E08.1080p",
            TorrentState::Completed,
            vec![f(0, "Better.Call.Saul.S04E08.1080p.mkv", 1.0)],
            Some("tt3032476"),
        );
        let libs = vec![p.clone(), unrelated.clone(), other.clone()];
        assert_eq!(pick(&libs, "a", 0).map(|(i, e)| (i, e.episode)), Some((2, 8)));

        // No IMDb ids: falls back to the normalised parsed name.
        let mut p2 = p.clone();
        p2.imdb = None;
        let mut o2 = other.clone();
        o2.imdb = None;
        let mut u2 = unrelated.clone();
        u2.imdb = None;
        let libs = vec![p2, u2, o2];
        assert_eq!(pick(&libs, "a", 0).map(|(i, e)| (i, e.episode)), Some((2, 8)));

        // Different IMDb ids never match, even with the same name.
        let mut o3 = other;
        o3.imdb = Some("tt0000001".into());
        assert_eq!(pick(&[p, o3], "a", 0), None);
    }

    #[test]
    fn only_playable_files_and_season_rollover() {
        let p = pack();
        // E08 only in a paused, incomplete torrent: not playable; a downloading one is.
        let mut cur = p.clone();
        cur.files.truncate(1);
        let paused = lib(
            "b",
            "Breaking.Bad.S04E08.720p",
            TorrentState::Paused,
            vec![f(0, "Breaking.Bad.S04E08.720p.mkv", 0.3)],
            None,
        );
        let s5 = lib(
            "c",
            "Breaking.Bad.S05E01.720p",
            TorrentState::Downloading,
            vec![f(0, "Breaking.Bad.S05E01.720p.mkv", 0.05)],
            None,
        );
        let libs = vec![cur.clone(), paused.clone(), s5];
        let (i, ep) = pick(&libs, "a", 0).unwrap();
        assert_eq!((i, ep.season, ep.episode), (2, Some(5), 1), "skips the paused E08");

        let mut live = paused;
        live.state = TorrentState::Downloading;
        let libs = vec![cur, live];
        assert_eq!(pick(&libs, "a", 0).map(|(i, e)| (i, e.episode)), Some((1, 8)));
    }

    #[test]
    fn tie_prefers_complete_then_same_torrent() {
        let p = pack();
        let dup = lib(
            "b",
            "Breaking.Bad.S04E08.2160p",
            TorrentState::Completed,
            vec![f(0, "Breaking.Bad.S04E08.2160p.mkv", 1.0)],
            Some("tt0903747"),
        );
        assert_eq!(pick(&[dup.clone(), p.clone()], "a", 0).map(|(i, _)| i), Some(1));
        let mut half = p.clone();
        half.state = TorrentState::Downloading;
        half.files[1].progress = 0.4;
        assert_eq!(pick(&[half, dup], "a", 0).map(|(i, _)| i), Some(1));
    }

    #[test]
    fn labels() {
        let p = pack();
        let ep = metadata::episodes_of(&p.name, &p.files)[1].clone();
        assert_eq!(label_for(&ep, None), "S04E08 · Breaking Bad");
        let info = MediaInfo {
            imdb_id: "tt0903747".into(),
            title: "Breaking Bad".into(),
            season: Some(4),
            episode: Some(8),
            episode_title: Some("Hermanos".into()),
            ..Default::default()
        };
        assert_eq!(label_for(&ep, Some(&info)), "S04E08 · Hermanos");
        let pack_info = MediaInfo { episode: None, episode_title: None, ..info };
        assert_eq!(label_for(&ep, Some(&pack_info)), "S04E08 · Breaking Bad");
    }
}
