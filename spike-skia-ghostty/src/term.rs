//! One terminal session: a child process on a PTY, its libghostty-vt terminal, and the
//! encoding of keyboard, mouse, wheel and paste input into bytes for the child.
//!
//! Answers: how do bytes and input flow between the user, libghostty-vt and the child?
//! Must not know: where the session is on screen (callers pass grid sizes and positions
//! local to the terminal content), how it is drawn, or which window it belongs to.
//!
//! The IO thread only forwards bytes; the `Terminal` is `!Send` and stays on the main thread.

use std::cell::Cell;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use libghostty_vt::effect::{
    ClipboardMime, ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType,
    PrimaryDeviceAttributes, SecondaryDeviceAttributes,
};
use libghostty_vt::key::{self, Key};
use libghostty_vt::report::SizeReportSize;
use libghostty_vt::terminal::{ScrollViewport, Terminal};
use libghostty_vt::{mouse, paste};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key as WKey, KeyCode, ModifiersState, PhysicalKey};

pub const TERM: &str = "xterm-256color";

/// Grid geometry of a session's content area, in physical pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
    /// Padding around the grid inside the content area.
    pub pad: u32,
    /// Size of the content area.
    pub width: u32,
    pub height: u32,
}

impl GridSize {
    /// The largest grid that fits `width`×`height` with `pad` on every side (at least 2×2).
    pub fn fit(width: u32, height: u32, cell_w: u32, cell_h: u32, pad: u32) -> GridSize {
        let cells = |len: u32, cell: u32| (len.saturating_sub(2 * pad) / cell).clamp(2, u16::MAX as u32) as u16;
        GridSize { cols: cells(width, cell_w), rows: cells(height, cell_h), cell_w, cell_h, pad, width, height }
    }
}

/// Effect callbacks write here; the app reads and clears the flags.
#[derive(Default)]
pub struct Shared {
    grid: Cell<(u16, u16, u32, u32)>, // cols, rows, cell_w, cell_h
    pub render_hold: Cell<bool>,
    pub title_changed: Cell<bool>,
    /// Set by OSC 9 / OSC 777 desktop notifications.
    pub notified: Cell<bool>,
}

/// Result of feeding pending PTY output into the terminal.
pub struct Fed {
    pub bytes: usize,
    /// The budget ran out; more output may be waiting.
    pub more: bool,
    /// The child has exited and all its output has been consumed.
    pub exited: bool,
}

/// The OS side of a session. Kept together so that closing ends them in a safe order:
/// see `Drop for Session`.
struct Process {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    /// Never read: held only so the PTY writer outlives the child (see `Drop for Session`).
    _writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

pub struct Session {
    pub term: Terminal<'static, 'static>,
    pub shared: Rc<Shared>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// `Some` until the session drops.
    process: Option<Process>,
    rx: mpsc::Receiver<Vec<u8>>,
    eof: Arc<AtomicBool>,
    size: GridSize,
    key_enc: key::Encoder<'static>,
    key_ev: key::Event<'static>,
    mouse_enc: mouse::Encoder<'static>,
    mouse_ev: mouse::Event<'static>,
    buf: Vec<u8>,
    scroll_acc: f32,
}

impl Session {
    /// Runs `cmd` via `/bin/sh -c`, or the user's login shell when `cmd` is `None`.
    /// `wake` is called from the IO thread whenever output arrives or the child exits.
    pub fn spawn(cmd: Option<&str>, size: GridSize, wake: impl Fn() + Send + 'static) -> Session {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: size.width as u16,
                pixel_height: size.height as u16,
            })
            .expect("openpty");
        let mut cb = match cmd {
            Some(c) => {
                let mut b = CommandBuilder::new("/bin/sh");
                b.args(["-c", c]);
                b
            }
            None => CommandBuilder::new_default_prog(),
        };
        cb.env("TERM", TERM);
        cb.env("COLORTERM", "truecolor");
        cb.env("TERM_PROGRAM", "spike-skia-ghostty");
        // Apps launched outside a shell (Finder, launchd) carry no locale; without a UTF-8
        // LC_CTYPE, zsh's line editor shows typed CJK as <00ad>-style escapes.
        let utf8 = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .is_some_and(|v| v.to_ascii_uppercase().replace('-', "").contains("UTF8"));
        if !utf8 {
            cb.env("LANG", "en_US.UTF-8");
        }
        if let Ok(dir) = std::env::current_dir() {
            cb.cwd(dir);
        }
        let child = pair.slave.spawn_command(cb).expect("spawn");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("pty writer")));
        let (tx, rx) = mpsc::channel();
        let eof = Arc::new(AtomicBool::new(false));
        let eof_io = eof.clone();
        thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        eof_io.store(true, Ordering::Release);
                        wake();
                        break;
                    }
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // session dropped
                        }
                        wake();
                    }
                }
            }
        });
        let shared = Rc::new(Shared::default());
        let term = make_terminal(size, writer.clone(), shared.clone());
        Session {
            term,
            shared,
            process: Some(Process { child, master: pair.master, _writer: writer.clone() }),
            writer,
            rx,
            eof,
            size,
            key_enc: key::Encoder::new().expect("key encoder"),
            key_ev: key::Event::new().expect("key event"),
            mouse_enc: mouse::Encoder::new().expect("mouse encoder"),
            mouse_ev: mouse::Event::new().expect("mouse event"),
            buf: Vec::with_capacity(64),
            scroll_acc: 0.0,
        }
    }

    /// Parses pending output, at most about `budget` bytes, so other sessions and input
    /// get their turn.
    pub fn feed(&mut self, budget: usize) -> Fed {
        // Read EOF before draining: the IO thread sets it after its last send, so EOF seen
        // here plus an empty channel afterwards means every byte has been parsed.
        let eof = self.eof.load(Ordering::Acquire);
        let mut bytes = 0;
        while bytes < budget {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    bytes += chunk.len();
                    self.term.vt_write(&chunk);
                }
                Err(_) => return Fed { bytes, more: false, exited: eof },
            }
        }
        Fed { bytes, more: true, exited: false }
    }

    /// Resizes terminal and PTY. Returns false when nothing changed.
    pub fn resize(&mut self, size: GridSize) -> bool {
        if size == self.size {
            return false;
        }
        self.size = size;
        self.shared.grid.set((size.cols, size.rows, size.cell_w, size.cell_h));
        self.term.resize(size.cols, size.rows, size.cell_w, size.cell_h).expect("terminal resize");
        let master = &self.process.as_ref().expect("process lives until drop").master;
        let _ = master.resize(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.width as u16,
            pixel_height: size.height as u16,
        });
        true
    }

    pub fn write(&self, data: &[u8]) {
        let _ = self.writer.lock().unwrap().write_all(data);
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
        let text = event
            .text
            .as_ref()
            .filter(|t| pressed && !t.chars().all(char::is_control))
            .map(|t| t.to_string());
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
            let _ = self.writer.lock().unwrap().write_all(&self.buf);
            self.term.scroll_viewport(ScrollViewport::Bottom);
        }
    }

    pub fn wants_mouse(&self) -> bool {
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
            padding_top: s.pad,
            padding_bottom: s.pad,
            padding_left: s.pad,
            padding_right: s.pad,
        });
        self.mouse_enc.set_any_button_pressed(any_pressed).set_track_last_cell(true);
        self.mouse_ev
            .set_mods(to_mods(mods))
            .set_position(mouse::Position { x, y })
            .set_action(action)
            .set_button(button);
        self.buf.clear();
        if self.mouse_enc.encode_to_vec(&self.mouse_ev, &mut self.buf).is_ok() && !self.buf.is_empty() {
            let _ = self.writer.lock().unwrap().write_all(&self.buf);
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
        let _ = self.term.paste(paste::Options::new(), &[ClipboardMime::new(b"text/plain")], move |_mime, w| {
            w.write_all(&text)
        });
    }
}

impl Drop for Session {
    /// Closing a pane ends its program, as closing a terminal window does, without blocking
    /// the main thread and without leaving a zombie.
    ///
    /// portable-pty's `kill` sends SIGHUP, polls for up to ~200 ms, then sends SIGKILL, and
    /// never reaps; and dropping the last PTY writer writes `\n` + VEOF into the PTY, which
    /// would run a half-typed command line if the child were still alive. So the child, the
    /// master and one writer handle move to a thread that kills, then waits, and only then
    /// lets the writer go.
    /// @see portable-pty 0.9.0 src/lib.rs `impl ChildKiller for std::process::Child`
    /// @see portable-pty 0.9.0 src/unix.rs `impl Drop for UnixMasterWriter`
    fn drop(&mut self) {
        let Some(mut p) = self.process.take() else { return };
        thread::spawn(move || {
            let _ = p.child.kill();
            let _ = p.child.wait();
            drop(p);
        });
    }
}

fn make_terminal(
    size: GridSize,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    shared: Rc<Shared>,
) -> Terminal<'static, 'static> {
    let mut t = Terminal::new(size.cols, size.rows).expect("terminal");
    t.set_scrollback_max_lines(Some(10_000)).unwrap().set_terminfo_name(TERM).unwrap();
    t.resize(size.cols, size.rows, size.cell_w, size.cell_h).unwrap();
    shared.grid.set((size.cols, size.rows, size.cell_w, size.cell_h));

    let s = shared.clone();
    t.on_pty_write(move |_t, data| {
        let _ = writer.lock().unwrap().write_all(data);
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
    .on_xtversion(|_t| Some("spike-skia-ghostty"))
    .unwrap()
    .on_color_scheme(|_t| None)
    .unwrap();

    let s = shared.clone();
    t.on_render_hold(move |_t, hold| s.render_hold.set(hold)).unwrap();
    let s = shared.clone();
    t.on_title_changed(move |_t| s.title_changed.set(true)).unwrap();
    let s = shared;
    t.on_desktop_notification(move |_t, n| {
        s.notified.set(true);
        eprintln!(
            "[osc-notify] title={:?} body={:?}",
            String::from_utf8_lossy(n.title()),
            String::from_utf8_lossy(n.body())
        );
    })
    .unwrap();
    t.on_clipboard_write(move |_t, w| eprintln!("[osc52-write] granted={}", w.granted())).unwrap();
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
fn map_key(code: KeyCode) -> Option<Key> {
    use KeyCode as K;
    Some(match code {
        K::KeyA => Key::A, K::KeyB => Key::B, K::KeyC => Key::C, K::KeyD => Key::D,
        K::KeyE => Key::E, K::KeyF => Key::F, K::KeyG => Key::G, K::KeyH => Key::H,
        K::KeyI => Key::I, K::KeyJ => Key::J, K::KeyK => Key::K, K::KeyL => Key::L,
        K::KeyM => Key::M, K::KeyN => Key::N, K::KeyO => Key::O, K::KeyP => Key::P,
        K::KeyQ => Key::Q, K::KeyR => Key::R, K::KeyS => Key::S, K::KeyT => Key::T,
        K::KeyU => Key::U, K::KeyV => Key::V, K::KeyW => Key::W, K::KeyX => Key::X,
        K::KeyY => Key::Y, K::KeyZ => Key::Z,
        K::Digit0 => Key::Digit0, K::Digit1 => Key::Digit1, K::Digit2 => Key::Digit2,
        K::Digit3 => Key::Digit3, K::Digit4 => Key::Digit4, K::Digit5 => Key::Digit5,
        K::Digit6 => Key::Digit6, K::Digit7 => Key::Digit7, K::Digit8 => Key::Digit8,
        K::Digit9 => Key::Digit9,
        K::Backquote => Key::Backquote, K::Backslash => Key::Backslash,
        K::BracketLeft => Key::BracketLeft, K::BracketRight => Key::BracketRight,
        K::Comma => Key::Comma, K::Equal => Key::Equal, K::Minus => Key::Minus,
        K::Period => Key::Period, K::Quote => Key::Quote, K::Semicolon => Key::Semicolon,
        K::Slash => Key::Slash, K::IntlBackslash => Key::IntlBackslash,
        K::Backspace => Key::Backspace, K::Enter => Key::Enter, K::Space => Key::Space,
        K::Tab => Key::Tab, K::Escape => Key::Escape, K::Delete => Key::Delete,
        K::End => Key::End, K::Home => Key::Home, K::Insert => Key::Insert,
        K::PageDown => Key::PageDown, K::PageUp => Key::PageUp,
        K::ArrowDown => Key::ArrowDown, K::ArrowLeft => Key::ArrowLeft,
        K::ArrowRight => Key::ArrowRight, K::ArrowUp => Key::ArrowUp,
        K::ShiftLeft => Key::ShiftLeft, K::ShiftRight => Key::ShiftRight,
        K::ControlLeft => Key::ControlLeft, K::ControlRight => Key::ControlRight,
        K::AltLeft => Key::AltLeft, K::AltRight => Key::AltRight,
        K::MetaLeft => Key::MetaLeft, K::MetaRight => Key::MetaRight,
        K::CapsLock => Key::CapsLock,
        K::F1 => Key::F1, K::F2 => Key::F2, K::F3 => Key::F3, K::F4 => Key::F4,
        K::F5 => Key::F5, K::F6 => Key::F6, K::F7 => Key::F7, K::F8 => Key::F8,
        K::F9 => Key::F9, K::F10 => Key::F10, K::F11 => Key::F11, K::F12 => Key::F12,
        K::Numpad0 => Key::Numpad0, K::Numpad1 => Key::Numpad1, K::Numpad2 => Key::Numpad2,
        K::Numpad3 => Key::Numpad3, K::Numpad4 => Key::Numpad4, K::Numpad5 => Key::Numpad5,
        K::Numpad6 => Key::Numpad6, K::Numpad7 => Key::Numpad7, K::Numpad8 => Key::Numpad8,
        K::Numpad9 => Key::Numpad9, K::NumpadAdd => Key::NumpadAdd,
        K::NumpadDecimal => Key::NumpadDecimal, K::NumpadDivide => Key::NumpadDivide,
        K::NumpadEnter => Key::NumpadEnter, K::NumpadEqual => Key::NumpadEqual,
        K::NumpadMultiply => Key::NumpadMultiply, K::NumpadSubtract => Key::NumpadSubtract,
        _ => return None,
    })
}
