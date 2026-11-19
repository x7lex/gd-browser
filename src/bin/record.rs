//! Offline render of the GRIEF macro to an MP4 with the level's song.
//!
//! Same replay as `demo` (macro inputs + xdBot frame fixes), but paced by
//! game time, not the wall clock: every video frame is exactly 240/FPS
//! sim ticks, so the video lines up with the song however slow rendering is.
//! Usage: `record [out.mp4]` (default `grief.mp4`), RECORD_FPS (default 60).
//! RECORD_LEVEL=<id>: another level from levels/ (song from its .json), played
//! by its real macro if macros/ has one, else the solver's (see bot::auto).
//!
//! The video is written first; a second ffmpeg pass lays the song under it,
//! cut at every song-trigger cue (the sim's `song`), so it plays as in GD.

use gdrust::bot::gdr2::parse_gdr2;
use gdrust::bot::Playback;
use gdrust::level::parse_level;
use gdrust::render::{Renderer, FRAME_H, FRAME_W};
use gdrust::sim::objects::ObjectTable;
use gdrust::sim::{InputState, RunState, Sim};
use std::io::Write;
use std::process::{Command, Stdio};

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let out = std::env::args().nth(1).unwrap_or_else(|| "grief.mp4".into());
    let fps: usize = std::env::var("RECORD_FPS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    assert!(240 % fps == 0, "RECORD_FPS must divide 240");
    let table = ObjectTable::load(std::path::Path::new(&format!("{manifest}/data/objects.json"))).unwrap();
    let (level, m, song) = match std::env::var("RECORD_LEVEL").ok().and_then(|v| v.parse::<u32>().ok()) {
        Some(id) => {
            let mut level = parse_level(&std::fs::read_to_string(format!("{manifest}/levels/{id}.gmd")).expect("levels/<id>.gmd"));
            level.id = id;
            let meta: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(format!("{manifest}/levels/{id}.json")).expect("levels/<id>.json")).expect("meta json");
            let song = meta["song"].as_str().map(|s| format!("{manifest}/{s}")).expect("level has no song");
            let macros = std::path::Path::new(manifest).join("macros");
            let m = match gdrust::bot::auto::macro_for(&macros, &level, &table, |p| eprint!("\rsolving {p:.0}%   ")) {
                Ok(m) => m,
                Err((m, p)) => {
                    eprintln!("\nsolver stopped at {p:.1}%: recording the partial run");
                    m
                }
            };
            (level, m, song)
        }
        None => {
            let mut level = parse_level(&std::fs::read_to_string(format!("{manifest}/levels/149992717.gmd")).unwrap());
            level.id = 149992717;
            let m = parse_gdr2(&std::fs::read(format!("{manifest}/macros/Cerealmilkbowl-GRIEF-xdBot.gdr2")).unwrap())
                .expect("gdr2 parse");
            // GRIEF = Newgrounds 482872 (tools/fetch_songs.sh); level song offset kA13 = 0
            (level, m, format!("{manifest}/audio/482872.mp3"))
        }
    };
    let assets = gdrust::assets::Assets::fs(std::path::Path::new(&gdrust::assets::find_or_ask()));
    let mut renderer = Renderer::new(&assets);

    let video = format!("{out}.video.mp4");
    let mut ff = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .args(["-s", &format!("{FRAME_W}x{FRAME_H}"), "-r", &fps.to_string(), "-i", "-"])
        .args(["-c:v", "libx264", "-preset", "medium", "-crf", "18", "-pix_fmt", "yuv420p", &video])
        .stdin(Stdio::piped())
        .spawn()
        .expect("ffmpeg spawn");
    let mut pipe = ff.stdin.take().expect("piped stdin");

    let mut sim = Sim::new(&level, &table);
    // frame fixes keep a real recording on GD's path; solver macros run pure
    sim.follow = !m.frames.is_empty();
    let mut pb = Playback::new(m.clone());
    let ticks_per_frame = 240 / fps;
    // whole macro, then 1 s past the end
    let total = m.frames.len().max((m.duration as f64 * 240.0) as usize) + 240;
    let t0 = std::time::Instant::now();
    let mut tick = 0usize;
    // (video seconds, song seconds) at each song cue
    let mut cues = vec![(0.0, sim.song_pos().1)];
    let mut last_cue = sim.song;
    while tick < total {
        for _ in 0..ticks_per_frame {
            tick += 1;
            let (p1, p2) = pb.step();
            if pb.stalled() || sim.state != RunState::Playing {
                continue;
            }
            sim.step_single(InputState { hold: p2, hold_p2: p1 });
            if let Some(f) = m.frames.get(tick) {
                sim.apply_fix(f[0] as f64, f[1] as f64, f[2] as f64);
            }
            if let Some(f) = m.frames_p2.get(tick) {
                sim.apply_fix_p2(f[0] as f64, f[1] as f64, f[2] as f64);
            }
        }
        if sim.song != last_cue {
            last_cue = sim.song;
            cues.push((tick as f64 / 240.0, sim.song_pos().1));
        }
        if pipe.write_all(&renderer.draw(&level, &table, &sim)).is_err() {
            break; // ffmpeg exited; its own error is on stderr
        }
        if tick % (240 * 10) < ticks_per_frame {
            eprint!("\r{:5.1}% rendered ({:.0}s elapsed)", 100.0 * tick as f64 / total as f64, t0.elapsed().as_secs_f64());
        }
    }
    drop(pipe);
    let mut ok = ff.wait().map(|s| s.success()).unwrap_or(false);
    if ok {
        ok = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-i", &video, "-i", &song, "-filter_complex", &song_filter(&cues)])
            .args(["-map", "0:v", "-map", "[a]", "-c:v", "copy", "-c:a", "aac", "-b:a", "192k"])
            .args(["-shortest", "-movflags", "+faststart", &out])
            .status()
            .is_ok_and(|s| s.success());
        let _ = std::fs::remove_file(&video);
    }
    eprintln!("\n{} {out} ({:.1}s video, {:.0}s to render)", if ok { "wrote" } else { "ffmpeg FAILED for" }, tick as f64 / 240.0, t0.elapsed().as_secs_f64());
}

/// ffmpeg filter: song input 1 cut into one piece per cue, joined as `[a]`.
/// Each piece starts at its cue's song time and lasts until the next cue.
fn song_filter(cues: &[(f64, f64)]) -> String {
    let mut f = String::new();
    for (i, &(at, from)) in cues.iter().enumerate() {
        let end = cues.get(i + 1).map_or(String::new(), |n| format!(":end={:.4}", from + n.0 - at));
        f += &format!("[1:a]atrim=start={from:.4}{end},asetpts=PTS-STARTPTS[s{i}];");
    }
    f += &(0..cues.len()).map(|i| format!("[s{i}]")).collect::<String>();
    f + &format!("concat=n={}:v=0:a=1[a]", cues.len())
}

#[cfg(test)]
mod tests {
    #[test]
    fn song_filter_cuts_at_cues() {
        assert_eq!(
            super::song_filter(&[(0.0, 0.0), (10.0, 9.5)]),
            "[1:a]atrim=start=0.0000:end=10.0000,asetpts=PTS-STARTPTS[s0];\
             [1:a]atrim=start=9.5000,asetpts=PTS-STARTPTS[s1];[s0][s1]concat=n=2:v=0:a=1[a]"
        );
    }
}
