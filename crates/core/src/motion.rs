//! Layout transitions (ADR 10): panes spring from where they were drawn to where the layout
//! now puts them.
//!
//! Answers: where is each pane drawn at a given moment while the arrangement changes?
//! Must not know: the clock (callers pass `now`), layout rules, windows, or drawing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::geom::Rect;
use crate::layout::PaneId;

/// One rect's offset from its target, as x, y, w, h, and how fast that offset changes, at
/// `start`. It decays to zero along the spring, so a target that moves on its own (the window
/// is resized) carries the pane along at once (ADR 10).
#[derive(Clone, Copy, Debug)]
struct Spring {
    start: Instant,
    offset: [f32; 4],
    velocity: [f32; 4],
}

impl Spring {
    /// Offset and velocity at `now` for angular frequency `omega`:
    /// d(t) = (d0 + (v0 + ω d0) t) e^(−ω t).
    fn at(&self, omega: f32, now: Instant) -> ([f32; 4], [f32; 4]) {
        let t = now.saturating_duration_since(self.start).as_secs_f32();
        let decay = (-omega * t).exp();
        let mut d = [0.0; 4];
        let mut v = [0.0; 4];
        for i in 0..4 {
            let (d0, v0) = (self.offset[i], self.velocity[i]);
            let b = v0 + omega * d0;
            d[i] = (d0 + b * t) * decay;
            v[i] = (b - omega * (d0 + b * t)) * decay;
        }
        (d, v)
    }
}

fn parts(r: Rect) -> [f32; 4] {
    [r.x, r.y, r.w, r.h]
}

/// The springs of panes in flight, and the panes the latest change took out.
pub struct Motion {
    /// The springs' angular frequency, 2π / duration.
    omega: f32,
    /// How long a spring runs before it stops on its target.
    settle: Duration,
    springs: HashMap<PaneId, Spring>,
    /// Where each pane the latest change took out was drawn, and when the change came.
    left: Vec<(PaneId, Rect)>,
    left_at: Option<Instant>,
}

impl Motion {
    /// Panes move along SwiftUI's `Spring(duration:bounce: 0)`, which is critically damped with
    /// angular frequency 2π / `duration`; measured on macOS 26.5, `Spring()` (0.5 s) has
    /// stiffness 157.91 and damping 25.13. A spring stops on its target after 1.6 times
    /// `duration`, when what is left of the move is under 0.05%, half a pixel over 1,000,
    /// whatever the duration. That is `Spring().settlingDuration`, 0.8 s, but not Apple's for
    /// every duration: measured, 0.1, 0.25, 1 and 2 s settle after 0.2, 0.4, 1.5 and 3 s, the
    /// first multiple of 0.1 s at which under 0.1% is left. A zero `duration` moves nothing.
    /// @see https://developer.apple.com/documentation/swiftui/spring
    pub fn new(duration: Duration) -> Motion {
        Motion {
            omega: 2.0 * std::f32::consts::PI / duration.as_secs_f32(),
            settle: duration.saturating_mul(8) / 5,
            springs: HashMap::new(),
            left: Vec::new(),
            left_at: None,
        }
    }

    /// The arrangement changed at `now`: each pane had target rect `before` and now has
    /// `after`. A pane in both springs from where it is drawn at `now` and keeps its speed,
    /// so a change in mid-flight bends the path instead of jumping; a pane that comes back
    /// while still leaving springs from where it was left. Panes only in `before` start
    /// leaving; those still leaving from earlier changes stay so until those settle.
    pub fn retarget(&mut self, before: &[(PaneId, Rect)], after: &[(PaneId, Rect)], now: Instant) {
        if self.settle.is_zero() {
            return; // nothing moves, and the infinite `omega` stays out of the math
        }
        let left: Vec<(PaneId, Rect)> = self.leaving(now).collect();
        let stays = |p: &PaneId| after.iter().any(|(q, _)| q == p);
        let gone: Vec<(PaneId, Rect)> =
            before.iter().filter(|(p, _)| !stays(p)).map(|&(p, r)| (p, self.rect(p, r, now))).collect();
        for &(pane, to) in after {
            let (drawn, velocity) = match before.iter().find(|(p, _)| *p == pane) {
                // Same target: its spring, if any, runs on unchanged.
                Some(&(_, from)) if from == to => continue,
                Some(&(_, from)) => {
                    (self.rect(pane, from, now), self.live(pane, now).map_or([0.0; 4], |s| s.at(self.omega, now).1))
                }
                None => match left.iter().find(|(p, _)| *p == pane) {
                    Some(&(_, r)) => (r, [0.0; 4]),
                    None => continue,
                },
            };
            let (drawn, to) = (parts(drawn), parts(to));
            let offset: [f32; 4] = std::array::from_fn(|i| drawn[i] - to[i]);
            if offset == [0.0; 4] && velocity == [0.0; 4] {
                self.springs.remove(&pane);
            } else {
                self.springs.insert(pane, Spring { start: now, offset, velocity });
            }
        }
        if !gone.is_empty() {
            self.left_at = Some(now);
        }
        let still: Vec<_> = left.into_iter().filter(|(p, _)| !stays(p) && !gone.iter().any(|(q, _)| q == p)).collect();
        self.left = gone.into_iter().chain(still).collect();
        if self.left.is_empty() {
            self.left_at = None;
        }
    }

    /// Where `pane`, which the layout puts at `target`, is drawn at `now`.
    pub fn rect(&self, pane: PaneId, target: Rect, now: Instant) -> Rect {
        let Some(s) = self.live(pane, now) else { return target };
        let (d, _) = s.at(self.omega, now);
        Rect::new(target.x + d[0], target.y + d[1], target.w + d[2], target.h + d[3])
    }

    /// Where `pane`, which the layout puts at `target`, is drawn at `now` while it morphs,
    /// and how far its morph has come, 0 to 1: the unit response of the same spring, so its
    /// old look fades as it moves. `None` once it rests on `target`.
    pub fn morph(&self, pane: PaneId, target: Rect, now: Instant) -> Option<(Rect, f32)> {
        let s = self.live(pane, now)?;
        let t = now.saturating_duration_since(s.start).as_secs_f32();
        let progress = 1.0 - (1.0 + self.omega * t) * (-self.omega * t).exp();
        Some((self.rect(pane, target, now), progress))
    }

    /// The panes that changes took out, where they were drawn then, until the latest change
    /// that took one out settles: a tile a zoom hid can stay under the zoomed one growing
    /// over it. The caller picks which of them to draw.
    pub fn leaving(&self, now: Instant) -> impl Iterator<Item = (PaneId, Rect)> + '_ {
        let live = self.left_at.is_some_and(|t| now.saturating_duration_since(t) < self.settle);
        self.left.iter().copied().filter(move |_| live)
    }

    /// Whether `pane` is still on its way at `now`.
    pub fn moving(&self, pane: PaneId, now: Instant) -> bool {
        self.live(pane, now).is_some()
    }

    /// `pane`'s spring, unless it has come to rest by `now`.
    fn live(&self, pane: PaneId, now: Instant) -> Option<&Spring> {
        self.springs.get(&pane).filter(|s| now.saturating_duration_since(s.start) < self.settle)
    }

    /// Drops the springs that have come to rest by `now`; returns whether any still moves.
    pub fn settle(&mut self, now: Instant) -> bool {
        self.springs.retain(|_, s| now.saturating_duration_since(s.start) < self.settle);
        if self.left_at.is_some_and(|t| now.saturating_duration_since(t) >= self.settle) {
            self.left.clear();
            self.left_at = None;
        }
        !self.springs.is_empty() || self.left_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PaneId = PaneId(1);

    fn at(t0: Instant, secs: f64) -> Instant {
        t0 + Duration::from_secs_f64(secs)
    }

    /// SwiftUI's default `Spring()`.
    fn apple() -> Motion {
        Motion::new(Duration::from_millis(500))
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    // Expected positions come from SwiftUI's `Spring()` on macOS 26.5:
    // `Spring().value(target: 100, initialVelocity: 0, time: 0.05)` is 13.1311.
    #[test]
    fn a_moved_pane_starts_where_it_was_and_follows_apples_default_spring() {
        let t0 = Instant::now();
        let (from, to) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(100.0, 0.0, 100.0, 100.0));
        let mut m = apple();
        m.retarget(&[(A, from)], &[(A, to)], t0);
        assert_eq!(m.rect(A, to, t0), from, "no jump when the move begins");
        let r = m.rect(A, to, at(t0, 0.05));
        assert!(close(r.x, 13.1311) && r.y == 0.0 && r.w == 100.0, "{r:?}");
    }

    // `Spring()` moving 0 → 100 is at 35.7740 going 449.4374 per second after 0.1 s; sent on
    // to 200 from there at that speed, it is at 35.7740 +
    // `Spring().value(target: 164.226, initialVelocity: 449.4374, time: 0.05)` = 69.3272.
    #[test]
    fn a_pane_sent_elsewhere_in_flight_keeps_its_place_and_speed() {
        let t0 = Instant::now();
        let r = |x| Rect::new(x, 0.0, 100.0, 100.0);
        let mut m = apple();
        m.retarget(&[(A, r(0.0))], &[(A, r(100.0))], t0);
        let t1 = at(t0, 0.1);
        assert!(close(m.rect(A, r(100.0), t1).x, 35.7740));
        m.retarget(&[(A, r(100.0))], &[(A, r(200.0))], t1);
        assert!(close(m.rect(A, r(200.0), t1).x, 35.7740), "no jump");
        let x = m.rect(A, r(200.0), at(t0, 0.15)).x;
        assert!((x - 69.3272).abs() < 1e-2, "{x}");
    }

    #[test]
    fn a_pane_comes_to_rest_on_its_target_after_apples_settling_time() {
        let t0 = Instant::now();
        let (from, to) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(500.0, 40.0, 300.0, 200.0));
        let mut m = apple();
        m.retarget(&[(A, from)], &[(A, to)], t0);
        assert!(m.settle(at(t0, 0.79)), "still moving");
        assert_ne!(m.rect(A, to, at(t0, 0.79)), to);
        assert!(!m.settle(at(t0, 0.8)), "`Spring().settlingDuration` is 0.8 s");
        assert_eq!(m.rect(A, to, at(t0, 0.8)), to, "exactly on target");
    }

    #[test]
    fn a_longer_duration_stretches_the_spring_and_its_settling_in_proportion() {
        // `Spring(duration: 1)` at 0.1 s is `Spring()` at 0.05 s: 13.1311 of 100.
        let t0 = Instant::now();
        let (from, to) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(100.0, 0.0, 100.0, 100.0));
        let mut m = Motion::new(Duration::from_secs(1));
        m.retarget(&[(A, from)], &[(A, to)], t0);
        assert!(close(m.rect(A, to, at(t0, 0.1)).x, 13.1311));
        assert!(m.settle(at(t0, 1.59)), "still moving");
        assert!(!m.settle(at(t0, 1.6)), "settles after 1.6 times its duration");
    }

    #[test]
    fn a_zero_duration_puts_panes_on_their_targets_at_once() {
        let t0 = Instant::now();
        let (a, b) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(100.0, 0.0, 100.0, 100.0));
        let mut m = Motion::new(Duration::ZERO);
        m.retarget(&[(A, a), (PaneId(2), b)], &[(A, b)], t0);
        assert_eq!(m.rect(A, b, t0), b);
        assert_eq!(m.morph(A, b, t0), None);
        assert_eq!(m.leaving(t0).count(), 0, "a pane taken out is gone at once");
        assert!(!m.settle(t0));
    }

    #[test]
    fn an_absurd_duration_from_the_config_moves_slowly_without_overflowing() {
        let t0 = Instant::now();
        let (from, to) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(100.0, 0.0, 100.0, 100.0));
        let mut m = Motion::new(Duration::from_secs_f32(1e19));
        m.retarget(&[(A, from)], &[(A, to)], t0);
        assert!(m.settle(at(t0, 1.0)), "still moving");
        assert_eq!(m.leaving(at(t0, 1.0)).count(), 0);
    }

    #[test]
    fn a_target_that_moves_on_its_own_carries_the_pane_along_at_once() {
        // The window is resized mid-flight: no new spring, the pane follows by the same amount.
        let t0 = Instant::now();
        let (from, to) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(100.0, 0.0, 100.0, 100.0));
        let mut m = apple();
        m.retarget(&[(A, from)], &[(A, to)], t0);
        let t = at(t0, 0.05);
        let grown = Rect::new(100.0, 0.0, 160.0, 100.0);
        let (a, b) = (m.rect(A, to, t), m.rect(A, grown, t));
        assert!(close(b.x, a.x) && close(b.w, a.w + 60.0), "{a:?} {b:?}");
    }

    #[test]
    fn panes_that_appear_or_stay_put_are_drawn_on_their_target() {
        let t0 = Instant::now();
        let (r, s) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(0.0, 0.0, 50.0, 100.0));
        let b = PaneId(2);
        let mut m = apple();
        m.retarget(&[(A, r)], &[(A, r), (b, s)], t0);
        assert_eq!(m.rect(A, r, at(t0, 0.05)), r);
        assert_eq!(m.rect(b, s, at(t0, 0.05)), s, "a new pane opens in place");
        assert!(!m.settle(t0), "nothing moves");
    }

    #[test]
    fn panes_that_leave_the_arrangement_stay_where_they_were_drawn_until_it_settles() {
        // Zooming B hides A: A stays in place under B while B grows over it.
        let t0 = Instant::now();
        let b = PaneId(2);
        let (a_rect, b_rect, full) =
            (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(101.0, 0.0, 99.0, 100.0), Rect::new(0.0, 0.0, 200.0, 100.0));
        let mut m = apple();
        m.retarget(&[(A, a_rect), (b, b_rect)], &[(b, full)], t0);
        assert_eq!(m.leaving(at(t0, 0.79)).collect::<Vec<_>>(), vec![(A, a_rect)]);
        assert_eq!(m.leaving(at(t0, 0.8)).count(), 0, "gone once the change settles");
        m.retarget(&[(b, full)], &[(A, a_rect), (b, b_rect)], at(t0, 0.85));
        assert_eq!(m.leaving(at(t0, 0.9)).count(), 0, "back in, so not leaving");
    }

    #[test]
    fn a_change_that_takes_nothing_out_keeps_the_panes_still_leaving() {
        // Zoom B over A, then raise a card mid-flight: A stays under B until the zoom settles.
        let t0 = Instant::now();
        let (b, c) = (PaneId(2), PaneId(3));
        let (a_rect, b_rect, full, card) = (
            Rect::new(0.0, 0.0, 100.0, 100.0),
            Rect::new(101.0, 0.0, 99.0, 100.0),
            Rect::new(0.0, 0.0, 200.0, 100.0),
            Rect::new(50.0, 50.0, 40.0, 40.0),
        );
        let mut m = apple();
        m.retarget(&[(A, a_rect), (b, b_rect), (c, card)], &[(b, full), (c, card)], t0);
        m.retarget(&[(b, full), (c, card)], &[(b, full), (c, card)], at(t0, 0.3));
        assert_eq!(m.leaving(at(t0, 0.5)).collect::<Vec<_>>(), vec![(A, a_rect)]);
        assert_eq!(m.leaving(at(t0, 0.8)).count(), 0, "timed from the zoom, not the raise");
    }

    #[test]
    fn a_pane_that_comes_back_while_leaving_starts_where_it_was_left() {
        // Split, zoom the new pane mid-flight, unzoom: A comes back from where it was left
        // under the zoom, not from what remains of its split spring.
        let t0 = Instant::now();
        let b = PaneId(2);
        let (whole, half, other, full) = (
            Rect::new(0.0, 0.0, 200.0, 100.0),
            Rect::new(0.0, 0.0, 100.0, 100.0),
            Rect::new(101.0, 0.0, 99.0, 100.0),
            Rect::new(0.0, 0.0, 200.0, 100.0),
        );
        let mut m = apple();
        m.retarget(&[(A, whole)], &[(A, half), (b, other)], t0);
        let t1 = at(t0, 0.1);
        let left = m.rect(A, half, t1);
        m.retarget(&[(A, half), (b, other)], &[(b, full)], t1);
        let t2 = at(t0, 0.2);
        m.retarget(&[(b, full)], &[(A, half), (b, other)], t2);
        assert_eq!(m.rect(A, half, t2), left, "no jump");
        assert!(m.rect(A, half, at(t0, 1.0)) == half);
        assert_eq!(m.leaving(t2).count(), 0, "back, so no longer leaving");
    }

    #[test]
    fn a_morph_fades_along_the_unit_response_of_apples_default_spring() {
        // `Spring().value(target: 1, initialVelocity: 0, time: 0.05)` is 0.131311.
        let t0 = Instant::now();
        let (from, to) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(100.0, 0.0, 100.0, 100.0));
        let mut m = apple();
        m.retarget(&[(A, from)], &[(A, to)], t0);
        let (r, progress) = m.morph(A, to, at(t0, 0.05)).expect("morphing");
        assert_eq!(r, m.rect(A, to, at(t0, 0.05)));
        assert!(close(progress, 0.131311), "{progress}");
        assert_eq!(m.morph(A, to, at(t0, 0.8)), None, "settled");
    }

    #[test]
    fn a_change_that_leaves_a_panes_target_alone_lets_its_morph_run_on() {
        let t0 = Instant::now();
        let (from, to) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(100.0, 0.0, 100.0, 100.0));
        let mut m = apple();
        m.retarget(&[(A, from)], &[(A, to)], t0);
        m.retarget(&[(A, to)], &[(A, to)], at(t0, 0.1)); // a card was raised
        // `Spring().value(target: 1, initialVelocity: 0, time: 0.15)` is 0.561960: timed from
        // the move, not the raise.
        let (_, progress) = m.morph(A, to, at(t0, 0.15)).expect("morphing");
        assert!(close(progress, 0.561960), "{progress}");
    }

    #[test]
    fn a_pane_at_rest_stays_at_rest_when_its_target_does_not_change() {
        let t0 = Instant::now();
        let (from, to) = (Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(100.0, 0.0, 100.0, 100.0));
        let mut m = apple();
        m.retarget(&[(A, from)], &[(A, to)], t0);
        m.retarget(&[(A, to)], &[(A, to)], at(t0, 0.9)); // settled, but `settle` never ran
        assert!(!m.settle(at(t0, 0.9)), "no new spring from the old one's leftover speed");
    }
}
