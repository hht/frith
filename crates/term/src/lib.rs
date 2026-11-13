//! One terminal session: a child process on a PTY, its libghostty-vt terminal, and the
//! encoding of keyboard, mouse, wheel and paste input into bytes for the child.
//!
//! Answers: how do bytes and input flow between the user, libghostty-vt and the child?
//! Must not know: where the session is on screen (callers pass grid sizes and positions
//! local to the terminal content) or how it is drawn.
//!
//! Two IO threads only move bytes. A reader collects output in a buffer capped at
//! `OUTPUT_CAP` bytes, so a child that floods output blocks on its PTY until the main thread
//! catches up. A writer drains an input queue, so a child that reads no input never blocks
//! the main thread. The input queue is unbounded, like Ghostty's write queue: it holds only
//! what the user typed or pasted. The `Terminal` is `!Send` and stays on the main thread.
//! @see https://github.com/ghostty-org/ghostty/blob/108cacb17ed8990840cd14abfce43d8ed8503ca1/src/termio/Exec.zig (`queueWrite`)

pub mod pty;
pub mod shell;

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use libghostty_vt::color::Rgb;
use libghostty_vt::effect::{
    ClipboardMime, ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, PrimaryDeviceAttributes,
    SecondaryDeviceAttributes,
};
use libghostty_vt::key::{self, Key};
use libghostty_vt::mode::Mode;
use libghostty_vt::report::SizeReportSize;
use libghostty_vt::terminal::{ScrollViewport, Terminal};
use libghostty_vt::{mouse, paste};
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key as WKey, KeyCode, ModifiersState, PhysicalKey};

use crate::pty::{Program, Pty};
use frith_core::config::Config;

pub(crate) const TERM: &str = "xterm-256color";
const NAME: &str = "frith";

/// Bytes of output the reader may hold for `feed`, enough for several frames of a fast stream;
/// with the backlog `feed` already took, up to twice this waits unparsed. Counted in bytes,
/// not reads: a macOS PTY read returns at most 1024 bytes (measured).
const OUTPUT_CAP: usize = 1 << 20;
/// The longest a program may hold its frame (DEC 2026); Ghostty's `sync_reset_ms`.
/// @see https://github.com/ghostty-org/ghostty/blob/108cacb17ed8990840cd14abfce43d8ed8503ca1/src/termio/Thread.zig (`sync_reset_ms`)
const HOLD_LIMIT: Duration = Duration::from_secs(1);
/// How long resizes gather after the first before one reaches the terminal and the child,
/// so a dragged window edge sends a burst of SIGWINCH as one; Ghostty's `Coalesce.min_ms`.
/// @see https://github.com/ghostty-org/ghostty/blob/108cacb17ed8990840cd14abfce43d8ed8503ca1/src/termio/Thread.zig (`handleResize`)
const RESIZE_GATHER: Duration = Duration::from_millis(25);

/// Output the reader thread has read and `feed` has not taken yet.
#[derive(Default)]
struct Output {
    pending: Mutex<Pending>,
    /// Signaled when `feed` takes the pending bytes or the session closes.
    room: Condvar,
}

#[derive(Default)]
struct Pending {
    bytes: Vec<u8>,
    /// The child side is gone; `bytes` holds the last of its output.
    eof: bool,
    /// The session is gone; the reader stops.
    closed: bool,
}

/// Grid geometry of a session's content area, in physical pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
    /// Padding around the grid inside the content area.
    pub pad: Pad,
    /// Size of the content area.
    pub width: u32,
    pub height: u32,
}

/// Space between each edge of the content area and the grid, in physical pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Pad {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl GridSize {
    /// The largest grid that fits `width`×`height` inside `pad` (at least 2×2). Padding is
    /// cut to the area, so sums of it never overflow here or in libghostty-vt.
    pub fn fit(width: u32, height: u32, cell_w: u32, cell_h: u32, pad: Pad) -> GridSize {
        let (left, top) = (pad.left.min(width), pad.top.min(height));
        let (right, bottom) = (pad.right.min(width - left), pad.bottom.min(height - top));
        let pad = Pad { left, top, right, bottom };
        let cells = |len: u32, cell: u32| (len / cell).clamp(2, u16::MAX as u32) as u16;
        let (cols, rows) = (cells(width - left - right, cell_w), cells(height - top - bottom, cell_h));
        GridSize { cols, rows, cell_w, cell_h, pad, width, height }
    }
}

/// Effect callbacks write here; the app reads and clears the flags.
#[derive(Default)]
pub struct Shared {
    grid: Cell<(u16, u16, u32, u32)>, // cols, rows, cell_w, cell_h
    /// When the program's current frame hold (DEC 2026) began; read through `Session::hold`.
    render_hold: Cell<Option<Instant>>,
    pub title_changed: Cell<bool>,
    /// Set by OSC 9 / OSC 777 desktop notifications.
    pub notified: Cell<bool>,
}

/// Result of feeding pending PTY output into the terminal.
pub struct Fed {
    pub bytes: usize,
    /// Output is waiting (the budget ran out, or more arrived meanwhile); feed again.
    pub more: bool,
    /// The child has exited and all its output has been consumed.
    pub exited: bool,
}

pub struct Session {
    pub term: Terminal<'static, 'static>,
    pub shared: Rc<Shared>,
    input: mpsc::Sender<Vec<u8>>,
    pty: Pty,
    output: Arc<Output>,
    /// Output taken from `output`, parsed up to `parsed`.
    backlog: Vec<u8>,
    parsed: usize,
    /// `backlog` holds the child's last output.
    eof: bool,
    size: GridSize,
    /// When a gathered resize is applied; `None` when the size is current.
    resize_due: Option<Instant>,
    key_enc: key::Encoder<'static>,
    key_ev: key::Event<'static>,
    mouse_enc: mouse::Encoder<'static>,
    mouse_ev: mouse::Event<'static>,
    buf: Vec<u8>,
    scroll_acc: f32,
}

impl Session {
    /// Starts `program` in `cwd`; a login shell loads the shell integration installed in
    /// `integration`, when given. `wake` is called from the IO thread whenever output arrives
    /// or the child exits.
    pub fn spawn(
        program: &Program,
        cwd: &Path,
        size: GridSize,
        config: &Config,
        integration: Option<&Path>,
        wake: impl Fn() + Send + 'static,
    ) -> std::io::Result<Session> {
        // Apps launched from Finder or launchd carry no LANG; without a UTF-8 LC_CTYPE, zsh's
        // line editor shows typed CJK as <00ad>-style escapes. Like Ghostty, set LANG only when
        // it is unset or empty: locale variables the user did set (LC_ALL and LC_CTYPE
        // included) are the user's choice. Unlike Ghostty, which derives LANG from the macOS
        // language settings and also replaces an invalid LANG, this sets a fixed en_US.UTF-8.
        // @see https://github.com/ghostty-org/ghostty/blob/108cacb17ed8990840cd14abfce43d8ed8503ca1/src/os/locale.zig
        let mut env: Vec<(&str, OsString)> =
            vec![("TERM", TERM.into()), ("COLORTERM", "truecolor".into()), ("TERM_PROGRAM", NAME.into())];
        if std::env::var_os("LANG").is_none_or(|v| v.is_empty()) {
            env.push(("LANG", "en_US.UTF-8".into()));
        }
        if let (Program::LoginShell, Some(dir)) = (program, integration) {
            env.extend(shell::env(&pty::login_shell()?, dir, std::env::var_os("ZDOTDIR")));
        }
        let env: Vec<(&str, &OsStr)> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        let pty = Pty::spawn(program, pty_size(size), cwd, &env)?;
        let mut reader = pty.reader()?;
        let mut writer = pty.writer()?;
        let (input, queued) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            // Ends when the session drops the sender, or when the child side is gone.
            for data in queued {
                if writer.write_all(&data).is_err() {
                    break;
                }
            }
        });
        let output = Arc::new(Output::default());
        let out = output.clone();
        thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    // A signal handler ran; not the end of output.
                    // @see https://github.com/alacritty/alacritty/blob/v0.15.1/alacritty_terminal/src/event_loop.rs#L127
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    // EIO once the child side is gone (macOS), or EOF.
                    Ok(0) | Err(_) => 0,
                    Ok(n) => n,
                };
                let mut p = out.pending.lock().unwrap();
                while n > 0 && p.bytes.len() >= OUTPUT_CAP && !p.closed {
                    p = out.room.wait(p).unwrap();
                }
                if p.closed {
                    break;
                }
                // The main thread was already woken for output it has not taken yet.
                let idle = p.bytes.is_empty();
                p.bytes.extend_from_slice(&buf[..n]);
                p.eof = n == 0;
                drop(p);
                if idle || n == 0 {
                    wake();
                }
                if n == 0 {
                    break;
                }
            }
        });
        let shared = Rc::new(Shared::default());
        let term = make_terminal(size, config, input.clone(), shared.clone());
        Ok(Session {
            term,
            shared,
            pty,
            input,
            output,
            backlog: Vec::new(),
            parsed: 0,
            eof: false,
            size,
            resize_due: None,
            key_enc: key::Encoder::new().expect("key encoder"),
            key_ev: key::Event::new().expect("key event"),
            mouse_enc: mouse::Encoder::new().expect("mouse encoder"),
            mouse_ev: mouse::Event::new().expect("mouse event"),
            buf: Vec::with_capacity(64),
            scroll_acc: 0.0,
        })
    }

    /// Parses pending output, at most `budget` bytes, so other sessions and input get their
    /// turn. After `more: false` the next output calls `wake` again.
    pub fn feed(&mut self, budget: usize) -> Fed {
        self.take_output();
        let end = (self.parsed + budget).min(self.backlog.len());
        let bytes = end - self.parsed;
        self.term.vt_write(&self.backlog[self.parsed..end]);
        self.parsed = end;
        // Take what arrived meanwhile. The reader wakes the main thread only when it finds
        // `pending` empty, and a wake that came during a `more` chain may already be used up,
        // so `more: false` must mean `pending` was seen empty.
        self.take_output();
        let done = self.parsed == self.backlog.len();
        Fed { bytes, more: !done, exited: done && self.eof }
    }

    /// Takes the reader's pending output once the backlog is all parsed.
    fn take_output(&mut self) {
        if self.parsed < self.backlog.len() {
            return;
        }
        self.backlog.clear();
        self.parsed = 0;
        let mut p = self.output.pending.lock().unwrap();
        // The reader keeps the emptied backlog's allocation for its next bytes.
        std::mem::swap(&mut self.backlog, &mut p.bytes);
        // Taken under the same lock: once eof is set, the reader adds nothing more.
        self.eof = p.eof;
        drop(p);
        self.output.room.notify_one();
    }

    /// Resizes terminal and PTY to `size`, the latest wanted size, once `RESIZE_GATHER` has
    /// passed since the first change; returns when to call again with the then latest size,
    /// or `None` when the size is current.
    pub fn resize(&mut self, size: GridSize, now: Instant) -> Option<Instant> {
        if size == self.size && self.resize_due.is_none() {
            return None;
        }
        let due = *self.resize_due.get_or_insert(now + RESIZE_GATHER);
        if now < due {
            return Some(due);
        }
        self.resize_due = None;
        if size != self.size {
            self.size = size;
            self.shared.grid.set((size.cols, size.rows, size.cell_w, size.cell_h));
            self.term.resize(size.cols, size.rows, size.cell_w, size.cell_h).expect("terminal resize");
            // The child may already be gone; its pane closes on the next feed.
            let _ = self.pty.resize(pty_size(size));
        }
        None
    }

    /// When the program's frame hold (DEC 2026) must end, or `None` when it holds none. A
    /// hold that has lasted `HOLD_LIMIT` by `now` ends here: the terminal has no clock, so a
    /// program that never releases its hold would otherwise freeze the pane, and resetting
    /// the mode lets its next hold begin.
    /// @see https://github.com/ghostty-org/ghostty/blob/108cacb17ed8990840cd14abfce43d8ed8503ca1/include/ghostty/vt/terminal.h (`GhosttyTerminalRenderHoldFn`)
    pub fn hold(&mut self, now: Instant) -> Option<Instant> {
        let deadline = self.shared.render_hold.get()? + HOLD_LIMIT;
        if now < deadline {
            return Some(deadline);
        }
        self.term.set_mode(Mode::SYNC_OUTPUT, false).expect("set mode 2026");
        self.shared.render_hold.set(None);
        None
    }

    /// Queues `data` for the child; never blocks. Input to a child that is gone is dropped.
    pub fn write(&self, data: &[u8]) {
        let _ = self.input.send(data.to_vec());
    }

    pub fn title(&self) -> String {
        self.term.title().map(|t| String::from_utf8_lossy(t).into_owned()).unwrap_or_default()
    }

    /// Encodes a key event per the terminal's current modes (legacy, kitty keyboard, ...).
    pub fn key(&mut self, event: &KeyEvent, mods: ModifiersState) {
        let PhysicalKey::Code(code) = event.physical_key else { return };
        let Some(k) = map_key(code) else { return };
        let pressed = event.state == ElementState::Pressed;
        let action = match (pressed, event.repeat) {
            (false, _) => key::Action::Release,
            (true, true) => key::Action::Repeat,
            (true, false) => key::Action::Press,
        };
        let mods = to_mods(mods);
        let text = event.text.as_ref().filter(|t| pressed && !t.chars().all(char::is_control)).map(|t| t.to_string());
        let unshifted = match event.key_without_modifiers.as_ref() {
            WKey::Character(s) => s.chars().next().unwrap_or('\0'),
            _ => '\0',
        };
        let mut consumed = key::Mods::empty();
        if text.is_some() {
            consumed |= mods & (key::Mods::SHIFT | key::Mods::ALT);
        }
        self.key_ev
            .set_action(action)
            .set_key(k)
            .set_mods(mods)
            .set_consumed_mods(consumed)
            .set_unshifted_codepoint(unshifted)
            .set_utf8(text);
        self.buf.clear();
        let encoded = self.key_enc.set_options_from_terminal(&self.term).encode_to_vec(&self.key_ev, &mut self.buf);
        if encoded.is_ok() && !self.buf.is_empty() {
            self.write(&self.buf);
            self.term.scroll_viewport(ScrollViewport::Bottom);
        }
    }

    pub(crate) fn wants_mouse(&self) -> bool {
        self.term.is_mouse_tracking().unwrap_or(false)
    }

    /// Reports a mouse event at `(x, y)` relative to the content area's top-left corner.
    /// Does nothing unless the child enabled mouse tracking.
    pub fn mouse(
        &mut self,
        action: mouse::Action,
        button: Option<mouse::Button>,
        (x, y): (f32, f32),
        mods: ModifiersState,
        any_pressed: bool,
    ) {
        if !self.wants_mouse() {
            return;
        }
        let s = self.size;
        let _ = self.mouse_enc.set_options_from_terminal(&self.term).set_size(mouse::EncoderSize {
            screen_width: s.width,
            screen_height: s.height,
            cell_width: s.cell_w,
            cell_height: s.cell_h,
            padding_top: s.pad.top,
            padding_bottom: s.pad.bottom,
            padding_left: s.pad.left,
            padding_right: s.pad.right,
        });
        self.mouse_enc.set_any_button_pressed(any_pressed).set_track_last_cell(true);
        self.mouse_ev
            .set_mods(to_mods(mods))
            .set_position(mouse::Position { x, y })
            .set_action(action)
            .set_button(button);
        self.buf.clear();
        if self.mouse_enc.encode_to_vec(&self.mouse_ev, &mut self.buf).is_ok() && !self.buf.is_empty() {
            self.write(&self.buf);
        }
    }

    /// Scrolls by `lines` (positive = up): wheel buttons for mouse-tracking apps, otherwise
    /// the scrollback viewport, 3 rows per line. Fractions accumulate across calls.
    pub fn wheel(&mut self, lines: f32, at: (f32, f32), mods: ModifiersState) {
        self.scroll_acc += lines;
        let whole = self.scroll_acc.trunc();
        self.scroll_acc -= whole;
        if whole == 0.0 {
            return;
        }
        if self.wants_mouse() {
            let b = if whole > 0.0 { mouse::Button::Four } else { mouse::Button::Five };
            for _ in 0..(whole.abs() as i32).min(10) {
                self.mouse(mouse::Action::Press, Some(b), at, mods, false);
                self.mouse(mouse::Action::Release, Some(b), at, mods, false);
            }
        } else {
            self.term.scroll_viewport(ScrollViewport::Delta(-(whole as isize) * 3));
        }
    }

    /// Pastes `text` honoring bracketed-paste mode.
    pub fn paste(&mut self, text: Vec<u8>) {
        let _ = self
            .term
            .paste(paste::Options::new(), &[ClipboardMime::new(b"text/plain")], move |_mime, w| w.write_all(&text));
    }
}

impl Drop for Session {
    /// Releases a reader waiting for room. A reader blocked in `read`, and a writer blocked
    /// mid-write, end once every process holding the child side has closed it; dropping the
    /// `Pty` hangs up the child's group, but a descendant outside it (a disowned job) keeps
    /// both threads blocked until it exits or writes.
    fn drop(&mut self) {
        self.output.pending.lock().unwrap().closed = true;
        self.output.room.notify_one();
    }
}

/// Plain text of the visible screen, for tests.
#[cfg(test)]
impl Session {
    pub fn screen_text(&self) -> String {
        let mut f = libghostty_vt::fmt::Formatter::new(&self.term).expect("formatter");
        f.set_format(libghostty_vt::fmt::Format::Plain).expect("plain");
        String::from_utf8_lossy(&f.format_alloc(None).expect("format")).into_owned()
    }
}

fn pty_size(s: GridSize) -> pty::Size {
    // A content area lies inside one Metal drawable, a texture of at most 16384 px a side.
    // @see https://developer.apple.com/metal/Metal-Feature-Set-Tables.pdf ("Maximum 2D texture width and height")
    let px = |n: u32| u16::try_from(n).expect("content area fits a u16 pixel size");
    pty::Size { cols: s.cols, rows: s.rows, width: px(s.width), height: px(s.height) }
}

fn make_terminal(
    size: GridSize,
    config: &Config,
    input: mpsc::Sender<Vec<u8>>,
    shared: Rc<Shared>,
) -> Terminal<'static, 'static> {
    let mut t = Terminal::new(size.cols, size.rows).expect("terminal");
    t.set_scrollback_max_lines(Some(10_000)).unwrap().set_terminfo_name(TERM).unwrap();
    // The configured colors become the terminal's defaults, which programs query (OSC 4,
    // 10, 11, 12) and may override. Background and foreground are always set: without them
    // libghostty-vt draws white on black and answers no OSC 10 or 11 query. An unset cursor
    // color and palette entries keep libghostty-vt's.
    let rgb = |c: frith_core::config::Rgb| Rgb { r: c.r, g: c.g, b: c.b };
    let mut palette = t.default_color_palette().unwrap();
    for (&i, &c) in &config.palette {
        palette.0[usize::from(i)] = rgb(c);
    }
    t.set_default_fg_color(Some(rgb(config.foreground)))
        .unwrap()
        .set_default_bg_color(Some(rgb(config.background)))
        .unwrap()
        .set_default_cursor_color(config.cursor_color.map(rgb))
        .unwrap()
        .set_default_color_palette(Some(palette))
        .unwrap();
    t.resize(size.cols, size.rows, size.cell_w, size.cell_h).unwrap();
    shared.grid.set((size.cols, size.rows, size.cell_w, size.cell_h));

    let s = shared.clone();
    t.on_pty_write(move |_t, data| {
        let _ = input.send(data.to_vec());
    })
    .unwrap()
    .on_size(move |_t| {
        let (columns, rows, cell_width, cell_height) = s.grid.get();
        Some(SizeReportSize { rows, columns, cell_width, cell_height })
    })
    .unwrap()
    .on_device_attributes(|_t| {
        Some(DeviceAttributes {
            primary: PrimaryDeviceAttributes::new(
                ConformanceLevel::VT220,
                &[
                    DeviceAttributeFeature::COLUMNS_132,
                    DeviceAttributeFeature::SELECTIVE_ERASE,
                    DeviceAttributeFeature::ANSI_COLOR,
                ],
            ),
            secondary: SecondaryDeviceAttributes {
                device_type: DeviceType::VT220,
                firmware_version: 1,
                rom_cartridge: 0,
            },
            tertiary: Default::default(),
        })
    })
    .unwrap()
    .on_xtversion(|_t| Some(NAME))
    .unwrap()
    .on_color_scheme(|_t| None)
    .unwrap();

    let s = shared.clone();
    // Timed from here, the moment the hold begins, not from when a frame is next drawn.
    t.on_render_hold(move |_t, hold| s.render_hold.set(hold.then(Instant::now))).unwrap();
    let s = shared.clone();
    t.on_title_changed(move |_t| s.title_changed.set(true)).unwrap();
    let s = shared;
    t.on_desktop_notification(move |_t, _n| s.notified.set(true)).unwrap();
    t
}

fn to_mods(m: ModifiersState) -> key::Mods {
    let mut out = key::Mods::empty();
    if m.shift_key() {
        out |= key::Mods::SHIFT;
    }
    if m.control_key() {
        out |= key::Mods::CTRL;
    }
    if m.alt_key() {
        out |= key::Mods::ALT;
    }
    if m.meta_key() {
        out |= key::Mods::SUPER;
    }
    out
}

/// winit physical key -> ghostty key. Both follow W3C UI Events `code` names.
#[expect(clippy::too_many_lines, reason = "a table: one arm per key code")]
fn map_key(code: KeyCode) -> Option<Key> {
    use KeyCode as K;
    Some(match code {
        K::KeyA => Key::A,
        K::KeyB => Key::B,
        K::KeyC => Key::C,
        K::KeyD => Key::D,
        K::KeyE => Key::E,
        K::KeyF => Key::F,
        K::KeyG => Key::G,
        K::KeyH => Key::H,
        K::KeyI => Key::I,
        K::KeyJ => Key::J,
        K::KeyK => Key::K,
        K::KeyL => Key::L,
        K::KeyM => Key::M,
        K::KeyN => Key::N,
        K::KeyO => Key::O,
        K::KeyP => Key::P,
        K::KeyQ => Key::Q,
        K::KeyR => Key::R,
        K::KeyS => Key::S,
        K::KeyT => Key::T,
        K::KeyU => Key::U,
        K::KeyV => Key::V,
        K::KeyW => Key::W,
        K::KeyX => Key::X,
        K::KeyY => Key::Y,
        K::KeyZ => Key::Z,
        K::Digit0 => Key::Digit0,
        K::Digit1 => Key::Digit1,
        K::Digit2 => Key::Digit2,
        K::Digit3 => Key::Digit3,
        K::Digit4 => Key::Digit4,
        K::Digit5 => Key::Digit5,
        K::Digit6 => Key::Digit6,
        K::Digit7 => Key::Digit7,
        K::Digit8 => Key::Digit8,
        K::Digit9 => Key::Digit9,
        K::Backquote => Key::Backquote,
        K::Backslash => Key::Backslash,
        K::BracketLeft => Key::BracketLeft,
        K::BracketRight => Key::BracketRight,
        K::Comma => Key::Comma,
        K::Equal => Key::Equal,
        K::Minus => Key::Minus,
        K::Period => Key::Period,
        K::Quote => Key::Quote,
        K::Semicolon => Key::Semicolon,
        K::Slash => Key::Slash,
        K::IntlBackslash => Key::IntlBackslash,
        K::Backspace => Key::Backspace,
        K::Enter => Key::Enter,
        K::Space => Key::Space,
        K::Tab => Key::Tab,
        K::Escape => Key::Escape,
        K::Delete => Key::Delete,
        K::End => Key::End,
        K::Home => Key::Home,
        K::Insert => Key::Insert,
        K::PageDown => Key::PageDown,
        K::PageUp => Key::PageUp,
        K::ArrowDown => Key::ArrowDown,
        K::ArrowLeft => Key::ArrowLeft,
        K::ArrowRight => Key::ArrowRight,
        K::ArrowUp => Key::ArrowUp,
        K::ShiftLeft => Key::ShiftLeft,
        K::ShiftRight => Key::ShiftRight,
        K::ControlLeft => Key::ControlLeft,
        K::ControlRight => Key::ControlRight,
        K::AltLeft => Key::AltLeft,
        K::AltRight => Key::AltRight,
        K::MetaLeft => Key::MetaLeft,
        K::MetaRight => Key::MetaRight,
        K::CapsLock => Key::CapsLock,
        K::F1 => Key::F1,
        K::F2 => Key::F2,
        K::F3 => Key::F3,
        K::F4 => Key::F4,
        K::F5 => Key::F5,
        K::F6 => Key::F6,
        K::F7 => Key::F7,
        K::F8 => Key::F8,
        K::F9 => Key::F9,
        K::F10 => Key::F10,
        K::F11 => Key::F11,
        K::F12 => Key::F12,
        K::Numpad0 => Key::Numpad0,
        K::Numpad1 => Key::Numpad1,
        K::Numpad2 => Key::Numpad2,
        K::Numpad3 => Key::Numpad3,
        K::Numpad4 => Key::Numpad4,
        K::Numpad5 => Key::Numpad5,
        K::Numpad6 => Key::Numpad6,
        K::Numpad7 => Key::Numpad7,
        K::Numpad8 => Key::Numpad8,
        K::Numpad9 => Key::Numpad9,
        K::NumpadAdd => Key::NumpadAdd,
        K::NumpadDecimal => Key::NumpadDecimal,
        K::NumpadDivide => Key::NumpadDivide,
        K::NumpadEnter => Key::NumpadEnter,
        K::NumpadEqual => Key::NumpadEqual,
        K::NumpadMultiply => Key::NumpadMultiply,
        K::NumpadSubtract => Key::NumpadSubtract,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(line: &str) -> Session {
        let size = GridSize::fit(800, 240, 10, 20, Pad::default());
        let mut s =
            Session::spawn(&Program::Command(line.into()), Path::new("/"), size, &Config::default(), None, || {})
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !s.feed(1 << 20).exited {
            assert!(Instant::now() < deadline, "child did not exit");
            thread::sleep(Duration::from_millis(5));
        }
        s
    }

    /// Starts `line` and feeds its output until the screen shows `marker`.
    fn started(line: &str, marker: &str) -> Session {
        started_with(line, marker, GridSize::fit(800, 240, 10, 20, Pad::default()), &Config::default())
    }

    fn started_with(line: &str, marker: &str, size: GridSize, config: &Config) -> Session {
        let mut s = Session::spawn(&Program::Command(line.into()), Path::new("/"), size, config, None, || {}).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !s.screen_text().contains(marker) {
            assert!(Instant::now() < deadline, "no {marker:?}: {:?}", s.screen_text());
            s.feed(1 << 20);
            thread::sleep(Duration::from_millis(5));
        }
        s
    }

    #[test]
    fn a_large_paste_into_a_program_that_reads_no_input_does_not_block_the_caller() {
        let s = started("stty raw -echo; printf ready; sleep 3", "ready");
        let start = Instant::now();
        s.write(&vec![b'a'; 1 << 20]);
        assert!(start.elapsed() < Duration::from_millis(500), "write blocked for {:?}", start.elapsed());
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore = "a debug libghostty-vt parses about 0.12 MB/s; run with --release")]
    fn output_keeps_flowing_while_the_main_thread_renders() {
        // 10 MB with 8 ms of rendering before each feed. Measured in release on an M-series Mac:
        // 1.7 MB/s (about 5.8 s) when 16 reads of 1024 bytes could wait between frames;
        // 0.6-0.7 s with `OUTPUT_CAP` at 1 MiB.
        let mut s = started("printf ready; sleep 0.3; head -c 10000000 /dev/zero | tr '\\0' a", "ready");
        let start = Instant::now();
        while !s.feed(1 << 20).exited {
            assert!(start.elapsed() < Duration::from_millis(2500), "output throttled by the reader buffer");
            thread::sleep(Duration::from_millis(8));
        }
    }

    #[test]
    fn an_app_that_feeds_only_on_wake_or_more_sees_all_output_and_the_exit() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let woken = Arc::new(AtomicBool::new(false));
        let w = woken.clone();
        let size = GridSize::fit(800, 240, 10, 20, Pad::default());
        // The child stays alive: its exit would wake the app and hide a stall.
        let line = "head -c 100000 /dev/zero | tr '\\0' a; printf done; sleep 30";
        let mut s =
            Session::spawn(&Program::Command(line.into()), Path::new("/"), size, &Config::default(), None, move || {
                w.store(true, Ordering::Release);
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut more = false;
        loop {
            // A wake that arrives mid-chain is handled (and so used up) like any other.
            let wake = woken.swap(false, Ordering::AcqRel);
            if more || wake {
                more = s.feed(1024).more; // small budget: long `more` chains, as with many busy panes
            } else if s.screen_text().contains("done") {
                break;
            } else {
                assert!(Instant::now() < deadline, "stalled without a wake: {:?}", s.screen_text());
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    #[test]
    fn without_a_config_the_terminal_reports_ghosttys_default_colors() {
        let line = "stty raw -echo min 0 time 5; printf '\\033]10;?\\a\\033]11;?\\a'; \
                    r=$(cat); stty sane; printf '%s' \"$r\" | tr '\\007' '\\n' | cat -v; echo end";
        let s = started(line, "end");
        let text = s.screen_text();
        for want in ["]10;rgb:ffff/ffff/ffff", "]11;rgb:2828/2c2c/3434"] {
            assert!(text.contains(want), "no {want:?} in {text:?}");
        }
    }

    #[test]
    fn the_configured_colors_are_what_the_terminal_reports() {
        let mut c = Config::default();
        c.read("background = #21252b\nforeground = #abb2bf\ncursor-color = #112233\npalette = 1=#e06c75\n");
        // The replies come back on stdin; `cat` reads them until 0.5 s pass without input
        // (`min 0 time 5`), then prints them with BEL as a line break.
        let line = "stty raw -echo min 0 time 5; \
                    printf '\\033]10;?\\a\\033]11;?\\a\\033]12;?\\a\\033]4;1;?\\a'; \
                    r=$(cat); stty sane; \
                    printf '%s' \"$r\" | tr '\\007' '\\n' | cat -v; echo end";
        let s = started_with(line, "end", GridSize::fit(800, 240, 10, 20, Pad::default()), &c);
        let text = s.screen_text();
        for want in
            ["]10;rgb:abab/b2b2/bfbf", "]11;rgb:2121/2525/2b2b", "]12;rgb:1111/2222/3333", "]4;1;rgb:e0e0/6c6c/7575"]
        {
            assert!(text.contains(want), "no {want:?} in {text:?}");
        }
    }

    #[test]
    fn padding_on_each_side_shrinks_the_grid_and_offsets_mouse_reports() {
        let pad = Pad { left: 30, top: 12, right: 10, bottom: 8 };
        let size = GridSize::fit(800, 240, 10, 20, pad);
        assert_eq!((size.cols, size.rows), (76, 11), "(800 - 40) / 10, (240 - 20) / 20");
        // SGR mouse reports; the child prints the first report it reads.
        let line = "printf '\\033[?1000h\\033[?1006h'; stty raw -echo min 1; printf ready; \
                    r=$(dd bs=64 count=1 2>/dev/null); stty sane; printf '%s' \"$r\" | cat -v; echo end";
        let mut s = started_with(line, "ready", size, &Config::default());
        // Inside the third column and the second row of the grid.
        s.mouse(
            mouse::Action::Press,
            Some(mouse::Button::Left),
            (30.0 + 25.0, 12.0 + 25.0),
            ModifiersState::empty(),
            true,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !s.screen_text().contains("end") {
            assert!(Instant::now() < deadline, "{:?}", s.screen_text());
            s.feed(1 << 20);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(s.screen_text().contains("^[[<0;3;2M"), "{:?}", s.screen_text());
    }

    #[test]
    fn padding_larger_than_the_content_leaves_the_smallest_grid() {
        let pad = Pad { left: u32::MAX, top: u32::MAX, right: u32::MAX, bottom: u32::MAX };
        let size = GridSize::fit(800, 240, 10, 20, pad);
        assert_eq!((size.cols, size.rows), (2, 2));
        assert!(size.pad.left + size.pad.right <= 800 && size.pad.top + size.pad.bottom <= 240, "{:?}", size.pad);
    }

    #[test]
    fn input_reaches_the_child_in_order() {
        let mut s = started("stty -echo; printf ready; read a; read b; printf '<%s|%s>' \"$a\" \"$b\"", "ready");
        s.write(b"one\r");
        s.write(b"two\r");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !s.screen_text().contains("<one|two>") {
            assert!(Instant::now() < deadline, "{:?}", s.screen_text());
            s.feed(1 << 20);
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn output_is_parsed_into_the_screen_including_styles_and_wide_characters() {
        let s = run(r"printf 'hello \033[1;31m世界\033[0m!'");
        assert!(s.screen_text().contains("hello 世界!"), "{:?}", s.screen_text());
    }

    #[test]
    fn a_frame_hold_is_timed_from_its_own_start_and_a_timed_out_hold_lets_the_next_one_begin() {
        let line = r"printf '\033[?2026hA'; sleep 0.3; printf '\033[?2026l\033[?2026hB'; read x; printf '\033[?2026hC'; sleep 3";
        let mut s = started(line, "A");
        let first = s.hold(Instant::now()).expect("A is held");
        let wait = Instant::now() + Duration::from_secs(5);
        while !s.screen_text().contains('B') {
            assert!(Instant::now() < wait, "no B: {:?}", s.screen_text());
            s.feed(1 << 20);
            thread::sleep(Duration::from_millis(5));
        }
        let deadline = s.hold(Instant::now()).expect("B is held");
        assert!(deadline >= first + Duration::from_millis(300), "timed from the second hold, not the first");
        assert!(deadline <= Instant::now() + HOLD_LIMIT);
        assert_eq!(s.hold(deadline), None, "a hold ends at its deadline");
        assert!(!s.term.mode(Mode::SYNC_OUTPUT).unwrap(), "and the mode is reset, as the terminal has no clock");
        s.write(b"\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !s.screen_text().contains('C') {
            assert!(Instant::now() < deadline, "no C: {:?}", s.screen_text());
            s.feed(1 << 20);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(s.hold(Instant::now()).is_some(), "the program can hold again");
    }

    #[test]
    fn resizes_within_25_ms_of_the_first_reach_the_terminal_and_child_as_one() {
        let mut s = started("trap 'printf \"<winch>\"' WINCH; printf ready; while :; do sleep 0.01; done", "ready");
        let (before, wide, narrow) = (
            s.term.cols().unwrap(),
            GridSize::fit(600, 240, 10, 20, Pad::default()),
            GridSize::fit(500, 240, 10, 20, Pad::default()),
        );
        let t0 = Instant::now();
        let due = t0 + Duration::from_millis(25);
        assert_eq!(s.resize(wide, t0), Some(due), "waits for more");
        assert_eq!(s.term.cols().unwrap(), before);
        assert_eq!(s.resize(narrow, t0 + Duration::from_millis(20)), Some(due), "later ones do not extend the wait");
        let settle = |s: &mut Session| {
            let end = Instant::now() + Duration::from_millis(300);
            while Instant::now() < end {
                s.feed(1 << 20);
                thread::sleep(Duration::from_millis(5));
            }
        };
        settle(&mut s);
        assert_eq!(s.screen_text().matches("<winch>").count(), 0, "the child waits too: {:?}", s.screen_text());
        assert_eq!(s.resize(narrow, due), None, "the last size is applied once the wait is over");
        assert_eq!(s.term.cols().unwrap(), narrow.cols);
        assert_eq!(s.resize(narrow, due + Duration::from_millis(1)), None, "unchanged: nothing to do");
        settle(&mut s);
        assert_eq!(s.screen_text().matches("<winch>").count(), 1, "{:?}", s.screen_text());
    }

    #[test]
    fn the_child_sees_the_terminal_type_and_a_utf8_locale() {
        // Assumes the test runs with LANG unset, empty or UTF-8; a LANG=C is respected.
        let s = run(r#"printf '%s %s %s' "$TERM" "$TERM_PROGRAM" "$(locale charmap)""#);
        let text = s.screen_text();
        assert!(text.contains("xterm-256color frith UTF-8"), "{text:?}");
    }

    /// An interactive zsh with Frith's integration, as `Session::spawn` sets it up, but with
    /// HOME in a scratch directory holding `zshenv` and `zshrc`, so the user's own dotfiles
    /// stay out of the test. With `zdotdir`, the files sit in a ZDOTDIR of their own, else in
    /// HOME, as for most users.
    fn zsh_in(name: &str, zdotdir: bool, zshenv: &str, zshrc: &str) -> Session {
        let root = crate::shell::tests::scratch(name);
        let user = if zdotdir { root.join("user") } else { root.clone() };
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join(".zshenv"), zshenv).unwrap();
        std::fs::write(user.join(".zshrc"), zshrc).unwrap();
        let dir = crate::shell::install(&root.join("cache")).unwrap();
        let vars = crate::shell::env(Path::new("/bin/zsh"), &dir, zdotdir.then(|| user.clone().into()));
        let assigns: Vec<String> = vars.iter().map(|(k, v)| format!("{k}='{}'", v.to_str().unwrap())).collect();
        // `env -u` keeps a ZDOTDIR of the developer's out of the shell.
        let line = format!("exec env -u ZDOTDIR HOME='{}' {} /bin/zsh -i", root.display(), assigns.join(" "));
        let size = GridSize::fit(800, 240, 10, 20, Pad::default());
        let mut s =
            Session::spawn(&Program::Command(line), Path::new("/"), size, &Config::default(), None, || {}).unwrap();
        settle_until(&mut s, |t| t.trim_end().ends_with('>'));
        s
    }

    fn settle_until(s: &mut Session, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(&s.screen_text()) {
            assert!(Instant::now() < deadline, "timed out: {:?}", s.screen_text());
            s.feed(1 << 20);
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// The prompt after `echo $((6*7))` is drawn: its `top` line is the second on screen.
    fn at_second_prompt(t: &str) -> bool {
        t.matches("top").count() == 2 && t.trim_end().ends_with('>')
    }

    /// No resize duplicated or lost a prompt or the command's output.
    #[track_caller]
    fn assert_two_prompts_and_one_answer(s: &Session) {
        let t = s.screen_text();
        assert_eq!((t.matches("top").count(), t.matches("\n42").count()), (2, 1), "{t:?}");
    }

    /// Resizes to each width in turn, letting zsh redraw after each.
    fn resize_through(s: &mut Session, widths: &[u32]) {
        for &w in widths {
            let size = GridSize::fit(w * 10, 240, 10, 20, Pad::default());
            let now = Instant::now();
            s.resize(size, now);
            assert_eq!(s.resize(size, now + RESIZE_GATHER), None);
            let end = Instant::now() + Duration::from_millis(150);
            while Instant::now() < end {
                s.feed(1 << 20);
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Like p10k's first line, the prompt's top line fills the width, so a narrower terminal
    /// wraps it onto two rows while zsh, redrawing, moves up only the one it printed.
    const FULL_WIDTH_PROMPT: &str = "setopt prompt_subst\nPS1='top${(l:COLUMNS-3::-:)}'$'\\n> '\n";

    #[test]
    fn resizing_at_a_zsh_prompt_leaves_one_prompt() {
        let mut s = zsh_in("one-prompt", true, "", FULL_WIDTH_PROMPT);
        resize_through(&mut s, &[60, 80, 50, 80, 40, 80]);
        assert_eq!(s.screen_text().matches("top").count(), 1, "{:?}", s.screen_text());
    }

    #[test]
    fn a_prompt_rebuilt_by_the_last_precmd_hook_like_p10k_is_still_cleared() {
        // p10k rebuilds PROMPT in a precmd hook it keeps last, marking it itself when asked
        // through this global (internal/p10k.zsh, `_p9k_init_params`).
        let p10k = r#"
_like_p10k() {
  if (( ${+__p9k_force_term_shell_integration} )); then
    PROMPT=$'%{\e]133;A\a%}top${(l:COLUMNS-3::-:)}\n> %{\e]133;B\a%}'
  else
    PROMPT=$'top${(l:COLUMNS-3::-:)}\n> '
  fi
}
setopt prompt_subst
precmd_functions+=(_like_p10k)
"#;
        let mut s = zsh_in("like-p10k", true, "", p10k);
        // After a command, only the marks on the new prompt tell where it starts.
        s.write(b"echo $((6*7))\r");
        settle_until(&mut s, at_second_prompt);
        resize_through(&mut s, &[60, 80, 50, 80]);
        assert_two_prompts_and_one_answer(&s);
    }

    #[test]
    fn command_output_and_earlier_prompts_survive_a_resize() {
        let mut s = zsh_in("output", true, "", FULL_WIDTH_PROMPT);
        // Resized while the command still runs, and again at the next prompt.
        s.write(b"echo $((6*7)); sleep 0.5\r");
        settle_until(&mut s, |t| t.contains("\n42"));
        resize_through(&mut s, &[60, 80]);
        settle_until(&mut s, at_second_prompt);
        resize_through(&mut s, &[50, 80]);
        assert_two_prompts_and_one_answer(&s);
    }

    #[test]
    fn without_a_zdotdir_the_users_home_files_apply_and_nothing_leaks_to_children() {
        let rc = "print -r -- \"rc zdotdir=${ZDOTDIR-unset} env=$FROM_USER_ZSHENV child=$(env | grep -c -e ZDOTDIR)\"\nPS1='> '\n";
        let s = zsh_in("home-files", false, "export FROM_USER_ZSHENV=yes\n", rc);
        assert!(s.screen_text().contains("rc zdotdir=unset env=yes child=0"), "{:?}", s.screen_text());
    }

    #[test]
    fn startup_output_survives_a_resize_even_when_nobody_marks_the_prompt() {
        // A prompt framework that rebuilds the prompt after our hook and marks nothing: its
        // prompts may pile up as without integration, but nothing .zshrc printed is lost.
        let rc = r#"
print BANNER
_unmarked() { PROMPT=$'top${(l:COLUMNS-3::-:)}\n> ' }
setopt prompt_subst
precmd_functions+=(_unmarked)
"#;
        let mut s = zsh_in("banner", false, "", rc);
        resize_through(&mut s, &[60, 80]);
        assert!(s.screen_text().contains("BANNER"), "{:?}", s.screen_text());
    }

    #[test]
    fn prompts_are_marked_only_where_zsh_expands_percent_escapes() {
        let rc = "setopt no_prompt_percent\nPS1='> '\n";
        let mut s = zsh_in("no-percent", false, "", rc);
        s.write(b"echo hi\r");
        settle_until(&mut s, |t| t.contains("\nhi"));
        assert!(!s.screen_text().contains("%{"), "{:?}", s.screen_text());
    }

    #[test]
    fn a_login_shell_session_loads_the_integration_it_is_given() {
        let shell = pty::login_shell().unwrap();
        if shell.file_name() != Some(std::ffi::OsStr::new("zsh")) {
            eprintln!("skipped: the login shell {shell:?} is not zsh");
            return;
        }
        // A stand-in for the installed integration that only reports and exits.
        let dir = crate::shell::tests::scratch("wiring");
        std::fs::create_dir_all(dir.join("zsh")).unwrap();
        std::fs::write(dir.join("zsh/.zshenv"), "print -r -- \"wired ${ZDOTDIR:t}\"\nexit\n").unwrap();
        let size = GridSize::fit(800, 240, 10, 20, Pad::default());
        let mut s =
            Session::spawn(&Program::LoginShell, Path::new("/"), size, &Config::default(), Some(&dir), || {}).unwrap();
        settle_until(&mut s, |t| t.contains("wired"));
        assert!(s.screen_text().contains("wired zsh"), "{:?}", s.screen_text());
    }

    #[test]
    fn the_users_zdotdir_and_zshenv_still_apply() {
        let rc =
            "print -r -- \"zdotdir=${ZDOTDIR:t} env=$FROM_USER_ZSHENV saved=${FRITH_ZSH_ZDOTDIR-none}\"\nPS1='> '\n";
        let s = zsh_in("user-files", true, "export FROM_USER_ZSHENV=yes\n", rc);
        assert!(s.screen_text().contains("zdotdir=user env=yes saved=none"), "{:?}", s.screen_text());
    }
}
