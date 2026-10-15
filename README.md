# Tide

A minimal macOS video downloader. Paste a link, it starts downloading, and you can press play and watch before the download finishes.

## What it does

- **Paste a link or click one in the browser.** Tide registers itself as the default handler for `magnet:` links, so clicking one in Chrome or Safari opens Tide and starts the download. You can also press ⌘V anywhere in the window, or use the `+` button.
- **Watch while it downloads.** Press play on any video or audio file at any time. The part you are watching is fetched first.
- **Built-in player for common formats.** mp4, m4v, mov, webm, mp3, m4a, aac, wav and flac play in a lightweight player window.
- **VLC for everything else.** mkv, avi, wmv and other formats open in VLC automatically. So do files whose codecs the built-in player turns out not to support (for example AC3/DTS audio, or HEVC not tagged `hvc1`). If VLC isn't installed, Tide shows a "Get VLC" link.
- **Automatic subtitles.** Subtitle files that come with the video (`.srt`, `.vtt`, `.ass`, `.ssa`) are found, matched to the video and attached. The best match is turned on by default.
- **Minimal UI.** One row per download with name, progress, speed and time left. Controls appear on hover: play, pause/resume, remove (optionally deleting the files) and Show in Finder. Click a row to see its files.
- **Survives restarts.** Downloads resume where they left off. Files go to `~/Downloads`.

Supported today: magnet links (and bare 40-character info hashes). Downloading from video sites such as YouTube is planned for v2 (see [Roadmap](#roadmap)).

## Technical

### Stack

| Layer | Choice |
|---|---|
| Shell | [Tauri 2](https://tauri.app) (Rust). Uses the system WKWebView, no Electron and no bundled Chromium |
| Engine | [librqbit](https://github.com/ikatson/rqbit) `=9.0.1`, features `http-api` + `tracing-subscriber-utils` |
| Frontend | Plain HTML/CSS/JS in `ui/`. No framework, no bundler, no build step |
| macOS integration | `tauri-plugin-deep-link` + `LSSetDefaultHandlerForURLScheme` (via `core-foundation`) |
| Async runtime | tokio (multi-thread), shared with Tauri via `tauri::async_runtime::set` |

### Architecture

```
┌────────────────────────── WKWebView windows (ui/) ──────────────────────────┐
│  main window: index.html / app.js        player window: player.html / .js   │
│  list, add, pause, remove, files         <video>/<audio>, subtitle <track>  │
└──────────────┬──────────────────────────────────────▲───────────────────────┘
               │ invoke(command, args)                │ events
               │                                      │  torrents-updated (1 Hz, all windows)
               ▼                                      │  player-load (player window only)
┌─────────────────────────── Rust backend (src-tauri/src) ────────────────────┐
│  main.rs      runtime, setup, command registration, 1 s ticker              │
│  engine.rs    add/pause/resume/remove/reveal, file list, stream URLs        │
│  player.rs    play_media: builtin vs VLC, player window, VLC launch         │
│  subtitles.rs subtitle matching/ranking, SRT/ASS → WebVTT                   │
│  macos.rs     magnet: deep-link handling, default-handler registration      │
│  state.rs     AppState (Session, Api, stream port, dirs, pending magnets)   │
└──────────────┬──────────────────────────────────────────────────────────────┘
               │
               ▼
┌────────────────────────────── librqbit ─────────────────────────────────────┐
│  Session (peers, pieces, disk, JSON persistence + fastresume)               │
│  HTTP API on 127.0.0.1:<random port>                                        │
│    GET /torrents/{id}/stream/{file_idx}/{filename}                          │
│    Range requests → 206, prioritizes the next 32 MiB after the read point   │
└──────────────┬────────────────────────────────────────┬─────────────────────┘
               │ http://127.0.0.1:<port>/…              │ same URL (+ --sub-file)
               ▼                                        ▼
      built-in <video>/<audio>                   VLC (external process)
```

### Streaming while downloading

On startup Tide binds librqbit's HTTP API to a random port on `127.0.0.1`. Playing a file resumes the download if it was paused, then hands the player a URL of the form:

```
http://127.0.0.1:<port>/torrents/<id>/stream/<file_idx>/<url-encoded file name>
```

The file name is always included so `<video>` and VLC can tell the format from the extension. The player issues HTTP Range requests (`bytes=START-` returns `206 Partial Content`). Each open stream makes librqbit prioritize the pieces covering the next 32 MiB from the current read position. A read blocks until its pieces arrive, which the player shows as buffering. Seeking simply opens a new range and moves the priority window.

### Built-in player or VLC

The decision happens in two stages:

1. **By extension (backend).** `mp4 m4v mov webm mp3 m4a aac wav flac` go to the built-in player window. Every other video or audio extension (`mkv avi wmv flv mpg ts m2ts vob ogv 3gp`, `ogg opus wma mka aiff`, …) launches VLC directly.
2. **By runtime codec detection (player window).** Even with a supported container, WKWebView may not support the codecs inside. The player switches to VLC automatically, with no dialog, when any of these happen:
   - a media error with code 3 (decode) or 4 (source not supported)
   - `loadedmetadata` fires with `videoWidth === 0` for a video file
   - `videoTracks` / `audioTracks` report zero tracks, where those APIs exist

   It then calls `open_in_vlc` with the selected subtitle and closes itself. An "Open in VLC" button is also available manually.

VLC is looked up at `/Applications/VLC.app`, `~/Applications/VLC.app`, `/usr/local/bin/vlc` and `/opt/homebrew/bin/vlc`, always by absolute path because GUI apps don't inherit the shell `PATH`. It is launched detached:

```
VLC --play-and-exit --quiet --meta-title=<title> [--sub-file=<local subtitle>] <stream_url>
```

### Subtitles

- **Discovery.** Subtitle files inside the same download are fetched first (read through the stream API, which prioritizes their pieces; 2 MB cap, 60 s timeout).
- **Ranking per video.** Candidates rank in this order: exact basename (`Movie.srt`), then `Movie.<lang>.srt`, then same folder, then a `Subs/` or `Subtitles/` folder, then anything else. On ties, English is preferred. The top track is the default; duplicate labels are numbered ("English", "English 2").
- **Conversion to WebVTT** for the built-in player:
  - decode UTF-8, falling back to lossy Latin-1/cp1252
  - strip the BOM and add the `WEBVTT` header
  - change `HH:MM:SS,mmm` to `HH:MM:SS.mmm`
  - map `{\i1}`/`{i}`-style tags to `<i>`, `<b>` and `<u>`
  - turn `{\an8}` into `line:5%`

  `.vtt` passes through unchanged, and `.ass`/`.ssa` get a basic conversion. The player loads tracks via `get_subtitle_vtt` as Blob URLs.
- **For VLC** the original file is written to `<data dir>/subs/<key>_<idx>.<ext>` and passed with `--sub-file`.

### Default `magnet:` handler

`tauri-plugin-deep-link` declares the `magnet` URL scheme in the app bundle and delivers clicked links to the running app (or launches it). On startup `src/macos.rs` calls `LSSetDefaultHandlerForURLScheme("magnet", "com.musahinist.tide")` so Tide becomes the system default. Both the deep-link handler and the `add_magnet` command go through the same `engine::add_magnet_internal`.

Adding a magnet returns immediately. Metadata resolution can take minutes and runs as a background task; meanwhile the row shows "Getting metadata…". Removing a row that is still resolving cancels the task.

### Persistence

| What | Where |
|---|---|
| Session state (torrent list, fastresume data) | `~/Library/Application Support/com.musahinist.tide/session/` |
| Subtitle files handed to VLC | `~/Library/Application Support/com.musahinist.tide/subs/` |
| Downloaded files | `~/Downloads` |
| Playback position (per file, resume if < 90 % watched) | WebView `localStorage` |

### IPC surface

Commands return `Result<T, String>`. Errors are `"CODE: message"` with codes `INVALID_INPUT`, `NOT_FOUND`, `NOT_READY`, `VLC_NOT_FOUND`, `NOT_IMPLEMENTED` and `INTERNAL`. JS args are camelCase and returned JSON is snake_case.

- Download commands: `list_torrents`, `add_magnet`, `pause_torrent`, `resume_torrent`, `remove_torrent`, `reveal_torrent`, `list_files`, `get_download_dir`
- Player commands: `play_media`, `player_current`, `get_subtitle_vtt`, `open_in_vlc`, `open_external` (https only)
- Events: `torrents-updated` (every 1 s), `player-load`

All IPC types live in `src-tauri/src/types.rs`. `MediaRef` is a tagged enum (`{ kind: "torrent", key, fileIndex }`) so other sources can be added later.

### Project layout

```
.
├── README.md
├── ui/                         frontend, served as-is (frontendDist = ../ui)
│   ├── index.html  styles.css  app.js        main window
│   └── player.html player.css  player.js     player window
└── src-tauri/
    ├── Cargo.toml              package "tide"
    ├── build.rs
    ├── tauri.conf.json         productName Tide, identifier com.musahinist.tide
    ├── capabilities/default.json
    ├── icons/
    └── src/
        ├── main.rs             runtime, Builder, invoke_handler, 1 s ticker
        ├── state.rs            AppState + init (Session, HTTP API, dirs)
        ├── types.rs            IPC structs
        ├── engine.rs           download logic + commands
        ├── player.rs           play / VLC / subtitle commands
        ├── subtitles.rs        matching + SRT/ASS → WebVTT
        └── macos.rs            magnet: default handler
```

## Requirements

- macOS (tested on Apple Silicon)
- Rust stable via [rustup](https://rustup.rs)
- Tauri CLI v2: `cargo install tauri-cli --version "^2"`
- Xcode Command Line Tools: `xcode-select --install`
- Optional: [VLC](https://www.videolan.org/vlc/) for formats the built-in player can't handle

## Build and run

```sh
git clone https://github.com/musahinist/tide.git
cd tide/src-tauri

cargo tauri dev      # run in development
cargo tauri build    # release build
```

The release build produces:

```
src-tauri/target/release/bundle/macos/Tide.app
```

Copy it to `/Applications`. The `magnet:` default-handler registration works best from an installed app.

### Unsigned app / Gatekeeper

The app is not signed or notarized, so macOS blocks it on first launch. Either right-click `Tide.app` → **Open** → **Open**, or clear the quarantine flag:

```sh
xattr -dr com.apple.quarantine /Applications/Tide.app
```

On first run macOS may also ask whether Tide may accept incoming network connections. Allow it so peers can connect.

## Roadmap

- **v2: video-site downloads.** Paste a YouTube (or other supported site) URL, pick a preset (720p, 480p, 360p, or audio-only MP3) and download it via [yt-dlp](https://github.com/yt-dlp/yt-dlp) + ffmpeg. Video presets prefer h264 + AAC so the result plays in the built-in player. These downloads will appear in the same list and play through the same player.
- **Later ideas:** `.torrent` files and drag-and-drop, a small settings panel (download folder, limits).

## Credits

Inspired by [WebTorrent Desktop](https://github.com/webtorrent/webtorrent-desktop): its minimal UI, playback behavior and VLC fallback. Streaming is powered by [rqbit / librqbit](https://github.com/ikatson/rqbit).
