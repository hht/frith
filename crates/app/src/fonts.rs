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
    pub ascent: f32,
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
        let font = &fonts[faces[0].regular];
        let (_, m) = font.metrics();
        let cell_h = (m.descent - m.ascent + m.leading).ceil();
        let cell_w = font.measure_str("M", None).0.round();
        let ascent = (-m.ascent).round();
        Fonts { mgr, size, fonts, faces, map: HashMap::new(), cell_w, cell_h, ascent }
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
