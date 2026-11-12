//! Pane arrangement of one window: a binary split tree of tiled panes, at most one zoomed
//! pane, and the dock of minimized panes.
//!
//! Answers: where does each pane sit, and how does the arrangement change when a pane is
//! split off, moved, swapped, zoomed, minimized, restored, or removed?
//! Must not know: terminals, drawing, window-system events, or chrome (headers, dock bar
//! geometry). Units are whatever the caller passes; the app uses physical pixels.
//!
//! Invariants (checked by `debug_check` after every mutation in debug builds):
//! - every pane appears exactly once, either in the tree or in the dock;
//! - the tree is empty only when the dock is empty too (a window never shows nothing);
//! - the zoomed pane, if any, is in the tree;
//! - every split ratio lies strictly between 0 and 1.
//!
//! The tree follows Ghostty's split tree: binary splits with a ratio, new splits at 50%,
//! splitting resets zoom, removing a pane keeps zoom on the survivor.
//! @see https://github.com/ghostty-org/ghostty/blob/108cacb17ed8990840cd14abfce43d8ed8503ca1/src/datastruct/split_tree.zig

/// Stable identity of a pane for its whole life, across windows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct PaneId(pub u32);

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }
}

/// Axis-aligned rectangle; `contains` is half-open so adjacent rects never both claim a point.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    /// Cuts `self` along `axis` into two parts separated by `gap`; the first part gets
    /// `ratio` of the space left after the gap, rounded to whole units so that terminal
    /// grids stay pixel-aligned.
    fn split(&self, axis: Axis, ratio: f32, gap: f32) -> (Rect, Rect) {
        match axis {
            Axis::Horizontal => {
                let first = ((self.w - gap).max(0.0) * ratio).round();
                let second_x = self.x + first + gap;
                (
                    Rect::new(self.x, self.y, first, self.h),
                    Rect::new(second_x, self.y, (self.right() - second_x).max(0.0), self.h),
                )
            }
            Axis::Vertical => {
                let first = ((self.h - gap).max(0.0) * ratio).round();
                let second_y = self.y + first + gap;
                (
                    Rect::new(self.x, self.y, self.w, first),
                    Rect::new(self.x, second_y, self.w, (self.bottom() - second_y).max(0.0)),
                )
            }
        }
    }

    /// The half of `self` touching `edge`, as a fresh 50% split would produce it.
    fn half(&self, edge: Edge, gap: f32) -> Rect {
        let (first, second) = self.split(edge.axis(), 0.5, gap);
        if edge.is_leading() { first } else { second }
    }
}

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

/// Where a dragged pane lands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drop {
    /// Split the target pane and put the dragged pane on `Edge` of it.
    Split(PaneId, Edge),
    /// Exchange places with the target pane (only for a pane already tiled here).
    Swap(PaneId),
    /// Split the whole window and put the dragged pane along `Edge`.
    Window(Edge),
}

/// The gap between the two children of one split, for hit testing and resizing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Divider {
    /// Pre-order index of the split; valid until the next structural change.
    pub index: usize,
    pub axis: Axis,
    /// The gap strip itself (zero-sized when `gap` is zero).
    pub rect: Rect,
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

/// A minimized pane and where it came from, so restoring puts it back beside the same pane.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Docked {
    pane: PaneId,
    home: Option<(PaneId, Edge)>,
}

/// What detaching a leaf from the tree left behind.
struct Detached {
    /// The leaf of the former sibling that sat right next to the detached pane.
    neighbor: PaneId,
    /// Side of `neighbor` on which the detached pane sat.
    edge: Edge,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    root: Option<Node>,
    zoomed: Option<PaneId>,
    dock: Vec<Docked>,
    revision: u64,
}

/// Divider ratio when pane geometry cannot satisfy the minimum size on both sides.
const FALLBACK_RATIO: f32 = 0.5;
/// Fraction of a pane's width/height, measured from an edge, that targets a split on that edge;
/// the rest is the center (swap) zone.
const EDGE_ZONE: f32 = 0.3;

impl Layout {
    pub fn new(pane: PaneId) -> Layout {
        Layout { root: Some(Node::Leaf(pane)), zoomed: None, dock: Vec::new(), revision: 0 }
    }

    /// Changes whenever panes are added, removed, moved, zoomed, minimized or restored, so a
    /// gesture that captured a pane or divider index can tell it may be stale. Divider drags
    /// keep it: they change sizes only.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.is_tiled(pane) || self.is_docked(pane)
    }

    pub fn is_tiled(&self, pane: PaneId) -> bool {
        self.root.as_ref().is_some_and(|n| n.contains(pane))
    }

    /// Tiled and not hidden behind another pane's zoom.
    pub fn is_visible(&self, pane: PaneId) -> bool {
        self.zoomed.map_or_else(|| self.is_tiled(pane), |z| z == pane)
    }

    pub fn is_docked(&self, pane: PaneId) -> bool {
        self.dock.iter().any(|d| d.pane == pane)
    }

    /// Tiled panes in reading order (left before right, top before bottom).
    pub fn tiled(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.leaves(&mut out);
        }
        out
    }

    /// Minimized panes in the order they were minimized.
    pub fn docked(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.dock.iter().map(|d| d.pane)
    }

    pub fn zoomed(&self) -> Option<PaneId> {
        self.zoomed
    }

    /// Splits tiled pane `at` and puts the new `pane` on `edge` of it, at 50%. Resets zoom.
    pub fn split(&mut self, at: PaneId, edge: Edge, pane: PaneId) {
        assert!(!self.contains(pane), "{pane:?} is already in this layout");
        let leaf = self.root.as_mut().and_then(|r| r.find_leaf_mut(at));
        let leaf = leaf.unwrap_or_else(|| panic!("{at:?} is not tiled in this layout"));
        let old = std::mem::replace(leaf, Node::Leaf(pane));
        *leaf = Node::split(edge, old, Node::Leaf(pane));
        self.zoomed = None;
        self.changed();
    }

    /// Splits the whole window and puts `pane` along `edge`, at 50%. Resets zoom.
    pub fn split_window(&mut self, edge: Edge, pane: PaneId) {
        assert!(!self.contains(pane), "{pane:?} is already in this layout");
        self.root = Some(match self.root.take() {
            Some(old) => Node::split(edge, old, Node::Leaf(pane)),
            None => Node::Leaf(pane),
        });
        self.zoomed = None;
        self.changed();
    }

    /// Removes `pane` from the tree or the dock. Returns the pane that should take focus,
    /// or `None` when the layout is now empty. If the last tiled pane goes while panes are
    /// docked, the first docked pane is restored so the window keeps showing a terminal.
    pub fn remove(&mut self, pane: PaneId) -> Option<PaneId> {
        if let Some(i) = self.dock.iter().position(|d| d.pane == pane) {
            self.dock.remove(i);
            self.changed();
            return self.tiled().first().copied();
        }
        let next = match self.detach(pane) {
            Some(d) => Some(d.neighbor),
            None if self.dock.is_empty() => None,
            None => {
                let first = self.dock.remove(0);
                self.root = Some(Node::Leaf(first.pane));
                Some(first.pane)
            }
        };
        self.changed();
        next
    }

    /// Exchanges the positions of two tiled panes; ratios and zoom stay as they are.
    pub fn swap(&mut self, a: PaneId, b: PaneId) {
        assert!(self.is_tiled(a) && self.is_tiled(b), "swap needs two tiled panes");
        if let Some(root) = &mut self.root {
            root.swap(a, b);
        }
        self.changed();
    }

    /// Zooms `pane` to fill the window, or unzooms if it is already zoomed.
    pub fn toggle_zoom(&mut self, pane: PaneId) {
        assert!(self.is_tiled(pane), "only a tiled pane can zoom");
        self.zoomed = if self.zoomed == Some(pane) { None } else { Some(pane) };
        self.changed();
    }

    /// Moves a tiled pane to the dock and returns the pane that should take focus. Refuses
    /// (returns `None`, nothing changes) when `pane` is the only tiled pane.
    pub fn minimize(&mut self, pane: PaneId) -> Option<PaneId> {
        assert!(self.is_tiled(pane), "only a tiled pane can be minimized");
        if self.root == Some(Node::Leaf(pane)) {
            return None;
        }
        let detached = self.detach(pane).expect("pane is tiled and not the root leaf");
        self.dock.push(Docked { pane, home: Some((detached.neighbor, detached.edge)) });
        self.changed();
        Some(detached.neighbor)
    }

    /// Puts a docked pane back beside the pane it was minimized from, or along that same
    /// edge of the window when that pane is no longer tiled. Resets zoom.
    pub fn restore(&mut self, pane: PaneId) {
        let i = self.dock.iter().position(|d| d.pane == pane);
        let i = i.unwrap_or_else(|| panic!("{pane:?} is not docked"));
        let docked = self.dock.remove(i);
        match docked.home {
            Some((at, edge)) if self.is_tiled(at) => self.split(at, edge, pane),
            Some((_, edge)) => self.split_window(edge, pane),
            None => self.split_window(Edge::Right, pane),
        }
    }

    /// Moves `pane` to `drop` and resets zoom, so the moved pane is visible. `pane` may be
    /// tiled here already or come from another window. A drop computed by `drop_target` for
    /// this layout and pane is always valid here.
    pub fn apply(&mut self, pane: PaneId, drop: Drop) {
        self.zoomed = None;
        match drop {
            Drop::Swap(target) => self.swap(pane, target),
            Drop::Split(target, edge) => {
                assert_ne!(pane, target, "cannot split a pane off itself");
                if self.is_tiled(pane) {
                    self.detach(pane).expect("target is tiled, so pane is not alone");
                }
                self.split(target, edge, pane);
            }
            Drop::Window(edge) => {
                if self.is_tiled(pane) {
                    self.detach(pane).expect("a window drop needs another tiled pane");
                }
                self.split_window(edge, pane);
            }
        }
    }

    /// Rectangles of the visible panes inside `area`, `gap` apart. A zoomed pane fills `area`.
    pub fn tiles(&self, area: Rect, gap: f32) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        match (self.zoomed, &self.root) {
            (Some(z), _) => out.push((z, area)),
            (None, Some(root)) => root.tiles(area, gap, &mut out),
            (None, None) => {}
        }
        out
    }

    /// Gaps between split children, in pre-order. None while a pane is zoomed.
    pub fn dividers(&self, area: Rect, gap: f32) -> Vec<Divider> {
        let mut out = Vec::new();
        if let (None, Some(root)) = (self.zoomed, &self.root) {
            root.dividers(area, gap, &mut out);
        }
        out
    }

    /// Moves divider `index` so its gap is centered at `at` (an x for horizontal splits,
    /// a y for vertical ones), keeping both sides at least `min` long.
    pub fn drag_divider(&mut self, area: Rect, gap: f32, index: usize, at: f32, min: f32) {
        if self.zoomed.is_some() {
            return;
        }
        let Some(root) = &mut self.root else { return };
        let mut counter = 0;
        let Some((split, span)) = root.split_at(area, gap, index, &mut counter) else { return };
        let (start, len) = match split.axis {
            Axis::Horizontal => (span.x, span.w - gap),
            Axis::Vertical => (span.y, span.h - gap),
        };
        let lo = min / len;
        split.ratio = if len <= 0.0 || lo >= 0.5 {
            FALLBACK_RATIO
        } else {
            ((at - start - gap / 2.0) / len).clamp(lo, 1.0 - lo)
        };
        self.debug_check();
    }

    /// Where `dragged` would land if released at `p`. Points within `band` of the area's
    /// border target the whole window; elsewhere the pane under `p` is split on its nearest
    /// edge, or swapped when `p` is in its center and `dragged` is tiled here. Returns `None`
    /// over gaps, over `dragged` itself, and when `dragged` is the only tiled pane here.
    pub fn drop_target(
        &self,
        area: Rect,
        gap: f32,
        band: f32,
        p: Point,
        dragged: PaneId,
    ) -> Option<Drop> {
        if !area.contains(p) {
            return None;
        }
        let local = self.is_tiled(dragged);
        if local && self.tiled().len() == 1 {
            return None;
        }
        let to_border = [
            (p.x - area.x, Edge::Left),
            (area.right() - p.x, Edge::Right),
            (p.y - area.y, Edge::Top),
            (area.bottom() - p.y, Edge::Bottom),
        ];
        let (d, edge) = nearest(to_border);
        if d < band {
            return Some(Drop::Window(edge));
        }
        let (target, r) = self.tiles(area, gap).into_iter().find(|(_, r)| r.contains(p))?;
        if target == dragged {
            return None;
        }
        let to_edge = [
            ((p.x - r.x) / r.w, Edge::Left),
            ((r.right() - p.x) / r.w, Edge::Right),
            ((p.y - r.y) / r.h, Edge::Top),
            ((r.bottom() - p.y) / r.h, Edge::Bottom),
        ];
        let (d, edge) = nearest(to_edge);
        Some(if local && d > EDGE_ZONE { Drop::Swap(target) } else { Drop::Split(target, edge) })
    }

    /// The zone `drop` covers in the current arrangement, for a preview. For a pane from
    /// another window this is exactly where it lands; a pane already tiled here can end up
    /// larger, because taking it out first lets its neighbors grow.
    pub fn drop_preview(&self, area: Rect, gap: f32, drop: Drop) -> Rect {
        let tile = |pane| {
            let tiles = self.tiles(area, gap);
            tiles.into_iter().find(|(p, _)| *p == pane).expect("drop target is visible").1
        };
        match drop {
            Drop::Split(target, edge) => tile(target).half(edge, gap),
            Drop::Swap(target) => tile(target),
            Drop::Window(edge) => area.half(edge, gap),
        }
    }

    /// Takes tiled leaf `pane` out of the tree; its parent split collapses into the sibling.
    /// Returns `None` when `pane` was the root leaf, leaving the tree empty: callers that must
    /// keep a pane on screen check for that case first.
    fn detach(&mut self, pane: PaneId) -> Option<Detached> {
        assert!(self.is_tiled(pane), "{pane:?} is not tiled in this layout");
        if self.zoomed == Some(pane) {
            self.zoomed = None;
        }
        let root = self.root.as_mut().expect("a tiled pane implies a root");
        if *root == Node::Leaf(pane) {
            self.root = None;
            return None;
        }
        Some(root.detach(pane).expect("a tiled non-root leaf has a parent split"))
    }

    /// Records a structural change.
    fn changed(&mut self) {
        self.revision += 1;
        self.debug_check();
    }

    /// Panics when an invariant from the module header no longer holds.
    fn debug_check(&self) {
        if !cfg!(debug_assertions) {
            return;
        }
        let tiled = self.tiled();
        let mut all: Vec<PaneId> = tiled.iter().copied().chain(self.docked()).collect();
        all.sort();
        let len = all.len();
        all.dedup();
        assert_eq!(len, all.len(), "a pane appears twice: {self:?}");
        assert!(self.root.is_some() || self.dock.is_empty(), "docked panes but no tiles: {self:?}");
        if let Some(z) = self.zoomed {
            assert!(tiled.contains(&z), "zoomed pane is not tiled: {self:?}");
        }
        if let Some(root) = &self.root {
            assert!(root.ratios_valid(), "split ratio out of (0, 1): {self:?}");
        }
    }
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

    fn tiles(&self, r: Rect, gap: f32, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(p) => out.push((*p, r)),
            Node::Split(s) => {
                let (a, b) = r.split(s.axis, s.ratio, gap);
                s.first.tiles(a, gap, out);
                s.second.tiles(b, gap, out);
            }
        }
    }

    fn dividers(&self, r: Rect, gap: f32, out: &mut Vec<Divider>) {
        let Node::Split(s) = self else { return };
        let (a, b) = r.split(s.axis, s.ratio, gap);
        let rect = match s.axis {
            Axis::Horizontal => Rect::new(a.right(), r.y, b.x - a.right(), r.h),
            Axis::Vertical => Rect::new(r.x, a.bottom(), r.w, b.y - a.bottom()),
        };
        out.push(Divider { index: out.len(), axis: s.axis, rect });
        s.first.dividers(a, gap, out);
        s.second.dividers(b, gap, out);
    }

    /// The split with pre-order `index` and the rectangle it divides.
    fn split_at(
        &mut self,
        r: Rect,
        gap: f32,
        index: usize,
        counter: &mut usize,
    ) -> Option<(&mut Split, Rect)> {
        let Node::Split(s) = self else { return None };
        if *counter == index {
            return Some((s, r));
        }
        *counter += 1;
        let (a, b) = r.split(s.axis, s.ratio, gap);
        if let Some(found) = s.first.split_at(a, gap, index, counter) {
            return Some(found);
        }
        s.second.split_at(b, gap, index, counter)
    }

    fn ratios_valid(&self) -> bool {
        match self {
            Node::Leaf(_) => true,
            Node::Split(s) => {
                s.ratio > 0.0 && s.ratio < 1.0 && s.first.ratios_valid() && s.second.ratios_valid()
            }
        }
    }
}

fn nearest(candidates: [(f32, Edge); 4]) -> (f32, Edge) {
    candidates.into_iter().fold((f32::INFINITY, Edge::Left), |best, c| if c.0 < best.0 { c } else { best })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PaneId = PaneId(1);
    const B: PaneId = PaneId(2);
    const C: PaneId = PaneId(3);
    const D: PaneId = PaneId(4);
    const AREA: Rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 };

    fn rect_of(l: &Layout, pane: PaneId) -> Rect {
        l.tiles(AREA, 0.0).into_iter().find(|(p, _)| *p == pane).unwrap().1
    }

    /// A | B over C:  ┌─A─┬─B─┐
    ///                └───C───┘
    fn abc() -> Layout {
        let mut l = Layout::new(A);
        l.split(A, Edge::Bottom, C);
        l.split(A, Edge::Right, B);
        l
    }

    #[test]
    fn revision_changes_with_structure_but_not_with_divider_drags() {
        let mut l = abc();
        let mut seen = vec![l.revision()];
        let mut step = |l: &Layout| {
            assert!(!seen.contains(&l.revision()), "revision repeated: {l:?}");
            seen.push(l.revision());
        };
        l.split(A, Edge::Left, D);
        step(&l);
        l.swap(A, D);
        step(&l);
        l.toggle_zoom(A);
        step(&l);
        l.minimize(B);
        step(&l);
        l.restore(B);
        step(&l);
        l.apply(C, Drop::Window(Edge::Top));
        step(&l);
        l.remove(D);
        step(&l);
        let before = l.revision();
        l.drag_divider(AREA, 0.0, 0, 200.0, 10.0);
        assert_eq!(l.revision(), before, "resizing keeps pane identities and divider indices");
    }

    #[test]
    fn split_places_new_pane_on_requested_edge_at_half() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Left, B);
        assert_eq!(l.tiled(), vec![B, A]);
        assert_eq!(rect_of(&l, B), Rect::new(0.0, 0.0, 500.0, 600.0));
        assert_eq!(rect_of(&l, A), Rect::new(500.0, 0.0, 500.0, 600.0));
    }

    #[test]
    fn gap_separates_tiles_and_rounds_to_whole_units() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Right, B);
        let tiles = l.tiles(Rect::new(0.0, 0.0, 101.0, 50.0), 6.0);
        assert_eq!(tiles[0].1, Rect::new(0.0, 0.0, 48.0, 50.0));
        assert_eq!(tiles[1].1, Rect::new(54.0, 0.0, 47.0, 50.0));
        let div = l.dividers(Rect::new(0.0, 0.0, 101.0, 50.0), 6.0);
        assert_eq!(div[0].rect, Rect::new(48.0, 0.0, 6.0, 50.0));
    }

    #[test]
    fn split_window_wraps_the_whole_tree() {
        let mut l = abc();
        l.split_window(Edge::Top, D);
        assert_eq!(rect_of(&l, D), Rect::new(0.0, 0.0, 1000.0, 300.0));
        assert_eq!(rect_of(&l, C), Rect::new(0.0, 450.0, 1000.0, 150.0));
    }

    #[test]
    fn remove_collapses_parent_and_focuses_adjacent_leaf() {
        let mut l = abc();
        assert_eq!(l.remove(C), Some(A), "A is the first leaf of A | B, which C sat below");
        assert_eq!(l.tiled(), vec![A, B]);
        assert_eq!(rect_of(&l, B), Rect::new(500.0, 0.0, 500.0, 600.0));
    }

    #[test]
    fn remove_trailing_pane_focuses_the_leaf_that_touched_it() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Right, B);
        l.split(B, Edge::Right, C);
        // A | (B | C) then D right of everything: (A | (B | C)) | D
        l.split_window(Edge::Right, D);
        assert_eq!(l.remove(D), Some(C), "C is the rightmost leaf of D's sibling");
    }

    #[test]
    fn remove_last_pane_empties_layout() {
        let mut l = Layout::new(A);
        assert_eq!(l.remove(A), None);
        assert!(l.tiled().is_empty() && !l.contains(A));
    }

    #[test]
    fn removing_last_tiled_pane_restores_a_docked_one() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Right, B);
        l.minimize(B);
        assert_eq!(l.remove(A), Some(B));
        assert_eq!(l.tiled(), vec![B]);
        assert_eq!(l.docked().count(), 0);
    }

    #[test]
    fn swap_exchanges_positions_and_keeps_ratios() {
        let mut l = abc();
        l.drag_divider(AREA, 0.0, 0, 200.0, 10.0);
        let before = rect_of(&l, C);
        l.swap(A, C);
        assert_eq!(rect_of(&l, A), before);
        assert_eq!(l.tiled(), vec![C, B, A]);
    }

    #[test]
    fn zoom_fills_area_and_split_resets_it() {
        let mut l = abc();
        l.toggle_zoom(B);
        assert_eq!(l.tiles(AREA, 4.0), vec![(B, AREA)]);
        assert!(l.dividers(AREA, 4.0).is_empty());
        l.split(A, Edge::Right, D);
        assert_eq!(l.zoomed(), None);
    }

    #[test]
    fn zoom_hides_other_tiled_panes() {
        let mut l = abc();
        assert!(l.is_visible(A) && l.is_visible(B));
        l.toggle_zoom(B);
        assert!(!l.is_visible(A) && l.is_visible(B));
        l.minimize(C);
        assert!(!l.is_visible(C), "docked panes are never visible");
    }

    #[test]
    fn zoom_toggles_off_and_survives_removal_of_other_panes() {
        let mut l = abc();
        l.toggle_zoom(B);
        l.remove(C);
        assert_eq!(l.zoomed(), Some(B));
        l.toggle_zoom(B);
        assert_eq!(l.zoomed(), None);
    }

    #[test]
    fn removing_or_minimizing_the_zoomed_pane_unzooms() {
        let mut l = abc();
        l.toggle_zoom(B);
        l.minimize(B);
        assert_eq!(l.zoomed(), None);
        let mut l = abc();
        l.toggle_zoom(C);
        l.remove(C);
        assert_eq!(l.zoomed(), None);
    }

    #[test]
    fn minimize_then_restore_returns_pane_to_its_place() {
        let mut l = abc();
        let before = l.tiles(AREA, 0.0);
        assert_eq!(l.minimize(B), Some(A));
        assert_eq!(l.tiled(), vec![A, C]);
        assert_eq!(l.docked().collect::<Vec<_>>(), vec![B]);
        l.restore(B);
        assert_eq!(l.tiles(AREA, 0.0), before);
    }

    #[test]
    fn minimize_refuses_the_last_tiled_pane() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Right, B);
        l.minimize(A);
        let snapshot = l.clone();
        assert_eq!(l.minimize(B), None);
        assert_eq!(l, snapshot);
    }

    #[test]
    fn restore_falls_back_to_window_edge_when_home_pane_is_gone() {
        let mut l = abc();
        l.minimize(B); // home: right of A
        l.remove(A);
        l.restore(B);
        assert_eq!(rect_of(&l, B), Rect::new(500.0, 0.0, 500.0, 600.0));
    }

    #[test]
    fn dock_keeps_minimize_order() {
        let mut l = abc();
        l.minimize(C);
        l.minimize(A);
        assert_eq!(l.docked().collect::<Vec<_>>(), vec![C, A]);
        l.restore(C);
        assert_eq!(l.docked().collect::<Vec<_>>(), vec![A]);
    }

    #[test]
    fn drag_divider_clamps_to_minimum_size() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Right, B);
        l.drag_divider(AREA, 0.0, 0, 5.0, 100.0);
        assert_eq!(rect_of(&l, A).w, 100.0);
        l.drag_divider(AREA, 0.0, 0, 990.0, 100.0);
        assert_eq!(rect_of(&l, B).w, 100.0);
        l.drag_divider(AREA, 0.0, 0, 300.0, 100.0);
        assert_eq!(rect_of(&l, A).w, 300.0);
    }

    #[test]
    fn drag_divider_addresses_nested_splits_in_preorder() {
        let mut l = abc(); // divider 0: A|B over C (vertical), divider 1: A|B (horizontal)
        l.drag_divider(AREA, 0.0, 1, 250.0, 10.0);
        assert_eq!(rect_of(&l, A).w, 250.0);
        assert_eq!(rect_of(&l, C).h, 300.0);
    }

    #[test]
    fn drag_divider_falls_back_when_minimum_cannot_fit() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Right, B);
        l.drag_divider(AREA, 0.0, 0, 100.0, 600.0);
        assert_eq!(rect_of(&l, A).w, 500.0);
    }

    #[test]
    fn drop_target_edges_center_and_window_band() {
        let l = abc(); // A = (0,0,500,300), B = (500,0,500,300), C = (0,300,1000,300)
        let at = |x, y| l.drop_target(AREA, 0.0, 20.0, Point::new(x, y), C);
        assert_eq!(at(5.0, 100.0), Some(Drop::Window(Edge::Left)));
        assert_eq!(at(60.0, 150.0), Some(Drop::Split(A, Edge::Left)));
        assert_eq!(at(250.0, 280.0), Some(Drop::Split(A, Edge::Bottom)));
        assert_eq!(at(250.0, 150.0), Some(Drop::Swap(A)));
        assert_eq!(at(500.0, 400.0), None, "over the dragged pane itself");
        assert_eq!(at(-1.0, 100.0), None, "outside the area");
    }

    #[test]
    fn foreign_pane_has_no_swap_zone() {
        let l = abc();
        let d = l.drop_target(AREA, 0.0, 20.0, Point::new(250.0, 140.0), D);
        assert_eq!(d, Some(Drop::Split(A, Edge::Top)));
    }

    #[test]
    fn sole_tiled_pane_has_no_drop_target_in_its_own_window() {
        let mut l = Layout::new(A);
        l.split(A, Edge::Right, B);
        l.minimize(B);
        assert_eq!(l.drop_target(AREA, 0.0, 20.0, Point::new(5.0, 5.0), A), None);
    }

    #[test]
    fn apply_moves_a_tiled_pane() {
        let mut l = abc();
        l.apply(C, Drop::Split(B, Edge::Right));
        assert_eq!(l.tiled(), vec![A, B, C]);
        assert_eq!(rect_of(&l, C), Rect::new(750.0, 0.0, 250.0, 600.0));
        l.apply(A, Drop::Window(Edge::Bottom));
        assert_eq!(l.tiled(), vec![B, C, A]);
        l.apply(A, Drop::Swap(B));
        assert_eq!(l.tiled(), vec![A, C, B]);
    }

    #[test]
    fn apply_resets_zoom_so_the_moved_pane_shows() {
        let mut l = abc();
        l.toggle_zoom(C);
        l.apply(A, Drop::Swap(C));
        assert_eq!(l.zoomed(), None);
        assert!(l.is_visible(A));
    }

    #[test]
    fn apply_inserts_a_foreign_pane() {
        let mut l = abc();
        l.apply(D, Drop::Split(C, Edge::Left));
        assert_eq!(rect_of(&l, D), Rect::new(0.0, 300.0, 500.0, 300.0));
    }

    #[test]
    fn preview_matches_where_a_foreign_pane_lands() {
        for drop in [Drop::Split(A, Edge::Top), Drop::Split(C, Edge::Bottom), Drop::Window(Edge::Right)] {
            let mut l = abc();
            let preview = l.drop_preview(AREA, 0.0, drop);
            l.apply(D, drop);
            assert_eq!(preview, rect_of(&l, D), "{drop:?}");
        }
    }

    #[test]
    #[should_panic(expected = "is not tiled")]
    fn removing_an_unknown_pane_panics() {
        abc().remove(D);
    }

    #[test]
    #[should_panic(expected = "already in this layout")]
    fn inserting_a_pane_twice_panics() {
        let mut l = abc();
        l.split(A, Edge::Right, B);
    }
}
