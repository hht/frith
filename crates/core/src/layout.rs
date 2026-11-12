//! Pane arrangement of the root window: a binary split tree of tiled panes, at most one
//! zoomed pane, a z-ordered stack of floating panes, and the dock of minimized floating panes.
//!
//! Answers: where does each pane sit, and how does the arrangement change when a pane is
//! split off, closed, floated, embedded, minimized, restored, swapped, zoomed, or dropped?
//! Must not know: terminals, drawing, window-system events, chrome (headers, dock bar), or
//! time. Units are whatever the caller passes; the app uses physical pixels.

use crate::geom::{Point, Rect};

/// Stable identity of a pane for its whole life.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct PaneId(pub u32);

/// Direction in which a split lays out its two children.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Axis {
    /// Children side by side: first on the left.
    Horizontal,
    /// Children stacked: first on top.
    Vertical,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn axis(self) -> Axis {
        match self {
            Edge::Left | Edge::Right => Axis::Horizontal,
            Edge::Top | Edge::Bottom => Axis::Vertical,
        }
    }

    /// Left and Top come first in a split.
    pub fn is_leading(self) -> bool {
        matches!(self, Edge::Left | Edge::Top)
    }

    fn of(axis: Axis, leading: bool) -> Edge {
        match (axis, leading) {
            (Axis::Horizontal, true) => Edge::Left,
            (Axis::Horizontal, false) => Edge::Right,
            (Axis::Vertical, true) => Edge::Top,
            (Axis::Vertical, false) => Edge::Bottom,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Leaf(PaneId),
    Split(Box<Split>),
}

#[derive(Clone, Debug, PartialEq)]
struct Split {
    axis: Axis,
    ratio: f32,
    first: Node,
    second: Node,
}

/// A floating pane and where it was tiled before it floated.
/// Where a dragged pane lands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drop {
    /// Split the target tile and put the dragged pane on `Edge` of it.
    Split(PaneId, Edge),
    /// Exchange places with the target tile (only for a tiled pane).
    Swap(PaneId),
    /// Split the whole tiled area and put the dragged pane along `Edge`.
    Window(Edge),
}

/// The gap between the two children of one split, for hit testing and resizing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Divider {
    /// Pre-order index of the split; valid until the next structural change (see `revision`).
    pub index: usize,
    pub axis: Axis,
    /// The gap strip itself (zero-sized when the gap is zero).
    pub rect: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Floating {
    pane: PaneId,
    rect: Rect,
    /// The pane it sat beside and on which side; `None` if it was the only tile.
    home: Option<(PaneId, Edge)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    root: Option<Node>,
    zoomed: Option<PaneId>,
    /// Bottom to top.
    floating: Vec<Floating>,
    /// Minimized floating panes, oldest first; each keeps its rect and home.
    dock: Vec<Floating>,
    revision: u64,
}

impl Layout {
    /// Splits the whole tiled area and puts `pane` along `edge`, at 50%; into an empty tiled
    /// layer, `pane` becomes the only tile.
    pub fn split_window(&mut self, edge: Edge, pane: PaneId) {
        assert!(!self.contains(pane), "{pane:?} is already in the layout");
        self.root = Some(match self.root.take() {
            Some(old) => Node::split(edge, old, Node::Leaf(pane)),
            None => Node::Leaf(pane),
        });
        self.zoomed = None;
        self.changed();
    }

    /// Splits tiled pane `at` and puts the new `pane` on `edge` of it, at 50%.
    pub fn split(&mut self, at: PaneId, edge: Edge, pane: PaneId) {
        assert!(!self.contains(pane), "{pane:?} is already in the layout");
        let leaf = self.root.as_mut().and_then(|r| r.find_leaf_mut(at));
        let leaf = leaf.unwrap_or_else(|| panic!("{at:?} is not tiled"));
        let old = std::mem::replace(leaf, Node::Leaf(pane));
        *leaf = Node::split(edge, old, Node::Leaf(pane));
        self.zoomed = None;
        self.changed();
    }

    /// Changes whenever panes are added, removed, moved between layers or tiles, zoomed,
    /// raised, minimized or restored, so a gesture that captured a pane or divider index can
    /// tell it may be stale. Divider drags and card moves keep it: they change sizes only.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Removes `pane` and returns the pane that should take focus, or `None` when nothing
    /// is left to focus.
    pub fn remove(&mut self, pane: PaneId) -> Option<PaneId> {
        let neighbor = if let Some(i) = self.floating.iter().position(|f| f.pane == pane) {
            self.floating.remove(i);
            None
        } else if let Some(i) = self.dock.iter().position(|f| f.pane == pane) {
            self.dock.remove(i);
            None
        } else {
            self.detach(pane).map(|d| d.neighbor)
        };
        self.changed();
        neighbor.or_else(|| self.floating.last().map(|f| f.pane)).or_else(|| self.first_tile())
    }

    fn first_tile(&self) -> Option<PaneId> {
        let mut node = self.root.as_ref()?;
        loop {
            match node {
                Node::Leaf(p) => return Some(*p),
                Node::Split(s) => node = &s.first,
            }
        }
    }

    /// Zooms tiled `pane` to fill the tiled area, or unzooms if it is already zoomed.
    pub fn toggle_zoom(&mut self, pane: PaneId) {
        assert!(self.is_tiled(pane), "{pane:?} is not tiled");
        self.zoomed = if self.zoomed == Some(pane) { None } else { Some(pane) };
        self.changed();
    }

    pub fn zoomed(&self) -> Option<PaneId> {
        self.zoomed
    }

    /// Exchanges the positions of two tiled panes; sizes and zoom stay as they are.
    pub fn swap(&mut self, a: PaneId, b: PaneId) {
        assert!(self.is_tiled(a) && self.is_tiled(b), "swap needs two tiled panes");
        if let Some(root) = &mut self.root {
            root.swap(a, b);
        }
        self.changed();
    }

    /// Gaps between split children, in pre-order. None while a pane is zoomed.
    pub fn dividers(&self, area: Rect, gap: f32) -> Vec<Divider> {
        let mut out = Vec::new();
        if let (None, Some(root)) = (self.zoomed, &self.root) {
            root.dividers(area, gap, &mut out);
        }
        out
    }

    /// Moves divider `index` so its gap is centered at `at` (an x for side-by-side splits, a
    /// y for stacked ones), keeping both sides at least `min` long.
    pub fn drag_divider(&mut self, area: Rect, gap: f32, index: usize, at: f32, min: f32) {
        assert!(self.zoomed.is_none(), "dividers are hidden while zoomed");
        let root = self.root.as_mut().expect("a divider implies tiles");
        let found = root.split_at(area, gap, index, &mut 0);
        let (split, span) = found.unwrap_or_else(|| panic!("no divider {index}"));
        let (start, len) = match split.axis {
            Axis::Horizontal => (span.x, span.w - gap),
            Axis::Vertical => (span.y, span.h - gap),
        };
        let lo = min / len;
        split.ratio = if len <= 0.0 || lo >= 0.5 { 0.5 } else { ((at - start - gap / 2.0) / len).clamp(lo, 1.0 - lo) };
        self.debug_check();
    }

    /// Where `dragged` would land if released at `p`. Within `band` of the area's border the
    /// whole window is targeted. A tiled pane targets the nearest edge of the tile under `p`
    /// within 30% of its size, else swaps with it; a floating pane moves freely except
    /// within `band` of a tile's edge (ADR 6). `None` means no dock: over `dragged` itself,
    /// outside `area`, or a free move.
    pub fn drop_target(&self, area: Rect, gap: f32, band: f32, p: Point, dragged: PaneId) -> Option<Drop> {
        if !area.contains(p) {
            return None;
        }
        let tiled = self.is_tiled(dragged);
        if tiled && self.root == Some(Node::Leaf(dragged)) {
            return None; // nowhere else to go
        }
        let (d, edge) = nearest_edge(p, area);
        if d < band {
            return Some(Drop::Window(edge));
        }
        let (target, r) = self.tiles(area, gap).into_iter().find(|(_, r)| r.contains(p))?;
        if target == dragged {
            return None;
        }
        let (d, edge) = nearest_edge(p, r);
        if tiled {
            let span = if edge.axis() == Axis::Horizontal { r.w } else { r.h };
            Some(if d < EDGE_ZONE * span { Drop::Split(target, edge) } else { Drop::Swap(target) })
        } else {
            (d < band).then_some(Drop::Split(target, edge))
        }
    }

    /// The zone `drop` covers in the current arrangement, for a preview.
    pub fn drop_preview(&self, area: Rect, gap: f32, drop: Drop) -> Rect {
        let tile = |pane| {
            let tiles = self.tiles(area, gap);
            tiles.into_iter().find(|(p, _)| *p == pane).unwrap_or_else(|| panic!("{pane:?} is not visible")).1
        };
        match drop {
            Drop::Split(target, edge) => half(tile(target), edge, gap),
            Drop::Swap(target) => tile(target),
            Drop::Window(_) if self.root.is_none() => area,
            Drop::Window(edge) => half(area, edge, gap),
        }
    }

    /// Moves tiled or floating `pane` to `drop`, as computed by `drop_target` for this
    /// layout and pane, and resets zoom so the moved pane is visible.
    pub fn apply(&mut self, pane: PaneId, drop: Drop) {
        self.zoomed = None;
        if let Drop::Swap(target) = drop {
            self.swap(pane, target);
            return;
        }
        if self.is_tiled(pane) {
            self.detach(pane);
        } else {
            self.floating.remove(self.float_index(pane));
        }
        match drop {
            Drop::Split(target, edge) => self.split(target, edge, pane),
            Drop::Window(edge) => self.split_window(edge, pane),
            Drop::Swap(_) => unreachable!(),
        }
    }

    /// Lifts tiled `pane` into the floating layer at `rect`, on top of other floating panes.
    /// Remembers where it sat so `embed` can put it back.
    pub fn float(&mut self, pane: PaneId, rect: Rect) {
        let home = self.detach(pane).map(|d| (d.neighbor, d.edge));
        self.floating.push(Floating { pane, rect, home });
        self.changed();
    }

    /// Puts floating `pane` back into the tiled layer beside the pane it was floated from, or
    /// along the same window edge when that pane is no longer tiled.
    pub fn embed(&mut self, pane: PaneId) {
        let f = self.floating.remove(self.float_index(pane));
        match f.home {
            Some((at, edge)) if self.is_tiled(at) => self.split(at, edge, pane),
            Some((_, edge)) => self.split_window(edge, pane),
            None => self.split_window(Edge::Right, pane),
        }
    }

    pub fn is_tiled(&self, pane: PaneId) -> bool {
        self.root.as_ref().is_some_and(|n| n.contains(pane))
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.is_tiled(pane) || self.floating.iter().chain(&self.dock).any(|f| f.pane == pane)
    }

    /// Moves floating `pane` to the dock. Tiled panes cannot be minimized (ADR 7).
    pub fn minimize(&mut self, pane: PaneId) {
        let f = self.floating.remove(self.float_index(pane));
        self.dock.push(f);
        self.changed();
    }

    /// Puts docked `pane` back on top of the floating layer where it was.
    pub fn restore(&mut self, pane: PaneId) {
        let i = self.dock.iter().position(|f| f.pane == pane);
        let f = self.dock.remove(i.unwrap_or_else(|| panic!("{pane:?} is not docked")));
        self.floating.push(f);
        self.changed();
    }

    /// Minimized panes in the order they were minimized.
    pub fn docked(&self) -> Vec<PaneId> {
        self.dock.iter().map(|f| f.pane).collect()
    }

    /// Brings floating `pane` to the top of the floating layer.
    pub fn raise(&mut self, pane: PaneId) {
        let f = self.floating.remove(self.float_index(pane));
        self.floating.push(f);
        self.changed();
    }

    /// Moves or resizes floating `pane`. `floats` keeps it inside the area.
    pub fn move_float(&mut self, pane: PaneId, rect: Rect) {
        let i = self.float_index(pane);
        self.floating[i].rect = rect;
    }

    /// Floating panes from bottom to top, each kept inside `area`.
    pub fn floats(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        self.floating.iter().map(|f| (f.pane, keep_inside(f.rect, area))).collect()
    }

    /// Rectangles of the visible tiled panes inside `area`, `gap` apart, in reading order.
    pub fn tiles(&self, area: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        match (self.zoomed, &self.root) {
            (Some(z), _) => out.push((z, area)),
            (None, Some(root)) => root.tiles(area, gap, &mut out),
            (None, None) => {}
        }
        out
    }
}

impl Layout {
    /// Records a structural change.
    fn changed(&mut self) {
        self.revision += 1;
        self.debug_check();
    }

    /// Panics when an invariant no longer holds: every pane appears once (in the tree, the
    /// floating layer, or the dock), the zoomed pane is tiled, and every ratio is in (0, 1).
    fn debug_check(&self) {
        if !cfg!(debug_assertions) {
            return;
        }
        let mut all = Vec::new();
        if let Some(root) = &self.root {
            root.leaves(&mut all);
            assert!(root.ratios_valid(), "split ratio out of (0, 1): {self:?}");
        }
        all.extend(self.floating.iter().chain(&self.dock).map(|f| f.pane));
        let len = all.len();
        all.sort();
        all.dedup();
        assert_eq!(len, all.len(), "a pane appears twice: {self:?}");
        if let Some(z) = self.zoomed {
            assert!(self.is_tiled(z), "zoomed pane is not tiled: {self:?}");
        }
    }

    fn float_index(&self, pane: PaneId) -> usize {
        let i = self.floating.iter().position(|f| f.pane == pane);
        i.unwrap_or_else(|| panic!("{pane:?} is not floating"))
    }

    /// Takes tiled leaf `pane` out of the tree; its parent split collapses into the sibling.
    /// Returns `None` when `pane` was the only tile, leaving the tiled layer empty.
    fn detach(&mut self, pane: PaneId) -> Option<Detached> {
        if self.zoomed == Some(pane) {
            self.zoomed = None;
        }
        let root = self.root.as_mut().unwrap_or_else(|| panic!("{pane:?} is not tiled"));
        if *root == Node::Leaf(pane) {
            self.root = None;
            return None;
        }
        Some(root.detach(pane).unwrap_or_else(|| panic!("{pane:?} is not tiled")))
    }
}

/// What detaching a leaf from the tree left behind.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Detached {
    /// The leaf of the former sibling that sat right next to the detached pane.
    neighbor: PaneId,
    /// Side of `neighbor` on which the detached pane sat.
    edge: Edge,
}

impl Node {
    fn split(edge: Edge, existing: Node, new: Node) -> Node {
        let (first, second) = if edge.is_leading() { (new, existing) } else { (existing, new) };
        Node::Split(Box::new(Split { axis: edge.axis(), ratio: 0.5, first, second }))
    }

    fn contains(&self, pane: PaneId) -> bool {
        match self {
            Node::Leaf(p) => *p == pane,
            Node::Split(s) => s.first.contains(pane) || s.second.contains(pane),
        }
    }

    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(p) => out.push(*p),
            Node::Split(s) => {
                s.first.leaves(out);
                s.second.leaves(out);
            }
        }
    }

    fn ratios_valid(&self) -> bool {
        match self {
            Node::Leaf(_) => true,
            Node::Split(s) => s.ratio > 0.0 && s.ratio < 1.0 && s.first.ratios_valid() && s.second.ratios_valid(),
        }
    }

    fn find_leaf_mut(&mut self, pane: PaneId) -> Option<&mut Node> {
        match self {
            Node::Leaf(p) if *p == pane => Some(self),
            Node::Leaf(_) => None,
            Node::Split(s) => match s.first.find_leaf_mut(pane) {
                Some(n) => Some(n),
                None => s.second.find_leaf_mut(pane),
            },
        }
    }

    /// The leaf of this subtree nearest to a removed sibling that sat on the `leading`
    /// (left/top) or trailing side along `axis`.
    fn adjacent_leaf(&self, axis: Axis, leading: bool) -> PaneId {
        match self {
            Node::Leaf(p) => *p,
            Node::Split(s) if s.axis == axis && !leading => s.second.adjacent_leaf(axis, leading),
            Node::Split(s) => s.first.adjacent_leaf(axis, leading),
        }
    }

    /// Detaches leaf `pane` from below `self` (never `self` itself).
    fn detach(&mut self, pane: PaneId) -> Option<Detached> {
        let Node::Split(s) = self else { return None };
        let in_first = s.first == Node::Leaf(pane);
        if !in_first && s.second != Node::Leaf(pane) {
            return match s.first.detach(pane) {
                Some(d) => Some(d),
                None => s.second.detach(pane),
            };
        }
        let axis = s.axis;
        let Node::Split(s) = std::mem::replace(self, Node::Leaf(pane)) else { unreachable!() };
        let sibling = if in_first { s.second } else { s.first };
        let neighbor = sibling.adjacent_leaf(axis, in_first);
        *self = sibling;
        Some(Detached { neighbor, edge: Edge::of(axis, in_first) })
    }

    fn dividers(&self, r: Rect, gap: f32, out: &mut Vec<Divider>) {
        let Node::Split(s) = self else { return };
        let (a, b) = cut(r, s.axis, s.ratio, gap);
        let rect = match s.axis {
            Axis::Horizontal => Rect::new(a.right(), r.y, b.x - a.right(), r.h),
            Axis::Vertical => Rect::new(r.x, a.bottom(), r.w, b.y - a.bottom()),
        };
        out.push(Divider { index: out.len(), axis: s.axis, rect });
        s.first.dividers(a, gap, out);
        s.second.dividers(b, gap, out);
    }

    /// The split with pre-order `index` and the rectangle it divides.
    fn split_at(&mut self, r: Rect, gap: f32, index: usize, counter: &mut usize) -> Option<(&mut Split, Rect)> {
        let Node::Split(s) = self else { return None };
        if *counter == index {
            return Some((s, r));
        }
        *counter += 1;
        let (a, b) = cut(r, s.axis, s.ratio, gap);
        if let Some(found) = s.first.split_at(a, gap, index, counter) {
            return Some(found);
        }
        s.second.split_at(b, gap, index, counter)
    }

    fn swap(&mut self, a: PaneId, b: PaneId) {
        match self {
            Node::Leaf(p) if *p == a => *p = b,
            Node::Leaf(p) if *p == b => *p = a,
            Node::Leaf(_) => {}
            Node::Split(s) => {
                s.first.swap(a, b);
                s.second.swap(a, b);
            }
        }
    }

    fn tiles(&self, r: Rect, gap: f32, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(p) => out.push((*p, r)),
            Node::Split(s) => {
                let (a, b) = cut(r, s.axis, s.ratio, gap);
                s.first.tiles(a, gap, out);
                s.second.tiles(b, gap, out);
            }
        }
    }
}

/// Fraction of a tile's width or height, measured from an edge, in which a dragged tile
/// splits that edge; the rest of the tile is the swap zone.
const EDGE_ZONE: f32 = 0.3;

/// Distance from `p` (inside `r`) to the nearest edge of `r`, and that edge.
fn nearest_edge(p: Point, r: Rect) -> (f32, Edge) {
    [(p.x - r.x, Edge::Left), (r.right() - p.x, Edge::Right), (p.y - r.y, Edge::Top), (r.bottom() - p.y, Edge::Bottom)]
        .into_iter()
        .fold((f32::INFINITY, Edge::Left), |best, c| if c.0 < best.0 { c } else { best })
}

/// The half of `r` touching `edge`, as a fresh 50% split would produce it.
fn half(r: Rect, edge: Edge, gap: f32) -> Rect {
    let (first, second) = cut(r, edge.axis(), 0.5, gap);
    if edge.is_leading() { first } else { second }
}

/// `r` shrunk to fit `area` if needed, then shifted so it lies inside `area`.
fn keep_inside(r: Rect, area: Rect) -> Rect {
    let (w, h) = (r.w.min(area.w), r.h.min(area.h));
    Rect::new(r.x.clamp(area.x, area.right() - w), r.y.clamp(area.y, area.bottom() - h), w, h)
}

/// Cuts `r` along `axis` into two parts separated by `gap`; the first part gets `ratio` of
/// the space left after the gap, rounded to whole units so terminal grids stay pixel-aligned.
fn cut(r: Rect, axis: Axis, ratio: f32, gap: f32) -> (Rect, Rect) {
    match axis {
        Axis::Horizontal => {
            let first = ((r.w - gap).max(0.0) * ratio).round();
            let second_x = r.x + first + gap;
            (Rect::new(r.x, r.y, first, r.h), Rect::new(second_x, r.y, (r.right() - second_x).max(0.0), r.h))
        }
        Axis::Vertical => {
            let first = ((r.h - gap).max(0.0) * ratio).round();
            let second_y = r.y + first + gap;
            (Rect::new(r.x, r.y, r.w, first), Rect::new(r.x, second_y, r.w, (r.bottom() - second_y).max(0.0)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PaneId = PaneId(1);
    const B: PaneId = PaneId(2);
    const C: PaneId = PaneId(3);
    const D: PaneId = PaneId(4);
    const AREA: Rect = Rect::new(0.0, 0.0, 1000.0, 600.0);

    fn rect_of(l: &Layout, pane: PaneId) -> Rect {
        l.tiles(AREA, 0.0).into_iter().find(|(p, _)| *p == pane).expect("pane is tiled").1
    }

    /// A | B over C:  ┌─A─┬─B─┐
    ///                └───C───┘
    fn abc() -> Layout {
        let mut l = Layout::default();
        l.split_window(Edge::Right, A);
        l.split(A, Edge::Bottom, C);
        l.split(A, Edge::Right, B);
        l
    }

    #[test]
    fn closing_a_tile_lets_its_sibling_take_the_space_and_focuses_the_nearest_neighbor() {
        let mut l = abc();
        assert_eq!(l.remove(B), Some(A));
        assert_eq!(rect_of(&l, A), Rect::new(0.0, 0.0, 1000.0, 300.0));
        assert_eq!(l.remove(C), Some(A));
        assert_eq!(l.tiles(AREA, 0.0), vec![(A, AREA)]);
        assert_eq!(l.remove(A), None, "nothing left to focus");
        assert_eq!(l.tiles(AREA, 0.0), vec![]);
    }

    #[test]
    fn floating_a_tile_reflows_the_rest_and_embedding_returns_it_to_where_it_came_from() {
        let mut l = abc();
        let card = Rect::new(600.0, 20.0, 300.0, 200.0);
        l.float(B, card);
        assert_eq!(l.floats(AREA), vec![(B, card)]);
        assert_eq!(rect_of(&l, A), Rect::new(0.0, 0.0, 1000.0, 300.0), "A takes B's space");
        l.embed(B);
        assert_eq!(l.floats(AREA), vec![]);
        assert_eq!(l.tiles(AREA, 0.0), abc().tiles(AREA, 0.0), "B is back right of A");
    }

    #[test]
    fn embedding_after_the_old_neighbor_closed_docks_along_the_same_window_edge() {
        let mut l = Layout::default();
        l.split_window(Edge::Right, A);
        l.split(A, Edge::Right, B);
        l.split(B, Edge::Bottom, C);
        l.float(A, Rect::new(10.0, 10.0, 100.0, 100.0)); // A sat left of B
        l.remove(B);
        l.embed(A);
        assert_eq!(
            l.tiles(AREA, 0.0),
            vec![(A, Rect::new(0.0, 0.0, 500.0, 600.0)), (C, Rect::new(500.0, 0.0, 500.0, 600.0))]
        );
    }

    #[test]
    fn the_tiled_layer_may_empty_out_and_an_embedded_pane_then_fills_it() {
        let mut l = Layout::default();
        l.split_window(Edge::Right, A);
        l.float(A, Rect::new(10.0, 10.0, 100.0, 100.0));
        assert_eq!(l.tiles(AREA, 0.0), vec![]);
        l.embed(A);
        assert_eq!(l.tiles(AREA, 0.0), vec![(A, AREA)]);
    }

    #[test]
    fn newer_and_raised_floats_stack_on_top_and_cards_stay_inside_the_area() {
        let mut l = abc();
        l.float(A, Rect::new(0.0, 0.0, 200.0, 100.0));
        l.float(B, Rect::new(50.0, 50.0, 200.0, 100.0));
        assert_eq!(l.floats(AREA).iter().map(|f| f.0).collect::<Vec<_>>(), vec![A, B]);
        l.raise(A);
        assert_eq!(l.floats(AREA).iter().map(|f| f.0).collect::<Vec<_>>(), vec![B, A]);
        l.move_float(A, Rect::new(900.0, -40.0, 200.0, 100.0));
        assert_eq!(l.floats(AREA)[1], (A, Rect::new(800.0, 0.0, 200.0, 100.0)), "pushed back inside");
        let small = Rect::new(0.0, 0.0, 150.0, 80.0);
        assert_eq!(l.floats(small)[1], (A, Rect::new(0.0, 0.0, 150.0, 80.0)), "shrunk to fit");
    }

    #[test]
    fn a_minimized_float_waits_in_the_dock_and_restores_on_top_where_it_was() {
        let mut l = abc();
        let card = Rect::new(100.0, 100.0, 300.0, 200.0);
        l.float(B, card);
        l.float(C, Rect::new(0.0, 0.0, 100.0, 100.0));
        l.minimize(B);
        assert_eq!(l.docked(), vec![B]);
        assert_eq!(l.floats(AREA).iter().map(|f| f.0).collect::<Vec<_>>(), vec![C]);
        l.restore(B);
        assert_eq!(l.docked(), vec![]);
        assert_eq!(l.floats(AREA).last(), Some(&(B, card)));
    }

    #[test]
    #[should_panic(expected = "is not floating")]
    fn a_tiled_pane_cannot_be_minimized() {
        abc().minimize(A);
    }

    #[test]
    fn closing_a_float_or_docked_pane_focuses_the_top_float_else_the_first_tile() {
        let mut l = abc();
        l.float(B, Rect::new(0.0, 0.0, 100.0, 100.0));
        l.float(C, Rect::new(0.0, 0.0, 100.0, 100.0));
        l.minimize(C);
        assert_eq!(l.remove(C), Some(B), "docked C goes; B is the top float");
        assert_eq!(l.remove(B), Some(A));
    }

    #[test]
    fn a_zoomed_tile_fills_the_area_until_unzoomed_split_or_floated() {
        let mut l = abc();
        l.toggle_zoom(B);
        assert_eq!(l.tiles(AREA, 0.0), vec![(B, AREA)]);
        l.remove(C);
        assert_eq!(l.zoomed(), Some(B), "closing another pane keeps the zoom");
        l.toggle_zoom(B);
        assert_eq!(l.tiles(AREA, 0.0).len(), 2);
        l.toggle_zoom(B);
        l.split(A, Edge::Bottom, C);
        assert_eq!(l.zoomed(), None, "a split shows the new pane");
        l.toggle_zoom(B);
        l.float(B, Rect::new(0.0, 0.0, 10.0, 10.0));
        assert_eq!(l.zoomed(), None);
        assert_eq!(l.tiles(AREA, 0.0).len(), 2);
    }

    #[test]
    fn swapping_exchanges_two_tiles_and_keeps_the_sizes() {
        let mut l = abc();
        let (a, c) = (rect_of(&l, A), rect_of(&l, C));
        l.swap(A, C);
        assert_eq!((rect_of(&l, C), rect_of(&l, A)), (a, c));
    }

    #[test]
    fn gaps_separate_tiles_in_whole_units_and_each_split_has_a_divider_in_its_gap() {
        let mut l = Layout::default();
        l.split_window(Edge::Right, A);
        l.split(A, Edge::Right, B);
        let area = Rect::new(0.0, 0.0, 101.0, 50.0);
        assert_eq!(
            l.tiles(area, 1.0),
            vec![(A, Rect::new(0.0, 0.0, 50.0, 50.0)), (B, Rect::new(51.0, 0.0, 50.0, 50.0))]
        );
        assert_eq!(
            l.dividers(area, 1.0),
            vec![Divider { index: 0, axis: Axis::Horizontal, rect: Rect::new(50.0, 0.0, 1.0, 50.0) }]
        );
    }

    #[test]
    fn dragging_a_divider_moves_it_to_the_pointer_but_keeps_both_sides_at_least_min() {
        let mut l = abc(); // divider 0: A|B over C (vertical); divider 1: A | B
        l.drag_divider(AREA, 0.0, 1, 250.0, 100.0);
        assert_eq!(rect_of(&l, A).w, 250.0);
        l.drag_divider(AREA, 0.0, 1, 990.0, 100.0);
        assert_eq!(rect_of(&l, B).w, 100.0, "clamped at min");
        l.drag_divider(AREA, 0.0, 0, 450.0, 100.0);
        assert_eq!(rect_of(&l, C), Rect::new(0.0, 450.0, 1000.0, 150.0));
    }

    #[test]
    fn dragging_a_tile_targets_pane_edges_centers_for_swap_and_window_edges() {
        let l = abc(); // A (0,0,500,300)  B (500,0,500,300)  C (0,300,1000,300)
        let at = |x, y| l.drop_target(AREA, 0.0, 24.0, Point::new(x, y), A);
        assert_eq!(at(750.0, 20.0 + 24.0), Some(Drop::Split(B, Edge::Top)));
        assert_eq!(at(980.0 - 24.0, 150.0), Some(Drop::Split(B, Edge::Right)));
        assert_eq!(at(750.0, 150.0), Some(Drop::Swap(B)), "center swaps");
        assert_eq!(at(500.0, 590.0), Some(Drop::Window(Edge::Bottom)), "window band");
        assert_eq!(at(250.0, 150.0), None, "over the dragged pane itself");
        assert_eq!(at(1200.0, 150.0), None, "outside the area");
    }

    #[test]
    fn dragging_a_float_only_docks_inside_narrow_bands_and_otherwise_moves_freely() {
        let mut l = abc();
        l.float(B, Rect::new(0.0, 0.0, 100.0, 100.0)); // A (0,0,1000,300)  C (0,300,1000,300)
        let at = |x, y| l.drop_target(AREA, 0.0, 24.0, Point::new(x, y), B);
        assert_eq!(at(500.0, 150.0), None, "pane center: free move");
        assert_eq!(at(500.0, 280.0), Some(Drop::Split(A, Edge::Bottom)), "pane edge band");
        assert_eq!(at(500.0, 10.0), Some(Drop::Window(Edge::Top)), "window band wins");
        assert_eq!(at(500.0, 400.0), None);
    }

    #[test]
    fn a_float_dropped_on_an_empty_tiled_layer_docks_from_the_window_band() {
        let mut l = Layout::default();
        l.split_window(Edge::Right, A);
        l.float(A, Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(l.drop_target(AREA, 0.0, 24.0, Point::new(5.0, 300.0), A), Some(Drop::Window(Edge::Left)));
        assert_eq!(l.drop_target(AREA, 0.0, 24.0, Point::new(500.0, 300.0), A), None);
        assert_eq!(l.drop_preview(AREA, 0.0, Drop::Window(Edge::Left)), AREA, "it will fill the area");
    }

    #[test]
    fn drop_previews_show_the_half_or_whole_tile_the_pane_will_take() {
        let l = abc();
        assert_eq!(l.drop_preview(AREA, 0.0, Drop::Split(C, Edge::Left)), Rect::new(0.0, 300.0, 500.0, 300.0));
        assert_eq!(l.drop_preview(AREA, 0.0, Drop::Swap(B)), Rect::new(500.0, 0.0, 500.0, 300.0));
        assert_eq!(l.drop_preview(AREA, 0.0, Drop::Window(Edge::Top)), Rect::new(0.0, 0.0, 1000.0, 300.0));
    }

    #[test]
    fn dropping_a_tile_moves_it_and_its_old_neighbors_close_the_gap() {
        let mut l = abc();
        l.toggle_zoom(C);
        l.apply(A, Drop::Split(C, Edge::Left));
        assert_eq!(l.zoomed(), None, "the moved pane must be visible");
        assert_eq!(
            l.tiles(AREA, 0.0),
            vec![
                (B, Rect::new(0.0, 0.0, 1000.0, 300.0)),
                (A, Rect::new(0.0, 300.0, 500.0, 300.0)),
                (C, Rect::new(500.0, 300.0, 500.0, 300.0)),
            ]
        );
        l.apply(A, Drop::Swap(B));
        assert_eq!(rect_of(&l, A), Rect::new(0.0, 0.0, 1000.0, 300.0));
    }

    #[test]
    fn dropping_a_float_docks_it_into_the_tiled_layer() {
        let mut l = abc();
        l.float(B, Rect::new(0.0, 0.0, 100.0, 100.0));
        l.apply(B, Drop::Window(Edge::Top));
        assert_eq!(l.floats(AREA), vec![]);
        assert_eq!(rect_of(&l, B), Rect::new(0.0, 0.0, 1000.0, 300.0));
    }

    #[test]
    fn revision_changes_with_structure_but_not_with_sizes() {
        let mut l = abc();
        let mut seen = vec![l.revision()];
        let mut changed = |l: &Layout, what: &str| {
            assert!(!seen.contains(&l.revision()), "{what} kept the revision");
            seen.push(l.revision());
        };
        let card = Rect::new(0.0, 0.0, 100.0, 100.0);
        l.split(A, Edge::Left, D);
        changed(&l, "split");
        l.swap(A, D);
        changed(&l, "swap");
        l.toggle_zoom(A);
        changed(&l, "zoom");
        l.float(B, card);
        changed(&l, "float");
        l.float(C, card);
        l.raise(B);
        changed(&l, "raise");
        l.minimize(B);
        changed(&l, "minimize");
        l.restore(B);
        changed(&l, "restore");
        l.embed(B);
        changed(&l, "embed");
        l.apply(C, Drop::Window(Edge::Top));
        changed(&l, "apply");
        l.remove(D);
        changed(&l, "remove");
        l.float(C, card);
        let before = l.revision();
        l.drag_divider(AREA, 0.0, 0, 200.0, 10.0);
        assert_eq!(l.revision(), before, "resizing keeps pane identities and divider indices");
        l.move_float(C, Rect::new(50.0, 50.0, 100.0, 100.0));
        assert_eq!(l.revision(), before, "moving a card keeps pane identities");
    }

    #[test]
    fn first_pane_fills_the_area_and_a_split_halves_it_on_the_requested_edge() {
        let mut l = Layout::default();
        l.split_window(Edge::Right, A);
        assert_eq!(l.tiles(AREA, 0.0), vec![(A, AREA)]);
        l.split(A, Edge::Left, B);
        assert_eq!(
            l.tiles(AREA, 0.0),
            vec![(B, Rect::new(0.0, 0.0, 500.0, 600.0)), (A, Rect::new(500.0, 0.0, 500.0, 600.0))]
        );
    }
}
