//! Painting the window chrome of a `Scene`: top bar, tile headers and divider lines, floating
//! cards, dock bar, dock guides with the drop preview, empty state, and IME preedit text.
//!
//! Answers: what does each piece of chrome look like (ADR 6)?
//! Must not know: terminals, layout rules, or window events. Terminal grids are drawn by
//! `grid`; the app decides the order and passes what each piece shows.
//!
//! Tiles look like TUI splits: a narrow header, 1 px divider lines, no gaps or rounding, and
//! header buttons only while the pane is focused or hovered. Cards are rounded, bordered and
//! shadowed above the tiles.

use frith_core::geom::{Point, Rect};
use frith_core::layout::{Divider, Drop, Edge, PaneId};
use frith_core::scene::{ChromeButton, Frame, Guides, Hit, PaneButton, Scene};
use skia_safe::{
    BlendMode, Canvas, ClipOp, Color, MaskFilter, Paint, PaintStyle, RRect, Rect as SkRect, TextBlobBuilder,
};

use crate::fonts::{Fonts, Glyph};

pub mod theme {
    use skia_safe::Color;
    pub const WINDOW: Color = Color::from_rgb(15, 17, 24);
    pub const BAR: Color = Color::from_rgb(20, 22, 31);
    pub const HEADER: Color = Color::from_rgb(24, 27, 37);
    pub const HEADER_FOCUSED: Color = Color::from_rgb(31, 35, 50);
    pub const LINE: Color = Color::from_rgb(46, 50, 66);
    pub const TEXT: Color = Color::from_rgb(222, 226, 240);
    pub const TEXT_DIM: Color = Color::from_rgb(132, 138, 160);
    pub const ACCENT: Color = Color::from_rgb(110, 150, 255);
    pub const ATTENTION: Color = Color::from_rgb(255, 184, 77);
    pub const HOVER: Color = Color::from_argb(40, 255, 255, 255);
    pub const CHIP: Color = Color::from_argb(26, 255, 255, 255);
    pub const CARD_BORDER: Color = Color::from_argb(56, 255, 255, 255);
    pub const SHADOW: Color = Color::from_argb(150, 0, 0, 0);
    pub const PREVIEW: Color = Color::from_argb(64, 110, 150, 255);
    pub const LABEL: Color = Color::from_rgb(38, 44, 66);
    pub const PREEDIT: Color = Color::from_rgb(40, 44, 60);
    pub const PREEDIT_INK: Color = Color::from_rgb(255, 214, 102);
}

fn sk(r: Rect) -> SkRect {
    SkRect::from_xywh(r.x, r.y, r.w, r.h)
}

/// What chrome is painted with this frame.
pub struct Pen<'a> {
    pub canvas: &'a Canvas,
    pub fonts: &'a mut Fonts,
    /// Physical pixels per logical pixel.
    pub scale: f32,
}

/// How one pane's frame looks this frame.
pub struct PaneLook<'a> {
    pub title: &'a str,
    pub focused: bool,
    /// The pointer is over this pane.
    pub hovered: bool,
    pub attention: bool,
    pub zoomed: bool,
}

pub fn background(pen: &Pen, scene: &Scene) {
    let canvas = pen.canvas;
    canvas.clear(theme::WINDOW);
    let mut p = Paint::default();
    p.set_color(theme::BAR);
    canvas.draw_rect(sk(scene.top), &p);
    canvas.draw_rect(sk(scene.bar), &p);
    if let Some(side) = scene.sidebar {
        canvas.draw_rect(sk(side), &p);
        p.set_color(theme::LINE);
        canvas.draw_rect(SkRect::from_xywh(side.right() - scene.metrics.gap, side.y, scene.metrics.gap, side.h), &p);
    }
}

/// Top bar: chrome buttons and the focused pane's title, centered.
pub fn top_bar(pen: &mut Pen, scene: &Scene, title: &str, hover: Hit) {
    let (canvas, scale) = (pen.canvas, pen.scale);
    let fonts = &mut *pen.fonts;
    for &(b, r) in &scene.chrome {
        button_back(canvas, r, hover == Hit::Chrome(b));
        let mut p = stroke(theme::TEXT_DIM, scale);
        let (cx, cy, s) = (r.x + r.w / 2.0, r.y + r.h / 2.0, r.w * 0.28);
        match b {
            ChromeButton::Sidebar => {
                let frame = SkRect::from_xywh(cx - s, cy - s * 0.8, s * 2.0, s * 1.6);
                canvas.draw_rect(frame, &p);
                canvas.draw_line((cx - s * 0.3, cy - s * 0.8), (cx - s * 0.3, cy + s * 0.8), &p);
            }
            ChromeButton::Launcher => {
                p.set_stroke_width(1.5 * scale);
                canvas.draw_line((cx - s, cy), (cx + s, cy), &p);
                canvas.draw_line((cx, cy - s), (cx, cy + s), &p);
            }
        }
    }
    let w = text_width(fonts, title).min(scene.top.w / 2.0);
    let x = (scene.top.x + (scene.top.w - w) / 2.0).round();
    label(canvas, fonts, title, x, scene.top, w, theme::TEXT_DIM);
}

/// A tile's header strip (the grid is drawn separately), with buttons only while the pane
/// is focused or hovered.
pub fn tile_header(pen: &mut Pen, f: &Frame, look: &PaneLook, hover: Hit) {
    let (canvas, scale) = (pen.canvas, pen.scale);
    let mut p = Paint::default();
    p.set_color(if look.focused { theme::HEADER_FOCUSED } else { theme::HEADER });
    canvas.draw_rect(sk(f.header), &p);
    if look.focused {
        p.set_color(theme::ACCENT);
        canvas.draw_rect(SkRect::from_xywh(f.header.x, f.header.bottom() - scale, f.header.w, scale), &p);
    }
    header_content(pen, f, look, hover, look.focused || look.hovered);
}

/// Everything below the tiles' grids: the 1 px lines between tiles, `lit` in the accent.
pub fn dividers(pen: &Pen, dividers: &[Divider], lit: Option<usize>) {
    let canvas = pen.canvas;
    let mut p = Paint::default();
    for d in dividers {
        p.set_color(if Some(d.index) == lit { theme::ACCENT } else { theme::LINE });
        canvas.draw_rect(sk(d.rect), &p);
    }
}

/// Shown when every pane floats or is minimized (ADR 6).
pub fn empty_state(pen: &mut Pen, area: Rect) {
    let canvas = pen.canvas;
    let fonts = &mut *pen.fonts;
    let text = "No tiled panes. Drag a card to an edge, or press ⌘D.";
    let w = text_width(fonts, text).min(area.w);
    let line = Rect::new(area.x, area.y + area.h / 2.0 - fonts.cell_h, area.w, fonts.cell_h * 2.0);
    label(canvas, fonts, text, (area.x + (area.w - w) / 2.0).round(), line, w, theme::TEXT_DIM);
}

/// Card radius in physical pixels.
fn radius(scale: f32) -> f32 {
    10.0 * scale
}

/// The card's shadow, then a clip to its rounded frame for the grid and header; pair with
/// `card_over`.
pub fn card_under(pen: &Pen, f: &Frame) {
    let (canvas, scale) = (pen.canvas, pen.scale);
    let rr = RRect::new_rect_xy(sk(f.outer), radius(scale), radius(scale));
    let mut p = Paint::default();
    p.set_anti_alias(true).set_color(theme::SHADOW);
    p.set_mask_filter(MaskFilter::blur(skia_safe::BlurStyle::Normal, 12.0 * scale, false));
    canvas.draw_rrect(rr.with_offset((0.0, 6.0 * scale)), &p);
    canvas.save();
    canvas.clip_rrect(rr, ClipOp::Intersect, true);
}

/// The card's header, the end of its clip, and its border; the focused card's is the accent.
pub fn card_over(pen: &mut Pen, f: &Frame, look: &PaneLook, hover: Hit) {
    let (canvas, scale) = (pen.canvas, pen.scale);
    let mut p = Paint::default();
    p.set_color(if look.focused { theme::HEADER_FOCUSED } else { theme::HEADER });
    canvas.draw_rect(sk(f.header), &p);
    header_content(pen, f, look, hover, true);
    canvas.restore();
    let rr = RRect::new_rect_xy(sk(f.outer), radius(scale), radius(scale));
    let border = if look.focused { theme::ACCENT } else { theme::CARD_BORDER };
    canvas.draw_rrect(rr, &stroke(border, scale));
}

/// Attention dot, title, and (when `buttons`) the header buttons.
fn header_content(pen: &mut Pen, f: &Frame, look: &PaneLook, hover: Hit, buttons: bool) {
    let (canvas, scale) = (pen.canvas, pen.scale);
    let fonts = &mut *pen.fonts;
    let inset = (6.0 * scale).round();
    let mut x = f.header.x + inset;
    if look.attention {
        let r = 3.0 * scale;
        let mut p = Paint::default();
        p.set_anti_alias(true).set_color(theme::ATTENTION);
        canvas.draw_circle((x + r, f.header.y + f.header.h / 2.0), r, &p);
        x += r * 2.0 + inset;
    }
    let right = if buttons { f.buttons.first().map_or(f.header.right(), |(_, r)| r.x) } else { f.header.right() };
    let ink = if look.focused { theme::TEXT } else { theme::TEXT_DIM };
    label(canvas, fonts, look.title, x, f.header, right - inset - x, ink);
    if !buttons {
        return;
    }
    for &(b, r) in &f.buttons {
        button_back(canvas, r, hover == Hit::Button(f.pane, b));
        icon(canvas, b, r, look.zoomed, ink, scale);
    }
}

fn button_back(canvas: &Canvas, r: Rect, hovered: bool) {
    if hovered {
        let mut p = Paint::default();
        p.set_anti_alias(true).set_color(theme::HOVER);
        canvas.draw_round_rect(sk(r), r.w / 4.0, r.w / 4.0, &p);
    }
}

fn stroke(color: Color, scale: f32) -> Paint {
    let mut p = Paint::default();
    p.set_anti_alias(true).set_color(color).set_style(PaintStyle::Stroke).set_stroke_width(scale.max(1.0));
    p
}

fn icon(canvas: &Canvas, b: PaneButton, r: Rect, zoomed: bool, ink: Color, scale: f32) {
    let p = stroke(ink, scale);
    let (cx, cy, s) = (r.x + r.w / 2.0, r.y + r.h / 2.0, r.w * 0.26);
    let square = SkRect::from_xywh(cx - s, cy - s, s * 2.0, s * 2.0);
    match b {
        PaneButton::SplitRight => {
            canvas.draw_rect(square, &p);
            canvas.draw_line((cx, cy - s), (cx, cy + s), &p);
        }
        PaneButton::SplitDown => {
            canvas.draw_rect(square, &p);
            canvas.draw_line((cx - s, cy), (cx + s, cy), &p);
        }
        PaneButton::Float => {
            canvas.draw_rect(SkRect::from_xywh(cx - s, cy - s * 0.4, s * 1.4, s * 1.4), &p);
            canvas.draw_rect(SkRect::from_xywh(cx - s * 0.4, cy - s, s * 1.4, s * 1.4), &p);
        }
        PaneButton::Zoom if zoomed => {
            for (dx, dy) in [(-1.0, -1.0), (1.0, 1.0)] {
                let (x, y) = (cx + dx * s * 0.25, cy + dy * s * 0.25);
                canvas.draw_line((x, y), (x + dx * s * 0.75, y + dy * s * 0.75), &p);
            }
            canvas.draw_line((cx - s * 0.25, cy - s * 0.25), (cx - s * 0.25, cy - s), &p);
            canvas.draw_line((cx + s * 0.25, cy + s * 0.25), (cx + s * 0.25, cy + s), &p);
        }
        PaneButton::Zoom => {
            canvas.draw_line((cx + s * 0.2, cy - s), (cx + s, cy - s), &p);
            canvas.draw_line((cx + s, cy - s), (cx + s, cy - s * 0.2), &p);
            canvas.draw_line((cx - s * 0.2, cy + s), (cx - s, cy + s), &p);
            canvas.draw_line((cx - s, cy + s), (cx - s, cy + s * 0.2), &p);
        }
        PaneButton::Close => {
            canvas.draw_line((cx - s, cy - s), (cx + s, cy + s), &p);
            canvas.draw_line((cx - s, cy + s), (cx + s, cy - s), &p);
        }
        PaneButton::Minimize => {
            canvas.draw_line((cx - s, cy + s * 0.6), (cx + s, cy + s * 0.6), &p);
        }
        PaneButton::Embed => {
            canvas.draw_rect(square, &p);
            let mut fill = Paint::default();
            fill.set_anti_alias(true).set_color(ink);
            canvas.draw_rect(SkRect::from_xywh(cx - s, cy - s, s, s * 2.0), &fill);
        }
    }
}

/// The dock bar's chips: one per minimized pane, with its title and attention dot.
pub fn dock(pen: &mut Pen, scene: &Scene, look: impl Fn(PaneId) -> (String, bool), hover: Hit) {
    let (canvas, scale) = (pen.canvas, pen.scale);
    let fonts = &mut *pen.fonts;
    let mut p = Paint::default();
    p.set_anti_alias(true);
    for &(pane, r) in &scene.chips {
        let (title, attention) = look(pane);
        let rad = r.h / 2.0;
        p.set_color(if hover == Hit::Chip(pane) { theme::HOVER } else { theme::CHIP });
        canvas.draw_round_rect(sk(r), rad, rad, &p);
        let mut x = r.x + rad;
        if attention {
            let d = 3.0 * scale;
            p.set_color(theme::ATTENTION);
            canvas.draw_circle((x + d, r.y + r.h / 2.0), d, &p);
            x += d * 3.0;
        }
        label(canvas, fonts, &title, x, r, r.right() - rad - x, theme::TEXT);
    }
}

/// Dock guides during a pane drag (ADR 6): the target tile's outline, where the pane will
/// land if the guide under the pointer is active (`preview`: the zone and a label saying what
/// the drop does), and the guides themselves above everything else. The label sits beside
/// the pointer and flips to its other side where it would leave the window.
pub fn dock_guides(pen: &mut Pen, scene: &Scene, guides: &Guides, preview: Option<(Rect, &str)>, at: Point) {
    let (canvas, scale) = (pen.canvas, pen.scale);
    let fonts = &mut *pen.fonts;
    if let Some(f) = guides.target.and_then(|t| scene.tiles.iter().find(|f| f.pane == t)) {
        let o = f.outer;
        let inset = scale;
        canvas.draw_rect(
            SkRect::from_xywh(o.x + inset, o.y + inset, o.w - 2.0 * inset, o.h - 2.0 * inset),
            &stroke(theme::ACCENT, 2.0 * scale),
        );
    }
    if let Some((zone, _)) = preview {
        let mut p = Paint::default();
        p.set_color(theme::PREVIEW).set_blend_mode(BlendMode::SrcOver);
        canvas.draw_rect(sk(zone), &p);
        canvas.draw_rect(sk(zone), &stroke(theme::ACCENT, 2.0 * scale));
    }
    for &(drop, r) in &guides.buttons {
        guide(canvas, drop, r, guides.active == Some(drop), scale);
    }
    let Some((_, text)) = preview else { return };
    let bounds = Rect::new(0.0, 0.0, scene.top.w, scene.bar.bottom());
    let pad = 8.0 * scale;
    let h = fonts.cell_h + pad;
    let (w, off) = (text_width(fonts, text) + 2.0 * pad, 14.0 * scale);
    let x = if at.x + off + w <= bounds.right() { at.x + off } else { at.x - off - w };
    let y = if at.y + off + h <= bounds.bottom() { at.y + off } else { at.y - off - h };
    let r = Rect::new(x, y, w, h);
    let mut back = Paint::default();
    back.set_anti_alias(true).set_color(theme::LABEL);
    canvas.draw_round_rect(sk(r), h / 2.0, h / 2.0, &back);
    canvas.draw_round_rect(sk(r), h / 2.0, h / 2.0, &stroke(theme::ACCENT, scale));
    label(canvas, fonts, text, r.x + pad, r, r.w - pad, theme::TEXT);
}

/// One guide: a rounded square holding a small pane outline with the part the dragged pane
/// takes filled in: half of it for a split, a narrow strip along the edge for the whole
/// window, all of it for a swap.
fn guide(canvas: &Canvas, drop: Drop, r: Rect, active: bool, scale: f32) {
    let rad = 6.0 * scale;
    let mut back = Paint::default();
    back.set_anti_alias(true).set_color(if active { theme::ACCENT } else { theme::LABEL });
    canvas.draw_round_rect(sk(r), rad, rad, &back);
    canvas.draw_round_rect(sk(r), rad, rad, &stroke(theme::CARD_BORDER, scale));
    let ink = if active { theme::WINDOW } else { theme::ACCENT };
    let i = (r.w * 0.24).round();
    let b = Rect::new(r.x + i, r.y + i, r.w - 2.0 * i, r.h - 2.0 * i);
    let (edge, part) = match drop {
        Drop::Split(_, e) => (Some(e), 0.5),
        Drop::Window(e) => (Some(e), 0.25),
        Drop::Swap(_) => (None, 1.0),
    };
    let fill = match edge {
        Some(Edge::Left) => Rect::new(b.x, b.y, b.w * part, b.h),
        Some(Edge::Right) => Rect::new(b.right() - b.w * part, b.y, b.w * part, b.h),
        Some(Edge::Top) => Rect::new(b.x, b.y, b.w, b.h * part),
        Some(Edge::Bottom) => Rect::new(b.x, b.bottom() - b.h * part, b.w, b.h * part),
        None => b,
    };
    let mut p = Paint::default();
    p.set_anti_alias(true).set_color(ink);
    canvas.draw_rect(sk(fill), &p);
    canvas.draw_rect(sk(b), &stroke(ink, scale));
}

/// IME composition text over the cursor cell: a dark box, the text, and an underline.
pub fn preedit(pen: &mut Pen, at: SkRect, text: &str) {
    let canvas = pen.canvas;
    let fonts = &mut *pen.fonts;
    if text.is_empty() {
        return;
    }
    let r = Rect::new(at.left, at.top, text_width(fonts, text), at.height());
    let mut p = Paint::default();
    p.set_color(theme::PREEDIT);
    canvas.draw_rect(sk(r), &p);
    label(canvas, fonts, text, r.x, r, f32::INFINITY, theme::PREEDIT_INK);
    p.set_color(theme::PREEDIT_INK);
    canvas.draw_rect(SkRect::from_xywh(r.x, r.bottom() - 2.0, r.w, 2.0), &p);
}

/// Cells a character takes: two for wide (CJK) characters, else one.
fn cells(c: char) -> f32 {
    libghostty_vt::unicode::codepoint_width(c).max(1) as f32
}

fn text_width(fonts: &Fonts, text: &str) -> f32 {
    text.chars().filter(|c| !c.is_control()).map(cells).sum::<f32>() * fonts.cell_w
}

/// Single-line text vertically centered in `line`, starting at `x`, cut with "…" to fit
/// `max_w`. Monospace advances with per-character font fallback, like the grid.
fn label(canvas: &Canvas, fonts: &mut Fonts, text: &str, x: f32, line: Rect, max_w: f32, ink: Color) {
    let cw = fonts.cell_w;
    let mut chars: Vec<char> = Vec::new();
    let mut used = 0.0;
    for c in text.chars().filter(|c| !c.is_control()) {
        if used + cells(c) * cw > max_w {
            while used + cw > max_w {
                match chars.pop() {
                    Some(last) => used -= cells(last) * cw,
                    None => return,
                }
            }
            chars.push('…');
            break;
        }
        used += cells(c) * cw;
        chars.push(c);
    }
    let baseline = line.y + ((line.h - fonts.cell_h) / 2.0).round() + fonts.ascent;
    // Runs of glyphs from one font, each keyed by its first glyph.
    let mut runs: Vec<(Glyph, Vec<skia_safe::GlyphId>, Vec<f32>)> = Vec::new();
    let mut pen = x;
    for c in chars {
        if let Some(g) = fonts.lookup(c, false) {
            match runs.last_mut() {
                Some((first, gs, xs)) if first.font == g.font => {
                    gs.push(g.id);
                    xs.push(pen);
                }
                _ => runs.push((g, vec![g.id], vec![pen])),
            }
        }
        pen += cells(c) * cw;
    }
    let mut b = TextBlobBuilder::new();
    for (first, gs, xs) in &runs {
        let (g2, x2) = b.alloc_run_pos_h(fonts.font(*first), gs.len(), baseline, None);
        g2.copy_from_slice(gs);
        x2.copy_from_slice(xs);
    }
    if let Some(blob) = b.make() {
        let mut p = Paint::default();
        p.set_anti_alias(true).set_color(ink);
        canvas.draw_text_blob(&blob, (0.0, 0.0), &p);
    }
}
