//! The application: the root window, the panes it shows, and the routing of every event to
//! the layout, a gesture, or a terminal session.
//!
//! Answers: which pane does an event belong to, and how do layout edits and terminal output
//! become redraws?
//! Must not know: arrangement rules (`layout`), chrome geometry (`scene`), gesture rules
//! (`gesture`), or how anything is drawn (`render`, `grid`).
//!
//! The app has one window (ADR 6). Every pane lives in `panes`; the `Layout` holds their ids.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use frith_core::colors::{Chrome, Tint};
use frith_core::config::{Config, Rgb};
use frith_core::geom::{Point, Rect};
use frith_core::gesture::{Effect, Gesture};
use frith_core::layout::{Axis, Drop, Edge, Layout, PaneId};
use frith_core::motion::Motion;
use frith_core::scene::{self, ChromeButton, Frame, Guides, Hit, Metrics, PaneButton, Resize, Scene};
use frith_term::pty::Program;
use frith_term::{GridSize, Pad, Session, shell};
use libghostty_vt::mouse;
use skia_safe::Rect as SkRect;
use winit::application::ApplicationHandler;
use winit::cursor::CursorIcon;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ButtonSource, ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::platform::macos::WindowAttributesMacOS;
use winit::window::{
    ImeCapabilities, ImeEnableRequest, ImeRequest, ImeRequestData, Window, WindowAttributes, WindowId,
};

use crate::atlas::Atlas;
use crate::fonts::{self, Fonts};
use crate::gpu::Gpu;
use crate::grid::Grid;
use crate::render;
use crate::screens::Screens;
use crate::stage::{Kind, Origin, Pane, Placed, Stage};
use crate::{Cfg, unescape};

/// Upper bound on PTY bytes parsed per pane per wake-up, so one flooding pane cannot starve
/// input handling or the other panes.
const FEED_BUDGET: usize = 1 << 20;

// Sizes in logical pixels.
const WINDOW: (f64, f64) = (1200.0, 800.0);
/// Chrome text size in points: macOS's Callout style, a step below its 13 pt Body.
/// @see https://developer.apple.com/design/human-interface-guidelines/typography#macOS-built-in-text-styles
const UI_TEXT: f32 = 12.0;
/// Pointer travel that turns a header press into a pane drag.
const DRAG_THRESHOLD: f32 = 4.0;
const MIN_TILE: f32 = 80.0;
const MIN_CARD: (f32, f32) = (240.0, 120.0);

/// The root window and everything that depends on its scale factor.
struct Win {
    window: Box<dyn Window>,
    gpu: Gpu,
    fonts: Fonts,
    /// The system UI font at `UI_TEXT` pt, for chrome text.
    ui: skia_safe::Font,
    atlas: Atlas,
    metrics: Metrics,
    scale: f32,
    size: (u32, u32),
    screens: Screens,
    /// AppKit reports the window cannot be seen, as when minimized or covered.
    occluded: bool,
    /// A frame was skipped while hidden, so the window shows a stale one. Layout changes made
    /// meanwhile stay unplaced, so they morph once the window shows.
    missed: bool,
}

impl Win {
    fn px(&self, logical: f32) -> f32 {
        (logical * self.scale).round()
    }

    /// Nothing drawn would be seen. Frames then cost nothing; Ghostty skips them too.
    /// @see Ghostty `src/renderer/Thread.zig` (`.visible`)
    fn hidden(&self) -> bool {
        self.occluded || self.screens.asleep()
    }
}

/// A pane drag past the threshold.
struct Drag {
    pane: PaneId,
    /// Pointer offset from the dragged frame's top-left corner.
    grab: (f32, f32),
    /// A dragged card's rect when the drag began, to put it back on cancel.
    card: Option<Rect>,
    guides: Guides,
}

/// A card resize: the card's rect and the pointer when it began.
struct Resizing {
    pane: PaneId,
    from: Rect,
    start: Point,
}

pub struct App {
    cfg: Cfg,
    config: Config,
    /// Where the shell integration is installed, checked before each shell starts.
    cache: PathBuf,
    /// Chrome colors, from the terminal colors the config sets (ADR 14).
    chrome: Chrome,
    /// Laid over unfocused panes' content while more than one shows; `None` when the
    /// config turns the fade off.
    fade: Option<Tint>,
    proxy: EventLoopProxy,
    win: Option<Win>,
    sidebar: bool,
    layout: Layout,
    panes: HashMap<PaneId, Pane>,
    /// A visible pane, or `None` when every pane is minimized.
    focus: Option<PaneId>,
    next_pane: u32,
    gesture: Gesture,
    /// Pane springs between arrangements (ADR 10).
    motion: Motion,
    /// Each pane's rect and kind when last drawn, and `layout.revision()` then: a new
    /// revision morphs panes from these.
    placed: Vec<Placed>,
    placed_revision: u64,
    /// Where each morphing pane's morph began.
    origins: HashMap<PaneId, Origin>,
    /// `layout.revision()` when the current gesture began; a structural change since then
    /// makes the panes and divider indices it captured stale.
    gesture_revision: u64,
    drag: Option<Drag>,
    resizing: Option<Resizing>,
    pointer: Point,
    hover: Hit,
    mods: ModifiersState,
    preedit: String,
    ime_area: Option<SkRect>,
    title: String,
    shot: Option<String>,
    start: Instant,
    script_next: usize,
    /// When a pane last drawn next needs a frame without any event: its frame hold (DEC
    /// 2026) ends (`Session::hold`) or its gathered resize is due (`Session::resize`).
    frame_due: Option<Instant>,
}

impl App {
    pub fn new(cfg: Cfg, config: Config, cache: PathBuf, proxy: EventLoopProxy) -> App {
        App {
            cfg,
            chrome: chrome_for(&config),
            fade: fade_for(&config),
            motion: Motion::new(config.pane_move_duration),
            config,
            cache,
            proxy,
            win: None,
            sidebar: false,
            layout: Layout::default(),
            panes: HashMap::new(),
            focus: None,
            next_pane: 0,
            gesture: Gesture::Idle,
            gesture_revision: 0,
            placed: Vec::new(),
            origins: HashMap::new(),
            placed_revision: 0,
            drag: None,
            resizing: None,
            pointer: Point::default(),
            hover: Hit::Nothing,
            mods: ModifiersState::empty(),
            preedit: String::new(),
            ime_area: None,
            title: String::new(),
            shot: None,
            start: Instant::now(),
            script_next: 0,
            frame_due: None,
        }
    }

    fn win(&self) -> &Win {
        self.win.as_ref().expect("the window exists once panes do")
    }

    /// Once the window can be seen again, draws the frame skipped while it could not.
    fn show_missed(&mut self) {
        if let Some(w) = self.win.as_mut().filter(|w| w.missed && !w.hidden()) {
            w.missed = false;
            w.window.request_redraw();
        }
    }

    fn redraw(&self) {
        if let Some(w) = &self.win {
            w.window.request_redraw();
        }
    }

    fn scene(&self) -> Scene {
        let w = self.win();
        scene::scene(&self.layout, w.size.0 as f32, w.size.1 as f32, self.sidebar, &w.metrics)
    }

    fn grid_size(&self, content: Rect) -> GridSize {
        let w = self.win();
        let f = &w.fonts;
        GridSize::fit(content.w as u32, content.h as u32, f.cell_w as u32, f.cell_h as u32, self.pad())
    }

    /// The configured space between a pane's content edge and its grid, in physical pixels,
    /// rounded down as Ghostty does. @see Ghostty `src/Surface.zig` (`scaledPadding`)
    fn pad(&self) -> Pad {
        let (scale, c) = (self.win().scale, &self.config);
        let px = |points: f32| (points * scale).floor() as u32;
        Pad {
            left: px(c.padding_x.start),
            top: px(c.padding_y.start),
            right: px(c.padding_x.end),
            bottom: px(c.padding_y.end),
        }
    }

    // ---- window ------------------------------------------------------------------------

    fn open_window(&mut self, el: &dyn ActiveEventLoop) {
        // Content runs under a transparent title bar; the traffic lights stay (ADR 6).
        let mac = WindowAttributesMacOS::default()
            .with_titlebar_transparent(true)
            .with_title_hidden(true)
            .with_fullsize_content_view(true);
        let attrs = WindowAttributes::default()
            .with_title("Frith")
            .with_surface_size(LogicalSize::new(WINDOW.0, WINDOW.1))
            .with_platform_attributes(Box::new(mac));
        let window = el.create_window(attrs).expect("create window");
        enable_ime(window.as_ref());
        let gpu = Gpu::attach(window.as_ref());
        let scale = window.scale_factor() as f32;
        let size = window.surface_size();
        let fonts = Fonts::new(&self.config.font_family, self.config.font_size * scale);
        let atlas = Atlas::new(&fonts);
        let ui = fonts::ui_font(UI_TEXT * scale);
        window.request_redraw();
        let metrics = Metrics::for_scale(scale);
        let screens = Screens::watch(self.proxy.clone());
        let size = (size.width, size.height);
        self.win =
            Some(Win { window, gpu, fonts, ui, atlas, metrics, scale, size, screens, occluded: false, missed: false });
    }

    fn rescale(&mut self, scale: f32) {
        let w = self.win.as_mut().expect("window");
        w.scale = scale;
        w.metrics = Metrics::for_scale(scale);
        w.fonts = Fonts::new(&self.config.font_family, self.config.font_size * scale);
        w.atlas = Atlas::new(&w.fonts);
        w.ui = fonts::ui_font(UI_TEXT * scale);
        for p in self.panes.values_mut() {
            p.grid.invalidate();
        }
        self.redraw();
    }

    // ---- panes -------------------------------------------------------------------------

    /// Adds a pane with `place`, starts `program` in it at its exact size, and focuses it.
    fn open_pane(&mut self, program: Program, place: impl FnOnce(&mut Layout, PaneId)) {
        self.next_pane += 1;
        let pane = PaneId(self.next_pane);
        place(&mut self.layout, pane);
        let scene = self.scene();
        let content = frame_of(&scene, pane).expect("a new pane is visible").content;
        let proxy = self.proxy.clone();
        let cwd = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let title = match &program {
            Program::LoginShell => "shell".to_string(),
            Program::Command(c) => c.clone(),
        };
        // Without it, prompts pile up on resize but everything else works (ADR 11).
        let integration = match program {
            Program::LoginShell => shell::install(&self.cache)
                .inspect_err(|e| eprintln!("frith: shell integration not installed: {e}"))
                .ok(),
            Program::Command(_) => None,
        };
        let size = self.grid_size(content);
        match Session::spawn(&program, &cwd, size, &self.config, integration.as_deref(), move || proxy.wake_up()) {
            Ok(session) => {
                let pane_state = Pane { session, grid: Grid::new(), title, attention: false };
                self.panes.insert(pane, pane_state);
                self.focus(pane);
            }
            Err(e) => {
                eprintln!("frith: cannot start {program:?}: {e}");
                self.layout.remove(pane);
            }
        }
        self.redraw();
    }

    /// A new login shell beside the focused tile, or along the window's right edge when no
    /// tile is focused.
    fn new_pane(&mut self, edge: Edge) {
        match self.focus.filter(|p| self.layout.is_tiled(*p)) {
            Some(at) => self.open_pane(Program::LoginShell, |l, p| l.split(at, edge, p)),
            None => self.open_pane(Program::LoginShell, |l, p| l.split_window(edge, p)),
        }
    }

    fn is_floating(&self, pane: PaneId) -> bool {
        self.layout.contains(pane) && !self.layout.is_tiled(pane) && !self.layout.docked().contains(&pane)
    }

    fn focus(&mut self, pane: PaneId) {
        self.focus = Some(pane);
        if let Some(p) = self.panes.get_mut(&pane) {
            p.attention = false;
        }
        if self.is_floating(pane) {
            let top = self.layout.floats(self.scene().area).last().map(|f| f.0);
            if top != Some(pane) {
                self.layout.raise(pane);
            }
        }
        self.redraw();
    }

    /// Moves focus to `next`, or to the top card or first tile, when the focused pane is no
    /// longer visible.
    fn refocus(&mut self, next: Option<PaneId>) {
        let scene = self.scene();
        if self.focus.is_some_and(|p| frame_of(&scene, p).is_some()) {
            return;
        }
        let fallback = scene.cards.last().or(scene.tiles.first()).map(|f| f.pane);
        self.focus = next.filter(|p| frame_of(&scene, *p).is_some()).or(fallback);
        if let Some(p) = self.focus {
            self.focus(p);
        }
    }

    /// Ends `pane` and its program; the app quits with its last pane (ADR 6).
    fn close_pane(&mut self, el: &dyn ActiveEventLoop, pane: PaneId) {
        if self.drag.as_ref().is_some_and(|d| d.pane == pane) {
            self.drag = None;
        }
        self.panes.remove(&pane);
        let next = self.layout.remove(pane);
        if self.panes.is_empty() {
            el.exit();
            return;
        }
        self.refocus(next);
        self.redraw();
    }

    /// Lifts tiled `pane` into a card near where it sat (ADR 6).
    fn float(&mut self, pane: PaneId) {
        let scene = self.scene();
        let outer = frame_of(&scene, pane).expect("a clicked pane is visible").outer;
        let w = self.win();
        let (mw, mh) = (w.px(MIN_CARD.0), w.px(MIN_CARD.1));
        let size = ((outer.w * 0.8).max(mw).round(), (outer.h * 0.8).max(mh).round());
        let offset = w.px(24.0);
        let card = Rect::new(outer.x + offset, outer.y + offset, size.0, size.1);
        self.layout.float(pane, card);
        self.focus(pane);
    }

    // ---- terminal output ---------------------------------------------------------------

    /// Parses pending output of every pane and redraws if any of it shows.
    fn feed(&mut self, el: &dyn ActiveEventLoop) {
        let mut more = false;
        let mut redraw = false;
        let mut exited = Vec::new();
        for (&id, pane) in &mut self.panes {
            let fed = pane.session.feed(FEED_BUDGET);
            more |= fed.more;
            redraw |= fed.bytes > 0;
            if fed.exited {
                exited.push(id);
            }
            if pane.session.shared.title_changed.replace(false) {
                pane.title = pane.session.title();
                redraw = true;
            }
            if pane.session.shared.notified.replace(false) && Some(id) != self.focus {
                pane.attention = true;
                redraw = true;
            }
        }
        for id in exited {
            self.close_pane(el, id);
        }
        if redraw {
            self.redraw();
        }
        if more {
            self.proxy.wake_up();
        }
    }

    // ---- drawing -----------------------------------------------------------------------

    /// Fits each shown pane's terminal to its content rect; returns when a gathered resize is
    /// due (see `Session::resize`).
    fn resize_sessions(&mut self, scene: &Scene) -> Option<Instant> {
        let mut due = None;
        for f in scene.tiles.iter().chain(&scene.cards) {
            let size = self.grid_size(f.content);
            let pane = self.panes.get_mut(&f.pane).expect("every placed pane exists");
            due = [due, pane.session.resize(size, Instant::now())].into_iter().flatten().min();
        }
        due
    }

    fn draw(&mut self) {
        let scene = self.scene();
        let frame_due = self.resize_sessions(&scene);
        let (lit, guides) = (self.lit_divider(), self.drag_guides(&scene));
        let shot = self.shot.take();
        let hovered = hit_pane(self.hover);
        let pad = self.pad();
        let pad = (pad.left as f32, pad.top as f32);
        // Only structural changes morph; dragged dividers and cards, and window resizes, keep
        // the revision and follow the pointer at once (ADR 10).
        let placed = placed(&scene);
        // With panes not moving, a change captures nothing to morph from either.
        let animate = !self.config.pane_move_duration.is_zero() && !reduce_motion();
        let changed = self.layout.revision() != self.placed_revision && animate;
        let revision = std::mem::replace(&mut self.placed_revision, self.layout.revision());
        let before = std::mem::replace(&mut self.placed, placed.clone());
        let App { win, panes, layout, focus, hover, pointer, preedit, chrome, fade, motion, origins, .. } = self;
        let fade = fade.filter(|_| scene.tiles.len() + scene.cards.len() > 1);
        let (layout, focus, hover, pointer) = (&*layout, *focus, *hover, *pointer);
        let w = win.as_mut().expect("window");
        let Win { gpu, fonts, ui, atlas, scale, .. } = w;
        let scale = *scale;
        let mut moving = false;
        let drawn = gpu.present(shot.as_deref(), |canvas, ctx| {
            // Sampled once the drawable is in hand, so each frame is timed by when it shows
            // rather than when it was asked for (CLAUDE.md principle 3).
            let now = Instant::now();
            let mut pen = render::Pen { canvas, chrome, ui, fonts, scale };
            let mut stage = Stage {
                panes,
                atlas,
                metrics: &scene.metrics,
                focus,
                hovered,
                hover,
                zoomed: layout.zoomed(),
                fade,
                pad,
                now,
                frame_due: None,
                cursor: None,
            };
            if changed {
                begin_morphs(&mut stage, &mut pen, ctx, (motion, origins), (&before, &placed), now);
            }
            moving = motion.settle(now);
            origins.retain(|p, _| motion.moving(*p, now));
            let (motion, origins) = (&*motion, &*origins);
            let morph = |pane, target| motion.morph(pane, target, now).map(|(r, p)| (r, p, origins.get(&pane)));
            render::background(&pen, &scene);
            // Tiles a zoom just hid stay under the zoomed one growing over them; then tiles at
            // rest, so a moving tile uncovers what it makes room for (a new split).
            let mut tiles_moving = false;
            for (pane, r) in motion.leaving(now).filter(|(p, _)| layout.is_tiled(*p)) {
                stage.live(&mut pen, ctx, pane, stage.shape(Kind::Tile, scale), r);
                tiles_moving = true;
            }
            let (resting, morphing): (Vec<_>, Vec<_>) =
                scene.tiles.iter().partition(|f| morph(f.pane, f.outer).is_none());
            for f in resting.iter().chain(&morphing) {
                stage.show(&mut pen, ctx, f.pane, Kind::Tile, f.outer, morph(f.pane, f.outer));
            }
            // Dividers sit where tiles end up; while tiles move, the gaps show instead.
            if !tiles_moving && morphing.is_empty() {
                render::dividers(&pen, &scene.dividers, lit);
            }
            for f in &scene.cards {
                stage.show(&mut pen, ctx, f.pane, Kind::Card, f.outer, morph(f.pane, f.outer));
            }
            if scene.tiles.is_empty() {
                render::empty_state(&mut pen, scene.area);
            }
            render::top_bar(&pen, &scene, hover);
            // Chips at rest, then panes on their way into or along the dock bar, above it.
            let (resting, morphing): (Vec<_>, Vec<_>) =
                scene.chips.iter().partition(|&&(p, chip)| morph(p, chip).is_none());
            for &(pane, chip) in resting.into_iter().chain(morphing) {
                stage.show(&mut pen, ctx, pane, Kind::Chip, chip, morph(pane, chip));
            }
            if let Some((guides, preview)) = &guides {
                let preview = preview.as_ref().map(|(zone, text)| (*zone, text.as_str()));
                render::dock_guides(&mut pen, &scene, guides, preview, pointer);
            }
            if let Some(c) = stage.cursor {
                render::preedit(&mut pen, c, preedit);
            }
            (stage.cursor, stage.frame_due)
        });
        if moving {
            w.window.request_redraw();
        }
        let Some((cursor, held)) = drawn else {
            // Nothing drew, so no change began: the next frame starts it.
            self.frame_due = frame_due;
            self.placed = before;
            self.placed_revision = revision;
            return;
        };
        self.frame_due = [frame_due, held].into_iter().flatten().min();
        self.sync_window(cursor);
    }

    /// The divider drawn in the accent: the one dragged, else the one under the pointer.
    fn lit_divider(&self) -> Option<usize> {
        match (self.gesture, self.hover) {
            (Gesture::DraggingDivider(i), _) | (_, Hit::Divider(i)) => Some(i),
            _ => None,
        }
    }

    /// While a pane is dragged, its dock guides and, over an active one, the zone a drop would
    /// fill with its label.
    fn drag_guides(&self, scene: &Scene) -> Option<(Guides, Option<(Rect, String)>)> {
        self.drag.as_ref().map(|d| {
            let preview = d.guides.active.map(|drop| {
                let zone = self.layout.drop_preview(scene.area, scene.metrics.gap, d.pane, drop);
                (zone, drop_label(drop))
            });
            (d.guides.clone(), preview)
        })
    }

    /// Points the IME at the focused cursor where it was drawn, and titles the window after
    /// the focused pane, each only when it changed.
    fn sync_window(&mut self, cursor: Option<SkRect>) {
        if let Some(c) = cursor.filter(|c| self.ime_area != Some(*c)) {
            self.ime_area = Some(c);
            let area = ImeRequestData::default().with_cursor_area(
                PhysicalPosition::new(c.left as i32, c.top as i32).into(),
                PhysicalSize::new(c.width() as u32, c.height() as u32).into(),
            );
            let _ = self.win().window.request_ime_update(ImeRequest::Update(area));
        }
        let title = self.focus.map(|p| self.panes[&p].title.clone()).unwrap_or_default();
        if title != self.title {
            self.win().window.set_title(if title.is_empty() { "Frith" } else { &title });
            self.title = title;
        }
    }

    /// Wakes the event loop for the next script step and for `frame_due`: the end of a frame
    /// hold or a gathered resize needs a frame even if no event comes.
    /// Runs in `about_to_wait`, which on macOS follows every redraw, so it sees `frame_due`.
    fn schedule(&mut self, el: &dyn ActiveEventLoop) {
        if self.frame_due.is_some_and(|t| t <= Instant::now()) {
            self.frame_due = None;
            self.redraw();
        }
        let step = self.cfg.script.get(self.script_next).map(|s| self.start + Duration::from_secs_f32(s.at));
        let wake = [step, self.frame_due].into_iter().flatten().min();
        el.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    // ---- pointer -----------------------------------------------------------------------

    fn pointer_moved(&mut self, p: Point) {
        self.pointer = p;
        let threshold = self.win().px(DRAG_THRESHOLD);
        match self.gesture.moved(p, threshold) {
            Effect::DragStart(pane) => self.start_drag(pane, p),
            Effect::DragMove(_) => self.drag_to(p),
            Effect::ResizeCard(pane, r) => self.resize_card(pane, r, p),
            Effect::ResizeDivider(i) => {
                let scene = self.scene();
                let Some(d) = scene.dividers.iter().find(|d| d.index == i) else { return };
                let along = if d.axis == Axis::Horizontal { p.x } else { p.y };
                let min = self.win().px(MIN_TILE);
                self.layout.drag_divider(scene.area, scene.metrics.gap, i, along, min);
                self.redraw();
            }
            Effect::TerminalMotion(pane) => {
                self.terminal_mouse(pane, mouse::Action::Motion, Some(mouse::Button::Left), true)
            }
            _ => {
                let scene = self.scene();
                let hit = scene.hit(p);
                self.win().window.set_cursor(cursor_icon(hit, &scene).into());
                if hit != self.hover {
                    // Buttons, chips and dividers light up; tile buttons show on hover.
                    if hit_pane(hit) != hit_pane(self.hover) || lit(hit) || lit(self.hover) {
                        self.redraw();
                    }
                    self.hover = hit;
                }
                if let Hit::Content(pane) = hit {
                    self.terminal_mouse(pane, mouse::Action::Motion, None, false);
                }
            }
        }
    }

    fn pointer_button(&mut self, el: &dyn ActiveEventLoop, pressed: bool, button: MouseButton, p: Point) {
        self.pointer = p;
        let scene = self.scene();
        let hit = scene.hit(p);
        if button != MouseButton::Left {
            let b = match button {
                MouseButton::Right => mouse::Button::Right,
                MouseButton::Middle => mouse::Button::Middle,
                _ => return,
            };
            if let Hit::Content(pane) = hit {
                let action = if pressed { mouse::Action::Press } else { mouse::Action::Release };
                self.terminal_mouse(pane, action, Some(b), pressed);
            }
            return;
        }
        if pressed {
            if let Hit::Resize(pane, _) = hit {
                let from = frame_of(&scene, pane).expect("hit pane is visible").outer;
                self.resizing = Some(Resizing { pane, from, start: p });
            }
            for effect in self.gesture.press(hit, p) {
                self.effect(el, effect);
            }
            self.gesture_revision = self.layout.revision();
        } else {
            let effect = self.gesture.release(hit);
            self.resizing = None;
            self.effect(el, effect);
        }
    }

    /// Carries out a gesture effect.
    fn effect(&mut self, el: &dyn ActiveEventLoop, effect: Effect) {
        match effect {
            Effect::None => {}
            Effect::Focus(pane) => self.focus(pane),
            Effect::TerminalPress(pane) => {
                self.focus(pane);
                self.terminal_mouse(pane, mouse::Action::Press, Some(mouse::Button::Left), true);
            }
            Effect::TerminalRelease(pane) => {
                self.terminal_mouse(pane, mouse::Action::Release, Some(mouse::Button::Left), false);
            }
            Effect::Click(pane, b) => self.click(el, pane, b),
            Effect::Chrome(ChromeButton::Sidebar) => {
                self.sidebar = !self.sidebar;
                self.redraw();
            }
            Effect::Chrome(ChromeButton::Launcher) => self.new_pane(Edge::Right),
            Effect::Restore(pane) => {
                self.layout.restore(pane);
                self.focus(pane);
            }
            Effect::Drop(_) => self.finish_drag(),
            Effect::CancelDrag(_) => self.cancel_drag(),
            Effect::DragWindow => {
                if let Err(e) = self.win().window.drag_window() {
                    eprintln!("frith: window drag failed: {e:?}");
                }
            }
            Effect::DragStart(_)
            | Effect::DragMove(_)
            | Effect::ResizeCard(..)
            | Effect::ResizeDivider(_)
            | Effect::TerminalMotion(_) => unreachable!("{effect:?} comes only from moves"),
        }
    }

    fn click(&mut self, el: &dyn ActiveEventLoop, pane: PaneId, b: PaneButton) {
        match b {
            PaneButton::SplitRight => self.open_pane(Program::LoginShell, |l, p| l.split(pane, Edge::Right, p)),
            PaneButton::SplitDown => self.open_pane(Program::LoginShell, |l, p| l.split(pane, Edge::Bottom, p)),
            PaneButton::Float => self.float(pane),
            PaneButton::Zoom => {
                self.layout.toggle_zoom(pane);
                self.focus(pane);
            }
            PaneButton::Close => self.close_pane(el, pane),
            PaneButton::Minimize => {
                self.layout.minimize(pane);
                self.refocus(None);
                self.redraw();
            }
            PaneButton::Embed => {
                self.layout.embed(pane);
                self.focus(pane);
            }
        }
    }

    fn start_drag(&mut self, pane: PaneId, p: Point) {
        let scene = self.scene();
        let outer = frame_of(&scene, pane).expect("a dragged pane is visible").outer;
        let card = scene.cards.iter().any(|f| f.pane == pane).then_some(outer);
        let guides = Guides { target: None, buttons: Vec::new(), active: None };
        self.drag = Some(Drag { pane, grab: (p.x - outer.x, p.y - outer.y), card, guides });
        self.win().window.set_cursor(CursorIcon::Grabbing.into());
        self.drag_to(p);
    }

    /// Moves a dragged card with the pointer and updates the dock guides.
    fn drag_to(&mut self, p: Point) {
        let Some(d) = &self.drag else { return };
        if let Some(card) = d.card {
            let at = Rect::new(p.x - d.grab.0, p.y - d.grab.1, card.w, card.h);
            self.layout.move_float(d.pane, at);
        }
        let guides = self.scene().guides(&self.layout, d.pane, p, d.guides.target);
        self.drag.as_mut().expect("dragging").guides = guides;
        self.redraw();
    }

    /// Docks the dragged pane where the preview showed. A card released outside the window
    /// goes back where it was (ADR 6); otherwise it stays where it was moved.
    fn finish_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        let (w, h) = self.win().size;
        let inside = Rect::new(0.0, 0.0, w as f32, h as f32).contains(self.pointer);
        match (d.guides.active, d.card) {
            (Some(drop), _) if inside => {
                self.layout.apply(d.pane, drop);
                self.focus(d.pane);
            }
            (_, Some(card)) if !inside => self.layout.move_float(d.pane, card),
            _ => {}
        }
        self.redraw();
    }

    fn cancel_drag(&mut self) {
        if let Some(Drag { pane, card: Some(card), .. }) = self.drag.take() {
            self.layout.move_float(pane, card);
        }
        self.redraw();
    }

    fn resize_card(&mut self, pane: PaneId, r: Resize, p: Point) {
        let Some(rs) = self.resizing.as_ref().filter(|rs| rs.pane == pane) else { return };
        let (mw, mh) = (self.win().px(MIN_CARD.0), self.win().px(MIN_CARD.1));
        let (dx, dy) = (p.x - rs.start.x, p.y - rs.start.y);
        let f = rs.from;
        let (mut left, mut top, mut right, mut bottom) = (f.x, f.y, f.right(), f.bottom());
        if r.left {
            left = (left + dx).min(right - mw);
        }
        if r.right {
            right = (right + dx).max(left + mw);
        }
        if r.top {
            top = (top + dy).min(bottom - mh);
        }
        if r.bottom {
            bottom = (bottom + dy).max(top + mh);
        }
        self.layout.move_float(pane, Rect::new(left, top, right - left, bottom - top));
        self.redraw();
    }

    /// Reports a mouse event to `pane`'s program at the pointer.
    fn terminal_mouse(
        &mut self,
        pane: PaneId,
        action: mouse::Action,
        button: Option<mouse::Button>,
        any_pressed: bool,
    ) {
        let scene = self.scene();
        let Some(f) = frame_of(&scene, pane) else { return };
        let local = (self.pointer.x - f.content.x, self.pointer.y - f.content.y);
        if let Some(p) = self.panes.get_mut(&pane) {
            p.session.mouse(action, button, local, self.mods, any_pressed);
        }
    }

    fn wheel(&mut self, delta: MouseScrollDelta) {
        let scene = self.scene();
        let (Hit::Content(pane) | Hit::Header(pane)) = scene.hit(self.pointer) else { return };
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => y,
            MouseScrollDelta::PixelDelta(p) => p.y as f32 / self.win().fonts.cell_h,
            _ => return, // non-exhaustive
        };
        let f = frame_of(&scene, pane).expect("hit pane is visible");
        let local = (self.pointer.x - f.content.x, self.pointer.y - f.content.y);
        self.panes.get_mut(&pane).expect("pane").session.wheel(lines, local, self.mods);
        self.redraw();
    }

    /// Ends a gesture that a structural change (a pane exited, a shortcut) made stale, so no
    /// later event acts on a pane or divider it no longer refers to.
    fn settle(&mut self, el: &dyn ActiveEventLoop) {
        if self.gesture != Gesture::Idle && self.layout.revision() != self.gesture_revision {
            let effect = self.gesture.abort();
            self.resizing = None;
            match effect {
                Effect::CancelDrag(_) => self.cancel_drag(),
                Effect::TerminalRelease(p) if self.panes.contains_key(&p) => self.effect(el, effect),
                _ => {}
            }
        }
    }

    // ---- keyboard ----------------------------------------------------------------------

    fn key(&mut self, el: &dyn ActiveEventLoop, event: KeyEvent) {
        let pressed = event.state == ElementState::Pressed;
        if pressed && event.logical_key == Key::Named(NamedKey::Escape) && self.drag.is_some() {
            let effect = self.gesture.cancel();
            return self.effect(el, effect);
        }
        if self.mods.meta_key() {
            if pressed {
                self.shortcut(el, &event);
            }
            return;
        }
        if !self.preedit.is_empty() {
            return; // IME composition owns the keyboard
        }
        if let Some(p) = self.focus.and_then(|f| self.panes.get_mut(&f)) {
            p.session.key(&event, self.mods);
        }
    }

    /// Cmd shortcuts; split, zoom and pane-cycling keys follow Ghostty's macOS defaults.
    /// @see https://github.com/ghostty-org/ghostty/blob/108cacb17ed8990840cd14abfce43d8ed8503ca1/src/config/Config.zig (`new_split`, `goto_split`, `toggle_split_zoom`)
    fn shortcut(&mut self, el: &dyn ActiveEventLoop, event: &KeyEvent) {
        let shift = self.mods.shift_key();
        match event.key_without_modifiers.as_ref() {
            Key::Character("q") => el.exit(),
            Key::Character("d") => self.new_pane(if shift { Edge::Bottom } else { Edge::Right }),
            Key::Character("v") => {
                if let (Some(text), Some(p)) = (clipboard_text(), self.focus.and_then(|f| self.panes.get_mut(&f))) {
                    p.session.paste(text.into_bytes());
                }
            }
            Key::Character("w") => {
                if let Some(p) = self.focus {
                    self.close_pane(el, p);
                }
            }
            Key::Character("[") => self.cycle_focus(-1),
            Key::Character("]") => self.cycle_focus(1),
            Key::Named(NamedKey::Enter) if shift => {
                if let Some(p) = self.focus.filter(|p| self.layout.is_tiled(*p)) {
                    self.layout.toggle_zoom(p);
                    self.redraw();
                }
            }
            _ => {}
        }
    }

    /// Moves focus `step` panes along the visible panes: tiles in reading order, then cards.
    fn cycle_focus(&mut self, step: isize) {
        let scene = self.scene();
        let order: Vec<PaneId> = scene.tiles.iter().chain(&scene.cards).map(|f| f.pane).collect();
        let Some(at) = self.focus.and_then(|f| order.iter().position(|p| *p == f)) else { return };
        let next = order[(at as isize + step).rem_euclid(order.len() as isize) as usize];
        self.focus(next);
    }

    // ---- scripted runs (see main.rs) ---------------------------------------------------

    fn run_script(&mut self, el: &dyn ActiveEventLoop) {
        let elapsed = self.start.elapsed().as_secs_f32();
        while let Some(step) = self.cfg.script.get(self.script_next).filter(|s| s.at <= elapsed) {
            let (action, arg) = (step.action.clone(), step.arg.clone());
            self.script_next += 1;
            self.script_step(el, &action, &arg);
            self.settle(el);
            if el.exiting() {
                return;
            }
        }
    }

    fn script_step(&mut self, el: &dyn ActiveEventLoop, action: &str, arg: &str) {
        match action {
            "send" => {
                if let Some(p) = self.focus.and_then(|f| self.panes.get(&f)) {
                    p.session.write(&unescape(arg));
                }
            }
            "shot" => {
                self.shot = Some(arg.to_string());
                self.redraw();
            }
            "press" | "move" | "release" => {
                let v: Vec<f32> = arg.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                let [x, y] = v[..] else {
                    return eprintln!("frith: script {action} needs x y, got {arg:?}");
                };
                // Through the same handlers real events use; logical pixels.
                let s = self.win().scale;
                let p = Point::new(x * s, y * s);
                match action {
                    "move" => self.pointer_moved(p),
                    _ => self.pointer_button(el, action == "press", MouseButton::Left, p),
                }
            }
            "size" => {
                let v: Vec<f64> = arg.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                let [w, h] = v[..] else {
                    return eprintln!("frith: script size needs w h, got {arg:?}");
                };
                let _ = self.win().window.request_surface_size(LogicalSize::new(w, h).into());
            }
            "quit" => el.exit(),
            _ => eprintln!("frith: unknown script action {action}"),
        }
    }
}

/// Where each pane sits and what it shows as: tiles, cards, then dock chips.
fn placed(scene: &Scene) -> Vec<Placed> {
    let tiles = scene.tiles.iter().map(|f| (f.pane, f.outer, Kind::Tile));
    let cards = scene.cards.iter().map(|f| (f.pane, f.outer, Kind::Card));
    tiles.chain(cards).chain(scene.chips.iter().map(|&(p, r)| (p, r, Kind::Chip))).collect()
}

/// Starts the morphs of panes whose rects changed from `before` to `placed`. Each starts from
/// the shape it shows now, mid-morph or not; one turning into or out of its chip, or already
/// fading an image, also starts from an image of what is on screen; others draw live (see
/// `stage`).
fn begin_morphs(
    stage: &mut Stage,
    pen: &mut render::Pen,
    ctx: &mut skia_safe::gpu::DirectContext,
    (motion, origins): (&mut Motion, &mut HashMap<PaneId, Origin>),
    (before, placed): (&[Placed], &[Placed]),
    now: Instant,
) {
    for &was @ (pane, from, _) in before {
        let Some(&(_, to, next)) = placed.iter().find(|(p, ..)| *p == pane) else { continue };
        if to == from {
            continue;
        }
        let (old, shown) = (origins.remove(&pane), motion.morph(pane, from, now));
        origins.insert(pane, stage.origin(pen, ctx, was, next, shown, old));
    }
    motion.retarget(&rects(before), &rects(placed), now);
}

fn rects(placed: &[Placed]) -> Vec<(PaneId, Rect)> {
    placed.iter().map(|&(p, r, _)| (p, r)).collect()
}

/// The tile or card frame of `pane`, if it is visible.
fn frame_of(scene: &Scene, pane: PaneId) -> Option<&Frame> {
    scene.tiles.iter().chain(&scene.cards).find(|f| f.pane == pane)
}

/// The pane a hit belongs to.
fn hit_pane(hit: Hit) -> Option<PaneId> {
    match hit {
        Hit::Button(p, _) | Hit::Resize(p, _) | Hit::Header(p) | Hit::Content(p) => Some(p),
        _ => None,
    }
}

/// Hits that are drawn highlighted.
fn lit(hit: Hit) -> bool {
    matches!(hit, Hit::Button(..) | Hit::Chip(_) | Hit::Chrome(_) | Hit::Divider(_))
}

fn drop_label(drop: Drop) -> String {
    let side = |e: Edge| match e {
        Edge::Left => "left",
        Edge::Right => "right",
        Edge::Top => "top",
        Edge::Bottom => "bottom",
    };
    match drop {
        Drop::Split(_, e) => format!("Split {}", side(e)),
        Drop::Swap(_) => "Swap".to_string(),
        Drop::Window(e) => format!("Dock {}", side(e)),
    }
}

fn cursor_icon(hit: Hit, scene: &Scene) -> CursorIcon {
    match hit {
        Hit::Divider(i) => match scene.dividers.iter().find(|d| d.index == i).map(|d| d.axis) {
            Some(Axis::Horizontal) => CursorIcon::ColResize,
            _ => CursorIcon::RowResize,
        },
        Hit::Resize(_, r) => match (r.left || r.right, r.top || r.bottom) {
            (true, true) if r.left == r.top => CursorIcon::NwseResize,
            (true, true) => CursorIcon::NeswResize,
            (true, false) => CursorIcon::EwResize,
            _ => CursorIcon::NsResize,
        },
        Hit::Header(_) => CursorIcon::Grab,
        Hit::Button(..) | Hit::Chip(_) | Hit::Chrome(_) => CursorIcon::Pointer,
        Hit::Content(_) => CursorIcon::Text,
        Hit::DragWindow | Hit::Nothing => CursorIcon::Default,
    }
}

fn clipboard_text() -> Option<String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    // SAFETY: reading an AppKit constant that is set before `main` runs.
    let kind = unsafe { NSPasteboardTypeString };
    NSPasteboard::generalPasteboard().stringForType(kind).map(|s| s.to_string())
}

/// Whether macOS's Reduce Motion setting is on; layout changes then skip their springs
/// (ADR 10). @see https://developer.apple.com/documentation/appkit/nsworkspace/accessibilitydisplayshouldreducemotion
fn reduce_motion() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

fn enable_ime(window: &dyn Window) {
    let request = ImeEnableRequest::new(
        ImeCapabilities::new().with_cursor_area(),
        ImeRequestData::default()
            .with_cursor_area(PhysicalPosition::new(0, 0).into(), PhysicalSize::new(10, 10).into()),
    )
    .expect("IME request carries the cursor area it declares");
    if let Err(e) = window.request_ime_update(ImeRequest::Enable(request)) {
        eprintln!("frith: IME enable failed: {e:?}");
    }
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, el: &dyn ActiveEventLoop) {
        if self.win.is_some() {
            return;
        }
        self.open_window(el);
        let program = self.cfg.cmd.clone().map_or(Program::LoginShell, Program::Command);
        self.open_pane(program, |l, p| l.split_window(Edge::Right, p));
    }

    fn proxy_wake_up(&mut self, el: &dyn ActiveEventLoop) {
        self.show_missed();
        self.feed(el);
        self.settle(el);
    }

    fn about_to_wait(&mut self, el: &dyn ActiveEventLoop) {
        if !el.exiting() && self.win.is_some() {
            self.run_script(el);
            self.schedule(el);
        }
    }

    fn window_event(&mut self, el: &dyn ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if self.win.is_none() || el.exiting() {
            return;
        }
        let pointer = matches!(
            event,
            WindowEvent::PointerMoved { .. } | WindowEvent::PointerButton { .. } | WindowEvent::MouseWheel { .. }
        );
        if pointer && !self.cfg.script.is_empty() {
            return; // scripted runs drive the pointer themselves
        }
        match event {
            WindowEvent::RedrawRequested => {
                // A pending screenshot still draws, so scripted runs get it while hidden.
                let w = self.win.as_mut().expect("window");
                w.missed = w.hidden() && self.shot.is_none();
                if !w.missed {
                    self.draw();
                }
            }
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::SurfaceResized(size) => {
                let w = self.win.as_mut().expect("window");
                w.size = (size.width.max(1), size.height.max(1));
                w.gpu.resize(w.size.0, w.size.1);
                w.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => self.rescale(scale_factor as f32),
            WindowEvent::Occluded(occluded) => {
                self.win.as_mut().expect("window").occluded = occluded;
                self.show_missed();
            }
            WindowEvent::Focused(false) => {
                let effect = self.gesture.abort();
                self.resizing = None;
                self.effect(el, effect);
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::Ime(ime) => {
                match ime {
                    Ime::Preedit(text, _) => self.preedit = text,
                    Ime::Commit(text) => {
                        self.preedit.clear();
                        if let Some(p) = self.focus.and_then(|f| self.panes.get(&f)) {
                            p.session.write(text.as_bytes());
                        }
                    }
                    _ => {}
                }
                self.redraw();
            }
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => self.key(el, event),
            WindowEvent::PointerMoved { position, .. } => {
                self.pointer_moved(Point::new(position.x as f32, position.y as f32));
            }
            WindowEvent::PointerButton { state, button: ButtonSource::Mouse(button), position, .. } => {
                let p = Point::new(position.x as f32, position.y as f32);
                self.pointer_button(el, state == ElementState::Pressed, button, p);
            }
            WindowEvent::PointerLeft { .. } => {
                if self.hover != Hit::Nothing && self.gesture == Gesture::Idle {
                    self.hover = Hit::Nothing;
                    self.redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => self.wheel(delta),
            _ => {}
        }
        self.settle(el);
    }
}

/// The chrome for the configured colors. Palette 3 (yellow) is the attention color, at
/// libghostty-vt's default unless configured.
/// @see Ghostty `src/terminal/color.zig` (`Name.default`)
fn chrome_for(config: &Config) -> Chrome {
    let attention = config.palette.get(&3).copied().unwrap_or(Rgb { r: 0xf0, g: 0xc6, b: 0x74 });
    Chrome::derive(config.background, config.foreground, config.accent, attention)
}

/// The fade over unfocused panes: their content shows through at
/// `unfocused-split-opacity`, under `unfocused-split-fill` or the background.
/// @see Ghostty `macos/Sources/Ghostty/Ghostty.Config.swift` (`unfocusedSplitOpacity`, `unfocusedSplitFill`)
fn fade_for(config: &Config) -> Option<Tint> {
    let alpha = ((1.0 - config.unfocused_split_opacity) * 255.0).round() as u8;
    let rgb = config.unfocused_split_fill.unwrap_or(config.background);
    (alpha > 0).then_some(Tint { rgb, alpha })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfocused_panes_fade_by_the_complement_of_the_opacity_under_the_fill_or_background() {
        let mut c = Config::default();
        assert_eq!(fade_for(&c), Some(Tint { rgb: c.background, alpha: 77 }), "0.7: 30% of the background");
        c.read("unfocused-split-opacity = 0.15\nunfocused-split-fill = #102030\n");
        assert_eq!(fade_for(&c), Some(Tint { rgb: Rgb { r: 0x10, g: 0x20, b: 0x30 }, alpha: 217 }));
        c.read("unfocused-split-opacity = 1\n");
        assert_eq!(fade_for(&c), None, "fully opaque: no fade");
    }
}
