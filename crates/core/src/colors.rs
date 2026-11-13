//! The chrome's colors, derived from the terminal's own and the configured accent (ADR 14),
//! so the window and its panes read as one surface under any theme.
//!
//! Answers: what color is each piece of chrome, given the terminal's colors and the accent?
//! Must not know: how or where chrome is painted, fonts, or terminals.
//!
//! Surfaces stay a few steps from the terminal background: the bars recede (darker), headers
//! lift toward the foreground. Text keeps at least 4.5:1 contrast on every surface it sits on
//! (WCAG 1.4.3) and the accent at least 3:1 (WCAG 1.4.11), moving toward black or white
//! where the given colors fall short.
//! @see https://www.w3.org/TR/WCAG22/#contrast-minimum
//! @see https://www.w3.org/TR/WCAG22/#non-text-contrast

use crate::config::Rgb;

/// A color drawn over what is beneath it; `alpha` 255 is opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tint {
    pub rgb: Rgb,
    pub alpha: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chrome {
    /// Top bar, dock bar, sidebar, and whatever no pane covers.
    pub bar: Rgb,
    pub header: Rgb,
    pub header_focused: Rgb,
    /// Lines between tiles.
    pub line: Rgb,
    /// What floats above panes while dragging: dock guides and the drop label.
    pub raised: Rgb,
    pub text: Rgb,
    /// Secondary text: unfocused titles, hints.
    pub text_dim: Rgb,
    /// Focus, active guides, drop previews.
    pub accent: Rgb,
    /// Ink on an accent fill.
    pub on_accent: Rgb,
    /// A pane asking for the user: a dot in its header or dock chip.
    pub attention: Rgb,
    pub hover: Tint,
    /// A dock chip at rest.
    pub chip: Tint,
    pub card_border: Tint,
    pub shadow: Tint,
    pub preview: Tint,
    /// The background is the brighter of the terminal's two colors.
    pub light: bool,
}

const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };
const WHITE: Rgb = Rgb { r: 255, g: 255, b: 255 };

impl Chrome {
    /// The chrome for a terminal drawn in `fg` on `bg`, with the configured `accent` and
    /// `attention` (palette 3, yellow) from its palette.
    pub fn derive(bg: Rgb, fg: Rgb, accent: Rgb, attention: Rgb) -> Chrome {
        // A light theme has the brighter background; equal ones count as dark.
        let light = luminance(bg) > luminance(fg);
        // With plenty of contrast, bars recede (darker) and headers lift toward the
        // foreground. Otherwise every surface moves away from where text will end up, so
        // even a mid-gray theme keeps room for 4.5:1.
        let (bar, lift) = if contrast(fg, bg) >= 7.0 {
            (mix(bg, BLACK, if light { 0.05 } else { 0.2 }), fg)
        } else {
            let away = if extreme(&[bg]) == WHITE { BLACK } else { WHITE };
            (mix(bg, away, 0.1), away)
        };
        let header = mix(bg, lift, 0.05);
        let header_focused = mix(bg, lift, 0.1);
        // Dividers and floating guides must show against the panes; where the lift direction
        // has no room left (a near-white background lifting toward white), they go the
        // other way.
        let other = if luminance(lift) > luminance(bg) { BLACK } else { WHITE };
        let distinct = |t: f32| {
            let on = [bg, header_focused];
            let ok = |c: Rgb| on.iter().all(|&s| contrast(c, s) >= 1.3);
            Some(toward(mix(bg, lift, t), lift, &on, 1.3))
                .filter(|&c| ok(c))
                .unwrap_or_else(|| toward(mix(bg, other, t), other, &on, 1.3))
        };
        let raised = distinct(0.2);
        let line = distinct(0.15);
        let (hover, chip) = (31, 20);
        // Text sits on the surfaces and on dock chips, resting or hovered, which tint the bar
        // the way headers lift: toward the text in ordinary themes, away from it otherwise.
        // Where that direction has no room (a near-black bar darkening), the other way.
        let tint = if contrast(over(bar, lift, chip), bar) >= 1.05 { lift } else { other };
        let chip_at = over(bar, tint, chip);
        let hovered = over(bar, tint, hover);
        let text = legible(fg, &[bar, bg, header, header_focused, raised, chip_at, hovered], 4.5);
        // Dim text (unfocused titles, hints) sits only on bars, headers and the background;
        // it brightens toward the text, never past it.
        let text_dim = toward(mix(text, bg, 0.4), text, &[bar, bg, header, header_focused], 4.5);
        let marks = [bar, bg, header, header_focused, raised];
        let accent = legible(accent, &marks, 3.0);
        let attention =
            legible(attention, &[header, header_focused, over(bar, text, chip), over(bar, text, hover)], 3.0);
        Chrome {
            bar,
            header,
            header_focused,
            line,
            raised,
            text,
            text_dim,
            accent,
            on_accent: extreme(&[accent]),
            attention,
            hover: Tint { rgb: tint, alpha: hover },
            chip: Tint { rgb: tint, alpha: chip },
            card_border: Tint { rgb: text, alpha: 46 },
            shadow: Tint { rgb: BLACK, alpha: if light { 60 } else { 150 } },
            preview: Tint { rgb: accent, alpha: 64 },
            light,
        }
    }
}

/// What `tint` at `alpha` over `base` looks like, blended in sRGB as Skia blends on an
/// untagged surface.
pub fn over(base: Rgb, tint: Rgb, alpha: u8) -> Rgb {
    mix(base, tint, f32::from(alpha) / 255.0)
}

/// `c` moved toward black or white, whichever stands out more, just enough to reach `min`
/// contrast on every one of `on`.
fn legible(c: Rgb, on: &[Rgb], min: f32) -> Rgb {
    toward(c, extreme(on), on, min)
}

/// `c` moved toward `target` just enough to reach `min` contrast on every one of `on`, or
/// `target` itself when nothing short of it does.
fn toward(c: Rgb, target: Rgb, on: &[Rgb], min: f32) -> Rgb {
    (0..=20)
        .map(|i| mix(c, target, i as f32 / 20.0))
        .find(|&m| on.iter().all(|&s| contrast(m, s) >= min))
        .unwrap_or(target)
}

/// Black or white, whichever has the higher worst contrast against `on`.
fn extreme(on: &[Rgb]) -> Rgb {
    let worst = |c: Rgb| on.iter().map(|&s| contrast(c, s)).fold(f32::INFINITY, f32::min);
    if worst(WHITE) >= worst(BLACK) { WHITE } else { BLACK }
}

/// `a` moved `t` of the way to `b`, per sRGB channel.
pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let ch = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgb { r: ch(a.r, b.r), g: ch(a.g, b.g), b: ch(a.b, b.b) }
}

/// WCAG relative luminance. @see https://www.w3.org/TR/WCAG22/#dfn-relative-luminance
pub fn luminance(c: Rgb) -> f32 {
    let lin = |v: u8| {
        let s = f32::from(v) / 255.0;
        if s <= 0.04045 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG contrast ratio, 1 to 21. @see https://www.w3.org/TR/WCAG22/#dfn-contrast-ratio
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::rgb;

    /// (name, background, foreground, accent, palette 3); themes other than Frith's take
    /// their palette 4 as the accent, as a user matching one would.
    const THEMES: [(&str, u32, u32, u32, u32); 11] = [
        ("Frith's defaults", 0x0a0a0a, 0xeeeee6, 0xf6763e, 0xf0c674),
        ("the user's One Dark", 0x21252b, 0xabb2bf, 0x61afef, 0xe5c07b),
        ("Ghostty's defaults", 0x282c34, 0xffffff, 0x81a2be, 0xf0c674),
        ("One Light", 0xfafafa, 0x383a42, 0x4078f2, 0xc18401),
        ("Solarized Dark", 0x002b36, 0x839496, 0x268bd2, 0xb58900),
        ("Solarized Light", 0xfdf6e3, 0x657b83, 0x268bd2, 0xb58900),
        ("black on white", 0xffffff, 0x000000, 0x0000ff, 0xffff00),
        ("white on black", 0x000000, 0xffffff, 0x0000ff, 0xffff00),
        ("gray on gray", 0x777777, 0x808080, 0x777777, 0x777777),
        ("black on mid gray", 0x777777, 0x000000, 0x0000aa, 0xaaaa00),
        ("white on mid gray", 0x767676, 0xffffff, 0x0000aa, 0xaaaa00),
    ];

    fn each_with_fg(f: impl Fn(&str, Rgb, Rgb, Chrome)) {
        for (name, bg, fg, accent, attention) in THEMES {
            f(name, rgb(bg), rgb(fg), Chrome::derive(rgb(bg), rgb(fg), rgb(accent), rgb(attention)));
        }
    }

    #[test]
    fn contrast_matches_wcags_reference_values() {
        assert!((contrast(BLACK, WHITE) - 21.0).abs() < 0.01);
        // Colored: each channel weighs differently (0.2126 R, 0.7152 G, 0.0722 B).
        assert!((contrast(rgb(0xff0000), WHITE) - 4.0).abs() < 0.01, "pure red on white");
        assert!((contrast(rgb(0x0000ff), WHITE) - 8.59).abs() < 0.01, "pure blue on white");
        assert!((contrast(rgb(0x777777), WHITE) - 4.48).abs() < 0.01, "WCAG's #777 on white");
        assert_eq!(contrast(rgb(0x123456), rgb(0x123456)), 1.0);
    }

    #[test]
    fn text_stays_legible_on_every_chrome_surface_in_every_theme() {
        each_with_fg(|name, bg, fg, c| {
            let chip = over(c.bar, c.chip.rgb, c.chip.alpha);
            let hovered = over(c.bar, c.hover.rgb, c.hover.alpha);
            let surfaces = [
                ("bar", c.bar),
                ("header", c.header),
                ("focused header", c.header_focused),
                ("raised", c.raised),
                ("background", bg),
                ("dock chip", chip),
                ("hovered dock chip", hovered),
            ];
            for (on, s) in surfaces {
                let r = contrast(c.text, s);
                assert!(r >= 4.5, "{name}: text on the {on} is {r:.2}:1");
            }
            for (on, s) in &surfaces[..5] {
                if *on != "raised" {
                    let r = contrast(c.text_dim, *s);
                    assert!(r >= 4.5, "{name}: dim text on the {on} is {r:.2}:1");
                }
            }
            // A mid-gray theme has no room for dimmer text that still reads at 4.5:1.
            assert!(contrast(c.text_dim, bg) <= contrast(c.text, bg), "{name}: dim text is no brighter");
            if contrast(fg, bg) >= 7.0 {
                assert!(contrast(c.text_dim, bg) < contrast(c.text, bg), "{name}: dim text is dimmer");
            }
            assert!(contrast(c.on_accent, c.accent) >= 3.0, "{name}: ink on the accent");
            for (on, s) in [("header", c.header), ("focused header", c.header_focused), ("dock chip", chip)] {
                let r = contrast(c.attention, s);
                assert!(r >= 3.0, "{name}: an attention dot on the {on} is {r:.2}:1");
            }
        });
    }

    #[test]
    fn the_accent_stands_out_wherever_it_is_drawn_and_keeps_the_configured_hue_where_it_can() {
        let orange = rgb(0xf6763e);
        // Each theme with its own accent, as a user matching it would set, and with the default.
        let cases = THEMES.iter().flat_map(|&(name, bg, fg, accent, attention)| {
            [(name, rgb(accent)), (name, orange)]
                .map(|(name, a)| (name, rgb(bg), Chrome::derive(rgb(bg), rgb(fg), a, rgb(attention))))
        });
        for (name, bg, c) in cases {
            // Focus lines and borders, the cursor, lit dividers, drop outlines, guide icons.
            for (on, s) in [
                ("header", c.header),
                ("focused header", c.header_focused),
                ("bar", c.bar),
                ("background", bg),
                ("raised", c.raised),
            ] {
                let r = contrast(c.accent, s);
                assert!(r >= 3.0, "{name}: accent on the {on} is {r:.2}:1");
            }
        }
        let c = Chrome::derive(rgb(0x21252b), rgb(0xabb2bf), rgb(0x61afef), rgb(0xe5c07b));
        assert_eq!(c.accent, rgb(0x61afef), "already legible: unchanged");
        let c = Chrome::derive(rgb(0x0a0a0a), rgb(0xeeeee6), orange, rgb(0xf0c674));
        assert_eq!(c.accent, orange, "Frith's defaults show the accent as configured");
    }

    #[test]
    fn surfaces_stay_close_to_the_background_bars_recede_and_focus_lifts() {
        each_with_fg(|name, bg, fg, c| {
            for (what, s) in [("bar", c.bar), ("header", c.header), ("focused header", c.header_focused)] {
                let r = contrast(s, bg);
                assert!(r < 1.6, "{name}: {what} is {r:.2}:1 from the background");
            }
            let away = |s: Rgb| contrast(s, bg);
            assert!(away(c.header_focused) > away(c.header), "{name}: focus lifts the header further");
            if contrast(fg, bg) >= 7.0 {
                assert!(luminance(c.bar) <= luminance(bg), "{name}: bars are no lighter than the terminal");
            }
            for (on, s) in [("background", bg), ("focused header", c.header_focused)] {
                let r = contrast(c.line, s);
                assert!(r >= 1.3, "{name}: a divider on the {on} is {r:.2}:1");
                let r = contrast(c.raised, s);
                assert!(r >= 1.25, "{name}: a dock guide on the {on} is {r:.2}:1");
            }
            let chip = over(c.bar, c.chip.rgb, c.chip.alpha);
            let r = contrast(chip, c.bar);
            assert!((1.05..1.6).contains(&r), "{name}: a resting dock chip is a quiet {r:.2}:1 on the bar");
        });
    }

    #[test]
    fn a_light_theme_gets_light_chrome_and_a_dark_one_dark_chrome() {
        let light = Chrome::derive(rgb(0xfafafa), rgb(0x383a42), rgb(0x4078f2), rgb(0xc18401));
        let dark = Chrome::derive(rgb(0x21252b), rgb(0xabb2bf), rgb(0x61afef), rgb(0xe5c07b));
        assert!(luminance(light.header) > 0.7 && luminance(dark.header) < 0.05);
        assert!(luminance(light.text) < luminance(light.header));
        assert!(light.shadow.alpha < dark.shadow.alpha, "a softer shadow on light chrome");
        assert!(light.light && !dark.light, "light adds onto a dark theme only (ADR 10)");
    }
}
