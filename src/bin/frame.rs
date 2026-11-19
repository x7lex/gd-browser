// Headless frame dump: render GRIEF at a chosen sim tick to /tmp/opencode/frame.png
// using the player's Geometry Dash art (gdrust::assets), for visual inspection.
// usage: frame [tick] [outdir]
use gdrust::bot::gdr2::parse_gdr2;
use gdrust::bot::Playback;
use gdrust::level::parse_level;
use gdrust::render::Renderer;
use gdrust::sim::objects::ObjectTable;
use gdrust::sim::{InputState, Sim};
use tiny_skia::Pixmap;

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let tick: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(200);
    let level_id: u32 = std::env::var("FRAME_LEVEL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(149992717);
    let mut level = parse_level(
        &std::fs::read_to_string(format!("{manifest}/levels/{level_id}.gmd")).unwrap(),
    );
    level.id = level_id;
    let table =
        ObjectTable::load(std::path::Path::new(&format!("{manifest}/data/objects.json"))).unwrap();
    let m = parse_gdr2(
        &std::fs::read(format!("{manifest}/macros/Cerealmilkbowl-GRIEF-xdBot.gdr2")).unwrap(),
    )
    .unwrap();
    // another level: its own macro, the cached solve, or a fresh solve
    let m = if m.level_id == level_id {
        m
    } else {
        let t = std::time::Instant::now();
        let r = gdrust::bot::auto::macro_for(&std::path::Path::new(manifest).join("macros"), &level, &table, |p| eprint!("\rsolving {p:5.1}%"));
        eprintln!("\nmacro in {:.1}s", t.elapsed().as_secs_f64());
        r.unwrap_or_else(|(m, p)| {
            eprintln!("couldn't solve past {p:.1}%");
            m
        })
    };
    let assets = gdrust::assets::Assets::fs(std::path::Path::new(&gdrust::assets::find_or_ask()));
    let mut r = Renderer::new(&assets);

    let mut sim = Sim::new(&level, &table);
    // follow the recorded real path (frame fixes) unless FRAME_PURE is set
    let pure = std::env::var("FRAME_PURE").is_ok() || m.frames.is_empty();
    sim.follow = !pure;
    let frames = m.frames.clone();
    let frames_p2 = m.frames_p2.clone();
    let mut pb = Playback::new(m);
    let t_sim = std::time::Instant::now();
    for k in 1..=tick as usize {
        if sim.state == gdrust::sim::RunState::Dead {
            break;
        }
        let (p1, p2) = pb.step();
        if pb.stalled() {
            continue;
        }
        let before = (sim.players[0].mode, sim.players[0].mini, sim.players[0].grav_flip);
        sim.step_single(InputState { hold: p2, hold_p2: p1 });
        if let Some(f) = frames.get(k).filter(|_| !pure) {
            sim.apply_fix(f[0] as f64, f[1] as f64, f[2] as f64);
        }
        if let Some(f) = frames_p2.get(k).filter(|_| !pure) {
            sim.apply_fix_p2(f[0] as f64, f[1] as f64, f[2] as f64);
        }
        let after = (sim.players[0].mode, sim.players[0].mini, sim.players[0].grav_flip);
        // FRAME_TRACE: "tick x progress" every 24 ticks (match against a video's %)
        if std::env::var("FRAME_TRACE").is_ok() && k % 24 == 0 {
            println!("TRACE {k} {:.0} {:.3} {:.0}", sim.players[0].x, sim.progress(), sim.players[0].y);
        }
        // draw the last ~100 frames like the demo does, so particles/effects are live in the shot
        if k % 4 == 0 && k + 400 > tick as usize && std::env::var("FRAME_SWEEP").is_err() {
            r.draw(&level, &table, &sim);
        }
        if std::env::var("FRAME_MODES").is_ok() && before != after {
            println!("MODE tick {k} x {:.0} y {:.0} {:?} -> {:?}", sim.players[0].x, sim.players[0].y, before, after);
        }
    }
    if let Ok(n) = std::env::var("FRAME_SWEEP").map(|v| v.parse::<usize>().unwrap_or(600)) {
        // perf sweep: play on like the demo (4 ticks per frame), time each draw
        let mut times = Vec::new();
        let hash_frames = std::env::var("FRAME_HASH").is_ok();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut k = tick as usize;
        for _ in 0..n {
            for _ in 0..4 {
                k += 1;
                let (p1, p2) = pb.step();
                if pb.stalled() {
                    continue;
                }
                sim.step_single(InputState { hold: p2, hold_p2: p1 });
                if let Some(f) = frames.get(k).filter(|_| !pure) {
                    sim.apply_fix(f[0] as f64, f[1] as f64, f[2] as f64);
                }
                if let Some(f) = frames_p2.get(k).filter(|_| !pure) {
                    sim.apply_fix_p2(f[0] as f64, f[1] as f64, f[2] as f64);
                }
            }
            let t = std::time::Instant::now();
            let px = r.draw(&level, &table, &sim);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            // FRAME_HASH: one hash over every frame, to prove a speed-up changes no pixel
            if hash_frames {
                use std::hash::{Hash, Hasher};
                px.hash(&mut hasher);
                if std::env::var("FRAME_HASH").as_deref() == Ok("each") { println!("fh {}", hasher.finish()); }
                if let Ok(d) = std::env::var("FRAME_DUMP") {
                    if d.parse::<usize>().ok() == Some(times.len()) { std::fs::write(format!("{d}.rgba"), &px).unwrap(); }
                }
            }
        }
        if hash_frames {
            use std::hash::Hasher;
            println!("frames hash {:016x}", hasher.finish());
        }
        let first = times[0];
        let mut s = times[1..].to_vec(); // frame 0 loads the level's sprites
        println!("first frame {first:.0}ms; p99 {:.1}", { let mut t = s.clone(); t.sort_by(f64::total_cmp); t[t.len() * 99 / 100] });
        s.sort_by(f64::total_cmp);
        let avg = s.iter().sum::<f64>() / s.len() as f64;
        println!("sweep {n} frames: avg {avg:.1}ms p50 {:.1} p95 {:.1} max {:.1} | >33ms: {}",
            s[s.len() / 2], s[s.len() * 95 / 100], s[s.len() - 1], s.iter().filter(|&&t| t > 33.0).count());
        println!("players {}", sim.players.len());
        return;
    }
    let n_sim = t_sim.elapsed();
    if std::env::var("FRAME_SIMTIME").is_ok() {
        println!("sim: {tick} ticks in {n_sim:?} = {:.3} ms/tick ({:.0}% of real time)", n_sim.as_secs_f64() * 1000.0 / tick as f64, n_sim.as_secs_f64() / (tick as f64 / 240.0) * 100.0);
    }
    let out = std::env::var("FRAME_OUT").unwrap_or_else(|_| "/tmp/opencode/frame.png".into());
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let t0 = std::time::Instant::now();
    let mut rgba = Vec::new();
    for _ in 0..n {
        rgba = r.draw(&level, &table, &sim);
    }
    let per = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("render avg {per:.1} ms/frame over {n}");

    let pm = Pixmap::from_vec(
        rgba,
        tiny_skia::IntSize::from_wh(gdrust::render::FRAME_W, gdrust::render::FRAME_H).unwrap(),
    )
    .unwrap();
    std::fs::write(&out, pm.encode_png().unwrap()).unwrap();
    println!("wrote {out} at tick {tick} (x={:.0}, y={:.0}, state={:?}, mode={:?}, hidden={}, options={:b}, song={:?})", sim.players[0].x, sim.players[0].y, sim.state, sim.players[0].mode, sim.player_hidden, sim.options, sim.song_pos());
    for (i, p) in sim.players.iter().enumerate().skip(1) {
        println!("  player {} x={:.0} y={:.0} mode={:?} flip={}", i + 1, p.x, p.y, p.mode, p.grav_flip);
    }
}
