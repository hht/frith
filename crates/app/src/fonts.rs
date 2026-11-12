//! Which font draws each character, and the terminal cell metrics.
//!
//! Answers: given a character, which typeface and glyph draw it?
//! Must not know: terminals, layout, or how glyphs are rasterized and cached.
//!
//! Lookup order (ADR 5, as in Ghostty): the configured families that are installed, the
//! bundled JetBrains Mono, the bundled Symbols Nerd Font, then the system's per-character
//! fallback. LastResort is never used: it claims every code point with placeholder boxes, so
//! once loaded it would shadow every later fallback (spike problem 9: braille showed as `?`
//! after p10k icons were drawn).

use std::collections::HashMap;

use skia_safe::font_arguments::{VariationPosition, variation_position::Coordinate};
use skia_safe::{Data, Font, FontArguments, FontMgr, FontStyle, FourByteTag, GlyphId, Typeface, font::Edging};

/// Draws text when no configured family is installed, and characters the configured ones
/// lack; Ghostty's built-in font. Variable: bold is its `wght` axis at 700, as in Ghostty.
/// @see https://github.com/JetBrains/JetBrainsMono/releases/tag/v2.304 (`fonts/variable/JetBrainsMono[wght].ttf`)
/// @see Ghostty `src/font/SharedGridSet.zig` (the embedded `variable` font at `wght` 700)
static JETBRAINS_MONO: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Variable.ttf");

/// Icons only, no text glyphs. @see https://github.com/ryanoasis/nerd-fonts/tree/v3.5.1/patched-fonts/NerdFontsSymbolsOnly
static NERD_SYMBOLS: &[u8] = include_bytes!("../../../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

/// The macOS font that draws a placeholder for any code point. Its family name carries the
/// leading dot of hidden system fonts (observed on macOS 26.5).
const LAST_RESORT: &str = ".LastResort";

/// The system UI font's family on macOS: it resolves to SF (observed on macOS 26.5), while
/// Skia's default typeface without a name is Helvetica.
const UI_FAMILY: &str = ".AppleSystemUIFont";

/// Languages hinted to the system fallback, so Han characters prefer Simplified Chinese forms.
const FALLBACK_LANGS: [&str; 3] = ["zh-Hans", "ja", "ko"];

/// One typeface of the lookup order, as indices into `Fonts::fonts`.
struct Face {
    regular: usize,
    /// The family's real bold face, when it has one heavier than regular.
    bold: Option<usize>,
    /// The regular face emboldened, for bold text the real bold face has no glyph for
    /// (Menlo Bold lacks the box-drawing characters Menlo Regular has).
    synthetic_bold: usize,
}

/// A glyph id together with the font it belongs to. Ids are per typeface (Menlo Regular and
/// Bold number 485 shared code points differently), so an id is meaningless without its font.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Glyph {
    /// Index of the font that draws it; see `Fonts::font`.
    pub font: usize,
    pub id: GlyphId,
}

pub struct Fonts {
    mgr: FontMgr,
    size: f32,
    /// Every font a `Glyph` can name.
    fonts: Vec<Font>,
    /// Lookup order: configured and bundled faces first, then faces the system fallback supplied.
    faces: Vec<Face>,
    map: HashMap<(char, bool), Option<Glyph>>,
    pub cell_w: f32,
    pub cell_h: f32,
    /// From the cell's top to the baseline.
    pub ascent: f32,
    /// Horizontal offset of every glyph from the cell's left edge.
    pub glyph_x: f32,
    /// Width of a light box-drawing line in whole pixels.
    pub box_thickness: u32,
}

impl Fonts {
    /// `families` in lookup order; the first installed one sets the cell size, as in Ghostty.
    /// `size` is in physical pixels.
    pub fn new(families: &[String], size: f32) -> Fonts {
        let mgr = FontMgr::new();
        let mut typefaces: Vec<(Typeface, Option<Typeface>)> = families
            .iter()
            .filter_map(|name| {
                let tf = mgr.match_family_style(name, FontStyle::normal());
                if tf.is_none() {
                    eprintln!("frith: font family {name:?} is not installed; skipping it");
                }
                tf.map(|tf| {
                    let bold = system_bold(&mgr, &tf);
                    (tf, bold)
                })
            })
            .collect();
        let jbm = mgr.new_from_data(Data::new_copy(JETBRAINS_MONO), None).expect("bundled JetBrains Mono parses");
        let wght = [Coordinate { axis: FourByteTag::from_chars('w', 'g', 'h', 't'), value: 700.0 }];
        let args = FontArguments::new().set_variation_design_position(VariationPosition { coordinates: &wght });
        let jbm_bold = jbm.clone_with_arguments(&args).expect("JetBrains Mono has a wght axis");
        typefaces.push((jbm, Some(jbm_bold)));
        let nerd = mgr.new_from_data(Data::new_copy(NERD_SYMBOLS), None).expect("bundled Nerd Font parses");
        typefaces.push((nerd, None));
        let mut fonts = Vec::new();
        let faces = typefaces.into_iter().map(|(tf, bold)| add_face(&mut fonts, tf, bold, size)).collect::<Vec<_>>();
        let primary = &fonts[faces[0].regular];
        let Cell { w: cell_w, h: cell_h, ascent, glyph_x } = cell_metrics(primary);
        let box_thickness = underline_thickness(primary).ceil().max(1.0) as u32;
        Fonts { mgr, size, fonts, faces, map: HashMap::new(), cell_w, cell_h, ascent, glyph_x, box_thickness }
    }

    /// The glyph that draws `c` in regular or bold, or `None` when no usable font has it.
    /// The face is chosen by regular coverage, so bold and regular text agree on the face.
    pub fn lookup(&mut self, c: char, bold: bool) -> Option<Glyph> {
        if let Some(g) = self.map.get(&(c, bold)) {
            return *g;
        }
        let has = |font: &Font| font.unichar_to_glyph(c as i32) != 0;
        let face = self.faces.iter().position(|f| has(&self.fonts[f.regular])).or_else(|| self.fallback_face(c));
        let found = face.map(|i| self.styled(i, c, bold));
        self.map.insert((c, bold), found);
        found
    }

    pub fn font(&self, g: Glyph) -> &Font {
        &self.fonts[g.font]
    }

    /// Asks the system for a face that has `c` and appends it to the lookup order.
    fn fallback_face(&mut self, c: char) -> Option<usize> {
        let tf = self.mgr.match_family_style_character("", FontStyle::normal(), &FALLBACK_LANGS, c as i32)?;
        if tf.family_name() == LAST_RESORT || tf.unichar_to_glyph(c as i32) == 0 {
            return None;
        }
        let bold = system_bold(&self.mgr, &tf);
        let face = add_face(&mut self.fonts, tf, bold, self.size);
        self.faces.push(face);
        Some(self.faces.len() - 1)
    }

    /// The glyph for `c` in face `i`, whose regular font has it.
    fn styled(&self, i: usize, c: char, bold: bool) -> Glyph {
        let f = &self.faces[i];
        let glyph = |font: usize| Glyph { font, id: self.fonts[font].unichar_to_glyph(c as i32) };
        if !bold {
            return glyph(f.regular);
        }
        f.bold.map(glyph).filter(|g| g.id != 0).unwrap_or_else(|| glyph(f.synthetic_bold))
    }
}

/// The system's UI font at `size` pixels, for chrome text (ADR 14). Characters it lacks
/// fall back to the terminal's lookup order.
pub fn ui_font(size: f32) -> Font {
    let mgr = FontMgr::new();
    let tf = mgr
        .match_family_style(UI_FAMILY, FontStyle::normal())
        .or_else(|| mgr.legacy_make_typeface(None, FontStyle::normal()))
        .expect("the system has a default typeface");
    font(tf, size, false)
}

/// The cell `Fonts` exposes, in pixels.
struct Cell {
    w: f32,
    h: f32,
    ascent: f32,
    glyph_x: f32,
}

/// The cell for `font` by Ghostty's rules, so the grid matches Ghostty's: the widest
/// printable ASCII advance and the line height, each rounded, with the face centered in the
/// rounded cell (vertically on a whole-pixel baseline). Vertical metrics come from the
/// font's own tables, not the platform's text stack, so they are the same on every platform.
/// @see Ghostty `src/font/face/coretext.zig` (`getMetrics`) and `src/font/Metrics.zig` (`calc`)
fn cell_metrics(font: &Font) -> Cell {
    let (ascent, descent, gap) = table_metrics(&font.typeface(), f64::from(font.size())).unwrap_or_else(|| {
        // A font without these tables, such as a bitmap font: the platform's metrics, as
        // Ghostty does.
        let (_, m) = font.metrics();
        (f64::from(-m.ascent), f64::from(-m.descent), f64::from(m.leading))
    });
    let ascii: String = (32u8..127).map(char::from).collect();
    let glyphs = font.str_to_glyphs_vec(&ascii);
    let mut widths = vec![0.0; glyphs.len()];
    font.get_widths(&glyphs, &mut widths);
    let face_w = f64::from(widths.iter().fold(0.0f32, |a, &b| a.max(b)));
    let face_h = ascent - descent + gap;
    // At least 1 px, as Ghostty's `Metrics.Minimums`.
    let (cell_w, cell_h) = (face_w.round().max(1.0), face_h.round().max(1.0));
    let baseline_from_bottom = (gap / 2.0 - descent - (cell_h - face_h) / 2.0).round();
    // A face wider than the cell keeps only the fraction of its shift, as in Ghostty.
    let dx = (cell_w - face_w) / 2.0;
    let glyph_x = if dx < 0.0 { dx - dx.trunc() } else { dx };
    Cell { w: cell_w as f32, h: cell_h as f32, ascent: (cell_h - baseline_from_bottom) as f32, glyph_x: glyph_x as f32 }
}

/// Ascent, descent (negative below the baseline) and line gap in pixels at `size`, from the
/// table Ghostty would choose: OS/2 typo metrics when the font asks for them, else hhea, else
/// typo, else win metrics. An OS/2 table too short to hold these fields counts as missing,
/// as Ghostty's parser rejects it. `None` when the font has no usable `hhea` or units per em.
/// @see https://learn.microsoft.com/en-us/typography/opentype/spec/os2 (field offsets; fsSelection bit 7)
/// @see https://learn.microsoft.com/en-us/typography/opentype/spec/hhea
fn table_metrics(tf: &Typeface, size: f64) -> Option<(f64, f64, f64)> {
    let table = |tag: &[u8; 4]| tf.copy_table_data(u32::from_be_bytes(*tag));
    let u16_at = |d: &[u8], at: usize| d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let i16_at = |d: &[u8], at: usize| u16_at(d, at).map(|v| f64::from(v as i16));
    let units = tf.units_per_em().filter(|&u| u > 0)?;
    let per_unit = size / f64::from(units);
    let scaled = |(a, d, g): (f64, f64, f64)| Some((a * per_unit, d * per_unit, g * per_unit));
    let hhea = table(b"hhea")?;
    let from_hhea = (i16_at(&hhea, 4)?, i16_at(&hhea, 6)?, i16_at(&hhea, 8)?);
    let os2 = table(b"OS/2");
    let Some(os2) = os2.as_deref().filter(|t| t.len() >= 78) else { return scaled(from_hhea) };
    let typo = (i16_at(os2, 68)?, i16_at(os2, 70)?, i16_at(os2, 72)?);
    const USE_TYPO_METRICS: u16 = 1 << 7;
    if u16_at(os2, 62)? & USE_TYPO_METRICS != 0 {
        scaled(typo)
    } else if from_hhea.0 != 0.0 || from_hhea.1 != 0.0 {
        scaled(from_hhea)
    } else if typo.0 != 0.0 || typo.1 != 0.0 {
        scaled(typo)
    } else {
        scaled((f64::from(u16_at(os2, 74)?), -f64::from(u16_at(os2, 76)?), 0.0))
    }
}

/// The underline thickness in pixels, from the font's `post` table; when the table is
/// missing or gives 0, 15% of the x-height, as Ghostty estimates it. Ghostty draws box
/// lines this thick, rounded up to whole pixels and at least 1.
/// @see https://learn.microsoft.com/en-us/typography/opentype/spec/post (underlineThickness at offset 10)
/// @see Ghostty `src/font/face/coretext.zig` (`getMetrics`), `src/font/Metrics.zig` (`underlineThickness`, `calc`)
fn underline_thickness(font: &Font) -> f32 {
    let tf = font.typeface();
    let units = tf.units_per_em().filter(|&u| u > 0);
    let post = tf.copy_table_data(u32::from_be_bytes(*b"post"));
    let from_post = post.as_deref().and_then(|d| d.get(10..12)).map(|b| i16::from_be_bytes([b[0], b[1]]));
    match (from_post, units) {
        (Some(t), Some(u)) if t > 0 => (f64::from(t) * f64::from(font.size()) / f64::from(u)) as f32,
        // Ghostty prefers the OS/2 sxHeight here; Skia's x-height comes from the platform
        // (CoreText on macOS), which may differ. Only fonts without a usable post table get here.
        _ => 0.15 * font.metrics().1.x_height,
    }
}

/// The installed bold face of `tf`'s family, when it is heavier than `tf`.
fn system_bold(mgr: &FontMgr, tf: &Typeface) -> Option<Typeface> {
    mgr.match_family_style(tf.family_name(), FontStyle::bold())
        .filter(|b| b.font_style().weight() > tf.font_style().weight())
}

/// Adds `tf`'s regular, real bold (when it has one) and synthetic bold fonts.
fn add_face(fonts: &mut Vec<Font>, tf: Typeface, bold_tf: Option<Typeface>, size: f32) -> Face {
    let mut push = |f: Font| {
        fonts.push(f);
        fonts.len() - 1
    };
    let regular = push(font(tf.clone(), size, false));
    let bold = bold_tf.map(|b| push(font(b, size, false)));
    let synthetic_bold = push(font(tf, size, true));
    Face { regular, bold, synthetic_bold }
}

fn font(tf: Typeface, size: f32, embolden: bool) -> Font {
    let mut f = Font::from_typeface(tf, size);
    f.set_edging(Edging::AntiAlias);
    f.set_subpixel(true);
    f.set_embolden(embolden);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Fonts {
        with(&["Menlo"])
    }

    fn with(families: &[&str]) -> Fonts {
        Fonts::new(&families.iter().map(|f| f.to_string()).collect::<Vec<_>>(), 26.0)
    }

    fn family(f: &mut Fonts, c: char) -> Option<String> {
        f.lookup(c, false).map(|g| f.font(g).typeface().family_name())
    }

    #[test]
    fn bold_glyphs_come_from_the_bold_face_or_else_an_emboldened_regular_one() {
        let mut f = fonts();
        for c in ['a', '●', '❯', '✓'] {
            let g = f.lookup(c, true).unwrap();
            let font = f.font(g);
            assert_eq!(*font.typeface().font_style().weight(), 700, "{c}: drawn by Menlo Bold");
            assert_eq!(font.unichar_to_glyph(c as i32), g.id, "{c}: glyph id of the font that draws it");
        }
        // Menlo Bold has no box-drawing characters; Menlo Regular does.
        let g = f.lookup('─', true).unwrap();
        assert_eq!((*f.font(g).typeface().font_style().weight(), f.font(g).is_embolden()), (400, true));
        assert_eq!(f.font(g).unichar_to_glyph('─' as i32), g.id);
        let g = f.lookup('\u{f07b}', true).unwrap();
        assert!(f.font(g).is_embolden(), "the bundled Nerd font has no bold face");
        let g = f.lookup('●', false).unwrap();
        assert_eq!((*f.font(g).typeface().font_style().weight(), f.font(g).is_embolden()), (400, false));
    }

    #[test]
    fn ascii_comes_from_the_configured_family() {
        assert_eq!(family(&mut fonts(), 'a').as_deref(), Some("Menlo"));
    }

    #[test]
    fn the_cell_and_baseline_match_ghostty() {
        // Measured on the user's Ghostty screenshot (font-size 12 on a 2x display, so 24 px,
        // JetBrains Mono): powerline backgrounds 14 px per column and 32 px tall, and glyph
        // ink 1 px lower in the cell than Frith drew it with a 24 px baseline.
        let f = Fonts::new(&[], 24.0);
        assert_eq!((f.cell_w, f.cell_h, f.ascent), (14.0, 32.0, 25.0));
        // JetBrains Mono advances 600 of 1000 units, 14.4 px: centered in 14 px, 0.2 px left.
        assert!((f.glyph_x + 0.2).abs() < 1e-4, "{}", f.glyph_x);
    }

    #[test]
    fn chrome_text_uses_the_system_ui_font_not_skias_default() {
        let f = ui_font(24.0);
        // SF's PostScript name; without a family name Skia falls back to Helvetica.
        assert_eq!(f.typeface().post_script_name().as_deref(), Some(".SFNS-Regular"));
        assert_eq!(f.size(), 24.0);
    }

    #[test]
    fn box_lines_are_as_thick_as_the_underline_rounded_up() {
        // JetBrains Mono's post table: underlineThickness 50 of 1000 units, 1.2 px at 24 px.
        assert_eq!(Fonts::new(&[], 24.0).box_thickness, 2);
        // The table gives 1.5 px, rounded up to 2; the x-height estimate (sxHeight 550) would
        // give 2.475 px, rounded up to 3.
        assert_eq!(Fonts::new(&[], 30.0).box_thickness, 2);
        assert_eq!(Fonts::new(&[], 12.0).box_thickness, 1);
        assert_eq!(Fonts::new(&[], 0.1).box_thickness, 1, "at least 1 px");
    }

    #[test]
    fn a_tiny_font_still_has_a_cell_of_at_least_one_pixel() {
        let f = Fonts::new(&[], 0.1);
        assert!(f.cell_w >= 1.0 && f.cell_h >= 1.0, "{} x {}", f.cell_w, f.cell_h);
    }

    #[test]
    fn the_first_installed_family_draws_text_and_sets_the_cell() {
        let mut f = with(&["No Such Font", "Monaco", "Menlo"]);
        assert_eq!(family(&mut f, 'a').as_deref(), Some("Monaco"));
        let (menlo, monaco) = (fonts(), with(&["Monaco"]));
        assert_ne!(menlo.cell_h, monaco.cell_h, "the two families need different cells for this test");
        assert_eq!((f.cell_w, f.cell_h), (monaco.cell_w, monaco.cell_h));
    }

    #[test]
    fn without_an_installed_family_the_bundled_jetbrains_mono_draws_text() {
        for families in [&[][..], &["No Such Font"]] {
            let mut f = with(families);
            assert_eq!(family(&mut f, 'a').as_deref(), Some("JetBrains Mono"), "{families:?}");
        }
    }

    fn weight(f: &Fonts, g: Glyph) -> Option<f32> {
        let wght = skia_safe::FourByteTag::from_chars('w', 'g', 'h', 't');
        let axes = f.font(g).typeface().variation_design_position()?;
        axes.iter().find(|c| c.axis == wght).map(|c| c.value)
    }

    #[test]
    fn bundled_bold_is_jetbrains_mono_at_weight_700() {
        let mut f = with(&[]);
        let (regular, bold) = (f.lookup('a', false).unwrap(), f.lookup('a', true).unwrap());
        assert_eq!((weight(&f, regular), weight(&f, bold)), (Some(400.0), Some(700.0)));
        assert_eq!(family(&mut f, 'a').as_deref(), Some("JetBrains Mono"));
        assert!(!f.font(bold).is_embolden(), "a real weight, not a synthetic one");
    }

    #[test]
    fn jetbrains_mono_comes_after_the_configured_families_and_before_the_system() {
        let (mut menlo, mut jbm) = (fonts(), with(&[]));
        let only_in_jbm = (0x2000..0x2c00)
            .filter_map(char::from_u32)
            .find(|&c| {
                let m = menlo.lookup(c, false).map(|g| menlo.font(g).typeface().family_name());
                let j = jbm.lookup(c, false).map(|g| jbm.font(g).typeface().family_name());
                m.as_deref() != Some("Menlo") && j.as_deref() == Some("JetBrains Mono")
            })
            .expect("a character JetBrains Mono has and Menlo lacks");
        assert_eq!(family(&mut menlo, 'a').as_deref(), Some("Menlo"));
        assert_eq!(family(&mut menlo, only_in_jbm).as_deref(), Some("JetBrains Mono"), "{only_in_jbm:?}");
    }

    #[test]
    fn nerd_icons_come_from_the_bundled_symbols_font() {
        let mut f = fonts();
        // JetBrains Mono has the powerline symbols; it comes first, as in Ghostty.
        assert_eq!(family(&mut f, '\u{e0a0}').as_deref(), Some("JetBrains Mono"), "powerline branch");
        assert_eq!(family(&mut f, '\u{f07b}').as_deref(), Some("Symbols Nerd Font Mono"), "folder icon");
    }

    #[test]
    fn han_still_resolves_after_a_character_no_font_covers() {
        let mut f = fonts();
        let odd = f.lookup('\u{10fffd}', false).map(|g| (f.font(g).typeface().family_name(), g));
        assert_eq!(odd, None, "only LastResort claims this; it must not be used");
        let han = family(&mut f, '世').expect("a system CJK font");
        assert!(!han.contains("LastResort"), "{han}");
        let g = f.lookup('世', false).unwrap();
        assert_ne!(g.id, 0);
        assert_eq!(f.font(g).unichar_to_glyph('世' as i32), g.id);
    }
}
