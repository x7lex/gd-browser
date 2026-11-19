//! Interactive test rig: a looping showcase level with every orb, pad,
//! portal, a few blocks/slopes and hazards, driven by hand.
//!
//! Keys:
//!   Left / Right  cycle game mode (cube ship ball ufo wave robot spider swing)
//!   Up / Space    hold = jump / fly (the normal click)
//!   Down          teleport up to jump height and drop (landing / fall effects)
//!   M mini   G gravity flip   S speed (0.5x..4x)   R restart   Esc/Q quit
//! Reaching the end shows the level-complete effect for 3 s, then restarts;
//! deaths respawn. Both re-apply the picked mode, size, gravity and speed.

use gdrust::level::{ColorChannel, GdObject, Level};
use gdrust::render::{Renderer, FRAME_H, FRAME_W};
use gdrust::sim::objects::ObjectTable;
use gdrust::sim::player::GameMode;
use gdrust::sim::{InputState, RunState, Sim};
use minifb::{Key, KeyRepeat, Window, WindowOptions};

const MODES: [GameMode; 8] = [
    GameMode::Cube,
    GameMode::Ship,
    GameMode::Ball,
    GameMode::Ufo,
    GameMode::Wave,
    GameMode::Robot,
    GameMode::Spider,
    GameMode::Swing,
];
const SPEED_NAMES: [&str; 5] = ["0.5x", "1x", "2x", "3x", "4x"];

/// Showcase level: sections of orbs, pads, mode/size/gravity/speed portals,
/// blocks + slopes, then hazards (death effect). Editor y is the object
/// centre above the floor: 15 = a block on the floor, 2 = a pad, 45 = row 2.
fn showcase() -> Level {
    let mut objs: Vec<(u16, f32, f32, f32)> = Vec::new();
    let mut x = 450.0;
    let row = |ids: &[u16], y: f32, gap: f32, objs: &mut Vec<(u16, f32, f32, f32)>, x: &mut f32| {
        for &id in ids {
            objs.push((id, *x, y, 0.0));
            *x += gap;
        }
        *x += 300.0;
    };
    // orbs: yellow pink red blue green black(drop) dash pink-dash spider toggle
    row(&[36, 141, 1333, 84, 1022, 1330, 1704, 1751, 3649, 1594], 45.0, 180.0, &mut objs, &mut x);
    // pads on the floor: yellow pink red blue(gravity)
    row(&[35, 140, 1332, 67], 2.0, 240.0, &mut objs, &mut x);
    // secret coin and user coin (collect: they vanish with GD's pickup burst)
    row(&[142, 1329], 45.0, 180.0, &mut objs, &mut x);
    // mode portals: cube ship ball ufo wave robot spider swing
    row(&[12, 13, 47, 111, 660, 745, 1331, 1933], 45.0, 360.0, &mut objs, &mut x);
    // size, gravity, mirror, dual/solo portals (then back to normal)
    row(&[101, 99, 11, 10, 45, 46, 286, 287], 45.0, 300.0, &mut objs, &mut x);
    // speed portals: 0.5x 1x 2x 3x 4x, back to 1x
    row(&[200, 201, 202, 203, 1334, 201], 45.0, 300.0, &mut objs, &mut x);
    // a block staircase and slopes to land on
    for i in 0..6 {
        for j in 0..=i {
            objs.push((1, x + i as f32 * 30.0, j as f32 * 30.0 + 15.0, 0.0));
        }
    }
    x += 300.0;
    objs.push((363, x, 15.0, 0.0));
    objs.push((1, x + 30.0, 15.0, 0.0));
    objs.push((364, x + 75.0, 15.0, 0.0));
    x += 450.0;
    // hazards last: spike, small spike, saw (death effect)
    objs.push((8, x, 15.0, 0.0));
    objs.push((39, x + 240.0, 6.0, 0.0)); // small spike sits low
    objs.push((88, x + 480.0, 15.0, 0.0));
    let mut lvl = Level::default();
    lvl.header.start_speed = 0;
    let ch = |id, (r, g, b)| ColorChannel { id, r, g, b, opacity: 1.0, copy: None, blending: false };
    lvl.header.channels = vec![ch(1000, (40, 62, 160)), ch(1001, (0, 70, 200)), ch(1002, (255, 255, 255))];
    lvl.objects = objs
        .into_iter()
        .map(|(id, x, y, rot)| GdObject { id, x, y, rot, scale_x: 1.0, scale_y: 1.0, ..GdObject::default() })
        .collect();
    lvl
}

/// What the keys have selected; re-applied after deaths / wraps.
struct Pick {
    mode: usize,
    mini: bool,
    flip: bool,
    speed: u8,
}

fn apply(sim: &mut Sim, pick: &Pick) {
    let p = &mut sim.players[0];
    if p.mode != MODES[pick.mode] {
        let y = p.y;
        p.enter_mode(MODES[pick.mode], y);
    }
    p.mini = pick.mini;
    if p.grav_flip != pick.flip {
        p.flip_gravity(pick.flip);
    }
    p.speed_idx = pick.speed;
}

/// Teleport to a cube jump's apex (~2.3 blocks) above the current surface
/// (ceiling when flipped) and let it fall.
fn drop_from_jump_height(sim: &mut Sim) {
    let p = &mut sim.players[0];
    let (lo, hi) = p.band;
    let surf = if p.grav_flip { hi.min(lo + 300.0) - p.outer_half() } else { lo + p.outer_half() };
    p.y = surf + if p.grav_flip { -70.0 } else { 70.0 };
    p.vel_y = 0.0;
    p.on_ground = false;
}

/// Flipped into open sky (cube-like modes have no ceiling): put the picked
/// state back and drop in again.
fn keep_in_view(sim: &mut Sim, pick: &Pick) {
    if sim.players[0].y > 1500.0 || sim.players[0].y < 0.0 {
        apply(sim, pick);
        drop_from_jump_height(sim);
    }
}

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let table = ObjectTable::load(std::path::Path::new(&format!("{manifest}/data/objects.json"))).unwrap();
    let mut renderer = Renderer::new(&gdrust::assets::Assets::fs(std::path::Path::new(&gdrust::assets::find_or_ask())));
    let level = showcase();
    let mut sim = Sim::new(&level, &table);
    let mut pick = Pick { mode: 0, mini: false, flip: false, speed: 1 };
    apply(&mut sim, &pick);
    let mut attempt = sim.attempt;

    // headless check: RIG_SHOT=<mode index>:<ticks>:<out.png> renders one frame and exits
    if let Ok(shot) = std::env::var("RIG_SHOT") {
        let v: Vec<&str> = shot.splitn(3, ':').collect();
        pick.mode = v[0].parse().expect("mode index");
        apply(&mut sim, &pick);
        let ticks = v[1].parse::<usize>().expect("ticks");
        for t in 0..ticks {
            sim.step_single(InputState { hold: t % 90 < 30, hold_p2: false });
            keep_in_view(&mut sim, &pick);
            // draw the last ~100 frames too, so particles and timers have built up
            if t % 4 == 0 && t + 400 > ticks {
                renderer.draw(&level, &table, &sim);
            }
        }
        let buf = renderer.draw(&level, &table, &sim);
        let pm = tiny_skia::Pixmap::from_vec(buf, tiny_skia::IntSize::from_wh(FRAME_W, FRAME_H).unwrap()).unwrap();
        pm.save_png(v[2]).expect("png");
        println!("{:?} x {:.0} y {:.0} state {:?}", sim.players[0].mode, sim.players[0].x, sim.players[0].y, sim.state);
        return;
    }
    // RIG_TOUR=<out.mp4>: 4 s of every mode (a drop, then a click pattern), 60 fps
    if let Ok(out) = std::env::var("RIG_TOUR") {
        let mut ff = std::process::Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba"])
            .args(["-s", &format!("{FRAME_W}x{FRAME_H}"), "-r", "60", "-i", "-"])
            .args(["-c:v", "libx264", "-crf", "20", "-pix_fmt", "yuv420p", "-movflags", "+faststart", &out])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("ffmpeg");
        let mut pipe = ff.stdin.take().expect("stdin");
        for m in 0..MODES.len() {
            pick.mode = m;
            apply(&mut sim, &pick);
            drop_from_jump_height(&mut sim);
            for f in 0..240 {
                for _ in 0..4 {
                    let hold = f > 40 && f % 45 < 18;
                    sim.step_single(InputState { hold, hold_p2: hold });
                    if sim.attempt != attempt && sim.state == RunState::Playing {
                        attempt = sim.attempt;
                        apply(&mut sim, &pick);
                    }
                }
                use std::io::Write;
                pipe.write_all(&renderer.draw(&level, &table, &sim)).expect("ffmpeg pipe");
            }
        }
        drop(pipe);
        assert!(ff.wait().expect("ffmpeg").success(), "ffmpeg failed");
        println!("wrote {out}");
        return;
    }
    let mut window = Window::new("gd-rust rig", FRAME_W as usize, FRAME_H as usize, WindowOptions::default())
        .expect("window");
    window.set_target_fps(0);
    println!("rig: Left/Right mode, Up/Space jump, Down drop from jump height, M mini, G gravity, S speed, R restart, Esc quit");
    let mut fb = vec![0u32; FRAME_W as usize * FRAME_H as usize];
    let (mut clock, mut acc) = (std::time::Instant::now(), 0.0f64);
    let mut complete_ticks = 0u32;
    let mut title_t = std::time::Instant::now();
    let mut frames = 0u32;

    while window.is_open() && !window.is_key_down(Key::Escape) && !window.is_key_down(Key::Q) {
        let pressed = |k| window.is_key_pressed(k, KeyRepeat::No);
        let n = MODES.len();
        let mut changed = false;
        if pressed(Key::Right) {
            pick.mode = (pick.mode + 1) % n;
            changed = true;
        }
        if pressed(Key::Left) {
            pick.mode = (pick.mode + n - 1) % n;
            changed = true;
        }
        if pressed(Key::M) {
            pick.mini = !pick.mini;
            changed = true;
        }
        if pressed(Key::G) {
            pick.flip = !pick.flip;
            changed = true;
        }
        if pressed(Key::S) {
            pick.speed = (pick.speed + 1) % 5;
            changed = true;
        }
        if pressed(Key::R) {
            sim.reset();
            changed = true;
        }
        if changed {
            apply(&mut sim, &pick);
        }
        if pressed(Key::Down) {
            drop_from_jump_height(&mut sim);
        }
        let hold = window.is_key_down(Key::Up) || window.is_key_down(Key::Space);

        let now = std::time::Instant::now();
        acc = (acc + (now - clock).as_secs_f64()).min(0.1);
        clock = now;
        while acc >= 1.0 / 240.0 {
            acc -= 1.0 / 240.0;
            sim.step_single(InputState { hold, hold_p2: hold });
            // the end: 3 s of the level-complete effect, then a fresh run
            if sim.state == RunState::Complete {
                complete_ticks += 1;
                if complete_ticks > 720 {
                    complete_ticks = 0;
                    sim.reset();
                    apply(&mut sim, &pick);
                }
            }
            // deaths respawn at the level start mode: put the picked state back
            if sim.attempt != attempt && sim.state == RunState::Playing {
                attempt = sim.attempt;
                apply(&mut sim, &pick);
            }
            keep_in_view(&mut sim, &pick);
        }

        let buf = renderer.draw(&level, &table, &sim);
        for (d, s) in fb.iter_mut().zip(buf.chunks_exact(4)) {
            *d = (s[0] as u32) << 16 | (s[1] as u32) << 8 | s[2] as u32;
        }
        let _ = window.update_with_buffer(&fb, FRAME_W as usize, FRAME_H as usize);
        frames += 1;
        if title_t.elapsed().as_secs_f64() >= 0.5 {
            let p = &sim.players[0];
            window.set_title(&format!(
                "rig | {:?}{}{} | speed {} | x {:.0} y {:.0} vel {:.1} | {:.0} fps | ←/→ mode ↑ jump ↓ drop M G S R",
                p.mode,
                if p.mini { " mini" } else { "" },
                if p.grav_flip { " flipped" } else { "" },
                SPEED_NAMES[p.speed_idx as usize % 5],
                p.x,
                p.y,
                p.vel_y,
                frames as f64 / title_t.elapsed().as_secs_f64()
            ));
            title_t = std::time::Instant::now();
            frames = 0;
        }
    }
}
