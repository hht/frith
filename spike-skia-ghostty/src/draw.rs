//! Skia drawing: glyph lookup and atlas per window, a cached grid view per pane, and the
//! window chrome (pane frames, dock bar, drop preview, drag ghost, glass).
//!
//! Answers: how does a window's state turn into Skia draw calls?
//! Must not know: window-system events, PTYs, or layout rules; it paints a `chrome::Scene`.
//!
//! Two grid strategies are kept for measurement: `Blob` (a TextBlob per row per color) and
//! `Atlas` (a CPU glyph atlas drawn with one `draw_atlas` per pane). No shaping: one glyph
//! per cell (see RESULTS.md, problem 4).

use std::collections::HashMap;

use libghostty_vt::color::Rgb;
use libghostty_vt::render::{CellIterator, Dirty, RenderState, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::Underline;
use libghostty_vt::terminal::Terminal;
use skia_safe::{
    BlendMode, Canvas, ClipOp, Color, Data, Font, FontMgr, FontStyle, GlyphId, Image, Paint,
    PaintStyle, Point as SkPoint, RRect, RSXform, Rect as SkRect, RuntimeEffect, SamplingOptions,
    Shader, TextBlob, TextBlobBuilder, TileMode, canvas::SaveLayerRec, font::Edging, gpu,
    gradient::{Colors, Gradient, Interpolation, shaders::linear_gradient},
    image_filters,
};

use crate::chrome::{Button, Frame, Hit, Metrics, Scene};
use crate::layout::{PaneId, Rect};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Blob,
    Atlas,
}

fn sk(r: Rect) -> SkRect {
    SkRect::from_xywh(r.x, r.y, r.w, r.h)
}

fn color(rgb: Rgb) -> Color {
    Color::from_rgb(rgb.r, rgb.g, rgb.b)
}

mod theme {
    use skia_safe::Color;
    pub const BG_TOP: Color = Color::from_rgb(14, 17, 30);
    pub const BG_BOTTOM: Color = Color::from_rgb(26, 20, 46);
    pub const HEADER: Color = Color::from_rgb(27, 30, 43);
    pub const HEADER_FOCUSED: Color = Color::from_rgb(35, 39, 58);
    pub const TEXT: Color = Color::from_rgb(222, 226, 242);
    pub const TEXT_DIM: Color = Color::from_rgb(140, 146, 170);
    pub const ACCENT: Color = Color::from_rgb(112, 152, 255);
    pub const ATTENTION: Color = Color::from_rgb(255, 184, 77);
    pub const BORDER: Color = Color::from_argb(40, 255, 255, 255);
    pub const HOVER: Color = Color::from_argb(36, 255, 255, 255);
    pub const DIM: Color = Color::from_argb(46, 0, 0, 0);
    pub const GLASS: Color = Color::from_argb(110, 24, 27, 42);
    pub const PREVIEW: Color = Color::from_argb(70, 112, 152, 255);
    pub const CHIP: Color = Color::from_argb(28, 255, 255, 255);
}

// ---------------------------------------------------------------------------------------
// Fonts and glyph atlas (one per window: both depend on the window's scale factor)
// ---------------------------------------------------------------------------------------

pub struct Fonts {
    mgr: FontMgr,
    size: f32,
    regular: Vec<Font>,
    bold: Vec<Font>,
    map: HashMap<char, (usize, GlyphId)>,
    pub cell_w: f32,
    pub cell_h: f32,
    pub ascent: f32,
}

impl Fonts {
    pub fn new(family: &str, size: f32) -> Fonts {
        let mgr = FontMgr::new();
        let tf = mgr
            .match_family_style(family, FontStyle::normal())
            .or_else(|| mgr.match_family_style("Menlo", FontStyle::normal()))
            .expect("no usable font");
        let font = Self::make(tf, size, false);
        let (_, m) = font.metrics();
        let cell_h = (m.descent - m.ascent + m.leading).ceil();
        let cell_w = font.measure_str("M", None).0.round();
        let ascent = (-m.ascent).round();
        let bold = Self::make(font.typeface(), size, true);
        Fonts { mgr, size, regular: vec![font], bold: vec![bold], map: HashMap::new(), cell_w, cell_h, ascent }
    }

    fn make(tf: skia_safe::Typeface, size: f32, bold: bool) -> Font {
        let mut f = Font::from_typeface(tf, size);
        f.set_edging(Edging::AntiAlias);
        f.set_subpixel(true);
        f.set_embolden(bold);
        f
    }

    fn font(&self, idx: usize, bold: bool) -> &Font {
        if bold { &self.bold[idx] } else { &self.regular[idx] }
    }

    /// Font index and glyph for `c`, falling back to a system font per codepoint.
    /// Glyph 0 means no installed font covers `c`.
    fn lookup(&mut self, c: char) -> (usize, GlyphId) {
        if let Some(v) = self.map.get(&c) {
            return *v;
        }
        let found = self.regular.iter().enumerate().find_map(|(i, f)| {
            let g = f.unichar_to_glyph(c as i32);
            (g != 0).then_some((i, g))
        });
        let v = found.unwrap_or_else(|| {
            match self.mgr.match_family_style_character("", FontStyle::normal(), &["zh-Hans", "ja", "ko"], c as i32) {
                Some(tf) => {
                    let f = Self::make(tf.clone(), self.size, false);
                    let g = f.unichar_to_glyph(c as i32);
                    self.regular.push(f);
                    self.bold.push(Self::make(tf, self.size, true));
                    (self.regular.len() - 1, g)
                }
                None => (0, 0),
            }
        });
        self.map.insert(c, v);
        v
    }
}

struct Atlas {
    surface: skia_safe::Surface,
    image: Option<Image>,
    dirty: bool,
    slots: HashMap<(usize, bool, GlyphId), SkRect>,
    next: (f32, f32),
    slot: (f32, f32),
    uploads: u32,
    /// Bumped whenever the atlas is wiped; cached slot rects from an older generation are stale.
    generation: u64,
}

const ATLAS: i32 = 2048;
const APAD: f32 = 4.0;

impl Atlas {
    fn new(cell_w: f32, cell_h: f32) -> Atlas {
        Atlas {
            surface: skia_safe::surfaces::raster_n32_premul((ATLAS, ATLAS)).expect("atlas surface"),
            image: None,
            dirty: true,
            slots: HashMap::new(),
            next: (0.0, 0.0),
            slot: ((cell_w * 2.0 + APAD * 2.0).ceil(), (cell_h + APAD * 2.0).ceil()),
            uploads: 0,
            generation: 0,
        }
    }

    fn get(&mut self, fonts: &Fonts, fi: usize, bold: bool, g: GlyphId) -> SkRect {
        if let Some(r) = self.slots.get(&(fi, bold, g)) {
            return *r;
        }
        let (sw, sh) = self.slot;
        if self.next.0 + sw > ATLAS as f32 {
            self.next = (0.0, self.next.1 + sh);
        }
        if self.next.1 + sh > ATLAS as f32 {
            // Full: wipe and start over. PROTOTYPE: a real atlas would grow or evict LRU.
            self.slots.clear();
            self.surface.canvas().clear(Color::TRANSPARENT);
            self.next = (0.0, 0.0);
            self.generation += 1;
        }
        let r = SkRect::from_xywh(self.next.0, self.next.1, sw, sh);
        self.next.0 += sw;
        let mut b = TextBlobBuilder::new();
        let (gs, xs) = b.alloc_run_pos_h(fonts.font(fi, bold), 1, 0.0, None);
        gs[0] = g;
        xs[0] = 0.0;
        if let Some(blob) = b.make() {
            let mut p = Paint::default();
            p.set_color(Color::WHITE).set_anti_alias(true);
            self.surface.canvas().draw_text_blob(&blob, (r.left + APAD, r.top + APAD + fonts.ascent), &p);
        }
        self.slots.insert((fi, bold, g), r);
        self.dirty = true;
        r
    }

    fn image(&mut self, ctx: &mut gpu::DirectContext) -> Image {
        if self.dirty || self.image.is_none() {
            let snap = self.surface.image_snapshot();
            self.image = snap.new_texture_image(ctx, gpu::Mipmapped::No);
            self.dirty = false;
            self.uploads += 1;
        }
        self.image.clone().expect("atlas texture upload")
    }
}

/// Fonts and glyph atlas for one window.
pub struct Glyphs {
    pub fonts: Fonts,
    atlas: Atlas,
}

impl Glyphs {
    pub fn new(family: &str, size: f32) -> Glyphs {
        let fonts = Fonts::new(family, size);
        let atlas = Atlas::new(fonts.cell_w, fonts.cell_h);
        Glyphs { fonts, atlas }
    }

    pub fn atlas_uploads(&self) -> u32 {
        self.atlas.uploads
    }
}

// ---------------------------------------------------------------------------------------
// Terminal grid view (one per pane)
// ---------------------------------------------------------------------------------------

/// Blob-mode glyph runs of one row: (ARGB, font index, bold) -> (glyphs, x positions).
type Runs = HashMap<([u8; 4], usize, bool), (Vec<GlyphId>, Vec<f32>)>;

#[derive(Default)]
struct RowCache {
    bgs: Vec<(SkRect, Color)>,
    lines: Vec<(SkRect, Color)>,
    blobs: Vec<(Color, TextBlob)>,
    xf: Vec<RSXform>,
    tex: Vec<SkRect>,
    col: Vec<Color>,
}

impl RowCache {
    fn is_empty(&self) -> bool {
        self.bgs.is_empty() && self.lines.is_empty() && self.blobs.is_empty() && self.xf.is_empty()
    }
}

/// Retained draw data for one pane's grid. Row caches hold positions relative to the
/// content origin, so a pane that moves within its window keeps them; a pane that moves to
/// another window must `invalidate`, because the caches index that window's glyphs.
pub struct TermView {
    rs: RenderState<'static>,
    row_it: RowIterator<'static>,
    cell_it: CellIterator<'static>,
    rows: Vec<RowCache>,
    atlas_generation: u64,
    background: Color,
    cursor: Option<SkRect>,
    // frame-reused buffers for the single draw_atlas call
    xf: Vec<RSXform>,
    tex: Vec<SkRect>,
    col: Vec<Color>,
}

/// How to draw one pane's grid this frame.
#[derive(Clone, Copy)]
pub struct ViewOpts {
    pub mode: Mode,
    /// The content area, in window coordinates.
    pub content: Rect,
    /// Padding between the content edge and the grid.
    pub pad: f32,
    /// Unfocused panes get a hollow cursor.
    pub focused: bool,
    /// False draws the cached rows unchanged: DEC 2026 synchronized output holds a frame.
    pub update: bool,
}

pub struct ViewInfo {
    pub rebuilt_rows: u32,
    /// Cursor cell in window coordinates.
    pub cursor: Option<Rect>,
    pub text_draws: u32,
}

impl TermView {
    pub fn new() -> TermView {
        TermView {
            rs: RenderState::new().expect("render state"),
            row_it: RowIterator::new().expect("row iterator"),
            cell_it: CellIterator::new().expect("cell iterator"),
            rows: Vec::new(),
            atlas_generation: 0,
            background: Color::BLACK,
            cursor: None,
            xf: Vec::new(),
            tex: Vec::new(),
            col: Vec::new(),
        }
    }

    pub fn invalidate(&mut self) {
        self.rows.clear();
    }

    /// Draws the grid as `opts` says, re-reading dirty rows first unless `opts.update` is false.
    pub fn draw(
        &mut self,
        canvas: &Canvas,
        glyphs: &mut Glyphs,
        ctx: &mut gpu::DirectContext,
        term: &Terminal<'static, 'static>,
        opts: &ViewOpts,
    ) -> ViewInfo {
        let ViewOpts { mode, content, pad, focused, update } = *opts;
        let mut rebuilt = 0;
        // Rebuild when asked, when the atlas was wiped, or after `invalidate` (a moved pane
        // must not show blank while DEC 2026 holds its output).
        if update || self.rows.is_empty() || self.atlas_generation != glyphs.atlas.generation {
            // A wipe during the rebuild leaves earlier rows pointing at stale slots: rebuild
            // once more from scratch. Two passes bound the work if one screen overflows.
            for _ in 0..2 {
                let generation = glyphs.atlas.generation;
                if generation != self.atlas_generation {
                    self.rows.clear();
                    self.atlas_generation = generation;
                }
                rebuilt += self.rebuild(glyphs, term, mode, pad);
                if glyphs.atlas.generation == generation {
                    break;
                }
            }
        }

        canvas.save();
        canvas.clip_rect(sk(content), ClipOp::Intersect, false);
        canvas.translate((content.x, content.y));
        let mut p = Paint::default();
        p.set_color(self.background);
        canvas.draw_rect(SkRect::from_wh(content.w, content.h), &p);
        for rc in &self.rows {
            for (r, c) in &rc.bgs {
                p.set_color(*c);
                canvas.draw_rect(*r, &p);
            }
        }
        let mut tp = Paint::default();
        tp.set_anti_alias(true);
        let mut text_draws = 0;
        match mode {
            Mode::Blob => {
                for (c, blob) in self.rows.iter().flat_map(|rc| &rc.blobs) {
                    tp.set_color(*c);
                    canvas.draw_text_blob(blob, (0.0, 0.0), &tp);
                    text_draws += 1;
                }
            }
            Mode::Atlas => {
                self.xf.clear();
                self.tex.clear();
                self.col.clear();
                for rc in &self.rows {
                    self.xf.extend_from_slice(&rc.xf);
                    self.tex.extend_from_slice(&rc.tex);
                    self.col.extend_from_slice(&rc.col);
                }
                if !self.xf.is_empty() {
                    let img = glyphs.atlas.image(ctx);
                    canvas.draw_atlas(
                        &img,
                        &self.xf,
                        &self.tex,
                        &self.col[..],
                        BlendMode::Modulate,
                        SamplingOptions::default(),
                        None,
                        &tp,
                    );
                    text_draws = 1;
                }
            }
        }
        for (r, c) in self.rows.iter().flat_map(|rc| &rc.lines) {
            p.set_color(*c);
            canvas.draw_rect(*r, &p);
        }
        if let Some(r) = self.cursor {
            let mut cp = Paint::default();
            cp.set_color(theme::ACCENT).set_anti_alias(true);
            if focused {
                cp.set_alpha(170);
            } else {
                cp.set_style(PaintStyle::Stroke).set_stroke_width(1.5);
            }
            canvas.draw_rect(r, &cp);
        }
        canvas.restore();

        let cursor = self.cursor.map(|r| Rect::new(content.x + r.left, content.y + r.top, r.width(), r.height()));
        ViewInfo { rebuilt_rows: rebuilt, cursor, text_draws }
    }

    /// Re-reads dirty rows from the terminal into the row caches. Returns rows rebuilt.
    fn rebuild(&mut self, glyphs: &mut Glyphs, term: &Terminal<'static, 'static>, mode: Mode, pad: f32) -> u32 {
        let snap = self.rs.update(term).expect("render state update");
        let colors = snap.colors().expect("colors");
        let (dfg, dbg) = (colors.foreground, colors.background);
        self.background = color(dbg);
        let full = matches!(snap.dirty().expect("dirty"), Dirty::Full);
        let nrows = snap.rows().expect("rows") as usize;
        if self.rows.len() != nrows {
            self.rows.clear();
            self.rows.resize_with(nrows, RowCache::default);
        }
        let fonts = &mut glyphs.fonts;
        let atlas = &mut glyphs.atlas;
        let (cw, ch, asc) = (fonts.cell_w, fonts.cell_h, fonts.ascent);
        let mut rebuilt = 0;
        let mut row_it = self.row_it.update(&snap).expect("row iterator");
        let mut y = 0usize;
        while let Some(row) = row_it.next() {
            if y >= nrows {
                break;
            }
            let rc = &mut self.rows[y];
            if !(full || rc.is_empty() || row.dirty().expect("row dirty")) {
                y += 1;
                continue;
            }
            rebuilt += 1;
            *rc = RowCache::default();
            let top = pad + y as f32 * ch;
            // glyph runs grouped by (color, font, bold) for the blob path
            let mut runs: Runs = HashMap::new();
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
                let left = pad + x as f32 * cw;
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
                    rc.bgs.push((SkRect::from_xywh(left, top, cw * span, ch), color(b)));
                }
                if ul {
                    rc.lines.push((SkRect::from_xywh(left, top + asc + 2.0, cw * span, 1.5), color(fg)));
                }
                if strike {
                    rc.lines.push((SkRect::from_xywh(left, top + ch * 0.5, cw * span, 1.5), color(fg)));
                }
                let cp = raw.codepoint().expect("codepoint");
                if let Some(c) = char::from_u32(cp).filter(|_| !invisible && cp > 32) {
                    let (fi, g) = fonts.lookup(c);
                    if g != 0 {
                        match mode {
                            Mode::Blob => {
                                let col = color(fg);
                                let e = runs.entry(([col.a(), col.r(), col.g(), col.b()], fi, bold)).or_default();
                                e.0.push(g);
                                e.1.push(left);
                            }
                            Mode::Atlas => {
                                let r = atlas.get(fonts, fi, bold, g);
                                rc.xf.push(RSXform::new(1.0, 0.0, (left - APAD, top - APAD)));
                                rc.tex.push(r);
                                rc.col.push(color(fg));
                            }
                        }
                    }
                }
                x += 1;
            }
            for ((col, fi, bold), (gs, xs)) in runs {
                let mut b = TextBlobBuilder::new();
                let (g2, x2) = b.alloc_run_pos_h(fonts.font(fi, bold), gs.len(), top + asc, None);
                g2.copy_from_slice(&gs);
                x2.copy_from_slice(&xs);
                if let Some(blob) = b.make() {
                    rc.blobs.push((Color::from_argb(col[0], col[1], col[2], col[3]), blob));
                }
            }
            let _ = row.set_dirty(false);
            y += 1;
        }

        self.cursor = None;
        if snap.cursor_visible().unwrap_or(false)
            && let Ok(Some(vp)) = snap.cursor_viewport()
        {
            self.cursor = Some(SkRect::from_xywh(pad + vp.x as f32 * cw, pad + vp.y as f32 * ch, cw, ch));
        }
        let _ = snap.clean();
        rebuilt
    }
}

// ---------------------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------------------

/// How one pane's frame should look this frame.
pub struct PaneLook<'a> {
    pub title: &'a str,
    pub focused: bool,
    pub attention: bool,
    pub zoomed: bool,
}

/// Compiled shaders and look settings shared by every window.
pub struct Painter {
    sheen: RuntimeEffect,
    /// `SPIKE_BLUR` demo: number of animated glass panes and their blur sigma (logical px).
    demo: (usize, f32),
}

/// Focus ring: a soft blue-violet gradient with a bright band; `t` moves the band.
const SHEEN_SKSL: &str = r#"
uniform float2 origin;
uniform float2 size;
uniform float t;
half4 main(float2 p) {
    float2 q = (p - origin) / size;
    float band = smoothstep(0.0, 0.15, 0.15 - abs(fract(q.x * 0.6 + q.y * 0.4 - t * 0.25) - 0.5));
    half3 tint = mix(half3(0.35, 0.55, 1.0), half3(0.9, 0.5, 1.0), half(q.x));
    return half4(tint * (0.35 + 0.65 * band), 1.0) * 0.9;
}
"#;

impl Painter {
    pub fn new(demo_panes: usize, demo_sigma: f32) -> Painter {
        let sheen = RuntimeEffect::make_for_shader(SHEEN_SKSL, None)
            .unwrap_or_else(|e| panic!("focus ring SkSL does not compile: {e}"));
        Painter { sheen, demo: (demo_panes, demo_sigma) }
    }

    pub fn background(&self, canvas: &Canvas, width: f32, height: f32) {
        let colors = [theme::BG_TOP.into(), theme::BG_BOTTOM.into()];
        let gradient = Gradient::new(Colors::new_evenly_spaced(&colors, TileMode::Clamp, None), Interpolation::default());
        let shader = linear_gradient((SkPoint::new(0.0, 0.0), SkPoint::new(0.0, height)), &gradient, None);
        let mut p = Paint::default();
        p.set_shader(shader);
        canvas.draw_rect(SkRect::from_wh(width, height), &p);
    }

    /// Clips to the pane's rounded frame; pair with `canvas.restore()` after drawing content.
    pub fn clip_frame(&self, canvas: &Canvas, f: &Frame, m: &Metrics) {
        canvas.save();
        canvas.clip_rrect(RRect::new_rect_xy(sk(f.outer), m.radius, m.radius), ClipOp::Intersect, true);
    }

    /// Header (inside the frame clip) and, for unfocused panes, a dimming veil over content.
    pub fn header(&self, canvas: &Canvas, glyphs: &mut Glyphs, f: &Frame, look: &PaneLook, hover: Hit, m: &Metrics) {
        let mut p = Paint::default();
        p.set_anti_alias(true);
        if !look.focused {
            p.set_color(theme::DIM);
            canvas.draw_rect(sk(f.content), &p);
        }
        p.set_color(if look.focused { theme::HEADER_FOCUSED } else { theme::HEADER });
        canvas.draw_rect(sk(f.header), &p);

        let inset = ((m.header - m.button) / 2.0).round();
        let mut x = f.header.x + inset * 2.0;
        if look.attention {
            let r = m.button * 0.18;
            p.set_color(theme::ATTENTION);
            canvas.draw_circle((x + r, f.header.y + f.header.h / 2.0), r, &p);
            x += r * 2.0 + inset;
        }
        let buttons_left = f.buttons.iter().map(|(_, r)| r.x).fold(f32::INFINITY, f32::min);
        let fg = if look.focused { theme::TEXT } else { theme::TEXT_DIM };
        label(canvas, &mut glyphs.fonts, look.title, x, f.header, buttons_left - inset - x, fg);

        for (b, r) in f.buttons {
            if hover == Hit::Button(f.pane, b) {
                p.set_color(theme::HOVER).set_style(PaintStyle::Fill);
                canvas.draw_round_rect(sk(r), m.button / 4.0, m.button / 4.0, &p);
            }
            self.icon(canvas, b, r, look.zoomed, fg, m);
        }
    }

    fn icon(&self, canvas: &Canvas, b: Button, r: Rect, zoomed: bool, fg: Color, m: &Metrics) {
        let mut p = Paint::default();
        p.set_anti_alias(true).set_color(fg).set_style(PaintStyle::Stroke).set_stroke_width((m.button / 12.0).max(1.0));
        let (cx, cy, s) = (r.x + r.w / 2.0, r.y + r.h / 2.0, r.w * 0.22);
        match b {
            Button::Minimize => {
                canvas.draw_line((cx - s, cy + s * 0.5), (cx + s, cy + s * 0.5), &p);
            }
            Button::Zoom if zoomed => {
                let back = SkRect::from_xywh(cx - s * 0.6, cy - s, s * 1.6, s * 1.6);
                let front = SkRect::from_xywh(cx - s, cy - s * 0.6, s * 1.6, s * 1.6);
                canvas.draw_rect(back, &p);
                canvas.draw_rect(front, &p);
            }
            Button::Zoom => {
                canvas.draw_rect(SkRect::from_xywh(cx - s, cy - s, s * 2.0, s * 2.0), &p);
            }
            Button::Close => {
                canvas.draw_line((cx - s, cy - s), (cx + s, cy + s), &p);
                canvas.draw_line((cx - s, cy + s), (cx + s, cy - s), &p);
            }
        }
    }

    /// Frame border, outside the clip: the focus ring, an attention ring, or a hairline.
    pub fn border(&self, canvas: &Canvas, f: &Frame, look: &PaneLook, m: &Metrics, scale: f32) {
        let rr = RRect::new_rect_xy(sk(f.outer), m.radius, m.radius);
        let mut p = Paint::default();
        p.set_anti_alias(true).set_style(PaintStyle::Stroke);
        if look.focused {
            p.set_stroke_width(1.5 * scale).set_shader(self.sheen(f.outer, 0.0));
        } else if look.attention {
            p.set_stroke_width(1.5 * scale).set_color(theme::ATTENTION);
        } else {
            p.set_stroke_width(1.0 * scale).set_color(theme::BORDER);
        }
        canvas.draw_rrect(rr, &p);
    }

    fn sheen(&self, r: Rect, t: f32) -> Option<Shader> {
        let mut u = Vec::with_capacity(20);
        for v in [r.x, r.y, r.w, r.h, t] {
            u.extend_from_slice(&v.to_ne_bytes());
        }
        self.sheen.make_shader(Data::new_copy(&u), &[], None)
    }

    /// Hairline highlight on a hovered or dragged divider.
    pub fn divider(&self, canvas: &Canvas, r: Rect) {
        let mut p = Paint::default();
        p.set_color(theme::ACCENT).set_alpha(150);
        canvas.draw_rect(sk(r), &p);
    }

    /// The dock bar: frosted glass with a chip per minimized pane.
    pub fn dock(
        &self,
        canvas: &Canvas,
        glyphs: &mut Glyphs,
        scene: &Scene,
        look: impl Fn(PaneId) -> (String, bool),
        hover: Hit,
        m: &Metrics,
    ) {
        let Some(bar) = scene.dock else { return };
        glass(canvas, RRect::new_rect_xy(sk(bar), m.radius, m.radius), 12.0, theme::GLASS);
        let mut p = Paint::default();
        p.set_anti_alias(true).set_style(PaintStyle::Stroke).set_stroke_width(1.0).set_color(theme::BORDER);
        canvas.draw_round_rect(sk(bar), m.radius, m.radius, &p);
        p.set_style(PaintStyle::Fill);
        for chip in &scene.chips {
            let (title, attention) = look(chip.pane);
            let radius = chip.rect.h / 2.0;
            p.set_color(if hover == Hit::Chip(chip.pane) { theme::HOVER } else { theme::CHIP });
            canvas.draw_round_rect(sk(chip.rect), radius, radius, &p);
            let mut x = chip.rect.x + radius;
            if attention {
                let r = chip.rect.h * 0.18;
                p.set_color(theme::ATTENTION);
                canvas.draw_circle((x + r, chip.rect.y + chip.rect.h / 2.0), r, &p);
                x += r * 3.0;
            }
            label(canvas, &mut glyphs.fonts, &title, x, chip.rect, chip.rect.right() - radius - x, theme::TEXT);
        }
    }

    /// Where a dragged pane would land: tinted glass with an accent outline.
    pub fn drop_preview(&self, canvas: &Canvas, r: Rect, m: &Metrics) {
        let rr = RRect::new_rect_xy(sk(r), m.radius, m.radius);
        glass(canvas, rr, 6.0, theme::PREVIEW);
        let mut p = Paint::default();
        p.set_anti_alias(true).set_style(PaintStyle::Stroke).set_stroke_width(2.0).set_color(theme::ACCENT);
        canvas.draw_rrect(rr, &p);
    }

    /// A small glass card with the dragged pane's title, following the pointer.
    pub fn ghost(&self, canvas: &Canvas, glyphs: &mut Glyphs, at: (f32, f32), title: &str, m: &Metrics) {
        let r = Rect::new(at.0 + m.gap, at.1 + m.gap, m.chip, m.header);
        let rr = RRect::new_rect_xy(sk(r), m.radius, m.radius);
        glass(canvas, rr, 10.0, theme::GLASS);
        let mut p = Paint::default();
        p.set_anti_alias(true).set_style(PaintStyle::Stroke).set_stroke_width(1.5).set_shader(self.sheen(r, 0.0));
        canvas.draw_rrect(rr, &p);
        let inset = m.radius;
        label(canvas, &mut glyphs.fonts, title, r.x + inset, r, r.w - 2.0 * inset, theme::TEXT);
    }

    /// `SPIKE_BLUR` demo: animated frosted panes over everything (measurement aid).
    pub fn demo_glass(&self, canvas: &Canvas, glyphs: &mut Glyphs, t: f32, (w, h): (f32, f32), scale: f32) {
        let (n, sigma) = self.demo;
        for i in 0..n {
            let (pw, ph) = (w * 0.40, h * 0.34);
            let fi = i as f32;
            let x = (w - pw) * (0.5 + 0.45 * (t * 0.5 + fi * 2.1).sin());
            let y = (h - ph) * (0.5 + 0.42 * (t * 0.37 + fi * 1.3).cos());
            let r = Rect::new(x, y, pw, ph);
            let rr = RRect::new_rect_xy(sk(r), 14.0 * scale, 14.0 * scale);
            glass(canvas, rr, sigma * scale, Color::from_argb(96, 22, 24, 34));
            let mut p = Paint::default();
            p.set_anti_alias(true).set_style(PaintStyle::Stroke).set_stroke_width(1.5 * scale).set_shader(self.sheen(r, t));
            canvas.draw_rrect(rr, &p);
            let head = Rect::new(r.x + 14.0 * scale, r.y + 8.0 * scale, r.w, glyphs.fonts.cell_h);
            label(canvas, &mut glyphs.fonts, &format!("floating pane {} · blur σ={sigma}", i + 1), head.x, head, r.w, theme::TEXT);
        }
    }

    /// IME composition text drawn over the cursor cell: dark box, text, underline.
    pub fn preedit(&self, canvas: &Canvas, glyphs: &mut Glyphs, at: Rect, text: &str) {
        if text.is_empty() {
            return;
        }
        let cw = glyphs.fonts.cell_w;
        let w: f32 = text.chars().map(|c| libghostty_vt::unicode::codepoint_width(c).max(1) as f32 * cw).sum();
        let r = Rect::new(at.x, at.y, w, at.h);
        let mut p = Paint::default();
        p.set_color(Color::from_rgb(40, 44, 60));
        canvas.draw_rect(sk(r), &p);
        let ink = Color::from_rgb(255, 214, 102);
        label(canvas, &mut glyphs.fonts, text, r.x, r, f32::INFINITY, ink);
        p.set_color(ink);
        canvas.draw_rect(SkRect::from_xywh(r.x, r.bottom() - 2.0, r.w, 2.0), &p);
    }
}

/// Backdrop-blurred, tinted fill of `rr`: whatever is already drawn beneath shows through.
fn glass(canvas: &Canvas, rr: RRect, sigma: f32, tint: Color) {
    let blur = image_filters::blur((sigma, sigma), TileMode::Clamp, None, None).expect("blur filter");
    canvas.save();
    canvas.clip_rrect(rr, ClipOp::Intersect, true);
    canvas.save_layer(&SaveLayerRec::default().bounds(rr.rect()).backdrop(&blur));
    canvas.draw_color(tint, BlendMode::SrcOver);
    canvas.restore();
    canvas.restore();
}

/// Single-line text vertically centered in `line`, starting at `x`, cut with "…" to fit
/// `max_w`. Monospace advances with per-codepoint font fallback, like the grid.
fn label(canvas: &Canvas, fonts: &mut Fonts, text: &str, x: f32, line: Rect, max_w: f32, ink: Color) {
    let cw = fonts.cell_w;
    let width = |c: char| libghostty_vt::unicode::codepoint_width(c).max(1) as f32 * cw;
    let mut chars: Vec<char> = Vec::new();
    let mut used = 0.0;
    for c in text.chars().filter(|c| !c.is_control()) {
        if used + width(c) > max_w {
            while used + cw > max_w {
                match chars.pop() {
                    Some(last) => used -= width(last),
                    None => return,
                }
            }
            chars.push('…');
            break;
        }
        used += width(c);
        chars.push(c);
    }
    let baseline = line.y + ((line.h - fonts.cell_h) / 2.0).round() + fonts.ascent;
    let mut runs: Vec<(usize, Vec<GlyphId>, Vec<f32>)> = Vec::new();
    let mut pen = x;
    for c in chars {
        let (fi, g) = fonts.lookup(c);
        match runs.last_mut() {
            Some((f, gs, xs)) if *f == fi => {
                gs.push(g);
                xs.push(pen);
            }
            _ => runs.push((fi, vec![g], vec![pen])),
        }
        pen += width(c);
    }
    let mut b = TextBlobBuilder::new();
    for (fi, gs, xs) in &runs {
        let (g2, x2) = b.alloc_run_pos_h(fonts.font(*fi, false), gs.len(), baseline, None);
        g2.copy_from_slice(gs);
        x2.copy_from_slice(xs);
    }
    if let Some(blob) = b.make() {
        let mut p = Paint::default();
        p.set_anti_alias(true).set_color(ink);
        canvas.draw_text_blob(&blob, (0.0, 0.0), &p);
    }
}
