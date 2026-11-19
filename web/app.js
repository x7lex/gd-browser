// gd-browser: the engine (web/pkg/gdrust.js, Rust -> wasm) driven from
// the page. One GdWeb on the main thread plays levels; a Web Worker owns
// another one for solving. Game art (GD's Resources) arrives as bytes via
// add_asset: fetched from dist/assets when the packer found an install,
// else picked from the player's own folder. Nothing of RobTop's is bundled.

import init, { GdWeb } from './pkg/gdrust.js';

const $ = (id) => document.getElementById(id);
const canvas = $('c'), ctx = canvas.getContext('2d', { alpha: false });
const overlay = $('overlay'), ovTitle = $('ov-title'), ovSub = $('ov-sub'), ovBar = $('bar').firstElementChild;
const toastEl = $('toast');

const W = 1280, H = 720;

const state = {
  wasm: null,       // wasm exports (memory lives here)
  g: null,          // GdWeb
  objectsText: '',   // data/objects.json (also fed to the solve worker)
  levels: [],       // levels.json
  wanted: [],       // asset names the renderer may read
  current: null,    // the loaded level's entry
  cache: {},        // id -> {gmd, meta, macroBytes, macroJson} for reloads
  held: false,
  lastT: 0,
  fps: 60,
  raf: 0,
  worker: null,
  solving: false,
};

const audio = {
  ctx: null, song: null, sfx: null,
  playing: null, cueKey: null, startPos: 0, startedAt: 0,
};

// ---------- loading UI ----------

function overlayShow(title, sub = '') {
  overlay.classList.remove('hidden');
  $('ov-spin').style.display = '';
  ovTitle.textContent = title;
  ovSub.textContent = sub;
}

function overlayProgress(frac) {
  ovBar.style.width = `${Math.min(100, frac * 100).toFixed(1)}%`;
}

function overlayHide() {
  overlay.classList.add('hidden');
}

let toastTimer = 0;
function toast(msg, ms = 3200) {
  toastEl.textContent = msg;
  toastEl.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toastEl.classList.remove('show'), ms);
}

// fetch with a byte progress callback (content-length, when the server sends one)
async function fetchProgress(url, onFrac, as = 'arrayBuffer') {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  const total = +res.headers.get('content-length') || 0;
  if (!res.body || !total || onFrac == null) {
    const data = await res[as]();
    return data;
  }
  const reader = res.body.getReader();
  const parts = [];
  let got = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    parts.push(value);
    got += value.length;
    onFrac(got / total, got, total);
  }
  const blob = new Blob(parts);
  return as === 'arrayBuffer' ? blob.arrayBuffer() : blob.text();
}

// ---------- boot ----------

async function boot() {
  overlayShow('loading engine…', 'compiling wasm');
  ensureAudioCtx(); // starts suspended until the first gesture; decodeAudioData works anyway
  state.wasm = await init();
  overlayShow('loading engine…', 'object table');
  state.objectsText = await (await fetch('data/objects.json')).text();
  state.g = new GdWeb(state.objectsText);
  state.g.set_seed(Date.now());

  overlayShow('loading engine…', 'level index');
  const index = await (await fetch('levels.json')).json();
  state.levels = index.levels;
  state.wanted = index.wanted ?? [];

  await loadArt(index.assets ?? []);
  await loadSfx();

  buildLevelList();
  const want = location.hash.replace('#', '');
  const first = state.levels.find((l) => String(l.id) === want) ?? state.levels[0];
  updateArtNote();
  if (first) {
    await loadLevel(first);
  } else {
    overlayHide();
    toast('no levels packed: run tools/web_pack.py with levels/ present');
  }
}

// game art: dist/assets when the packer found a GD install
async function loadArt(files) {
  if (!files.length) return;
  const total = files.reduce((s, f) => s + f.size, 0);
  let got = 0;
  overlayShow('loading game art…', `${files.length} files`);
  for (const f of files) {
    const bytes = await fetchProgress(f.file, (frac, g) => {
      overlayProgress((got + g) / total);
      ovSub.textContent = `${f.name} (${((got + g) / 1e6).toFixed(1)} / ${(total / 1e6).toFixed(0)} MB)`;
    });
    state.g.add_asset(f.name, new Uint8Array(bytes));
    got += f.size;
  }
}

// the player's own GD folder: match files by name against the wanted list
function pickArt(fileList) {
  const byBasename = new Map();
  for (const f of fileList) {
    const rel = f.webkitRelativePath || f.name;
    const parts = rel.split('/');
    const base = parts[parts.length - 1];
    byBasename.set(base, f);
    if (parts.length >= 2 && parts[parts.length - 2] === 'icons') {
      byBasename.set(`icons/${base}`, f);
    }
  }
  let added = 0, bytes = 0;
  const readers = [];
  for (const name of state.wanted) {
    const f = byBasename.get(name);
    if (!f) continue;
    added++;
    bytes += f.size;
    readers.push(f.arrayBuffer().then((b) => state.g.add_asset(name, new Uint8Array(b))));
  }
  ovSub.textContent = `reading ${added} files…`;
  Promise.all(readers).then(async () => {
    updateArtNote();
    await loadSfx();
    if (state.current) {
      // the renderer snapshots the store per level: load it again
      loadLevel(state.current, `added ${added} art files (${(bytes / 1e6).toFixed(0)} MB)`);
    } else {
      toast(`added ${added} art files`);
    }
  });
}

function updateArtNote() {
  const n = state.g.asset_count();
  $('art-note').innerHTML = n
    ? `<b>${n} art files loaded</b> — GD's own look.`
    : `No game art packed: levels render as <b>hitbox outlines</b>. Load your Geometry Dash folder (its <code>Resources</code>) to see the game's real look — nothing is uploaded anywhere.`;
}

// ---------- levels ----------

function buildLevelList() {
  const list = $('levels');
  list.innerHTML = '';
  for (const l of state.levels) {
    const b = document.createElement('button');
    b.innerHTML = `<span>${l.name}</span><small>${l.macro ? 'macro' : 'solve'}</small>`;
    b.title = `level ${l.id}`;
    b.addEventListener('click', () => loadLevel(l));
    b.dataset.id = l.id;
    list.appendChild(b);
  }
}

function markActive(id) {
  for (const b of $('levels').children) b.classList.toggle('active', b.dataset.id === String(id));
}

async function loadLevel(entry, doneMsg = null) {
  state.current = entry;
  markActive(entry.id);
  overlayShow(`loading ${entry.name}…`, 'fetching level');
  try {
    let c = (state.cache[entry.id] ??= {});
    if (!c.gmd) c.gmd = await fetchProgress(entry.gmd, (f) => overlayProgress(f * 0.7), 'text');
    overlayProgress(0.75);
    if (c.meta === undefined) c.meta = await fetch(entry.meta).then((r) => (r.ok ? r.text() : null)).catch(() => null);
    if (c.macroBytes === undefined && c.macroJson === undefined) {
      if (entry.macro?.type === 'gdr2') {
        c.macroBytes = new Uint8Array(await fetchProgress(entry.macro.file, null));
      } else if (entry.macro?.type === 'json') {
        c.macroJson = await fetch(entry.macro.file).then((r) => r.text());
      }
    }
    overlayProgress(0.85);
    ovSub.textContent = 'building the world (first frame)';
    state.g.load_level(entry.id, c.gmd, c.meta ?? null, c.macroBytes ?? null, c.macroJson ?? null);
    overlayProgress(1);

    // the song, decoded while the first frame renders
    if (entry.song) {
      loadSong(entry.song);
    } else {
      audio.song = null;
    }
  } catch (e) {
    console.error(e);
    toast(`could not load ${entry.name}: ${e.message}`);
    overlayHide();
    return;
  }
  $('hud-name').textContent = entry.name;
  $('hud-miss').textContent = `${(100 * state.g.miss_chance()).toFixed(0)}%`;
  modeChanged();
  overlayHide();
  state.lastT = performance.now();
  if (!state.raf) state.raf = requestAnimationFrame(frame);
  if (doneMsg) toast(doneMsg);
  if (!state.g.has_macro()) maybeSolve();
}

// ---------- audio ----------

function ensureAudioCtx() {
  if (!audio.ctx) audio.ctx = new (window.AudioContext || window.webkitAudioContext)();
  if (audio.ctx.state === 'suspended') audio.ctx.resume();
}

async function loadSong(path) {
  try {
    const buf = await fetchProgress(path, null);
    audio.song = await audio.ctx.decodeAudioData(buf);
  } catch (e) {
    audio.song = null;
    console.warn('song', e);
  }
}

async function loadSfx() {
  if (audio.sfx || audio.sfx === false || !state.g?.has_asset('explode_11.ogg')) return;
  try {
    const buf = await fetchProgress('assets/explode_11.ogg', null);
    audio.sfx = await audio.ctx.decodeAudioData(buf);
  } catch (e) {
    audio.sfx = false; // absent or undecodable: deaths are silent
  }
}

function playBuf(buf) {
  if (!buf || !audio.ctx || audio.ctx.state !== 'running') return;
  const src = audio.ctx.createBufferSource();
  src.buffer = buf;
  src.connect(audio.ctx.destination);
  src.start(0);
}

function stopMusic() {
  if (audio.playing) {
    try { audio.playing.stop(); } catch {}
    audio.playing = null;
  }
  audio.cueKey = null;
}

// the demo's Music.sync: (attempt, song) is the cue; a drift over 0.25 s
// or a new cue restarts the song at the exact sample
function syncMusic() {
  if (!audio.song) return;
  const [songId, pos, attempt] = state.g.song_cue();
  const key = `${attempt}:${songId}`;
  if (audio.playing && key === audio.cueKey) {
    const at = audio.startPos + (audio.ctx.currentTime - audio.startedAt);
    if (Math.abs(at - pos) < 0.25) return;
  }
  stopMusic();
  if (state.g.state() !== 0 || state.g.complete()) return;
  if (!audio.ctx || audio.ctx.state !== 'running') return;
  const src = audio.ctx.createBufferSource();
  src.buffer = audio.song;
  src.connect(audio.ctx.destination);
  const at = Math.max(0, pos);
  src.start(0, at);
  audio.playing = src;
  audio.cueKey = key;
  audio.startPos = at;
  audio.startedAt = audio.ctx.currentTime;
}

// ---------- the frame loop ----------

function frame(t) {
  state.raf = requestAnimationFrame(frame);
  if (!state.g || !state.g.level_loaded()) return;
  const dt = state.lastT ? Math.min((t - state.lastT) / 1000, 0.25) : 0.016;
  state.lastT = t;

  state.g.step(dt, state.held);
  state.g.render();

  const len = state.g.pixels_len();
  if (len === W * H * 4) {
    const px = new Uint8ClampedArray(state.wasm.memory.buffer, state.g.pixels_ptr(), len);
    ctx.putImageData(new ImageData(px, W, H), 0, 0);
  }

  if (dt > 0) state.fps = state.fps * 0.9 + (1 / dt) * 0.1;
  state.g.set_fps(state.fps | 0);

  $('hud-attempt').textContent = state.g.attempt();
  $('hud-progress').textContent = state.g.progress().toFixed(2);

  const deaths = state.g.take_deaths();
  if (deaths > 0) playBuf(audio.sfx);

  syncMusic();

  if (state.g.complete()) {
    stopMusic();
    if (!frame.completeShown) {
      frame.completeShown = true;
      toast(`Level complete! (${state.g.attempt()} attempts)`, 4000);
    }
  } else {
    frame.completeShown = false;
  }
}

// ---------- input ----------

function setHeld(v) {
  state.held = v;
}

function toggleMode() {
  if (!state.g || !state.g.level_loaded()) return;
  state.g.set_manual(!state.g.manual());
  modeChanged();
  maybeSolve();
}

function modeChanged() {
  const m = state.g?.manual();
  $('mode-badge').textContent = m ? 'MANUAL' : 'AUTO';
  $('btn-mode').textContent = m ? '→ Auto' : '→ Manual';
}

function restart() {
  if (state.g?.level_loaded()) state.g.restart();
}

addEventListener('keydown', (e) => {
  if (e.repeat) return;
  ensureAudioCtx();
  if (e.code === 'Space' || e.code === 'ArrowUp' || e.code === 'KeyW') {
    setHeld(true);
    e.preventDefault();
  } else if (e.code === 'KeyR') {
    restart();
  } else if (e.code === 'KeyT') {
    toggleMode();
  }
});

addEventListener('keyup', (e) => {
  if (e.code === 'Space' || e.code === 'ArrowUp' || e.code === 'KeyW') setHeld(false);
});

const hold = (v) => (e) => {
  ensureAudioCtx();
  setHeld(v);
  e.preventDefault();
};
canvas.addEventListener('pointerdown', hold(true));
addEventListener('pointerup', hold(false));
addEventListener('pointercancel', hold(false));
canvas.addEventListener('contextmenu', (e) => e.preventDefault());

$('btn-mode').addEventListener('click', toggleMode);
$('btn-restart').addEventListener('click', restart);
$('btn-solve').addEventListener('click', () => maybeSolve(true));
$('art-picker').addEventListener('change', (e) => pickArt(e.target.files));

// ---------- the solver worker ----------

function maybeSolve(force = false) {
  if (!state.current || state.solving) return;
  if (!force && state.g.has_macro()) return; // a recorded macro already plays
  startSolve();
}

function startSolve() {
  const entry = state.current;
  const c = state.cache[entry.id];
  if (!c?.gmd) return;
  if (!state.worker) {
    state.worker = new Worker('./worker.js', { type: 'module' });
    state.worker.onmessage = (e) => {
      if (e.data.cmd === 'progress') {
        $('solve-state').textContent = `solving… ${e.data.p.toFixed(0)}%`;
        ovSub.textContent = `solving ${state.current?.name} — ${e.data.p.toFixed(0)}%`;
      } else if (e.data.cmd === 'solved') {
        state.solving = false;
        $('solve-state').textContent = '';
        const r = e.data.res;
        if (r.macro) {
          state.g.set_macro(JSON.stringify(r.macro), r.score);
          modeChanged();
          toast(r.ok ? 'solved: playing the engine run' : `solver reached ${r.percent.toFixed(1)}% — playing the partial run`);
        } else {
          toast(`could not solve ${state.current?.name}`);
        }
      }
    };
    state.worker.onerror = (e) => {
      state.solving = false;
      $('solve-state').textContent = '';
      toast(`solver worker: ${e.message}`);
    };
  }
  state.solving = true;
  $('solve-state').textContent = 'solving… 0%';
  state.worker.postMessage({ cmd: 'solve', objects: state.objectsText, id: entry.id, gmd: c.gmd });
}

// ---------- go ----------

boot().catch((e) => {
  overlayShow('failed to start', String(e.message ?? e));
  console.error(e);
});
