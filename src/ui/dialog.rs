//! The connect dialog: a small egui window asking for host, user and
//! password when there is no terminal to prompt on. It runs on the same
//! winit event loop as the viewer, before (and between) sessions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::window::{Window, WindowId};

use super::Tagged;

/// What the dialog edits; also what it returns on Connect.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Fields {
    pub host: String,
    pub user: String,
    pub password: String,
    /// "Save password in keyring"; opt-in, so off unless the caller says.
    pub save: bool,
}

// By hand so the password never reaches a log line.
impl std::fmt::Debug for Fields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fields")
            .field("host", &self.host)
            .field("user", &self.user)
            .field("password", &if self.password.is_empty() { "" } else { "<redacted>" })
            .field("save", &self.save)
            .finish()
    }
}

impl Fields {
    /// Ready to connect; an empty password is fine when one is saved.
    fn complete(&self, saved: bool) -> bool {
        !self.host.trim().is_empty() && !self.user.trim().is_empty() && (saved || !self.password.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_password() {
        let f = Fields { host: "mac".into(), user: "alice".into(), password: "hunter2".into(), save: false };
        let s = format!("{f:?}");
        assert!(!s.contains("hunter2") && s.contains("alice"), "{s}");
    }
}

struct Gpu {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    view_format: wgpu::TextureFormat,
    egui: egui_wgpu::Renderer,
    state: egui_winit::State,
}

struct Dialog {
    instance: wgpu::Instance,
    ctx: egui::Context,
    gpu: Option<Gpu>,
    fields: Fields,
    error: Option<String>,
    focused: bool,
    result: Option<Fields>,
    failure: Option<anyhow::Error>,
    repaint_at: Option<Instant>,
    /// Asks whether a password is saved for (host, user).
    has_saved: Box<dyn Fn(&str, &str) -> bool>,
    /// Answer for the host and user as of the last lookup. Refreshed when
    /// those fields lose focus, not per frame, since it's a D-Bus call.
    saved: bool,
    /// Whether the keyring works; the save checkbox is hidden without one.
    keyring: bool,
}

impl Dialog {
    fn refresh_saved(&mut self) {
        let (h, u) = (self.fields.host.trim(), self.fields.user.trim());
        self.saved = !h.is_empty() && !u.is_empty() && (self.has_saved)(h, u);
    }

    fn create(&mut self, el: &ActiveEventLoop) -> Result<Gpu> {
        let attrs = super::app_window_attributes()
            .with_title("Connect — pippin")
            .with_inner_size(LogicalSize::new(400.0, if self.error.is_some() { 250.0 } else { 220.0 }))
            .with_resizable(false);
        let window = Arc::new(el.create_window(attrs).context("creating dialog window")?);
        let size = window.inner_size();
        let surface = self.instance.create_surface(window.clone()).context("creating surface")?;
        let adapter = pollster::block_on(self.instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("no GPU adapter for the dialog")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("pippin dialog"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("creating GPU device")?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| {
                matches!(f.remove_srgb_suffix(), wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm)
            })
            .or_else(|| caps.formats.first().copied())
            .context("the dialog surface supports no formats")?;
        // egui blends in gamma space and wants a non-sRGB target.
        let view_format = format.remove_srgb_suffix();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: *caps.alpha_modes.first().context("the dialog surface supports no alpha modes")?,
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: if view_format != format { vec![view_format] } else { vec![] },
        };
        surface.configure(&device, &config);
        let egui = egui_wgpu::Renderer::new(&device, view_format, Default::default());
        let state = egui_winit::State::new(
            self.ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(device.limits().max_texture_dimension_2d as usize),
        );
        Ok(Gpu { window, surface, device, queue, config, view_format, egui, state })
    }

    /// Lays out the form; returns `Some(true)` on Connect, `Some(false)` on
    /// Cancel.
    fn ui(&mut self, ui: &mut egui::Ui) -> Option<bool> {
        let mut action = None;
        let (enter, esc) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(4.0);
            ui.heading("Connect to a Mac");
            ui.add_space(8.0);
            let focus_first = !self.focused;
            let f = &mut self.fields;
            let mut first_empty = None;
            let mut target_changed = false;
            egui::Grid::new("fields").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                for (label, value, secret) in
                    [("Host", &mut f.host, false), ("User", &mut f.user, false), ("Password", &mut f.password, true)]
                {
                    ui.label(label);
                    let empty = value.is_empty();
                    let r = ui.add(egui::TextEdit::singleline(value).password(secret).desired_width(260.0));
                    if empty && first_empty.is_none() {
                        first_empty = Some(r.id);
                    }
                    target_changed |= !secret && r.lost_focus();
                    ui.end_row();
                }
            });
            if focus_first {
                if let Some(id) = first_empty {
                    ui.memory_mut(|m| m.request_focus(id));
                }
                self.focused = true;
            }
            ui.add_space(4.0);
            if self.saved && f.password.is_empty() {
                ui.weak("Saved password will be used");
            }
            if self.keyring {
                ui.checkbox(&mut f.save, "Save password in keyring");
            }
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(8.0);
            let ready = f.complete(self.saved);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if ui.add_enabled(ready, egui::Button::new("Connect")).clicked() || (enter && ready) {
                    action = Some(true);
                }
                if ui.button("Cancel").clicked() || esc {
                    action = Some(false);
                }
            });
            if target_changed {
                self.refresh_saved();
            }
        });
        action
    }

    fn redraw(&mut self, el: &ActiveEventLoop) {
        let Some(mut gpu) = self.gpu.take() else { return };
        let raw = gpu.state.take_egui_input(&gpu.window);
        let ctx = self.ctx.clone();
        let mut action = None;
        let mut out = ctx.run_ui(raw, |ui| action = action.or(self.ui(ui)));
        gpu.state.handle_platform_output(&gpu.window, out.platform_output);
        let delay = out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.repaint_delay);
        self.repaint_at = delay.filter(|d| *d < Duration::from_secs(3600)).map(|d| Instant::now() + d);
        let jobs = ctx.tessellate(out.shapes, out.pixels_per_point);
        for (id, deltas) in out.textures_delta.set.drain() {
            for delta in deltas {
                gpu.egui.update_texture(&gpu.device, &gpu.queue, id, &delta);
            }
        }
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [gpu.config.width, gpu.config.height],
            pixels_per_point: out.pixels_per_point,
        };
        match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor { format: Some(gpu.view_format), ..Default::default() });
                let mut enc = gpu.device.create_command_encoder(&Default::default());
                let extra = gpu.egui.update_buffers(&gpu.device, &gpu.queue, &mut enc, &jobs, &screen);
                {
                    let mut pass = enc
                        .begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("dialog"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                depth_slice: None,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    // The central panel paints the whole window.
                                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            ..Default::default()
                        })
                        .forget_lifetime();
                    gpu.egui.render(&mut pass, &jobs, &screen);
                }
                gpu.queue.submit(extra.into_iter().chain([enc.finish()]));
                gpu.queue.present(frame);
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                gpu.window.request_redraw();
            }
            _ => {}
        }
        for id in out.textures_delta.free.drain() {
            gpu.egui.free_texture(&id);
        }
        self.gpu = Some(gpu);
        match action {
            Some(true) => {
                self.result = Some(self.fields.clone());
                el.exit();
            }
            Some(false) => el.exit(),
            None => {}
        }
    }
}

impl ApplicationHandler<Tagged> for Dialog {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        match self.create(el) {
            Ok(g) => {
                g.window.request_redraw();
                self.gpu = Some(g);
            }
            Err(e) => {
                self.failure = Some(e);
                el.exit();
            }
        }
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _event: Tagged) {}

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gpu) = &mut self.gpu else { return };
        let resp = gpu.state.on_window_event(&gpu.window, &event);
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                gpu.config.width = size.width;
                gpu.config.height = size.height;
                gpu.surface.configure(&gpu.device, &gpu.config);
                gpu.window.request_redraw();
            }
            WindowEvent::RedrawRequested => return self.redraw(el),
            _ => {}
        }
        if resp.repaint {
            gpu.window.request_redraw();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        match self.repaint_at {
            Some(t) if Instant::now() >= t => {
                self.repaint_at = None;
                if let Some(g) = &self.gpu {
                    g.window.request_redraw();
                }
                el.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => el.set_control_flow(ControlFlow::WaitUntil(t)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Shows the dialog until Connect (returns the fields) or Cancel / close
/// (returns `None`). `error` is shown under the fields, e.g. after the host
/// rejected the previous attempt.
/// `has_saved` tells whether a password is stored for a host and user; an
/// empty password field may then be submitted. `keyring` false hides the
/// save checkbox.
pub fn run(
    el: &mut EventLoop<Tagged>,
    fields: Fields,
    error: Option<String>,
    keyring: bool,
    has_saved: impl Fn(&str, &str) -> bool + 'static,
) -> Result<Option<Fields>> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(
        el.owned_display_handle(),
    )));
    let mut app = Dialog {
        instance,
        ctx: egui::Context::default(),
        gpu: None,
        fields,
        error,
        focused: false,
        result: None,
        failure: None,
        repaint_at: None,
        has_saved: Box::new(has_saved),
        saved: false,
        keyring,
    };
    app.refresh_saved();
    el.run_app_on_demand(&mut app)?;
    if let Some(e) = app.failure {
        return Err(e);
    }
    Ok(app.result.map(|mut f| {
        f.host = f.host.trim().to_owned();
        f.user = f.user.trim().to_owned();
        f
    }))
}
