'use strict';

// Video-site downloads (yt-dlp). Exposes window.YT.openProbeDialog(url); rows live in #yt-root.
(() => {
  const T = window.__TAURI__;
  const invoke = T ? T.core.invoke : mockInvoke;
  const listen = T ? T.event.listen : mockListen;

  // ---------- helpers ----------
  const h = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };
  const ICONS = {
    play: '<path class="fill" d="M5 3.2v9.6c0 .5.5.8.9.5l7.3-4.8a.6.6 0 0 0 0-1L5.9 2.7c-.4-.3-.9 0-.9.5z"/>',
    stop: '<rect class="fill" x="4.5" y="4.5" width="7" height="7" rx="1"/>',
    remove: '<path d="M4 4l8 8M12 4l-8 8"/>',
    folder: '<path d="M2 4.5c0-.6.4-1 1-1h3.2l1.5 1.5H13c.6 0 1 .4 1 1v6c0 .6-.4 1-1 1H3c-.6 0-1-.4-1-1z"/>',
  };
  function iconBtn(name, title, onClick) {
    const b = h('button', 'icon');
    b.type = 'button';
    b.innerHTML = `<svg viewBox="0 0 16 16">${ICONS[name]}</svg>`;
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
  function eta(s) {
    if (s < 60) return 'less than a minute left';
    const m = Math.round(s / 60);
    if (m < 60) return `${m} min left`;
    return `${Math.floor(m / 60)} h ${m % 60} min left`;
  }
  function duration(s) {
    if (s == null) return null;
    s = Math.round(s);
    const hh = Math.floor(s / 3600), mm = Math.floor((s % 3600) / 60), ss = String(s % 60).padStart(2, '0');
    return hh ? `${hh}:${String(mm).padStart(2, '0')}:${ss}` : `${mm}:${ss}`;
  }
  const errCode = (e) => String(e).split(':')[0];
  const errMsg = (e) => { const s = String(e); const i = s.indexOf(': '); return i > 0 && /^[A-Z_]+$/.test(s.slice(0, i)) ? s.slice(i + 2) : s; };
  const PRESET_LABEL = { '720p': '720p', '480p': '480p', '360p': '360p', audio_mp3: 'MP3' };
  const PRESETS = [['720p', 720], ['480p', 480], ['360p', 360], ['audio_mp3', 0]];

  function statsLine(j) {
    const kind = PRESET_LABEL[j.preset] || j.preset;
    switch (j.state) {
      case 'queued': return `Getting video info… · ${kind}`;
      case 'postprocessing': return `Processing… · ${kind}`;
      case 'done': return [`Completed`, j.total_bytes ? bytes(j.total_bytes) : null, kind].filter(Boolean).join(' · ');
      case 'error': return j.error || 'Error';
      case 'cancelled': return `Cancelled · ${kind}`;
      default: {
        const parts = [`${Math.floor(j.progress * 100)}%`];
        parts.push(j.total_bytes ? `${bytes(j.downloaded_bytes)} of ${bytes(j.total_bytes)}` : bytes(j.downloaded_bytes));
        if (j.speed_bps) parts.push(`↓ ${bytes(Math.round(j.speed_bps))}/s`);
        if (j.eta_secs != null && j.speed_bps) parts.push(eta(j.eta_secs));
        parts.push(kind);
        return parts.join(' · ');
      }
    }
  }
  // Reuse the torrent row state classes for the dot/bar colours.
  const CSS_STATE = { queued: 'initializing', downloading: 'downloading', postprocessing: 'initializing', done: 'completed', error: 'error', cancelled: 'paused' };
  const running = (s) => s === 'queued' || s === 'downloading' || s === 'postprocessing';

  // ---------- toast ----------
  let toastEl, toastTimer;
  function toast(msg) {
    if (!toastEl) { toastEl = h('div', 'yt-toast'); toastEl.setAttribute('role', 'status'); document.body.append(toastEl); }
    toastEl.textContent = msg;
    toastEl.classList.add('show');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => toastEl.classList.remove('show'), 4000);
  }
  const report = (e) => toast(errCode(e) === 'VLC_NOT_FOUND' ? 'VLC is not installed' : errMsg(e));

  // ---------- modal ----------
  let modal = null; // { el, sheet, token }
  let probeToken = 0;
  function closeModal() {
    probeToken++;
    if (modal) { modal.el.remove(); modal = null; }
    document.removeEventListener('keydown', onKey, true);
  }
  function onKey(e) {
    if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); closeModal(); }
  }
  function showModal() {
    closeModal();
    const el = h('div', 'yt-modal');
    const sheet = h('div', 'yt-sheet');
    el.append(sheet);
    el.addEventListener('mousedown', (e) => { if (e.target === el) closeModal(); });
    document.body.append(el);
    document.addEventListener('keydown', onKey, true);
    modal = { el, sheet };
    return sheet;
  }
  function closeRow(sheet, label = 'Close') {
    const actions = h('div', 'yt-actions');
    const c = h('button', null, label);
    c.type = 'button';
    c.addEventListener('click', closeModal);
    actions.append(c);
    sheet.append(actions);
    c.focus();
  }

  async function openProbeDialog(url) {
    url = String(url || '').trim();
    if (!/^https?:\/\/\S+$/i.test(url)) return;
    const sheet = showModal();
    const token = ++probeToken;
    const wait = h('div', 'yt-wait');
    wait.append(h('span', 'yt-spin'), h('span', 'yt-url', url));
    sheet.append(wait);
    closeRow(sheet, 'Cancel');
    let p;
    try {
      p = await invoke('yt_probe', { url });
    } catch (e) {
      if (token !== probeToken) return;
      sheet.textContent = '';
      sheet.append(h('p', 'yt-title', "Couldn't read this link"));
      const msg = errCode(e) === 'YTDLP_NOT_FOUND'
        ? 'yt-dlp is not installed. Install it with “brew install yt-dlp”.'
        : errMsg(e);
      sheet.append(h('p', 'yt-err', msg), h('p', 'yt-url', url));
      closeRow(sheet);
      return;
    }
    if (token !== probeToken) return;
    renderProbe(sheet, p);
  }

  function renderProbe(sheet, p) {
    sheet.textContent = '';
    if (p.thumbnail) {
      const box = h('div', 'yt-thumb');
      const img = h('img');
      img.alt = '';
      img.referrerPolicy = 'no-referrer';
      img.onerror = () => box.remove();
      img.src = p.thumbnail;
      const d = duration(p.duration_secs);
      box.append(img);
      if (d) box.append(h('span', 'yt-dur', d));
      sheet.append(box);
    }
    sheet.append(h('p', 'yt-title', p.title));
    const meta = [p.uploader, !p.thumbnail ? duration(p.duration_secs) : null].filter(Boolean).join(' · ');
    if (meta) sheet.append(h('p', 'yt-meta', meta));
    if (p.is_live) {
      sheet.append(h('p', 'yt-err', 'Live streams can’t be downloaded.'));
      closeRow(sheet);
      return;
    }
    const err = h('p', 'yt-err');
    const grid = h('div', 'yt-presets');
    // Heights above the source are pointless (yt-dlp would pick the same file); keep the
    // smallest video preset so there is always one.
    const max = p.max_height;
    for (const [preset, height] of PRESETS) {
      const b = h('button', height ? null : 'audio', height ? PRESET_LABEL[preset] : 'Audio MP3');
      b.type = 'button';
      if (height && max && height > max && height !== 360) {
        b.disabled = true;
        b.title = `Source is ${max}p`;
      }
      const go = async (force) => {
        grid.querySelectorAll('button').forEach((x) => { x.disabled = true; });
        try {
          const job = await invoke('yt_download', { url: p.url, preset, force: !!force });
          upsert(job);
          closeModal();
        } catch (e) {
          err.textContent = errMsg(e);
          if (errCode(e) === 'METERED') {
            const any = h('button', 'yt-force', 'Download anyway');
            any.type = 'button';
            any.addEventListener('click', () => go(true));
            err.textContent = 'On mobile data. ';
            err.append(any);
          }
          grid.querySelectorAll('button').forEach((x) => { x.disabled = !!x.title; });
        }
      };
      b.addEventListener('click', () => go(false));
      grid.append(b);
    }
    sheet.append(grid, err);
    closeRow(sheet, 'Cancel');
    grid.querySelector('button:not(:disabled)')?.focus();
  }

  function askRemove(j) {
    const sheet = showModal();
    sheet.append(h('p', 'yt-title', `Remove ${j.title}?`));
    const lab = h('label', 'yt-check');
    const cb = h('input');
    cb.type = 'checkbox';
    lab.append(cb, document.createTextNode(' Also delete files'));
    sheet.append(lab);
    const actions = h('div', 'yt-actions');
    const cancel = h('button', null, 'Cancel');
    cancel.type = 'button';
    cancel.addEventListener('click', closeModal);
    const ok = h('button', 'danger', 'Remove');
    ok.type = 'button';
    ok.addEventListener('click', () => {
      closeModal();
      invoke('yt_remove', { id: j.id, deleteFile: cb.checked }).then(refresh, report);
    });
    actions.append(cancel, ok);
    sheet.append(actions);
    ok.focus();
  }

  // ---------- rows ----------
  const root = () => document.getElementById('yt-root');
  const rows = new Map(); // id -> { el, name, statsText, fill, btns, last }
  let jobs = [];

  function createRow(id) {
    const el = h('section', 'torrent yt');
    const head = h('div', 'head');
    const name = h('div', 'name');
    const badge = h('span', 'yt-badge', 'VIDEO');
    const title = h('span');
    name.append(badge, title);
    const stats = h('div', 'stats');
    const statsText = h('span');
    stats.append(h('span', 'dot'), statsText);
    const bar = h('div', 'bar');
    const fill = h('i');
    bar.append(fill);
    const r = { el, title, badge, statsText, fill, last: null };
    const ctl = h('div', 'ctl');
    r.play = iconBtn('play', 'Play', () => invoke('play_media', { media: { kind: 'yt', id } }).catch(report));
    r.cancel = iconBtn('stop', 'Cancel download', () => invoke('yt_cancel', { id }).then(refresh, report));
    r.reveal = iconBtn('folder', 'Show in Finder', () => invoke('yt_reveal', { id }).catch(report));
    r.remove = iconBtn('remove', 'Remove', () => askRemove(r.last));
    ctl.append(r.play, r.cancel, r.reveal, r.remove);
    head.append(name, stats, ctl, bar);
    head.addEventListener('dblclick', () => { if (r.last && r.last.state === 'done') r.play.click(); });
    el.append(head);
    return r;
  }

  function updateRow(r, j) {
    if (r.title.textContent !== j.title) { r.title.textContent = j.title; r.el.querySelector('.name').title = `${j.title}\n${j.url}`; }
    const badge = j.preset === 'audio_mp3' ? 'AUDIO' : 'VIDEO';
    if (r.badge.textContent !== badge) r.badge.textContent = badge;
    const cls = `torrent yt s-${CSS_STATE[j.state] || j.state}`;
    if (r.el.className !== cls) r.el.className = cls;
    const line = statsLine(j);
    if (r.statsText.textContent !== line) r.statsText.textContent = line;
    const w = `${((j.state === 'done' ? 1 : j.progress) * 100).toFixed(2)}%`;
    if (r.fill.style.width !== w) r.fill.style.width = w;
    r.play.hidden = j.state !== 'done';
    r.cancel.hidden = !running(j.state);
    r.reveal.hidden = !j.file_path && j.state === 'queued';
    r.last = j;
  }

  function render(list) {
    jobs = list.slice();
    const el = root();
    if (!el) return;
    const ordered = jobs.slice().reverse(); // newest first
    const seen = new Set();
    let prev = null;
    for (const j of ordered) {
      seen.add(j.id);
      let r = rows.get(j.id);
      if (!r) { r = createRow(j.id); rows.set(j.id, r); }
      updateRow(r, j);
      const want = prev ? prev.nextSibling : el.firstChild;
      if (want !== r.el) el.insertBefore(r.el, want);
      prev = r.el;
    }
    for (const [id, r] of rows) if (!seen.has(id)) { r.el.remove(); rows.delete(id); }
    document.body.classList.toggle('yt-has', ordered.length > 0);
  }
  function upsert(job) {
    const i = jobs.findIndex((j) => j.id === job.id);
    if (i >= 0) jobs[i] = job; else jobs.push(job);
    render(jobs);
  }
  const refresh = () => invoke('yt_list').then(render, () => {});

  window.YT = { openProbeDialog, refresh };

  function start() {
    listen('yt-jobs-updated', (e) => render(e.payload));
    refresh();
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start); else start();

  // ---------- mock (browser without Tauri) ----------
  function mockListen(name, cb) {
    (mockListen.subs[name] = mockListen.subs[name] || []).push(cb);
    return Promise.resolve(() => {});
  }
  function mockInvoke(cmd, args = {}) {
    const M = mockInvoke;
    if (!M.jobs) { M.jobs = []; M.n = 0; }
    const emit = () => (mockListen.subs['yt-jobs-updated'] || []).forEach((cb) => cb({ payload: M.jobs.map((j) => ({ ...j })) }));
    const delay = (ms, v) => new Promise((res) => setTimeout(() => res(v), ms));
    switch (cmd) {
      case 'yt_probe':
        if (/fail/.test(args.url)) return delay(700).then(() => Promise.reject('YTDLP_ERROR: [generic] Unsupported URL: ' + args.url));
        return delay(900, {
          url: args.url, title: 'Big Buck Bunny — Official Trailer (2008)', uploader: 'Blender Foundation',
          duration_secs: 596, thumbnail: 'https://peach.blender.org/wp-content/uploads/title_anouncement.jpg',
          is_live: /live/.test(args.url), max_height: 1080,
        });
      case 'yt_download': {
        if (localStorage.getItem('mockMetered') === '1' && !args.force) return delay(100).then(() => Promise.reject('METERED: on mobile data'));
        const j = {
          id: `yt-${++M.n}`, url: args.url, title: 'Big Buck Bunny — Official Trailer (2008)', thumbnail: null,
          preset: args.preset, state: 'queued', progress: 0, downloaded_bytes: 0, total_bytes: null,
          speed_bps: null, eta_secs: null, file_path: null, error: null,
        };
        M.jobs.push(j);
        const total = 48e6;
        const iv = setInterval(() => {
          if (j.state === 'cancelled') return clearInterval(iv);
          if (j.state === 'queued') { j.state = 'downloading'; j.total_bytes = total; }
          else if (j.state === 'downloading') {
            j.downloaded_bytes = Math.min(total, j.downloaded_bytes + 2.5e6);
            j.progress = j.downloaded_bytes / total; j.speed_bps = 2.5e6 * 4; j.eta_secs = Math.round((total - j.downloaded_bytes) / 1e7);
            if (j.progress >= 1) { j.state = 'postprocessing'; j.speed_bps = null; j.eta_secs = null; }
          } else if (j.state === 'postprocessing') {
            j.state = 'done'; j.file_path = `/Users/me/Downloads/Tidebox/${j.title}/${j.title} [abc].mp4`; clearInterval(iv);
          }
          emit();
        }, 250);
        return delay(50, { ...j });
      }
      case 'yt_list': return delay(10, M.jobs.map((j) => ({ ...j })));
      case 'yt_cancel': { const j = M.jobs.find((x) => x.id === args.id); if (j && running(j.state)) j.state = 'cancelled'; emit(); return delay(10, null); }
      case 'yt_remove': M.jobs = M.jobs.filter((x) => x.id !== args.id); emit(); return delay(10, null);
      case 'yt_reveal': return delay(10, null);
      case 'play_media': return delay(10).then(() => Promise.reject('NOT_IMPLEMENTED: no player in mock mode'));
      default: return Promise.reject(`NOT_IMPLEMENTED: ${cmd}`);
    }
  }
  mockListen.subs = {};
})();
