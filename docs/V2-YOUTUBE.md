# V2 (later): YouTube / yt-dlp download (NOT part of v1)

This is extracted from the original plan. Section names refer to PLAN.md.

Types:
```ts
YtProbe = { url, title, uploader:string|null, duration_secs:number|null, thumbnail:string|null, is_live:boolean, max_height:number|null }
YtPreset = "720p"|"480p"|"360p"|"audio_mp3"
YtJob = { id /*"yt-<n>"*/, url, title, thumbnail:string|null, preset:YtPreset,
  state:"queued"|"downloading"|"postprocessing"|"done"|"error"|"cancelled",
  progress, downloaded_bytes, total_bytes:number|null, speed_bps:number|null, eta_secs:number|null,
  file_path:string|null, error:string|null }
```

Extra: MediaRef variant { kind:"yt", id }; event `yt-jobs-updated` (YtJob[], max 4/s); AppState.yt: YtState; ytdlp.rs exposes job_file(st,id)->Option<(PathBuf,String)>; play via play_media with stream_url = asset://localhost/<urlencoded abs path> (needs tauri feature protocol-asset + assetProtocol scope $HOME/Downloads/**). Main-window input: http(s):// → window.YT.openProbeDialog(url) from ui/youtube.js. ffmpeg at /opt/homebrew/bin/ffmpeg is needed for merging (720p/480p) and mp3.

### yt-dlp commands (WP4)
| command | args | returns |
|---|---|---|
| yt_probe | {url} | YtProbe |
| yt_download | {url, preset} | YtJob |
| yt_list | – | YtJob[] |
| yt_cancel | {id} | null |
| yt_remove | {id, deleteFile} | null |
| yt_reveal | {id} | null |

- **yt_probe** runs `yt-dlp -J --no-playlist <url>`.
- **yt_cancel** kills the process and sets the job's state to `cancelled`.
- **yt_remove** cancels the job first if it is still running.
- **Missing binary:** `yt_probe` and `yt_download` return `YTDLP_NOT_FOUND`.
- **Playback:** a finished job plays via `play_media({media:{kind:"yt",id}})`. Its `stream_url` is `asset://localhost/` + urlencoded absolute path.


### WP4: yt-dlp
Owns ytdlp.rs, youtube.js, youtube.css.
- **Binary lookup:** /opt/homebrew/bin/yt-dlp, then /usr/local/bin/yt-dlp, then ~/.local/bin/yt-dlp.
- **Child environment:** set `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin` and pass `--ffmpeg-location <ffmpeg path>`.
- **Probe:** `-J --no-playlist`.
- **Common args:** `--no-playlist --newline --progress --no-simulate -o "<download_dir>/%(title)s [%(id)s].%(ext)s" --print "after_move:[done] %(filepath)s" --progress-template "download:[dl] %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s"`
- **Presets:**
  - 720p: `-f "bv*[height<=720]+ba/b[height<=720]" -S "vcodec:h264,acodec:m4a" --merge-output-format mp4`. The h264+m4a preference keeps the result playable in the builtin player.
  - 480p and 360p: the same with 480 or 360.
  - audio_mp3: `-f ba -x --audio-format mp3`.
- **Output parsing:**
  - `[dl]` lines: `NA` → null.
  - `[Merger]`, `[ExtractAudio]` and `[FixupM3u8]` → postprocessing.
  - `[done] <path>` → file_path. On exit 0 → done.
  - Keep the last stderr line as the error.
- **Jobs:** in memory only. Keep the Child so the job can be killed.
- **youtube.js:** a modal with a spinner, then thumbnail, title and duration, then 4 preset buttons (720p, 480p, 360p, Audio MP3). Errors are shown inline.

