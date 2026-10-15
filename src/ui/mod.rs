//! The viewer window: renders the remote canvas, forwards input and keeps
//! the host's virtual display sized to the window.

mod controls;
pub mod dialog;
mod keymap;
mod reconnect;
mod render;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::window::{Fullscreen, CursorIcon, CustomCursor, Window, WindowAttributes, WindowId};

use crate::proto::messages::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SCROLL_DOWN, BTN_SCROLL_UP, DisplayRequest};
use crate::session::{Config, CursorShape, Event, EventSink, Session};
use reconnect::{OnEnd, Reconnector};
use render::Renderer;

/// Settle time before asking the host to follow a window resize; each
/// request restarts the host's encoder.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(500);
/// The host scrolls exactly one point per wheel click it receives (the
/// press/release pairs are its only scroll input), so scrolling is sent as
/// one click per point. A mouse-wheel notch becomes this many points.
const SCROLL_POINTS_PER_LINE: f64 = 32.0;
/// Clicks sent per input event at most; the rest carries over, so a hard
/// flick doesn't flood the control channel in one go.
const SCROLL_MAX_CLICKS_PER_EVENT: f64 = 200.0;

pub struct Options {
    pub title: String,
    /// The host as the user named it, for messages.
    pub host_name: String,
    /// Initial window size in logical pixels.
    pub size: (u32, u32),
    /// Backing scale to request; `None` follows the window's scale factor.
    pub scale: Option<f64>,
    pub auto_resize: bool,
    pub swap_ctrl_cmd: bool,
    /// Start a new session when an established one drops.
    pub reconnect: bool,
}

static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A session event tagged with the session it came from, so stragglers from
/// a replaced session are ignored.
pub struct Tagged {
    generation: u64,
    event: Event,
}

struct ProxySink {
    proxy: EventLoopProxy<Tagged>,
    generation: u64,
}

impl EventSink for ProxySink {
    fn emit(&self, event: Event) {
        let _ = self.proxy.send_event(Tagged { generation: self.generation, event });
    }
}

struct App {
    cfg: Config,
    opts: Options,
    proxy: EventLoopProxy<Tagged>,
    instance: wgpu::Instance,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    session: Option<Session>,
    /// Bumped for every session started.
    generation: u64,
    reconnector: Reconnector,
    /// When to start the next reconnect attempt.
    retry_due: Option<Instant>,
    /// Error that ended the session, reported after the window closes.
    error: Option<String>,

    last_request: Option<DisplayRequest>,
    resize_due: Option<Instant>,

    cursor_pos: Option<PhysicalPosition<f64>>,
    buttons: u8,
    scroll_accum: f64,
    modifiers: ModifiersState,
    /// Keysym sent for each held physical key, so releases match presses
    /// even if modifiers changed in between.
    pressed: HashMap<KeyCode, u32>,
    cursors: HashMap<u32, CustomCursor>,
    remote_cursor: Option<CursorShape>,
    local_cursor: bool,
    status: String,
    ui_due: Option<Instant>,
}

impl App {
    fn toggle_fullscreen(&mut self) {
        self.release_all();
        if let Some(w) = &self.window {
            w.set_fullscreen(if w.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) });
            w.request_redraw();
        }
    }

    fn sync_cursor(&mut self, el: &ActiveEventLoop) {
        let local = self.renderer.as_ref().is_some_and(|r| r.controls.local_pointer());
        if local == self.local_cursor {
            return;
        }
        self.local_cursor = local;
        if local {
            if let Some(w) = &self.window {
                w.set_cursor_visible(true);
                w.set_cursor(CursorIcon::Default);
            }
        } else if let Some(c) = self.remote_cursor.clone() {
            self.set_cursor(el, c);
        }
    }

    fn apply_controls(&mut self, el: &ActiveEventLoop, actions: controls::Actions) {
        if actions.fullscreen {
            self.toggle_fullscreen();
        }
        if let Some(scale) = actions.render_scale {
            self.cfg.op.lock().render_scale = scale;
            self.send_resize();
        }
        if let Some(after_loss) = actions.after_loss {
            self.cfg.op.lock().after_loss = after_loss;
        }
        if actions.disconnect {
            self.release_all();
            el.exit();
        } else if actions.restart {
            self.release_all();
            if let Some(s) = self.session.take() {
                s.shutdown();
            }
            self.retry_due = None;
            self.cursors.clear();
            self.remote_cursor = None;
            self.scroll_accum = 0.0;
            self.status = "Connecting…".into();
            if let Some(w) = &self.window {
                w.set_title(&format!("{} — connecting…", self.opts.title));
                w.request_redraw();
            }
            if let Some(r) = &mut self.renderer {
                r.set_dimmed(true);
            }
            // Explicit restart works even with --no-reconnect.
            self.start_session();
        }
    }

    fn display_request(&self, size: PhysicalSize<u32>, scale_factor: f64) -> DisplayRequest {
        let scale = self.opts.scale.unwrap_or(if scale_factor >= 1.5 { 2.0 } else { 1.0 });
        // Same logical size, fewer backing pixels: the Mac lays out as usual
        // and we scale up its smaller picture.
        let r = self.cfg.op.lock().render_scale;
        let px = |v: u32| (v as f64 * r).round() as u32;
        DisplayRequest::for_pixels(px(size.width), px(size.height), scale * r)
    }

    fn send_resize(&mut self) {
        let (Some(w), Some(s)) = (&self.window, &self.session) else { return };
        let req = self.display_request(w.inner_size(), w.scale_factor());
        if self.last_request != Some(req) {
            self.last_request = Some(req);
            s.resize(req);
        }
    }

    /// Window position → canvas pixel, clamped to the canvas.
    fn canvas_point(&self, pos: PhysicalPosition<f64>) -> Option<(u16, u16)> {
        let r = self.renderer.as_ref()?;
        let vp = r.viewport()?;
        let (iw, ih) = r.image_size()?;
        let x = ((pos.x as f32 - vp.x) / vp.w * iw as f32).clamp(0.0, iw.saturating_sub(1) as f32);
        let y = ((pos.y as f32 - vp.y) / vp.h * ih as f32).clamp(0.0, ih.saturating_sub(1) as f32);
        Some((x as u16, y as u16))
    }

    fn send_pointer(&self, buttons: u8) {
        if let (Some(s), Some(pos)) = (&self.session, self.cursor_pos)
            && let Some((x, y)) = self.canvas_point(pos) {
                s.pointer(buttons, x, y);
            }
    }

    /// Scrolls by `points` (positive is up), carrying any fraction.
    fn scroll(&mut self, points: f64) {
        self.scroll_accum += points;
        let clicks = self.scroll_accum.trunc().clamp(-SCROLL_MAX_CLICKS_PER_EVENT, SCROLL_MAX_CLICKS_PER_EVENT);
        self.scroll_accum -= clicks;
        let bit = if clicks > 0.0 { BTN_SCROLL_UP } else { BTN_SCROLL_DOWN };
        for _ in 0..clicks.abs() as u32 {
            // Each press/release pair is one wheel click on the host.
            self.send_pointer(self.buttons | bit);
            self.send_pointer(self.buttons);
        }
    }

    fn key(&mut self, ev: &KeyEvent) {
        let Some(s) = &self.session else { return };
        let PhysicalKey::Code(code) = ev.physical_key else { return };
        let down = ev.state == ElementState::Pressed;
        if !down {
            if let Some(ks) = self.pressed.remove(&code) {
                s.key(false, ks);
            }
            return;
        }
        let m = self.modifiers;
        let shortcut = m.control_key() || m.alt_key() || m.super_key();
        let logical = if shortcut { ev.key_without_modifiers() } else { ev.logical_key.clone() };
        let Some(ks) = keymap::keysym(ev.physical_key, &logical, self.opts.swap_ctrl_cmd) else {
            return;
        };
        if let Some(prev) = self.pressed.insert(code, ks)
            && prev != ks {
                s.key(false, prev);
            }
        s.key(true, ks);
    }

    /// Releases everything we hold on the host, e.g. when focus is lost
    /// mid-shortcut and the release would never arrive.
    fn release_all(&mut self) {
        if let Some(s) = &self.session {
            for (_, ks) in self.pressed.drain() {
                s.key(false, ks);
            }
        }
        if self.buttons != 0 {
            self.buttons = 0;
            self.send_pointer(0);
        }
    }

    fn set_cursor(&mut self, el: &ActiveEventLoop, c: CursorShape) {
        let Some(w) = &self.window else { return };
        if !c.fresh
            && let Some(cur) = self.cursors.get(&c.cache_id) {
                w.set_cursor_visible(true);
                w.set_cursor(cur.clone());
                return;
            }
        let Some(rgba) = c.rgba else {
            w.set_cursor_visible(true);
            w.set_cursor(CursorIcon::Default);
            return;
        };
        if c.width == 0 || c.height == 0 {
            w.set_cursor_visible(false);
            return;
        }
        w.set_cursor_visible(true);
        let hot = (c.hotspot.0.min(c.width - 1), c.hotspot.1.min(c.height - 1));
        match CustomCursor::from_rgba(rgba.to_vec(), c.width, c.height, hot.0, hot.1) {
            Ok(src) => {
                let cur = el.create_custom_cursor(src);
                if self.cursors.len() >= 64 {
                    self.cursors.clear();
                }
                self.cursors.insert(c.cache_id, cur.clone());
                w.set_cursor(cur);
            }
            Err(e) => tracing::debug!("cursor image rejected: {e}"),
        }
    }

    fn start_session(&mut self) {
        let Some(w) = &self.window else { return };
        let req = self.display_request(w.inner_size(), w.scale_factor());
        self.last_request = Some(req);
        // Process-wide, so events left queued by a previous viewer run on
        // the shared event loop can't match a new session.
        self.generation = NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sink = ProxySink { proxy: self.proxy.clone(), generation: self.generation };
        let cfg = Config { resumed: self.reconnector.reconnecting(), ..self.cfg.clone() };
        self.session = Some(Session::start(cfg, req, Arc::new(sink)));
    }

    /// The session ended; either quit or schedule a fresh one.
    fn session_ended(&mut self, el: &ActiveEventLoop, reason: String, retryable: bool) {
        let delay = match self.reconnector.ended(retryable) {
            OnEnd::Exit => return self.fail(el, reason),
            OnEnd::GiveUp => {
                let host = &self.opts.host_name;
                return self.fail(el, format!("Gave up reconnecting to {host} after 10 minutes without video ({reason})"));
            }
            OnEnd::Retry(d) => d,
        };
        self.status = format!("Reconnecting: {reason}");
        tracing::warn!("{reason}; reconnecting in {} s", delay.as_secs());
        // Joins the old threads so their sockets and UDP ports are released
        // before the next session binds them.
        if let Some(s) = self.session.take() {
            s.shutdown();
        }
        self.retry_due = Some(Instant::now() + delay);
        // Input state belongs to the dead session; the host forgets held
        // keys and buttons with it.
        self.pressed.clear();
        self.buttons = 0;
        self.scroll_accum = 0.0;
        self.cursors.clear();
        self.remote_cursor = None;
        if let Some(w) = &self.window {
            w.set_cursor_visible(true);
            w.set_cursor(CursorIcon::Default);
            w.set_title(&format!("{} — reconnecting…", self.opts.title));
            w.request_redraw();
        }
        if let Some(r) = &mut self.renderer {
            r.set_dimmed(true);
        }
        self.sync_cursor(el);
    }

    fn fail(&mut self, el: &ActiveEventLoop, msg: String) {
        self.error = Some(msg);
        el.exit();
    }
}

/// Window attributes carrying the app id (Wayland) and WM_CLASS (X11), so
/// desktops match windows to `pippin.desktop`.
pub(crate) fn app_window_attributes() -> WindowAttributes {
    use winit::platform::{wayland::WindowAttributesExtWayland, x11::WindowAttributesExtX11};
    let attrs = Window::default_attributes();
    let attrs = WindowAttributesExtWayland::with_name(attrs, "pippin", "pippin");
    WindowAttributesExtX11::with_name(attrs, "pippin", "pippin")
}

impl ApplicationHandler<Tagged> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = app_window_attributes()
            .with_title(format!("{} — connecting…", self.opts.title))
            .with_inner_size(LogicalSize::new(self.opts.size.0, self.opts.size.1));
        let window = match el.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(el, format!("creating window: {e}")),
        };
        match Renderer::new(&self.instance, window.clone()) {
            Ok(r) => self.renderer = Some(r),
            Err(e) => return self.fail(el, format!("{e:#}")),
        }
        self.window = Some(window);
        self.start_session();
    }

    fn user_event(&mut self, el: &ActiveEventLoop, Tagged { generation, event }: Tagged) {
        if generation != self.generation {
            return;
        }
        match event {
            Event::Connected => {
                if self.reconnector.reconnecting() {
                    tracing::info!("reconnected");
                }
                self.reconnector.connected();
                self.status = "Connected".into();
                if let Some(r) = &mut self.renderer {
                    r.set_dimmed(false);
                }
                if let Some(w) = &self.window {
                    w.set_title(&self.opts.title);
                }
                // The window may have been resized while connecting.
                if self.opts.auto_resize {
                    self.send_resize();
                }
            }
            Event::Frame => {
                self.reconnector.video();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Event::Canvas { width, height } => {
                tracing::info!("canvas {width}x{height}");
                if let Some(r) = &mut self.renderer {
                    r.set_canvas(width, height);
                }
            }
            Event::Cursor(c) => {
                self.remote_cursor = Some(c.clone());
                if !self.local_cursor {
                    self.set_cursor(el, c);
                }
            },
            Event::Closed(reason) => {
                self.error = reason;
                el.exit();
            }
            Event::Lost(reason) => self.session_ended(el, reason, true),
            // A new render scale: same path as a resize (the host's layout
            // answer makes the session re-offer).
            Event::Reconfigure => self.send_resize(),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let WindowEvent::KeyboardInput { event: ref key, is_synthetic: false, .. } = event {
            let open = self.renderer.as_ref().is_some_and(|r| r.controls.open);
            if let PhysicalKey::Code(code) = key.physical_key
                && let Some(shortcut) = controls::shortcut(code, self.modifiers, open)
                && key.state == ElementState::Pressed
            {
                if !key.repeat {
                    self.release_all();
                    match shortcut {
                        controls::Shortcut::Fullscreen => self.toggle_fullscreen(),
                        controls::Shortcut::Menu => {
                            if let Some(r) = &mut self.renderer {
                                r.controls.open = !r.controls.open;
                            }
                        }
                        controls::Shortcut::Close => {
                            if let Some(r) = &mut self.renderer {
                                r.controls.open = false;
                            }
                        }
                    }
                    self.sync_cursor(el);
                    if let Some(w) = &self.window { w.request_redraw(); }
                }
                return;
            }
        }
        let consumed = match (&mut self.renderer, &self.window) {
            (Some(r), Some(w)) => r.controls.event(w, &event),
            _ => false,
        };
        self.sync_cursor(el);
        if consumed {
            // A drag or shortcut started on the remote desktop must not
            // leave buttons or modifiers held when entering local UI.
            self.release_all();
            return;
        }
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                if self.opts.auto_resize && size.width > 0 && size.height > 0 {
                    self.resize_due = Some(Instant::now() + RESIZE_DEBOUNCE);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                if self.opts.auto_resize {
                    self.resize_due = Some(Instant::now() + RESIZE_DEBOUNCE);
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(r), Some(s)) = (&mut self.renderer, &self.session) {
                    let started = Instant::now();
                    let frames = s.frames().take();
                    let n = frames.len();
                    for (tile, pic) in frames {
                        r.upload(tile, &pic);
                    }
                    if n > 0 {
                        tracing::trace!("uploaded {n} tiles in {:.2} ms", started.elapsed().as_secs_f64() * 1000.0);
                    }
                }
                let scale = self.opts.scale.unwrap_or_else(|| {
                    if self.window.as_ref().is_some_and(|w| w.scale_factor() >= 1.5) { 2.0 } else { 1.0 }
                });
                let op = *self.cfg.op.lock();
                let decoder = self.session.as_ref().map_or("", Session::decoder);
                let decoder = if decoder.is_empty() { "Waiting for video" } else { decoder };
                if let (Some(r), Some(w)) = (&mut self.renderer, &self.window) {
                    let was_open = r.controls.open;
                    let actions = r.render(w, op, &self.status, decoder, scale == 2.0);
                    if !was_open && r.controls.open {
                        self.release_all();
                    }
                    self.apply_controls(el, actions);
                    self.sync_cursor(el);
                    self.ui_due = self.renderer.as_ref().and_then(|r| r.controls.repaint_at);
                }
            }
            WindowEvent::Focused(false) => self.release_all(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => self.key(&event),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some(position);
                self.send_pointer(self.buttons);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let bit = match button {
                    MouseButton::Left => BTN_LEFT,
                    MouseButton::Right => BTN_RIGHT,
                    MouseButton::Middle => BTN_MIDDLE,
                    _ => return,
                };
                match state {
                    ElementState::Pressed => self.buttons |= bit,
                    ElementState::Released => self.buttons &= !bit,
                }
                self.send_pointer(self.buttons);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let points = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64 * SCROLL_POINTS_PER_LINE,
                    // Physical pixels → logical points, so the remote
                    // content tracks the fingers 1:1.
                    MouseScrollDelta::PixelDelta(p) => {
                        p.y / self.window.as_ref().map_or(1.0, |w| w.scale_factor())
                    }
                };
                self.scroll(points);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if let Some(due) = self.retry_due
            && Instant::now() >= due {
                self.retry_due = None;
                tracing::info!("reconnecting");
                self.start_session();
            }
        if self.ui_due.is_some_and(|t| Instant::now() >= t) {
            self.ui_due = None;
            if let Some(w) = &self.window { w.request_redraw(); }
        }
        if self.resize_due.is_some_and(|t| Instant::now() >= t) {
            self.resize_due = None;
            self.send_resize();
        }
        let wake = [self.resize_due, self.retry_due, self.ui_due].into_iter().flatten().min();
        el.set_control_flow(match wake {
            Some(due) => ControlFlow::WaitUntil(due),
            None => ControlFlow::Wait,
        });
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        if let Some(s) = self.session.take() {
            s.shutdown();
        }
    }
}

/// The one event loop shared by the connect dialog and the viewer; winit
/// allows only one per process, so each runs on it in turn.
pub fn event_loop() -> Result<EventLoop<Tagged>> {
    Ok(EventLoop::<Tagged>::with_user_event().build()?)
}

/// Runs the viewer until the window closes or the session ends for good.
pub fn run(event_loop: &mut EventLoop<Tagged>, cfg: Config, opts: Options) -> Result<()> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(
        event_loop.owned_display_handle(),
    )));
    let reconnect = opts.reconnect;
    let mut app = App {
        cfg,
        opts,
        proxy: event_loop.create_proxy(),
        instance,
        window: None,
        renderer: None,
        session: None,
        generation: 0,
        reconnector: Reconnector::new(reconnect),
        retry_due: None,
        error: None,
        last_request: None,
        resize_due: None,
        cursor_pos: None,
        buttons: 0,
        scroll_accum: 0.0,
        modifiers: ModifiersState::empty(),
        pressed: HashMap::new(),
        cursors: HashMap::new(),
        remote_cursor: None,
        local_cursor: false,
        status: "Connecting…".into(),
        ui_due: None,
    };
    event_loop.run_app_on_demand(&mut app)?;
    if let Some(s) = app.session.take() {
        s.shutdown();
    }
    match app.error {
        Some(e) => anyhow::bail!(e),
        None => Ok(()),
    }
}
