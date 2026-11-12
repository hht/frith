//! Frame-cost measurement: per-phase samples, percentiles, and the periodic and final reports.
//!
//! Answers: what do parsing and drawing cost, as p50/p99 and throughput?
//! Must not know: what produced the samples.

use std::time::Instant;

#[derive(Default)]
pub struct Samples {
    /// Whole frame: draw start to present, including the wait for a drawable.
    pub total: Vec<f32>,
    /// One `feed` pass over all panes.
    pub parse: Vec<f32>,
    /// Encoding draw commands, after a drawable was acquired.
    pub draw: Vec<f32>,
    /// Flush, submit (and, with `SPIKE_SYNC`, GPU completion) plus present.
    pub flush: Vec<f32>,
    pub rebuilt: Vec<u32>,
    pub bytes: u64,
}

pub fn ms(a: Instant, b: Instant) -> f32 {
    (b - a).as_secs_f32() * 1000.0
}

fn pct(v: &[f32], p: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f32::total_cmp);
    s[((s.len() - 1) as f32 * p).round() as usize]
}

impl Samples {
    pub fn line(&self, secs: f32) -> String {
        let n = self.total.len();
        format!(
            "frames={n} fps={:.0} total p50={:.2} p99={:.2} max={:.2}ms | parse p50={:.2} p99={:.2} | draw p50={:.2} p99={:.2} | flush p50={:.2} p99={:.2} | rows rebuilt avg={:.1} | parsed {:.1} MB/s",
            n as f32 / secs,
            pct(&self.total, 0.5),
            pct(&self.total, 0.99),
            pct(&self.total, 1.0),
            pct(&self.parse, 0.5),
            pct(&self.parse, 0.99),
            pct(&self.draw, 0.5),
            pct(&self.draw, 0.99),
            pct(&self.flush, 0.5),
            pct(&self.flush, 0.99),
            self.rebuilt.iter().sum::<u32>() as f32 / n.max(1) as f32,
            self.bytes as f32 / secs / 1e6,
        )
    }

    pub fn append(&mut self, other: Samples) {
        self.total.extend(other.total);
        self.parse.extend(other.parse);
        self.draw.extend(other.draw);
        self.flush.extend(other.flush);
        self.rebuilt.extend(other.rebuilt);
        self.bytes += other.bytes;
    }

    /// One JSON object with the run's percentiles; `extra` is spliced in verbatim.
    pub fn summary(&self, extra: &str, secs: f32, parse_secs: f32) -> String {
        format!(
            "{{{extra},\"secs\":{secs:.2},\"bytes\":{},\"throughput_mb_s\":{:.1},\"frames\":{},\"total_p50\":{:.3},\"total_p99\":{:.3},\"total_max\":{:.3},\"parse_p50\":{:.3},\"parse_p99\":{:.3},\"draw_p50\":{:.3},\"draw_p99\":{:.3},\"flush_p50\":{:.3},\"flush_p99\":{:.3}}}",
            self.bytes,
            self.bytes as f32 / parse_secs / 1e6,
            self.total.len(),
            pct(&self.total, 0.5),
            pct(&self.total, 0.99),
            pct(&self.total, 1.0),
            pct(&self.parse, 0.5),
            pct(&self.parse, 0.99),
            pct(&self.draw, 0.5),
            pct(&self.draw, 0.99),
            pct(&self.flush, 0.5),
            pct(&self.flush, 0.99),
        )
    }
}

/// Samples of the current 2-second window plus everything since start.
pub struct Stats {
    pub window: Samples,
    all: Samples,
    window_start: Instant,
    pub start: Instant,
    pub first_byte: Option<Instant>,
}

impl Stats {
    pub fn new() -> Stats {
        let now = Instant::now();
        Stats { window: Samples::default(), all: Samples::default(), window_start: now, start: now, first_byte: None }
    }

    /// Prints and rolls the window every 2 s; `note` is appended to the line.
    pub fn tick(&mut self, note: impl FnOnce() -> String) {
        let secs = self.window_start.elapsed().as_secs_f32();
        if secs >= 2.0 {
            eprintln!("[stats] {} | {}", self.window.line(secs), note());
            self.all.append(std::mem::take(&mut self.window));
            self.window_start = Instant::now();
        }
    }

    pub fn summary(&mut self, extra: &str) -> String {
        self.all.append(std::mem::take(&mut self.window));
        let secs = self.start.elapsed().as_secs_f32();
        let parse_secs = self.first_byte.map(|t| t.elapsed().as_secs_f32()).unwrap_or(secs);
        self.all.summary(extra, secs, parse_secs)
    }
}
