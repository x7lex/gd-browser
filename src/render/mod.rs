//! Level renderer: 720×1280 RGBA frames from a Level + ObjectTable + Sim via
//! tiny-skia software raster (Task 7).

pub mod atlas;
pub mod fx;
pub(crate) mod play_fx;
mod shader;

use crate::assets::Assets;
use crate::level::object::GdObject;
use crate::level::Level;
use crate::sim::objects::{sim_object, ObjectTable};
use crate::sim::player::{GameMode, Player};
use crate::sim::Sim;
use crate::sim::triggers::PulseColor;
use atlas::SpriteStore;
use tiny_skia::{
    Color, FillRule, FilterQuality, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke,
    Transform,
};

pub const FRAME_W: u32 = 1280;
pub const FRAME_H: u32 = 720;
/// internal render resolution (upscaled ×2 on output — 4× fewer pixels)
// world canvas at the output size: a half-size canvas doubled with
// nearest pixels made spike edges stair-step and seams between blocks 2 px
const RW: u32 = FRAME_W;
const RH: u32 = FRAME_H;
/// pixels per world unit on the internal canvas: 360u vertical view
const SCALE: f32 = RH as f32 / 320.0; // GD shows 320u vertically
/// uhd sheets are 4x retina: sprite px per world unit
const SHEET_SCALE: f32 = 4.0;
/// vertical half view in units
const HALF_VIEW_Y: f32 = RH as f32 / SCALE / 2.0;
/// pre-cull margin for the x scan; exact cull uses real sprite extents after
/// shortcut: objects wider than ~800u can be skipped early, none known in these levels
const CULL_PAD: f32 = 400.0;

pub struct Renderer {
    store: SpriteStore,
    /// lazily-decoded untinted background (png name, texture; None = missing)
    bg: Option<(String, Option<Pixmap>)>,
    fps: Option<u32>,
    /// prescaled sprites (extracted, tinted, rotated, downscaled to canvas
    /// resolution) cached per (texture, channel, rot, flips, scale)
    prescaled: crate::sim::fx::HashMap<SpriteKey, (std::sync::atomic::AtomicU64, Option<Sprite>)>,
    /// draw counter: last-use stamps for prescaled-cache eviction
    frame_no: u64,
    /// per level object: its (secondary, main) layers, static per level
    obj_layers: Vec<[Option<ObjLayer>; 2]>,
    /// smoothed camera y and the sim tick it was computed at
    cam: Option<(f32, u64)>,
    /// per player: cube/ball spin (deg) and the sim tick it was updated at
    spin: Vec<(f32, u64)>,
    /// use effects: (world x, y, start tick, color) of orbs/pads/portals just entered
    fx: Vec<(f64, f64, u64, (u8, u8, u8), bool)>,
    /// last frame's touched sets, to spot new contacts
    prev_touch: Vec<crate::sim::fx::HashSet<usize>>,
    /// objects sorted by x, cached per level (invalidated by pointer)
    sorted: Option<Vec<(f32, usize)>>,
    sorted_key: u64,
    /// live color channels, driven by the level's own color triggers;
    /// rebuilt per (level, attempt) so every attempt starts from the header
    colors: Option<ColorEngine>,
    colors_key: (u64, u32),
    /// object-id lookups, built once per object table (keyed by pointer)
    ids: Option<(usize, IdTables)>,
    /// tinted player dart per player color
    icons: crate::sim::fx::HashMap<(String, (u8, u8, u8)), Pixmap>,
    /// GD effect data (emitters, robot/spider anims, font), loaded on first draw
    gfx: Option<play_fx::Gfx>,
    /// particles, effect timers, camera triggers
    fxs: play_fx::State,
    /// ground tile (ground id, tile at canvas scale) and floor line (line id, sprite)
    ground: Option<(u32, Option<Pixmap>)>,
    floor_line: Option<(u32, Option<Pixmap>)>,
    /// sprites still off the canvas build here, so the frame they enter
    /// range in (400u ahead) does not stall: keys in flight, results
    ahead: (crate::sim::fx::HashSet<SpriteKey>, std::sync::mpsc::Sender<(SpriteKey, Option<Sprite>)>, std::sync::mpsc::Receiver<(SpriteKey, Option<Sprite>)>),
    /// the pool `ahead` builds on; None on wasm (no spawnable threads
    /// there): misses build in-frame instead
    ahead_pool: Option<rayon::ThreadPool>,
    /// counter labels (1615) per level uid, orbs per sim grid: found once,
    /// not by scanning every object each frame (GRIEF has 190k)
    counters: (u64, Vec<usize>),
    orbs: (usize, Vec<usize>),
    /// demo menu lines drawn over the frame (None: closed)
    menu: Option<Vec<String>>,
}

/// prescaled-sprite cache key: (texture, rot×1000, flipX, flipY, scale bits)
type SpriteKey = (String, i32, bool, bool, u64);
/// prescaled sprite + its top-left offset from the object center (canvas px)
/// + per row the [start, end) x span of non-transparent pixels
type Sprite = std::sync::Arc<(Pixmap, f32, f32, Vec<(u16, u16)>)>;

/// Per row: [first, last+1) x of pixels with alpha > 0; (0, 0) for empty rows.
/// Blits walk only these spans (rotated / glow sprites are mostly empty).
fn opaque_spans(p: &Pixmap) -> Vec<(u16, u16)> {
    let w = p.width() as usize;
    p.data()
        .chunks_exact(w * 4)
        .map(|row| {
            let a = |x: &usize| row[x * 4 + 3] != 0;
            match (0..w).find(a) {
                Some(first) => (first as u16, ((0..w).rev().find(a).unwrap_or(first) + 1) as u16),
                None => (0, 0),
            }
        })
        .collect()
}

/// One color layer of an object: prescaled-sprite key, channel (0 =
/// uncolorable art), the object's own HSV shift.
struct ObjLayer {
    key: SpriteKey,
    cid: u16,
    hsv: Option<[f32; 5]>,
}

/// Per-object-id lookups from the object table.
struct IdTables {
    tex: Vec<Option<String>>,
    /// multi-sprite objects: tex is "#<id>", built lazily by `composite`
    /// (parent texture, parent color slot, children, has a secondary-color layer)
    kids: crate::sim::fx::HashMap<String, (Option<String>, Option<String>, Vec<crate::sim::objects::ChildSprite>, bool)>,
    /// table default (main, secondary) color channel per id
    chans: Vec<(u16, Option<u16>)>,
    hitbox4: Vec<bool>,
    deco: Vec<bool>,
    /// table default (z-layer, z-order) per id
    z: Vec<(i32, i32)>,
    /// saws, gears, wheels: GD spins them continuously
    spin: Vec<bool>,
}

impl IdTables {
    fn new(table: &ObjectTable) -> IdTables {
        let n = u16::MAX as usize + 1;
        let mut t = IdTables {
            tex: vec![None; n],
            kids: Default::default(),
            chans: vec![(1004, None); n],
            hitbox4: vec![false; n],
            deco: vec![false; n],
            z: vec![(1, 0); n],
            spin: vec![false; n],
        };
        for (k, def) in &table.objects {
            if let Ok(id) = k.parse::<u16>() {
                let id = id as usize;
                t.z[id] = (def.default_z_layer as i32, def.default_z_order as i32);
                let ch = |c: i64| u16::try_from(c).ok().filter(|&c| c > 0);
                // default channel 0 (portals, orbs, pads): drawn untinted
                let main = if def.default_primary_channel == 0 && def.color_channel.is_none() {
                    0
                } else if def.color_channel.as_deref() == Some("black") {
                    1010 // "black" art (pit spikes, b-blocks) is drawn black, like black children
                } else {
                    ch(def.default_primary_channel).unwrap_or(1004)
                };
                t.chans[id] = (main, ch(def.default_secondary_channel));
                // editor-only art (trigger/text/start-pos icons, empty
                // frames) never shows in game: no sprite, no outline
                let tex = def.texture_name.as_deref().unwrap_or("");
                let lt = tex.to_ascii_lowercase();
                t.spin[id] = ["saw", "blade", "wheel", "rotating", "cog"].iter().any(|k| lt.contains(k)) && !lt.contains("trap");
                if let Some(kids) = def.children.as_ref().filter(|k| !k.is_empty()) {
                    let parent = def.texture_name.clone().filter(|t| t != "emptyFrame");
                    t.tex[id] = Some(format!("#{id}"));
                    let pc = def.color_channel.clone();
                    let sec = kids.iter().any(|k| is_secondary(k, pc.as_deref()));
                    t.kids.insert(format!("#{id}"), (parent, pc, kids.clone(), sec));
                    t.deco[id] = def.object_type == 7;
                    continue;
                }
                if back_of(tex).is_some() {
                    // portal: back half under the front, flattened like children
                    t.tex[id] = Some(format!("#{id}"));
                    t.kids.insert(format!("#{id}"), (Some(tex.to_string()), None, Vec::new(), false));
                    t.deco[id] = def.object_type == 7;
                    continue;
                }
                if tex.starts_with("edit_") || tex == "emptyFrame" {
                    continue;
                }
                t.tex[id] = def.texture_name.clone();
                t.hitbox4[id] = def.hitbox.as_ref().is_some_and(|h| h.len() == 4);
                // GD object_type 7 = decoration: drawn behind gameplay
                t.deco[id] = def.object_type == 7;
            }
        }
        t
    }
}

/// Render a sprite once at canvas resolution (tint, rotation, flips, scale
/// baked in), cropped to its alpha bounds. Returns the crop and its top-left
/// offset from the sprite center, in canvas pixels.
fn prescale(
    sp: &Pixmap,
    rot: f32,
    fx: bool,
    fy: bool,
    kx: f32,
    ky: f32,
) -> Option<(Pixmap, f32, f32)> {
    // square on the scaled diagonal: any rotation fits (crop trims it after)
    let k = SCALE / SHEET_SCALE;
    let diag = ((sp.width() as f32 * k * kx).powi(2) + (sp.height() as f32 * k * ky).powi(2)).sqrt();
    let (dw, dh) = (diag.ceil() as u32 + 2, diag.ceil() as u32 + 2);
    let mut off = Pixmap::new(dw, dh)?;
    let tinted = sp;
    let mut paint = PixmapPaint::default();
    paint.quality = FilterQuality::Bilinear;
    let t = Transform::from_translate(dw as f32 / 2.0, dh as f32 / 2.0)
        .pre_concat(Transform::from_rotate(rot))
        .pre_concat(Transform::from_scale(
            (if fx { -1.0 } else { 1.0 }) * SCALE / SHEET_SCALE * kx,
            (if fy { -1.0 } else { 1.0 }) * SCALE / SHEET_SCALE * ky,
        ))
        .pre_concat(Transform::from_translate(
            -(tinted.width() as f32) * 0.5,
            -(tinted.height() as f32) * 0.5,
        ));
    off.draw_pixmap(0, 0, tinted.as_ref(), &paint, t, None);
    // alpha-bounds crop (huge deco sprites carry large transparent margins —
    // blitting those was the frame-time killer)
    let (w, h) = (off.width() as usize, off.height() as usize);
    let data = off.data();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    let mut any = false;
    for j in 0..h {
        for i in 0..w {
            if data[(j * w + i) * 4 + 3] != 0 {
                any = true;
                x0 = x0.min(i);
                x1 = x1.max(i);
                y0 = y0.min(j);
                y1 = y1.max(j);
            }
        }
    }
    if !any {
        return None; // fully transparent
    }
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut crop = Pixmap::new(cw as u32, ch as u32)?;
    for j in 0..ch {
        let s = ((y0 + j) * w + x0) * 4;
        crop.data_mut()[j * cw * 4..(j + 1) * cw * 4].copy_from_slice(&data[s..s + cw * 4]);
    }
    Some((crop, x0 as f32 - dw as f32 / 2.0, y0 as f32 - dh as f32 / 2.0))
}

/// GD per-object HSV (keys 41/43 base, 42/44 detail): "h a s a v a sAdd a vAdd".
/// Hue shifts in degrees; s and v multiply, or add when their flag is 1.
fn obj_hsv(obj: &GdObject, detail: bool) -> Option<[f32; 5]> {
    let (on, spec) = if detail { (42, 44) } else { (41, 43) };
    let get = |k: u16| obj.raw.iter().find(|(key, _)| *key == k).map(|(_, v)| v.as_str());
    if get(on) != Some("1") {
        return None;
    }
    let mut out = [0.0, 1.0, 1.0, 0.0, 0.0];
    for (i, part) in get(spec)?.split('a').take(5).enumerate() {
        out[i] = part.parse().ok()?;
    }
    Some(out)
}

fn apply_hsv((r, g, b): (u8, u8, u8), [dh, s, v, s_add, v_add]: [f32; 5]) -> (u8, u8, u8) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let mut h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let mut sat = if max == 0.0 { 0.0 } else { d / max };
    let mut val = max;
    h = (h + dh).rem_euclid(360.0);
    sat = if s_add != 0.0 { sat + s } else { sat * s }.clamp(0.0, 1.0);
    val = if v_add != 0.0 { val + v } else { val * v }.clamp(0.0, 1.0);
    let c = val * sat;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = val - c;
    let to = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (to(r1), to(g1), to(b1))
}

/// Portal art comes in two halves: `portal_NN_front_001` + `_back_001`.
fn back_of(tex: &str) -> Option<String> {
    tex.contains("_front_").then(|| tex.replace("_front_", "_back_"))
}

/// A child drawn with the object's secondary color: its slot differs from
/// the parent's ("black" children are pre-blackened in the main layer).
fn is_secondary(k: &crate::sim::objects::ChildSprite, parent_slot: Option<&str>) -> bool {
    let c = k.color_channel.as_deref();
    c.is_some() && c != Some("black") && c != parent_slot
}


/// The parent box (sheet px) a child's position is measured from. A copy
/// of the parent's own sprite (a saw's mirrored half, ring segments) is
/// placed in the data's content box: there it sits on the center (205 of
/// 209 such parts); the sprite box put the saw half 1.7 units off and
/// opened a seam. Other children are centered on the sprite box instead
/// (cogs, spikes: the content box pushes their parts off center).
fn child_box(k: &crate::sim::objects::ChildSprite, parent: Option<&str>, pw: f32, ph: f32) -> (f32, f32) {
    let same = parent.is_some_and(|p| k.texture_name.trim_end_matches(".png") == p.trim_end_matches(".png"));
    if same && k.content_x > 0.0 && k.content_y > 0.0 { (k.content_x * SHEET_SCALE, k.content_y * SHEET_SCALE) } else { (pw, ph) }
}

/// `composite` minus the store writes (sprites must be extracted already),
/// so a level's composites can be built in parallel up front.
fn composite_build(
    store: &SpriteStore,
    parent: Option<&str>,
    parent_slot: Option<&str>,
    kids: &[crate::sim::objects::ChildSprite],
    secondary: bool,
) -> Option<Pixmap> {
    let mut layers: Vec<(i32, Pixmap, Transform)> = Vec::new();
    let par = parent.and_then(|t| store.cached(t).cloned());
    let (pw, ph) = par.as_ref().map(|p| (p.width() as f32, p.height() as f32)).unwrap_or((SHEET_SCALE, SHEET_SCALE));
    if let Some(p) = par.filter(|_| !secondary) {
        layers.push((0, p, Transform::from_translate(-pw / 2.0, -ph / 2.0)));
        // shortcut: GD draws the back half behind the player; here both
        // halves sit at the object's z (player always on top)
        if let Some(b) = parent.and_then(back_of).and_then(|b| store.cached(&b).cloned()) {
            let (bw, bh) = (b.width() as f32, b.height() as f32);
            layers.push((-1, b, Transform::from_translate(-bw / 2.0, -bh / 2.0)));
        }
    }
    for k in kids.iter().filter(|k| is_secondary(k, parent_slot) == secondary) {
        let Some(mut sp) = store.cached(&k.texture_name).cloned() else { continue };
        if k.color_channel.as_deref() == Some("black") {
            for px in sp.pixels_mut() {
                *px = tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, px.alpha()).unwrap();
            }
        }
        let (cw, ch) = (sp.width() as f32, sp.height() as f32);
        // cocos: y up, position from the parent's bottom-left, clockwise rot.
        let (bw, bh) = child_box(k, parent, pw, ph);
        let t = Transform::from_translate(k.x * SHEET_SCALE - bw / 2.0, -(k.y * SHEET_SCALE - bh / 2.0))
            .pre_concat(Transform::from_rotate(k.rot))
            .pre_concat(Transform::from_scale(
                k.scale_x * if k.flip_x { -1.0 } else { 1.0 },
                k.scale_y * if k.flip_y { -1.0 } else { 1.0 },
            ))
            .pre_concat(Transform::from_translate(-k.anchor_x * cw, -(1.0 - k.anchor_y) * ch));
        layers.push((k.z, sp, t));
    }
    // canvas symmetric around the parent center (prescale centers sprites)
    let (mut hx, mut hy) = (1.0f32, 1.0f32);
    for (_, sp, t) in &layers {
        let mut pts = [
            tiny_skia::Point::from_xy(0.0, 0.0),
            tiny_skia::Point::from_xy(sp.width() as f32, 0.0),
            tiny_skia::Point::from_xy(0.0, sp.height() as f32),
            tiny_skia::Point::from_xy(sp.width() as f32, sp.height() as f32),
        ];
        t.map_points(&mut pts);
        for p in pts {
            hx = hx.max(p.x.abs());
            hy = hy.max(p.y.abs());
        }
    }
    let (w, h) = ((hx.ceil() as u32 + 1) * 2, (hy.ceil() as u32 + 1) * 2);
    let mut out = Pixmap::new(w, h)?;
    layers.sort_by_key(|l| l.0);
    let mut paint = PixmapPaint::default();
    paint.quality = FilterQuality::Bilinear;
    for (_, sp, t) in &layers {
        let t = Transform::from_translate(w as f32 / 2.0, h as f32 / 2.0).pre_concat(*t);
        out.draw_pixmap(0, 0, sp.as_ref(), &paint, t, None);
    }
    Some(out)
}

/// Integer-offset premultiplied blit: source-over, or additive (GD
/// "blending" channels). Much cheaper than a tiny-skia draw per sprite.
/// `tint` multiplies the (white) sprite: rgb color and opacity, done here so
/// color fades never rebuild prescaled sprites.
/// `d` holds canvas rows band_y..band_y+dh (width dw).
#[allow(clippy::too_many_arguments)]
fn blit(d: &mut [u8], dw: i32, band_y: i32, dh: i32, sprite: &(Pixmap, f32, f32, Vec<(u16, u16)>), x: i32, y: i32, add: bool, tint: (u8, u8, u8, u8)) {
    let (src, spans) = (&sprite.0, &sprite.3);
    // per-component factor scaled by 2^16: c * f >> 16 ~= c * t/255 * a/255
    let k = |t: u8| (t as u32 * tint.3 as u32 * 65536 + 32512) / 65025;
    let f = [k(tint.0), k(tint.1), k(tint.2), k(255)];
    let (sw, sh) = (src.width() as i32, src.height() as i32);
    let (x0, y0) = (x.max(0), y.max(band_y));
    let (x1, y1) = ((x + sw).min(dw), (y + sh).min(band_y + dh));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let s = src.data();
    for j in y0..y1 {
        let (a, b) = spans[(j - y) as usize];
        let (xs, xe) = (x0.max(x + a as i32), x1.min(x + b as i32));
        if xs >= xe {
            continue;
        }
        let srow = (((j - y) * sw + (xs - x)) * 4) as usize;
        let drow = (((j - band_y) * dw + xs) * 4) as usize;
        let n = ((xe - xs) * 4) as usize;
        let (sp, dp) = (&s[srow..srow + n], &mut d[drow..drow + n]);
        blend_row(sp, dp, f, add);
    }
}

/// Composite one premultiplied source row onto the canvas, tinted by `f`
/// (per-channel factor << 16). The skip / copy branches beat a branch-free
/// loop here: most sprite pixels are fully clear or fully opaque.
#[inline]
fn blend_row(sp: &[u8], dp: &mut [u8], f: [u32; 4], add: bool) {
    // 4 pixels at a time with SSE2 (every x86-64 CPU), the rest below
    #[cfg(target_arch = "x86_64")]
    let (sp, dp) = {
        let n = sp.len() / 16 * 16;
        // SAFETY: SSE2 is part of the x86-64 baseline; n is within both slices
        unsafe { blend_sse2(&sp[..n], &mut dp[..n], f, add) };
        (&sp[n..], &mut dp[n..])
    };
    blend_scalar(sp, dp, f, add);
}

/// blend_scalar's exact integer math on 16-bit lanes. The skip / copy
/// branches need none: at alpha 0 the premultiplied source is 0 and
/// d * 256 >> 8 = d; at alpha 255, d * 1 >> 8 = 0 leaves the source.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn blend_sse2(sp: &[u8], dp: &mut [u8], f: [u32; 4], add: bool) {
    use std::arch::x86_64::*;
    // (s * f) >> 16 with f <= 65536: mulhi(s, f & 0xffff) + s * (f >> 16)
    let lo = |c: usize| (f[c] & 0xffff) as u16 as i16;
    let hi = |c: usize| (f[c] >> 16) as i16;
    let flo = _mm_setr_epi16(lo(0), lo(1), lo(2), lo(3), lo(0), lo(1), lo(2), lo(3));
    let fhi = _mm_setr_epi16(hi(0), hi(1), hi(2), hi(3), hi(0), hi(1), hi(2), hi(3));
    let (zero, c256) = (_mm_setzero_si128(), _mm_set1_epi16(256));
    let amask = _mm_set1_epi32(0xff00_0000u32 as i32);
    for (s, d) in sp.chunks_exact(16).zip(dp.chunks_exact_mut(16)) {
        let sv = _mm_loadu_si128(s.as_ptr() as *const __m128i);
        let dv = _mm_loadu_si128(d.as_ptr() as *const __m128i);
        let tint = |x: __m128i| _mm_add_epi16(_mm_mulhi_epu16(x, flo), _mm_mullo_epi16(x, fhi));
        let (tl, th) = (tint(_mm_unpacklo_epi8(sv, zero)), tint(_mm_unpackhi_epi8(sv, zero)));
        let out = if add {
            // rgb: min(255, d + t); alpha: max(d, t)
            let t = _mm_packus_epi16(tl, th);
            let (sum, max) = (_mm_adds_epu8(dv, t), _mm_max_epu8(dv, t));
            _mm_or_si128(_mm_andnot_si128(amask, sum), _mm_and_si128(amask, max))
        } else {
            let over = |t: __m128i, d: __m128i| {
                let a = _mm_shufflehi_epi16(_mm_shufflelo_epi16(t, 0xff), 0xff);
                _mm_add_epi16(t, _mm_srli_epi16(_mm_mullo_epi16(d, _mm_sub_epi16(c256, a)), 8))
            };
            _mm_packus_epi16(over(tl, _mm_unpacklo_epi8(dv, zero)), over(th, _mm_unpackhi_epi8(dv, zero)))
        };
        _mm_storeu_si128(d.as_mut_ptr() as *mut __m128i, out);
    }
}

fn blend_scalar(sp: &[u8], dp: &mut [u8], f: [u32; 4], add: bool) {
    if add {
        for (sc, dc) in sp.chunks_exact(4).zip(dp.chunks_exact_mut(4)) {
            if sc[3] == 0 {
                continue;
            }
            for c in 0..3 {
                dc[c] = (dc[c] as u32 + (sc[c] as u32 * f[c] >> 16)).min(255) as u8;
            }
            dc[3] = dc[3].max((sc[3] as u32 * f[3] >> 16) as u8);
        }
        return;
    }
    let white = f == [65536; 4];
    for (sc, dc) in sp.chunks_exact(4).zip(dp.chunks_exact_mut(4)) {
        if sc[3] == 0 {
            continue;
        }
        let sv = u32::from_le_bytes([sc[0], sc[1], sc[2], sc[3]]);
        // tinted source (premultiplied), one multiply per channel
        let sv = if white {
            sv
        } else {
            u32::from_le_bytes([0, 1, 2, 3].map(|c| (sc[c] as u32 * f[c] >> 16) as u8))
        };
        let a = sv >> 24;
        if a == 255 {
            dc.copy_from_slice(&sv.to_le_bytes());
            continue;
        }
        // SWAR source-over: dst * (255 - a) / 255 two channels per multiply
        // (x * (ia + 1) >> 8: exact at ia = 0 and 255, off by <= 1 between)
        let dv = u32::from_le_bytes([dc[0], dc[1], dc[2], dc[3]]);
        let ia = 256 - a;
        let rb = ((dv & 0x00ff_00ff) * ia >> 8) & 0x00ff_00ff;
        let ag = ((dv >> 8) & 0x00ff_00ff) * ia & 0xff00_ff00;
        dc.copy_from_slice(&(sv + (rb | ag)).to_le_bytes());
    }
}

/// Prescale `key` on the background pool (once: `inflight` holds queued
/// keys); the result reaches the cache at the next frame's drain. On wasm
/// there is no pool: nothing is prefetched, and misses build in-frame
/// (build_sprites) instead.
fn build_ahead(
    pool: &Option<rayon::ThreadPool>,
    inflight: &mut crate::sim::fx::HashSet<SpriteKey>,
    tx: &std::sync::mpsc::Sender<(SpriteKey, Option<Sprite>)>,
    src: &std::sync::Arc<Pixmap>,
    key: SpriteKey,
) {
    if !inflight.insert(key.clone()) {
        return;
    }
    if let Some(pool) = pool {
        let (src, tx) = (src.clone(), tx.clone());
        pool.spawn(move || {
            let (kx, ky) = (f32::from_bits((key.4 >> 32) as u32), f32::from_bits(key.4 as u32));
            let sp = prescale(&src, key.1 as f32 / 1000.0, key.2, key.3, kx, ky)
                .map(|(p, ox, oy)| std::sync::Arc::new({ let spans = opaque_spans(&p); (p, ox, oy, spans) }));
            let _ = tx.send((key, sp));
        });
    }
}

thread_local!(static BUILDS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) });

/// The object's prescaled sprite, built on first use.
/// Text object (914) string: key 31, URL-safe base64.
fn text_of(obj: &GdObject) -> Option<String> {
    use base64::Engine;
    let v = obj.raw.iter().find(|(k, _)| *k == 31)?.1.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(v).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned()).filter(|s| !s.trim().is_empty())
}

fn sprite_key(tex: &str, obj: &GdObject) -> SpriteKey {
    let kx = if obj.scale_x != 0.0 { obj.scale_x.abs() } else { 1.0 };
    let ky = if obj.scale_y != 0.0 { obj.scale_y.abs() } else { 1.0 };
    (
        tex.to_string(),
        (obj.rot * 1000.0).round() as i32,
        obj.flip_x,
        obj.flip_y,
        (kx.to_bits() as u64) << 32 | ky.to_bits() as u64,
    )
}

/// Build missing prescaled sprites in parallel (a section reveal can need
/// thousands in one frame) and insert them into the cache.
fn build_sprites(
    cache: &mut crate::sim::fx::HashMap<SpriteKey, (std::sync::atomic::AtomicU64, Option<Sprite>)>,
    store: &mut SpriteStore,
    keys: Vec<SpriteKey>,
    now: u64,
) {
    // bound memory: drop what was not drawn in the last ~2s, so the live
    // working set (often >4k sprites in GRIEF) never rebuilds at once
    if cache.len() + keys.len() >= 16384 {
        cache.retain(|_, e| e.0.load(std::sync::atomic::Ordering::Relaxed) + 120 >= now);
    }
    BUILDS.with(|b| b.set(b.get() + keys.len() as u32));
    for k in &keys {
        store.sprite(&k.0); // extract (needs &mut); prescale below only reads
    }
    let store = &*store;
    let chunk = keys.len().div_ceil(16).max(1);
    // wasm has no spawnable threads: build the chunks inline (still exactly
    // the same sprites, just sequentially)
    let build = |ks: &[SpriteKey]| {
        ks.iter()
            .map(|(tex, rot, fx, fy, k)| {
                let (kx, ky) = (f32::from_bits((k >> 32) as u32), f32::from_bits(*k as u32));
                store
                    .cached(tex)
                    .and_then(|sp| prescale(sp, *rot as f32 / 1000.0, *fx, *fy, kx, ky))
                    .map(|(p, ox, oy)| {
                        let spans = opaque_spans(&p);
                        std::sync::Arc::new((p, ox, oy, spans))
                    })
            })
            .collect::<Vec<_>>()
    };
    let built: Vec<Vec<Option<Sprite>>> = if cfg!(target_family = "wasm") {
        keys.chunks(chunk).map(build).collect()
    } else {
        std::thread::scope(|sc| {
            let hs: Vec<_> = keys
                .chunks(chunk)
                .map(|ks| {
                    let build = build;
                    sc.spawn(move || build(ks))
                })
                .collect();
            hs.into_iter().map(|h| h.join().expect("prescale thread")).collect()
        })
    };
    for (k, b) in keys.into_iter().zip(built.into_iter().flatten()) {
        cache.insert(k, (now.into(), b));
    }
}

/// One placed color trigger (899 / legacy 29|30): x-cross → lerp a channel.
#[derive(Clone)]
struct ColorCmd {
    x: f32,
    ch: u16,
    to: (f32, f32, f32),
    dur: f32,
    /// key 50: fade to this channel's color instead of `to`
    copy: Option<u16>,
    /// key 35
    opacity: f32,
    /// key 17
    blend: bool,
}

struct Transition {
    ch: u16,
    from: (f32, f32, f32),
    to: (f32, f32, f32),
    from_op: f32,
    to_op: f32,
    t: f32,
    dur: f32,
    /// re-link the channel to this copy source when the fade ends
    copy: Option<u16>,
}

/// Evaluates the level's color triggers (ids 899 / 29 / 30) against the
/// leading player x, so every level's palette comes from its own data.
/// GD's key map: 23 = target channel, 7/8/9 = RGB, 10 = duration.
/// Pulse (1006) / Alpha (1007) triggers and per-channel opacity are not
/// evaluated — channels only.
struct ColorEngine {
    chans: crate::sim::fx::HashMap<u16, (f32, f32, f32)>,
    /// header copy-color links (key 9); a color trigger on the channel cuts it
    copies: crate::sim::fx::HashMap<u16, u16>,
    /// additive-blending channels (key 5)
    blending: crate::sim::fx::HashSet<u16>,
    /// channel opacity (header key 7, trigger key 35); absent = 1
    ops: crate::sim::fx::HashMap<u16, f32>,
    cmds: Vec<ColorCmd>,
    cursor: usize,
    live: Vec<Transition>,
    last_frame: u64,
    /// 2919-2923 color shaders
    shaders: shader::Shaders,
}

fn kind_is_portal(k: crate::sim::objects::ObjectKind) -> bool {
    use crate::sim::objects::ObjectKind as K;
    matches!(
        k,
        K::PortalCube | K::PortalShip | K::PortalBall | K::PortalUfo | K::PortalWave | K::PortalRobot | K::PortalSpider
            | K::PortalSwing | K::PortalSize(_) | K::PortalDual | K::PortalSolo | K::PortalMirror | K::GravityPortal(_)
    )
}

impl ColorEngine {
    fn new(level: &crate::level::Level) -> ColorEngine {
        let mut chans: crate::sim::fx::HashMap<u16, (f32, f32, f32)> = Default::default();
        let mut copies: crate::sim::fx::HashMap<u16, u16> = Default::default();
        let mut blending: crate::sim::fx::HashSet<u16> = Default::default();
        let mut ops: crate::sim::fx::HashMap<u16, f32> = Default::default();
        for c in &level.header.channels {
            ops.insert(c.id, c.opacity);
            chans.insert(c.id, (c.r as f32, c.g as f32, c.b as f32));
            if let Some(src) = c.copy {
                copies.insert(c.id, src);
            }
            if c.blending {
                blending.insert(c.id);
            }
        }
        let raw_kv = |o: &crate::level::GdObject, k: u16| -> Option<f32> {
            o.raw.iter().find(|(kk, _)| *kk == k).and_then(|(_, v)| v.parse().ok())
        };
        let mut cmds: Vec<ColorCmd> = level
            .objects
            .iter()
            .filter_map(|o| {
                // pre-2.0 triggers have one fixed channel each and no key 23
                let legacy = match o.id {
                    29 => 1000, // background
                    30 => 1001, // ground
                    104 => 1002, // line
                    105 => 1004, // object
                    221 => 1,
                    717 => 2,
                    718 => 3,
                    743 => 4,
                    744 => 1003, // 3DL
                    900 => 1009, // ground 2
                    915 => 1002, // line (2.0)
                    899 => 0,
                    _ => return None,
                };
                Some(ColorCmd {
                    x: o.x,
                    ch: if legacy != 0 { legacy } else { raw_kv(o, 23)? as u16 },
                    to: (
                        raw_kv(o, 7).unwrap_or(255.0),
                        raw_kv(o, 8).unwrap_or(255.0),
                        raw_kv(o, 9).unwrap_or(255.0),
                    ),
                    dur: raw_kv(o, 10).unwrap_or(0.0),
                    copy: raw_kv(o, 50).map(|c| c as u16).filter(|&c| c != 0),
                    opacity: raw_kv(o, 35).unwrap_or(1.0),
                    blend: raw_kv(o, 17).is_some_and(|b| b != 0.0),
                })
            })
            .collect();
        cmds.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        ColorEngine {
            chans, copies, blending, ops, cmds, cursor: 0, live: Vec::new(), last_frame: 0,
            shaders: shader::Shaders::new(level),
        }
    }

    fn step(&mut self, player_x: f64, frame: u64) {
        let dt = (frame.saturating_sub(self.last_frame)) as f32 / 240.0;
        self.last_frame = frame;
        while self.cursor < self.cmds.len()
            && (self.cmds[self.cursor].x as f64) <= player_x
        {
            let c = &self.cmds[self.cursor];
            let from = self.resolve(c.ch).unwrap_or((255.0, 255.0, 255.0));
            // shortcut: copy-color fades to the source's color at fire time, then follows it live; copy-HSV (key 49) ignored
            let to = c.copy.and_then(|src| self.resolve(src)).unwrap_or(c.to);
            self.copies.remove(&c.ch);
            if c.blend {
                self.blending.insert(c.ch);
            } else {
                self.blending.remove(&c.ch);
            }
            let from_op = self.ops.get(&c.ch).copied().unwrap_or(1.0);
            self.live.retain(|tr| tr.ch != c.ch); // a new trigger on the channel replaces the running fade
            self.live.push(Transition { ch: c.ch, from, to, from_op, to_op: c.opacity, t: 0.0, dur: c.dur, copy: c.copy });
            self.cursor += 1;
        }
        self.shaders.step(player_x, dt);
        let (live, chans, ops, copies) = (&mut self.live, &mut self.chans, &mut self.ops, &mut self.copies);
        live.retain_mut(|tr| {
            tr.t += dt;
            let k = if tr.dur <= 0.0 { 1.0 } else { (tr.t / tr.dur).min(1.0) };
            let col = (
                tr.from.0 + (tr.to.0 - tr.from.0) * k,
                tr.from.1 + (tr.to.1 - tr.from.1) * k,
                tr.from.2 + (tr.to.2 - tr.from.2) * k,
            );
            chans.insert(tr.ch, col);
            ops.insert(tr.ch, tr.from_op + (tr.to_op - tr.from_op) * k);
            let done = tr.t >= tr.dur;
            if done {
                if let Some(src) = tr.copy {
                    copies.insert(tr.ch, src);
                }
            }
            !done
        });
    }

    /// live color of a channel: follows copy links; 1010 is GD's fixed black
    fn resolve(&self, mut id: u16) -> Option<(f32, f32, f32)> {
        for _ in 0..8 {
            if id == 1010 {
                return Some((0.0, 0.0, 0.0));
            }
            match self.copies.get(&id) {
                Some(&src) => id = src,
                None => break,
            }
        }
        self.chans.get(&id).copied()
    }

    fn snapshot(&self) -> crate::sim::fx::HashMap<u16, (u8, u8, u8)> {
        self.chans
            .keys()
            .chain(self.copies.keys())
            .chain(std::iter::once(&1010))
            .filter_map(|&k| self.resolve(k).map(|(r, g, b)| (k, (r as u8, g as u8, b as u8))))
            .collect()
    }
}

/// Frame-time clock for PROFILE laps: seconds since process start.
/// wasm32-unknown-unknown has no clock (Instant panics there): 0.0, and
/// the laps are then all zero — PROFILE is a native-only debugging aid.
#[cfg(not(target_family = "wasm"))]
fn now_secs() -> f64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
}
#[cfg(target_family = "wasm")]
fn now_secs() -> f64 {
    0.0
}

impl Renderer {
    /// Missing sheets degrade to hitbox-outline rendering (see SpriteStore::load).
    pub fn new(assets: &Assets) -> Renderer {
        Renderer {
            store: SpriteStore::load(assets),
            bg: None,
            fps: None,
            prescaled: Default::default(),
            frame_no: 0,
            obj_layers: Vec::new(),
            cam: None,
            spin: Vec::new(),
            fx: Vec::new(),
            prev_touch: Vec::new(),
            sorted: None,
            sorted_key: 0,
            colors: None,
            colors_key: (0, 0),
            ids: None,
            icons: Default::default(),
            gfx: None,
            fxs: play_fx::State::default(),
            ground: None,
            floor_line: None,
            ahead: { let (tx, rx) = std::sync::mpsc::channel(); (Default::default(), tx, rx) },
            // 2 threads: big builds must not take the frame's own workers.
            // wasm has no spawnable threads: None (misses build in-frame)
            ahead_pool: if cfg!(target_family = "wasm") {
                None
            } else {
                Some(rayon::ThreadPoolBuilder::new().num_threads(2).build().expect("build pool"))
            },
            counters: (u64::MAX, Vec::new()),
            orbs: (0, Vec::new()),
            menu: None,
        }
    }

    /// Show (Some) or hide (None) a centered menu panel, one line per entry.
    pub fn set_menu(&mut self, lines: Option<Vec<String>>) {
        self.menu = lines;
    }

    /// Show an fps counter (3x5 digits, top-right).
    pub fn set_fps(&mut self, fps: u32) {
        self.fps = Some(fps);
    }

    /// Render one frame following player 0. Returns opaque RGBA,
    /// `FRAME_W * FRAME_H * 4` bytes (ready for Streamer::submit).
    pub fn draw(&mut self, level: &Level, table: &ObjectTable, sim: &Sim) -> Vec<u8> {
        let mut out = Vec::with_capacity(FRAME_W as usize * FRAME_H as usize * 4);
        self.draw_into(level, table, sim, &mut out);
        out
    }

    /// `draw` into a caller-owned buffer (the browser build keeps one wasm
    /// buffer alive instead of allocating a fresh frame each rAF).
    pub fn draw_into(&mut self, level: &Level, table: &ObjectTable, sim: &Sim, out: &mut Vec<u8>) {
        let prof = std::env::var("PROFILE").is_ok();
        let t_start = now_secs();
        // PROFILE: named laps, one line per frame
        let mut laps: Vec<(&str, f64)> = Vec::new();
        let mut lap = |name: &'static str| if prof { laps.push((name, now_secs())) };
        let mut canvas = Pixmap::new(RW, RH).expect("static size");
        let view_w = RW as f32 / SCALE;

        // camera: x puts player 0 at 40% from the left; y see camera_y
        let px = sim.players.first().map_or(0.0, |p| p.x as f32);
        let lead_x = sim.players.iter().map(|p| p.x).fold(f64::MIN, f64::max);
        // camera triggers (offset, shake, zoom) on top of the follow camera
        let (zoom, cam_off) = self.fxs.camera(level, sim, lead_x);
        let camx = px - view_w * 0.4 + cam_off.0;
        let static_y = self.fxs.cam.as_ref().and_then(|c| c.static_y);
        let camy = self.camera_y(sim, static_y) + cam_off.1;
        let store_dir = self.store.assets().clone();
        let gfx = self.gfx.get_or_insert_with(|| play_fx::Gfx::load(&store_dir));
        self.fxs.update(gfx, sim, level, (camx, camx + view_w, camy));

        lap("cam");
        // live channel colors: driven by the level's own color triggers
        // (899/29/30), evaluated against the leading player x
        // a respawn rewinds the player: replay the color triggers from the
        // header (deco slabs bake colors, so they rebuild too)
        let colors_key = (level.uid, sim.attempt);
        if self.colors_key != colors_key {
            self.colors = Some(ColorEngine::new(level));
            self.colors_key = colors_key;
        }
        let colors = self.colors.as_mut().expect("color engine");
        colors.step(lead_x, sim.frame);
        let chan_map = colors.snapshot();
        let shader = colors.shaders.now();
        let blending = colors.blending.clone();
        // additive blend of black adds nothing: such objects are invisible
        let invisible = |cid: u16, rgb: (u8, u8, u8)| rgb == (0, 0, 0) && blending.contains(&cid);
        let chan = |cid: u16| -> (u8, u8, u8) {
            chan_map.get(&cid).copied().unwrap_or((255, 255, 255))
        };
        // live channel opacity (header, faded by color triggers); a copy keeps
        // its own opacity (GD's "copy opacity" flag, key 60, is not modeled)
        let chan_op: crate::sim::fx::HashMap<u16, f32> =
            colors.ops.iter().filter(|&(_, &o)| o < 1.0).map(|(&c, &o)| (c, o)).collect();
        let bg_rgb = chan(1000);

let to_screen = |wx: f32, wy: f32| -> (f32, f32) {
            ((wx - camx) * SCALE, RH as f32 * 0.5 - (wy - camy) * SCALE)
        };

        lap("colors");
        // tiled background texture: kA6 bg id → game_bg_NN-uhd; one 1x
        // texture tile spans 512u, slow horizontal parallax, bottom of the
        // tile row anchored at the floor line
        let bg_id: u32 = level
            .header
            .ka
            .iter()
            .find(|(k, _)| k == "kA6")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(1)
            .max(1); // 0 = default bg 1 (old levels)
        let bg_name = format!("game_bg_{bg_id:02}_001-uhd.png");
        if self.bg.as_ref().map(|(n, _)| n != &bg_name).unwrap_or(true) {
            // scaled once to its on-screen size (one tile = 512u)
            let tex = self.store.png(&bg_name).and_then(|t| {
                let s = 512.0 * SCALE / t.width() as f32;
                let (w, h) = ((t.width() as f32 * s).round() as u32, (t.height() as f32 * s).round() as u32);
                let mut p = Pixmap::new(w.max(1), h.max(1))?;
                let mut paint = PixmapPaint::default();
                paint.quality = FilterQuality::Bicubic;
                let tf = Transform::from_scale(w as f32 / t.width() as f32, h as f32 / t.height() as f32);
                p.draw_pixmap(0, 0, t.as_ref(), &paint, tf, None);
                Some(p)
            });
            self.bg = Some((bg_name, tex));
        }
        // one pass: texture strip (slow parallax, bottom on the floor line)
        // tinted by the live bg channel, under GRIEF's 36-band depth gradient
        // drawn at alpha 235 (darker up top)
        {
            let tex = self.bg.as_ref().and_then(|b| b.1.as_ref());
            let (_, ty) = to_screen(0.0, crate::physics::GROUND_Y as f32 + 512.0);
            let ty = ty.round() as i32;
            let (tw, th) = tex.map_or((1, 0), |t| (t.width() as usize, t.height() as i32));
            let scroll = (camx * 0.125).rem_euclid(tw as f32) as usize;
            let bgc = [bg_rgb.0 as u32, bg_rgb.1 as u32, bg_rgb.2 as u32];
            let band_h = RH as f32 / 36.0;
            let grief = level.id == 149992717;
            let w = RW as usize;
            use rayon::prelude::*;
            canvas.data_mut().par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
                // GD: texture tinted by the bg channel. GRIEF alone keeps its
                // calibrated dark look (36-band gradient, texture at 20/255).
                // shortcut: that look is keyed on GRIEF's id; find what in the
                // level makes it dark if another level needs it
                let m = 0.12 + (((y as f32 / band_h) as usize).min(35) as f32 / 35.0) * 0.33;
                let add = if grief { bgc.map(|c| (c as f32 * m) as u32 * 235 / 255) } else { [0; 3] };
                let ty_row = y as i32 - ty;
                match tex.filter(|_| (0..th).contains(&ty_row)) {
                    Some(t) => {
                        // tex * bg/255 * 20/255, as a 16.16 factor per channel
                        let f = bgc.map(|c| if grief { c * 20 * 65536 / (255 * 255) } else { c * 65536 / 255 });
                        let src = &t.data()[ty_row as usize * tw * 4..(ty_row as usize + 1) * tw * 4];
                        // the texture row repeats every tw px: copy it in runs, no per-pixel modulo
                        let (mut x, mut sx) = (0, scroll % tw);
                        while x < w {
                            let n = (tw - sx).min(w - x);
                            for (d, sp) in row[x * 4..(x + n) * 4].chunks_exact_mut(4).zip(src[sx * 4..(sx + n) * 4].chunks_exact(4)) {
                                for c in 0..3 {
                                    d[c] = (add[c] + (sp[c] as u32 * f[c] >> 16)) as u8;
                                }
                                d[3] = 255;
                            }
                            (x, sx) = (x + n, 0);
                        }
                    }
                    None => {
                        let px = [0, 1, 2].map(|c| if grief { (add[c] + bgc[c] * 20 / 255) as u8 } else { bgc[c] as u8 });
                        for d in row.chunks_exact_mut(4) {
                            d.copy_from_slice(&[px[0], px[1], px[2], 255]);
                        }
                    }
                }
            });
        }

        // ground / ceiling: GD's ground tile (kA7) tinted by the live ground
        // channel 1001, scrolling with the world; the ceiling mirrors it.
        // Floor line sprite (kA17) in the line channel 1002, additive.
        let ka = |k: &str| level.header.ka.iter().find(|(kk, _)| kk == k).and_then(|(_, v)| v.parse::<u32>().ok());
        let gid = ka("kA7").unwrap_or(0).max(1);
        if self.ground.as_ref().map(|g| g.0) != Some(gid) {
            let tile = self.store.png(&format!("groundSquare_{gid:02}_001-uhd.png")).and_then(|t| {
                // one tile = 128 units wide, like GD
                let s = 128.0 * SCALE / t.width() as f32;
                let (w, h) = ((t.width() as f32 * s).round() as u32, (t.height() as f32 * s).round() as u32);
                let mut p = Pixmap::new(w.max(1), h.max(1))?;
                let paint = PixmapPaint { quality: FilterQuality::Bicubic, ..Default::default() };
                p.draw_pixmap(0, 0, t.as_ref(), &paint, Transform::from_scale(s, s), None);
                Some(p)
            });
            self.ground = Some((gid, tile));
        }
        let lid = ka("kA17").unwrap_or(0);
        if self.floor_line.as_ref().map(|l| l.0) != Some(lid) {
            let name = if lid == 0 { "floorLine_01_001.png" } else { "floorLine_02_001.png" };
            let line = self.store.sprite(name).and_then(|sp| {
                // spans the view width, fading at both ends (the sprite's own alpha)
                let s = RW as f32 / sp.width() as f32;
                let h = (sp.height() as f32 * s).ceil().max(1.0) as u32;
                let mut p = Pixmap::new(RW, h)?;
                let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() };
                p.draw_pixmap(0, 0, sp.as_ref(), &paint, Transform::from_scale(s, s), None);
                Some(p)
            });
            self.floor_line = Some((lid, line));
        }
        let ground_rgb = chan(1001);
        let line_rgb = chan(1002);
        let (_, gy) = to_screen(0.0, crate::physics::GROUND_Y as f32);
        let (_, cy) = to_screen(0.0, crate::physics::CEIL_Y as f32);
        if self.sorted_key != level.uid {
            self.prebuild_composites(level, table);
        }
        let table_key = table as *const ObjectTable as usize;
        if self.ids.as_ref().map(|(k, _)| *k != table_key).unwrap_or(true) {
            self.ids = Some((table_key, IdTables::new(table)));
        }
        let ids = &self.ids.as_ref().expect("id tables").1;


        lap("bg");
        let t_bg = now_secs() - t_start;
        let skip_deco = std::env::var("SKIP_DECO").is_ok();
        if self.sorted_key != level.uid {
            let mut v: Vec<(f32, usize)> =
                level.objects.iter().enumerate().map(|(i, o)| (o.x, i)).collect();
            v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            self.sorted = Some(v);
            self.sorted_key = level.uid;
            // text objects: each distinct string becomes a white texture in
            // bigFont (uhd px = sheet px), then draws like any sprite
            // font: the level's kA18 (0 = bigFont, n = gjFont<n>)
            let font_id: u32 = level.header.ka.iter().find(|(k, _)| k == "kA18").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
            let fnt = if font_id == 0 { "bigFont-uhd.fnt".to_string() } else { format!("gjFont{font_id:02}-uhd.fnt") };
            let level_font = fx::Font::load(self.store.assets(), &fnt);
            if let Some(font) = level_font.as_ref().or_else(|| self.gfx.as_ref().and_then(|g| g.font.as_ref())) {
                for s in level.objects.iter().filter(|o| o.id == 914).filter_map(text_of) {
                    let name = format!("text:{s}");
                    if self.store.cached(&name).is_some() {
                        continue;
                    }
                    let (w, h) = (font.width(&s).ceil() as u32 + 4, font.line_h.ceil() as u32 + 4);
                    if let Some(mut pm) = Pixmap::new(w, h) {
                        font.draw(&mut pm, &s, w as f32 / 2.0, h as f32 / 2.0, 1.0, 1.0);
                        self.store.insert(&name, pm);
                    }
                }
            }
            // two color layers: secondary (key 22) under main (key 21); each
            // falls back to the object's table default channel
            self.obj_layers = level
                .objects
                .iter()
                .map(|obj| {
                    if obj.id == 914 {
                        // text: white unless the level colors it (key 21)
                        return [None, text_of(obj).map(|s| ObjLayer {
                            key: sprite_key(&format!("text:{s}"), obj),
                            cid: obj.base_color.unwrap_or(0),
                            hsv: obj_hsv(obj, false),
                        })];
                    }
                    let id = obj.id as usize;
                    let tex = ids.tex[id].as_deref();
                    let (def_main, def_sec) = ids.chans[id];
                    let sec_tex =
                        tex.filter(|t| ids.kids.get(*t).is_some_and(|k| k.3)).map(|t| format!("{t}:d"));
                    let sec = sec_tex.zip(obj.detail_color.or(def_sec)).map(|(t, cid)| ObjLayer {
                        key: sprite_key(&t, obj),
                        cid,
                        hsv: obj_hsv(obj, true),
                    });
                    let main = tex.map(|t| ObjLayer {
                        key: sprite_key(t, obj),
                        cid: obj.base_color.unwrap_or(def_main),
                        hsv: obj_hsv(obj, false),
                    });
                    [sec, main]
                })
                .collect();
        }
        let sorted = self.sorted.as_ref().unwrap();
        let x_lo = camx - CULL_PAD;
        let x_hi = camx + view_w + CULL_PAD;
        let lo = sorted.partition_point(|(x, _)| *x < x_lo);
        let hi = sorted.partition_point(|(x, _)| *x <= x_hi);
        self.frame_no += 1;
        while let Ok((k, sp)) = self.ahead.2.try_recv() {
            self.ahead.0.remove(&k);
            self.prescaled.entry(k).or_insert((self.frame_no.into(), sp));
        }
        // (sprite, or index into `missing` until built; screen center; additive; tint)
        let mut draws: Vec<(Result<Sprite, usize>, f32, f32, bool, (u8, u8, u8, u8))> = Vec::new();
        let mut missing: Vec<SpriteKey> = Vec::new();
        let mut pending: crate::sim::fx::HashMap<SpriteKey, usize> = Default::default();
        // GD draw order: z-layer, then additive (blending) objects under the
        // layer's normal ones, then z-order, then level order
        // (decorations included: pulse/alpha/color triggers hit them live)
        let obj_layers = &self.obj_layers;
        let mut vis: Vec<(i32, bool, i32, usize)> = sorted[lo..hi]
            .iter()
            .filter(|(_, oi)| !skip_deco || !ids.deco[level.objects[*oi].id as usize])
            .map(|&(_, oi)| {
                let o = &level.objects[oi];
                let (dl, dz) = ids.z[o.id as usize];
                let add = obj_layers[oi].iter().flatten().any(|l| blending.contains(&l.cid));
                (o.z_layer.map(i32::from).unwrap_or(dl), !add, o.z_order.map(i32::from).unwrap_or(dz), oi)
            })
            .collect();
        vis.sort_unstable();
        lap("vis");
        // phase 1, parallel and read-only: each visible object's layers
        // (trigger offset, tint, cached sprite). Phase 2, in draw order:
        // cache misses and the no-texture outline.
        enum Lay {
            Hidden,
            Hit(Option<Sprite>, f32, f32, bool, (u8, u8, u8, u8)),
            // spin: the object's editor rotation when it is a spinning saw
            Miss(SpriteKey, f32, f32, bool, (u8, u8, u8, u8), Option<f32>),
        }
        let frame_no = self.frame_no;
        let (obj_layers, prescaled) = (&self.obj_layers, &self.prescaled);
        let t = sim.frame as f32 / 240.0;
        let per_obj: Vec<Option<(usize, f64, f64, [Option<Lay>; 2])>> = {
            use rayon::prelude::*;
            vis.par_iter()
                .map(|&(_, _, _, oi)| {
                    let obj = &level.objects[oi];
                    // live trigger state: toggled-off objects vanish, moved ones shift.
                    // shortcut: rotate-trigger turns are not drawn (sprites stay at
                    // their editor angle) and objects moved further than CULL_PAD from
                    // their editor x are culled; upgrade if a level shows it.
                    let (dx, dy, _) = sim.triggers.transform(oi)?;
                    if sim.coins.contains(&oi) || obj.hidden {
                        return None; // collected this attempt / hidden in game (key 135)
                    }
                    let wx = obj.x + dx as f32;
                    let wy = obj.y + crate::physics::EDITOR_Y0 as f32 + dy as f32;
                    let id = obj.id as usize;
                    let alpha = sim.triggers.alpha(oi);
                    let lays = [0, 1].map(|li| {
                        let layer = obj_layers[oi][li].as_ref()?;
                        let (cid, hsv) = (layer.cid, layer.hsv);
                        let mut tint = if cid == 0 { (255, 255, 255) } else { chan(cid) };
                        // pulse triggers blend the channel color toward theirs, then
                        // the object's own HSV applies on top; weights in 1/16 steps
                        // so fades reuse prescaled sprites
                        // uncolorable art (cid 0: portals, orbs, pads) ignores pulses, as in GD
                        for (c, w) in sim.triggers.pulses(oi, cid).filter(|_| cid != 0) {
                            let to = match c {
                                PulseColor::Rgb(r, g, b) => (r, g, b),
                                PulseColor::Hsv(h) => apply_hsv(tint, h),
                            };
                            let w = (w * 16.0).round() / 16.0;
                            let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * w).round() as u8;
                            tint = (mix(tint.0, to.0), mix(tint.1, to.1), mix(tint.2, to.2));
                        }
                        if let Some(h) = hsv {
                            tint = apply_hsv(tint, h);
                        }
                        // opacity: alpha-trigger groups x static channel opacity
                        let op = alpha * chan_op.get(&cid).copied().unwrap_or(1.0);
                        let op = ((op.clamp(0.0, 1.0) * 16.0).round() * 255.0 / 16.0) as u8;
                        if invisible(cid, tint) || op == 0 {
                            return Some(Lay::Hidden); // invisible on purpose: no fallback outline
                        }
                        let add = blending.contains(&cid);
                        let tint = (tint.0, tint.1, tint.2, op);
                        // composites were all built at level load (prebuild_composites)
                        let mut key = &layer.key;
                        let live_key;
                        // shortcut: one spin rate (1 turn/s, 10° steps) for every saw;
                        // GD's per-object rotation speeds are not in the object table
                        if ids.spin[id] {
                            let ang = ((t * 360.0 / 10.0) as i32 * 10) as f32;
                            live_key = (key.0.clone(), (((obj.rot + ang).rem_euclid(360.0)) * 1000.0).round() as i32, key.2, key.3, key.4);
                            key = &live_key;
                        } else if matches!(obj.id, 142 | 1329) {
                            // coin: GD's 4-frame shine loop
                            let n = 1 + (t * 10.0) as usize % 4;
                            live_key = (key.0.replace("_001", &format!("_00{n}")), key.1, key.2, key.3, key.4);
                            key = &live_key;
                        }
                        let (cx, cy) = to_screen(wx, wy);
                        Some(match prescaled.get(key) {
                            Some(e) => {
                                e.0.store(frame_no, std::sync::atomic::Ordering::Relaxed);
                                Lay::Hit(e.1.clone(), cx, cy, add, tint)
                            }
                            None => Lay::Miss(key.clone(), cx, cy, add, tint, ids.spin[id].then_some(obj.rot)),
                        })
                    });
                    Some((oi, dx, dy, lays))
                })
                .collect()
        };
        for (oi, dx, dy, lays) in per_obj.into_iter().flatten() {
            let obj = &level.objects[oi];
            let id = obj.id as usize;
            let mut drawn = false;
            for lay in lays.into_iter().flatten() {
                match lay {
                    Lay::Hidden => drawn = true,
                    Lay::Hit(None, ..) => {}
                    Lay::Hit(Some(sp), cx, cy, add, tint) => {
                        drawn = true;
                        // cull on the sprite's own pixel rect, as blit clips: a
                        // size-around-center test culled trimmed art whose pixels
                        // sit off center while they were still on screen
                        let (x0, y0) = ((cx + sp.1).round(), (cy + sp.2).round());
                        if x0 + sp.0.width() as f32 <= 0.0 || x0 >= RW as f32 || y0 + sp.0.height() as f32 <= 0.0 || y0 >= RH as f32 {
                            continue;
                        }
                        draws.push((Ok(sp), cx, cy, add, tint));
                    }
                    Lay::Miss(key, cx, cy, add, tint, spin) => {
                        let key = &key;
                        drawn = true;
                        // a saw needs a new angle every 10° step: queue its next
                        // 36 steps (the key the spin code above will form at each)
                        // so they are cached by the time they show
                        if let (Some(rot), Some(src)) = (spin, self.store.sprite(&key.0)) {
                            let src = std::sync::Arc::new(src.clone());
                            let s0 = (t * 360.0 / 10.0) as i32;
                            for step in s0 + 1..=s0 + 36 {
                                let k = (key.0.clone(), (((rot + (step * 10) as f32).rem_euclid(360.0)) * 1000.0).round() as i32, key.2, key.3, key.4);
                                if !self.prescaled.contains_key(&k) {
                                    build_ahead(&self.ahead_pool, &mut self.ahead.0, &self.ahead.1, &src, k);
                                }
                            }
                        }
                        // off the canvas even at its largest (any rotation fits in
                        // prescale's diagonal square): blit would clip all of it, so
                        // build it in the background for the frames that show it
                        if let Some(src) = self.store.sprite(&key.0) {
                            let (kx, ky) = (f32::from_bits((key.4 >> 32) as u32), f32::from_bits(key.4 as u32));
                            let k = SCALE / SHEET_SCALE;
                            let diag = ((src.width() as f32 * k * kx).powi(2) + (src.height() as f32 * k * ky).powi(2)).sqrt();
                            let half = (diag.ceil() + 2.0) / 2.0 + 2.0;
                            if cx + half < 0.0 || cx - half > RW as f32 || cy + half < 0.0 || cy - half > RH as f32 {
                            build_ahead(&self.ahead_pool, &mut self.ahead.0, &self.ahead.1, &std::sync::Arc::new(src.clone()), key.clone());
                                continue;
                            }
                        }
                        // not built yet: queue it, keep its slot in draw order
                        let at = *pending.entry(key.clone()).or_insert(missing.len());
                        if at == missing.len() {
                            missing.push(key.clone());
                        }
                        draws.push((Err(at), cx, cy, add, tint));
                    }
                }
            }
            if !drawn {
                // no texture: outline the hitbox so nothing silently disappears
                {
                    if ids.hitbox4[id] {
                        let so = sim_object(obj, table, 0);
                        let (rx, ry, rw, rh) = so.rect;
                        let (rx, ry) = (rx + dx, ry + dy);
                        let (x0, y0) = to_screen(rx as f32, ry as f32 + rh as f32);
                        let wpx = rw as f32 * SCALE;
                        let hpx = rh as f32 * SCALE;
                        if x0 + wpx < 0.0 || x0 > FRAME_W as f32 || hpx <= 0.0 || wpx <= 0.0 {
                            continue;
                        }
                        if let Some(r) = Rect::from_xywh(x0, y0, wpx, hpx) {
                            let mut pb = PathBuilder::new();
                            pb.push_rect(r);
                            if let Some(path) = pb.finish() {
                                let mut paint = Paint::default();
                                paint.shader = tiny_skia::Shader::SolidColor(
                                    Color::from_rgba8(255, 255, 255, 110),
                                );
                                let stroke = Stroke { width: 2.0, ..Default::default() };
                                canvas.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                            }
                        }
                    }
                }
            }
        }

        lap("list");
        let t_list = now_secs() - t_start;
        let keys = missing.clone();
        build_sprites(&mut self.prescaled, &mut self.store, missing, self.frame_no);
        let draws: Vec<(Sprite, i32, i32, bool, (u8, u8, u8, u8))> = draws
            .into_iter()
            .filter_map(|(sp, cx, cy, add, tint)| {
                let sp = match sp {
                    Ok(sp) => sp,
                    Err(i) => self.prescaled.get(&keys[i])?.1.clone()?,
                };
                let (x, y) = ((cx + sp.1).round() as i32, (cy + sp.2).round() as i32);
                Some((sp, x, y, add, tint))
            })
            .collect();
        lap("build");
        let t_built = now_secs() - t_start;
        // blit the draw list in parallel horizontal bands (order kept per band)
        {
            // small bands: dense rows (block fields) no longer leave cores idle
            use rayon::prelude::*;
            const BAND_H: usize = 16;
            let w = RW as usize;
            let draws = &draws;
            canvas.data_mut().par_chunks_mut(BAND_H * w * 4).enumerate().for_each(|(i, rows)| {
                let y0 = (i * BAND_H) as i32;
                let h = (rows.len() / (w * 4)) as i32;
                for (sp, x, y, add, tint) in draws {
                    blit(rows, w as i32, y0, h, sp, *x, *y, *add, *tint);
                }
            });
        }
        lap("blit");
        // ground and ceiling after the objects: GD draws them in front, so
        // whatever reaches below the floor (floor saws, gears) is hidden
        if sim.options & crate::sim::triggers::OPT_HIDE_GROUND == 0 {
            let w = RW as usize;
            let (gy_i, cy_i) = (gy.round().clamp(0.0, RH as f32) as usize, cy.round().clamp(0.0, RH as f32) as usize);
            let tile = self.ground.as_ref().and_then(|g| g.1.as_ref());
            let gc = [ground_rgb.0 as u32, ground_rgb.1 as u32, ground_rgb.2 as u32];
            let scroll = (camx * SCALE).rem_euclid(tile.map_or(1, |t| t.width()) as f32) as usize;
            let data = canvas.data_mut();
            // rows below the floor (ty counted down from it) and above the
            // ceiling (counted up; it wins where both apply, as drawn last)
            use rayon::prelude::*;
            data.par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
                let ty = if y < cy_i { cy_i - 1 - y } else if y >= gy_i { y - gy_i } else { return };
                match tile {
                    Some(t) => {
                        let (tw, th) = (t.width() as usize, t.height() as usize);
                        let src = &t.data()[(ty % th) * tw * 4..(ty % th + 1) * tw * 4];
                        // the tile row repeats every tw px: runs, no per-pixel modulo
                        let (mut x, mut sx) = (0, scroll % tw);
                        while x < w {
                            let n = (tw - sx).min(w - x);
                            for (d, sp) in row[x * 4..(x + n) * 4].chunks_exact_mut(4).zip(src[sx * 4..(sx + n) * 4].chunks_exact(4)) {
                                for c in 0..3 {
                                    d[c] = (sp[c] as u32 * gc[c] / 255) as u8;
                                }
                                d[3] = 255;
                            }
                            (x, sx) = (x + n, 0);
                        }
                    }
                    None => row.chunks_exact_mut(4).for_each(|d| d.copy_from_slice(&[gc[0] as u8, gc[1] as u8, gc[2] as u8, 255])),
                }
            });
            let line = self.floor_line.as_ref().and_then(|l| l.1.as_ref());
            for ly in [gy, cy] {
                match line {
                    Some(l) => {
                        let lc = [line_rgb.0 as u32, line_rgb.1 as u32, line_rgb.2 as u32];
                        let top = ly.round() as i32 - l.height() as i32 / 2;
                        for (ry, srow) in l.data().chunks_exact(w * 4).enumerate() {
                            let y = top + ry as i32;
                            if !(0..RH as i32).contains(&y) {
                                continue;
                            }
                            let drow = &mut data[y as usize * w * 4..(y as usize + 1) * w * 4];
                            for (d, sp) in drow.chunks_exact_mut(4).zip(srow.chunks_exact(4)) {
                                for c in 0..3 {
                                    // additive: premultiplied sprite luminance x line color
                                    d[c] = (d[c] as u32 + sp[c] as u32 * lc[c] / 255).min(255) as u8;
                                }
                            }
                        }
                    }
                    None => {
                        let y = (ly.round() as i32 - 2).clamp(0, RH as i32 - 2) as usize;
                        for d in data[y * w * 4..(y + 2) * w * 4].chunks_exact_mut(4) {
                            d.copy_from_slice(&[line_rgb.0, line_rgb.1, line_rgb.2, 255]);
                        }
                    }
                }
            }
        }

        lap("ground");
        let t_objs = now_secs() - t_start - t_bg;
        // mirror portal: the whole world view flips left-right (HUD drawn after)
        // shortcut: instant flip; GD animates it over ~0.5 s
        if sim.mirror {
            use rayon::prelude::*;
            // reverse the row's bytes, then each pixel's bytes back: pixel order flipped
            canvas.data_mut().par_chunks_exact_mut(RW as usize * 4).for_each(|row| {
                row.reverse();
                row.chunks_exact_mut(4).for_each(|p| p.reverse());
            });
        }
        // shortcut: the shader tints the world canvas only; particles and
        // players drawn later on the output frame are not tinted
        shader::apply(canvas.data_mut(), shader);
        if prof {
            eprintln!("prof-objs: vis {} draws {} list {:?} build {:?} blit {:?}", vis.len(), draws.len(), t_list - t_bg, t_built - t_list, now_secs() - t_start - t_built);
        }
        lap("shader");
        // hud: progress bar + %, fps, death overlay (attempt text + effect)
        let prog = sim.progress();
        let bar_w = (RW as f32 * 0.6) as f32;
        let mut paint = Paint::default();
        paint.anti_alias = false;
        if let Some(r) = Rect::from_xywh((RW as f32 - bar_w) / 2.0, 8.0, bar_w, 6.0) {
            paint.shader = tiny_skia::Shader::SolidColor(Color::from_rgba8(255, 255, 255, 40));
            canvas.fill_rect(r, &paint, Transform::identity(), None);
        }
        if let Some(r) = Rect::from_xywh(
            (RW as f32 - bar_w) / 2.0,
            8.0,
            bar_w * (prog / 100.0).clamp(0.0, 1.0) as f32,
            6.0,
        ) {
            paint.shader = tiny_skia::Shader::SolidColor(Color::from_rgba8(0, 255, 60, 220));
            canvas.fill_rect(r, &paint, Transform::identity(), None);
        }
        let pct = format!("{:.2}%", prog);
        draw_text(&mut canvas, &pct, RW as f32 - 16.0, 24.0, 4.0, Align::Right, (255, 255, 255));
        if let Some(fps) = self.fps {
            draw_text(
                &mut canvas,
                &fps.to_string(),
                RW as f32 - 16.0,
                8.0,
                4.0,
                Align::Right,
                (200, 200, 200),
            );
        }
        let t_pl = now_secs() - t_start - t_bg - t_objs;


        lap("hud");
        // a zoom trigger > 1 samples a smaller centered window (nearest)
        let zoomed = if zoom > 1.001 {
            let src = canvas.data();
            let mut out = vec![0u8; (FRAME_W * FRAME_H * 4) as usize];
            let ow = FRAME_W as usize;
            let map = |o: usize, n: u32| ((n as f32 / 2.0 + (o as f32 + 0.5 - n as f32 / 2.0) / zoom) as usize).min(n as usize - 1);
            let xs: Vec<usize> = (0..ow).map(|x| map(x, RW)).collect();
            for (oy, orow) in out.chunks_exact_mut(ow * 4).enumerate() {
                let sy = map(oy, RH);
                let row = &src[sy * RW as usize * 4..(sy + 1) * RW as usize * 4];
                for (d, &sx) in orow.chunks_exact_mut(4).zip(&xs) {
                    d.copy_from_slice(&row[sx * 4..sx * 4 + 4]);
                }
            }
            out
        } else {
            canvas.take()
        };
        // trail + player at full output resolution, filtered: the 2x
        // nearest upscale above made them blocky
        let mut outpm = Pixmap::from_vec(zoomed, tiny_skia::IntSize::from_wh(FRAME_W, FRAME_H).expect("frame size"))
            .expect("frame buffer");
        let k = FRAME_W as f32 / RW as f32;
        let (z, mirror) = (zoom.max(1.0), sim.mirror);
        let (hw, hh) = (FRAME_W as f32 / 2.0, FRAME_H as f32 / 2.0);
        let to_out = |wx: f32, wy: f32| {
            let (x, y) = to_screen(wx, wy);
            let (x, y) = ((x * k - hw) * z + hw, (y * k - hh) * z + hh);
            (if mirror { FRAME_W as f32 - x } else { x }, y)
        };
        let ppu = SCALE * k * z; // output pixels per world unit
        lap("outbuf");
        self.draw_world_fx(&mut outpm, level, sim, &to_out, ppu);
        lap("worldfx");
        let hidden = |i: usize| sim.player_hidden || sim.options & (crate::sim::triggers::OPT_HIDE_P1 << i.min(1)) != 0;
        for (i, p) in sim.players.iter().enumerate().filter(|&(i, _)| !hidden(i)) {
            let col = play_fx::player_color(i);
            // sim-recorded per-tick trail (every 3rd point is plenty) + the live position
            let trail: Vec<(f64, f64)> = sim
                .trails
                .get(i)
                .filter(|_| p.mode == GameMode::Wave)
                .map(|t| t.iter().step_by(3).copied().chain(std::iter::once((p.x, p.y))).collect())
                .unwrap_or_default();
            if trail.len() >= 2 {
                let mut pb = PathBuilder::new();
                let (x0, y0) = to_out(trail[0].0 as f32, trail[0].1 as f32);
                pb.move_to(x0, y0);
                for (wx, wy) in trail.iter().skip(1) {
                    let (x, y) = to_out(*wx as f32, *wy as f32);
                    pb.line_to(x, y);
                }
                if let Some(path) = pb.finish() {
                    let mut glow = Paint::default();
                    glow.shader = tiny_skia::Shader::SolidColor(Color::from_rgba8(col.0, col.1, col.2, 110));
                    let w = if p.mini { 0.6 } else { 1.0 } * k;
                    let stroke = Stroke { width: 9.0 * w, line_join: tiny_skia::LineJoin::Miter, ..Default::default() };
                    outpm.stroke_path(&path, &glow, &stroke, Transform::identity(), None);
                    let mut core = Paint::default();
                    core.shader = tiny_skia::Shader::SolidColor(Color::from_rgba8(255, 255, 255, 255));
                    let core_stroke =
                        Stroke { width: 3.5 * w, line_join: tiny_skia::LineJoin::Miter, ..Default::default() };
                    outpm.stroke_path(&path, &core, &core_stroke, Transform::identity(), None);
                }
            }
            lap("trail");
            let (cx, cy) = to_out(p.x as f32, p.y as f32);
            self.spin.resize(sim.players.len(), (0.0, sim.frame));
            let spin = advance_spin(&mut self.spin[i], p, sim.frame);
            let parts = self.gfx.as_ref().zip(self.fxs.prev.get(i)).and_then(|(g, pv)| g.anim_frame(p, pv.phase, pv.phase_t));
            draw_player(&mut outpm, &mut self.store, &mut self.icons, p, col, cx, cy, ppu, spin, parts, mirror);
        }
        lap("players");
        // use effects: GD's expanding ring when an orb, pad or portal fires
        use crate::sim::objects::ObjectKind as K;
        let mut bursts = Vec::new();
        // pads/portals ring on enter, orbs when they fire
        self.prev_touch.resize_with(sim.touching.len(), Default::default);
        for (pi, touch) in sim.touching.iter().enumerate() {
            let now: crate::sim::fx::HashSet<usize> = touch
                .iter()
                .filter(|&&ci| !crate::sim::is_orb(sim.grid.all()[ci].kind))
                .chain(&sim.orbs_used[pi])
                .copied()
                .collect();
            // sorted: two portals entered on one tick flash in a fixed order
            // (HashSet order is random per process, so their discs swapped)
            let mut new: Vec<usize> = now.difference(&self.prev_touch[pi]).copied().collect();
            new.sort_unstable();
            for ci in new {
                let o = &sim.grid.all()[ci];
                if !play_fx::shows(level, sim, o.src) {
                    continue;
                }
                let col = play_fx::kind_color(o.kind).unwrap_or((255, 255, 255));
                if let K::SpeedPortal(idx) = o.kind {
                    let p = &sim.players[pi.min(sim.players.len() - 1)];
                    bursts.push((idx, p.x as f32, p.y as f32));
                }
                let (dx, dy, _) = sim.triggers.transform(o.src).unwrap_or_default();
                // portals (not speed) flash a filled disc in their color (GRIEF
                // ref 1.5% mini, 45.7% ufo+ship, 57.6% gravity); pads and orbs ring
                let disc = !matches!(o.kind, K::SpeedPortal(_)) && kind_is_portal(o.kind);
                self.fx.push((o.x + dx, o.y + dy, sim.frame, col, disc));
            }
            self.prev_touch[pi] = now;
        }
        if let Some(g) = self.gfx.as_ref() {
            for (idx, x, y) in bursts {
                self.fxs.speed_burst(g, idx, x, y);
            }
        }
        const FX_TICKS: u64 = 72; // 0.3 s
        self.fx.retain(|f| sim.frame >= f.2 && sim.frame - f.2 < FX_TICKS);
        for &(wx, wy, t0, col, disc) in &self.fx {
            let t = (sim.frame - t0) as f32 / FX_TICKS as f32;
            let (x, y) = to_out(wx as f32, wy as f32);
            let rad = if disc { (14.0 + 12.0 * t) * ppu } else { (12.0 + 38.0 * t) * ppu };
            if let Some(path) = PathBuilder::from_circle(x, y, rad) {
                let mut paint = Paint::default();
                paint.anti_alias = true;
                paint.shader = tiny_skia::Shader::SolidColor(Color::from_rgba8(col.0, col.1, col.2, ((1.0 - t) * 220.0) as u8));
                if disc {
                    outpm.fill_path(&path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
                } else {
                    let stroke = Stroke { width: (1.0 - t) * 5.0 * k + 1.0, ..Default::default() };
                    outpm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                }
            }
        }
        lap("fx2");
        self.draw_complete(&mut outpm, sim);
        if let Some(lines) = self.menu.as_ref() {
            // dark panel, then bigFont lines 52 px apart
            let (lh, h) = (52.0, 52.0 * lines.len() as f32 + 40.0);
            if let Some(r) = Rect::from_xywh(FRAME_W as f32 * 0.2, (FRAME_H as f32 - h) / 2.0, FRAME_W as f32 * 0.6, h) {
                let mut paint = Paint::default();
                paint.shader = tiny_skia::Shader::SolidColor(Color::from_rgba8(0, 0, 0, 200));
                outpm.fill_rect(r, &paint, Transform::identity(), None);
            }
            // text in GD's bigFont, from the player's own install
            for (i, l) in lines.iter().enumerate().filter(|_| self.gfx.as_ref().is_some_and(|g| g.font.is_some())) {
                let font = self.gfx.as_ref().and_then(|g| g.font.as_ref()).expect("checked");
                let y = (FRAME_H as f32 - h) / 2.0 + 20.0 + lh * (i as f32 + 0.5);
                font.draw(&mut outpm, l, FRAME_W as f32 / 2.0, y, 38.0 / font.line_h, 1.0);
            }
        }
        lap("end");
        let frame = outpm.take();
        out.clear();
        out.extend_from_slice(&frame);
        if prof {
            let mut t = t_start;
            eprintln!("laps: {}", laps.iter().map(|(n, at)| { let d = *at - t; t = *at; format!("{n} {:.3}", d * 1000.0) }).collect::<Vec<_>>().join(" "));
            let t_all = now_secs() - t_start;
            eprintln!("prof: bg {:?} objs {:?} players {:?} upscale {:?} total {:?} builds {} cache {}",
                t_bg, t_objs, t_pl, t_all - t_bg - t_objs - t_pl, t_all, BUILDS.with(|b| b.replace(0)), self.prescaled.len());
        }
    }
}

impl Renderer {
    /// World-space effects under the players: in-world "Attempt N", orb
    /// pulse rings, particles, spider streaks, death and respawn rings.
    fn draw_world_fx(&mut self, out: &mut Pixmap, level: &Level, sim: &Sim, to_out: &dyn Fn(f32, f32) -> (f32, f32), ppu: f32) {
        let solid = |c: (u8, u8, u8), a: f32| {
            let mut p = Paint::default();
            p.anti_alias = true;
            p.shader = tiny_skia::Shader::SolidColor(Color::from_rgba8(c.0, c.1, c.2, (a.clamp(0.0, 1.0) * 255.0) as u8));
            p
        };
        let ring = |out: &mut Pixmap, x: f32, y: f32, r: f32, w: f32, paint: &Paint| {
            if let Some(path) = PathBuilder::from_circle(x, y, r.max(0.5)) {
                out.stroke_path(&path, paint, &Stroke { width: w, ..Default::default() }, Transform::identity(), None);
            }
        };
        // GD shows "Attempt N" in the level, just ahead of the spawn point
        let show = sim.options & crate::sim::triggers::OPT_HIDE_ATTEMPTS == 0;
        if let (Some(font), Some(spawn_x), true) = (self.gfx.as_ref().and_then(|g| g.font.as_ref()), self.fxs.attempt.1, show) {
            let (ax, ay) = to_out(spawn_x as f32 + 165.0, crate::physics::GROUND_Y as f32 + 115.0);
            if ax > -600.0 && ax < FRAME_W as f32 + 600.0 {
                font.draw(out, &format!("Attempt {}", sim.attempt), ax, ay, ppu / 4.0 * 0.85, 1.0);
            }
        }
        // counter labels (1615): the live value of item key 80
        // shortcut: bigFont in white; the label's color channel and the
        // timer / attempts / points modes are ignored
        if self.counters.0 != level.uid {
            self.counters = (level.uid, level.objects.iter().enumerate().filter(|(_, o)| o.id == 1615).map(|(i, _)| i).collect());
        }
        let grid_key = std::sync::Arc::as_ptr(&sim.grid) as usize;
        if self.orbs.0 != grid_key {
            self.orbs = (grid_key, sim.grid.all().iter().enumerate().filter(|(_, o)| crate::sim::is_orb(o.kind)).map(|(i, _)| i).collect());
        }
        if let Some(font) = self.gfx.as_ref().and_then(|g| g.font.as_ref()) {
            for (oi, o) in self.counters.1.iter().map(|&i| (i, &level.objects[i])) {
                let Some((dx, dy, _)) = sim.triggers.transform(oi) else { continue };
                if !play_fx::shows(level, sim, oi) {
                    continue;
                }
                let (x, y) = to_out(o.x + dx as f32, o.y + dy as f32);
                if x > -300.0 && x < FRAME_W as f32 + 300.0 && y > -100.0 && y < FRAME_H as f32 + 100.0 {
                    let item = o.raw.iter().find(|(k, _)| *k == 80).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                    let k = if o.scale_y != 0.0 { o.scale_y.abs() } else { 1.0 };
                    font.draw(out, &sim.triggers.item(item).to_string(), x, y, ppu / 4.0 * k, sim.triggers.alpha(oi));
                }
            }
        }
        // orbs pulse (GD pulses them with the music; a steady 2 Hz here)
        let t = sim.frame as f32 / 240.0;
        let pulse = 1.0 + 0.12 * (t * std::f32::consts::TAU * 2.0).sin().abs();
        for o in self.orbs.1.iter().map(|&i| &sim.grid.all()[i]) {
            let Some((dx, dy, _)) = sim.triggers.transform(o.src) else { continue };
            if !play_fx::shows(level, sim, o.src) {
                continue;
            }
            let (x, y) = to_out((o.x + dx) as f32, (o.y + dy) as f32);
            if x < -100.0 || x > FRAME_W as f32 + 100.0 || y < -100.0 || y > FRAME_H as f32 + 100.0 {
                continue;
            }
            let c = play_fx::kind_color(o.kind).unwrap_or((255, 255, 255));
            ring(out, x, y, 17.0 * pulse * ppu, 1.5 * ppu / SCALE, &solid(c, 0.45));
        }
        let (fw, fh) = (FRAME_W as usize, FRAME_H as usize);
        self.fxs.parts.draw(out.data_mut(), fw, fh, |x, y| to_out(x, y), ppu);
        let col = play_fx::player_color(0);
        for &(sx, y0, y1, age) in &self.fxs.streaks {
            let (x, ya) = to_out(sx as f32, y0 as f32);
            let (_, yb) = to_out(sx as f32, y1 as f32);
            let w = 12.0 * ppu;
            if let Some(r) = Rect::from_xywh(x - w / 2.0, ya.min(yb), w, (ya - yb).abs().max(1.0)) {
                out.fill_rect(r, &solid(col, (1.0 - age / 0.25) * 0.7), Transform::identity(), None);
            }
        }
        if let Some((dx, dy, age)) = self.fxs.death {
            let (x, y) = to_out(dx as f32, dy as f32);
            let k = age / 0.6;
            ring(out, x, y, (5.0 + 80.0 * k) * ppu, 3.0 * ppu / SCALE, &solid(col, (1.0 - k) * 0.8));
        }
        // respawn: a ring closing in on the new player
        if sim.state == crate::sim::RunState::Playing && self.fxs.respawn_age < 0.35 && sim.attempt > 1 {
            let p = &sim.players[0];
            let (x, y) = to_out(p.x as f32, p.y as f32);
            let k = self.fxs.respawn_age / 0.35;
            ring(out, x, y, (60.0 * (1.0 - k) + 4.0) * ppu, 3.0 * ppu / SCALE, &solid(col, 1.0 - k));
        }
    }

    /// Level complete: GD's "Level Complete!" banner pops in (fireworks are particles).
    fn draw_complete(&mut self, out: &mut Pixmap, sim: &Sim) {
        if sim.state != crate::sim::RunState::Complete {
            return;
        }
        let age = self.fxs.complete_age;
        let Some(sp) = self.store.sprite("GJ_levelComplete_001.png") else { return };
        // overshoot pop: 0 -> 1.15 -> 1.0
        let pop = if age < 0.25 { age / 0.25 * 1.15 } else { 1.0 + 0.15 * (1.0 - ((age - 0.25) / 0.15).min(1.0)) };
        let s = FRAME_W as f32 * 0.55 / sp.width() as f32 * pop;
        let t = Transform::from_translate(FRAME_W as f32 / 2.0, FRAME_H as f32 * 0.4)
            .pre_scale(s, s)
            .pre_translate(-(sp.width() as f32) / 2.0, -(sp.height() as f32) / 2.0);
        let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() };
        out.draw_pixmap(0, 0, sp.as_ref(), &paint, t, None);
    }
}

impl Renderer {
    /// GD-style vertical camera: flying modes center on their portal band,
    /// cube/robot follow the player with a dead zone; never below the
    /// ground view. Eased per sim tick so frame rate does not change it.
    fn camera_y(&mut self, sim: &Sim, static_y: Option<f32>) -> f32 {
        let Some(p) = sim.players.first() else { return 240.0 };
        // lowest camera centre: screen bottom at world 0, so the floor
        // (y 90) sits 90u up the 320u view, as in real GD footage
        let (py, lo) = (p.y as f32, HALF_VIEW_Y);
        // band only while the player is inside it (replay fixes can leave the
        // sim's band stale); otherwise follow like the cube
        let flying = p.band != (crate::physics::GROUND_Y, crate::physics::CEIL_Y)
            && (p.band.0..=p.band.1).contains(&p.y);
        let (prev, last) = self.cam.unwrap_or((py.max(lo), sim.frame));
        // dual: frame both players (GD keeps the pair on screen)
        let mid = sim.players.get(1).map(|q| (py + q.y as f32) / 2.0);
        let target = if let Some(s) = static_y {
            s
        } else if let Some(m) = mid {
            m
        } else if flying {
            ((p.band.0 + p.band.1) / 2.0) as f32
        } else {
            // GD's cube dead zone: player kept 120u above the screen bottom
            // and 90u below the top (swapped when flipped)
            if p.grav_flip { prev.clamp(py - 40.0, py + 70.0) } else { prev.clamp(py - 70.0, py + 40.0) }
        }
        .max(lo);
        let ticks = sim.frame.saturating_sub(last) as i32;
        // respawn / teleport / far jump: snap; otherwise ease ~8% per 4 ticks
        let y = if sim.frame < last || (target - prev).abs() > 400.0 {
            target
        } else {
            prev + (target - prev) * (1.0 - 0.92f32.powf(ticks as f32 / 4.0))
        };
        // the player must stay on screen whatever the easing does
        let c = mid.unwrap_or(py);
        let y = y.clamp(c - HALF_VIEW_Y + 20.0, c + HALF_VIEW_Y - 20.0).max(lo);
        self.cam = Some((y, sim.frame));
        y
    }

    /// Build every multi-sprite composite this level uses, in parallel, so
    /// none is built mid-run (some take 100+ ms).
    fn prebuild_composites(&mut self, level: &Level, table: &ObjectTable) {
        let table_key = table as *const ObjectTable as usize;
        if self.ids.as_ref().map(|(k, _)| *k != table_key).unwrap_or(true) {
            self.ids = Some((table_key, IdTables::new(table)));
        }
        let ids = &self.ids.as_ref().expect("id tables").1;
        let used: crate::sim::fx::HashSet<u16> = level.objects.iter().map(|o| o.id).collect();
        let mut jobs = Vec::new();
        for id in used {
            let Some(tex) = ids.tex[id as usize].as_deref() else { continue };
            let Some((p, pc, k, sec)) = ids.kids.get(tex) else { continue };
            for t in p.iter().map(|s| s.as_str()).chain(k.iter().map(|c| c.texture_name.as_str())) {
                self.store.sprite(t);
            }
            if let Some(b) = p.as_deref().and_then(back_of) {
                self.store.sprite(&b);
            }
            jobs.push((tex.to_string(), p.clone(), pc.clone(), k.clone(), false));
            if *sec {
                jobs.push((format!("{tex}:d"), p.clone(), pc.clone(), k.clone(), true));
            }
        }
        let store = &self.store;
        let chunk = jobs.len().div_ceil(16).max(1);
        // wasm has no spawnable threads: build the chunks inline
        let built: Vec<(String, Option<Pixmap>)> = if cfg!(target_family = "wasm") {
            jobs.chunks(chunk)
                .flat_map(|js| js.iter().map(|(key, p, pc, k, sec)| (key.clone(), composite_build(store, p.as_deref(), pc.as_deref(), k, *sec))))
                .collect()
        } else {
            std::thread::scope(|sc| {
                let hs: Vec<_> = jobs
                    .chunks(chunk)
                    .map(|js| {
                        sc.spawn(move || {
                            js.iter()
                                .map(|(key, p, pc, k, sec)| {
                                    (key.clone(), composite_build(store, p.as_deref(), pc.as_deref(), k, *sec))
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                hs.into_iter().flat_map(|h| h.join().expect("composite thread")).collect()
            })
        };
        for (key, pm) in built {
            if let Some(pm) = pm {
                self.store.insert(&key, pm);
            }
        }
    }
}

/// GD icon frames per mode: (primary, secondary); robot and spider are
/// multi-part animated icons. shortcut: they draw their first part only.
fn icon_frames(mode: GameMode) -> (&'static str, &'static str) {
    match mode {
        GameMode::Cube => ("player_01_001.png", "player_01_2_001.png"),
        GameMode::Ship => ("ship_01_001.png", "ship_01_2_001.png"),
        GameMode::Ball => ("player_ball_01_001.png", "player_ball_01_2_001.png"),
        GameMode::Ufo => ("bird_01_001.png", "bird_01_2_001.png"),
        GameMode::Wave => ("dart_01_001.png", "dart_01_2_001.png"),
        GameMode::Robot => ("robot_01_01_001.png", "robot_01_01_2_001.png"),
        GameMode::Spider => ("spider_01_01_001.png", "spider_01_01_2_001.png"),
        GameMode::Swing => ("swing_01_001.png", "swing_01_2_001.png"),
    }
}

/// Player icon for its current mode, tinted (primary color, white
/// secondary), rotated, bilinear at output resolution. `px_per_unit` maps
/// world units to output pixels.
/// Pure-sim icon spin, advanced by the sim ticks since the last draw: the
/// cube turns ~180° per flat jump while airborne and snaps to the nearest 90°
/// on landing; the ball rolls with its ground speed. Clockwise, mirrored when flipped.
/// shortcut: rates are eyeballed from GD footage, not extracted from the binary
fn advance_spin(state: &mut (f32, u64), p: &Player, now: u64) -> f32 {
    let ticks = now.saturating_sub(state.1) as f32;
    state.1 = now;
    let dir = if p.grav_flip { -1.0 } else { 1.0 };
    match p.mode {
        GameMode::Cube if p.on_ground => state.0 = (state.0 / 90.0).round() * 90.0,
        GameMode::Cube => state.0 += 1.75 * dir * ticks,
        GameMode::Ball => state.0 += (p.dx() as f32 / 15.0).to_degrees() * dir * ticks,
        _ => state.0 = 0.0,
    }
    state.0 = state.0.rem_euclid(360.0);
    state.0
}

#[allow(clippy::too_many_arguments)]
fn draw_player(
    canvas: &mut Pixmap,
    store: &mut SpriteStore,
    icons: &mut crate::sim::fx::HashMap<(String, (u8, u8, u8)), Pixmap>,
    p: &Player,
    tint: (u8, u8, u8),
    cx: f32,
    cy: f32,
    px_per_unit: f32,
    spin: f32,
    parts: Option<&[fx::Part]>,
    mirror: bool,
) {
    let (main, second) = icon_frames(p.mode);
    if store.sprite_sheet(main).is_none() {
        // no icon sheet: fall back to a bright shape
        let col = Color::from_rgba8(tint.0, tint.1, tint.2, 255);
        draw_fallback(canvas, p, col, cx, cy);
        return;
    }
    // rotation: the recorded in-game angle when replaying; otherwise flying
    // modes aim along their actual movement and the rest stay level
    let deg = p.vis_rot.map(|r| r as f32).unwrap_or_else(|| match p.mode {
        GameMode::Cube | GameMode::Ball => spin,
        GameMode::Wave | GameMode::Ship | GameMode::Ufo | GameMode::Swing => {
            let a = -(p.last_move_dy as f32).atan2(p.dx() as f32).to_degrees();
            if p.mode == GameMode::Wave { a } else { a * 0.5 }
        }
        _ => 0.0,
    });
    let size = if p.mini { 0.6 } else { 1.0 };
    let flip = if p.grav_flip && p.mode != GameMode::Wave { -1.0 } else { 1.0 };
    let u = px_per_unit * size;
    // body frame: center, mirror, rotation, gravity flip
    let body = Transform::from_translate(cx, cy)
        .pre_scale(if mirror { -1.0 } else { 1.0 }, 1.0)
        .pre_concat(Transform::from_rotate(deg))
        .pre_scale(1.0, flip);
    let mut paint = PixmapPaint::default();
    paint.quality = FilterQuality::Bicubic;
    // one sprite, tinted (cached per color), centered on `at` (units in the body frame)
    let mut layer = |canvas: &mut Pixmap, frame: &str, col: (u8, u8, u8), at: Transform| {
        let Some((sprite, sscale)) = store.sprite_sheet(frame) else { return };
        let s = u / sscale;
        let tinted = icons.entry((frame.to_string(), col)).or_insert_with(|| {
            let mut t = sprite.clone();
            for px in t.pixels_mut() {
                let c = px.demultiply();
                *px = tiny_skia::ColorU8::from_rgba(
                    (c.red() as u16 * col.0 as u16 / 255) as u8,
                    (c.green() as u16 * col.1 as u16 / 255) as u8,
                    (c.blue() as u16 * col.2 as u16 / 255) as u8,
                    c.alpha(),
                )
                .premultiply();
            }
            t
        });
        let t = body
            .pre_concat(at)
            .pre_scale(s, s)
            .pre_translate(-(tinted.width() as f32) * 0.5, -(tinted.height() as f32) * 0.5);
        canvas.draw_pixmap(0, 0, tinted.as_ref(), &paint, t, None);
    };
    const WHITE: (u8, u8, u8) = (255, 255, 255);
    // ship / UFO carry a small cube rider, drawn behind the vehicle
    // shortcut: GD's rider offsets eyeballed (ship 5 up 3 back, ufo 5 up), scale 0.55
    let rider = match p.mode {
        GameMode::Ship => Some((-3.0, 5.0)),
        GameMode::Ufo => Some((0.0, 5.0)),
        _ => None,
    };
    if let Some((rx, ry)) = rider {
        let at = Transform::from_translate(rx * u, -ry * u).pre_scale(0.55, 0.55);
        let (cm, cs) = icon_frames(GameMode::Cube);
        layer(canvas, cs, WHITE, at);
        layer(canvas, cm, tint, at);
    }
    // robot / spider: GD's multi-part animation (legs etc.), secondary layer under primary
    if let Some(parts) = parts.filter(|_| matches!(p.mode, GameMode::Robot | GameMode::Spider)) {
        for part in parts {
            let at = Transform::from_translate(part.x * u, -part.y * u)
                .pre_concat(Transform::from_rotate(part.rot))
                .pre_scale(part.sx * if part.fx { -1.0 } else { 1.0 }, part.sy * if part.fy { -1.0 } else { 1.0 });
            layer(canvas, &part.tex.replace("_001.png", "_2_001.png"), WHITE, at);
            layer(canvas, &part.tex, tint, at);
        }
        return;
    }
    // secondary color layer under the primary one, as GD stacks them
    layer(canvas, second, WHITE, Transform::identity());
    layer(canvas, main, tint, Transform::identity());
}

fn draw_fallback(canvas: &mut Pixmap, p: &Player, col: Color, cx: f32, cy: f32) {
    let half = p.outer_half() as f32 * SCALE;
    let mut paint = Paint::default();
    paint.shader = tiny_skia::Shader::SolidColor(col);

    let (path, t) = match p.mode {
        GameMode::Wave => {
            // dart pointing along the velocity
            let vx = p.dx() as f32;
            let vy = p.vel_y as f32;
            let deg = vy.atan2(vx).to_degrees();
            let mut pb = PathBuilder::new();
            pb.move_to(half * 1.4, 0.0);
            pb.line_to(-half * 0.9, half * 0.7);
            pb.line_to(-half * 0.9, -half * 0.7);
            pb.close();
            (pb.finish(), Transform::from_translate(cx, cy).pre_concat(Transform::from_rotate(-deg)))
        }
        GameMode::Ship | GameMode::Ufo | GameMode::Swing => {
            square_path(half * 0.7, cx, cy, 0.0)
        }
        GameMode::Cube | GameMode::Robot => {
            // tumble while airborne, land flat
            let deg = if p.on_ground { 0.0 } else { (p.x as f32 + 20.0) * 0.9 };
            square_path(half, cx, cy, deg)
        }
        GameMode::Ball => square_path(half, cx, cy, p.x as f32 * 2.0),
        GameMode::Spider => square_path(half, cx, cy, 0.0),
    };
    if let Some(path) = path {
        canvas.fill_path(&path, &paint, FillRule::Winding, t, None);
    }
}

fn square_path(half: f32, cx: f32, cy: f32, deg: f32) -> (Option<tiny_skia::Path>, Transform) {
    let mut pb = PathBuilder::new();
    if let Some(r) = Rect::from_xywh(-half, -half, half * 2.0, half * 2.0) {
        pb.push_rect(r);
    }
    (
        pb.finish(),
        Transform::from_translate(cx, cy).pre_concat(Transform::from_rotate(deg)),
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Right,
}

/// 3x5 glyphs for 0-9, A T E M P % and space
fn glyph(ch: u8) -> Option<[u8; 5]> {
    Some(match ch {
        b'0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        b'1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        b'2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        b'3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        b'4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        b'5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        b'6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        b'7' => [0b111, 0b001, 0b001, 0b010, 0b010],
        b'8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        b'9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        b'A' => [0b111, 0b101, 0b111, 0b101, 0b101],
        b'T' => [0b111, 0b010, 0b010, 0b010, 0b010],
        b'E' => [0b111, 0b100, 0b111, 0b100, 0b111],
        b'M' => [0b101, 0b111, 0b111, 0b101, 0b101],
        b'P' => [0b111, 0b101, 0b111, 0b100, 0b100],
        b'%' => [0b101, 0b001, 0b010, 0b100, 0b101],
        b'.' => [0b000, 0b000, 0b000, 0b000, 0b010],
        b' ' => [0b000, 0b000, 0b000, 0b000, 0b000],
        _ => return None,
    })
}

/// draw `text` with the 3x5 font; (x, y) is the anchor per `align`
fn draw_text(
    canvas: &mut Pixmap,
    text: &str,
    x: f32,
    y: f32,
    cell: f32,
    align: Align,
    col: (u8, u8, u8),
) {
    let gw: f32 = text.len() as f32 * (4.0 * cell) - cell;
    let x0 = match align {
        Align::Right => x - gw,
    };
    let mut paint = Paint::default();
    paint.anti_alias = false;
    for (shadow, col) in [
        (1.0, Color::from_rgba8(0, 0, 0, 200)),
        (0.0, Color::from_rgba8(col.0, col.1, col.2, 255)),
    ] {
        let mut cx = x0 + shadow;
        for ch in text.bytes() {
            if let Some(rows) = glyph(ch.to_ascii_uppercase()) {
                for (row, bits) in rows.iter().enumerate() {
                    for col_i in 0..3 {
                        if bits & (1 << (2 - col_i)) != 0 {
                            if let Some(r) = Rect::from_xywh(
                                cx + col_i as f32 * cell,
                                y + shadow + row as f32 * cell,
                                cell,
                                cell,
                            ) {
                                paint.shader = tiny_skia::Shader::SolidColor(col);
                                canvas.fill_rect(r, &paint, Transform::identity(), None);
                            }
                        }
                    }
                }
            }
            cx += 4.0 * cell;
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Assets;
    use crate::level::parse_level;
    use std::path::Path;

    fn assets() -> Assets {
        Assets::fs(&Path::new(env!("CARGO_MANIFEST_DIR")).join("assets"))
    }

    fn table() -> ObjectTable {
        ObjectTable::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("data/objects.json")).unwrap()
    }

    fn synth(objs: Vec<(u16, f32, f32, f32)>) -> Level {
        let mut lvl = Level::default();
        for (id, x, y, rot) in objs {
            lvl.objects.push(GdObject {
                id,
                x,
                y,
                rot,
                scale_x: 1.0,
                scale_y: 1.0,
                ..GdObject::default()
            });
        }
        lvl
    }

    fn pixel(buf: &[u8], x: usize, y: usize) -> (u8, u8, u8, u8) {
        let i = (y * FRAME_W as usize + x) * 4;
        (buf[i], buf[i + 1], buf[i + 2], buf[i + 3])
    }

    #[test]
    fn a_mirrored_half_is_placed_in_the_content_box() {
        let table = ObjectTable::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/objects.json")).unwrap();
        let (pw, ph) = (83.5 * SHEET_SCALE, 82.0 * SHEET_SCALE); // this sheet's saw
        // saw 88: its second half is the same sprite, centered in an 87x85 box
        let saw = table.objects["88"].children.as_ref().unwrap();
        let (bw, _) = child_box(&saw[0], Some("sawblade_01_001"), pw, ph);
        assert!((saw[0].x * SHEET_SCALE - bw / 2.0).abs() < 1.0, "saw half off center: seam");
        // cog 676: its parts are other sprites, centered on the sprite box
        let cog = table.objects["676"].children.as_ref().unwrap();
        assert_eq!(child_box(&cog[0], Some("blackCogwheel_02_001"), pw, ph), (pw, ph));
    }

    #[test]
    fn legacy_color_triggers_target_their_fixed_channel() {
        // Stereo Madness style: 29 = background, 30 = ground, no key 23
        let lvl = crate::level::parse_level("kS29,1_40_2_62_3_255;1,29,2,135,3,135,7,158,8,3,9,255,10,1;1,30,2,165,3,135,7,96,8,0,9,185;");
        let cmds = ColorEngine::new(&lvl).cmds;
        assert_eq!(cmds.iter().map(|c| (c.ch, c.to)).collect::<Vec<_>>(), [(1000, (158.0, 3.0, 255.0)), (1001, (96.0, 0.0, 185.0))]);
    }

    #[test]
    fn texture_lookup_for_common_ids() {
        let mut store = SpriteStore::load(&assets());
        let t = table();
        for (id, name, w, h) in
            [(1u16, "square_01_001", 120u32, 120u32), (8, "spike_01_001", 120, 120), (218, "colorSpike_03_001", 80, 76)]
        {
            let tex = t.objects.get(&id.to_string()).unwrap().texture_name.clone().unwrap();
            assert_eq!(tex, name);
            let pm = store.sprite(tex.as_str()).unwrap();
            assert_eq!((pm.width(), pm.height()), (w, h));
        }
    }

    #[test]
    fn renders_synthetic_frame() {
        let lvl = synth(vec![(1, 60.0, 0.0, 0.0), (8, 120.0, 0.0, 0.0)]);
        let t = table();
        let sim = Sim::new(&lvl, &t);
        let mut r = Renderer::new(&assets());
        let buf = r.draw(&lvl, &t, &sim);
        assert_eq!(buf.len(), (FRAME_W * FRAME_H * 4) as usize);
        assert!(buf.iter().any(|&b| b != buf[0]), "frame is uniform");

        // player 0: 40% from the left at ground level → its icon is drawn
        // there in the player color (the exact pixel differs per art source:
        // gd-clone's icon is solid pink, RobTop's has white borders)
        // (16:9: camx = -256, camy = 160 → spawn (0,99) at (512, 470))
        let mut primary = false;
        for dx in 0..40 {
            for dy in 0..40 {
                if pixel(&buf, 492 + dx, 450 + dy) == (255, 45, 140, 255) {
                    primary = true;
                }
            }
        }
        assert!(primary, "player icon's primary color not found at the spawn point");

        // block 1 at world x=60 (y=105): dark square_01 sprite over the bg
        let (br, bgc, bb, _) = pixel(&buf, 632, 500);
        let (fr, fg, fb, _) = pixel(&buf, 632, 300);
        assert!(br != fr || bgc != fg || bb != fb, "block not drawn");
    }

    #[test]
    fn sse2_blend_matches_the_scalar_blend() {
        // premultiplied sources (c <= a) at every alpha, random backgrounds
        let mut r = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = || { r ^= r << 13; r ^= r >> 7; r ^= r << 17; r };
        let sp: Vec<u8> = (0..4096).flat_map(|i| { let a = (i % 256) as u8; let x = rnd(); [0, 8, 16].map(|k| ((x >> k) as u8) % a.saturating_add(1)).into_iter().chain([a]).collect::<Vec<_>>() }).collect();
        let dp: Vec<u8> = (0..4096 * 4).map(|_| rnd() as u8).collect();
        let k = |t: u32, a: u32| (t * a * 65536 + 32512) / 65025;
        for f in [[65536; 4], [k(255, 128), k(30, 128), k(200, 128), k(255, 128)], [k(7, 255), k(255, 255), k(0, 255), k(255, 255)], [k(90, 16), k(1, 16), k(254, 16), k(255, 16)]] {
            for add in [false, true] {
                let (mut a, mut b) = (dp.clone(), dp.clone());
                blend_row(&sp[4..], &mut a[4..], f, add); // odd length: SIMD body + scalar tail
                blend_scalar(&sp[4..], &mut b[4..], f, add);
                assert_eq!(a, b, "tint {f:?} add {add}");
            }
        }
    }

    #[test]
    fn menu_panel_draws_over_the_frame_and_goes_away() {
        let t = table();
        let mut r = Renderer::new(&assets());
        let lvl = synth(vec![]);
        let sim = Sim::new(&lvl, &t);
        let plain = r.draw(&lvl, &t, &sim);
        r.set_menu(Some(vec!["Level ID: 123_".into(), "Mode: Auto (Tab)".into()]));
        let menu = r.draw(&lvl, &t, &sim);
        // panel spans the middle 60%: darker there, with lit text pixels
        let lum = |b: &[u8], x, y| { let (r, g, b, _) = pixel(b, x, y); r as u32 + g as u32 + b as u32 };
        assert!(lum(&menu, 300, 360) < lum(&plain, 300, 360), "no dark panel");
        // text needs GD's bigFont (the player's install; not in assets/)
        if r.gfx.as_ref().is_some_and(|g| g.font.is_some()) {
            assert!((380..900).any(|x| [340, 360, 380].iter().any(|&y| lum(&menu, x, y) > 600)), "no menu text");
        }
        r.set_menu(None);
        assert_eq!(r.draw(&lvl, &t, &sim), plain, "closed menu must leave the frame as it was");
    }

    #[test]
    fn object_rotation_flips_spike_on_screen() {
        // spike_01_001's tip points up; rot=180 (world CCW) must point it down
        let t = table();
        let mut r = Renderer::new(&assets());
        // spike standing on the ground: level y 15 (the ground is drawn in
        // front, like GD, so one sunk into it would be hidden), one cell
        // (30u = 60px) above the old y 0 spot → x [542, 602], y [480, 540]
        let mut render = |rot: f32| -> Vec<u8> {
            let lvl = synth(vec![(8, 30.0, 15.0, rot)]);
            let sim = Sim::new(&lvl, &t);
            r.draw(&lvl, &t, &sim)
        };
        let buf0 = render(0.0);
        let buf180 = render(180.0);
        // the bg is a tiled texture — assert the sprite flips by counting
        // differing pixels across the whole 60px cell
        let mut diff = 0usize;
        for y in (476..544).step_by(2) {
            for x in (538..606).step_by(2) {
                if pixel(&buf0, x, y) != pixel(&buf180, x, y) {
                    diff += 1;
                }
            }
        }
        assert!(diff > 40, "rot 0 vs 180 must flip the sprite (diff={diff})");
    }

    #[test]
    fn cube_spins_in_air_and_snaps_on_landing() {
        let mut p = Player::new(GameMode::Cube, 1);
        p.on_ground = false;
        let mut st = (0.0, 0);
        let a = advance_spin(&mut st, &p, 40); // 40 ticks airborne
        assert!((a - 70.0).abs() < 1e-3, "{a}");
        p.on_ground = true;
        assert_eq!(advance_spin(&mut st, &p, 41), 90.0);
        p.grav_flip = true;
        p.on_ground = false;
        assert!((advance_spin(&mut st, &p, 51) - 72.5).abs() < 1e-3);
    }

    #[test]
    fn renders_grief_first_frame() {
        let s = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("levels/149992717.gmd"),
        )
        .unwrap();
        let lvl = parse_level(&s);
        let t = table();
        let sim = Sim::new(&lvl, &t);
        let mut r = Renderer::new(&assets());
        let buf = r.draw(&lvl, &t, &sim);
        assert_eq!(buf.len(), (FRAME_W * FRAME_H * 4) as usize);
        // bg channel 1000 drives the background: GRIEF's opening 899 sets it
        // to (0,0,0) instantly, so the top-left pixel is near-black
        let (r8, g8, b8, _) = pixel(&buf, 10, 10);
        assert!(r8 < 130 && g8 < 40, "bg must come from GRIEF's own 899 trigger data, got ({r8},{g8},{b8})");
    }
}
