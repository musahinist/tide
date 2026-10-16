// WP3: player window.
(() => {
  'use strict';

  const T = window.__TAURI__;
  const invoke = (cmd, args) =>
    T ? T.core.invoke(cmd, args) : Promise.reject('NOT_IMPLEMENTED: no Tauri');

  const $ = (id) => document.getElementById(id);
  const video = $('video');
  const audio = $('audio');
  const audioView = $('audio-view');
  const audioTitle = $('audio-title');
  const spinner = $('spinner');
  const osd = $('osd');
  const subsSel = $('subs');
  const vlcBtn = $('vlc');
  const msg = $('msg');
  const msgText = $('msg-text');
  const msgGetVlc = $('msg-get-vlc');
  const msgVlc = $('msg-vlc');
  const airBtn = $('airplay');
  const nextBtn = $('next-btn');
  const nextBox = $('next');
  const nextLabel = $('next-label');
  const nextCount = $('next-count');
  const nextPlay = $('next-play');
  const nextCancel = $('next-cancel');

  const AUDIO_EXT = new Set(['mp3', 'm4a', 'aac', 'wav', 'flac', 'ogg', 'oga', 'opus', 'aif', 'aiff']);
  const VLC_URL = 'https://www.videolan.org/vlc/';

  let current = null;      // PlayResult
  let el = video;          // active media element
  let isAudio = false;
  let selectedSub = null;  // track id or null
  let subTouched = false;  // user picked from the menu for this item
  let fallingBack = false;
  let resumeApplied = false;
  const trackEls = new Map(); // sub id -> <track>
  const blobUrls = [];

  // ------------------------------------------------------------ helpers

  const errCode = (e) => String(e && e.message ? e.message : e).split(':')[0].trim();
  const errText = (e) => {
    const s = String(e && e.message ? e.message : e);
    const i = s.indexOf(':');
    return i > 0 ? s.slice(i + 1).trim() : s;
  };

  const extOf = (name) => {
    const m = /\.([a-z0-9]+)$/i.exec(name || '');
    return m ? m[1].toLowerCase() : '';
  };

  function resumeKey() {
    if (!current || !current.media) return null;
    const m = current.media;
    return m.kind === 'torrent' ? `tide:resume:${m.key}:${m.fileIndex}` : null;
  }

  // Duration next to the resume position (main window's "Continue watching").
  const durKey = () => { const k = resumeKey(); return k ? k.replace('tide:resume:', 'tide:dur:') : null; };
  function saveDuration() {
    const k = durKey(), d = el.duration;
    if (k && isFinite(d) && d > 0) lsSet(k, String(Math.round(d)));
  }
  function clearResume() {
    const k = resumeKey(), dk = durKey();
    if (k) lsDel(k);
    if (dk) lsDel(dk);
  }

  function lsGet(k) { try { return localStorage.getItem(k); } catch { return null; } }
  function lsSet(k, v) { try { localStorage.setItem(k, v); } catch { /* ignore */ } }
  function lsDel(k) { try { localStorage.removeItem(k); } catch { /* ignore */ } }

  let osdTimer = 0;
  function flash(text) {
    osd.textContent = text;
    osd.classList.add('show');
    clearTimeout(osdTimer);
    osdTimer = setTimeout(() => osd.classList.remove('show'), 900);
  }

  function setSpinner(on) { spinner.hidden = !on; }

  function showMessage(text, { getVlc = false, openVlc = false } = {}) {
    msgText.textContent = text;
    msgGetVlc.hidden = !getVlc;
    msgVlc.hidden = !openVlc;
    msg.hidden = false;
    setSpinner(false);
  }
  function hideMessage() { msg.hidden = true; }

  // ------------------------------------------------------------ resume

  let lastSave = 0;
  function savePosition(force) {
    const k = resumeKey();
    if (!k || !resumeApplied) return;
    const now = Date.now();
    if (!force && now - lastSave < 3000) return;
    lastSave = now;
    const t = el.currentTime, d = el.duration;
    if (!isFinite(t) || t < 5) return;
    if (isFinite(d) && d > 0 && (t >= 0.9 * d || d - t <= 10)) { clearResume(); return; }
    lsSet(k, String(Math.floor(t)));
  }

  function applyResume() {
    if (resumeApplied) return;
    resumeApplied = true;
    const k = resumeKey();
    const saved = k ? parseFloat(lsGet(k)) : NaN;
    const d = el.duration;
    if (!isFinite(saved) || saved <= 0) return;
    // Resume only if < 90% watched and > 10 s remain.
    if (isFinite(d) && d > 0 && (saved >= 0.9 * d || d - saved <= 10)) return;
    el.currentTime = saved;
  }

  // ------------------------------------------------------------ subtitles

  function clearTracks() {
    for (const t of video.querySelectorAll('track')) t.remove();
    trackEls.clear();
    while (blobUrls.length) URL.revokeObjectURL(blobUrls.pop());
  }

  function renderSubOptions() {
    subsSel.textContent = '';
    const subs = (current && current.subtitles) || [];
    const off = new Option('Subtitles off', '');
    subsSel.add(off);
    for (const s of subs) subsSel.add(new Option(s.label, s.id));
    subsSel.hidden = isAudio || subs.length === 0;
    subsSel.value = selectedSub || '';
  }

  function applyTrackModes() {
    for (const [id, t] of trackEls) t.track.mode = id === selectedSub ? 'showing' : 'hidden';
  }

  async function selectSub(id) {
    selectedSub = id || null;
    subsSel.value = selectedSub || '';
    applyTrackModes();
    if (!selectedSub || trackEls.has(selectedSub) || isAudio) return;
    const sub = (current.subtitles || []).find((s) => s.id === selectedSub);
    if (!sub) return;
    const forUrl = current.stream_url;
    try {
      const vtt = await invoke('get_subtitle_vtt', { subId: sub.id });
      if (!current || current.stream_url !== forUrl || trackEls.has(sub.id)) return;
      const url = URL.createObjectURL(new Blob([vtt], { type: 'text/vtt' }));
      blobUrls.push(url);
      const t = document.createElement('track');
      t.kind = 'subtitles';
      t.label = sub.label;
      if (sub.lang) t.srclang = sub.lang;
      t.src = url;
      video.appendChild(t);
      trackEls.set(sub.id, t);
      t.addEventListener('load', applyTrackModes);
      applyTrackModes();
    } catch (e) {
      if (selectedSub === sub.id) flash('Subtitle unavailable');
      console.warn('subtitle load failed', e);
    }
  }

  // Online subtitles (backend downloads missing preferred languages once; no-op on
  // metered networks). Merged into the menu; auto-selected only if nothing is chosen.
  async function fetchOnlineSubs(pr) {
    if (isAudio || !pr.media || pr.media.kind !== 'torrent') return;
    let list = [];
    try { list = await invoke('fetch_online_subtitles', { media: pr.media }); } catch (e) { console.warn('fetch_online_subtitles', e); }
    if (current !== pr || !list || !list.length) return;
    const have = new Set((pr.subtitles || []).map((s) => s.id));
    const added = list.filter((s) => !have.has(s.id));
    if (!added.length) return;
    pr.subtitles = (pr.subtitles || []).concat(added);
    renderSubOptions();
    if (!selectedSub && !subTouched) selectSub(added[0].id);
  }

  subsSel.addEventListener('change', () => {
    subTouched = true;
    selectSub(subsSel.value);
    subsSel.blur();
  });

  // ------------------------------------------------------------ VLC

  async function openInVlc(auto) {
    if (!current || fallingBack) return;
    fallingBack = true;
    savePosition(true);
    const media = current.media;
    const subId = isAudio ? null : selectedSub;
    // Stop our stream so VLC gets the bandwidth.
    try { el.pause(); el.removeAttribute('src'); el.load(); } catch { /* ignore */ }
    showMessage(auto ? 'This file can’t play here. Opening in VLC…' : 'Opening in VLC…');
    setSpinner(true);
    try {
      await invoke('open_in_vlc', { media, subId });
      // The backend closes this window after launching VLC.
    } catch (e) {
      fallingBack = false;
      if (errCode(e) === 'VLC_NOT_FOUND') {
        showMessage(
          auto
            ? 'This file can’t be played here. Install VLC to play it.'
            : 'VLC isn’t installed.',
          { getVlc: true }
        );
      } else {
        showMessage(`Couldn’t open VLC: ${errText(e)}`, { openVlc: true });
      }
    }
  }

  vlcBtn.addEventListener('click', () => { vlcBtn.blur(); openInVlc(false); });
  msgVlc.addEventListener('click', () => openInVlc(false));
  msgGetVlc.addEventListener('click', () => {
    invoke('open_external', { url: VLC_URL }).catch((e) => console.warn(e));
  });

  // ------------------------------------------------------------ next episode

  const NEXT_LEAD_S = 10;   // prompt this long before the end
  const NEXT_COUNT_S = 5;   // autoplay countdown
  let nextInfo = null;      // NextEpisode {media, title, label, builtin} | null
  let nextSeq = 0;          // drops stale fetches after a source change
  let nextShown = false;
  let nextCancelled = false;
  let nextLeft = 0;
  let nextTimer = 0;
  let watchedMarked = false;

  async function fetchNext() {
    const seq = ++nextSeq;
    const m = current && current.media;
    if (!m || m.kind !== 'torrent') { nextInfo = null; nextBtn.hidden = true; return null; }
    let n = null;
    try { n = await invoke('next_episode', { media: m }); } catch (e) { console.warn('next_episode', e); }
    if (seq !== nextSeq) return null;
    nextInfo = n || null;
    nextBtn.hidden = !nextInfo;
    if (nextInfo) nextBtn.title = `Next: ${nextInfo.label} (N)`;
    return nextInfo;
  }

  function markWatched() {
    const m = current && current.media;
    if (watchedMarked || !m || m.kind !== 'torrent') return;
    watchedMarked = true;
    invoke('set_watched', { key: m.key, fileIndex: m.fileIndex, watched: true })
      .then(() => T.event.emit('watched-changed', { key: m.key, fileIndex: m.fileIndex }).catch(() => {}))
      .catch((e) => { watchedMarked = false; console.warn('set_watched', e); });
  }

  const nearEnd = () => {
    const t = el.currentTime, d = el.duration;
    return el.ended || (isFinite(d) && d > 0 && isFinite(t) && (d - t <= NEXT_LEAD_S + 2 || t >= 0.9 * d));
  };

  function hideNext() {
    clearInterval(nextTimer);
    nextTimer = 0;
    nextShown = false;
    nextBox.hidden = true;
  }

  function renderCount() {
    nextCount.textContent = nextInfo && nextInfo.builtin ? ` — playing in ${nextLeft} s` : '';
  }

  async function showNext() {
    if (nextShown || nextCancelled || fallingBack) return;
    nextShown = true;
    const forUrl = current && current.stream_url;
    const n = await fetchNext(); // fresh: torrents may have progressed
    // No next: stay "shown" (invisible) so timeupdate doesn't re-query until a seek back.
    if (!n || !current || current.stream_url !== forUrl || !nextShown) return;
    nextLabel.textContent = n.label;
    nextPlay.textContent = n.builtin ? 'Play now' : 'Play in VLC';
    nextLeft = NEXT_COUNT_S;
    renderCount();
    nextBox.hidden = false;
    clearInterval(nextTimer);
    if (!n.builtin) return; // VLC files: button only
    nextTimer = setInterval(() => {
      if (el.paused && !el.ended) return; // paused: hold the countdown
      nextLeft -= 1;
      renderCount();
      if (nextLeft <= 0) playNext();
    }, 1000);
  }

  async function playNext() {
    const n = nextInfo || (await fetchNext());
    if (!n) { flash('No next episode'); return; }
    hideNext();
    if (nearEnd()) markWatched();
    savePosition(true);
    try {
      if (n.builtin) {
        const pr = await invoke('play_media', { media: n.media });
        if (pr && pr.mode === 'builtin') load(pr); // the player-load event repeats it (no-op)
      } else {
        try { el.pause(); } catch { /* ignore */ }
        fallingBack = true;
        showMessage('Opening in VLC…');
        setSpinner(true);
        await invoke('open_in_vlc', { media: n.media, subId: null });
      }
    } catch (e) {
      fallingBack = false;
      hideMessage();
      flash(`Next episode unavailable: ${errText(e)}`);
    }
  }

  function nextTick() {
    if (nextCancelled || el.seeking) return;
    const t = el.currentTime, d = el.duration;
    if (!isFinite(d) || d <= 0 || !isFinite(t)) return;
    const left = d - t;
    if (!nextShown && left <= NEXT_LEAD_S && t > 1 && !el.paused) showNext();
    else if (nextShown && left > NEXT_LEAD_S + 2) hideNext(); // seeked back
  }

  nextBtn.addEventListener('click', () => { nextBtn.blur(); playNext(); });
  nextPlay.addEventListener('click', () => playNext());
  nextCancel.addEventListener('click', () => { nextCancelled = true; hideNext(); });

  // ------------------------------------------------------------ AirPlay

  // WKWebView allows AirPlay by default (allowsAirPlayForMediaPlayback = YES; wry 0.57
  // doesn't change it). The button only shows when a target is available.
  if (window.WebKitPlaybackTargetAvailabilityEvent && video.webkitShowPlaybackTargetPicker) {
    video.addEventListener('webkitplaybacktargetavailabilitychanged', (e) => {
      airBtn.hidden = e.availability !== 'available';
    });
    video.addEventListener('webkitcurrentplaybacktargetiswirelesschanged', () => {
      flash(video.webkitCurrentPlaybackTargetIsWireless ? 'Playing on AirPlay' : 'AirPlay off');
    });
    airBtn.addEventListener('click', () => {
      airBtn.blur();
      try { video.webkitShowPlaybackTargetPicker(); } catch (e) { console.warn('airplay', e); }
    });
  } else {
    airBtn.remove();
  }

  // ------------------------------------------------------------ media events

  function onLoadedMetadata(e) {
    if (e.target !== el || fallingBack) return;
    if (!isAudio) {
      if (video.videoWidth === 0) return openInVlc(true);
      if (video.videoTracks && video.videoTracks.length === 0) return openInVlc(true);
    }
    // Only audio files need an audio track; a silent video is fine.
    if (isAudio && el.audioTracks && el.audioTracks.length === 0) return openInVlc(true);
    saveDuration();
    applyResume();
  }

  function onError(e) {
    if (e.target !== el || fallingBack || !el.getAttribute('src')) return;
    const err = el.error;
    if (!err) return;
    // 3 = MEDIA_ERR_DECODE, 4 = MEDIA_ERR_SRC_NOT_SUPPORTED
    // WebKit reports HTTP/network failures before metadata as code 4 too, so only
    // treat 4 as "unsupported format" once metadata was read.
    if (err.code === 3 || (err.code === 4 && el.readyState >= 1)) return openInVlc(true);
    showMessage('Playback stopped because of a network error.', { openVlc: true });
  }

  for (const m of [video, audio]) {
    m.addEventListener('loadedmetadata', onLoadedMetadata);
    m.addEventListener('durationchange', () => { if (m === el && !fallingBack) saveDuration(); });
    m.addEventListener('error', onError);
    m.addEventListener('loadstart', () => m === el && setSpinner(true));
    m.addEventListener('waiting', () => m === el && setSpinner(true));
    m.addEventListener('stalled', () => m === el && !m.paused && setSpinner(true));
    for (const ev of ['playing', 'canplay', 'pause', 'ended']) {
      m.addEventListener(ev, () => m === el && setSpinner(false));
    }
    m.addEventListener('timeupdate', () => {
      if (m !== el) return;
      if (!m.paused && !m.seeking && m.readyState >= 3) setSpinner(false);
      savePosition(false);
      nextTick();
    });
    m.addEventListener('pause', () => m === el && savePosition(true));
    m.addEventListener('ended', () => {
      if (m !== el) return;
      clearResume();
      markWatched();
      if (!nextCancelled) showNext();
    });
    m.addEventListener('volumechange', () => {
      if (m === el) lsSet('tide:volume', String(m.muted ? 0 : m.volume));
    });
  }

  window.addEventListener('beforeunload', () => savePosition(true));
  document.addEventListener('visibilitychange', () => savePosition(true));

  // ------------------------------------------------------------ loading

  function load(pr) {
    if (!pr || !pr.stream_url) return;
    if (current && current.stream_url === pr.stream_url && !fallingBack && el.getAttribute('src')) {
      if (el.paused) el.play().catch(() => {});
      return;
    }
    savePosition(true);

    // Reset everything from the previous item.
    for (const m of [video, audio]) {
      try { m.pause(); m.removeAttribute('src'); m.load(); } catch { /* ignore */ }
    }
    clearTracks();
    hideMessage();
    fallingBack = false;
    resumeApplied = false;
    hideNext();
    nextCancelled = false;
    watchedMarked = false;
    nextInfo = null;
    nextBtn.hidden = true;

    current = pr;
    // Played from another Mac: the file lives there, so VLC here can't open it.
    vlcBtn.style.display = pr.media && pr.media.kind === 'yt' && pr.media.id === 'remote' ? 'none' : '';
    document.title = pr.title || 'Player';
    isAudio = AUDIO_EXT.has(extOf(pr.title)) || AUDIO_EXT.has(extOf(decodeURIComponent(pr.stream_url)));
    el = isAudio ? audio : video;
    video.hidden = isAudio;
    audioView.hidden = !isAudio;
    audioTitle.textContent = pr.title || '';

    const vol = parseFloat(lsGet('tide:volume'));
    if (isFinite(vol)) el.volume = Math.min(1, Math.max(0, vol));

    selectedSub = isAudio ? null : pr.default_subtitle_id || null;
    renderSubOptions();

    setSpinner(true);
    el.src = pr.stream_url;
    el.play().catch(() => { /* autoplay may need a gesture; native controls remain */ });
    if (selectedSub) selectSub(selectedSub);
    subTouched = false;
    fetchOnlineSubs(pr);
    fetchNext();
  }

  // ------------------------------------------------------------ playback controls

  function togglePlay() {
    if (!el.getAttribute('src')) return;
    if (el.paused) el.play().catch(() => {}); else el.pause();
  }

  function seekBy(s) {
    if (!isFinite(el.currentTime)) return;
    let t = el.currentTime + s;
    if (isFinite(el.duration)) t = Math.min(el.duration, t);
    el.currentTime = Math.max(0, t);
  }

  function changeVolume(d) {
    el.muted = false;
    el.volume = Math.round(Math.min(1, Math.max(0, el.volume + d)) * 100) / 100;
    flash(`Volume ${Math.round(el.volume * 100)}%`);
  }

  function changeSpeed(d) {
    const r = Math.min(2, Math.max(0.25, Math.round((el.playbackRate + d) * 100) / 100));
    el.playbackRate = r;
    flash(`Speed ${r}×`);
  }

  const fsElement = () => document.fullscreenElement || document.webkitFullscreenElement || null;
  function toggleFullscreen() {
    const stage = $('stage');
    if (fsElement()) {
      (document.exitFullscreen || document.webkitExitFullscreen || (() => {})).call(document);
    } else {
      const req = stage.requestFullscreen || stage.webkitRequestFullscreen;
      if (req) {
        const p = req.call(stage);
        if (p && p.catch) p.catch(() => video.webkitEnterFullscreen && video.webkitEnterFullscreen());
      } else if (video.webkitEnterFullscreen) {
        video.webkitEnterFullscreen();
      }
    }
  }

  window.addEventListener('keydown', (e) => {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    const tag = e.target && e.target.tagName;
    if (tag === 'SELECT' || tag === 'INPUT') return;
    if (!msg.hidden && e.key !== 'Escape') return;
    let handled = true;
    switch (e.key) {
      case ' ': case 'k': case 'K': togglePlay(); break;
      case 'ArrowLeft': seekBy(-5); break;
      case 'ArrowRight': seekBy(5); break;
      case 'j': case 'J': seekBy(-10); break;
      case 'l': case 'L': seekBy(10); break;
      case 'ArrowUp': changeVolume(0.1); break;
      case 'ArrowDown': changeVolume(-0.1); break;
      case 'f': case 'F': toggleFullscreen(); break;
      case 'Escape': if (fsElement()) toggleFullscreen(); else handled = false; break;
      case '>': case '.': changeSpeed(0.25); break;
      case '<': case ',': changeSpeed(-0.25); break;
      case 'n': case 'N': playNext(); break;
      default: handled = false;
    }
    if (handled) { e.preventDefault(); e.stopPropagation(); poke(); }
  }, true);

  // Click on the picture toggles play (but not on the native control bar).
  // If WebKit already toggled natively, don't toggle a second time.
  const CONTROLS_H = 64;
  video.addEventListener('click', (e) => {
    if (e.offsetY > video.clientHeight - CONTROLS_H) return;
    const was = video.paused;
    setTimeout(() => { if (video.paused === was) togglePlay(); }, 60);
  });
  video.addEventListener('dblclick', (e) => {
    if (e.offsetY > video.clientHeight - CONTROLS_H) return;
    e.preventDefault();
    toggleFullscreen();
  });

  // ------------------------------------------------------------ hover HUD

  let idleTimer = 0;
  function poke() {
    document.body.classList.add('active');
    document.body.classList.remove('idle');
    clearTimeout(idleTimer);
    idleTimer = setTimeout(() => {
      document.body.classList.remove('active');
      if (!el.paused) document.body.classList.add('idle');
    }, 2500);
  }
  document.addEventListener('mousemove', poke);
  document.addEventListener('mouseleave', () => {
    clearTimeout(idleTimer);
    document.body.classList.remove('active', 'idle');
  });

  // ------------------------------------------------------------ boot

  async function boot() {
    if (!T) { showMessage('Open this window from Tide.'); return; }
    try {
      await T.event.listen('player-load', (ev) => load(ev.payload));
    } catch (e) { console.warn('listen failed', e); }
    try {
      const pr = await invoke('player_current');
      if (pr && !current) load(pr);
    } catch (e) {
      showMessage(errText(e));
    }
  }
  boot();
})();
