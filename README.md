# gd-rust

A Geometry Dash engine in Rust: 240 Hz physics, a CPU renderer that matches the
real game's look, and a bot that solves levels on its own.

**gd-browser** (see [web/](web/) and [tools/web_pack.py](tools/web_pack.py)):
the same engine compiled to WebAssembly, playable in the browser — levels from
`levels/`, the bot's own solve for levels without a recording, game art from
`assets/` (or your own GD install).

## Install

Download a package from **Releases** (Linux, macOS, Windows), unpack it and run `demo`.
macOS builds are unsigned: run `xattr -dr com.apple.quarantine gd-rust-macos-arm64` once.

Or build it: `cargo run --release --bin demo` (Linux needs `libasound2-dev`).

## Requirements

- Geometry Dash installed. The demo draws with the game's own art and plays its
  songs; nothing of RobTop's is bundled. On first run Linux and macOS ask for the
  Geometry Dash folder (remembered after); Windows uses Steam's default folder.
  `DEMO_ASSETS=<dir>` overrides both.
- Python 3, to download levels and their songs (`tools/fetch_level.py`).
- FFmpeg only for `record` and `live`. The demo plays audio itself.

## Demo

```sh
demo [level id]   # default 149992717 (GRIEF)
```

A missing level is downloaded with its song (`tools/fetch_level.py`). It plays a
recorded xdBot macro from `macros/` if there is one, otherwise the bot's own
solve, which is cached in `macros/<id>.auto.json`.

| Key | Action |
|---|---|
| Insert | menu: type a level ID, Enter loads it, Tab switches Auto / Manual |
| Space / Up / W / mouse | jump (Manual mode) |
| R | restart |
| Q / Esc | quit |

You can also type a level ID in the terminal to queue it: it loads in the background.

## Browser (gd-browser)

```sh
trunk serve                        # packs dist/ and serves http://localhost:8080
# or
python3 tools/web_pack.py --serve  # builds dist/ and serves http://localhost:8000
```

`trunk serve` runs the same packer through a hook in [Trunk.toml](Trunk.toml);
the copy hooks are in [web/index.html](web/index.html).

The same engine as WebAssembly (`wasm-pack build --release --target web`), the
same behavior: 240 Hz sim ticks from wall clock, deaths restart the attempt,
recorded macros play with their frame fixes, and a level without a recording
gets the bot's own solve — in a Web Worker, with a progress readout, then
replays automatically. Manual mode is always one keypress away.

| Key | Action |
|---|---|
| Space / ↑ / W / click / touch | jump (Manual mode) |
| T | auto ⇄ manual |
| R | restart |
| `#<level id>` in the URL | open a specific level |

Songs play through Web Audio at the sim's song cue (level offset, song
triggers), restarted at the exact sample like the native demo; GD's death
sound plays when it is among the art.

**Game art**: the needed sheets in `assets/` are copied into `dist/assets/`
and the page loads them with a progress bar. Without them levels render as
hitbox outlines; the page's *load GD folder* picker reads art from your own
Geometry Dash install at runtime — matched by name, never uploaded anywhere.

`node web/smoke.mjs` runs the whole wasm stack headlessly (boot, GRIEF +
macro replay, render, deaths, manual, solve → set_macro → 100%).

## Other tools

| Binary | Purpose |
|---|---|
| `record [out.mp4]` | offline render to MP4 with the song (`RECORD_LEVEL=<id>`) |
| `live` | 24/7 vertical stream to TikTok LIVE (keys in `.env`, see `deploy/`) |
| `frame <tick>` | render one frame to PNG; `FRAME_SWEEP=n` times n frames |
| `fidelity`, `truth`, `autocheck` | compare the sim against real GD recordings |
| `rig` | hand-driven test level with every orb, pad and portal |
| `web_index` | the browser build's manifests (levels.json / assets.json) |

## Layout

```
src/sim/      physics, collisions, triggers
src/render/   renderer (sprite atlas, effects, shaders)
src/bot/      macro parsing (.gdr2) and the beam-search solver
src/level/    level string parser
src/web/      the wasm-bindgen API the browser app drives
data/         object table (ids, hitboxes, sprites)
```

`cargo test --release` runs the test suite.
