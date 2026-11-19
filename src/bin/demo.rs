//! 16:9 visual demo: a level's macro replayed in a local window.
//!
//! Usage: `demo [level id] [macro url]` (default GRIEF, 149992717). A level
//! not in levels/ is downloaded with its song by tools/fetch_level.py (the
//! optional macro url is saved as macros/<id>.gdr2). The macro is any
//! macros/*.gdr2 recorded on that level id (xdBot ones with frame fixes
//! first), else the engine's own solve (bot::auto, cached as
//! macros/<id>.auto.json).
//!
//! Level queue: type a level id (and optionally a macro url) in the
//! terminal and press Enter. It is downloaded and solved in the background
//! while the current level keeps playing, then the window switches to it.
//! A level the solver can't finish is reported and not switched to.
//!
//! Insert opens a menu in the window: type a level id and Enter to load it,
//! Tab switches Auto (the macro plays) / Manual (you play: Space, Up, W or
//! the left mouse button; deaths restart). Insert or Escape closes it.
//!
//! Art from the player's Geometry Dash install (see gdrust::assets: asked
//! for once on Linux/macOS, Steam's folder on Windows; DEMO_ASSETS overrides),
//! macro p2 drives sim p0 (SWAP wiring, same as calibrate). R restarts the
//! attempt, Escape/Q quits. Uncapped frame rate; the sim advances 240Hz
//! ticks by wall-clock time, and prints the run time on completion. Progress/attempt live in the window title.
//!
//! The player follows xdBot's recorded real positions (frame fixes), as
//! xdBot does in-game, so the replay runs the whole level; DEMO_PURE=1
//! replays inputs only (pure sim physics, dies where the sim drifts).
//!
//! The song and GD's death sound play in-process (rodio) from the sim's song
//! cue (level offset, then song triggers); DEMO_MUTE=1 turns them off.

use gdrust::bot::Playback;
use gdrust::level::parse_level;
use gdrust::render::{Renderer, FRAME_H, FRAME_W};
use gdrust::sim::objects::ObjectTable;
use gdrust::sim::{InputState, Sim};
use minifb::{Key, Window, WindowOptions};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

/// A level ready to play.
struct Loaded {
    level: gdrust::level::Level,
    name: String,
    song: Option<String>,
    m: gdrust::bot::gdr2::Macro,
}

/// Level + metadata, downloading them (and the macro url, if given) when missing.
fn load_level(manifest: &str, id: u32, macro_url: Option<&str>) -> Result<(gdrust::level::Level, String, Option<String>), String> {
    let (gmd, meta) = (format!("{manifest}/levels/{id}.gmd"), format!("{manifest}/levels/{id}.json"));
    let have_macro = std::path::Path::new(&format!("{manifest}/macros/{id}.gdr2")).exists();
    if !std::path::Path::new(&gmd).exists() || !std::path::Path::new(&meta).exists() || (macro_url.is_some() && !have_macro) {
        // Python's name differs by OS (Windows: python / py)
        let ok = ["python3", "python", "py"].iter().find_map(|py| {
            Command::new(py).args(["-I", &format!("{manifest}/tools/fetch_level.py"), &id.to_string()]).args(macro_url).status().ok()
        });
        let ok = ok.is_some_and(|s| s.success());
        if !ok {
            return Err(format!("could not download level {id}"));
        }
    }
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
    let mut level = parse_level(&read(&gmd)?);
    level.id = id;
    let meta: serde_json::Value = serde_json::from_str(&read(&meta)?).map_err(|e| format!("level {id} meta: {e}"))?;
    let name = meta["name"].as_str().unwrap_or("level").to_string();
    let song = meta["song"].as_str().map(|s| format!("{manifest}/{s}"));
    Ok((level, name, song))
}

/// Download (if needed) and find or solve the macro. Err: what went wrong,
/// plus the level with its partial solve when the solver gave up.
fn prepare(
    manifest: &str,
    table: &ObjectTable,
    id: u32,
    macro_url: Option<&str>,
    status: &dyn Fn(String),
) -> Result<Loaded, (String, Option<Loaded>)> {
    let (level, name, song) = load_level(manifest, id, macro_url).map_err(|e| (e, None))?;
    let t = Instant::now();
    let macros = std::path::Path::new(manifest).join("macros");
    let solved = gdrust::bot::auto::macro_for(&macros, &level, table, |p| status(format!("solving {name} {p:.0}%")));
    status(String::new());
    match solved {
        Ok(m) => {
            if m.bot_name == "gd-rust auto" {
                println!("{name}: engine solve ready ({:.1}s)", t.elapsed().as_secs_f64());
            }
            Ok(Loaded { level, name, song, m })
        }
        Err((m, p)) => {
            Err((format!("couldn't solve {name} past {p:.1}% ({:.1}s)", t.elapsed().as_secs_f64()), Some(Loaded { level, name, song, m })))
        }
    }
}

/// A decoded sound, all in memory so playback can start at any sample.
struct Pcm {
    samples: Vec<i16>,
    channels: u16,
    rate: u32,
}

/// Plays a shared Pcm from one interleaved sample index on.
struct PcmFrom {
    pcm: Arc<Pcm>,
    at: usize,
}

impl Iterator for PcmFrom {
    type Item = i16;
    fn next(&mut self) -> Option<i16> {
        let s = self.pcm.samples.get(self.at).copied();
        self.at += 1;
        s
    }
}

impl rodio::Source for PcmFrom {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        self.pcm.channels
    }
    fn sample_rate(&self) -> u32 {
        self.pcm.rate
    }
    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

/// Interleaved sample index of `secs` into a song (frame-aligned).
fn sample_at(pcm: &Pcm, secs: f64) -> usize {
    (secs.max(0.0) * pcm.rate as f64) as usize * pcm.channels as usize
}

/// Song and sound effects through the system audio device (rodio), from
/// the sim's cue. Songs decode once, in the background, then any restart
/// (new attempt, song trigger, drift from the sim after a hitch) starts
/// at the exact sample with no delay. No device: silent.
/// shortcut: a restart is a hard cut (GD fades), fine for a local preview.
struct Music {
    /// audio/ (downloaded songs) and GD's Resources/ (official songs, sfx)
    dirs: [std::path::PathBuf; 2],
    /// the level's own song (sim song id 0)
    level_song: Option<String>,
    out: Option<(rodio::OutputStream, rodio::OutputStreamHandle)>,
    sink: Option<rodio::Sink>,
    /// (attempt, sim cue) it plays, and when/where it started
    cue: Option<((u32, (u32, f64, u64)), Instant, f64)>,
    /// decoded sounds by path; None while decoding or when it failed
    pcm: Arc<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Option<Arc<Pcm>>>>>,
}

impl Music {
    fn new(audio: std::path::PathBuf, resources: std::path::PathBuf, level_song: Option<String>) -> Music {
        let out = rodio::OutputStream::try_default().map_err(|e| eprintln!("no audio output: {e}")).ok();
        Music { dirs: [audio, resources], level_song, out, sink: None, cue: None, pcm: Default::default() }
    }

    /// A sound file: as given, else by name in audio/ then Resources/.
    fn find(&self, file: &str) -> Option<std::path::PathBuf> {
        let p = std::path::Path::new(file);
        let name = p.file_name()?;
        std::iter::once(p.to_path_buf()).chain(self.dirs.iter().map(|d| d.join(name))).find(|c| c.exists())
    }

    /// The decoded sound, or None while it decodes (started here, on a thread).
    fn decoded(&self, path: &std::path::Path) -> Option<Arc<Pcm>> {
        let mut map = self.pcm.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = map.get(path) {
            return p.clone();
        }
        map.insert(path.to_path_buf(), None);
        let (pcm, path) = (self.pcm.clone(), path.to_path_buf());
        std::thread::spawn(move || {
            let dec = std::fs::File::open(&path).map_err(|e| e.to_string()).and_then(|f| {
                rodio::Decoder::new(std::io::BufReader::new(f)).map_err(|e| e.to_string())
            });
            match dec {
                Ok(d) => {
                    use rodio::Source;
                    let (channels, rate) = (d.channels(), d.sample_rate());
                    let p = Pcm { samples: d.collect(), channels, rate };
                    pcm.lock().unwrap_or_else(|e| e.into_inner()).insert(path, Some(Arc::new(p)));
                }
                Err(e) => eprintln!("can't play {}: {e}", path.display()),
            }
        });
        None
    }

    fn sync(&mut self, sim: &Sim) {
        let (id, pos) = sim.song_pos();
        let key = (sim.attempt, sim.song);
        let drift = self.cue.as_ref().map_or(f64::MAX, |(_, t, p)| (p + t.elapsed().as_secs_f64() - pos).abs());
        if self.cue.as_ref().is_some_and(|c| c.0 == key) && drift < 0.25 {
            return;
        }
        // id 0 = the level's song; song triggers name Newgrounds ids
        let file = if id == 0 { self.level_song.clone() } else { Some(format!("{id}.mp3")) };
        let Some(path) = file.and_then(|f| self.find(&f)) else { return };
        let (Some(pcm), Some((_, handle))) = (self.decoded(&path), self.out.as_ref()) else { return };
        let Ok(sink) = rodio::Sink::try_new(handle) else { return };
        self.stop();
        sink.append(PcmFrom { at: sample_at(&pcm, pos), pcm });
        self.sink = Some(sink);
        self.cue = Some((key, Instant::now(), pos));
    }

    /// Fire-and-forget sound effect (GD's own, from Resources/).
    fn sfx(&self, name: &str) {
        use rodio::Source;
        let (Some(path), Some((_, handle))) = (self.find(name), self.out.as_ref()) else { return };
        if let Some(pcm) = self.decoded(&path) {
            let _ = handle.play_raw(PcmFrom { pcm, at: 0 }.convert_samples());
        }
    }

    fn stop(&mut self) {
        if let Some(s) = self.sink.take() {
            s.stop();
        }
        self.cue = None;
    }
}

/// A typed digit (top row or keypad) for the level id field.
fn key_digit(k: Key) -> Option<char> {
    use Key::*;
    let d = match k {
        Key0 | NumPad0 => 0,
        Key1 | NumPad1 => 1,
        Key2 | NumPad2 => 2,
        Key3 | NumPad3 => 3,
        Key4 | NumPad4 => 4,
        Key5 | NumPad5 => 5,
        Key6 | NumPad6 => 6,
        Key7 | NumPad7 => 7,
        Key8 | NumPad8 => 8,
        Key9 | NumPad9 => 9,
        _ => return None,
    };
    char::from_digit(d, 10)
}

fn main() {
    // shipped build: levels/ macros/ data/ next to the exe; dev: the source tree
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let manifest: &'static str = match exe_dir.filter(|d| d.join("data/objects.json").exists()) {
        Some(d) => Box::leak(d.to_string_lossy().into_owned().into_boxed_str()),
        None => env!("CARGO_MANIFEST_DIR"),
    };
    let args: Vec<String> = std::env::args().collect();
    // the player's own Geometry Dash art (RobTop's, never shipped): asked
    // for on the first run, before anything slow happens
    let assets: std::path::PathBuf = gdrust::assets::find_or_ask();
    let art = gdrust::assets::Assets::fs(&assets);
    let id: u32 = args.get(1).map(|a| a.parse().expect("level id must be a number")).unwrap_or(149992717);
    let table =
        ObjectTable::load(std::path::Path::new(&format!("{manifest}/data/objects.json"))).unwrap();
    // the first level: solved before the window opens; a partial solve
    // still plays (up to where the solver got)
    let last = std::cell::RefCell::new(String::new());
    let print = |s: String| {
        if !s.is_empty() && *last.borrow() != s {
            eprint!("\r{s}   ");
            *last.borrow_mut() = s;
        }
    };
    let first = match prepare(manifest, &table, id, args.get(2).map(|s| s.as_str()), &print) {
        Ok(l) => l,
        Err((e, Some(partial))) => {
            eprintln!("\n{e}: playing the partial run");
            partial
        }
        Err((e, None)) => panic!("{e}"),
    };
    let Loaded { mut level, mut name, song: level_song, mut m } = first;

    // level queue: "<id> [macro url]" lines from the terminal or the Insert
    // menu, prepared one at a time; manual ones (the menu in manual mode)
    // are not solved, the player drives
    let status = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Loaded>();
    let (req_tx, req_rx) = std::sync::mpsc::channel::<(String, bool)>();
    {
        let req_tx = req_tx.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lines().map_while(Result::ok) {
                if req_tx.send((line, false)).is_err() {
                    return;
                }
            }
        });
    }
    {
        let status = status.clone();
        let manifest = manifest.to_string();
        std::thread::spawn(move || {
            let table = ObjectTable::load(std::path::Path::new(&format!("{manifest}/data/objects.json"))).expect("object table");
            let set = |s: String| *status.lock().unwrap_or_else(|e| e.into_inner()) = s;
            println!("type a level id (+ optional macro url) and Enter to queue it, or press Insert in the window");
            for (line, manual) in req_rx {
                let mut words = line.split_whitespace();
                let Some(id) = words.next().and_then(|w| w.parse::<u32>().ok()) else {
                    if !line.trim().is_empty() {
                        println!("not a level id: {line}");
                    }
                    continue;
                };
                set(format!("fetching {id}"));
                let got = if manual {
                    load_level(&manifest, id, None)
                        .map(|(level, name, song)| Loaded { level, name, song, m: Default::default() })
                        .map_err(|e| (e, None))
                } else {
                    prepare(&manifest, &table, id, words.next(), &set)
                };
                match got {
                    Ok(l) => {
                        println!("{}: ready, switching", l.name);
                        if ready_tx.send(l).is_err() {
                            return; // window closed
                        }
                    }
                    Err((e, _)) => println!("{e}: keeping the current level"),
                }
                set(String::new());
            }
        });
    }

    let mut renderer = Renderer::new(&art);

    let mut sim = Sim::new(&level, &table);
    let mut pure = std::env::var("DEMO_PURE").is_ok() || m.frames.is_empty();
    sim.follow = !pure;
    let mut score = gdrust::bot::auto::difficulty(&level, &table).0;
    println!("{name}: difficulty {score:.2}, misses a click in {:.0}% of attempts", 100.0 * gdrust::bot::auto::fail_chance(score));
    let mut cur = gdrust::bot::auto::attempt(&m, score, seed());
    let mut pb = Playback::new(cur.clone());
    let mut tick = 0usize;

    let mut window = Window::new(
        &format!("{name} — gd-rust demo"),
        FRAME_W as usize,
        FRAME_H as usize,
        WindowOptions::default(),
    )
    .expect("window");
    // uncapped: the sim advances by wall-clock time (240Hz ticks), the
    // renderer draws as fast as it can
    window.set_target_fps(0);
    let tick_dt = 1.0 / 240.0;
    let mut acc = 0.0f64;
    let mut dropped = 0.0f64;
    let mut stalls = 0usize;
    let mut clock = std::time::Instant::now();
    let mut run_start = std::time::Instant::now();
    let mut frames_drawn = 0u64;
    let mut reported = false;
    let mut fb = vec![0u32; FRAME_W as usize * FRAME_H as usize];
    let mut last = std::time::Instant::now();
    let mut fps_smooth = 60.0f32;
    let mut music = Music::new(format!("{manifest}/audio").into(), assets.clone(), level_song);
    let mute = std::env::var("DEMO_MUTE").is_ok();
    let mut prof = std::env::var("DEMO_PROF").is_ok().then_some([0.0f64; 5]);
    // Insert menu: the level id being typed (None: closed); manual: the
    // player clicks (Space / Up / W / left mouse) instead of the macro
    let mut menu: Option<String> = None;
    let mut manual = false;

    while window.is_open() {
        // a queued level finished preparing: switch to it now
        if let Ok(next) = ready_rx.try_recv() {
            (level, name, m) = (next.level, next.name, next.m);
            music.stop();
            music.level_song = next.song;
            sim = Sim::new(&level, &table);
            score = gdrust::bot::auto::difficulty(&level, &table).0;
            println!("{name}: difficulty {score:.2}, misses a click in {:.0}% of attempts", 100.0 * gdrust::bot::auto::fail_chance(score));
            pure = std::env::var("DEMO_PURE").is_ok() || m.frames.is_empty();
            sim.follow = !pure;
            pb = { cur = gdrust::bot::auto::attempt(&m, score, seed()); Playback::new(cur.clone()) };
            tick = 0;
            run_start = std::time::Instant::now();
            frames_drawn = 0;
            reported = false;
            dropped = 0.0;
            stalls = 0;
        }
        // as many 240Hz ticks as real time elapsed (capped so a hitch does
        // not snowball); the macro advances one frame per tick
        let now = std::time::Instant::now();
        acc += (now - clock).as_secs_f64();
        if acc > 0.25 {
            dropped += acc - 0.25; // a hitch: skip ahead rather than snowball
            acc = 0.25;
        }
        clock = now;
        let held = menu.is_none()
            && ([Key::Space, Key::Up, Key::W].iter().any(|&k| window.is_key_down(k)) || window.get_mouse_down(minifb::MouseButton::Left));
        while acc >= tick_dt {
            acc -= tick_dt;
            // death: restart immediately (the sim's internal respawn would
            // otherwise resume the macro at the death frame, not frame 0)
            if sim.state == gdrust::sim::RunState::Dead || (reported && sim.progress() >= 100.0) {
                if sim.state == gdrust::sim::RunState::Dead && !mute {
                    music.sfx("explode_11.ogg"); // GD's death sound
                }
                sim.reset();
                pb = { cur = gdrust::bot::auto::attempt(&m, score, seed()); Playback::new(cur.clone()) };
                tick = 0;
                run_start = std::time::Instant::now();
                frames_drawn = 0;
                reported = false;
                dropped = 0.0;
                stalls = 0;
            }
            if manual {
                // the player's own clicks, our physics alone: deaths count
                tick += 1;
                sim.follow = false;
                sim.step_single(InputState { hold: held, hold_p2: held });
                continue;
            }
            let (p1, p2) = pb.step();
            tick += 1;
            if pb.stalled() {
                stalls += 1;
                continue; // the real game ran no physics this frame
            }
            // deaths are off only while GD's recorded path still pins the
            // player: past a missed click (attempt() cuts the frames there)
            // the run is our physics alone and must be able to die
            sim.follow = !pure && tick < cur.frames.len();
            sim.step_single(InputState { hold: p2, hold_p2: p1 });
            if let Some(f) = cur.frames.get(tick).filter(|_| !pure) {
                sim.apply_fix(f[0] as f64, f[1] as f64, f[2] as f64);
            }
            if let Some(f) = cur.frames_p2.get(tick).filter(|_| !pure) {
                sim.apply_fix_p2(f[0] as f64, f[1] as f64, f[2] as f64);
            }
        }
        let menu_was_open = menu.is_some();
        let mut restart = false;
        if window.is_key_pressed(Key::Insert, minifb::KeyRepeat::No) {
            menu = if menu_was_open { None } else { Some(String::new()) };
        }
        if let Some(code) = menu.as_mut() {
            let mut close = false;
            for k in window.get_keys_pressed(minifb::KeyRepeat::Yes) {
                match k {
                    Key::Backspace => drop(code.pop()),
                    Key::Escape => close = true,
                    Key::Tab => {
                        manual = !manual;
                        restart = true;
                        // a level loaded for manual play has no macro yet: solve it
                        if !manual && m.inputs.is_empty() {
                            let _ = req_tx.send((level.id.to_string(), false));
                        }
                    }
                    Key::Enter | Key::NumPadEnter if !code.is_empty() => {
                        let _ = req_tx.send((code.clone(), manual));
                        close = true;
                    }
                    k => {
                        if let Some(d) = key_digit(k).filter(|_| code.len() < 12) {
                            code.push(d);
                        }
                    }
                }
            }
            if close {
                menu = None;
            }
        }
        if restart || (!menu_was_open && window.is_key_pressed(Key::R, minifb::KeyRepeat::No)) {
            sim.reset();
            pb = { cur = gdrust::bot::auto::attempt(&m, score, seed()); Playback::new(cur.clone()) };
            tick = 0;
            run_start = std::time::Instant::now();
            frames_drawn = 0;
            dropped = 0.0;
            stalls = 0;
        }
        if !reported && (sim.progress() >= 100.0 || sim.state == gdrust::sim::RunState::Complete) {
            // run timing: wall clock vs the macro's in-game time
            let wall = run_start.elapsed().as_secs_f64();
            println!(
                "level complete: wall {wall:.2}s ({dropped:.2}s dropped by hitches), macro {:.2}s ({tick} frames, {stalls} stall), avg {:.0} fps",
                tick as f64 / 240.0,
                frames_drawn as f64 / wall
            );
            reported = true;
        }
        if !menu_was_open && (window.is_key_pressed(Key::Escape, minifb::KeyRepeat::No) || window.is_key_pressed(Key::Q, minifb::KeyRepeat::No)) {
            break;
        }

        if !mute && sim.state == gdrust::sim::RunState::Playing && !reported {
            music.sync(&sim);
        } else {
            music.stop();
        }
        frames_drawn += 1;
        let dt = last.elapsed().as_secs_f32();
        last = std::time::Instant::now();
        if dt > 0.0 {
            fps_smooth = fps_smooth * 0.9 + (1.0 / dt) * 0.1;
        }
        renderer.set_fps(fps_smooth as u32);
        let next = status.lock().map(|s| s.clone()).unwrap_or_default();
        renderer.set_menu(menu.as_ref().map(|code| {
            vec![
                format!("Level ID: {code}_"),
                format!("Mode: {} (Tab)", if manual { "Manual" } else { "Auto" }),
                if next.is_empty() { "Enter: load   Insert: close".to_string() } else { next.clone() },
            ]
        }));
        // draw this frame on a worker while the window shows the last one:
        // a frame costs max(draw, present), not their sum (one frame later)
        let t_draw = std::time::Instant::now();
        let (rgba, present_ms) = std::thread::scope(|sc| {
            let job = sc.spawn(|| renderer.draw(&level, &table, &sim));
            let t = std::time::Instant::now();
            let _ = window.update_with_buffer(&fb, FRAME_W as usize, FRAME_H as usize);
            let present_ms = t.elapsed().as_secs_f64() * 1000.0;
            (job.join().expect("render thread"), present_ms)
        });
        let t_conv = std::time::Instant::now();
        {
            // renderer output is RGBA; minifb wants 0RGB
            use rayon::prelude::*;
            let w = FRAME_W as usize;
            fb.par_chunks_mut(w * 16).zip(rgba.par_chunks(w * 16 * 4)).for_each(|(d, s)| {
                for (px, c) in d.iter_mut().zip(s.chunks_exact(4)) {
                    *px = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
                }
            });
        }
        let t_title = std::time::Instant::now();
        window.set_title(&format!(
            "{name} — {} — attempt {} — {:.2}% — x {:.0}{}  [Insert menu, R restart, Q quit]",
            if manual { "manual" } else { "auto" },
            sim.attempt,
            sim.progress(),
            sim.players.first().map(|p| p.x).unwrap_or(0.0),
            if next.is_empty() { String::new() } else { format!(" — next: {next}") }
        ));
        if let Some(p) = prof.as_mut() {
            // DEMO_PROF: where a frame's wall time goes (sim, draw alongside
            // present, convert, title; present alone shown for reference)
            let e = [t_draw - now, t_conv - t_draw, t_title - t_conv, t_title.elapsed()].map(|d| d.as_secs_f64() * 1000.0);
            for (a, d) in p.iter_mut().zip(e.into_iter().chain([present_ms])) { *a += d; }
            if frames_drawn % 300 == 0 {
                println!("prof ms/frame: sim {:.2} draw||present {:.2} conv {:.2} title {:.2} (present {:.2}) | {:.0} fps", p[0] / 300.0, p[1] / 300.0, p[2] / 300.0, p[3] / 300.0, p[4] / 300.0, 300.0 / (p[..4].iter().sum::<f64>() / 1000.0));
                *p = [0.0; 5];
            }
        }
    }
    music.stop();
}

/// a fresh seed per attempt, so solver macros sway differently each time
fn seed() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_starts_at_the_frame_for_its_cue() {
        let pcm = Arc::new(Pcm { samples: (0..20).collect(), channels: 2, rate: 4 });
        // 1.5 s at 4 Hz stereo = frame 6 = sample 12; never mid-frame
        assert_eq!(sample_at(&pcm, 1.5), 12);
        assert_eq!(sample_at(&pcm, 1.6), 12);
        assert_eq!(sample_at(&pcm, -1.0), 0);
        let rest: Vec<i16> = PcmFrom { at: sample_at(&pcm, 2.0), pcm: pcm.clone() }.collect();
        assert_eq!(rest, (16..20).collect::<Vec<i16>>());
    }
}
