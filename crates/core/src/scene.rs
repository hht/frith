//! Window chrome geometry: the top bar, the sidebar, each tile's header and content, the
//! dividers, the floating cards, and the dock bar of minimized panes.
//!
//! Answers: where does each piece of chrome sit, and what is under a given point?
//! Must not know: how chrome is painted, terminals, time, or window-system events. The
//! layout decides the arrangement; this module only dresses it.

use crate::geom::{Point, Rect};
use crate::layout::{Axis, Divider, Drop, Edge, Layout, PaneId};

/// Chrome sizes in the caller's units (the app passes physical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// Height of the top bar holding the window controls and app buttons.
    pub top: f32,
    pub sidebar: f32,
    /// Height of the dock bar at the bottom, while it holds a minimized pane.
    pub bar: f32,
    /// Line between neighboring tiles.
    pub gap: f32,
    pub header: f32,
    /// Side of a header button, small enough to leave the header's top and bottom edges to
    /// divider and resize bands.
    pub button: f32,
    /// The smallest hit target (WCAG 2.5.8): top bar buttons and dock chips are this big;
    /// header buttons, smaller, sit this far apart, which the criterion's spacing exception
    /// accepts. @see https://www.w3.org/TR/WCAG22/#target-size-minimum
    pub target: f32,
    /// Width the system window controls (traffic lights) take at the left of the top bar.
    pub controls: f32,
    /// Inset of top bar buttons and dock chips from the window edge.
    pub pad: f32,
    pub chip: f32,
    /// Width of the band around a divider that grabs it.
    pub divider_grab: f32,
    /// Width of the band inside a card's border that resizes it.
    pub resize_grab: f32,
    /// Side of a square dock guide.
    pub guide: f32,
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
            target: px(24.0),
            controls: px(78.0),
            pad: px(8.0),
            chip: px(160.0),
            divider_grab: px(8.0),
            resize_grab: px(6.0),
            guide: px(32.0),
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

/// Dock guides while a pane is dragged (ADR 6).
#[derive(Clone, Debug, PartialEq)]
pub struct Guides {
    /// The tile whose guides are shown, drawn highlighted.
    pub target: Option<PaneId>,
    /// Each guide and the drop it picks.
    pub buttons: Vec<(Drop, Rect)>,
    /// The drop of the guide under the pointer; `None` means no dock.
    pub active: Option<Drop>,
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
    // The dock bar takes room only while it holds a minimized pane (ADR 14).
    let docked = layout.docked();
    let bar_h = if docked.is_empty() { 0.0 } else { m.bar };
    let bar = Rect::new(0.0, height - bar_h, width, bar_h);
    let body = Rect::new(0.0, top.bottom(), width, (bar.y - top.bottom()).max(0.0));
    let (sidebar, area) = if sidebar {
        let w = m.sidebar.min(body.w);
        (Some(Rect::new(body.x, body.y, w, body.h)), Rect::new(body.x + w, body.y, body.w - w, body.h))
    } else {
        (None, body)
    };
    let tiles = layout.tiles(area, m.gap).into_iter().map(|(pane, outer)| tile(pane, outer, m)).collect();
    let (t, y) = (m.target, top.y + ((top.h - m.target) / 2.0).round());
    let chrome = vec![
        (ChromeButton::Sidebar, Rect::new(top.x + m.controls, y, t, t)),
        (ChromeButton::Launcher, Rect::new(top.right() - m.pad - t, y, t, t)),
    ];
    let cards = layout.floats(area).into_iter().map(|(pane, outer)| card(pane, outer, m)).collect();
    let (gap, y) = (((bar.h - m.button) / 2.0).round(), bar.y + ((bar.h - m.target) / 2.0).round());
    let chips = (docked.into_iter().enumerate())
        .map(|(i, pane)| (pane, Rect::new(bar.x + m.pad + i as f32 * (m.chip + gap), y, m.chip, m.target)))
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

/// Splits `outer` into header and content and lays `buttons` out right-aligned in the header,
/// dropping those that do not fit whole, leftmost first, so Close survives longest.
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
            let x = header.right() - inset - m.button - (from_right - 1.0) * m.target;
            (*b, Rect::new(x, header.y + inset, m.button, m.button))
        })
        .filter(|(_, r)| r.x >= header.x && r.bottom() <= header.bottom())
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
            // Buttons sit inside the resize bands near the top-right corner; they win there.
            if let Some((b, _)) = f.buttons.iter().find(|(_, r)| r.contains(p)) {
                return Hit::Button(f.pane, *b);
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

    /// Dock guides for dragging `dragged` with the pointer at `p` (ADR 6): one inside the
    /// middle of each area edge for the whole window, and for the target tile one on each side
    /// of its center, plus a swap guide on it when `dragged` is tiled. Bottom to top.
    ///
    /// The target is `held` (the previous target) while `p` stays within its guides, which may
    /// reach past the tile and draw above every card; otherwise it is the tile under `p`
    /// unless another card hides it there. A tile's guides keep clear of the window guides so
    /// every guide stays reachable.
    pub fn guides(&self, layout: &Layout, dragged: PaneId, p: Point, held: Option<PaneId>) -> Guides {
        if layout.is_only_tile(dragged) {
            return Guides { target: None, buttons: Vec::new(), active: None };
        }
        let (a, s, pad) = (self.area, self.metrics.guide, self.metrics.pad);
        let (cx, cy) = (a.center().x - s / 2.0, a.center().y - s / 2.0);
        let mut buttons = vec![
            (Drop::Window(Edge::Left), Rect::new(a.x + pad, cy, s, s)),
            (Drop::Window(Edge::Right), Rect::new(a.right() - pad - s, cy, s, s)),
            (Drop::Window(Edge::Top), Rect::new(cx, a.y + pad, s, s)),
            (Drop::Window(Edge::Bottom), Rect::new(cx, a.bottom() - pad - s, s, s)),
        ];
        let compass = |f: &Frame| self.compass(f.outer.center(), &buttons);
        let candidates = self.tiles.iter().filter(|f| f.pane != dragged);
        let kept = candidates.clone().find(|f| Some(f.pane) == held && compass(f).contains(p));
        let target = kept.or_else(|| {
            let hidden = self.cards.iter().any(|c| c.pane != dragged && c.outer.contains(p));
            candidates.clone().find(|f| f.outer.contains(p)).filter(|_| !hidden)
        });
        if let Some(f) = target {
            let c = compass(f).center();
            let step = s + pad / 2.0;
            for (edge, dx, dy) in
                [(Edge::Left, -step, 0.0), (Edge::Right, step, 0.0), (Edge::Top, 0.0, -step), (Edge::Bottom, 0.0, step)]
            {
                buttons.push((Drop::Split(f.pane, edge), Rect::new(c.x + dx - s / 2.0, c.y + dy - s / 2.0, s, s)));
            }
            if layout.is_tiled(dragged) {
                buttons.push((Drop::Swap(f.pane), Rect::new(c.x - s / 2.0, c.y - s / 2.0, s, s)));
            }
        }
        // Tile guides draw above window guides and win where a tiny area leaves no room.
        let active = buttons.iter().rev().find(|(_, r)| r.contains(p)).map(|(d, _)| *d);
        Guides { target: target.map(|f| f.pane), buttons, active }
    }

    /// The square spanned by a tile's guides when centered on `center`, kept inside the area
    /// and then moved clear of the `window` guides by half a pad. An area under about three
    /// compasses wide or high leaves no room to clear guides on both sides; tile guides win
    /// there (see `guides`).
    fn compass(&self, center: Point, window: &[(Drop, Rect)]) -> Rect {
        let (s, pad, a) = (self.metrics.guide, self.metrics.pad, self.area);
        let side = 3.0 * s + pad;
        let x = (center.x - side / 2.0).min(a.right() - side).max(a.x);
        let y = (center.y - side / 2.0).min(a.bottom() - side).max(a.y);
        let mut r = Rect::new(x, y, side, side);
        let clear = pad / 2.0;
        for (drop, w) in window {
            let near = r.x < w.right() + clear
                && w.x - clear < r.right()
                && r.y < w.bottom() + clear
                && w.y - clear < r.bottom();
            if !near {
                continue;
            }
            match drop {
                Drop::Window(Edge::Left) => r.x = w.right() + clear,
                Drop::Window(Edge::Right) => r.x = w.x - clear - side,
                Drop::Window(Edge::Top) => r.y = w.bottom() + clear,
                Drop::Window(Edge::Bottom) => r.y = w.y - clear - side,
                _ => unreachable!("only window guides are passed"),
            }
        }
        r
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
    use crate::layout::{Drop, Edge};

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

    /// A | B tiles, C floating at the bottom left over A.
    fn two_and_a_card() -> Layout {
        let mut l = one();
        l.split(A, Edge::Right, B);
        l.split(A, Edge::Bottom, C);
        l.float(C, Rect::new(20.0, 400.0, 200.0, 100.0));
        l
    }

    fn center_of(s: &Scene, pane: PaneId) -> Point {
        s.tiles.iter().find(|f| f.pane == pane).unwrap().outer.center()
    }

    /// Guides are 32 px squares; the four around a tile's center sit 36 px from it.
    const STEP: f32 = 36.0;

    #[test]
    fn dragging_a_card_over_a_tile_targets_it_with_four_guides_that_each_pick_a_split() {
        let s = scene(&two_and_a_card(), 800.0, 600.0, false, &m());
        let c = center_of(&s, B);
        let at = |dx: f32, dy: f32| s.guides(&two_and_a_card(), C, Point::new(c.x + dx, c.y + dy), None);
        let g = at(100.0, 100.0);
        assert_eq!(g.target, Some(B));
        assert_eq!(g.active, None, "off the guides the card moves freely");
        assert_eq!(at(STEP, 0.0).active, Some(Drop::Split(B, Edge::Right)));
        assert_eq!(at(-STEP, 0.0).active, Some(Drop::Split(B, Edge::Left)));
        assert_eq!(at(0.0, -STEP).active, Some(Drop::Split(B, Edge::Top)));
        assert_eq!(at(0.0, STEP + 15.0).active, Some(Drop::Split(B, Edge::Bottom)), "anywhere on the guide");
        assert_eq!(at(0.0, 0.0).active, None, "no swap for a card");
    }

    #[test]
    fn dragging_a_tile_adds_a_center_guide_that_swaps_but_none_over_the_tile_itself() {
        let l = two_and_a_card();
        let s = scene(&l, 800.0, 600.0, false, &m());
        let c = center_of(&s, B);
        assert_eq!(s.guides(&l, A, c, None).active, Some(Drop::Swap(B)));
        assert_eq!(s.guides(&l, A, Point::new(c.x + STEP, c.y), None).active, Some(Drop::Split(B, Edge::Right)));
        let own = s.guides(&l, A, center_of(&s, A), None);
        assert_eq!((own.target, own.active), (None, None));
    }

    #[test]
    fn guides_at_the_middle_of_each_area_edge_dock_to_the_whole_window() {
        let l = two_and_a_card();
        let s = scene(&l, 800.0, 570.0, false, &m()); // area (0, 30, 800, 540)
        let at = |x, y| s.guides(&l, A, Point::new(x, y), None).active;
        assert_eq!(at(24.0, 300.0), Some(Drop::Window(Edge::Left)), "over A itself, still active");
        assert_eq!(at(776.0, 300.0), Some(Drop::Window(Edge::Right)));
        assert_eq!(at(400.0, 54.0), Some(Drop::Window(Edge::Top)));
        assert_eq!(at(400.0, 546.0), Some(Drop::Window(Edge::Bottom)));
        assert_eq!(at(4.0, 300.0), None, "inset from the edge, not a band along it");
        assert_eq!(at(24.0, 100.0), None);
    }

    #[test]
    fn other_cards_hide_the_tiles_under_them_unless_the_pointer_brings_a_target_along() {
        let mut l = two_and_a_card();
        l.move_float(C, Rect::new(100.0, 250.0, 250.0, 150.0)); // over A's center
        let s = scene(&l, 800.0, 600.0, false, &m());
        let c = center_of(&s, A);
        let right_guide = Point::new(c.x + STEP, c.y);
        let g = s.guides(&l, B, right_guide, None);
        assert_eq!((g.target, g.active), (None, None), "C hides A, guides and all");
        let g = s.guides(&l, B, Point::new(80.0, 300.0), None);
        assert_eq!(g.target, Some(A), "A's visible part makes it the target");
        let g = s.guides(&l, B, right_guide, g.target);
        assert_eq!(g.active, Some(Drop::Split(A, Edge::Right)), "its guides draw above cards");
        let between = Point::new(c.x + STEP / 2.0, c.y - STEP / 2.0);
        assert_eq!(s.guides(&l, B, between, Some(A)).target, Some(A), "no flicker between guides");
        assert_eq!(s.guides(&l, B, Point::new(120.0, 380.0), Some(A)).target, None, "off the guides, C hides A again");
        assert_eq!(s.guides(&l, C, Point::new(120.0, 380.0), None).target, Some(A), "the dragged card hides nothing");
    }

    /// A narrow tile A at the left edge, B beside it, C floating.
    fn narrow_left() -> Layout {
        let mut l = two_and_a_card();
        let area = scene(&l, 800.0, 600.0, false, &m()).area;
        l.drag_divider(area, 1.0, 0, 120.0, 10.0);
        l
    }

    #[test]
    fn a_narrow_tiles_guides_make_way_for_the_window_guides_and_reach_past_its_edge() {
        let l = narrow_left();
        let s = scene(&l, 800.0, 570.0, false, &m());
        assert_eq!(s.tiles[0].outer, Rect::new(0.0, 30.0, 120.0, 540.0), "setup");
        let at = |x, y, held| s.guides(&l, C, Point::new(x, y), held).active;
        assert_eq!(at(24.0, 300.0, None), Some(Drop::Window(Edge::Left)), "the window guide stays reachable");
        // A's guides move right until clear of the window guide (8..40) by half a pad: left
        // guide 44..76, right guide 116..148, reaching into B.
        assert_eq!(at(60.0, 300.0, None), Some(Drop::Split(A, Edge::Left)));
        assert_eq!(at(132.0, 300.0, Some(A)), Some(Drop::Split(A, Edge::Right)), "held while on its guides");
        assert_eq!(s.guides(&l, C, Point::new(132.0, 300.0), None).target, Some(B), "fresh over B, B is the target");
    }

    #[test]
    fn a_short_tiles_guides_stay_inside_the_area_and_clear_of_the_window_guides() {
        let mut l = one();
        l.split(A, Edge::Bottom, B);
        l.split(A, Edge::Right, D);
        l.split(B, Edge::Right, C);
        l.float(C, Rect::new(500.0, 400.0, 200.0, 100.0));
        let area = scene(&l, 800.0, 600.0, false, &m()).area;
        l.drag_divider(area, 1.0, 0, 90.0, 10.0);
        let s = scene(&l, 800.0, 600.0, false, &m());
        assert_eq!(s.tiles[0].outer, Rect::new(0.0, 30.0, 400.0, 60.0), "setup: A top left");
        assert_eq!(s.tiles[1].outer, Rect::new(401.0, 30.0, 399.0, 60.0), "setup: D top right");
        let at = |x, y| s.guides(&l, C, Point::new(x, y), None).active;
        // Centered on A the top guide would sit at y 8..40, over the top bar; kept inside the
        // area it sits at 30..62.
        assert_eq!(at(200.0, 50.0), Some(Drop::Split(A, Edge::Top)));
        assert_eq!(at(600.0, 50.0), Some(Drop::Split(D, Edge::Top)), "the same for D");
        assert_eq!(at(400.0, 54.0), Some(Drop::Window(Edge::Top)), "between them, the window's");
    }

    #[test]
    fn the_only_tile_has_nowhere_to_go_and_a_card_over_an_empty_layer_docks_at_the_edges() {
        let mut l = one();
        l.toggle_zoom(A);
        let s = scene(&l, 800.0, 600.0, false, &m());
        assert_eq!(s.guides(&l, A, Point::new(24.0, 300.0), None).buttons, vec![], "even zoomed");
        l.float(A, Rect::new(300.0, 200.0, 200.0, 150.0));
        let s = scene(&l, 800.0, 600.0, false, &m());
        let g = s.guides(&l, A, Point::new(24.0, 300.0), None);
        assert_eq!((g.target, g.buttons.len(), g.active), (None, 4, Some(Drop::Window(Edge::Left))));
    }

    #[test]
    fn each_of_the_eight_guides_lands_the_card_exactly_where_its_preview_shows() {
        let l = two_and_a_card(); // area (0, 30, 800, 540): A (0, 30, 400, 540), B (401, 30, 399, 540)
        let s = scene(&l, 800.0, 570.0, false, &m());
        let c = Point::new(600.5, 300.0); // B's center
        let cases = [
            (Point::new(c.x - STEP, c.y), Drop::Split(B, Edge::Left), Rect::new(401.0, 30.0, 199.0, 540.0)),
            (Point::new(c.x + STEP, c.y), Drop::Split(B, Edge::Right), Rect::new(601.0, 30.0, 199.0, 540.0)),
            (Point::new(c.x, c.y - STEP), Drop::Split(B, Edge::Top), Rect::new(401.0, 30.0, 399.0, 270.0)),
            (Point::new(c.x, c.y + STEP), Drop::Split(B, Edge::Bottom), Rect::new(401.0, 301.0, 399.0, 269.0)),
            (Point::new(24.0, 300.0), Drop::Window(Edge::Left), Rect::new(0.0, 30.0, 400.0, 540.0)),
            (Point::new(776.0, 300.0), Drop::Window(Edge::Right), Rect::new(401.0, 30.0, 399.0, 540.0)),
            (Point::new(400.0, 54.0), Drop::Window(Edge::Top), Rect::new(0.0, 30.0, 800.0, 270.0)),
            (Point::new(400.0, 546.0), Drop::Window(Edge::Bottom), Rect::new(0.0, 301.0, 800.0, 269.0)),
        ];
        for (at, drop, lands) in cases {
            assert_eq!(s.guides(&l, C, at, None).active, Some(drop), "guide at {at:?}");
            assert_eq!(l.drop_preview(s.area, 1.0, C, drop), lands, "preview of {drop:?}");
            let mut after = l.clone();
            after.apply(C, drop);
            let placed = after.tiles(s.area, 1.0).into_iter().find(|(p, _)| *p == C).unwrap().1;
            assert_eq!(placed, lands, "landing of {drop:?}");
            assert_eq!(after.floats(s.area), vec![], "{drop:?} docked the card");
        }
    }

    #[test]
    fn each_guide_lands_a_dragged_tile_exactly_where_its_preview_shows() {
        let l = two_and_a_card(); // A (0, 30, 400, 540), B (401, 30, 399, 540); A leaves, B fills the area
        let s = scene(&l, 800.0, 570.0, false, &m());
        let c = Point::new(600.5, 300.0); // B's center
        let cases = [
            (Point::new(c.x - STEP, c.y), Drop::Split(B, Edge::Left), Rect::new(0.0, 30.0, 400.0, 540.0)),
            (Point::new(c.x + STEP, c.y), Drop::Split(B, Edge::Right), Rect::new(401.0, 30.0, 399.0, 540.0)),
            (Point::new(c.x, c.y - STEP), Drop::Split(B, Edge::Top), Rect::new(0.0, 30.0, 800.0, 270.0)),
            (Point::new(c.x, c.y + STEP), Drop::Split(B, Edge::Bottom), Rect::new(0.0, 301.0, 800.0, 269.0)),
            (c, Drop::Swap(B), Rect::new(401.0, 30.0, 399.0, 540.0)),
        ];
        for (at, drop, lands) in cases {
            assert_eq!(s.guides(&l, A, at, None).active, Some(drop), "guide at {at:?}");
            assert_eq!(l.drop_preview(s.area, 1.0, A, drop), lands, "preview of {drop:?}");
            let mut after = l.clone();
            after.apply(A, drop);
            let placed = after.tiles(s.area, 1.0).into_iter().find(|(p, _)| *p == A).unwrap().1;
            assert_eq!(placed, lands, "landing of {drop:?}");
        }
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
    fn top_bar_buttons_and_dock_chips_are_at_least_24_by_24() {
        // WCAG 2.5.8 Target Size (Minimum). @see https://www.w3.org/TR/WCAG22/#target-size-minimum
        let s = scene(&busy(), 800.0, 600.0, false, &m());
        let rects: Vec<Rect> = s.chrome.iter().map(|b| b.1).chain(s.chips.iter().map(|c| c.1)).collect();
        assert_eq!(rects.len(), 3);
        for r in rects {
            assert!(r.w >= 24.0 && r.h >= 24.0, "{r:?}");
        }
    }

    #[test]
    fn header_buttons_are_spaced_so_24_px_circles_around_them_do_not_overlap() {
        // WCAG 2.5.8's spacing exception for targets under 24 px; the header keeps 3 px above
        // and below for divider and resize bands.
        let s = scene(&busy(), 800.0, 600.0, false, &m());
        for f in s.tiles.iter().chain(&s.cards) {
            for w in f.buttons.windows(2) {
                let (a, b) = (w[0].1.center(), w[1].1.center());
                assert!(b.x - a.x >= 24.0, "{:?} and {:?}", w[0], w[1]);
            }
            for (_, r) in &f.buttons {
                assert_eq!((r.y - f.header.y, f.header.bottom() - r.bottom()), (3.0, 3.0));
            }
        }
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
    fn card_buttons_take_clicks_before_the_resize_band_they_sit_in() {
        let s = scene(&busy(), 800.0, 600.0, false, &m());
        let close = s.cards[0].buttons[2].1;
        let corner = Point::new(close.right() - 1.0, close.y + 1.0); // inside the top and right bands
        assert_eq!(s.hit(corner), Hit::Button(C, PaneButton::Close));
        assert_eq!(
            s.hit(Point::new(499.0, 201.0)),
            Hit::Resize(C, Resize { right: true, top: true, ..Resize::default() })
        );
    }

    #[test]
    fn a_header_too_small_for_every_button_keeps_only_those_that_fit_from_the_right() {
        let narrow = scene(&one(), 60.0, 600.0, false, &m());
        let f = &narrow.tiles[0];
        let order: Vec<PaneButton> = f.buttons.iter().map(|b| b.0).collect();
        assert_eq!(order, vec![PaneButton::Zoom, PaneButton::Close]);
        let short = scene(&one(), 800.0, 30.0 + 15.0, false, &m());
        assert_eq!(short.tiles[0].buttons, vec![], "a 15 px header cannot hold an 18 px button");
    }

    #[test]
    fn the_dock_bar_shows_only_while_a_pane_is_minimized() {
        let none = scene(&two_and_a_card(), 800.0, 600.0, false, &m());
        assert_eq!(none.bar.h, 0.0, "no bar without minimized panes");
        assert_eq!(none.area.bottom(), 600.0, "tiles reach the window's bottom");
        let some = scene(&busy(), 800.0, 600.0, false, &m());
        assert_eq!((some.bar.y, some.bar.h), (570.0, 30.0));
        assert_eq!(some.area.bottom(), 570.0);
    }

    #[test]
    fn a_card_at_the_bottom_moves_up_while_the_dock_bar_shows_and_back_after() {
        let mut l = one();
        l.split(A, Edge::Right, B);
        l.split(A, Edge::Bottom, C);
        l.float(B, Rect::new(100.0, 300.0, 200.0, 100.0));
        l.float(C, Rect::new(500.0, 500.0, 200.0, 100.0)); // its bottom is the window's
        let card = |l: &Layout| scene(l, 800.0, 600.0, false, &m()).cards.iter().find(|f| f.pane == C).unwrap().outer;
        assert_eq!(card(&l), Rect::new(500.0, 500.0, 200.0, 100.0));
        l.minimize(B);
        assert_eq!(card(&l), Rect::new(500.0, 470.0, 200.0, 100.0), "kept above the bar");
        l.restore(B);
        assert_eq!(card(&l), Rect::new(500.0, 500.0, 200.0, 100.0), "back where it was");
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
    fn a_single_tile_fills_the_space_below_the_top_bar() {
        let s = scene(&one(), 800.0, 570.0, false, &m());
        assert_eq!(s.area, Rect::new(0.0, 30.0, 800.0, 540.0));
        assert_eq!(s.tiles[0].header, Rect::new(0.0, 30.0, 800.0, 24.0));
        assert_eq!(s.tiles[0].content, Rect::new(0.0, 54.0, 800.0, 516.0));
        assert_eq!(s.hit(Point::new(400.0, 10.0)), Hit::DragWindow);
        assert_eq!(s.hit(Point::new(400.0, 40.0)), Hit::Header(A));
        assert_eq!(s.hit(Point::new(400.0, 300.0)), Hit::Content(A));
        assert_eq!(s.hit(Point::new(400.0, 569.0)), Hit::Content(A), "down to the window's bottom");
    }
}
