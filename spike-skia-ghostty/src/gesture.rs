//! Primary-button pointer gestures inside one window.
//!
//! Answers: what does a press, the moves after it, and the release mean — a button click,
//! a dock-chip click, a pane drag, a divider drag, or terminal mouse input?
//! Must not know: the layout tree, terminals, other windows, or what an effect does. The app
//! hit-tests with `chrome::Scene::hit`, feeds the results here, and carries out the effects.
//!
//! Buttons and chips act on release over the same target, so a press can be abandoned by
//! moving away, as with native buttons.

use crate::chrome::{Button, Hit};
use crate::layout::{PaneId, Point};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Gesture {
    Idle,
    /// Pressed on chrome; becomes a pane drag if a header press travels far enough.
    Pressed { on: Hit, at: Point },
    DraggingPane(PaneId),
    DraggingDivider(usize),
    /// The press landed in a terminal; it owns the pointer until release.
    Terminal(PaneId),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    None,
    Focus(PaneId),
    TerminalPress(PaneId),
    TerminalMotion(PaneId),
    TerminalRelease(PaneId),
    Click(PaneId, Button),
    Restore(PaneId),
    /// A pane drag began; the app also treats this as the first `DragMove`.
    DragStart(PaneId),
    DragMove,
    Drop,
    ResizeDivider(usize),
}

impl Gesture {
    pub fn press(&mut self, on: Hit, at: Point) -> Effect {
        let (next, effect) = match on {
            Hit::Content(p) => (Gesture::Terminal(p), Effect::TerminalPress(p)),
            Hit::Header(p) => (Gesture::Pressed { on, at }, Effect::Focus(p)),
            Hit::Button(..) | Hit::Chip(_) => (Gesture::Pressed { on, at }, Effect::None),
            Hit::Divider(i) => (Gesture::DraggingDivider(i), Effect::None),
            Hit::Nothing => (Gesture::Idle, Effect::None),
        };
        *self = next;
        effect
    }

    /// `threshold` is the travel that turns a header press into a drag.
    pub fn moved(&mut self, at: Point, threshold: f32) -> Effect {
        match *self {
            Gesture::Pressed { on: Hit::Header(p), at: start } => {
                if (at.x - start.x).hypot(at.y - start.y) >= threshold {
                    *self = Gesture::DraggingPane(p);
                    Effect::DragStart(p)
                } else {
                    Effect::None
                }
            }
            Gesture::DraggingPane(_) => Effect::DragMove,
            Gesture::DraggingDivider(i) => Effect::ResizeDivider(i),
            Gesture::Terminal(p) => Effect::TerminalMotion(p),
            Gesture::Pressed { .. } | Gesture::Idle => Effect::None,
        }
    }

    pub fn release(&mut self, over: Hit) -> Effect {
        let effect = match *self {
            Gesture::Pressed { on: Hit::Button(p, b), .. } if over == Hit::Button(p, b) => Effect::Click(p, b),
            Gesture::Pressed { on: Hit::Chip(p), .. } if over == Hit::Chip(p) => Effect::Restore(p),
            Gesture::DraggingPane(_) => Effect::Drop,
            Gesture::Terminal(p) => Effect::TerminalRelease(p),
            Gesture::Pressed { .. } | Gesture::DraggingDivider(_) | Gesture::Idle => Effect::None,
        };
        *self = Gesture::Idle;
        effect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PaneId = PaneId(1);
    const O: Point = Point { x: 10.0, y: 10.0 };

    #[test]
    fn header_press_focuses_and_drag_starts_past_threshold() {
        let mut g = Gesture::Idle;
        assert_eq!(g.press(Hit::Header(A), O), Effect::Focus(A));
        assert_eq!(g.moved(Point::new(12.0, 11.0), 4.0), Effect::None);
        assert_eq!(g.moved(Point::new(14.0, 10.0), 4.0), Effect::DragStart(A));
        assert_eq!(g.moved(Point::new(50.0, 50.0), 4.0), Effect::DragMove);
        assert_eq!(g.release(Hit::Nothing), Effect::Drop);
        assert_eq!(g, Gesture::Idle);
    }

    #[test]
    fn header_click_without_travel_does_nothing_on_release() {
        let mut g = Gesture::Idle;
        g.press(Hit::Header(A), O);
        assert_eq!(g.release(Hit::Header(A)), Effect::None);
    }

    #[test]
    fn button_clicks_only_when_released_over_same_button() {
        let mut g = Gesture::Idle;
        g.press(Hit::Button(A, Button::Zoom), O);
        assert_eq!(g.moved(Point::new(90.0, 90.0), 4.0), Effect::None, "buttons never drag");
        assert_eq!(g.release(Hit::Button(A, Button::Zoom)), Effect::Click(A, Button::Zoom));
        g.press(Hit::Button(A, Button::Zoom), O);
        assert_eq!(g.release(Hit::Button(A, Button::Close)), Effect::None);
    }

    #[test]
    fn chip_restores_on_release_over_it() {
        let mut g = Gesture::Idle;
        g.press(Hit::Chip(A), O);
        assert_eq!(g.release(Hit::Chip(A)), Effect::Restore(A));
        g.press(Hit::Chip(A), O);
        assert_eq!(g.release(Hit::Nothing), Effect::None);
    }

    #[test]
    fn terminal_press_captures_pointer_until_release() {
        let mut g = Gesture::Idle;
        assert_eq!(g.press(Hit::Content(A), O), Effect::TerminalPress(A));
        assert_eq!(g.moved(Point::new(500.0, 0.0), 4.0), Effect::TerminalMotion(A));
        assert_eq!(g.release(Hit::Nothing), Effect::TerminalRelease(A));
    }

    #[test]
    fn divider_drag_resizes_on_every_move() {
        let mut g = Gesture::Idle;
        assert_eq!(g.press(Hit::Divider(2), O), Effect::None);
        assert_eq!(g.moved(Point::new(11.0, 10.0), 4.0), Effect::ResizeDivider(2));
        assert_eq!(g.release(Hit::Nothing), Effect::None);
    }
}
