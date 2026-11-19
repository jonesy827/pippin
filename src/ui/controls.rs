//! The session toolbar: a small bar of icon buttons composited over the
//! remote desktop. It rests as a thin handle, expands while the pointer is
//! over it, and can be dragged anywhere in the window.
//!
//! Only the pointer is ever local, and only while it is over the toolbar (or
//! dragging it). The keyboard always belongs to the Mac, apart from the
//! shortcuts in [`shortcut`].

use std::f32::consts::{FRAC_PI_2, TAU};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align2, Color32, Id, Painter, Pos2, Rect, Response, Sense, Stroke, Ui, pos2, vec2};
use winit::event::WindowEvent;
use winit::keyboard::{KeyCode, ModifiersState};
use winit::window::Window;

/// How long the toolbar stays expanded on connect, so it can be noticed.
const INTRO: Duration = Duration::from_secs(4);
/// How long it stays expanded after the pointer leaves.
const LINGER: Duration = Duration::from_millis(700);
/// Gap from the window edge for the default position.
const EDGE: f32 = 8.0;
const BUTTON: f32 = 28.0;
/// Hit area of the resting handle.
const HANDLE: egui::Vec2 = vec2(56.0, 16.0);

#[derive(Default)]
pub(super) struct Actions {
    pub fullscreen: bool,
    pub restart: bool,
    pub render_scale: Option<f64>,
}

/// What the toolbar shows; owned by the app.
pub(super) struct View<'a> {
    pub status: &'a str,
    pub connected: bool,
    pub decoder: &'a str,
    pub half: bool,
    /// The window has a 2× backing scale, so half resolution keeps the Mac's
    /// layout; at 1× the Mac's desktop gets larger instead.
    pub retina: bool,
    pub fullscreen: bool,
}

pub(super) struct Controls {
    pub ctx: egui::Context,
    pub state: egui_winit::State,
    pub gpu: egui_wgpu::Renderer,
    /// Hidden entirely with Ctrl+Alt+M.
    pub visible: bool,
    pub repaint_at: Option<Instant>,
    /// Toolbar top-center as a fraction of the window, so it keeps its place
    /// across resizes and fullscreen.
    anchor: Pos2,
    /// What was drawn last frame: the handle or the expanded bar.
    rect: Rect,
    /// Expanded size, for keeping the bar on screen while it rests.
    bar_size: egui::Vec2,
    expanded_until: Instant,
    dragging: bool,
    /// The info card was pinned open by clicking ⓘ.
    info_open: bool,
    /// A remote button is held: the pointer stays with the Mac until it is
    /// released, even when it crosses the toolbar.
    remote_busy: bool,
    pointer: Option<Pos2>,
    #[cfg(test)]
    buttons: std::collections::HashMap<&'static str, Rect>,
}

impl Controls {
    pub fn new(window: &Arc<Window>, device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|s| {
            s.interaction.tooltip_delay = 0.25;
            // A resting hand on a mouse or trackpad is rarely perfectly still.
            s.interaction.show_tooltips_only_when_still = false;
        });
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(device.limits().max_texture_dimension_2d as usize),
        );
        let gpu = egui_wgpu::Renderer::new(device, format, Default::default());
        Self {
            ctx,
            state,
            gpu,
            visible: true,
            repaint_at: None,
            anchor: pos2(0.5, 0.0),
            rect: Rect::NOTHING,
            bar_size: vec2(150.0, BUTTON + 8.0),
            expanded_until: Instant::now() + INTRO,
            dragging: false,
            info_open: false,
            remote_busy: false,
            pointer: None,
            #[cfg(test)]
            buttons: Default::default(),
        }
    }

    /// The pointer currently drives the toolbar rather than the Mac.
    pub fn local_pointer(&self) -> bool {
        self.visible
            && (self.dragging || (!self.remote_busy && self.pointer.is_some_and(|p| self.rect.contains(p))))
    }

    /// Feeds a window event to the toolbar. Returns true if it was the
    /// toolbar's and must not reach the Mac. `remote_busy`: a mouse button
    /// is held on the remote desktop.
    pub fn event(&mut self, window: &Window, event: &WindowEvent, remote_busy: bool) -> bool {
        // Keys always go to the Mac, so egui never sees them. Nor does it see
        // redraws: it answers each one with "repaint", which would request
        // the next redraw and spin the event loop at full speed.
        if matches!(
            event,
            WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_) | WindowEvent::RedrawRequested
        ) {
            return false;
        }
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let pixels_per_point = egui_winit::pixels_per_point(&self.ctx, window);
                self.pointer = Some(pos2(
                    position.x as f32 / pixels_per_point,
                    position.y as f32 / pixels_per_point,
                ));
            }
            WindowEvent::CursorLeft { .. } => self.pointer = None,
            _ => {}
        }
        // Decided before egui sees a press, so a click that starts on the
        // Mac stays there, and one that starts on the toolbar stays local.
        self.remote_busy = remote_busy && !self.dragging;
        let local = self.local_pointer();
        let response = self.state.on_window_event(window, event);
        if response.repaint || (self.visible && matches!(event, WindowEvent::CursorMoved { .. })) {
            window.request_redraw();
        }
        match event {
            WindowEvent::CursorMoved { .. } | WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } => local,
            _ => false,
        }
    }

    pub fn ui(&mut self, window: &Window, view: View) -> (egui::FullOutput, Actions) {
        let mut actions = Actions::default();
        let raw = self.state.take_egui_input(window);
        let ctx = self.ctx.clone();
        let now = Instant::now();
        let mut out = ctx.run_ui(raw, |_ui| {
            if !self.visible {
                self.rect = Rect::NOTHING;
                self.dragging = false;
                self.info_open = false;
                return;
            }
            let screen = ctx.content_rect();
            let hover = ctx.input(|i| i.pointer.hover_pos());
            let over = !self.remote_busy && hover.is_some_and(|p| self.rect.expand(4.0).contains(p));
            // A click anywhere else, including on the Mac, closes the card.
            if self.info_open && !over && ctx.input(|i| i.pointer.any_pressed()) {
                self.info_open = false;
            }
            if self.dragging || self.info_open || over {
                self.expanded_until = self.expanded_until.max(now + LINGER);
            }
            let expanded = now < self.expanded_until;
            let size = if expanded { self.bar_size } else { HANDLE };
            let pivot = self.pivot(screen, size);
            let area = egui::Area::new(Id::new("toolbar"))
                .order(egui::Order::Foreground)
                .pivot(Align2::CENTER_TOP)
                .fixed_pos(pivot)
                .constrain(false)
                .sense(Sense::drag())
                .show(&ctx, |ui| {
                    if expanded {
                        self.bar(ui, &view, &mut actions);
                    } else {
                        handle(ui);
                    }
                });
            let drag = area.response.on_hover_cursor(egui::CursorIcon::Grab);
            self.dragging = drag.dragged();
            self.rect = drag.rect;
            if self.dragging {
                ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                let moved = pivot + drag.drag_delta();
                self.anchor = pos2(
                    (moved.x - screen.left()) / screen.width().max(1.0),
                    (moved.y - screen.top()) / screen.height().max(1.0),
                );
            }
            if expanded {
                self.bar_size = self.rect.size();
            }
        });
        // Nothing here takes keyboard input; drop focus so no ring is drawn.
        ctx.memory_mut(|memory| {
            if let Some(id) = memory.focused() {
                memory.surrender_focus(id);
            }
        });
        let delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay);
        // Collapse on time, and keep the status tooltip fresh while expanded.
        let collapse = self
            .expanded_until
            .checked_duration_since(now)
            .filter(|_| self.visible)
            .map(|d| d.min(Duration::from_millis(250)));
        let delay = match (delay, collapse) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.repaint_at = delay
            .filter(|d| *d < Duration::from_secs(3600))
            .map(|d| now + d);
        // The remote cursor owns the window outside the toolbar.
        if self.local_pointer() {
            self.state
                .handle_platform_output(window, std::mem::take(&mut out.platform_output));
        }
        (out, actions)
    }

    /// Top-center of a toolbar of `size`, kept fully inside the window.
    fn pivot(&self, screen: Rect, size: egui::Vec2) -> Pos2 {
        // Clamp with the larger of both sizes so expanding never pushes the
        // bar off screen and resting never jumps the handle.
        let w = size.x.max(self.bar_size.x) / 2.0;
        let h = size.y.max(self.bar_size.y);
        let x = screen.left() + self.anchor.x * screen.width();
        let y = screen.top() + self.anchor.y * screen.height();
        pos2(
            x.clamp(screen.left() + w + EDGE, (screen.right() - w - EDGE).max(screen.left() + w + EDGE)),
            y.clamp(screen.top() + EDGE, (screen.bottom() - h - EDGE).max(screen.top() + EDGE)),
        )
    }

    fn bar(&mut self, ui: &mut Ui, view: &View, actions: &mut Actions) {
        egui::Frame::popup(ui.style())
            .inner_margin(4.0)
            .corner_radius(10.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(2.0, 0.0);
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(14.0, BUTTON), Sense::hover());
                    grip(ui.painter(), rect, ui.visuals().weak_text_color());

                    let tip = if view.retina {
                        "Half resolution\nStreams a quarter of the pixels: less bandwidth, softer text."
                    } else {
                        "Half resolution\nStreams a quarter of the pixels: less bandwidth, softer text.\nOn this screen the Mac's desktop also appears twice as large."
                    };
                    if self.button(ui, "half", view.half, tip, |p, r, c| {
                        p.text(r.center(), Align2::CENTER_CENTER, "½", egui::FontId::proportional(17.0), c);
                    })
                    .clicked()
                    {
                        actions.render_scale = Some(if view.half { 1.0 } else { 0.5 });
                    }

                    let fs = view.fullscreen;
                    let tip = if fs { "Exit fullscreen  (Ctrl+Alt+Enter)" } else { "Fullscreen  (Ctrl+Alt+Enter)" };
                    if self.button(ui, "fullscreen", false, tip, |p, r, c| fullscreen(p, r, c, fs)).clicked() {
                        actions.fullscreen = true;
                    }

                    if self.button(ui, "restart", false, "Restart session", restart).clicked() {
                        actions.restart = true;
                    }

                    let info_color = (!view.connected).then_some(Color32::from_rgb(0xe0, 0xa0, 0x30));
                    let open = self.info_open;
                    let r = self.button(ui, "info", open, "", |p, r, c| info(p, r, info_color.unwrap_or(c)));
                    if r.clicked() {
                        self.info_open = !open;
                    }
                    if self.info_open {
                        // Pinned: shown below the button until closed.
                        egui::Area::new(Id::new("toolbar-info"))
                            .order(egui::Order::Tooltip)
                            .pivot(Align2::RIGHT_TOP)
                            .fixed_pos(r.rect.right_bottom() + vec2(4.0, 8.0))
                            .interactable(false)
                            .show(ui.ctx(), |ui| {
                                egui::Frame::popup(ui.style()).show(ui, |ui| info_tip(ui, view));
                            });
                    } else {
                        r.on_hover_ui(|ui| info_tip(ui, view));
                    }
                });
            });
    }

    fn button(
        &mut self,
        ui: &mut Ui,
        _name: &'static str,
        selected: bool,
        tip: &str,
        icon: impl FnOnce(&Painter, Rect, Color32),
    ) -> Response {
        let (rect, response) = ui.allocate_exact_size(vec2(BUTTON, BUTTON), Sense::click());
        #[cfg(test)]
        self.buttons.insert(_name, rect);
        let visuals = ui.style().interact_selectable(&response, selected);
        if selected || response.hovered() || response.is_pointer_button_down_on() {
            ui.painter().rect_filled(rect, 6.0, visuals.weak_bg_fill);
        }
        icon(ui.painter(), rect.shrink(7.0), visuals.fg_stroke.color);
        if tip.is_empty() { response } else { response.on_hover_text(tip) }
    }
}

/// The resting toolbar: a short bar that expands when the pointer reaches it.
fn handle(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(HANDLE, Sense::hover());
    let bar = Rect::from_center_size(pos2(rect.center().x, rect.top() + 5.0), vec2(40.0, 5.0));
    ui.painter().rect_filled(bar.expand(1.0), 4.0, Color32::from_black_alpha(90));
    ui.painter().rect_filled(bar, 3.0, Color32::from_white_alpha(150));
}

fn info_tip(ui: &mut Ui, view: &View) {
    ui.set_max_width(260.0);
    if view.connected {
        ui.label(view.status);
    } else {
        ui.colored_label(Color32::from_rgb(0xe0, 0xa0, 0x30), view.status);
    }
    ui.weak(format!("Decoder: {}", view.decoder));
    ui.separator();
    egui::Grid::new("shortcuts").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
        ui.strong("Ctrl+Alt+Enter");
        ui.label("Fullscreen");
        ui.end_row();
        ui.strong("Ctrl+Alt+M");
        ui.label("Hide or show this toolbar");
        ui.end_row();
        ui.strong("Drag");
        ui.label("Move this toolbar");
        ui.end_row();
    });
}

fn grip(p: &Painter, r: Rect, c: Color32) {
    for dx in [-2.5, 2.5] {
        for dy in [-5.0, 0.0, 5.0] {
            p.circle_filled(r.center() + vec2(dx, dy), 1.3, c);
        }
    }
}

/// Corner brackets: pointing out to enter fullscreen, in to leave it.
fn fullscreen(p: &Painter, r: Rect, c: Color32, leave: bool) {
    let stroke = Stroke::new(1.6, c);
    let len = 4.5;
    for (corner, dir) in [
        (r.left_top(), vec2(1.0, 1.0)),
        (r.right_top(), vec2(-1.0, 1.0)),
        (r.left_bottom(), vec2(1.0, -1.0)),
        (r.right_bottom(), vec2(-1.0, -1.0)),
    ] {
        if leave {
            let q = corner + dir * len;
            p.line_segment([q, q - vec2(dir.x * len, 0.0)], stroke);
            p.line_segment([q, q - vec2(0.0, dir.y * len)], stroke);
        } else {
            p.line_segment([corner, corner + vec2(dir.x * len, 0.0)], stroke);
            p.line_segment([corner, corner + vec2(0.0, dir.y * len)], stroke);
        }
    }
}

/// A clockwise arrow around a circle.
fn restart(p: &Painter, r: Rect, c: Color32) {
    let center = r.center();
    let radius = r.width() / 2.0;
    let start = -FRAC_PI_2 + 0.7;
    let sweep = TAU - 1.4;
    let at = |a: f32| center + radius * vec2(a.cos(), a.sin());
    let points = (0..=24).map(|i| at(start + sweep * i as f32 / 24.0)).collect();
    p.add(egui::Shape::line(points, Stroke::new(1.6, c)));
    let end = start + sweep;
    let tangent = vec2(-end.sin(), end.cos());
    let normal = vec2(end.cos(), end.sin());
    let tip = at(end) + tangent * 3.5;
    p.add(egui::Shape::convex_polygon(
        vec![tip, at(end) + normal * 3.0, at(end) - normal * 3.0],
        c,
        Stroke::NONE,
    ));
}

fn info(p: &Painter, r: Rect, c: Color32) {
    let center = r.center();
    p.circle_stroke(center, r.width() / 2.0, Stroke::new(1.4, c));
    p.circle_filled(center + vec2(0.0, -3.2), 1.1, c);
    p.line_segment([center + vec2(0.0, -0.8), center + vec2(0.0, 3.6)], Stroke::new(1.6, c));
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Shortcut {
    Fullscreen,
    Toolbar,
}

pub(super) fn shortcut(code: KeyCode, modifiers: ModifiersState) -> Option<Shortcut> {
    if modifiers != (ModifiersState::CONTROL | ModifiersState::ALT) {
        return None;
    }
    match code {
        KeyCode::Enter => Some(Shortcut::Fullscreen),
        KeyCode::KeyM => Some(Shortcut::Toolbar),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_always_goes_to_the_mac() {
        assert_eq!(shortcut(KeyCode::Escape, ModifiersState::empty()), None);
    }

    #[test]
    fn local_shortcuts_require_exact_modifiers() {
        let mods = ModifiersState::CONTROL | ModifiersState::ALT;
        assert_eq!(shortcut(KeyCode::Enter, mods), Some(Shortcut::Fullscreen));
        assert_eq!(shortcut(KeyCode::KeyM, mods), Some(Shortcut::Toolbar));
        for mods in [
            ModifiersState::empty(),
            ModifiersState::CONTROL,
            mods | ModifiersState::SHIFT,
            mods | ModifiersState::SUPER,
        ] {
            assert_eq!(shortcut(KeyCode::Enter, mods), None);
            assert_eq!(shortcut(KeyCode::KeyM, mods), None);
        }
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use crate::ui::render::Renderer;
    use winit::application::ApplicationHandler;
    use winit::event::{DeviceId, ElementState, Ime, MouseButton, MouseScrollDelta, TouchPhase};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::platform::x11::EventLoopBuilderExtX11;
    use winit::window::WindowId;

    fn view(retina: bool) -> View<'static> {
        View {
            status: "Connected",
            connected: true,
            decoder: "software",
            half: false,
            retina,
            fullscreen: false,
        }
    }

    /// Exercises the actual wgpu composition and egui input handling without
    /// a Mac. Run in an isolated X server: xvfb-run cargo test viewer_controls
    /// -- --ignored --nocapture (requires an X11/Vulkan or GL renderer).
    /// PIPPIN_SMOKE_HOVER=1 leaves the toolbar expanded for a screenshot.
    #[test]
    #[ignore = "requires an X11 display and GPU or software graphics driver"]
    fn viewer_controls_render_and_capture_input() {
        struct Smoke {
            until: Instant,
            graphics: Option<(Arc<Window>, Renderer)>,
        }
        fn moved(x: f64, y: f64) -> WindowEvent {
            WindowEvent::CursorMoved {
                device_id: DeviceId::dummy(),
                position: winit::dpi::PhysicalPosition::new(x, y),
            }
        }
        fn click(r: &mut Renderer, w: &Window, pos: Pos2) -> Actions {
            let mut actions = Actions::default();
            for pressed in [true, false] {
                r.controls.state.egui_input_mut().events.extend([
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
                let a = r.render(w, view(true));
                actions.restart |= a.restart;
                actions.fullscreen |= a.fullscreen;
                actions.render_scale = actions.render_scale.or(a.render_scale);
            }
            actions
        }
        impl ApplicationHandler for Smoke {
            fn resumed(&mut self, el: &ActiveEventLoop) {
                let window = Arc::new(
                    el.create_window(
                        Window::default_attributes()
                            .with_title("pippin controls smoke test")
                            .with_inner_size(winit::dpi::LogicalSize::new(900, 600)),
                    )
                    .unwrap(),
                );
                let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(
                    Box::new(el.owned_display_handle()),
                ));
                let mut r = Renderer::new(&instance, window.clone()).unwrap();
                let w = &*window;
                for _ in 0..2 {
                    r.render(w, view(true));
                }
                let restart = r.controls.buttons["restart"].center();
                // Keys and IME always belong to the Mac.
                assert!(!r.controls.event(w, &WindowEvent::Ime(Ime::Commit("a".into())), false));
                // Over the toolbar the pointer is local; elsewhere it's the Mac's.
                assert!(r.controls.event(w, &moved(restart.x as f64, restart.y as f64), false));
                assert!(!r.controls.event(w, &moved(800.0, 500.0), false));
                let scroll = WindowEvent::MouseWheel {
                    device_id: DeviceId::dummy(),
                    delta: MouseScrollDelta::LineDelta(0.0, 1.0),
                    phase: TouchPhase::Moved,
                };
                assert!(!r.controls.event(w, &scroll, false));
                // A drag held on the Mac passes over the toolbar untouched.
                assert!(!r.controls.event(w, &moved(restart.x as f64, restart.y as f64), true));
                let press = WindowEvent::MouseInput {
                    device_id: DeviceId::dummy(),
                    state: ElementState::Released,
                    button: MouseButton::Left,
                };
                assert!(!r.controls.event(w, &press, true));
                let a = click(&mut r, w, restart);
                assert!(a.restart, "clicking restart should restart");
                let half = r.controls.buttons["half"].center();
                let a = click(&mut r, w, half);
                assert_eq!(a.render_scale, Some(0.5));
                // Clicking ⓘ pins the info card; a click on the Mac closes it.
                let info = r.controls.buttons["info"].center();
                click(&mut r, w, info);
                assert!(r.controls.info_open, "clicking info should pin the card");
                click(&mut r, w, pos2(800.0, 500.0));
                assert!(!r.controls.info_open, "a click elsewhere should close the card");
                // Hidden with Ctrl+Alt+M: nothing is local.
                r.controls.visible = false;
                r.render(w, view(true));
                assert!(!r.controls.event(w, &moved(restart.x as f64, restart.y as f64), false));
                r.controls.visible = true;
                if std::env::var_os("PIPPIN_SMOKE_HOVER").is_none() {
                    r.controls.expanded_until = Instant::now();
                }
                self.graphics = Some((window, r));
                eprintln!("controls smoke window ready");
            }
            fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
                if let Some((window, renderer)) = &mut self.graphics
                    && matches!(event, WindowEvent::RedrawRequested)
                {
                    if std::env::var_os("PIPPIN_SMOKE_HOVER").is_some() {
                        renderer.controls.expanded_until = Instant::now() + Duration::from_secs(1);
                    }
                    renderer.render(window, view(true));
                }
            }
            fn about_to_wait(&mut self, el: &ActiveEventLoop) {
                el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(50)));
                if let Some((window, _)) = &self.graphics {
                    window.request_redraw();
                }
                if Instant::now() >= self.until {
                    el.exit();
                }
            }
        }
        let mut events = EventLoop::builder().with_x11().with_any_thread(true).build().unwrap();
        use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
        events
            .run_app_on_demand(&mut Smoke {
                until: Instant::now() + Duration::from_secs(5),
                graphics: None,
            })
            .unwrap();
    }
}
