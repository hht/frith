//! Which font draws each character, and the terminal cell metrics.
//!
//! Answers: given a character, which typeface and glyph draw it?
//! Must not know: terminals, layout, or how glyphs are rasterized and cached.
//!
//! Lookup order (ADR 5): the configured family, the configured fallbacks, the bundled
//! Symbols Nerd Font, then the system's per-character fallback. LastResort is never used: it
//! claims every code point with placeholder boxes, so once loaded it would shadow every later
//! fallback (spike problem 9: braille showed as `?` after p10k icons were drawn).

use std::collections::HashMap;

use skia_safe::{Data, Font, FontMgr, FontStyle, GlyphId, Typeface, font::Edging};

/// Icons only, no text glyphs. @see https://github.com/ryanoasis/nerd-fonts/tree/v3.5.1/patched-fonts/NerdFontsSymbolsOnly
static NERD_SYMBOLS: &[u8] = include_bytes!("../../../assets/fonts/SymbolsNerdFontMono-Regular.ttf");

/// The macOS font that draws a placeholder for any code point. Its family name carries the
/// leading dot of hidden system fonts (observed on macOS 26.5).
const LAST_RESORT: &str = ".LastResort";

/// Languages hinted to the system fallback, so Han characters prefer Simplified Chinese forms.
const FALLBACK_LANGS: [&str; 3] = ["zh-Hans", "ja", "ko"];

#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    pub family: String,
    pub fallbacks: Vec<String>,
}

/// A typeface in regular and bold.
struct Face {
    regular: Font,
    bold: Font,
}

pub struct Fonts {
    mgr: FontMgr,
    size: f32,
    /// Configured and bundled faces first, then faces the system fallback supplied.
    faces: Vec<Face>,
    map: HashMap<char, Option<(usize, GlyphId)>>,
    pub cell_w: f32,
    pub cell_h: f32,
    pub ascent: f32,
}

impl Fonts {
    /// `size` is in physical pixels.
    pub fn new(cfg: &FontConfig, size: f32) -> Fonts {
        let mgr = FontMgr::new();
        let primary = mgr.match_family_style(&cfg.family, FontStyle::normal()).unwrap_or_else(|| {
            eprintln!("frith: font family {:?} is not installed; using Menlo", cfg.family);
            mgr.match_family_style("Menlo", FontStyle::normal()).expect("Menlo ships with macOS")
        });
        let configured = cfg.fallbacks.iter().filter_map(|name| {
            let tf = mgr.match_family_style(name, FontStyle::normal());
            if tf.is_none() {
                eprintln!("frith: fallback font {name:?} is not installed; skipping it");
            }
            tf
        });
        let nerd = mgr.new_from_data(Data::new_copy(NERD_SYMBOLS), None).expect("bundled Nerd Font parses");
        let typefaces: Vec<Typeface> = std::iter::once(primary).chain(configured).chain([nerd]).collect();
        let faces = typefaces.into_iter().map(|tf| face(&mgr, tf, size)).collect::<Vec<_>>();
        let font = &faces[0].regular;
        let (_, m) = font.metrics();
        let cell_h = (m.descent - m.ascent + m.leading).ceil();
        let cell_w = font.measure_str("M", None).0.round();
        let ascent = (-m.ascent).round();
        Fonts { mgr, size, faces, map: HashMap::new(), cell_w, cell_h, ascent }
    }

    /// The face index and glyph that draw `c`, or `None` when no usable font has it.
    pub fn lookup(&mut self, c: char) -> Option<(usize, GlyphId)> {
        if let Some(v) = self.map.get(&c) {
            return *v;
        }
        let known = self.faces.iter().enumerate().find_map(|(i, f)| {
            let g = f.regular.unichar_to_glyph(c as i32);
            (g != 0).then_some((i, g))
        });
        let found = known.or_else(|| {
            let tf = self.mgr.match_family_style_character("", FontStyle::normal(), &FALLBACK_LANGS, c as i32)?;
            if tf.family_name() == LAST_RESORT {
                return None;
            }
            let f = face(&self.mgr, tf, self.size);
            let g = f.regular.unichar_to_glyph(c as i32);
            if g == 0 {
                return None;
            }
            self.faces.push(f);
            Some((self.faces.len() - 1, g))
        });
        self.map.insert(c, found);
        found
    }

    pub fn font(&self, idx: usize, bold: bool) -> &Font {
        let face = &self.faces[idx];
        if bold { &face.bold } else { &face.regular }
    }
}

/// `tf` in regular and bold: the family's real bold face when it has one, otherwise
/// synthetic emboldening.
fn face(mgr: &FontMgr, tf: Typeface, size: f32) -> Face {
    let bold_tf = mgr
        .match_family_style(tf.family_name(), FontStyle::bold())
        .filter(|b| b.font_style().weight() > tf.font_style().weight());
    let embolden = bold_tf.is_none();
    Face { regular: font(tf.clone(), size, false), bold: font(bold_tf.unwrap_or(tf), size, embolden) }
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
        Fonts::new(&FontConfig { family: "Menlo".into(), fallbacks: vec![] }, 26.0)
    }

    fn family(f: &mut Fonts, c: char) -> Option<String> {
        f.lookup(c).map(|(i, _)| f.font(i, false).typeface().family_name())
    }

    #[test]
    fn ascii_comes_from_the_configured_family() {
        assert_eq!(family(&mut fonts(), 'a').as_deref(), Some("Menlo"));
    }

    #[test]
    fn nerd_icons_come_from_the_bundled_symbols_font() {
        let mut f = fonts();
        assert_eq!(family(&mut f, '\u{e0a0}').as_deref(), Some("Symbols Nerd Font Mono"), "powerline branch");
        assert_eq!(family(&mut f, '\u{f07b}').as_deref(), Some("Symbols Nerd Font Mono"), "folder icon");
    }

    #[test]
    fn han_still_resolves_after_a_character_no_font_covers() {
        let mut f = fonts();
        let odd = f.lookup('\u{10fffd}').map(|(i, g)| (f.font(i, false).typeface().family_name(), g));
        assert_eq!(odd, None, "only LastResort claims this; it must not be used");
        let han = family(&mut f, '世').expect("a system CJK font");
        assert!(!han.contains("LastResort"), "{han}");
        let (i, g) = f.lookup('世').unwrap();
        assert_ne!(g, 0);
        assert_eq!(f.font(i, false).unichar_to_glyph('世' as i32), g);
    }
}
