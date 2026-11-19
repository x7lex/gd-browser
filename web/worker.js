// The solver worker: its own GdWeb (own wasm instance) so the page keeps
// playing while a level is solved. wasm has no spawnable threads, so the
// beam search runs sequentially in here; the progress callback (called
// synchronously from wasm) posts updates out.

import init, { GdWeb } from './pkg/gdrust.js';

let g = null;

self.onmessage = async (e) => {
  const { cmd, objects, id, gmd } = e.data;
  if (cmd !== 'solve') return;
  try {
    await init();
    if (!g) g = new GdWeb(objects);
    let last = -1;
    const res = g.solve(id, gmd, (p) => {
      const pc = Math.floor(p);
      if (pc > last) {
        last = pc;
        postMessage({ cmd: 'progress', p });
      }
    });
    postMessage({ cmd: 'solved', res: JSON.parse(res) });
  } catch (err) {
    postMessage({ cmd: 'solved', res: { ok: false, percent: 0, score: 0, macro: null }, error: String(err) });
  }
};
