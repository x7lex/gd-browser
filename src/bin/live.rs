//! 24/7 TikTok LIVE loop: replays the GRIEF macro in real time and pushes
//! it through ffmpeg (vertical 720x1280, H.264 + AAC) to TikTok's RTMP ingest.
//! A viewer gift restarts the run (debounced 1 s), as does reaching the end.
//!
//! Env:
//! - `TT_RTMP_URL` + `TT_STREAM_KEY`: push target. Unset → `LIVE_OUT`.
//! - `LIVE_OUT`: file path for a local encode test (e.g. /out/test.mp4); unset → null muxer.
//! - `LIVE_SECONDS`: stop after this many seconds (encode tests). Unset → forever.
//! - `TT_USERNAME`: TikTok handle whose gifts reset the run (spawns `node bridge/index.js`).
//! - `LIVE_ROOT`: dir with levels/ data/ macros/ audio/ bridge/ (default: the crate dir).
//! - `LIVE_ASSETS`: GD Resources dir (default: gdrust::assets::find).
//!
//! Pacing is wall-clock: one frame per 1/30 s. The sim, the song and the
//! frame slot always advance together; when rendering falls behind, the last
//! frame is re-sent instead of rendering, so the stream stays real time.

use gdrust::bot::gdr2::parse_gdr2;
use gdrust::bot::Playback;
use gdrust::bridge::{BridgeEvent, GiftBridge};
use gdrust::level::parse_level;
use gdrust::render::{Renderer, FRAME_H, FRAME_W};
use gdrust::sim::objects::ObjectTable;
use gdrust::sim::{InputState, RunState, Sim};
use gdrust::stream::ffmpeg::{Streamer, AUDIO_CHANNELS, AUDIO_RATE};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const FPS: u32 = 30;
const TICKS_PER_FRAME: usize = 240 / FPS as usize;
const SAMPLES_PER_FRAME: usize = (AUDIO_RATE / FPS) as usize;
const BYTES_PER_SAMPLE: usize = 2 * AUDIO_CHANNELS as usize; // s16le

/// One video frame of PCM, the song's `frame`-th; silence past its end.
fn pcm_chunk(song: &[u8], frame: usize, out: &mut Vec<u8>) {
    let len = SAMPLES_PER_FRAME * BYTES_PER_SAMPLE;
    let start = (frame * len).min(song.len());
    let end = (start + len).min(song.len());
    out.clear();
    out.extend_from_slice(&song[start..end]);
    out.resize(len, 0);
}

/// Mix one frame's worth of a playing sound effect into `pcm` (saturating
/// s16le add) from byte `pos`; false once the effect has finished.
fn mix_sfx(pcm: &mut [u8], sfx: &[u8], pos: &mut usize) -> bool {
    for (d, s) in pcm.chunks_exact_mut(2).zip(sfx.get(*pos..).unwrap_or(&[]).chunks_exact(2)) {
        let v = i16::from_le_bytes([d[0], d[1]]).saturating_add(i16::from_le_bytes([s[0], s[1]]));
        d.copy_from_slice(&v.to_le_bytes());
    }
    *pos += pcm.len();
    *pos < sfx.len()
}

/// Decode a whole sound file to s16le PCM once (≈30 MB for a 3-minute song).
fn decode_song(path: &Path) -> Vec<u8> {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-f", "s16le", "-ar", &AUDIO_RATE.to_string(), "-ac", &AUDIO_CHANNELS.to_string(), "-"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("ffmpeg (song decode)");
    let mut pcm = Vec::new();
    child.stdout.take().expect("piped").read_to_end(&mut pcm).expect("read pcm");
    assert!(child.wait().map(|s| s.success()).unwrap_or(false), "song decode failed: {}", path.display());
    pcm
}

/// One ffmpeg process plus one writer thread per input. Separate threads
/// matter: ffmpeg holds one input back until the other catches up, so a
/// single thread blocked writing video could never deliver the audio it waits for.
struct Session {
    video: mpsc::SyncSender<Arc<Vec<u8>>>,
    video_thread: std::thread::JoinHandle<bool>,
    audio: mpsc::SyncSender<Vec<u8>>,
    fifo: PathBuf,
}

impl Session {
    fn start(out: Option<&str>, n: u32) -> std::io::Result<Session> {
        let fifo = std::env::temp_dir().join(format!("gdrust-audio-{}-{n}.pcm", std::process::id()));
        let _ = std::fs::remove_file(&fifo);
        if !Command::new("mkfifo").arg(&fifo).status()?.success() {
            return Err(std::io::Error::other("mkfifo failed"));
        }
        let mut streamer = Streamer::start(out, Some(&fifo), FRAME_W, FRAME_H, FPS)?;
        let (vtx, vrx) = mpsc::sync_channel::<Arc<Vec<u8>>>(FPS as usize);
        let video_thread = std::thread::spawn(move || {
            for f in vrx {
                if streamer.submit(&f).is_err() {
                    break;
                }
            }
            streamer.finish()
        });
        // ~2 s of audio may queue; the video pipe is the real backpressure
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(60);
        let path = fifo.clone();
        std::thread::spawn(move || {
            // shortcut: if ffmpeg dies before opening the FIFO this thread stays blocked in open(); one leaked thread per failed session
            let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&path) else { return };
            for chunk in rx {
                if f.write_all(&chunk).is_err() {
                    return;
                }
            }
        });
        Ok(Session { video: vtx, video_thread, audio: tx, fifo })
    }

    /// false once ffmpeg is gone (a writer thread exited)
    fn send(&mut self, frame: &Arc<Vec<u8>>, pcm: Vec<u8>) -> bool {
        // audio first: ffmpeg probes the PCM input before it reads video
        self.audio.send(pcm).is_ok() && self.video.send(frame.clone()).is_ok()
    }

    fn finish(self) -> bool {
        drop(self.audio); // audio thread closes the FIFO -> ffmpeg sees EOF
        drop(self.video);
        let ok = self.video_thread.join().unwrap_or(false);
        let _ = std::fs::remove_file(&self.fifo);
        ok
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("LIVE_ROOT").unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").into()));
    // headless (Docker): never asks; LIVE_ASSETS, else gdrust::assets::find()
    let assets = std::env::var("LIVE_ASSETS").map(PathBuf::from).ok().or_else(gdrust::assets::find)
        .expect("set LIVE_ASSETS to Geometry Dash's Resources folder");
    let out = match (std::env::var("TT_RTMP_URL"), std::env::var("TT_STREAM_KEY")) {
        (Ok(url), Ok(key)) if !url.is_empty() && !key.is_empty() => Some(format!("{}/{key}", url.trim_end_matches('/'))),
        _ => std::env::var("LIVE_OUT").ok().filter(|s| !s.is_empty()),
    };
    let limit = std::env::var("LIVE_SECONDS").ok().and_then(|s| s.parse::<f64>().ok());
    let target_desc = match &out {
        Some(o) if o.starts_with("rtmp") => "TikTok RTMP (key hidden)".to_string(),
        Some(o) => o.clone(),
        None => "null muxer".to_string(),
    };

    let mut level = parse_level(&std::fs::read_to_string(root.join("levels/149992717.gmd")).expect("level"));
    level.id = 149992717;
    let table = ObjectTable::load(&root.join("data/objects.json")).expect("objects.json");
    let m = parse_gdr2(&std::fs::read(root.join("macros/Cerealmilkbowl-GRIEF-xdBot.gdr2")).expect("macro"))
        .expect("gdr2 parse");
    // GRIEF = Newgrounds 482872 (tools/fetch_songs.sh); song offset kA13 = 0
    let song = decode_song(&root.join("audio/482872.mp3"));
    let mut renderer = Renderer::new(&gdrust::assets::Assets::fs(Path::new(&assets)));
    // GD sound effects: death and level complete (missing files = silent)
    let res = Path::new(&assets).join("Resources");
    let res = if res.is_dir() { res } else { PathBuf::from(&assets) };
    let sfx = |n: &str| res.join(n).is_file().then(|| Arc::new(decode_song(&res.join(n))));
    let (sfx_death, sfx_complete) = (sfx("explode_11.ogg"), sfx("endStart_02.ogg"));
    let mut voices: Vec<(Arc<Vec<u8>>, usize)> = Vec::new();

    // gifts: bridge child in a thread, restarted with backoff when it exits
    let (gift_tx, gift_rx) = mpsc::channel::<String>();
    if std::env::var("TT_USERNAME").is_ok_and(|u| !u.is_empty()) {
        let root = root.clone();
        std::thread::spawn(move || loop {
            match GiftBridge::spawn(&root) {
                Ok(mut b) => {
                    while let Ok(Some(ev)) = b.next_event() {
                        match ev {
                            BridgeEvent::Gift { user, name, count } => {
                                let _ = gift_tx.send(format!("{user} sent {count}x {name}"));
                            }
                            BridgeEvent::Status { state } => eprintln!("bridge: {state}"),
                        }
                    }
                }
                Err(e) => eprintln!("bridge spawn failed: {e}"),
            }
            std::thread::sleep(Duration::from_secs(5));
        });
    }

    eprintln!("live: {}x{} -> 720x1280 @ {FPS}fps, output {target_desc}", FRAME_W, FRAME_H);
    let mut sim = Sim::new(&level, &table);
    sim.follow = true;
    let mut pb = Playback::new(m.clone());
    let mut run_tick = 0usize;
    let mut run_frame = 0usize;
    // song cue in force and the run frame it took effect: the song runs on
    // the stream clock (it keeps playing through stalls, like GD's), song
    // triggers only move where it is.
    // shortcut: one song file; a trigger naming another song id seeks this one
    let mut cue = (sim.song, 0usize);
    let mut last_reset = Instant::now() - Duration::from_secs(10);
    let run_len = m.frames.len() + 240; // macro + 1 s on the end screen

    let mut frame = Arc::new(renderer.draw(&level, &table, &sim));
    let mut pcm = Vec::with_capacity(SAMPLES_PER_FRAME * BYTES_PER_SAMPLE);
    let mut session_no = 0;
    let mut session = Some(Session::start(out.as_deref(), session_no).expect("ffmpeg start"));
    let start = Instant::now();
    // frame schedule origin; rebased after an ffmpeg restart so the loop
    // does not think it is behind forever (and stop rendering)
    let mut epoch = start;
    let (mut sent, mut rendered, mut render_time) = (0u64, 0u64, Duration::ZERO);
    let mut window = (Instant::now(), 0u64, 0u64, Duration::ZERO);

    loop {
        // restart the run on a gift (debounced) or at the end of the macro
        let mut gift = None;
        while let Ok(g) = gift_rx.try_recv() {
            gift = Some(g);
        }
        let gift_reset = gift.is_some() && last_reset.elapsed() >= Duration::from_secs(1);
        if gift_reset || run_tick >= run_len {
            if let Some(g) = gift.filter(|_| gift_reset) {
                eprintln!("gift reset: {g}");
            }
            sim.reset();
            pb = Playback::new(m.clone());
            (run_tick, run_frame) = (0, 0);
            cue = (sim.song, 0);
            last_reset = Instant::now();
        }

        let state_before = sim.state;
        for _ in 0..TICKS_PER_FRAME {
            run_tick += 1;
            let (p1, p2) = pb.step();
            if pb.stalled() || sim.state != RunState::Playing {
                continue;
            }
            sim.step_single(InputState { hold: p2, hold_p2: p1 });
            if let Some(f) = m.frames.get(run_tick) {
                sim.apply_fix(f[0] as f64, f[1] as f64, f[2] as f64);
            }
            if let Some(f) = m.frames_p2.get(run_tick) {
                sim.apply_fix_p2(f[0] as f64, f[1] as f64, f[2] as f64);
            }
        }

        // render only when on schedule; otherwise repeat the last frame
        let due = epoch + Duration::from_secs_f64(sent as f64 / FPS as f64);
        if Instant::now() < due + Duration::from_secs_f64(1.0 / FPS as f64) {
            let t = Instant::now();
            frame = Arc::new(renderer.draw(&level, &table, &sim));
            render_time += t.elapsed();
            rendered += 1;
        }
        if sim.song != cue.0 {
            cue = (sim.song, run_frame);
        }
        pcm_chunk(&song, (cue.0 .1 * FPS as f64).round() as usize + run_frame - cue.1, &mut pcm);
        if sim.state != state_before {
            let hit = match sim.state {
                RunState::Dead => &sfx_death,
                RunState::Complete => &sfx_complete,
                RunState::Playing => &None,
            };
            voices.extend(hit.iter().map(|s| (s.clone(), 0)));
        }
        voices.retain_mut(|(s, pos)| mix_sfx(&mut pcm, s, pos));
        run_frame += 1;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        if !session.as_mut().is_some_and(|s| s.send(&frame, pcm.clone())) {
            eprintln!("ffmpeg not running; restarting in 5 s");
            if let Some(s) = session.take() {
                let _ = s.finish();
            }
            std::thread::sleep(Duration::from_secs(5));
            session_no += 1;
            session = Session::start(out.as_deref(), session_no)
                .map_err(|e| eprintln!("ffmpeg restart failed: {e}"))
                .ok();
            epoch = Instant::now() - Duration::from_secs_f64(sent as f64 / FPS as f64);
            continue;
        }
        sent += 1;

        if window.0.elapsed() >= Duration::from_secs(10) {
            let (dt, ds, dr, drt) = (window.0.elapsed().as_secs_f64(), sent - window.1, rendered - window.2, render_time - window.3);
            eprintln!(
                "t={:6.0}s sent {:.1} fps, rendered {:.1} fps ({:.0}% repeats), render {:.1} ms avg, lag {:.2}s, run {:.1}%",
                start.elapsed().as_secs_f64(),
                ds as f64 / dt,
                dr as f64 / dt,
                100.0 * (ds - dr.min(ds)) as f64 / ds.max(1) as f64,
                drt.as_secs_f64() * 1000.0 / dr.max(1) as f64,
                (Instant::now() - due).as_secs_f64().max(0.0),
                sim.progress()
            );
            window = (Instant::now(), sent, rendered, render_time);
        }
        if limit.is_some_and(|l| start.elapsed().as_secs_f64() >= l) {
            break;
        }
    }
    let ok = session.is_some_and(|s| s.finish());
    eprintln!(
        "done: {sent} frames ({:.1}s of video) in {:.1}s, {rendered} rendered, ffmpeg {}",
        sent as f64 / FPS as f64,
        start.elapsed().as_secs_f64(),
        if ok { "ok" } else { "FAILED" }
    );
    std::process::exit(if ok { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_chunk_slices_by_frame_and_pads_with_silence() {
        let len = SAMPLES_PER_FRAME * BYTES_PER_SAMPLE;
        let song: Vec<u8> = (0..len + 10).map(|i| (i % 251) as u8 + 1).collect();
        let mut out = Vec::new();
        pcm_chunk(&song, 0, &mut out);
        assert_eq!(out, song[..len]);
        pcm_chunk(&song, 1, &mut out);
        assert_eq!(out.len(), len);
        assert_eq!(out[..10], song[len..]);
        assert!(out[10..].iter().all(|&b| b == 0));
        pcm_chunk(&song, 5, &mut out);
        assert!(out.iter().all(|&b| b == 0) && out.len() == len);
    }

    #[test]
    fn sfx_mixes_saturating_and_ends() {
        let mut pcm = [1000i16, i16::MAX - 5].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>();
        let sfx: Vec<u8> = [500i16, 100, 7, 7].iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut pos = 0;
        assert!(mix_sfx(&mut pcm, &sfx, &mut pos), "half played");
        assert_eq!(pcm, [1500i16, i16::MAX].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
        assert!(!mix_sfx(&mut pcm, &sfx, &mut pos), "finished");
    }
}
