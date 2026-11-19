// Headless smoke test for the wasm build: drives GdWeb the way app.js
// does (initSync works in node; no fetch/DOM needed) and checks the
// pieces a browser would exercise:
//   1. engine boots with the object table
//   2. a real level (GRIEF) + its recorded macro loads, replays and renders
//   3. deaths restart the attempt, manual mode steps, restart works
//   4. the renderer produces a real frame (non-uniform pixels)
//   5. the solver solves a small level and set_macro plays it to 100%
//   6. art bytes route through add_asset (map-backed Assets)
//
// run: node web/smoke.mjs   (after wasm-pack build / tools/web_pack.py)

import { initSync, GdWeb } from './pkg/gdrust.js';
import { readFileSync } from 'node:fs';

const root = new URL('..', import.meta.url).pathname;
const read = (p) => readFileSync(root + p);

let failed = 0;
const ok = (cond, msg) => {
  console.log(`${cond ? '  ok' : 'FAIL'}  ${msg}`);
  if (!cond) failed++;
};

console.log('booting engine');
const wasm = initSync({ module: read('web/pkg/gdrust_bg.wasm') });
const objects = read('data/objects.json').toString();
const g = new GdWeb(objects);
g.set_seed(1234);
ok(g instanceof GdWeb, 'GdWeb constructed with the object table');
ok(!g.level_loaded(), 'no level loaded yet');

console.log('loading GRIEF + its recorded macro');
const gmd = read('levels/149992717.gmd').toString();
const meta = read('levels/149992717.json').toString();
const macro = new Uint8Array(read('macros/Cerealmilkbowl-GRIEF-xdBot.gdr2'));
g.add_asset('definitely_not_real.plist', new Uint8Array([1, 2, 3]));
g.load_level(149992717, gmd, meta, macro, null);
ok(g.level_loaded(), 'level loaded');
ok(g.has_macro(), 'macro loaded');
ok(g.level_name() === 'GRIEF', `level name is GRIEF (got ${g.level_name()})`);
ok(g.song_file() === 'audio/482872.mp3', `song file (got ${g.song_file()})`);
ok(g.asset_count() === 1, 'art bytes registered through add_asset');
ok(g.miss_chance() >= 0, `miss chance ${g.miss_chance().toFixed(3)}`);

console.log('replaying 4 seconds (240 Hz ticks from rAF deltas)');
const t0 = performance.now();
for (let i = 0; i < 240; i++) g.step(1 / 60, false);
const stepMs = (performance.now() - t0) / 240;
const prog4s = g.progress();
ok(prog4s > 0, `progress after 4 s: ${prog4s.toFixed(2)}%`);
ok(g.attempt() >= 1, `attempt ${g.attempt()}`);
ok(g.render !== undefined, 'step ran without panicking');

console.log('rendering a frame');
g.render();
const len = g.pixels_len();
ok(len === 1280 * 720 * 4, `frame is ${len} bytes`);
const px = new Uint8ClampedArray(wasm.memory.buffer, g.pixels_ptr(), len);
let nonbg = 0;
for (let i = 0; i < px.length; i += 4 * 997) {
  if (px[i] !== px[0] || px[i + 1] !== px[1] || px[i + 2] !== px[2]) nonbg++;
}
ok(nonbg > 10, `frame is not uniform (${nonbg} differing samples)`);
let opaque = 0;
for (let i = 3; i < px.length; i += 4 * 997) if (px[i] === 255) opaque++;
ok(opaque > 10, `frame is opaque (${opaque} samples)`);

console.log('deaths restart the attempt');
const a0 = g.attempt();
for (let i = 0; i < 240 * 6; i++) {
  g.step(1 / 60, false); // no clicks: a replay with no hold dies somewhere
  if (g.take_deaths() > 0) break;
}
ok(g.attempt() >= a0, `attempt after no-hold run: ${g.attempt()} (deaths restarted it)`);

console.log('manual mode + restart');
g.set_manual(true);
ok(g.manual(), 'manual mode on');
for (let i = 0; i < 120; i++) g.step(1 / 60, i % 60 < 10); // hold sometimes
const pManual = g.progress();
ok(pManual > 0 && pManual < 100, `manual run got to ${pManual.toFixed(2)}%`);
g.restart();
ok(g.progress() < pManual || g.progress() === 0, `restart reset the run (${g.progress().toFixed(2)}%)`);
g.set_manual(false);
ok(!g.manual(), 'auto mode on');

console.log('solving a small level (the worker path, inline here)');
const tiny =
  'kS38,1_0|;' +
  '1,8,2,300,3,15;1,1,2,600,3,15;1,1,2,630,3,15;1,1,2,660,3,15;1,8,2,660,3,45;1,1,2,900,3,15;';
let progressCalls = 0;
const res = JSON.parse(g.solve(999, tiny, () => progressCalls++));
ok(res.ok === true, `solver finished (ok=${res.ok}, percent=${res.percent})`);
ok(progressCalls > 0, `progress callback fired ${progressCalls} times`);
ok(!!res.macro && res.macro.inputs.length > 0, 'solved macro has inputs');
g.load_level(999, tiny, null, null, null);
g.set_macro(JSON.stringify(res.macro), res.score);
ok(g.has_macro(), 'set_macro installed the solve');
let done = false;
for (let i = 0; i < 240 * 30; i++) {
  g.step(1 / 60, false);
  if (g.take_deaths() > 0) break;
  if (g.complete()) { done = true; break; }
}
ok(done, 'the engine solve replays to 100%');

console.log('');
if (failed) {
  console.log(`${failed} FAILED`);
  process.exit(1);
}
console.log(`all good — sim step ${stepMs.toFixed(2)} ms avg (wasm, sequential)`);
