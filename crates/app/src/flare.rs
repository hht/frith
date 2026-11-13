//! How panes opening and closing show (ADR 10): an opening pane's body is drawn live, faded
//! and scaled about its center; a closing pane is captured once as it closes and its image
//! fades the same way; both get a soft glow along the border and motes of light, in the
//! accent. Each flare shows at its pane's height: a tile's under every card, a card's among
//! the cards where the card is or was.
//!
//! Answers: which flares run, at which height, and how are they painted?
//! Must not know: layout rules or the clock; `frith_core::flare` decides what shows when.
//!
//! The cost is the same whatever the pane shows: a layer or one image for the body, two
//! strokes for the glow, and one `draw_atlas` of a cached sprite for all motes.

use std::time::{Duration, Instant};

use frith_core::config::Rgb;
use frith_core::flare::{Body, Flare, Look, Mote, Way};
use frith_core::geom::Rect;
use frith_core::layout::PaneId;
use frith_core::motion::Motion;
use frith_core::scene::Scene;
use skia_safe::gpu::DirectContext;
use skia_safe::gradient::{Colors, Gradient, Interpolation};
use skia_safe::{
    AlphaType, BlendMode, BlurStyle, Canvas, Color, Color4f, ColorType, FilterMode, Image, ImageInfo, MaskFilter,
    Matrix, MipmapMode, Paint, PaintStyle, RRect, RSXform, Rect as SkRect, SamplingOptions, TileMode, shaders,
    surfaces,
};

use crate::render::{self, Pen};
use crate::stage::{Kind, Origin, Pane, Placed, Stage};

fn sk(r: Rect) -> SkRect {
    SkRect::from_xywh(r.x, r.y, r.w, r.h)
}

/// Scales the canvas by `body.scale` about the center of `r`, the pane's whole rect, and
/// opens a layer at `body.alpha`, so what is drawn next shows as that body. The layer reaches
/// past `r` by a card's shadow, three blur sigmas and its offset at `px` pixels per point.
/// Returns the save count to restore to once the pane is drawn.
fn enter_body(canvas: &Canvas, r: Rect, body: Body, px: f32) -> usize {
    let n = canvas.save();
    canvas.concat(&about_center(r, body.scale));
    let reach = (3.0 * 12.0 + 6.0) * px;
    canvas.save_layer_alpha_f(sk(r).with_outset((reach, reach)), body.alpha);
    n
}

/// Scales by `k` about the center of `r`.
fn about_center(r: Rect, k: f32) -> Matrix {
    let c = r.center();
    Matrix::scale_translate((k, k), (c.x * (1.0 - k), c.y * (1.0 - k)))
}

/// Draws `image`, a pane captured at its whole rect `r`, as `body`, over a card's shadow
/// with corners of radius `corner` when `card`.
fn image_body(pen: &Pen, image: &Image, r: Rect, body: Body, (card, corner): (bool, f32)) {
    let canvas = pen.canvas;
    let n = canvas.save();
    canvas.concat(&about_center(r, body.scale));
    if card {
        render::card_shadow(pen, r, corner, body.alpha);
    }
    let mut p = Paint::default();
    p.set_alpha_f(body.alpha);
    let sampling = SamplingOptions::new(FilterMode::Linear, MipmapMode::None);
    canvas.draw_image_rect_with_sampling_options(image, None, sk(r), sampling, &p);
    canvas.restore_to_count(n);
}

/// Where a flare shows among the panes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layer {
    /// Over the tiles, under every card.
    Tiles,
    /// Just over this card.
    Over(PaneId),
}

/// A pane opening or closing.
struct Lit {
    pane: PaneId,
    flare: Flare,
    /// How a closing pane looked as it closed.
    image: Option<Image>,
    card: bool,
    corner: f32,
    /// A closing card stays just over the card it was over, if that one still shows.
    under: Option<PaneId>,
    layer: Layer,
}

/// How flares that start run: the configured durations, how much of the light shows, and
/// pixels per point.
pub struct Kindle {
    pub open: Duration,
    pub close: Duration,
    pub look: Look,
    pub px: f32,
}

impl Kindle {
    fn flare(&self, way: Way, pane: PaneId, rect: Rect, start: Instant) -> Flare {
        let duration = if way == Way::Open { self.open } else { self.close };
        let seed = u64::from(pane.0) << 1 | u64::from(way == Way::Close);
        Flare { way, rect, start, duration, look: self.look, seed, px: self.px }
    }
}

/// The flares running, and the panes whose flares start with the next frame.
pub struct Flares {
    /// Panes opened since the last frame.
    opened: Vec<PaneId>,
    /// Panes closed since the last frame, and whether each had the focus, kept until it
    /// captures how they looked; dropping one ends its program.
    closed: Vec<(PaneId, Pane, bool)>,
    lits: Vec<Lit>,
    painter: Painter,
}

impl Flares {
    pub fn new() -> Flares {
        Flares { opened: Vec::new(), closed: Vec::new(), lits: Vec::new(), painter: Painter::new() }
    }

    /// `pane` opened; its flare starts with the next frame.
    pub fn open(&mut self, pane: PaneId) {
        self.opened.push(pane);
    }

    /// `pane` closed, `focused` or not; the next frame captures it as it looked then and
    /// starts its flare.
    pub fn close(&mut self, pane: PaneId, p: Pane, focused: bool) {
        self.closed.push((pane, p, focused));
    }

    /// Ends the programs of the panes closed since the last frame, without their flares: a
    /// frame was skipped, and the next may not come for a long time.
    pub fn forget_closed(&mut self) {
        self.closed.clear();
    }

    /// Starts, at the stage's time, the flares of panes opened or closed since the last
    /// frame. `before` and `placed` are the panes as last drawn and now; `motion` still holds
    /// the springs of the last frame, so a pane closing mid-morph is captured where it was.
    pub fn start(
        &mut self,
        stage: &mut Stage,
        pen: &mut Pen,
        ctx: &mut DirectContext,
        k: &Kindle,
        (before, placed): (&[Placed], &[Placed]),
        motion: &Motion,
    ) {
        let now = stage.now;
        for (pane, p, focused) in self.closed.drain(..) {
            let Some(i) = before.iter().position(|b| b.0 == pane && b.2 != Kind::Chip) else { continue };
            let (_, at, kind) = before[i];
            let r = motion.rect(pane, at, now);
            stage.panes.insert(pane, p);
            let shape = stage.shape(kind, pen.scale);
            // Focus has moved on, but the pane fades as it was.
            let focus = stage.focus;
            stage.focus = if focused { Some(pane) } else { focus };
            let image = stage.capture(pen, ctx, r, |st, pen, ctx| st.live(pen, ctx, pane, shape, r));
            stage.focus = focus;
            stage.panes.remove(&pane);
            let under = before[..i].iter().rev().find(|b| b.2 == Kind::Card).map(|b| b.0);
            let under = if kind == Kind::Card { under } else { None };
            let (flare, card) = (k.flare(Way::Close, pane, r, now), kind == Kind::Card);
            self.lits.push(Lit { pane, flare, image, card, corner: shape.corner, under, layer: Layer::Tiles });
        }
        for pane in self.opened.drain(..) {
            let Some(&(_, r, kind)) = placed.iter().find(|b| b.0 == pane && b.2 != Kind::Chip) else { continue };
            let (flare, corner) = (k.flare(Way::Open, pane, r, now), stage.shape(kind, pen.scale).corner);
            let card = kind == Kind::Card;
            self.lits.push(Lit { pane, flare, image: None, card, corner, under: None, layer: Layer::Tiles });
        }
    }

    /// Follows each opening pane to the rect `drawn` gives for it this frame, ends the
    /// flares of panes no longer shown and those done by `now`, and places each at its
    /// pane's height. Returns whether any still runs.
    pub fn track(&mut self, scene: &Scene, drawn: impl Fn(PaneId, Rect) -> Rect, now: Instant) -> bool {
        self.lits.retain_mut(|l| {
            if l.flare.done(now) {
                return false;
            }
            if l.flare.way == Way::Close {
                // Under every card once the card it was over is gone too.
                let under = l.under.filter(|u| scene.cards.iter().any(|f| f.pane == *u));
                l.layer = under.map_or(Layer::Tiles, Layer::Over);
                return true;
            }
            let tile = scene.tiles.iter().find(|f| f.pane == l.pane).map(|f| (f.outer, Layer::Tiles));
            let card = scene.cards.iter().find(|f| f.pane == l.pane).map(|f| (f.outer, Layer::Over(l.pane)));
            let Some((outer, layer)) = tile.or(card) else { return false };
            l.flare.rect = drawn(l.pane, outer);
            l.layer = layer;
            true
        });
        !self.lits.is_empty()
    }

    /// Shows `pane` as a `kind` at `target` (see `Stage::show`), faded and scaled while it
    /// opens.
    pub fn show(
        &self,
        stage: &mut Stage,
        pen: &mut Pen,
        ctx: &mut DirectContext,
        (pane, kind, target): (PaneId, Kind, Rect),
        morph: Option<(Rect, f32, Option<&Origin>)>,
    ) {
        let opening = self.lits.iter().find(|l| l.pane == pane && l.flare.way == Way::Open);
        let body = opening.map(|l| (l.flare.rect, l.flare.body(stage.now), l.flare.px));
        let Some((r, b, px)) = body.filter(|(_, b, _)| *b != Body { alpha: 1.0, scale: 1.0 }) else {
            return stage.show(pen, ctx, pane, kind, target, morph);
        };
        let (n, cursor) = (enter_body(pen.canvas, r, b, px), stage.cursor);
        stage.show(pen, ctx, pane, kind, target, morph);
        pen.canvas.restore_to_count(n);
        if stage.cursor != cursor {
            // The IME follows the cursor where it shows.
            stage.cursor = stage.cursor.map(|c| about_center(r, b.scale).map_rect(c).0);
        }
    }

    /// Paints the flares at `layer`: closing panes' images, then the light.
    pub fn paint(&mut self, pen: &Pen, layer: Layer, now: Instant) {
        for l in self.lits.iter().filter(|l| l.layer == layer) {
            if let Some(image) = &l.image {
                image_body(pen, image, l.flare.rect, l.flare.body(now), (l.card, l.corner));
            }
            let c = pen.chrome;
            self.painter.light(pen.canvas, &l.flare, now, l.corner, c.accent, c.light);
        }
    }
}

/// The light's sprite and buffers, kept across frames.
struct Painter {
    sprite: Image,
    motes: Vec<Mote>,
    xf: Vec<RSXform>,
    tex: Vec<SkRect>,
    colors: Vec<Color>,
}

/// Light adds onto a dark theme (`BlendMode::Plus`) and is laid over a light one.
fn blend(light_theme: bool) -> BlendMode {
    if light_theme { BlendMode::SrcOver } else { BlendMode::Plus }
}

impl Painter {
    fn new() -> Painter {
        Painter { sprite: sprite(), motes: Vec::new(), xf: Vec::new(), tex: Vec::new(), colors: Vec::new() }
    }

    /// Paints `flare`'s glow and motes at `now` around a border with corners of radius
    /// `corner`.
    fn light(&mut self, canvas: &Canvas, flare: &Flare, now: Instant, corner: f32, accent: Rgb, light_theme: bool) {
        let a = flare.glow(now);
        let c = Color4f::new(accent.r as f32 / 255.0, accent.g as f32 / 255.0, accent.b as f32 / 255.0, 1.0);
        if a > 0.003 {
            glow(canvas, flare, now, corner, c, a, light_theme);
        }
        flare.motes(now, &mut self.motes);
        if self.motes.is_empty() {
            return;
        }
        let size = self.sprite.width() as f32;
        let ink = lit(c);
        self.xf.clear();
        self.tex.clear();
        self.colors.clear();
        for m in &self.motes {
            self.xf.push(RSXform::new(2.0 * m.radius / size, 0.0, (m.center.x - m.radius, m.center.y - m.radius)));
            self.tex.push(SkRect::from_wh(size, size));
            self.colors.push(Color4f { a: m.alpha, ..ink }.to_color());
        }
        let mut paint = Paint::default();
        paint.set_blend_mode(blend(light_theme));
        let sampling = SamplingOptions::new(FilterMode::Linear, MipmapMode::None);
        let colors = &self.colors[..];
        canvas.draw_atlas(&self.sprite, &self.xf, &self.tex, colors, BlendMode::Modulate, sampling, None, &paint);
    }
}

/// A soft halo along the border with a fine bright line in it, at `a` of full strength,
/// following the body's scale.
fn glow(canvas: &Canvas, flare: &Flare, now: Instant, corner: f32, c: Color4f, a: f32, light_theme: bool) {
    let rect = about_center(flare.rect, flare.body(now).scale).map_rect(sk(flare.rect)).0;
    let rrect = RRect::new_rect_xy(rect, corner, corner);
    let px = flare.px;
    let mut p = Paint::default();
    p.set_anti_alias(true).set_style(PaintStyle::Stroke).set_blend_mode(blend(light_theme));
    p.set_stroke_width(3.0 * px);
    p.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, 7.0 * px, false));
    p.set_color4f(Color4f { a: a * 0.9, ..c }, None);
    canvas.draw_rrect(rrect, &p);
    p.set_mask_filter(None);
    p.set_stroke_width(px);
    let w = |v: f32| v * 0.6 + 0.4;
    p.set_color4f(Color4f::new(w(c.r), w(c.g), w(c.b), a * 0.7), None);
    canvas.draw_rrect(rrect, &p);
}

/// `c` made into light: full brightness at its hue, a quarter of the way to white.
fn lit(c: Color4f) -> Color4f {
    let m = c.r.max(c.g).max(c.b).max(0.05);
    let mix = |v: f32| (v / m) * 0.75 + 0.25;
    Color4f::new(mix(c.r), mix(c.g), mix(c.b), 1.0)
}

/// A white mote, 64 px across: a small hot core in a soft halo.
fn sprite() -> Image {
    let n = 64;
    let info = ImageInfo::new((n, n), ColorType::N32, AlphaType::Premul, None);
    let mut surface = surfaces::raster(&info, None, None).expect("a 64 px raster surface");
    let half = n as f32 / 2.0;
    let white = |a: f32| Color4f::new(1.0, 1.0, 1.0, a);
    let stops = [white(1.0), white(0.9), white(0.47), white(0.18), white(0.05), white(0.0)];
    let pos = [0.0, 0.08, 0.2, 0.4, 0.66, 1.0];
    let colors = Colors::new(&stops, Some(&pos), TileMode::Clamp, None);
    let gradient = Gradient::new(colors, Interpolation::default());
    let shader = shaders::radial_gradient(((half, half), half), &gradient, None).expect("radial gradient");
    let mut p = Paint::default();
    p.set_shader(shader);
    surface.canvas().draw_paint(&p);
    surface.image_snapshot()
}
