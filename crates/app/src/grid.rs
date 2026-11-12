//! One pane's terminal grid on screen: retained per-row draw data read from libghostty-vt's
//! render state, drawn with one `draw_atlas` call for all glyphs.
//!
//! Answers: how does a terminal's screen turn into Skia draw calls?
//! Must not know: panes, chrome, layout, or window events; callers pass the content rect.
//!
//! Only dirty rows are re-read, so an idle pane redraws from its caches. No shaping yet: one
//! glyph per cell (spike problem 4).

use libghostty_vt::color::Rgb;
use libghostty_vt::render::{CellIterator, Dirty, RenderState, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::Underline;
use libghostty_vt::terminal::Terminal;
use skia_safe::{BlendMode, Canvas, ClipOp, Color, Paint, PaintStyle, RSXform, Rect, SamplingOptions, gpu};

use crate::atlas::{self, Atlas};
use crate::fonts::Fonts;

fn color(rgb: Rgb) -> Color {
    Color::from_rgb(rgb.r, rgb.g, rgb.b)
}

#[derive(Default)]
struct Row {
    bgs: Vec<(Rect, Color)>,
    lines: Vec<(Rect, Color)>,
    xf: Vec<RSXform>,
    tex: Vec<Rect>,
    col: Vec<Color>,
}

/// Retained draw data for one pane's grid, relative to the content origin, so a pane that
/// moves keeps it. `invalidate` after the fonts or atlas are replaced.
pub struct Grid {
    rs: RenderState<'static>,
    row_it: RowIterator<'static>,
    cell_it: CellIterator<'static>,
    /// `None` until the row is read; a blank row reads as `Some` of an empty `Row`.
    rows: Vec<Option<Row>>,
    atlas_generation: u64,
    background: Color,
    cursor: Option<Rect>,
    /// The terminal's cursor color (configured, or set by OSC 12), if it has one.
    cursor_color: Option<Color>,
    // Reused every frame for the single draw_atlas call.
    xf: Vec<RSXform>,
    tex: Vec<Rect>,
    col: Vec<Color>,
}

/// How to draw one grid this frame.
#[derive(Clone, Copy)]
pub struct Look {
    /// The content area, in window coordinates.
    pub content: skia_safe::Rect,
    /// Padding between the content's left and top edges and the grid.
    pub pad: (f32, f32),
    /// An unfocused pane gets a hollow cursor.
    pub focused: bool,
    /// The cursor color when the terminal has none.
    pub cursor: Color,
    /// False draws the cached rows unchanged: DEC 2026 synchronized output holds the frame.
    pub update: bool,
}

impl Grid {
    pub fn new() -> Grid {
        Grid {
            rs: RenderState::new().expect("render state"),
            row_it: RowIterator::new().expect("row iterator"),
            cell_it: CellIterator::new().expect("cell iterator"),
            rows: Vec::new(),
            atlas_generation: 0,
            background: Color::BLACK,
            cursor: None,
            cursor_color: None,
            xf: Vec::new(),
            tex: Vec::new(),
            col: Vec::new(),
        }
    }

    pub fn invalidate(&mut self) {
        self.rows.clear();
    }

    /// Draws the grid, re-reading dirty rows first unless `look.update` is false. Returns the
    /// cursor cell in window coordinates.
    pub fn draw(
        &mut self,
        canvas: &Canvas,
        fonts: &mut Fonts,
        atlas: &mut Atlas,
        ctx: &mut gpu::DirectContext,
        term: &Terminal<'static, 'static>,
        look: &Look,
    ) -> Option<Rect> {
        // Rebuild when asked, after `invalidate` (a held pane must not show blank), or when
        // the atlas was wiped. A wipe during the rebuild leaves earlier rows pointing at stale
        // slots, so rebuild once more from scratch; two passes bound the work.
        if look.update || self.rows.is_empty() || self.atlas_generation != atlas.generation() {
            for _ in 0..2 {
                let generation = atlas.generation();
                if generation != self.atlas_generation {
                    self.rows.clear();
                    self.atlas_generation = generation;
                }
                self.rebuild(fonts, atlas, term, look.pad);
                if atlas.generation() == generation {
                    break;
                }
            }
        }

        let content = look.content;
        canvas.save();
        canvas.clip_rect(content, ClipOp::Intersect, false);
        canvas.translate((content.left, content.top));
        let mut p = Paint::default();
        p.set_color(self.background);
        canvas.draw_rect(Rect::from_wh(content.width(), content.height()), &p);
        for (r, c) in self.rows.iter().flatten().flat_map(|row| &row.bgs) {
            p.set_color(*c);
            canvas.draw_rect(*r, &p);
        }
        self.xf.clear();
        self.tex.clear();
        self.col.clear();
        for row in self.rows.iter().flatten() {
            self.xf.extend_from_slice(&row.xf);
            self.tex.extend_from_slice(&row.tex);
            self.col.extend_from_slice(&row.col);
        }
        if !self.xf.is_empty() {
            let mut tp = Paint::default();
            tp.set_anti_alias(true);
            let img = atlas.image(ctx);
            let sampling = SamplingOptions::default();
            canvas.draw_atlas(&img, &self.xf, &self.tex, &self.col[..], BlendMode::Modulate, sampling, None, &tp);
        }
        for (r, c) in self.rows.iter().flatten().flat_map(|row| &row.lines) {
            p.set_color(*c);
            canvas.draw_rect(*r, &p);
        }
        if let Some(r) = self.cursor {
            let mut cp = Paint::default();
            cp.set_color(self.cursor_color.unwrap_or(look.cursor)).set_anti_alias(true);
            if look.focused {
                cp.set_alpha(170);
            } else {
                cp.set_style(PaintStyle::Stroke).set_stroke_width(1.5);
            }
            canvas.draw_rect(r, &cp);
        }
        canvas.restore();
        self.cursor.map(|r| r.with_offset((content.left, content.top)))
    }

    /// Re-reads dirty rows from the terminal into the row caches.
    fn rebuild(&mut self, fonts: &mut Fonts, atlas: &mut Atlas, term: &Terminal<'static, 'static>, pad: (f32, f32)) {
        let snap = self.rs.update(term).expect("render state update");
        let colors = snap.colors().expect("colors");
        let (dfg, dbg) = (colors.foreground, colors.background);
        self.background = color(dbg);
        self.cursor_color = colors.cursor.map(color);
        let full = matches!(snap.dirty().expect("dirty"), Dirty::Full);
        let nrows = snap.rows().expect("rows") as usize;
        if self.rows.len() != nrows {
            self.rows.clear();
            self.rows.resize_with(nrows, || None);
        }
        let (cw, ch, asc) = (fonts.cell_w, fonts.cell_h, fonts.ascent);
        let mut row_it = self.row_it.update(&snap).expect("row iterator");
        let mut y = 0usize;
        while let Some(row) = row_it.next() {
            if y >= nrows {
                break;
            }
            let slot = &mut self.rows[y];
            if !(full || slot.is_none() || row.dirty().expect("row dirty")) {
                y += 1;
                continue;
            }
            let rc = slot.insert(Row::default());
            let top = pad.1 + y as f32 * ch;
            let mut cells = self.cell_it.update(row).expect("cell iterator");
            let mut x = 0usize;
            while let Some(cell) = cells.next() {
                let raw = cell.raw_cell().expect("raw cell");
                let wide = raw.wide().expect("cell width");
                if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                    x += 1;
                    continue;
                }
                let span = if matches!(wide, CellWide::Wide) { 2.0 } else { 1.0 };
                let left = pad.0 + x as f32 * cw;
                let mut fg = cell.fg_color().expect("fg").unwrap_or(dfg);
                let mut bg = cell.bg_color().expect("bg");
                let (mut bold, mut ul, mut strike, mut invisible) = (false, false, false, false);
                if cell.has_styling().expect("styling") {
                    let st = cell.style().expect("style");
                    if st.inverse {
                        let b = bg.unwrap_or(dbg);
                        bg = Some(fg);
                        fg = b;
                    }
                    if st.faint {
                        fg = Rgb { r: fg.r / 2 + dbg.r / 2, g: fg.g / 2 + dbg.g / 2, b: fg.b / 2 + dbg.b / 2 };
                    }
                    bold = st.bold;
                    ul = st.underline != Underline::None;
                    strike = st.strikethrough;
                    invisible = st.invisible;
                }
                if cell.is_selected().expect("selected") {
                    bg = Some(dfg);
                    fg = dbg;
                }
                if let Some(b) = bg {
                    rc.bgs.push((Rect::from_xywh(left, top, cw * span, ch), color(b)));
                }
                if ul {
                    rc.lines.push((Rect::from_xywh(left, top + asc + 2.0, cw * span, 1.5), color(fg)));
                }
                if strike {
                    rc.lines.push((Rect::from_xywh(left, top + ch * 0.5, cw * span, 1.5), color(fg)));
                }
                let cp = raw.codepoint().expect("codepoint");
                if let Some(g) =
                    char::from_u32(cp).filter(|_| !invisible && cp > 32).and_then(|c| fonts.lookup(c, bold))
                {
                    let slot = atlas.get(fonts, g);
                    rc.xf.push(RSXform::new(1.0, 0.0, (left - atlas::PAD, top - atlas::PAD)));
                    rc.tex.push(slot);
                    rc.col.push(color(fg));
                }
                x += 1;
            }
            y += 1;
        }

        self.cursor = None;
        if snap.cursor_visible().unwrap_or(false)
            && let Ok(Some(vp)) = snap.cursor_viewport()
        {
            self.cursor = Some(Rect::from_xywh(pad.0 + vp.x as f32 * cw, pad.1 + vp.y as f32 * ch, cw, ch));
        }
        let _ = snap.clean();
    }
}
