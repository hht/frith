//! Window chrome geometry: each pane's frame (header with title and buttons, terminal
//! content), the dividers between panes, and the dock bar of minimized panes.
//!
//! Answers: where does each piece of chrome sit, and what is under a given point?
//! Must not know: how chrome is painted, terminals, or window-system events. The layout
//! decides the arrangement; this module only dresses it.

use crate::layout::{Divider, Layout, PaneId, Point, Rect};

/// Chrome sizes in the caller's units (the app passes physical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// Space between the window border and the panes.
    pub margin: f32,
    /// Space between neighboring panes.
    pub gap: f32,
    pub header: f32,
    pub button: f32,
    pub radius: f32,
    /// Padding between a pane's content edge and its terminal grid.
    pub pad: f32,
    pub dock: f32,
    pub chip: f32,
    /// Width of the band around a divider that grabs it.
    pub divider_grab: f32,
    /// Width of the band inside the window border that targets a whole-window drop.
    pub edge_band: f32,
    /// Pointer travel that turns a press on a header into a pane drag.
    pub drag_threshold: f32,
    /// Smallest pane side a divider drag may produce.
    pub min_pane: f32,
}

impl Metrics {
    /// Metrics for a display with `scale` physical pixels per logical pixel.
    pub fn for_scale(scale: f32) -> Metrics {
        let px = |logical: f32| (logical * scale).round();
        Metrics {
            margin: px(8.0),
            gap: px(6.0),
            header: px(28.0),
            button: px(20.0),
            radius: px(10.0),
            pad: px(6.0),
            dock: px(34.0),
            chip: px(180.0),
            divider_grab: px(10.0),
            edge_band: px(24.0),
            drag_threshold: px(4.0),
            min_pane: px(80.0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Minimize,
    Zoom,
    Close,
}

/// One visible pane: its whole rounded frame, the header strip, and the terminal content.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub pane: PaneId,
    pub outer: Rect,
    pub header: Rect,
    pub content: Rect,
    /// Header buttons, right to left: close, zoom, minimize.
    pub buttons: [(Button, Rect); 3],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chip {
    pub pane: PaneId,
    pub rect: Rect,
}

/// Everything on screen for one window at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    /// Where the panes are tiled (the window minus margins and dock bar).
    pub area: Rect,
    pub frames: Vec<Frame>,
    pub dividers: Vec<Divider>,
    /// The dock bar, present while any pane is minimized.
    pub dock: Option<Rect>,
    pub chips: Vec<Chip>,
}

/// What a point is over, in priority order: dividers, then pane parts, then dock chips.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Divider(usize),
    Button(PaneId, Button),
    Header(PaneId),
    Content(PaneId),
    Chip(PaneId),
    Nothing,
}

pub fn scene(layout: &Layout, width: f32, height: f32, m: &Metrics) -> Scene {
    let inner = Rect::new(m.margin, m.margin, width - 2.0 * m.margin, height - 2.0 * m.margin);
    let docked: Vec<PaneId> = layout.docked().collect();
    let (area, dock) = if docked.is_empty() {
        (inner, None)
    } else {
        let bar = Rect::new(inner.x, inner.bottom() - m.dock, inner.w, m.dock);
        (Rect::new(inner.x, inner.y, inner.w, (inner.h - m.dock - m.gap).max(0.0)), Some(bar))
    };
    let frames = layout.tiles(area, m.gap).into_iter().map(|(pane, outer)| frame(pane, outer, m)).collect();
    let chips = match dock {
        None => Vec::new(),
        Some(bar) => {
            let inset = ((m.dock - m.button) / 2.0).round();
            let h = bar.h - 2.0 * inset;
            (0..docked.len())
                .map(|i| Rect::new(bar.x + inset + i as f32 * (m.chip + inset), bar.y + inset, m.chip, h))
                .zip(docked)
                .filter(|(rect, _)| rect.right() <= bar.right())
                .map(|(rect, pane)| Chip { pane, rect })
                .collect()
        }
    };
    Scene { area, frames, dividers: layout.dividers(area, m.gap), dock, chips }
}

fn frame(pane: PaneId, outer: Rect, m: &Metrics) -> Frame {
    let header = Rect::new(outer.x, outer.y, outer.w, m.header.min(outer.h));
    let content = Rect::new(outer.x, header.bottom(), outer.w, outer.bottom() - header.bottom());
    let inset = ((m.header - m.button) / 2.0).round();
    let slot = |i: f32| {
        let x = header.right() - inset - m.button - i * (m.button + inset / 2.0);
        Rect::new(x, header.y + inset, m.button, m.button)
    };
    Frame {
        pane,
        outer,
        header,
        content,
        buttons: [(Button::Close, slot(0.0)), (Button::Zoom, slot(1.0)), (Button::Minimize, slot(2.0))],
    }
}

impl Scene {
    pub fn hit(&self, p: Point, m: &Metrics) -> Hit {
        for d in &self.dividers {
            let grab = match d.axis {
                crate::layout::Axis::Horizontal => {
                    let cx = d.rect.x + d.rect.w / 2.0;
                    Rect::new(cx - m.divider_grab / 2.0, d.rect.y, m.divider_grab, d.rect.h)
                }
                crate::layout::Axis::Vertical => {
                    let cy = d.rect.y + d.rect.h / 2.0;
                    Rect::new(d.rect.x, cy - m.divider_grab / 2.0, d.rect.w, m.divider_grab)
                }
            };
            if grab.contains(p) {
                return Hit::Divider(d.index);
            }
        }
        for f in &self.frames {
            if !f.outer.contains(p) {
                continue;
            }
            if let Some((b, _)) = f.buttons.iter().find(|(_, r)| r.contains(p)) {
                return Hit::Button(f.pane, *b);
            }
            return if f.header.contains(p) { Hit::Header(f.pane) } else { Hit::Content(f.pane) };
        }
        match self.chips.iter().find(|c| c.rect.contains(p)) {
            Some(c) => Hit::Chip(c.pane),
            None => Hit::Nothing,
        }
    }

    pub fn frame(&self, pane: PaneId) -> Option<&Frame> {
        self.frames.iter().find(|f| f.pane == pane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Edge;

    const A: PaneId = PaneId(1);
    const B: PaneId = PaneId(2);

    fn m() -> Metrics {
        Metrics::for_scale(1.0)
    }

    #[test]
    fn single_pane_fills_window_inside_margins() {
        let s = scene(&Layout::new(A), 800.0, 600.0, &m());
        assert_eq!(s.area, Rect::new(8.0, 8.0, 784.0, 584.0));
        let f = s.frames[0];
        assert_eq!(f.header, Rect::new(8.0, 8.0, 784.0, 28.0));
        assert_eq!(f.content, Rect::new(8.0, 36.0, 784.0, 556.0));
        assert_eq!(s.dock, None);
    }

    #[test]
    fn buttons_sit_right_aligned_inside_header() {
        let f = scene(&Layout::new(A), 800.0, 600.0, &m()).frames[0];
        let [(close, c), (zoom, z), (min, n)] = f.buttons;
        assert_eq!((close, zoom, min), (Button::Close, Button::Zoom, Button::Minimize));
        assert_eq!(c.right(), f.header.right() - 4.0);
        assert!(n.right() < z.x && z.right() < c.x);
        assert!(c.y >= f.header.y && c.bottom() <= f.header.bottom());
    }

    #[test]
    fn hit_resolves_buttons_header_content_and_nothing() {
        let s = scene(&Layout::new(A), 800.0, 600.0, &m());
        let f = s.frames[0];
        let center = |r: Rect| Point::new(r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert_eq!(s.hit(center(f.buttons[0].1), &m()), Hit::Button(A, Button::Close));
        assert_eq!(s.hit(Point::new(20.0, 20.0), &m()), Hit::Header(A));
        assert_eq!(s.hit(Point::new(400.0, 300.0), &m()), Hit::Content(A));
        assert_eq!(s.hit(Point::new(2.0, 2.0), &m()), Hit::Nothing);
    }

    #[test]
    fn divider_grab_band_is_wider_than_the_gap() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Right, B);
        let s = scene(&l, 800.0, 600.0, &m());
        let gap = s.dividers[0].rect;
        let mid = gap.x + gap.w / 2.0;
        assert_eq!(s.hit(Point::new(mid, 300.0), &m()), Hit::Divider(0));
        assert_eq!(s.hit(Point::new(mid - 4.5, 300.0), &m()), Hit::Divider(0), "inside pane edge");
        assert_eq!(s.hit(Point::new(mid - 6.0, 300.0), &m()), Hit::Content(A));
    }

    #[test]
    fn minimized_panes_shrink_the_area_and_show_as_chips() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Right, B);
        l.minimize(B);
        let s = scene(&l, 800.0, 600.0, &m());
        let bar = s.dock.unwrap();
        assert_eq!(bar, Rect::new(8.0, 558.0, 784.0, 34.0));
        assert_eq!(s.area.bottom(), bar.y - 6.0);
        assert_eq!(s.chips.len(), 1);
        let c = s.chips[0].rect;
        assert_eq!(s.hit(Point::new(c.x + 1.0, c.y + 1.0), &m()), Hit::Chip(B));
    }

    #[test]
    fn chips_that_do_not_fit_are_left_out() {
        let mut l = Layout::new(PaneId(0));
        for i in 1..=6 {
            l.split(PaneId(0), Edge::Right, PaneId(i));
            l.minimize(PaneId(i));
        }
        let s = scene(&l, 600.0, 400.0, &m());
        assert!(s.chips.iter().all(|c| c.rect.right() <= s.dock.unwrap().right()));
        assert!(s.chips.len() < 6);
    }
}
