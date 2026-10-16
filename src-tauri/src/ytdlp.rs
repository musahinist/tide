//! Video-site downloads through yt-dlp (v2, docs/V2-YOUTUBE.md).
//!
//! Every download goes to `~/Downloads/Tidebox/<clean title>/<title> [<id>].<ext>`, like
//! torrents. Jobs are persisted minimally to `data_dir/yt-jobs.json`; a job that was
//! running when the app quit comes back as `error: interrupted`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

use crate::engine;
use crate::state::AppState;
use crate::types::{YtJob, YtJobState, YtPreset, YtProbe};

type St<'a> = State<'a, Arc<AppState>>;

const JOBS_FILE: &str = "yt-jobs.json";
/// `yt-jobs-updated` is emitted at most this often (≤ 4/s).
const EMIT_EVERY: Duration = Duration::from_millis(250);
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);
/// How long `yt_remove` waits for a cancelled process before deleting its folder.
const KILL_WAIT: Duration = Duration::from_secs(10);
/// GUI apps start without a useful PATH; yt-dlp may need ffmpeg/ffprobe/deno from it.
const CHILD_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
const PROGRESS_TEMPLATE: &str = "download:[dl] %(progress.downloaded_bytes)s %(progress.total_bytes)s \
     %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s";

/// `AppState.yt` (Default).
#[derive(Default)]
pub struct YtState {
    inner: Mutex<Inner>,
    save_lock: Mutex<()>,
    dirty: AtomicBool,
    emitter_started: AtomicBool,
}

#[derive(Default)]
struct Inner {
    loaded: bool,
    next: u64,
    jobs: Vec<Job>,
    /// url -> last probe (yt_download reuses it instead of querying again).
    probes: HashMap<String, YtProbe>,
}

struct Job {
    info: YtJob,
    /// The job's own folder inside Tidebox (created by us).
    dir: Option<PathBuf>,
    cancel: Option<Arc<Notify>>,
    task: Option<tauri::async_runtime::JoinHandle<()>>,
}

/// One entry of `yt-jobs.json`.
#[derive(Serialize, Deserialize)]
struct Saved {
    #[serde(flatten)]
    job: YtJob,
    #[serde(default)]
    dir: Option<PathBuf>,
}

fn internal<E: std::fmt::Display>(e: E) -> String {
    format!("INTERNAL: {e}")
}

fn lock(st: &AppState) -> std::sync::MutexGuard<'_, Inner> {
    st.yt.inner.lock().unwrap_or_else(|e| e.into_inner())
}

fn is_running(s: YtJobState) -> bool {
    matches!(s, YtJobState::Queued | YtJobState::Downloading | YtJobState::Postprocessing)
}

// ---------------------------------------------------------------- binaries

fn candidates(name: &str, home: Option<&Path>) -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/opt/homebrew/bin").join(name),
        PathBuf::from("/usr/local/bin").join(name),
    ];
    if let Some(h) = home {
        v.push(h.join(".local/bin").join(name));
    }
    v
}

fn find_bin(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    candidates(name, home.as_deref()).into_iter().find(|p| p.is_file())
}

pub fn find_ytdlp() -> Result<PathBuf, String> {
    find_bin("yt-dlp").ok_or_else(|| "YTDLP_NOT_FOUND: yt-dlp is not installed (brew install yt-dlp)".into())
}

fn command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env("PATH", CHILD_PATH)
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

fn valid_url(url: &str) -> Result<(), String> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let ok = (lower.starts_with("https://") && url.len() > 8 || lower.starts_with("http://") && url.len() > 7)
        && url.len() < 4096
        && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    if ok {
        Ok(())
    } else {
        Err("INVALID_INPUT: not an http(s) URL".into())
    }
}

// ---------------------------------------------------------------- args

fn preset_args(preset: YtPreset) -> Vec<String> {
    let h = match preset {
        YtPreset::P720 => 720,
        YtPreset::P480 => 480,
        YtPreset::P360 => 360,
        YtPreset::AudioMp3 => {
            return ["-f", "ba/b", "-x", "--audio-format", "mp3"].map(String::from).to_vec();
        }
    };
    // h264 + m4a first so the builtin (WebKit) player can play the result.
    vec![
        "-f".into(),
        // `<=?` keeps formats of unknown height (direct files, some sites); never above `h`.
        format!("bv*[height<=?{h}]+ba/b[height<=?{h}]"),
        "-S".into(),
        format!("vcodec:h264,res:{h},acodec:m4a"),
        "--merge-output-format".into(),
        "mp4".into(),
    ]
}

fn download_args(preset: YtPreset, dir: &Path, ffmpeg: Option<&Path>, url: &str) -> Vec<String> {
    let mut a: Vec<String> = [
        "--no-playlist",
        "--newline",
        "--progress",
        "--no-simulate",
        "--print",
        "after_move:[done] %(filepath)s",
        // --print implies --quiet, which would hide "[download] Destination" / "[Merger]".
        "--no-quiet",
        "--progress-template",
        PROGRESS_TEMPLATE,
        "-o",
    ]
    .map(String::from)
    .to_vec();
    a.push(dir.join("%(title)s [%(id)s].%(ext)s").to_string_lossy().into_owned());
    if let Some(f) = ffmpeg {
        a.push("--ffmpeg-location".into());
        a.push(f.to_string_lossy().into_owned());
    }
    a.extend(preset_args(preset));
    a.push("--".into());
    a.push(url.into());
    a
}

// ---------------------------------------------------------------- probe

fn parse_probe(url: &str, v: &serde_json::Value) -> Result<YtProbe, String> {
    if v.get("_type").and_then(|t| t.as_str()) == Some("playlist") {
        return Err("INVALID_INPUT: playlists are not supported; open a single video".into());
    }
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::trim).filter(|x| !x.is_empty()).map(String::from);
    let title = s("title").or_else(|| s("fulltitle")).or_else(|| s("id")).unwrap_or_else(|| "Video".into());
    let thumbnail = s("thumbnail").or_else(|| {
        v.get("thumbnails")?
            .as_array()?
            .iter()
            .rev()
            .find_map(|t| t.get("url")?.as_str().map(String::from))
    });
    let live_status = s("live_status");
    let is_live = v.get("is_live").and_then(|x| x.as_bool()).unwrap_or(false)
        || matches!(live_status.as_deref(), Some("is_live" | "is_upcoming"));
    let max_height = v
        .get("formats")
        .and_then(|f| f.as_array())
        .and_then(|fs| {
            fs.iter()
                .filter(|f| f.get("vcodec").and_then(|c| c.as_str()) != Some("none"))
                .filter_map(|f| f.get("height")?.as_u64())
                .max()
        })
        .or_else(|| v.get("height").and_then(|x| x.as_u64()))
        .map(|h| h as u32);
    Ok(YtProbe {
        url: url.to_string(),
        title,
        uploader: s("uploader").or_else(|| s("channel")).or_else(|| s("uploader_id")),
        duration_secs: v.get("duration").and_then(|d| d.as_f64()),
        thumbnail,
        is_live,
        max_height,
    })
}

/// Last meaningful stderr line ("ERROR: " stripped); an ERROR line wins over warnings.
fn error_line(stderr: &str) -> Option<String> {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let pick = lines.iter().rev().find(|l| l.starts_with("ERROR:")).or(lines.last())?;
    Some(pick.strip_prefix("ERROR:").unwrap_or(pick).trim().to_string())
}

async fn probe(bin: &Path, url: &str) -> Result<YtProbe, String> {
    let mut cmd = command(bin);
    cmd.args(["-J", "--no-playlist", "--no-warnings", "--", url]);
    let out = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "YTDLP_ERROR: timed out reading video info".to_string())?
        .map_err(|e| internal(format!("failed to start yt-dlp: {e}")))?;
    if !out.status.success() {
        let msg = error_line(&String::from_utf8_lossy(&out.stderr)).unwrap_or_else(|| format!("yt-dlp exited with {}", out.status));
        return Err(format!("YTDLP_ERROR: {msg}"));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("YTDLP_ERROR: unreadable video info: {e}"))?;
    parse_probe(url, &v)
}

// ---------------------------------------------------------------- progress parsing

#[derive(Debug, Clone, PartialEq)]
enum Line {
    /// `[dl] downloaded total total_estimate speed eta` (`NA` -> None).
    Progress { downloaded: Option<u64>, total: Option<u64>, speed: Option<f64>, eta: Option<u64> },
    /// `[download] Destination: …` — a new stream (video, then audio) starts.
    Destination,
    Postprocessing,
    Done(String),
    Other,
}

fn num(s: &str) -> Option<f64> {
    s.parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0)
}

fn classify(line: &str) -> Line {
    let line = line.trim_end_matches(['\r', '\n']);
    if let Some(rest) = line.strip_prefix("[dl] ") {
        let f: Vec<&str> = rest.split_whitespace().collect();
        let get = |i: usize| f.get(i).and_then(|x| num(x));
        return Line::Progress {
            downloaded: get(0).map(|x| x as u64),
            total: get(1).or(get(2)).map(|x| x as u64),
            speed: get(3),
            eta: get(4).map(|x| x as u64),
        };
    }
    if let Some(p) = line.strip_prefix("[done] ") {
        return Line::Done(p.to_string());
    }
    if line.starts_with("[download] Destination:") {
        return Line::Destination;
    }
    if ["[Merger]", "[ExtractAudio]", "[FixupM3u8]", "[Fixup", "[VideoConvertor]"].iter().any(|p| line.starts_with(p)) {
        return Line::Postprocessing;
    }
    Line::Other
}

/// Live view of a running download (merged over its video + audio streams).
#[derive(Debug, Clone, Default, PartialEq)]
struct Snap {
    post: bool,
    downloaded: u64,
    total: Option<u64>,
    speed: Option<f64>,
    eta: Option<u64>,
    file: Option<String>,
}

#[derive(Default)]
struct Acc {
    /// Bytes of streams already finished.
    done: u64,
    cur: u64,
    cur_total: Option<u64>,
    snap: Snap,
}

impl Acc {
    /// Feed one stdout line; true when the snapshot changed.
    fn feed(&mut self, line: &str) -> bool {
        let before = self.snap.clone();
        match classify(line) {
            Line::Progress { downloaded, total, speed, eta } => {
                self.cur = downloaded.unwrap_or(self.cur);
                self.cur_total = total.or(self.cur_total);
                self.snap.speed = speed;
                self.snap.eta = eta;
            }
            Line::Destination => {
                if self.cur > 0 {
                    self.done += self.cur_total.unwrap_or(self.cur).max(self.cur);
                }
                self.cur = 0;
                self.cur_total = None;
            }
            Line::Postprocessing => {
                self.snap.post = true;
                self.snap.speed = None;
                self.snap.eta = None;
            }
            Line::Done(p) => self.snap.file = Some(p),
            Line::Other => {}
        }
        self.snap.downloaded = self.done + self.cur;
        self.snap.total = self.cur_total.map(|t| self.done + t.max(self.cur));
        self.snap != before
    }
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Ok(Option<String>),
    Failed(String),
    Cancelled,
}

/// Kill yt-dlp and its children (ffmpeg): it runs in its own process group.
async fn kill_group(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
}

/// Run a yt-dlp download command, reporting progress through `on`, until it exits or
/// `cancel` is notified.
async fn drive(mut cmd: Command, cancel: Arc<Notify>, mut on: impl FnMut(&Snap)) -> Outcome {
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(format!("failed to start yt-dlp: {e}")),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        // Only the tail matters.
        let s = String::from_utf8_lossy(&buf[buf.len().saturating_sub(16 * 1024)..]).into_owned();
        s
    });
    let mut lines = BufReader::new(stdout).lines();
    let mut acc = Acc::default();
    loop {
        tokio::select! {
            _ = cancel.notified() => {
                kill_group(&mut child).await;
                err_task.abort();
                return Outcome::Cancelled;
            }
            l = lines.next_line() => match l {
                Ok(Some(l)) => {
                    if acc.feed(&l) {
                        on(&acc.snap);
                    }
                }
                _ => break,
            }
        }
    }
    let status = tokio::select! {
        _ = cancel.notified() => {
            kill_group(&mut child).await;
            err_task.abort();
            return Outcome::Cancelled;
        }
        s = child.wait() => s,
    };
    let stderr = err_task.await.unwrap_or_default();
    match status {
        Ok(s) if s.success() => Outcome::Ok(acc.snap.file.clone()),
        Ok(s) => Outcome::Failed(error_line(&stderr).unwrap_or_else(|| format!("yt-dlp exited with {s}"))),
        Err(e) => Outcome::Failed(format!("yt-dlp: {e}")),
    }
}

// ---------------------------------------------------------------- jobs / persistence

fn ensure_loaded(st: &AppState) {
    let mut g = lock(st);
    if g.loaded {
        return;
    }
    g.loaded = true;
    let path = st.data_dir.join(JOBS_FILE);
    let saved: Vec<Saved> = match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
            tracing::warn!("{} unreadable: {e}", path.display());
            Vec::new()
        }),
        Err(_) => Vec::new(),
    };
    for s in saved {
        let mut info = restore(s.job);
        if let Some(n) = info.id.strip_prefix("yt-").and_then(|n| n.parse::<u64>().ok()) {
            g.next = g.next.max(n);
        }
        if g.jobs.iter().any(|j| j.info.id == info.id) {
            continue;
        }
        info.speed_bps = None;
        g.jobs.push(Job { info, dir: s.dir, cancel: None, task: None });
    }
}

/// A job that was running when the app quit cannot continue.
fn restore(mut j: YtJob) -> YtJob {
    if is_running(j.state) {
        j.state = YtJobState::Error;
        j.error = Some("interrupted".into());
        j.eta_secs = None;
    }
    j
}

fn save(st: &AppState) {
    let saved: Vec<Saved> = lock(st)
        .jobs
        .iter()
        .map(|j| Saved { job: j.info.clone(), dir: j.dir.clone() })
        .collect();
    let _g = st.yt.save_lock.lock().unwrap_or_else(|e| e.into_inner());
    let path = st.data_dir.join(JOBS_FILE);
    let tmp = st.data_dir.join(".yt-jobs.json.tmp");
    let res = serde_json::to_vec_pretty(&saved)
        .map_err(std::io::Error::other)
        .and_then(|b| std::fs::write(&tmp, b))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = res {
        tracing::warn!("saving {} failed: {e}", path.display());
    }
}

fn list(st: &AppState) -> Vec<YtJob> {
    lock(st).jobs.iter().map(|j| j.info.clone()).collect()
}

fn with_job<R>(st: &AppState, id: &str, f: impl FnOnce(&mut Job) -> R) -> Option<R> {
    lock(st).jobs.iter_mut().find(|j| j.info.id == id).map(f)
}

/// Mark the list changed; a single background task emits `yt-jobs-updated` ≤ 4/s.
fn touch(app: &AppHandle, st: &Arc<AppState>) {
    st.yt.dirty.store(true, Ordering::Release);
    if st.yt.emitter_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let app = app.clone();
    let st = st.clone();
    tauri::async_runtime::spawn(async move {
        let mut iv = tokio::time::interval(EMIT_EVERY);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            if st.yt.dirty.swap(false, Ordering::AcqRel) {
                if let Err(e) = app.emit("yt-jobs-updated", list(&st)) {
                    tracing::debug!("emit yt-jobs-updated failed: {e}");
                }
            }
        }
    });
}

/// Finished file of a done job and a title for the player.
pub fn job_file(st: &AppState, id: &str) -> Option<(PathBuf, String)> {
    ensure_loaded(st);
    let g = lock(st);
    let j = g.jobs.iter().find(|j| j.info.id == id)?;
    if j.info.state != YtJobState::Done {
        return None;
    }
    let p = PathBuf::from(j.info.file_path.as_deref()?);
    p.is_file().then(|| {
        let title = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| j.info.title.clone());
        (p, title)
    })
}

/// `Tidebox/<clean title>`, or a unique sibling "Title (2)" when that exists.
fn job_dir(tidebox: &Path, title: &str) -> PathBuf {
    let base = engine::clean_folder_name(title);
    if !tidebox.join(&base).exists() {
        return tidebox.join(base);
    }
    tidebox.join(engine::unique_name(&base, |c| tidebox.join(c).exists()))
}

async fn run_job(st: Arc<AppState>, app: AppHandle, id: String, bin: PathBuf, cancel: Arc<Notify>) {
    let Some((url, preset)) = with_job(&st, &id, |j| (j.info.url.clone(), j.info.preset)) else { return };
    let cached = lock(&st).probes.get(&url).cloned();
    let info = match cached {
        Some(p) => Ok(p),
        None => tokio::select! {
            _ = cancel.notified() => return,
            r = probe(&bin, &url) => r,
        },
    };
    let info = match info {
        Ok(p) if p.is_live => Err("live streams are not supported".to_string()),
        Ok(p) => Ok(p),
        Err(e) => Err(e.split_once(": ").map_or(e.clone(), |(_, m)| m.to_string())),
    };
    let info = match info {
        Ok(p) => p,
        Err(e) => return finish(&st, &app, &id, Outcome::Failed(e)),
    };

    let tidebox = st.download_dir.clone();
    let dir = job_dir(&tidebox, &info.title);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return finish(&st, &app, &id, Outcome::Failed(format!("create {}: {e}", dir.display())));
    }
    let go = with_job(&st, &id, |j| {
        if j.info.state != YtJobState::Queued {
            return false;
        }
        j.info.title = info.title.clone();
        j.info.thumbnail = info.thumbnail.clone();
        j.info.state = YtJobState::Downloading;
        j.dir = Some(dir.clone());
        true
    });
    if go != Some(true) {
        // Cancelled/removed while probing; drop the folder we just made if it is empty.
        let _ = std::fs::remove_dir(&dir);
        return;
    }
    save(&st);
    touch(&app, &st);

    let mut cmd = command(&bin);
    let mut args = download_args(preset, &dir, find_bin("ffmpeg").as_deref(), &url);
    if let Some(bps) = crate::netmode::limit_bps() {
        // Options must precede the "--" url terminator.
        let at = args.len() - 2;
        args.splice(at..at, ["--limit-rate".to_string(), bps.to_string()]);
    }
    cmd.args(args);
    let outcome = drive(cmd, cancel, |s| {
        with_job(&st, &id, |j| {
            if !is_running(j.info.state) {
                return;
            }
            j.info.state = if s.post { YtJobState::Postprocessing } else { YtJobState::Downloading };
            j.info.downloaded_bytes = s.downloaded;
            j.info.total_bytes = s.total;
            j.info.progress = match s.total {
                Some(t) if t > 0 => (s.downloaded as f64 / t as f64).clamp(0.0, 1.0),
                _ => j.info.progress,
            };
            j.info.speed_bps = s.speed;
            j.info.eta_secs = s.eta;
            if s.file.is_some() {
                j.info.file_path = s.file.clone();
            }
        });
        touch(&app, &st);
    })
    .await;
    finish(&st, &app, &id, outcome);
}

fn finish(st: &Arc<AppState>, app: &AppHandle, id: &str, outcome: Outcome) {
    with_job(st, id, |j| {
        j.cancel = None;
        if !is_running(j.info.state) {
            return; // cancelled by the user meanwhile
        }
        j.info.speed_bps = None;
        j.info.eta_secs = None;
        match outcome {
            Outcome::Ok(file) => {
                j.info.state = YtJobState::Done;
                j.info.progress = 1.0;
                if file.is_some() {
                    j.info.file_path = file;
                }
                if let Some(size) = j.info.file_path.as_deref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len()) {
                    j.info.downloaded_bytes = size;
                    j.info.total_bytes = Some(size);
                }
            }
            Outcome::Failed(e) => {
                j.info.state = YtJobState::Error;
                j.info.error = Some(e);
            }
            Outcome::Cancelled => j.info.state = YtJobState::Cancelled,
        }
    });
    save(st);
    touch(app, st);
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub async fn yt_probe(state: St<'_>, url: String) -> Result<YtProbe, String> {
    let url = url.trim().to_string();
    valid_url(&url)?;
    let bin = find_ytdlp()?;
    let p = probe(&bin, &url).await?;
    lock(&state).probes.insert(url, p.clone());
    Ok(p)
}

/// JS: `{ url, preset, force?: bool }` — `force` starts even on a metered network.
#[tauri::command]
pub async fn yt_download(app: AppHandle, state: St<'_>, url: String, preset: YtPreset, force: Option<bool>) -> Result<YtJob, String> {
    let st: Arc<AppState> = state.inner().clone();
    let url = url.trim().to_string();
    valid_url(&url)?;
    if force != Some(true) && crate::netmode::is_metered() && crate::netmode::limit_bps().is_none() {
        return Err("METERED: on mobile data".into());
    }
    let bin = find_ytdlp()?;
    ensure_loaded(&st);
    let cancel = Arc::new(Notify::new());
    let info = {
        let mut g = lock(&st);
        let probe = g.probes.get(&url).cloned();
        if probe.as_ref().is_some_and(|p| p.is_live) {
            return Err("INVALID_INPUT: live streams are not supported".into());
        }
        g.next += 1;
        let info = YtJob {
            id: format!("yt-{}", g.next),
            url: url.clone(),
            title: probe.as_ref().map_or_else(|| url.clone(), |p| p.title.clone()),
            thumbnail: probe.and_then(|p| p.thumbnail),
            preset,
            state: YtJobState::Queued,
            progress: 0.0,
            downloaded_bytes: 0,
            total_bytes: None,
            speed_bps: None,
            eta_secs: None,
            file_path: None,
            error: None,
        };
        g.jobs.push(Job { info: info.clone(), dir: None, cancel: Some(cancel.clone()), task: None });
        info
    };
    let task = tauri::async_runtime::spawn(run_job(st.clone(), app.clone(), info.id.clone(), bin, cancel));
    with_job(&st, &info.id, |j| j.task = Some(task));
    save(&st);
    touch(&app, &st);
    Ok(info)
}

#[tauri::command]
pub fn yt_list(state: St<'_>) -> Result<Vec<YtJob>, String> {
    ensure_loaded(&state);
    Ok(list(&state))
}

/// Stop a job: the state becomes `cancelled` right away; the process is killed.
fn cancel_job(st: &AppState, id: &str) -> Result<Option<tauri::async_runtime::JoinHandle<()>>, String> {
    with_job(st, id, |j| {
        if is_running(j.info.state) {
            j.info.state = YtJobState::Cancelled;
            j.info.speed_bps = None;
            j.info.eta_secs = None;
        }
        if let Some(c) = j.cancel.take() {
            c.notify_one();
        }
        j.task.take()
    })
    .ok_or_else(|| format!("NOT_FOUND: no download {id}"))
}

#[tauri::command]
pub fn yt_cancel(app: AppHandle, state: St<'_>, id: String) -> Result<(), String> {
    let st: Arc<AppState> = state.inner().clone();
    ensure_loaded(&st);
    cancel_job(&st, &id)?;
    save(&st);
    touch(&app, &st);
    Ok(())
}

/// JS: `{ id, deleteFile }`. Cancels first if still running; `deleteFile` removes the
/// job's own folder in Tidebox (or just its file when the folder is unknown).
#[tauri::command]
pub async fn yt_remove(app: AppHandle, state: St<'_>, id: String, delete_file: bool) -> Result<(), String> {
    let st: Arc<AppState> = state.inner().clone();
    ensure_loaded(&st);
    let task = cancel_job(&st, &id)?;
    let removed = {
        let mut g = lock(&st);
        let i = g.jobs.iter().position(|j| j.info.id == id);
        i.map(|i| g.jobs.remove(i))
    };
    save(&st);
    touch(&app, &st);
    let Some(job) = removed else { return Ok(()) };
    if !delete_file {
        return Ok(());
    }
    if let Some(t) = task {
        let _ = tokio::time::timeout(KILL_WAIT, t).await;
    }
    let tidebox = &st.download_dir;
    match (&job.dir, &job.info.file_path) {
        // Only ever a direct child of Tidebox that we created for this job.
        (Some(d), _) if d.parent() == Some(tidebox.as_path()) && d != tidebox => {
            if d.exists() {
                tokio::fs::remove_dir_all(d).await.map_err(|e| internal(format!("delete {}: {e}", d.display())))?;
            }
        }
        (_, Some(f)) => {
            let f = Path::new(f);
            if f.starts_with(tidebox) && f.is_file() {
                tokio::fs::remove_file(f).await.map_err(|e| internal(format!("delete {}: {e}", f.display())))?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[tauri::command]
pub async fn yt_reveal(state: St<'_>, id: String) -> Result<(), String> {
    let st: Arc<AppState> = state.inner().clone();
    ensure_loaded(&st);
    let (file, dir) = with_job(&st, &id, |j| (j.info.file_path.clone().map(PathBuf::from), j.dir.clone()))
        .ok_or_else(|| format!("NOT_FOUND: no download {id}"))?;
    let p = [file, dir]
        .into_iter()
        .flatten()
        .find(|p| p.exists())
        .unwrap_or_else(|| st.download_dir.clone());
    let status = Command::new("/usr/bin/open")
        .arg("-R")
        .arg(&p)
        .stdin(Stdio::null())
        .status()
        .await
        .map_err(|e| internal(format!("open -R failed: {e}")))?;
    if !status.success() {
        return Err(internal(format!("open -R exited with {status}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE_JSON: &str = r#"{
        "id": "dQw4w9WgXcQ", "title": "Never Gonna Give You Up", "uploader": "Rick Astley",
        "duration": 212.0, "thumbnail": "https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg",
        "is_live": false, "live_status": "not_live",
        "formats": [
            {"format_id": "140", "vcodec": "none", "acodec": "mp4a.40.2", "height": null},
            {"format_id": "18", "vcodec": "avc1.42001E", "acodec": "mp4a.40.2", "height": 360},
            {"format_id": "136", "vcodec": "avc1.4d401f", "acodec": "none", "height": 720},
            {"format_id": "137", "vcodec": "avc1.640028", "acodec": "none", "height": 1080}
        ]
    }"#;

    #[test]
    fn probe_parsing() {
        let v: serde_json::Value = serde_json::from_str(PROBE_JSON).unwrap();
        let p = parse_probe("https://youtu.be/dQw4w9WgXcQ", &v).unwrap();
        assert_eq!(p.title, "Never Gonna Give You Up");
        assert_eq!(p.uploader.as_deref(), Some("Rick Astley"));
        assert_eq!(p.duration_secs, Some(212.0));
        assert_eq!(p.max_height, Some(1080));
        assert!(!p.is_live);
        assert!(p.thumbnail.unwrap().ends_with("maxresdefault.jpg"));

        let live: serde_json::Value =
            serde_json::from_str(r#"{"id":"x","channel":"C","live_status":"is_live","thumbnails":[{"url":"a"},{"url":"b"}]}"#).unwrap();
        let p = parse_probe("https://x", &live).unwrap();
        assert!(p.is_live);
        assert_eq!(p.title, "x");
        assert_eq!(p.uploader.as_deref(), Some("C"));
        assert_eq!(p.thumbnail.as_deref(), Some("b"));
        assert_eq!(p.max_height, None);

        let pl: serde_json::Value = serde_json::from_str(r#"{"_type":"playlist","entries":[]}"#).unwrap();
        assert!(parse_probe("https://x", &pl).unwrap_err().starts_with("INVALID_INPUT"));
    }

    #[test]
    fn line_classification() {
        assert_eq!(
            classify("[dl] 1024 4096 NA 512.5 6"),
            Line::Progress { downloaded: Some(1024), total: Some(4096), speed: Some(512.5), eta: Some(6) }
        );
        assert_eq!(
            classify("[dl] 1024 NA 9000.0 NA NA"),
            Line::Progress { downloaded: Some(1024), total: Some(9000), speed: None, eta: None }
        );
        assert_eq!(classify("[done] /a/b [x].mp4"), Line::Done("/a/b [x].mp4".into()));
        assert_eq!(classify("[download] Destination: /a/b.f136.mp4"), Line::Destination);
        assert_eq!(classify("[Merger] Merging formats into \"/a/b.mp4\""), Line::Postprocessing);
        assert_eq!(classify("[ExtractAudio] Destination: /a/b.mp3"), Line::Postprocessing);
        assert_eq!(classify("[youtube] dQw4w9WgXcQ: Downloading webpage"), Line::Other);
    }

    #[test]
    fn accumulates_two_streams() {
        let mut a = Acc::default();
        a.feed("[download] Destination: v.f136.mp4");
        a.feed("[dl] 500 1000 NA 100.0 5");
        assert_eq!((a.snap.downloaded, a.snap.total), (500, Some(1000)));
        a.feed("[dl] 1000 1000 NA 100.0 0");
        a.feed("[download] Destination: v.f140.m4a");
        a.feed("[dl] 50 100 NA 10.0 5");
        assert_eq!((a.snap.downloaded, a.snap.total), (1050, Some(1100)));
        assert!(!a.snap.post);
        a.feed("[Merger] Merging formats into \"v.mp4\"");
        assert!(a.snap.post);
        assert_eq!(a.snap.speed, None);
        a.feed("[done] /t/v [id].mp4");
        assert_eq!(a.snap.file.as_deref(), Some("/t/v [id].mp4"));
    }

    #[test]
    fn args_shape() {
        let a = download_args(YtPreset::P480, Path::new("/T/Song"), Some(Path::new("/opt/homebrew/bin/ffmpeg")), "https://x/y");
        let s = a.join(" ");
        assert!(s.contains("-f bv*[height<=?480]+ba/b[height<=?480]"));
        assert!(s.contains("-S vcodec:h264,res:480,acodec:m4a --merge-output-format mp4"));
        assert!(s.contains("-o /T/Song/%(title)s [%(id)s].%(ext)s"));
        assert!(s.contains("--ffmpeg-location /opt/homebrew/bin/ffmpeg"));
        assert!(s.contains("--no-quiet"));
        assert_eq!(&a[a.len() - 2..], &["--".to_string(), "https://x/y".to_string()]);
        let m = download_args(YtPreset::AudioMp3, Path::new("/T/S"), None, "https://x");
        assert!(m.join(" ").contains("-f ba/b -x --audio-format mp3"));
        assert!(!m.iter().any(|x| x == "--ffmpeg-location"));
        assert_eq!(serde_json::to_string(&YtPreset::AudioMp3).unwrap(), "\"audio_mp3\"");
        assert_eq!(serde_json::from_str::<YtPreset>("\"720p\"").unwrap(), YtPreset::P720);
        assert!(serde_json::from_str::<YtPreset>("\"1080p\"").is_err());
    }

    #[test]
    fn url_rules_and_errors() {
        assert!(valid_url("https://www.youtube.com/watch?v=x").is_ok());
        assert!(valid_url("HTTP://vimeo.com/1").is_ok());
        assert!(valid_url("magnet:?xt=urn").is_err());
        assert!(valid_url("https://").is_err());
        assert!(valid_url("https://a b").is_err());
        assert_eq!(
            error_line("WARNING: x\nERROR: [youtube] abc: Video unavailable\nWARNING: y\n").as_deref(),
            Some("[youtube] abc: Video unavailable")
        );
        assert_eq!(error_line("just this\n").as_deref(), Some("just this"));
        assert_eq!(error_line(""), None);
    }

    #[test]
    fn restore_marks_interrupted() {
        let v: serde_json::Value = serde_json::json!({
            "id": "yt-3", "url": "https://x", "title": "T", "thumbnail": null, "preset": "360p",
            "state": "downloading", "progress": 0.4, "downloaded_bytes": 4, "total_bytes": 10,
            "speed_bps": 1.0, "eta_secs": 6, "file_path": null, "error": null, "dir": "/T/T"
        });
        let s: Saved = serde_json::from_value(v).unwrap();
        assert_eq!(s.dir.as_deref(), Some(Path::new("/T/T")));
        let j = restore(s.job);
        assert_eq!(j.state, YtJobState::Error);
        assert_eq!(j.error.as_deref(), Some("interrupted"));
    }

    #[test]
    fn job_dirs_are_unique() {
        let tmp = std::env::temp_dir().join(format!("tide-yt-dir-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("My Video")).unwrap();
        assert_eq!(job_dir(&tmp, "My: Video"), tmp.join("My- Video"));
        assert_eq!(job_dir(&tmp, "My Video"), tmp.join("My Video (2)"));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    fn fake_ytdlp(name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tide-yt-fake-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("yt-dlp");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[tokio::test]
    async fn drive_fake_success() {
        let bin = fake_ytdlp(
            "ok",
            r#"echo '[youtube] x: Downloading webpage'
echo '[download] Destination: /t/v.f136.mp4'
echo '[dl] 400 1000 NA 200.0 3'
echo '[dl] 1000 1000 NA 200.0 0'
echo '[download] Destination: /t/v.f140.m4a'
echo '[dl] 100 100 NA 50.0 0'
echo '[Merger] Merging formats into "/t/v.mp4"'
echo 'WARNING: harmless' >&2
echo '[done] /t/v [x].mp4'
exit 0"#,
        );
        let mut snaps = Vec::new();
        let out = drive(command(&bin), Arc::new(Notify::new()), |s| snaps.push(s.clone())).await;
        assert_eq!(out, Outcome::Ok(Some("/t/v [x].mp4".into())));
        assert!(snaps.iter().any(|s| s.downloaded == 400 && s.total == Some(1000) && !s.post));
        assert!(snaps.iter().any(|s| s.post));
        let last = snaps.last().unwrap();
        assert_eq!((last.downloaded, last.total), (1100, Some(1100)));
    }

    #[tokio::test]
    async fn drive_fake_error() {
        let bin = fake_ytdlp("err", "echo '[youtube] x: Downloading webpage'\necho 'ERROR: [youtube] x: Video unavailable' >&2\nexit 1");
        let out = drive(command(&bin), Arc::new(Notify::new()), |_| {}).await;
        assert_eq!(out, Outcome::Failed("[youtube] x: Video unavailable".into()));
    }

    #[tokio::test]
    async fn drive_fake_cancel() {
        let bin = fake_ytdlp("slow", "echo '[dl] 1 100 NA 1.0 99'\nsleep 30\necho '[done] /never'");
        let cancel = Arc::new(Notify::new());
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            c2.notify_one();
        });
        let t0 = std::time::Instant::now();
        let out = drive(command(&bin), cancel, |_| {}).await;
        assert_eq!(out, Outcome::Cancelled);
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn binary_candidates() {
        let c = candidates("yt-dlp", Some(Path::new("/Users/x")));
        assert_eq!(c[0], PathBuf::from("/opt/homebrew/bin/yt-dlp"));
        assert_eq!(c[1], PathBuf::from("/usr/local/bin/yt-dlp"));
        assert_eq!(c[2], PathBuf::from("/Users/x/.local/bin/yt-dlp"));
    }
}
