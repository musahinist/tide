use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use librqbit::http_api::{HttpApi, HttpApiOptions};
use librqbit::{
    Api, ListenerMode, ListenerOptions, Session, SessionOptions, SessionPersistenceConfig,
};
use tracing::debug_span;

#[allow(dead_code)]
pub struct AppState {
    pub api: Api,
    pub session: Arc<Session>,
    pub stream_port: u16,
    pub download_dir: PathBuf,
    pub data_dir: PathBuf,
    /// info_hash -> magnet being resolved (WP1).
    pub pending: std::sync::Mutex<HashMap<String, PendingMagnet>>,
    /// WP3 fills; Default.
    pub player: crate::player::PlayerState,
    /// WP6 movie/TV metadata; Default.
    pub meta: crate::metadata::MetaState,
    /// v2 yt-dlp downloads; Default.
    pub yt: crate::ytdlp::YtState,
}

#[allow(dead_code)]
pub struct PendingMagnet {
    /// Normalised magnet URI; persisted to `data_dir/pending.json`.
    pub magnet: String,
    pub name: String,
    pub error: Option<String>,
    pub task: Option<tokio::task::JoinHandle<()>>,
}

fn bind_stream_listener() -> anyhow::Result<librqbit_dualstack_sockets::TcpListener> {
    use librqbit_dualstack_sockets::TcpListener;
    match TcpListener::bind_tcp("127.0.0.1:0".parse()?, Default::default()) {
        Ok(l) if l.bind_addr().port() != 0 => return Ok(l),
        Ok(_) => tracing::warn!("ephemeral bind returned port 0; trying fixed ports"),
        Err(e) => tracing::warn!("ephemeral bind failed: {e:#}; trying fixed ports"),
    }
    let mut last_err = None;
    for port in 47821u16..=47830 {
        match TcpListener::bind_tcp(format!("127.0.0.1:{port}").parse()?, Default::default()) {
            Ok(l) => return Ok(l),
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow::anyhow!("could not bind stream listener: {last_err:?}"))
}

/// Preferred BitTorrent peer-listen ports (the classic 6881–6889 range).
const PEER_PORTS: std::ops::RangeInclusive<u16> = 6881..=6889;

/// Pick the first free TCP port in `PEER_PORTS` (dual-stack, all interfaces),
/// falling back to 0 (OS-assigned) if the whole range is taken — e.g. another
/// BitTorrent client is running. The probe socket is dropped right away; librqbit
/// re-binds the port itself.
fn pick_peer_port() -> u16 {
    use librqbit_dualstack_sockets::TcpListener;
    for port in PEER_PORTS {
        let addr = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
        if TcpListener::bind_tcp(addr, Default::default()).is_ok() {
            return port;
        }
    }
    tracing::warn!("peer ports {PEER_PORTS:?} all busy; using an OS-assigned port");
    0
}

/// All new downloads go to `~/Downloads/Tidebox` (torrents added earlier keep the output
/// folder librqbit persisted for them).
pub fn download_dir_for(home: &std::path::Path) -> PathBuf {
    home.join("Downloads").join("Tidebox")
}

/// Must be called inside the tokio runtime (binding needs a reactor).
pub async fn init() -> anyhow::Result<AppState> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    let home = PathBuf::from(home);

    let download_dir = download_dir_for(&home);
    std::fs::create_dir_all(&download_dir).context("create download dir")?;

    let data_dir = home.join("Library/Application Support/com.musahinist.tide");
    let session_dir = data_dir.join("session");
    std::fs::create_dir_all(&session_dir).context("create session dir")?;
    std::fs::create_dir_all(data_dir.join("subs")).context("create subs dir")?;

    let session = Session::new_with_opts(
        download_dir.clone(),
        SessionOptions {
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(session_dir),
            }),
            fastresume: true,
            // Accept incoming peer connections on a stable port and ask the router
            // to forward it via UPnP, so peers behind NAT can reach us too.
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: (std::net::Ipv6Addr::UNSPECIFIED, pick_peer_port()).into(),
                enable_upnp_port_forwarding: true,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .context("init librqbit session")?;
    // UPnP runs in the background: librqbit logs "starting UPnP port forwarder"
    // here, then (target librqbit_upnp, debug level, enabled by default in main.rs)
    // "successfully port forwarded" or a "failed to ..." warning.
    tracing::info!(
        listen = ?session.listen_addr(),
        announce_port = ?session.announce_port(),
        upnp = true,
        "peer listener ready"
    );

    // With feature "tracing-subscriber-utils", Api::new takes 3 args.
    let api = Api::new(session.clone(), None, None);

    let listener = bind_stream_listener()?;
    let stream_port = listener.bind_addr().port();

    session.spawn(
        debug_span!("http_api"),
        "http_api",
        HttpApi::new(
            api.clone(),
            // read_only: Tide only needs GET /torrents/{id}/stream/...; all mutations go
            // through Session/Api directly. With read_only:false any web page could POST
            // /torrents?output_folder=<anywhere> (no-cors simple request) or /delete.
            Some(HttpApiOptions {
                read_only: true,
                ..Default::default()
            }),
        )
        .make_http_api_and_run(listener, None),
    );

    Ok(AppState {
        api,
        session,
        stream_port,
        download_dir,
        data_dir,
        pending: std::sync::Mutex::new(HashMap::new()),
        player: Default::default(),
        meta: Default::default(),
        yt: Default::default(),
    })
}
