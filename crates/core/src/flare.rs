//! Spirit light (ADR 10): how a pane opening or closing looks at a moment. The pane itself
//! only fades and scales a little about its center; its border glows once; a few motes of
//! light gather onto the border (open) or rise from it and wander off (close).
//!
//! Answers: at a given moment, how strong is a pane's body and glow, and where is each mote?
//! Must not know: the clock (callers pass `now`), colors, drawing, or layout.

use std::time::{Duration, Instant};

use crate::geom::{Point, Rect};

/// 0 below `a`, 1 above `b`, and an S-curve between.
fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A number in [0, 1) fixed by `seed`, the mote `k` and what it is for, `j` (SplitMix64's
/// finalizer). @see https://prng.di.unimi.it/splitmix64.c
fn hash(seed: u64, k: u64, j: u64) -> f32 {
    let mut z = seed ^ k.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ j.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 40) as f32 / (1u64 << 24) as f32
}

/// The point `u` (0 to 1) of the way clockwise around `r` from its top left, and the
/// outward normal there.
fn on_border(r: Rect, u: f32) -> (Point, Point) {
    let mut d = u.fract() * 2.0 * (r.w + r.h);
    if d < r.w {
        return (Point::new(r.x + d, r.y), Point::new(0.0, -1.0));
    }
    d -= r.w;
    if d < r.h {
        return (Point::new(r.right(), r.y + d), Point::new(1.0, 0.0));
    }
    d -= r.h;
    if d < r.w {
        return (Point::new(r.right() - d, r.bottom()), Point::new(0.0, 1.0));
    }
    d -= r.w;
    (Point::new(r.x, r.bottom() - d), Point::new(-1.0, 0.0))
}

/// Whether the pane opens or closes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Way {
    Open,
    Close,
}

/// How much of the light shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Look {
    /// Body, glow and motes.
    Full,
    /// The body only (`pane-particles = false`).
    Plain,
    /// The body fading at its full size (macOS's Reduce Motion).
    Still,
}

/// One pane's open or close, from `start`, over its whole rect `rect`.
pub struct Flare {
    pub way: Way,
    pub rect: Rect,
    pub start: Instant,
    /// The body's duration: the spring's (open) or the fade's (close).
    pub duration: Duration,
    pub look: Look,
    /// Picks the motes' places, paths and flicker.
    pub seed: u64,
    /// Pixels per point, the unit `rect` is in.
    pub px: f32,
}

/// One mote of light at a moment, in the flare's pixels.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mote {
    pub center: Point,
    pub radius: f32,
    pub alpha: f32,
}

/// How the pane's body shows at a moment: drawn at `alpha`, scaled by `scale` about its center.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Body {
    pub alpha: f32,
    pub scale: f32,
}

impl Flare {
    /// The body at `now`: an opening one comes in along the layout's spring (SwiftUI's
    /// `Spring(duration:)`, see `motion`), a closing one leaves slowly, then fast (ease-in).
    pub fn body(&self, now: Instant) -> Body {
        let b = self.moving_body(now);
        if self.look == Look::Still { Body { scale: 1.0, ..b } } else { b }
    }

    fn moving_body(&self, now: Instant) -> Body {
        let t = now.saturating_duration_since(self.start).as_secs_f32();
        let d = self.duration.as_secs_f32();
        match self.way {
            _ if t >= self.body_end() => match self.way {
                Way::Open => Body { alpha: 1.0, scale: 1.0 },
                Way::Close => Body { alpha: 0.0, scale: 0.97 },
            },
            Way::Open => {
                let omega = std::f32::consts::TAU / d;
                let p = 1.0 - (1.0 + omega * t) * (-omega * t).exp();
                Body { alpha: p, scale: 0.97 + 0.03 * p }
            }
            Way::Close => {
                let gone = (t / d).min(1.0).powi(2);
                Body { alpha: 1.0 - gone, scale: 1.0 - 0.03 * gone }
            }
        }
    }

    /// Seconds from the start until the body is whole (open: the spring settles after 1.6
    /// times its duration, as in `motion`) or gone (close).
    fn body_end(&self) -> f32 {
        let d = self.duration.as_secs_f32();
        match self.way {
            Way::Open => d * 1.6,
            Way::Close => d,
        }
    }

    /// How strongly the border glows at `now`, 0 to 0.8: up quickly, down slowly.
    pub fn glow(&self, now: Instant) -> f32 {
        if self.look != Look::Full {
            return 0.0;
        }
        let t = self.light_t(now);
        let (peak, out) = self.glow_times();
        0.8 * smooth(0.0, peak, t) * (1.0 - smooth(peak, out, t))
    }

    /// When the glow peaks and when it is out, in seconds.
    fn glow_times(&self) -> (f32, f32) {
        match self.way {
            Way::Open => (0.14, 0.75),
            Way::Close => (0.04, 0.4),
        }
    }

    /// Seconds since the start for the light, which keeps its own time whatever the body's
    /// duration (ADR 10: the motes are out after about a second); with no body there is no
    /// light either.
    fn light_t(&self, now: Instant) -> f32 {
        let t = now.saturating_duration_since(self.start).as_secs_f32();
        if self.duration.is_zero() { f32::INFINITY } else { t }
    }

    /// Fills `out` with the motes that show at `now`: spread around the border, an opening
    /// pane's drift in from outside and go out as they touch it; a closing pane's leave the
    /// border, rise, slowing, and wander as they fade. Each flickers.
    pub fn motes(&self, now: Instant, out: &mut Vec<Mote>) {
        out.clear();
        if self.look != Look::Full {
            return;
        }
        let (t, pt, tau) = (self.light_t(now), self.px, std::f32::consts::TAU);
        let n = match self.way {
            Way::Open => 14,
            Way::Close => 20,
        };
        for k in 0..n {
            let h = |j| hash(self.seed, k, j);
            let (at, normal) = on_border(self.rect, (k as f32 + 0.8 * h(0)) / n as f32);
            let (w1, w2) = (tau * (0.25 + 0.35 * h(2)), tau * (0.2 + 0.3 * h(3)));
            let (p1, p2) = (h(4) * tau, h(5) * tau);
            let (ax, ay) = ((5.0 + 7.0 * h(6)) * pt, (4.0 + 5.0 * h(7)) * pt);
            let wander =
                |s: f32| Point::new(ax * ((w1 * s + p1).sin() - p1.sin()), ay * ((w2 * s + p2).sin() - p2.sin()));
            let flicker = 0.45 + 0.55 * (0.5 + 0.5 * (tau * (0.7 + 1.3 * h(8)) * t + h(9) * tau).sin()).powi(2);
            let (center, life) = match self.way {
                Way::Close => {
                    let s = t - 0.15 * h(1);
                    let span = 1.1 * (0.75 + 0.5 * h(10));
                    if !(0.0..=span).contains(&s) {
                        continue;
                    }
                    let away = (6.0 + 10.0 * h(11)) * pt * (1.0 - (-s / 0.35).exp());
                    let up = (18.0 + 26.0 * h(12)) * pt * 0.9 * (1.0 - (-s / 0.9).exp());
                    let w = wander(s);
                    let c = Point::new(at.x + normal.x * away + w.x, at.y + normal.y * away - up + w.y);
                    (c, smooth(0.0, 0.08, s) * (1.0 - smooth(span * 0.4, span, s)))
                }
                Way::Open => {
                    let land = 0.3 + 0.25 * h(1);
                    if t > land + 0.1 {
                        continue;
                    }
                    let left = (1.0 - (t / land).clamp(0.0, 1.0)).powi(3);
                    let far = (22.0 + 30.0 * h(11)) * pt;
                    let w = wander(t);
                    let from = Point::new(normal.x * far + w.x, normal.y * far - 10.0 * pt * h(12) + w.y);
                    let c = Point::new(at.x + from.x * left, at.y + from.y * left);
                    (c, smooth(0.0, 0.12, t) * (1.0 - smooth(land - 0.04, land + 0.1, t)))
                }
            };
            let alpha = (life * flicker).clamp(0.0, 1.0);
            if alpha > 0.004 {
                out.push(Mote { center, radius: 7.0 * (0.75 + 0.5 * h(14)) * pt, alpha });
            }
        }
    }

    /// Whether nothing of the flare is left to draw at `now`.
    pub fn done(&self, now: Instant) -> bool {
        let t = now.saturating_duration_since(self.start).as_secs_f32();
        // The light is out once the glow is (open: the last mote lands at 0.65 s) or the
        // last mote fades (close: born by 0.15 s, gone within 1.1 × 1.25 s).
        let light_out = match self.way {
            Way::Open => self.glow_times().1,
            Way::Close => 0.15 + 1.1 * 1.25,
        };
        t >= self.body_end() && (self.look != Look::Full || self.light_t(now) >= light_out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECT: Rect = Rect::new(100.0, 50.0, 400.0, 300.0);

    fn at(t0: Instant, secs: f64) -> Instant {
        t0 + Duration::from_secs_f64(secs)
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn open(t0: Instant) -> Flare {
        Flare {
            way: Way::Open,
            rect: RECT,
            start: t0,
            duration: Duration::from_millis(500),
            look: Look::Full,
            seed: 7,
            px: 2.0,
        }
    }

    #[test]
    fn a_closing_body_fades_slowly_then_fast_and_is_gone_at_its_duration() {
        // Ease-in (quadratic) over 0.3 s, shrinking toward 0.97 as it goes.
        let t0 = Instant::now();
        let f = Flare { way: Way::Close, duration: Duration::from_millis(300), ..open(t0) };
        assert_eq!(f.body(t0), Body { alpha: 1.0, scale: 1.0 });
        let b = f.body(at(t0, 0.15));
        assert!(close(b.alpha, 0.75) && close(b.scale, 1.0 - 0.03 * 0.25), "{b:?}");
        assert_eq!(f.body(at(t0, 0.3)), Body { alpha: 0.0, scale: 0.97 });
        assert_eq!(f.body(at(t0, 0.5)), Body { alpha: 0.0, scale: 0.97 });
    }

    #[test]
    fn the_border_glows_up_quickly_and_fades_slowly_on_its_own_time() {
        // Tuned at the default durations: open peaks at 0.14 s and is out by 0.75 s, close
        // peaks at 0.04 s and is out by 0.4 s; both at 0.8 strength.
        let t0 = Instant::now();
        let f = open(t0);
        assert_eq!(f.glow(t0), 0.0);
        assert!(close(f.glow(at(t0, 0.14)), 0.8));
        assert!(f.glow(at(t0, 0.4)) > 0.0 && f.glow(at(t0, 0.4)) < 0.8);
        assert_eq!(f.glow(at(t0, 0.75)), 0.0);
        let slow = Flare { duration: Duration::from_secs(10), ..open(t0) };
        assert!(close(slow.glow(at(t0, 0.14)), 0.8), "the light keeps its time whatever the body's");
        assert!(slow.done(at(t0, 16.0)) && !slow.done(at(t0, 15.9)), "done when the body is whole");
        let shut = Flare { way: Way::Close, duration: Duration::from_millis(300), ..open(t0) };
        assert!(close(shut.glow(at(t0, 0.04)), 0.8));
        assert_eq!(shut.glow(at(t0, 0.4)), 0.0);
    }

    #[test]
    fn without_particles_only_the_body_shows_and_reduce_motion_also_keeps_its_size() {
        let t0 = Instant::now();
        let plain = Flare { look: Look::Plain, ..open(t0) };
        assert_eq!(plain.glow(at(t0, 0.14)), 0.0);
        assert_eq!(plain.body(at(t0, 0.05)), open(t0).body(at(t0, 0.05)));
        let still = Flare { look: Look::Still, ..open(t0) };
        assert_eq!(still.glow(at(t0, 0.14)), 0.0);
        let b = still.body(at(t0, 0.05));
        assert!(close(b.alpha, 0.131311) && b.scale == 1.0, "{b:?}");
        let shut = Flare { way: Way::Close, look: Look::Still, ..open(t0) };
        assert_eq!(shut.body(at(t0, 0.25)).scale, 1.0);
        assert_eq!(shut.body(at(t0, 1.0)).scale, 1.0);
    }

    #[test]
    fn fourteen_motes_gather_and_twenty_rise_the_same_for_the_same_seed() {
        let t0 = Instant::now();
        let (f, mut out) = (open(t0), Vec::new());
        f.motes(at(t0, 0.1), &mut out);
        assert_eq!(out.len(), 14);
        // 7 pt across, give or take a quarter, at 2 px per pt.
        assert!(out.iter().all(|m| (10.5..=17.5).contains(&m.radius) && m.alpha > 0.0 && m.alpha <= 1.0), "{out:?}");
        let again = {
            let mut v = Vec::new();
            open(t0).motes(at(t0, 0.1), &mut v);
            v
        };
        assert_eq!(out, again, "same seed, same motes");
        let other = {
            let mut v = Vec::new();
            Flare { seed: 8, ..open(t0) }.motes(at(t0, 0.1), &mut v);
            v
        };
        assert_ne!(out, other);
        let shut = Flare { way: Way::Close, duration: Duration::from_millis(300), ..open(t0) };
        shut.motes(at(t0, 0.2), &mut out);
        assert_eq!(out.len(), 20, "the buffer is refilled, not appended to");
        for look in [Look::Plain, Look::Still] {
            Flare { look, ..open(t0) }.motes(at(t0, 0.1), &mut out);
            assert!(out.is_empty(), "{look:?}");
        }
    }

    /// How far `p` is from the nearest point of `r`'s border, and whether it is outside `r`.
    fn from_border(r: Rect, p: Point) -> (f32, bool) {
        let dx = (r.x - p.x).max(p.x - r.right());
        let dy = (r.y - p.y).max(p.y - r.bottom());
        if dx > 0.0 || dy > 0.0 { (dx.max(0.0).hypot(dy.max(0.0)), true) } else { (-dx.max(dy), false) }
    }

    fn mean_y(motes: &[Mote]) -> f32 {
        motes.iter().map(|m| m.center.y).sum::<f32>() / motes.len() as f32
    }

    #[test]
    fn closing_motes_leave_the_border_and_rise_until_the_last_fades() {
        let t0 = Instant::now();
        let f = Flare { way: Way::Close, duration: Duration::from_millis(300), ..open(t0) };
        let (mut early, mut late) = (Vec::new(), Vec::new());
        f.motes(at(t0, 0.1), &mut early);
        assert!(!early.is_empty());
        // 2 px per pt: within 12 pt of the border just after leaving it.
        assert!(early.iter().all(|m| from_border(RECT, m.center).0 < 24.0), "{early:?}");
        // All twenty show from 0.15 s, when the last is born, until past 0.7 s.
        let mut all = Vec::new();
        f.motes(at(t0, 0.2), &mut all);
        f.motes(at(t0, 0.7), &mut late);
        assert_eq!((all.len(), late.len()), (20, 20));
        assert!(mean_y(&late) < mean_y(&all) - 10.0, "risen by more than 5 pt");
        assert!(!f.done(at(t0, 1.0)), "motes linger after the body is gone");
        f.motes(at(t0, 1.6), &mut late);
        assert!(late.is_empty() && f.done(at(t0, 1.6)));
        let slow = Flare { duration: Duration::from_secs(10), ..f };
        slow.motes(at(t0, 1.6), &mut late);
        assert!(late.is_empty() && !slow.done(at(t0, 1.6)), "out after about a second and a half, ADR 10");
    }

    #[test]
    fn opening_motes_drift_in_from_outside_and_go_out_on_the_border() {
        let t0 = Instant::now();
        let f = open(t0);
        let (mut early, mut later) = (Vec::new(), Vec::new());
        f.motes(at(t0, 0.1), &mut early);
        assert!(early.iter().all(|m| from_border(RECT, m.center).1), "outside at first: {early:?}");
        f.motes(at(t0, 0.3), &mut later);
        let mean = |v: &[Mote]| v.iter().map(|m| from_border(RECT, m.center).0).sum::<f32>() / v.len() as f32;
        assert!(mean(&later) < mean(&early) / 2.0, "closing in");
        f.motes(at(t0, 0.66), &mut later);
        assert!(later.is_empty(), "all gone by the time the glow is");
    }

    #[test]
    fn a_zero_duration_opens_or_closes_at_once() {
        let t0 = Instant::now();
        let open = Flare { duration: Duration::ZERO, ..open(t0) };
        assert_eq!(open.body(t0), Body { alpha: 1.0, scale: 1.0 });
        assert!(open.done(t0));
        let shut = Flare { way: Way::Close, ..open };
        assert_eq!(shut.body(t0).alpha, 0.0);
        assert!(shut.done(t0));
    }

    #[test]
    fn an_opening_body_fades_in_and_grows_from_097_along_apples_default_spring() {
        // `Spring().value(target: 1, initialVelocity: 0, time: 0.05)` is 0.131311.
        let t0 = Instant::now();
        let f = open(t0);
        assert_eq!(f.body(t0), Body { alpha: 0.0, scale: 0.97 });
        let b = f.body(at(t0, 0.05));
        assert!(close(b.alpha, 0.131311) && close(b.scale, 0.97 + 0.03 * 0.131311), "{b:?}");
    }
}
