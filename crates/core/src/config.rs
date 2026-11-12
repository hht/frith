//! The user's settings, read from config text in Ghostty's format (ADR 13).
//!
//! Answers: what does a config file say, as exact types?
//! Must not know: where config files live, fonts, terminals, or windows.
//!
//! @see https://ghostty.org/docs/config (syntax), and Ghostty's `src/cli/args.zig`
//! `LineIterator.next` and `src/config/Config.zig` for the parsing rules followed here.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Padding on the two sides of one axis, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Padding {
    /// Left or top.
    pub start: f32,
    /// Right or bottom.
    pub end: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Font families in lookup order; empty means the built-in default.
    pub font_family: Vec<String>,
    /// In points.
    pub font_size: f32,
    pub background: Rgb,
    pub foreground: Rgb,
    /// `None` draws the cursor in the chrome's accent.
    pub cursor_color: Option<Rgb>,
    /// Palette entries the user set; the others keep the terminal's defaults.
    pub palette: BTreeMap<u8, Rgb>,
    pub padding_x: Padding,
    pub padding_y: Padding,
}

/// Ghostty's defaults, so a Ghostty config that leaves a key out looks the same in Frith.
/// @see Ghostty `src/config/Config.zig` (`font-size`, `background`, `foreground`,
/// `window-padding-x`, `window-padding-y`)
impl Default for Config {
    fn default() -> Config {
        Config {
            font_family: Vec::new(),
            font_size: 13.0,
            background: Rgb { r: 0x28, g: 0x2c, b: 0x34 },
            foreground: Rgb { r: 0xff, g: 0xff, b: 0xff },
            cursor_color: None,
            palette: BTreeMap::new(),
            padding_x: Padding { start: 2.0, end: 2.0 },
            padding_y: Padding { start: 2.0, end: 2.0 },
        }
    }
}

/// What reading one config text found besides the settings.
#[derive(Debug, Default, PartialEq)]
pub struct Notes {
    /// Keys Frith does not use (Ghostty settings it lacks, or typos), each once, in order.
    pub ignored: Vec<String>,
    /// Lines that could not be used, as `line N: reason`; each leaves its setting unchanged.
    pub errors: Vec<String>,
}

const BLANK: [char; 2] = [' ', '\t'];

impl Config {
    /// Applies `text` on top of the current settings, as Ghostty does for each file it loads:
    /// later lines win, repeatable keys append, and an empty value restores the default.
    pub fn read(&mut self, text: &str) -> Notes {
        let mut notes = Notes::default();
        // `lines` also drops the `\r` of CRLF endings, which Ghostty trims too.
        for (n, line) in text.lines().enumerate() {
            let line = line.trim_matches(BLANK);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                notes.errors.push(format!("line {}: expected `key = value`", n + 1));
                continue;
            };
            let key = key.trim_matches(BLANK);
            let value = value.trim_matches(BLANK);
            let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
            match self.set(key, value) {
                Ok(true) => {}
                Ok(false) => {
                    if !notes.ignored.iter().any(|k| k == key) {
                        notes.ignored.push(key.to_string());
                    }
                }
                Err(e) => notes.errors.push(format!("line {}: {key}: {e}", n + 1)),
            }
        }
        notes
    }

    /// Sets one key; `Ok(false)` when Frith does not use the key.
    fn set(&mut self, key: &str, value: &str) -> Result<bool, String> {
        let d = Config::default();
        let empty = value.is_empty();
        match key {
            "font-family" if empty => self.font_family = d.font_family,
            "font-family" => self.font_family.push(value.to_string()),
            "font-size" if empty => self.font_size = d.font_size,
            "font-size" => self.font_size = size(value)?,
            "background" => self.background = if empty { d.background } else { color(value)? },
            "foreground" => self.foreground = if empty { d.foreground } else { color(value)? },
            "cursor-color" => self.cursor_color = if empty { d.cursor_color } else { Some(color(value)?) },
            "palette" if empty => self.palette = d.palette,
            "palette" => {
                let (i, c) = palette_entry(value)?;
                self.palette.insert(i, c);
            }
            "window-padding-x" => self.padding_x = if empty { d.padding_x } else { padding(value)? },
            "window-padding-y" => self.padding_y = if empty { d.padding_y } else { padding(value)? },
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// Points, clamped to 1–255 as Ghostty clamps them. @see Ghostty `src/Surface.zig` (`font_size`)
fn size(value: &str) -> Result<f32, String> {
    let points = value.parse::<f32>().ok().filter(|s| s.is_finite() && *s > 0.0);
    points.map(|p| p.clamp(1.0, 255.0)).ok_or_else(|| format!("not a size: {value:?}"))
}

/// `#RRGGBB`, `RRGGBB`, `#RGB` or `RGB`. Ghostty also takes X11 names and `rgb:` forms.
/// @see Ghostty `src/terminal/color.zig`, `RGB.parse`
fn color(value: &str) -> Result<Rgb, String> {
    let bad = || format!("not a #RRGGBB color: {value:?}");
    let hex = value.strip_prefix('#').unwrap_or(value);
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let digit = |i: usize, n: usize| u8::from_str_radix(&hex[i * n..i * n + n], 16).expect("hex digits");
    match hex.len() {
        3 => Ok(Rgb { r: digit(0, 1) * 17, g: digit(1, 1) * 17, b: digit(2, 1) * 17 }),
        6 => Ok(Rgb { r: digit(0, 2), g: digit(1, 2), b: digit(2, 2) }),
        _ => Err(bad()),
    }
}

/// `N=COLOR`, N in 0–255, in decimal or with a `0x`, `0o` or `0b` prefix.
/// @see Ghostty `src/terminal/color.zig`, `parsePaletteEntry`
fn palette_entry(value: &str) -> Result<(u8, Rgb), String> {
    let (i, c) = value.split_once('=').ok_or_else(|| format!("expected N=COLOR: {value:?}"))?;
    let i = i.trim_matches(BLANK);
    let index = match i.get(..2) {
        Some("0x" | "0X") => u8::from_str_radix(&i[2..], 16),
        Some("0o" | "0O") => u8::from_str_radix(&i[2..], 8),
        Some("0b" | "0B") => u8::from_str_radix(&i[2..], 2),
        _ => i.parse::<u8>(),
    };
    let index = index.map_err(|_| format!("not an index 0-255: {i:?}"))?;
    Ok((index, color(c.trim_matches(BLANK))?))
}

/// `N` for both sides or `START,END`, in whole points.
/// @see Ghostty `src/config/Config.zig`, `WindowPadding.parseCLI`
fn padding(value: &str) -> Result<Padding, String> {
    let points = |s: &str| s.trim_matches(BLANK).parse::<u32>().map(|p| p as f32);
    let bad = |_| format!("expected N or N,N: {value:?}");
    match value.split_once(',') {
        Some((a, b)) => Ok(Padding { start: points(a).map_err(bad)?, end: points(b).map_err(bad)? }),
        None => {
            let p = points(value).map_err(bad)?;
            Ok(Padding { start: p, end: p })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(hex: u32) -> Rgb {
        Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
    }

    #[test]
    fn reads_fonts_colors_palette_and_padding() {
        let mut c = Config::default();
        let notes = c.read(
            "# ── 字体 ──\n\
             font-family = \"Cascadia Code NF\"\n\
             font-size = 12\n\
             \n\
             window-padding-x = 8 \n\
             window-padding-y = 6,2\n\
             palette = 0=#21252b\n\
             palette = 12=61afef\n\
             background = #21252b\n\
             foreground = #abb2bf\n\
             cursor-color = #abb2bf\n",
        );
        assert_eq!(notes, Notes::default());
        let want = Config {
            font_family: vec!["Cascadia Code NF".into()],
            font_size: 12.0,
            background: rgb(0x21252b),
            foreground: rgb(0xabb2bf),
            cursor_color: Some(rgb(0xabb2bf)),
            palette: BTreeMap::from([(0, rgb(0x21252b)), (12, rgb(0x61afef))]),
            padding_x: Padding { start: 8.0, end: 8.0 },
            padding_y: Padding { start: 6.0, end: 2.0 },
        };
        assert_eq!(c, want);
    }

    #[test]
    fn a_bad_line_is_reported_and_keeps_the_earlier_value() {
        let mut c = Config::default();
        let notes = c.read(
            "background = #102030\n\
             background = slategray\n\
             font-size = big\n\
             palette = 256=#000000\n\
             window-padding-x = 8,\n\
             window-padding-y = wide\n\
             just some words\n",
        );
        assert_eq!(c.background, rgb(0x102030));
        assert_eq!(c.font_size, 13.0);
        assert!(c.palette.is_empty());
        assert_eq!((c.padding_x, c.padding_y), (Config::default().padding_x, Config::default().padding_y));
        let lines: Vec<&str> = notes.errors.iter().map(|e| e.split(':').next().unwrap()).collect();
        assert_eq!(lines, ["line 2", "line 3", "line 4", "line 5", "line 6", "line 7"]);
    }

    #[test]
    fn keys_frith_does_not_use_are_listed_once() {
        let notes = Config::default().read("theme = nord\nbackground-opacity = 0.85\ntheme = dark\n");
        assert_eq!(notes.ignored, ["theme", "background-opacity"]);
        assert!(notes.errors.is_empty());
    }

    #[test]
    fn an_empty_value_restores_the_default_and_repeats_append() {
        let mut c = Config::default();
        c.read("font-family = A\nfont-family = B\npalette = 1=#fff\nfont-size = 20\n");
        assert_eq!(c.font_family, ["A", "B"]);
        assert_eq!(c.palette, BTreeMap::from([(1, rgb(0xffffff))]));
        // A second file applies on top, as Ghostty loads its XDG then App Support files.
        c.read("font-family =\nfont-family = C\npalette =\nfont-size =\n");
        assert_eq!(c.font_family, ["C"]);
        assert!(c.palette.is_empty());
        assert_eq!(c.font_size, 13.0);
    }

    #[test]
    fn defaults_and_limits_follow_ghostty() {
        let d = Config::default();
        assert_eq!((d.font_size, d.padding_x, d.padding_y), (13.0, Padding { start: 2.0, end: 2.0 }, d.padding_x));
        // Without these the terminal draws black: libghostty-vt leaves the defaults to its
        // caller (src/terminal/render.zig, `bg_fg`).
        assert_eq!((d.background, d.foreground), (rgb(0x282c34), rgb(0xffffff)));
        let mut c = Config::default();
        c.read("background = #000\nforeground = #000\nbackground =\nforeground =\n");
        assert_eq!((c.background, c.foreground), (d.background, d.foreground), "an empty value restores them");
        let mut c = Config::default();
        c.read("font-size = 0.5\n");
        assert_eq!(c.font_size, 1.0);
        c.read("font-size = 1000\n");
        assert_eq!(c.font_size, 255.0);
    }

    #[test]
    fn palette_indices_take_ghosttys_number_prefixes() {
        let mut c = Config::default();
        let notes = c.read("palette = 0x0f=#000\npalette = 0b10=#000\npalette = 0o7=#000\n");
        assert!(notes.errors.is_empty(), "{:?}", notes.errors);
        assert_eq!(c.palette.keys().copied().collect::<Vec<_>>(), [2, 7, 15]);
    }
}
