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
    case 'seeding': {
      const parts = [`Done · ${bytes(t.total_bytes)}`];
      if (t.up_bps > 0) parts.push(`↑ ${speed(t.up_bps)}`);
      if (t.peers) parts.push(plural(t.peers, 'peer'));
      return parts.join(' · ');
    }
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
  if (!$('modal').hidden) closeConfirm();
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
  invoke('play_media', { media: { kind: 'torrent', key, fileIndex } }).catch(reportError);

async function playLargest(key) {
  try {
    const media = (await invoke('list_files', { key })).filter((f) => f.kind === 'video' || f.kind === 'audio');
    if (!media.length) return toast('No video or audio in this torrent');
    play(key, media.reduce((a, b) => (b.size > a.size ? b : a)).index);
  } catch (e) { reportError(e); }
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
  r.pauseBtn = iconBtn('pause', 'Pause', () => invoke('pause_torrent', { key }).then(refresh, reportError));
  r.resumeBtn = iconBtn('resume', 'Resume', () => invoke('resume_torrent', { key }).then(refresh, reportError));
  r.revealBtn = iconBtn('folder', 'Show in Finder', () => invoke('reveal_torrent', { key }).catch(reportError));
  ctl.append(r.playBtn, r.pauseBtn, r.resumeBtn, r.revealBtn, iconBtn('remove', 'Remove', () => askRemove(r.last)));

  r.art = h('div', 'art');
  head.append(r.art, name, stats, ctl, bar);
  head.addEventListener('click', () => toggle(key));
  el.append(head);
  return r;
}

function updateRow(r, t) {
  const p = r.last || {};
  const m = infoOf(t.key);
  const title = mediaTitle(t, m);
  if (r.name.textContent !== title) r.name.textContent = title;
  if (p.name !== t.name) r.name.title = t.name;
  if (p.state !== t.state) r.el.className = `torrent s-${t.state}${t.key === openKey ? ' open' : ''}`;
  const rt = rating(m);
  const line = statsLine(t) + (rt ? ` · IMDb ${rt}` : '');
  if (r.statsText.textContent !== line) r.statsText.textContent = line;
  const w = `${(t.state === 'resolving' ? 0 : t.progress * 100).toFixed(2)}%`;
  if (r.fill.style.width !== w) r.fill.style.width = w;

  const ready = t.state !== 'resolving' && t.id != null;
  r.playBtn.hidden = !ready || t.state === 'error';
  r.pauseBtn.hidden = !(t.state === 'downloading' || t.state === 'seeding' || t.state === 'initializing');
  r.resumeBtn.hidden = !(t.state === 'paused' || (t.state === 'error' && ready));
  r.revealBtn.hidden = !t.output_path;
  r.last = t;
  wantArtwork(t);
  wantMeta(t);
}

function render(list) {
  const seen = new Set();
  const container = $('list');
  let prev = null;
  for (const t of list) {
    seen.add(t.key);
    let r = rows.get(t.key);
    if (!r) { r = createRow(t.key); rows.set(t.key, r); }
    updateRow(r, t);
    const want = prev ? prev.nextSibling : container.firstChild;
    if (want !== r.el) container.insertBefore(r.el, want);
    prev = r.el;
  }
  for (const [key, r] of rows) {
    if (!seen.has(key)) {
      r.el.remove(); rows.delete(key); art.delete(key); meta.delete(key);
      if (openKey === key) openKey = null;
    }
  }
  pumpArtwork();
  pumpMeta();
  $('empty').hidden = list.length > 0;
  if (openKey) updatePanel(openKey);
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
    mk(2, 'Sintel.2010.2160p.mkv', 'seeding', 3.9 * GB, 1, { up_bps: 320e3, peers: 4 }),
    mk(3, 'ubuntu-24.04.1-desktop-amd64.iso', 'paused', 6.1 * GB, 0.18),
    mk(4, 'Tears of Steel', 'resolving', 0, 0),
    mk(5, 'Cosmos Laundromat – First Cycle', 'error', 1.6 * GB, 0.07, { error: 'Tracker unreachable: connection timed out' }),
    mk(6, 'The Daily Podcast – Season 3 Complete', 'downloading', 812 * MB, 0.83, { down_bps: 640e3, peers: 31 }),
    mk(7, 'Severance.S02E05.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb', 'downloading', 2.4 * GB, 0.62, { down_bps: 3.4 * MB, up_bps: 90e3, peers: 22, uploaded_bytes: 310 * MB }),
  ];
  const files = {
    [hash(1)]: [['Big Buck Bunny.mp4', 'video', 4.1 * GB], ['Big Buck Bunny.en.srt', 'subtitle', 48e3], ['Big Buck Bunny.tr.srt', 'subtitle', 51e3], ['poster.jpg', 'other', 820e3]],
    [hash(2)]: [['Sintel.2010.2160p.mkv', 'video', 3.9 * GB], ['Sintel.en.srt', 'subtitle', 40e3]],
    [hash(3)]: [['ubuntu-24.04.1-desktop-amd64.iso', 'other', 6.1 * GB]],
    [hash(5)]: [['Cosmos Laundromat.mkv', 'video', 1.6 * GB]],
    [hash(7)]: [['Severance.S02E05.1080p.mkv', 'video', 2.4 * GB], ['Severance.S02E05.en.srt', 'subtitle', 60e3]],
    [hash(6)]: Array.from({ length: 8 }, (_, i) => [`Season 3/Episode ${String(i + 1).padStart(2, '0')}.mp3`, 'audio', 101 * MB]),
  };
  const listeners = [];
  const find = (key) => list.find((t) => t.key === key);
  const err = (s) => Promise.reject(s);

  setInterval(() => {
    for (const t of list) {
      if (t.state !== 'downloading') continue;
      t.down_bps = Math.max(80e3, t.down_bps * (0.85 + Math.random() * 0.3));
      t.progress_bytes = Math.min(t.total_bytes, t.progress_bytes + t.down_bps);
      t.progress = t.progress_bytes / t.total_bytes;
      t.eta_secs = Math.round((t.total_bytes - t.progress_bytes) / t.down_bps);
      if (t.progress >= 1) Object.assign(t, { state: 'seeding', down_bps: 0, eta_secs: null });
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
    resume_torrent: ({ key }) => { const t = find(key); Object.assign(t, t.progress >= 1 ? { state: 'seeding' } : { state: 'downloading', down_bps: 1.2 * MB, peers: 9, error: null }); return null; },
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
    [hash(2)]: { ...base, imdb_id: '', media_type: 'movie', title: 'Sintel', year: 2010, genres: [], runtime_min: null, imdb_rating: null, overview: '', needs_match: true },
  };
  const MOCK_RESULTS = [
    { ...base, imdb_id: 'tt1727587', media_type: 'movie', title: 'Sintel', year: 2010, release_info: '2010', genres: ['Animation', 'Fantasy'], runtime_min: 15, imdb_rating: 6.9, overview: 'A lonely young woman, Sintel, helps and befriends a dragon, whom she calls Scales.' },
    { ...base, imdb_id: 'tt0000002', media_type: 'movie', title: 'Sintel: The Making Of', year: 2011, release_info: '2011', genres: ['Documentary'], runtime_min: 40, imdb_rating: 7.1, overview: '' },
  ];
  const MOCK_MEDIA_POSTERS = { [hash(1)]: MOCK_POSTERS[hash(1)], [hash(7)]: svgUrl(`<rect width="200" height="300" fill="#e9ecef"/><rect x="40" y="90" width="120" height="80" fill="#0b3d5c"/>
      <text x="100" y="250" text-anchor="middle" font-family="Helvetica" font-size="24" letter-spacing="4" fill="#0b3d5c">SEVERANCE</text>`) };
  MOCK_RESULTS[0].poster_url = MOCK_POSTERS[hash(2)];
  MOCK_RESULTS[1].poster_url = MOCK_POSTERS[hash(2)];

  // Preview flags.
  setTimeout(() => {
    const f = location.hash;
    if (f.includes('open') || f.includes('movie')) toggle(hash(1));
    if (f.includes('tv')) toggle(hash(7));
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
