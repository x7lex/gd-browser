//! Where the player's own Geometry Dash art is (RobTop's, never bundled).
//!
//! The engine reads the game's files through [`Assets`]: a folder on disk
//! (native), or bytes by name in the browser (src/web: fetched, or picked
//! from the player's own install with a folder picker).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Read-only access to the game's resource files, by name relative to the
/// resources root (e.g. `GJ_GameSheet-uhd.plist`, `icons/dart_01-uhd.png`).
#[derive(Clone, Default)]
pub enum Assets {
    /// No art: the renderer degrades to hitbox outlines.
    #[default]
    None,
    /// A Geometry Dash install / its Resources folder on disk.
    Fs(PathBuf),
    /// Files by name, added one by one (the browser build).
    Map(HashMap<String, Arc<[u8]>>),
}

impl Assets {
    /// A folder on disk: the dir itself, its Resources/ (a game install) or
    /// Contents/Resources (a macOS .app), whichever holds the sheets.
    pub fn fs(dir: &Path) -> Assets {
        Assets::Fs(sheets_in(dir).unwrap_or_else(|| dir.to_path_buf()))
    }

    /// An empty byte store (the browser fills it with `insert`).
    pub fn map() -> Assets {
        Assets::Map(HashMap::new())
    }

    /// Add a file's bytes (Map only; ignored on the other variants).
    pub fn insert(&mut self, name: &str, bytes: &[u8]) {
        if let Assets::Map(m) = self {
            m.insert(name.to_string(), Arc::from(bytes));
        }
    }

    /// How many files are in the store (Map only; 0 elsewhere).
    pub fn len(&self) -> usize {
        match self {
            Assets::Map(m) => m.len(),
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a file by this name is present.
    pub fn exists(&self, name: &str) -> bool {
        match self {
            Assets::None => false,
            Assets::Fs(dir) => dir.join(name).exists(),
            Assets::Map(m) => m.contains_key(name),
        }
    }

    /// A file's bytes.
    pub fn get(&self, name: &str) -> Option<Arc<[u8]>> {
        match self {
            Assets::None => None,
            Assets::Fs(dir) => std::fs::read(dir.join(name)).ok().map(Arc::from),
            Assets::Map(m) => m.get(name).cloned(),
        }
    }

    /// A text file (plists, anim descs, the font description).
    pub fn text(&self, name: &str) -> Option<String> {
        self.get(name).and_then(|b| String::from_utf8(b.to_vec()).ok())
    }

    /// A PNG file, decoded.
    pub fn png(&self, name: &str) -> Option<tiny_skia::Pixmap> {
        self.get(name).and_then(|b| tiny_skia::Pixmap::decode_png(&b).ok())
    }
}

/// The folder that holds GD's sprite sheets inside `dir`: the dir itself,
/// its Resources/ (a game install), or Contents/Resources (a macOS .app).
pub fn sheets_in(dir: &Path) -> Option<PathBuf> {
    [dir.to_path_buf(), dir.join("Resources"), dir.join("Contents/Resources")]
        .into_iter()
        .find(|d| d.join("GJ_GameSheet-uhd.plist").exists())
}

/// The remembered folder lives here (one line: the path).
fn saved_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home?.join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home.map(|h| h.join(".config")))?
    };
    Some(base.join("gd-rust").join("assets_dir"))
}

/// The art folder without asking: env, remembered, or (Windows) Steam's default.
pub fn find() -> Option<PathBuf> {
    let env = std::env::var_os("DEMO_ASSETS").map(PathBuf::from);
    let saved = saved_file().and_then(|f| std::fs::read_to_string(f).ok()).map(|s| PathBuf::from(s.trim()));
    let steam = cfg!(windows).then(|| PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common\Geometry Dash"));
    [env, saved, steam].into_iter().flatten().find_map(|d| sheets_in(&d))
}

/// The art folder, asking with a native folder picker when none is known
/// yet (Linux / macOS; remembered for later runs). Exits if the player cancels.
#[cfg(not(target_family = "wasm"))]
pub fn find_or_ask() -> PathBuf {
    if let Some(d) = find() {
        return d;
    }
    #[cfg(not(windows))]
    loop {
        let picked = rfd::FileDialog::new()
            .set_title("Choose your Geometry Dash folder (the game's art is read from it)")
            .pick_folder();
        let Some(dir) = picked else { break };
        if let Some(sheets) = sheets_in(&dir) {
            if let Some(f) = saved_file() {
                let saved = f.parent().map(std::fs::create_dir_all).transpose().and_then(|_| std::fs::write(&f, sheets.to_string_lossy().as_bytes()));
                if let Err(e) = saved {
                    eprintln!("could not remember the folder ({}): {e}", f.display());
                }
            }
            return sheets;
        }
        rfd::MessageDialog::new()
            .set_title("Not a Geometry Dash folder")
            .set_description(format!("{} has no GJ_GameSheet-uhd.plist. Pick the folder Geometry Dash is installed in (or its Resources folder).", dir.display()))
            .show();
    }
    eprintln!("no Geometry Dash art folder: install Geometry Dash, or set DEMO_ASSETS to its folder");
    std::process::exit(1);
}

/// Every file the renderer may read from the game's resources: sprite
/// sheets (plist + png), effect plists, animation descs, the big font and
/// its page, the death sound, and the background / ground tiles. The
/// browser build fetches or picks exactly these.
pub fn wanted_names() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (name, _) in crate::render::atlas::sheet_list() {
        out.push(format!("{name}.plist"));
        out.push(format!("{name}.png"));
    }
    for n in crate::render::play_fx::effect_names() {
        out.push(format!("{n}.plist"));
    }
    out.push("Robot_AnimDesc.plist".into());
    out.push("Spider_AnimDesc.plist".into());
    out.push("bigFont-uhd.fnt".into());
    out.push("bigFont-uhd.png".into());
    out.push("explode_11.ogg".into());
    out.push("GJ_levelComplete_001.png".into());
    for i in 1..=30 {
        out.push(format!("game_bg_{i:02}_001-uhd.png"));
        out.push(format!("groundSquare_{i:02}_001-uhd.png"));
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_sheets_in_an_install_a_resources_dir_or_an_app() {
        let root = std::env::temp_dir().join(format!("gdrust-assets-{}", std::process::id()));
        for sub in ["game/Resources", "GD.app/Contents/Resources", "plain"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
            std::fs::write(root.join(sub).join("GJ_GameSheet-uhd.plist"), "").unwrap();
        }
        assert_eq!(sheets_in(&root.join("game")), Some(root.join("game/Resources")));
        assert_eq!(sheets_in(&root.join("GD.app")), Some(root.join("GD.app/Contents/Resources")));
        assert_eq!(sheets_in(&root.join("plain")), Some(root.join("plain")));
        assert_eq!(sheets_in(&root), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn map_backed_assets_read_bytes_text_and_pngs() {
        let mut a = Assets::map();
        assert!(a.is_empty());
        a.insert("note.txt", b"hello");
        assert_eq!(a.text("note.txt").as_deref(), Some("hello"));
        assert!(a.exists("note.txt") && !a.exists("missing.txt"));
        // a real 1x1 png, decoded from bytes
        let pm = tiny_skia::Pixmap::new(1, 1).unwrap();
        a.insert("dot.png", &pm.encode_png().unwrap());
        assert_eq!(a.png("dot.png").map(|p| p.width()), Some(1));
        // a non-utf8 file is not text, but its bytes still come through
        a.insert("bin", &[0xff, 0xfe]);
        assert!(a.text("bin").is_none());
        assert_eq!(a.get("bin").as_deref().map(|b| b.len()), Some(2));
    }

    #[test]
    fn wanted_names_cover_the_sheets_font_and_death_sound() {
        let w = wanted_names();
        assert!(w.contains(&"GJ_GameSheet-uhd.plist".to_string()));
        assert!(w.contains(&"GJ_GameSheet-uhd.png".to_string()));
        assert!(w.contains(&"icons/dart_01-uhd.png".to_string()));
        assert!(w.contains(&"bigFont-uhd.fnt".to_string()));
        assert!(w.contains(&"explode_11.ogg".to_string()));
        assert!(w.contains(&"game_bg_01_001-uhd.png".to_string()));
    }
}
