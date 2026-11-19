//! The browser build's manifests (tools/web_pack.py runs this):
//! one JSON object on stdout:
//! {
//!   "levels":  [{id, name, song, gmd, meta, macro: {type, file} | null}],
//!   "assets": [{name, file, size}],   // game art found, copied to dist/assets
//!   "wanted": [name]                  // every name the renderer may read
//! }
//!
//! The macro choice mirrors bot::auto::macro_for: a recorded .gdr2 on the
//! level's id (ones with frame fixes first), else the cached solve
//! <id>.auto.json, else null (the browser solves in a Web Worker).
//! Art comes from DEMO_ASSETS / the assets/ folder / the remembered
//! install; nothing is bundled when none exists (the renderer degrades
//! to hitbox outlines, and the page offers a folder picker instead).

use gdrust::assets::{self, Assets};
use gdrust::bot::gdr2;
use std::path::{Path, PathBuf};

fn main() {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let manifest: &Path = match exe_dir.as_deref().filter(|d| d.join("data/objects.json").exists()) {
        Some(d) => d,
        None => Path::new(env!("CARGO_MANIFEST_DIR")),
    };

    let sorted = |dir: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
        v.sort();
        v
    };

    // recorded macros, by level id, with their repo-relative path
    let mut gdr2s: Vec<(u32, bool, String)> = sorted(&manifest.join("macros"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "gdr2"))
        .filter_map(|p| {
            let bytes = std::fs::read(&p).ok()?;
            let m = gdr2::parse_gdr2(&bytes).ok()?;
            let rel = p.strip_prefix(manifest).ok()?.to_str()?.to_string();
            Some((m.level_id, m.frames.is_empty(), rel))
        })
        .collect();
    // frame-fix recordings first, like macro_for
    gdr2s.sort_by(|a, b| a.1.cmp(&b.1));

    let mut levels: Vec<serde_json::Value> = Vec::new();
    let mut bg_ids: Vec<u32> = Vec::new();
    for gmd in sorted(&manifest.join("levels")) {
        if gmd.extension().is_some_and(|e| e != "gmd") {
            continue;
        }
        let Some(id) = gmd.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(text) = std::fs::read_to_string(&gmd) else { continue };
        let meta_path = gmd.with_extension("json");
        let meta_text = std::fs::read_to_string(&meta_path)
            .unwrap_or_else(|_| format!("{{\"name\":\"{id}\",\"song\":null}}"));
        let meta: serde_json::Value = serde_json::from_str(&meta_text).unwrap_or(serde_json::Value::Null);
        let name = meta["name"].as_str().unwrap_or("level").to_string();
        let song = meta["song"].as_str().map(str::to_string);
        // the macro, macro_for-style: recorded .gdr2 on this id, else the
        // cached solve, else the browser's Web Worker solve
        let macro_entry = gdr2s
            .iter()
            .find(|(lid, _, _)| *lid == id)
            .map(|(_, _, rel)| serde_json::json!({ "type": "gdr2", "file": rel }))
            .or_else(|| {
                let cache = format!("macros/{id}.auto.json");
                (manifest.join(&cache).exists()).then(|| serde_json::json!({ "type": "json", "file": cache }))
            });
        // which bg (kA6) / ground (kA7) this level wants: only those tiles
        // are worth copying into dist
        let header = text.split(';').next().unwrap_or("");
        for want in ["kA6", "kA7"] {
            if let Some(at) = header.find(&format!(",{want},")) {
                let rest = &header[at + want.len() + 3..];
                let v: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = v.parse::<u32>() {
                    bg_ids.push(n);
                }
            }
        }
        levels.push(serde_json::json!({
            "id": id,
            "name": name,
            "song": song,
            "gmd": format!("levels/{id}.gmd"),
            "meta": format!("levels/{id}.json"),
            "macro": macro_entry,
        }));
    }
    bg_ids.sort_unstable();
    bg_ids.dedup();

    // game art: what the renderer may read, and which of those exist
    let art_dir = std::env::var_os("DEMO_ASSETS")
        .map(PathBuf::from)
        .or_else(|| {
            let a = manifest.join("assets");
            a.join("GJ_GameSheet-uhd.plist").exists().then_some(a)
        })
        .or_else(assets::find);
    let mut found: Vec<serde_json::Value> = Vec::new();
    if let Some(src) = art_dir.as_deref().map(Assets::fs) {
        for name in assets::wanted_names() {
            // bg/ground tiles: only the ids the bundled levels use
            if (name.starts_with("game_bg_") || name.starts_with("groundSquare_"))
                && !bg_ids.iter().any(|&i| name.contains(&format!("_{i:02}_")))
            {
                continue;
            }
            if src.get(&name).is_some() {
                found.push(serde_json::json!({
                    "name": name,
                    "file": format!("assets/{name}"),
                    // where the packer copies it from (the art install)
                    "path": art_dir.as_deref().unwrap_or(Path::new("")).join(&name).to_str().unwrap_or_default(),
                    "size": src.get(&name).map(|b| b.len()).unwrap_or(0),
                }));
            }
        }
    }

    let out = serde_json::json!({
        "levels": levels,
        "assets": found,
        "wanted": assets::wanted_names(),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
