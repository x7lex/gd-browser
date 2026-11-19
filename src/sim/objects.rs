//! Object kinds, the runtime object table (data/objects.json), and the spatial hash.

use crate::level::object::GdObject;
use serde::Deserialize;
use super::fx::HashMap;

/// Interactive object ids (stable in GD). Portals map by texture evidence in objects.json.
pub const SPEED_PORTALS: [(u16, u8); 5] = [(200, 0), (201, 1), (202, 2), (203, 3), (1334, 4)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Solid,
    Hazard,
    Slope, // triangle solid with a ridable hypotenuse (SlopeGeom)
    Decoration,
    SpeedPortal(u8),
    GravityPortal(bool), // true = flip to inverted
    PortalCube,
    PortalShip,
    PortalBall,
    PortalUfo,
    PortalWave,
    PortalRobot,
    PortalSpider,
    PortalSwing,
    PortalTeleport,
    PortalDual,
    PortalSolo,
    PortalMirror,
    PortalSize(bool), // true = mini
    OrbYellow,
    OrbPink,
    OrbRed,
    OrbBlue,  // gravity orb
    OrbGreen, // gravity flip + jump
    OrbSpider,
    OrbDrop,   // black orb (1330) and the 2.2 drop orbs (3641/3642)
    OrbDash(bool), // dash orb: true = pink (flips gravity first)
    OrbToggle, // trigger orb (1594): a click toggles its target group
    PadYellow,
    PadPink,
    PadRed,
    PadGravity,
    Coin,
    Trigger, // no gameplay effect in sim
}

#[derive(Debug, Deserialize)]
pub struct ObjectDef {
    #[serde(default)]
    pub object_type: i64,
    #[serde(default)]
    pub texture_name: Option<String>,
    #[serde(default)]
    pub hitbox: Option<Vec<f64>>, // [w, h, x, y] offsets from object center
    #[serde(default)]
    pub hitbox_radius: Option<f64>,
    #[serde(default)]
    pub is_solid: bool,
    #[serde(default)]
    pub default_z_layer: i64,
    #[serde(default)]
    pub default_primary_channel: i64,
    #[serde(default)]
    pub default_secondary_channel: i64,
    /// which color slot the main texture takes: "base" / "detail" / "black"
    #[serde(default)]
    pub color_channel: Option<String>,
    #[serde(default)]
    pub default_z_order: i64,
    /// extra sprites drawn with the object (render only)
    #[serde(default)]
    pub children: Option<Vec<ChildSprite>>,
}

/// One child sprite of a multi-sprite object, in cocos2d node terms:
/// position in parent content units (origin bottom-left), anchor 0..1.
#[derive(Debug, Clone, Deserialize)]
pub struct ChildSprite {
    pub texture_name: String,
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub rot: f32,
    #[serde(default = "one")]
    pub scale_x: f32,
    #[serde(default = "one")]
    pub scale_y: f32,
    #[serde(default = "half")]
    pub anchor_x: f32,
    #[serde(default = "half")]
    pub anchor_y: f32,
    #[serde(default)]
    pub flip_x: bool,
    #[serde(default)]
    pub flip_y: bool,
    #[serde(default)]
    pub z: i32,
    #[serde(default)]
    pub color_channel: Option<String>,
    /// the parent's content box the position is measured in (0 = unknown)
    #[serde(default)]
    pub content_x: f32,
    #[serde(default)]
    pub content_y: f32,
}

fn one() -> f32 {
    1.0
}
fn half() -> f32 {
    0.5
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct ObjectTable {
    pub objects: HashMap<String, ObjectDef>,
}

impl ObjectTable {
    pub fn load(path: &std::path::Path) -> Result<ObjectTable, String> {
        let f = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        ObjectTable::from_str(&f)
    }

    /// The object table straight from `data/objects.json`'s text (the
    /// browser build, which fetches the file itself).
    pub fn from_str(f: &str) -> Result<ObjectTable, String> {
        serde_json::from_str(f).map_err(|e| e.to_string())
    }

    fn kind_of(&self, id: u16) -> ObjectKind {
        let def = match self.objects.get(&id.to_string()) {
            Some(d) => d,
            None => return ObjectKind::Decoration,
        };
        // explicit interactive ids first (verified against real GD portal textures)
        let kind = match id {
            200 | 201 | 202 | 203 | 1334 => SPEED_PORTALS
                .iter()
                .find(|(pid, _)| *pid == id)
                .map(|(_, idx)| ObjectKind::SpeedPortal(*idx)),
            10 => Some(ObjectKind::GravityPortal(false)),
            11 => Some(ObjectKind::GravityPortal(true)),
            12 => Some(ObjectKind::PortalCube),
            13 => Some(ObjectKind::PortalShip),
            47 => Some(ObjectKind::PortalBall),
            111 => Some(ObjectKind::PortalUfo),
            660 => Some(ObjectKind::PortalWave),
            745 => Some(ObjectKind::PortalRobot),
            1331 => Some(ObjectKind::PortalSpider),
            1933 => Some(ObjectKind::PortalSwing),
            99 => Some(ObjectKind::PortalSize(false)), // normal size (green)
            101 => Some(ObjectKind::PortalSize(true)), // mini (pink)
            747 => Some(ObjectKind::PortalTeleport),
            286 => Some(ObjectKind::PortalDual),
            287 => Some(ObjectKind::PortalSolo),
            45 | 46 => Some(ObjectKind::PortalMirror),
            // shortcut: pit slopes ride as plain slopes; gd-sim treats
            // 363/364/366/1717/1718 as hazard slopes (expectedY ±4). Add a
            // hazard-slope kind when the truth tool reaches one.
            363 | 366 | 1717 | 1723 | 364 | 367 | 1718 | 1724 => Some(ObjectKind::Slope),
            36 => Some(ObjectKind::OrbYellow),
            141 => Some(ObjectKind::OrbPink),
            1333 => Some(ObjectKind::OrbRed),
            84 => Some(ObjectKind::OrbBlue),
            1022 => Some(ObjectKind::OrbGreen),
            3649 => Some(ObjectKind::OrbSpider),
            1330 | 3641 | 3642 => Some(ObjectKind::OrbDrop),
            1704 | 3646 | 3647 | 3648 => Some(ObjectKind::OrbDash(false)),
            1751 => Some(ObjectKind::OrbDash(true)),
            1594 => Some(ObjectKind::OrbToggle),
            35 => Some(ObjectKind::PadYellow),
            140 => Some(ObjectKind::PadPink),
            1332 => Some(ObjectKind::PadRed),
            67 => Some(ObjectKind::PadGravity),
            142 | 1329 => Some(ObjectKind::Coin), // secret coin, user coin (143 is a breakable block)
            // triangle_h_square_02 filler: marked solid in objects.json, but
            // the real GRIEF run (xdBot frame fixes, tick 835) flies through
            // its box and gd-sim's collision table omits it
            329 => Some(ObjectKind::Decoration),
            _ => None,
        };
        if let Some(k) = kind {
            return k;
        }
        match def.object_type {
            // outline art is solid too: the real recordings only pass through
            // outlines marked No Touch (key 121, dropped in Sim::new)
            0 | 1 if def.is_solid || def.hitbox.is_some() => ObjectKind::Solid,
            2 => ObjectKind::Hazard,
            25 => ObjectKind::Slope,
            _ => ObjectKind::Decoration,
        }
    }
}

/// Slope triangle: hypotenuse endpoints + the solid triangle (world space).
/// Mirroring (flipX/flipY) and rotation of the default {BL,BR,TR} triangle
/// under the BL→TR hypotenuse produce every slope variant, so one local
/// definition covers all flip/rot combinations.
#[derive(Debug, Clone, Copy)]
pub struct SlopeGeom {
    /// hypotenuse surface endpoints
    pub surf: ((f64, f64), (f64, f64)),
    /// the 3 solid vertices; the solid side of the surface is the side the
    /// triangle's centroid is on
    pub tri: [(f64, f64); 3],
}

/// A level object preprocessed for the sim.
#[derive(Debug, Clone)]
pub struct SimObject {
    pub id: u16,
    pub kind: ObjectKind,
    /// index into `Level::objects` (trigger-engine key)
    pub src: usize,
    /// world-space center
    pub x: f64,
    pub y: f64,
    /// editor rotation, degrees CW
    pub rot: f64,
    /// world-space bounding box of `quad` (min_x, min_y, w, h)
    pub rect: (f64, f64, f64, f64),
    /// hitbox corners in order around the box
    pub quad: [(f64, f64); 4],
    /// quad is axis-aligned (rect is exact; skip the oriented test)
    pub aligned: bool,
    /// circle hitbox radius, scaled (saws); hazards with one use the circle
    pub radius: Option<f64>,
    pub slope: Option<SlopeGeom>,
    /// object groups for move-trigger displacement (key 57)
    pub groups: Vec<u16>,
    /// teleport portal: y offset of the exit portal (key 54)
    pub tele_dy: f64,
    /// trigger orb: (target group, activate) from keys 51 / 56
    pub link: Option<(u16, bool)>,
    pub flip_y: bool,
}

/// rotation is a multiple of 90° (the bounding box is the hitbox)
fn is_aligned(rot: f64) -> bool {
    let r = rot.rem_euclid(90.0);
    r < 1e-6 || r > 90.0 - 1e-6
}

/// `o` moved by (dx, dy) and turned `drot` degrees CW about its new center.
pub fn placed(o: &SimObject, dx: f64, dy: f64, drot: f64) -> SimObject {
    let mut s = o.clone();
    s.x += dx;
    s.y += dy;
    let (cx, cy) = (s.x, s.y);
    let (sn, cs) = (-drot.to_radians()).sin_cos();
    let f = |(px, py): (f64, f64)| -> (f64, f64) {
        let (rx, ry) = (px + dx - cx, py + dy - cy);
        if drot == 0.0 {
            (cx + rx, cy + ry)
        } else {
            (cx + rx * cs - ry * sn, cy + rx * sn + ry * cs)
        }
    };
    s.quad = o.quad.map(f);
    if let Some(sl) = &mut s.slope {
        sl.surf = (f(sl.surf.0), f(sl.surf.1));
        sl.tri = sl.tri.map(f);
    }
    s.rot = o.rot + drot;
    s.aligned = o.aligned && is_aligned(drot);
    s.rect = bbox(&s.quad);
    s
}

fn bbox(q: &[(f64, f64); 4]) -> (f64, f64, f64, f64) {
    let min_x = q.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
    let max_x = q.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
    let min_y = q.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let max_y = q.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    (min_x, min_y, max_x - min_x, max_y - min_y)
}

pub fn sim_object(obj: &GdObject, table: &ObjectTable, src: usize) -> SimObject {
    let kind = table.kind_of(obj.id);
    let def = table.objects.get(&obj.id.to_string());
    // world position: editor y=0 is the CENTER of ground-row cell [90,120],
    // i.e. world 105 (floor surface = GROUND_Y = 90). x stays raw editor units.
    let wx = obj.x as f64;
    let wy = obj.y as f64 + crate::physics::EDITOR_Y0;
    let raw = |k: u16| obj.raw.iter().find(|(key, _)| *key == k).map(|(_, v)| v.as_str());
    let (quad, aligned, slope, scale) = match def.and_then(|d| d.hitbox.as_ref()) {
        Some(hb) if hb.len() == 4 => {
            // objects.json hitbox = [h, w, ox, oy]: ox/oy is the rect's
            // bottom-left corner relative to the object center.
            let (h, w, ox, oy) = (hb[0], hb[1], hb[2], hb[3]);
            let sx = if obj.scale_x != 0.0 { obj.scale_x.abs() as f64 } else { 1.0 };
            let sy = if obj.scale_y != 0.0 { obj.scale_y.abs() as f64 } else { 1.0 };
            let w = w * sx;
            let h = h * sy;
            // hitbox rect corners (BL, BR, TL, TR) around the object center
            let x0 = wx + ox * sx;
            let y0 = wy + oy * sy;
            let mut pts = [
                (x0, y0),           // 0 BL
                (x0 + w, y0),       // 1 BR
                (x0, y0 + h),       // 2 TL
                (x0 + w, y0 + h),   // 3 TR
            ];
            // flips mirror points around the object center
            for q in pts.iter_mut() {
                // a negative scale mirrors too (abs() above only sizes it)
                if obj.flip_x != (obj.scale_x < 0.0) {
                    q.0 = 2.0 * wx - q.0;
                }
                if obj.flip_y != (obj.scale_y < 0.0) {
                    q.1 = 2.0 * wy - q.1;
                }
            }
            // rotate around the object center
            // GD editor rotation is CLOCKWISE on the y-up grid (decompiled
            // GCCRotation transform); rot_pt math is CCW, so negate.
            let t = -(obj.rot as f64).to_radians();
            let (c, s) = (t.cos(), t.sin());
            let rot_pt = |(px, py): (f64, f64)| -> (f64, f64) {
                let dx = px - wx;
                let dy = py - wy;
                (wx + dx * c - dy * s, wy + dx * s + dy * c)
            };
            let pts = pts.map(rot_pt);
            let slope = if kind == ObjectKind::Slope {
                Some(SlopeGeom { surf: (pts[0], pts[3]), tri: [pts[0], pts[1], pts[3]] })
            } else {
                None
            };
            let quad = [pts[0], pts[1], pts[3], pts[2]];
            (quad, is_aligned(obj.rot as f64), slope, sx.max(sy))
        }
        _ => {
            let (x0, y0) = (wx - 15.0, wy - 15.0);
            ([(x0, y0), (x0 + 30.0, y0), (x0 + 30.0, y0 + 30.0), (x0, y0 + 30.0)], true, None, 1.0)
        }
    };
    SimObject {
        id: obj.id,
        kind,
        x: wx,
        y: wy,
        src,
        rot: obj.rot as f64,
        rect: bbox(&quad),
        quad,
        aligned,
        radius: def.and_then(|d| d.hitbox_radius).map(|r| r * scale),
        slope,
        tele_dy: raw(54).and_then(|v| v.parse().ok()).unwrap_or(0.0),
        link: raw(51).and_then(|v| v.parse().ok()).map(|g| (g, raw(56) == Some("1"))),
        flip_y: obj.flip_y,
        groups: obj.groups.clone(),
    }
}

/// Uniform spatial hash; cell 120 units covers the widest portal hitbox.
pub struct Grid {
    cell: f64,
    map: HashMap<(i64, i64), Vec<usize>>,
    objects: Vec<SimObject>,
    /// level object index -> grid index
    by_src: HashMap<usize, usize>,
    /// per group: bounding box (x0, y0, x1, y1) of its grid objects
    group_box: HashMap<u16, (f64, f64, f64, f64)>,
}

impl Grid {
    pub fn build(objects: Vec<SimObject>) -> Grid {
        let cell = 120.0;
        let mut map: HashMap<(i64, i64), Vec<usize>> = HashMap::default();
        for (i, o) in objects.iter().enumerate() {
            let (rx, ry, rw, rh) = o.rect;
            // rect already bounds scaled/rotated hitboxes; small trigger
            // moves are covered by the query pad
            let x0 = (rx / cell).floor() as i64;
            let x1 = ((rx + rw) / cell).floor() as i64;
            let y0 = (ry / cell).floor() as i64;
            let y1 = ((ry + rh) / cell).floor() as i64;
            for cx in x0..=x1 {
                for cy in y0..=y1 {
                    map.entry((cx, cy)).or_default().push(i);
                }
            }
        }
        let by_src = objects.iter().enumerate().map(|(i, o)| (o.src, i)).collect();
        let mut group_box: HashMap<u16, (f64, f64, f64, f64)> = HashMap::default();
        for o in &objects {
            let (rx, ry, rw, rh) = o.rect;
            for &g in &o.groups {
                let b = group_box.entry(g).or_insert((rx, ry, rx + rw, ry + rh));
                *b = (b.0.min(rx), b.1.min(ry), b.2.max(rx + rw), b.3.max(ry + rh));
            }
        }
        Grid { cell, map, objects, by_src, group_box }
    }

    /// Could `query(x, y, w, h, pad)` return a member of group `g`? False for
    /// groups with no grid objects (pure decoration) or far from the rect.
    /// Margin = query pad + one cell, so it never drops a real hit.
    pub fn group_may_hit(&self, g: u16, x: f64, y: f64, w: f64, h: f64, pad: f64) -> bool {
        let m = pad + self.cell;
        self.group_box
            .get(&g)
            .is_some_and(|b| b.0 - m <= x + w && b.2 + m >= x && b.1 - m <= y + h && b.3 + m >= y)
    }

    pub fn index_of(&self, src: usize) -> Option<usize> {
        self.by_src.get(&src).copied()
    }

    pub fn all(&self) -> &[SimObject] {
        &self.objects
    }

    /// Indices whose cell rectangle is near the query rect (padded by pad units).
    pub fn query(&self, x: f64, y: f64, w: f64, h: f64, pad: f64) -> Vec<usize> {
        let x0 = ((x - pad) / self.cell).floor() as i64;
        let x1 = ((x + w + pad) / self.cell).floor() as i64;
        let y0 = ((y - pad) / self.cell).floor() as i64;
        let y1 = ((y + h + pad) / self.cell).floor() as i64;
        let mut seen = Vec::new();
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(v) = self.map.get(&(cx, cy)) {
                    seen.extend_from_slice(v);
                }
            }
        }
        seen.sort_unstable();
        seen.dedup();
        seen
    }
}

