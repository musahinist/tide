# Feature: archive to Mac mini (NEO Storage), user-approved 2026-10-16

## How the user works
- The MacBook Pro downloads, usually on café Wi-Fi.
- At home there is no Wi-Fi. Both Macs join the same Android hotspot, which is metered for internet but free for LAN traffic.
- The Mac mini is never on Wi-Fi by itself. It has an external SSD at `/Volumes/NEO Storage` (about 185 GB free).

## Behaviour
1. **Archive location (Mac mini only):**
   - Moved items go to `/Volumes/NEO Storage/Tidebox`.
   - Add a setting "Archive location". Default: `/Volumes/NEO Storage/Tidebox` if that volume exists, else none.
   - The Mac mini doesn't download, so only the archive location matters there.
2. **Auto-move (MacBook):**
   - Applies to completed torrents and completed yt-dlp items, including sidecars, online subtitles and posters.
   - Only runs when the paired archive Mac is online on the same LAN.
   - LAN traffic only, so it is allowed even when the network is metered.
   - Toggle "Move finished downloads to <device>": default ON once a device with an archive location is paired. A manual "Move to Mac mini" action also exists per item.
   - Transfer runs over the authenticated LAN server (remote.rs). It must be resumable and verified by size + SHA-256 per file.
   - Only after the receiver confirms does the MacBook forget the torrent and delete its local files.
   - On the receiver: re-add the torrent from its .torrent bytes with `output_folder` = archive dir, `paused` = true, completed (no seeding, no network), so it shows in the Mac mini's library with its metadata.
3. **MacBook view:**
   - Moved items keep showing via the synced remote library with the label "on Mac mini". Metadata, poster, watched state and resume stay visible.
   - ▶ streams the file from the mini. 📺 plays it on the mini.
4. **Auto-clean on the archive Mac:**
   - Setting "Auto-delete watched items after N days" (default 30, on for the archive device).
   - Runs on app start, then daily. Uses `cleanup.rs` logic with apply, without a confirmation dialog.
   - Shows a notification "Freed 6.4 GB (3 items)".
   - Deletions sync, so the item disappears from the MacBook's list too.
5. **Safety:**
   - Never delete a local copy before the remote copy is verified.
   - If NEO is not mounted, refuse the move and show "NEO Storage not connected".
   - Don't move items that are currently playing or have resume progress less than 1 min old.
