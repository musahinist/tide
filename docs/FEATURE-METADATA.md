# Tide — FEATURE: movie/TV metadata (TMDB) — implementation plan

Status: PLANNED, not implemented. Nothing in the project has been changed for this feature.
Read PLAN.md first for the IPC conventions: JS args are camelCase, JSON is snake_case, and errors are `"CODE: msg"`.

## 0. Goal in one paragraph
For every torrent, guess from its name whether it is a movie or a TV episode/season. Look it up **once** on TMDB and write the result **to disk next to the downloaded content** as Kodi/Plex/Infuse-compatible sidecars (`tide.json`, `poster.jpg`, `fanart.jpg`, `movie.nfo`/`tvshow.nfo`). From then on the app reads only those sidecars; it never queries TMDB again for that item. The UI shows the backdrop as a dimmed card background, a nicer title, the rating, and an expanded detail block with an IMDb link and a "Wrong match?" manual fix. IMDb has no free API, so TMDB's `imdb_id` is used to link to IMDb.

## 1. Files

| file | change |
|---|---|
| `src-tauri/src/metadata.rs` | **NEW**. Holds everything: the name parser, the TMDB client, sidecars, the index, settings and the commands. |
| `src-tauri/src/types.rs` | Additive only: `MediaType`, `MediaInfo`, `Settings`. |
| `src-tauri/src/state.rs` | Additive only: add `pub meta: crate::metadata::MetaState` to `AppState`, and `meta: Default::default()` in `init()`. |
| `src-tauri/src/main.rs` | Add `mod metadata;` and register 7 commands in `generate_handler!`. |
| `src-tauri/Cargo.toml` | Add `reqwest`, `regex` and possibly `base64` (see §2). |
| `ui/index.html, styles.css, app.js` | Gear button, settings panel, row decoration, detail block, match search, and mock data. |
| `engine.rs` | **One small hook, made by whoever owns engine.rs:** `remove_torrent` with `deleteFiles=true` must also delete `metadata::sidecar_paths(st,key)`. See §9. |
| `player.rs` | **No change needed.** `open_external` already accepts any `https://` URL, including `https://www.imdb.com/...` and `https://www.themoviedb.org/...`. |

## 2. Dependencies (verified against the current Cargo.lock)

The dependency tree already contains:
- `reqwest 0.13.5`, pulled in by librqbit with `default-features=false` and features `json`, `socks`, `stream`. librqbit's own default feature `default-tls` turns on `reqwest/default-tls`.
- In reqwest 0.13, **`default-tls` = `rustls` (aws-lc-rs + rustls-platform-verifier)**. That is the TLS stack already compiled, so there is no native-tls / second stack.
- `regex 1.13.1`, which librqbit depends on.
- `base64 0.23.1`, which reqwest depends on.

Add:
```toml
reqwest = { version = "0.13", default-features = false, features = ["json", "default-tls"] }
regex = "1"
base64 = "0.23"          # only if images are returned as data: URLs (recommended, §7)
```

**Gotcha:** in reqwest 0.13, `RequestBuilder::query()` sits behind the optional feature **`query`**, which nothing in the tree enables today. Either:
- add `"query"` to the features list (this only recompiles reqwest), or
- build query strings by hand with the existing `urlencoding` dep.

Building by hand is recommended (`fn qs(params:&[(&str,&str)])->String`), because it keeps reqwest's features identical to what's already compiled.

**Don't use `bearer_auth`/`header` for the v3 key.** The v3 key goes in the query string as `api_key=`.

Create one `reqwest::Client` (kept in a `OnceLock` in MetaState) with:
- a 15 s timeout
- user agent `Tide/0.1`

## 3. Name parsing

### Crate choice
Write our own small regex parser in `metadata.rs`. It is about 150 lines, deterministic and unit-tested.
- Rust ports of parse-torrent-title exist (search crates.io for `torrent-name-parser` and `ptt`). They were **not evaluated**: the network/crate check was not done.
- Use a crate only if it is maintained (released in the last 12 months), MIT/Apache, has no heavy deps, and passes the test table below. Otherwise own code.

### Output
```rust
pub struct Parsed {
    title: String,
    year: Option<u16>,
    season: Option<u32>,
    episode: Option<u32>,
}
// is_tv = season.is_some()
```

### Algorithm
Compile the regexes once (`OnceLock<Regex>`) and use `(?i)` throughout.

1. **Strip a known file extension** (mkv, mp4, m4v, avi, mov, wmv, webm, ts, m2ts, mpg, mpeg).
2. **Strip leading `[Group]` tags.**
3. **Normalise separators.**
   - Replace `.`, `_`, `(`, `)`, `[`, `]`, `{`, `}` with a space.
   - Collapse whitespace.
   - Example: "Mr. Robot" becomes "Mr Robot", which is fine for search.
4. **Find markers.** Each match gives a position, and the title is cut at the earliest one.
   - **SxxEyy:** `\bS(\d{1,2})\s?E(\d{1,3})\b` sets season and episode. Also accept `S01E01E02` and take the first episode.
   - **NxNN:** `\b(\d{1,2})x(\d{2,3})\b` sets season and episode.
   - **Season pack:**
     - `\bS(\d{1,2})\b` sets season.
     - `\bSeason\s?(\d{1,2})\b` sets season.
     - `\bComplete\b` is only a cut point.
   - **Quality/source tags (cut points only):** `\b(2160p|1080p|720p|576p|480p|4k|uhd|bluray|blu ray|bdrip|brrip|web ?dl|webrip|web|hdtv|dvdrip|hdrip|remux|x264|x265|h ?264|h ?265|hevc|hdr|dv|proper|repack|extended|unrated|imax|multi|dual)\b`
   - **Year:** `\b(19\d\d|20\d\d)\b`, between 1900 and the current year + 1.
     - Take the **last** year that appears **before the first quality tag / SxxEyy** and **is not at position 0**.
     - "Blade Runner 2049 2017 1080p" gives 2017 and the title "Blade Runner 2049".
     - "1917 2019" gives 2019 and the title "1917".
     - "2012" alone has no year and the title "2012".
5. **Title.** It is the text before the earliest cut point. Trim trailing `-`, `–`, `—`, `:` and spaces. If it ends up empty, fall back to the whole normalised name.

### Unit-test table (must pass)

| input | title | year | S | E |
|---|---|---|---|---|
| `The.Matrix.1999.1080p.BluRay.x264-GRP` | The Matrix | 1999 | – | – |
| `Show.Name.S02E05.720p.WEB` | Show Name | – | 2 | 5 |
| `Show Name Season 3 Complete` | Show Name | – | 3 | – |
| `Sintel.2010.2160p.mkv` | Sintel | 2010 | – | – |
| `Big Buck Bunny (2008) 1080p` | Big Buck Bunny | 2008 | – | – |
| `Blade.Runner.2049.2017.1080p` | Blade Runner 2049 | 2017 | – | – |
| `1917.2019.2160p.UHD` | 1917 | 2019 | – | – |
| `The Office US S05 1080p` | The Office US | – | 5 | – |
| `Breaking.Bad.5x14.HDTV` | Breaking Bad | – | 5 | 14 |
| `The Daily Podcast – Season 3 Complete` | The Daily Podcast | – | 3 | – |
| `[SubsPlease] Frieren - 05 (1080p)` | Frieren - 05 is acceptable; anime is out of scope | | | |
| `ubuntu-24.04.1-desktop-amd64.iso` | (anything) | – | – | – |

The ubuntu row is skipped anyway because it has no video files (§5).

## 4. TMDB client

### Auth detection (`fn detect_auth(k:&str)->Auth`)
- A trimmed key that contains `.` or is longer than 40 characters is a **v4 Read Access Token**. It is a JWT starting `eyJ`, about 200+ characters. Send it as `Authorization: Bearer <token>`.
- Otherwise it is a **v3 API key** (32 hex characters). Append it as `api_key=<key>` to the query string.
- Unit-test both.

### Base URLs
- API: `https://api.themoviedb.org`. Keep it as a field on the `Tmdb` struct so tests can point it at a local mock server.
- Images: `https://image.tmdb.org/t/p/{size}{path}`.

### Endpoints

| purpose | request |
|---|---|
| validate key | `GET /3/authentication`. 200 `{"success":true}` means OK; 401 means invalid. |
| search movie | `GET /3/search/movie?query=..&year=YYYY&language=tr-TR&include_adult=false` |
| search tv | `GET /3/search/tv?query=..&first_air_date_year=YYYY&language=tr-TR` (send the year only when there is no season) |
| search any (manual, type unknown) | `GET /3/search/multi?query=..&language=tr-TR`. Keep only `media_type` in {movie, tv}. |
| movie details | `GET /3/movie/{id}?append_to_response=external_ids&language=tr-TR` |
| tv details | `GET /3/tv/{id}?append_to_response=external_ids&language=tr-TR` |
| episode | `GET /3/tv/{id}/season/{s}/episode/{e}?language=tr-TR`. Only when S and E are both known. A 404 is ignored. |

### Fields used

**Search result:**
- `id`
- `title` (movie) or `name` (tv)
- `release_date` (movie) or `first_air_date` (tv)
- `overview`
- `poster_path`
- `backdrop_path`
- `vote_average`
- `media_type` (multi only)

**Movie details:**
- `title`
- `release_date`
- `overview`
- `genres[].name`
- `runtime`
- `vote_average`
- `imdb_id` (or `external_ids.imdb_id`)
- `poster_path`
- `backdrop_path`

**TV details:**
- `name`
- `first_air_date`
- `overview`
- `genres[].name`
- `episode_run_time[0]`
- `vote_average`
- `external_ids.imdb_id`
- `poster_path`
- `backdrop_path`

**Episode:**
- `name` becomes `episode_title`
- `runtime` overrides runtime when it is present

### Language
Request `tr-TR`. If the resulting `overview` is empty, repeat the details call with `en-US` and take only its `overview`.

### Auto-match
1. If season is set, search **tv**. Otherwise search **movie** with the year.
2. If that returns nothing:
   - for a movie search with a year, retry the movie search without the year;
   - then try the other type.
3. Pick the first result whose year equals the parsed year (looking at the top 5). Otherwise pick result #0.
4. Then fetch details. The result of a search is at most 4 HTTP calls.

### Errors (`enum TmdbErr { Unauthorized, NotFound, Net(String) }`)
- `Unauthorized` becomes `NOT_READY: TMDB key rejected` during lookups, or `INVALID_INPUT: TMDB rejected this key` in `set_tmdb_key`.
- `Net` becomes `INTERNAL: ...`. **Never record a network error as a negative result.**

### Parsing in pure functions so they can be tested on canned JSON
- `fn info_from_movie(v:&Value) -> MediaInfo`
- `fn info_from_tv(v:&Value, ep:Option<&Value>, s,e) -> MediaInfo`
- `fn infos_from_search(v:&Value, kind:Option<MediaType>) -> Vec<MediaInfo>`

### Concurrency
- MetaState holds a `tokio::sync::Semaphore(2)` around network lookups.
- After acquiring it, re-check the in-memory map in case another call finished the same key.

## 5. Where metadata lives

### Locating a torrent (`fn locate(st,key) -> Result<Loc,String>`)
1. `engine::list_rows(st)` → find the row by key.
   - If the row is missing → `NOT_FOUND`.
   - If `output_path` is null (still resolving) → `NOT_READY`.
2. `engine::files(st,key)` → `Vec<FileRow>` (`NOT_READY` while resolving).
3. Interpret the result. Engine semantics: `output_path` is the **file itself** for single-file torrents and the **torrent folder** for multi-file ones.
   - **Single-file** (`files.len()==1`): `dir = output_path.parent()`, `stem = file_stem(output_path)`. "Exists" means the file exists.
   - **Multi-file:** `dir = output_path`, `stem = None`. "Exists" means `dir.is_dir()`.
4. Also return:
   - `has_video`: any `kind=="video"`
   - `torrent_files`: a lowercase set of `FileRow.name`
   - `name`: `row.name`

### Sidecar names

| | multi-file (folder) | single-file (next to file in ~/Downloads) |
|---|---|---|
| our data | `tide.json` | `<stem>.tide.json` |
| poster (TMDB w500) | `poster.jpg` | `<stem>-poster.jpg` |
| backdrop (TMDB w1280) | `fanart.jpg` | `<stem>-fanart.jpg` |
| Kodi nfo | `movie.nfo` or `tvshow.nfo` | `<stem>.nfo` |

### Collision rule (important)
A torrent may itself contain `poster.jpg` or `movie.nfo`. Overwriting it would corrupt that torrent file, and librqbit would fail its piece hashes.
- **Never write a sidecar whose relative name (lowercased) is in `torrent_files`.** Skip that one sidecar silently.
- If `tide.json` itself collides, which is extremely unlikely, keep the data in memory plus the index only.

### Writing
- Write sidecars only when the location "exists" (the metadata is resolved and librqbit has created the folder or file).
- Write images and the nfo first. Write `tide.json` last and atomically (write `.tide.json.tmp`, then rename), so `tide.json` present means complete.
- A failed write (permissions, disk) is logged with `tracing::warn` and does not fail the command. The info is still returned and kept in memory.
- librqbit ignores extra files in the torrent folder, so writing there is safe.

### `tide.json` shape
```json
{ "tide": 1, "key": "<infohash>", "torrent_name": "...", "manual": false, "fetched_at": 1760550000,
  "poster_path": "/abc.jpg", "backdrop_path": "/def.jpg",
  "info": { ...MediaInfo... } }
```
- When reading, accept the file even if `key` differs. Log it.
- Unknown or missing fields → serde defaults. Put `#[serde(default)]` on MediaInfo.

### Kodi nfo (minimal; XML-escape `& < > " '`)

**Movie** (`movie.nfo` or `<stem>.nfo`):
```xml
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<movie>
  <title>The Matrix</title>
  <year>1999</year>
  <plot>...</plot>
  <runtime>136</runtime>
  <genre>Action</genre><genre>Science Fiction</genre>
  <ratings><rating name="themoviedb" max="10" default="true"><value>8.2</value></rating></ratings>
  <uniqueid type="tmdb" default="true">603</uniqueid>
  <uniqueid type="imdb">tt0133093</uniqueid>
</movie>
```
- **TV folder** (`tvshow.nfo`): same as the movie nfo with root `<tvshow>`, plus `<premiered>YYYY-MM-DD</premiered>`.
  - On a type change, delete the other nfo file (`movie.nfo` ↔ `tvshow.nfo`).
- **Single-file TV episode** (`<stem>.nfo`): root `<episodedetails>` with these children:
  - `<title>` (the episode title, or `Episode N`)
  - `<showtitle>`
  - `<season>`
  - `<episode>`
  - `<plot>`
  - Omit `uniqueid` here: the episode id is not the show id.

### Negative-result index
- Location: `data_dir/metadata.json`, as `{ "misses": { "<key>": <unix_secs> } }`.
- Write it only on "no match" (a search with zero usable results). Never write it on a network error. Never write sidecars for a miss.
- Retry after 24 h.
- `set_media_match` removes the entry for that key.

### Settings
- Stored in `data_dir/settings.json` as `{ "tmdb_key": "..." }`.
  - Read and write it as a `serde_json::Map` so that settings other features add later are preserved.
  - Write it with mode 0600.
- `data_dir = ~/Library/Application Support/com.musahinist.tide`. **Never in the repo.**
- The key is never returned to the UI. Only `tmdb_key_set` is.

### MetaState (`#[derive]` is impossible because of the Semaphore → `impl Default` by hand)
```rust
pub struct MetaState {
  inner: std::sync::Mutex<Inner>,          // loaded lazily from data_dir on first use
  gate: tokio::sync::Semaphore,            // 2 concurrent TMDB lookups
  images: std::sync::Mutex<VecDeque<((String,ImgKind), Arc<String>)>>, // data-URL LRU, cap ~16
  http: std::sync::OnceLock<reqwest::Client>,
}
struct Inner { loaded: bool, tmdb_key: Option<String>, misses: HashMap<String,u64>, infos: HashMap<String,MediaInfo> }
```
- Never hold the std Mutex across `.await`.

## 6. Lookup order (`get_media_info`)
1. If the in-memory `infos[key]` is present, return it.
2. `locate()`. On `NOT_READY` or `NOT_FOUND`, pass the error through.
3. If the sidecar `tide.json` exists and parses, load it into memory and return it. **This works without an API key.**
4. If there is no video file in the torrent → `Ok(None)`. This makes no network call.
5. If no API key is set → `Err("NOT_READY: no TMDB key")`.
6. If the location doesn't exist yet → `Err("NOT_READY: waiting for files")`.
7. If there is a miss younger than 24 h → `Ok(None)`.
8. Acquire the semaphore, re-check memory, then parse the name and auto-match.
   - **Match:** download the poster and backdrop, write the sidecars with `manual=false`, store in memory, return `Some`.
   - **No match:** record the miss and return `Ok(None)`.
   - **Error:** `Err` (`INTERNAL` or `NOT_READY`).

## 7. IPC (types in types.rs)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType { #[default] Movie, Tv }

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaInfo {
  pub tmdb_id: u64, pub media_type: MediaType, pub title: String, pub year: Option<u16>,
  pub overview: String,
  pub poster_url: Option<String>,   // https://image.tmdb.org/t/p/w300{path}  (remote; used by search tiles)
  pub backdrop_url: Option<String>, // https://image.tmdb.org/t/p/w780{path}
  pub rating: Option<f64>,          // vote_average, None when 0
  pub genres: Vec<String>, pub runtime_min: Option<u32>,
  pub imdb_id: Option<String>, pub imdb_url: Option<String>, // https://www.imdb.com/title/{imdb_id}/
  pub season: Option<u32>, pub episode: Option<u32>, pub episode_title: Option<String>,
  pub manual: bool,                 // mirrors tide.json
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings { pub tmdb_key_set: bool }
```

### Commands

| command | JS args | returns | errors |
|---|---|---|---|
| `get_settings` | – | `{tmdb_key_set}` | – |
| `set_tmdb_key` | `{key}` (empty string clears it) | `{tmdb_key_set}` | `INVALID_INPUT` if TMDB returns 401, `INTERNAL` if there is no network |
| `get_media_info` | `{key}` | `MediaInfo \| null` | `NOT_READY` (resolving / no key / waiting for files), `NOT_FOUND`, `INTERNAL` |
| `get_media_image` | `{key, kind:"poster"\|"backdrop"}` | `"data:image/jpeg;base64,..."` | `NOT_FOUND` if no sidecar image |
| `search_media` | `{query, year:number\|null, mediaType:"movie"\|"tv"\|null}` | `MediaInfo[]` (top 8; no genres or imdb) | `INVALID_INPUT` for an empty query, `NOT_READY` if there is no key |
| `set_media_match` | `{key, tmdbId, mediaType}` | `MediaInfo` | `NOT_READY` if the location doesn't exist yet |
| `clear_media_match` | `{key}` | `null` | – |

**Image serving:**
- `get_media_image` reads the sidecar jpg from disk, base64-encodes it, and keeps it in the LRU.
- Images are never served from image.tmdb.org for matched rows.
- Alternative: enable tauri feature `protocol-asset` with an `assetProtocol` scope of `$HOME/Downloads/**`, then return a path and use `convertFileSrc`. This touches tauri features and the config, so a data URL is preferred.

**`set_media_match`:**
- Fetches details for the given id and type.
- Keeps the S/E parsed from the torrent name.
- Downloads the images and **rewrites all sidecars** with `manual=true`.
- Deletes the stale other-type nfo.
- Invalidates the image LRU for the key.
- Removes the miss.

**`clear_media_match`:** deletes the sidecars (via `sidecar_paths`) and clears memory, the LRU and the miss. The next `get_media_info` auto-matches again.

**`pub fn sidecar_paths(st:&AppState, key:&str) -> Vec<PathBuf>`:**
- Covers every sidecar name for both nfo variants that **exists on disk** and is **not** a torrent-owned file.
- Must work without an API key and without anything in memory.

## 8. UI (keep the WebTorrent-Desktop minimalism)

### Top strip
- A gear icon button (inline SVG, same `.icon` style) placed left of "+".
- It toggles a small popover/sheet titled "TMDB API key". The sheet contains:
  - a password-type input (placeholder `•••• saved` if set)
  - a Save button
  - a muted link "Get a free key", which calls `open_external("https://www.themoviedb.org/settings/api")`
  - when no key is set, one subtle muted line: "Add a key to show posters and details."
- Esc closes it. A save error shows inline in `--err`.
- There is no nagging anywhere else.

### Row with MediaInfo
- **Card background:**
  - The backdrop data URL (or the poster if there is no backdrop) goes on an absolutely-positioned `.art` layer behind `.head`.
  - Styles: `background-size: cover; background-position: center 30%; opacity: .28; filter: saturate(.8)`.
  - Add a `mask-image: linear-gradient(to right, transparent, #000 25%, #000 75%, transparent)` feather so the edges blur into `--row`.
  - Text stays readable. The hover gradient of `.ctl` must still cover it.
- **Title line:**
  - Movie: `Title (Year)`.
  - TV: `Show · S02E05 · Episode title` (with two-digit padding). For a season pack: `Show · Season 3`.
  - The torrent name moves to the `title` tooltip.
- **Stats line:** append ` · ★ 7.8` in muted text when a rating is present.
- **A row without info renders exactly as now.**

### Expanded row
The block sits above the file list:
- **Layout:** a poster thumbnail (about 64×96, rounded 4px) on the left.
- **Right side:**
  - overview (3-line clamp, `-webkit-line-clamp:3`, `#bbb`)
  - a muted meta line: `Drama, Sci-Fi · 2 h 16 min`
  - the links "IMDb ↗" (`open_external(imdb_url)`) and a tiny "Wrong match?"

**"Wrong match?"** reveals an inline search box:
- It is prefilled with the parsed title (or the current title).
- Enter calls `search_media`.
- Results show as a horizontal row of small poster tiles (remote `poster_url` w300, about 54×80, with title and year below).
- Clicking a tile calls `set_media_match`, then the row re-renders and the images reload.
- Also add a "Remove match" text link that calls `clear_media_match`.

### Lazy fetch scheduler in app.js
- Track `metaState: Map<key, {status:'pending'|'done'|'none'|'wait', info, art}>`.
- When `render()` sees a row with `output_path` that is not in the map:
  - queue it with a max of 2 concurrent `get_media_info` calls;
  - never await inside `render` (1 Hz updates must not block).
- **Done:** fetch `get_media_image(backdrop)`, falling back to the poster, then decorate the row.
- **On `NOT_READY`:**
  - if the key is set (from `get_settings` at boot), retry that key after 20 s;
  - if no key is set, mark it `'wait'` and re-queue all `'wait'` keys after a key is saved.
- **On other errors:** mark it `'none'` (no retry this session). Show no toast; metadata is silent.

### Mock mode
- Add a mock `get_settings` (key set), `get_media_info` and `get_media_image`.
- Use data URLs or local gradient SVGs as stand-in art, so it works offline in headless Chrome.
- Add `search_media`, `set_media_match` and `clear_media_match`.
- Provide MediaInfo for two rows:
  - Big Buck Bunny (2008, movie, ★ 6.4, Animation/Comedy, 10 min, tt1254207)
  - Sintel (2010, movie, ★ 6.9, Animation/Fantasy, 15 min, tt1727587)
- Add a preview hash flag `#meta`, which expands row 1 and opens the settings panel, alongside the existing `#open` and `#hover` flags.

### Screenshot check
```
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless --screenshot=<scratchpad>/wp6/x.png --window-size=960,620 file:///Users/mshn/Documents/downloader/ui/index.html#open
```
Iterate until a decorated row and a plain row sit side by side without looking out of place.

## 9. Frozen-file edits and integration (exact)
- **types.rs:** append the §7 types.
- **state.rs:**
  - add the field `pub meta: crate::metadata::MetaState,` (with a doc comment "WP6; Default");
  - add `meta: Default::default(),` in `init()`'s struct literal.
- **main.rs:**
  - add `mod metadata;`;
  - append to `generate_handler!`:
    ```
    // metadata (WP6)
    metadata::get_settings, metadata::set_tmdb_key, metadata::get_media_info, metadata::get_media_image,
    metadata::search_media, metadata::set_media_match, metadata::clear_media_match,
    ```
- **engine.rs (owner's job, later):**
  - In `remove_torrent`, when `delete_files` is true, collect `crate::metadata::sidecar_paths(&st, &key)` **before** forgetting the torrent (it needs the row and file list), then `std::fs::remove_file` each one after librqbit deletes the files.
  - librqbit only deletes torrent-owned files, so a multi-file folder would otherwise be left behind holding just the sidecars. After removing them, try `std::fs::remove_dir(folder)` (non-recursive; it only succeeds when the folder is empty).
  - Also call `metadata::forget(st,key)` (pub fn) to drop the memory/LRU/miss entries.
  - Without `deleteFiles`, leave the sidecars on disk. Re-adding the torrent reuses them.
- **Cargo.toml:** see §2.
- **No change** to `player.rs`, `subtitles.rs` or `tauri.conf.json` (unless the asset protocol is chosen instead of data URLs).

## 10. Tests (`cargo test`, in metadata.rs `#[cfg(test)]`)
1. The name-parser table in §3.
2. `detect_auth`: a 32-hex key gives ApiKey; `eyJhbGciOi...x.y.z` gives Bearer.
3. Pure JSON mappers on canned TMDB JSON (store small fixtures inline as `r#"..."#`):
   - `search/movie`, `search/tv`, `search/multi` (with a person entry filtered out)
   - `movie/603?append_to_response=external_ids`
   - `tv/1396` + `season/5/episode/14`
   - `vote_average: 0` gives `rating: None`
   - a missing `poster_path` gives `None`
   - check the image URL sizes (w300/w780 in MediaInfo; w500/w1280 downloads)
4. Auto-match selection: the year-preferring pick, falling back to #0.
5. **Mock HTTP server test** (`#[tokio::test]`):
   - Use a tiny `tokio::net::TcpListener` on 127.0.0.1:0 that answers by path prefix with canned JSON, 401 for `/3/authentication?api_key=bad`, and 404 for unknown paths.
   - Point `Tmdb.base` at it and run validate, auto-match and the tr→en overview fallback (the tr-TR response has an empty overview).
   - Assert that the Bearer header vs `api_key` query param is sent.
6. Sidecars in a `std::env::temp_dir()` subfolder:
   - Multi- and single-file names.
   - The collision rule: if a torrent file named `poster.jpg` exists, it is not written and not listed in `sidecar_paths`.
   - The atomic `tide.json`.
   - nfo XML escaping (`Tom & Jerry <3>`).
   - The movie→tv switch removes `movie.nfo`.
7. Negative cache: a miss within 24 h gives `None` without a request; after 24 h (inject a clock `now` param) it retries.

Then run:
```
source ~/.cargo/env; cd src-tauri && cargo check && cargo test
```
The target dir is shared: lock waits are normal, and never run `cargo clean`.

## 11. Risks and notes
- **reqwest `query` feature** is off in 0.13 (see §2). Using `.query()` without enabling it is a compile error.
- **Single-file torrents in ~/Downloads:** the sidecars are visible loose files in Downloads (`Sintel-poster.jpg` ...). This is accepted per the user's requirement, and they are cleaned on remove-with-delete once the engine hook exists.
- **Writing into a folder librqbit is still writing to** is safe. It never touches unknown files, and the file names are checked against the torrent's file list.
- **Wrong auto-matches** (generic titles, years missing) are expected. The "Wrong match?" fix is persisted as `manual=true` and never auto-overwritten.
- **TMDB rate limit** is about 50 req/s. The semaphore of 2, the at-most-4 calls per item, and the once-per-item design keep us far below it.
- **Key privacy:**
  - The key lives only in `settings.json` (0600), never in the repo or logs. Do not `tracing` request URLs when they contain `api_key`.
  - TMDB terms require attribution: add a tiny muted "Metadata from TMDB" line in the settings panel.
- **Large data URLs** (a w1280 backdrop is about 150–400 KB, about 30% more as base64). Keep the LRU small (about 16). The UI keeps one per visible row. That's fine for typical list sizes.
- **Offline/no key** is completely silent. Rows look exactly as today.
- **v2 (YouTube)** has no interaction. MediaInfo is keyed by torrent key only.
