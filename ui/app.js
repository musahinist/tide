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
const rows = new Map(); // key -> { el, head, name, stats, bar, playBtn, pauseBtn, resumeBtn, files, last }
const expanded = new Set();

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

  const r = { el, head, name, statsText, fill, files: null, last: null };
  const ctl = h('div', 'ctl');
  r.playBtn = iconBtn('play', 'Play', () => playLargest(key));
  r.pauseBtn = iconBtn('pause', 'Pause', () => invoke('pause_torrent', { key }).then(refresh, reportError));
  r.resumeBtn = iconBtn('resume', 'Resume', () => invoke('resume_torrent', { key }).then(refresh, reportError));
  r.revealBtn = iconBtn('folder', 'Show in Finder', () => invoke('reveal_torrent', { key }).catch(reportError));
  ctl.append(r.playBtn, r.pauseBtn, r.resumeBtn, r.revealBtn, iconBtn('remove', 'Remove', () => askRemove(r.last)));

  head.append(name, stats, ctl, bar);
  head.addEventListener('click', () => toggle(key));
  el.append(head);
  return r;
}

function updateRow(r, t) {
  const p = r.last || {};
  if (p.name !== t.name) { r.name.textContent = t.name; r.name.title = t.name; }
  if (p.state !== t.state) r.el.className = `torrent s-${t.state}`;
  const line = statsLine(t);
  if (r.statsText.textContent !== line) r.statsText.textContent = line;
  const w = `${(t.state === 'resolving' ? 0 : t.progress * 100).toFixed(2)}%`;
  if (r.fill.style.width !== w) r.fill.style.width = w;

  const ready = t.state !== 'resolving' && t.id != null;
  r.playBtn.hidden = !ready || t.state === 'error';
  r.pauseBtn.hidden = !(t.state === 'downloading' || t.state === 'seeding' || t.state === 'initializing');
  r.resumeBtn.hidden = !(t.state === 'paused' || (t.state === 'error' && ready));
  r.revealBtn.hidden = !t.output_path;
  r.last = t;
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
    if (!seen.has(key)) { r.el.remove(); rows.delete(key); expanded.delete(key); }
  }
  $('empty').hidden = list.length > 0;
  for (const key of expanded) loadFiles(key);
}

// ---------- file list ----------
function toggle(key) {
  const r = rows.get(key);
  if (expanded.delete(key)) {
    if (r.files) { r.files.remove(); r.files = null; }
    return;
  }
  expanded.add(key);
  r.files = h('div', 'files');
  r.files.append(h('div', 'msg', 'Loading…'));
  r.el.append(r.files);
  loadFiles(key);
}

async function loadFiles(key) {
  let files, msg = null;
  try { files = await invoke('list_files', { key }); } catch (e) {
    msg = errCode(e) === 'NOT_READY' ? 'Getting metadata…' : errMsg(e);
  }
  const r = rows.get(key);
  if (!r || !r.files) return;
  const box = r.files;
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
  ];
  const files = {
    [hash(1)]: [['Big Buck Bunny.mp4', 'video', 4.1 * GB], ['Big Buck Bunny.en.srt', 'subtitle', 48e3], ['Big Buck Bunny.tr.srt', 'subtitle', 51e3], ['poster.jpg', 'other', 820e3]],
    [hash(2)]: [['Sintel.2010.2160p.mkv', 'video', 3.9 * GB], ['Sintel.en.srt', 'subtitle', 40e3]],
    [hash(3)]: [['ubuntu-24.04.1-desktop-amd64.iso', 'other', 6.1 * GB]],
    [hash(5)]: [['Cosmos Laundromat.mkv', 'video', 1.6 * GB]],
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
  };

  // Preview flags.
  setTimeout(() => {
    const f = location.hash;
    if (f.includes('open')) { toggle(hash(1)); toggle(hash(6)); }
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
