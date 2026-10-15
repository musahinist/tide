# Tide: macOS torrent client — PLAN v1 (canonical)

> **v1 = TORRENT ONLY.** YouTube/yt-dlp is postponed to v2 (see V2-YOUTUBE.md). Do not implement anything yt-related in v1, but keep the design open for it: MediaRef is a tagged enum, so a `yt` variant can be added later.

- **Project:** /Users/mshn/Documents/downloader
- **Stack:** Tauri 2.12 + librqbit =9.0.1 (feature http-api) + plain HTML/CSS/JS (no bundler)
- **Player, VLC and subtitle details:** RESEARCH.md in this same folder. Read it.
- **Reference implementation:** rqbit's own Tauri app, tag v9.0.1, `desktop/src-tauri/src/main.rs`. A local clone is at `<scratchpad>/rqbit/`, and the librqbit source is in `~/.cargo/registry/src/*/librqbit-9.0.1/`. Do NOT follow GitHub main; it has diverged.
- **Paths found on this machine:**
  - VLC: /Applications/VLC.app/Contents/MacOS/VLC
  - ffmpeg: /opt/homebrew/bin/ffmpeg
  - yt-dlp: /opt/homebrew/bin/yt-dlp (once installed)
  - Always use absolute paths: a GUI app does not get the shell PATH.

## Scope
**In scope:**
- Add a magnet; pause, resume, remove (optionally deleting files); Show in Finder.
- Torrent file list.
- Play while downloading:
  - The builtin player window handles mp4/m4v/mov/webm and the common audio formats.
  - Everything else opens in VLC automatically, as do files that fail at runtime.
- Subtitles from inside the torrent are attached automatically.
- Session persistence.
- ~/Downloads/Tidebox as the download folder (created at start and before each add). A name already used by different content gets a unique sibling folder, e.g. "Sintel (2)".
- Unsigned .app build.

**Also in scope (added later):** default macOS handler for magnet: links (Chrome click → app opens and adds the torrent). WP0 sets it up with tauri-plugin-deep-link + LSSetDefaultHandlerForURLScheme in src/macos.rs; engine.rs exposes `pub async fn add_magnet_internal(st:&AppState, magnet:&str)`, which the `add_magnet` command and the deep-link handler both call (WP1 implements it).

**Out of scope:** YouTube/yt-dlp (v2), .torrent files and drag-and-drop, settings panel.

## Layout (owner in [])
```
.gitignore                      [WP0] src-tauri/target, src-tauri/gen
README.md                       [WP5]
ui/
  index.html, styles.css, app.js   [WP2 main window]
  player.html, player.css, player.js [WP3]
src-tauri/
  Cargo.toml, build.rs, tauri.conf.json, capabilities/default.json, icons/*   [WP0, frozen]
  src/main.rs    [WP0, frozen] runtime + init + Builder + invoke_handler with ALL commands + 1s ticker
  src/state.rs   [WP0, frozen] AppState
  src/types.rs   [WP0, frozen] all IPC structs
  src/engine.rs  [WP1] torrent logic + torrent commands + internal fns
  src/player.rs  [WP3] play/VLC/subtitle commands
  src/subtitles.rs [WP3] srt/ass→vtt, matching
```
- WP0 writes every module as a **compiling stub**: real signatures, with bodies returning `Err("NOT_IMPLEMENTED: ...".into())` or empty values.
- WP1–WP3 each edit only their own files. If one needs a change to a frozen file, it must report it instead of editing.

## Cargo.toml
```toml
[package]
name = "tide"
version = "0.1.0"
edition = "2021"

[build-dependencies]
tauri-build = { version = "2.7", features = [] }

[dependencies]
tauri = { version = "2.12", features = [] }
librqbit = { version = "=9.0.1", features = ["http-api"] }
librqbit-dualstack-sockets = "0.7"
tokio = { version = "1", features = ["rt-multi-thread","macros","sync","time","process","io-util","fs"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
urlencoding = "2"

[profile.dev.package."*"]
opt-level = 2
```

## tauri.conf.json key fields
```json
{ "productName":"Tide", "identifier":"com.musahinist.tide", "build":{"frontendDist":"../ui"},
  "app":{ "withGlobalTauri":true,
    "windows":[{"label":"main","title":"Tide","url":"index.html","width":960,"height":620,"minWidth":680,"minHeight":420,"theme":"Dark"}],
    "security":{"csp":null} },
  "bundle":{"active":true,"targets":["app"],"icon":["icons/32x32.png","icons/128x128.png","icons/128x128@2x.png","icons/icon.icns","icons/icon.png"]} }
```

`capabilities/default.json`:
```json
{"identifier":"default","windows":["main","player"],"permissions":["core:default"]}
```

## main.rs flow
1. Build a tokio multi-thread runtime `rt`, then call `tauri::async_runtime::set(rt.handle().clone())`. Keep `rt` alive for the whole program.
2. `let st = Arc::new(rt.block_on(state::init())?)`. `init` sets up:
   - download_dir = `$HOME/Downloads/Tidebox` (create it if missing; re-created before each add_magnet)
   - data_dir = `~/Library/Application Support/com.musahinist.tide`, with `session/` and `subs/` subfolders
   - Session: `Session::new_with_opts(download_dir, SessionOptions{ persistence: Some(SessionPersistenceConfig::Json{folder: Some(data_dir/session)}), fastresume: true, ..Default::default() })`
   - Api: `Api::new(session.clone(), None)` (2 args)
   - Stream listener: `librqbit_dualstack_sockets::TcpListener::bind_tcp("127.0.0.1:0".parse()?, Default::default())?`. Take the port from `listener.bind_addr().port()`; if that is not available, try fixed ports 47821..47830.
   - HTTP API: `session.spawn(debug_span!("http_api"), "http_api", HttpApi::new(api.clone(), Some(HttpApiOptions{read_only:false, ..Default::default()})).make_http_api_and_run(listener, None))`
3. Build the app: `tauri::Builder::default().manage(st.clone())`.
   - In `.setup`, spawn a ticker that runs every 1000 ms: `app.emit("torrents-updated", engine::list_rows(&st))` (needs `use tauri::Emitter`).
   - Register commands with `.invoke_handler(generate_handler![...all commands...])`.
4. Commands take `state: tauri::State<'_, Arc<AppState>>` and `app: AppHandle` as needed, and return `Result<T,String>`.
   - Any command that creates a window MUST be `async`.

## state.rs
```rust
pub struct AppState {
  pub api: librqbit::Api, pub session: Arc<librqbit::Session>,
  pub stream_port: u16, pub download_dir: PathBuf, pub data_dir: PathBuf,
  pub pending: std::sync::Mutex<HashMap<String /*info_hash*/, PendingMagnet>>,  // WP1
  pub player: crate::player::PlayerState, // WP0 stub: Default; WP3 fills
}
pub struct PendingMagnet { pub name: String, pub error: Option<String>, pub task: Option<tokio::task::JoinHandle<()>> }
```
- WP0 defines PlayerState in `player.rs` with `#[derive(Default)]` (Mutex inside).
- WP3 may add fields, but only fields with Default.

## Internal functions WP1 provides (engine.rs; WP0 stubs them)
```rust
pub fn list_rows(st:&AppState) -> Vec<TorrentRow>;
pub fn files(st:&AppState, key:&str) -> Result<Vec<FileRow>,String>;
pub fn file_abs_path(st:&AppState, key:&str, idx:usize) -> Result<PathBuf,String>;
pub fn stream_url(st:&AppState, key:&str, idx:usize) -> Result<String,String>; // http://127.0.0.1:{port}/torrents/{id}/stream/{idx}/{urlencoded basename}
pub async fn ensure_live(st:&AppState, key:&str) -> Result<(),String>;          // unpause if paused
pub async fn read_file(st:&AppState, key:&str, idx:usize, max_bytes:usize, timeout_s:u64) -> Result<Vec<u8>,String>; // api_stream + read_to_end (prioritizes pieces)
```

## librqbit 9.0.1 facts
- **Adding a torrent:**
  - `api.api_add_torrent(AddTorrent::from_url(magnet), Some(AddTorrentOptions::default()))` (or `session.add_torrent`).
  - **It BLOCKS until metadata is resolved, which can take minutes.** Spawn it as a task and keep a pending row. Abort the task if the row is removed.
- **Magnet parsing:** `Magnet::parse(&str)?` gives `.as_id20()` and `.name`. `TorrentIdOrHash::parse(&str)`.
- **Controls:**
  - `api_torrent_action_pause / _start / _forget / _delete(idx)`
  - `session.get(idx)`
  - `session.with_torrents(|it| ...)` to iterate
  - `session.update_only_files(&h, &HashSet<usize>)`
- **Stats:** `h.stats()` returns `{state: Initializing|Live|Paused|Error, error, progress_bytes, uploaded_bytes, total_bytes, finished, file_progress, live: Option<LiveStats>}`.
  - Speed: `live.download_speed.as_bytes()` gives bytes/s.
  - Peers: `live.snapshot.peer_stats.live`.
  - ETA is not exposed: compute `(total − progress) / down_bps`.
- **Identity:** `h.name()`, `h.info_hash().as_string()`.
- **Details:** `api_torrent_details(idx)` returns `.files: Option<Vec<{name, components, length, included}>>` and `.output_folder`.
- **Stream route:** `GET /torrents/{id}/stream/{file_idx}/{*filename}`.
  - Range requests return 206.
  - It prioritizes the next 32 MiB of the stream.
  - CORS already allows `tauri://localhost`.

## IPC contract (frozen, lives in types.rs)
**Conventions:**
- JS calls `window.__TAURI__.core.invoke(name,args)` and `window.__TAURI__.event.listen(name,cb)`.
- **JS arg keys are camelCase:** `deleteFiles`, `fileIndex`, `subId`.
- **Returned JSON is snake_case.**
- **Errors** reject with the string `"CODE: message"`. Codes:
  - `INVALID_INPUT`
  - `NOT_FOUND`
  - `NOT_READY`
  - `VLC_NOT_FOUND`
  - `NOT_IMPLEMENTED`
  - `INTERNAL`
- **`key`** is the 40-char lowercase hex info hash.

```ts
TorrentRow = { key, id:number|null, name, state:"resolving"|"initializing"|"downloading"|"seeding"|"paused"|"error",
  error:string|null, total_bytes, progress_bytes, progress /*0..1*/, uploaded_bytes, down_bps, up_bps, peers,
  eta_secs:number|null, output_path:string|null }
FileRow = { index, name /*components joined "/"*/, size, progress_bytes, progress,
  kind:"video"|"audio"|"subtitle"|"other", playable_builtin:boolean }
MediaRef = { kind:"torrent", key, fileIndex }   // Rust enum: #[serde(tag="kind", rename_all="lowercase")], field #[serde(rename="fileIndex")]; v2 adds { kind:"yt", id }
SubtitleTrack = { id /*opaque "t:<key>:<idx>"*/, label, lang:string|null, format:"srt"|"vtt"|"ass"|"ssa" }
PlayResult = { mode:"builtin"|"vlc", title, media:MediaRef, stream_url, subtitles:SubtitleTrack[], default_subtitle_id:string|null }
```

### Torrent commands (WP1)
| command | args | returns |
|---|---|---|
| list_torrents | – | TorrentRow[] |
| add_magnet | {magnet} | {key, name, already_exists} |
| pause_torrent | {key} | null |
| resume_torrent | {key} | null |
| remove_torrent | {key, deleteFiles} | null |
| reveal_torrent | {key} | null |
| list_files | {key} | FileRow[] |
| get_download_dir | – | string |

- **add_magnet:** a bad magnet returns `INVALID_INPUT`. Otherwise it returns immediately and resolves in the background. While resolving, the row's state is `resolving`; if resolving fails, the state is `error`.
- **remove_torrent:** also cancels a pending (resolving) row.
- **reveal_torrent:** runs `open -R <output_path>`.
- **list_files:** returns `NOT_READY` until the metadata is resolved.

### Player commands (WP3)
| command | args | returns |
|---|---|---|
| play_media | {media} | PlayResult |
| player_current | – | PlayResult\|null |
| get_subtitle_vtt | {subId} | string |
| open_in_vlc | {media, subId:string\|null} | null |
| open_external | {url} | null |

- **play_media:**
  - For a torrent, it first runs `ensure_live` and prefetches the subtitles.
  - If the file is builtin-playable: open or focus the `player` window and emit `player-load` to it.
  - Otherwise: launch VLC with the best-matching subtitle.
  - If VLC is not installed, return `VLC_NOT_FOUND`. The UI then shows a dialog with a button that calls `open_external("https://www.videolan.org/vlc/")`.
- **open_external:** https URLs only.

### Events
- **`torrents-updated`**: payload is TorrentRow[], emitted every 1000 ms to all windows.
- **`player-load`**: payload is a PlayResult, sent only to the `player` window.

### Main-window input routing (WP2)
- Trim the input first.
- If it starts with `magnet:` or is 40 hex characters → `add_magnet`.
- Otherwise → show an inline error.

## Work packages
### WP0: scaffold
Starts after the toolchain is installed.
- Write every WP0-owned file, including the stub modules and the full invoke_handler.
- Icon: `sips -s format png -z 1024 1024 /System/Library/CoreServices/CoreTypes.bundle/Contents/Resources/GenericApplicationIcon.icns --out <scratchpad png>`, then `cargo tauri icon <png>`.
- Write stub `ui/index.html`, `ui/player.html`, and empty js/css files.
- Run `cargo build` to warm the cache.

**Acceptance:**
- `cargo build` succeeds.
- The session initializes.
- `curl http://127.0.0.1:<port>/` returns JSON. A small debug log line printing the port is acceptable.

### WP1: torrent backend
Owns engine.rs.
- **State mapping:**
  - Initializing → `initializing`
  - Live + finished → `seeding`
  - Live → `downloading`
  - Paused → `paused`
  - Error → `error`
  - Pending → `resolving` or `error`
- **Name:** `h.name()`, then the magnet's `dn`, then the hash.
- **Kinds:**
  - video: mkv mp4 m4v avi mov wmv flv webm mpg mpeg ts m2ts vob ogv 3gp
  - audio: mp3 flac m4a aac ogg opus wav wma aiff mka
  - subtitle: srt vtt ass ssa
  - builtin-playable: mp4 m4v mov webm mp3 m4a aac wav flac
- **Duplicates:** if the session already manages the torrent → `already_exists: true`.

**Acceptance:**
- A magnet goes resolving → downloading, with speed and peers shown.
- Pause, resume and remove (with or without deleting files) all work.
- Reveal works.
- Torrents survive a restart.
- `curl -r 0-1000 <stream_url>` returns 206.

### WP2: main window
Owns index.html, styles.css, app.js.

**Design rule from the user: VERY MINIMAL, like WebTorrent Desktop.** No table, no column headers, no toolbar full of buttons. Calm, quiet, lots of breathing room.

- **Look:**
  - Colours: background #1e1e1e, rows #2a2a2a, text #ddd, muted #888, accent #2f7ee6. ok/warn/err colours are only for tiny state hints.
  - Font: the system font (-apple-system).
- **Top:**
  - A slim title-bar-height strip containing only a "+" button on the right.
  - "+" reveals a single input: "Paste a magnet link". Enter adds it, Esc closes it.
  - Cmd+V anywhere with a magnet on the clipboard adds it directly, no input needed.
  - **Empty state:** a centered muted hint, "Drop a magnet link or press ⌘V", and nothing else.
- **Each torrent is one row/card**, about 72–80 px tall, with a 1 px gap between rows:
  - **Line 1:** the name, large, white, ellipsized.
  - **Line 2:** a single muted stats line, e.g. "45% · 312 MB of 700 MB · ↓ 2.1 MB/s · 12 peers · 4 min left". When paused it reads "Paused". When resolving it reads "Getting metadata…".
  - **Bottom of the row:** a thin 3 px progress bar.
  - **Controls appear on hover only**, right-aligned as small circular icon buttons (inline SVG, no icon libraries):
    - ▶ play: opens the largest video/audio file through `play_media`
    - pause/resume
    - ✕ remove: a confirm dialog with a "Also delete files" checkbox
    - folder: Show in Finder
- **Click a row** to expand a minimal file list underneath:
  - Each file shows name, size, progress %, and a small ▶ for media files.
  - Use `list_files`, refreshed while the row is expanded.
- **Errors:** a small toast at the bottom that fades out. On VLC_NOT_FOUND, the toast has a "Get VLC" link that calls `open_external`.
- **No footer.**
- **Rendering:**
  - Diff rows by key so the expanded state survives the 1 Hz updates.
  - Use human-readable bytes, speed and ETA.
  - Use mock data when `window.__TAURI__` is missing.

### WP3: player
Owns player.rs, subtitles.rs, player.html, player.css, player.js. Follow RESEARCH.md for subtitle ranking, srt→vtt conversion, VLC arguments, key bindings and resume.
- **Builtin window:** `WebviewWindowBuilder::new(&app,"player",WebviewUrl::App("player.html".into())).title(..).inner_size(960.,540.)`, in an async command. Store the current item and call `emit_to("player","player-load",..)`.
- **VLC:**
  - Command: `<VLC> --play-and-exit --quiet --meta-title=<title> [--sub-file=<local>] <stream_url>`, spawned detached and reaped in a task.
  - The subtitle is downloaded via `engine::read_file` (2 MB cap, 60 s timeout) and written to `data_dir/subs/<key>_<idx>.<ext>`.
- **Player window (minimal too: black background, native controls, subtitle selector only visible on hover):**
  - `<video controls autoplay>` without `crossorigin`, plus `<audio>` for audio.
  - Subtitle `<select>`: Off plus the available tracks. A track loads via `get_subtitle_vtt` → Blob → `<track>`.
  - On `player-load`, swap the source.
- **Automatic VLC fallback:** trigger it on media error code 3/4, or on `loadedmetadata` with `videoWidth===0` for a video-kind file, or `videoTracks`/`audioTracks` length 0 when those APIs exist.
  - Call `open_in_vlc(media, selectedSubId)`, then close the window.
  - On `VLC_NOT_FOUND`, show a message and a "Get VLC" button.
  - Also provide a manual "Open in VLC" button.

### WP5: build + smoke test
Runs last and may touch any file.
- Run `cargo tauri build`, which produces `src-tauri/target/release/bundle/macos/Tide.app`. If needed, `xattr -dr com.apple.quarantine`.
- Smoke-test:
  - adding a magnet
  - pause and resume
  - persistence
  - builtin mp4 playback
  - mkv in VLC
  - Reveal
- Write the README.

## Risks
- **Toolchain:** yt-dlp may already be installed by the toolchain agent; ignore it in v1.
- **Shared build directory:** agents share `src-tauri/target`. Never run `cargo clean`. Concurrent cargo runs wait on a lock, which is fine. Prefer `cargo check` while developing.
- **WKWebView playback:**
  - HEVC plays only when tagged hvc1.
  - mkv, avi and wmv don't play.
  - AC3/DTS audio may play silently.
  - That's why both the runtime fallback and the manual VLC button exist.
- **Cmd+V:** keep Tauri's default Edit menu, or Cmd+V stops working.
- **Unsigned app:** Gatekeeper and the firewall will prompt on first run.
