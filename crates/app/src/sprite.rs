//! Characters Frith draws itself instead of taking them from a font: box drawing
//! (U+2500–257F), block elements (U+2580–259F) and the geometric powerline symbols
//! (U+E0B0–E0BF, U+E0D2, U+E0D4), so lines, blocks and separators meet across cells in any
//! font (ADR 5, ADR 12). As in Ghostty, these always win over the fonts.
//!
//! Answers: what does each such character look like inside one cell?
//! Must not know: fonts, the atlas, terminals or colors; callers pass the cell size and the
//! line thickness and get white ink on a transparent canvas, (0, 0) at the cell's top left.
//!
//! Follows Ghostty's sprite face, which is MIT licensed; shapes, rounding and thicknesses
//! are the same so the two terminals draw the same pixels.
//! @see Ghostty `src/font/sprite/draw/box.zig`, `block.zig`, `powerline.zig`, `common.zig`
//! @see Ghostty `src/font/CodepointResolver.zig` (the sprite face is always the result)

use skia_safe::{Canvas, ClipOp, Color, IRect, Paint, PaintCap, PaintStyle, Path, PathBuilder};

/// The cell a sprite fills, in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub w: u32,
    pub h: u32,
    /// The light line width: the font's underline thickness rounded up, at least 1 px, as
    /// Ghostty's `box_thickness`.
    pub thickness: u32,
}

/// Whether `c` is drawn here rather than by a font.
pub fn covers(c: char) -> bool {
    matches!(c as u32, 0x2500..=0x259f | 0xe0b0..=0xe0bf | 0xe0d2 | 0xe0d4)
}

/// Draws `c`, which `covers`, in white with its cell's top left at the canvas origin. Some
/// strokes reach up to one pixel past the cell, as in Ghostty, so diagonals join at corners.
pub fn draw(canvas: &Canvas, c: char, cell: Cell) {
    let mut ink = Ink { canvas, cell };
    let cp = c as u32;
    match cp {
        0x2500..=0x257f => ink.box_drawing(cp),
        0x2580..=0x259f => ink.block(cp),
        _ => match mirror_of(cp) {
            Some(right) => {
                canvas.save();
                canvas.translate((cell.w as f32, 0.0));
                canvas.scale((-1.0, 1.0));
                ink.powerline(right);
                canvas.restore();
            }
            None => ink.powerline(cp),
        },
    }
}

/// The right-facing symbol a left-facing powerline symbol mirrors, as Ghostty draws them.
fn mirror_of(cp: u32) -> Option<u32> {
    match cp {
        0xe0b3 => Some(0xe0b1),
        0xe0b6 => Some(0xe0b4),
        0xe0b7 => Some(0xe0b5),
        0xe0d4 => Some(0xe0d2),
        _ => None,
    }
}

/// How much of the full ink a shade leaves, as in Ghostty's `Shade`.
#[derive(Clone, Copy)]
enum Shade {
    Light = 0x40,
    Medium = 0x80,
    Dark = 0xc0,
    On = 0xff,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    None,
    Light,
    Heavy,
    Double,
}

/// The line from each edge of the cell to its center.
#[derive(Clone, Copy)]
struct Lines {
    up: Style,
    right: Style,
    down: Style,
    left: Style,
}

const fn lines(up: Style, right: Style, down: Style, left: Style) -> Lines {
    Lines { up, right, down, left }
}

struct Ink<'a> {
    canvas: &'a Canvas,
    cell: Cell,
}

impl Ink<'_> {
    fn light(&self) -> u32 {
        self.cell.thickness
    }

    fn heavy(&self) -> u32 {
        self.cell.thickness * 2
    }

    /// Fills whole pixels from (x0, y0) up to (x1, y1), without antialiasing.
    fn rect(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, shade: Shade) {
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let mut p = Paint::default();
        p.set_color(Color::from_argb(shade as u8, 255, 255, 255));
        self.canvas.draw_irect(IRect::new(x0 as i32, y0 as i32, x1 as i32, y1 as i32), &p);
    }

    fn fill_path(&mut self, path: &Path) {
        let mut p = Paint::default();
        p.set_anti_alias(true).set_color(Color::WHITE);
        self.canvas.draw_path(path, &p);
    }

    fn stroke_path(&mut self, path: &Path, width: f32) {
        let mut p = Paint::default();
        p.set_anti_alias(true)
            .set_color(Color::WHITE)
            .set_style(PaintStyle::Stroke)
            .set_stroke_width(width)
            .set_stroke_cap(PaintCap::Butt);
        self.canvas.draw_path(path, &p);
    }

    fn polygon(&mut self, points: &[(f32, f32)]) {
        let mut b = PathBuilder::new();
        b.move_to(points[0]);
        for &pt in &points[1..] {
            b.line_to(pt);
        }
        b.close();
        self.fill_path(&b.detach());
    }

    fn box_drawing(&mut self, cp: u32) {
        use Style::{Double as D, Heavy as H, Light as L, None as N};
        let (light, heavy) = (self.light(), self.heavy());
        let gap = light.max(4);
        // Order of `lines`: up, right, down, left.
        let l = match cp {
            0x2500 => lines(N, L, N, L),
            0x2501 => lines(N, H, N, H),
            0x2502 => lines(L, N, L, N),
            0x2503 => lines(H, N, H, N),
            0x2504 => return self.dash_h(3, light, gap),
            0x2505 => return self.dash_h(3, heavy, gap),
            0x2506 => return self.dash_v(3, light, gap),
            0x2507 => return self.dash_v(3, heavy, gap),
            0x2508 => return self.dash_h(4, light, gap),
            0x2509 => return self.dash_h(4, heavy, gap),
            0x250a => return self.dash_v(4, light, gap),
            0x250b => return self.dash_v(4, heavy, gap),
            0x250c => lines(N, L, L, N),
            0x250d => lines(N, H, L, N),
            0x250e => lines(N, L, H, N),
            0x250f => lines(N, H, H, N),
            0x2510 => lines(N, N, L, L),
            0x2511 => lines(N, N, L, H),
            0x2512 => lines(N, N, H, L),
            0x2513 => lines(N, N, H, H),
            0x2514 => lines(L, L, N, N),
            0x2515 => lines(L, H, N, N),
            0x2516 => lines(H, L, N, N),
            0x2517 => lines(H, H, N, N),
            0x2518 => lines(L, N, N, L),
            0x2519 => lines(L, N, N, H),
            0x251a => lines(H, N, N, L),
            0x251b => lines(H, N, N, H),
            0x251c => lines(L, L, L, N),
            0x251d => lines(L, H, L, N),
            0x251e => lines(H, L, L, N),
            0x251f => lines(L, L, H, N),
            0x2520 => lines(H, L, H, N),
            0x2521 => lines(H, H, L, N),
            0x2522 => lines(L, H, H, N),
            0x2523 => lines(H, H, H, N),
            0x2524 => lines(L, N, L, L),
            0x2525 => lines(L, N, L, H),
            0x2526 => lines(H, N, L, L),
            0x2527 => lines(L, N, H, L),
            0x2528 => lines(H, N, H, L),
            0x2529 => lines(H, N, L, H),
            0x252a => lines(L, N, H, H),
            0x252b => lines(H, N, H, H),
            0x252c => lines(N, L, L, L),
            0x252d => lines(N, L, L, H),
            0x252e => lines(N, H, L, L),
            0x252f => lines(N, H, L, H),
            0x2530 => lines(N, L, H, L),
            0x2531 => lines(N, L, H, H),
            0x2532 => lines(N, H, H, L),
            0x2533 => lines(N, H, H, H),
            0x2534 => lines(L, L, N, L),
            0x2535 => lines(L, L, N, H),
            0x2536 => lines(L, H, N, L),
            0x2537 => lines(L, H, N, H),
            0x2538 => lines(H, L, N, L),
            0x2539 => lines(H, L, N, H),
            0x253a => lines(H, H, N, L),
            0x253b => lines(H, H, N, H),
            0x253c => lines(L, L, L, L),
            0x253d => lines(L, L, L, H),
            0x253e => lines(L, H, L, L),
            0x253f => lines(L, H, L, H),
            0x2540 => lines(H, L, L, L),
            0x2541 => lines(L, L, H, L),
            0x2542 => lines(H, L, H, L),
            0x2543 => lines(H, L, L, H),
            0x2544 => lines(H, H, L, L),
            0x2545 => lines(L, L, H, H),
            0x2546 => lines(L, H, H, L),
            0x2547 => lines(H, H, L, H),
            0x2548 => lines(L, H, H, H),
            0x2549 => lines(H, L, H, H),
            0x254a => lines(H, H, H, L),
            0x254b => lines(H, H, H, H),
            0x254c => return self.dash_h(2, light, light),
            0x254d => return self.dash_h(2, heavy, heavy),
            0x254e => return self.dash_v(2, light, heavy),
            0x254f => return self.dash_v(2, heavy, heavy),
            0x2550 => lines(N, D, N, D),
            0x2551 => lines(D, N, D, N),
            0x2552 => lines(N, D, L, N),
            0x2553 => lines(N, L, D, N),
            0x2554 => lines(N, D, D, N),
            0x2555 => lines(N, N, L, D),
            0x2556 => lines(N, N, D, L),
            0x2557 => lines(N, N, D, D),
            0x2558 => lines(L, D, N, N),
            0x2559 => lines(D, L, N, N),
            0x255a => lines(D, D, N, N),
            0x255b => lines(L, N, N, D),
            0x255c => lines(D, N, N, L),
            0x255d => lines(D, N, N, D),
            0x255e => lines(L, D, L, N),
            0x255f => lines(D, L, D, N),
            0x2560 => lines(D, D, D, N),
            0x2561 => lines(L, N, L, D),
            0x2562 => lines(D, N, D, L),
            0x2563 => lines(D, N, D, D),
            0x2564 => lines(N, D, L, D),
            0x2565 => lines(N, L, D, L),
            0x2566 => lines(N, D, D, D),
            0x2567 => lines(L, D, N, D),
            0x2568 => lines(D, L, N, L),
            0x2569 => lines(D, D, N, D),
            0x256a => lines(L, D, L, D),
            0x256b => lines(D, L, D, L),
            0x256c => lines(D, D, D, D),
            0x256d => return self.arc(Corner::BottomRight),
            0x256e => return self.arc(Corner::BottomLeft),
            0x256f => return self.arc(Corner::TopLeft),
            0x2570 => return self.arc(Corner::TopRight),
            0x2571 => return self.diagonal(true),
            0x2572 => return self.diagonal(false),
            0x2573 => {
                self.diagonal(true);
                return self.diagonal(false);
            }
            0x2574 => lines(N, N, N, L),
            0x2575 => lines(L, N, N, N),
            0x2576 => lines(N, L, N, N),
            0x2577 => lines(N, N, L, N),
            0x2578 => lines(N, N, N, H),
            0x2579 => lines(H, N, N, N),
            0x257a => lines(N, H, N, N),
            0x257b => lines(N, N, H, N),
            0x257c => lines(N, H, N, L),
            0x257d => lines(L, N, H, N),
            0x257e => lines(N, L, N, H),
            0x257f => lines(H, N, L, N),
            _ => unreachable!("not box drawing: U+{cp:04X}"),
        };
        self.lines(l);
    }

    /// Lines from the edges to the center, joined where they meet. A port of Ghostty's
    /// `linesChar`, with its saturating arithmetic.
    fn lines(&mut self, l: Lines) {
        use Style::{Double, Heavy, Light, None};
        let Cell { w, h, .. } = self.cell;
        let (light, heavy) = (self.light(), self.heavy());
        let h_light_top = h.saturating_sub(light) / 2;
        let h_light_bottom = h_light_top + light;
        let h_heavy_top = h.saturating_sub(heavy) / 2;
        let h_heavy_bottom = h_heavy_top + heavy;
        let h_double_top = h_light_top.saturating_sub(light);
        let h_double_bottom = h_light_bottom + light;
        let v_light_left = w.saturating_sub(light) / 2;
        let v_light_right = v_light_left + light;
        let v_heavy_left = w.saturating_sub(heavy) / 2;
        let v_heavy_right = v_heavy_left + heavy;
        let v_double_left = v_light_left.saturating_sub(light);
        let v_double_right = v_light_right + light;

        let up_bottom = if l.left == Heavy || l.right == Heavy {
            h_heavy_bottom
        } else if l.left != l.right || l.down == l.up {
            if l.left == Double || l.right == Double { h_double_bottom } else { h_light_bottom }
        } else if l.left == None && l.right == None {
            h_light_bottom
        } else {
            h_light_top
        };
        let down_top = if l.left == Heavy || l.right == Heavy {
            h_heavy_top
        } else if l.left != l.right || l.up == l.down {
            if l.left == Double || l.right == Double { h_double_top } else { h_light_top }
        } else if l.left == None && l.right == None {
            h_light_top
        } else {
            h_light_bottom
        };
        let left_right = if l.up == Heavy || l.down == Heavy {
            v_heavy_right
        } else if l.up != l.down || l.left == l.right {
            if l.up == Double || l.down == Double { v_double_right } else { v_light_right }
        } else if l.up == None && l.down == None {
            v_light_right
        } else {
            v_light_left
        };
        let right_left = if l.up == Heavy || l.down == Heavy {
            v_heavy_left
        } else if l.up != l.down || l.right == l.left {
            if l.up == Double || l.down == Double { v_double_left } else { v_light_left }
        } else if l.up == None && l.down == None {
            v_light_left
        } else {
            v_light_right
        };

        let on = Shade::On;
        match l.up {
            None => {}
            Light => self.rect(v_light_left, 0, v_light_right, up_bottom, on),
            Heavy => self.rect(v_heavy_left, 0, v_heavy_right, up_bottom, on),
            Double => {
                let left_bottom = if l.left == Double { h_light_top } else { up_bottom };
                let right_bottom = if l.right == Double { h_light_top } else { up_bottom };
                self.rect(v_double_left, 0, v_light_left, left_bottom, on);
                self.rect(v_light_right, 0, v_double_right, right_bottom, on);
            }
        }
        match l.right {
            None => {}
            Light => self.rect(right_left, h_light_top, w, h_light_bottom, on),
            Heavy => self.rect(right_left, h_heavy_top, w, h_heavy_bottom, on),
            Double => {
                let top_left = if l.up == Double { v_light_right } else { right_left };
                let bottom_left = if l.down == Double { v_light_right } else { right_left };
                self.rect(top_left, h_double_top, w, h_light_top, on);
                self.rect(bottom_left, h_light_bottom, w, h_double_bottom, on);
            }
        }
        match l.down {
            None => {}
            Light => self.rect(v_light_left, down_top, v_light_right, h, on),
            Heavy => self.rect(v_heavy_left, down_top, v_heavy_right, h, on),
            Double => {
                let left_top = if l.left == Double { h_light_bottom } else { down_top };
                let right_top = if l.right == Double { h_light_bottom } else { down_top };
                self.rect(v_double_left, left_top, v_light_left, h, on);
                self.rect(v_light_right, right_top, v_double_right, h, on);
            }
        }
        match l.left {
            None => {}
            Light => self.rect(0, h_light_top, left_right, h_light_bottom, on),
            Heavy => self.rect(0, h_heavy_top, left_right, h_heavy_bottom, on),
            Double => {
                let top_right = if l.up == Double { v_light_left } else { left_right };
                let bottom_right = if l.down == Double { v_light_left } else { left_right };
                self.rect(0, h_double_top, top_right, h_light_top, on);
                self.rect(0, h_light_bottom, bottom_right, h_double_bottom, on);
            }
        }
    }

    /// A light diagonal from corner to corner, overshooting each corner a little along its
    /// slope so neighbors join.
    fn diagonal(&mut self, rising: bool) {
        let (w, h) = (self.cell.w as f32, self.cell.h as f32);
        let (sx, sy) = ((w / h).min(1.0), (h / w).min(1.0));
        let mut b = PathBuilder::new();
        if rising {
            b.move_to((w + 0.5 * sx, -0.5 * sy));
            b.line_to((-0.5 * sx, h + 0.5 * sy));
        } else {
            b.move_to((-0.5 * sx, -0.5 * sy));
            b.line_to((w + 0.5 * sx, h + 0.5 * sy));
        }
        self.stroke_path(&b.detach(), self.light() as f32);
    }

    /// A rounded corner joining the light lines from the two edges at `corner`.
    fn arc(&mut self, corner: Corner) {
        let (w, h) = (self.cell.w as f32, self.cell.h as f32);
        let thick = self.light();
        let t = thick as f32;
        let cx = (self.cell.w.saturating_sub(thick) / 2) as f32 + t / 2.0;
        let cy = (self.cell.h.saturating_sub(thick) / 2) as f32 + t / 2.0;
        let r = w.min(h) / 2.0;
        // How far from the center the middle control points sit.
        let s = 0.25;
        let (vy, dy) = match corner {
            Corner::TopLeft | Corner::TopRight => (0.0, -1.0),
            Corner::BottomLeft | Corner::BottomRight => (h, 1.0),
        };
        let (hx, dx) = match corner {
            Corner::TopLeft | Corner::BottomLeft => (0.0, -1.0),
            Corner::TopRight | Corner::BottomRight => (w, 1.0),
        };
        let mut b = PathBuilder::new();
        b.move_to((cx, vy));
        b.line_to((cx, cy + dy * r));
        b.cubic_to((cx, cy + dy * s * r), (cx + dx * s * r, cy), (cx + dx * r, cy));
        b.line_to((hx, cy));
        self.stroke_path(&b.detach(), t);
    }

    /// `count` dashes across the cell, with half a gap at each end so they tile evenly.
    fn dash_h(&mut self, count: u32, thick: u32, desired_gap: u32) {
        let Cell { w, h, .. } = self.cell;
        if w < count * 2 {
            let light = self.light();
            let y = h.saturating_sub(light) / 2;
            return self.rect(0, y, w, y + light, Shade::On);
        }
        let gap = desired_gap.min(w / (2 * count));
        let dashes = w - count * gap;
        let (dash, mut extra) = (dashes / count, dashes % count);
        let y = h.saturating_sub(thick) / 2;
        let mut x = gap / 2;
        for _ in 0..count {
            let mut x1 = x + dash;
            if extra > 0 {
                extra -= 1;
                x1 += 1;
            }
            self.rect(x, y, x1, y + thick, Shade::On);
            x = x1 + gap;
        }
    }

    /// `count` dashes down the cell, with one whole gap at the bottom so they tile evenly.
    fn dash_v(&mut self, count: u32, thick: u32, desired_gap: u32) {
        let Cell { w, h, .. } = self.cell;
        if h < count * 2 {
            let light = self.light();
            let x = w.saturating_sub(light) / 2;
            return self.rect(x, 0, x + light, h, Shade::On);
        }
        let gap = desired_gap.min(h / (2 * count));
        let dashes = h - count * gap;
        let (dash, mut extra) = (dashes / count, dashes % count);
        let x = w.saturating_sub(thick) / 2;
        let mut y = 0;
        for _ in 0..count {
            let mut y1 = y + dash;
            if extra > 0 {
                extra -= 1;
                y1 += 1;
            }
            self.rect(x, y, x + thick, y1, Shade::On);
            y = y1 + gap;
        }
    }

    fn block(&mut self, cp: u32) {
        use Align::{Bottom, Left, Right, Top};
        let (e, f) = (0.125, 0.5);
        match cp {
            0x2580 => self.part(Top, 1.0, f),
            0x2581..=0x2587 => self.part(Bottom, 1.0, (cp - 0x2580) as f64 * e),
            0x2588 => self.shade(Shade::On),
            0x2589..=0x258f => self.part(Left, (0x2590 - cp) as f64 * e, 1.0),
            0x2590 => self.part(Right, f, 1.0),
            0x2591 => self.shade(Shade::Light),
            0x2592 => self.shade(Shade::Medium),
            0x2593 => self.shade(Shade::Dark),
            0x2594 => self.part(Top, 1.0, e),
            0x2595 => self.part(Right, e, 1.0),
            _ => {
                // Quadrants, as (top left, top right, bottom left, bottom right).
                let quads = match cp {
                    0x2596 => [false, false, true, false],
                    0x2597 => [false, false, false, true],
                    0x2598 => [true, false, false, false],
                    0x2599 => [true, false, true, true],
                    0x259a => [true, false, false, true],
                    0x259b => [true, true, true, false],
                    0x259c => [true, true, false, true],
                    0x259d => [false, true, false, false],
                    0x259e => [false, true, true, false],
                    0x259f => [false, true, true, true],
                    _ => unreachable!("not a block element: U+{cp:04X}"),
                };
                let Cell { w, h, .. } = self.cell;
                // Ghostty's `Fraction.min` and `max` for one half, so the two halves of an
                // odd size together cover it exactly.
                let (xm0, xm1) = (w - (w as f64 * 0.5).round() as u32, (w as f64 * 0.5).round() as u32);
                let (ym0, ym1) = (h - (h as f64 * 0.5).round() as u32, (h as f64 * 0.5).round() as u32);
                let on = Shade::On;
                if quads[0] {
                    self.rect(0, 0, xm1, ym1, on);
                }
                if quads[1] {
                    self.rect(xm0, 0, w, ym1, on);
                }
                if quads[2] {
                    self.rect(0, ym0, xm1, h, on);
                }
                if quads[3] {
                    self.rect(xm0, ym0, w, h, on);
                }
            }
        }
    }

    /// A block of `fw` by `fh` of the cell against `align`.
    fn part(&mut self, align: Align, fw: f64, fh: f64) {
        let Cell { w, h, .. } = self.cell;
        let (bw, bh) = ((w as f64 * fw).round() as u32, (h as f64 * fh).round() as u32);
        let x = if align == Align::Right { w - bw } else { 0 };
        let y = if align == Align::Bottom { h - bh } else { 0 };
        self.rect(x, y, x + bw, y + bh, Shade::On);
    }

    fn shade(&mut self, shade: Shade) {
        self.rect(0, 0, self.cell.w, self.cell.h, shade);
    }

    /// The powerline symbols facing right; the left-facing ones are these mirrored.
    fn powerline(&mut self, cp: u32) {
        let (w, h) = (self.cell.w as f32, self.cell.h as f32);
        match cp {
            0xe0b0 => self.polygon(&[(0.0, 0.0), (w, h / 2.0), (0.0, h)]),
            0xe0b2 => self.polygon(&[(w, 0.0), (0.0, h / 2.0), (w, h)]),
            0xe0b8 => self.polygon(&[(0.0, 0.0), (w, h), (0.0, h)]),
            0xe0ba => self.polygon(&[(w, 0.0), (w, h), (0.0, h)]),
            0xe0bc => self.polygon(&[(0.0, 0.0), (w, 0.0), (0.0, h)]),
            0xe0be => self.polygon(&[(0.0, 0.0), (w, 0.0), (w, h)]),
            0xe0b9 | 0xe0bf => self.diagonal(false),
            0xe0bb | 0xe0bd => self.diagonal(true),
            0xe0b1 => {
                let mut b = PathBuilder::new();
                b.move_to((0.0, 0.0));
                b.line_to((w, h / 2.0));
                b.line_to((0.0, h));
                self.stroke_path(&b.detach(), self.light() as f32);
            }
            0xe0b4 => self.fill_path(&half_disc(w, h, false)),
            0xe0b5 => {
                // Ghostty strokes this outline inset by half the line width; stroking it at
                // twice the width inside a clip to the filled shape leaves the same band.
                let outline = half_disc(w, h, true);
                self.canvas.save();
                self.canvas.clip_path(&half_disc(w, h, false), ClipOp::Intersect, true);
                self.stroke_path(&outline, 2.0 * self.cell.thickness as f32);
                self.canvas.restore();
            }
            0xe0d2 => {
                let t = self.cell.thickness as f32;
                self.polygon(&[(0.0, 0.0), (w, 0.0), (w / 2.0, h / 2.0 - t / 2.0), (0.0, h / 2.0 - t / 2.0)]);
                self.polygon(&[(0.0, h), (w, h), (w / 2.0, h / 2.0 + t / 2.0), (0.0, h / 2.0 + t / 2.0)]);
            }
            _ => unreachable!("not a drawn powerline symbol: U+{cp:04X}"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Clone, Copy)]
enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// The right-facing half disc of ``: closed for filling, or open along its left edge for
/// the outline of ``.
fn half_disc(w: f32, h: f32, open: bool) -> Path {
    // Cubic coefficient for a quarter circle.
    let c = (std::f32::consts::SQRT_2 - 1.0) * 4.0 / 3.0;
    let r = w.min(h / 2.0);
    let edge = if open { 1.0 } else { 0.0 };
    let mut b = PathBuilder::new();
    b.move_to((0.0, 0.0));
    if open {
        b.line_to((edge, 0.0));
    }
    b.cubic_to((r * c, 0.0), (r, r - r * c), (r, r));
    b.line_to((r, h - r));
    b.cubic_to((r, h - r + r * c), (r * c, h), (edge, h));
    if open {
        b.line_to((0.0, h));
    } else {
        b.close();
    }
    b.detach()
}

#[cfg(test)]
mod tests {
    use super::*;
    use skia_safe::{ImageInfo, surfaces};

    /// Pixels drawn around the cell, for strokes that reach past it.
    const OUT: u32 = 2;

    /// `c`'s ink as alpha over the cell and a margin around it: pixels, cell size, margin.
    struct Ink(Vec<u8>, u32, u32, u32);

    impl Ink {
        fn at(&self, x: i32, y: i32) -> u8 {
            let out = self.3;
            let (cw, ch) = (self.1 + 2 * out, self.2 + 2 * out);
            let (px, py) = (x + out as i32, y + out as i32);
            assert!(px >= 0 && py >= 0 && (px as u32) < cw && (py as u32) < ch, "({x}, {y}) is off the canvas");
            self.0[(py as u32 * cw + px as u32) as usize]
        }
        /// The rows of column `x` at least half covered; a curve's antialiased fringe is not.
        fn rows(&self, x: i32) -> Vec<i32> {
            (0..self.2 as i32).filter(|&y| self.at(x, y) >= 128).collect()
        }
        /// The columns of row `y` at least half covered.
        fn cols(&self, y: i32) -> Vec<i32> {
            (0..self.1 as i32).filter(|&x| self.at(x, y) >= 128).collect()
        }
    }

    fn ink(c: char, w: u32, h: u32, thickness: u32) -> Ink {
        ink_with_margin(c, w, h, thickness, OUT)
    }

    fn ink_with_margin(c: char, w: u32, h: u32, thickness: u32, out: u32) -> Ink {
        assert!(covers(c), "{c:?}");
        let (cw, ch) = (w + 2 * out, h + 2 * out);
        let mut s = surfaces::raster_n32_premul((cw as i32, ch as i32)).unwrap();
        s.canvas().clear(Color::TRANSPARENT);
        s.canvas().translate((out as f32, out as f32));
        draw(s.canvas(), c, Cell { w, h, thickness });
        let mut a = vec![0u8; (cw * ch) as usize];
        assert!(s.read_pixels(&ImageInfo::new_a8((cw as i32, ch as i32)), &mut a, cw as usize, (0, 0)));
        Ink(a, w, h, out)
    }

    /// Cells to check: JetBrains Mono at 24 px (the user's), and an odd-sized small one.
    const CELLS: [(u32, u32, u32); 2] = [(14, 32, 2), (9, 19, 1)];

    #[test]
    fn covers_box_drawing_blocks_and_the_geometric_powerline_symbols_only() {
        for c in ['\u{2500}', '\u{257f}', '\u{2580}', '\u{259f}', '\u{e0b0}', '\u{e0bf}', '\u{e0d2}', '\u{e0d4}'] {
            assert!(covers(c), "U+{:04X}", c as u32);
        }
        for c in ['\u{24ff}', '\u{25a0}', '\u{e0a0}', '\u{e0af}', '\u{e0c0}', '\u{e0d3}', 'a'] {
            assert!(!covers(c), "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn lines_and_rounded_corners_meet_their_neighbors_at_the_cell_edges() {
        for (w, h, t) in CELLS {
            let (hl, vl) = (ink('─', w, h, t), ink('│', w, h, t));
            let rows = hl.rows(0);
            let cols = vl.cols(0);
            assert_eq!(rows.len(), t as usize, "{w}x{h}: ─ is one line thick");
            assert_eq!(cols.len(), t as usize, "{w}x{h}: │ is one line thick");
            for x in 0..w as i32 {
                assert_eq!(hl.rows(x), rows, "{w}x{h}: ─ is straight across the cell");
            }
            for y in 0..h as i32 {
                assert_eq!(vl.cols(y), cols, "{w}x{h}: │ is straight down the cell");
            }
            let (right, bottom) = (w as i32 - 1, h as i32 - 1);
            // The p10k prompt frame: ╭─ over ├─ over ╰─, each reaching the ─ beside it and the
            // vertical line above or below.
            for (c, top, low, left, side) in [
                ('╭', false, true, false, true),
                ('╰', true, false, false, true),
                ('╮', false, true, true, false),
                ('╯', true, false, true, false),
                ('├', true, true, false, true),
                ('┼', true, true, true, true),
            ] {
                let g = ink(c, w, h, t);
                let expect = |on: bool, line: &Vec<i32>| if on { line.clone() } else { vec![] };
                assert_eq!(g.cols(0), expect(top, &cols), "{w}x{h}: {c} at the top edge");
                assert_eq!(g.cols(bottom), expect(low, &cols), "{w}x{h}: {c} at the bottom edge");
                assert_eq!(g.rows(0), expect(left, &rows), "{w}x{h}: {c} at the left edge");
                assert_eq!(g.rows(right), expect(side, &rows), "{w}x{h}: {c} at the right edge");
            }
        }
    }

    #[test]
    fn heavy_and_double_lines_are_two_and_three_light_lines_wide() {
        let (w, h, t) = (14, 32, 2);
        assert_eq!(ink('━', w, h, t).rows(0).len(), 2 * t as usize);
        let double = ink('═', w, h, t).rows(0);
        assert_eq!(double.len(), 2 * t as usize, "two strokes");
        assert_eq!(double[double.len() - 1] - double[0] + 1, 3 * t as i32, "a light line apart");
    }

    #[test]
    fn full_and_shaded_blocks_fill_the_whole_cell_evenly() {
        for (w, h, t) in CELLS {
            for (c, alpha) in [('█', 255), ('▓', 0xc0), ('▒', 0x80), ('░', 0x40)] {
                let g = ink(c, w, h, t);
                for y in -1..=h as i32 {
                    for x in -1..=w as i32 {
                        let inside = x >= 0 && y >= 0 && x < w as i32 && y < h as i32;
                        assert_eq!(g.at(x, y), if inside { alpha } else { 0 }, "{w}x{h}: {c} at ({x}, {y})");
                    }
                }
            }
        }
    }

    #[test]
    fn partial_blocks_take_their_fraction_and_opposite_quadrants_tile_the_cell() {
        let (w, h, t) = (9, 19, 1);
        assert_eq!(ink('▄', w, h, t).rows(0), (9..19).collect::<Vec<_>>(), "half of 19 rounds to 10");
        assert_eq!(ink('▁', w, h, t).rows(4), vec![17, 18], "an eighth of 19 rounds to 2");
        assert_eq!(ink('▌', w, h, t).cols(0), (0..5).collect::<Vec<_>>());
        assert_eq!(ink('▕', w, h, t).cols(0), vec![8]);
        let (a, b) = (ink('▚', w, h, t), ink('▞', w, h, t));
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let (pa, pb) = (a.at(x, y), b.at(x, y));
                assert!(pa == 255 || pb == 255, "({x}, {y}) is covered by ▚ or ▞");
            }
        }
    }

    #[test]
    fn powerline_arrows_fill_the_cell_height_and_point_at_its_far_edge() {
        let (w, h, t) = (14, 32, 2);
        let (right, mid) = (w as i32 - 1, h as i32 / 2);
        let arrow = ink('\u{e0b0}', w, h, t);
        // The corner pixels are cut by the slanted sides, as in Ghostty.
        assert_eq!(arrow.rows(0), (1..h as i32 - 1).collect::<Vec<_>>(), " covers its left edge");
        assert!(arrow.at(right, mid) > 0 && arrow.at(right, 0) == 0, " points at the right edge's middle");
        let back = ink('\u{e0b2}', w, h, t);
        assert_eq!(back.rows(right), (1..h as i32 - 1).collect::<Vec<_>>());
        assert!(back.at(0, mid) > 0 && back.at(0, 0) == 0);
        // The angled separators of p10k's rainbow style.
        let slant = ink('\u{e0bc}', w, h, t);
        assert_eq!(slant.cols(0), (0..w as i32).collect::<Vec<_>>(), " covers its top edge");
        assert_eq!(slant.rows(0), (0..h as i32 - 1).collect::<Vec<_>>(), " covers its left edge");
        assert_eq!(slant.at(right, h as i32 - 1), 0);
    }

    #[test]
    fn left_facing_powerline_symbols_mirror_the_right_facing_ones() {
        let (w, h, t) = (14, 32, 2);
        for (l, r) in
            [('\u{e0b6}', '\u{e0b4}'), ('\u{e0b3}', '\u{e0b1}'), ('\u{e0b7}', '\u{e0b5}'), ('\u{e0d4}', '\u{e0d2}')]
        {
            let (a, b) = (ink(l, w, h, t), ink(r, w, h, t));
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let (pa, pb) = (a.at(x, y), b.at(w as i32 - 1 - x, y));
                    // Skia's antialiasing is not exactly symmetric under a mirror: an edge pixel
                    // may differ by about 10 levels, while drawing on the wrong side differs by 255.
                    assert!(pa.abs_diff(pb) <= 32, "U+{:04X} at ({x}, {y}): {pa} vs {pb}", l as u32);
                }
            }
            assert!(!b.rows(0).is_empty(), "U+{:04X} draws something", r as u32);
        }
    }

    /// The ink at `x` across a vertical edge, or `y` across a horizontal one: the line style
    /// that reaches that edge, by its stroke count and width.
    fn edge(g: &Ink, vertical: bool, at: i32) -> &'static str {
        let span = if vertical { g.rows(at) } else { g.cols(at) };
        let strokes = span.windows(2).filter(|p| p[1] != p[0] + 1).count() + usize::from(!span.is_empty());
        match (strokes, span.len()) {
            (0, _) => "none",
            (1, 2) => "light",
            (1, 4) => "heavy",
            (2, 4) => "double",
            other => panic!("unexpected edge {other:?}"),
        }
    }

    #[test]
    fn junctions_carry_the_styles_their_unicode_names_give_each_side() {
        let (w, h, t) = (14, 32, 2);
        // (character, up, right, down, left), from the Unicode character names.
        for (c, up, right, down, left) in [
            ('┢', "light", "heavy", "heavy", "none"),  // UP LIGHT AND RIGHT DOWN HEAVY
            ('╅', "light", "light", "heavy", "heavy"), // LEFT DOWN HEAVY AND RIGHT UP LIGHT
            ('┭', "none", "light", "light", "heavy"),  // LEFT HEAVY AND RIGHT DOWN LIGHT
            ('╡', "light", "none", "light", "double"), // VERTICAL SINGLE AND LEFT DOUBLE
            ('╓', "none", "light", "double", "none"),  // DOWN DOUBLE AND RIGHT SINGLE
            ('╬', "double", "double", "double", "double"),
            ('╼', "none", "heavy", "none", "light"), // LIGHT LEFT AND HEAVY RIGHT
        ] {
            let g = ink(c, w, h, t);
            let got =
                [edge(&g, false, 0), edge(&g, true, w as i32 - 1), edge(&g, false, h as i32 - 1), edge(&g, true, 0)];
            assert_eq!(got, [up, right, down, left], "{c}: up, right, down, left");
        }
    }

    #[test]
    fn the_outlined_half_disc_is_one_line_thick() {
        let (w, h, t) = (14, 32, 2);
        // The disc's radius is the cell width, so its outline meets the right edge mid-cell.
        assert_eq!(ink('\u{e0b5}', w, h, t).cols(h as i32 / 2), vec![12, 13]);
    }

    #[test]
    fn nothing_draws_past_the_atlas_padding_even_in_odd_cells() {
        let out = crate::atlas::PAD as i32;
        // The user's cell, an odd one, and a 1 px one. Lines wider than the cell (a font whose
        // underline is thicker than its characters are wide) may reach past the padding;
        // `odd_sizes_draw_without_panicking` covers them.
        for (w, h, t) in [(14, 32, 2), (9, 19, 1), (1, 1, 1)] {
            for cp in (0x2500..=0x259f).chain(0xe0b0..=0xe0bf).chain([0xe0d2, 0xe0d4]) {
                let c = char::from_u32(cp).unwrap();
                let big = ink_with_margin(c, w, h, t, out as u32 + 2);
                let mut total = 0u32;
                for y in -out - 2..h as i32 + out + 2 {
                    for x in -out - 2..w as i32 + out + 2 {
                        let a = big.at(x, y);
                        total += u32::from(a);
                        let beyond = x < -out || y < -out || x >= w as i32 + out || y >= h as i32 + out;
                        assert!(!beyond || a == 0, "{w}x{h}/{t}: U+{cp:04X} inks ({x}, {y}) past the padding");
                    }
                }
                if w > 1 {
                    assert!(total > 0, "{w}x{h}/{t}: U+{cp:04X} draws something");
                }
            }
        }
    }

    #[test]
    fn odd_sizes_draw_without_panicking() {
        for (w, h, t) in [(1, 1, 1), (3, 5, 4), (2, 40, 1), (40, 2, 3)] {
            for cp in (0x2500..=0x259f).chain(0xe0b0..=0xe0bf).chain([0xe0d2, 0xe0d4]) {
                ink_with_margin(char::from_u32(cp).unwrap(), w, h, t, 8);
            }
        }
    }
}
