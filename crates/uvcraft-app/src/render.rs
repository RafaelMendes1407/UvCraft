//! Offscreen wgpu rendering of the 3D and UV viewports. Each view renders
//! into its own multisampled texture, which egui then shows as an image.

use bytemuck::{Pod, Zeroable};
use eframe::egui;
use eframe::egui_wgpu::RenderState;
use eframe::wgpu::{self, util::DeviceExt};
use glam::Mat4;

const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const SAMPLES: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct TriVertex {
    pub pos: [f32; 3],
    pub uv: [f32; 2],
    pub normal: [f32; 3],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct LineVertex {
    pub pos: [f32; 3],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
    params: [f32; 4],
    light: [f32; 4],
}

/// What to draw in one view.
pub struct ViewParams {
    pub view_proj: Mat4,
    pub uv_mode: bool,
    /// 0 = checker only, 1 = vertex color only.
    pub tint: f32,
    pub checker_cells: f32,
    pub light: [f32; 3],
    pub wireframe: bool,
}

struct Target {
    size: [u32; 2],
    msaa: wgpu::TextureView,
    resolve: wgpu::TextureView,
    depth: wgpu::TextureView,
    texture_id: Option<egui::TextureId>,
}

struct View {
    target: Option<Target>,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

struct Mesh {
    buffer: wgpu::Buffer,
    count: u32,
}

pub struct Renderer {
    tri_3d: wgpu::RenderPipeline,
    tri_uv: wgpu::RenderPipeline,
    line_3d: wgpu::RenderPipeline,
    line_uv: wgpu::RenderPipeline,
    views: [View; 2],
    tris: Option<Mesh>,
    /// Interior wireframe (drawn only when enabled).
    wire: Option<Mesh>,
    /// Seams, borders and selected edges (always drawn).
    edges: Option<Mesh>,
    grid: Option<Mesh>,
}

fn pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    lines: bool,
    depth_test: bool,
) -> wgpu::RenderPipeline {
    let tri_attrs =
        wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2, 2 => Float32x3, 3 => Float32x4];
    let line_attrs = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2, 2 => Float32x4];
    let (stride, attrs): (u64, &[wgpu::VertexAttribute]) = if lines {
        (std::mem::size_of::<LineVertex>() as u64, &line_attrs)
    } else {
        (std::mem::size_of::<TriVertex>() as u64, &tri_attrs)
    };
    let buffers = [Some(wgpu::VertexBufferLayout {
        array_stride: stride,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: attrs,
    })];
    let targets = [Some(wgpu::ColorTargetState {
        format: COLOR_FORMAT,
        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
        write_mask: wgpu::ColorWrites::ALL,
    })];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(if lines {
            "uvcraft lines"
        } else {
            "uvcraft tris"
        }),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some(if lines { "vs_line" } else { "vs_tri" }),
            compilation_options: Default::default(),
            buffers: &buffers,
        },
        primitive: wgpu::PrimitiveState {
            topology: if lines {
                wgpu::PrimitiveTopology::LineList
            } else {
                wgpu::PrimitiveTopology::TriangleList
            },
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(depth_test && !lines),
            depth_compare: Some(if depth_test {
                wgpu::CompareFunction::LessEqual
            } else {
                wgpu::CompareFunction::Always
            }),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: wgpu::MultisampleState {
            count: SAMPLES,
            ..Default::default()
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(if lines { "fs_line" } else { "fs_tri" }),
            compilation_options: Default::default(),
            targets: &targets,
        }),
        multiview_mask: None,
        cache: None,
    })
}

impl Renderer {
    pub fn new(rs: &RenderState) -> Self {
        let device = &rs.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("uvcraft shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uvcraft uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("uvcraft layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let view = || {
            let uniform = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("uvcraft uniform"),
                size: std::mem::size_of::<Uniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("uvcraft bind group"),
                layout: &bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                }],
            });
            View {
                target: None,
                uniform,
                bind_group,
            }
        };
        Renderer {
            tri_3d: pipeline(device, &layout, &module, false, true),
            tri_uv: pipeline(device, &layout, &module, false, false),
            line_3d: pipeline(device, &layout, &module, true, true),
            line_uv: pipeline(device, &layout, &module, true, false),
            views: [view(), view()],
            tris: None,
            wire: None,
            edges: None,
            grid: None,
        }
    }

    fn upload<T: Pod>(rs: &RenderState, label: &str, data: &[T]) -> Option<Mesh> {
        (!data.is_empty()).then(|| Mesh {
            buffer: rs
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some(label),
                    contents: bytemuck::cast_slice(data),
                    usage: wgpu::BufferUsages::VERTEX,
                }),
            count: data.len() as u32,
        })
    }

    pub fn set_scene(
        &mut self,
        rs: &RenderState,
        tris: &[TriVertex],
        wire: &[LineVertex],
        edges: &[LineVertex],
        grid: &[LineVertex],
    ) {
        self.tris = Self::upload(rs, "uvcraft tris", tris);
        self.wire = Self::upload(rs, "uvcraft wire", wire);
        self.edges = Self::upload(rs, "uvcraft edges", edges);
        self.grid = Self::upload(rs, "uvcraft grid", grid);
    }

    fn ensure_target(&mut self, rs: &RenderState, view: usize, size: [u32; 2]) {
        let size = [size[0].clamp(1, 8192), size[1].clamp(1, 8192)];
        let v = &mut self.views[view];
        if v.target.as_ref().is_some_and(|t| t.size == size) {
            return;
        }
        let extent = wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        };
        let texture = |label, format, samples, usage| {
            rs.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: extent,
                    mip_level_count: 1,
                    sample_count: samples,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let msaa = texture(
            "uvcraft msaa",
            COLOR_FORMAT,
            SAMPLES,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let resolve = texture(
            "uvcraft view",
            COLOR_FORMAT,
            1,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let depth = texture(
            "uvcraft depth",
            DEPTH_FORMAT,
            SAMPLES,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let mut renderer = rs.renderer.write();
        let texture_id = match v.target.as_ref().and_then(|t| t.texture_id) {
            Some(id) => {
                renderer.update_egui_texture_from_wgpu_texture(
                    &rs.device,
                    &resolve,
                    wgpu::FilterMode::Linear,
                    id,
                );
                id
            }
            None => {
                renderer.register_native_texture(&rs.device, &resolve, wgpu::FilterMode::Linear)
            }
        };
        v.target = Some(Target {
            size,
            msaa,
            resolve,
            depth,
            texture_id: Some(texture_id),
        });
    }

    /// Renders one view (0 = 3D, 1 = UV) at `size` pixels; returns its egui texture.
    pub fn render(
        &mut self,
        rs: &RenderState,
        view: usize,
        size: [u32; 2],
        p: &ViewParams,
    ) -> egui::TextureId {
        self.ensure_target(rs, view, size);
        let v = &self.views[view];
        let target = v.target.as_ref().expect("created above");
        let uniforms = Uniforms {
            view_proj: p.view_proj.to_cols_array_2d(),
            params: [
                f32::from(u8::from(p.uv_mode)),
                p.tint,
                p.checker_cells,
                if p.uv_mode { 0.0 } else { 2e-4 },
            ],
            light: [p.light[0], p.light[1], p.light[2], 0.0],
        };
        rs.queue
            .write_buffer(&v.uniform, 0, bytemuck::bytes_of(&uniforms));

        let mut encoder = rs.device.create_command_encoder(&Default::default());
        {
            let bg = if p.uv_mode {
                wgpu::Color {
                    r: 0.035,
                    g: 0.037,
                    b: 0.042,
                    a: 1.0,
                }
            } else {
                wgpu::Color {
                    r: 0.045,
                    g: 0.05,
                    b: 0.06,
                    a: 1.0,
                }
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("uvcraft view"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.msaa,
                    depth_slice: None,
                    resolve_target: Some(&target.resolve),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(bg),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &target.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &v.bind_group, &[]);
            let draw = |pass: &mut wgpu::RenderPass<'_>,
                        pipe: &wgpu::RenderPipeline,
                        mesh: &Option<Mesh>| {
                if let Some(m) = mesh {
                    pass.set_pipeline(pipe);
                    pass.set_vertex_buffer(0, m.buffer.slice(..));
                    pass.draw(0..m.count, 0..1);
                }
            };
            if p.uv_mode {
                draw(&mut pass, &self.line_uv, &self.grid);
                draw(&mut pass, &self.tri_uv, &self.tris);
                if p.wireframe {
                    draw(&mut pass, &self.line_uv, &self.wire);
                }
                draw(&mut pass, &self.line_uv, &self.edges);
            } else {
                draw(&mut pass, &self.tri_3d, &self.tris);
                if p.wireframe {
                    draw(&mut pass, &self.line_3d, &self.wire);
                }
                draw(&mut pass, &self.line_3d, &self.edges);
            }
        }
        rs.queue.submit([encoder.finish()]);
        target.texture_id.expect("registered in ensure_target")
    }
}
