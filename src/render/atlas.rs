//! Texture atlas: TexturePacker/cocos2d format-3 plist parsing + lazy PNG decode + sprite extraction.

use std::collections::{HashMap, HashSet};
use tiny_skia::Pixmap;

use crate::assets::Assets;

/// One plist frame. `w`/`h` are the natural (trimmed) sprite dims; for rotated
/// frames the sheet stores the region transposed (width h, height w) with the
/// pixels rotated 90° clockwise (cocos2d Sprite::setTextureCoords convention,
/// verified against the sheet pixels).
#[derive(Clone, Debug)]
pub(crate) struct Frame {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub rotated: bool,
    /// untrimmed source size — the sprite's world size is this / 4 (uhd = 4x)
    pub uw: u32,
    pub uh: u32,
    pub ox: i32,
    pub oy: i32,
}

struct Sheet {
    /// the sheet png's file name (in the asset source)
    png: String,
    frames: HashMap<String, Frame>,
    atlas: Option<Pixmap>,
    /// sheet px per world unit (uhd = 4, hd = 2)
    scale: f32,
}

pub struct SpriteStore {
    sheets: Vec<Sheet>,
    cache: HashMap<String, Pixmap>,
    misses: HashSet<String>,
    assets: Assets,
}

/// (sheet name, sheet px per unit)
const SHEETS: &[(&str, f32)] = &[
    ("GJ_GameSheet-uhd", 4.0),
    ("GJ_GameSheet02-uhd", 4.0),
    ("GJ_GameSheet03-uhd", 4.0),
    ("GJ_GameSheet04-uhd", 4.0),
    ("GJ_GameSheetGlow-uhd", 4.0),
    // 2.2 decoration: pixel art, particle shapes, animated fire
    ("PixelSheet_01-uhd", 4.0),
    ("GJ_ParticleSheet-uhd", 4.0),
    ("FireSheet_01-uhd", 4.0),
    ("GJ_GameSheetIcons-hd", 2.0),
    // player icons live in their own sheets (icons/dart_NN-uhd)
    ("icons/dart_01-uhd", 4.0),
    ("icons/player_01-uhd", 4.0),
    ("icons/ship_01-uhd", 4.0),
    ("icons/player_ball_01-uhd", 4.0),
    ("icons/bird_01-uhd", 4.0),
    ("icons/robot_01-uhd", 4.0),
    ("icons/spider_01-uhd", 4.0),
    ("icons/swing_01-uhd", 4.0),
];

/// The sheets the renderer knows, in load order (asset names to expose to
/// the browser build).
pub fn sheet_list() -> &'static [(&'static str, f32)] {
    SHEETS
}

impl SpriteStore {
    /// Missing/undecodable sheets are skipped: the renderer degrades to
    /// hitbox outlines instead of failing.
    pub fn load(assets: &Assets) -> SpriteStore {
        let sheets = SHEETS
            .iter()
            .filter_map(|(name, scale)| {
                let text = assets.text(&format!("{name}.plist"))?;
                Some(Sheet { png: format!("{name}.png"), frames: parse_plist(&text), atlas: None, scale: *scale })
            })
            .collect();
        SpriteStore { sheets, cache: HashMap::new(), misses: HashSet::new(), assets: assets.clone() }
    }

    /// The asset source behind the store (backgrounds, fonts, effects).
    pub fn assets(&self) -> &Assets {
        &self.assets
    }

    /// A text file from the asset source.
    pub fn text(&self, name: &str) -> Option<String> {
        self.assets.text(name)
    }

    /// A decoded png from the asset source.
    pub fn png(&self, name: &str) -> Option<Pixmap> {
        self.assets.png(name)
    }

    /// Extracted sprite (un-rotated, un-trimmed to source size) for a texture
    /// name, cached on first use, with its sheet's px-per-unit scale.
    pub fn sprite_sheet(&mut self, name: &str) -> Option<(&Pixmap, f32)> {
        if !self.cache.contains_key(name) {
            if self.misses.contains(name) {
                return None;
            }
            let mut hit = None;
            for sheet in &mut self.sheets {
                let frame = sheet
                    .frames
                    .get(name)
                    .or_else(|| sheet.frames.get(&format!("{name}.png")))
                    .cloned();
                let Some(frame) = frame else { continue };
                if sheet.atlas.is_none() {
                    sheet.atlas = self.assets.png(&sheet.png);
                }
                if let Some(pm) = sheet.atlas.as_ref().and_then(|a| extract(a, &frame)) {
                    self.cache.insert(name.to_string(), pm);
                    hit = Some(sheet.scale);
                    break;
                }
            }
            if hit.is_none() {
                self.misses.insert(name.to_string());
                return None;
            }
        }
        let scale = self
            .sheets
            .iter()
            .find(|s| s.frames.contains_key(name) || s.frames.contains_key(&format!("{name}.png")))
            .map(|s| s.scale)
            .unwrap_or(4.0);
        self.cache.get(name).map(|p| (p, scale))
    }

    /// An already-extracted sprite (call `sprite` first), shared read-only.
    pub fn cached(&self, name: &str) -> Option<&Pixmap> {
        self.cache.get(name)
    }

    /// Register a sprite built elsewhere (multi-sprite composites).
    pub fn insert(&mut self, name: &str, pm: Pixmap) {
        self.cache.insert(name.to_string(), pm);
    }

    /// Extracted sprite (un-rotated, un-trimmed to source size) for a texture
    /// name, cached on first use. None if no sheet has it.
    pub fn sprite(&mut self, name: &str) -> Option<&Pixmap> {
        self.sprite_sheet(name).map(|(p, _)| p)
    }
}

/// Copy a frame out of its sheet atlas: undo the 90° CW pack rotation, then
/// place the trimmed pixels at spriteOffset inside the source-size canvas.
fn extract(atlas: &Pixmap, f: &Frame) -> Option<Pixmap> {
    let (w, h) = (f.w, f.h);
    let mut trim = Pixmap::new(w, h)?;
    let aw = atlas.width() as usize;
    let ah = atlas.height() as usize;
    {
        let src = atlas.data();
        let dst = trim.data_mut();
        for j in 0..h as usize {
            for i in 0..w as usize {
                // rotated: natural(i,j) = atlas(x + h-1-j, y + i)
                let (ax, ay) = if f.rotated {
                    (f.x as usize + h as usize - 1 - j, f.y as usize + i)
                } else {
                    (f.x as usize + i, f.y as usize + j)
                };
                if ax >= aw || ay >= ah {
                    return None;
                }
                let s = (ay * aw + ax) * 4;
                let d = (j * w as usize + i) * 4;
                dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
    }
    if w == f.uw && h == f.uh && f.ox == 0 && f.oy == 0 {
        return Some(trim);
    }
    // cocos2d un-trim: offset is y-up from the source box center
    let mut full = Pixmap::new(f.uw, f.uh)?;
    let left = (f.uw as i64 - w as i64) / 2 + f.ox as i64;
    let top = (f.uh as i64 - h as i64) / 2 - f.oy as i64;
    {
        let src = trim.data();
        let dst = full.data_mut();
        for j in 0..h as i64 {
            let ty = top + j;
            if ty < 0 || ty >= f.uh as i64 {
                continue;
            }
            for i in 0..w as i64 {
                let tx = left + i;
                if tx < 0 || tx >= f.uw as i64 {
                    continue;
                }
                let s = (j as usize * w as usize + i as usize) * 4;
                let d = (ty as usize * f.uw as usize + tx as usize) * 4;
                dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
    }
    Some(full)
}

fn find_from(s: &str, from: usize, pat: &str) -> Option<usize> {
    s.get(from..)?.find(pat).map(|i| i + from)
}

/// Value after `<key>key</key>` in a flat dict: `<string>v</string>` → v,
/// `<true/>`/`<false/>` → "true"/"false".
fn field<'a>(dict: &'a str, key: &str) -> Option<&'a str> {
    let k = dict.find(&format!("<key>{key}</key>"))?;
    let after = dict[k + key.len() + 11..].trim_start();
    if let Some(v) = after.strip_prefix("<string>") {
        Some(&v[..v.find("</string>")?])
    } else if after.starts_with("<true/>") {
        Some("true")
    } else if after.starts_with("<false/>") {
        Some("false")
    } else {
        None
    }
}

/// All integers in a "{{x,y},{w,h}}" / "{a,b}" style plist value.
fn ints(s: &str) -> Vec<i64> {
    s.split(|c: char| !c.is_ascii_digit() && c != '-' && c != '+')
        .filter_map(|t| t.parse::<i64>().ok())
        .collect()
}

/// Hand-rolled scan of the `<key>frames</key>` dict: flat
/// `<key>NAME</key><dict>…</dict>` entries (no nested dicts inside).
fn parse_plist(text: &str) -> HashMap<String, Frame> {
    let mut out = HashMap::new();
    let Some(a) = text.find("<key>frames</key>") else { return out };
    let end = find_from(text, a, "<key>metadata</key>").unwrap_or(text.len());
    let sec = &text[a + 17..end];
    let mut pos = 0;
    while let Some(k) = find_from(sec, pos, "<key>") {
        let name = &sec[k + 5..];
        let Some(ne) = name.find("</key>") else { break };
        let name = &name[..ne];
        let Some(d) = find_from(sec, k + 5 + ne + 6, "<dict>") else { break };
        let body = d + 6;
        let Some(close) = sec[body..].find("</dict>") else { break };
        let dict = &sec[body..body + close];
        if let Some(f) = parse_frame(dict) {
            out.insert(name.to_string(), f);
        }
        pos = body + close + 7;
    }
    out
}

fn parse_frame(dict: &str) -> Option<Frame> {
    let sz = ints(field(dict, "spriteSize")?);
    let src = ints(field(dict, "spriteSourceSize")?);
    let tr = ints(field(dict, "textureRect")?);
    let off = ints(field(dict, "spriteOffset").unwrap_or("{0,0}"));
    if sz.len() < 2 || src.len() < 2 || tr.len() < 4 {
        return None;
    }
    Some(Frame {
        x: tr[0] as u32,
        y: tr[1] as u32,
        w: sz[0] as u32,
        h: sz[1] as u32,
        rotated: field(dict, "textureRotated").as_deref() == Some("true"),
        uw: src[0] as u32,
        uh: src[1] as u32,
        ox: off.first().copied().unwrap_or(0) as i32,
        oy: off.get(1).copied().unwrap_or(0) as i32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Assets;

    fn assets() -> Assets {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        Assets::fs(&dir)
    }

    #[test]
    fn plists_parse_with_known_frames() {
        let store = SpriteStore::load(&assets());
        let total: usize = store.sheets.iter().map(|s| s.frames.len()).sum();
        assert!(total > 3000, "sprites parsed: {total}");
        let f = store.sheets[0].frames.get("square_01_001.png").unwrap();
        assert!(!f.rotated);
        assert_eq!((f.x, f.y, f.w, f.h), (2963, 3073, 120, 120));
        let p = store.sheets[1].frames.get("portal_13_front_001.png").unwrap();
        assert!(p.rotated);
        assert_eq!((p.w, p.h, p.uw, p.uh), (134, 340, 178, 342));
    }

    #[test]
    fn extract_unrotates_and_untrims() {
        // lazy extraction needs &mut, so go through sprite()
        let mut store = SpriteStore::load(&assets());
        // spike_01_001: not rotated, untrimmed → 120×120
        let pm = store.sprite("spike_01_001").unwrap();
        assert_eq!((pm.width(), pm.height()), (120, 120));
        // portal_13_front_001: rotated + trimmed → source size 178×342, tip-up content
        let pm = store.sprite("portal_13_front_001").unwrap();
        assert_eq!((pm.width(), pm.height()), (178, 342));
        assert!(store.sprite("definitely_not_a_texture").is_none());
    }

    /// The tip of spike_01 must land in the top half after un-rotation.
    #[test]
    fn extracted_rotated_sprite_points_up() {
        let mut store = SpriteStore::load(&assets());
        let pm = store.sprite("colorSpike_03_001").unwrap();
        let d = pm.data();
        let at = |x: usize, y: usize| d[(y * pm.width() as usize + x) * 4 + 3];
        // 80×76 sprite: apex top-center opaque, top-left corner transparent
        assert!(at(40, 8) > 0, "apex missing");
        assert_eq!(at(4, 4), 0, "corner should be transparent");
    }
}
