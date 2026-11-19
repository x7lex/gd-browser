//! Per-frame GD gameplay effects: which particle emitter fires when (ground
//! drag, landing, ship/swing/robot fire, dash, portal/orb/pad idle, death,
//! coin pickup, speed portals, level-complete fireworks, end wall), spider
//! teleport streaks, the respawn ring, robot/spider animation phase and the
//! x-triggered camera triggers (shake 1520, zoom 1913, offset 1916).
//! Everything is derived from `Sim` state, so skipped frames do not matter.

use super::fx::{self, Emitter, Particles};
use crate::assets::Assets;
use crate::level::Level;
use crate::sim::objects::ObjectKind as K;
use crate::sim::player::{GameMode, Player};
use crate::sim::{RunState, Sim};
use std::collections::HashMap;

/// Emitter plists loaded from the GD resources folder.
pub fn effect_names() -> &'static [&'static str] {
    &[
        "dragEffect",
        "shipDragEffect",
        "landEffect",
        "dashEffect",
        "swingBurstEffect",
        "portalEffect01",
        "portalEffect02",
        "ringEffect",
        "bumpEffect",
        "explodeEffect",
        "firework",
        "coinPickupEffect",
        "endEffectPortal",
        "speedEffect_slow",
        "speedEffect_normal",
        "speedEffect_fast",
        "speedEffect_vfast",
        "speedEffect_vvfast",
    ]
}

/// Is a level object visible in game: not hidden (key 135) and not faded
/// out by alpha triggers. Invisible objects keep working but show no effects.
pub fn shows(level: &Level, sim: &Sim, src: usize) -> bool {
    level.objects.get(src).is_some_and(|o| !o.hidden) && sim.triggers.alpha(src) > 0.05
}

pub fn player_color(i: usize) -> (u8, u8, u8) {
    if i == 0 { (255, 45, 140) } else { (255, 120, 190) }
}

/// Color of an orb / pad / portal (use rings, idle particles).
pub fn kind_color(k: K) -> Option<(u8, u8, u8)> {
    Some(match k {
        K::OrbYellow | K::PadYellow => (255, 220, 0),
        K::OrbPink | K::PadPink => (255, 90, 220),
        K::OrbRed | K::PadRed => (255, 60, 60),
        K::OrbBlue | K::PadGravity | K::GravityPortal(false) => (60, 210, 255),
        K::GravityPortal(true) => (255, 230, 60),
        K::OrbGreen | K::PortalCube | K::PortalSize(false) => (80, 255, 90),
        K::OrbDash(_) => (120, 255, 160),
        K::OrbDrop | K::OrbSpider | K::PortalSpider => (190, 90, 255),
        K::OrbToggle => (255, 255, 255),
        K::PortalShip | K::PortalSize(true) => (255, 90, 255),
        K::PortalBall => (255, 110, 40),
        K::PortalUfo | K::PortalDual | K::PortalMirror => (255, 170, 40),
        K::PortalWave | K::PortalSolo | K::PortalTeleport => (60, 160, 255),
        K::PortalRobot => (235, 235, 235),
        K::PortalSwing => (255, 235, 60),
        K::SpeedPortal(_) => (255, 255, 255),
        K::Coin => (255, 230, 60),
        _ => return None,
    })
}

/// GD assets the effects need; each piece is optional (missing = skipped).
pub struct Gfx {
    pub em: HashMap<&'static str, Emitter>,
    /// ship / robot flame (GD animates fire sprites; particles stand in)
    pub fire: Emitter,
    pub robot: Option<fx::Anims>,
    pub spider: Option<fx::Anims>,
    pub font: Option<fx::Font>,
}

impl Gfx {
    pub fn load(assets: &Assets) -> Gfx {
        let read = |n: &str| assets.text(n);
        let em = effect_names()
            .iter()
            .filter_map(|&n| Some((n, Emitter::from_plist(&fx::parse_plist(&read(&format!("{n}.plist"))?)?))))
            .collect();
        // shortcut: ship fire is particles, GD plays shipfire sprite frames
        let fire = Emitter {
            duration: -1.0,
            max: 24.0,
            life: (0.22, 0.06),
            angle: (180.0, 12.0),
            speed: (70.0, 20.0),
            size0: (6.0, 2.0),
            size1: (1.0, 0.0),
            c0: [1.0, 0.8, 0.25, 1.0],
            c0v: [0.0, 0.1, 0.1, 0.0],
            c1: [1.0, 0.2, 0.0, 0.0],
            src_var: (1.0, 2.0),
            additive: true,
            ..Default::default()
        };
        Gfx {
            em,
            fire,
            robot: read("Robot_AnimDesc.plist").and_then(|t| fx::parse_anims(&t)),
            spider: read("Spider_AnimDesc.plist").and_then(|t| fx::parse_anims(&t)),
            font: fx::Font::load(assets, "bigFont-uhd.fnt"),
        }
    }

    /// Current robot/spider animation frame for a player, from its phase
    /// (0 ground, 1 rising, 2 falling) and seconds in that phase.
    pub fn anim_frame(&self, p: &Player, phase: u8, t: f32) -> Option<&[fx::Part]> {
        let speed = [0.8, 1.0, 1.2, 1.4, 1.6][p.speed_idx as usize % 5];
        let (anims, seq): (&fx::Anims, &[&str]) = match (p.mode, phase) {
            (GameMode::Robot, 0) => (self.robot.as_ref()?, &["run"]),
            (GameMode::Robot, 1) => (self.robot.as_ref()?, &["jump_start", "jump_loop"]),
            (GameMode::Robot, _) => (self.robot.as_ref()?, &["fall_start", "fall_loop"]),
            (GameMode::Spider, 0) => (self.spider.as_ref()?, &["run"]),
            (GameMode::Spider, 1) => (self.spider.as_ref()?, &["jump"]),
            (GameMode::Spider, _) => (self.spider.as_ref()?, &["fall"]),
            _ => return None,
        };
        // shortcut: 24 fps (x speed on the ground); GD's per-anim delays are in the binary
        let fps = if phase == 0 { 24.0 * speed } else { 24.0 };
        let mut n = (t * fps) as usize;
        // a "start" part plays once, then the last part loops; one-part anims loop
        for (i, name) in seq.iter().enumerate() {
            let frames = anims.get(*name)?;
            let last = i + 1 == seq.len();
            if last {
                let once = seq.len() == 1 && phase != 0 && name.as_bytes()[0] != b'r';
                let k = if once { n.min(frames.len() - 1) } else { n % frames.len() };
                return frames.get(k).map(|f| f.as_slice());
            }
            if n < frames.len() {
                return frames.get(n).map(|f| f.as_slice());
            }
            n -= frames.len();
        }
        None
    }
}

#[derive(Clone, Copy, Default)]
pub struct Prev {
    on_ground: bool,
    y: f64,
    mode: Option<GameMode>,
    pub phase: u8,
    pub phase_t: f32,
}

/// One x-triggered camera trigger.
struct CamCmd {
    x: f32,
    kind: u8, // 0 shake, 1 zoom, 2 offset, 3 static (a = target y, NaN = exit)
    a: f32,
    b: f32,
    dur: f32,
}

/// Camera triggers fired as the player passes their x (spawn/touch-triggered
/// ones are skipped, like the color triggers).
/// shortcut: zoom < 1 is clamped to 1 (the view is rendered at a fixed size),
/// easing is linear, and static/edge/rotate/guide camera triggers are ignored.
pub struct Cam {
    cmds: Vec<CamCmd>,
    cursor: usize,
    zoom: (f32, f32, f32, f32), // from, to, t, dur
    off: ((f32, f32), (f32, f32), f32, f32),
    shake: (f32, f32, f32), // strength, interval, time left
    shake_t: f32,
    pub shake_off: (f32, f32),
    /// static camera (1914): world y the camera centers on
    pub static_y: Option<f32>,
    rng: Particles,
}

impl Cam {
    pub fn new(level: &Level) -> Cam {
        let kv = |o: &crate::level::GdObject, k: u16| -> Option<f32> {
            o.raw.iter().find(|(kk, _)| *kk == k).and_then(|(_, v)| v.parse().ok())
        };
        // static camera target: first object in the group, at its placed position
        // shortcut: Y axis only (GD's key 101 = 2, all GRIEF uses), no easing
        // time and no follow of a moving target; touch-activated ones skipped
        let group_y = |g: f32| level.objects.iter().find(|o| o.groups.contains(&(g as u16))).map(|o| o.y);
        let mut cmds: Vec<CamCmd> = level
            .objects
            .iter()
            .filter(|o| kv(o, 62) != Some(1.0) && kv(o, 11) != Some(1.0))
            .filter_map(|o| {
                let dur = kv(o, 10).unwrap_or(0.5);
                Some(match o.id {
                    1520 => CamCmd { x: o.x, kind: 0, a: kv(o, 75).unwrap_or(0.0), b: kv(o, 84).unwrap_or(0.0), dur },
                    1913 => CamCmd { x: o.x, kind: 1, a: kv(o, 371).unwrap_or(1.0), b: 0.0, dur },
                    1916 => CamCmd { x: o.x, kind: 2, a: kv(o, 28).unwrap_or(0.0), b: kv(o, 29).unwrap_or(0.0), dur },
                    1914 if kv(o, 110) == Some(1.0) => CamCmd { x: o.x, kind: 3, a: f32::NAN, b: 0.0, dur },
                    1914 if kv(o, 101) == Some(2.0) => CamCmd { x: o.x, kind: 3, a: group_y(kv(o, 71)?)?, b: 0.0, dur },
                    _ => return None,
                })
            })
            .collect();
        cmds.sort_by(|a, b| a.x.total_cmp(&b.x));
        Cam {
            cmds,
            cursor: 0,
            zoom: (1.0, 1.0, 0.0, 0.0),
            off: ((0.0, 0.0), (0.0, 0.0), 0.0, 0.0),
            shake: (0.0, 0.0, 0.0),
            shake_t: 0.0,
            shake_off: (0.0, 0.0),
            static_y: None,
            rng: Particles::default(),
        }
    }

    /// Advance by `dt` s with the lead player at `x`; returns (zoom, offset incl. shake).
    pub fn step(&mut self, x: f64, dt: f32) -> (f32, (f32, f32)) {
        let lerp = |a: f32, b: f32, k: f32| a + (b - a) * k;
        let k_of = |t: f32, d: f32| if d <= 0.0 { 1.0 } else { (t / d).min(1.0) };
        while self.cursor < self.cmds.len() && self.cmds[self.cursor].x as f64 <= x {
            let c = &self.cmds[self.cursor];
            match c.kind {
                0 => self.shake = (c.a, c.b, c.dur),
                3 => self.static_y = Some(c.a).filter(|y| !y.is_nan()),
                1 => {
                    let cur = lerp(self.zoom.0, self.zoom.1, k_of(self.zoom.2, self.zoom.3));
                    self.zoom = (cur, c.a.max(0.1), 0.0, c.dur);
                }
                _ => {
                    let k = k_of(self.off.2, self.off.3);
                    let cur = (lerp(self.off.0 .0, self.off.1 .0, k), lerp(self.off.0 .1, self.off.1 .1, k));
                    self.off = (cur, (c.a, c.b), 0.0, c.dur);
                }
            }
            self.cursor += 1;
        }
        self.zoom.2 += dt;
        self.off.2 += dt;
        if self.shake.2 > 0.0 {
            self.shake.2 -= dt;
            self.shake_t -= dt;
            if self.shake_t <= 0.0 {
                self.shake_t = self.shake.1;
                self.shake_off = (self.rng.rand() * self.shake.0, self.rng.rand() * self.shake.0);
            }
        } else {
            self.shake_off = (0.0, 0.0);
        }
        let zk = k_of(self.zoom.2, self.zoom.3);
        let ok = k_of(self.off.2, self.off.3);
        let zoom = lerp(self.zoom.0, self.zoom.1, zk);
        let off = (lerp(self.off.0 .0, self.off.1 .0, ok), lerp(self.off.0 .1, self.off.1 .1, ok));
        (zoom, (off.0 + self.shake_off.0, off.1 + self.shake_off.1))
    }
}

/// Effect state carried between frames.
#[derive(Default)]
pub struct State {
    pub parts: Particles,
    frame: u64,
    pub prev: Vec<Prev>,
    acc: HashMap<(u8, usize), f32>,
    /// spider teleports: (x, from y, to y, age s)
    pub streaks: Vec<(f64, f64, f64, f32)>,
    /// (attempt, its spawn x) for the in-world "Attempt N" text; no x when
    /// the first frame drawn was not the spawn (a renderer joining mid-run)
    pub attempt: (u32, Option<f64>),
    pub respawn_age: f32,
    pub complete_age: f32,
    /// (x, y, age) of the death ring
    pub death: Option<(f64, f64, f32)>,
    coins: crate::sim::fx::HashSet<usize>,
    dead: bool,
    pub cam: Option<Cam>,
    cam_key: (u64, u32),
}

impl State {
    /// Camera trigger offset/zoom for this frame (rebuilt per attempt).
    pub fn camera(&mut self, level: &Level, sim: &Sim, lead_x: f64) -> (f32, (f32, f32)) {
        let key = (level.uid, sim.attempt);
        if self.cam_key != key || self.cam.is_none() {
            self.cam = Some(Cam::new(level));
            self.cam_key = key;
        }
        let dt = if sim.frame >= self.frame { (sim.frame - self.frame) as f32 / 240.0 } else { 0.0 };
        self.cam.as_mut().expect("cam").step(lead_x, dt.min(0.25))
    }

    /// Spawn/advance particles for this frame. `view` = (left x, right x, center y), world units.
    pub fn update(&mut self, gfx: &Gfx, sim: &Sim, level: &Level, view: (f32, f32, f32)) {
        let dt = if sim.frame >= self.frame { (sim.frame - self.frame) as f32 / 240.0 } else { 0.0 }.min(0.25);
        self.frame = sim.frame;
        let Some(p0) = sim.players.first() else { return };
        if self.attempt.0 != sim.attempt {
            self.attempt = (sim.attempt, (sim.frame < 24).then_some(p0.x));
            self.respawn_age = 0.0;
            self.complete_age = 0.0;
            self.prev.clear();
            self.coins.clear();
            self.streaks.clear();
        }
        self.respawn_age += dt;
        let em = |n: &str| gfx.em.get(n);
        let State { parts, acc, prev, streaks, coins, .. } = self;

        // death: GD's explode burst in the player color + expanding ring
        let dead = sim.state == RunState::Dead;
        if dead && !self.dead {
            for (i, p) in sim.players.iter().enumerate() {
                if let Some(e) = em("explodeEffect") {
                    parts.spawn(&e.tinted(player_color(i)), p.x as f32, p.y as f32, e.max as usize, false, 0.0);
                }
            }
            self.death = Some((p0.x, p0.y, 0.0));
        }
        self.dead = dead;
        if let Some(d) = &mut self.death {
            d.2 += dt;
            if d.2 > 0.6 {
                self.death = None;
            }
        }

        // coins: pickup burst where the coin was
        for &c in sim.coins.difference(coins) {
            if let (Some(o), Some(e)) = (level.objects.get(c), em("coinPickupEffect")) {
                let y = o.y + crate::physics::EDITOR_Y0 as f32;
                parts.spawn(e, o.x, y, e.max as usize, false, 0.0);
            }
        }
        coins.clone_from(&sim.coins);

        // player effects
        prev.resize(sim.players.len(), Prev::default());
        for (i, p) in sim.players.iter().enumerate().filter(|_| !dead) {
            let pv = &mut prev[i];
            let flip = p.grav_flip;
            let gs = if flip { -1.0 } else { 1.0 };
            let half = p.outer_half() as f32;
            let (x, y) = (p.x as f32, p.y as f32);
            let bottom = y - half * gs;
            let col = player_color(i);
            let mut emit = |parts: &mut Particles, k: u8, e: &Emitter, x: f32, y: f32, dir: f32| {
                parts.emit(e, x, y, dt, acc.entry((k, i)).or_default(), flip, dir);
            };
            match p.mode {
                GameMode::Cube | GameMode::Ball | GameMode::Robot | GameMode::Spider => {
                    if p.on_ground {
                        if let Some(e) = em("dragEffect") {
                            emit(parts, 0, &e.tinted(col), x - half * 0.6, bottom, 0.0);
                        }
                        if !pv.on_ground && pv.mode == Some(p.mode) {
                            if let Some(e) = em("landEffect") {
                                parts.spawn(&e.tinted(col), x, bottom, e.max as usize, flip, 0.0);
                            }
                        }
                    }
                }
                GameMode::Ship => {
                    if p.on_ground {
                        if let Some(e) = em("shipDragEffect") {
                            emit(parts, 0, &e.tinted(col), x - half * 0.4, bottom, 0.0);
                        }
                    }
                    let mut fire = gfx.fire.clone();
                    if p.hold {
                        fire.max *= 2.0;
                        fire.speed.0 *= 1.5;
                    }
                    emit(parts, 1, &fire, x - half * 1.1, y - half * 0.35 * gs, 0.0);
                }
                GameMode::Swing => {
                    if let Some(e) = em("swingBurstEffect") {
                        emit(parts, 1, e, x - half, y + half * 0.5 * gs, 180.0);
                    }
                }
                _ => {}
            }
            // robot jump: flame under the feet while the boost is held
            if p.mode == GameMode::Robot && p.hold && !p.on_ground && p.vel_y * gs as f64 > 0.0 {
                emit(parts, 2, &gfx.fire, x, bottom, 90.0);
            }
            if p.dash.is_some() {
                if let Some(e) = em("dashEffect") {
                    emit(parts, 3, e, x, y, 0.0);
                }
            }
            if p.mode == GameMode::Spider && pv.mode == Some(GameMode::Spider) && (p.y - pv.y).abs() > 45.0 {
                streaks.push((p.x, pv.y, p.y, 0.0));
            }
            let phase = if p.on_ground { 0 } else if p.vel_y * gs as f64 > 0.0 { 1 } else { 2 };
            if phase != pv.phase || pv.mode != Some(p.mode) {
                pv.phase = phase;
                pv.phase_t = 0.0;
            } else {
                pv.phase_t += dt;
            }
            pv.on_ground = p.on_ground;
            pv.y = p.y;
            pv.mode = Some(p.mode);
        }
        for s in streaks.iter_mut() {
            s.3 += dt;
        }
        streaks.retain(|s| s.3 < 0.25);

        // idle particles on portals, orbs and pads in view
        let (x0, x1, cy) = view;
        // grid objects are sorted by x (Sim::new): binary-search the view
        // instead of scanning every object each frame
        let all = sim.grid.all();
        let lo = all.partition_point(|o| (o.x as f32) < x0 - 60.0);
        let hi = all.partition_point(|o| (o.x as f32) <= x1 + 60.0);
        for (ci, o) in all.iter().enumerate().take(hi).skip(lo) {
            let name = match o.kind {
                K::GravityPortal(false) => "portalEffect01",
                K::GravityPortal(true) => "portalEffect02",
                K::PortalCube | K::PortalShip | K::PortalBall | K::PortalUfo | K::PortalWave | K::PortalRobot
                | K::PortalSpider | K::PortalSwing | K::PortalSize(_) | K::PortalDual | K::PortalSolo
                | K::PortalMirror | K::PortalTeleport => "portalEffect01",
                k if crate::sim::is_orb(k) => "ringEffect",
                K::PadYellow | K::PadPink | K::PadRed | K::PadGravity => "bumpEffect",
                _ => continue,
            };
            let (Some(e), Some((dx, dy, _))) = (em(name), sim.triggers.transform(o.src)) else { continue };
            if !shows(level, sim, o.src) {
                continue;
            }
            let mut e = match (o.kind, kind_color(o.kind)) {
                (K::GravityPortal(_), _) | (_, None) => e.clone(),
                (_, Some(c)) => e.tinted(c),
            };
            // particles follow the object's scale (GRIEF shrinks many portals)
            let lo = &level.objects[o.src];
            let k = (lo.scale_x.abs() + lo.scale_y.abs()) * 0.5;
            if (k - 1.0).abs() > 0.01 && k > 0.0 {
                e.radius_max.0 *= k;
                e.src_var = (e.src_var.0 * k, e.src_var.1 * k);
                e.size0 = (e.size0.0 * k, e.size0.1 * k);
                e.speed = (e.speed.0 * k, e.speed.1 * k);
            }
            let slot = acc.entry((10, ci)).or_default();
            parts.emit(&e, (o.x + dx) as f32, (o.y + dy) as f32, dt, slot, false, -o.rot as f32);
        }
        // end wall: GD's end portal particles at the level's end
        if let Some(e) = em("endEffectPortal") {
            let ex = sim.last_obj_x as f32 + 60.0;
            if ex > x0 - 100.0 && ex < x1 + 100.0 {
                let slot = acc.entry((11, 0)).or_default();
                parts.emit(e, ex, cy, dt, slot, false, 0.0);
            }
        }
        // level complete: fireworks over the view
        if sim.state == RunState::Complete {
            self.complete_age += dt;
            if let Some(e) = em("firework") {
                let slot = acc.entry((12, 0)).or_default();
                *slot += dt * 3.0;
                while *slot >= 1.0 {
                    *slot -= 1.0;
                    const COLS: [(u8, u8, u8); 5] =
                        [(255, 80, 80), (80, 255, 120), (80, 160, 255), (255, 230, 60), (255, 80, 255)];
                    let (r1, r2, r3) = (parts.rand(), parts.rand(), parts.rand());
                    let c = COLS[((r3 + 1.0) * 2.49) as usize % 5];
                    let fx = x0 + (x1 - x0) * (0.5 + 0.4 * r1);
                    parts.spawn(&e.tinted(c), fx, cy + 80.0 * r2, e.max as usize, false, 0.0);
                }
            }
        }
        parts.update(dt);
    }

    /// Particle burst for a speed portal the player just entered.
    pub fn speed_burst(&mut self, gfx: &Gfx, idx: u8, x: f32, y: f32) {
        let name = ["speedEffect_slow", "speedEffect_normal", "speedEffect_fast", "speedEffect_vfast", "speedEffect_vvfast"]
            [idx as usize % 5];
        if let Some(e) = gfx.em.get(name) {
            let n = (e.rate() * e.duration.max(0.1)) as usize;
            self.parts.spawn(e, x + 150.0, y, n.min(80), false, 0.0);
        }
    }
}
