'use strict';

// ---------- bridge (real Tauri or in-browser mock) ----------
const T = window.__TAURI__;
const invoke = T ? T.core.invoke : mockInvoke;
const listen = T ? T.event.listen : mockListen;

// ---------- helpers ----------
const $ = (id) => document.getElementById(id);
const h = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};

const ICONS = {
  play: '<path class="fill" d="M5 3.2v9.6c0 .5.5.8.9.5l7.3-4.8a.6.6 0 0 0 0-1L5.9 2.7c-.4-.3-.9 0-.9.5z"/>',
  pause: '<path d="M5.5 3.5v9M10.5 3.5v9"/>',
  resume: '<path d="M8 2.5v9M4 7.5l4 4 4-4M3.5 13.5h9"/>',
  remove: '<path d="M4 4l8 8M12 4l-8 8"/>',
  more: '<path class="fill" d="M3 8a1.2 1.2 0 1 0 2.4 0A1.2 1.2 0 0 0 3 8zm3.8 0a1.2 1.2 0 1 0 2.4 0 1.2 1.2 0 0 0-2.4 0zm3.8 0a1.2 1.2 0 1 0 2.4 0 1.2 1.2 0 0 0-2.4 0z"/>',
  screen: '<rect x="2" y="3" width="12" height="8" rx="1.2"/><path d="M6 13.5h4M8 11v2.5"/>',
  folder: '<path d="M2 4.5c0-.6.4-1 1-1h3.2l1.5 1.5H13c.6 0 1 .4 1 1v6c0 .6-.4 1-1 1H3c-.6 0-1-.4-1-1z"/>',
};
const svg = (name) => `<svg viewBox="0 0 16 16">${ICONS[name]}</svg>`;
function iconBtn(name, title, onClick) {
  const b = h('button', 'icon');
  b.innerHTML = svg(name);
  b.title = title;
  b.setAttribute('aria-label', title);
  b.addEventListener('click', (e) => { e.stopPropagation(); onClick(); });
  return b;
}

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];
function bytes(n) {
  let i = 0;
  while (n >= 1000 && i < UNITS.length - 1) { n /= 1000; i++; }
  return `${i === 0 ? n : n < 10 ? n.toFixed(1) : Math.round(n)} ${UNITS[i]}`;
}
const speed = (bps) => `${bytes(Math.round(bps))}/s`;
function eta(s) {
  if (s < 60) return 'less than a minute left';
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min left`;
  const hr = Math.floor(m / 60);
  if (hr < 48) return `${hr} h ${m % 60} min left`;
  return `${Math.round(hr / 24)} days left`;
}
const pct = (p) => `${Math.floor(p * 100)}%`;
const plural = (n, w) => `${n} ${w}${n === 1 ? '' : 's'}`;

function statsLine(t) {
  switch (t.state) {
    case 'resolving': return 'Getting metadata…';
    case 'error': return t.error || 'Error';
    case 'paused': return t.total_bytes ? `Paused · ${pct(t.progress)} · ${bytes(t.progress_bytes)} of ${bytes(t.total_bytes)}` : 'Paused';
    case 'initializing': return `Checking files… ${pct(t.progress)}`;
    case 'completed': return `Completed · ${bytes(t.total_bytes)}`;
    default: {
      const parts = [pct(t.progress), `${bytes(t.progress_bytes)} of ${bytes(t.total_bytes)}`, `↓ ${speed(t.down_bps)}`];
      if (t.up_bps > 1000) parts.push(`↑ ${speed(t.up_bps)}`);
      parts.push(plural(t.peers, 'peer'));
      if (t.eta_secs != null && t.down_bps > 0) parts.push(eta(t.eta_secs));
      return parts.join(' · ');
    }
  }
}

const errCode = (e) => String(e).split(':')[0];
const errMsg = (e) => { const s = String(e); const i = s.indexOf(': '); return i > 0 ? s.slice(i + 2) : s; };

// ---------- toast ----------
let toastTimer;
function toast(msg, action) {
  const act = $('toast-act');
  $('toast-msg').textContent = msg;
  act.hidden = !action;
  if (action) { act.textContent = action.label; act.onclick = () => { action.run(); hideToast(); }; }
  $('toast').classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(hideToast, action ? 8000 : 4000);
}
function hideToast() { $('toast').classList.remove('show'); }

function reportError(e) {
  if (errCode(e) === 'VLC_NOT_FOUND') {
    toast('This file needs VLC to play.', {
      label: 'Get VLC',
      run: () => invoke('open_external', { url: 'https://www.videolan.org/vlc/' }).catch(reportError),
    });
  } else {
    toast(errMsg(e));
  }
}

// ---------- adding magnets ----------
const isMagnet = (s) => /^magnet:/i.test(s) || /^[0-9a-f]{40}$/i.test(s);
const isWebUrl = (s) => /^https?:\/\//i.test(s.trim());

async function addMagnet(raw) {
  const magnet = raw.trim();
  if (!isMagnet(magnet)) throw 'INVALID_INPUT: Not a magnet link';
  const r = await invoke('add_magnet', { magnet });
  if (r && r.already_exists) toast('Already in your list');
  refresh();
}

function openAdd() {
  document.body.classList.add('adding');
  $('magnet').focus();
}
function closeAdd() {
  document.body.classList.remove('adding');
  $('magnet').value = '';
  setAddError('');
  $('magnet').blur();
}
function setAddError(msg) {
  $('add-err').textContent = msg;
  $('add').classList.toggle('invalid', !!msg);
}

$('plus').addEventListener('click', () => (document.body.classList.contains('adding') ? closeAdd() : openAdd()));
$('magnet').addEventListener('input', () => setAddError(''));
$('add').addEventListener('submit', async (e) => {
  e.preventDefault();
  const v = $('magnet').value;
  if (!v.trim()) return;
  if (isWebUrl(v) && window.YT) { closeAdd(); window.YT.openProbeDialog(v.trim()); return; }
  try { await addMagnet(v); closeAdd(); } catch (err) { setAddError(errMsg(err)); }
});

document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  if (!menu.hidden) closeMenu();
  else if (!$('modal').hidden) closeConfirm();
  else if (document.body.classList.contains('adding')) closeAdd();
  else if (openKey) closePanel();
});

// ⌘V anywhere (outside text fields) adds a magnet straight from the clipboard.
document.addEventListener('paste', (e) => {
  if (e.target.closest && e.target.closest('input, textarea')) return;
  const text = (e.clipboardData && e.clipboardData.getData('text')) || '';
  e.preventDefault();
  if (isWebUrl(text) && window.YT) return window.YT.openProbeDialog(text.trim());
  if (!isMagnet(text.trim())) return toast('No magnet link on the clipboard');
  addMagnet(text).catch(reportError);
});

// Dropped magnet text (when the webview lets it through).
document.addEventListener('dragover', (e) => e.preventDefault());
document.addEventListener('drop', (e) => {
  e.preventDefault();
  const text = (e.dataTransfer.getData('text/uri-list') || e.dataTransfer.getData('text') || '').trim();
  if (text) addMagnet(text.split('\n')[0]).catch(reportError);
});

// ---------- remove confirm ----------
let confirmKey = null;
function askRemove(t) {
  confirmKey = t.key;
  $('confirm-name').textContent = `“${t.name}”`;
  $('confirm-delete').checked = false;
  $('modal').hidden = false;
  $('confirm').querySelector('.danger').focus();
}
function closeConfirm() { $('modal').hidden = true; confirmKey = null; }
$('confirm-cancel').addEventListener('click', closeConfirm);
$('modal').addEventListener('click', (e) => { if (e.target === $('modal')) closeConfirm(); });
$('confirm').addEventListener('submit', (e) => {
  e.preventDefault();
  const key = confirmKey, deleteFiles = $('confirm-delete').checked;
  closeConfirm();
  invoke('remove_torrent', { key, deleteFiles }).then(refresh, reportError);
});

// ---------- playback ----------
// Unknown command (backend older than this UI) -> fall back quietly.
const missingCmd = (e) => /^command \S+ not found|unknown command|NOT_IMPLEMENTED|not allowed/i.test(String(e && e.message ? e.message : e));
function play(key, fileIndex) {
  const ro = remoteOnly(key);
  if (ro) {
    return invoke('remote_play_local', { deviceId: ro.dev.id, key, fileIndex })
      .catch((e) => (missingCmd(e) ? toast(`This Tide can’t play from ${ro.dev.name} yet`) : playError(e)));
  }
  return invoke('play_media', { media: { kind: 'torrent', key, fileIndex } }).catch(playError);
}
// Files of a torrent: local backend, or the other Mac's list from remote_library.
const listFiles = (key) => { const ro = remoteOnly(key); return ro ? Promise.resolve(ro.files) : invoke('list_files', { key }); };
function playError(e) {
  if (errCode(e) === 'NOT_READY') toast('Still checking files…'); else reportError(e);
}
// Resume refuses finished torrents ("COMPLETED: …"); that is not worth a toast.
const resumeT = (key) => invoke('resume_torrent', { key }).then(refresh, (e) => (errCode(e) === 'COMPLETED' ? refresh() : reportError(e)));
const pauseT = (key) => invoke('pause_torrent', { key }).then(refresh, reportError);

// Index of the largest video/audio file, or null (after a toast).
async function largestIdx(key) {
  try {
    const media = (await listFiles(key)).filter((f) => f.kind === 'video' || f.kind === 'audio');
    if (!media.length) { toast('No video or audio in this torrent'); return null; }
    const f = media.reduce((a, b) => (b.size > a.size ? b : a));
    mainFile.set(key, f.index);
    return f.index;
  } catch (e) { playError(e); return null; }
}
async function playLargest(key) {
  const idx = await largestIdx(key);
  if (idx != null) play(key, idx);
}

// ---------- play on another Mac ----------
let remote = null; // RemoteStatus
const onlineDevices = () => ((remote && remote.devices) || []).filter((d) => d.online);
function remotePlay(dev, key, fileIndex) {
  invoke('remote_play', { deviceId: dev.id, media: { kind: 'torrent', key, fileIndex } })
    .then(() => toast(`Playing on ${dev.name}`), reportError);
}
// Screen button next to a ▶; `target` resolves to [key, fileIndex] (or null). `keyOf`
// (sync) names the torrent: one stored only on another Mac plays there, on its owner.
function remoteBtn(target, keyOf) {
  const owner = () => { const k = keyOf && keyOf(); const ro = k && remoteOnly(k); return ro ? ro.dev : null; };
  const b = iconBtn('screen', 'Play on another Mac', async () => {
    const own = owner();
    if (own) { closeMenu(); const t = await target(); if (t && t[1] != null) remotePlay(own, t[0], t[1]); return; }
    const devs = onlineDevices();
    if (!devs.length) return;
    const go = async (d) => { closeMenu(); const t = await target(); if (t && t[1] != null) remotePlay(d, t[0], t[1]); };
    if (devs.length === 1) return go(devs[0]);
    menu.replaceChildren(h('div', 'mhead', 'Play on'), ...devs.map((d) => menuItem(d.name, () => go(d))));
    menu.hidden = false;
    const a = b.getBoundingClientRect();
    menu.style.top = `${Math.min(a.bottom + 4, innerHeight - menu.offsetHeight - 8)}px`;
    menu.style.left = `${Math.max(8, a.right - menu.offsetWidth)}px`;
  });
  b.classList.add('rbtn');
  b.addEventListener('mouseenter', () => {
    const own = owner();
    if (own) { b.title = `Play on ${own.name}`; return; }
    const devs = onlineDevices();
    b.title = devs.length === 1 ? `Play on ${devs[0].name}` : 'Play on another Mac';
  });
  return b;
}

// ---------- other Macs' libraries (merged into the list and the library) ----------
// remote_library: per paired Mac its rows + media info + files. Torrents only on another
// Mac join the list as rows with `t.remote = { id, name, online }`; a local copy wins, so
// items on both Macs show once. Refreshed on `sync-updated` and every 30 s.
let localList = []; // last list from this Mac's backend
let remoteRowsCache = []; // other Macs' rows (with .remote), rebuilt per remote_library
const remoteByKey = new Map(); // key -> { dev, media, files }
let localKeys = new Set();
const remoteOnly = (key) => (localKeys.has(key) ? null : remoteByKey.get(key) || null);
const shortDev = (name) => String(name || 'another Mac').replace(/^.*?['’]s\s+/, '');
let remoteLibOk = true;
function loadRemoteLib() {
  if (!remoteLibOk) return Promise.resolve();
  return invoke('remote_library').then((libs) => {
    remoteByKey.clear();
    remoteRowsCache = [];
    for (const d of Array.isArray(libs) ? libs : []) {
      const dev = { id: d.device_id, name: d.device_name || 'another Mac', online: !!d.online };
      for (const t of d.rows || []) {
        if (!t || !t.key || remoteByKey.has(t.key)) continue;
        remoteByKey.set(t.key, { dev, media: (d.media || {})[t.key] || null, files: (d.files || {})[t.key] || [] });
        remoteRowsCache.push({ ...t, remote: dev });
      }
    }
    render(localList);
  }, (e) => { if (missingCmd(e)) remoteLibOk = false; });
}
function mergeRemote(list) {
  localKeys = new Set(list.map((t) => t.key));
  const extra = remoteRowsCache.filter((t) => !localKeys.has(t.key));
  return extra.length ? list.concat(extra) : list;
}

// ---------- rows (diffed by key) ----------
const rows = new Map(); // key -> { el, head, name, statsText, fill, buttons, panel, details, last }
let openKey = null; // the one expanded row

const pad2 = (n) => String(n).padStart(2, '0');
const infoOf = (key) => { const m = meta.get(key); return m ? m.info : null; };
const matched = (m) => !!(m && !m.needs_match && m.title);

function mediaTitle(t, m) {
  if (!matched(m)) return t.name;
  if (m.media_type === 'tv') {
    const parts = [m.title];
    if (m.season != null && m.episode != null) parts.push(`S${pad2(m.season)}E${pad2(m.episode)}`);
    else if (m.season != null) parts.push(`Season ${m.season}`);
    else if (m.air_date) parts.push(m.air_date);
    if (m.episode_title) parts.push(m.episode_title);
    return parts.join(' · ');
  }
  return m.year ? `${m.title} (${m.year})` : m.title;
}
const rating = (m) => (matched(m) && m.imdb_rating != null ? Number(m.imdb_rating).toFixed(1) : null);
const votes = (n) => (n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}K` : String(n));
function runtime(min) {
  if (!min) return null;
  return min < 60 ? `${min} min` : `${Math.floor(min / 60)} h${min % 60 ? ` ${min % 60} min` : ''}`;
}

function createRow(key) {
  const el = h('section', 'torrent');
  const head = h('div', 'head');
  const name = h('div', 'name');
  const stats = h('div', 'stats');
  const dot = h('span', 'dot');
  const statsText = h('span');
  const dev = h('span', 'devtag');
  dev.hidden = true;
  stats.append(dot, dev, statsText);
  const bar = h('div', 'bar');
  const fill = h('i');
  bar.append(fill);

  const r = { el, head, name, statsText, dev, fill, panel: null, details: null, last: null, artPri: 0 };
  const ctl = h('div', 'ctl');
  r.playBtn = iconBtn('play', 'Play', () => playLargest(key));
  r.pauseBtn = iconBtn('pause', 'Pause', () => pauseT(key));
  r.resumeBtn = iconBtn('resume', 'Resume', () => resumeT(key));
  r.revealBtn = iconBtn('folder', 'Show in Finder', () => invoke('reveal_torrent', { key }).catch(reportError));
  r.moreBtn = iconBtn('more', 'More', () => openMenu(r.moreBtn, { key }));
  r.removeBtn = iconBtn('remove', 'Remove', () => askRemove(r.last));
  ctl.append(r.playBtn, remoteBtn(async () => { const i = await largestIdx(key); return [key, i]; }, () => key), r.pauseBtn, r.resumeBtn, r.revealBtn, r.removeBtn, r.moreBtn);

  r.art = h('div', 'art');
  r.check = h('button', 'wcheck', '✓');
  r.check.type = 'button';
  r.check.addEventListener('click', (e) => { e.stopPropagation(); toggleWatched(key); });
  head.append(r.art, r.check, name, stats, ctl, bar);
  head.addEventListener('click', () => toggle(key));
  el.append(head);
  return r;
}

// "on Mac mini" label for items stored on another Mac (rem = { name, online } or null).
function setDevTag(el, rem) {
  const txt = rem ? `on ${shortDev(rem.name)}` : '';
  if (el.textContent !== txt) el.textContent = txt;
  el.hidden = !rem;
  el.title = rem ? `Stored on ${rem.name}${rem.online ? '' : ' (offline)'}` : '';
  el.classList.toggle('off', !!rem && !rem.online);
}

function updateRow(r, t) {
  const p = r.last || {};
  const m = infoOf(t.key);
  const title = r.group ? episodeTitle(t, m) : mediaTitle(t, m);
  const w0 = isWatched(t.key);
  if (r.watched !== w0) { r.watched = w0; r.check.classList.toggle('on', w0); r.check.title = w0 ? 'Watched — click to unmark' : 'Mark as watched'; }
  if (r.name.textContent !== title) r.name.textContent = title;
  if (p.name !== t.name) r.name.title = t.name;
  const rem = t.remote || null;
  const cls = `torrent s-${t.state}${t.key === openKey ? ' open' : ''}${r.group ? ' ep' : ''}${r.pack ? ' pack' : ''}${rem ? ' remote' : ''}`;
  setDevTag(r.dev, rem);
  if (r.el.className !== cls) r.el.className = cls;
  const rt = rating(m);
  const line = statsLine(t) + (rt ? ` · IMDb ${rt}` : '');
  if (r.statsText.textContent !== line) r.statsText.textContent = line;
  const w = `${(t.state === 'resolving' ? 0 : t.progress * 100).toFixed(2)}%`;
  if (r.fill.style.width !== w) r.fill.style.width = w;

  const ready = t.state !== 'resolving' && t.id != null;
  r.playBtn.hidden = !ready || t.state === 'error' || t.state === 'initializing';
  r.pauseBtn.hidden = !!rem || !(t.state === 'downloading' || t.state === 'initializing');
  r.resumeBtn.hidden = !!rem || !(t.state === 'paused' || (t.state === 'error' && ready));
  r.revealBtn.hidden = !!rem || !t.output_path;
  r.removeBtn.hidden = !!rem;
  r.last = t;
  wantArtwork(t);
  wantMeta(t);
}

// ---------- groups (computed here; the backend only persists groups.json / watched.json) ----------
// Group identity: show "nodes" per torrent — `title:<norm>` from the offline name parse
// (instant) and `imdb:<id>` once MediaInfo arrives — unioned into one component. The group
// id is the component's smallest node, so imdb wins once known; when the id changes, the
// card (and its expansion) is carried over by member overlap, so nothing flickers.
let groupsData = { groups: [], excluded: {} };
let watched = {}; // "key:fileIndex" -> true
let watchedKeys = new Set(); // torrent keys with any watched file
const mainFile = new Map(); // key -> index of the main media file (largest video/audio)
const groupEls = new Map(); // gid -> group card
const fileEls = new Map(); // "key:fileIndex" -> episode-file row inside a group
const expanded = new Set(); // gids of expanded group cards (survives re-renders)
let lastList = [];
let lastGroups = new Map(); // gid -> { gid, id, name, keys, autoKey }

const code = (s, e) => (s == null ? `E${pad2(e)}` : `S${pad2(s)}E${pad2(e)}`);

// Offline name parse (parse_names): torrent name -> { title, norm, year, season, episode, is_tv } | null.
const parsed = new Map();
let parseBusy = false;
function wantParse(list) {
  if (parseBusy) return;
  const names = [...new Set(list.map((t) => t.name).filter((n) => !parsed.has(n)))];
  if (!names.length) return;
  parseBusy = true;
  invoke('parse_names', { names })
    .then((res) => names.forEach((n, i) => parsed.set(n, (res && res[i]) || null)),
      () => names.forEach((n) => parsed.set(n, null)))
    .finally(() => { parseBusy = false; render(lastList); });
}

// Episode files per torrent (list_episodes). ≥ 2 episodes = season pack.
// key -> { eps, busy, at, tries, next, state }. Live packs refresh every 2 s.
const packs = new Map();
function wantEpisodes(t) {
  if (t.state === 'resolving' || t.id == null) return;
  let p = packs.get(t.key);
  if (!p) packs.set(t.key, (p = { eps: null, busy: false, at: 0, tries: 0, next: 0, state: null }));
  const now = Date.now();
  if (p.busy || now < p.next) return;
  const live = t.state === 'downloading' || t.state === 'initializing';
  if (p.eps && p.state === t.state && !(live && p.eps.length > 1 && now - p.at > 2000)) return;
  p.busy = true;
  episodesOf(t.key).then((eps) => {
    const before = p.eps ? p.eps.map((e) => e.file_index).join() : null;
    Object.assign(p, { eps: eps || [], state: t.state, at: Date.now(), tries: 0 });
    if (before !== p.eps.map((e) => e.file_index).join()) render(lastList);
  }, (e) => {
    p.tries++;
    p.next = Date.now() + (errCode(e) === 'NOT_FOUND' ? 60e3 : Math.min(60e3, 3e3 * 2 ** (p.tries - 1)));
  }).finally(() => { p.busy = false; });
}
// Episode files of a torrent; for one stored on another Mac, parsed from its file list.
function episodesOf(key) {
  const ro = remoteOnly(key);
  if (!ro) return invoke('list_episodes', { key });
  const vids = ro.files.filter((f) => f.kind === 'video' && !/(^|\/)sample/i.test(f.name));
  if (!vids.length) return Promise.resolve([]);
  return invoke('parse_names', { names: vids.map((f) => f.name.split('/').pop()) }).then((ps) => vids
    .map((f, i) => ({ f, p: ps && ps[i] }))
    .filter(({ p }) => p && p.is_tv && p.episode != null)
    .map(({ f, p }) => ({ file_index: f.index, season: p.season, episode: p.episode, title_hint: p.title, norm: p.norm, size: f.size, progress: f.progress })));
}
const packOf = (key) => { const p = packs.get(key); return p && p.eps && p.eps.length > 1 ? p.eps : null; };

// Show nodes of a torrent (null = not a show episode).
function showNodes(t) {
  const m = infoOf(t.key);
  if (matched(m) && m.media_type !== 'tv') return null;
  const nodes = [];
  if (matched(m) && m.imdb_id) nodes.push(`imdb:${m.imdb_id}`);
  const p = parsed.get(t.name);
  if (p && p.is_tv && p.norm && (p.season != null || p.episode != null)) nodes.push(`title:${p.norm}`);
  const eps = packOf(t.key);
  if (eps && eps[0].norm) nodes.push(`title:${eps[0].norm}`);
  return nodes.length ? [...new Set(nodes)] : null;
}
function showName(t) {
  const m = infoOf(t.key);
  if (matched(m)) return m.title;
  const p = parsed.get(t.name);
  if (p && p.title) return p.title;
  const eps = packOf(t.key);
  return eps ? eps[0].title_hint : t.name;
}
// Episode facts for a single-episode torrent: { season, episode, title }.
function epInfo(t) {
  const m = infoOf(t.key);
  if (matched(m) && m.media_type === 'tv') return { season: m.season, episode: m.episode, title: m.episode_title };
  const p = parsed.get(t.name);
  return p && p.is_tv ? { season: p.season, episode: p.episode, title: null } : {};
}
function episodeTitle(t) {
  if (packOf(t.key)) return t.name;
  const e = epInfo(t);
  if (e.episode == null) return t.name;
  return e.title ? `${code(e.season, e.episode)} · ${e.title}` : code(e.season, e.episode);
}

const isWatched = (key) => (mainFile.has(key) ? !!watched[`${key}:${mainFile.get(key)}`] : watchedKeys.has(key));
const playable = (t) => t.id != null && !['resolving', 'initializing', 'error'].includes(t.state);

function computeGroups(list) {
  const byKey = new Map(list.map((t) => [t.key, t]));
  // Union-find over show nodes; root = smallest node ("imdb:" sorts before "title:").
  const parent = new Map();
  const find = (x) => { while (parent.get(x) !== x) { parent.set(x, parent.get(parent.get(x))); x = parent.get(x); } return x; };
  const union = (a, b) => { a = find(a); b = find(b); if (a !== b) { if (a < b) parent.set(b, a); else parent.set(a, b); } };
  const nodesOf = new Map();
  for (const t of list) {
    const n = showNodes(t);
    if (!n) continue;
    nodesOf.set(t.key, n);
    n.forEach((x) => { if (!parent.has(x)) parent.set(x, x); });
    n.slice(1).forEach((x) => union(n[0], x));
  }
  const showKey = (key) => (nodesOf.has(key) ? find(nodesOf.get(key)[0]) : null);
  const weight = (key) => (packOf(key) || [0]).length;

  const out = new Map();
  const owner = new Map();
  const autoInto = new Map(); // show key -> manual gid that already holds an episode of that show
  for (const g of groupsData.groups) {
    const gid = `g:${g.id}`;
    out.set(gid, { gid, id: g.id, name: g.name, keys: [] });
    if (g.auto_key) autoInto.set(parent.has(g.auto_key) ? find(g.auto_key) : g.auto_key, gid);
    for (const k of g.members) {
      if (!byKey.has(k) || owner.has(k)) continue;
      owner.set(k, gid);
      const sk = showKey(k);
      if (sk && !autoInto.has(sk)) autoInto.set(sk, gid);
    }
  }
  const autos = new Map();
  for (const t of list) {
    if (owner.has(t.key) || groupsData.excluded[t.key]) continue;
    const sk = showKey(t.key);
    if (!sk) continue;
    if (!autos.has(sk)) autos.set(sk, { name: null, keys: [], eps: 0 });
    const a = autos.get(sk);
    a.keys.push(t.key);
    a.eps += weight(t.key);
    if (!a.name || matched(infoOf(t.key))) a.name = showName(t);
  }
  for (const [sk, a] of autos) {
    const into = autoInto.get(sk);
    if (into) { a.keys.forEach((k) => owner.set(k, into)); continue; }
    if (a.eps < 2) continue;
    const gid = `a:${sk}`;
    out.set(gid, { gid, id: null, name: a.name, keys: [], autoKey: sk });
    a.keys.forEach((k) => owner.set(k, gid));
  }
  for (const t of list) { const g = owner.get(t.key); if (g) out.get(g).keys.push(t.key); }
  return { groups: out, owner };
}

// A group whose id changed (name -> imdb, merge) keeps its card and expansion.
function carryOver(groups) {
  for (const [gid, gr] of groups) {
    if (groupEls.has(gid)) continue;
    for (const [old, G] of groupEls) {
      if (groups.has(old) || !gr.keys.some((k) => G.keys.includes(k))) continue;
      groupEls.delete(old);
      G.gid = gid;
      groupEls.set(gid, G);
      if (expanded.delete(old)) expanded.add(gid);
      break;
    }
  }
}

function render(list) {
  if (list !== lastList) localList = list || []; // render(lastList) = re-render, same local data
  list = mergeRemote(localList);
  lastList = list;
  wantParse(list);
  for (const t of list) wantEpisodes(t);
  const { groups, owner } = computeGroups(list);
  lastGroups = groups;
  carryOver(groups);
  const seen = new Set();
  for (const t of list) {
    seen.add(t.key);
    let r = rows.get(t.key);
    if (!r) { r = createRow(t.key); rows.set(t.key, r); }
    r.group = owner.get(t.key) || null;
    r.pack = !!(r.group && packOf(t.key));
    updateRow(r, t);
  }
  const container = $('list');
  const seenG = new Set();
  const seenF = new Set();
  let prev = cwEl; // "Continue watching" stays first
  const place = (el) => {
    const want = prev ? prev.nextSibling : container.firstChild;
    if (want !== el) container.insertBefore(el, want);
    prev = el;
  };
  for (const t of list) {
    const r = rows.get(t.key);
    if (!r.group) { place(r.el); continue; }
    if (seenG.has(r.group)) continue;
    seenG.add(r.group);
    let G = groupEls.get(r.group);
    if (!G) { G = createGroup(r.group); groupEls.set(r.group, G); }
    updateGroup(G, groups.get(r.group), seenF);
    place(G.el);
  }
  for (const [key, r] of rows) {
    if (!seen.has(key)) {
      r.el.remove(); rows.delete(key); art.delete(key); meta.delete(key); packs.delete(key);
      if (openKey === key) openKey = null;
    }
  }
  for (const [gid, G] of groupEls) if (!seenG.has(gid)) { G.el.remove(); groupEls.delete(gid); }
  for (const [fk, F] of fileEls) if (!seenF.has(fk)) { F.el.remove(); fileEls.delete(fk); }
  pumpArtwork();
  pumpMeta();
  $('empty').hidden = list.length > 0;
  if (openKey) updatePanel(openKey);
  renderContinue();
  if (view === 'library') renderLibrary();
}

function createGroup(gid) {
  const G = { gid, keys: [], el: h('section', 'torrent group'), head: h('div', 'head'), name: h('div', 'name'), seasons: new Map() };
  const stats = h('div', 'stats');
  G.statsText = h('span');
  stats.append(h('span', 'dot'), G.statsText);
  const bar = h('div', 'bar');
  G.fill = h('i');
  bar.append(G.fill);
  G.art = h('div', 'art');
  const ctl = h('div', 'gctl');
  G.nextBtn = h('button', 'gnext');
  G.nextBtn.type = 'button';
  G.nextBtn.addEventListener('click', (e) => { e.stopPropagation(); if (G.next) playItem(G.next); });
  const more = iconBtn('more', 'More', () => openMenu(more, { gid: G.gid }));
  more.classList.add('gmore');
  const gRemote = remoteBtn(async () => {
    const x = G.next;
    if (!x) return null;
    return [x.key, x.f ? x.f.file_index : await largestIdx(x.key)];
  }, () => G.next && G.next.key);
  ctl.append(G.nextBtn, gRemote, more);
  G.head.append(G.art, G.name, stats, ctl, bar);
  G.head.addEventListener('click', () => {
    if (expanded.has(G.gid)) expanded.delete(G.gid); else expanded.add(G.gid);
    render(lastList);
  });
  G.eps = h('div', 'eps');
  G.el.append(G.head, G.eps);
  return G;
}

// Episode-file row (one file of a season pack).
function createFileRow(key, idx) {
  const F = { el: h('section', 'torrent ep efile'), head: h('div', 'head'), name: h('div', 'name'), statsText: h('span'), dev: h('span', 'devtag'), fill: h('i') };
  F.dev.hidden = true;
  const stats = h('div', 'stats');
  stats.append(h('span', 'dot'), F.dev, F.statsText);
  const bar = h('div', 'bar');
  bar.append(F.fill);
  F.check = h('button', 'wcheck', '✓');
  F.check.type = 'button';
  F.check.addEventListener('click', (e) => { e.stopPropagation(); toggleFileWatched(key, idx); });
  const ctl = h('div', 'ctl');
  F.playBtn = iconBtn('play', 'Play', () => play(key, idx));
  ctl.append(F.playBtn, remoteBtn(() => [key, idx], () => key));
  F.head.append(F.check, F.name, stats, ctl, bar);
  F.el.append(F.head);
  return F;
}
function updateFileRow(F, x) {
  const { t, f, key } = x;
  const name = code(f.season, f.episode);
  if (F.name.textContent !== name) F.name.textContent = name;
  const done = f.progress >= 1;
  const line = done ? bytes(f.size) : `${pct(f.progress)} · ${bytes(f.size)}`;
  if (F.statsText.textContent !== line) F.statsText.textContent = line;
  const w = `${(f.progress * 100).toFixed(2)}%`;
  if (F.fill.style.width !== w) F.fill.style.width = w;
  const cls = `torrent ep efile s-${done ? 'completed' : t.state}${t.remote ? ' remote' : ''}`;
  setDevTag(F.dev, t.remote || null);
  if (F.el.className !== cls) F.el.className = cls;
  const on = !!watched[`${key}:${f.file_index}`];
  if (F.watched !== on) { F.watched = on; F.check.classList.toggle('on', on); F.check.title = on ? 'Watched — click to unmark' : 'Mark as watched'; }
  F.playBtn.hidden = !playable(t);
}

const itemWatched = (x) => (x.f ? !!watched[`${x.key}:${x.f.file_index}`] : isWatched(x.key));
const playItem = (x) => (x.f ? play(x.key, x.f.file_index) : playLargest(x.key));

function updateGroup(G, gr, seenF) {
  G.keys = gr.keys;
  // Items: one per single-episode torrent, one per episode file of a pack.
  const items = [];
  const packRows = [];
  for (const k of gr.keys) {
    const r = rows.get(k), t = r.last, eps = packOf(k);
    if (!eps) { items.push({ key: k, t, r, e: epInfo(t) }); continue; }
    packRows.push(r);
    for (const f of eps) items.push({ key: k, t, f, e: { season: f.season, episode: f.episode } });
  }
  const num = (v) => (v == null ? 1e9 : v);
  items.sort((a, b) => num(a.e.season) - num(b.e.season) || num(a.e.episode) - num(b.e.episode) || a.t.name.localeCompare(b.t.name));
  const withEp = items.filter((x) => x.e.episode != null);
  const first = withEp[0], last = withEp[withEp.length - 1];
  let name = gr.name;
  if (!gr.id) { const m = gr.keys.map(infoOf).find(matched); if (m) name = m.title; }
  if (G.name.textContent !== name) G.name.textContent = name;

  let total = 0, done = 0, down = 0;
  const ts = gr.keys.map((k) => rows.get(k).last);
  for (const t of ts) { total += t.total_bytes; done += t.progress_bytes; down += t.down_bps; }
  const allDone = ts.every((t) => t.state === 'completed');
  const parts = [plural(items.length, 'episode')];
  if (first) parts.push(first === last ? code(first.e.season, first.e.episode) : `${code(first.e.season, first.e.episode)}–${code(last.e.season, last.e.episode)}`);
  parts.push(bytes(total));
  if (!allDone && total) parts.push(pct(done / total));
  if (down > 0) parts.push(`↓ ${speed(down)}`);
  const line = parts.join(' · ');
  if (G.statsText.textContent !== line) G.statsText.textContent = line;
  const w = `${(total ? (done / total) * 100 : 0).toFixed(2)}%`;
  if (G.fill.style.width !== w) G.fill.style.width = w;
  const st = allDone ? 'completed' : ts.some((t) => t.state === 'downloading') ? 'downloading' : ts.some((t) => t.state === 'error') ? 'error' : 'paused';
  const open = expanded.has(G.gid);
  const cls = `torrent group s-${st}${open ? ' expanded' : ''}`;
  if (G.el.className !== cls) G.el.className = cls;

  const next = items.find((x) => playable(x.t) && !itemWatched(x)) || items.find((x) => playable(x.t));
  G.next = next || null;
  const label = next ? `▶ ${next.e.episode != null ? code(next.e.season, next.e.episode) : 'Play'}` : '';
  if (G.nextBtn.textContent !== label) G.nextBtn.textContent = label;
  G.nextBtn.hidden = !next;

  const withArt = gr.keys.map((k) => rows.get(k)).find((r) => r.artUrl);
  const url = withArt ? withArt.artUrl : null;
  if (url && G.artUrl !== url) {
    G.artUrl = url;
    G.art.style.backgroundImage = `url("${url}")`;
    const img = new Image();
    img.className = 'thumb';
    img.alt = '';
    img.draggable = false;
    img.src = url;
    if (G.thumb) G.thumb.replaceWith(img); else G.head.insertBefore(img, G.name);
    G.thumb = img;
    G.head.classList.add('has-art');
    requestAnimationFrame(() => requestAnimationFrame(() => G.head.classList.add('art-on')));
  }

  // Episode list: season headers + episode rows, then the pack torrents themselves
  // (for pause / remove / details). Kept inside even when collapsed.
  G.eps.hidden = !open;
  const want = [];
  const header = (label) => {
    if (!G.seasons.has(label)) G.seasons.set(label, h('div', 'season', label));
    want.push(G.seasons.get(label));
  };
  const seasons = new Set(items.map((x) => x.e.season));
  const showHeaders = seasons.size > 1 || !seasons.has(undefined);
  let cur = {};
  for (const x of items) {
    if (showHeaders && x.e.season !== cur) {
      cur = x.e.season;
      header(cur == null ? 'Other' : cur === 0 ? 'Specials' : `Season ${cur}`);
    }
    if (!x.f) { want.push(x.r.el); continue; }
    const fk = `${x.key}:${x.f.file_index}`;
    let F = fileEls.get(fk);
    if (!F) { F = createFileRow(x.key, x.f.file_index); fileEls.set(fk, F); }
    seenF.add(fk);
    updateFileRow(F, x);
    want.push(F.el);
  }
  if (packRows.length) {
    header(packRows.length === 1 ? 'Season pack' : 'Season packs');
    packRows.forEach((r) => want.push(r.el));
  }
  want.forEach((el, i) => { if (G.eps.children[i] !== el) G.eps.insertBefore(el, G.eps.children[i] || null); });
  while (G.eps.children.length > want.length) G.eps.lastChild.remove();
}

// ---------- groups/watched persistence ----------
function reloadGroups() {
  return invoke('list_groups').then((g) => { groupsData = { groups: (g && g.groups) || [], excluded: (g && g.excluded) || {} }; render(lastList); }, () => {});
}
function reloadWatched() {
  return invoke('get_watched').then((w) => {
    watched = w || {};
    watchedKeys = new Set(Object.keys(watched).filter((k) => watched[k]).map((k) => k.slice(0, k.lastIndexOf(':'))));
    render(lastList);
  }, () => {});
}
async function mainIndex(key) {
  if (mainFile.has(key)) return mainFile.get(key);
  const media = (await listFiles(key)).filter((f) => f.kind === 'video' || f.kind === 'audio');
  if (!media.length) throw 'NOT_FOUND: no video or audio in this torrent';
  const idx = media.reduce((a, b) => (b.size > a.size ? b : a)).index;
  mainFile.set(key, idx);
  return idx;
}
function toggleFileWatched(key, fileIndex) {
  invoke('set_watched', { key, fileIndex, watched: !watched[`${key}:${fileIndex}`] }).then(reloadWatched, reportError);
}
async function toggleWatched(key) {
  try {
    const on = !isWatched(key);
    if (on) await invoke('set_watched', { key, fileIndex: await mainIndex(key), watched: true });
    else {
      for (const k of Object.keys(watched)) {
        if (k.startsWith(`${key}:`)) await invoke('set_watched', { key, fileIndex: Number(k.slice(key.length + 1)), watched: false });
      }
    }
    await reloadWatched();
  } catch (e) { playError(e); }
}

// The player keeps "tide:resume:<key>:<fileIndex>" = seconds in localStorage and drops it
// once ≥ 90 % is watched. Either signal marks the file watched (once).
const marked = new Set();
function markWatched(key, idx) {
  const k = `${key}:${idx}`;
  if (watched[k] || marked.has(k)) return;
  marked.add(k);
  invoke('set_watched', { key, fileIndex: idx, watched: true }).then(reloadWatched, () => marked.delete(k));
}
const RESUME_RE = /^tide:resume:([0-9a-f]{40}):(\d+)$/;
function scanResume() {
  let ls;
  try { ls = window.localStorage; } catch { return; }
  for (let i = 0; i < ls.length; i++) {
    const m = RESUME_RE.exec(ls.key(i) || '');
    if (!m) continue;
    const secs = parseFloat(ls.getItem(m[0]));
    const info = infoOf(m[1]);
    const dur = info && info.runtime_min ? info.runtime_min * 60 : 0;
    if (dur && secs >= 0.9 * dur) markWatched(m[1], Number(m[2]));
  }
}
window.addEventListener('storage', (e) => {
  const m = RESUME_RE.exec(e.key || '');
  if (m && e.oldValue != null && e.newValue == null && parseFloat(e.oldValue) >= 60) markWatched(m[1], Number(m[2]));
});
setInterval(scanResume, 5000);
window.addEventListener('focus', () => { scanResume(); reloadWatched(); });

// ---------- ⋯ menu ----------
const menu = h('div', 'menu');
menu.hidden = true;
document.body.append(menu);
let menuCtx = null;
function closeMenu() { menu.hidden = true; menuCtx = null; }
document.addEventListener('mousedown', (e) => { if (!menu.hidden && !menu.contains(e.target)) closeMenu(); });
$('list').addEventListener('scroll', closeMenu);

function menuItem(text, run, cls) {
  const b = h('button', `mi${cls ? ` ${cls}` : ''}`, text);
  b.type = 'button';
  b.addEventListener('click', (e) => { e.stopPropagation(); run(); });
  return b;
}
function menuInput(value, placeholder, done) {
  const f = h('form', 'mform');
  const i = h('input');
  i.type = 'text';
  i.spellcheck = false;
  i.value = value || '';
  i.placeholder = placeholder;
  f.append(i);
  f.addEventListener('submit', (e) => { e.preventDefault(); const v = i.value.trim(); if (v) { closeMenu(); done(v); } });
  i.addEventListener('keydown', (e) => { if (e.key === 'Escape') { e.stopPropagation(); closeMenu(); } });
  menu.replaceChildren(f);
  i.focus();
  i.select();
}
const act = (p) => Promise.resolve(p).then(reloadGroups, reportError);

function openMenu(anchor, ctx) {
  menuCtx = ctx;
  const items = [];
  if (ctx.key) {
    const key = ctx.key, gid = rows.get(key) && rows.get(key).group;
    items.push(menuItem('Add to group…', () => groupPicker(key, gid)));
    if (gid) items.push(menuItem('Remove from group', () => { closeMenu(); act(invoke('remove_from_group', { key })); }));
    const target = archiveTarget(), row = rows.get(key);
    if (target && row && row.last && row.last.state === 'completed') {
      items.push(menuItem(`Move to ${target}`, () => { closeMenu(); archiveMove({ key }); }));
    }
  } else {
    const gr = lastGroups.get(ctx.gid);
    if (!gr) return;
    items.push(menuItem('Rename group', () => menuInput(gr.name, 'Group name', (name) => act(gr.id
      ? invoke('rename_group', { id: gr.id, name })
      : invoke('create_group', { name, keys: gr.keys })))));
    items.push(menuItem('Ungroup', () => { closeMenu(); act(ungroup(gr)); }));
    const fol = followItem(gr);
    if (fol) items.push(fol);
  }
  menu.replaceChildren(...items);
  menu.hidden = false;
  const a = anchor.getBoundingClientRect();
  menu.style.top = `${Math.min(a.bottom + 4, innerHeight - menu.offsetHeight - 8)}px`;
  menu.style.left = `${Math.max(8, a.right - menu.offsetWidth)}px`;
}

function groupPicker(key, current) {
  const items = [];
  for (const gr of lastGroups.values()) {
    if (gr.gid === current) continue;
    items.push(menuItem(gr.name, () => {
      closeMenu();
      act(gr.id ? invoke('add_to_group', { id: gr.id, key }) : invoke('create_group', { name: gr.name, keys: [...gr.keys, key] }));
    }));
  }
  items.push(menuItem('New group…', () => menuInput('', 'Group name', (name) => act(invoke('create_group', { name, keys: [key] }))), 'new'));
  menu.replaceChildren(...items);
}

async function ungroup(gr) {
  for (const key of gr.keys) await invoke('remove_from_group', { key });
  if (gr.id) await invoke('delete_group', { id: gr.id });
  expanded.delete(gr.gid);
}

// ---------- artwork ----------
// Priority: media poster from metadata (2) > local poster inside the torrent (1) > none.
// key -> { done, busy, tries, next }. One get_artwork per row; NOT_READY / errors
// retry with backoff (3 s, 6 s, … capped at 60 s); stops once found or the backend
// reports "metadata known, no image" (poster: null).
const art = new Map();
const ART_MAX = 2;
let artBusy = 0;

function wantArtwork(t) {
  const dev = t.remote ? t.remote.id : null; // other Mac's item: art via remote_art
  if (!art.has(t.key) || art.get(t.key).dev !== dev) art.set(t.key, { done: false, busy: false, tries: 0, next: 0, dev });
  const a = art.get(t.key);
  a.eligible = t.state !== 'resolving' && t.id != null;
}

function pumpArtwork() {
  const now = Date.now();
  for (const [key, a] of art) {
    if (artBusy >= ART_MAX) return;
    if (a.done || a.busy || !a.eligible || now < a.next) continue;
    a.busy = true; artBusy++;
    const req = a.dev
      ? invoke('remote_art', { deviceId: a.dev, key, kind: 'poster' }).then((url) => ({ poster: url || null, kind: 'poster', pri: 2 }))
      : invoke('get_artwork', { key });
    req.then((res) => {
      a.done = true;
      if (res && res.poster) showArtwork(key, res.poster, res.kind, res.pri || 1);
    }, (e) => {
      a.tries++;
      a.next = Date.now() + Math.min(60e3, 3e3 * 2 ** (a.tries - 1));
      if (errCode(e) === 'NOT_FOUND' || (a.dev && missingCmd(e))) a.done = true;
    }).finally(() => { a.busy = false; artBusy--; pumpArtwork(); });
  }
}

function showArtwork(key, url, kind, pri) {
  const r = rows.get(key);
  if (!r || r.artPri > pri) return;
  const img = new Image();
  img.className = 'thumb';
  img.alt = '';
  img.draggable = false;
  img.onload = () => {
    if (rows.get(key) !== r || r.artPri > pri) return;
    r.artPri = pri;
    r.artUrl = url;
    r.art.style.backgroundImage = `url("${url}")`;
    if (r.thumb) r.thumb.replaceWith(img); else r.head.classList.add('has-art');
    r.head.insertBefore(img, r.name);
    r.thumb = img;
    r.head.classList.toggle('k-backdrop', kind === 'backdrop');
    r.head.classList.toggle('k-poster', kind !== 'backdrop');
    requestAnimationFrame(() => requestAnimationFrame(() => r.head.classList.add('art-on')));
    const mm = meta.get(key);
    if (mm && mm.dev && kind === 'poster') { mm.poster = url; if (openKey === key) updatePanel(key); }
  };
  img.src = url;
}

// ---------- media info (Cinemeta via the backend) ----------
// key -> { done, busy, tries, next, eligible, info, poster }. Silent: errors only mean
// "no media info"; NOT_READY retries with backoff (5 s … 60 s).
const meta = new Map();
const META_MAX = 2;
let metaBusy = 0;

function wantMeta(t) {
  const dev = t.remote ? t.remote.id : null;
  if (!meta.has(t.key) || meta.get(t.key).dev !== dev) meta.set(t.key, { done: false, busy: false, tries: 0, next: 0, info: null, poster: null, dev });
  const m = meta.get(t.key);
  if (dev) {
    // Other Mac's item: media info came with remote_library.
    m.done = true;
    const ro = remoteByKey.get(t.key), info = (ro && ro.media) || null;
    if (JSON.stringify(m.info) !== JSON.stringify(info)) setInfo(t.key, info);
    return;
  }
  m.eligible = t.state !== 'resolving' && t.id != null && !!t.output_path;
}

function pumpMeta() {
  const now = Date.now();
  for (const [key, m] of meta) {
    if (metaBusy >= META_MAX) return;
    if (m.done || m.busy || !m.eligible || now < m.next) continue;
    m.busy = true; metaBusy++;
    invoke('get_media_info', { key }).then((info) => {
      m.done = true;
      setInfo(key, info || null);
    }, (e) => {
      if (errCode(e) !== 'NOT_FOUND') {
        m.tries++;
        m.next = Date.now() + Math.min(60e3, 5e3 * 2 ** (m.tries - 1));
      } else m.done = true;
    }).finally(() => { m.busy = false; metaBusy--; pumpMeta(); });
  }
}

function setInfo(key, info) {
  const m = meta.get(key);
  if (!m) return;
  m.info = info;
  m.poster = null;
  const r = rows.get(key);
  if (r && r.last) updateRow(r, r.last);
  if (m.dev) {
    // Poster from the other Mac (the art pump fetches it; reuse it for the panel).
    if (r && r.artUrl) m.poster = r.artUrl;
  } else if (matched(info) && info.imdb_id) {
    invoke('get_media_image', { key, kind: 'poster' }).then((url) => {
      if (meta.get(key) !== m || m.info !== info) return;
      m.poster = url;
      showArtwork(key, url, 'poster', 2);
      if (openKey === key) updatePanel(key);
    }, () => {});
  }
  if (openKey === key) updatePanel(key);
}

// ---------- detail panel (one open at a time) ----------
function toggle(key) {
  const was = openKey;
  closePanel();
  if (was === key) return;
  const r = rows.get(key);
  if (!r) return;
  openKey = key;
  r.el.classList.add('open');
  r.panel = buildPanel(key);
  r.el.append(r.panel.el);
  updatePanel(key);
  loadDetails(key, r);
}

function loadDetails(key, r) {
  if (remoteOnly(key)) return; // the other Mac's torrent: only the info hash is known here
  invoke('get_torrent_details', { key }).then((d) => { r.details = d; if (r.panel) fillMore(r.panel, key, d); }, () => {});
}

function closePanel() {
  const r = openKey && rows.get(openKey);
  if (r) { r.el.classList.remove('open'); if (r.panel) r.panel.el.remove(); r.panel = null; }
  openKey = null;
  document.body.classList.remove('sheet-open');
}

function linkBtn(text, cls, onClick) {
  const b = h('button', `link${cls ? ` ${cls}` : ''}`, text);
  b.type = 'button';
  b.addEventListener('click', (e) => { e.stopPropagation(); onClick(e); });
  return b;
}

function copy(text) {
  (navigator.clipboard ? navigator.clipboard.writeText(text) : Promise.reject())
    .then(() => toast('Copied'), () => toast('Could not copy'));
}

function buildPanel(key) {
  const p = { el: h('div', 'detail'), info: undefined, poster: undefined };
  p.media = h('div', 'media');
  p.media.hidden = true;
  const tor = h('div', 'tor');
  p.stats = h('div', 'tstats');
  p.path = linkBtn('', 'path', () => invoke('reveal_torrent', { key }).catch(reportError));
  p.path.title = 'Show in Finder';
  p.files = h('div', 'files');
  p.files.append(h('div', 'msg', 'Loading…'));
  p.more = h('div', 'more');
  p.more.hidden = true;
  p.moreBtn = linkBtn('More', 'more-toggle', () => {
    p.more.hidden = !p.more.hidden;
    p.moreBtn.textContent = p.more.hidden ? 'More' : 'Less';
  });
  fillMore(p, key, null);
  tor.append(p.stats, p.path, p.files, p.moreBtn, p.more);
  p.el.append(p.media, tor);
  return p;
}

function torrentStats(t) {
  if (t.state === 'resolving') return 'Getting metadata…';
  if (t.state === 'completed') return `${bytes(t.total_bytes)} · Completed`;
  const parts = [bytes(t.total_bytes), `${pct(t.progress)} · ${bytes(t.progress_bytes)} / ${bytes(t.total_bytes)}`,
    `↓ ${speed(t.down_bps)}`, `↑ ${speed(t.up_bps)}`, plural(t.peers, 'peer')];
  if (t.progress_bytes > 0) parts.push(`ratio ${(t.uploaded_bytes / t.progress_bytes).toFixed(2)}`);
  if (t.eta_secs != null && t.down_bps > 0) parts.push(eta(t.eta_secs));
  if (t.state === 'error' && t.error) parts.push(t.error);
  return parts.join(' · ');
}

function updatePanel(key) {
  const r = rows.get(key);
  if (!r || !r.panel || !r.last) return;
  const p = r.panel, t = r.last, m = meta.get(key) || {};
  const line = (t.remote ? `On ${t.remote.name}${t.remote.online ? '' : ' (offline)'} · ` : '') + torrentStats(t);
  if (p.stats.textContent !== line) p.stats.textContent = line;
  const path = t.remote ? '' : t.output_path || (r.details && r.details.save_path) || '';
  if (p.path.textContent !== path) p.path.textContent = path;
  p.path.hidden = !path;
  if (p.info !== m.info) { p.info = m.info; p.poster = undefined; renderMedia(p, key, m.info); }
  if (p.poster !== m.poster && p.posterEl) {
    p.poster = m.poster;
    p.posterEl.style.backgroundImage = m.poster ? `url("${m.poster}")` : '';
    p.posterEl.classList.toggle('on', !!m.poster);
  }
  loadFiles(key);
}

function renderMedia(p, key, m) {
  p.media.replaceChildren();
  p.posterEl = null;
  p.media.hidden = !m;
  if (!m) return;
  p.search = h('div', 'msearch');
  p.search.hidden = true;
  const ro = !!remoteOnly(key); // matching happens on the Mac that has the files
  if (m.needs_match) {
    const row = h('div', 'nomatch');
    row.append(h('span', null, `No match for “${m.title || 'this torrent'}”`));
    if (!ro) row.append(linkBtn('Match', 'btn', () => openSearch(p, key, m)));
    p.media.append(row, p.search);
    return;
  }
  p.posterEl = h('div', 'mposter');
  const col = h('div', 'minfo');
  col.append(h('div', 'mtitle', m.year ? `${m.title} (${m.year})` : m.title));
  if (m.media_type === 'tv') {
    const se = [];
    if (m.season != null && m.episode != null) se.push(`S${pad2(m.season)}E${pad2(m.episode)}`);
    else if (m.season != null) se.push(`Season ${m.season}`);
    else if (m.air_date) se.push(m.air_date);
    if (m.episode_title) se.push(m.episode_title);
    if (se.length) col.append(h('div', 'mep', se.join(' · ')));
  }
  const ml = h('div', 'mmeta');
  const rt = rating(m);
  if (rt) {
    const rs = h('span', 'rating');
    rs.append(h('span', 'imdb', 'IMDb'), ` ${rt}`);
    if (m.imdb_votes) rs.append(h('span', 'votes', ` (${votes(m.imdb_votes)})`));
    ml.append(rs);
  }
  const rest = [(m.genres || []).slice(0, 3).join(', '), runtime(m.runtime_min)].filter(Boolean).join(' · ');
  if (rest) ml.append(document.createTextNode(`${rt ? ' · ' : ''}${rest}`));
  if (ml.childNodes.length) col.append(ml);
  const text = (m.media_type === 'tv' && m.episode_overview) || m.overview;
  if (text) {
    const ov = h('div', 'ov', text);
    const more = linkBtn('more', 'ovmore', () => { ov.classList.add('full'); more.remove(); });
    more.hidden = true;
    col.append(ov, more);
    requestAnimationFrame(() => { more.hidden = ov.scrollHeight <= ov.clientHeight + 1; });
  }
  const links = h('div', 'mlinks');
  const imdb = m.imdb_url || (m.imdb_id ? `https://www.imdb.com/title/${m.imdb_id}/` : null);
  if (imdb) links.append(linkBtn('IMDb ↗', null, () => invoke('open_external', { url: imdb }).catch(reportError)));
  if (!ro) links.append(linkBtn('Wrong match?', 'muted', () => openSearch(p, key, m)));
  col.append(links, p.search);
  p.media.append(p.posterEl, col);
}

function openSearch(p, key, m) {
  const box = p.search;
  if (!box.hidden) { box.hidden = true; return; }
  box.hidden = false;
  box.replaceChildren();
  const form = h('form', 'sform');
  const input = h('input');
  input.type = 'text';
  input.spellcheck = false;
  input.placeholder = 'Title or IMDb link';
  input.value = m.title || '';
  const res = h('div', 'tiles');
  form.append(input);
  box.append(form, res);
  input.addEventListener('keydown', (e) => { if (e.key === 'Escape') { e.stopPropagation(); box.hidden = true; } });
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    const query = input.value.trim();
    if (!query) return;
    res.replaceChildren(h('div', 'msg', 'Searching…'));
    const mediaType = m.media_type === 'tv' || m.season != null ? 'tv' : null;
    invoke('search_media', { query, mediaType }).then((list) => {
      if (!list || !list.length) return res.replaceChildren(h('div', 'msg', 'Nothing found'));
      res.replaceChildren(...list.map((x) => tile(key, x)));
    }, (err) => res.replaceChildren(h('div', 'msg', errMsg(err))));
  });
  input.focus();
  input.select();
  if (m.needs_match && input.value) form.requestSubmit();
}

function tile(key, x) {
  const b = h('button', 'tile');
  b.type = 'button';
  const pic = h('div', 'tpic');
  if (x.poster_url) pic.style.backgroundImage = `url("${x.poster_url}")`;
  b.append(pic, h('div', 'tname', x.title), h('div', 'tyear', x.release_info || (x.year ? String(x.year) : '')));
  b.title = `${x.title}${x.year ? ` (${x.year})` : ''}`;
  b.addEventListener('click', () => {
    invoke('set_media_match', { key, imdbId: x.imdb_id, mediaType: x.media_type || 'movie' })
      .then((info) => setInfo(key, info), reportError);
  });
  return b;
}

function fillMore(p, key, d) {
  const hash = (d && d.info_hash) || key;
  const line = (label, value, mono) => {
    const row = h('div', 'mrow');
    const v = h('span', `mval${mono ? ' mono' : ''}`, value);
    v.title = value;
    row.append(h('span', 'mlabel', label), v, linkBtn('Copy', null, () => copy(value)));
    return row;
  };
  p.more.replaceChildren(line('Info hash', hash, true));
  if (d && d.magnet) p.more.append(line('Magnet', d.magnet, true));
  if (d && d.trackers != null) p.more.append(h('div', 'mrow mlabel', plural(d.trackers, 'tracker')));
}

// ---------- file list ----------
async function loadFiles(key) {
  let files, msg = null;
  try { files = await listFiles(key); } catch (e) {
    msg = errCode(e) === 'NOT_READY' ? 'Getting metadata…' : errMsg(e);
  }
  const r = rows.get(key);
  if (!r || !r.panel) return;
  const box = r.panel.files;
  if (msg || !files.length) {
    box.replaceChildren(h('div', 'msg', msg || 'No files'));
    box._n = 0;
    return;
  }
  if (box._n !== files.length) {
    box.replaceChildren(...files.map((f) => fileRow(key, f)));
    box._n = files.length;
  }
  files.forEach((f, i) => {
    const pc = box.children[i]._pct, txt = pct(f.progress);
    if (pc.textContent !== txt) pc.textContent = txt;
  });
}

function fileRow(key, f) {
  const row = h('div', 'file');
  const name = h('span', 'fname', f.name);
  name.title = f.name;
  if (f.kind === 'subtitle') name.prepend(h('span', 'cc', 'CC'));
  row._pct = h('span', 'fpct');
  const media = f.kind === 'video' || f.kind === 'audio';
  row.append(name, h('span', 'fsize', bytes(f.size)), row._pct,
    media ? iconBtn('play', 'Play', () => play(key, f.index)) : h('span', 'nop'));
  if (media) row.append(remoteBtn(() => [key, f.index], () => key));
  return row;
}

function refresh() { invoke('list_torrents').then(render, reportError); }

// ---------- new-episode watcher ----------
// Banner above the list for aired episodes newer than what's in Tide; search results come
// only from the user's own Torznab endpoint (Prowlarr / Jackett). Gear = settings popover.
ICONS.gear = '<circle cx="8" cy="8" r="2.2"/><path d="M8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6l1.1 1.1M11.3 11.3l1.1 1.1M3.6 12.4l1.1-1.1M11.3 4.7l1.1-1.1"/>';
let wSettings = { torznab_url: '', api_key: '', auto_add: false, quality: '1080p', unfollowed: [], last_check: 0 };
let wEpisodes = [];
const wBanner = h('div', 'wbanner');
wBanner.hidden = true;
$('list').before(wBanner);
const wGear = iconBtn('gear', 'Settings', () => (wPop.hidden ? openWatcherSettings() : closeWatcherSettings()));
wGear.id = 'wgear';
$('plus').before(wGear);

const epCode = (n) => `S${pad2(n.season)}E${pad2(n.episode)}`;
const resultLine = (r) => [r.quality, r.size ? bytes(r.size) : null, `${r.seeders} seed${r.seeders === 1 ? '' : 's'}`].filter(Boolean).join(' · ');
const wSearchOn = () => !!(wSettings.torznab_url || '').trim();

function setEpisodes(list) { wEpisodes = Array.isArray(list) ? list : []; renderBanner(); }
function dismissEpisode(n) {
  wEpisodes = wEpisodes.filter((x) => x !== n);
  renderBanner();
  invoke('watcher_dismiss', { imdbId: n.imdb_id, season: n.season, episode: n.episode }).catch(reportError);
}
async function addResult(n, r) {
  closeMenu();
  try {
    await addMagnet(r.magnet);
    toast(`Added ${n.show} ${epCode(n)}`);
    dismissEpisode(n);
  } catch (e) { reportError(e); }
}

function renderBanner() {
  const shown = wEpisodes.slice(0, 3);
  wBanner.hidden = !shown.length;
  wBanner.replaceChildren(...shown.map((n) => {
    const line = h('div', 'wline');
    const what = h('span', 'wwhat');
    what.append(h('b', null, `${n.show} ${epCode(n)}`), document.createTextNode(' is out'));
    if (n.episode_title) what.title = n.episode_title;
    line.append(h('span', 'wdot'), what);
    const best = n.results && n.results[0];
    if (best) {
      const chip = h('span', 'wchip');
      chip.title = best.title;
      const add = h('button', 'wadd', 'Add');
      add.type = 'button';
      add.addEventListener('click', () => addResult(n, best));
      chip.append(h('span', null, resultLine(best)), add);
      line.append(chip);
      if (n.results.length > 1) {
        const more = h('button', 'wlink', 'more ▾');
        more.type = 'button';
        more.addEventListener('click', (e) => { e.stopPropagation(); openResults(more, n); });
        line.append(more);
      }
    } else if (!wSearchOn()) {
      const setup = h('button', 'wlink', 'Set up search');
      setup.type = 'button';
      setup.addEventListener('click', (e) => { e.stopPropagation(); openWatcherSettings(); });
      line.append(setup);
    } else {
      line.append(h('span', 'wmuted', 'no results yet'));
    }
    const x = h('button', 'wx', '×');
    x.type = 'button';
    x.title = 'Dismiss';
    x.addEventListener('click', () => dismissEpisode(n));
    line.append(x);
    return line;
  }));
  if (wEpisodes.length > 3) wBanner.append(h('div', 'wline wmore', `+${wEpisodes.length - 3} more`));
}

function openResults(anchor, n) {
  const items = n.results.slice(1).map((r) => {
    const b = menuItem(resultLine(r), () => addResult(n, r));
    b.title = r.title;
    return b;
  });
  menuCtx = { results: n };
  menu.replaceChildren(...items);
  menu.hidden = false;
  const a = anchor.getBoundingClientRect();
  menu.style.top = `${a.bottom + 4}px`;
  menu.style.left = `${Math.min(a.left, innerWidth - menu.offsetWidth - 8)}px`;
}

// Settings popover.
const wPop = h('form', 'wpop');
wPop.hidden = true;
wPop.autocomplete = 'off';
wPop.spellcheck = false;
const wField = (label, input) => { const l = h('label', 'wfield'); l.append(h('span', null, label), input); return l; };
const wUrl = h('input'); wUrl.type = 'text'; wUrl.placeholder = 'http://localhost:9696/1/';
const wKey = h('input'); wKey.type = 'password'; wKey.placeholder = 'API key';
const wQual = h('select');
[['1080p', '1080p'], ['720p', '720p'], ['any', 'Any quality']].forEach(([v, t]) => { const o = h('option', null, t); o.value = v; wQual.append(o); });
const wAuto = h('input'); wAuto.type = 'checkbox';
const wAutoL = h('label', 'check'); wAutoL.append(wAuto, document.createTextNode(' Auto-add best result'));
const wStatus = h('p', 'wstatus');
const wSave = h('button', 'wprimary', 'Save'); wSave.type = 'submit';
const wCheck = h('button', null, 'Check now'); wCheck.type = 'button';
const wActs = h('div', 'actions'); wActs.append(wCheck, wSave);
wPop.append(h('p', 'title', 'New episodes'), h('p', 'whint', 'Search uses your own Torznab feed (Prowlarr or Jackett). Leave empty to only get notified.'),
  wField('Torznab URL', wUrl), wField('API key', wKey), wField('Quality', wQual), wAutoL, wStatus, wActs);
document.body.append(wPop);

function fillWatcherForm() {
  wUrl.value = wSettings.torznab_url || '';
  wKey.value = wSettings.api_key || '';
  wQual.value = wSettings.quality || '1080p';
  wAuto.checked = !!wSettings.auto_add;
  wStatus.textContent = wSettings.last_check ? `Last checked ${new Date(wSettings.last_check * 1000).toLocaleString()}` : '';
  wStatus.className = 'wstatus';
}
function openWatcherSettings() {
  closeMenu();
  fillWatcherForm();
  wPop.hidden = false;
  const a = wGear.getBoundingClientRect();
  wPop.style.top = `${a.bottom + 6}px`;
  wPop.style.right = `${Math.max(8, innerWidth - a.right - 8)}px`;
  wUrl.focus();
}
function closeWatcherSettings() { wPop.hidden = true; }
const wSay = (msg, bad) => { wStatus.textContent = msg; wStatus.className = `wstatus${bad ? ' bad' : ''}`; };
document.addEventListener('mousedown', (e) => { if (!wPop.hidden && !wPop.contains(e.target) && !wGear.contains(e.target)) closeWatcherSettings(); });
wPop.addEventListener('keydown', (e) => { if (e.key === 'Escape') { e.stopPropagation(); closeWatcherSettings(); } });
wPop.addEventListener('submit', async (e) => {
  e.preventDefault();
  wSave.disabled = true;
  wSay(wUrl.value.trim() ? 'Checking the feed…' : 'Saving…');
  try {
    wSettings = await invoke('watcher_set_settings', { torznabUrl: wUrl.value.trim(), apiKey: wKey.value.trim(), autoAdd: wAuto.checked, quality: wQual.value });
    wSay('Saved');
    renderBanner();
  } catch (err) { wSay(errMsg(err), true); }
  wSave.disabled = false;
});
wCheck.addEventListener('click', async () => {
  wCheck.disabled = true;
  wSay('Checking…');
  try {
    setEpisodes(await invoke('watcher_check', { force: true }));
    wSay(wEpisodes.length ? `${plural(wEpisodes.length, 'new episode')}` : 'Up to date');
    loadWatcher();
  } catch (err) { wSay(errMsg(err), true); }
  wCheck.disabled = false;
});

function loadWatcher() {
  return invoke('watcher_get_settings').then((s) => { if (s) { wSettings = s; renderBanner(); } }, () => {});
}

// ---------- more settings (mobile data, notifications, cleanup) in the same popover ----------
let net = null; // NetStatus
const nBanner = h('div', 'nbanner');
nBanner.hidden = true;
$('list').before(nBanner);
const sSection = (title) => { const s = h('div', 'wsec'); s.append(h('p', 'title', title)); return s; };
const nLine = h('p', 'whint');
const nMode = h('select');
[['pause', 'Pause downloads'], ['limit', 'Limit speed'], ['off', 'Off']].forEach(([v, t]) => { const o = h('option', null, t); o.value = v; nMode.append(o); });
const nKbps = h('input'); nKbps.type = 'number'; nKbps.min = '1'; nKbps.className = 'nkbps'; nKbps.title = 'KB/s';
const nModeRow = h('div', 'nrow'); nModeRow.append(nMode, nKbps, h('span', 'wmuted', 'KB/s'));
const nMark = h('input'); nMark.type = 'checkbox';
const nMarkL = h('label', 'check'); nMarkL.append(nMark, document.createTextNode(' Treat this network as mobile data'));
const nAuto = h('button', 'wlink', 'auto'); nAuto.type = 'button'; nAuto.title = 'Back to automatic detection';
const nMarkRow = h('div', 'nrow'); nMarkRow.append(nMarkL, nAuto);
const netSec = sSection('Mobile data'); netSec.append(nLine, nModeRow, nMarkRow);
const nfOn = h('input'); nfOn.type = 'checkbox';
const nfL = h('label', 'check'); nfL.append(nfOn, document.createTextNode(' Notify when downloads finish'));
const nfSec = sSection('Notifications'); nfSec.append(nfL);
const cBtn = h('button', null, 'Free up space…'); cBtn.type = 'button';
const cSec = sSection('Storage'); cSec.append(h('p', 'whint', 'Delete watched downloads older than 30 days.'), cBtn);
wPop.append(netSec, nfSec, cSec);

function renderNet() {
  const s = net;
  if (!s) return;
  const paused = (s.paused_by_netmode || []).length;
  const where = !s.detected ? 'Detecting network…' : `This network: ${s.metered ? 'metered' : 'not metered'}${s.ssid ? ` (${s.ssid})` : ''}`;
  nLine.textContent = s.metered && paused ? `${where} · ${plural(paused, 'download')} paused` : where;
  if (document.activeElement !== nMode) nMode.value = s.mode;
  if (document.activeElement !== nKbps) nKbps.value = s.limit_kbps;
  nKbps.hidden = s.mode !== 'limit';
  nModeRow.lastChild.hidden = s.mode !== 'limit';
  nMark.checked = s.manual != null ? s.manual : !!s.metered;
  nAuto.hidden = s.manual == null;
  nBanner.hidden = !(s.metered && s.mode === 'pause' && paused);
  if (!nBanner.hidden) {
    const go = h('button', 'wlink', 'Resume anyway');
    go.type = 'button';
    go.addEventListener('click', () => setNet('net_set_mode', { mode: 'off' }));
    const line = h('div', 'wline');
    line.append(h('span', 'wmuted', `On mobile data — ${plural(paused, 'download')} paused`), go);
    nBanner.replaceChildren(line);
  }
}
const setNet = (cmd, args) => invoke(cmd, args).then((s) => { net = s; renderNet(); refresh(); }, reportError);
nMode.addEventListener('change', () => setNet('net_set_mode', { mode: nMode.value, limitKbps: Math.max(1, Number(nKbps.value) || 200) }));
nKbps.addEventListener('change', () => setNet('net_set_mode', { mode: nMode.value, limitKbps: Math.max(1, Number(nKbps.value) || 200) }));
nMark.addEventListener('change', () => setNet('net_mark_current', { metered: nMark.checked }));
nAuto.addEventListener('click', () => setNet('net_mark_current', { metered: null }));
nfOn.addEventListener('change', () => invoke('notify_set_settings', { enabled: nfOn.checked }).then((s) => { nfOn.checked = !!s.enabled; }, reportError));
function loadMoreSettings() {
  invoke('net_status').then((s) => { net = s; renderNet(); }, () => {});
  invoke('notify_get_settings').then((s) => { nfOn.checked = !!(s && s.enabled); }, () => {});
}
wGear.addEventListener('click', () => { if (!wPop.hidden) loadMoreSettings(); });

// Cleanup sheet.
const cSheet = h('div', 'wpop csheet');
cSheet.hidden = true;
document.body.append(cSheet);
document.addEventListener('mousedown', (e) => { if (!cSheet.hidden && !cSheet.contains(e.target)) cSheet.hidden = true; });
cSheet.addEventListener('keydown', (e) => { if (e.key === 'Escape') { e.stopPropagation(); cSheet.hidden = true; } });
function ageOf(t) {
  if (!t) return '';
  const d = Math.floor((Date.now() / 1000 - t) / 86400);
  return d < 1 ? 'today' : d < 60 ? `${d} days ago` : `${Math.round(d / 30)} months ago`;
}
cBtn.addEventListener('click', async () => {
  let list;
  try { list = await invoke('cleanup_candidates', {}); } catch (e) { return reportError(e); }
  closeWatcherSettings();
  const items = (list && list.items) || [];
  const boxes = [];
  const go = h('button', 'wprimary'); go.type = 'button';
  const cancel = h('button', null, 'Cancel'); cancel.type = 'button';
  const sum = () => {
    const sel = items.filter((_, i) => boxes[i].checked);
    go.textContent = `Delete ${plural(sel.length, 'item')} (${bytes(sel.reduce((a, c) => a + c.size, 0))})`;
    go.disabled = !sel.length;
    return sel;
  };
  const rowsEl = h('div', 'clist');
  items.forEach((c, i) => {
    const l = h('label', 'crow');
    const b = h('input'); b.type = 'checkbox'; b.checked = true; b.addEventListener('change', sum);
    boxes.push(b);
    const name = h('span', 'cname', c.name); name.title = c.name;
    l.append(b, name, h('span', 'wmuted', `${bytes(c.size)} · ${ageOf(c.completed_at)}`));
    rowsEl.append(l);
  });
  const acts = h('div', 'actions'); acts.append(cancel, go);
  cSheet.replaceChildren(h('p', 'title', 'Free up space'),
    items.length ? rowsEl : h('p', 'whint', 'Nothing to clean up — no watched downloads older than 30 days.'), acts);
  if (!items.length) go.hidden = true;
  sum();
  cancel.addEventListener('click', () => { cSheet.hidden = true; });
  go.addEventListener('click', async () => {
    go.disabled = true;
    try {
      const r = await invoke('cleanup_apply', { items: sum().map((c) => ({ key: c.key, fileIndex: c.file_index })) });
      cSheet.hidden = true;
      const skip = (r.skipped || []).map((s) => s.reason);
      toast(`Freed ${bytes(r.freed_bytes || 0)}${skip.length ? ` · ${skip.length} skipped: ${[...new Set(skip)].join(', ')}` : ''}`);
      refresh();
      reloadWatched();
    } catch (e) { go.disabled = false; reportError(e); }
  });
  cSheet.hidden = false;
});
// IMDb id of a group's series (for Follow / Unfollow), if matched.
function groupSeries(gr) {
  const m = gr.keys.map(infoOf).find((i) => matched(i) && i.media_type === 'tv' && i.imdb_id);
  return m ? m.imdb_id : null;
}
function followItem(gr) {
  const tt = groupSeries(gr);
  if (!tt) return null;
  const off = (wSettings.unfollowed || []).includes(tt);
  return menuItem(off ? 'Follow' : 'Unfollow', () => {
    closeMenu();
    invoke(off ? 'watcher_follow' : 'watcher_unfollow', { imdbId: tt }).then(() => {
      if (!off) setEpisodes(wEpisodes.filter((n) => n.imdb_id !== tt));
      toast(off ? 'Following new episodes' : 'No longer following');
      return loadWatcher();
    }, reportError);
  });
}

// ---------- continue watching ----------
// Source: resume_list — positions from both Macs, merged by the backend ({ key, file_index,
// pos, dur, updated_at, device }), newest first. Fallback when that command is missing:
// the player's "tide:resume:<key>:<fileIndex>" = seconds entries in the shared localStorage
// (no duration there, so the bar uses the matched runtime; change times tracked in "tide:cwAt").
const CW_MAX = 6;
const cwEl = h('section', 'cw');
cwEl.hidden = true;
const cwStrip = h('div', 'cwstrip');
cwEl.append(h('div', 'cwhead', 'Continue watching'), cwStrip);
$('list').prepend(cwEl);
const cwCards = new Map(); // "key:fileIndex" -> card
const cwVal = new Map(); // "key:fileIndex" -> last seen seconds
let cwScanned = false;
let cwAt = {};
try { cwAt = JSON.parse(localStorage.getItem('tide:cwAt') || '{}') || {}; } catch { cwAt = {}; }

let resumeList = null; // resume_list result; null = command unavailable -> localStorage
let resumeListOk = true;
let resumeLoading = false;
function loadResume() {
  if (!resumeListOk || resumeLoading) { renderContinue(); return Promise.resolve(); }
  resumeLoading = true;
  return invoke('resume_list').then((l) => { resumeList = Array.isArray(l) ? l : []; },
    (e) => { if (missingCmd(e)) { resumeListOk = false; resumeList = null; } })
    .finally(() => { resumeLoading = false; renderContinue(); });
}
let resumeTimer;
const loadResumeSoon = () => { clearTimeout(resumeTimer); resumeTimer = setTimeout(loadResume, 400); };

function resumeEntries() {
  if (resumeList) {
    return resumeList.filter((x) => x && x.key && isFinite(x.pos) && x.pos > 0).map((x) => ({
      fk: `${x.key}:${x.file_index}`, lsKey: `tide:resume:${x.key}:${x.file_index}`, key: x.key, idx: Number(x.file_index),
      secs: x.pos, dur: x.dur > 0 ? x.dur : 0, at: x.updated_at || 0, device: x.device || null }));
  }
  let ls;
  try { ls = window.localStorage; } catch { return []; }
  const out = [];
  let dirty = false;
  try {
    for (let i = 0; i < ls.length; i++) {
      const m = RESUME_RE.exec(ls.key(i) || '');
      if (!m) continue;
      const secs = parseFloat(ls.getItem(m[0]));
      if (!isFinite(secs) || secs <= 0) continue;
      const fk = `${m[1]}:${m[2]}`;
      const d = parseFloat(ls.getItem(`tide:dur:${fk}`));
      if ((cwScanned && cwVal.get(fk) !== secs) || !cwAt[fk]) { cwAt[fk] = cwScanned ? Date.now() : 1; dirty = true; }
      cwVal.set(fk, secs);
      out.push({ fk, lsKey: m[0], key: m[1], idx: Number(m[2]), secs, dur: isFinite(d) && d > 0 ? d : 0, at: cwAt[fk] || 0 });
    }
  } catch { return []; }
  cwScanned = true;
  if (dirty) {
    const live = new Set(out.map((e) => e.fk));
    for (const k of Object.keys(cwAt)) if (!live.has(k)) delete cwAt[k];
    try { ls.setItem('tide:cwAt', JSON.stringify(cwAt)); } catch { /* ignore */ }
  }
  return out;
}

// [title line, episode/year] for a resume entry.
function cwLabel(e) {
  const t = rows.get(e.key).last, m = infoOf(e.key), eps = packOf(e.key);
  const f = eps && eps.find((x) => x.file_index === e.idx);
  if (f && f.episode != null) return `${showName(t)} · ${code(f.season, f.episode)}`;
  if (matched(m) && m.media_type === 'tv') return m.season != null && m.episode != null ? `${m.title} · ${code(m.season, m.episode)}` : m.title;
  const p = parsed.get(t.name);
  if (!matched(m) && p && p.is_tv && p.episode != null) return `${p.title} · ${code(p.season, p.episode)}`;
  return mediaTitle(t, m);
}
const clock = (s) => { s = Math.floor(s); const hh = Math.floor(s / 3600), mm = Math.floor(s / 60) % 60; return `${hh ? `${hh}:${pad2(mm)}` : mm}:${pad2(s % 60)}`; };
function posterFor(key) {
  const r = rows.get(key);
  if (r && r.artUrl) return r.artUrl;
  const gid = r && r.group, gr = gid && lastGroups.get(gid);
  const w = gr && gr.keys.map((k) => rows.get(k)).find((x) => x && x.artUrl);
  return w ? w.artUrl : null;
}

function createCwCard(e) {
  const C = { el: h('div', 'cwcard'), pic: h('div', 'cwpic'), title: h('div', 'cwtitle'), sub: h('div', 'cwsub'), bar: h('div', 'cwbar'), fill: h('i') };
  C.el.setAttribute('role', 'button');
  C.el.tabIndex = 0;
  const text = h('div', 'cwtext');
  text.append(C.title, C.sub);
  C.bar.append(C.fill);
  const x = h('button', 'cwx', '×');
  x.type = 'button';
  x.title = 'Remove from Continue watching';
  x.addEventListener('click', (ev) => {
    ev.stopPropagation();
    try { localStorage.removeItem(e.lsKey); localStorage.removeItem(e.lsKey.replace('tide:resume:', 'tide:dur:')); } catch { /* ignore */ }
    if (resumeList) {
      resumeList = resumeList.filter((r) => `${r.key}:${r.file_index}` !== e.fk);
      invoke('resume_set', { key: e.key, fileIndex: e.idx, pos: 0, dur: 0 }).catch(() => {});
    }
    renderContinue();
  });
  const rb = remoteBtn(() => [e.key, e.idx], () => e.key);
  rb.classList.add('cwr');
  C.el.append(C.pic, text, x, rb, C.bar);
  C.el.addEventListener('click', () => play(e.key, e.idx));
  C.el.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') play(e.key, e.idx); });
  return C;
}

function renderContinue() {
  const entries = resumeEntries()
    .filter((e) => { const r = rows.get(e.key); return r && r.last && !watched[e.fk]; })
    .sort((a, b) => b.at - a.at || a.fk.localeCompare(b.fk))
    .slice(0, CW_MAX);
  cwEl.hidden = !entries.length;
  const seen = new Set();
  entries.forEach((e, i) => {
    seen.add(e.fk);
    let C = cwCards.get(e.fk);
    if (!C) { C = createCwCard(e); cwCards.set(e.fk, C); }
    const title = cwLabel(e);
    if (C.title.textContent !== title) { C.title.textContent = title; C.el.title = title; }
    const info = infoOf(e.key), ro = remoteOnly(e.key);
    const dur = e.dur || (info && info.runtime_min ? info.runtime_min * 60 : 0);
    const sub = (dur && e.secs < dur ? eta(dur - e.secs) : `Resume at ${clock(e.secs)}`) + (ro ? ` · on ${shortDev(ro.dev.name)}` : '');
    if (C.sub.textContent !== sub) C.sub.textContent = sub;
    const w = `${dur ? Math.min(100, (e.secs / dur) * 100).toFixed(1) : 0}%`;
    if (C.fill.style.width !== w) C.fill.style.width = w;
    C.bar.hidden = !dur;
    const url = posterFor(e.key);
    if (C.url !== url) {
      C.url = url;
      C.pic.style.backgroundImage = url ? `url("${url}")` : '';
      C.pic.classList.toggle('none', !url);
    }
    if (cwStrip.children[i] !== C.el) cwStrip.insertBefore(C.el, cwStrip.children[i] || null);
  });
  for (const [fk, C] of cwCards) if (!seen.has(fk)) { C.el.remove(); cwCards.delete(fk); }
}
// The player mirrors positions into localStorage, so its writes also arrive here.
window.addEventListener('storage', (e) => { if (!e.key || RESUME_RE.test(e.key)) loadResumeSoon(); });

// ---------- library view (poster grid) ----------
let view = 'list';
try { if (localStorage.getItem('tide:view') === 'library') view = 'library'; } catch { /* ignore */ }
const seg = h('div', 'seg');
seg.setAttribute('role', 'tablist');
const segBtn = (v, label) => {
  const b = h('button', null, label);
  b.type = 'button';
  b.addEventListener('click', () => setView(v));
  seg.append(b);
  return b;
};
const segList = segBtn('list', 'List');
const segLib = segBtn('library', 'Library');
wGear.before(seg);

const lib = h('main');
lib.id = 'lib';
const libGrid = h('div', 'lgrid');
const libEmpty = h('p', 'libempty', 'No movies or shows yet');
lib.append(libGrid, libEmpty);
$('list').after(lib);
const libTiles = new Map(); // tile id -> tile

function setView(v, save = true) {
  view = v === 'library' ? 'library' : 'list';
  document.body.classList.toggle('lib-on', view === 'library');
  segList.classList.toggle('on', view === 'list');
  segLib.classList.toggle('on', view === 'library');
  if (save) try { localStorage.setItem('tide:view', view); } catch { /* ignore */ }
  closeMenu();
  if (view === 'library') renderLibrary();
}

const downloadingish = (t) => ['downloading', 'initializing', 'paused'].includes(t.state) && t.progress < 1;

function libItems() {
  const items = [];
  const seenG = new Set();
  for (const t of lastList) {
    const r = rows.get(t.key);
    if (!r || !r.last) continue;
    if (r.group) {
      if (seenG.has(r.group)) continue;
      seenG.add(r.group);
      const gr = lastGroups.get(r.group);
      if (!gr) continue;
      let eps = 0, seen = 0, total = 0, done = 0, live = false;
      for (const k of gr.keys) {
        const x = rows.get(k).last, pk = packOf(k);
        if (pk) { eps += pk.length; seen += pk.filter((f) => watched[`${k}:${f.file_index}`]).length; } else { eps++; if (isWatched(k)) seen++; }
        total += x.total_bytes; done += x.progress_bytes;
        if (downloadingish(x)) live = true;
      }
      const G = groupEls.get(gr.gid);
      const best = gr.keys.map((k) => rows.get(k)).filter((x) => x.artUrl).sort((a, b) => b.artPri - a.artPri)[0];
      const rems = gr.keys.map((k) => rows.get(k).last.remote);
      items.push({ id: `s:${gr.keys[0]}`, series: true, keys: gr.keys, name: (G && G.name.textContent) || gr.name,
        remote: rems.every(Boolean) ? rems[0] : null,
        sub: `${plural(eps, 'episode')}${seen ? ` · ${seen} watched` : ''}`, url: best ? best.artUrl : null,
        progress: live && total ? done / total : null, watched: eps > 0 && seen >= eps });
      continue;
    }
    if (t.state === 'resolving') continue;
    const m = infoOf(t.key), p = parsed.get(t.name);
    if (!(matched(m) || r.artUrl || (p && (p.year || p.is_tv)))) continue;
    let name, sub;
    if (matched(m)) {
      name = m.title;
      sub = m.media_type === 'tv' && m.season != null && m.episode != null ? code(m.season, m.episode) : m.year ? String(m.year) : '';
    } else {
      name = (p && p.title) || t.name;
      sub = p && p.is_tv && p.episode != null ? code(p.season, p.episode) : p && p.year ? String(p.year) : '';
    }
    const rt = rating(m);
    if (rt) sub = sub ? `${sub} · ★ ${rt}` : `★ ${rt}`;
    items.push({ id: `t:${t.key}`, key: t.key, name, sub, url: r.artUrl || null, remote: t.remote || null,
      progress: downloadingish(t) ? t.progress : null, watched: isWatched(t.key) });
  }
  return items;
}

function createTile(it) {
  const L = { el: h('div', 'ltile'), pic: h('div', 'lpic'), ph: h('div', 'lph'), name: h('div', 'lname'), sub: h('div', 'lsub'),
    bar: h('div', 'lbar'), fill: h('i'), badge: h('span', 'lbadge', '✓'), dev: h('span', 'devtag ldev') };
  L.el.setAttribute('role', 'button');
  L.el.tabIndex = 0;
  L.bar.append(L.fill);
  L.dev.hidden = true;
  L.pic.append(L.ph, L.badge, L.dev, L.bar);
  L.el.append(L.pic, L.name, L.sub);
  const open = () => (L.item.series ? showSeries(L.item.keys) : openSheet(L.item.key));
  L.el.addEventListener('click', open);
  L.el.addEventListener('keydown', (e) => { if (e.key === 'Enter') open(); });
  return L;
}

function renderLibrary() {
  const items = libItems();
  const seen = new Set();
  items.forEach((it, i) => {
    seen.add(it.id);
    let L = libTiles.get(it.id);
    if (!L) { L = createTile(it); libTiles.set(it.id, L); }
    L.item = it;
    if (L.name.textContent !== it.name) { L.name.textContent = it.name; L.ph.textContent = it.name; L.el.title = it.name; }
    if (L.sub.textContent !== it.sub) L.sub.textContent = it.sub;
    if (L.url !== it.url) {
      L.url = it.url;
      L.pic.style.backgroundImage = it.url ? `url("${it.url}")` : '';
      L.pic.classList.toggle('none', !it.url);
    }
    L.bar.hidden = it.progress == null;
    const w = `${((it.progress || 0) * 100).toFixed(2)}%`;
    if (L.fill.style.width !== w) L.fill.style.width = w;
    L.badge.hidden = !it.watched;
    setDevTag(L.dev, it.remote);
    const cls = `ltile${it.remote ? ' remote' : ''}`;
    if (L.el.className !== cls) L.el.className = cls;
    if (libGrid.children[i] !== L.el) libGrid.insertBefore(L.el, libGrid.children[i] || null);
  });
  for (const [id, L] of libTiles) if (!seen.has(id)) { L.el.remove(); libTiles.delete(id); }
  libEmpty.hidden = !!items.length || !lastList.length;
}

// Movie -> its detail panel in an overlay sheet (same renderer as the list).
const libSheet = h('div', 'lsheet');
const libBox = h('div', 'lbox');
const libClose = h('button', 'wx lclose', '×');
libClose.type = 'button';
libClose.title = 'Close';
libClose.addEventListener('click', closePanel);
libBox.append(libClose);
libSheet.append(libBox);
libSheet.addEventListener('mousedown', (e) => { if (e.target === libSheet) closePanel(); });
document.body.append(libSheet);

function openSheet(key) {
  closePanel();
  const r = rows.get(key);
  if (!r) return;
  openKey = key;
  r.panel = buildPanel(key);
  libBox.append(r.panel.el);
  libBox.scrollTop = 0;
  document.body.classList.add('sheet-open');
  updatePanel(key);
  loadDetails(key, r);
}

// Series -> the group in the list view, expanded and briefly highlighted.
function showSeries(keys) {
  const r = keys.map((k) => rows.get(k)).find((x) => x && x.group);
  setView('list');
  if (!r) return;
  const gid = r.group;
  expanded.add(gid);
  render(lastList);
  const G = groupEls.get(gid);
  if (!G) return;
  G.el.scrollIntoView({ block: 'start' });
  G.head.classList.add('hl');
  setTimeout(() => G.head.classList.remove('hl'), 1200);
}
setView(view, false);

// =====================================================================
// Mock backend: lets the UI run in a plain browser (no window.__TAURI__).
// URL hash flags for previews: #empty #open #adding #confirm #toast #hover
// =====================================================================
function mockInvoke(cmd, args) { return MOCK.invoke(cmd, args || {}); }
function mockListen(name, cb) { MOCK.listeners.push(cb); return Promise.resolve(() => {}); }

var MOCK = T ? null : (() => {
  const GB = 1e9, MB = 1e6;
  const hash = (n) => n.toString(16).padStart(40, 'a');
  const mk = (n, name, state, total, progress, extra) => ({
    key: hash(n), id: state === 'resolving' ? null : n, name, state, error: null,
    total_bytes: total, progress_bytes: Math.round(total * progress), progress, uploaded_bytes: 0,
    down_bps: 0, up_bps: 0, peers: 0, eta_secs: null,
    output_path: state === 'resolving' ? null : `/Users/me/Downloads/${name}`, ...extra,
  });
  const empty = location.hash.includes('empty');
  const list = empty ? [] : [
    mk(1, 'Big Buck Bunny (2008) 1080p', 'downloading', 4.2 * GB, 0.45, { down_bps: 2.1 * MB, up_bps: 140e3, peers: 12 }),
    mk(2, 'Sintel.2010.2160p.mkv', 'completed', 3.9 * GB, 1),
    mk(3, 'ubuntu-24.04.1-desktop-amd64.iso', 'paused', 6.1 * GB, 0.18),
    mk(4, 'Tears of Steel', 'resolving', 0, 0),
    mk(5, 'Cosmos Laundromat – First Cycle', 'error', 1.6 * GB, 0.07, { error: 'Tracker unreachable: connection timed out' }),
    mk(6, 'The Daily Podcast – Season 3 Complete', 'downloading', 812 * MB, 0.83, { down_bps: 640e3, peers: 31 }),
    mk(7, 'Severance.S02E05.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb', 'downloading', 2.4 * GB, 0.62, { down_bps: 3.4 * MB, up_bps: 90e3, peers: 22, uploaded_bytes: 310 * MB }),
    mk(8, 'The.Lighthouse.Keepers.S01E01.1080p.WEB-DL', 'completed', 1.4 * GB, 1),
    mk(9, 'The.Lighthouse.Keepers.S01E02.1080p.WEB-DL', 'completed', 1.5 * GB, 1),
    mk(10, 'The.Lighthouse.Keepers.S01E03.1080p.WEB-DL', 'downloading', 1.5 * GB, 0.38, { down_bps: 1.8 * MB, peers: 17 }),
    mk(11, 'The Lighthouse Keepers S01 Part 2 1080p', 'completed', 4.4 * GB, 1),
    mk(12, 'Northern.Lights.S02.COMPLETE.1080p.WEB-DL', 'downloading', 9.6 * GB, 0.41, { down_bps: 4.2 * MB, peers: 38 }),
  ];
  const files = {
    [hash(1)]: [['Big Buck Bunny.mp4', 'video', 4.1 * GB], ['Big Buck Bunny.en.srt', 'subtitle', 48e3], ['Big Buck Bunny.tr.srt', 'subtitle', 51e3], ['poster.jpg', 'other', 820e3]],
    [hash(2)]: [['Sintel.2010.2160p.mkv', 'video', 3.9 * GB], ['Sintel.en.srt', 'subtitle', 40e3]],
    [hash(3)]: [['ubuntu-24.04.1-desktop-amd64.iso', 'other', 6.1 * GB]],
    [hash(5)]: [['Cosmos Laundromat.mkv', 'video', 1.6 * GB]],
    [hash(7)]: [['Severance.S02E05.1080p.mkv', 'video', 2.4 * GB], ['Severance.S02E05.en.srt', 'subtitle', 60e3]],
    [hash(8)]: [['The.Lighthouse.Keepers.S01E01.mp4', 'video', 1.4 * GB]],
    [hash(9)]: [['The.Lighthouse.Keepers.S01E02.mp4', 'video', 1.5 * GB]],
    [hash(10)]: [['The.Lighthouse.Keepers.S01E03.mp4', 'video', 1.5 * GB]],
    [hash(11)]: [4, 5, 6].map((e) => [`The.Lighthouse.Keepers.S01E0${e}.1080p.mp4`, 'video', 1.45 * GB]).concat([['Sample/lighthouse.sample.mp4', 'video', 20 * MB]]),
    [hash(12)]: [...Array.from({ length: 6 }, (_, i) => [`Northern.Lights.S02/Northern.Lights.S02E0${i + 1}.1080p.WEB-DL.mkv`, 'video', 1.6 * GB]),
      ['Northern.Lights.S02/Northern.Lights.S02E01.en.srt', 'subtitle', 52e3], ['Northern.Lights.S02/Sample/northern.lights.s02e01.sample.mkv', 'video', 30 * MB]],
    [hash(6)]: Array.from({ length: 8 }, (_, i) => [`Season 3/Episode ${String(i + 1).padStart(2, '0')}.mp3`, 'audio', 101 * MB]),
  };
  // Tiny stand-in for the Rust name parser (mock only).
  const mockParse = (name) => {
    const s = name.replace(/\.(mkv|mp4|m4v|avi|mp3)$/i, '');
    const se = /^(.*?)[\s._-]+s(\d{1,2})(?:[\s._-]?e(\d{1,3}))?(?=[\s._-]|$)/i.exec(s) || /^(.*?)[\s._-]+(\d{1,2})x(\d{2,3})\b/i.exec(s);
    const yr = /^(.*?)[\s._(-]+((?:19|20)\d{2})\b/.exec(s);
    const raw = se ? se[1] : yr ? yr[1] : s;
    const title = raw.replace(/[._]+/g, ' ').trim();
    const norm = title.toLowerCase().replace(/[^a-z0-9]+/g, ' ').trim().replace(/^(the|a|an) /, '');
    return { title, norm, year: yr ? +yr[2] : null, season: se ? +se[2] : null, episode: se && se[3] ? +se[3] : null, is_tv: !!se };
  };
  // Mock resume positions for "Continue watching" (seeded once; #nocw skips).
  try {
    if (!location.hash.includes('nocw') && !Object.keys(localStorage).some((k) => k.startsWith('tide:resume:'))) {
      [[7, 0, 1260], [9, 0, 1530], [12, 2, 610], [1, 0, 240], [8, 0, 900]]
        .forEach(([n, i, s]) => localStorage.setItem(`tide:resume:${hash(n)}:${i}`, String(s)));
    }
  } catch { /* ignore */ }
  const listeners = [];
  const groupsFile = { groups: [], excluded: {} };
  const watchedFile = { [`${hash(8)}:0`]: true };
  let gseq = 0;
  const watcherFile = { settings: { torznab_url: location.hash.includes('nosearch') ? '' : 'http://localhost:9696/1/', api_key: 'mock', auto_add: false, quality: '1080p' },
    unfollowed: [], last_check: 0, seen: {} };
  const mres = (title, size, seeders, quality, n) => ({ title, size, seeders, quality, magnet: `magnet:?xt=urn:btih:${hash(100 + n)}&dn=${encodeURIComponent(title)}` });
  const MOCK_NEW_EPS = [
    { imdb_id: 'tt9000001', show: 'The Lighthouse Keepers', season: 1, episode: 7, episode_title: 'Slack Water', released: '2026-10-14T05:00:00.000Z', results: [
      mres('The.Lighthouse.Keepers.S01E07.1080p.WEB-DL.DDP5.1.H.264', 2.1 * GB, 340, '1080p', 1),
      mres('The.Lighthouse.Keepers.S01E07.1080p.WEB.h265', 1.2 * GB, 85, '1080p', 2),
      mres('The.Lighthouse.Keepers.S01E07.720p.WEB.x264', 980 * MB, 610, '720p', 3),
      mres('The.Lighthouse.Keepers.S01E07.2160p.WEB-DL.HDR', 6.4 * GB, 41, '2160p', 4)] },
    { imdb_id: 'tt11280740', show: 'Severance', season: 2, episode: 6, episode_title: 'Attila', released: '2026-10-15T05:00:00.000Z', results: [] },
  ];
  const detach = (key) => { groupsFile.groups.forEach((g) => { g.members = g.members.filter((k) => k !== key); }); delete groupsFile.excluded[key]; };
  const find = (key) => list.find((t) => t.key === key);
  const err = (s) => Promise.reject(s);

  setInterval(() => {
    for (const t of list) {
      if (t.state !== 'downloading') continue;
      t.down_bps = Math.max(80e3, t.down_bps * (0.85 + Math.random() * 0.3));
      t.progress_bytes = Math.min(t.total_bytes, t.progress_bytes + t.down_bps);
      t.progress = t.progress_bytes / t.total_bytes;
      t.eta_secs = Math.round((t.total_bytes - t.progress_bytes) / t.down_bps);
      if (t.progress >= 1) Object.assign(t, { state: 'completed', down_bps: 0, up_bps: 0, peers: 0, eta_secs: null });
    }
    listeners.forEach((cb) => cb({ payload: list.map((t) => ({ ...t })) }));
  }, 1000);

  const netFile = { detected: true, metered: true, expensive: true, constrained: false, mode: 'pause', limit_kbps: 200,
    paused_by_netmode: [hash(1), hash(2), hash(3)], ssid: 'Pixel 7', network_id: 'ssid:Pixel 7', manual: null };
  let notifyOn = true;
  let arAuto = true;
  let arQueue = [];
  const ago = (d) => Math.floor(Date.now() / 1000) - d * 86400;
  let cleanupItems = [
    { key: hash(1), file_index: null, name: 'Big Buck Bunny (2008)', size: 2.4 * GB, completed_at: ago(45) },
    { key: hash(2), file_index: 3, name: 'Sintel S01E03.mkv', size: 1.8 * GB, completed_at: ago(62) },
  ];
  const rDevs = [{ id: 'd1', name: "mshn's Mac mini", host: '192.168.1.30', port: 47900, online: true }];
  let rEnabled = true;
  const rStatus = () => ({ enabled: rEnabled, port: rEnabled ? 47900 : null, name: 'MacBook Air', lanIp: '192.168.1.20', devices: rDevs.map((d) => ({ ...d })) });
  // Mac↔Mac sync: two items stored only on the Mac mini (a movie, and a Severance episode
  // that groups with the local S02E05), plus resume positions from both Macs.
  const R1 = hash(20), R2 = hash(21);
  const rLibRows = [
    { ...mk(20, 'Elephants.Dream.2006.1080p.BluRay.x264', 'completed', 1.1 * GB, 1), output_path: '/Users/mshn/Tidebox/Elephants Dream (2006)' },
    { ...mk(21, 'Severance.S02E04.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb', 'completed', 2.3 * GB, 1), output_path: '/Users/mshn/Tidebox/Severance S02E04' },
  ];
  const nowS = () => Math.floor(Date.now() / 1000);
  const resumeDb = {}; // "key:idx" -> { pos, dur, updated_at, device }
  [[7, 0, 1260, 3000, 'MacBook Air', 60], [9, 0, 1530, 2700, 'MacBook Air', 3600], [12, 2, 610, 3300, 'MacBook Air', 7200], [1, 0, 240, 600, 'MacBook Air', 9000],
    [21, 0, 1980, 3120, "mshn's Mac mini", 30]].forEach(([n, i, pos, dur, device, ago]) => {
    resumeDb[`${hash(n)}:${i}`] = { pos, dur, updated_at: nowS() - ago, device };
  });
  let lastSync = nowS() - 120;
  const api = {
    remote_library: () => [{ device_id: 'd1', device_name: "mshn's Mac mini", online: true, rows: rLibRows.map((t) => ({ ...t })),
      media: {
        [R1]: { ...base, imdb_id: 'tt0807840', media_type: 'movie', title: 'Elephants Dream', year: 2006, genres: ['Animation', 'Short', 'Sci-Fi'], runtime_min: 11, imdb_rating: 6.5,
          overview: 'Two strange characters explore a capricious and seemingly infinite machine.' },
        [R2]: { ...base, imdb_id: 'tt11280740', media_type: 'tv', title: 'Severance', year: 2022, season: 2, episode: 4, episode_title: 'Woe’s Hollow',
          genres: ['Drama', 'Mystery', 'Sci-Fi'], runtime_min: 52, imdb_rating: 8.7, overview: 'Mark leads a team of office workers whose memories have been surgically divided between their work and personal lives.' },
      },
      files: {
        [R1]: [{ index: 0, name: 'Elephants.Dream.2006.1080p.mp4', size: 1.1 * GB, progress_bytes: 1.1 * GB, progress: 1, kind: 'video', playable_builtin: true }],
        [R2]: [{ index: 0, name: 'Severance.S02E04.1080p.mkv', size: 2.3 * GB, progress_bytes: 2.3 * GB, progress: 1, kind: 'video', playable_builtin: false },
          { index: 1, name: 'Severance.S02E04.en.srt', size: 58e3, progress_bytes: 58e3, progress: 1, kind: 'subtitle', playable_builtin: false }],
      } }],
    remote_play_local: () => null,
    remote_art: ({ key }) => (key === R1 ? ED_POSTER : key === R2 ? MOCK_MEDIA_POSTERS[hash(7)] : err('NOT_FOUND: no image')),
    sync_status: () => ({ devices: rDevs.map((d) => ({ id: d.id, name: d.name, online: d.online, last_sync: lastSync, error: d.id === 'd2' ? 'NETWORK: connection refused' : null })) }),
    sync_now: () => { lastSync = nowS(); return null; },
    resume_list: () => Object.entries(resumeDb).map(([fk, v]) => ({ key: fk.split(':')[0], file_index: Number(fk.split(':')[1]), ...v }))
      .sort((a, b) => b.updated_at - a.updated_at),
    resume_get: ({ key, fileIndex }) => { const v = resumeDb[`${key}:${fileIndex}`]; return v ? { pos: v.pos, dur: v.dur, updated_at: v.updated_at } : null; },
    resume_set: ({ key, fileIndex, pos, dur }) => {
      if (!pos) delete resumeDb[`${key}:${fileIndex}`];
      else resumeDb[`${key}:${fileIndex}`] = { pos, dur, updated_at: nowS(), device: 'MacBook Air' };
      return null;
    },
    remote_status: rStatus,
    archive_settings: () => ({ archive_dir: null, available: false, free_bytes: null, error: null, autoclean_days: null, archive_auto: arAuto,
      device: { id: 'd1', name: "mshn's Mac mini", online: true, available: true, archive_dir: '/Volumes/NEO Storage/Tidebox', free_bytes: 185e9 } }),
    archive_set_settings: ({ archiveAuto }) => { if (archiveAuto != null) arAuto = archiveAuto; return api.archive_settings(); },
    archive_status: () => ({ queue: arQueue, device: api.archive_settings().device, auto: arAuto }),
    archive_move: ({ key }) => { arQueue = [{ kind: 'torrent', key, name: 'Animals (2026)', bytes_done: 1.2e9, bytes_total: 1.7e9, state: 'moving', error: null }]; return api.archive_status(); },
    remote_set_enabled: ({ enabled }) => { rEnabled = enabled; return rStatus(); },
    remote_discover: () => [{ id: 'd1', name: "mshn's Mac mini", host: '192.168.1.30', port: 47900, paired: !!rDevs.find((d) => d.id === 'd1') },
      { id: 'd2', name: 'Living Room iMac', host: '192.168.1.41', port: 47900, paired: !!rDevs.find((d) => d.id === 'd2') }],
    remote_pair_start: ({ host }) => ({ id: 'd2', name: host === '192.168.1.30' ? "mshn's Mac mini" : 'Living Room iMac' }),
    remote_add_manual: ({ host }) => ({ id: 'd2', name: `Mac at ${host}` }),
    remote_pair_finish: ({ host, port, code }) => {
      if (code !== '123456') return err('BAD_CODE: wrong code — try again');
      const d = { id: 'd2', name: 'Living Room iMac', host, port, online: true };
      rDevs.push(d); return d;
    },
    remote_unpair: ({ id }) => { const i = rDevs.findIndex((d) => d.id === id); if (i >= 0) rDevs.splice(i, 1); return null; },
    remote_play: ({ deviceId }) => (rDevs.find((d) => d.id === deviceId) ? null : err('NOT_FOUND: device is not paired')),
    list_torrents: () => list.map((t) => ({ ...t })),
    add_magnet: ({ magnet }) => {
      const m = /btih:([0-9a-f]{40})/i.exec(magnet) || /^([0-9a-f]{40})$/i.exec(magnet);
      if (!m) return err('INVALID_INPUT: could not parse magnet link');
      const key = m[1].toLowerCase();
      if (find(key)) return { key, name: find(key).name, already_exists: true };
      const dn = /[?&]dn=([^&]+)/.exec(magnet);
      const name = dn ? decodeURIComponent(dn[1].replace(/\+/g, ' ')) : key;
      list.push({ ...mk(0, name, 'resolving', 0, 0), key });
      return { key, name, already_exists: false };
    },
    pause_torrent: ({ key }) => { Object.assign(find(key), { state: 'paused', down_bps: 0, up_bps: 0, peers: 0, eta_secs: null }); return null; },
    resume_torrent: ({ key }) => { const t = find(key); if (t.progress >= 1) return err('COMPLETED: already finished'); Object.assign(t, { state: 'downloading', down_bps: 1.2 * MB, peers: 9, error: null }); return null; },
    remove_torrent: ({ key }) => { list.splice(list.indexOf(find(key)), 1); return null; },
    reveal_torrent: () => null,
    get_download_dir: () => '/Users/me/Downloads',
    list_files: ({ key }) => {
      const t = find(key);
      if (!t) return err('NOT_FOUND: no such torrent');
      if (t.state === 'resolving') return err('NOT_READY: metadata not resolved yet');
      return (files[key] || []).map(([name, kind, size], index) => {
        const progress = Math.min(1, t.progress * (1 + index * 0.1));
        return { index, name, size, progress_bytes: Math.round(size * progress), progress, kind,
          playable_builtin: /\.(mp4|m4v|mov|webm|mp3|m4a|aac|wav|flac)$/i.test(name) };
      });
    },
    parse_names: ({ names }) => names.map(mockParse),
    list_episodes: ({ key }) => Promise.resolve(api.list_files({ key })).then((fs) => fs
      .filter((f) => f.kind === 'video' && !/(^|\/)sample/i.test(f.name))
      .map((f) => ({ f, p: mockParse(f.name.split('/').pop()) }))
      .filter(({ p }) => p.episode != null)
      .map(({ f, p }) => ({ file_index: f.index, season: p.season, episode: p.episode, title_hint: p.title, norm: p.norm, size: f.size, progress: f.progress }))),
    play_media: ({ media }) => (/\.mkv$/i.test(files[media.key][media.fileIndex][0]) ? err('VLC_NOT_FOUND: VLC is not installed') : null),
    open_external: () => null,
    get_artwork: ({ key }) => {
      const t = find(key);
      if (!t) return err('NOT_FOUND: no such torrent');
      if (t.state === 'resolving') return err('NOT_READY: metadata not resolved yet');
      const p = MOCK_POSTERS[key];
      return new Promise((res) => setTimeout(() => res(p ? { poster: p, kind: 'poster' } : { poster: null, kind: null }), 400));
    },
    get_media_info: ({ key }) => {
      const t = find(key);
      if (!t) return err('NOT_FOUND: no such torrent');
      if (t.state === 'resolving') return err('NOT_READY: waiting for files');
      return new Promise((res) => setTimeout(() => res(MOCK_INFO[key] || null), 300));
    },
    get_media_image: ({ key }) => (MOCK_INFO[key] && MOCK_MEDIA_POSTERS[key]) || err('NOT_FOUND: no sidecar image'),
    search_media: ({ query }) => MOCK_RESULTS.filter((x) => x.title.toLowerCase().includes(query.toLowerCase().split(' ')[0])),
    set_media_match: ({ key, imdbId }) => {
      const x = MOCK_RESULTS.find((r) => r.imdb_id === imdbId);
      MOCK_INFO[key] = { ...x, manual: true, needs_match: false };
      MOCK_MEDIA_POSTERS[key] = x.poster_url;
      return MOCK_INFO[key];
    },
    clear_media_match: ({ key }) => { delete MOCK_INFO[key]; return null; },
    list_groups: () => JSON.parse(JSON.stringify(groupsFile)),
    create_group: ({ name, keys }) => {
      keys.forEach(detach);
      const g = { id: `g${++gseq}`, name, members: [...new Set(keys)] };
      groupsFile.groups.push(g);
      return { ...g };
    },
    rename_group: ({ id, name }) => { groupsFile.groups.find((g) => g.id === id).name = name; return null; },
    add_to_group: ({ id, key }) => { detach(key); groupsFile.groups.find((g) => g.id === id).members.push(key); return null; },
    remove_from_group: ({ key }) => { detach(key); groupsFile.excluded[key] = true; return null; },
    delete_group: ({ id }) => { groupsFile.groups = groupsFile.groups.filter((g) => g.id !== id); return null; },
    get_watched: () => ({ ...watchedFile }),
    set_watched: ({ key, fileIndex, watched: w }) => { if (w) watchedFile[`${key}:${fileIndex}`] = true; else delete watchedFile[`${key}:${fileIndex}`]; return null; },
    watcher_get_settings: () => ({ ...watcherFile.settings, unfollowed: [...watcherFile.unfollowed], last_check: watcherFile.last_check }),
    watcher_set_settings: ({ torznabUrl, apiKey, autoAdd, quality }) => {
      if (torznabUrl && !/^https?:\/\//.test(torznabUrl)) return err('INVALID_INPUT: Torznab URL must start with http:// or https://');
      watcherFile.settings = { torznab_url: torznabUrl, api_key: apiKey, auto_add: autoAdd, quality };
      return api.watcher_get_settings();
    },
    watcher_check: () => {
      watcherFile.last_check = Math.floor(Date.now() / 1000);
      const search = !!watcherFile.settings.torznab_url;
      return MOCK_NEW_EPS
        .filter((n) => !watcherFile.unfollowed.includes(n.imdb_id) && !watcherFile.seen[`${n.imdb_id}:${n.season}:${n.episode}`])
        .map((n) => ({ ...n, results: search ? n.results : [] }));
    },
    watcher_follow: ({ imdbId }) => { watcherFile.unfollowed = watcherFile.unfollowed.filter((x) => x !== imdbId); return null; },
    watcher_unfollow: ({ imdbId }) => { if (!watcherFile.unfollowed.includes(imdbId)) watcherFile.unfollowed.push(imdbId); return null; },
    watcher_dismiss: ({ imdbId, season, episode }) => { watcherFile.seen[`${imdbId}:${season}:${episode}`] = 'dismissed'; return null; },
    net_status: () => ({ ...netFile }),
    net_set_mode: ({ mode, limitKbps }) => { netFile.mode = mode; if (limitKbps) netFile.limit_kbps = limitKbps; if (mode === 'off') netFile.paused_by_netmode = []; return { ...netFile }; },
    net_mark_current: ({ metered }) => { netFile.manual = metered; netFile.metered = metered == null ? true : metered; return { ...netFile }; },
    notify_get_settings: () => ({ enabled: notifyOn }),
    notify_set_settings: ({ enabled }) => { notifyOn = enabled; return { enabled }; },
    cleanup_candidates: () => {
      const items = cleanupItems.slice();
      return { items, total_bytes: items.reduce((a, c) => a + c.size, 0) };
    },
    cleanup_apply: ({ items }) => {
      const removed = cleanupItems.filter((c) => items.some((i) => i.key === c.key && i.fileIndex === c.file_index));
      cleanupItems = cleanupItems.filter((c) => !removed.includes(c));
      return { removed, skipped: [], freed_bytes: removed.reduce((a, c) => a + c.size, 0) };
    },
    get_torrent_details: ({ key }) => ({ info_hash: key, magnet: `magnet:?xt=urn:btih:${key}&dn=${encodeURIComponent(find(key).name)}`, trackers: 7, save_path: find(key).output_path }),
  };

  const svgUrl = (body) => `data:image/svg+xml;utf8,${encodeURIComponent(
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 300">${body}</svg>`)}`;
  const MOCK_POSTERS = {
    [hash(1)]: svgUrl(`<defs><linearGradient id="s" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#7ec8f0"/><stop offset=".7" stop-color="#d9f1c8"/></linearGradient></defs>
      <rect width="200" height="300" fill="url(#s)"/><circle cx="150" cy="70" r="26" fill="#ffe27a"/>
      <path d="M0 210 Q60 160 120 200 T200 190 V300 H0z" fill="#6dbb4f"/><path d="M0 240 Q80 205 200 245 V300 H0z" fill="#3f8f3a"/>
      <ellipse cx="100" cy="222" rx="30" ry="24" fill="#f4f1ea"/><ellipse cx="88" cy="186" rx="7" ry="24" fill="#f4f1ea"/><ellipse cx="110" cy="186" rx="7" ry="24" fill="#f4f1ea"/>
      <text x="100" y="278" text-anchor="middle" font-family="Helvetica" font-weight="700" font-size="22" fill="#fff">BIG BUCK BUNNY</text>`),
    [hash(2)]: svgUrl(`<defs><linearGradient id="s" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#1b2440"/><stop offset=".6" stop-color="#8a4b2a"/><stop offset="1" stop-color="#e2a35a"/></linearGradient></defs>
      <rect width="200" height="300" fill="url(#s)"/><circle cx="100" cy="190" r="60" fill="#f2c27a" opacity=".55"/>
      <path d="M40 120 L100 150 L160 112 L120 160 L150 200 L100 172 L52 205 L80 160z" fill="#1a1414"/>
      <path d="M0 260 L50 220 L90 250 L140 210 L200 255 V300 H0z" fill="#120d0c"/>
      <text x="100" y="60" text-anchor="middle" font-family="Georgia" font-size="34" letter-spacing="6" fill="#f6e3c4">SINTEL</text>`),
  };

  const base = { release_info: null, poster_url: null, imdb_url: null, season: null, episode: null, episode_title: null,
    episode_overview: null, air_date: null, manual: false, needs_match: false, match_note: null };
  const MOCK_INFO = {
    [hash(1)]: { ...base, imdb_id: 'tt1254207', media_type: 'movie', title: 'Big Buck Bunny', year: 2008, genres: ['Animation', 'Short', 'Comedy'],
      runtime_min: 10, imdb_rating: 6.4, imdb_url: 'https://www.imdb.com/title/tt1254207/',
      overview: 'A large and lovable rabbit deals with three tiny bullies, led by a flying squirrel, who are determined to squelch his happiness. What follows is a gentle, slapstick revenge plot played out across a sunny meadow, with nuts, traps and one very confused butterfly. Made by the Blender Institute as an open movie, every frame and asset is freely available.' },
    [hash(7)]: { ...base, imdb_id: 'tt11280740', media_type: 'tv', title: 'Severance', year: 2022, season: 2, episode: 5, episode_title: "Trojan's Horse",
      genres: ['Drama', 'Mystery', 'Sci-Fi'], runtime_min: 50, imdb_rating: 8.7,
      overview: 'Mark leads a team of office workers whose memories have been surgically divided between their work and personal lives.',
      episode_overview: 'Mark tries to reconnect with an old colleague while Helly questions who she can trust on the severed floor.' },
    ...Object.fromEntries([[8, 'Lamp Oil'], [9, 'The Fog Bell'], [10, 'Low Tide']].map(([n, et], i) => [hash(n), { ...base, imdb_id: 'tt9000001',
      media_type: 'tv', title: 'The Lighthouse Keepers', year: 2025, season: 1, episode: i + 1, episode_title: et, genres: ['Drama'], runtime_min: 45,
      imdb_rating: 8.1, overview: 'Two keepers, one island, and a light that must never go out.' }])),
    [hash(11)]: { ...base, imdb_id: 'tt9000001', media_type: 'tv', title: 'The Lighthouse Keepers', year: 2025, season: 1, genres: ['Drama'], runtime_min: 45,
      imdb_rating: 8.1, overview: 'Two keepers, one island, and a light that must never go out.' },
    [hash(2)]: { ...base, imdb_id: '', media_type: 'movie', title: 'Sintel', year: 2010, genres: [], runtime_min: null, imdb_rating: null, overview: '', needs_match: true },
  };
  const MOCK_RESULTS = [
    { ...base, imdb_id: 'tt1727587', media_type: 'movie', title: 'Sintel', year: 2010, release_info: '2010', genres: ['Animation', 'Fantasy'], runtime_min: 15, imdb_rating: 6.9, overview: 'A lonely young woman, Sintel, helps and befriends a dragon, whom she calls Scales.' },
    { ...base, imdb_id: 'tt0000002', media_type: 'movie', title: 'Sintel: The Making Of', year: 2011, release_info: '2011', genres: ['Documentary'], runtime_min: 40, imdb_rating: 7.1, overview: '' },
  ];
  const LH = svgUrl(`<rect width="200" height="300" fill="#13243a"/><path d="M88 110h24l8 130H80z" fill="#e8e1d2"/><path d="M84 96h32v14H84z" fill="#f4c95d"/>
      <path d="M100 103 L200 70 V136z" fill="#f4c95d" opacity=".35"/><path d="M0 240 Q100 225 200 240 V300 H0z" fill="#0a1626"/>
      <text x="100" y="282" text-anchor="middle" font-family="Georgia" font-size="17" fill="#e8e1d2">THE LIGHTHOUSE KEEPERS</text>`);
  const MOCK_MEDIA_POSTERS = { [hash(8)]: LH, [hash(9)]: LH, [hash(10)]: LH, [hash(11)]: LH, [hash(1)]: MOCK_POSTERS[hash(1)], [hash(7)]: svgUrl(`<rect width="200" height="300" fill="#e9ecef"/><rect x="40" y="90" width="120" height="80" fill="#0b3d5c"/>
      <text x="100" y="250" text-anchor="middle" font-family="Helvetica" font-size="24" letter-spacing="4" fill="#0b3d5c">SEVERANCE</text>`) };
  const ED_POSTER = svgUrl(`<rect width="200" height="300" fill="#2b1d12"/><circle cx="100" cy="120" r="70" fill="none" stroke="#d98b3a" stroke-width="10"/>
      <circle cx="100" cy="120" r="38" fill="none" stroke="#a65d22" stroke-width="6"/><path d="M30 230h140" stroke="#d98b3a" stroke-width="4"/>
      <text x="100" y="272" text-anchor="middle" font-family="Georgia" font-size="20" fill="#f0d2a8">ELEPHANTS DREAM</text>`);
  MOCK_RESULTS[0].poster_url = MOCK_POSTERS[hash(2)];
  MOCK_RESULTS[1].poster_url = MOCK_POSTERS[hash(2)];

  // Preview flags.
  setTimeout(() => {
    const f = location.hash;
    if (f.includes('open') || f.includes('movie')) toggle(hash(1));
    if (f.includes('tv')) toggle(hash(7));
    if (f.includes('group')) { expanded.add('a:imdb:tt9000001'); render(list.map((t) => ({ ...t }))); }
    if (f.includes('packs')) setTimeout(() => { groupEls.forEach((G, gid) => expanded.add(gid)); render(list.map((t) => ({ ...t }))); }, 2500);
    if (f.includes('adding')) { openAdd(); $('magnet').value = 'not a magnet'; setAddError('Not a magnet link'); }
    if (f.includes('confirm')) askRemove(list[0]);
    if (f.includes('toast')) reportError('VLC_NOT_FOUND: VLC is not installed');
    if (f.includes('hover')) rows.get(hash(1)).head.classList.add('hl');
    if (f.includes('lib')) setView('library', false);
    if (f.includes('sheet')) setTimeout(() => openSheet(hash(1)), 1500);
    if (f.includes('settings')) setTimeout(() => wGear.click(), 300);
    if (f.includes('pairreq')) showPairRequest({ code: '123456', from: "mshn's Mac mini" });
  }, 50);

  return {
    listeners,
    invoke: (cmd, args) => new Promise((res, rej) => setTimeout(() => {
      try { Promise.resolve(api[cmd] ? api[cmd](args) : err(`NOT_IMPLEMENTED: ${cmd}`)).then(res, rej); } catch (e) { rej(String(e)); }
    }, 30)),
  };
})();

// ---------- other Macs (remote play) in the settings popover ----------
const rSec = sSection('Other Macs');
const rMe = h('p', 'whint rme');
const rOn = h('input'); rOn.type = 'checkbox';
const rOnL = h('label', 'check'); rOnL.append(rOn, document.createTextNode(' Allow playing from other Macs'));
const rList = h('div', 'rlist');
const rFind = h('button', 'wlink', 'Find Macs…'); rFind.type = 'button';
const rAdd = h('button', 'wlink', 'Add by IP…'); rAdd.type = 'button';
const rSync = h('button', 'wlink', 'Sync now'); rSync.type = 'button'; rSync.hidden = true;
const rBtns = h('div', 'nrow rbtns'); rBtns.append(rFind, rAdd, rSync);
const rExtra = h('div', 'rextra'); // discover results / IP input / code input
const rMsg = h('p', 'wstatus');
rSec.append(rMe, rOnL, rList, rBtns, rExtra, rMsg);
wPop.append(rSec);
const rSay = (m, bad) => { rMsg.textContent = m || ''; rMsg.className = `wstatus${bad ? ' bad' : ''}`; };
// The popover is a <form>: Enter in our inputs must not submit the watcher settings.
const rInput = (ph, onEnter) => {
  const i = h('input'); i.type = 'text'; i.placeholder = ph; i.spellcheck = false;
  i.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); onEnter(); } });
  return i;
};
const rButton = (text, run, primary) => { const b = h('button', primary ? 'rbtn-p' : 'rbtn-s', text); b.type = 'button'; b.addEventListener('click', run); return b; };

function renderRemote() {
  const s = remote;
  document.body.classList.toggle('has-remote', onlineDevices().length > 0);
  if (!s) return;
  rMe.textContent = s.enabled && s.port
    ? `This Mac: ${s.name} · ${s.lanIp || '—'}:${s.port}`
    : `This Mac: ${s.name}`;
  rOn.checked = !!s.enabled;
  rList.replaceChildren(...(s.devices || []).map((d) => {
    const row = h('div', 'rdev');
    const dot = h('span', `rdot${d.online ? ' on' : ''}`);
    dot.title = d.online ? 'Online' : 'Offline';
    const name = h('span', 'rname', d.name);
    name.title = `${d.host}:${d.port}`;
    const un = h('button', 'wlink', 'Unpair'); un.type = 'button';
    un.addEventListener('click', () => invoke('remote_unpair', { id: d.id }).then(loadRemote, reportError));
    row.append(dot, name);
    const st = syncLine(d.id);
    if (st) row.append(st);
    row.append(un);
    return row;
  }));
  rSync.hidden = !syncOk || !(s.devices || []).length;
}

// ---------- Mac↔Mac sync status (sync_status / sync_now) ----------
let syncSt = null; // { devices: [{ id, name, online, last_sync, error }] }
let syncOk = true;
function agoText(ts) {
  if (!ts) return 'Not synced yet';
  const ms = ts > 1e12 ? ts : ts * 1000; // seconds or milliseconds
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 45) return 'Synced just now';
  if (s < 3600) return `Synced ${Math.max(1, Math.round(s / 60))} min ago`;
  if (s < 86400) return `Synced ${Math.round(s / 3600)} h ago`;
  return `Synced ${plural(Math.round(s / 86400), 'day')} ago`;
}
function syncLine(id) {
  const d = syncSt && (syncSt.devices || []).find((x) => x.id === id);
  if (!d) return null;
  const el = h('span', `rsync${d.error ? ' bad' : ''}`, d.error ? 'Sync failed' : agoText(d.last_sync));
  el.title = d.error ? errMsg(d.error) : d.last_sync ? new Date(d.last_sync > 1e12 ? d.last_sync : d.last_sync * 1000).toLocaleString() : '';
  return el;
}
function loadSync() {
  if (!syncOk) return Promise.resolve();
  return invoke('sync_status').then((st) => { syncSt = st || null; renderRemote(); },
    (e) => { if (missingCmd(e)) { syncOk = false; syncSt = null; renderRemote(); } });
}
rSync.addEventListener('click', () => {
  rSync.disabled = true;
  rSay('Syncing…');
  invoke('sync_now')
    .then(() => Promise.all([loadSync(), loadRemoteLib(), loadResume()]))
    .then(() => rSay(''), (e) => { if (missingCmd(e)) { syncOk = false; renderRemote(); rSay(''); } else rSay(errMsg(e), true); })
    .finally(() => { rSync.disabled = false; });
});
function syncRefresh() { loadRemoteLib(); loadResume(); loadSync(); }
function loadRemote() { return invoke('remote_status').then((s) => { remote = s; renderRemote(); }, () => {}); }
rOn.addEventListener('change', () => invoke('remote_set_enabled', { enabled: rOn.checked })
  .then((s) => { remote = s; renderRemote(); }, (e) => { rOn.checked = !rOn.checked; reportError(e); }));

// Step 2 of pairing: type the code the other Mac shows.
function askCode(peer, host, port) {
  const i = rInput('123456', () => go());
  i.inputMode = 'numeric'; i.maxLength = 7; i.className = 'rcode';
  const go = () => {
    const code = i.value.replace(/\D/g, '');
    if (code.length !== 6) return rSay('The code has 6 digits', true);
    rSay('Pairing…');
    invoke('remote_pair_finish', { host, port, code }).then((d) => {
      rExtra.replaceChildren(); rSay(`Paired with ${d.name}`); loadRemote();
    }, (e) => rSay(errMsg(e), true));
  };
  const row = h('div', 'nrow'); row.append(i, rButton('Pair', go, true), rButton('Cancel', () => { rExtra.replaceChildren(); rSay(''); }));
  rExtra.replaceChildren(h('p', 'whint rask', `Enter the code shown on ${peer.name}`), row);
  rSay('');
  i.focus();
}
function startPair(p, host, port) {
  rSay('Contacting…');
  p.then((peer) => askCode(peer, host, port), (e) => rSay(errMsg(e), true));
}
rFind.addEventListener('click', () => {
  rSay('Looking for Macs…');
  rExtra.replaceChildren();
  invoke('remote_discover').then((list) => {
    rSay(list.length ? '' : 'No Macs found. Make sure Tide is open there, or add it by IP.');
    rExtra.replaceChildren(...list.map((f) => {
      const row = h('div', 'rdev');
      const name = h('span', 'rname', f.name);
      name.title = `${f.host}:${f.port}`;
      row.append(name, f.paired ? h('span', 'wmuted', 'Paired')
        : rButton('Pair', () => startPair(invoke('remote_pair_start', { host: f.host, port: f.port }), f.host, f.port), true));
      return row;
    }));
  }, (e) => rSay(errMsg(e), true));
});
rAdd.addEventListener('click', () => {
  const go = () => {
    const v = i.value.trim();
    const m = /^(.+?)(?::(\d{1,5}))?$/.exec(v);
    if (!v || !m) return rSay('Type an address like 192.168.1.20:47900', true);
    const host = m[1], port = m[2] ? Number(m[2]) : null;
    startPair(invoke('remote_add_manual', port ? { host, port } : { host }), host, port || 47900);
  };
  const i = rInput('192.168.1.20:47900', go);
  const row = h('div', 'nrow'); row.append(i, rButton('Add', go, true));
  rExtra.replaceChildren(row);
  rSay('');
  i.focus();
});
wGear.addEventListener('click', () => { if (!wPop.hidden) { rExtra.replaceChildren(); rSay(''); loadRemote(); loadSync(); } });

// ---------- archive (Mac mini / NEO Storage) in the settings popover ----------
let arSet = null; // archive_settings
let arSt = null; // archive_status / "archive-progress"
let arOk = true;
const aSec = sSection('Archive');
aSec.hidden = true;
const aDir = h('p', 'whint adir');
const aDays = h('input'); aDays.type = 'number'; aDays.min = '0'; aDays.max = '3650'; aDays.className = 'nkbps'; aDays.title = 'Days (0 = off)';
const aDaysRow = h('div', 'nrow'); aDaysRow.append(h('span', null, 'Auto-delete watched items after'), aDays, h('span', 'wmuted', 'days'));
const aAuto = h('input'); aAuto.type = 'checkbox';
const aAutoText = document.createTextNode('');
const aAutoL = h('label', 'check'); aAutoL.append(aAuto, aAutoText);
const aLine = h('p', 'whint aline');
aSec.append(aDir, aDaysRow, aAutoL, aLine);
wPop.append(aSec);
const archiveTarget = () => {
  const d = arSet && !arSet.archive_dir && arSet.device;
  return d ? d.name : null;
};
window.TideArchive = { target: archiveTarget, move: (args) => archiveMove(args) };
function archiveLine() {
  const q = (arSt && arSt.queue) || [];
  const cur = q.find((e) => e.state === 'moving');
  if (cur) return cur.bytes_total ? `Moving ${cur.name} — ${(cur.bytes_done / 1e9).toFixed(1)} / ${(cur.bytes_total / 1e9).toFixed(1)} GB` : `Moving ${cur.name}…`;
  const waiting = q.filter((e) => e.state === 'queued').length;
  const failed = q.find((e) => e.state === 'failed');
  if (failed) return `Could not move ${failed.name}: ${errMsg(failed.error || '')}`;
  const d = arSet && arSet.device;
  if (waiting) return `${plural(waiting, 'item')} waiting${d && !d.online ? ` for ${d.name}` : ''}`;
  if (d && !d.online) return `${d.name} is offline`;
  if (d && d.online && !d.available) return errMsg(d.error || 'Archive not available');
  return '';
}
function renderArchive() {
  const s = arSet;
  aSec.hidden = !s || (!s.archive_dir && !s.device);
  if (aSec.hidden) return;
  const isArchive = !!s.archive_dir;
  aDir.hidden = !isArchive;
  aDaysRow.hidden = !isArchive;
  aAutoL.hidden = isArchive;
  if (isArchive) {
    aDir.textContent = s.available
      ? `Archive folder: ${s.archive_dir}${s.free_bytes != null ? ` (${bytes(s.free_bytes)} free)` : ''}`
      : `⚠︎ ${errMsg(s.error || 'Archive folder not available')}`;
    aDir.classList.toggle('bad', !s.available);
    if (document.activeElement !== aDays) aDays.value = s.autoclean_days == null ? 0 : s.autoclean_days;
  } else {
    aAutoText.textContent = ` Move finished downloads to ${s.device.name}`;
    aAuto.checked = !!s.archive_auto;
  }
  const line = isArchive ? '' : archiveLine();
  aLine.textContent = line;
  aLine.hidden = !line;
}
function loadArchive() {
  if (!arOk) return Promise.resolve();
  return Promise.all([invoke('archive_settings'), invoke('archive_status')]).then(([s, st]) => { arSet = s; arSt = st; renderArchive(); if (window.YT) window.YT.refresh(); },
    (e) => { if (missingCmd(e)) { arOk = false; arSet = null; renderArchive(); } });
}
const setArchive = (args) => invoke('archive_set_settings', args).then((s) => { arSet = s; renderArchive(); }, reportError);
aAuto.addEventListener('change', () => setArchive({ archiveAuto: aAuto.checked }));
aDays.addEventListener('change', () => setArchive({ autocleanDays: Math.max(0, Math.round(Number(aDays.value) || 0)) }));
aDays.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); aDays.blur(); } });
function archiveMove(args) {
  return invoke('archive_move', args).then((st) => {
    arSt = st; renderArchive();
    const d = arSet && arSet.device;
    toast(d && d.online ? `Moving to ${d.name}…` : `Will move when ${d ? d.name : 'the archive Mac'} is online`);
  }, reportError);
}
wGear.addEventListener('click', () => { if (!wPop.hidden) loadArchive(); });

// Incoming pair request (this Mac shows the code the other one must type).
const prCard = h('div', 'prcard');
prCard.hidden = true;
document.body.append(prCard);
let prTimer;
function showPairRequest(p) {
  const code = String(p.code || '');
  const close = h('button', 'rbtn-s', 'Close'); close.type = 'button';
  close.addEventListener('click', () => { prCard.hidden = true; });
  prCard.replaceChildren(h('p', 'prtitle', `Pair with ${p.from || 'another Mac'}?`), h('p', 'whint', 'Type this code on that Mac:'),
    h('div', 'prcode', code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code), close);
  prCard.hidden = false;
  clearTimeout(prTimer);
  prTimer = setTimeout(() => { prCard.hidden = true; }, 120000);
}

// ---------- boot ----------
listen('torrents-updated', (e) => render(e.payload));
refresh();
reloadGroups();
reloadWatched();
if (T) listen('new-episodes', (e) => setEpisodes(e.payload));
if (T) {
  listen('net-status', (e) => { net = e.payload; renderNet(); });
  listen('watched-changed', () => reloadWatched());
}
loadMoreSettings();
loadRemote();
loadArchive();
syncRefresh();
setInterval(() => { loadRemoteLib(); loadResume(); }, 30000);
window.addEventListener('focus', loadResume);
if (T) {
  listen('sync-updated', () => syncRefresh());
  listen('remote-devices-updated', () => { loadRemote(); loadRemoteLib(); });
  listen('remote-pair-request', (e) => showPairRequest(e.payload || {}));
  listen('archive-progress', (e) => {
    const prev = arSt;
    arSt = e.payload || null;
    if (arSt && arSt.device && arSet) arSet.device = arSt.device;
    if (!arSet && arSt && arSt.device) loadArchive(); else renderArchive();
    if (prev && arSt && (prev.queue || []).length > (arSt.queue || []).length) refresh();
  });
}
loadWatcher().then(() => invoke('watcher_check', { force: false })).then(setEpisodes, () => {});
