# Research findings: WebTorrent Desktop behaviour + librqbit streaming

Sources (read-only clones in the scratchpad): wtd/ (webtorrent-desktop), wt/ (webtorrent 1.9.7), s2v/ (srt-to-vtt), rqbit/ (librqbit 9.0.1).

## Playability (wtd/src/renderer/lib/media-extensions.js)
- WTD decides by extension only, then detects codec failures at runtime.
- Runtime detection (player-page.js:153-224):
  - The media element's `error` event.
  - In `loadedmetadata`: for video, `videoTracks.length===0` means "Video codec unsupported" and `audioTracks.length===0` means "Audio codec unsupported".
  - Our version: on any of these, fall back to VLC automatically. The user wants an automatic fallback, with no modal.
- Our builtin set is in the plan's media rules (mp4, m4v, mov, webm, mp3, m4a, aac, wav, flac). Everything else in WTD's video/audio lists goes to VLC.
  - Video: avi mp4 m4v webm mov mkv mpg mpeg ogv wmv m2ts
  - Audio: aac aif aiff asf dff dsf flac m2a m4a mpc m4b mka mp2 mp3 oga ogg opus spx wma wav wv wvp

## VLC (wtd/src/main/external-player.js, vlc-command)
- Look for VLC in this order, taking the first that exists:
  1. /Applications/VLC.app/Contents/MacOS/VLC
  2. ~/Applications/VLC.app/Contents/MacOS/VLC
  3. /usr/local/bin/vlc
  4. /opt/homebrew/bin/vlc
- Command: `VLC --play-and-exit --quiet --meta-title=<title> <stream_url>`, plus `--sub-file=<local path>` when a subtitle exists. The subtitle argument is our addition; WTD doesn't pass subtitles.
- Spawn detached with stdio ignored, and don't wait on the process.
- If VLC is missing, return the error code VLC_NOT_FOUND. The UI then shows "Get VLC", which opens https://www.videolan.org/vlc/.

## Subtitles (wtd subtitles-controller.js, s2v/index.js)
- **Formats:** srt and vtt (plus a basic ass/ssa conversion, which is our extra).
- **WTD's behaviour:** it attaches every srt/vtt file in the torrent that is already complete.
- **Our behaviour:** fetch subtitle files first by reading them fully through `api_stream`, which prioritises their pieces. Then rank them for each video:
  1. exact basename
  2. basename.<lang>
  3. same folder
  4. a Subs/ or Subtitles/ folder
  5. everything else
- **Default track:** the top-ranked track, preferring "en" when ranks tie.
- **SRT to VTT conversion:**
  1. Decode the bytes. Try UTF-8 first; on failure, fall back to cp1252/latin1 via lossy decoding, or use encoding_rs if it's available.
  2. Strip the BOM, then trim each line. Blank lines separate cues.
  3. Write the `WEBVTT\n\n` header.
  4. In timing lines, change `HH:MM:SS,mmm` to `HH:MM:SS.mmm`.
  5. Convert tags: `{\i1}` → `<i>`, `{\i0}` and `{\i}` → `</i>`, and the same for b and u. `{i}` → `<i>` and `{/i}` → `</i>`.
  6. If a cue's text starts with `{\an8}`, remove the tag and append ` line:5%` to its timing line.
  7. Cue numbers can stay.
  8. `.vtt` files pass through unchanged.
- **Player side:**
  - Each track: `<track kind="subtitles" label srclang src=blob: or data:text/vtt;base64,...>`.
  - Selection sets `textTracks[i].mode = 'showing'` or `'hidden'`. "Off" hides all tracks.
  - Duplicate labels get numbered: "English", "English 2", …

## Streaming (librqbit 9.0.1: http_api/handlers/mod.rs, streaming.rs)
- **Endpoint:** `GET /torrents/{id_or_infohash}/stream/{file_idx}/{filename}`. Always include the filename so VLC and `<video>` see the extension.
- **Range requests:**
  - `bytes=START-` and `bytes=START-END` return 206.
  - Suffix ranges and multi-range requests return a full 200.
  - Out-of-bounds ranges return 416.
  - Content-Type comes from mime_guess on the filename.
- **Prioritisation:** an open stream prioritises the next 32 MiB from its read position. Streaming works even for files excluded by `only_files`. Reads block until their pieces arrive.
- **Selecting files at runtime:** `session.update_only_files(&handle, &HashSet<usize>)`.

## Player UX to copy (wtd main.js / menu.js / playback-controller.js)
- **Keys:**

  | Key | Action |
  |---|---|
  | Space or k | play/pause |
  | ←/→ | seek 5 s |
  | j/l | seek 10 s |
  | ↑/↓ | volume ±0.1 |
  | f | fullscreen (double-click does the same) |
  | Esc | exit fullscreen |
  | `>` / `<` | speed ±0.25, clamped to 0.25–2× |
  | Click on video | play/pause |

- **Resume:** save `currentTime` per (key, file_index) in localStorage. Resume only if less than 90% has been watched and more than 10 s remain.
- **Audio-only files:** use `<audio>` with a simple poster area. Subtitles are disabled.
- **Stalls:** the `waiting` and `stalled` events show a spinner; they are not errors.
