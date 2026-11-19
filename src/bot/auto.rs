//! Autoplayer: finds inputs that beat a level in this engine when no macro
//! exists. Beam search over real sim snapshots: every `D` ticks each live
//! candidate tries every action (release/hold; per player in 2-player dual)
//! for `D` ticks, dead ones drop, near-identical states merge, and the best
//! `width` survive (furthest, then fewest input changes so it plays like a
//! person, then spread over height so corridors keep options).
//!
//! A solved run is then pruned: every click the level does not need is
//! dropped, so the bot only jumps when it has to.
//!
//! The result is valid in *this* engine, not necessarily in real GD.
//! Inputs use the xdBot files' wiring (sim player 1 = macro "player 2"), so
//! every `InputState { hold: p2, hold_p2: p1 }` call site plays it as is.

use super::gdr2::{Macro, MacroInput};
use crate::level::Level;
use crate::sim::objects::ObjectTable;
use crate::sim::{InputState, RunState, Sim};
use std::collections::HashMap;

/// ticks per decision (60 decisions per second)
const D: usize = 4;
/// beam widths: the first, then wider retries after the whole beam died
const WIDTHS: [usize; 4] = [64, 256, 1024, 4096];
/// bytes all live sim copies may take (beam, its children, checkpoints):
/// wide retries on big levels otherwise ran machines out of memory
/// (GRIEF's sim is ~200 KB mid-level: 4096 wide x 11 copies = 9 GB).
/// shortcut: fixed, not sized from the machine's free RAM.
const MEM_BUDGET: usize = 1 << 30;
/// checkpoints kept (retry n restarts n+1 back)
const KEEP_CHECKPOINTS: usize = 8;

/// Beam width that keeps every live sim copy within MEM_BUDGET: the beam,
/// its children (one per action) and the checkpoints, each `sim_bytes`.
fn capped_width(want: usize, sim_bytes: usize, acts: usize) -> usize {
    let copies = 1 + acts + KEEP_CHECKPOINTS;
    want.min(MEM_BUDGET / (sim_bytes.max(1) * copies)).max(8)
}

/// retries before giving up; past the widths list they stay at the widest
/// and only reach further back
const RETRIES: usize = 7;
/// how far above the level's highest object a run may go (5 blocks)
const ABOVE_TOP: f64 = 150.0;
/// layers between beam checkpoints (retry n restarts n+1 checkpoints back)
const CHECKPOINT: usize = 40;

/// One search-tree node: the action held for its D ticks, and its parent.
struct Node {
    parent: usize,
    act: (bool, bool),
}

#[derive(Clone)]
struct Cand {
    sim: Sim,
    node: usize,
    changes: u32,
}

/// Solve `level`. Ok = a complete run; Err = the best partial run and how far
/// it got (percent). `progress` gets percent as the search advances.
pub fn solve(level: &Level, table: &ObjectTable, mut progress: impl FnMut(f64)) -> Result<Macro, (Macro, f64)> {
    let start = Sim::new(level, table);
    let duals = start.players.len() > 1 && level.header.two_player;
    let acts: &[(bool, bool)] =
        if duals { &[(false, false), (true, false), (false, true), (true, true)] } else { &[(false, false), (true, false)] };
    let mut nodes = vec![Node { parent: usize::MAX, act: (false, false) }];
    let mut beam = vec![Cand { sim: start, node: 0, changes: 0 }];
    let mut checkpoints: Vec<Vec<Cand>> = Vec::new();
    let mut tries = 0;
    let mut best = (0usize, 0.0f64);
    let mut layer = 0usize;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    // AUTO_TRACE=1: per layer the beam's heights, and every death's killer
    let trace = std::env::var("AUTO_TRACE").is_ok();
    loop {
        if let Some(done) = beam.iter().find(|c| finished(&c.sim)) {
            return Ok(to_macro(level, &nodes, done.node));
        }
        if layer % CHECKPOINT == 0 {
            checkpoints.push(beam.clone());
            if checkpoints.len() > KEEP_CHECKPOINTS {
                checkpoints.remove(0);
            }
        }
        // expand every candidate by every action, D ticks each, in parallel
        let jobs: Vec<(usize, usize)> = (0..beam.len()).flat_map(|c| (0..acts.len()).map(move |a| (c, a))).collect();
        let chunk = jobs.len().div_ceil(threads).max(1);
        let beam_ref = &beam;
        // wasm has no spawnable threads: grow the beam inline
        let grow = |js: &[(usize, usize)]| {
            js.iter()
                .map(|&(c, a)| {
                    let mut sim = beam_ref[c].sim.clone();
                    let act = acts[a];
                    for _ in 0..D {
                        sim.step_single(InputState { hold: act.0, hold_p2: act.1 });
                        if sim.state == RunState::Dead {
                            if trace {
                                let p = &sim.players[0];
                                eprintln!("  dies x {:.0} y {:.0} act {act:?} killer {:?} {:?} flip {} vy {:.1}", p.x, p.y, sim.killer, p.mode, p.grav_flip, p.vel_y);
                            }
                            return None;
                        }
                        if finished(&sim) {
                            break;
                        }
                    }
                    Some(Cand { sim, node: usize::MAX, changes: a as u32 })
                })
                .collect::<Vec<_>>()
        };
        let grown: Vec<Option<Cand>> = if cfg!(target_family = "wasm") {
            jobs.chunks(chunk).flat_map(grow).collect()
        } else {
            std::thread::scope(|sc| {
                let hs: Vec<_> = jobs
                    .chunks(chunk)
                    .map(|js| {
                        let grow = grow;
                        sc.spawn(move || grow(js))
                    })
                    .collect();
                hs.into_iter().flat_map(|h| h.join().expect("solver thread")).collect()
            })
        };
        // attach tree nodes, merge near-identical states (keep fewest changes)
        if trace {
            let ys: Vec<i64> = beam.iter().map(|c| c.sim.players[0].y as i64).collect();
            eprintln!("layer {layer} x {:.0} beam y {ys:?}", beam[0].sim.players[0].x);
        }
        let mut merged: HashMap<Vec<i64>, Cand> = HashMap::new();
        for (&(c, a), g) in jobs.iter().zip(grown) {
            let Some(mut g) = g else { continue };
            let parent = &beam[c];
            let act = acts[a];
            g.changes = parent.changes + (nodes[parent.node].act != act) as u32;
            nodes.push(Node { parent: parent.node, act });
            g.node = nodes.len() - 1;
            let k = key(&g.sim);
            // same state: a grounded ball that already released beats one still
            // holding (only a fresh press flips it; holding through wins on
            // changes alone and loses the flip), then the fewest changes.
            // shortcut: grounded ball and ufo only (both act on a fresh press);
            // cube excluded: held can also be worth
            // more (holding through an orb triggers it), so neither dominates
            use crate::sim::player::GameMode as M;
            let stuck = |p: &crate::sim::player::Player| p.hold && ((p.on_ground && p.mode == M::Ball) || p.mode == M::Ufo);
            let rank = |c: &Cand| (c.sim.players.iter().filter(|p| stuck(p)).count(), c.changes);
            if merged.get(&k).is_none_or(|m| rank(&g) < rank(m)) {
                merged.insert(k, g);
            }
        }
        let sim_bytes = merged.values().next().map_or(0, |c| c.sim.heap_bytes());
        let width = capped_width(WIDTHS[tries.min(WIDTHS.len() - 1)], sim_bytes, acts.len());
        let kept = select(merged.into_values().collect(), width);
        if kept.is_empty() {
            // the whole beam died (or left the route): retry wider from further back each time
            // (the re-run re-adds the later checkpoints, so a fixed "two
            // back" would land on the same one every retry)
            tries += 1;
            if tries >= RETRIES || checkpoints.is_empty() {
                return Err((to_macro(level, &nodes, best.0), best.1));
            }
            let back = checkpoints.len().saturating_sub(1 + tries);
            beam = checkpoints[back].clone();
            checkpoints.truncate(back);
            layer = 0; // re-checkpoint the restored beam next round
            continue;
        }
        beam = kept;
        layer += 1;
        let lead = beam.iter().max_by(|a, b| a.sim.progress().total_cmp(&b.sim.progress())).expect("non-empty beam");
        if lead.sim.progress() > best.1 {
            best = (lead.node, lead.sim.progress());
            progress(best.1);
        }
    }
}

fn finished(sim: &Sim) -> bool {
    sim.state == RunState::Complete || sim.players.first().is_some_and(|p| p.x >= sim.last_obj_x)
}

/// Quantized state: candidates equal here behave the same from now on.
/// Quarter-unit and held-button resolution: coarser keys merged a run that
/// still lived with one that died (Bloodbath 31% UFO, 53% orbs: a held cube
/// takes the next orb, a released one doesn't).
/// shortcut: trigger state (touched/toggled groups) is not in the key; two
/// paths differing only there merge. Add it if a level relies on that.
fn key(sim: &Sim) -> Vec<i64> {
    sim.players
        .iter()
        .flat_map(|p| {
            [(p.x * 4.0) as i64, (p.y * 4.0) as i64, (p.vel_y * 16.0) as i64, p.mode as i64, p.grav_flip as i64, p.mini as i64, p.queued_hold as i64, p.hold as i64,
             // a spent ball flip changes what the next press does
             p.ball_ready as i64]
        })
        .chain([sim.players.len() as i64])
        .collect()
}

/// Keep the beam spread over the state space: the fewest-clicks candidate
/// of each band (height, vertical speed, x), evenly sampled when there are
/// more bands than slots, then fill with the rest by fewest clicks. Not
/// ranked by x (a slow portal on the real route puts it behind) nor by
/// clicks first (riding the ceiling needs none and would crowd out routes).
fn select(mut c: Vec<Cand>, width: usize) -> Vec<Cand> {
    // a run that flew past a mode portal is off the level's route (Bloodbath
    // 38%: a wave over the UFO portal then threads the rest of the level):
    // drop a run already past the x where another run made more mode
    // changes (runs still behind that point may yet take the portal)
    // per mode-change count k: the earliest x where a run made its k-th change
    let mut first: std::collections::BTreeMap<u32, f64> = std::collections::BTreeMap::new();
    for c in &c {
        let (k, x) = c.sim.mode_portals;
        let e = first.entry(k).or_insert(x);
        *e = e.min(x);
    }
    // a run with k changes is off route once past where any run made more
    let past = |k: u32, x: f64| first.range(k + 1..).any(|(_, &d)| x > d + 90.0);
    // and a run far above every object rides outside the level (Clutterfunk
    // 74%: a flipped cube falls up to the sim's ceiling and skips the end)
    // shortcut: static bound; a level that moves its objects up with
    // triggers can raise the route above it, and loses those runs
    c.retain(|c| !past(c.sim.mode_portals.0, c.sim.players[0].x) && c.sim.players[0].y < c.sim.top_y + ABOVE_TOP);
    // ties broken by state, not HashMap order, so a solve is reproducible
    c.sort_by_key(|c| (c.changes, key(&c.sim)));
    // gravity and mode split bands: an upside-down run is a different route
    // (Bloodbath 31%: only a UFO entered upside down clears the next cube part)
    let band = |c: &Cand| {
        c.sim.players.first().map_or((0, 0, 0, 0), |p| {
            ((p.x / 30.0).floor() as i64, (p.y / 10.0).floor() as i64, (p.vel_y / 2.0).floor() as i64, p.grav_flip as i64 + 2 * p.mode as i64)
        })
    };
    let mut seen = std::collections::HashSet::new();
    let (mut reps, mut rest): (Vec<Cand>, Vec<Cand>) = (Vec::new(), Vec::new());
    for cand in c {
        if seen.insert(band(&cand)) {
            reps.push(cand);
        } else {
            rest.push(cand);
        }
    }
    if reps.len() > width {
        reps.sort_by_key(|c| band(c));
        let n = reps.len();
        let mut pick: Vec<Option<Cand>> = reps.into_iter().map(Some).collect();
        return (0..width).filter_map(|i| pick[i * n / width].take()).collect();
    }
    let room = width - reps.len();
    reps.extend(rest.into_iter().take(room));
    reps
}

/// Walk the tree from `leaf` to the root and emit press/release edges.
fn to_macro(level: &Level, nodes: &[Node], leaf: usize) -> Macro {
    let mut acts = Vec::new();
    let mut n = leaf;
    while n != 0 && n != usize::MAX {
        acts.push(nodes[n].act);
        n = nodes[n].parent;
    }
    acts.reverse();
    let mut inputs = Vec::new();
    let mut prev = (false, false);
    for (i, &a) in acts.iter().enumerate() {
        let frame = (i * D + 1) as u64; // Playback frames are 1-based
        // sim player 1 is the macro's "player 2" (xdBot wiring)
        if a.0 != prev.0 {
            inputs.push(MacroInput { frame, button: 1, player2: true, down: a.0 });
        }
        if a.1 != prev.1 {
            inputs.push(MacroInput { frame, button: 1, player2: false, down: a.1 });
        }
        prev = a;
    }
    Macro {
        bot_name: "gd-rust auto".into(),
        level_id: level.id,
        framerate: 240.0,
        duration: (acts.len() * D) as f32 / 240.0,
        inputs,
        ..Default::default()
    }
}

/// The macro to play `level` with: a real .gdr2 in `macros` recorded on its
/// id (ones with xdBot frame fixes first), else the cached solve
/// `<id>.auto.json`, else a fresh solve (cached only when it completes and
/// a fresh-sim replay reproduces it). Err = partial run, percent reached.
pub fn macro_for(
    macros: &std::path::Path,
    level: &Level,
    table: &ObjectTable,
    progress: impl FnMut(f64),
) -> Result<Macro, (Macro, f64)> {
    let mut real: Vec<Macro> = std::fs::read_dir(macros)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "gdr2"))
        .filter_map(|e| super::gdr2::parse_gdr2(&std::fs::read(e.path()).ok()?).ok())
        .filter(|m| m.level_id == level.id)
        .collect();
    real.sort_by_key(|m| m.frames.is_empty());
    if let Some(m) = real.into_iter().next() {
        return Ok(m);
    }
    let cache = macros.join(format!("{}.auto.json", level.id));
    if let Some(m) = std::fs::read(&cache).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
        return Ok(m);
    }
    let m = solve(level, table, progress)?;
    if !replays(level, table, &m) {
        // shortcut: the solver's snapshot and a fresh sim disagree (e.g.
        // HashMap order); not cached so the next run retries
        return Err((m, 0.0));
    }
    let mut m = prune(level, table, m);
    m.sway = find_sway(level, table, &m);
    if let Ok(json) = serde_json::to_vec(&m) {
        let _ = std::fs::write(&cache, json);
    }
    Ok(m)
}

/// The post-solve steps of `macro_for` — verify on a fresh sim, prune the
/// clicks the run does not need, find the sway windows — without the
/// macros/ cache (the browser build has no filesystem).
pub fn finish(level: &Level, table: &ObjectTable, m: Macro) -> Result<Macro, Macro> {
    if !replays(level, table, &m) {
        return Err(m);
    }
    let mut m = prune(level, table, m);
    m.sway = find_sway(level, table, &m);
    Ok(m)
}

/// Drop every click the run does not need: remove one press (with its
/// release) at a time and keep the removal when the level still completes.
/// Clicks before the one being tried are final, so one running snapshot of
/// the sim just before it serves every try.
/// shortcut: each try replays to the end (clicks x level length); fine for
/// the background solve, cache makes it one-off.
fn prune(level: &Level, table: &ObjectTable, mut m: Macro) -> Macro {
    let need = portals(level, table, &m);
    let mut base = (Sim::new(level, table), 0u64); // sim after `.1` ticks
    let mut k = 0; // presses before index k are kept for good
    loop {
        let Some(j) = m.inputs.iter().enumerate().filter(|(_, i)| i.down).map(|(j, _)| j).nth(k) else { return m };
        let press = m.inputs[j].clone();
        // advance the snapshot to just before this press (same inputs as m there)
        let mut pb = super::Playback::new(m.clone());
        for _ in 0..base.1 {
            pb.step();
        }
        while base.1 + 1 < press.frame {
            let (p1, p2) = pb.step();
            base.0.step_single(InputState { hold: p2, hold_p2: p1 });
            base.1 += 1;
        }
        let mut cand = m.clone();
        cand.inputs.remove(j);
        if let Some(r) = cand.inputs[j..].iter().position(|i| !i.down && i.player2 == press.player2) {
            cand.inputs.remove(j + r);
        }
        if replays_from(base.0.clone(), base.1, &cand, need) {
            m = cand;
        } else {
            k += 1;
        }
    }
}

/// Largest click shift tried, in ticks (half a second), and the step.
const SWAY_MAX: i32 = 120;
const SWAY_STEP: i32 = 2;

/// Per press, the timing shifts (min, max ticks) that keep the run safe:
/// shifted alone, the run is back in exactly the original state before the
/// next click's earliest shifted time (the last click must still finish).
/// Found last click first, so each window is checked against the next
/// click's earliest shift; any mix of shifts within the windows is then
/// safe (see `swayed`).
/// shortcut: 2-player dual macros get no sway, and the state compared is
/// the players' motion (x, y, vy, mode, gravity, size, ground, click
/// buffer), not trigger state; a shifted jump that touches a trigger the
/// original missed could slip through.
pub fn find_sway(level: &Level, table: &ObjectTable, m: &Macro) -> Vec<(i32, i32)> {
    let ins = &m.inputs;
    if ins.iter().any(|i| !i.player2) {
        return vec![];
    }
    // presses with their releases (None = held to the end)
    let presses: Vec<(usize, Option<usize>)> = ins
        .iter()
        .enumerate()
        .filter(|(_, i)| i.down)
        .map(|(j, _)| (j, ins.iter().skip(j + 1).position(|i| !i.down).map(|r| j + 1 + r)))
        .collect();
    // the original run's sim after every input frame (and at 0)
    let mut snaps: Vec<(u64, Sim)> = vec![(0, Sim::new(level, table))];
    {
        let mut sim = snaps[0].1.clone();
        let mut pb = super::Playback::new(m.clone());
        let last = ins.last().map_or(0, |i| i.frame);
        for t in 1..=last {
            let (p1, p2) = pb.step();
            sim.step_single(InputState { hold: p2, hold_p2: p1 });
            if ins.iter().any(|i| i.frame == t) {
                snaps.push((t, sim.clone()));
            }
        }
    }
    let at = |t: u64| -> Sim {
        let (f, s) = snaps.iter().rev().find(|(f, _)| *f <= t).expect("snapshot 0");
        let mut sim = s.clone();
        let mut pb = super::Playback::new(m.clone());
        for _ in 0..*f {
            pb.step();
        }
        for _ in *f..t {
            let (p1, p2) = pb.step();
            sim.step_single(InputState { hold: p2, hold_p2: p1 });
        }
        sim
    };
    let need = portals(level, table, m);
    let mut win = vec![(0, 0); presses.len()];
    for k in (0..presses.len()).rev() {
        let (j, r) = presses[k];
        let p = ins[j].frame as i64;
        let a = if j > 0 { ins[j - 1].frame as i64 } else { 0 };
        let last_held = r.map_or(p, |r| ins[r].frame as i64);
        // compare just before the next press at its earliest shift
        let cmp = (k + 1 < presses.len()).then(|| ins[presses[k + 1].0].frame as i64 + win[k + 1].0 as i64 - 1);
        let target = cmp.map(|c| at(c as u64));
        let start = at(a as u64);
        let test = |s: i64| -> bool {
            if p + s <= a || cmp.is_some_and(|c| last_held + s > c) {
                return false;
            }
            let mut cand = m.clone();
            cand.inputs[j].frame = (p + s) as u64;
            if let Some(r) = r {
                cand.inputs[r].frame = (last_held + s) as u64;
            }
            let Some(c) = cmp else { return replays_from(start.clone(), a as u64, &cand, need) };
            let mut sim = start.clone();
            let mut pb = super::Playback::new(cand);
            for _ in 0..a {
                pb.step();
            }
            for _ in a..c {
                let (p1, p2) = pb.step();
                sim.step_single(InputState { hold: p2, hold_p2: p1 });
                if sim.state != RunState::Playing {
                    return false;
                }
            }
            same(&sim, target.as_ref().expect("target with cmp"))
        };
        for dir in [-1i32, 1] {
            let mut s = dir * SWAY_STEP;
            while s.abs() <= SWAY_MAX && test(s as i64) {
                if dir < 0 { win[k].0 = s } else { win[k].1 = s }
                s += dir * SWAY_STEP;
            }
        }
    }
    win
}

/// Two sims whose players move identically from here on.
fn same(a: &Sim, b: &Sim) -> bool {
    a.state == b.state
        && a.players.len() == b.players.len()
        && a.players.iter().zip(&b.players).all(|(p, q)| {
            p.x == q.x
                && (p.y - q.y).abs() < 1e-9
                && (p.vel_y - q.vel_y).abs() < 1e-9
                && (p.mode, p.grav_flip, p.mini, p.on_ground, p.queued_hold, p.dash, p.ball_ready) == (q.mode, q.grav_flip, q.mini, q.on_ground, q.queued_hold, q.dash, q.ball_ready)
        })
}

/// `m` with every press (and its release) moved by a random step within its
/// sway window, so each attempt clicks a little differently.
pub fn swayed(m: &Macro, seed: u64) -> Macro {
    let mut out = m.clone();
    let mut x = seed | 1;
    let mut k = 0;
    for j in 0..out.inputs.len() {
        if !out.inputs[j].down {
            continue;
        }
        let Some(&(lo, hi)) = m.sway.get(k) else { break };
        k += 1;
        // xorshift64
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let n = ((hi - lo) / SWAY_STEP + 1) as u64;
        let s = (lo + SWAY_STEP * (x % n) as i32) as i64;
        out.inputs[j].frame = (out.inputs[j].frame as i64 + s) as u64;
        if let Some(r) = (j + 1..out.inputs.len()).find(|&r| !m.inputs[r].down) {
            out.inputs[r].frame = (out.inputs[r].frame as i64 + s) as u64;
        }
    }
    out
}

/// Raw hardness of the two reference levels (see `difficulty`).
const REF_EASY: f64 = 7.247; // Stereo Madness
const REF_HARD: f64 = 82.365; // GRIEF

/// Arbitrary hardness, 0..1, from the level alone: things the player must
/// react to (hazards, orbs, pads, portals) per second of play at the level's
/// speeds, on a log scale with Stereo Madness = 0 and GRIEF = 1 (clamped;
/// Bloodbath lands near 0.67).
/// Returns (score, raw per-second value).
/// shortcut: counts objects, not how tight the path through them is; a level
/// full of hazards off the path scores high. If the counting changes,
/// re-measure REF_* (autocheck prints the raw value).
pub fn difficulty(level: &Level, table: &ObjectTable) -> (f64, f64) {
    use crate::sim::objects::ObjectKind as K;
    let sim = Sim::new(level, table);
    let mut objs: Vec<(f64, K)> = sim.grid.all().iter().map(|o| (o.x, o.kind)).collect();
    objs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let x0 = sim.players.first().map_or(0.0, |p| p.x);
    let mut speed = crate::physics::SPEEDS[sim.players.first().map_or(1, |p| p.speed_idx as usize) % 5];
    let (mut secs, mut at, mut n) = (0.0, x0, 0usize);
    for &(x, k) in objs.iter().filter(|(x, _)| *x >= x0 && *x <= sim.last_obj_x) {
        let interactive = !matches!(k, K::Solid | K::Slope | K::Decoration | K::Trigger | K::Coin);
        n += interactive as usize;
        if let K::SpeedPortal(i) = k {
            secs += (x - at) / speed;
            at = x;
            speed = crate::physics::SPEEDS[i as usize % 5];
        }
    }
    secs += (sim.last_obj_x - at).max(0.0) / speed;
    let raw = n as f64 / secs.max(1.0);
    let score = ((raw.max(1e-3) / REF_EASY).ln() / (REF_HARD / REF_EASY).ln()).clamp(0.0, 1.0);
    (score, raw)
}

/// Chance an attempt fails on purpose (the demo plays like a person):
/// about 1 in 100 at score 0, 3 in 4 at score 1.
pub fn fail_chance(score: f64) -> f64 {
    0.01 + 0.74 * score.clamp(0.0, 1.0).powf(1.5)
}

/// One demo attempt: `swayed`, and with `fail_chance(score)` a missed click
/// (a random press and its release dropped), so the run dies there the way a
/// person would. Frame fixes stop at the miss so real recordings can die too.
pub fn attempt(m: &Macro, score: f64, seed: u64) -> Macro {
    let mut out = swayed(m, seed);
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let presses: Vec<usize> = (0..out.inputs.len()).filter(|&j| out.inputs[j].down).collect();
    if presses.is_empty() || (next() % 10_000) as f64 >= fail_chance(score) * 10_000.0 {
        return out;
    }
    let j = presses[(next() % presses.len() as u64) as usize];
    let at = out.inputs[j].frame as usize;
    if let Some(r) = (j + 1..out.inputs.len()).find(|&r| !out.inputs[r].down) {
        out.inputs.remove(r);
    }
    out.inputs.remove(j);
    out.frames.truncate(at);
    out.frames_p2.truncate(at);
    out
}

/// Play `m` on a fresh sim the way the demo does; true if it completes.
pub fn replays(level: &Level, table: &ObjectTable, m: &Macro) -> bool {
    replays_from(Sim::new(level, table), 0, m, 0)
}

/// Mode changes `m` makes on its way to the end (0 if it doesn't finish):
/// a pruned or shifted run must make as many, or it skipped a portal.
fn portals(level: &Level, table: &ObjectTable, m: &Macro) -> u32 {
    play_out(Sim::new(level, table), 0, m).map_or(0, |s| s.mode_portals.0)
}

/// `replays`, from `sim` already stepped through `done` ticks of `m`, and
/// taking at least `need` mode portals.
fn replays_from(sim: Sim, done: u64, m: &Macro, need: u32) -> bool {
    play_out(sim, done, m).is_some_and(|s| s.mode_portals.0 >= need)
}

/// The sim at the end if `m` finishes the level on its route (no death,
/// never above the level, see `select`).
fn play_out(mut sim: Sim, done: u64, m: &Macro) -> Option<Sim> {
    let mut pb = super::Playback::new(m.clone());
    for _ in 0..done {
        pb.step();
    }
    for _ in 0..(m.duration as f64 * 240.0) as usize + 240 {
        let (p1, p2) = pb.step();
        sim.step_single(InputState { hold: p2, hold_p2: p1 });
        if sim.state == RunState::Dead || sim.players[0].y >= sim.top_y + ABOVE_TOP {
            return None;
        }
        if finished(&sim) {
            return Some(sim);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beam_width_shrinks_to_fit_the_memory_budget() {
        assert_eq!(capped_width(64, 10_000, 2), 64); // small sims: as asked
        let w = capped_width(4096, 200_000, 2); // GRIEF-sized sims
        assert!(w < 4096 && w * 200_000 * (3 + KEEP_CHECKPOINTS) <= MEM_BUDGET, "{w}");
        assert_eq!(capped_width(4096, usize::MAX / 64, 4), 8); // never below 8
    }

    fn level(objs: &[(u16, f32, f32)]) -> Level {
        let mut lvl = Level::default();
        for &(id, x, y) in objs {
            lvl.objects.push(crate::level::GdObject { id, x, y, ..Default::default() });
        }
        lvl
    }

    #[test]
    fn solves_spikes_and_a_step_and_the_replay_matches() {
        let table = ObjectTable::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/objects.json")).unwrap();
        // spike on the floor, then a one-block step to climb, then a spike on it
        let lvl = level(&[(8, 300.0, 15.0), (1, 600.0, 15.0), (1, 630.0, 15.0), (1, 660.0, 15.0), (8, 660.0, 45.0), (1, 900.0, 15.0)]);
        let mut none = Sim::new(&lvl, &table);
        // step_single respawns after the death pause: stop at the first death
        let died = (0..2000).any(|_| {
            none.step_single(InputState::single(false));
            none.state == RunState::Dead
        });
        assert!(died, "the level must need input");
        let m = solve(&lvl, &table, |_| {}).expect("solvable");
        assert!(!m.inputs.is_empty());
        assert!(replays(&lvl, &table, &m), "fresh-sim replay diverged");
    }

    #[test]
    fn select_drops_runs_off_the_route() {
        let table = ObjectTable::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/objects.json")).unwrap();
        let lvl = level(&[(1, 900.0, 15.0)]);
        let cand = |x: f64, portals: (u32, f64)| {
            let mut sim = Sim::new(&lvl, &table);
            sim.players[0].x = x;
            sim.mode_portals = portals;
            Cand { sim, node: 0, changes: 0 }
        };
        // run 0 took a portal at 500; run 1 skipped it and is well past;
        // run 2 skipped it too but is still beside the portal; run 3 flies
        // above the whole level
        let mut high = cand(530.0, (1, 500.0));
        high.sim.players[0].y = high.sim.top_y + ABOVE_TOP + 1.0; // above every object
        let kept = select(vec![cand(520.0, (1, 500.0)), cand(700.0, (0, 0.0)), cand(560.0, (0, 0.0)), high], 64);
        let mut xs: Vec<i64> = kept.iter().map(|c| c.sim.players[0].x as i64).collect();
        xs.sort();
        assert_eq!(xs, vec![520, 560]);
    }

    #[test]
    fn prune_drops_clicks_the_level_does_not_need() {
        let table = ObjectTable::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/objects.json")).unwrap();
        // nothing to jump over: both clicks are needless
        let lvl = level(&[(1, 600.0, 105.0)]);
        let click = |f, down| MacroInput { frame: f, button: 1, player2: true, down };
        let m = Macro {
            framerate: 240.0,
            duration: 3.0,
            inputs: vec![click(10, true), click(12, false), click(200, true), click(230, false)],
            ..Default::default()
        };
        assert!(replays(&lvl, &table, &m));
        let pruned = prune(&lvl, &table, m);
        assert!(pruned.inputs.is_empty(), "{:?}", pruned.inputs.iter().map(|i| i.frame).collect::<Vec<_>>());
        assert!(replays(&lvl, &table, &pruned));
    }

    #[test]
    fn any_mix_of_sway_shifts_still_completes() {
        let table = ObjectTable::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/objects.json")).unwrap();
        // three separate floor spikes: three jumps with slack around each
        let lvl = level(&[(8, 400.0, 15.0), (8, 800.0, 15.0), (8, 1200.0, 15.0), (1, 1500.0, 15.0)]);
        let m = prune(&lvl, &table, solve(&lvl, &table, |_| {}).expect("solvable"));
        let mut m2 = m.clone();
        m2.sway = find_sway(&lvl, &table, &m);
        assert!(m2.sway.iter().any(|&(lo, hi)| hi > lo), "some click has slack: {:?}", m2.sway);
        for seed in 1..40 {
            assert!(replays(&lvl, &table, &swayed(&m2, seed)), "seed {seed}, sway {:?}", m2.sway);
        }
    }

    #[test]
    fn hard_levels_miss_clicks_often_easy_ones_rarely() {
        let table = ObjectTable::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/objects.json")).unwrap();
        let lvl = level(&[(8, 400.0, 15.0), (8, 800.0, 15.0), (8, 1200.0, 15.0), (1, 1500.0, 15.0)]);
        let m = prune(&lvl, &table, solve(&lvl, &table, |_| {}).expect("solvable"));
        let misses = |score| (1..2000u64).filter(|&s| attempt(&m, score, s).inputs.len() < m.inputs.len()).count();
        let (easy, hard) = (misses(0.0), misses(1.0));
        assert!(easy < 60 && hard > 1300, "easy {easy} hard {hard}");
        let missed = (1..2000u64).map(|s| attempt(&m, 1.0, s)).find(|a| a.inputs.len() < m.inputs.len()).unwrap();
        assert!(!replays(&lvl, &table, &missed), "a missed click dies");
    }

    #[test]
    fn difficulty_runs_from_stereo_madness_to_grief() {
        let table = ObjectTable::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/objects.json")).unwrap();
        let score = |id: &str| {
            let text = std::fs::read_to_string(format!("{}/levels/{id}.gmd", env!("CARGO_MANIFEST_DIR"))).unwrap();
            difficulty(&crate::level::parse_level(&text), &table).0
        };
        let (sm, bb, grief) = (score("1"), score("10565740"), score("149992717"));
        assert!(sm < 0.01 && grief > 0.99 && (0.3..0.9).contains(&bb), "{sm} {bb} {grief}");
        assert!(fail_chance(sm) < 0.02 && fail_chance(grief) > 0.7);
    }

    #[test]
    fn cloned_sim_steps_like_the_original() {
        let table = ObjectTable::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/objects.json")).unwrap();
        let lvl = level(&[(1, 300.0, 15.0), (8, 500.0, 15.0)]);
        let mut a = Sim::new(&lvl, &table);
        for i in 0..100 {
            a.step_single(InputState::single(i % 30 < 5));
        }
        let mut b = a.clone();
        for i in 0..300 {
            a.step_single(InputState::single(i % 40 < 6));
            b.step_single(InputState::single(i % 40 < 6));
        }
        assert_eq!((a.players[0].x, a.players[0].y, a.state), (b.players[0].x, b.players[0].y, b.state));
    }
}
