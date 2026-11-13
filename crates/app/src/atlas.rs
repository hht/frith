//! The glyph atlas: one CPU raster of every glyph drawn so far, uploaded to the GPU when it
//! changes, so a whole terminal grid draws with one `draw_atlas` call.
//!
//! Answers: where in a texture is each glyph's image?
//! Must not know: terminals, cells, colors, or what the glyphs spell.
//!
//! Glyphs are drawn white; `draw_atlas` with `BlendMode::Modulate` tints each one. Each slot
//! is two cells wide plus padding, so wide (CJK) glyphs and overhanging strokes fit.
//! Font glyphs are centered in the cell as Ghostty places them; sprites fill it exactly.

use std::collections::HashMap;

use skia_safe::{Color, Image, Paint, Rect, TextBlobBuilder, gpu};

use crate::fonts::{Fonts, Glyph};
use crate::sprite;

/// What a slot holds: a glyph from a font, or a character Frith draws itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    Glyph(Glyph),
    Sprite(char),
}

/// Side of the square atlas texture, in pixels.
const SIDE: i32 = 2048;
/// Padding around each glyph inside its slot, so antialiased edges never bleed between slots.
pub const PAD: f32 = 4.0;

pub struct Atlas {
    surface: skia_safe::Surface,
    image: Option<Image>,
    dirty: bool,
    slots: HashMap<Key, Rect>,
    next: (f32, f32),
    slot: (f32, f32),
    ascent: f32,
    glyph_x: f32,
    sprite_cell: sprite::Cell,
    /// Bumped whenever the atlas is wiped; slot rects from an older generation are stale.
    generation: u64,
}

impl Atlas {
    pub fn new(fonts: &Fonts) -> Atlas {
        Atlas {
            surface: skia_safe::surfaces::raster_n32_premul((SIDE, SIDE)).expect("atlas surface"),
            image: None,
            dirty: true,
            slots: HashMap::new(),
            next: (0.0, 0.0),
            slot: ((fonts.cell_w * 2.0 + PAD * 2.0).ceil(), (fonts.cell_h + PAD * 2.0).ceil()),
            ascent: fonts.ascent,
            glyph_x: fonts.glyph_x,
            sprite_cell: sprite::Cell {
                w: fonts.cell_w as u32,
                h: fonts.cell_h as u32,
                thickness: fonts.box_thickness,
            },
            generation: 0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The slot holding `key`, drawing it first if needed. The cell's top left sits at
    /// (`PAD`, `PAD`) inside the slot. A full atlas is wiped and `generation` bumps.
    pub fn get(&mut self, fonts: &Fonts, key: Key) -> Rect {
        if let Some(r) = self.slots.get(&key) {
            return *r;
        }
        let (sw, sh) = self.slot;
        if self.next.0 + sw > SIDE as f32 {
            self.next = (0.0, self.next.1 + sh);
        }
        if self.next.1 + sh > SIDE as f32 {
            // An LRU would evict only what is unused.
            self.slots.clear();
            self.surface.canvas().clear(Color::TRANSPARENT);
            self.next = (0.0, 0.0);
            self.generation += 1;
        }
        let r = Rect::from_xywh(self.next.0, self.next.1, sw, sh);
        self.next.0 += sw;
        let canvas = self.surface.canvas();
        match key {
            Key::Glyph(g) => {
                let mut b = TextBlobBuilder::new();
                let (gs, xs) = b.alloc_run_pos_h(fonts.font(g), 1, 0.0, None);
                gs[0] = g.id;
                xs[0] = 0.0;
                if let Some(blob) = b.make() {
                    let mut p = Paint::default();
                    p.set_color(Color::WHITE).set_anti_alias(true);
                    canvas.draw_text_blob(&blob, (r.left + PAD + self.glyph_x, r.top + PAD + self.ascent), &p);
                }
            }
            Key::Sprite(c) => {
                canvas.save();
                canvas.translate((r.left + PAD, r.top + PAD));
                sprite::draw(canvas, c, self.sprite_cell);
                canvas.restore();
            }
        }
        self.slots.insert(key, r);
        self.dirty = true;
        r
    }

    /// The atlas as a GPU texture, uploaded again only when glyphs were added.
    pub fn image(&mut self, ctx: &mut gpu::DirectContext) -> Image {
        if self.dirty || self.image.is_none() {
            let snap = self.surface.image_snapshot();
            self.image = snap.new_texture_image(ctx, gpu::Mipmapped::No);
            self.dirty = false;
        }
        self.image.clone().expect("atlas texture upload")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::ImageInfo;

    #[test]
    fn a_sprite_sits_exactly_on_the_cell_at_the_slots_padding() {
        // JetBrains Mono at 24 px shifts its glyphs 0.2 px left; sprites must not shift. An
        // antialiased sprite shows a fractional shift that a whole-pixel block would round away.
        let fonts = Fonts::new(&[], 24.0);
        assert!(fonts.glyph_x != 0.0);
        let mut atlas = Atlas::new(&fonts);
        let r = atlas.get(&fonts, Key::Sprite('\u{e0b0}'));
        let (w, h) = (r.width() as i32, r.height() as i32);
        let info = ImageInfo::new_a8((w, h));
        let mut got = vec![0u8; (w * h) as usize];
        assert!(atlas.surface.read_pixels(&info, &mut got, w as usize, (r.left as i32, r.top as i32)));
        let mut alone = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        alone.canvas().clear(Color::TRANSPARENT);
        alone.canvas().translate((PAD, PAD));
        sprite::draw(alone.canvas(), '\u{e0b0}', atlas.sprite_cell);
        let mut want = vec![0u8; (w * h) as usize];
        assert!(alone.read_pixels(&info, &mut want, w as usize, (0, 0)));
        assert!(want.iter().any(|&a| a > 0 && a < 255), "the sprite has antialiased edges");
        assert_eq!(got, want);
    }
}
