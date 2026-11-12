//! The glyph atlas: one CPU raster of every glyph drawn so far, uploaded to the GPU when it
//! changes, so a whole terminal grid draws with one `draw_atlas` call.
//!
//! Answers: where in a texture is each glyph's image?
//! Must not know: terminals, cells, colors, or what the glyphs spell.
//!
//! Glyphs are drawn white; `draw_atlas` with `BlendMode::Modulate` tints each one. Each slot
//! is two cells wide plus padding, so wide (CJK) glyphs and overhanging strokes fit.

use std::collections::HashMap;

use skia_safe::{Color, Image, Paint, Rect, TextBlobBuilder, gpu};

use crate::fonts::{Fonts, Glyph};

/// Side of the square atlas texture, in pixels.
const SIDE: i32 = 2048;
/// Padding around each glyph inside its slot, so antialiased edges never bleed between slots.
pub const PAD: f32 = 4.0;

pub struct Atlas {
    surface: skia_safe::Surface,
    image: Option<Image>,
    dirty: bool,
    slots: HashMap<Glyph, Rect>,
    next: (f32, f32),
    slot: (f32, f32),
    ascent: f32,
    glyph_x: f32,
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
            generation: 0,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The slot holding `g`, drawing it first if needed. The glyph's origin sits at
    /// (`PAD` + glyph_x, `PAD` + ascent) inside the slot. A full atlas is wiped and `generation` bumps.
    pub fn get(&mut self, fonts: &Fonts, g: Glyph) -> Rect {
        if let Some(r) = self.slots.get(&g) {
            return *r;
        }
        let (sw, sh) = self.slot;
        if self.next.0 + sw > SIDE as f32 {
            self.next = (0.0, self.next.1 + sh);
        }
        if self.next.1 + sh > SIDE as f32 {
            // Spike problem 10: an LRU would evict only what is unused.
            self.slots.clear();
            self.surface.canvas().clear(Color::TRANSPARENT);
            self.next = (0.0, 0.0);
            self.generation += 1;
        }
        let r = Rect::from_xywh(self.next.0, self.next.1, sw, sh);
        self.next.0 += sw;
        let mut b = TextBlobBuilder::new();
        let (gs, xs) = b.alloc_run_pos_h(fonts.font(g), 1, 0.0, None);
        gs[0] = g.id;
        xs[0] = 0.0;
        if let Some(blob) = b.make() {
            let mut p = Paint::default();
            p.set_color(Color::WHITE).set_anti_alias(true);
            self.surface.canvas().draw_text_blob(&blob, (r.left + PAD + self.glyph_x, r.top + PAD + self.ascent), &p);
        }
        self.slots.insert(g, r);
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
