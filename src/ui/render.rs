//! Draws the decoded canvas: the decoder's planes uploaded as-is (R8, Rg8
//! or Rgba8) and converted to RGB in a fragment shader, letterboxed into
//! the window.

use std::sync::Arc;

use anyhow::{Context, Result};
use winit::window::Window;

use crate::video::decoder::{Layout, Picture, PlaneRef};
use crate::session::OperatingPoint;
use super::controls::{Actions, Controls};

const SHADER: &str = r#"
@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var u_tex: texture_2d<f32>;
@group(0) @binding(2) var v_tex: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;

// Channel of each texture holding Y, U and V; set per layout. Textures
// shared between components are bound more than once.
override y_ch: u32 = 0u;
override u_ch: u32 = 0u;
override v_ch: u32 = 0u;

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle covering the viewport.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: Out;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(p.x, 1.0 - p.y);
    return o;
}

// BT.709, full range.
fn rgb_at(i: Out) -> vec4<f32> {
    let y = textureSample(y_tex, samp, i.uv)[y_ch];
    let cb = textureSample(u_tex, samp, i.uv)[u_ch] - 0.5;
    let cr = textureSample(v_tex, samp, i.uv)[v_ch] - 0.5;
    let rgb = vec3<f32>(
        y + 1.5748 * cr,
        y - 0.187324 * cb - 0.468124 * cr,
        y + 1.8556 * cb,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

@fragment
fn fs(i: Out) -> @location(0) vec4<f32> {
    return rgb_at(i);
}

// The last picture, dimmed while the session is reconnecting.
@fragment
fn fs_dim(i: Out) -> @location(0) vec4<f32> {
    let c = rgb_at(i);
    return vec4<f32>(c.rgb * 0.35, 1.0);
}
"#;

/// Each layout with the texture channels holding Y, U and V.
const LAYOUTS: [(Layout, [f64; 3]); 3] =
    [(Layout::Planar, [0.0, 0.0, 0.0]), (Layout::SemiPlanar, [0.0, 0.0, 1.0]), (Layout::Vuyx, [2.0, 1.0, 0.0])];

fn layout_index(layout: Layout) -> usize {
    LAYOUTS.iter().position(|(l, _)| *l == layout).unwrap()
}

struct Planes {
    width: u32,
    height: u32,
    layout: Layout,
    /// Texture size of each plane.
    sizes: Vec<(u32, u32)>,
    textures: Vec<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
}

/// Where the canvas lands in the window, in physical pixels.
#[derive(Clone, Copy, Debug, Default)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub struct Renderer {
    pub controls: Controls,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    view_format: wgpu::TextureFormat,
    /// Normal and dimmed pipelines, per entry of LAYOUTS.
    pipelines: Vec<(wgpu::RenderPipeline, wgpu::RenderPipeline)>,
    dimmed: bool,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    planes: Option<Planes>,
    /// Canvas size announced by the host; tiles stack into this.
    canvas: Option<(u32, u32)>,
}

impl Renderer {
    pub fn new(instance: &wgpu::Instance, window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let surface = instance.create_surface(window.clone()).context("creating surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("no GPU adapter for this window")?;
        tracing::info!("GPU: {} ({:?})", adapter.get_info().name, adapter.get_info().backend);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("pippin"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("creating GPU device")?;

        let caps = surface.get_capabilities(&adapter);
        // An 8-bit format; float formats would be treated as linear light.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| {
                matches!(f.remove_srgb_suffix(), wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm)
            })
            .unwrap_or(caps.formats[0]);
        // The shader outputs gamma-encoded RGB already; render through a
        // non-sRGB view so it isn't encoded twice.
        let view_format = format.remove_srgb_suffix();
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: if view_format != format { vec![view_format] } else { vec![] },
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("planes"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let make_pipeline = |fs, ch: [f64; 3]| device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(fs),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(fs),
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &[("y_ch", ch[0]), ("u_ch", ch[1]), ("v_ch", ch[2])],
                    ..Default::default()
                },
                targets: &[Some(view_format.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
        let pipelines = LAYOUTS.iter().map(|&(_, ch)| (make_pipeline("fs", ch), make_pipeline("fs_dim", ch))).collect();
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("planes"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let controls = Controls::new(&window, &device, view_format);
        Ok(Self {
            controls,
            surface,
            device,
            queue,
            config,
            view_format,
            pipelines,
            dimmed: false,
            layout,
            sampler,
            planes: None,
            canvas: None,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn set_canvas(&mut self, width: u32, height: u32) {
        self.canvas = Some((width, height));
        if self.planes.as_ref().is_some_and(|p| p.width != width || p.height != height) {
            // Recreated at the new size by the next picture.
            self.planes = None;
        }
    }

    /// Darkens the canvas, e.g. while the session is down.
    pub fn set_dimmed(&mut self, dimmed: bool) {
        self.dimmed = dimmed;
    }

    /// Size of the drawn image in stream pixels (the pointer coordinate space).
    pub fn image_size(&self) -> Option<(u32, u32)> {
        self.planes.as_ref().map(|p| (p.width, p.height)).or(self.canvas)
    }

    fn make_planes(&self, width: u32, height: u32, layout: Layout, sizes: Vec<(u32, u32)>) -> Planes {
        let textures: Vec<_> = sizes
            .iter()
            .enumerate()
            .map(|(i, &(w, h))| {
                let (format, fill): (_, &[u8]) = match (layout, i) {
                    (Layout::Vuyx, _) => (wgpu::TextureFormat::Rgba8Unorm, &[0x80, 0x80, 0x00, 0xff]),
                    (_, 0) => (wgpu::TextureFormat::R8Unorm, &[0x00]),
                    (Layout::SemiPlanar, _) => (wgpu::TextureFormat::Rg8Unorm, &[0x80, 0x80]),
                    _ => (wgpu::TextureFormat::R8Unorm, &[0x80]),
                };
                let t = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("plane"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                // Black until the first picture for each tile arrives.
                self.queue.write_texture(
                    t.as_image_copy(),
                    &fill.repeat((w * h) as usize),
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(w * fill.len() as u32),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                );
                t
            })
            .collect();
        let views: Vec<_> = textures.iter().map(|t| t.create_view(&Default::default())).collect();
        // Y, U, V bindings: a plane holding several components is bound to each.
        let view = |b: usize| wgpu::BindingResource::TextureView(&views[b.min(views.len() - 1)]);
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("planes"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: view(0) },
                wgpu::BindGroupEntry { binding: 1, resource: view(1) },
                wgpu::BindGroupEntry { binding: 2, resource: view(2) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        Planes { width, height, layout, sizes, textures, bind_group }
    }

    /// Copies one tile's picture into the canvas textures.
    pub fn upload(&mut self, tile: usize, pic: &Picture) {
        if pic.width == 0 || pic.height == 0 {
            return;
        }
        let tile = tile as u32;
        let height = match self.canvas {
            Some((_, h)) => h,
            None => pic.height * (tile + 1),
        };
        let n = pic.layout.planes();
        // Each plane's tiles stack to the canvas height, scaled by subsampling.
        let tex_h = |p: PlaneRef| (height as u64 * p.height as u64).div_ceil(pic.height as u64) as u32;
        let fits = self.planes.as_ref().is_some_and(|p| {
            p.width == pic.width
                && p.height == height
                && p.layout == pic.layout
                && (0..n).all(|i| p.sizes[i] == (pic.plane(i).width, tex_h(pic.plane(i))))
        });
        if !fits {
            let sizes: Vec<_> = (0..n).map(|i| (pic.plane(i).width, tex_h(pic.plane(i)))).collect();
            tracing::debug!("canvas textures {}x{}, {:?} {sizes:?}", pic.width, height, pic.layout);
            self.planes = Some(self.make_planes(pic.width, height, pic.layout, sizes));
        }
        let planes = self.planes.as_ref().unwrap();
        for (i, t) in planes.textures.iter().enumerate() {
            let plane = pic.plane(i);
            let (w, h, tex_h) = (plane.width, plane.height, planes.sizes[i].1);
            let y0 = tile * h;
            if y0 >= tex_h {
                continue;
            }
            let rows = h.min(tex_h - y0);
            let row_bytes = w * pic.layout.texel_bytes(i);
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: t,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: y0, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &plane.data[..((rows - 1) * plane.stride + row_bytes) as usize],
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(plane.stride), rows_per_image: None },
                wgpu::Extent3d { width: w, height: rows, depth_or_array_layers: 1 },
            );
        }
    }

    /// The letterboxed rectangle the canvas occupies.
    pub fn viewport(&self) -> Option<Viewport> {
        let (iw, ih) = self.image_size()?;
        let (ww, wh) = (self.config.width as f32, self.config.height as f32);
        let s = (ww / iw as f32).min(wh / ih as f32);
        let (w, h) = (iw as f32 * s, ih as f32 * s);
        Some(Viewport { x: ((ww - w) / 2.0).floor(), y: ((wh - h) / 2.0).floor(), w, h })
    }

    pub fn render(&mut self, window: &Window, op: OperatingPoint, status: &str, decoder: &str, half_supported: bool) -> Actions {
        let (mut out, actions) = self.controls.ui(window, op, status, decoder, half_supported);
        let jobs = self.controls.ctx.tessellate(out.shapes, out.pixels_per_point);
        for (id, deltas) in out.textures_delta.set.drain() {
            for delta in deltas {
                self.controls.gpu.update_texture(&self.device, &self.queue, id, &delta);
            }
        }
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: out.pixels_per_point,
        };
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                window.request_redraw();
                for id in out.textures_delta.free.drain() {
                    self.controls.gpu.free_texture(&id);
                }
                return actions;
            }
            _ => {
                for id in out.textures_delta.free.drain() {
                    self.controls.gpu.free_texture(&id);
                }
                return actions;
            },
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("canvas"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            if let (Some(p), Some(vp)) = (&self.planes, self.viewport())
                && vp.w >= 1.0 && vp.h >= 1.0 {
                    pass.set_viewport(vp.x, vp.y, vp.w, vp.h, 0.0, 1.0);
                    let (normal, dim) = &self.pipelines[layout_index(p.layout)];
                    pass.set_pipeline(if self.dimmed { dim } else { normal });
                    pass.set_bind_group(0, &p.bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
        }
        let extra = self.controls.gpu.update_buffers(&self.device, &self.queue, &mut enc, &jobs, &screen);
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("session controls"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, depth_slice: None, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            }).forget_lifetime();
            self.controls.gpu.render(&mut pass, &jobs, &screen);
        }
        self.queue.submit(extra.into_iter().chain([enc.finish()]));
        self.queue.present(frame);
        for id in out.textures_delta.free.drain() {
            self.controls.gpu.free_texture(&id);
        }
        actions
    }
}
