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
const play = (key, fileIndex) =>
  invoke('play_media', { media: { kind: 'torrent', key, fileIndex } }).catch(playError);
function playError(e) {
  if (errCode(e) === 'NOT_READY') toast('Still checking files…'); else reportError(e);
}
// Resume refuses finished torrents ("COMPLETED: …"); that is not worth a toast.
const resumeT = (key) => invoke('resume_torrent', { key }).then(refresh, (e) => (errCode(e) === 'COMPLETED' ? refresh() : reportError(e)));
const pauseT = (key) => invoke('pause_torrent', { key }).then(refresh, reportError);

async function playLargest(key) {
  try {
    const media = (await invoke('list_files', { key })).filter((f) => f.kind === 'video' || f.kind === 'audio');
    if (!media.length) return toast('No video or audio in this torrent');
    const f = media.reduce((a, b) => (b.size > a.size ? b : a));
    mainFile.set(key, f.index);
    play(key, f.index);
  } catch (e) { playError(e); }
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
  stats.append(dot, statsText);
  const bar = h('div', 'bar');
  const fill = h('i');
  bar.append(fill);

  const r = { el, head, name, statsText, fill, panel: null, details: null, last: null, artPri: 0 };
  const ctl = h('div', 'ctl');
  r.playBtn = iconBtn('play', 'Play', () => playLargest(key));
  r.pauseBtn = iconBtn('pause', 'Pause', () => pauseT(key));
  r.resumeBtn = iconBtn('resume', 'Resume', () => resumeT(key));
  r.revealBtn = iconBtn('folder', 'Show in Finder', () => invoke('reveal_torrent', { key }).catch(reportError));
  r.moreBtn = iconBtn('more', 'More', () => openMenu(r.moreBtn, { key }));
  ctl.append(r.playBtn, r.pauseBtn, r.resumeBtn, r.revealBtn, iconBtn('remove', 'Remove', () => askRemove(r.last)), r.moreBtn);

  r.art = h('div', 'art');
  r.check = h('button', 'wcheck', '✓');
  r.check.type = 'button';
  r.check.addEventListener('click', (e) => { e.stopPropagation(); toggleWatched(key); });
  head.append(r.art, r.check, name, stats, ctl, bar);
  head.addEventListener('click', () => toggle(key));
  el.append(head);
  return r;
}

function updateRow(r, t) {
  const p = r.last || {};
  const m = infoOf(t.key);
  const title = r.group ? episodeTitle(t, m) : mediaTitle(t, m);
  const w0 = isWatched(t.key);
  if (r.watched !== w0) { r.watched = w0; r.check.classList.toggle('on', w0); r.check.title = w0 ? 'Watched — click to unmark' : 'Mark as watched'; }
  if (r.name.textContent !== title) r.name.textContent = title;
  if (p.name !== t.name) r.name.title = t.name;
  const cls = `torrent s-${t.state}${t.key === openKey ? ' open' : ''}${r.group ? ' ep' : ''}`;
  if (r.el.className !== cls) r.el.className = cls;
  const rt = rating(m);
  const line = statsLine(t) + (rt ? ` · IMDb ${rt}` : '');
  if (r.statsText.textContent !== line) r.statsText.textContent = line;
  const w = `${(t.state === 'resolving' ? 0 : t.progress * 100).toFixed(2)}%`;
  if (r.fill.style.width !== w) r.fill.style.width = w;

  const ready = t.state !== 'resolving' && t.id != null;
  r.playBtn.hidden = !ready || t.state === 'error' || t.state === 'initializing';
  r.pauseBtn.hidden = !(t.state === 'downloading' || t.state === 'initializing');
  r.resumeBtn.hidden = !(t.state === 'paused' || (t.state === 'error' && ready));
  r.revealBtn.hidden = !t.output_path;
  r.last = t;
  wantArtwork(t);
  wantMeta(t);
}

// ---------- groups (computed here; the backend only persists groups.json / watched.json) ----------
let groupsData = { groups: [], excluded: {} };
let watched = {}; // "key:fileIndex" -> true
let watchedKeys = new Set(); // torrent keys with any watched file
const mainFile = new Map(); // key -> index of the main media file (largest video/audio)
const groupEls = new Map(); // gid -> group card
const expanded = new Set(); // gids of expanded group cards (survives re-renders)
let lastList = [];
let lastGroups = new Map(); // gid -> { gid, id, name, keys, autoKey }

const code = (s, e) => `S${pad2(s)}E${pad2(e)}`;
const normTitle = (s) => s.toLowerCase().replace(/[^a-z0-9]+/g, ' ').trim();
function parseSE(name) {
  const m = /^(.*?)[\s._-]+s(\d{1,2})[\s._-]?e(\d{1,3})/i.exec(name) || /^(.*?)[\s._-]+(\d{1,2})x(\d{2,3})\b/i.exec(name);
  return m && m[1] ? { show: m[1].replace(/[._]+/g, ' ').trim(), season: +m[2], episode: +m[3] } : null;
}
// Episode facts for a row: { autoKey, show, season, episode, title } or null.
function epInfo(t) {
  const m = infoOf(t.key);
  if (matched(m) && m.media_type === 'tv') {
    return { autoKey: m.imdb_id ? `imdb:${m.imdb_id}` : null, show: m.title, season: m.season, episode: m.episode, title: m.episode_title };
  }
  const p = parseSE(t.name);
  if (!p) return null;
  return { autoKey: matched(m) ? null : `title:${normTitle(p.show)}`, show: p.show, season: p.season, episode: p.episode, title: null };
}
function episodeTitle(t) {
  const e = epInfo(t);
  if (!e || e.season == null || e.episode == null) return t.name;
  return e.title ? `${code(e.season, e.episode)} · ${e.title}` : code(e.season, e.episode);
}

const isWatched = (key) => (mainFile.has(key) ? !!watched[`${key}:${mainFile.get(key)}`] : watchedKeys.has(key));
const playable = (t) => t.id != null && !['resolving', 'initializing', 'error'].includes(t.state);

function computeGroups(list) {
  const byKey = new Map(list.map((t) => [t.key, t]));
  const out = new Map();
  const owner = new Map();
  const autoInto = new Map(); // auto key -> manual gid that already holds an episode of that show
  for (const g of groupsData.groups) {
    const gid = `g:${g.id}`;
    out.set(gid, { gid, id: g.id, name: g.name, keys: [] });
    if (g.auto_key) autoInto.set(g.auto_key, gid);
    for (const k of g.members) {
      const t = byKey.get(k);
      if (!t || owner.has(k)) continue;
      owner.set(k, gid);
      const e = epInfo(t);
      if (e && e.autoKey && !autoInto.has(e.autoKey)) autoInto.set(e.autoKey, gid);
    }
  }
  const autos = new Map();
  for (const t of list) {
    if (owner.has(t.key) || groupsData.excluded[t.key]) continue;
    const e = epInfo(t);
    if (!e || !e.autoKey) continue;
    if (!autos.has(e.autoKey)) autos.set(e.autoKey, { name: e.show, keys: [] });
    autos.get(e.autoKey).keys.push(t.key);
  }
  for (const [ak, a] of autos) {
    const into = autoInto.get(ak);
    if (into) { a.keys.forEach((k) => owner.set(k, into)); continue; }
    if (a.keys.length < 2) continue;
    const gid = `a:${ak}`;
    out.set(gid, { gid, id: null, name: a.name, keys: [], autoKey: ak });
    a.keys.forEach((k) => owner.set(k, gid));
  }
  for (const t of list) { const g = owner.get(t.key); if (g) out.get(g).keys.push(t.key); }
  return { groups: out, owner };
}

function render(list) {
  lastList = list;
  const { groups, owner } = computeGroups(list);
  lastGroups = groups;
  const seen = new Set();
  for (const t of list) {
    seen.add(t.key);
    let r = rows.get(t.key);
    if (!r) { r = createRow(t.key); rows.set(t.key, r); }
    r.group = owner.get(t.key) || null;
    updateRow(r, t);
  }
  const container = $('list');
  const seenG = new Set();
  let prev = null;
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
    updateGroup(G, groups.get(r.group));
    place(G.el);
  }
  for (const [key, r] of rows) {
    if (!seen.has(key)) {
      r.el.remove(); rows.delete(key); art.delete(key); meta.delete(key);
      if (openKey === key) openKey = null;
    }
  }
  for (const [gid, G] of groupEls) if (!seenG.has(gid)) { G.el.remove(); groupEls.delete(gid); }
  pumpArtwork();
  pumpMeta();
  $('empty').hidden = list.length > 0;
  if (openKey) updatePanel(openKey);
}

function createGroup(gid) {
  const G = { gid, el: h('section', 'torrent group'), head: h('div', 'head'), name: h('div', 'name'), seasons: new Map() };
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
  G.nextBtn.addEventListener('click', (e) => { e.stopPropagation(); if (G.nextKey) playLargest(G.nextKey); });
  const more = iconBtn('more', 'More', () => openMenu(more, { gid }));
  more.classList.add('gmore');
  ctl.append(G.nextBtn, more);
  G.head.append(G.art, G.name, stats, ctl, bar);
  G.head.addEventListener('click', () => {
    if (expanded.has(gid)) expanded.delete(gid); else expanded.add(gid);
    render(lastList);
  });
  G.eps = h('div', 'eps');
  G.el.append(G.head, G.eps);
  return G;
}

function updateGroup(G, gr) {
  const items = gr.keys.map((k) => ({ r: rows.get(k), t: rows.get(k).last, e: epInfo(rows.get(k).last) || {} }));
  const num = (v) => (v == null ? 1e9 : v);
  items.sort((a, b) => num(a.e.season) - num(b.e.season) || num(a.e.episode) - num(b.e.episode) || a.t.name.localeCompare(b.t.name));
  const first = items.find((x) => x.e.season != null && x.e.episode != null);
  const last = [...items].reverse().find((x) => x.e.season != null && x.e.episode != null);
  let name = gr.name;
  if (!gr.id) { const m = items.map((x) => infoOf(x.t.key)).find(matched); if (m) name = m.title; }
  if (G.name.textContent !== name) G.name.textContent = name;

  let total = 0, done = 0, down = 0;
  for (const { t } of items) { total += t.total_bytes; done += t.progress_bytes; down += t.down_bps; }
  const allDone = items.every((x) => x.t.state === 'completed');
  const parts = [plural(items.length, 'episode')];
  if (first) parts.push(first === last ? code(first.e.season, first.e.episode) : `${code(first.e.season, first.e.episode)}–${code(last.e.season, last.e.episode)}`);
  parts.push(bytes(total));
  if (!allDone && total) parts.push(pct(done / total));
  if (down > 0) parts.push(`↓ ${speed(down)}`);
  const line = parts.join(' · ');
  if (G.statsText.textContent !== line) G.statsText.textContent = line;
  const w = `${(total ? (done / total) * 100 : 0).toFixed(2)}%`;
  if (G.fill.style.width !== w) G.fill.style.width = w;
  const st = allDone ? 'completed' : items.some((x) => x.t.state === 'downloading') ? 'downloading' : items.some((x) => x.t.state === 'error') ? 'error' : 'paused';
  const open = expanded.has(G.gid);
  const cls = `torrent group s-${st}${open ? ' expanded' : ''}`;
  if (G.el.className !== cls) G.el.className = cls;

  const next = items.find((x) => playable(x.t) && !isWatched(x.t.key)) || items.find((x) => playable(x.t));
  G.nextKey = next ? next.t.key : null;
  const label = next ? `▶ ${next.e.season != null && next.e.episode != null ? code(next.e.season, next.e.episode) : 'Play'}` : '';
  if (G.nextBtn.textContent !== label) G.nextBtn.textContent = label;
  G.nextBtn.hidden = !next;

  const withArt = items.find((x) => x.r.artUrl);
  const url = withArt ? withArt.r.artUrl : null;
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

  // Episode list: season headers + the rows themselves (kept inside even when collapsed).
  G.eps.hidden = !open;
  const want = [];
  const seasons = new Set(items.map((x) => x.e.season));
  const showHeaders = seasons.size > 1 || !seasons.has(undefined);
  let cur;
  for (const x of items) {
    if (showHeaders && x.e.season !== cur) {
      cur = x.e.season;
      const label = cur == null ? 'Other' : cur === 0 ? 'Specials' : `Season ${cur}`;
      if (!G.seasons.has(label)) G.seasons.set(label, h('div', 'season', label));
      want.push(G.seasons.get(label));
    }
    want.push(x.r.el);
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
  const media = (await invoke('list_files', { key })).filter((f) => f.kind === 'video' || f.kind === 'audio');
  if (!media.length) throw 'NOT_FOUND: no video or audio in this torrent';
  const idx = media.reduce((a, b) => (b.size > a.size ? b : a)).index;
  mainFile.set(key, idx);
  return idx;
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
  } else {
    const gr = lastGroups.get(ctx.gid);
    if (!gr) return;
    items.push(menuItem('Rename group', () => menuInput(gr.name, 'Group name', (name) => act(gr.id
      ? invoke('rename_group', { id: gr.id, name })
      : invoke('create_group', { name, keys: gr.keys })))));
    items.push(menuItem('Ungroup', () => { closeMenu(); act(ungroup(gr)); }));
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
  if (!art.has(t.key)) art.set(t.key, { done: false, busy: false, tries: 0, next: 0 });
  const a = art.get(t.key);
  a.eligible = t.state !== 'resolving' && t.id != null;
}

function pumpArtwork() {
  const now = Date.now();
  for (const [key, a] of art) {
    if (artBusy >= ART_MAX) return;
    if (a.done || a.busy || !a.eligible || now < a.next) continue;
    a.busy = true; artBusy++;
    invoke('get_artwork', { key }).then((res) => {
      a.done = true;
      if (res && res.poster) showArtwork(key, res.poster, res.kind, 1);
    }, (e) => {
      a.tries++;
      a.next = Date.now() + Math.min(60e3, 3e3 * 2 ** (a.tries - 1));
      if (errCode(e) === 'NOT_FOUND') a.done = true;
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
  if (!meta.has(t.key)) meta.set(t.key, { done: false, busy: false, tries: 0, next: 0, info: null, poster: null });
  meta.get(t.key).eligible = t.state !== 'resolving' && t.id != null && !!t.output_path;
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
  if (matched(info) && info.imdb_id) {
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
  invoke('get_torrent_details', { key }).then((d) => { r.details = d; if (r.panel) fillMore(r.panel, key, d); }, () => {});
}

function closePanel() {
  const r = openKey && rows.get(openKey);
  if (r) { r.el.classList.remove('open'); if (r.panel) r.panel.el.remove(); r.panel = null; }
  openKey = null;
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
  const line = torrentStats(t);
  if (p.stats.textContent !== line) p.stats.textContent = line;
  const path = t.output_path || (r.details && r.details.save_path) || '';
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
  if (m.needs_match) {
    const row = h('div', 'nomatch');
    row.append(h('span', null, `No match for “${m.title || 'this torrent'}”`),
      linkBtn('Match', 'btn', () => openSearch(p, key, m)));
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
  links.append(linkBtn('Wrong match?', 'muted', () => openSearch(p, key, m)));
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
  try { files = await invoke('list_files', { key }); } catch (e) {
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
  return row;
}

function refresh() { invoke('list_torrents').then(render, reportError); }

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
    [hash(6)]: Array.from({ length: 8 }, (_, i) => [`Season 3/Episode ${String(i + 1).padStart(2, '0')}.mp3`, 'audio', 101 * MB]),
  };
  const listeners = [];
  const groupsFile = { groups: [], excluded: {} };
  const watchedFile = { [`${hash(8)}:0`]: true };
  let gseq = 0;
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

  const api = {
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
    [hash(2)]: { ...base, imdb_id: '', media_type: 'movie', title: 'Sintel', year: 2010, genres: [], runtime_min: null, imdb_rating: null, overview: '', needs_match: true },
  };
  const MOCK_RESULTS = [
    { ...base, imdb_id: 'tt1727587', media_type: 'movie', title: 'Sintel', year: 2010, release_info: '2010', genres: ['Animation', 'Fantasy'], runtime_min: 15, imdb_rating: 6.9, overview: 'A lonely young woman, Sintel, helps and befriends a dragon, whom she calls Scales.' },
    { ...base, imdb_id: 'tt0000002', media_type: 'movie', title: 'Sintel: The Making Of', year: 2011, release_info: '2011', genres: ['Documentary'], runtime_min: 40, imdb_rating: 7.1, overview: '' },
  ];
  const LH = svgUrl(`<rect width="200" height="300" fill="#13243a"/><path d="M88 110h24l8 130H80z" fill="#e8e1d2"/><path d="M84 96h32v14H84z" fill="#f4c95d"/>
      <path d="M100 103 L200 70 V136z" fill="#f4c95d" opacity=".35"/><path d="M0 240 Q100 225 200 240 V300 H0z" fill="#0a1626"/>
      <text x="100" y="282" text-anchor="middle" font-family="Georgia" font-size="17" fill="#e8e1d2">THE LIGHTHOUSE KEEPERS</text>`);
  const MOCK_MEDIA_POSTERS = { [hash(8)]: LH, [hash(9)]: LH, [hash(10)]: LH, [hash(1)]: MOCK_POSTERS[hash(1)], [hash(7)]: svgUrl(`<rect width="200" height="300" fill="#e9ecef"/><rect x="40" y="90" width="120" height="80" fill="#0b3d5c"/>
      <text x="100" y="250" text-anchor="middle" font-family="Helvetica" font-size="24" letter-spacing="4" fill="#0b3d5c">SEVERANCE</text>`) };
  MOCK_RESULTS[0].poster_url = MOCK_POSTERS[hash(2)];
  MOCK_RESULTS[1].poster_url = MOCK_POSTERS[hash(2)];

  // Preview flags.
  setTimeout(() => {
    const f = location.hash;
    if (f.includes('open') || f.includes('movie')) toggle(hash(1));
    if (f.includes('tv')) toggle(hash(7));
    if (f.includes('group')) { expanded.add('a:imdb:tt9000001'); render(list.map((t) => ({ ...t }))); }
    if (f.includes('adding')) { openAdd(); $('magnet').value = 'not a magnet'; setAddError('Not a magnet link'); }
    if (f.includes('confirm')) askRemove(list[0]);
    if (f.includes('toast')) reportError('VLC_NOT_FOUND: VLC is not installed');
    if (f.includes('hover')) rows.get(hash(1)).head.classList.add('hl');
  }, 50);

  return {
    listeners,
    invoke: (cmd, args) => new Promise((res, rej) => setTimeout(() => {
      try { Promise.resolve(api[cmd] ? api[cmd](args) : err(`NOT_IMPLEMENTED: ${cmd}`)).then(res, rej); } catch (e) { rej(String(e)); }
    }, 30)),
  };
})();

// ---------- boot ----------
listen('torrents-updated', (e) => render(e.payload));
refresh();
reloadGroups();
reloadWatched();
