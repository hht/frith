//! The application: OS windows, the panes they show, and the routing of every event to
//! the right window and pane.
//!
//! Answers: which window and pane does an event belong to, and how do layout edits, pane
//! drags across windows, and terminal output become redraws?
//! Must not know: split-tree rules (`layout`), chrome geometry (`chrome`), gesture rules
//! (`gesture`), or how anything is drawn (`draw`).
//!
//! Panes live in one map for the whole app; a window only holds the `Layout` of pane ids it
//! shows. Moving a pane between windows therefore moves an id, never a terminal.
//!
//! Cross-window drags rely on AppKit delivering `mouseDragged:` to the window where the
//! press began even when the pointer is outside it, which winit forwards while a button is
//! held. Points are converted through desktop coordinates in logical pixels, because
//! physical pixels differ between displays.
//! @see winit-appkit 0.31.0-beta.4 src/view.rs `mouse_motion` (forwards events outside the view while a button is down)

use std::collections::HashMap;
use std::time::{Duration, Instant};

use libghostty_vt::mouse;
use winit::application::ApplicationHandler;
use winit::cursor::CursorIcon;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ButtonSource, ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{
    ImeCapabilities, ImeEnableRequest, ImeRequest, ImeRequestData, Window, WindowAttributes,
    WindowId,
};

use crate::chrome::{self, Button, Hit, Metrics, Scene};
use crate::draw::{Glyphs, PaneLook, Painter, TermView, ViewOpts};
use crate::gesture::{Effect, Gesture};
use crate::gpu::{Gpu, Surface};
use crate::layout::{Axis, Drop, Edge, Layout, PaneId, Point, Rect};
use crate::stats::{Stats, ms};
use crate::term::{GridSize, Session};
use crate::{Cfg, unescape};

/// Upper bound on PTY bytes parsed per pane per wake-up, so one flooding pane cannot
/// starve input handling or the other panes.
const FEED_BUDGET: usize = 1 << 20;
/// DEC 2026: the longest a pane may hold its frame (libghostty-vt has no clock).
const HOLD_LIMIT: Duration = Duration::from_secs(1);
/// Smallest logical size for windows opened by tearing a pane off.
const MIN_WINDOW: (f64, f64) = (480.0, 320.0);

struct Pane {
    session: Session,
    view: TermView,
    title: String,
    /// An OSC 9/777 notification arrived while the pane was not in front of the user.
    attention: bool,
    hold_since: Option<Instant>,
}

impl Pane {
    /// True while the child holds its output under DEC 2026 synchronized output.
    fn held(&mut self) -> bool {
        if !self.session.shared.render_hold.get() {
            self.hold_since = None;
            return false;
        }
        self.hold_since.get_or_insert_with(Instant::now).elapsed() < HOLD_LIMIT
    }
}

struct Win {
    window: Box<dyn Window>,
    surface: Surface,
    glyphs: Glyphs,
    metrics: Metrics,
    scale: f32,
    size: (u32, u32),
    layout: Layout,
    /// Always a visible pane of `layout`.
    focus: PaneId,
    gesture: Gesture,
    /// `layout.revision()` when the current gesture began; a divider index is only
    /// meaningful while it is unchanged.
    gesture_revision: u64,
    pointer: Point,
    hover: Hit,
    preedit: String,
    ime_area: Option<Rect>,
    title: String,
    /// While a pane drag is over this window: the drop target, if any, and the pointer.
    drop: Option<Drop>,
    ghost: Option<Point>,
    shot: Option<String>,
}

impl Win {
    fn scene(&self) -> Scene {
        chrome::scene(&self.layout, self.size.0 as f32, self.size.1 as f32, &self.metrics)
    }

    /// Desktop position, in logical pixels, of the surface's top-left corner.
    fn origin(&self) -> (f64, f64) {
        let outer = self.window.outer_position().expect("macOS reports window positions");
        let inset = self.window.surface_position();
        let s = self.scale as f64;
        ((outer.x + inset.x) as f64 / s, (outer.y + inset.y) as f64 / s)
    }

    fn local_to_desktop(&self, p: Point) -> (f64, f64) {
        let (x, y) = self.origin();
        let s = self.scale as f64;
        (x + p.x as f64 / s, y + p.y as f64 / s)
    }

    fn desktop_to_local(&self, (dx, dy): (f64, f64)) -> Point {
        let (x, y) = self.origin();
        let s = self.scale as f64;
        Point::new(((dx - x) * s) as f32, ((dy - y) * s) as f32)
    }

    /// True when `d` is over this window's frame, title bar included, so a release over
    /// any part of a window never tears a pane off. Minimized windows cover nothing.
    fn covers_desktop(&self, (dx, dy): (f64, f64)) -> bool {
        if self.window.is_minimized() == Some(true) {
            return false;
        }
        let outer = self.window.outer_position().expect("macOS reports window positions");
        let size = self.window.outer_size();
        let s = self.scale as f64;
        let (x, y) = (outer.x as f64 / s, outer.y as f64 / s);
        dx >= x && dx < x + size.width as f64 / s && dy >= y && dy < y + size.height as f64 / s
    }

    fn grid(&self, content: Rect) -> GridSize {
        let f = &self.glyphs.fonts;
        GridSize::fit(content.w as u32, content.h as u32, f.cell_w as u32, f.cell_h as u32, self.metrics.pad as u32)
    }

    /// Moves focus to `next` if the focused pane is no longer visible.
    fn refocus(&mut self, next: PaneId) {
        if !self.layout.is_visible(self.focus) {
            self.focus = next;
        }
    }
}

struct PaneDrag {
    pane: PaneId,
    source: WindowId,
    /// Pointer offset from the dragged frame's top-left corner, in logical pixels.
    grab: (f64, f64),
}

pub struct App {
    cfg: Cfg,
    proxy: EventLoopProxy,
    gpu: Option<Gpu>,
    painter: Painter,
    panes: HashMap<PaneId, Pane>,
    windows: HashMap<WindowId, Win>,
    /// Windows front to back, by most recent focus; decides which window a drag is over.
    z: Vec<WindowId>,
    next_pane: u32,
    drag: Option<PaneDrag>,
    mods: ModifiersState,
    stats: Stats,
    script_next: usize,
}

impl App {
    pub fn new(cfg: Cfg, proxy: EventLoopProxy) -> App {
        App {
            proxy,
            gpu: None,
            painter: Painter::new(cfg.blur, cfg.sigma),
            panes: HashMap::new(),
            windows: HashMap::new(),
            z: Vec::new(),
            next_pane: 0,
            drag: None,
            mods: ModifiersState::empty(),
            stats: Stats::new(),
            script_next: 0,
            cfg,
        }
    }

    // ---- windows and panes -------------------------------------------------------------

    fn new_pane_id(&mut self) -> PaneId {
        self.next_pane += 1;
        PaneId(self.next_pane)
    }

    fn window_of(&self, pane: PaneId) -> Option<WindowId> {
        self.windows.iter().find(|(_, w)| w.layout.contains(pane)).map(|(id, _)| *id)
    }

    /// Topmost window under a desktop point. The order is by recent focus: winit exposes
    /// no z-order, and windows on other Spaces are not excluded.
    fn window_at(&self, d: (f64, f64)) -> Option<WindowId> {
        self.z.iter().copied().find(|id| self.windows[id].covers_desktop(d))
    }

    /// Opens a window showing `layout`. `at` is the desktop position for the surface's
    /// top-left corner; `None` lets the system place the window.
    fn open_window(
        &mut self,
        el: &dyn ActiveEventLoop,
        layout: Layout,
        focus: PaneId,
        size: (f64, f64),
        at: Option<(f64, f64)>,
    ) -> WindowId {
        let mut attrs = WindowAttributes::default()
            .with_title("spike · skia + libghostty-vt")
            .with_surface_size(LogicalSize::new(size.0, size.1));
        if let Some((x, y)) = at {
            attrs = attrs.with_position(LogicalPosition::new(x, y));
        }
        let window = el.create_window(attrs).expect("create window");
        if let Some((x, y)) = at {
            // `with_position` places the outer frame; shift by the title bar so the surface
            // lands at `at`.
            let inset = window.surface_position();
            let s = window.scale_factor();
            window.set_outer_position(LogicalPosition::new(x - inset.x as f64 / s, y - inset.y as f64 / s).into());
        }
        enable_ime(window.as_ref());
        let gpu = self.gpu.get_or_insert_with(Gpu::new);
        let surface = Surface::attach(gpu, window.as_ref(), !self.cfg.bench);
        let scale = window.scale_factor() as f32;
        let px = window.surface_size();
        let id = window.id();
        window.request_redraw();
        self.windows.insert(
            id,
            Win {
                window,
                surface,
                glyphs: Glyphs::new(&self.cfg.font, self.cfg.font_size * scale),
                metrics: Metrics::for_scale(scale),
                scale,
                size: (px.width, px.height),
                layout,
                focus,
                gesture: Gesture::Idle,
                gesture_revision: 0,
                pointer: Point::default(),
                hover: Hit::Nothing,
                preedit: String::new(),
                ime_area: None,
                title: String::new(),
                drop: None,
                ghost: None,
                shot: None,
            },
        );
        self.z.insert(0, id);
        id
    }

    /// Opens a window with one new pane running `cmd` (or the login shell).
    fn new_window(&mut self, el: &dyn ActiveEventLoop, cmd: Option<&str>) {
        let pane = self.new_pane_id();
        let id = self.open_window(el, Layout::new(pane), pane, self.cfg.win, None);
        self.spawn(id, pane, cmd);
    }

    /// Starts the program of `pane`, which window `win` already shows, at its exact size.
    fn spawn(&mut self, win: WindowId, pane: PaneId, cmd: Option<&str>) {
        let w = &self.windows[&win];
        let content = w.scene().frame(pane).expect("a new pane is visible").content;
        let proxy = self.proxy.clone();
        let session = Session::spawn(cmd, w.grid(content), move || proxy.wake_up());
        let title = cmd.unwrap_or("shell").to_string();
        self.panes.insert(pane, Pane { session, view: TermView::new(), title, attention: false, hold_since: None });
    }

    fn split(&mut self, win: WindowId, edge: Edge, cmd: Option<&str>) {
        let pane = self.new_pane_id();
        let w = self.windows.get_mut(&win).expect("window");
        w.layout.split(w.focus, edge, pane);
        self.spawn(win, pane, cmd);
        self.focus(win, pane);
    }

    fn focus(&mut self, win: WindowId, pane: PaneId) {
        let w = self.windows.get_mut(&win).expect("window");
        assert!(w.layout.is_visible(pane), "focus must stay on a visible pane");
        w.focus = pane;
        if let Some(p) = self.panes.get_mut(&pane) {
            p.attention = false;
        }
        w.window.request_redraw();
    }

    /// Moves focus `step` panes along reading order. A zoom ends first, since the other
    /// panes are hidden behind it.
    fn cycle_focus(&mut self, win: WindowId, step: isize) {
        let w = self.windows.get_mut(&win).expect("window");
        if w.layout.zoomed().is_some() {
            w.layout.toggle_zoom(w.focus);
        }
        let tiled = w.layout.tiled();
        let at = tiled.iter().position(|p| *p == w.focus).expect("focus is tiled") as isize;
        let next = tiled[(at + step).rem_euclid(tiled.len() as isize) as usize];
        self.focus(win, next);
    }

    fn toggle_zoom(&mut self, win: WindowId) {
        let w = self.windows.get_mut(&win).expect("window");
        w.layout.toggle_zoom(w.focus);
        w.window.request_redraw();
    }

    fn minimize(&mut self, win: WindowId, pane: PaneId) {
        let w = self.windows.get_mut(&win).expect("window");
        if let Some(next) = w.layout.minimize(pane) {
            w.refocus(next);
            w.window.request_redraw();
        }
    }

    fn restore(&mut self, win: WindowId, pane: PaneId) {
        self.windows.get_mut(&win).expect("window").layout.restore(pane);
        self.focus(win, pane);
    }

    /// Ends `pane` and its program; closes its window if that was the last pane.
    fn close_pane(&mut self, el: &dyn ActiveEventLoop, pane: PaneId, reason: &str) {
        if self.drag.as_ref().is_some_and(|d| d.pane == pane) {
            self.cancel_drag();
        }
        self.panes.remove(&pane);
        let Some(id) = self.window_of(pane) else { return };
        let w = self.windows.get_mut(&id).expect("window");
        match w.layout.remove(pane) {
            Some(next) => {
                w.refocus(next);
                w.window.request_redraw();
            }
            None => self.close_window(el, id, reason),
        }
    }

    /// Closes a window and ends every pane still in it; exits after the last window.
    fn close_window(&mut self, el: &dyn ActiveEventLoop, id: WindowId, reason: &str) {
        let Some(w) = self.windows.remove(&id) else { return };
        if self.drag.as_ref().is_some_and(|d| d.source == id) {
            self.cancel_drag();
        }
        for pane in w.layout.tiled().into_iter().chain(w.layout.docked()) {
            self.panes.remove(&pane);
        }
        self.z.retain(|z| *z != id);
        if self.windows.is_empty() {
            self.finish(el, reason);
        }
    }

    fn finish(&mut self, el: &dyn ActiveEventLoop, reason: &str) {
        if el.exiting() {
            return;
        }
        let c = &self.cfg;
        let extra = format!(
            "\"reason\":\"{reason}\",\"mode\":\"{:?}\",\"blur\":{},\"sigma\":{},\"bench\":{},\"sync\":{}",
            c.mode, c.blur, c.sigma, c.bench, c.sync
        );
        let line = self.stats.summary(&extra);
        println!("[summary] {line}");
        if let Some(p) = &c.stats_out
            && let Err(e) = std::fs::write(p, line + "\n")
        {
            eprintln!("[stats] {p}: {e}");
        }
        self.drag = None;
        self.z.clear();
        self.windows.clear();
        self.panes.clear();
        el.exit();
    }

    // ---- terminal output ---------------------------------------------------------------

    /// Parses pending output of every pane and redraws the windows where it shows.
    fn feed(&mut self, el: &dyn ActiveEventLoop) {
        let t0 = Instant::now();
        let (mut bytes, mut more) = (0, false);
        let mut exited = Vec::new();
        // (pane, chrome changed): output only matters where the pane is visible, while a
        // title or attention change also shows on a dock chip.
        let mut changed = Vec::new();
        for (&id, pane) in &mut self.panes {
            let fed = pane.session.feed(FEED_BUDGET);
            bytes += fed.bytes;
            more |= fed.more;
            if fed.exited {
                exited.push(id);
            }
            let mut chrome = false;
            if pane.session.shared.title_changed.replace(false) {
                pane.title = pane.session.title();
                chrome = true;
            }
            if pane.session.shared.notified.replace(false) {
                pane.attention = true;
                chrome = true;
            }
            if fed.bytes > 0 || chrome {
                changed.push((id, chrome));
            }
        }
        if bytes > 0 {
            self.stats.first_byte.get_or_insert(t0);
            self.stats.window.bytes += bytes as u64;
            self.stats.window.parse.push(ms(t0, Instant::now()));
        }
        for w in self.windows.values_mut() {
            let mut redraw = false;
            for &(id, chrome) in &changed {
                redraw |= w.layout.is_visible(id) || (chrome && w.layout.contains(id));
                if id == w.focus && w.window.has_focus() {
                    self.panes.get_mut(&id).expect("pane").attention = false;
                }
            }
            if redraw {
                w.window.request_redraw();
            }
        }
        for id in exited {
            self.close_pane(el, id, "child-exited");
        }
        if more {
            self.proxy.wake_up();
        }
    }

    // ---- drawing -----------------------------------------------------------------------

    fn redraw(&mut self, id: WindowId) {
        let App { cfg, gpu, painter, panes, windows, drag, stats, .. } = self;
        let n_windows = windows.len();
        let Some(w) = windows.get_mut(&id) else { return };
        let Some(gpu) = gpu.as_mut() else { return };
        let t0 = Instant::now();
        let scene = w.scene();
        for f in &scene.frames {
            let pane = panes.get_mut(&f.pane).expect("every tiled pane exists");
            if pane.session.resize(w.grid(f.content)) {
                pane.view.invalidate();
            }
        }
        let lit_divider = match (w.gesture, w.hover) {
            (Gesture::DraggingDivider(i), _) | (_, Hit::Divider(i)) => {
                scene.dividers.iter().find(|d| d.index == i).map(|d| d.rect)
            }
            _ => None,
        };
        let ghost_title = drag.as_ref().map(|d| panes[&d.pane].title.clone());
        let elapsed = stats.start.elapsed().as_secs_f32();
        let shot = w.shot.take();
        let mut held_any = false;
        let Win { surface, glyphs, layout, metrics: m, scale, size, focus, hover, preedit, drop, ghost, .. } = &mut *w;
        let (m, scale, size, focus, hover) = (*m, *scale, *size, *focus, *hover);
        let frame = surface.present(gpu, cfg.sync, shot.as_deref(), |canvas, ctx| {
            painter.background(canvas, size.0 as f32, size.1 as f32);
            let (mut cursor, mut rebuilt, mut text_draws) = (None, 0, 0);
            for f in &scene.frames {
                let pane = panes.get_mut(&f.pane).expect("every tiled pane exists");
                let held = pane.held();
                held_any |= held;
                let focused = f.pane == focus;
                let look = PaneLook {
                    title: &pane.title,
                    focused,
                    attention: pane.attention,
                    zoomed: layout.zoomed() == Some(f.pane),
                };
                painter.clip_frame(canvas, f, &m);
                let opts = ViewOpts { mode: cfg.mode, content: f.content, pad: m.pad, focused, update: !held };
                let info = pane.view.draw(canvas, glyphs, ctx, &pane.session.term, &opts);
                painter.header(canvas, glyphs, f, &look, hover, &m);
                canvas.restore();
                painter.border(canvas, f, &look, &m, scale);
                rebuilt += info.rebuilt_rows;
                text_draws += info.text_draws;
                if focused {
                    cursor = info.cursor;
                }
            }
            if let Some(r) = lit_divider {
                painter.divider(canvas, r);
            }
            painter.dock(canvas, glyphs, &scene, |p| (panes[&p].title.clone(), panes[&p].attention), hover, &m);
            if let Some(d) = *drop {
                painter.drop_preview(canvas, layout.drop_preview(scene.area, m.gap, d), &m);
            }
            if let (Some(at), Some(title)) = (*ghost, &ghost_title) {
                painter.ghost(canvas, glyphs, (at.x, at.y), title, &m);
            }
            painter.demo_glass(canvas, glyphs, elapsed, (size.0 as f32, size.1 as f32), scale);
            if let Some(c) = cursor {
                painter.preedit(canvas, glyphs, c, preedit);
            }
            (cursor, rebuilt, text_draws)
        });
        let Some(((cursor, rebuilt, text_draws), t)) = frame else { return };

        stats.window.total.push(ms(t0, t.presented));
        stats.window.draw.push(ms(t.acquired, t.drawn));
        stats.window.flush.push(ms(t.drawn, t.presented));
        stats.window.rebuilt.push(rebuilt);
        let (n_panes, uploads) = (panes.len(), w.glyphs.atlas_uploads());
        stats.tick(|| format!("windows={n_windows} panes={n_panes} atlas uploads={uploads} text draws/frame={text_draws}"));

        if let Some(c) = cursor.filter(|c| w.ime_area != Some(*c)) {
            w.ime_area = Some(c);
            let _ = w.window.request_ime_update(ImeRequest::Update(ImeRequestData::default().with_cursor_area(
                PhysicalPosition::new(c.x as i32, c.y as i32).into(),
                PhysicalSize::new(c.w as u32, c.h as u32).into(),
            )));
        }
        let title = &panes[&w.focus].title;
        if *title != w.title {
            w.window.set_title(title);
            w.title = title.clone();
        }
        if cfg.bench || cfg.blur > 0 || held_any {
            w.window.request_redraw();
        }
    }

    // ---- pointer -----------------------------------------------------------------------

    fn pointer_moved(&mut self, id: WindowId, p: Point) {
        let Some(w) = self.windows.get_mut(&id) else { return };
        w.pointer = p;
        let scene = w.scene();
        match w.gesture.moved(p, w.metrics.drag_threshold) {
            Effect::DragStart(pane) => {
                let frame = scene.frame(pane).expect("a dragged pane is visible").outer;
                let s = w.scale as f64;
                let grab = ((p.x - frame.x) as f64 / s, (p.y - frame.y) as f64 / s);
                w.window.set_cursor(CursorIcon::Grabbing.into());
                self.drag = Some(PaneDrag { pane, source: id, grab });
                self.drag_to(id, p);
            }
            Effect::DragMove => self.drag_to(id, p),
            Effect::ResizeDivider(i) => {
                let Some(d) = scene.dividers.iter().find(|d| d.index == i) else { return };
                let along = if d.axis == Axis::Horizontal { p.x } else { p.y };
                let m = w.metrics;
                w.layout.drag_divider(scene.area, m.gap, i, along, m.min_pane);
                w.window.request_redraw();
            }
            Effect::TerminalMotion(pane) => {
                self.terminal_mouse(id, pane, mouse::Action::Motion, Some(mouse::Button::Left), true);
            }
            _ => {
                let hit = scene.hit(p, &w.metrics);
                w.window.set_cursor(cursor_icon(hit, &scene).into());
                if hit != w.hover {
                    let lit = |h: Hit| matches!(h, Hit::Button(..) | Hit::Chip(_) | Hit::Divider(_));
                    if lit(hit) || lit(w.hover) {
                        w.window.request_redraw();
                    }
                    w.hover = hit;
                }
                if let Hit::Content(pane) = hit {
                    self.terminal_mouse(id, pane, mouse::Action::Motion, None, false);
                }
            }
        }
    }

    fn pointer_button(&mut self, el: &dyn ActiveEventLoop, id: WindowId, pressed: bool, button: MouseButton, p: Point) {
        let Some(w) = self.windows.get_mut(&id) else { return };
        w.pointer = p;
        let hit = w.scene().hit(p, &w.metrics);
        if button != MouseButton::Left {
            let b = match button {
                MouseButton::Right => mouse::Button::Right,
                MouseButton::Middle => mouse::Button::Middle,
                _ => return,
            };
            if let Hit::Content(pane) = hit {
                let action = if pressed { mouse::Action::Press } else { mouse::Action::Release };
                self.terminal_mouse(id, pane, action, Some(b), pressed);
            }
            return;
        }
        let effect = if pressed {
            w.gesture_revision = w.layout.revision();
            w.gesture.press(hit, p)
        } else {
            w.gesture.release(hit)
        };
        match effect {
            Effect::Focus(pane) => self.focus(id, pane),
            Effect::TerminalPress(pane) => {
                self.focus(id, pane);
                self.terminal_mouse(id, pane, mouse::Action::Press, Some(mouse::Button::Left), true);
            }
            Effect::TerminalRelease(pane) => {
                self.terminal_mouse(id, pane, mouse::Action::Release, Some(mouse::Button::Left), false);
            }
            Effect::Click(pane, Button::Minimize) => self.minimize(id, pane),
            Effect::Click(pane, Button::Zoom) => {
                self.focus(id, pane);
                self.toggle_zoom(id);
            }
            Effect::Click(pane, Button::Close) => self.close_pane(el, pane, "closed"),
            Effect::Restore(pane) => self.restore(id, pane),
            Effect::Drop => self.drop_pane(el, id),
            Effect::None => {}
            other => unreachable!("{other:?} only comes from moves or cancel"),
        }
    }

    /// Reports a mouse event to `pane`'s program at the window's pointer position.
    fn terminal_mouse(
        &mut self,
        id: WindowId,
        pane: PaneId,
        action: mouse::Action,
        button: Option<mouse::Button>,
        any_pressed: bool,
    ) {
        let w = &self.windows[&id];
        let Some(f) = w.scene().frame(pane).copied() else { return };
        let local = (w.pointer.x - f.content.x, w.pointer.y - f.content.y);
        if let Some(p) = self.panes.get_mut(&pane) {
            p.session.mouse(action, button, local, self.mods, any_pressed);
        }
    }

    fn wheel(&mut self, id: WindowId, delta: MouseScrollDelta) {
        let Some(w) = self.windows.get(&id) else { return };
        let scene = w.scene();
        let (Hit::Content(pane) | Hit::Header(pane)) = scene.hit(w.pointer, &w.metrics) else { return };
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => y,
            MouseScrollDelta::PixelDelta(p) => p.y as f32 / w.glyphs.fonts.cell_h,
            _ => return,
        };
        let f = scene.frame(pane).expect("hit pane is visible");
        let local = (w.pointer.x - f.content.x, w.pointer.y - f.content.y);
        w.window.request_redraw();
        self.panes.get_mut(&pane).expect("pane").session.wheel(lines, local, self.mods);
    }

    // ---- pane drag across windows ------------------------------------------------------

    /// Shows the drop target and ghost in whichever window is under the pointer.
    fn drag_to(&mut self, source: WindowId, p: Point) {
        let Some(drag) = &self.drag else { return };
        let desktop = self.windows[&source].local_to_desktop(p);
        let target = self.window_at(desktop);
        for (id, w) in &mut self.windows {
            let (drop, ghost) = if Some(*id) == target {
                let local = w.desktop_to_local(desktop);
                let area = w.scene().area;
                let m = w.metrics;
                (w.layout.drop_target(area, m.gap, m.edge_band, local, drag.pane), Some(local))
            } else {
                (None, None)
            };
            if (drop, ghost) != (w.drop, w.ghost) {
                w.drop = drop;
                w.ghost = ghost;
                w.window.request_redraw();
            }
        }
    }

    /// Reconciles gestures and drag previews with layouts after an event may have changed
    /// them (a pane exited, a shortcut zoomed or closed one, a pane moved windows). Runs
    /// after every event, so redraws and later events never see a gesture or preview that
    /// refers to a pane that is gone or hidden, or to a divider that no longer exists.
    fn settle(&mut self) {
        for w in self.windows.values_mut() {
            let stale = match w.gesture {
                Gesture::Idle => false,
                Gesture::Pressed { on: Hit::Chip(p), .. } => !w.layout.is_docked(p),
                Gesture::Pressed { on: Hit::Header(p) | Hit::Button(p, _) | Hit::Content(p), .. }
                | Gesture::DraggingPane(p)
                | Gesture::Terminal(p) => !w.layout.is_visible(p),
                Gesture::Pressed { .. } | Gesture::DraggingDivider(_) => {
                    w.layout.revision() != w.gesture_revision
                }
            };
            if stale {
                w.gesture = Gesture::Idle;
                w.window.request_redraw();
            }
        }
        let Some(d) = &self.drag else { return };
        let (pane, source) = (d.pane, d.source);
        match self.windows.get(&source) {
            Some(w) if w.gesture == Gesture::DraggingPane(pane) => self.drag_to(source, w.pointer),
            _ => self.cancel_drag(),
        }
    }

    fn clear_drop_previews(&mut self) {
        for w in self.windows.values_mut() {
            if w.drop.is_some() || w.ghost.is_some() {
                w.drop = None;
                w.ghost = None;
                w.window.request_redraw();
            }
        }
    }

    fn cancel_drag(&mut self) {
        if let Some(d) = self.drag.take()
            && let Some(w) = self.windows.get_mut(&d.source)
        {
            w.gesture = Gesture::Idle;
        }
        self.clear_drop_previews();
    }

    /// Completes a pane drag released at the source window's pointer: dock it where the
    /// preview showed, do nothing over a window without a target, or tear it off when the
    /// pointer is outside every window.
    fn drop_pane(&mut self, el: &dyn ActiveEventLoop, source: WindowId) {
        let p = self.windows[&source].pointer;
        self.drag_to(source, p);
        let Some(drag) = self.drag.take() else { return };
        let desktop = self.windows[&source].local_to_desktop(p);
        let target = self.window_at(desktop);
        let drop = target.and_then(|t| self.windows[&t].drop);
        self.clear_drop_previews();
        match (target, drop) {
            (Some(to), Some(drop)) => self.move_pane(el, drag.pane, source, to, drop),
            (Some(_), None) => {}
            (None, _) => self.tear_off(el, drag, desktop),
        }
    }

    fn move_pane(&mut self, el: &dyn ActiveEventLoop, pane: PaneId, from: WindowId, to: WindowId, drop: Drop) {
        if from != to {
            let w = self.windows.get_mut(&from).expect("source window");
            let emptied = match w.layout.remove(pane) {
                Some(next) => {
                    w.refocus(next);
                    w.window.request_redraw();
                    false
                }
                None => true,
            };
            // The pane's row caches index the source window's glyph atlas.
            self.panes.get_mut(&pane).expect("pane").view.invalidate();
            if emptied {
                self.close_window(el, from, "merged");
            }
            self.windows[&to].window.focus_window();
        }
        self.windows.get_mut(&to).expect("target window").layout.apply(pane, drop);
        self.focus(to, pane);
    }

    /// Opens a new window for the dragged pane with its header under the pointer. A pane
    /// that is alone in its window moves the window instead.
    fn tear_off(&mut self, el: &dyn ActiveEventLoop, drag: PaneDrag, desktop: (f64, f64)) {
        let w = self.windows.get_mut(&drag.source).expect("source window");
        let s = w.scale as f64;
        let margin = w.metrics.margin as f64 / s;
        let at = (desktop.0 - drag.grab.0 - margin, desktop.1 - drag.grab.1 - margin);
        let alone = w.layout.tiled() == [drag.pane] && w.layout.docked().next().is_none();
        if alone {
            let inset = w.window.surface_position();
            w.window.set_outer_position(LogicalPosition::new(at.0 - inset.x as f64 / s, at.1 - inset.y as f64 / s).into());
            return;
        }
        let outer = w.scene().frame(drag.pane).expect("a dragged pane is visible").outer;
        let size = (
            (outer.w as f64 / s + 2.0 * margin).max(MIN_WINDOW.0),
            (outer.h as f64 / s + 2.0 * margin).max(MIN_WINDOW.1),
        );
        let next = w.layout.remove(drag.pane).expect("other panes remain in the source window");
        w.refocus(next);
        w.window.request_redraw();
        self.panes.get_mut(&drag.pane).expect("pane").view.invalidate();
        self.open_window(el, Layout::new(drag.pane), drag.pane, size, Some(at));
    }

    // ---- keyboard ----------------------------------------------------------------------

    fn key(&mut self, el: &dyn ActiveEventLoop, id: WindowId, event: KeyEvent) {
        let pressed = event.state == ElementState::Pressed;
        if pressed
            && event.logical_key == Key::Named(NamedKey::Escape)
            && self.drag.is_some()
        {
            self.cancel_drag();
            return;
        }
        if self.mods.meta_key() {
            if pressed {
                self.shortcut(el, id, &event);
            }
            return;
        }
        let Some(w) = self.windows.get(&id) else { return };
        if !w.preedit.is_empty() {
            return; // IME composition owns the keyboard
        }
        if let Some(p) = self.panes.get_mut(&w.focus) {
            p.session.key(&event, self.mods);
        }
    }

    /// Cmd shortcuts; split, zoom and pane-cycling keys follow Ghostty's macOS defaults.
    /// @see https://github.com/ghostty-org/ghostty/blob/108cacb17ed8990840cd14abfce43d8ed8503ca1/src/config/Config.zig (`new_split`, `goto_split`, `toggle_split_zoom`)
    fn shortcut(&mut self, el: &dyn ActiveEventLoop, id: WindowId, event: &KeyEvent) {
        let shift = self.mods.shift_key();
        let Some(focus) = self.windows.get(&id).map(|w| w.focus) else { return };
        match event.key_without_modifiers.as_ref() {
            Key::Character("v") => {
                if let Ok(out) = std::process::Command::new("pbpaste").output() {
                    self.panes.get_mut(&focus).expect("pane").session.paste(out.stdout);
                }
            }
            Key::Character("q") => self.finish(el, "cmd-q"),
            Key::Character("d") => self.split(id, if shift { Edge::Bottom } else { Edge::Right }, None),
            Key::Character("w") => self.close_pane(el, focus, "closed"),
            Key::Character("n") => self.new_window(el, None),
            Key::Character("[") => self.cycle_focus(id, -1),
            Key::Character("]") => self.cycle_focus(id, 1),
            Key::Named(NamedKey::Enter) if shift => self.toggle_zoom(id),
            _ => {}
        }
    }

    // ---- scripted runs -----------------------------------------------------------------

    fn run_script(&mut self, el: &dyn ActiveEventLoop) {
        if el.exiting() {
            return;
        }
        let elapsed = self.stats.start.elapsed().as_secs_f32();
        while let Some(step) = self.cfg.script.get(self.script_next).filter(|s| s.at <= elapsed) {
            let (action, arg) = (step.action.clone(), step.arg.clone());
            self.script_next += 1;
            self.script_step(el, &action, &arg);
            if el.exiting() {
                return;
            }
        }
        el.set_control_flow(match self.cfg.script.get(self.script_next) {
            Some(s) => ControlFlow::WaitUntil(self.stats.start + Duration::from_secs_f32(s.at)),
            None => ControlFlow::Wait,
        });
    }

    fn script_step(&mut self, el: &dyn ActiveEventLoop, action: &str, arg: &str) {
        let Some(id) = self.z.first().copied() else { return };
        let focus = self.windows[&id].focus;
        match action {
            "send" => self.panes[&focus].session.write(&unescape(arg)),
            "shot" => {
                let stem = arg.strip_suffix(".png").unwrap_or(arg);
                for (i, wid) in self.z.iter().enumerate() {
                    let w = self.windows.get_mut(wid).expect("window");
                    w.shot = Some(if i == 0 { arg.to_string() } else { format!("{stem}-{}.png", i + 1) });
                    w.window.request_redraw();
                }
            }
            "split" => {
                let (dir, cmd) = arg.split_once(' ').map_or((arg, None), |(d, c)| (d, Some(c)));
                let edge = match dir {
                    "right" => Edge::Right,
                    "down" => Edge::Bottom,
                    "left" => Edge::Left,
                    "up" => Edge::Top,
                    _ => return eprintln!("[script] split: unknown direction {dir:?}"),
                };
                self.split(id, edge, cmd);
            }
            "focus" => self.cycle_focus(id, if arg == "prev" { -1 } else { 1 }),
            "zoom" => self.toggle_zoom(id),
            "min" => self.minimize(id, focus),
            "restore" => {
                let first = self.windows[&id].layout.docked().next();
                if let Some(pane) = first {
                    self.restore(id, pane);
                }
            }
            "close" => self.close_pane(el, focus, "script"),
            "press" | "move" | "release" => {
                let v: Vec<f32> = arg.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                let [x, y] = v[..] else {
                    return eprintln!("[script] {action} needs x y, got {arg:?}");
                };
                // Through the same handlers real events use; logical px of the front window.
                let s = self.windows[&id].scale;
                let p = Point::new(x * s, y * s);
                match action {
                    "move" => self.pointer_moved(id, p),
                    _ => self.pointer_button(el, id, action == "press", MouseButton::Left, p),
                }
            }
            "quit" => self.finish(el, "script"),
            _ => eprintln!("[script] unknown action {action}"),
        }
    }

}

fn enable_ime(window: &dyn Window) {
    let request = ImeEnableRequest::new(
        ImeCapabilities::new().with_cursor_area(),
        ImeRequestData::default()
            .with_cursor_area(PhysicalPosition::new(0, 0).into(), PhysicalSize::new(10, 10).into()),
    )
    .expect("IME request carries the cursor area it declares");
    if let Err(e) = window.request_ime_update(ImeRequest::Enable(request)) {
        eprintln!("[ime] enable failed: {e:?}");
    }
}

fn cursor_icon(hit: Hit, scene: &Scene) -> CursorIcon {
    match hit {
        Hit::Divider(i) => match scene.dividers.iter().find(|d| d.index == i).map(|d| d.axis) {
            Some(Axis::Horizontal) => CursorIcon::ColResize,
            _ => CursorIcon::RowResize,
        },
        Hit::Header(_) => CursorIcon::Grab,
        Hit::Button(..) | Hit::Chip(_) => CursorIcon::Pointer,
        Hit::Content(_) => CursorIcon::Text,
        Hit::Nothing => CursorIcon::Default,
    }
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, el: &dyn ActiveEventLoop) {
        if self.next_pane == 0 {
            let cmd = self.cfg.cmd.clone();
            self.new_window(el, cmd.as_deref());
        }
    }

    fn proxy_wake_up(&mut self, el: &dyn ActiveEventLoop) {
        self.feed(el);
        self.settle();
    }

    fn about_to_wait(&mut self, el: &dyn ActiveEventLoop) {
        self.run_script(el);
        self.settle();
    }

    fn window_event(&mut self, el: &dyn ActiveEventLoop, id: WindowId, event: WindowEvent) {
        self.window_event_inner(el, id, event);
        self.settle();
    }
}

impl App {
    fn window_event_inner(&mut self, el: &dyn ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if !self.windows.contains_key(&id) {
            return;
        }
        let pointer = matches!(
            event,
            WindowEvent::PointerMoved { .. }
                | WindowEvent::PointerButton { .. }
                | WindowEvent::PointerLeft { .. }
                | WindowEvent::MouseWheel { .. }
        );
        if pointer && !self.cfg.script.is_empty() {
            return; // scripted runs drive the pointer themselves; the real mouse must not interfere
        }
        match event {
            WindowEvent::RedrawRequested => self.redraw(id),
            WindowEvent::CloseRequested => self.close_window(el, id, "closed"),
            WindowEvent::SurfaceResized(size) => {
                let w = self.windows.get_mut(&id).expect("window");
                w.size = (size.width.max(1), size.height.max(1));
                w.surface.resize(w.size.0, w.size.1);
                w.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let w = self.windows.get_mut(&id).expect("window");
                w.scale = scale_factor as f32;
                w.metrics = Metrics::for_scale(w.scale);
                w.glyphs = Glyphs::new(&self.cfg.font, self.cfg.font_size * w.scale);
                for pane in w.layout.tiled().into_iter().chain(w.layout.docked()) {
                    self.panes.get_mut(&pane).expect("pane").view.invalidate();
                }
                w.window.request_redraw();
            }
            WindowEvent::Focused(true) => {
                self.z.retain(|z| *z != id);
                self.z.insert(0, id);
                let focus = self.windows[&id].focus;
                self.focus(id, focus);
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::Ime(ime) => {
                let w = self.windows.get_mut(&id).expect("window");
                match ime {
                    Ime::Preedit(text, _) => w.preedit = text,
                    Ime::Commit(text) => {
                        w.preedit.clear();
                        self.panes[&w.focus].session.write(text.as_bytes());
                    }
                    Ime::Enabled | Ime::Disabled => {}
                    other => eprintln!("[ime] {other:?}"),
                }
                w.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => self.key(el, id, event),
            WindowEvent::PointerMoved { position, .. } => {
                self.pointer_moved(id, Point::new(position.x as f32, position.y as f32));
            }
            WindowEvent::PointerButton { state, button: ButtonSource::Mouse(button), position, .. } => {
                let p = Point::new(position.x as f32, position.y as f32);
                self.pointer_button(el, id, state == ElementState::Pressed, button, p);
            }
            WindowEvent::PointerLeft { .. } => {
                let w = self.windows.get_mut(&id).expect("window");
                if w.hover != Hit::Nothing && w.gesture == Gesture::Idle {
                    w.hover = Hit::Nothing;
                    w.window.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => self.wheel(id, delta),
            _ => {}
        }
    }
}
