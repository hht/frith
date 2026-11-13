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

use frith_core::colors::{Chrome, Tint};
use frith_core::config::Rgb;
use frith_core::geom::{Point, Rect};
use frith_core::layout::{Divider, Drop, Edge};
use frith_core::scene::{ChromeButton, Frame, Guides, Hit, PaneButton, Scene};
use skia_safe::{
    BlendMode, Canvas, ClipOp, Color, Font, GlyphId, MaskFilter, Paint, PaintStyle, RRect, Rect as SkRect,
    TextBlobBuilder,
};

use crate::fonts::{Fonts, Glyph};

/// A chrome color for Skia.
fn col(c: Rgb) -> Color {
    Color::from_rgb(c.r, c.g, c.b)
}

fn tint(t: Tint) -> Color {
    Color::from_argb(t.alpha, t.rgb.r, t.rgb.g, t.rgb.b)
}

fn sk(r: Rect) -> SkRect {
    SkRect::from_xywh(r.x, r.y, r.w, r.h)
}

/// What chrome is painted with this frame.
pub struct Pen<'a> {
    pub canvas: &'a Canvas,
    pub chrome: &'a Chrome,
    /// The system UI font, for chrome text (ADR 14).
    pub ui: &'a Font,
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
    let c = pen.chrome;
    let canvas = pen.canvas;
    canvas.clear(col(c.bar));
    let mut p = Paint::default();
    p.set_color(col(c.bar));
    canvas.draw_rect(sk(scene.top), &p);
    canvas.draw_rect(sk(scene.bar), &p);
    if let Some(side) = scene.sidebar {
        canvas.draw_rect(sk(side), &p);
        p.set_color(col(c.line));
        canvas.draw_rect(SkRect::from_xywh(side.right() - scene.metrics.gap, side.y, scene.metrics.gap, side.h), &p);
    }
}

/// Top bar: the chrome buttons. Pane titles show only in pane headers (ADR 14).
pub fn top_bar(pen: &Pen, scene: &Scene, hover: Hit) {
    let c = pen.chrome;
    let (canvas, scale) = (pen.canvas, pen.scale);
    for &(b, hit) in &scene.chrome {
        let r = visible(hit, scale);
        button_back(canvas, c, r, hover == Hit::Chrome(b));
        let mut p = stroke(col(c.text_dim), scale);
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
}

/// A tile's header strip (the grid is drawn separately), with buttons only while the pane
/// is focused or hovered.
pub fn tile_header(pen: &mut Pen, f: &Frame, look: &PaneLook, hover: Hit) {
    let c = pen.chrome;
    let (canvas, scale) = (pen.canvas, pen.scale);
    let mut p = Paint::default();
    p.set_color(if look.focused { col(c.header_focused) } else { col(c.header) });
    canvas.draw_rect(sk(f.header), &p);
    if look.focused {
        p.set_color(col(c.accent));
        canvas.draw_rect(SkRect::from_xywh(f.header.x, f.header.bottom() - scale, f.header.w, scale), &p);
    }
    header_content(pen, f, look, hover, look.focused || look.hovered);
}

/// Fades an unfocused pane's content, as Ghostty does, so the focused one stands out.
/// @see Ghostty `macos/Sources/Ghostty/Surface View/SurfaceView.swift` (the unfocused overlay)
pub fn fade(pen: &Pen, content: Rect, fade: Tint) {
    let mut p = Paint::default();
    p.set_color(tint(fade));
    pen.canvas.draw_rect(sk(content), &p);
}

/// Everything below the tiles' grids: the 1 px lines between tiles, `lit` in the accent.
pub fn dividers(pen: &Pen, dividers: &[Divider], lit: Option<usize>) {
    let c = pen.chrome;
    let canvas = pen.canvas;
    let mut p = Paint::default();
    for d in dividers {
        p.set_color(if Some(d.index) == lit { col(c.accent) } else { col(c.line) });
        canvas.draw_rect(sk(d.rect), &p);
    }
}

/// Shown when every pane floats or is minimized (ADR 6).
pub fn empty_state(pen: &mut Pen, area: Rect) {
    let c = pen.chrome;
    let text = "No tiled panes. Drag a card to an edge, or press ⌘D.";
    let w = ui_width(pen, text).min(area.w);
    let h = ui_line(pen.ui);
    let line = Rect::new(area.x, area.y + area.h / 2.0 - h, area.w, h * 2.0);
    ui_text(pen, text, (area.x + (area.w - w) / 2.0).round(), line, w, col(c.text_dim));
}

/// Card radius in physical pixels.
pub fn radius(scale: f32) -> f32 {
    10.0 * scale
}

/// The shadow under a card whose whole rect is `outer` with corners of radius `corner`, at
/// `alpha` (0 to 1) of its strength.
pub fn card_shadow(pen: &Pen, outer: Rect, corner: f32, alpha: f32) {
    let (c, scale) = (pen.chrome, pen.scale);
    let rr = RRect::new_rect_xy(sk(outer), corner, corner);
    let mut p = Paint::default();
    p.set_anti_alias(true).set_color(tint(Tint { alpha: (c.shadow.alpha as f32 * alpha).round() as u8, ..c.shadow }));
    p.set_mask_filter(MaskFilter::blur(skia_safe::BlurStyle::Normal, 12.0 * scale, false));
    pen.canvas.draw_rrect(rr.with_offset((0.0, 6.0 * scale)), &p);
}

/// A clip to the card's frame, with corners of radius `corner`, for its grid and header; pair
/// with `card_over`.
pub fn card_clip(pen: &Pen, f: &Frame, corner: f32) {
    let rr = RRect::new_rect_xy(sk(f.outer), corner, corner);
    pen.canvas.save();
    pen.canvas.clip_rrect(rr, ClipOp::Intersect, true);
}

/// The card's header, the end of its clip, and its border, with corners of radius `corner`, at
/// `border` (0 to 1) of its strength; the focused card's is the accent.
pub fn card_over(pen: &mut Pen, f: &Frame, look: &PaneLook, hover: Hit, corner: f32, border: f32) {
    let c = pen.chrome;
    let (canvas, scale) = (pen.canvas, pen.scale);
    let mut p = Paint::default();
    p.set_color(if look.focused { col(c.header_focused) } else { col(c.header) });
    canvas.draw_rect(sk(f.header), &p);
    header_content(pen, f, look, hover, true);
    canvas.restore();
    let rr = RRect::new_rect_xy(sk(f.outer), corner, corner);
    let color = if look.focused { col(c.accent) } else { tint(c.card_border) };
    let alpha = (color.a() as f32 * border).round() as u8;
    canvas.draw_rrect(rr, &stroke(color.with_a(alpha), scale));
}

/// Attention dot, title, and (when `buttons`) the header buttons.
fn header_content(pen: &mut Pen, f: &Frame, look: &PaneLook, hover: Hit, buttons: bool) {
    let c = pen.chrome;
    let (canvas, scale) = (pen.canvas, pen.scale);
    let inset = (6.0 * scale).round();
    let mut x = f.header.x + inset;
    if look.attention {
        let r = 3.0 * scale;
        let mut p = Paint::default();
        p.set_anti_alias(true).set_color(col(c.attention));
        canvas.draw_circle((x + r, f.header.y + f.header.h / 2.0), r, &p);
        x += r * 2.0 + inset;
    }
    let right = if buttons { f.buttons.first().map_or(f.header.right(), |(_, r)| r.x) } else { f.header.right() };
    let ink = if look.focused { col(c.text) } else { col(c.text_dim) };
    ui_text(pen, look.title, x, f.header, right - inset - x, ink);
    if !buttons {
        return;
    }
    for &(b, r) in &f.buttons {
        button_back(canvas, c, r, hover == Hit::Button(f.pane, b));
        icon(canvas, b, r, look.zoomed, ink, scale);
    }
}

/// The drawn part of a top bar button's 24 pt hit target: 18 pt, centered, the size of a
/// header button.
fn visible(hit: Rect, scale: f32) -> Rect {
    let side = (18.0 * scale).round().min(hit.w).min(hit.h);
    let (x, y) = ((hit.x + (hit.w - side) / 2.0).round(), (hit.y + (hit.h - side) / 2.0).round());
    Rect::new(x, y, side, side)
}

fn button_back(canvas: &Canvas, c: &Chrome, r: Rect, hovered: bool) {
    if hovered {
        let mut p = Paint::default();
        p.set_anti_alias(true).set_color(tint(c.hover));
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

/// A minimized pane's chip in the dock bar, with its title and attention dot.
pub fn chip(pen: &mut Pen, r: Rect, title: &str, attention: bool, hovered: bool) {
    let c = pen.chrome;
    let (canvas, scale) = (pen.canvas, pen.scale);
    let mut p = Paint::default();
    p.set_anti_alias(true);
    let rad = r.h / 2.0;
    p.set_color(if hovered { tint(c.hover) } else { tint(c.chip) });
    canvas.draw_round_rect(sk(r), rad, rad, &p);
    let mut x = r.x + rad;
    if attention {
        let d = 3.0 * scale;
        p.set_color(col(c.attention));
        canvas.draw_circle((x + d, r.y + r.h / 2.0), d, &p);
        x += d * 3.0;
    }
    ui_text(pen, title, x, r, r.right() - rad - x, col(c.text));
}

/// Dock guides during a pane drag (ADR 6): the target tile's outline, where the pane will
/// land if the guide under the pointer is active (`preview`: the zone and a label saying what
/// the drop does), and the guides themselves above everything else. The label sits beside
/// the pointer and flips to its other side where it would leave the window.
pub fn dock_guides(pen: &mut Pen, scene: &Scene, guides: &Guides, preview: Option<(Rect, &str)>, at: Point) {
    let c = pen.chrome;
    let (canvas, scale) = (pen.canvas, pen.scale);
    if let Some(f) = guides.target.and_then(|t| scene.tiles.iter().find(|f| f.pane == t)) {
        let o = f.outer;
        let inset = scale;
        canvas.draw_rect(
            SkRect::from_xywh(o.x + inset, o.y + inset, o.w - 2.0 * inset, o.h - 2.0 * inset),
            &stroke(col(c.accent), 2.0 * scale),
        );
    }
    if let Some((zone, _)) = preview {
        let mut p = Paint::default();
        p.set_color(tint(c.preview)).set_blend_mode(BlendMode::SrcOver);
        canvas.draw_rect(sk(zone), &p);
        canvas.draw_rect(sk(zone), &stroke(col(c.accent), 2.0 * scale));
    }
    for &(drop, r) in &guides.buttons {
        guide(canvas, c, drop, r, guides.active == Some(drop), scale);
    }
    let Some((_, text)) = preview else { return };
    let bounds = Rect::new(0.0, 0.0, scene.top.w, scene.bar.bottom());
    let pad = 8.0 * scale;
    let h = ui_line(pen.ui) + pad;
    let (w, off) = (ui_width(pen, text) + 2.0 * pad, 14.0 * scale);
    let x = if at.x + off + w <= bounds.right() { at.x + off } else { at.x - off - w };
    let y = if at.y + off + h <= bounds.bottom() { at.y + off } else { at.y - off - h };
    let r = Rect::new(x, y, w, h);
    let mut back = Paint::default();
    back.set_anti_alias(true).set_color(col(c.raised));
    canvas.draw_round_rect(sk(r), h / 2.0, h / 2.0, &back);
    canvas.draw_round_rect(sk(r), h / 2.0, h / 2.0, &stroke(col(c.accent), scale));
    ui_text(pen, text, r.x + pad, r, r.w - pad, col(c.text));
}

/// One guide: a rounded square holding a small pane outline with the part the dragged pane
/// takes filled in: half of it for a split, a narrow strip along the edge for the whole
/// window, all of it for a swap.
fn guide(canvas: &Canvas, c: &Chrome, drop: Drop, r: Rect, active: bool, scale: f32) {
    let rad = 6.0 * scale;
    let mut back = Paint::default();
    back.set_anti_alias(true).set_color(if active { col(c.accent) } else { col(c.raised) });
    canvas.draw_round_rect(sk(r), rad, rad, &back);
    canvas.draw_round_rect(sk(r), rad, rad, &stroke(tint(c.card_border), scale));
    let ink = if active { col(c.on_accent) } else { col(c.accent) };
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

/// IME composition text over the cursor cell: a box in the focused header's color, the text,
/// and an accent underline.
pub fn preedit(pen: &mut Pen, at: SkRect, text: &str) {
    let c = pen.chrome;
    let canvas = pen.canvas;
    let fonts = &mut *pen.fonts;
    if text.is_empty() {
        return;
    }
    let r = Rect::new(at.left, at.top, text_width(fonts, text), at.height());
    let mut p = Paint::default();
    p.set_color(col(c.header_focused));
    canvas.draw_rect(sk(r), &p);
    cell_text(canvas, fonts, text, r.x, r, col(c.text));
    p.set_color(col(c.accent));
    canvas.draw_rect(SkRect::from_xywh(r.x, r.bottom() - 2.0, r.w, 2.0), &p);
}

/// Cells a character takes: two for wide (CJK) characters, else one.
fn cells(c: char) -> f32 {
    libghostty_vt::unicode::codepoint_width(c).max(1) as f32
}

fn text_width(fonts: &Fonts, text: &str) -> f32 {
    text.chars().filter(|c| !c.is_control()).map(cells).sum::<f32>() * fonts.cell_w
}

/// Text in the terminal's cells, as the grid draws it, with its baseline centered in `line`.
fn cell_text(canvas: &Canvas, fonts: &mut Fonts, text: &str, x: f32, line: Rect, ink: Color) {
    let baseline = line.y + ((line.h - fonts.cell_h) / 2.0).round() + fonts.ascent;
    // Runs of glyphs from one font, each keyed by its first glyph.
    let mut runs: Vec<(Glyph, Vec<GlyphId>, Vec<f32>)> = Vec::new();
    let mut pen = x;
    for c in text.chars().filter(|c| !c.is_control()) {
        if let Some(g) = fonts.lookup(c, false) {
            match runs.last_mut() {
                Some((first, gs, xs)) if first.font == g.font => {
                    gs.push(g.id);
                    xs.push(pen);
                }
                _ => runs.push((g, vec![g.id], vec![pen])),
            }
        }
        pen += cells(c) * fonts.cell_w;
    }
    let mut b = TextBlobBuilder::new();
    for (first, gs, xs) in &runs {
        let (g2, x2) = b.alloc_run_pos_h(fonts.font(*first), gs.len(), baseline, None);
        g2.copy_from_slice(gs);
        x2.copy_from_slice(xs);
    }
    draw_blob(canvas, b, ink);
}

/// One glyph of chrome text: the font that draws it (the UI font, or a terminal font for a
/// character the UI font lacks), its id, and its advance.
type UiGlyph = (Option<Glyph>, GlyphId, f32);

/// The glyphs of `text` in the UI font, characters it lacks taken from the terminal's lookup
/// order at the UI font's size.
fn ui_glyphs(ui: &Font, fonts: &mut Fonts, text: &str) -> Vec<UiGlyph> {
    let mut out = Vec::new();
    for c in text.chars().filter(|c| !c.is_control()) {
        let id = ui.unichar_to_glyph(c as i32);
        if id != 0 {
            out.push((None, id, advance(ui, id)));
        } else if let Some(g) = fonts.lookup(c, false) {
            out.push((Some(g), g.id, advance(&sized(fonts, g, ui), g.id)));
        }
    }
    out
}

fn sized(fonts: &Fonts, g: Glyph, ui: &Font) -> Font {
    let mut f = fonts.font(g).clone();
    f.set_size(ui.size());
    f
}

fn advance(font: &Font, id: GlyphId) -> f32 {
    let mut w = [0.0];
    font.get_widths(&[id], &mut w);
    w[0]
}

/// The width chrome text takes in the UI font.
fn ui_width(pen: &mut Pen, text: &str) -> f32 {
    ui_glyphs(pen.ui, pen.fonts, text).iter().map(|g| g.2).sum()
}

/// The height of a line of chrome text.
fn ui_line(ui: &Font) -> f32 {
    let (_, m) = ui.metrics();
    (m.descent - m.ascent).ceil()
}

/// Chrome text in the UI font from `x`, its baseline centered in `line` on a whole pixel,
/// cut with "…" to fit `max_w`.
fn ui_text(pen: &mut Pen, text: &str, x: f32, line: Rect, max_w: f32, ink: Color) {
    let (canvas, ui) = (pen.canvas, pen.ui);
    let mut glyphs = ui_glyphs(ui, pen.fonts, text);
    let total: f32 = glyphs.iter().map(|g| g.2).sum();
    if total > max_w {
        let dots = ui.unichar_to_glyph('…' as i32);
        let dots = (None, dots, advance(ui, dots));
        let mut used = total;
        while used + dots.2 > max_w {
            match glyphs.pop() {
                Some(g) => used -= g.2,
                None => return,
            }
        }
        glyphs.push(dots);
    }
    let (_, m) = ui.metrics();
    let baseline = (line.y + (line.h - (m.descent - m.ascent)) / 2.0 - m.ascent).round();
    // Runs of glyphs from one font.
    let mut runs: Vec<(Option<Glyph>, Vec<GlyphId>, Vec<f32>)> = Vec::new();
    let mut at = x;
    for (src, id, adv) in glyphs {
        match runs.last_mut() {
            Some((first, ids, xs)) if first.map(|g| g.font) == src.map(|g| g.font) => {
                ids.push(id);
                xs.push(at);
            }
            _ => runs.push((src, vec![id], vec![at])),
        }
        at += adv;
    }
    let mut b = TextBlobBuilder::new();
    for (src, ids, xs) in &runs {
        let font = src.map_or_else(|| ui.clone(), |g| sized(pen.fonts, g, ui));
        let (g2, x2) = b.alloc_run_pos_h(&font, ids.len(), baseline, None);
        g2.copy_from_slice(ids);
        x2.copy_from_slice(xs);
    }
    draw_blob(canvas, b, ink);
}

fn draw_blob(canvas: &Canvas, mut b: TextBlobBuilder, ink: Color) {
    if let Some(blob) = b.make() {
        let mut p = Paint::default();
        p.set_anti_alias(true).set_color(ink);
        canvas.draw_text_blob(&blob, (0.0, 0.0), &p);
    }
}
