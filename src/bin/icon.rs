// reproduce draw_player's exact transform at known angles
use tiny_skia::{Pixmap, PixmapPaint, Transform};
fn main() {
    let assets = gdrust::assets::Assets::fs(std::path::Path::new(&gdrust::assets::find_or_ask()));
    let mut store = gdrust::render::atlas::SpriteStore::load(&assets);
    let (sprite, sscale) = store.sprite_sheet("dart_01_001.png").unwrap();
    for (name, deg) in [("d0", 0.0f32), ("dplus63", 63.4), ("dminus63", -63.4)] {
        let mut canvas = Pixmap::new(200, 200).unwrap();
        let s = 2.0 / sscale;
        let t = Transform::from_translate(100.0, 100.0)
            .pre_concat(Transform::from_rotate(-deg))
            .pre_concat(Transform::from_scale(s, s))
            .pre_concat(Transform::from_translate(
                -(sprite.width() as f32) * 0.5,
                -(sprite.height() as f32) * 0.5,
            ));
        canvas.draw_pixmap(0, 0, sprite.as_ref(), &PixmapPaint::default(), t, None);
        std::fs::write(format!("/tmp/opencode/t_{}.png", name), canvas.encode_png().unwrap()).unwrap();
    }
    println!("ok");
}
