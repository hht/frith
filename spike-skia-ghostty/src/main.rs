//! PROTOTYPE spike — throwaway. Rust + winit 0.31 + skia-safe (Ganesh/Metal) + libghostty-vt.
//! Questions:
//! 1. Can this stack run real TUIs fast, with in-window backdrop blur, on macOS?
//! 2. Can it host many panes per window — split, drag to dock, reorder, resize, zoom,
//!    minimize to a dock bar — and tear panes off into their own windows and back?
//!
//! Env knobs (all optional):
//!   SPIKE_MODE=atlas|blob   grid strategy (default atlas)
//!   SPIKE_BLUR=N            N animated floating glass panes (default 0), SPIKE_SIGMA=20
//!   SPIKE_BENCH=1           vsync off + redraw continuously (measure raw frame cost)
//!   SPIKE_SYNC=1            wait for GPU every frame so "flush" ≈ GPU time
//!   SPIKE_CMD="..."         first pane runs this via /bin/sh -c instead of the login shell
//!   SPIKE_FONT=Menlo SPIKE_FONT_SIZE=13 SPIKE_WIN=1200x800 SPIKE_STATS_OUT=path.json
//!   SPIKE_SCRIPT="t action arg; ..."  timed actions, t in seconds since start, applied to
//!     the front window and its focused pane:
//!       send <escaped>              type bytes (\e \r \n \t \s \xHH)
//!       shot <png>                  screenshot every window (2nd window: <png>-2.png, ...)
//!       split right|down|left|up [cmd]   new pane next to the focused one
//!       focus next|prev   zoom   min   restore   close
//!       press|move|release x y      primary button, logical px of the front window
//!                                   (release outside every window to tear a pane off)
//!       quit
//!
//! Keys: Cmd+D split right, Cmd+Shift+D split down, Cmd+W close pane, Cmd+Shift+Enter zoom,
//! Cmd+[ / Cmd+] previous / next pane, Cmd+N new window, Cmd+V paste, Cmd+Q quit,
//! Esc cancels a pane drag. Mouse: drag a pane header to dock it on another pane's edge,
//! swap it (center), dock it on a window edge, or drop it outside to open a new window;
//! drag the gaps to resize; header buttons minimize / zoom / close; dock chips restore.

mod app;
mod chrome;
mod draw;
mod gesture;
mod gpu;
mod layout;
mod stats;
mod term;

use winit::event_loop::EventLoop;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

pub struct Step {
    pub at: f32,
    pub action: String,
    pub arg: String,
}

pub struct Cfg {
    pub mode: draw::Mode,
    pub blur: usize,
    pub sigma: f32,
    pub bench: bool,
    pub sync: bool,
    pub cmd: Option<String>,
    pub script: Vec<Step>,
    pub font: String,
    pub font_size: f32,
    pub win: (f64, f64),
    pub stats_out: Option<String>,
}

impl Cfg {
    fn from_env() -> Cfg {
        let script = env("SPIKE_SCRIPT")
            .map(|s| {
                s.split(';')
                    .filter_map(|item| {
                        let mut it = item.trim().splitn(3, ' ');
                        let at = it.next()?.parse().ok()?;
                        let action = it.next()?.to_string();
                        Some(Step { at, action, arg: it.next().unwrap_or("").to_string() })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let win = env("SPIKE_WIN")
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((1200.0, 800.0));
        Cfg {
            mode: if env("SPIKE_MODE").as_deref() == Some("blob") { draw::Mode::Blob } else { draw::Mode::Atlas },
            blur: env("SPIKE_BLUR").and_then(|v| v.parse().ok()).unwrap_or(0),
            sigma: env("SPIKE_SIGMA").and_then(|v| v.parse().ok()).unwrap_or(20.0),
            bench: env("SPIKE_BENCH").is_some(),
            sync: env("SPIKE_SYNC").is_some(),
            cmd: env("SPIKE_CMD"),
            script,
            font: env("SPIKE_FONT").unwrap_or_else(|| "Menlo".into()),
            font_size: env("SPIKE_FONT_SIZE").and_then(|v| v.parse().ok()).unwrap_or(13.0),
            win,
            stats_out: env("SPIKE_STATS_OUT"),
        }
    }
}

/// Decodes the escapes accepted by the `send` script action.
pub fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
            match b[i] {
                b'r' => out.push(b'\r'),
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'e' => out.push(0x1b),
                b's' => out.push(b' '),
                b'x' if i + 2 < b.len() => {
                    let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                    out.push(hex.and_then(|h| u8::from_str_radix(h, 16).ok()).unwrap_or(b'?'));
                    i += 2;
                }
                o => out.push(o),
            }
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    out
}

fn main() {
    let cfg = Cfg::from_env();
    let event_loop = EventLoop::new().expect("event loop");
    let app = app::App::new(cfg, event_loop.create_proxy());
    event_loop.run_app(app).expect("event loop run");
}
