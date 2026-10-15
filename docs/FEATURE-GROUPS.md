# Feature: grouping (TV series), planned 2026-10-15

Depends on FEATURE-METADATA (MediaInfo: imdb_id, media_type, season, episode, episode_title).

## Automatic grouping
- **Group key:**
  1. If MediaInfo exists, use the series `imdb_id` (media_type = series).
  2. Otherwise use the normalised show title from `nameparse`, but only when S/E markers are present.
- **When a group exists:** a group needs at least 2 episodes across torrents, OR one torrent that contains at least 2 episode files (a season pack).
- **Episode rows** come from both sources:
  - one row per torrent (single-episode torrents);
  - one row per episode file inside a pack.
- **Group card:**
  - show poster (thumbnail plus blurred background, as rows have);
  - title;
  - "N episodes · S03E01–S03E06 · total size · aggregate ↓";
  - aggregate progress bar;
  - a "▶ SxxEyy" button that plays the first unwatched, playable episode.
- **Expanded group:**
  - season headers;
  - episodes sorted by (season, episode);
  - each episode shows its title, a watched ✓, its progress/state and per-episode actions (play, pause/resume, remove, reveal);
  - specials go under "Season 0 / Specials".
- **Watched flag:** comes from the player's resume data (more than 90% watched). Move it from localStorage to the backend, `data_dir/watched.json`, so the main window can read it. Add commands `get_watched` and `set_watched {key, fileIndex, watched}`.

## Manual groups and overrides
- Each row gets a ⋯ menu with these actions:
  - "Add to group…": pick an existing group or create a new one.
  - "Remove from group".
  - "Rename group".
- **Storage:** `data_dir/groups.json`, with this shape:

  ```json
  {groups:[{id, name, members:[key...], auto_key?:string}], excluded:{key: true}}
  ```

  - Manual membership overrides automatic grouping.
  - `excluded` stops a torrent from being auto-grouped.
- **Commands:**
  - `list_groups`
  - `create_group {name, keys}`
  - `rename_group {id, name}`
  - `add_to_group {id, key}`
  - `remove_from_group {key}`
  - `delete_group {id}`
- Deleting a group deletes neither the torrents nor their files.

## UI rules
- Grouping is computed in the frontend from rows + MediaInfo + groups.json. The backend only persists the data.
- Diff rendering must also keep group expansion state stable across the 1 Hz updates.
- Same minimal style as the rest of the app: hover-only controls, no extra chrome.
