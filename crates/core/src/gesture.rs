//! Primary-button pointer gestures inside the root window.
//!
//! Answers: what does a press, the moves after it, and the release mean — a button click, a
//! chip click, a pane drag, a card resize, a divider drag, a window drag, or terminal input?
//! Must not know: the layout, terminals, or what an effect does. The app hit-tests with
//! `Scene::hit`, feeds the results here, and carries out the effects.
//!
//! Buttons and chips act on release over the same target, so a press can be abandoned by
//! moving away, as with native buttons.

use crate::geom::Point;
use crate::layout::PaneId;
use crate::scene::{ChromeButton, Hit, PaneButton, Resize};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Gesture {
    Idle,
    /// Pressed on chrome; a header press becomes a pane drag once it travels far enough.
    Pressed {
        on: Hit,
        at: Point,
    },
    DraggingPane(PaneId),
    ResizingCard(PaneId, Resize),
    DraggingDivider(usize),
    /// The press landed in a terminal; it owns the pointer until release.
    Terminal(PaneId),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Effect {
    None,
    Click(PaneId, PaneButton),
    Chrome(ChromeButton),
    Restore(PaneId),
    Focus(PaneId),
    /// A pane drag began; the app also treats it as the first `DragMove`.
    DragStart(PaneId),
    DragMove(PaneId),
    Drop(PaneId),
    CancelDrag(PaneId),
    ResizeCard(PaneId, Resize),
    ResizeDivider(usize),
    TerminalPress(PaneId),
    TerminalMotion(PaneId),
    TerminalRelease(PaneId),
    /// Hand the drag to the window system so it moves the window natively.
    DragWindow,
}

impl Gesture {
    pub fn press(&mut self, on: Hit, at: Point) -> Effect {
        let (next, effect) = match on {
            Hit::Header(p) => (Gesture::Pressed { on, at }, Effect::Focus(p)),
            Hit::Button(..) | Hit::Chip(_) | Hit::Chrome(_) => (Gesture::Pressed { on, at }, Effect::None),
            Hit::Resize(p, r) => (Gesture::ResizingCard(p, r), Effect::Focus(p)),
            Hit::Divider(i) => (Gesture::DraggingDivider(i), Effect::None),
            Hit::Content(p) => (Gesture::Terminal(p), Effect::TerminalPress(p)),
            Hit::DragWindow => (Gesture::Idle, Effect::DragWindow),
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
            Gesture::DraggingPane(p) => Effect::DragMove(p),
            Gesture::ResizingCard(p, r) => Effect::ResizeCard(p, r),
            Gesture::DraggingDivider(i) => Effect::ResizeDivider(i),
            Gesture::Terminal(p) => Effect::TerminalMotion(p),
            Gesture::Pressed { .. } | Gesture::Idle => Effect::None,
        }
    }

    /// Esc during a pane drag puts everything back.
    pub fn cancel(&mut self) -> Effect {
        match *self {
            Gesture::DraggingPane(p) => {
                *self = Gesture::Idle;
                Effect::CancelDrag(p)
            }
            _ => Effect::None,
        }
    }

    pub fn release(&mut self, over: Hit) -> Effect {
        let effect = match *self {
            Gesture::Pressed { on, .. } if on != over => Effect::None,
            Gesture::Pressed { on: Hit::Button(p, b), .. } => Effect::Click(p, b),
            Gesture::Pressed { on: Hit::Chip(p), .. } => Effect::Restore(p),
            Gesture::Pressed { on: Hit::Chrome(b), .. } => Effect::Chrome(b),
            Gesture::DraggingPane(p) => Effect::Drop(p),
            Gesture::Terminal(p) => Effect::TerminalRelease(p),
            Gesture::Pressed { .. } | Gesture::ResizingCard(..) | Gesture::DraggingDivider(_) | Gesture::Idle => {
                Effect::None
            }
        };
        *self = Gesture::Idle;
        effect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PaneId = PaneId(1);
    const O: Point = Point::new(10.0, 10.0);

    #[test]
    fn a_header_press_focuses_and_becomes_a_pane_drag_past_the_threshold() {
        let mut g = Gesture::Idle;
        assert_eq!(g.press(Hit::Header(A), O), Effect::Focus(A));
        assert_eq!(g.moved(Point::new(12.0, 11.0), 4.0), Effect::None);
        assert_eq!(g.moved(Point::new(14.0, 10.0), 4.0), Effect::DragStart(A));
        assert_eq!(g.moved(Point::new(50.0, 50.0), 4.0), Effect::DragMove(A));
        assert_eq!(g.release(Hit::Nothing), Effect::Drop(A));
        assert_eq!(g, Gesture::Idle);
    }

    #[test]
    fn a_header_click_without_travel_only_focuses() {
        let mut g = Gesture::Idle;
        g.press(Hit::Header(A), O);
        assert_eq!(g.release(Hit::Header(A)), Effect::None);
    }

    #[test]
    fn cancel_ends_a_pane_drag_without_dropping() {
        let mut g = Gesture::Idle;
        g.press(Hit::Header(A), O);
        g.moved(Point::new(50.0, 50.0), 4.0);
        assert_eq!(g.cancel(), Effect::CancelDrag(A));
        assert_eq!(g.release(Hit::Nothing), Effect::None);
        assert_eq!(g.cancel(), Effect::None, "nothing to cancel");
    }

    #[test]
    fn a_border_press_resizes_the_card_on_every_move_and_raises_it_first() {
        let mut g = Gesture::Idle;
        let r = Resize { right: true, ..Resize::default() };
        assert_eq!(g.press(Hit::Resize(A, r), O), Effect::Focus(A));
        assert_eq!(g.moved(Point::new(11.0, 10.0), 4.0), Effect::ResizeCard(A, r));
        assert_eq!(g.release(Hit::Nothing), Effect::None);
    }

    #[test]
    fn a_divider_press_resizes_tiles_on_every_move() {
        let mut g = Gesture::Idle;
        assert_eq!(g.press(Hit::Divider(2), O), Effect::None);
        assert_eq!(g.moved(Point::new(11.0, 10.0), 4.0), Effect::ResizeDivider(2));
        assert_eq!(g.release(Hit::Nothing), Effect::None);
    }

    #[test]
    fn a_terminal_press_captures_the_pointer_until_release() {
        let mut g = Gesture::Idle;
        assert_eq!(g.press(Hit::Content(A), O), Effect::TerminalPress(A));
        assert_eq!(g.moved(Point::new(500.0, 0.0), 4.0), Effect::TerminalMotion(A));
        assert_eq!(g.release(Hit::Nothing), Effect::TerminalRelease(A));
    }

    #[test]
    fn a_press_on_the_empty_top_bar_hands_the_drag_to_the_system() {
        let mut g = Gesture::Idle;
        assert_eq!(g.press(Hit::DragWindow, O), Effect::DragWindow);
        assert_eq!(g, Gesture::Idle, "the system owns the rest of this drag");
    }

    #[test]
    fn buttons_chips_and_chrome_act_only_when_released_over_the_same_target() {
        let mut g = Gesture::Idle;
        let zoom = Hit::Button(A, PaneButton::Zoom);
        g.press(zoom, O);
        assert_eq!(g.release(zoom), Effect::Click(A, PaneButton::Zoom));
        g.press(zoom, O);
        assert_eq!(g.release(Hit::Button(A, PaneButton::Close)), Effect::None, "moved off");
        g.press(Hit::Chip(A), O);
        assert_eq!(g.release(Hit::Chip(A)), Effect::Restore(A));
        let launcher = Hit::Chrome(ChromeButton::Launcher);
        g.press(launcher, O);
        assert_eq!(g.release(launcher), Effect::Chrome(ChromeButton::Launcher));
        assert_eq!(g, Gesture::Idle);
    }
}
