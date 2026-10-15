//! Local session controls, composited over the remote desktop.

use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::keyboard::{KeyCode, ModifiersState};

use winit::event::WindowEvent;
use winit::window::Window;

use crate::session::{AfterLoss, OperatingPoint};

#[derive(Default)]
pub(super) struct Actions {
    pub fullscreen: bool,
    pub restart: bool,
    pub disconnect: bool,
    pub render_scale: Option<f64>,
    pub after_loss: Option<AfterLoss>,
}

pub(super) struct Controls {
    pub ctx: egui::Context,
    pub state: egui_winit::State,
    pub gpu: egui_wgpu::Renderer,
    pub open: bool,
    pub repaint_at: Option<Instant>,
    button: egui::Rect,
    pointer: Option<egui::Pos2>,
}

impl Controls {
    pub fn new(window: &Arc<Window>, device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let ctx = egui::Context::default();
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
            open: false,
            repaint_at: None,
            button: egui::Rect::NOTHING,
            pointer: None,
        }
    }

    pub fn local_pointer(&self) -> bool {
        self.open || self.pointer.is_some_and(|p| self.button.contains(p))
    }

    pub fn event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let pixels_per_point = egui_winit::pixels_per_point(&self.ctx, window);
                self.pointer = Some(egui::pos2(
                    position.x as f32 / pixels_per_point,
                    position.y as f32 / pixels_per_point,
                ));
            }
            WindowEvent::CursorLeft { .. } => self.pointer = None,
            _ => {}
        }
        let response = self.state.on_window_event(window, event);
        if response.repaint {
            window.request_redraw();
        }
        match event {
            WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_) => self.open,
            WindowEvent::CursorMoved { .. } | WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } => {
                self.local_pointer()
            }
            _ => false,
        }
    }

    pub fn ui(
        &mut self,
        window: &Window,
        op: OperatingPoint,
        status: &str,
        decoder: &str,
        half_supported: bool,
    ) -> (egui::FullOutput, Actions) {
        let mut actions = Actions::default();
        let raw = self.state.take_egui_input(window);
        let ctx = self.ctx.clone();
        let mut out = ctx.run_ui(raw, |_ui| {
            egui::Area::new("controls-toggle".into())
                .anchor(egui::Align2::CENTER_TOP, (0.0, 12.0))
                .show(&ctx, |ui| {
                    let r = ui.add(
                        egui::Button::new("Controls")
                            .selected(self.open)
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                    self.button = r.rect;
                    if r.clicked() {
                        self.open = !self.open;
                    }
                });
            if self.open {
                egui::Window::new("Session controls")
                    .fixed_pos((12.0, 52.0))
                    .resizable(false)
                    .collapsible(false)
                    .show(&ctx, |ui| {
                        ui.label(status);
                        ui.label(format!("Decoder: {decoder}"));
                        ui.separator();
                        ui.label("Stream resolution");
                        ui.horizontal(|ui| {
                            if ui.selectable_label(op.render_scale == 1.0, "Full").clicked() {
                                actions.render_scale = Some(1.0);
                            }
                            if ui
                                .add_enabled(
                                    half_supported || op.render_scale == 0.5,
                                    egui::Button::new("Half").selected(op.render_scale == 0.5),
                                )
                                .clicked()
                            {
                                actions.render_scale = Some(0.5);
                            }
                        });
                        ui.weak("Half resolution uses less bandwidth with softer text.");
                        if !half_supported {
                            ui.weak("Half resolution requires a 2× backing scale.");
                        }
                        ui.add_space(8.0);
                        ui.label("When video packets are lost");
                        ui.horizontal(|ui| {
                            if ui.selectable_label(op.after_loss == AfterLoss::Idr, "Freeze").clicked() {
                                actions.after_loss = Some(AfterLoss::Idr);
                            }
                            if ui
                                .selectable_label(op.after_loss == AfterLoss::Conceal, "Keep moving")
                                .clicked()
                            {
                                actions.after_loss = Some(AfterLoss::Conceal);
                            }
                        });
                        ui.weak("Keep moving may show artifacts until the next keyframe.");
                        ui.separator();
                        if ui
                            .button(if window.fullscreen().is_some() {
                                "Leave fullscreen"
                            } else {
                                "Fullscreen"
                            })
                            .clicked()
                        {
                            actions.fullscreen = true;
                        }
                        ui.horizontal(|ui| {
                            actions.restart = ui.button("Restart session").clicked();
                            actions.disconnect = ui.button("Disconnect").clicked();
                        });
                        ui.separator();
                        ui.weak("Ctrl+Alt+Enter: toggle fullscreen");
                        ui.weak("Ctrl+Alt+M: controls · Esc: close controls");
                        ui.weak("Input stays on this computer while controls are open.");
                    });
            }
        });
        let delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay);
        // Refresh the decoder/status while open, even on an idle remote desktop.
        let delay = if self.open {
            Some(delay.unwrap_or(Duration::MAX).min(Duration::from_millis(250)))
        } else {
            delay
        };
        self.repaint_at = delay
            .filter(|d| *d < Duration::from_secs(3600))
            .map(|d| Instant::now() + d);
        // The remote cursor owns the window outside the local controls.
        if self.local_pointer() {
            self.state
                .handle_platform_output(window, std::mem::take(&mut out.platform_output));
        }
        (out, actions)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Shortcut {
    Fullscreen,
    Menu,
    Close,
}

pub(super) fn shortcut(code: KeyCode, modifiers: ModifiersState, open: bool) -> Option<Shortcut> {
    if code == KeyCode::Escape && open {
        Some(Shortcut::Close)
    } else if modifiers == (ModifiersState::CONTROL | ModifiersState::ALT) {
        match code {
            KeyCode::Enter => Some(Shortcut::Fullscreen),
            KeyCode::KeyM => Some(Shortcut::Menu),
            _ => None,
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_is_remote_unless_controls_are_open() {
        assert_eq!(shortcut(KeyCode::Escape, ModifiersState::empty(), false), None);
        assert_eq!(
            shortcut(KeyCode::Escape, ModifiersState::empty(), true),
            Some(Shortcut::Close)
        );
    }

    #[test]
    fn local_shortcuts_require_exact_modifiers() {
        let mods = ModifiersState::CONTROL | ModifiersState::ALT;
        assert_eq!(shortcut(KeyCode::Enter, mods, false), Some(Shortcut::Fullscreen));
        assert_eq!(shortcut(KeyCode::KeyM, mods, false), Some(Shortcut::Menu));
        for mods in [
            ModifiersState::empty(),
            ModifiersState::CONTROL,
            mods | ModifiersState::SHIFT,
            mods | ModifiersState::SUPER,
        ] {
            assert_eq!(shortcut(KeyCode::Enter, mods, false), None);
            assert_eq!(shortcut(KeyCode::KeyM, mods, false), None);
        }
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;
    use crate::rtp::{assembler::HoldMode, rtcp::Recovery};
    use crate::session::Fps;
    use crate::ui::render::Renderer;
    use winit::application::ApplicationHandler;
    use winit::event::{DeviceId, Ime, MouseScrollDelta, TouchPhase};
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::platform::x11::EventLoopBuilderExtX11;
    use winit::window::WindowId;

    /// Exercises the actual wgpu composition and egui click handling without
    /// a Mac. Run in an isolated X server: xvfb-run cargo test viewer_controls
    /// -- --ignored --nocapture (requires an X11/Vulkan or GL renderer).
    #[test]
    #[ignore = "requires an X11 display and GPU or software graphics driver"]
    fn viewer_controls_render_and_capture_input() {
        struct Smoke {
            until: Instant,
            graphics: Option<(Arc<Window>, Renderer)>,
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
                let mut renderer = Renderer::new(&instance, window.clone()).unwrap();
                let op = OperatingPoint {
                    render_scale: 1.0,
                    fps: Fps::Sixty,
                    recovery: Recovery::Fir,
                    hold: HoldMode::Auto,
                    ltr: false,
                    after_loss: AfterLoss::Idr,
                };
                for _ in 0..2 {
                    renderer.render(&window, op, "Connected", "software", true);
                }
                let pos = renderer.controls.button.center();
                for pressed in [true, false] {
                    renderer.controls.state.egui_input_mut().events.extend([
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ]);
                    renderer.render(&window, op, "Connected", "software", true);
                }
                assert!(renderer.controls.open, "clicking Controls should open the panel");
                assert!(
                    renderer
                        .controls
                        .event(&window, &WindowEvent::Ime(Ime::Commit("a".into())))
                );
                let scroll = WindowEvent::MouseWheel {
                    device_id: DeviceId::dummy(),
                    delta: MouseScrollDelta::LineDelta(0.0, 1.0),
                    phase: TouchPhase::Moved,
                };
                let movement = WindowEvent::CursorMoved {
                    device_id: DeviceId::dummy(),
                    position: winit::dpi::PhysicalPosition::new(800.0, 500.0),
                };
                assert!(renderer.controls.event(&window, &movement));
                assert!(renderer.controls.event(&window, &scroll));
                renderer.controls.open = false;
                assert!(!renderer.controls.event(&window, &movement));
                assert!(!renderer.controls.event(&window, &scroll));
                assert!(
                    !renderer
                        .controls
                        .event(&window, &WindowEvent::Ime(Ime::Commit("a".into())))
                );
                renderer.controls.open = true;
                renderer.render(&window, op, "Connected", "software", true);
                self.graphics = Some((window, renderer));
                eprintln!("controls smoke window ready");
            }
            fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
                if let Some((window, renderer)) = &mut self.graphics {
                    renderer.controls.event(window, &event);
                    if matches!(event, WindowEvent::RedrawRequested) {
                        let op = OperatingPoint {
                            render_scale: 1.0,
                            fps: Fps::Sixty,
                            recovery: Recovery::Fir,
                            hold: HoldMode::Auto,
                            ltr: false,
                            after_loss: AfterLoss::Idr,
                        };
                        renderer.render(window, op, "Connected", "software", true);
                    }
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
