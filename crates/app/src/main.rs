//! Frith: a GPU terminal and multiplexer for running AI coding agents side by side.
//!
//! Development knobs (environment variables, all optional):
//!   FRITH_CMD="..."      the first pane runs this through the login shell instead of the shell
//!   FRITH_SCRIPT="t action arg; ..."   timed actions, t in seconds since start, for scripted
//!     checks; while a script runs, the real pointer is ignored:
//!       send <escaped>          type into the focused pane (\e \r \n \t \s \xHH)
//!       shot <png>              screenshot the window
//!       press|move|release x y  primary button at logical px of the window
//!       size w h                resize the window to w×h logical px
//!       quit
//!
//! Keys: Cmd+D split right, Cmd+Shift+D split down, Cmd+W close pane, Cmd+Shift+Enter zoom,
//! Cmd+[ / Cmd+] previous / next pane, Cmd+V paste, Cmd+Q quit, Esc cancels a pane drag.

mod app;
mod atlas;
mod config_files;
mod fonts;
mod gpu;
mod grid;
mod render;
mod sprite;
mod stage;

use std::path::PathBuf;

use winit::event_loop::EventLoop;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// One timed action of `FRITH_SCRIPT`.
pub struct Step {
    pub at: f32,
    pub action: String,
    pub arg: String,
}

pub struct Cfg {
    pub cmd: Option<String>,
    pub script: Vec<Step>,
}

impl Cfg {
    fn from_env() -> Cfg {
        let script = env("FRITH_SCRIPT")
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
        Cfg { cmd: env("FRITH_CMD"), script }
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
    let home = PathBuf::from(env("HOME").unwrap_or_else(|| "/".into()));
    let xdg = env("XDG_CONFIG_HOME").map(PathBuf::from);
    let (config, msgs) = config_files::load(&home, xdg.as_deref());
    for m in msgs {
        eprintln!("frith: {m}");
    }
    let event_loop = EventLoop::new().expect("event loop");
    let app = app::App::new(Cfg::from_env(), config, frith_term::shell::cache_dir(&home), event_loop.create_proxy());
    event_loop.run_app(app).expect("event loop run");
}
