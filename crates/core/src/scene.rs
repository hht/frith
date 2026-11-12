//! Window chrome geometry: the top bar, the sidebar, each tile's header and content, the
//! dividers, the floating cards, and the dock bar of minimized panes.
//!
//! Answers: where does each piece of chrome sit, and what is under a given point?
//! Must not know: how chrome is painted, terminals, time, or window-system events. The
//! layout decides the arrangement; this module only dresses it.

use crate::geom::{Point, Rect};
use crate::layout::{Axis, Divider, Layout, PaneId};

/// Chrome sizes in the caller's units (the app passes physical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// Height of the top bar holding the window controls and app buttons.
    pub top: f32,
    pub sidebar: f32,
    /// Height of the dock bar at the bottom.
    pub bar: f32,
    /// Line between neighboring tiles.
    pub gap: f32,
    pub header: f32,
    pub button: f32,
    /// Width the system window controls (traffic lights) take at the left of the top bar.
    pub controls: f32,
    /// Inset of top bar buttons and dock chips from the window edge.
    pub pad: f32,
    pub chip: f32,
    /// Width of the band around a divider that grabs it.
    pub divider_grab: f32,
    /// Width of the band inside a card's border that resizes it.
    pub resize_grab: f32,
}

impl Metrics {
    /// Metrics for a display with `scale` physical pixels per logical pixel.
    pub fn for_scale(scale: f32) -> Metrics {
        let px = |logical: f32| (logical * scale).round();
        Metrics {
            top: px(30.0),
            sidebar: px(220.0),
            bar: px(30.0),
            gap: px(1.0),
            header: px(24.0),
            button: px(18.0),
            controls: px(78.0),
            pad: px(8.0),
            chip: px(160.0),
            divider_grab: px(8.0),
            resize_grab: px(6.0),
        }
    }
}

/// Everything on screen for the root window at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub top: Rect,
    pub sidebar: Option<Rect>,
    /// Where tiles go.
    pub area: Rect,
    pub tiles: Vec<Frame>,
    pub bar: Rect,
    /// Top bar buttons.
    pub chrome: Vec<(ChromeButton, Rect)>,
    pub dividers: Vec<Divider>,
    /// Floating panes, bottom to top.
    pub cards: Vec<Frame>,
    /// Minimized panes in the dock bar, left to right.
    pub chips: Vec<(PaneId, Rect)>,
    pub metrics: Metrics,
}

/// Which borders of a card a resize drag moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Resize {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

/// One tile or card: its whole frame, the header strip, and the terminal content.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub pane: PaneId,
    pub outer: Rect,
    pub header: Rect,
    pub content: Rect,
    /// Header buttons, left to right.
    pub buttons: Vec<(PaneButton, Rect)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneButton {
    SplitRight,
    SplitDown,
    Float,
    Zoom,
    Close,
    Minimize,
    Embed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChromeButton {
    Sidebar,
    Launcher,
}

/// What a point is over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    /// Empty top bar: drags the window.
    DragWindow,
    Chrome(ChromeButton),
    Button(PaneId, PaneButton),
    Resize(PaneId, Resize),
    Divider(usize),
    Chip(PaneId),
    Header(PaneId),
    Content(PaneId),
    Nothing,
}

pub fn scene(layout: &Layout, width: f32, height: f32, sidebar: bool, m: &Metrics) -> Scene {
    let top = Rect::new(0.0, 0.0, width, m.top);
    let bar = Rect::new(0.0, height - m.bar, width, m.bar);
    let body = Rect::new(0.0, top.bottom(), width, (bar.y - top.bottom()).max(0.0));
    let (sidebar, area) = if sidebar {
        let w = m.sidebar.min(body.w);
        (Some(Rect::new(body.x, body.y, w, body.h)), Rect::new(body.x + w, body.y, body.w - w, body.h))
    } else {
        (None, body)
    };
    let tiles = layout.tiles(area, m.gap).into_iter().map(|(pane, outer)| tile(pane, outer, m)).collect();
    let y = top.y + ((top.h - m.button) / 2.0).round();
    let chrome = vec![
        (ChromeButton::Sidebar, Rect::new(top.x + m.controls, y, m.button, m.button)),
        (ChromeButton::Launcher, Rect::new(top.right() - m.pad - m.button, y, m.button, m.button)),
    ];
    let cards = layout.floats(area).into_iter().map(|(pane, outer)| card(pane, outer, m)).collect();
    let inset = ((bar.h - m.button) / 2.0).round();
    let chips = (layout.docked().into_iter().enumerate())
        .map(|(i, pane)| {
            (pane, Rect::new(bar.x + m.pad + i as f32 * (m.chip + inset), bar.y + inset, m.chip, m.button))
        })
        .take_while(|(_, r)| r.right() <= bar.right() - m.pad)
        .collect();
    Scene { top, sidebar, area, tiles, bar, chrome, dividers: layout.dividers(area, m.gap), cards, chips, metrics: *m }
}

const CARD_BUTTONS: [PaneButton; 3] = [PaneButton::Minimize, PaneButton::Embed, PaneButton::Close];

/// The frame of a floating card whose whole rect is `outer`.
pub fn card(pane: PaneId, outer: Rect, m: &Metrics) -> Frame {
    frame(pane, outer, &CARD_BUTTONS, m)
}

const TILE_BUTTONS: [PaneButton; 5] =
    [PaneButton::SplitRight, PaneButton::SplitDown, PaneButton::Float, PaneButton::Zoom, PaneButton::Close];

/// The frame of a tile whose whole rect is `outer`.
pub fn tile(pane: PaneId, outer: Rect, m: &Metrics) -> Frame {
    frame(pane, outer, &TILE_BUTTONS, m)
}

/// Splits `outer` into header and content and lays `buttons` out right-aligned in the header.
fn frame(pane: PaneId, outer: Rect, buttons: &[PaneButton], m: &Metrics) -> Frame {
    let header = Rect::new(outer.x, outer.y, outer.w, m.header.min(outer.h));
    let content = Rect::new(outer.x, header.bottom(), outer.w, outer.bottom() - header.bottom());
    let inset = ((m.header - m.button) / 2.0).round();
    let n = buttons.len() as f32;
    let buttons = buttons
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let from_right = n - i as f32; // the last button is 1 slot from the right
            let x = header.right() - from_right * (m.button + inset);
            (*b, Rect::new(x, header.y + inset, m.button, m.button))
        })
        .collect();
    Frame { pane, outer, header, content, buttons }
}

impl Scene {
    pub fn hit(&self, p: Point) -> Hit {
        let m = &self.metrics;
        if self.top.contains(p) {
            let on = self.chrome.iter().find(|(_, r)| r.contains(p));
            return on.map_or(Hit::DragWindow, |(b, _)| Hit::Chrome(*b));
        }
        for f in self.cards.iter().rev() {
            if !f.outer.contains(p) {
                continue;
            }
            let g = m.resize_grab;
            let o = f.outer;
            let resize = Resize {
                left: p.x < o.x + g,
                right: p.x >= o.right() - g,
                top: p.y < o.y + g,
                bottom: p.y >= o.bottom() - g,
            };
            if resize != Resize::default() {
                return Hit::Resize(f.pane, resize);
            }
            return f.hit(p);
        }
        if let Some((pane, _)) = self.chips.iter().find(|(_, r)| r.contains(p)) {
            return Hit::Chip(*pane);
        }
        for d in &self.dividers {
            let c = d.rect.center();
            let half = m.divider_grab / 2.0;
            let grab = match d.axis {
                Axis::Horizontal => Rect::new(c.x - half, d.rect.y, m.divider_grab, d.rect.h),
                Axis::Vertical => Rect::new(d.rect.x, c.y - half, d.rect.w, m.divider_grab),
            };
            if grab.contains(p) {
                return Hit::Divider(d.index);
            }
        }
        for f in &self.tiles {
            if f.outer.contains(p) {
                return f.hit(p);
            }
        }
        Hit::Nothing
    }
}

impl Frame {
    /// What `p`, inside this frame, is over.
    fn hit(&self, p: Point) -> Hit {
        if let Some((b, _)) = self.buttons.iter().find(|(_, r)| r.contains(p)) {
            return Hit::Button(self.pane, *b);
        }
        if self.header.contains(p) { Hit::Header(self.pane) } else { Hit::Content(self.pane) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Edge;

    const A: PaneId = PaneId(1);
    const B: PaneId = PaneId(2);
    const C: PaneId = PaneId(3);
    const D: PaneId = PaneId(4);

    fn m() -> Metrics {
        Metrics::for_scale(1.0)
    }

    fn one() -> Layout {
        let mut l = Layout::default();
        l.split_window(Edge::Right, A);
        l
    }

    #[test]
    fn tile_buttons_sit_right_aligned_in_the_header_in_a_fixed_order() {
        let s = scene(&one(), 800.0, 600.0, false, &m());
        let f = &s.tiles[0];
        let order: Vec<PaneButton> = f.buttons.iter().map(|b| b.0).collect();
        use PaneButton::*;
        assert_eq!(order, vec![SplitRight, SplitDown, Float, Zoom, Close]);
        let close = f.buttons[4].1;
        assert_eq!(close.right(), f.header.right() - 3.0);
        assert!(f.buttons.windows(2).all(|w| w[0].1.right() < w[1].1.x), "left to right, no overlap");
        assert_eq!(s.hit(close.center()), Hit::Button(A, Close));
        assert_eq!(s.hit(f.buttons[0].1.center()), Hit::Button(A, SplitRight));
    }

    #[test]
    fn the_top_bar_has_a_sidebar_toggle_after_the_window_controls_and_a_launcher_at_the_right() {
        let s = scene(&one(), 800.0, 600.0, false, &m());
        let (toggle, launcher) = (s.chrome[0], s.chrome[1]);
        assert_eq!(toggle.0, ChromeButton::Sidebar);
        assert!(toggle.1.x >= 78.0, "clear of the traffic lights");
        assert_eq!(launcher.0, ChromeButton::Launcher);
        assert_eq!(launcher.1.right(), 800.0 - 8.0);
        assert_eq!(s.hit(toggle.1.center()), Hit::Chrome(ChromeButton::Sidebar));
        assert_eq!(s.hit(launcher.1.center()), Hit::Chrome(ChromeButton::Launcher));
    }

    /// A | B tiles, C floating over both, D minimized.
    fn busy() -> Layout {
        let mut l = one();
        l.split(A, Edge::Right, B);
        l.split(B, Edge::Bottom, C);
        l.split(B, Edge::Bottom, D);
        l.float(C, Rect::new(300.0, 200.0, 200.0, 150.0));
        l.float(D, Rect::new(0.0, 0.0, 100.0, 100.0));
        l.minimize(D);
        l
    }

    #[test]
    fn cards_sit_above_tiles_and_take_clicks_first() {
        let s = scene(&busy(), 800.0, 600.0, false, &m());
        let card = &s.cards[0];
        assert_eq!(card.outer, Rect::new(300.0, 200.0, 200.0, 150.0));
        let order: Vec<PaneButton> = card.buttons.iter().map(|b| b.0).collect();
        assert_eq!(order, vec![PaneButton::Minimize, PaneButton::Embed, PaneButton::Close]);
        assert_eq!(s.hit(Point::new(400.0, 300.0)), Hit::Content(C), "over tile A and B, but C is on top");
        assert_eq!(s.hit(Point::new(400.0, 210.0)), Hit::Header(C));
        assert_eq!(s.hit(card.buttons[0].1.center()), Hit::Button(C, PaneButton::Minimize));
    }

    #[test]
    fn card_borders_resize_and_corners_resize_two_ways() {
        let s = scene(&busy(), 800.0, 600.0, false, &m());
        let both = Resize { left: false, right: true, top: false, bottom: true };
        assert_eq!(s.hit(Point::new(498.0, 348.0)), Hit::Resize(C, both));
        let left = Resize { left: true, right: false, top: false, bottom: false };
        assert_eq!(s.hit(Point::new(301.0, 300.0)), Hit::Resize(C, left));
    }

    #[test]
    fn minimized_panes_show_as_chips_in_the_dock_bar() {
        let s = scene(&busy(), 800.0, 600.0, false, &m());
        let chip = s.chips[0];
        assert_eq!(chip.0, D);
        assert!(s.bar.contains(chip.1.center()));
        assert_eq!(s.hit(chip.1.center()), Hit::Chip(D));
    }

    #[test]
    fn a_divider_grabs_a_band_wider_than_its_line() {
        let s = scene(&busy(), 800.0, 600.0, false, &m());
        let line = s.dividers[0].rect;
        let x = line.center().x;
        assert_eq!(s.hit(Point::new(x, 100.0)), Hit::Divider(0));
        assert_eq!(s.hit(Point::new(x - 3.5, 100.0)), Hit::Divider(0), "inside the tile edge");
        assert_eq!(s.hit(Point::new(x - 6.0, 100.0)), Hit::Content(A));
    }

    #[test]
    fn a_single_tile_fills_the_space_between_top_bar_and_dock_bar() {
        let s = scene(&one(), 800.0, 600.0, false, &m());
        assert_eq!(s.area, Rect::new(0.0, 30.0, 800.0, 540.0));
        assert_eq!(s.tiles[0].header, Rect::new(0.0, 30.0, 800.0, 24.0));
        assert_eq!(s.tiles[0].content, Rect::new(0.0, 54.0, 800.0, 516.0));
        assert_eq!(s.hit(Point::new(400.0, 10.0)), Hit::DragWindow);
        assert_eq!(s.hit(Point::new(400.0, 40.0)), Hit::Header(A));
        assert_eq!(s.hit(Point::new(400.0, 300.0)), Hit::Content(A));
        assert_eq!(s.hit(Point::new(400.0, 590.0)), Hit::Nothing, "empty dock bar");
    }
}
