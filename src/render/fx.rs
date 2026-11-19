//! GD effects driven by the game's own data files: cocos2d particle emitters
//! (`*Effect.plist`), robot/spider part animations (`*_AnimDesc.plist`) and
//! BMFont text (`bigFont-uhd.fnt`). Particles live in world units (y up),
//! like the plists themselves.

use std::collections::HashMap;
use tiny_skia::Pixmap;

use crate::assets::Assets;

/// Minimal XML plist value (enough for cocos particle/anim files).
#[derive(Debug, Clone)]
pub enum Pv {
    Dict(Vec<(String, Pv)>),
    Str(String),
    Arr(Vec<Pv>),
}

impl Pv {
    pub fn get(&self, k: &str) -> Option<&Pv> {
        match self {
            Pv::Dict(v) => v.iter().find(|(kk, _)| kk == k).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn num(&self, k: &str) -> f32 {
        match self.get(k) {
            Some(Pv::Str(s)) => s.trim().parse().unwrap_or(0.0),
            _ => 0.0,
        }
    }
    pub fn str(&self, k: &str) -> &str {
        match self.get(k) {
            Some(Pv::Str(s)) => s,
            _ => "",
        }
    }
}

/// Parse an XML plist (dict/array/string/real/integer/true/false; data blobs
/// are kept as strings). None on malformed input.
pub fn parse_plist(text: &str) -> Option<Pv> {
    let start = text.find("<plist")?;
    let mut s = &text[text[start..].find('>')? + start + 1..];
    fn tag<'a>(s: &mut &'a str) -> Option<&'a str> {
        let i = s.find('<')?;
        let j = s[i..].find('>')? + i;
        let t = &s[i + 1..j];
        *s = &s[j + 1..];
        Some(t)
    }
    fn value(s: &mut &str, open: &str) -> Option<Pv> {
        let name = open.trim_end_matches('/').trim();
        if open.ends_with('/') {
            return Some(match name {
                "dict" => Pv::Dict(Vec::new()),
                "array" => Pv::Arr(Vec::new()),
                "true" => Pv::Str("1".into()),
                "false" => Pv::Str("0".into()),
                _ => Pv::Str(String::new()),
            });
        }
        match name {
            "dict" => {
                let mut v = Vec::new();
                loop {
                    let t = tag(s)?;
                    if t == "/dict" {
                        return Some(Pv::Dict(v));
                    }
                    if t != "key" {
                        return None;
                    }
                    let end = s.find("</key>")?;
                    let k = s[..end].to_string();
                    *s = &s[end + 6..];
                    let open = tag(s)?;
                    v.push((k, value(s, open)?));
                }
            }
            "array" => {
                let mut v = Vec::new();
                loop {
                    let t = tag(s)?;
                    if t == "/array" {
                        return Some(Pv::Arr(v));
                    }
                    v.push(value(s, t)?);
                }
            }
            _ => {
                let close = format!("</{name}>");
                let end = s.find(&close)?;
                let txt = s[..end].to_string();
                *s = &s[end + close.len()..];
                Some(Pv::Str(txt))
            }
        }
    }
    let open = tag(&mut s)?;
    value(&mut s, open)
}

/// cocos2d ParticleSystem settings, as stored in GD's effect plists.
#[derive(Debug, Clone, Default)]
pub struct Emitter {
    pub radial: bool,
    /// seconds; < 0 = runs forever
    pub duration: f32,
    pub max: f32,
    pub life: (f32, f32),
    pub angle: (f32, f32),
    pub speed: (f32, f32),
    pub gravity: (f32, f32),
    pub size0: (f32, f32),
    /// < 0 = same as start
    pub size1: (f32, f32),
    pub c0: [f32; 4],
    pub c0v: [f32; 4],
    pub c1: [f32; 4],
    pub c1v: [f32; 4],
    pub src_var: (f32, f32),
    pub radius_max: (f32, f32),
    pub radius_min: f32,
    pub spin: (f32, f32),
    /// glBlendFunc dst == GL_ONE: additive
    pub additive: bool,
}

impl Emitter {
    pub fn from_plist(p: &Pv) -> Emitter {
        let n = |k: &str| p.num(k);
        let rgba = |pre: &str| ["Red", "Green", "Blue", "Alpha"].map(|c| n(&format!("{pre}{c}")));
        Emitter {
            radial: n("emitterType") == 1.0,
            duration: n("duration"),
            max: n("maxParticles"),
            life: (n("particleLifespan"), n("particleLifespanVariance")),
            angle: (n("angle"), n("angleVariance")),
            speed: (n("speed"), n("speedVariance")),
            gravity: (n("gravityx"), n("gravityy")),
            size0: (n("startParticleSize"), n("startParticleSizeVariance")),
            size1: (n("finishParticleSize"), n("finishParticleSizeVariance")),
            c0: rgba("startColor"),
            c0v: rgba("startColorVariance"),
            c1: rgba("finishColor"),
            c1v: rgba("finishColorVariance"),
            src_var: (n("sourcePositionVariancex"), n("sourcePositionVariancey")),
            radius_max: (n("maxRadius"), n("maxRadiusVariance")),
            radius_min: n("minRadius"),
            spin: (n("rotatePerSecond"), n("rotatePerSecondVariance")),
            additive: n("blendFuncDestination") == 1.0,
        }
    }

    /// cocos emissionRate = maxParticles / lifespan (particles per second)
    pub fn rate(&self) -> f32 {
        self.max / self.life.0.max(0.05)
    }

    /// Recolor: start rgb := `rgb`, finish rgb scaled by the plist's finish
    /// brightness (keeps "fade to black" additive effects fading).
    pub fn tinted(&self, rgb: (u8, u8, u8)) -> Emitter {
        let mut e = self.clone();
        let c = [rgb.0, rgb.1, rgb.2].map(|v| v as f32 / 255.0);
        let k = (self.c1[0] + self.c1[1] + self.c1[2]) / 3.0;
        for i in 0..3 {
            e.c0[i] = c[i];
            e.c1[i] = c[i] * k;
            e.c0v[i] = 0.05;
            e.c1v[i] = 0.0;
        }
        e
    }
}

#[derive(Debug, Clone)]
struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    g: (f32, f32),
    // radial mode: orbit around (ox, oy)
    radial: Option<(f32, f32, f32, f32, f32, f32)>, // ox, oy, angle, radius, d_radius, d_angle
    age: f32,
    life: f32,
    s0: f32,
    s1: f32,
    c0: [f32; 4],
    c1: [f32; 4],
    add: bool,
}

/// All live particles (world units, y up).
#[derive(Default)]
pub struct Particles {
    parts: Vec<Particle>,
    rng: u64,
}

impl Particles {
    /// uniform in [-1, 1)
    pub fn rand(&mut self) -> f32 {
        self.rnd()
    }

    fn rnd(&mut self) -> f32 {
        // xorshift64*, in [-1, 1)
        let mut x = self.rng.max(1);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        ((x.wrapping_mul(0x2545F4914F6CDD1D) >> 40) as f32 / (1u64 << 23) as f32) - 1.0
    }

    pub fn len(&self) -> usize {
        self.parts.len()
    }

    pub fn clear(&mut self) {
        self.parts.clear();
    }

    /// Spawn `n` particles at (x, y). `flip` mirrors the emitter vertically
    /// (gravity-flipped player); `dir` rotates its angle by degrees.
    pub fn spawn(&mut self, e: &Emitter, x: f32, y: f32, n: usize, flip: bool, dir: f32) {
        let fy = if flip { -1.0 } else { 1.0 };
        for _ in 0..n {
            let life = (e.life.0 + e.life.1 * self.rnd()).max(0.02);
            let px = x + e.src_var.0 * self.rnd();
            let py = y + e.src_var.1 * self.rnd() * fy;
            let ang = (e.angle.0 + e.angle.1 * self.rnd() + dir).to_radians();
            let s0 = (e.size0.0 + e.size0.1 * self.rnd()).max(0.0);
            let s1 = if e.size1.0 < 0.0 { s0 } else { (e.size1.0 + e.size1.1 * self.rnd()).max(0.0) };
            let mut col = |c: [f32; 4], v: [f32; 4]| [0, 1, 2, 3].map(|i| (c[i] + v[i] * self.rnd()).clamp(0.0, 1.0));
            let (c0, c1) = (col(e.c0, e.c0v), col(e.c1, e.c1v));
            let radial = e.radial.then(|| {
                let r0 = e.radius_max.0 + e.radius_max.1 * self.rnd();
                let da = (e.spin.0 + e.spin.1 * self.rnd()).to_radians();
                (px, py, ang, r0, (e.radius_min - r0) / life, da)
            });
            let speed = e.speed.0 + e.speed.1 * self.rnd();
            self.parts.push(Particle {
                x: px,
                y: py,
                vx: ang.cos() * speed,
                vy: ang.sin() * speed * fy,
                g: (e.gravity.0, e.gravity.1 * fy),
                radial,
                age: 0.0,
                life,
                s0,
                s1,
                c0,
                c1,
                add: e.additive,
            });
        }
    }

    /// Continuous emission for `dt` s; `acc` carries the fractional count.
    pub fn emit(&mut self, e: &Emitter, x: f32, y: f32, dt: f32, acc: &mut f32, flip: bool, dir: f32) {
        *acc += e.rate() * dt;
        let n = acc.floor();
        *acc -= n;
        self.spawn(e, x, y, (n as usize).min(64), flip, dir);
    }

    pub fn update(&mut self, dt: f32) {
        for p in &mut self.parts {
            p.age += dt;
            if let Some((ox, oy, a, r, dr, da)) = &mut p.radial {
                *a += *da * dt;
                *r += *dr * dt;
                // cocos radial mode: pos = -(cos, sin)(angle) * radius
                p.x = *ox - a.cos() * *r;
                p.y = *oy - a.sin() * *r;
            } else {
                p.vx += p.g.0 * dt;
                p.vy += p.g.1 * dt;
                p.x += p.vx * dt;
                p.y += p.vy * dt;
            }
        }
        self.parts.retain(|p| p.age < p.life);
        // bound the cost of a long effect-heavy section
        if self.parts.len() > 6000 {
            let n = self.parts.len() - 6000;
            self.parts.drain(..n);
        }
    }

    /// Squares (GD's `square.png` particle) into an RGBA8 premultiplied
    /// buffer. `to_px` maps world -> pixel, `ppu` = pixels per unit.
    pub fn draw(&self, buf: &mut [u8], w: usize, h: usize, to_px: impl Fn(f32, f32) -> (f32, f32), ppu: f32) {
        for p in &self.parts {
            let t = (p.age / p.life).clamp(0.0, 1.0);
            let size = (p.s0 + (p.s1 - p.s0) * t) * ppu;
            if size < 0.5 {
                continue;
            }
            let c = [0, 1, 2, 3].map(|i| p.c0[i] + (p.c1[i] - p.c0[i]) * t);
            let (cx, cy) = to_px(p.x, p.y);
            let half = size * 0.5;
            let (x0, x1) = (((cx - half).max(0.0) as usize).min(w), ((cx + half).max(0.0) as usize).min(w));
            let (y0, y1) = (((cy - half).max(0.0) as usize).min(h), ((cy + half).max(0.0) as usize).min(h));
            if x0 >= x1 || y0 >= y1 {
                continue; // off screen
            }
            let a = c[3];
            let src = [c[0] * a * 255.0, c[1] * a * 255.0, c[2] * a * 255.0];
            for y in y0..y1 {
                let row = &mut buf[(y * w + x0) * 4..(y * w + x1) * 4];
                for d in row.chunks_exact_mut(4) {
                    for i in 0..3 {
                        let v = if p.add { d[i] as f32 + src[i] } else { d[i] as f32 * (1.0 - a) + src[i] };
                        d[i] = v.min(255.0) as u8;
                    }
                }
            }
        }
    }
}

/// One sprite of an animation frame (cocos points, y up, degrees CW).
#[derive(Debug, Clone)]
pub struct Part {
    pub tex: String,
    pub x: f32,
    pub y: f32,
    pub sx: f32,
    pub sy: f32,
    pub rot: f32,
    pub fx: bool,
    pub fy: bool,
    pub z: i32,
}

/// Animations by name ("run", "jump", "fall", ...) -> frames of parts, z-sorted.
pub type Anims = HashMap<String, Vec<Vec<Part>>>;

pub fn parse_anims(text: &str) -> Option<Anims> {
    let root = parse_plist(text)?;
    let Pv::Dict(frames) = root.get("animationContainer")? else { return None };
    let pair = |s: &str| -> (f32, f32) {
        let v: Vec<f32> = s.trim_matches(|c| c == '{' || c == '}').split(',').filter_map(|t| t.trim().parse().ok()).collect();
        (v.first().copied().unwrap_or(0.0), v.get(1).copied().unwrap_or(0.0))
    };
    let mut named: Vec<(String, usize, Vec<Part>)> = Vec::new();
    for (key, frame) in frames {
        // "Robot_run_001.png" -> ("run", 1)
        let stem = key.trim_end_matches(".png");
        let mut it = stem.rsplitn(2, '_');
        let idx: usize = it.next()?.parse().ok()?;
        let name = it.next()?.split_once('_')?.1.to_string();
        let Pv::Dict(sprites) = frame else { continue };
        let mut parts: Vec<Part> = sprites
            .iter()
            .map(|(_, sp)| {
                let (x, y) = pair(sp.str("position"));
                let (sx, sy) = pair(sp.str("scale"));
                let (fx, fy) = pair(sp.str("flipped"));
                Part {
                    tex: sp.str("texture").to_string(),
                    x,
                    y,
                    sx,
                    sy,
                    rot: sp.num("rotation"),
                    fx: fx != 0.0,
                    fy: fy != 0.0,
                    z: sp.num("zValue") as i32,
                }
            })
            .collect();
        parts.sort_by_key(|p| p.z);
        named.push((name, idx, parts));
    }
    named.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    let mut out = Anims::new();
    for (name, _, parts) in named {
        out.entry(name).or_default().push(parts);
    }
    Some(out)
}

/// BMFont text (GD's bigFont): glyph pixmaps cut from the page once.
pub struct Font {
    glyphs: HashMap<char, (Pixmap, f32, f32, f32)>, // pixmap, xoffset, yoffset, xadvance (font px)
    pub line_h: f32,
}

impl Font {
    /// `fnt` is the font description's file name in the asset source; its
    /// page (named inside the file) is read from the same source.
    pub fn load(assets: &Assets, fnt: &str) -> Option<Font> {
        let text = assets.text(fnt)?;
        let kv = |line: &str, k: &str| -> Option<String> {
            line.split_whitespace().find_map(|t| t.strip_prefix(&format!("{k}=")).map(|v| v.trim_matches('"').to_string()))
        };
        let page_name = text.lines().find(|l| l.starts_with("page ")).and_then(|l| kv(l, "file"))?;
        let page = assets.png(&page_name)?;
        let line_h = text.lines().find(|l| l.starts_with("common ")).and_then(|l| kv(l, "lineHeight"))?.parse().ok()?;
        let mut glyphs = HashMap::new();
        for l in text.lines().filter(|l| l.starts_with("char ")) {
            let n = |k| kv(l, k).and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
            let Some(ch) = char::from_u32(n("id") as u32) else { continue };
            let (x, y, w, h) = (n("x"), n("y"), n("width"), n("height"));
            let pm = Pixmap::new(w.max(1) as u32, h.max(1) as u32).and_then(|mut pm| {
                pm.draw_pixmap(-x, -y, page.as_ref(), &tiny_skia::PixmapPaint::default(), tiny_skia::Transform::identity(), None);
                Some(pm)
            });
            if let Some(pm) = pm {
                glyphs.insert(ch, (pm, n("xoffset") as f32, n("yoffset") as f32, n("xadvance") as f32));
            }
        }
        Some(Font { glyphs, line_h })
    }

    pub fn width(&self, s: &str) -> f32 {
        s.chars().filter_map(|c| self.glyphs.get(&c)).map(|g| g.3).sum()
    }

    /// Draw `s` centered at (cx, cy) with `scale` (font px -> canvas px), alpha 0..1.
    pub fn draw(&self, canvas: &mut Pixmap, s: &str, cx: f32, cy: f32, scale: f32, alpha: f32) {
        let mut x = cx - self.width(s) * scale * 0.5;
        let top = cy - self.line_h * scale * 0.5;
        let paint = tiny_skia::PixmapPaint { opacity: alpha.clamp(0.0, 1.0), quality: tiny_skia::FilterQuality::Bilinear, ..Default::default() };
        for c in s.chars() {
            let Some((pm, xo, yo, adv)) = self.glyphs.get(&c) else { continue };
            let t = tiny_skia::Transform::from_translate(x + xo * scale, top + yo * scale).pre_scale(scale, scale);
            canvas.draw_pixmap(0, 0, pm.as_ref(), &paint, t, None);
            x += adv * scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_emitter_and_runs_radial_inward() {
        let xml = r#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>emitterType</key><real>1</real><key>maxParticles</key><integer>30</integer>
            <key>particleLifespan</key><real>0.5</real><key>maxRadius</key><real>40</real>
            <key>minRadius</key><real>0</real><key>startColorAlpha</key><real>1</real>
            <key>startParticleSize</key><real>6</real><key>blendFuncDestination</key><integer>1</integer>
            <key>textureFileName</key><string>square.png</string><key>nested</key><dict/>
            </dict></plist>"#;
        let e = Emitter::from_plist(&parse_plist(xml).unwrap());
        assert!(e.radial && e.additive && e.max == 30.0 && (e.rate() - 60.0).abs() < 1e-3);
        let mut ps = Particles::default();
        ps.spawn(&e, 100.0, 50.0, 1, false, 0.0);
        ps.update(0.01);
        let d0 = ((ps.parts[0].x - 100.0).powi(2) + (ps.parts[0].y - 50.0).powi(2)).sqrt();
        ps.update(0.3);
        let d1 = ((ps.parts[0].x - 100.0).powi(2) + (ps.parts[0].y - 50.0).powi(2)).sqrt();
        assert!(d0 > 35.0 && d1 < d0, "radial particles flow inward: {d0} -> {d1}");
        // drawing clips particles that are partly or fully off the buffer
        let mut buf = vec![0u8; 8 * 8 * 4];
        ps.draw(&mut buf, 8, 8, |x, y| (x - 90.0, y - 45.0), 10.0);
        ps.draw(&mut buf, 8, 8, |_, _| (100.0, -50.0), 10.0);
        ps.update(1.0);
        assert_eq!(ps.len(), 0, "dead after its lifespan");
    }

    #[test]
    fn parses_gd_robot_animation() {
        let Some(dir) = crate::assets::find() else { return }; // no GD install on this machine
        let Ok(text) = std::fs::read_to_string(dir.join("Robot_AnimDesc.plist")) else { return };
        let a = parse_anims(&text).expect("anim plist");
        assert_eq!(a["run"].len(), 16);
        assert_eq!(a["jump_start"].len() + a["jump_loop"].len(), 10);
        assert!(a["run"][0].iter().any(|p| p.tex == "robot_01_01_001.png"));
    }
}
