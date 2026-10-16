#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod archive;
mod artwork;
mod cleanup;
mod engine;
mod groups;
mod macos;
mod metadata;
mod nameparse;
mod netmode;
mod nextup;
mod notify;
mod player;
mod remote;
mod state;
mod subfetch;
mod subtitles;
mod sync;
mod types;
mod watcher;
mod ytdlp;

use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;

use crate::state::AppState;

const BUNDLE_ID: &str = "com.musahinist.tide";

fn focus_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Unix socket used by tauri-plugin-single-instance on macOS (identifier with `.`/`-`
/// replaced by `_`). Must match the plugin's `socket_path`.
fn single_instance_socket() -> std::path::PathBuf {
    format!("/tmp/{}_si.sock", BUNDLE_ID.replace(['.', '-'], "_")).into()
}

/// The single-instance plugin only checks during `tauri::Builder` setup, but the librqbit
/// session (DHT port, session dir) is created before that. Do the same handshake early: if
/// an instance is listening, hand it our cwd/argv (plugin wire format) and exit.
fn forward_to_running_instance() {
    use std::io::Write;
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(single_instance_socket()) else {
        return;
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let msg = format!("{}\0\0{}", cwd.to_string_lossy(), args.join("\0"));
    if stream.write_all(msg.as_bytes()).and_then(|_| stream.flush()).is_ok() {
        drop(stream);
        println!("tide: already running; handed off to the existing instance");
        std::process::exit(0);
    }
}

/// Add every `magnet:` URL via the shared engine logic, then show+focus the main window.
fn handle_urls(app: &AppHandle, st: &Arc<AppState>, urls: Vec<String>) {
    let magnets: Vec<String> = urls
        .into_iter()
        .filter(|u| u.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("magnet:")))
        .collect();
    if magnets.is_empty() {
        return;
    }
    let app = app.clone();
    let st = st.clone();
    tauri::async_runtime::spawn(async move {
        for m in magnets {
            match engine::add_magnet_internal(&st, &m).await {
                Ok(r) => tracing::info!(key = %r.key, name = %r.name, "magnet added via deep link"),
                Err(e) => tracing::warn!("deep-link add_magnet failed: {e}"),
            }
        }
        focus_main(&app);
    });
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,librqbit_upnp=debug")),
        )
        .init();

    forward_to_running_instance();

    // Keep `rt` alive for the whole program.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    tauri::async_runtime::set(rt.handle().clone());

    let st = Arc::new(rt.block_on(state::init())?);
    rt.block_on(engine::migrate_to_tidebox(&st));
    rt.block_on(engine::restore_pending(&st));
    println!("tide: stream server on http://127.0.0.1:{}/", st.stream_port);
    tracing::info!(port = st.stream_port, "stream server listening");

    macos::register_default_magnet_handler(BUNDLE_ID, &st.data_dir);

    let remote_state = Arc::new(remote::RemoteState::load(&st.data_dir, remote::computer_name()));
    sync::init_device(&remote_state.self_id());
    let sync_hub = Arc::new(sync::SyncHub::new(&st.data_dir, remote_state.self_id(), sync::SettingsIo::App));
    let placer = Arc::new(archive::AppPlacer { st: st.clone(), app: std::sync::OnceLock::new() });
    let receiver = Arc::new(archive::Receiver::new(archive::archive_dir_setting(), placer.clone()));
    let archiver = Arc::new(archive::Archiver::new());
    let setup_state = st.clone();
    let si_state = st.clone();
    let (setup_remote, setup_hub) = (remote_state.clone(), sync_hub.clone());
    let setup_archiver = archiver.clone();
    tauri::Builder::default()
        // Must be first. A second launch hands argv to us and exits; on macOS the
        // deep-link plugin ignores forwarded argv, so magnets are handled here too.
        .plugin(tauri_plugin_single_instance::init(move |app, args, _cwd| {
            focus_main(app);
            handle_urls(app, &si_state, args);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .manage(st.clone())
        .manage(remote_state)
        .manage(sync_hub)
        .manage(receiver)
        .manage(archiver)
        .setup(move |app| {
            let handle = app.handle().clone();

            // magnet: deep links (warm: on_open_url; cold launch: get_current).
            {
                let h = handle.clone();
                let st = setup_state.clone();
                app.deep_link().on_open_url(move |event| {
                    let urls = event.urls().into_iter().map(|u| u.to_string()).collect();
                    handle_urls(&h, &st, urls);
                });
                match app.deep_link().get_current() {
                    Ok(Some(urls)) => handle_urls(
                        &handle,
                        &setup_state,
                        urls.into_iter().map(|u| u.to_string()).collect(),
                    ),
                    Ok(None) => {}
                    Err(e) => tracing::debug!("deep_link get_current failed: {e}"),
                }
            }

            // New-episode watcher (network at most once per 24 h).
            watcher::spawn_background(handle.clone(), setup_state.clone());

            // "Play on another Mac" LAN server + Bonjour advert (if enabled).
            remote::spawn(handle.clone(), setup_state.clone());
            // Mac↔Mac sync of watch state, groups and shared settings.
            sync::spawn(handle.clone(), setup_hub.clone(), setup_remote.clone());
            // Archive moves to / auto-clean on the archive Mac (LAN only).
            let _ = placer.app.set(handle.clone());
            archive::spawn(handle.clone(), setup_state.clone(), setup_remote.clone(), setup_archiver.clone());
            archive::spawn_autoclean(setup_state.clone());
            // Mobile-data mode (path monitor + 1 s loop, "net-status" events).
            netmode::spawn_background(handle.clone(), setup_state.clone());

            // 1 s ticker.
            let st = setup_state.clone();
            tauri::async_runtime::spawn(async move {
                let mut iv = tokio::time::interval(Duration::from_millis(1000));
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    iv.tick().await;
                    engine::pause_finished(&st).await;
                    let rows = engine::list_rows(&st);
                    if let Err(e) = handle.emit("torrents-updated", rows) {
                        tracing::debug!("emit torrents-updated failed: {e}");
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // torrent (WP1)
            engine::list_torrents,
            engine::add_magnet,
            engine::pause_torrent,
            engine::resume_torrent,
            engine::remove_torrent,
            engine::reveal_torrent,
            engine::list_files,
            engine::get_download_dir,
            engine::get_torrent_details,
            // artwork
            artwork::get_artwork,
            // metadata (WP6)
            metadata::get_media_info,
            metadata::get_media_image,
            metadata::search_media,
            metadata::set_media_match,
            metadata::clear_media_match,
            metadata::parse_names,
            metadata::list_episodes,
            // player (WP3)
            player::play_media,
            player::player_current,
            player::get_subtitle_vtt,
            player::open_in_vlc,
            player::open_external,
            subfetch::fetch_online_subtitles,
            nextup::next_episode,
            // groups + watched
            groups::list_groups,
            groups::create_group,
            groups::rename_group,
            groups::add_to_group,
            groups::remove_from_group,
            groups::delete_group,
            groups::get_watched,
            groups::set_watched,
            // new-episode watcher
            watcher::watcher_get_settings,
            watcher::watcher_set_settings,
            watcher::watcher_check,
            watcher::watcher_follow,
            watcher::watcher_unfollow,
            watcher::watcher_dismiss,
            // yt-dlp (v2)
            ytdlp::yt_probe,
            ytdlp::yt_download,
            ytdlp::yt_list,
            ytdlp::yt_cancel,
            ytdlp::yt_remove,
            ytdlp::yt_reveal,
            // remote play (Tide → Tide on the LAN)
            remote::remote_status,
            remote::remote_set_enabled,
            remote::remote_discover,
            remote::remote_pair_start,
            remote::remote_add_manual,
            remote::remote_pair_finish,
            remote::remote_unpair,
            remote::remote_play,
            // Mac↔Mac sync
            remote::remote_library,
            remote::remote_play_local,
            remote::remote_art,
            sync::resume_get,
            sync::resume_set,
            sync::resume_list,
            sync::sync_status,
            sync::sync_now,
            // mobile-data mode, notifications, cleanup
            netmode::net_status,
            netmode::net_set_mode,
            netmode::net_mark_current,
            notify::notify_get_settings,
            notify::notify_set_settings,
            cleanup::cleanup_candidates,
            cleanup::cleanup_apply,
            // archive moves (Mac mini / NEO Storage)
            archive::archive_settings,
            archive::archive_set_settings,
            archive::archive_move,
            archive::archive_status,
        ])
        .run(tauri::generate_context!())?;

    drop(rt);
    Ok(())
}
