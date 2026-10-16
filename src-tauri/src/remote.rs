//! "Play on another Mac": Tide → Tide on the LAN.
//!
//! A tiny HTTP/1.1 server (plain tokio, no extra crates) runs while "Remote play" is
//! enabled (default on), on the first free port of `PORTS`, all interfaces. It is
//! advertised with Bonjour through `/usr/bin/dns-sd -R` and serves only:
//!
//! - `GET|HEAD /v1/stream/<token>/<key>/<idx>/<name>` (Range; disk or librqbit stream)
//! - `GET /v1/sub/<token>/<key>/<idx>.vtt`
//! - `POST /v1/play` (Bearer pair secret + `X-Tide-Id`) — this Tide plays the URL
//! - `POST /v1/pair/start`, `POST /v1/pair/finish` (6-digit code shown on this Mac)
//! - `GET|HEAD /v1/relay/<rid>/<name>` (loopback only): the builtin player loads remote
//!   streams through 127.0.0.1 so WebKit never sees plain-http LAN media.
//! - Mac↔Mac sync (all Bearer + `X-Tide-Id`): `GET /v1/library` (this Mac's rows,
//!   offline MediaInfo, media files), `GET /v1/art/<key>/<poster|backdrop>` (sidecar
//!   bytes), `POST /v1/stream-token {key,fileIndex}` (tokenized stream + subtitle URLs,
//!   a `PlayRequest`), `GET /v1/sync/state?since=<ms>`, `POST /v1/sync/push` (sync.rs).
//! - Archive moves (all Bearer + `X-Tide-Id`, archive.rs): `GET /v1/archive/info`,
//!   `POST /v1/archive/begin`, `PUT /v1/archive/<upload>/<rel path>` (Content-Range),
//!   `POST /v1/archive/<upload>/commit`.
//!
//! Stream/subtitle tokens are random, per play request, bound to one torrent file
//! (+ its subtitle files) and expire; nothing else on disk is reachable.
//! macOS asks once to "allow incoming connections" for Tide when the server starts.
//! LAN-only traffic: not gated on the mobile-data mode.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::engine;
use crate::state::AppState;
use crate::types::{FileKind, MediaRef, PlayMode, PlayResult, SubtitleFormat, SubtitleTrack};

pub const PORTS: RangeInclusive<u16> = 47900..=47909;
const SERVICE: &str = "_tide._tcp";
const INSTANCE_PREFIX: &str = "Tide on ";
const GRANT_TTL: Duration = Duration::from_secs(24 * 3600);
const PAIR_TTL: Duration = Duration::from_secs(120);
const PAIR_MAX_ATTEMPTS: u32 = 5;
const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
/// `POST /v1/sync/push` carries watch state for the whole library.
const MAX_SYNC_BODY: usize = 8 * 1024 * 1024;
/// `POST /v1/archive/begin` may carry big .torrent files (base64).
const MAX_ARCHIVE_BEGIN: usize = 32 * 1024 * 1024;
/// Header phase vs body phase of a request (archive chunks take a while on slow Wi-Fi).
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(120);
const ART_CACHE: usize = 64;
const SUB_MAX_BYTES: usize = 2 * 1024 * 1024;

fn internal<E: std::fmt::Display>(e: E) -> String {
    format!("INTERNAL: {e}")
}

// ------------------------------------------------------------------ config / state

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub name: String,
    /// What the user typed / Bonjour reported (IP or `.local` name).
    pub host: String,
    pub port: u16,
    pub secret: String,
    /// Last IP that answered (hotspot IPs change between sessions).
    #[serde(default)]
    pub last_ip: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    self_id: String,
    #[serde(default)]
    devices: Vec<Device>,
}

struct Grant {
    key: String,
    idx: usize,
    subs: HashSet<usize>,
    expires: Instant,
}

struct PendingPair {
    code: String,
    expires: Instant,
    attempts: u32,
}

struct Running {
    task: tokio::task::JoinHandle<()>,
    advert: Option<tokio::process::Child>,
}

pub struct RemoteState {
    path: PathBuf,
    name: String,
    cfg: Mutex<Config>,
    port: AtomicU16,
    running: tokio::sync::Mutex<Option<Running>>,
    grants: Mutex<HashMap<String, Grant>>,
    relays: Mutex<HashMap<String, String>>,
    pairs: Mutex<HashMap<String, PendingPair>>,
    http: reqwest::Client,
    /// Last `/v1/library` answer per paired device (shown while it is offline).
    library: Mutex<HashMap<String, LibrarySnapshot>>,
    /// `(device, key, kind)` -> data URL of remote artwork.
    art: Mutex<Vec<((String, String, String), String)>>,
}

impl RemoteState {
    /// Load `data_dir/remote.json` (created with a fresh id when missing).
    pub fn load(data_dir: &Path, name: String) -> Self {
        let path = data_dir.join("remote.json");
        let mut cfg: Config = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Config { enabled: true, self_id: String::new(), devices: Vec::new() });
        let fresh = cfg.self_id.len() != 32;
        if fresh {
            cfg.self_id = random_hex(16);
        }
        let rs = RemoteState {
            path,
            name,
            cfg: Mutex::new(cfg),
            port: AtomicU16::new(0),
            running: tokio::sync::Mutex::new(None),
            grants: Mutex::new(HashMap::new()),
            relays: Mutex::new(HashMap::new()),
            pairs: Mutex::new(HashMap::new()),
            library: Mutex::new(HashMap::new()),
            art: Mutex::new(Vec::new()),
            http: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(Duration::from_secs(3))
                .read_timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        };
        if fresh {
            rs.save();
        }
        rs
    }

    pub fn self_id(&self) -> String {
        self.cfg.lock().unwrap().self_id.clone()
    }

    #[cfg(test)]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Paired devices.
    pub fn devices(&self) -> Vec<Device> {
        self.cfg.lock().unwrap().devices.clone()
    }

    pub fn port(&self) -> Option<u16> {
        match self.port.load(Ordering::SeqCst) {
            0 => None,
            p => Some(p),
        }
    }

    pub fn device(&self, id: &str) -> Option<Device> {
        self.cfg.lock().unwrap().devices.iter().find(|d| d.id == id).cloned()
    }

    pub(crate) fn upsert_device(&self, dev: Device) {
        {
            let mut c = self.cfg.lock().unwrap();
            c.devices.retain(|d| d.id != dev.id);
            c.devices.push(dev);
        }
        self.save();
    }

    /// Update host/port/last_ip of a paired device; true if anything changed.
    fn touch_device(&self, id: &str, host: Option<&str>, port: Option<u16>, ip: Option<IpAddr>) -> bool {
        let changed = {
            let mut c = self.cfg.lock().unwrap();
            let Some(d) = c.devices.iter_mut().find(|d| d.id == id) else { return false };
            let before = d.clone();
            if let Some(h) = host {
                d.host = h.to_string();
            }
            if let Some(p) = port {
                d.port = p;
            }
            if let Some(ip) = ip {
                d.last_ip = Some(ip.to_string());
            }
            *d != before
        };
        if changed {
            self.save();
        }
        changed
    }

    fn save(&self) {
        let bytes = match serde_json::to_vec_pretty(&*self.cfg.lock().unwrap()) {
            Ok(b) => b,
            Err(e) => return tracing::warn!("remote.json serialize: {e}"),
        };
        let tmp = self.path.with_extension("json.tmp");
        let res = (|| {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600) // holds pair secrets
                .open(&tmp)?;
            f.write_all(&bytes)?;
            std::fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = res {
            tracing::warn!("remote.json save failed: {e}");
        }
    }

    /// Issue a stream token for one file (+ its subtitle files).
    fn grant(&self, key: &str, idx: usize, subs: impl IntoIterator<Item = usize>) -> String {
        let token = random_hex(16);
        let mut g = self.grants.lock().unwrap();
        let now = Instant::now();
        g.retain(|_, v| v.expires > now);
        g.insert(
            token.clone(),
            Grant { key: key.to_string(), idx, subs: subs.into_iter().collect(), expires: now + GRANT_TTL },
        );
        token
    }

    fn check_grant(&self, token: &str, key: &str, idx: usize, sub: bool) -> bool {
        let g = self.grants.lock().unwrap();
        match g.get(token) {
            Some(gr) if gr.expires > Instant::now() && gr.key == key => {
                if sub {
                    gr.subs.contains(&idx)
                } else {
                    gr.idx == idx
                }
            }
            _ => false,
        }
    }

    fn add_relay(&self, url: String) -> String {
        let rid = random_hex(16);
        let mut r = self.relays.lock().unwrap();
        if r.len() > 32 {
            r.clear();
        }
        r.insert(rid.clone(), url);
        rid
    }
}

// ------------------------------------------------------------------ small helpers

/// `n` random bytes as hex (from /dev/urandom; no rand crate).
fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    {
        use std::io::Read;
        let ok = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_ok();
        assert!(ok, "/dev/urandom unavailable");
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn random_code() -> String {
    let n = u64::from_str_radix(&random_hex(6), 16).unwrap_or(0) % 1_000_000;
    format!("{n:06}")
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn clean_name(s: &str) -> String {
    let n: String = s.chars().filter(|c| !c.is_control()).take(64).collect();
    let n = n.trim().to_string();
    if n.is_empty() {
        "Mac".into()
    } else {
        n
    }
}

/// `scutil --get ComputerName`, else the host name.
pub fn computer_name() -> String {
    let out = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .stderr(Stdio::null())
        .output();
    if let Ok(o) = out {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && !s.is_empty() {
            return clean_name(&s);
        }
    }
    clean_name(&std::env::var("HOST").unwrap_or_else(|_| "Mac".into()))
}

pub(crate) fn url_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v) => v.to_string(),
        IpAddr::V6(v) => format!("[{v}]"),
    }
}

/// Local IP of the interface that routes to `target` (no packet is sent).
fn local_ip_for(target: SocketAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if target.is_ipv4() { "0.0.0.0:0".parse().ok()? } else { "[::]:0".parse().ok()? };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect(target).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// This Mac's LAN IPv4 (what a user types on the other Mac).
pub fn lan_ipv4() -> Option<Ipv4Addr> {
    if let Some(IpAddr::V4(v)) = local_ip_for(SocketAddr::from(([192, 0, 2, 1], 9))) {
        if !v.is_loopback() {
            return Some(v);
        }
    }
    // No default route (e.g. hotspot without internet): ask the Wi-Fi/Ethernet ifaces.
    for ifc in ["en0", "en1", "en2"] {
        let o = std::process::Command::new("/usr/sbin/ipconfig").args(["getifaddr", ifc]).output().ok()?;
        if let Ok(v) = String::from_utf8_lossy(&o.stdout).trim().parse::<Ipv4Addr>() {
            return Some(v);
        }
    }
    None
}

async fn resolve(host: &str, port: u16) -> Option<SocketAddr> {
    if let Ok(ip) = host.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    let addrs: Vec<SocketAddr> =
        tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((host, port))).await.ok()?.ok()?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied()
}

async fn tcp_ok(addr: SocketAddr, ms: u64) -> bool {
    matches!(tokio::time::timeout(Duration::from_millis(ms), TcpStream::connect(addr)).await, Ok(Ok(_)))
}

/// Content type by extension (AVFoundation is picky for mp4/mov).
fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "mp4" => "video/mp4",
        "m4v" => "video/x-m4v",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "ts" => "video/mp2t",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

// ------------------------------------------------------------------ HTTP plumbing

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    /// Raw query string (after `?`), may be empty.
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn query_param(&self, name: &str) -> Option<String> {
        self.query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| {
            urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_else(|_| v.to_string())
        })
    }
}

async fn read_request<R: AsyncRead + Unpin>(sock: &mut R) -> std::io::Result<Request> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    let timed_out = || std::io::Error::new(std::io::ErrorKind::TimedOut, "request timeout");
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = vec![0u8; 64 * 1024];
    let deadline = tokio::time::Instant::now() + HEAD_TIMEOUT;
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > MAX_HEADER {
            return Err(bad("header too large"));
        }
        let n = tokio::time::timeout_at(deadline, sock.read(&mut tmp)).await.map_err(|_| timed_out())??;
        if n == 0 {
            return Err(bad("eof"));
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut rl = lines.next().unwrap_or("").split(' ');
    let method = rl.next().unwrap_or("").to_string();
    let target = rl.next().unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let limit = if path == "/v1/sync/push" {
        MAX_SYNC_BODY
    } else if path == "/v1/archive/begin" {
        MAX_ARCHIVE_BEGIN
    } else if method == "PUT" && path.starts_with("/v1/archive/") {
        crate::archive::MAX_CHUNK
    } else {
        MAX_BODY
    };
    if len > limit {
        return Err(bad("body too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    body.reserve(len.saturating_sub(body.len()));
    let deadline = tokio::time::Instant::now() + BODY_TIMEOUT;
    while body.len() < len {
        let n = tokio::time::timeout_at(deadline, sock.read(&mut tmp)).await.map_err(|_| timed_out())??;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    Ok(Request { method, path, query, headers, body })
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        409 => "Conflict",
        503 => "Service Unavailable",
        507 => "Insufficient Storage",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        410 => "Gone",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

async fn write_head<W: AsyncWriteExt + Unpin>(w: &mut W, code: u16, headers: &[(&str, String)]) -> std::io::Result<()> {
    let mut s = format!("HTTP/1.1 {code} {}\r\n", reason(code));
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("Connection: close\r\n\r\n");
    w.write_all(s.as_bytes()).await
}

async fn respond<W: AsyncWriteExt + Unpin>(w: &mut W, code: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    write_head(w, code, &[("Content-Type", ctype.to_string()), ("Content-Length", body.len().to_string())]).await?;
    w.write_all(body).await?;
    w.flush().await
}

async fn respond_json<W: AsyncWriteExt + Unpin>(w: &mut W, code: u16, v: serde_json::Value) -> std::io::Result<()> {
    respond(w, code, "application/json", v.to_string().as_bytes()).await
}

async fn respond_err<W: AsyncWriteExt + Unpin>(w: &mut W, code: u16, msg: &str) -> std::io::Result<()> {
    respond_json(w, code, json!({ "error": msg })).await
}

/// `Range` → inclusive byte span. `Ok(None)`: whole file; `Err`: 416.
fn parse_range(h: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(h) = h else { return Ok(None) };
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return Ok(None) };
    let first = spec.split(',').next().unwrap_or("").trim();
    let (a, b) = first.split_once('-').ok_or(())?;
    let (a, b) = (a.trim(), b.trim());
    if size == 0 {
        return Err(());
    }
    if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        return Ok(Some((size.saturating_sub(n), size - 1)));
    }
    let start: u64 = a.parse().map_err(|_| ())?;
    let end: u64 = if b.is_empty() { size - 1 } else { b.parse::<u64>().map_err(|_| ())?.min(size - 1) };
    if start >= size || end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

pub(crate) trait Media: AsyncRead + AsyncSeek + Send + Unpin {}
impl<T: AsyncRead + AsyncSeek + Send + Unpin> Media for T {}

pub(crate) struct Opened {
    reader: Box<dyn Media>,
    size: u64,
    name: String,
}

async fn serve_range<W: AsyncWriteExt + Unpin>(w: &mut W, req: &Request, mut o: Opened) -> std::io::Result<()> {
    let ctype = mime_for(&o.name).to_string();
    let (code, start, end) = match parse_range(req.header("range"), o.size) {
        Ok(Some((s, e))) => (206, s, e),
        Ok(None) => (200, 0, o.size.saturating_sub(1)),
        Err(()) => {
            return write_head(w, 416, &[("Content-Range", format!("bytes */{}", o.size)), ("Content-Length", "0".into())])
                .await
        }
    };
    let len = if o.size == 0 { 0 } else { end - start + 1 };
    let mut h = vec![
        ("Content-Type", ctype),
        ("Accept-Ranges", "bytes".to_string()),
        ("Content-Length", len.to_string()),
    ];
    if code == 206 {
        h.push(("Content-Range", format!("bytes {start}-{end}/{}", o.size)));
    }
    write_head(w, code, &h).await?;
    if req.method == "HEAD" || len == 0 {
        return w.flush().await;
    }
    o.reader.seek(std::io::SeekFrom::Start(start)).await?;
    let mut r = tokio::io::BufReader::with_capacity(256 * 1024, o.reader.take(len));
    tokio::io::copy_buf(&mut r, w).await?;
    w.flush().await
}

/// Forward a GET/HEAD (with Range) to `url` and stream the answer back.
async fn proxy<W: AsyncWriteExt + Unpin>(w: &mut W, http: &reqwest::Client, req: &Request, url: &str) -> std::io::Result<()> {
    let mut rb = if req.method == "HEAD" { http.head(url) } else { http.get(url) };
    if let Some(r) = req.header("range") {
        rb = rb.header("Range", r);
    }
    let mut resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => return respond_err(w, 502, &format!("upstream: {e}")).await,
    };
    let mut h: Vec<(&str, String)> = Vec::new();
    for name in ["content-type", "content-length", "content-range", "accept-ranges"] {
        if let Some(v) = resp.headers().get(name).and_then(|v| v.to_str().ok()) {
            h.push((name, v.to_string()));
        }
    }
    write_head(w, resp.status().as_u16(), &h).await?;
    if req.method != "HEAD" {
        while let Ok(Some(chunk)) = resp.chunk().await {
            w.write_all(&chunk).await?;
        }
    }
    w.flush().await
}

// ------------------------------------------------------------------ server core

pub(crate) type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the server needs from the app (mocked in tests).
pub(crate) trait Backend: Send + Sync + 'static {
    fn open(&self, key: String, idx: usize) -> BoxFut<'_, Result<Opened, String>>;
    fn sub_vtt(&self, key: String, idx: usize) -> BoxFut<'_, Result<String, String>>;
    /// Play an already validated request on this Mac.
    fn play(&self, req: PlayRequest) -> BoxFut<'_, Result<(), String>>;
    fn emit(&self, event: &str, payload: serde_json::Value);

    /// This Mac's library for `GET /v1/library` (no network).
    fn library(&self) -> BoxFut<'_, Result<Library, String>> {
        Box::pin(async { Err("NOT_FOUND: no library".to_string()) })
    }
    /// Sidecar artwork `(bytes, mime)`; `kind` is "poster" | "backdrop".
    fn art(&self, _key: String, _kind: String) -> BoxFut<'_, Result<(Vec<u8>, String), String>> {
        Box::pin(async { Err("NOT_FOUND: no artwork".to_string()) })
    }
    /// Title + subtitle files of a media file (for `POST /v1/stream-token`).
    fn stream_info(&self, _key: String, _idx: usize) -> BoxFut<'_, Result<StreamInfo, String>> {
        Box::pin(async { Err("NOT_FOUND: no such file".to_string()) })
    }
    /// `GET /v1/sync/state?since=` from paired device `from`.
    fn sync_state(&self, _from: &str, _since: u64) -> Result<serde_json::Value, String> {
        Err("NOT_FOUND: sync unavailable".into())
    }
    /// `POST /v1/sync/push` from paired device `from`.
    fn sync_push(&self, _from: &str, _body: serde_json::Value) -> Result<serde_json::Value, String> {
        Err("NOT_FOUND: sync unavailable".into())
    }
    /// Receiving side of archive moves (archive.rs).
    fn archive(&self) -> Option<Arc<crate::archive::Receiver>> {
        None
    }
}

/// What `stream-token` needs to know about one media file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StreamInfo {
    pub title: String,
    /// (torrent file index, label, lang), default subtitle first.
    pub subs: Vec<(usize, String, Option<String>)>,
    pub playable_builtin: bool,
}

/// `GET /v1/library` body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub rows: Vec<crate::types::TorrentRow>,
    #[serde(default)]
    pub media: std::collections::BTreeMap<String, crate::types::MediaInfo>,
    #[serde(default)]
    pub files: std::collections::BTreeMap<String, Vec<crate::types::FileRow>>,
}

/// Cached library of one paired device.
#[derive(Debug, Clone)]
struct LibrarySnapshot {
    name: String,
    lib: Library,
}

/// One entry of `remote_library`.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteLibrary {
    pub device_id: String,
    pub device_name: String,
    pub online: bool,
    pub rows: Vec<crate::types::TorrentRow>,
    pub media: std::collections::BTreeMap<String, crate::types::MediaInfo>,
    pub files: std::collections::BTreeMap<String, Vec<crate::types::FileRow>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteSub {
    pub label: String,
    #[serde(default)]
    pub lang: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlayRequest {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub subtitles: Vec<RemoteSub>,
    #[serde(default, alias = "modeHint")]
    pub mode_hint: Option<String>,
}

struct Ctx {
    rs: Arc<RemoteState>,
    be: Arc<dyn Backend>,
}

async fn serve(listener: TcpListener, ctx: Arc<Ctx>) {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!("remote accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let _ = sock.set_nodelay(true);
            if let Err(e) = handle_conn(sock, peer, &ctx).await {
                tracing::debug!("remote conn {peer}: {e}");
            }
        });
    }
}

async fn handle_conn(mut sock: TcpStream, peer: SocketAddr, ctx: &Ctx) -> std::io::Result<()> {
    let req = match read_request(&mut sock).await {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return Ok(()),
        Err(e) => return Err(e),
    };
    route(&mut sock, &req, peer.ip().to_canonical(), ctx).await
}

/// URL must be `http://<peer ip>:<port>/v1/...` (a paired Mac can only make us fetch
/// from itself).
fn url_from_peer(url: &str, peer: IpAddr) -> bool {
    if url.len() > 4096 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let (auth, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => return false,
    };
    if !path.starts_with("/v1/") || auth.contains('@') {
        return false;
    }
    let host = if let Some(h) = auth.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        auth.rsplit_once(':').map(|(h, _)| h).unwrap_or(auth)
    };
    host.parse::<IpAddr>().map(|ip| ip.to_canonical() == peer).unwrap_or(false)
}

async fn route(w: &mut TcpStream, req: &Request, peer: IpAddr, ctx: &Ctx) -> std::io::Result<()> {
    let segs: Vec<&str> = req.path.trim_start_matches('/').split('/').collect();
    let get = req.method == "GET" || req.method == "HEAD";
    match (req.method.as_str(), segs.as_slice()) {
        (_, ["v1", "stream", token, key, idx, _name]) if get => {
            let key = key.to_ascii_lowercase();
            let Ok(idx) = idx.parse::<usize>() else { return respond_err(w, 404, "not found").await };
            if !is_hex(&key, 40) || !ctx.rs.check_grant(token, &key, idx, false) {
                return respond_err(w, 401, "unauthorized").await;
            }
            match ctx.be.open(key, idx).await {
                Ok(o) => serve_range(w, req, o).await,
                Err(e) => respond_err(w, 404, &e).await,
            }
        }
        (_, ["v1", "sub", token, key, file]) if get => {
            let key = key.to_ascii_lowercase();
            let Some(Ok(idx)) = file.strip_suffix(".vtt").map(str::parse::<usize>) else {
                return respond_err(w, 404, "not found").await;
            };
            if !is_hex(&key, 40) || !ctx.rs.check_grant(token, &key, idx, true) {
                return respond_err(w, 401, "unauthorized").await;
            }
            match ctx.be.sub_vtt(key, idx).await {
                Ok(v) => respond(w, 200, "text/vtt; charset=utf-8", v.as_bytes()).await,
                Err(e) => respond_err(w, 404, &e).await,
            }
        }
        (_, ["v1", "relay", rid, _name]) if get => {
            let url = ctx.rs.relays.lock().unwrap().get(*rid).cloned();
            match url {
                Some(u) if peer.is_loopback() => proxy(w, &ctx.rs.http, req, &u).await,
                _ => respond_err(w, 401, "unauthorized").await,
            }
        }
        ("POST", ["v1", "play"]) => {
            let Some(id) = authed(req, ctx) else { return respond_err(w, 401, "unauthorized").await };
            let id = id.as_str();
            let Ok(pr) = serde_json::from_slice::<PlayRequest>(&req.body) else {
                return respond_err(w, 400, "bad request").await;
            };
            if !url_from_peer(&pr.url, peer) || !pr.subtitles.iter().all(|s| url_from_peer(&s.url, peer)) {
                return respond_err(w, 400, "urls must point at the sender").await;
            }
            ctx.rs.touch_device(id, None, None, Some(peer));
            match ctx.be.play(pr).await {
                Ok(()) => respond_json(w, 200, json!({ "ok": true })).await,
                Err(e) => respond_err(w, 500, &e).await,
            }
        }
        ("GET", ["v1", "library"]) => {
            let Some(id) = authed(req, ctx) else { return respond_err(w, 401, "unauthorized").await };
            ctx.rs.touch_device(&id, None, None, Some(peer));
            match ctx.be.library().await {
                Ok(lib) => {
                    let mut v = serde_json::to_value(&lib).unwrap_or_default();
                    v["device_id"] = json!(ctx.rs.self_id());
                    v["device_name"] = json!(ctx.rs.name);
                    respond_json(w, 200, v).await
                }
                Err(e) => respond_err(w, 500, &e).await,
            }
        }
        ("GET", ["v1", "art", key, kind]) => {
            if authed(req, ctx).is_none() {
                return respond_err(w, 401, "unauthorized").await;
            }
            let key = key.to_ascii_lowercase();
            if !is_hex(&key, 40) || !matches!(*kind, "poster" | "backdrop") {
                return respond_err(w, 404, "not found").await;
            }
            match ctx.be.art(key, kind.to_string()).await {
                Ok((bytes, mime)) => respond(w, 200, &mime, &bytes).await,
                Err(e) => respond_err(w, 404, &e).await,
            }
        }
        ("POST", ["v1", "stream-token"]) => {
            let Some(id) = authed(req, ctx) else { return respond_err(w, 401, "unauthorized").await };
            #[derive(Deserialize)]
            struct TokReq {
                key: String,
                #[serde(alias = "fileIndex")]
                file_index: usize,
            }
            let Ok(tr) = serde_json::from_slice::<TokReq>(&req.body) else {
                return respond_err(w, 400, "bad request").await;
            };
            let key = tr.key.to_ascii_lowercase();
            if !is_hex(&key, 40) {
                return respond_err(w, 400, "bad key").await;
            }
            ctx.rs.touch_device(&id, None, None, Some(peer));
            let info = match ctx.be.stream_info(key.clone(), tr.file_index).await {
                Ok(i) => i,
                Err(e) => return respond_err(w, 404, &e).await,
            };
            // URLs use the address the peer reached us at.
            let Ok(local) = w.local_addr() else { return respond_err(w, 500, "no local address").await };
            let port = ctx.rs.port().unwrap_or(local.port());
            let base = format!("http://{}:{port}", url_host(local.ip().to_canonical()));
            let pr = build_request(&ctx.rs, &base, &key, tr.file_index, &info);
            respond_json(w, 200, serde_json::to_value(&pr).unwrap_or_default()).await
        }
        ("GET", ["v1", "sync", "state"]) => {
            let Some(id) = authed(req, ctx) else { return respond_err(w, 401, "unauthorized").await };
            ctx.rs.touch_device(&id, None, None, Some(peer));
            let since = req.query_param("since").and_then(|s| s.parse().ok()).unwrap_or(0);
            match ctx.be.sync_state(&id, since) {
                Ok(v) => respond_json(w, 200, v).await,
                Err(e) => respond_err(w, 500, &e).await,
            }
        }
        ("POST", ["v1", "sync", "push"]) => {
            let Some(id) = authed(req, ctx) else { return respond_err(w, 401, "unauthorized").await };
            let Ok(body) = serde_json::from_slice::<serde_json::Value>(&req.body) else {
                return respond_err(w, 400, "bad request").await;
            };
            ctx.rs.touch_device(&id, None, None, Some(peer));
            match ctx.be.sync_push(&id, body) {
                Ok(v) => respond_json(w, 200, v).await,
                Err(e) => respond_err(w, 400, &e).await,
            }
        }
        (_, ["v1", "archive", ..]) => archive_srv(w, req, peer, &segs, ctx).await,
        ("POST", ["v1", "pair", "start"]) => pair_start_srv(w, req, ctx).await,
        ("POST", ["v1", "pair", "finish"]) => pair_finish_srv(w, req, peer, ctx).await,
        (m, ["v1", ..]) if m != "GET" && m != "HEAD" && m != "POST" => respond_err(w, 405, "method").await,
        _ => respond_err(w, 404, "not found").await,
    }
}

/// HTTP status for an archive error string ("CODE: message").
fn archive_status(e: &str) -> u16 {
    match e.split(':').next().unwrap_or("") {
        "INVALID_INPUT" => 400,
        "UNAUTHORIZED" => 401,
        "NOT_FOUND" => 404,
        "RANGE_MISMATCH" | "BUSY" => 409,
        "ARCHIVE_UNAVAILABLE" | "NOT_ARCHIVE" => 503,
        "ARCHIVE_FULL" => 507,
        _ => 500,
    }
}

/// `/v1/archive/...` (paired devices only).
async fn archive_srv(w: &mut TcpStream, req: &Request, peer: IpAddr, segs: &[&str], ctx: &Ctx) -> std::io::Result<()> {
    let Some(id) = authed(req, ctx) else { return respond_err(w, 401, "unauthorized").await };
    ctx.rs.touch_device(&id, None, None, Some(peer));
    let Some(rcv) = ctx.be.archive() else { return respond_err(w, 503, "NOT_ARCHIVE: archive unavailable").await };
    let done = |r: Result<serde_json::Value, String>| match r {
        Ok(v) => (200, v),
        Err(e) => (archive_status(&e), json!({ "error": e })),
    };
    let (code, v) = match (req.method.as_str(), segs) {
        ("GET", ["v1", "archive", "info"]) => (200, rcv.info()),
        ("POST", ["v1", "archive", "begin"]) => match serde_json::from_slice::<crate::archive::BeginReq>(&req.body) {
            Ok(b) => done(rcv.begin(&id, b).await),
            Err(e) => (400, json!({ "error": format!("INVALID_INPUT: {e}") })),
        },
        ("POST", ["v1", "archive", upload, "commit"]) => done(rcv.commit(&id, upload).await),
        ("PUT", ["v1", "archive", upload, rest @ ..]) if !rest.is_empty() => {
            let rel: Option<Vec<String>> =
                rest.iter().map(|s| urlencoding::decode(s).ok().map(|c| c.into_owned())).collect();
            let range = req.header("content-range").and_then(crate::archive::parse_content_range);
            match (rel, range) {
                (Some(rel), Some(range)) => match rcv.put(&id, upload, &rel.join("/"), range, &req.body).await {
                    Ok(have) => (200, json!({ "have": have })),
                    Err((e, have)) => (archive_status(&e), json!({ "error": e, "have": have })),
                },
                _ => (400, json!({ "error": "INVALID_INPUT: bad path or Content-Range" })),
            }
        }
        _ => (404, json!({ "error": "not found" })),
    };
    respond_json(w, code, v).await
}

/// Paired device id when the request carries its Bearer secret + `X-Tide-Id`.
fn authed(req: &Request, ctx: &Ctx) -> Option<String> {
    let id = req.header("x-tide-id").unwrap_or("");
    let bearer = req.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let ok = !bearer.is_empty() && ctx.rs.device(id).is_some_and(|d| ct_eq(d.secret.as_bytes(), bearer.as_bytes()));
    ok.then(|| id.to_string())
}

/// Grant a token for `key`/`idx` (+ its subtitle files) and build the URLs under `base`.
fn build_request(rs: &RemoteState, base: &str, key: &str, idx: usize, info: &StreamInfo) -> PlayRequest {
    let token = rs.grant(key, idx, info.subs.iter().map(|(i, _, _)| *i));
    PlayRequest {
        url: format!("{base}/v1/stream/{token}/{key}/{idx}/{}", urlencoding::encode(&info.title)),
        subtitles: info
            .subs
            .iter()
            .map(|(i, label, lang)| RemoteSub { label: label.clone(), lang: lang.clone(), url: format!("{base}/v1/sub/{token}/{key}/{i}.vtt") })
            .collect(),
        mode_hint: Some(if info.playable_builtin { "builtin" } else { "vlc" }.into()),
        title: info.title.clone(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PairHello {
    id: String,
    name: String,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    code: Option<String>,
}

async fn pair_start_srv(w: &mut TcpStream, req: &Request, ctx: &Ctx) -> std::io::Result<()> {
    let Ok(h) = serde_json::from_slice::<PairHello>(&req.body) else { return respond_err(w, 400, "bad request").await };
    if !is_hex(&h.id, 32) {
        return respond_err(w, 400, "bad id").await;
    }
    let code = random_code();
    let full = {
        let mut p = ctx.rs.pairs.lock().unwrap();
        let now = Instant::now();
        p.retain(|_, v| v.expires > now);
        let full = p.len() >= 8 && !p.contains_key(&h.id);
        if !full {
            p.insert(h.id.clone(), PendingPair { code: code.clone(), expires: now + PAIR_TTL, attempts: 0 });
        }
        full
    };
    if full {
        return respond_err(w, 429, "too many pairing requests").await;
    }
    ctx.be.emit("remote-pair-request", json!({ "code": code, "from": clean_name(&h.name) }));
    respond_json(w, 200, json!({ "id": ctx.rs.self_id(), "name": ctx.rs.name })).await
}

async fn pair_finish_srv(w: &mut TcpStream, req: &Request, peer: IpAddr, ctx: &Ctx) -> std::io::Result<()> {
    let Ok(h) = serde_json::from_slice::<PairHello>(&req.body) else { return respond_err(w, 400, "bad request").await };
    let code = h.code.clone().unwrap_or_default();
    let verdict = {
        let mut p = ctx.rs.pairs.lock().unwrap();
        match p.get_mut(&h.id) {
            None => Err((410, "no pairing in progress")),
            Some(pp) if pp.expires <= Instant::now() => {
                p.remove(&h.id);
                Err((410, "code expired"))
            }
            Some(pp) if !ct_eq(pp.code.as_bytes(), code.trim().as_bytes()) => {
                pp.attempts += 1;
                if pp.attempts >= PAIR_MAX_ATTEMPTS {
                    p.remove(&h.id);
                    Err((429, "too many wrong codes"))
                } else {
                    Err((401, "wrong code"))
                }
            }
            Some(_) => {
                p.remove(&h.id);
                Ok(())
            }
        }
    };
    if let Err((c, m)) = verdict {
        return respond_err(w, c, m).await;
    }
    let secret = random_hex(32);
    ctx.rs.upsert_device(Device {
        id: h.id.clone(),
        name: clean_name(&h.name),
        host: peer.to_string(),
        port: if PORTS.contains(&h.port) { h.port } else { *PORTS.start() },
        secret: secret.clone(),
        last_ip: Some(peer.to_string()),
    });
    ctx.be.emit("remote-devices-updated", serde_json::Value::Null);
    respond_json(w, 200, json!({ "id": ctx.rs.self_id(), "name": ctx.rs.name, "secret": secret })).await
}

// ------------------------------------------------------------------ client side

pub(crate) async fn post_json(
    rs: &RemoteState,
    addr: SocketAddr,
    path: &str,
    auth: Option<(&str, &str)>,
    body: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let url = format!("http://{}:{}{path}", url_host(addr.ip()), addr.port());
    let mut rb = rs
        .http
        .post(&url)
        .timeout(timeout)
        .header("Content-Type", "application/json")
        .body(body.to_string());
    if let Some((id, secret)) = auth {
        rb = rb.header("X-Tide-Id", id).header("Authorization", format!("Bearer {secret}"));
    }
    let resp = rb.send().await.map_err(|e| format!("UNREACHABLE: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(|e| format!("UNREACHABLE: {e}"))?;
    answer(status, text.as_bytes())
}

pub(crate) fn answer(status: u16, body: &[u8]) -> Result<serde_json::Value, String> {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    if (200..300).contains(&status) {
        Ok(v)
    } else {
        let msg = v.get("error").and_then(|e| e.as_str()).unwrap_or("request failed");
        let kind = match status {
            401 => "UNAUTHORIZED",
            404 => "NOT_FOUND",
            410 => "EXPIRED",
            429 => "RATE_LIMITED",
            _ => "REMOTE_ERROR",
        };
        Err(format!("{kind}: {msg}"))
    }
}

/// Authenticated GET → (content type, body). Non-2xx becomes "CODE: msg".
pub(crate) async fn get_raw(
    rs: &RemoteState,
    addr: SocketAddr,
    path: &str,
    auth: (&str, &str),
    timeout: Duration,
) -> Result<(String, Vec<u8>), String> {
    let url = format!("http://{}:{}{path}", url_host(addr.ip()), addr.port());
    let resp = rs
        .http
        .get(&url)
        .timeout(timeout)
        .header("X-Tide-Id", auth.0)
        .header("Authorization", format!("Bearer {}", auth.1))
        .send()
        .await
        .map_err(|e| format!("UNREACHABLE: {e}"))?;
    let status = resp.status().as_u16();
    let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = resp.bytes().await.map_err(|e| format!("UNREACHABLE: {e}"))?.to_vec();
    if (200..300).contains(&status) {
        Ok((ctype, body))
    } else {
        answer(status, &body).map(|_| (ctype, Vec::new()))
    }
}

pub(crate) async fn get_json(
    rs: &RemoteState,
    addr: SocketAddr,
    path: &str,
    auth: (&str, &str),
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let (_, body) = get_raw(rs, addr, path, auth, timeout).await?;
    serde_json::from_slice(&body).map_err(|e| format!("REMOTE_ERROR: bad answer: {e}"))
}

/// Quick reachability of a paired device (saved host, then last IP; no Bonjour).
pub(crate) async fn quick_addr(dev: &Device, ms: u64) -> Option<SocketAddr> {
    for h in std::iter::once(dev.host.clone()).chain(dev.last_ip.clone()) {
        if let Some(a) = resolve(&h, dev.port).await {
            if tcp_ok(a, ms).await {
                return Some(a);
            }
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
}

async fn client_pair_start(rs: &RemoteState, addr: SocketAddr) -> Result<PeerInfo, String> {
    let body = json!({ "id": rs.self_id(), "name": rs.name, "port": rs.port().unwrap_or(0) });
    let v = post_json(rs, addr, "/v1/pair/start", None, body, Duration::from_secs(10)).await?;
    serde_json::from_value(v).map_err(internal)
}

async fn client_pair_finish(rs: &RemoteState, host: &str, addr: SocketAddr, code: &str) -> Result<Device, String> {
    let body = json!({ "id": rs.self_id(), "name": rs.name, "port": rs.port().unwrap_or(0), "code": code.trim() });
    let v = post_json(rs, addr, "/v1/pair/finish", None, body, Duration::from_secs(10)).await?;
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
    let secret = v.get("secret").and_then(|x| x.as_str()).unwrap_or("");
    if !is_hex(id, 32) || !is_hex(secret, 64) {
        return Err("REMOTE_ERROR: bad pairing answer".into());
    }
    let dev = Device {
        id: id.to_string(),
        name: clean_name(v.get("name").and_then(|x| x.as_str()).unwrap_or("Mac")),
        host: host.to_string(),
        port: addr.port(),
        secret: secret.to_string(),
        last_ip: Some(addr.ip().to_string()),
    };
    rs.upsert_device(dev.clone());
    Ok(dev)
}

/// Find a working address for a paired device: saved host, last-known IP, then a
/// Bonjour re-resolve by id (hotspot IPs change between sessions).
pub(crate) async fn reach(rs: &RemoteState, dev: &Device) -> Result<SocketAddr, String> {
    let mut tried = HashSet::new();
    let mut cands: Vec<(String, u16)> = vec![(dev.host.clone(), dev.port)];
    if let Some(ip) = &dev.last_ip {
        cands.push((ip.clone(), dev.port));
    }
    for (h, p) in cands {
        if let Some(a) = resolve(&h, p).await {
            if tried.insert(a) && tcp_ok(a, 1500).await {
                rs.touch_device(&dev.id, None, None, Some(a.ip()));
                return Ok(a);
            }
        }
    }
    for f in discover(rs).await {
        if f.id.as_deref() == Some(dev.id.as_str()) {
            if let Some(a) = resolve(&f.host, f.port).await {
                if tcp_ok(a, 1500).await {
                    rs.touch_device(&dev.id, Some(&f.host), Some(f.port), Some(a.ip()));
                    return Ok(a);
                }
            }
        }
    }
    Err(format!("UNREACHABLE: {} is not reachable (is Tide open there with Remote play on?)", dev.name))
}

// ------------------------------------------------------------------ Bonjour (dns-sd)

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub id: Option<String>,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub paired: bool,
}

/// Instance names currently present in `dns-sd -B` output.
fn parse_browse(out: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 7 || !cols[5].starts_with(SERVICE) {
            continue;
        }
        // Instance name is everything after the 6th column (may contain spaces).
        let mut rest = line;
        for _ in 0..6 {
            rest = rest.trim_start();
            rest = &rest[rest.find(char::is_whitespace).unwrap_or(rest.len())..];
        }
        let name = rest.trim().to_string();
        match cols[1] {
            "Add" if !v.contains(&name) => v.push(name),
            "Rmv" => v.retain(|n| n != &name),
            _ => {}
        }
    }
    v
}

/// (host, port, TXT id) from `dns-sd -L` output.
fn parse_resolve(out: &str) -> Option<(String, u16, Option<String>)> {
    let mut lines = out.lines();
    while let Some(l) = lines.next() {
        if let Some(i) = l.find("can be reached at ") {
            let rest = &l[i + "can be reached at ".len()..];
            let hp = rest.split_whitespace().next()?;
            let (h, p) = hp.rsplit_once(':')?;
            let port: u16 = p.parse().ok()?;
            let host = h.trim_end_matches('.').to_string();
            let id = lines
                .next()
                .and_then(|t| t.split_whitespace().find_map(|kv| kv.strip_prefix("id=")))
                .filter(|id| is_hex(id, 32))
                .map(str::to_string);
            return Some((host, port, id));
        }
    }
    None
}

/// Run dns-sd for at most `dur`, return what it printed. `stop_after` ends early once a
/// line containing it and one more line were read.
async fn dns_sd(args: &[&str], dur: Duration, stop_after: Option<&str>) -> String {
    let child = tokio::process::Command::new("/usr/bin/dns-sd")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else { return String::new() };
    let Some(stdout) = child.stdout.take() else { return String::new() };
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut out = String::new();
    let deadline = tokio::time::Instant::now() + dur;
    let mut extra: Option<usize> = None;
    while let Ok(Ok(Some(l))) = tokio::time::timeout_at(deadline, lines.next_line()).await {
        out.push_str(&l);
        out.push('\n');
        if let Some(n) = extra.as_mut() {
            *n -= 1;
            if *n == 0 {
                break;
            }
        } else if stop_after.is_some_and(|s| l.contains(s)) {
            extra = Some(1);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    out
}

async fn discover(rs: &RemoteState) -> Vec<Found> {
    let names = parse_browse(&dns_sd(&["-B", SERVICE, "local"], Duration::from_millis(1800), None).await);
    let mut set = tokio::task::JoinSet::new();
    for n in names {
        set.spawn(async move {
            let out = dns_sd(&["-L", &n, SERVICE, "local"], Duration::from_millis(2500), Some("can be reached at")).await;
            parse_resolve(&out).map(|r| (n, r))
        });
    }
    let me = rs.self_id();
    let mut found = Vec::new();
    while let Some(r) = set.join_next().await {
        let Ok(Some((name, (host, port, id)))) = r else { continue };
        if id.as_deref() == Some(me.as_str()) {
            continue;
        }
        let paired = id.as_deref().is_some_and(|i| rs.device(i).is_some());
        let name = name.strip_prefix(INSTANCE_PREFIX).unwrap_or(&name).to_string();
        found.push(Found { id, name, host, port, paired });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// `dns-sd -R` kept alive by a shell that kills it when our stdin pipe closes, so the
/// advert also goes away if Tide exits without cleanup.
fn advertise(name: &str, port: u16, id: &str) -> Option<tokio::process::Child> {
    tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"/usr/bin/dns-sd -R "$1" _tide._tcp local "$2" "id=$3" >/dev/null 2>&1 & p=$!; read _; kill $p"#)
        .arg("sh")
        .arg(format!("{INSTANCE_PREFIX}{name}"))
        .arg(port.to_string())
        .arg(id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| tracing::warn!("dns-sd advert failed: {e}"))
        .ok()
}

// ------------------------------------------------------------------ server lifecycle

async fn bind_any(ip: IpAddr) -> Option<TcpListener> {
    for p in PORTS {
        if let Ok(l) = TcpListener::bind(SocketAddr::new(ip, p)).await {
            return Some(l);
        }
    }
    None
}

async fn start_server(rs: &Arc<RemoteState>, be: Arc<dyn Backend>) -> Result<u16, String> {
    let mut run = rs.running.lock().await;
    if let Some(p) = rs.port() {
        if run.is_some() {
            return Ok(p);
        }
    }
    let l = bind_any(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).await.ok_or("INTERNAL: no free port in 47900–47909")?;
    let port = l.local_addr().map_err(internal)?.port();
    let ctx = Arc::new(Ctx { rs: rs.clone(), be });
    let task = tokio::spawn(serve(l, ctx));
    let advert = advertise(&rs.name, port, &rs.self_id());
    rs.port.store(port, Ordering::SeqCst);
    *run = Some(Running { task, advert });
    tracing::info!(port, "remote play server listening");
    Ok(port)
}

/// Serve on an already bound listener (tests: in-process servers on loopback).
#[cfg(test)]
pub(crate) fn serve_on(listener: TcpListener, rs: Arc<RemoteState>, be: Arc<dyn Backend>) {
    if let Ok(a) = listener.local_addr() {
        rs.port.store(a.port(), Ordering::SeqCst);
    }
    tokio::spawn(serve(listener, Arc::new(Ctx { rs, be })));
}

async fn stop_server(rs: &RemoteState) {
    if let Some(mut r) = rs.running.lock().await.take() {
        r.task.abort();
        if let Some(c) = r.advert.as_mut() {
            let _ = c.start_kill();
        }
    }
    rs.port.store(0, Ordering::SeqCst);
}

/// Called from `setup`: starts the server if Remote play is enabled.
pub fn spawn(app: AppHandle, st: Arc<AppState>) {
    let rs: Arc<RemoteState> = app.state::<Arc<RemoteState>>().inner().clone();
    if !rs.cfg.lock().unwrap().enabled {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let be = Arc::new(AppBackend { app, st, rs: rs.clone() });
        if let Err(e) = start_server(&rs, be).await {
            tracing::warn!("remote play server: {e}");
        }
    });
}

// ------------------------------------------------------------------ app backend

struct AppBackend {
    app: AppHandle,
    st: Arc<AppState>,
    rs: Arc<RemoteState>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Builtin,
    Vlc,
}

/// Builtin for mp4/m4v/mov/webm (+ common audio), else VLC; `hint` "vlc" forces VLC
/// when installed.
fn choose_mode(url: &str, hint: Option<&str>, vlc: bool) -> Result<Mode, String> {
    let last = url.rsplit('/').next().unwrap_or("");
    let name = urlencoding::decode(last).map(|c| c.into_owned()).unwrap_or_default();
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let builtin_ok = matches!(ext.as_str(), "mp4" | "m4v" | "mov" | "webm" | "mp3" | "m4a" | "aac" | "wav");
    match (builtin_ok, vlc, hint) {
        (_, true, Some("vlc")) => Ok(Mode::Vlc),
        (true, _, _) => Ok(Mode::Builtin),
        (false, true, _) => Ok(Mode::Vlc),
        (false, false, _) => Err("VLC_NOT_FOUND: this file needs VLC on the receiving Mac".into()),
    }
}

impl AppBackend {
    async fn fetch_text(&self, url: &str) -> Option<String> {
        let mut r = self.rs.http.get(url).timeout(Duration::from_secs(10)).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        let mut b = Vec::new();
        while let Ok(Some(c)) = r.chunk().await {
            b.extend_from_slice(&c);
            if b.len() > SUB_MAX_BYTES {
                return None;
            }
        }
        Some(String::from_utf8_lossy(&b).into_owned())
    }

    async fn play_vlc(&self, req: PlayRequest) -> Result<(), String> {
        let vlc = crate::player::find_vlc().ok_or("VLC_NOT_FOUND: VLC is not installed")?;
        let mut sub_path = None;
        if let Some(s) = req.subtitles.first() {
            if let Some(text) = self.fetch_text(&s.url).await {
                let dir = self.st.data_dir.join("subs");
                let p = dir.join("remote.vtt");
                if tokio::fs::create_dir_all(&dir).await.is_ok() && tokio::fs::write(&p, text).await.is_ok() {
                    sub_path = Some(p);
                }
            }
        }
        let mut cmd = tokio::process::Command::new(vlc);
        cmd.arg("--play-and-exit").arg("--quiet").arg("--fullscreen").arg(format!("--meta-title={}", req.title));
        if let Some(p) = &sub_path {
            cmd.arg(format!("--sub-file={}", p.display()));
        }
        cmd.arg(&req.url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(false);
        cmd.process_group(0);
        let mut child = cmd.spawn().map_err(|e| format!("INTERNAL: failed to start VLC: {e}"))?;
        tauri::async_runtime::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(())
    }

    /// Load `req` (a LAN URL) into the builtin player through the loopback relay.
    /// `media` is what resume/watched are keyed by.
    async fn play_builtin(&self, req: PlayRequest, media: MediaRef, fullscreen: bool) -> Result<PlayResult, String> {
        let port = self.rs.port().ok_or("REMOTE_DISABLED: remote play is off")?;
        let last = req.url.rsplit('/').next().unwrap_or("media").to_string();
        let rid = self.rs.add_relay(req.url.clone());
        let stream_url = format!("http://127.0.0.1:{port}/v1/relay/{rid}/{last}");
        let mut subtitles = Vec::new();
        for (i, s) in req.subtitles.iter().enumerate().take(8) {
            let Some(text) = self.fetch_text(&s.url).await else { continue };
            let id = format!("r:{rid}:{i}");
            self.st.player.vtt_cache.lock().map_err(internal)?.insert(id.clone(), Arc::new(text));
            subtitles.push(SubtitleTrack { id, label: s.label.clone(), lang: s.lang.clone(), format: SubtitleFormat::Vtt });
        }
        let result = PlayResult {
            mode: PlayMode::Builtin,
            title: req.title.clone(),
            media,
            stream_url,
            default_subtitle_id: subtitles.first().map(|t| t.id.clone()),
            subtitles,
        };
        *self.st.player.current.lock().map_err(internal)? = Some(result.clone());
        let app = &self.app;
        if let Some(w) = app.get_webview_window("player") {
            let _ = w.set_title(&result.title);
            let _ = w.unminimize();
            let _ = w.show();
            if fullscreen {
                let _ = w.set_fullscreen(true);
            }
            let _ = w.set_focus();
            app.emit_to("player", "player-load", &result).map_err(internal)?;
        } else {
            let w = WebviewWindowBuilder::new(app, "player", WebviewUrl::App("player.html".into()))
                .title(&result.title)
                .inner_size(960., 540.)
                .min_inner_size(320., 180.)
                .background_color(tauri::window::Color(0, 0, 0, 255))
                .theme(Some(tauri::Theme::Dark))
                .fullscreen(fullscreen)
                .build()
                .map_err(internal)?;
            let _ = w.set_focus();
        }
        Ok(result)
    }
}

/// Title + subtitle files (default first) of a media file of this Mac.
fn stream_info_for(st: &AppState, key: &str, idx: usize) -> Result<StreamInfo, String> {
    let files = engine::files(st, key)?;
    let row = files.iter().find(|f| f.index == idx).cloned().ok_or("NOT_FOUND: no such file")?;
    if !matches!(row.kind, FileKind::Video | FileKind::Audio) {
        return Err("INVALID_INPUT: not a media file".into());
    }
    let (tracks, default_id) = if row.kind == FileKind::Video {
        let videos: Vec<&str> = files.iter().filter(|f| f.kind == FileKind::Video).map(|f| f.name.as_str()).collect();
        let subs: Vec<(usize, &str)> =
            files.iter().filter(|f| f.kind == FileKind::Subtitle).map(|f| (f.index, f.name.as_str())).collect();
        crate::subtitles::build_tracks(key, &row.name, &videos, &subs)
    } else {
        (Vec::new(), None)
    };
    let mut subs: Vec<(usize, &SubtitleTrack)> = tracks
        .iter()
        .filter_map(|t| crate::subtitles::parse_track_id(&t.id).map(|(_, i)| (i, t)))
        .collect();
    if let Some(d) = &default_id {
        if let Some(pos) = subs.iter().position(|(_, t)| &t.id == d) {
            let s = subs.remove(pos);
            subs.insert(0, s);
        }
    }
    Ok(StreamInfo {
        title: row.name.rsplit('/').next().unwrap_or(&row.name).to_string(),
        subs: subs.into_iter().map(|(i, t)| (i, t.label.clone(), t.lang.clone())).collect(),
        playable_builtin: row.playable_builtin,
    })
}

/// Decode a `data:<mime>;base64,<…>` URL.
fn decode_data_url(url: &str) -> Option<(Vec<u8>, String)> {
    let rest = url.strip_prefix("data:")?;
    let (mime, b64) = rest.split_once(";base64,")?;
    Some((base64_decode(b64)?, mime.to_string()))
}

pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

impl Backend for AppBackend {
    fn open(&self, key: String, idx: usize) -> BoxFut<'_, Result<Opened, String>> {
        Box::pin(async move {
            let files = engine::files(&self.st, &key)?;
            let row = files.into_iter().find(|f| f.index == idx).ok_or("NOT_FOUND: no such file")?;
            if !matches!(row.kind, FileKind::Video | FileKind::Audio) {
                return Err("NOT_FOUND: not a media file".into());
            }
            let name = row.name.rsplit('/').next().unwrap_or(&row.name).to_string();
            let disk = || async {
                let p = engine::file_abs_path(&self.st, &key, idx).ok()?;
                tokio::fs::File::open(p).await.ok()
            };
            if row.progress >= 1.0 {
                if let Some(f) = disk().await {
                    return Ok(Opened { reader: Box::new(f), size: row.size, name });
                }
            }
            if let Err(e) = engine::ensure_live(&self.st, &key).await {
                // Finished meanwhile: the file is on disk.
                if let Some(f) = disk().await {
                    return Ok(Opened { reader: Box::new(f), size: row.size, name });
                }
                return Err(e);
            }
            let id = key.parse::<librqbit::dht::Id20>().map_err(|_| "INVALID_INPUT: bad key".to_string())?;
            let s = self
                .st
                .api
                .api_stream(librqbit::api::TorrentIdOrHash::Hash(id), idx)
                .await
                .map_err(internal)?;
            Ok(Opened { reader: Box::new(s), size: row.size, name })
        })
    }

    fn sub_vtt(&self, key: String, idx: usize) -> BoxFut<'_, Result<String, String>> {
        Box::pin(async move {
            let id = crate::subtitles::track_id(&key, idx);
            crate::player::get_subtitle_vtt(self.app.state::<Arc<AppState>>(), id).await
        })
    }

    fn play(&self, req: PlayRequest) -> BoxFut<'_, Result<(), String>> {
        Box::pin(async move {
            match choose_mode(&req.url, req.mode_hint.as_deref(), crate::player::find_vlc().is_some())? {
                Mode::Vlc => self.play_vlc(req).await,
                // Not a local torrent: the player skips resume/watched/next-episode.
                Mode::Builtin => self.play_builtin(req, MediaRef::Yt { id: "remote".into() }, true).await.map(|_| ()),
            }
        })
    }

    fn emit(&self, event: &str, payload: serde_json::Value) {
        if event == "remote-pair-request" {
            if let Some(w) = self.app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
        if let Err(e) = self.app.emit(event, payload) {
            tracing::debug!("emit {event}: {e}");
        }
    }

    fn library(&self) -> BoxFut<'_, Result<Library, String>> {
        Box::pin(async move {
            let mut lib = Library::default();
            for r in engine::list_rows(&self.st) {
                if r.state == crate::types::TorrentState::Resolving {
                    continue;
                }
                if let Ok(files) = engine::files(&self.st, &r.key) {
                    let media: Vec<_> = files.into_iter().filter(|f| matches!(f.kind, FileKind::Video | FileKind::Audio)).collect();
                    lib.files.insert(r.key.clone(), media);
                }
                if let Some(info) = crate::metadata::offline_info(&self.st, &r.key) {
                    lib.media.insert(r.key.clone(), info);
                }
                lib.rows.push(r);
            }
            Ok(lib)
        })
    }

    fn art(&self, key: String, kind: String) -> BoxFut<'_, Result<(Vec<u8>, String), String>> {
        Box::pin(async move {
            let url = crate::metadata::get_media_image(self.app.state::<Arc<AppState>>(), key, kind).await?;
            decode_data_url(&url).ok_or_else(|| "NOT_FOUND: bad image".to_string())
        })
    }

    fn stream_info(&self, key: String, idx: usize) -> BoxFut<'_, Result<StreamInfo, String>> {
        Box::pin(async move { stream_info_for(&self.st, &key, idx) })
    }

    fn sync_state(&self, from: &str, since: u64) -> Result<serde_json::Value, String> {
        let hub = self.app.state::<Arc<crate::sync::SyncHub>>();
        hub.serve_state(from, since)
    }

    fn sync_push(&self, from: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let hub = self.app.state::<Arc<crate::sync::SyncHub>>();
        hub.serve_push(from, body)
    }

    fn archive(&self) -> Option<Arc<crate::archive::Receiver>> {
        self.app.try_state::<Arc<crate::archive::Receiver>>().map(|s| s.inner().clone())
    }
}

// ------------------------------------------------------------------ commands

type Rs<'a> = State<'a, Arc<RemoteState>>;
type St<'a> = State<'a, Arc<AppState>>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub online: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub enabled: bool,
    pub port: Option<u16>,
    pub name: String,
    /// This Mac's LAN IPv4 (for manual pairing when Bonjour is blocked).
    pub lan_ip: Option<String>,
    pub devices: Vec<DeviceInfo>,
}

async fn status_of(rs: &RemoteState) -> RemoteStatus {
    let (enabled, devices) = {
        let c = rs.cfg.lock().unwrap();
        (c.enabled, c.devices.clone())
    };
    let mut set = tokio::task::JoinSet::new();
    for (i, d) in devices.iter().cloned().enumerate() {
        set.spawn(async move {
            for h in std::iter::once(d.host.clone()).chain(d.last_ip.clone()) {
                if let Some(a) = resolve(&h, d.port).await {
                    if tcp_ok(a, 600).await {
                        return (i, true);
                    }
                }
            }
            (i, false)
        });
    }
    let mut online = vec![false; devices.len()];
    while let Some(Ok((i, ok))) = set.join_next().await {
        online[i] = ok;
    }
    RemoteStatus {
        enabled,
        port: rs.port(),
        name: rs.name.clone(),
        lan_ip: lan_ipv4().map(|v| v.to_string()),
        devices: devices
            .into_iter()
            .zip(online)
            .map(|(d, online)| DeviceInfo { id: d.id, name: d.name, host: d.host, port: d.port, online })
            .collect(),
    }
}

#[tauri::command]
pub async fn remote_status(rs: Rs<'_>) -> Result<RemoteStatus, String> {
    Ok(status_of(&rs).await)
}

/// JS: `{ enabled }`.
#[tauri::command]
pub async fn remote_set_enabled(app: AppHandle, rs: Rs<'_>, st: St<'_>, enabled: bool) -> Result<RemoteStatus, String> {
    let rs: Arc<RemoteState> = rs.inner().clone();
    rs.cfg.lock().unwrap().enabled = enabled;
    rs.save();
    if enabled {
        let be = Arc::new(AppBackend { app, st: st.inner().clone(), rs: rs.clone() });
        start_server(&rs, be).await?;
    } else {
        stop_server(&rs).await;
    }
    Ok(status_of(&rs).await)
}

#[tauri::command]
pub async fn remote_discover(app: AppHandle, rs: Rs<'_>) -> Result<Vec<Found>, String> {
    let found = discover(&rs).await;
    let mut changed = false;
    for f in &found {
        if let Some(id) = f.id.as_deref().filter(|_| f.paired) {
            changed |= rs.touch_device(id, Some(&f.host), Some(f.port), None);
        }
    }
    if changed {
        let _ = app.emit("remote-devices-updated", ());
    }
    Ok(found)
}

async fn addr_of(host: &str, port: u16) -> Result<SocketAddr, String> {
    let host = host.trim();
    if host.is_empty() || host.len() > 255 || port == 0 {
        return Err("INVALID_INPUT: bad host or port".into());
    }
    resolve(host, port).await.ok_or_else(|| format!("UNREACHABLE: cannot resolve {host}"))
}

/// JS: `{ host, port }`. Asks that Mac to show a 6-digit code; returns its `{id, name}`.
#[tauri::command]
pub async fn remote_pair_start(rs: Rs<'_>, host: String, port: u16) -> Result<PeerInfo, String> {
    let addr = addr_of(&host, port).await?;
    client_pair_start(&rs, addr).await
}

/// JS: `{ host, port }` typed by the user (IP:port shown in the other Mac's settings).
/// Same as `remote_pair_start`; finish with `remote_pair_finish`.
#[tauri::command]
pub async fn remote_add_manual(rs: Rs<'_>, host: String, port: Option<u16>) -> Result<PeerInfo, String> {
    let port = port.unwrap_or(*PORTS.start());
    let addr = addr_of(&host, port).await?;
    client_pair_start(&rs, addr).await
}

/// JS: `{ host, port, code }`.
#[tauri::command]
pub async fn remote_pair_finish(app: AppHandle, rs: Rs<'_>, host: String, port: u16, code: String) -> Result<DeviceInfo, String> {
    let addr = addr_of(&host, port).await?;
    let d = client_pair_finish(&rs, host.trim(), addr, &code).await?;
    let _ = app.emit("remote-devices-updated", ());
    Ok(DeviceInfo { id: d.id, name: d.name, host: d.host, port: d.port, online: true })
}

/// JS: `{ id }`.
#[tauri::command]
pub async fn remote_unpair(app: AppHandle, rs: Rs<'_>, id: String) -> Result<(), String> {
    rs.cfg.lock().unwrap().devices.retain(|d| d.id != id);
    rs.save();
    let _ = app.emit("remote-devices-updated", ());
    Ok(())
}

/// JS: `{ deviceId, media }`. Plays a torrent file of THIS Mac on the paired device.
#[tauri::command]
pub async fn remote_play(rs: Rs<'_>, st: St<'_>, device_id: String, media: MediaRef) -> Result<(), String> {
    let rs: Arc<RemoteState> = rs.inner().clone();
    let st: Arc<AppState> = st.inner().clone();
    let MediaRef::Torrent { key, file_index } = media else {
        return Err("INVALID_INPUT: only torrent files can be played remotely".into());
    };
    let key = key.to_ascii_lowercase();
    if !is_hex(&key, 40) {
        return Err("INVALID_INPUT: bad torrent key".into());
    }
    let my_port = rs.port().ok_or("REMOTE_DISABLED: turn on Remote play")?;
    let dev = rs.device(&device_id).ok_or("NOT_FOUND: device is not paired")?;
    let target = reach(&rs, &dev).await?;
    let ip = local_ip_for(target).ok_or("UNREACHABLE: no route to device")?;
    let info = stream_info_for(&st, &key, file_index)?;
    let base = format!("http://{}:{my_port}", url_host(ip));
    let req = build_request(&rs, &base, &key, file_index, &info);
    let body = serde_json::to_value(&req).map_err(internal)?;
    post_json(&rs, target, "/v1/play", Some((&rs.self_id(), &dev.secret)), body, Duration::from_secs(60)).await?;
    Ok(())
}

// ------------------------------------------------------------------ library of other Macs

async fn fetch_library(rs: &RemoteState, dev: &Device) -> Result<LibrarySnapshot, String> {
    let addr = quick_addr(dev, 800).await.ok_or_else(|| format!("UNREACHABLE: {} is offline", dev.name))?;
    let v = get_json(rs, addr, "/v1/library", (&rs.self_id(), &dev.secret), Duration::from_secs(20)).await?;
    let name = v.get("device_name").and_then(|x| x.as_str()).map(clean_name).unwrap_or_else(|| dev.name.clone());
    let lib: Library = serde_json::from_value(v).map_err(|e| format!("REMOTE_ERROR: bad library: {e}"))?;
    Ok(LibrarySnapshot { name, lib })
}

/// Libraries of all paired devices: fresh from online ones, the cached snapshot (online
/// false) from the others.
pub(crate) async fn libraries(rs: &Arc<RemoteState>) -> Vec<RemoteLibrary> {
    let devices = rs.devices();
    let mut set = tokio::task::JoinSet::new();
    for (i, d) in devices.iter().cloned().enumerate() {
        let rs = rs.clone();
        set.spawn(async move { (i, fetch_library(&rs, &d).await) });
    }
    let mut fresh: Vec<Option<Result<LibrarySnapshot, String>>> = vec![None; devices.len()];
    while let Some(Ok((i, r))) = set.join_next().await {
        fresh[i] = Some(r);
    }
    let mut cache = rs.library.lock().unwrap();
    cache.retain(|id, _| devices.iter().any(|d| &d.id == id));
    devices
        .iter()
        .zip(fresh)
        .map(|(d, r)| {
            let online = match r {
                Some(Ok(snap)) => {
                    cache.insert(d.id.clone(), snap);
                    true
                }
                Some(Err(e)) => {
                    tracing::debug!("library of {}: {e}", d.name);
                    false
                }
                None => false,
            };
            let snap = cache.get(&d.id).cloned().unwrap_or(LibrarySnapshot { name: d.name.clone(), lib: Library::default() });
            RemoteLibrary {
                device_id: d.id.clone(),
                device_name: snap.name,
                online,
                rows: snap.lib.rows,
                media: snap.lib.media,
                files: snap.lib.files,
            }
        })
        .collect()
}

/// Ask `dev` for a tokenized stream of one of its files.
pub(crate) async fn stream_token(rs: &RemoteState, dev: &Device, addr: SocketAddr, key: &str, idx: usize) -> Result<PlayRequest, String> {
    let body = json!({ "key": key, "fileIndex": idx });
    let v = post_json(rs, addr, "/v1/stream-token", Some((&rs.self_id(), &dev.secret)), body, Duration::from_secs(20)).await?;
    let pr: PlayRequest = serde_json::from_value(v).map_err(|e| format!("REMOTE_ERROR: bad answer: {e}"))?;
    // The URLs must point back at the device we asked.
    if !url_from_peer(&pr.url, addr.ip().to_canonical()) || !pr.subtitles.iter().all(|s| url_from_peer(&s.url, addr.ip().to_canonical())) {
        return Err("REMOTE_ERROR: stream URL does not point at the device".into());
    }
    Ok(pr)
}

/// `remote_library` → `[{device_id, device_name, online, rows, media, files}]`.
#[tauri::command]
pub async fn remote_library(rs: Rs<'_>) -> Result<Vec<RemoteLibrary>, String> {
    Ok(libraries(rs.inner()).await)
}

/// JS: `{ deviceId, key, fileIndex }`. Plays a file that lives on the paired device in
/// THIS Mac's player (through the loopback relay) or VLC. Resume/watched stay keyed by
/// `(key, fileIndex)`.
#[tauri::command]
pub async fn remote_play_local(
    app: AppHandle,
    rs: Rs<'_>,
    st: St<'_>,
    device_id: String,
    key: String,
    file_index: usize,
) -> Result<PlayResult, String> {
    let rs: Arc<RemoteState> = rs.inner().clone();
    let key = key.to_ascii_lowercase();
    if !is_hex(&key, 40) {
        return Err("INVALID_INPUT: bad torrent key".into());
    }
    let dev = rs.device(&device_id).ok_or("NOT_FOUND: device is not paired")?;
    let addr = reach(&rs, &dev).await?;
    let pr = stream_token(&rs, &dev, addr, &key, file_index).await?;
    let media = MediaRef::Torrent { key: key.clone(), file_index };
    let be = AppBackend { app, st: st.inner().clone(), rs: rs.clone() };
    match choose_mode(&pr.url, pr.mode_hint.as_deref(), crate::player::find_vlc().is_some())? {
        Mode::Builtin => {
            if rs.port().is_none() {
                return Err("REMOTE_DISABLED: turn on Remote play (needed to relay the stream)".into());
            }
            be.play_builtin(pr, media, false).await
        }
        Mode::Vlc => {
            let result = PlayResult {
                mode: PlayMode::Vlc,
                title: pr.title.clone(),
                media,
                stream_url: pr.url.clone(),
                subtitles: Vec::new(),
                default_subtitle_id: None,
            };
            be.play_vlc(pr).await?;
            Ok(result)
        }
    }
}

/// JS: `{ deviceId, key, kind }` ("poster" | "backdrop") → data URL.
#[tauri::command]
pub async fn remote_art(rs: Rs<'_>, device_id: String, key: String, kind: String) -> Result<String, String> {
    let key = key.to_ascii_lowercase();
    if !is_hex(&key, 40) || !matches!(kind.as_str(), "poster" | "backdrop") {
        return Err("INVALID_INPUT: bad key or kind".into());
    }
    let ck = (device_id.clone(), key.clone(), kind.clone());
    if let Some(hit) = rs.art.lock().unwrap().iter().find(|(k, _)| *k == ck).map(|(_, v)| v.clone()) {
        return Ok(hit);
    }
    let dev = rs.device(&device_id).ok_or("NOT_FOUND: device is not paired")?;
    let addr = quick_addr(&dev, 800).await.ok_or_else(|| format!("UNREACHABLE: {} is offline", dev.name))?;
    let (ctype, bytes) =
        get_raw(&rs, addr, &format!("/v1/art/{key}/{kind}"), (&rs.self_id(), &dev.secret), Duration::from_secs(15)).await?;
    let mime = ctype.split(';').next().unwrap_or("").trim();
    if !mime.starts_with("image/") {
        return Err("NOT_FOUND: not an image".into());
    }
    let url = format!("data:{mime};base64,{}", crate::artwork::base64(&bytes));
    let mut c = rs.art.lock().unwrap();
    c.push((ck, url.clone()));
    if c.len() > ART_CACHE {
        c.remove(0);
    }
    Ok(url)
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock {
        file: PathBuf,
        played: Mutex<Vec<PlayRequest>>,
        events: Mutex<Vec<(String, serde_json::Value)>>,
        opened: Mutex<Vec<(String, usize)>>,
    }

    impl Backend for Mock {
        fn open(&self, key: String, idx: usize) -> BoxFut<'_, Result<Opened, String>> {
            Box::pin(async move {
                self.opened.lock().unwrap().push((key, idx));
                let f = tokio::fs::File::open(&self.file).await.map_err(internal)?;
                let size = f.metadata().await.map_err(internal)?.len();
                Ok(Opened { reader: Box::new(f), size, name: "movie.mp4".into() })
            })
        }
        fn sub_vtt(&self, _key: String, idx: usize) -> BoxFut<'_, Result<String, String>> {
            Box::pin(async move { Ok(format!("WEBVTT\n\n{idx}")) })
        }
        fn play(&self, req: PlayRequest) -> BoxFut<'_, Result<(), String>> {
            Box::pin(async move {
                self.played.lock().unwrap().push(req);
                Ok(())
            })
        }
        fn emit(&self, event: &str, payload: serde_json::Value) {
            self.events.lock().unwrap().push((event.to_string(), payload));
        }
        fn library(&self) -> BoxFut<'_, Result<Library, String>> {
            Box::pin(async move {
                let row: crate::types::TorrentRow = serde_json::from_value(json!({
                    "key": KEY, "id": 0, "name": "Movie", "state": "completed", "error": null,
                    "total_bytes": 1000, "progress_bytes": 1000, "progress": 1.0, "uploaded_bytes": 0,
                    "down_bps": 0.0, "up_bps": 0.0, "peers": 0, "eta_secs": null, "output_path": "/x"
                }))
                .unwrap();
                let mut lib = Library { rows: vec![row], ..Default::default() };
                lib.media.insert(KEY.into(), crate::types::MediaInfo { title: "Movie".into(), ..Default::default() });
                Ok(lib)
            })
        }
        fn art(&self, _key: String, kind: String) -> BoxFut<'_, Result<(Vec<u8>, String), String>> {
            Box::pin(async move { Ok((kind.into_bytes(), "image/jpeg".to_string())) })
        }
        fn stream_info(&self, key: String, idx: usize) -> BoxFut<'_, Result<StreamInfo, String>> {
            Box::pin(async move {
                if key != KEY || idx != 3 {
                    return Err("NOT_FOUND: no such file".into());
                }
                Ok(StreamInfo { title: "Movie One.mp4".into(), subs: vec![(5, "English".into(), Some("en".into()))], playable_builtin: true })
            })
        }
    }

    const KEY: &str = "0123456789abcdef0123456789abcdef01234567";

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tide-remote-{tag}-{}", random_hex(4)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Server on 127.0.0.1 with a mock backend. Returns (state, mock, addr).
    async fn server(tag: &str) -> (Arc<RemoteState>, Arc<Mock>, SocketAddr) {
        let dir = tmpdir(tag);
        let file = dir.join("movie.mp4");
        std::fs::write(&file, (0..1000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>()).unwrap();
        let rs = Arc::new(RemoteState::load(&dir, format!("Recv {tag}")));
        let mock = Arc::new(Mock {
            file,
            played: Mutex::new(vec![]),
            events: Mutex::new(vec![]),
            opened: Mutex::new(vec![]),
        });
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        rs.port.store(addr.port(), Ordering::SeqCst);
        tokio::spawn(serve(l, Arc::new(Ctx { rs: rs.clone(), be: mock.clone() })));
        (rs, mock, addr)
    }

    /// Raw request → (status, headers, body).
    async fn raw(addr: SocketAddr, req: String) -> (u16, String, Vec<u8>) {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let p = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&out[..p]).into_owned();
        let code = head.split(' ').nth(1).unwrap().parse().unwrap();
        (code, head, out[p + 4..].to_vec())
    }

    async fn get(addr: SocketAddr, path: &str, range: Option<&str>) -> (u16, String, Vec<u8>) {
        let r = range.map(|r| format!("Range: {r}\r\n")).unwrap_or_default();
        raw(addr, format!("GET {path} HTTP/1.1\r\nHost: x\r\n{r}\r\n")).await
    }

    async fn post(addr: SocketAddr, path: &str, extra: &str, body: &str) -> (u16, String, Vec<u8>) {
        raw(
            addr,
            format!("POST {path} HTTP/1.1\r\nHost: x\r\n{extra}Content-Length: {}\r\n\r\n{body}", body.len()),
        )
        .await
    }

    fn pair_by_hand(rs: &RemoteState, id: &str, secret: &str) {
        rs.upsert_device(Device {
            id: id.into(),
            name: "Sender".into(),
            host: "127.0.0.1".into(),
            port: 47900,
            secret: secret.into(),
            last_ip: None,
        });
    }

    #[tokio::test]
    async fn pairing_handshake() {
        let (recv, mock, addr) = server("pair").await;
        let sender = RemoteState::load(&tmpdir("pair-s"), "Sender".into());

        let info = client_pair_start(&sender, addr).await.unwrap();
        assert_eq!(info.id, recv.self_id());
        let code = {
            let ev = mock.events.lock().unwrap();
            assert_eq!(ev[0].0, "remote-pair-request");
            assert_eq!(ev[0].1["from"], "Sender");
            ev[0].1["code"].as_str().unwrap().to_string()
        };
        assert_eq!(code.len(), 6);

        let wrong = if code == "000000" { "111111" } else { "000000" };
        let e = client_pair_finish(&sender, "127.0.0.1", addr, wrong).await.unwrap_err();
        assert!(e.starts_with("UNAUTHORIZED"), "{e}");

        let dev = client_pair_finish(&sender, "127.0.0.1", addr, &code).await.unwrap();
        assert_eq!(dev.id, recv.self_id());
        let back = recv.device(&sender.self_id()).expect("receiver stored the sender");
        assert_eq!(back.secret, dev.secret);
        assert_eq!(back.host, "127.0.0.1");
        // Code is single-use.
        assert!(client_pair_finish(&sender, "127.0.0.1", addr, &code).await.is_err());
        // Persisted with the secret, reloadable.
        let again = RemoteState::load(&recv.path.parent().unwrap().to_path_buf(), "x".into());
        assert_eq!(again.self_id(), recv.self_id());
        assert_eq!(again.device(&sender.self_id()).unwrap().secret, dev.secret);
    }

    #[tokio::test]
    async fn pairing_locks_after_wrong_codes() {
        let (_recv, mock, addr) = server("lock").await;
        let sender = RemoteState::load(&tmpdir("lock-s"), "S".into());
        client_pair_start(&sender, addr).await.unwrap();
        let code = mock.events.lock().unwrap()[0].1["code"].as_str().unwrap().to_string();
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for _ in 0..PAIR_MAX_ATTEMPTS {
            let _ = client_pair_finish(&sender, "127.0.0.1", addr, wrong).await;
        }
        assert!(client_pair_finish(&sender, "127.0.0.1", addr, &code).await.is_err());
    }

    #[tokio::test]
    async fn auth_rejection() {
        let (rs, mock, addr) = server("auth").await;
        let id = random_hex(16);
        pair_by_hand(&rs, &id, &"a".repeat(64));
        let body = r#"{"title":"t","url":"http://127.0.0.1:1/v1/stream/x/y/0/a.mp4","subtitles":[]}"#;
        assert_eq!(post(addr, "/v1/play", "", body).await.0, 401);
        let bad = format!("X-Tide-Id: {id}\r\nAuthorization: Bearer {}\r\n", "b".repeat(64));
        assert_eq!(post(addr, "/v1/play", &bad, body).await.0, 401);
        let unknown = format!("X-Tide-Id: {}\r\nAuthorization: Bearer {}\r\n", random_hex(16), "a".repeat(64));
        assert_eq!(post(addr, "/v1/play", &unknown, body).await.0, 401);
        assert_eq!(get(addr, &format!("/v1/stream/nope/{KEY}/0/a.mp4"), None).await.0, 401);
        assert_eq!(get(addr, &format!("/v1/sub/nope/{KEY}/1.vtt"), None).await.0, 401);
        // Relay only for loopback peers with a known id.
        assert_eq!(get(addr, "/v1/relay/nope/a.mp4", None).await.0, 401);
        assert!(mock.played.lock().unwrap().is_empty());
        assert!(mock.opened.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn range_serving() {
        let (rs, _mock, addr) = server("range").await;
        let tok = rs.grant(KEY, 3, []);
        let p = format!("/v1/stream/{tok}/{KEY}/3/movie.mp4");
        let all: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();

        let (c, h, b) = get(addr, &p, None).await;
        assert_eq!(c, 200);
        assert!(h.contains("Content-Length: 1000") && h.contains("Accept-Ranges: bytes") && h.contains("video/mp4"));
        assert_eq!(b, all);

        let (c, h, b) = get(addr, &p, Some("bytes=10-19")).await;
        assert_eq!(c, 206);
        assert!(h.contains("Content-Range: bytes 10-19/1000"), "{h}");
        assert_eq!(b, all[10..20]);

        let (c, _, b) = get(addr, &p, Some("bytes=990-")).await;
        assert_eq!((c, b.as_slice()), (206, &all[990..]));
        let (c, _, b) = get(addr, &p, Some("bytes=-5")).await;
        assert_eq!((c, b.as_slice()), (206, &all[995..]));
        let (c, h, _) = get(addr, &p, Some("bytes=2000-")).await;
        assert_eq!(c, 416);
        assert!(h.contains("bytes */1000"));

        let (c, h, b) = raw(addr, format!("HEAD {p} HTTP/1.1\r\nRange: bytes=0-1\r\n\r\n")).await;
        assert_eq!(c, 206);
        assert!(h.contains("Content-Length: 2"));
        assert!(b.is_empty());
    }

    #[tokio::test]
    async fn path_restriction() {
        let (rs, mock, addr) = server("path").await;
        let tok = rs.grant(KEY, 3, [5]);
        let other = "f".repeat(40);
        assert_eq!(get(addr, &format!("/v1/stream/{tok}/{KEY}/4/a.mp4"), None).await.0, 401);
        assert_eq!(get(addr, &format!("/v1/stream/{tok}/{other}/3/a.mp4"), None).await.0, 401);
        assert_eq!(get(addr, &format!("/v1/stream/{tok}/../../etc/passwd"), None).await.0, 404);
        assert_eq!(get(addr, &format!("/v1/stream/{tok}/{KEY}/3/../../x"), None).await.0, 404);
        assert_eq!(get(addr, &format!("/v1/sub/{tok}/{KEY}/3.vtt"), None).await.0, 401);
        assert_eq!(get(addr, "/etc/passwd", None).await.0, 404);
        assert!(mock.opened.lock().unwrap().is_empty());
        // The granted ones work.
        let (c, _, b) = get(addr, &format!("/v1/sub/{tok}/{KEY}/5.vtt"), None).await;
        assert_eq!((c, b), (200, b"WEBVTT\n\n5".to_vec()));
        assert_eq!(get(addr, &format!("/v1/stream/{tok}/{KEY}/3/a.mp4"), Some("bytes=0-0")).await.0, 206);
        assert_eq!(mock.opened.lock().unwrap().as_slice(), &[(KEY.to_string(), 3)]);
    }

    #[tokio::test]
    async fn play_routing() {
        let (recv, mock, addr) = server("play").await;
        let sender = RemoteState::load(&tmpdir("play-s"), "S".into());
        let secret = "c".repeat(64);
        pair_by_hand(&recv, &sender.self_id(), &secret);

        let req = PlayRequest {
            title: "Movie".into(),
            url: format!("http://127.0.0.1:47901/v1/stream/t/{KEY}/0/Movie.mkv"),
            subtitles: vec![RemoteSub {
                label: "English".into(),
                lang: Some("en".into()),
                url: format!("http://127.0.0.1:47901/v1/sub/t/{KEY}/1.vtt"),
            }],
            mode_hint: Some("vlc".into()),
        };
        let auth = Some((sender.self_id(), secret.clone()));
        let a = auth.as_ref().map(|(i, s)| (i.as_str(), s.as_str()));
        post_json(&sender, addr, "/v1/play", a, serde_json::to_value(&req).unwrap(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(mock.played.lock().unwrap().as_slice(), &[req.clone()]);

        // URLs pointing anywhere but the sender are refused.
        let mut evil = req.clone();
        evil.url = format!("http://10.1.2.3:47901/v1/stream/t/{KEY}/0/Movie.mkv");
        let e = post_json(&sender, addr, "/v1/play", a, serde_json::to_value(&evil).unwrap(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(e.starts_with("REMOTE_ERROR"), "{e}");
        let mut evil2 = req.clone();
        evil2.subtitles[0].url = "http://127.0.0.1:47901/etc/passwd".into();
        assert!(post_json(&sender, addr, "/v1/play", a, serde_json::to_value(&evil2).unwrap(), Duration::from_secs(5))
            .await
            .is_err());
        assert_eq!(mock.played.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn library_endpoint_auth() {
        let (recv, _mock, addr) = server("lib").await;
        let sender = RemoteState::load(&tmpdir("lib-s"), "S".into());
        let secret = "d".repeat(64);
        pair_by_hand(&recv, &sender.self_id(), &secret);
        let me = sender.self_id();
        assert_eq!(get(addr, "/v1/library", None).await.0, 401);
        assert_eq!(get(addr, &format!("/v1/art/{KEY}/poster"), None).await.0, 401);
        let e = get_json(&sender, addr, "/v1/library", (&me, &"e".repeat(64)), Duration::from_secs(5)).await.unwrap_err();
        assert!(e.starts_with("UNAUTHORIZED"), "{e}");
        let e = get_json(&sender, addr, "/v1/sync/state?since=0", (&me, &secret), Duration::from_secs(5)).await.unwrap_err();
        assert!(e.starts_with("REMOTE_ERROR"), "mock has no sync: {e}");

        let v = get_json(&sender, addr, "/v1/library", (&me, &secret), Duration::from_secs(5)).await.unwrap();
        assert_eq!(v["device_id"], json!(recv.self_id()));
        let lib: Library = serde_json::from_value(v).unwrap();
        assert_eq!(lib.rows[0].key, KEY);
        assert_eq!(lib.media[KEY].title, "Movie");

        let (ct, b) = get_raw(&sender, addr, &format!("/v1/art/{KEY}/backdrop"), (&me, &secret), Duration::from_secs(5)).await.unwrap();
        assert_eq!((ct.as_str(), b.as_slice()), ("image/jpeg", b"backdrop".as_slice()));
        assert!(get_raw(&sender, addr, &format!("/v1/art/{KEY}/evil"), (&me, &secret), Duration::from_secs(5)).await.is_err());
    }

    #[tokio::test]
    async fn stream_token_flow() {
        let (recv, mock, addr) = server("tok").await;
        let sender = RemoteState::load(&tmpdir("tok-s"), "S".into());
        let secret = "f".repeat(64);
        pair_by_hand(&recv, &sender.self_id(), &secret);
        let dev = Device {
            id: recv.self_id(),
            name: "Recv".into(),
            host: "127.0.0.1".into(),
            port: addr.port(),
            secret: secret.clone(),
            last_ip: None,
        };
        // Wrong secret / unknown file refused.
        let bad = Device { secret: "0".repeat(64), ..dev.clone() };
        assert!(stream_token(&sender, &bad, addr, KEY, 3).await.unwrap_err().starts_with("UNAUTHORIZED"));
        assert!(stream_token(&sender, &dev, addr, KEY, 4).await.unwrap_err().starts_with("NOT_FOUND"));

        let pr = stream_token(&sender, &dev, addr, KEY, 3).await.unwrap();
        assert_eq!(pr.title, "Movie One.mp4");
        assert_eq!(pr.mode_hint.as_deref(), Some("builtin"));
        assert!(pr.url.starts_with(&format!("http://127.0.0.1:{}/v1/stream/", addr.port())), "{}", pr.url);
        assert!(pr.url.ends_with("/Movie%20One.mp4"));
        assert_eq!(pr.subtitles.len(), 1);

        // The tokenized URLs work (via a plain GET, as WebKit/VLC would), nothing else.
        let path = pr.url.split_once(&format!(":{}", addr.port())).unwrap().1.to_string();
        let (c, _, b) = get(addr, &path, Some("bytes=0-9")).await;
        assert_eq!((c, b.len()), (206, 10));
        let sub = pr.subtitles[0].url.split_once(&format!(":{}", addr.port())).unwrap().1.to_string();
        assert_eq!(get(addr, &sub, None).await.2, b"WEBVTT\n\n5".to_vec());
        assert_eq!(get(addr, &path.replace(&format!("/{KEY}/3/"), &format!("/{KEY}/4/")), None).await.0, 401);
        assert_eq!(mock.opened.lock().unwrap().as_slice(), &[(KEY.to_string(), 3)]);
    }

    #[test]
    fn data_urls() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let url = format!("data:image/png;base64,{}", crate::artwork::base64(&bytes));
        assert_eq!(decode_data_url(&url), Some((bytes, "image/png".into())));
        assert_eq!(base64_decode("aGk="), Some(b"hi".to_vec()));
        assert!(decode_data_url("data:image/png,xx").is_none());
    }

    #[test]
    fn mode_choice() {
        assert_eq!(choose_mode("http://h/v1/stream/t/k/0/a%20b.mp4", Some("builtin"), true), Ok(Mode::Builtin));
        assert_eq!(choose_mode("http://h/v1/stream/t/k/0/a.MOV", None, false), Ok(Mode::Builtin));
        assert_eq!(choose_mode("http://h/v1/stream/t/k/0/a.mkv", None, true), Ok(Mode::Vlc));
        assert_eq!(choose_mode("http://h/v1/stream/t/k/0/a.mp4", Some("vlc"), true), Ok(Mode::Vlc));
        assert!(choose_mode("http://h/v1/stream/t/k/0/a.mkv", None, false).is_err());
    }

    #[test]
    fn peer_urls() {
        let p: IpAddr = "192.168.43.5".parse().unwrap();
        assert!(url_from_peer("http://192.168.43.5:47900/v1/stream/a/b/0/x.mp4", p));
        assert!(!url_from_peer("http://192.168.43.6:47900/v1/stream/a/b/0/x.mp4", p));
        assert!(!url_from_peer("https://192.168.43.5:47900/v1/x", p));
        assert!(!url_from_peer("http://192.168.43.5:47900/etc", p));
        assert!(!url_from_peer("http://evil@192.168.43.5/v1/x", p));
        assert!(!url_from_peer("file:///etc/passwd", p));
        let v6: IpAddr = "fe80::1".parse().unwrap();
        assert!(url_from_peer("http://[fe80::1]:47900/v1/x", v6));
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 10), Ok(None));
        assert_eq!(parse_range(Some("bytes=0-"), 10), Ok(Some((0, 9))));
        assert_eq!(parse_range(Some("bytes=2-100"), 10), Ok(Some((2, 9))));
        assert_eq!(parse_range(Some("bytes=-3"), 10), Ok(Some((7, 9))));
        assert_eq!(parse_range(Some("bytes=-30"), 10), Ok(Some((0, 9))));
        assert_eq!(parse_range(Some("bytes=10-"), 10), Err(()));
        assert_eq!(parse_range(Some("bytes=5-2"), 10), Err(()));
        assert_eq!(parse_range(Some("bytes=0-1,4-5"), 10), Ok(Some((0, 1))));
    }

    #[test]
    fn dns_sd_parsing() {
        let browse = "Browsing for _tide._tcp.local\n\
DATE: ---Fri 16 Oct 2026---\n\
12:00:00.000  ...STARTING...\n\
Timestamp     A/R    Flags  if Domain               Service Type         Instance Name\n\
12:00:00.100  Add        3   4 local.               _tide._tcp.          Tide on Mac mini\n\
12:00:00.101  Add        2   4 local.               _tide._tcp.          Tide on MacBook Air\n\
12:00:01.000  Rmv        0   4 local.               _tide._tcp.          Tide on MacBook Air\n";
        assert_eq!(parse_browse(browse), vec!["Tide on Mac mini".to_string()]);

        let id = "0123456789abcdef0123456789abcdef";
        let lookup = format!(
            "Lookup Tide on Mac mini._tide._tcp.local\n\
DATE: ---Fri 16 Oct 2026---\n\
12:00:00.000  ...STARTING...\n\
12:00:00.200  Tide\\032on\\032Mac\\032mini._tide._tcp.local. can be reached at Mac-mini.local.:47901 (interface 4) Flags: 1\n \
id={id}\n"
        );
        assert_eq!(parse_resolve(&lookup), Some(("Mac-mini.local".into(), 47901, Some(id.into()))));
        assert_eq!(parse_resolve("nothing"), None);
    }

    #[test]
    fn helpers() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        let c = random_code();
        assert!(c.len() == 6 && c.bytes().all(|b| b.is_ascii_digit()));
        assert_ne!(random_hex(16), random_hex(16));
        assert_eq!(url_host("::1".parse().unwrap()), "[::1]");
    }
}
