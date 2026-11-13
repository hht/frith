//! How each pane looks on screen this frame: live as a tile, card or dock chip at a rect, or
//! morphing from its old look to its new one (ADR 10).
//!
//! Answers: how is a pane painted at a rect, and how is a morph composed?
//! Must not know: layout rules, springs or the clock, or window events. Callers pass each
//! pane's rect and morph progress.
//!
//! Content is anchored, never stretched: a terminal's cells have a fixed pixel size, so a pane
//! between two pane rects is drawn live at the moving rect with its grid at 1:1 from the top
//! left, clipped. Only a pane shrinking into its chip, or growing out of it, is scaled, by
//! one factor for both axes; its old look is an image captured once, when the change begins,
//! and cross-faded with the live new look as niri's `mix(prev, next, progress)`: the new
//! look in a layer at `progress`, the image over it at `1 − progress`, which holds for a
//! translucent chip too. A morph costs at most one image and one layer per moving pane per
//! frame, whatever the pane shows, and only while it turns into or out of its chip.
//! @see https://github.com/YaLTeR/niri/blob/589bc406b45c8b89f69f9071a61d90d77336089f/src/render_helpers/shaders/resize.frag

use std::collections::HashMap;
use std::time::Instant;

use frith_core::colors::Tint;
use frith_core::geom::Rect;
use frith_core::layout::PaneId;
use frith_core::scene::{self, Hit, Metrics};
use frith_term::Session;
use skia_safe::gpu::{self, DirectContext, SurfaceOrigin};
use skia_safe::{
    ClipOp, Color, FilterMode, Image, ImageInfo, Matrix, MipmapMode, Paint, RRect, Rect as SkRect, SamplingOptions,
};

use crate::atlas::Atlas;
use crate::grid::{self, Grid};
use crate::render::{self, PaneLook, Pen};

pub struct Pane {
    pub session: Session,
    pub grid: Grid,
    pub title: String,
    /// An OSC 9/777 notification arrived while the pane was not focused.
    pub attention: bool,
}

/// What a pane shows as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Tile,
    Card,
    Chip,
}

/// The shape of a pane's frame: what it shows as, its corner radius, and how much of a card
/// it looks, 0 to 1 (its shadow's and border's strength).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Shape {
    pub kind: Kind,
    pub corner: f32,
    pub card: f32,
}

/// How a pane looked when its change began: the shape it was drawn with then, and, when it
/// turns into or out of its chip, an image of it. A change mid-morph starts from the shape
/// and image on screen, so neither jumps.
pub struct Origin {
    pub shape: Shape,
    pub image: Option<Image>,
}

/// A morph at one moment, `progress` of the way from one shape to another in rect `r`.
#[derive(PartialEq, Debug)]
pub struct Blend {
    pub corner: f32,
    pub card: f32,
    /// Where the live new look is laid out, and the factor it is scaled by about `r`'s top
    /// left: a chip keeps its own size, a pane growing out of its chip is scaled into `r`,
    /// and any other pane is laid out at `r` itself.
    pub layout: Rect,
    pub scale: f32,
    /// The factor the origin's image is scaled by about `r`'s top left: it shrinks with `r`
    /// into a chip and otherwise stays 1:1.
    pub image_scale: f32,
}

/// The morph from `from` to `to` drawn at `r`, `progress` (0 to 1) of the way, for a pane the
/// layout puts at `target`; `image_w` is the width of the origin's image, if any.
pub fn blend(from: Shape, to: Shape, image_w: f32, target: Rect, r: Rect, progress: f32) -> Blend {
    let mix = |a: f32, b: f32| a + (b - a) * progress;
    let (layout, scale) = match (from.kind, to.kind) {
        (_, Kind::Chip) => (Rect::new(r.x, r.y, target.w, target.h), 1.0),
        (Kind::Chip, _) => (target, r.w / target.w),
        _ => (r, 1.0),
    };
    Blend {
        corner: mix(from.corner, to.corner).min(r.w.min(r.h) / 2.0).max(0.0),
        card: mix(from.card, to.card),
        layout,
        scale,
        image_scale: if to.kind == Kind::Chip { r.w / image_w } else { 1.0 },
    }
}

fn sk(r: Rect) -> SkRect {
    SkRect::from_xywh(r.x, r.y, r.w, r.h)
}

/// Everything a pane's look depends on this frame besides its rect.
pub struct Stage<'a> {
    pub panes: &'a mut HashMap<PaneId, Pane>,
    pub atlas: &'a mut Atlas,
    pub metrics: &'a Metrics,
    pub focus: Option<PaneId>,
    /// The pane under the pointer.
    pub hovered: Option<PaneId>,
    pub hover: Hit,
    pub zoomed: Option<PaneId>,
    /// Laid over unfocused panes' content.
    pub fade: Option<Tint>,
    /// Padding between a pane's content edge and its grid.
    pub pad: (f32, f32),
    pub now: Instant,
    /// The earliest end of a frame hold among the panes drawn.
    pub frame_due: Option<Instant>,
    /// The focused pane's cursor cell, in window coordinates.
    pub cursor: Option<SkRect>,
}

impl Stage<'_> {
    /// Paints `pane` live with `shape` (see `Shape`) as its whole rect is `r`. A card's shadow
    /// is left to the caller, since a morph fades it.
    pub fn live(&mut self, pen: &mut Pen, ctx: &mut DirectContext, pane: PaneId, shape: Shape, r: Rect) {
        let is_card = match shape.kind {
            Kind::Chip => {
                let p = &self.panes[&pane];
                return render::chip(pen, r, &p.title, p.attention, self.hover == Hit::Chip(pane));
            }
            Kind::Card => true,
            Kind::Tile => false,
        };
        let f = if is_card { scene::card(pane, r, self.metrics) } else { scene::tile(pane, r, self.metrics) };
        if is_card {
            render::card_clip(pen, &f, shape.corner);
        }
        let p = self.panes.get_mut(&pane).expect("every drawn pane exists");
        let focused = Some(pane) == self.focus;
        let held = p.session.hold(self.now);
        self.frame_due = [self.frame_due, held].into_iter().flatten().min();
        let accent = pen.chrome.accent;
        let look = grid::Look {
            content: sk(f.content),
            pad: self.pad,
            focused,
            cursor: Color::from_rgb(accent.r, accent.g, accent.b),
            update: held.is_none(),
        };
        let cursor = p.grid.draw(pen.canvas, pen.fonts, self.atlas, ctx, &p.session.term, &look);
        if let Some(t) = self.fade.filter(|_| !focused) {
            render::fade(pen, f.content, t);
        }
        if focused {
            self.cursor = cursor;
        }
        let look = PaneLook {
            title: &p.title,
            focused,
            hovered: self.hovered == Some(pane),
            attention: p.attention,
            zoomed: self.zoomed == Some(pane),
        };
        if is_card {
            render::card_over(pen, &f, &look, self.hover, shape.corner, shape.card);
        } else {
            render::tile_header(pen, &f, &look, self.hover);
        }
    }

    /// The shape a pane showing as `kind` has at rest.
    pub fn shape(&self, kind: Kind, scale: f32) -> Shape {
        match kind {
            Kind::Tile => Shape { kind, corner: 0.0, card: 0.0 },
            Kind::Card => Shape { kind, corner: render::radius(scale), card: 1.0 },
            Kind::Chip => Shape { kind, corner: self.metrics.target / 2.0, card: 0.0 },
        }
    }

    /// Paints `pane` morphing from `from` (by default its shape at rest as `kind`) into a
    /// `kind` at `target`, drawn at `r`, `progress` (0 to 1) of the way (see `blend`): clipped
    /// to `r` with the blended corner, a card's shadow and border at the blended strength, and
    /// the live new look cross-faded with the origin's image, if any, both anchored at `r`'s
    /// top left. Returns the blend.
    #[allow(clippy::too_many_arguments)]
    pub fn morph(
        &mut self,
        pen: &mut Pen,
        ctx: &mut DirectContext,
        pane: PaneId,
        kind: Kind,
        target: Rect,
        r: Rect,
        progress: f32,
        from: Option<&Origin>,
    ) -> Blend {
        let to = self.shape(kind, pen.scale);
        let image = from.and_then(|o| o.image.as_ref());
        let image_w = image.map_or(1.0, |i| i.width() as f32);
        let b = blend(from.map_or(to, |o| o.shape), to, image_w, target, r, progress);
        if b.card > 0.0 {
            render::card_shadow(pen, r, b.corner, b.card);
        }
        let canvas = pen.canvas;
        canvas.save();
        canvas.clip_rrect(RRect::new_rect_xy(sk(r), b.corner, b.corner), ClipOp::Intersect, true);
        canvas.save();
        // Lays the new look out at `b.layout` and maps its top left onto `r`'s, scaled by
        // `b.scale`: the identity unless a pane grows out of its chip.
        let m = Matrix::translate((r.x, r.y))
            * Matrix::scale((b.scale, b.scale))
            * Matrix::translate((-b.layout.x, -b.layout.y));
        canvas.concat(&m);
        if image.is_some() {
            canvas.save_layer_alpha_f(sk(b.layout), progress);
        }
        let cursor = self.cursor;
        let shape = Shape { kind, corner: b.corner / b.scale, card: b.card };
        self.live(pen, ctx, pane, shape, b.layout);
        if self.cursor != cursor {
            // The IME follows the cursor where it shows.
            self.cursor = self.cursor.map(|c| m.map_rect(c).0);
        }
        if image.is_some() {
            canvas.restore();
        }
        canvas.restore();
        if let Some(image) = image {
            let mut p = Paint::default();
            p.set_alpha_f(1.0 - progress);
            let sampling = SamplingOptions::new(FilterMode::Linear, MipmapMode::None);
            let (w, h) = (image.width() as f32 * b.image_scale, image.height() as f32 * b.image_scale);
            canvas.draw_image_rect_with_sampling_options(image, None, SkRect::from_xywh(r.x, r.y, w, h), sampling, &p);
        }
        canvas.restore();
        b
    }

    /// Captures what `paint` draws within `r`, to fade out over a morph; `None` for an empty
    /// rect or when the GPU cannot make the surface. The focused cursor stays as it was: the
    /// capture is not on screen.
    pub fn capture(
        &mut self,
        pen: &mut Pen,
        ctx: &mut DirectContext,
        r: Rect,
        paint: impl FnOnce(&mut Self, &mut Pen, &mut DirectContext),
    ) -> Option<Image> {
        let (w, h) = (r.w.ceil() as i32, r.h.ceil() as i32);
        if w <= 0 || h <= 0 {
            return None;
        }
        let info = ImageInfo::new_n32_premul((w, h), None);
        let mut surface = gpu::surfaces::render_target(
            ctx,
            gpu::Budgeted::Yes,
            &info,
            None,
            SurfaceOrigin::TopLeft,
            None,
            false,
            None,
        )?;
        let canvas = surface.canvas();
        canvas.clear(Color::TRANSPARENT);
        canvas.translate((-r.x, -r.y));
        let mut off = Pen { canvas, chrome: pen.chrome, ui: pen.ui, fonts: &mut *pen.fonts, scale: pen.scale };
        let cursor = self.cursor;
        paint(self, &mut off, ctx);
        self.cursor = cursor;
        Some(surface.image_snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TILE: Shape = Shape { kind: Kind::Tile, corner: 0.0, card: 0.0 };
    const CARD: Shape = Shape { kind: Kind::Card, corner: 10.0, card: 1.0 };
    const CHIP: Shape = Shape { kind: Kind::Chip, corner: 12.0, card: 0.0 };

    #[test]
    fn a_floated_tile_starts_square_without_card_chrome_and_ends_a_card() {
        let (target, r) = (Rect::new(100.0, 100.0, 400.0, 300.0), Rect::new(80.0, 90.0, 500.0, 350.0));
        let start = blend(TILE, CARD, 0.0, target, r, 0.0);
        assert_eq!((start.corner, start.card), (0.0, 0.0), "no jump at the start");
        assert_eq!((start.layout, start.scale), (r, 1.0), "drawn live at the moving rect, 1:1");
        let mid = blend(TILE, CARD, 0.0, target, r, 0.25);
        assert_eq!((mid.corner, mid.card), (2.5, 0.25));
        let end = blend(TILE, CARD, 0.0, target, target, 1.0);
        assert_eq!((end.corner, end.card), (10.0, 1.0));
    }

    #[test]
    fn a_card_shrinking_into_its_chip_scales_its_image_with_the_rect_and_keeps_the_chip_whole() {
        let chip = Rect::new(8.0, 570.0, 160.0, 24.0);
        // Captured at 640 wide: 1:1 at the start, half size at 320 wide, the chip's width at the end.
        let start = Rect::new(300.0, 100.0, 640.0, 480.0);
        assert_eq!(blend(CARD, CHIP, 640.0, chip, start, 0.0).image_scale, 1.0);
        let r = Rect::new(150.0, 300.0, 320.0, 200.0);
        let b = blend(CARD, CHIP, 640.0, chip, r, 0.5);
        assert_eq!(b.image_scale, 0.5);
        assert_eq!((b.layout, b.scale), (Rect::new(150.0, 300.0, 160.0, 24.0), 1.0), "the chip at its own size");
        assert_eq!((b.corner, b.card), (11.0, 0.5));
        assert_eq!(blend(CARD, CHIP, 640.0, chip, chip, 1.0).image_scale, 0.25);
    }

    #[test]
    fn a_card_growing_out_of_its_chip_is_scaled_into_the_rect_over_the_chip_at_one_to_one() {
        let target = Rect::new(300.0, 100.0, 640.0, 480.0);
        let r = Rect::new(100.0, 400.0, 320.0, 100.0);
        let b = blend(CHIP, CARD, 160.0, target, r, 0.5);
        assert_eq!((b.layout, b.scale), (target, 0.5));
        assert_eq!(b.image_scale, 1.0);
    }

    #[test]
    fn the_corner_never_exceeds_half_the_short_side() {
        let flat = Rect::new(0.0, 0.0, 300.0, 8.0);
        assert_eq!(blend(CARD, CARD, 0.0, flat, flat, 0.5).corner, 4.0);
    }

    #[test]
    fn a_morph_interrupted_midway_starts_from_the_shape_on_screen() {
        let shown = Shape { kind: Kind::Card, corner: 7.0, card: 0.4 };
        let r = Rect::new(0.0, 0.0, 400.0, 300.0);
        let b = blend(shown, CHIP, 400.0, Rect::new(8.0, 570.0, 160.0, 24.0), r, 0.0);
        assert_eq!((b.corner, b.card, b.image_scale), (7.0, 0.4, 1.0));
    }
}
