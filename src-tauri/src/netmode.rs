//! Mobile-data mode. Network.framework's path monitor tells us whether the current
//! path is "expensive" (cellular, iPhone hotspot) or "constrained" (Low Data Mode).
//! While it is ("metered"), depending on `settings.json` `netmode`:
//! - "pause" (default): downloading torrents are paused once, at the switch to a metered
//!   network; exactly those are resumed when the network is unmetered again.
//! - "limit": session download/upload capped at `limit_kbps` KB/s.
//! - "off": nothing.
//! Metered = per-network manual override (settings `metered_networks`/`unmetered_networks`,
//! set by `net_mark_current`), else expensive || constrained. Only torrent peer traffic
//! (and, via `is_metered`, new yt-dlp jobs) is affected: the local stream server and
//! LAN features are never blocked, and a torrent the player resumes stays running.
//! Other modules ask `is_metered()` (false in mode "off") / `limit_bps()`.
//!
//! Also owns `data_dir/settings.json` (shared with notify.rs; unknown keys preserved).

use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use librqbit::TorrentStatsState;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::state::AppState;

// ---------------------------------------------------------------- settings.json

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetMode {
    #[default]
    Pause,
    Limit,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub netmode: NetMode,
    /// KB/s cap for mode "limit".
    pub limit_kbps: u32,
    pub notifications_enabled: bool,
    /// Manual per-network overrides (network ids, see `NetIdent`): macOS often does not
    /// flag an Android hotspot as expensive.
    pub metered_networks: Vec<String>,
    pub unmetered_networks: Vec<String>,
    /// Keys written by other versions/modules, kept on save.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { netmode: NetMode::Pause, limit_kbps: 200, notifications_enabled: true, metered_networks: vec![], unmetered_networks: vec![], extra: Default::default() }
    }
}

static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);

/// Same directory `state.rs` uses.
fn data_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/com.musahinist.tide"))
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &std::path::Path) -> T {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
            tracing::warn!("{}: invalid JSON, using defaults: {e}", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

fn write_json<T: Serialize>(path: &std::path::Path, v: &T) -> Result<(), String> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

pub fn with_settings<R>(f: impl FnOnce(&Settings) -> R) -> R {
    let mut g = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    let s = g.get_or_insert_with(|| data_dir().map(|d| read_json(&d.join("settings.json"))).unwrap_or_default());
    f(s)
}

pub fn update_settings(f: impl FnOnce(&mut Settings)) -> Result<Settings, String> {
    let mut g = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    let dir = data_dir().ok_or("INTERNAL: HOME not set")?;
    let mut next = g.clone().unwrap_or_else(|| read_json(&dir.join("settings.json")));
    f(&mut next);
    write_json(&dir.join("settings.json"), &next).map_err(|e| format!("INTERNAL: {e}"))?;
    *g = Some(next.clone());
    Ok(next)
}

// ---------------------------------------------------------------- detection

/// bit0 = known, bit1 = expensive, bit2 = constrained. Written by the monitor callback.
static PATH_BITS: AtomicU8 = AtomicU8::new(0);

const KNOWN: u8 = 1;
const EXPENSIVE: u8 = 2;
const CONSTRAINED: u8 = 4;

#[cfg(target_os = "macos")]
mod nw {
    use super::{CONSTRAINED, EXPENSIVE, KNOWN, PATH_BITS};
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;

    #[link(name = "Network", kind = "framework")]
    extern "C" {
        fn nw_path_monitor_create() -> *mut c_void;
        fn nw_path_monitor_set_update_handler(monitor: *mut c_void, handler: *const c_void);
        fn nw_path_monitor_set_queue(monitor: *mut c_void, queue: *mut c_void);
        fn nw_path_monitor_start(monitor: *mut c_void);
        fn nw_path_is_expensive(path: *mut c_void) -> bool;
        fn nw_path_is_constrained(path: *mut c_void) -> bool;
    }
    extern "C" {
        // libSystem (libdispatch / libclosure).
        fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
        static _NSConcreteGlobalBlock: [usize; 32];
    }

    /// Clang block ABI. A capture-less block flagged BLOCK_IS_GLOBAL: Block_copy returns
    /// it as is and it is never freed, so a leaked heap copy is a valid `void (^)(nw_path_t)`.
    #[repr(C)]
    struct BlockLiteral {
        isa: *const c_void,
        flags: i32,
        reserved: i32,
        invoke: unsafe extern "C" fn(*const BlockLiteral, *mut c_void),
        descriptor: *const BlockDescriptor,
    }
    #[repr(C)]
    struct BlockDescriptor {
        reserved: usize,
        size: usize,
    }
    const BLOCK_IS_GLOBAL: i32 = 1 << 28;
    static DESCRIPTOR: BlockDescriptor =
        BlockDescriptor { reserved: 0, size: std::mem::size_of::<BlockLiteral>() };

    unsafe extern "C" fn on_path(_block: *const BlockLiteral, path: *mut c_void) {
        if path.is_null() {
            return;
        }
        let mut bits = KNOWN;
        if nw_path_is_expensive(path) {
            bits |= EXPENSIVE;
        }
        if nw_path_is_constrained(path) {
            bits |= CONSTRAINED;
        }
        PATH_BITS.store(bits, Ordering::SeqCst);
    }

    /// Start the monitor (once). Monitor and block live for the whole process.
    pub fn start() -> bool {
        unsafe {
            let monitor = nw_path_monitor_create();
            if monitor.is_null() {
                return false;
            }
            let block = Box::leak(Box::new(BlockLiteral {
                isa: std::ptr::addr_of!(_NSConcreteGlobalBlock) as *const c_void,
                flags: BLOCK_IS_GLOBAL,
                reserved: 0,
                invoke: on_path,
                descriptor: &DESCRIPTOR,
            }));
            nw_path_monitor_set_update_handler(monitor, block as *const BlockLiteral as *const c_void);
            // QOS_CLASS_UTILITY = 0x11.
            nw_path_monitor_set_queue(monitor, dispatch_get_global_queue(0x11, 0));
            nw_path_monitor_start(monitor);
            true
        }
    }
}

fn start_monitor() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        #[cfg(target_os = "macos")]
        if !nw::start() {
            tracing::warn!("nw_path_monitor_create failed; mobile-data mode inactive");
        }
    });
}

/// (known, expensive, constrained). `TIDE_NETMODE_FORCE=metered|unmetered` overrides
/// (manual testing).
fn path_state() -> (bool, bool, bool) {
    match std::env::var("TIDE_NETMODE_FORCE").as_deref() {
        Ok("metered") => return (true, true, false),
        Ok("unmetered") => return (true, false, false),
        _ => {}
    }
    let b = PATH_BITS.load(Ordering::SeqCst);
    (b & KNOWN != 0, b & EXPENSIVE != 0, b & CONSTRAINED != 0)
}

/// Manual override for `id` from the settings lists (pure).
fn manual_for(s: &Settings, id: Option<&str>) -> Option<bool> {
    let id = id?;
    if s.metered_networks.iter().any(|n| n == id) {
        Some(true)
    } else if s.unmetered_networks.iter().any(|n| n == id) {
        Some(false)
    } else {
        None
    }
}

/// Metered = manual override for the current network, else expensive || constrained.
fn raw_metered() -> bool {
    let (known, exp, con) = path_state();
    if !known {
        return false;
    }
    if std::env::var_os("TIDE_NETMODE_FORCE").is_some() {
        return exp || con;
    }
    let id = current_ident().map(|i| i.id);
    with_settings(|s| manual_for(s, id.as_deref())).unwrap_or(exp || con)
}

// ---------------------------------------------------------------- network identity

/// Which network we are on. SSID when macOS reveals it (redacted without Location
/// permission on recent macOS), else the default gateway's MAC ("gw:aa:bb:..") — stable
/// for a given phone hotspot / router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetIdent {
    pub id: String,
    pub ssid: Option<String>,
}

static IDENT: Mutex<Option<NetIdent>> = Mutex::new(None);

fn current_ident() -> Option<NetIdent> {
    IDENT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn run(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// `route -n get default` → (gateway, interface).
fn parse_route(out: &str) -> (Option<String>, Option<String>) {
    let field = |name: &str| {
        out.lines()
            .filter_map(|l| l.trim().strip_prefix(name))
            .filter_map(|r| r.strip_prefix(':'))
            .map(|v| v.trim().to_string())
            .find(|v| !v.is_empty())
    };
    (field("gateway"), field("interface"))
}

/// SSID from `networksetup -getairportnetwork` or `ipconfig getsummary` output.
fn parse_ssid(out: &str) -> Option<String> {
    for l in out.lines() {
        let l = l.trim();
        let v = l
            .strip_prefix("Current Wi-Fi Network:")
            .or_else(|| l.strip_prefix("Current AirPort Network:"))
            .or_else(|| l.strip_prefix("SSID :"));
        if let Some(v) = v.map(str::trim) {
            if !v.is_empty() && v != "<redacted>" {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// `arp -n <ip>` → "aa:bb:cc:dd:ee:ff".
fn parse_arp_mac(out: &str) -> Option<String> {
    let rest = out.split(" at ").nth(1)?;
    let mac = rest.split_whitespace().next()?;
    (mac.contains(':') && mac != "(incomplete)").then(|| mac.to_ascii_lowercase())
}

/// Local commands only (no network traffic). Blocking.
fn probe_ident() -> Option<NetIdent> {
    let (gw, iface) = parse_route(&run("/sbin/route", &["-n", "get", "default"]));
    let iface = iface.unwrap_or_else(|| "en0".into());
    let ssid = parse_ssid(&run("/usr/sbin/networksetup", &["-getairportnetwork", &iface]))
        .or_else(|| parse_ssid(&run("/usr/sbin/ipconfig", &["getsummary", &iface])));
    if let Some(s) = &ssid {
        return Some(NetIdent { id: format!("ssid:{s}"), ssid });
    }
    let mac = gw.and_then(|g| parse_arp_mac(&run("/usr/sbin/arp", &["-n", &g])))?;
    Some(NetIdent { id: format!("gw:{mac}"), ssid: None })
}

async fn refresh_ident() {
    let id = tokio::task::spawn_blocking(probe_ident).await.ok().flatten();
    let mut g = IDENT.lock().unwrap_or_else(|e| e.into_inner());
    if *g != id {
        tracing::info!(network = ?id.as_ref().map(|i| &i.id), "current network");
        *g = id;
    }
}

/// True while on an expensive/constrained network and mobile-data mode isn't "off".
/// yt-dlp should not start jobs while this is true (in mode "limit", see `limit_bps`).
pub fn is_metered() -> bool {
    raw_metered() && with_settings(|s| s.netmode) != NetMode::Off
}

/// Bytes/s cap to apply to other downloaders (yt-dlp `--limit-rate`) right now, if any.
pub fn limit_bps() -> Option<u64> {
    let (mode, kbps) = with_settings(|s| (s.netmode, s.limit_kbps));
    (raw_metered() && mode == NetMode::Limit && kbps > 0).then(|| kbps as u64 * 1024)
}

// ---------------------------------------------------------------- state machine (pure)

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Machine {
    /// We are holding downloads (metered + mode "pause").
    pub active: bool,
    /// Keys Tide paused because of the network; resumed when unmetered.
    pub paused: BTreeSet<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Actions {
    pub pause: Vec<String>,
    pub resume: Vec<String>,
}

impl Machine {
    /// `downloading`: Live + unfinished keys. `paused_now`: keys currently paused.
    /// Torrents are paused only at the switch to holding; one the user (or the player)
    /// resumed meanwhile is no longer ours, and one already paused by the user is never
    /// touched.
    pub fn step(&mut self, hold: bool, downloading: &BTreeSet<String>, paused_now: &BTreeSet<String>) -> Actions {
        let mut a = Actions::default();
        if hold {
            if !self.active {
                self.active = true;
                for k in downloading {
                    if self.paused.insert(k.clone()) {
                        a.pause.push(k.clone());
                    }
                }
            } else {
                self.paused.retain(|k| paused_now.contains(k));
            }
        } else {
            self.active = false;
            a.resume = self.paused.iter().filter(|k| paused_now.contains(*k)).cloned().collect();
            self.paused.clear();
        }
        a
    }
}

// ---------------------------------------------------------------- runtime

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NetStatus {
    /// Network path known yet (monitor delivered its first update).
    pub detected: bool,
    pub metered: bool,
    pub expensive: bool,
    pub constrained: bool,
    pub mode: NetMode,
    pub limit_kbps: u32,
    pub paused_by_netmode: Vec<String>,
    /// Current Wi-Fi SSID when macOS reveals it (often null: redacted without Location).
    pub ssid: Option<String>,
    /// Id the manual override is stored under ("ssid:<name>" or "gw:<router mac>").
    pub network_id: Option<String>,
    /// Manual override for this network: true/false, null = automatic.
    pub manual: Option<bool>,
}

static MACHINE: Mutex<Option<Machine>> = Mutex::new(None);

fn paused_file() -> Option<PathBuf> {
    data_dir().map(|d| d.join("netmode.json"))
}

fn with_machine<R>(f: impl FnOnce(&mut Machine) -> R) -> R {
    let mut g = MACHINE.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(|| Machine {
        active: false,
        paused: paused_file().map(|p| read_json::<BTreeSet<String>>(&p)).unwrap_or_default(),
    });
    f(m)
}

fn status() -> NetStatus {
    let (detected, expensive, constrained) = path_state();
    let ident = current_ident();
    let (mode, limit_kbps, manual) =
        with_settings(|s| (s.netmode, s.limit_kbps, manual_for(s, ident.as_ref().map(|i| i.id.as_str()))));
    NetStatus {
        detected,
        metered: raw_metered(),
        expensive,
        constrained,
        mode,
        limit_kbps,
        paused_by_netmode: with_machine(|m| m.paused.iter().cloned().collect()),
        ssid: ident.as_ref().and_then(|i| i.ssid.clone()),
        network_id: ident.map(|i| i.id),
        manual,
    }
}

fn snapshot(st: &AppState) -> (BTreeSet<String>, BTreeSet<String>) {
    // 0 = other, 1 = downloading, 2 = paused
    let all: Vec<(String, u8)> = st.session.with_torrents(|it| {
        it.map(|(_, h)| {
            let s = h.stats();
            let c = match s.state {
                TorrentStatsState::Live if !s.finished => 1,
                TorrentStatsState::Paused | TorrentStatsState::Initializing { paused: true } => 2,
                _ => 0,
            };
            (h.info_hash().as_string(), c)
        })
        .collect()
    });
    let mut downloading = BTreeSet::new();
    let mut paused = BTreeSet::new();
    for (k, c) in all {
        match c {
            1 => downloading.insert(k),
            2 => paused.insert(k),
            _ => false,
        };
    }
    (downloading, paused)
}

/// Rate caps Tide set (so we only clear what we set).
static LIMIT_SET: AtomicU8 = AtomicU8::new(0);

fn apply_limit(st: &AppState, cap_bps: Option<u32>) {
    let want = cap_bps.and_then(NonZeroU32::new);
    let had = LIMIT_SET.load(Ordering::SeqCst) != 0;
    if want.is_none() && !had {
        return;
    }
    let rl = &st.session.ratelimits;
    if rl.get_download_bps() != want || rl.get_upload_bps() != want {
        rl.set_download_bps(want);
        rl.set_upload_bps(want);
        tracing::info!(cap_bps = ?want, "mobile-data rate limit updated");
    }
    LIMIT_SET.store(want.is_some() as u8, Ordering::SeqCst);
}

async fn tick(st: &AppState) {
    let metered = raw_metered();
    let (known, _, _) = path_state();
    let (mode, kbps) = with_settings(|s| (s.netmode, s.limit_kbps));
    if !known {
        return; // never act before the first path update
    }
    let hold = metered && mode == NetMode::Pause;
    apply_limit(st, (metered && mode == NetMode::Limit).then(|| kbps.saturating_mul(1024)));

    let (downloading, paused_now) = snapshot(st);
    let (actions, before, after) = with_machine(|m| {
        let before = m.paused.clone();
        let a = m.step(hold, &downloading, &paused_now);
        (a, before, m.paused.clone())
    });
    for k in &actions.pause {
        match crate::engine::pause(st, k).await {
            Ok(()) => tracing::info!(key = %k, "metered network: paused"),
            Err(e) => tracing::warn!(key = %k, "metered network: pause failed: {e}"),
        }
    }
    for k in &actions.resume {
        match crate::engine::ensure_live(st, k).await {
            Ok(()) => tracing::info!(key = %k, "unmetered network: resumed"),
            Err(e) => tracing::warn!(key = %k, "unmetered network: resume failed: {e}"),
        }
    }
    if before != after {
        if let Some(p) = paused_file() {
            if let Err(e) = write_json(&p, &after) {
                tracing::warn!("saving netmode.json failed: {e}");
            }
        }
    }
}

/// Start the path monitor and a 1 s loop that applies the mode and emits "net-status"
/// (payload `NetStatus`) whenever it changes.
pub fn spawn_background(app: AppHandle, st: Arc<AppState>) {
    start_monitor();
    tauri::async_runtime::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(1));
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last: Option<NetStatus> = None;
        let mut last_bits = u8::MAX;
        let mut since_probe = u32::MAX;
        loop {
            iv.tick().await;
            // Network identity: on every path change and every 30 s.
            let bits = PATH_BITS.load(Ordering::SeqCst);
            since_probe = since_probe.saturating_add(1);
            if bits != last_bits || since_probe >= 30 {
                last_bits = bits;
                since_probe = 0;
                refresh_ident().await;
            }
            tick(&st).await;
            let s = status();
            if last.as_ref() != Some(&s) {
                if let Some(prev) = &last {
                    if prev.metered != s.metered {
                        tracing::info!(metered = s.metered, expensive = s.expensive, constrained = s.constrained, "network path changed");
                    }
                }
                if let Err(e) = app.emit("net-status", &s) {
                    tracing::debug!("emit net-status failed: {e}");
                }
                last = Some(s);
            }
        }
    });
}

// ---------------------------------------------------------------- commands

#[tauri::command]
pub fn net_status() -> Result<NetStatus, String> {
    Ok(status())
}

/// JS: `{ mode: "pause"|"limit"|"off", limitKbps?: number }`. Applied by the next tick.
#[tauri::command]
pub fn net_set_mode(mode: NetMode, limit_kbps: Option<u32>) -> Result<NetStatus, String> {
    if limit_kbps == Some(0) {
        return Err("INVALID_INPUT: limitKbps must be > 0".into());
    }
    update_settings(|s| {
        s.netmode = mode;
        if let Some(k) = limit_kbps {
            s.limit_kbps = k.min(4_000_000);
        }
    })?;
    Ok(status())
}

/// JS: `{ metered: true|false|null }` — mark the current network as metered / not
/// metered, or null to go back to automatic detection.
#[tauri::command]
pub async fn net_mark_current(metered: Option<bool>) -> Result<NetStatus, String> {
    refresh_ident().await;
    let id = current_ident().map(|i| i.id).ok_or("NOT_READY: no current network")?;
    update_settings(|s| {
        s.metered_networks.retain(|n| n != &id);
        s.unmetered_networks.retain(|n| n != &id);
        match metered {
            Some(true) => s.metered_networks.push(id.clone()),
            Some(false) => s.unmetered_networks.push(id.clone()),
            None => {}
        }
    })?;
    Ok(status())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn pauses_downloading_and_resumes_exactly_those() {
        let mut m = Machine::default();
        // Unmetered: nothing to do.
        assert_eq!(m.step(false, &set(&["a", "b"]), &set(&["u"])), Actions::default());
        // Switch to metered: a, b paused; user-paused u untouched.
        let a = m.step(true, &set(&["a", "b"]), &set(&["u"]));
        assert_eq!(a.pause, vec!["a", "b"]);
        assert!(a.resume.is_empty());
        // Still metered: nothing more paused (new torrent c starts on hotspot).
        let a = m.step(true, &set(&["c"]), &set(&["a", "b", "u"]));
        assert_eq!(a, Actions::default());
        // Back on wifi: only a and b resume.
        let a = m.step(false, &set(&["c"]), &set(&["a", "b", "u"]));
        assert_eq!(a.resume, vec!["a", "b"]);
        assert!(m.paused.is_empty() && !m.active);
    }

    #[test]
    fn user_resume_or_remove_drops_key() {
        let mut m = Machine::default();
        m.step(true, &set(&["a", "b", "c"]), &set(&[]));
        // a resumed by the user (live again), c removed; b still paused.
        m.step(true, &set(&["a"]), &set(&["b"]));
        assert_eq!(m.paused, set(&["b"]));
        // User pauses a again: not ours, must not be resumed.
        let r = m.step(false, &set(&[]), &set(&["a", "b"]));
        assert_eq!(r.resume, vec!["b"]);
    }

    #[test]
    fn persisted_keys_resume_after_restart_and_survive_metered_restart() {
        // Restart on wifi with keys saved from last session.
        let mut m = Machine { active: false, paused: set(&["a"]) };
        assert_eq!(m.step(false, &set(&[]), &set(&["a"])).resume, vec!["a"]);
        // Restart still metered: saved key kept, new downloads also paused.
        let mut m = Machine { active: false, paused: set(&["a"]) };
        let r = m.step(true, &set(&["b"]), &set(&["a"]));
        assert_eq!(r.pause, vec!["b"]);
        assert_eq!(m.paused, set(&["a", "b"]));
    }

    #[test]
    fn settings_defaults_and_unknown_keys() {
        let s: Settings = serde_json::from_str(r#"{"other":1,"netmode":"limit"}"#).unwrap();
        assert_eq!(s.netmode, NetMode::Limit);
        assert_eq!(s.limit_kbps, 200);
        assert!(s.notifications_enabled);
        let back = serde_json::to_value(&s).unwrap();
        assert_eq!(back["other"], 1);
        let d: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(d.netmode, NetMode::Pause);
    }

    #[test]
    fn ident_parsing_and_overrides() {
        let route = "   route to: default\ndestination: default\n    gateway: 10.1.2.3\n  interface: en0\n";
        assert_eq!(parse_route(route), (Some("10.1.2.3".into()), Some("en0".into())));
        assert_eq!(parse_ssid("Current Wi-Fi Network: Pixel 7"), Some("Pixel 7".into()));
        assert_eq!(parse_ssid("You are not associated with an AirPort network."), None);
        assert_eq!(parse_ssid("  BSSID : <redacted>\n  SSID : <redacted>\n"), None);
        assert_eq!(parse_ssid("  SSID : Home Net\n"), Some("Home Net".into()));
        assert_eq!(
            parse_arp_mac("? (10.1.2.3) at F6:D8:BE:F7:28:EF on en0 ifscope [ethernet]"),
            Some("f6:d8:be:f7:28:ef".into())
        );
        assert_eq!(parse_arp_mac("? (10.1.2.3) at (incomplete) on en0"), None);
        let s = Settings { metered_networks: vec!["gw:a".into()], unmetered_networks: vec!["ssid:Home".into()], ..Default::default() };
        assert_eq!(manual_for(&s, Some("gw:a")), Some(true));
        assert_eq!(manual_for(&s, Some("ssid:Home")), Some(false));
        assert_eq!(manual_for(&s, Some("x")), None);
        assert_eq!(manual_for(&s, None), None);
    }

    /// Prints the live path state (no network traffic). `cargo test -- --ignored nw_probe --nocapture`
    #[test]
    #[ignore]
    fn nw_probe() {
        start_monitor();
        std::thread::sleep(Duration::from_millis(1500));
        eprintln!("path (known, expensive, constrained) = {:?}", path_state());
        eprintln!("network ident = {:?}", probe_ident());
        assert!(path_state().0, "monitor delivered no update");
    }
}
