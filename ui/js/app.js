/* kroppi — trim and crop videos, in a TUI-flavoured UI. */
(() => {
  'use strict';

  const $ = (s) => document.querySelector(s);
  const $$ = (s) => Array.from(document.querySelectorAll(s));
  const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

  const video = $('#video');
  const canvas = $('#preview');
  const ctx = canvas.getContext('2d');

  const HANDLE = 10;      // handle hit radius (css px)
  const MIN_CROP = 16;    // smallest crop, in video pixels
  const ACCENT = '#89b4fa';
  const PREFS_KEY = 'kroppi.prefs.v1';
  // aspect presets, in the order of the 1-8 keys
  const ASPECTS = ['free', 'orig', '1.7778', '1.3333', '1', '0.8', '0.5625', '2.3333'];
  // containers ffmpeg can copy this file's streams into as they are
  const COPYABLE = ['mp4', 'm4v', 'mov', 'mkv', 'webm'];

  // The Rust side of the app (src-tauri/) reads videos with ffprobe, streams
  // them to this window from a local server, and exports them with ffmpeg.
  const TAURI = window.__TAURI__;
  const invoke = (cmd, args) => TAURI.core.invoke(cmd, args);

  const S = {
    loaded: false, probe: null, dur: 0, vw: 0, vh: 0, fps: 30,
    pos: 0, start: 0, end: 0,
    crop: { x: 0, y: 0, w: 16, h: 9 }, aspect: 'free', view: 'edit',
    mode: 'exact', quality: 'medium', mute: false,
    playing: false, busy: false, ffmpeg: true, saved: false, lastOut: null,
  };
  const view = { w: 0, h: 0, dpr: 1 };

  // ------------------------------------------------------------ prefs
  function loadPrefs() {
    try {
      const p = JSON.parse(localStorage.getItem(PREFS_KEY) || '{}');
      if (p.mode === 'exact' || p.mode === 'fast') S.mode = p.mode;
      if (['high', 'medium', 'small'].includes(p.quality)) S.quality = p.quality;
    } catch (_) { /* ignore */ }
  }
  function savePrefs() {
    try { localStorage.setItem(PREFS_KEY, JSON.stringify({ mode: S.mode, quality: S.quality })); } catch (_) { /* ignore */ }
  }

  // ------------------------------------------------------------ helpers
  function fmtTime(t) {
    if (!isFinite(t)) t = 0;
    const neg = t < 0; t = Math.abs(t);
    const cs = Math.round(t * 100);
    const h = Math.floor(cs / 360000), m = Math.floor((cs % 360000) / 6000), s = Math.floor((cs % 6000) / 100), c = cs % 100;
    const mm = h ? String(m).padStart(2, '0') : String(m);
    return (neg ? '-' : '') + (h ? h + ':' : '') + mm + ':' + String(s).padStart(2, '0') + '.' + String(c).padStart(2, '0');
  }
  function parseTime(str) {
    str = String(str).trim().replace(',', '.').replace(/s$/i, '');
    if (!str) return NaN;
    const parts = str.split(':');
    if (parts.length > 3 || parts.some((p) => p === '' || isNaN(+p))) return NaN;
    return parts.reduce((acc, p) => acc * 60 + parseFloat(p), 0);
  }
  function fmtBytes(n) {
    if (n >= 1 << 30) return (n / (1 << 30)).toFixed(2) + ' GB';
    if (n >= 1 << 20) return (n / (1 << 20)).toFixed(1) + ' MB';
    return Math.max(1, Math.round(n / 1024)) + ' KB';
  }
  const esc = (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;');
  const minDur = () => 1 / S.fps;
  const even = (v) => Math.floor(v / 2) * 2;
  const isCropped = () => S.loaded && !(S.crop.x === 0 && S.crop.y === 0 && S.crop.w === S.vw && S.crop.h === S.vh);
  const ratio = () => (S.aspect === 'free' ? null : S.aspect === 'orig' ? S.vw / S.vh : parseFloat(S.aspect));
  /** what the export will actually do */
  const effMode = () => (isCropped() ? 'exact' : S.mode);
  const outSize = () => (isCropped() ? { w: even(S.crop.w), h: even(S.crop.h) } : { w: S.vw, h: S.vh });
  const errText = (e) => String((e && e.message) || e);
  const dirname = (p) => p.replace(/\/[^/]*$/, '') || '/';
  const basename = (p) => p.replace(/^.*\//, '');

  // ------------------------------------------------------------ status
  let statusTimer = 0;
  function status(msg, sticky) {
    const el = $('#status');
    el.textContent = msg || '';
    clearTimeout(statusTimer);
    if (msg && !sticky) statusTimer = setTimeout(() => { el.textContent = ''; }, 5000);
  }

  // ------------------------------------------------------------ video loading
  // How long a video may take to show its first frame. WebKitGTK can stall
  // without firing 'error' (an mkv with subtitles waits for subtitle data
  // forever), so a load that takes longer counts as failed.
  const LOAD_TIMEOUT_MS = 10000;

  function openVideoUrl(url) {
    return new Promise((resolve, reject) => {
      let timer = 0;
      const finish = (err) => {
        clearTimeout(timer);
        video.removeEventListener('loadeddata', onOk);
        video.removeEventListener('error', onErr);
        if (!err) { resolve(); return; }
        // stop the stalled player so it lets go of the file and the server
        video.removeAttribute('src');
        video.load();
        reject(err);
      };
      const onOk = () => finish(video.videoWidth > 0 ? null : new Error('no video track'));
      const onErr = () => finish(new Error('cannot decode'));
      video.addEventListener('loadeddata', onOk);
      video.addEventListener('error', onErr);
      timer = setTimeout(() => finish(new Error('timed out')), LOAD_TIMEOUT_MS);
      video.src = url;
      video.load();
    });
  }

  // Make a preview copy (see make_proxy in main.rs) and open it.
  async function openProxy(probe, kind, label) {
    const onProgress = new TAURI.core.Channel();
    onProgress.onmessage = (p) => status(`${label} … ${Math.round(p * 100)}%`, true);
    status(label + ' …', true);
    const url = await invoke('make_proxy', { path: probe.path, duration: probe.duration, kind, onProgress });
    await openVideoUrl(url);
  }

  async function loadPath(path) {
    if (S.busy) return;
    if (!S.ffmpeg) { status('kroppi needs ffmpeg: install it and try again', true); return; }
    stopPlay();
    const name = basename(path);
    status('opening ' + name + ' …', true);
    let probe;
    try {
      probe = await invoke('open_video', { path });
    } catch (e) {
      status("can't open " + name + ': ' + errText(e), true);
      return;
    }
    // Try the original, then a remux without subtitles (fast: streams copied),
    // then a small re-encoded copy (slow, plays anything ffmpeg reads).
    // Files with subtitles skip the original: the window stalls on those.
    let opened = false;
    if (!probe.subtitles) {
      try { await openVideoUrl(probe.url); opened = true; } catch (_) { /* try a copy */ }
    }
    if (!opened) {
      S.busy = true;
      try {
        try {
          await openProxy(probe, 'remux', `preparing ${name}`);
        } catch (_) {
          await openProxy(probe, 'encode', `making a preview copy of ${name}`);
        }
      } catch (e) {
        status("can't preview " + name + ': ' + errText(e), true);
        return;
      } finally {
        S.busy = false;
      }
    }
    S.probe = probe;
    S.vw = probe.width;
    S.vh = probe.height;
    S.fps = probe.fps;
    S.dur = probe.duration;
    S.start = 0;
    S.end = S.dur;
    S.pos = 0;
    S.crop = { x: 0, y: 0, w: S.vw, h: S.vh };
    S.aspect = 'free';
    S.view = 'edit';
    S.mute = false;
    S.lastOut = null;
    S.loaded = true;
    S.saved = false;
    video.muted = false;
    syncDirty();
    $('#empty').classList.add('hidden');
    canvas.classList.remove('hidden');
    const codecs = probe.vcodec + (probe.acodec ? '/' + probe.acodec : '');
    $('#file-label').textContent = `${probe.name}  ${S.vw}×${S.vh}  ${fmtTime(S.dur)}  ${codecs}`;
    $('#file-label').classList.remove('dim');
    status('opened ' + probe.name);
    layout();
    seek(0);
    refreshAll();
  }

  async function pickVideo() {
    if (S.busy) return;
    try {
      const p = await invoke('pick_video');
      if (p) loadPath(p);
    } catch (e) { status('open failed: ' + errText(e)); }
  }

  // ------------------------------------------------------------ seeking & playback
  let seekPending = null, seekBusy = false;
  function seek(t) {
    if (!S.loaded) return;
    S.pos = clamp(t, 0, S.dur);
    seekPending = S.pos;
    pumpSeek();
    refreshTime();
  }
  function pumpSeek() {
    if (seekBusy || seekPending == null) return;
    const t = seekPending;
    seekPending = null;
    if (Math.abs(video.currentTime - t) < 1e-4) { draw(); return; }
    seekBusy = true;
    video.currentTime = t;
  }
  video.addEventListener('seeked', () => {
    seekBusy = false;
    if (!S.playing) draw();
    pumpSeek();
  });

  function play() {
    if (!S.loaded || S.playing) return;
    if (S.pos < S.start || S.pos >= S.end - 0.02) video.currentTime = S.start;
    S.playing = true;
    video.play().catch(() => { S.playing = false; refreshPlay(); });
    refreshPlay();
    requestAnimationFrame(tick);
  }
  function stopPlay() {
    if (!S.playing) return;
    S.playing = false;
    video.pause();
    S.pos = video.currentTime;
    refreshPlay();
    refreshTime();
    draw();
  }
  function togglePlay() { S.playing ? stopPlay() : play(); }
  function tick() {
    if (!S.playing) return;
    if (video.currentTime >= S.end || video.ended) {
      video.currentTime = S.start;
      if (video.paused) video.play().catch(() => {});
    }
    S.pos = video.currentTime;
    draw();
    refreshTime();
    requestAnimationFrame(tick);
  }
  function step(frames) {
    stopPlay();
    seek(S.pos + frames / S.fps);
  }

  // ------------------------------------------------------------ drawing
  function layout() {
    const stage = $('#stage');
    const sw = Math.max(50, stage.clientWidth - 24), sh = Math.max(50, stage.clientHeight - 24);
    let aw = 16, ah = 9;
    if (S.loaded) {
      if (S.view === 'result') { aw = S.crop.w; ah = S.crop.h; } else { aw = S.vw; ah = S.vh; }
    }
    const k = Math.min(sw / aw, sh / ah);
    view.w = Math.max(1, Math.floor(aw * k));
    view.h = Math.max(1, Math.floor(ah * k));
    view.dpr = window.devicePixelRatio || 1;
    canvas.style.width = view.w + 'px';
    canvas.style.height = view.h + 'px';
    canvas.width = Math.round(view.w * view.dpr);
    canvas.height = Math.round(view.h * view.dpr);
    canvas.classList.toggle('result', S.view === 'result');
    buildRuler();
    draw();
  }

  function draw() {
    ctx.setTransform(view.dpr, 0, 0, view.dpr, 0, 0);
    ctx.clearRect(0, 0, view.w, view.h);
    if (!S.loaded) return;
    ctx.imageSmoothingQuality = 'high';
    // a preview copy can be smaller than the real video
    const vk = video.videoWidth / S.vw, c = S.crop;
    if (S.view === 'result') {
      ctx.drawImage(video, c.x * vk, c.y * vk, c.w * vk, c.h * vk, 0, 0, view.w, view.h);
      return;
    }
    ctx.drawImage(video, 0, 0, view.w, view.h);
    drawCrop();
  }

  function drawCrop() {
    const k = view.w / S.vw, c = S.crop;
    const x = c.x * k, y = c.y * k, w = c.w * k, h = c.h * k;
    if (isCropped()) {
      ctx.fillStyle = 'rgba(17,17,27,0.65)';
      ctx.fillRect(0, 0, view.w, y);
      ctx.fillRect(0, y + h, view.w, view.h - y - h);
      ctx.fillRect(0, y, x, h);
      ctx.fillRect(x + w, y, view.w - x - w, h);
      ctx.strokeStyle = 'rgba(137,180,250,0.35)';
      ctx.beginPath();
      for (let i = 1; i < 3; i++) {
        ctx.moveTo(x + (w * i) / 3, y); ctx.lineTo(x + (w * i) / 3, y + h);
        ctx.moveTo(x, y + (h * i) / 3); ctx.lineTo(x + w, y + (h * i) / 3);
      }
      ctx.stroke();
    }
    ctx.strokeStyle = ACCENT;
    ctx.lineWidth = 1;
    ctx.strokeRect(x + 0.5, y + 0.5, w - 1, h - 1);
    ctx.fillStyle = ACCENT;
    for (const [hx, hy] of [[x, y], [x + w, y], [x, y + h], [x + w, y + h]]) ctx.fillRect(hx - 5, hy - 5, 10, 10);
    if (!ratio()) {
      for (const [hx, hy] of [[x + w / 2, y], [x + w / 2, y + h], [x, y + h / 2], [x + w, y + h / 2]]) ctx.fillRect(hx - 3, hy - 3, 6, 6);
    }
  }

  // ------------------------------------------------------------ crop
  function clampCrop(c) {
    const w = even(clamp(Math.round(c.w), MIN_CROP, S.vw)), h = even(clamp(Math.round(c.h), MIN_CROP, S.vh));
    return { x: clamp(Math.round(c.x), 0, S.vw - w), y: clamp(Math.round(c.y), 0, S.vh - h), w, h };
  }

  /** The largest rectangle of aspect `a` that fits, centred on (cx, cy) as far as it can be. */
  function fitAspect(a, cx, cy) {
    let w = S.vw, h = w / a;
    if (h > S.vh) { h = S.vh; w = h * a; }
    return clampCrop({ x: cx - w / 2, y: cy - h / 2, w, h });
  }

  function setCropAspect(name) {
    if (!S.loaded) return;
    S.aspect = name;
    const a = ratio();
    if (a) {
      const c = S.crop;
      S.crop = fitAspect(a, c.x + c.w / 2, c.y + c.h / 2);
    }
    cropChanged();
  }

  /** Scale the crop around its centre, keeping its shape. */
  function scaleCrop(f) {
    const c = S.crop, a = c.w / c.h, cx = c.x + c.w / 2, cy = c.y + c.h / 2;
    let w = c.w * f, h = c.h * f;
    if (w > S.vw) { w = S.vw; h = w / a; }
    if (h > S.vh) { h = S.vh; w = h * a; }
    if (w < MIN_CROP || h < MIN_CROP) return;
    S.crop = clampCrop({ x: cx - w / 2, y: cy - h / 2, w, h });
    cropChanged();
  }

  function cropChanged() {
    if (S.view === 'result') layout();
    refreshAll();
  }

  // ------------------------------------------------------------ preview mouse
  let drag = null;

  function canvasPoint(e) {
    const r = canvas.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  }

  function cropHit(p) {
    const k = view.w / S.vw, c = S.crop;
    const x0 = c.x * k, y0 = c.y * k, x1 = (c.x + c.w) * k, y1 = (c.y + c.h) * k;
    const corners = { nw: [x0, y0], ne: [x1, y0], sw: [x0, y1], se: [x1, y1] };
    for (const [name, [cx, cy]] of Object.entries(corners)) {
      if (Math.hypot(cx - p.x, cy - p.y) <= HANDLE) return name;
    }
    if (!ratio()) {
      const inX = p.x > x0 && p.x < x1, inY = p.y > y0 && p.y < y1;
      if (inX && Math.abs(p.y - y0) <= HANDLE / 2) return 'n';
      if (inX && Math.abs(p.y - y1) <= HANDLE / 2) return 's';
      if (inY && Math.abs(p.x - x0) <= HANDLE / 2) return 'w';
      if (inY && Math.abs(p.x - x1) <= HANDLE / 2) return 'e';
    }
    // with no crop yet, dragging anywhere draws one
    if (isCropped() && p.x >= x0 && p.x <= x1 && p.y >= y0 && p.y <= y1) return 'move';
    return 'new';
  }

  function cropFromAnchor(ax, ay, px, py) {
    const a = ratio();
    let w = Math.abs(px - ax), h = Math.abs(py - ay);
    if (a) { if (w / Math.max(h, 1e-6) > a) w = h * a; else h = w / a; }
    const sx = px < ax ? -1 : 1, sy = py < ay ? -1 : 1;
    const maxW = sx > 0 ? S.vw - ax : ax, maxH = sy > 0 ? S.vh - ay : ay;
    if (w > maxW) { w = maxW; if (a) h = w / a; }
    if (h > maxH) { h = maxH; if (a) w = h * a; }
    w = Math.max(MIN_CROP, w);
    h = Math.max(MIN_CROP, h);
    return clampCrop({ x: sx > 0 ? ax : ax - w, y: sy > 0 ? ay : ay - h, w, h });
  }

  canvas.addEventListener('pointerdown', (e) => {
    if (!S.loaded || e.button !== 0 || S.view !== 'edit') return;
    stopPlay();
    const p = canvasPoint(e);
    canvas.setPointerCapture(e.pointerId);
    const k = view.w / S.vw, part = cropHit(p), c = S.crop;
    const anchors = { nw: [c.x + c.w, c.y + c.h], ne: [c.x, c.y + c.h], sw: [c.x + c.w, c.y], se: [c.x, c.y] };
    if (part === 'move') drag = { kind: 'move', p0: p, c0: Object.assign({}, c) };
    else if (part === 'new') drag = { kind: 'new', p0: p, anchor: [clamp(p.x / k, 0, S.vw), clamp(p.y / k, 0, S.vh)] };
    else if (part.length === 1) drag = { kind: 'edge', side: part, c0: Object.assign({}, c) };
    else drag = { kind: 'corner', anchor: anchors[part] };
  });

  canvas.addEventListener('pointermove', (e) => {
    if (!S.loaded || S.view !== 'edit') return;
    const p = canvasPoint(e);
    if (!drag) { hoverCursor(p); return; }
    const k = view.w / S.vw;
    const px = clamp(p.x / k, 0, S.vw), py = clamp(p.y / k, 0, S.vh);
    if (drag.kind === 'move') {
      const c0 = drag.c0;
      S.crop = clampCrop({ x: c0.x + (p.x - drag.p0.x) / k, y: c0.y + (p.y - drag.p0.y) / k, w: c0.w, h: c0.h });
    } else if (drag.kind === 'new') {
      // a click without a drag leaves the crop alone
      if (Math.hypot(p.x - drag.p0.x, p.y - drag.p0.y) < 4) return;
      S.crop = cropFromAnchor(drag.anchor[0], drag.anchor[1], px, py);
    } else if (drag.kind === 'corner') {
      S.crop = cropFromAnchor(drag.anchor[0], drag.anchor[1], px, py);
    } else if (drag.kind === 'edge') {
      const c0 = drag.c0, c = Object.assign({}, c0);
      if (drag.side === 'w') { c.x = Math.min(px, c0.x + c0.w - MIN_CROP); c.w = c0.x + c0.w - c.x; }
      if (drag.side === 'e') c.w = Math.max(MIN_CROP, px - c0.x);
      if (drag.side === 'n') { c.y = Math.min(py, c0.y + c0.h - MIN_CROP); c.h = c0.y + c0.h - c.y; }
      if (drag.side === 's') c.h = Math.max(MIN_CROP, py - c0.y);
      S.crop = clampCrop(c);
    }
    refreshCrop();
    refreshOutput();
    draw();
  });

  const endDrag = () => { if (drag) { drag = null; refreshAll(); } };
  canvas.addEventListener('pointerup', endDrag);
  canvas.addEventListener('pointercancel', endDrag);

  function hoverCursor(p) {
    const h = cropHit(p);
    const cursors = { n: 'ns-resize', s: 'ns-resize', w: 'ew-resize', e: 'ew-resize', nw: 'nwse-resize', se: 'nwse-resize', ne: 'nesw-resize', sw: 'nesw-resize', move: 'move', new: 'crosshair' };
    canvas.style.cursor = cursors[h];
  }

  function setView(v) {
    if (!S.loaded) return;
    S.view = v;
    layout();
    refreshAll();
  }

  // ------------------------------------------------------------ timeline
  const track = $('.tl-track');
  const timeToPct = (t) => (S.dur ? (t / S.dur) * 100 : 0);

  function buildRuler() {
    const ruler = $('#tl-ruler'), ticks = $('#tl-ticks');
    ruler.innerHTML = '';
    ticks.innerHTML = '';
    const width = track.clientWidth || 600;
    const tick = (x, major) => {
      const i = document.createElement('i');
      i.style.left = Math.round(x) + 'px';
      if (major) i.className = 'major';
      ticks.appendChild(i);
    };
    if (!S.loaded) {
      for (let x = 8; x < width; x += 10) tick(x, false);
      return;
    }
    const steps = [0.1, 0.25, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800, 3600, 7200];
    const step = steps.find((s) => (s / S.dur) * width >= 70) || 14400;
    // minor ticks: the finest of 1/5, 1/4, 1/2 of a label step that stays >= 6px apart
    const minor = [step / 5, step / 4, step / 2, step].find((m) => (m / S.dur) * width >= 6);
    const n = Math.round(S.dur / minor);
    for (let k = 0; k <= n; k++) {
      const t = k * minor;
      const major = Math.abs(t / step - Math.round(t / step)) < 1e-6;
      tick((t / S.dur) * (width - 1), major);
    }
    for (let t = 0; t <= S.dur + 1e-6; t += step) {
      const span = document.createElement('span');
      span.style.left = timeToPct(t) + '%';
      span.textContent = '│' + fmtTime(t).replace(/\.00$/, '');
      ruler.appendChild(span);
    }
  }

  function setSpan(a, b) { S.start = a; S.end = b; }
  function nudgeSpan(frames) {
    const len = S.end - S.start;
    const na = clamp(S.start + frames / S.fps, 0, S.dur - len);
    setSpan(na, na + len);
    refreshAll();
  }

  let tlDrag = null;
  const GRIP = 9; // px: top band of the selection that slides it
  function timeAt(e) {
    const r = track.getBoundingClientRect();
    return clamp(((e.clientX - r.left) / r.width) * S.dur, 0, S.dur);
  }
  function tlPart(e) {
    const r = track.getBoundingClientRect();
    const px = (t) => r.left + (t / S.dur) * r.width;
    if (Math.abs(e.clientX - px(S.start)) <= 8) return 'start';
    if (Math.abs(e.clientX - px(S.end)) <= 8) return 'end';
    if (e.clientY - r.top <= GRIP && e.clientX > px(S.start) && e.clientX < px(S.end)) return 'grip';
    return 'pos';
  }
  track.addEventListener('pointerdown', (e) => {
    if (!S.loaded) return;
    stopPlay();
    track.setPointerCapture(e.pointerId);
    const part = tlPart(e);
    if (part === 'grip') {
      tlDrag = { kind: 'move', t0: timeAt(e), a0: S.start, b0: S.end };
      track.style.cursor = 'grabbing';
      return;
    }
    tlDrag = part;
    tlMove(e);
  });
  track.addEventListener('pointermove', (e) => {
    if (tlDrag) tlMove(e);
    else if (S.loaded) {
      const part = tlPart(e);
      track.style.cursor = part === 'start' || part === 'end' ? 'ew-resize' : part === 'grip' ? 'grab' : 'pointer';
    }
  });
  const endTl = () => {
    if (tlDrag && tlDrag.kind === 'move') track.style.cursor = 'grab';
    tlDrag = null;
  };
  track.addEventListener('pointerup', endTl);
  track.addEventListener('pointercancel', endTl);
  function tlMove(e) {
    const t = timeAt(e);
    if (tlDrag && tlDrag.kind === 'move') {
      const len = tlDrag.b0 - tlDrag.a0;
      const na = clamp(tlDrag.a0 + (t - tlDrag.t0), 0, S.dur - len);
      setSpan(na, na + len);
    } else if (tlDrag === 'start') { setSpan(clamp(t, 0, S.end - minDur()), S.end); seek(S.start); }
    else if (tlDrag === 'end') { setSpan(S.start, clamp(t, S.start + minDur(), S.dur)); seek(S.end); }
    else seek(t);
    refreshAll();
  }

  function setStart(t) {
    const len = S.end - S.start;
    t = clamp(t, 0, S.dur);
    if (S.end - t < minDur()) setSpan(t, Math.min(S.dur, t + Math.max(len, minDur())));
    else setSpan(t, S.end);
    refreshAll();
  }
  function setEnd(t) {
    const len = S.end - S.start;
    t = clamp(t, 0, S.dur);
    if (t - S.start < minDur()) setSpan(Math.max(0, t - Math.max(len, minDur())), t);
    else setSpan(S.start, t);
    refreshAll();
  }

  // ------------------------------------------------------------ export
  function progress(frac, label) {
    const width = 40, n = Math.round(frac * width);
    $('#exp-bar').textContent = '[' + '█'.repeat(n) + '░'.repeat(width - n) + '] ' + String(Math.round(frac * 100)).padStart(3) + '%';
    $('#exp-stage').textContent = label || '';
  }

  function suggestedPath() {
    const p = S.probe, stem = p.name.replace(/\.[^.]+$/, '') || 'video';
    const dir = S.lastOut ? dirname(S.lastOut) : dirname(p.path);
    return `${dir}/${stem}-kroppi.${p.container}`;
  }

  async function exportVideo() {
    if (!S.loaded || S.busy) return;
    stopPlay();
    let output;
    try {
      output = await invoke('pick_save', { suggested: suggestedPath() });
    } catch (e) { status('save dialog failed: ' + errText(e)); return; }
    if (!output) return;
    if (output === S.probe.path) { status("pick a different file: kroppi won't overwrite the video it's reading"); return; }

    S.busy = true;
    const job = (S.job = { cancelled: false });
    $('#modal-export').classList.remove('hidden');
    $('#exp-progress').classList.remove('hidden');
    $('#exp-result').classList.add('hidden');
    $('#exp-title').textContent = 'exporting';
    const label = effMode() === 'fast' ? 'copying (lossless)' : 'encoding';
    progress(0, label + ' → ' + basename(output));
    try {
      const onProgress = new TAURI.core.Channel();
      onProgress.onmessage = (p) => { if (!job.cancelled) progress(p, label + ' → ' + basename(output)); };
      const res = await invoke('export', {
        opts: {
          input: S.probe.path, output, start: S.start, end: S.end,
          crop: isCropped() ? S.crop : null,
          mode: S.mode, mute: S.mute, quality: S.quality, width: S.vw, height: S.vh,
        },
        onProgress,
      });
      if (job.cancelled) return;
      S.lastOut = res.path;
      const { w, h } = outSize();
      $('#exp-meta').innerHTML = `<span class="accent">${fmtBytes(res.size)}</span> &middot; ${w}&times;${h} &middot; ${fmtTime(S.end - S.start)} &middot; ${res.mode === 'fast' ? 'lossless copy' : 're-encoded'}${S.mute ? ' &middot; muted' : ''}`;
      $('#exp-path').textContent = res.path;
      $('#exp-note').textContent = res.note || '';
      $('#exp-note').classList.toggle('hidden', !res.note);
      $('#exp-title').textContent = 'done';
      $('#exp-progress').classList.add('hidden');
      $('#exp-result').classList.remove('hidden');
      status('saved ' + res.path);
      markSaved();
    } catch (err) {
      const cancelled = job.cancelled || errText(err) === 'cancelled';
      $('#modal-export').classList.add('hidden');
      status(cancelled ? 'export cancelled' : 'export failed: ' + errText(err), !cancelled);
      if (!cancelled) console.error(err);
    } finally {
      S.busy = false;
      S.job = null;
    }
  }

  function closeExport() {
    if (S.job) {
      S.job.cancelled = true;
      invoke('cancel').catch(() => {});
    }
    $('#modal-export').classList.add('hidden');
  }

  // ------------------------------------------------------------ UI refresh
  function setSeg(sel, attr, value) {
    $$(sel + ' button').forEach((b) => b.classList.toggle('on', String(b.dataset[attr]) === String(value)));
  }
  function setVal(id, v) {
    const el = document.getElementById(id);
    if (el && document.activeElement !== el) el.value = v;
  }
  function setText(id, v, changed) {
    const el = document.getElementById(id);
    el.textContent = v;
    el.classList.toggle('changed', !!changed);
  }

  function refreshTime() {
    setVal('t-pos', fmtTime(S.pos));
    $('#tl-playhead').style.left = timeToPct(S.pos) + '%';
  }
  function refreshPlay() {
    $('#play-btn').innerHTML = S.playing ? '[ &#x275A;&#x275A; pause ]' : '[ &#x25B6; play ]';
  }
  function refreshTimeline() {
    setVal('t-start', fmtTime(S.start));
    setVal('t-end', fmtTime(S.end));
    setVal('t-dur', fmtTime(S.end - S.start));
    $('#tl-range').style.left = timeToPct(S.start) + '%';
    $('#tl-range').style.width = timeToPct(S.end - S.start) + '%';
    $('#tl-h-start').style.left = timeToPct(S.start) + '%';
    $('#tl-h-end').style.left = timeToPct(S.end) + '%';
    $('#tl-info').textContent = S.loaded
      ? `keeping ${fmtTime(S.end - S.start)} of ${fmtTime(S.dur)} · ${Math.round((S.end - S.start) * S.fps)} frames @ ${+S.fps.toFixed(3)} fps` : '';
    refreshTime();
  }
  function refreshCrop() {
    const c = S.crop;
    setVal('crop-x', c.x); setVal('crop-y', c.y); setVal('crop-w', c.w); setVal('crop-h', c.h);
    setSeg('#crop-aspect', 'aspect', S.aspect);
    setSeg('#view-seg', 'view', S.view);
    $('#preview-title').textContent = S.view === 'result' ? 'result' : 'crop';
    $('#preview-info').textContent = !S.loaded ? ''
      : isCropped() ? `${c.w}×${c.h} of ${S.vw}×${S.vh}` + (S.view === 'edit' ? ' · drag to change' : '')
        : 'no crop · drag on the video to draw one';
  }
  function refreshOutput() {
    setSeg('#mode-seg', 'mode', effMode());
    setSeg('#quality-seg', 'quality', S.quality);
    $('#mode-seg [data-mode=fast]').classList.toggle('locked', isCropped());
    const copyable = S.loaded && COPYABLE.includes((S.probe.name.match(/\.([^.]+)$/) || [])[1]?.toLowerCase());
    $('#mode-hint').textContent = isCropped() ? 'fast is off while cropped (crops re-encode)'
      : S.mode === 'fast' ? (S.loaded && !copyable ? 'this file type can only be copied into .mkv' : 'lossless copy · cuts snap to keyframes')
        : 're-encodes · cuts on the exact frame';
    $$('#quality-seg button').forEach((b) => { b.disabled = effMode() === 'fast'; });
    const audio = S.loaded && !!S.probe.acodec;
    $('#mute').checked = S.mute;
    $('#mute').disabled = S.loaded && !audio;
    $('#audio-info').textContent = S.loaded && !audio ? 'no audio track' : '';
    if (!S.loaded) return;
    const { w, h } = outSize(), p = S.probe;
    const trimmed = S.start > 0 || S.end < S.dur;
    setText('sum-size', `${w}×${h}`, isCropped());
    setText('sum-len', fmtTime(S.end - S.start), trimmed);
    const codecs = effMode() === 'fast'
      ? `${p.vcodec}${p.acodec && !S.mute ? '/' + p.acodec : ''} · copy`
      : (p.container === 'webm' ? 'vp9' : 'h264') + (audio && !S.mute ? (p.container === 'webm' ? '/opus' : '/aac') : '') + ' · ' + S.quality;
    setText('sum-fmt', `${p.container} · ${codecs}`, false);
    $('#out-info').textContent = `${w}×${h} · ${fmtTime(S.end - S.start)}`;
  }
  function refreshSource() {
    if (!S.loaded) return;
    const p = S.probe;
    setText('src-name', p.name);
    $('#src-name').title = p.path;
    setText('src-size', fmtBytes(p.size));
    setText('src-len', fmtTime(p.duration));
    setText('src-video', `${p.vcodec} ${p.width}×${p.height} @ ${+p.fps.toFixed(3)}` + (p.rotation ? ` · rotated ${p.rotation}°` : ''));
    setText('src-audio', p.acodec || 'none');
  }

  function refreshAll() {
    refreshTimeline();
    refreshCrop();
    refreshOutput();
    refreshSource();
    refreshPlay();
    draw();
  }

  // ------------------------------------------------------------ controls wiring
  $('#crop-aspect').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b) return;
    setCropAspect(b.dataset.aspect);
  });
  $('#view-seg').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b) return;
    setView(b.dataset.view);
  });
  $('#mode-seg').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b) return;
    setMode(b.dataset.mode);
  });
  $('#quality-seg').addEventListener('click', (e) => {
    const b = e.target.closest('button'); if (!b || b.disabled) return;
    S.quality = b.dataset.quality; savePrefs(); refreshOutput();
  });
  $('#mute').addEventListener('change', (e) => setMute(e.target.checked));
  for (const k of ['x', 'y', 'w', 'h']) {
    document.getElementById('crop-' + k).addEventListener('change', (e) => {
      const v = parseInt(e.target.value, 10);
      if (!isNaN(v) && S.loaded) {
        const c = Object.assign({}, S.crop, { [k]: v }), a = ratio();
        if (a && k === 'w') c.h = c.w / a;
        if (a && k === 'h') c.w = c.h * a;
        S.crop = clampCrop(c);
      }
      e.target.blur();
      cropChanged();
    });
  }

  function setMode(m) {
    if (m === 'fast' && isCropped()) { status('cropping needs a re-encode: reset the crop (0) to use fast mode'); return; }
    S.mode = m; savePrefs(); refreshOutput();
  }
  function setMute(m) {
    if (!S.loaded || !S.probe.acodec) return;
    S.mute = m;
    video.muted = m;
    refreshOutput();
  }

  // time inputs
  const timeInput = (id, apply) => {
    const el = document.getElementById(id);
    el.addEventListener('change', () => {
      const v = parseTime(el.value);
      if (!isNaN(v) && S.loaded) apply(v);
      el.blur();
      refreshAll();
    });
    el.addEventListener('keydown', (e) => { if (e.key === 'Enter') el.blur(); });
    el.addEventListener('blur', () => setTimeout(refreshAll, 0));
  };
  timeInput('t-pos', (v) => { stopPlay(); seek(v); });
  timeInput('t-start', (v) => { setStart(v); seek(S.start); });
  timeInput('t-end', (v) => { setEnd(v); seek(S.end); });
  timeInput('t-dur', (v) => setEnd(S.start + v));

  // ------------------------------------------------------------ quit protection
  // "Unsaved" = a video is loaded but nothing has been exported since.
  const isDirty = () => S.loaded && !S.saved;
  function syncDirty() {
    invoke('set_dirty', { dirty: isDirty() }).catch(() => {});
  }
  function markSaved() { S.saved = true; syncDirty(); }
  function askQuit() {
    // called when the window's close button is pressed with unsaved work
    $('#modal-help').classList.add('hidden');
    $('#modal-quit').classList.remove('hidden');
  }
  TAURI.event.listen('ask-quit', () => askQuit());

  // buttons with data-action
  const ACTIONS = {
    'open-video': pickVideo,
    help: () => $('#modal-help').classList.toggle('hidden'),
    play: togglePlay,
    'frame-back': () => step(-1),
    'frame-fwd': () => step(1),
    'set-start': () => setStart(S.pos),
    'set-end': () => setEnd(S.pos),
    'trim-reset': () => { if (S.loaded) { setSpan(0, S.dur); refreshAll(); } },
    'crop-reset': () => { if (S.loaded) { S.crop = { x: 0, y: 0, w: S.vw, h: S.vh }; S.aspect = 'free'; S.view = 'edit'; layout(); refreshAll(); } },
    'crop-center': () => { if (S.loaded) { const c = S.crop; S.crop = clampCrop({ x: (S.vw - c.w) / 2, y: (S.vh - c.h) / 2, w: c.w, h: c.h }); cropChanged(); } },
    export: exportVideo,
    'export-cancel': closeExport,
    'export-close': closeExport,
    'quit-yes': () => { $('#modal-quit').classList.add('hidden'); invoke('quit').catch(() => {}); },
    'quit-no': () => $('#modal-quit').classList.add('hidden'),
    reveal: () => { if (S.lastOut) invoke('reveal', { path: S.lastOut }).catch(() => {}); },
  };
  document.addEventListener('click', (e) => {
    const b = e.target.closest('[data-action]');
    if (b && ACTIONS[b.dataset.action]) { ACTIONS[b.dataset.action](); b.blur(); }
  });
  $('#empty').addEventListener('click', pickVideo);

  // drag & drop: the window hands over real paths
  TAURI.event.listen('tauri://drag-enter', () => document.body.classList.add('dropping'));
  TAURI.event.listen('tauri://drag-leave', () => document.body.classList.remove('dropping'));
  TAURI.event.listen('tauri://drag-drop', (e) => {
    document.body.classList.remove('dropping');
    const paths = (e.payload && e.payload.paths) || [];
    if (paths.length) loadPath(paths[0]);
  });

  // ------------------------------------------------------------ keyboard
  document.addEventListener('keydown', (e) => {
    const typing = e.target.matches('input:not([type=checkbox]):not([type=range]), textarea, select');
    if (e.key === 'Escape') {
      if (typing) { e.target.blur(); return; }
      if (!$('#modal-quit').classList.contains('hidden')) return $('#modal-quit').classList.add('hidden');
      if (!$('#modal-export').classList.contains('hidden')) return closeExport();
      if (!$('#modal-help').classList.contains('hidden')) return $('#modal-help').classList.add('hidden');
      if (S.view === 'result') return setView('edit');
      return;
    }
    if (typing) return;
    if (!$('#modal-quit').classList.contains('hidden')) {
      if (e.key === 'q' || e.key === 'y' || e.key === 'Enter') ACTIONS['quit-yes']();
      else if (e.key === 'n') ACTIONS['quit-no']();
      e.preventDefault();
      return;
    }
    if (!$('#modal-export').classList.contains('hidden')) {
      if (e.key === 'Enter' && !$('#exp-result').classList.contains('hidden')) ACTIONS.reveal();
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey) {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'o') { e.preventDefault(); pickVideo(); }
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') { e.preventDefault(); exportVideo(); }
      return;
    }
    const k = e.key;
    const handled = (() => {
      if (/^[1-8]$/.test(k)) { setCropAspect(ASPECTS[+k - 1]); return true; }
      switch (k) {
        case ' ': togglePlay(); return true;
        case '<': case ',': step(-1); return true;
        case '>': case '.': step(1); return true;
        case 'j': stopPlay(); seek(S.pos - 1); return true;
        case 'l': stopPlay(); seek(S.pos + 1); return true;
        case 'i': case '[': setStart(S.pos); return true;
        case 'o': case ']': setEnd(S.pos); return true;
        case '{': nudgeSpan(-1); return true;
        case '}': nudgeSpan(1); return true;
        case 'Home': stopPlay(); seek(S.start); return true;
        case 'End': stopPlay(); seek(S.end); return true;
        case 'c': setView(S.view === 'edit' ? 'result' : 'edit'); return true;
        case '0': ACTIONS['crop-reset'](); return true;
        case 'x': ACTIONS['crop-center'](); return true;
        case '+': case '=': if (S.loaded) scaleCrop(1.05); return true;
        case '-': case '_': if (S.loaded) scaleCrop(1 / 1.05); return true;
        case 'f': setMode(S.mode === 'fast' ? 'exact' : 'fast'); return true;
        case 'm': setMute(!S.mute); return true;
        case 'v': pickVideo(); return true;
        case 'e': exportVideo(); return true;
        case '?': ACTIONS.help(); return true;
        case 'ArrowLeft': case 'ArrowRight': case 'ArrowUp': case 'ArrowDown': {
          const dir = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[k];
          if (isCropped()) {
            const m = e.shiftKey ? 10 : 1, c = S.crop;
            S.crop = clampCrop({ x: c.x + dir[0] * m, y: c.y + dir[1] * m, w: c.w, h: c.h });
            cropChanged();
          } else if (dir[0]) {
            if (e.shiftKey) { stopPlay(); seek(S.pos + dir[0]); } else step(dir[0]);
          } else return false;
          return true;
        }
        default: return false;
      }
    })();
    if (handled) e.preventDefault();
  });

  // ------------------------------------------------------------ boot
  window.addEventListener('resize', () => { layout(); refreshAll(); });
  video.addEventListener('ended', () => { if (S.playing) { video.currentTime = S.start; video.play().catch(() => {}); } });

  loadPrefs();
  canvas.classList.add('hidden');
  layout();
  refreshAll();
  (async () => {
    S.ffmpeg = (await invoke('app_info')).ffmpeg;
    $('#no-ffmpeg').classList.toggle('hidden', S.ffmpeg);
    // video passed on the command line / "Open with kroppi"
    const initial = await invoke('initial_video');
    if (initial) loadPath(initial);
  })().catch((e) => status('startup failed: ' + errText(e)));

  // test/automation hook
  window.kroppi = { S, askQuit, loadPath, exportVideo, seek, setStart, setEnd, setCropAspect, setMode, setMute, setView, refreshAll, closeExport };
})();
