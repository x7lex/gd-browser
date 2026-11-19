//! The browser build: the same engine (sim + renderer + solver) behind a
//! small wasm-bindgen API, driven by web/app.js.
//!
//! One `GdWeb` holds the object table, the game art (bytes by name,
//! fetched or picked — see gdrust::assets::Assets) and the loaded level.
//! The frame loop: JS calls `step(dt, held)` (240 Hz sim ticks by wall
//! clock, deaths restart, exactly like the native demo), then `render()`
//! for the RGBA frame (putImageData) and the getters for HUD/music.
//! The solver runs in a Web Worker on its own `GdWeb` (wasm has no
//! spawnable threads; it solves sequentially with a progress callback).

use crate::assets::Assets;
use crate::bot::auto;
use crate::bot::gdr2::{self, Macro};
use crate::bot::Playback;
use crate::level::{parse_level, Level};
use crate::render::{Renderer, FRAME_H, FRAME_W};
use crate::sim::objects::ObjectTable;
use crate::sim::{InputState, RunState, Sim};
use wasm_bindgen::prelude::*;

/// one 240 Hz sim tick
const TICK_DT: f64 = 1.0 / 240.0;
/// how long the level-complete banner stays up before the next attempt
const COMPLETE_HOLD: f64 = 4.0;

/// A loaded level, ready to play (auto or manual).
struct Game {
    level: Level,
    name: String,
    /// the level's own song, a path like "audio/482872.mp3" (JS fetches it)
    song: Option<String>,
    /// the macro being replayed (empty: manual / solve pending)
    m: Macro,
    /// difficulty score, for attempt() and the miss-chance readout
    score: f64,
    sim: Sim,
    renderer: Renderer,
    pb: Playback,
    cur: Macro,
    tick: usize,
    /// manual: the player's clicks drive the sim; auto: the macro does
    manual: bool,
    /// replay the macro's inputs only, without its recorded positions
    pure: bool,
    /// 240 Hz accumulator, fed with rAF dt (capped like the demo)
    acc: f64,
    /// deaths since the last take_deaths() (the death sfx cue)
    deaths: u32,
    /// level-complete banner age; None while playing
    complete: Option<f64>,
    /// the frame buffer returned to JS (reused every render())
    buf: Vec<u8>,
}

impl Game {
    fn new_attempt(&mut self, seed: u64) {
        self.sim.reset();
        self.cur = auto::attempt(&self.m, self.score, seed);
        self.pb = Playback::new(self.cur.clone());
        self.tick = 0;
        self.complete = None;
    }

    /// The demo's tick loop: one 240 Hz step of auto or manual play.
    fn tick(&mut self, held: bool, seed: u64) {
        if let Some(age) = self.complete.as_mut() {
            // let the banner play out, then loop the level like the demo
            if *age >= COMPLETE_HOLD {
                self.new_attempt(seed);
            }
            return;
        }
        if self.sim.state == RunState::Dead || (self.sim.progress() >= 100.0 && self.tick > 0 && self.sim.state == RunState::Complete) {
            self.deaths += 1;
            self.new_attempt(seed);
        }
        if self.manual {
            self.tick += 1;
            self.sim.follow = false;
            self.sim.step_single(InputState { hold: held, hold_p2: held });
            return;
        }
        let (p1, p2) = self.pb.step();
        self.tick += 1;
        if self.pb.stalled() {
            return; // the real game ran no physics this frame
        }
        // deaths are off only while GD's recorded path still pins the
        // player: past a missed click (attempt() cuts the frames there)
        // the run is our physics alone and must be able to die
        self.sim.follow = !self.pure && self.tick < self.cur.frames.len();
        self.sim.step_single(InputState { hold: p2, hold_p2: p1 });
        if !self.pure {
            if let Some(f) = self.cur.frames.get(self.tick) {
                self.sim.apply_fix(f[0] as f64, f[1] as f64, f[2] as f64);
            }
            if let Some(f) = self.cur.frames_p2.get(self.tick) {
                self.sim.apply_fix_p2(f[0] as f64, f[1] as f64, f[2] as f64);
            }
        }
    }
}

/// The engine behind the browser build. Main thread: play. Web Worker:
/// construct with the object table and call `solve` (the solver shares
/// nothing with a running game, so both can live side by side).
#[wasm_bindgen]
pub struct GdWeb {
    assets: Assets,
    table: ObjectTable,
    game: Option<Game>,
    /// sway variety per attempt (set from JS: Date.now())
    seed: u64,
    /// fps counter for the in-frame HUD (set by JS each rAF)
    fps: u32,
}

#[wasm_bindgen]
impl GdWeb {
    /// `objects_json` is data/objects.json's text.
    #[wasm_bindgen(constructor)]
    pub fn new(objects_json: &str) -> Result<GdWeb, JsError> {
        let table = ObjectTable::from_str(objects_json).map_err(|e| JsError::new(&e))?;
        Ok(GdWeb { assets: Assets::map(), table, game: None, seed: 1, fps: 0 })
    }

    /// Add one game-art file by name (see gdrust::assets::wanted_names).
    /// Add them all before `load_level`; the renderer snapshots the store.
    pub fn add_asset(&mut self, name: String, bytes: &[u8]) {
        self.assets.insert(&name, bytes);
    }

    /// How many art files are loaded.
    pub fn asset_count(&self) -> usize {
        self.assets.len()
    }

    pub fn has_asset(&self, name: &str) -> bool {
        self.assets.exists(name)
    }

    /// Variety seed for the auto mode's sway (JS: Date.now()).
    pub fn set_seed(&mut self, seed: f64) {
        self.seed = seed as u64;
    }

    /// Load a level. `gmd` is the decoded level string; `meta_json` is
    /// levels/<id>.json ({"name", "song"}); the macro is a recorded .gdr2
    /// (bytes) or an engine solve cache (json string), whichever exists.
    /// The first frame is rendered here, so its cost lands under the
    /// loading overlay instead of the first animation frame.
    pub fn load_level(
        &mut self,
        id: u32,
        gmd: &str,
        meta_json: Option<String>,
        macro_gdr2: Option<Vec<u8>>,
        macro_json: Option<String>,
    ) -> Result<(), JsError> {
        let mut level = parse_level(gmd);
        level.id = id;
        let meta: serde_json::Value = match meta_json {
            Some(s) => serde_json::from_str(&s).map_err(|e| JsError::new(&format!("level {id} meta: {e}")))?,
            None => Default::default(),
        };
        let name = meta["name"].as_str().unwrap_or("level").to_string();
        let song = meta["song"].as_str().map(str::to_string);
        let m = if let Some(b) = macro_gdr2 {
            gdr2::parse_gdr2(&b).map_err(|e| JsError::new(&e))?
        } else if let Some(s) = macro_json {
            serde_json::from_str(&s).map_err(|e| JsError::new(&format!("macro json: {e}")))?
        } else {
            Macro::default()
        };
        let score = auto::difficulty(&level, &self.table).0;
        let mut sim = Sim::new(&level, &self.table);
        let pure = m.frames.is_empty();
        // no inputs at all: the player drives until a solve lands; any macro
        // (recorded or an engine solve) auto-plays
        let manual = m.inputs.is_empty();
        sim.follow = !pure;
        let renderer = Renderer::new(&self.assets);
        let cur = auto::attempt(&m, score, self.seed);
        let mut game = Game {
            level,
            name,
            song,
            m,
            score,
            sim,
            renderer,
            pb: Playback::new(cur.clone()),
            cur,
            tick: 0,
            manual,
            pure,
            acc: 0.0,
            deaths: 0,
            complete: None,
            buf: Vec::with_capacity(FRAME_W as usize * FRAME_H as usize * 4),
        };
        // warm-up: composites, prescales and the first frame's caches
        game.renderer.draw_into(&game.level, &self.table, &game.sim, &mut game.buf);
        self.game = Some(game);
        Ok(())
    }

    /// Replace the macro (an engine solve from the worker: auto's JSON).
    /// Restarts the attempt in auto mode.
    pub fn set_macro(&mut self, macro_json: &str, score: f64) -> Result<(), JsError> {
        let m: Macro = serde_json::from_str(macro_json).map_err(|e| JsError::new(&format!("macro json: {e}")))?;
        let Some(g) = self.game.as_mut() else { return Ok(()) };
        g.m = m;
        g.score = score;
        g.pure = g.m.frames.is_empty();
        g.manual = false; // a macro is here now: auto plays it
        g.new_attempt(self.seed);
        Ok(())
    }

    /// Manual (true: the player drives) or auto (the macro).
    pub fn set_manual(&mut self, manual: bool) {
        if let Some(g) = self.game.as_mut() {
            g.manual = manual;
            g.new_attempt(self.seed);
        }
    }

    pub fn manual(&self) -> bool {
        self.game.as_ref().is_some_and(|g| g.manual)
    }

    pub fn restart(&mut self) {
        if let Some(g) = self.game.as_mut() {
            g.new_attempt(self.seed);
        }
    }

    /// Advance the sim by `dt` seconds of wall clock (rAF deltas; capped
    /// at 0.25 s like the demo, so a hitch cannot snowball) with `held`
    /// (manual mode's click state). Deaths restart on their own.
    pub fn step(&mut self, dt: f64, held: bool) {
        let Some(g) = self.game.as_mut() else { return };
        if let Some(age) = g.complete.as_mut() {
            *age += dt.clamp(0.0, 0.25);
        }
        g.acc += dt.clamp(0.0, 0.25);
        while g.acc >= TICK_DT {
            g.acc -= TICK_DT;
            let seed = self.seed.wrapping_add(g.sim.attempt as u64);
            g.tick(held, seed);
            if g.complete.is_none() && (g.sim.progress() >= 100.0 || g.sim.state == RunState::Complete) && g.tick > 0 {
                g.complete = Some(0.0);
                break;
            }
        }
    }

    /// Render the frame into the reusable buffer (opaque RGBA,
    /// FRAME_W * FRAME_H * 4 bytes).
    pub fn render(&mut self) {
        if let Some(g) = self.game.as_mut() {
            g.renderer.set_fps(self.fps);
            g.renderer.draw_into(&g.level, &self.table, &g.sim, &mut g.buf);
        }
    }

    /// The frame `render()` drew: a pointer into wasm memory, valid only
    /// until the next wasm call. JS wraps it:
    /// `new Uint8ClampedArray(memory.buffer, ptr, len)` and putImageData.
    pub fn pixels_ptr(&self) -> *const u8 {
        self.game.as_ref().map_or(std::ptr::null(), |g| g.buf.as_ptr())
    }

    /// The frame's byte length (FRAME_W * FRAME_H * 4 once a level loaded).
    pub fn pixels_len(&self) -> usize {
        self.game.as_ref().map_or(0, |g| g.buf.len())
    }

    /// The frame size (w, h), for the canvas.
    pub fn frame_size(&self) -> Vec<u32> {
        vec![FRAME_W, FRAME_H]
    }

    /// Show an fps counter in the frame's HUD (JS updates this per rAF).
    pub fn set_fps(&mut self, fps: u32) {
        self.fps = fps;
    }

    /// HUD/music getters.
    pub fn progress(&self) -> f64 {
        self.game.as_ref().map_or(0.0, |g| g.sim.progress())
    }

    pub fn attempt(&self) -> u32 {
        self.game.as_ref().map_or(0, |g| g.sim.attempt)
    }

    /// 0 playing, 1 dead, 2 complete.
    pub fn state(&self) -> u8 {
        self.game.as_ref().map_or(0, |g| match g.sim.state {
            RunState::Playing => 0,
            RunState::Dead => 1,
            RunState::Complete => 2,
        })
    }

    pub fn level_name(&self) -> String {
        self.game.as_ref().map_or(String::new(), |g| g.name.clone())
    }

    /// The level's own song, a path like "audio/482872.mp3".
    pub fn song_file(&self) -> Option<String> {
        self.game.as_ref().and_then(|g| g.song.clone())
    }

    /// What should be playing: the song id (0 = the level's) and seconds
    /// into it. JS restarts the audio at this cue when it changes.
    pub fn song_cue(&self) -> Vec<f64> {
        let Some(g) = self.game.as_ref() else { return vec![0.0, 0.0, 0.0] };
        let (id, pos) = g.sim.song_pos();
        vec![id as f64, pos, g.sim.attempt as f64]
    }

    pub fn complete(&self) -> bool {
        self.game.as_ref().is_some_and(|g| g.complete.is_some())
    }

    /// Deaths since the last call (the death-sound cue).
    pub fn take_deaths(&mut self) -> u32 {
        self.game.as_mut().map_or(0, |g| std::mem::take(&mut g.deaths))
    }

    pub fn has_macro(&self) -> bool {
        self.game.as_ref().is_some_and(|g| !g.m.inputs.is_empty())
    }

    /// The difficulty score of the loaded level (for the miss-chance readout).
    pub fn miss_chance(&self) -> f64 {
        self.game.as_ref().map_or(0.0, |g| auto::fail_chance(g.score))
    }

    pub fn level_loaded(&self) -> bool {
        self.game.is_some()
    }

    /// Debug: replay `macro_json` on a fresh sim inside wasm, exactly like
    /// bot::auto::replays does, and return the death tick (0 = completed).
    pub fn dbg_replay(&mut self, id: u32, gmd: &str, macro_json: &str, seed: f64) -> u32 {
        let mut level = parse_level(gmd);
        level.id = id;
        let Ok(m): std::result::Result<Macro, _> = serde_json::from_str(macro_json) else { return u32::MAX };
        let score = auto::difficulty(&level, &self.table).0;
        let mut sim = Sim::new(&level, &self.table);
        let _ = sim.reset();
        let mut pb = Playback::new(auto::attempt(&m, score, seed as u64));
        let mut tick = 0u32;
        for _ in 0..240 * 60 {
            let (p1, p2) = pb.step();
            tick += 1;
            if pb.stalled() {
                continue;
            }
            sim.step_single(InputState { hold: p2, hold_p2: p1 });
            if sim.state == RunState::Dead {
                return tick;
            }
            if sim.progress() >= 100.0 || sim.state == RunState::Complete {
                return 0;
            }
        }
        u32::MAX - 1
    }

    /// Solve a level (in a Web Worker: this blocks for a long time).
    /// `on_progress(percent)` is called as the search advances. Returns
    /// JSON: {ok, percent, score, macro} — macro null when the solver
    /// gave up (percent = how far the partial run got; it still plays).
    pub fn solve(&mut self, id: u32, gmd: &str, on_progress: &js_sys::Function) -> Result<String, JsError> {
        let mut level = parse_level(gmd);
        level.id = id;
        let score = auto::difficulty(&level, &self.table).0;
        let progress = |p: f64| {
            let _ = on_progress.call1(&JsValue::NULL, &JsValue::from_f64(p));
        };
        let solved = auto::solve(&level, &self.table, progress);
        let (ok, m, percent) = match solved {
            Ok(m) => (true, m, 100.0),
            Err((m, p)) => (false, m, p),
        };
        // verified + pruned + swayed like macro_for; a solve that does not
        // replay on a fresh sim is returned as-is (still playable)
        let m = match auto::finish(&level, &self.table, m) {
            Ok(m) => m,
            Err(m) => m,
        };
        let out = serde_json::json!({
            "ok": ok,
            "percent": percent,
            "score": score,
            "macro": if ok || !m.inputs.is_empty() { serde_json::to_value(&m).ok() } else { None },
        });
        serde_json::to_string(&out).map_err(|e| JsError::new(&e.to_string()))
    }
}
