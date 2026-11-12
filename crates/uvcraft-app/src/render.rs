//! Offscreen wgpu rendering of the 3D and UV viewports. Each view renders
//! into its own multisampled texture, which egui then shows as an image.

use bytemuck::{Pod, Zeroable};
use eframe::egui;
use eframe::egui_wgpu::RenderState;
use eframe::wgpu::{self, util::DeviceExt};
use glam::Mat4;
use std::hash::{Hash, Hasher};
use std::path::Path;

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
struct TriGeometry {
    pos: [f32; 3],
    normal: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LineGeometry {
    pos: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct UvAttribute {
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ColorAttribute {
    color: [f32; 4],
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
    pub heatmap: bool,
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
    geometry: BufferSlot,
    uv: BufferSlot,
    color: BufferSlot,
    count: u32,
}

struct BufferSlot {
    buffer: wgpu::Buffer,
    capacity: u64,
    fingerprint: u64,
    len: usize,
}

/// Reverse lookup from one UV vertex to positions in an interleaved draw
/// stream. Built only when the full scene changes, then reused by previews.
pub struct UvSlotMap {
    offsets: Vec<usize>,
    slots: Vec<u32>,
    len: usize,
}

impl UvSlotMap {
    pub fn new(sources: &[u32], uv_count: usize) -> Self {
        let mut offsets = vec![0; uv_count + 1];
        for &source in sources {
            offsets[source as usize + 1] += 1;
        }
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        let mut cursor = offsets[..uv_count].to_vec();
        let mut slots = vec![0; sources.len()];
        for (slot, &source) in sources.iter().enumerate() {
            let at = &mut cursor[source as usize];
            slots[*at] = slot as u32;
            *at += 1;
        }
        Self {
            offsets,
            slots,
            len: sources.len(),
        }
    }

    fn updates(&self, changed: &[(u32, [f32; 2])]) -> Vec<(u32, [f32; 2])> {
        let mut updates = Vec::new();
        for &(source, uv) in changed {
            let source = source as usize;
            if source + 1 >= self.offsets.len() {
                continue;
            }
            for &slot in &self.slots[self.offsets[source]..self.offsets[source + 1]] {
                updates.push((slot, uv));
            }
        }
        updates.sort_unstable_by_key(|&(slot, _)| slot);
        updates
    }
}

pub struct UvStreamLayout {
    pub tris: UvSlotMap,
    pub wire: UvSlotMap,
    pub edges: UvSlotMap,
}

impl UvStreamLayout {
    pub fn new(tris: &[u32], wire: &[u32], edges: &[u32], uv_count: usize) -> Self {
        Self {
            tris: UvSlotMap::new(tris, uv_count),
            wire: UvSlotMap::new(wire, uv_count),
            edges: UvSlotMap::new(edges, uv_count),
        }
    }
}

fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn upload_changed(previous_len: usize, previous_hash: u64, bytes: &[u8]) -> (bool, u64) {
    let hash = fingerprint(bytes);
    (previous_len != bytes.len() || previous_hash != hash, hash)
}

fn split_tri_attributes(
    data: &[TriVertex],
) -> (Vec<TriGeometry>, Vec<UvAttribute>, Vec<ColorAttribute>) {
    let mut geometry = Vec::with_capacity(data.len());
    let mut uv = Vec::with_capacity(data.len());
    let mut color = Vec::with_capacity(data.len());
    for vertex in data {
        geometry.push(TriGeometry {
            pos: vertex.pos,
            normal: vertex.normal,
        });
        uv.push(UvAttribute { uv: vertex.uv });
        color.push(ColorAttribute {
            color: vertex.color,
        });
    }
    (geometry, uv, color)
}

impl BufferSlot {
    /// Stream an interactive attribute without hashing every vertex. The next
    /// regular update refreshes its fingerprint from the document.
    fn stream(&mut self, rs: &RenderState, bytes: &[u8]) {
        debug_assert!(bytes.len() as u64 <= self.capacity);
        rs.queue.write_buffer(&self.buffer, 0, bytes);
        self.len = 0;
    }

    fn stream_sparse(&mut self, rs: &RenderState, updates: &[(u32, [f32; 2])]) {
        let mut start = 0;
        while start < updates.len() {
            let mut end = start + 1;
            while end < updates.len() && updates[end].0 == updates[end - 1].0 + 1 {
                end += 1;
            }
            let values: Vec<[f32; 2]> = updates[start..end].iter().map(|&(_, uv)| uv).collect();
            rs.queue.write_buffer(
                &self.buffer,
                updates[start].0 as u64 * std::mem::size_of::<UvAttribute>() as u64,
                bytemuck::cast_slice(&values),
            );
            start = end;
        }
        if !updates.is_empty() {
            self.len = 0;
        }
    }

    fn update(&mut self, rs: &RenderState, label: &str, bytes: &[u8]) -> bool {
        let (changed, next_hash) = upload_changed(self.len, self.fingerprint, bytes);
        if !changed {
            return false;
        }
        if bytes.len() as u64 > self.capacity {
            self.capacity = bytes.len() as u64;
            self.buffer = rs.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: self.capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        rs.queue.write_buffer(&self.buffer, 0, bytes);
        self.fingerprint = next_hash;
        self.len = bytes.len();
        true
    }

    fn new(rs: &RenderState, label: &str, bytes: &[u8]) -> Self {
        let capacity = bytes.len() as u64;
        let buffer = rs.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        rs.queue.write_buffer(&buffer, 0, bytes);
        Self {
            buffer,
            capacity,
            fingerprint: fingerprint(bytes),
            len: bytes.len(),
        }
    }
}

pub struct Renderer {
    bind_group_layout: wgpu::BindGroupLayout,
    sample_texture: wgpu::Texture,
    sample_sampler: wgpu::Sampler,
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
    let tri_geometry_attrs = wgpu::vertex_attr_array![0 => Float32x3, 2 => Float32x3];
    let line_geometry_attrs = wgpu::vertex_attr_array![0 => Float32x3];
    let uv_attrs = wgpu::vertex_attr_array![1 => Float32x2];
    let color_attrs = wgpu::vertex_attr_array![3 => Float32x4];
    let line_color_attrs = wgpu::vertex_attr_array![2 => Float32x4];
    let (geometry_stride, geometry_attrs): (u64, &[wgpu::VertexAttribute]) = if lines {
        (
            std::mem::size_of::<LineGeometry>() as u64,
            &line_geometry_attrs,
        )
    } else {
        (
            std::mem::size_of::<TriGeometry>() as u64,
            &tri_geometry_attrs,
        )
    };
    let buffers = [
        Some(wgpu::VertexBufferLayout {
            array_stride: geometry_stride,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: geometry_attrs,
        }),
        Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<UvAttribute>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &uv_attrs,
        }),
        Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ColorAttribute>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: if lines {
                &line_color_attrs
            } else {
                &color_attrs
            },
        }),
    ];
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
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sample_texture = device.create_texture_with_data(
            &rs.queue,
            &wgpu::TextureDescriptor {
                label: Some("uvcraft default texture"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &[255, 255, 255, 255],
        );
        let sample_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let sample_view = sample_texture.create_view(&Default::default());
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
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&sample_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&sample_sampler),
                    },
                ],
            });
            View {
                target: None,
                uniform,
                bind_group,
            }
        };
        let views = [view(), view()];
        Renderer {
            bind_group_layout: bgl,
            sample_texture,
            sample_sampler,
            tri_3d: pipeline(device, &layout, &module, false, true),
            tri_uv: pipeline(device, &layout, &module, false, false),
            line_3d: pipeline(device, &layout, &module, true, true),
            line_uv: pipeline(device, &layout, &module, true, false),
            views,
            tris: None,
            wire: None,
            edges: None,
            grid: None,
        }
    }

    pub fn set_custom_texture(&mut self, rs: &RenderState, path: &Path) -> Result<(), String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err("texture file is larger than 64 MiB".into());
        }
        let image = image::load_from_memory(&bytes)
            .map_err(|e| e.to_string())?
            .to_rgba8();
        let (width, height) = image.dimensions();
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err("texture dimensions must be 1–8192 pixels".into());
        }
        self.sample_texture = rs.device.create_texture_with_data(
            &rs.queue,
            &wgpu::TextureDescriptor {
                label: Some("uvcraft custom texture"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &image,
        );
        let view = self.sample_texture.create_view(&Default::default());
        for v in &mut self.views {
            v.bind_group = rs.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("uvcraft textured bind group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: v.uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sample_sampler),
                    },
                ],
            });
        }
        Ok(())
    }

    fn upload(
        rs: &RenderState,
        label: &str,
        current: &mut Option<Mesh>,
        geometry: &[u8],
        uv: &[u8],
        color: &[u8],
        count: usize,
    ) {
        if count == 0 {
            *current = None;
            return;
        }
        if let Some(mesh) = current {
            mesh.geometry.update(rs, label, geometry);
            mesh.uv.update(rs, "uvcraft uv", uv);
            mesh.color.update(rs, "uvcraft color", color);
            mesh.count = count as u32;
        } else {
            *current = Some(Mesh {
                geometry: BufferSlot::new(rs, label, geometry),
                uv: BufferSlot::new(rs, "uvcraft uv", uv),
                color: BufferSlot::new(rs, "uvcraft color", color),
                count: count as u32,
            });
        }
    }

    fn upload_tris(rs: &RenderState, current: &mut Option<Mesh>, data: &[TriVertex]) {
        let (geometry, uv, color) = split_tri_attributes(data);
        Self::upload(
            rs,
            "uvcraft triangle geometry",
            current,
            bytemuck::cast_slice(&geometry),
            bytemuck::cast_slice(&uv),
            bytemuck::cast_slice(&color),
            data.len(),
        );
    }

    fn upload_lines(
        rs: &RenderState,
        label: &str,
        current: &mut Option<Mesh>,
        data: &[LineVertex],
    ) {
        let mut geometry = Vec::with_capacity(data.len());
        let mut uv = Vec::with_capacity(data.len());
        let mut color = Vec::with_capacity(data.len());
        for vertex in data {
            geometry.push(LineGeometry { pos: vertex.pos });
            uv.push(UvAttribute { uv: vertex.uv });
            color.push(ColorAttribute {
                color: vertex.color,
            });
        }
        Self::upload(
            rs,
            label,
            current,
            bytemuck::cast_slice(&geometry),
            bytemuck::cast_slice(&uv),
            bytemuck::cast_slice(&color),
            data.len(),
        );
    }

    pub fn set_scene(
        &mut self,
        rs: &RenderState,
        tris: &[TriVertex],
        wire: &[LineVertex],
        edges: &[LineVertex],
        grid: &[LineVertex],
    ) {
        Self::upload_tris(rs, &mut self.tris, tris);
        Self::upload_lines(rs, "uvcraft wire", &mut self.wire, wire);
        Self::upload_lines(rs, "uvcraft edges", &mut self.edges, edges);
        Self::upload_lines(rs, "uvcraft grid", &mut self.grid, grid);
    }

    /// Update only UV vertex attributes when topology and color are unchanged.
    /// Returns false if the current GPU buffers do not match this scene.
    pub fn set_scene_uvs(
        &mut self,
        rs: &RenderState,
        tris: &[[f32; 2]],
        wire: &[[f32; 2]],
        edges: &[[f32; 2]],
    ) -> bool {
        let matches = |slot: &Option<Mesh>, len: usize| {
            slot.as_ref()
                .map_or(len == 0, |mesh| mesh.count as usize == len)
        };
        if !matches(&self.tris, tris.len())
            || !matches(&self.wire, wire.len())
            || !matches(&self.edges, edges.len())
        {
            return false;
        }
        if let Some(mesh) = &mut self.tris {
            mesh.uv.stream(rs, bytemuck::cast_slice(tris));
        }
        if let Some(mesh) = &mut self.wire {
            mesh.uv.stream(rs, bytemuck::cast_slice(wire));
        }
        if let Some(mesh) = &mut self.edges {
            mesh.uv.stream(rs, bytemuck::cast_slice(edges));
        }
        true
    }

    /// Interactive distortion preview: UVs and heatmap colors change, while
    /// triangle geometry and line colors remain cached on the GPU.
    pub fn set_scene_uvs_and_colors(
        &mut self,
        rs: &RenderState,
        tris: &[[f32; 2]],
        colors: &[[f32; 4]],
        wire: &[[f32; 2]],
        edges: &[[f32; 2]],
    ) -> bool {
        if self.tris.as_ref().map_or(!colors.is_empty(), |mesh|
            mesh.count as usize != colors.len()) {
            return false;
        }
        if !self.set_scene_uvs(rs, tris, wire, edges) { return false; }
        if let Some(mesh) = &mut self.tris {
            mesh.color.stream(rs, bytemuck::cast_slice(colors));
        }
        true
    }

    /// Stream only positions whose UV vertex changed. Falls back to the full
    /// stream path when the mapping no longer matches or writes fragment into
    /// too many small ranges.
    pub fn set_scene_uvs_sparse(
        &mut self,
        rs: &RenderState,
        layout: &UvStreamLayout,
        changed: &[(u32, [f32; 2])],
    ) -> bool {
        let matches = |mesh: &Option<Mesh>, map: &UvSlotMap| {
            mesh.as_ref()
                .map_or(map.len == 0, |mesh| mesh.count as usize == map.len)
        };
        if !matches(&self.tris, &layout.tris)
            || !matches(&self.wire, &layout.wire)
            || !matches(&self.edges, &layout.edges)
        {
            return false;
        }
        let tri = layout.tris.updates(changed);
        let wire = layout.wire.updates(changed);
        let edges = layout.edges.updates(changed);
        let runs = |items: &[(u32, [f32; 2])]| {
            usize::from(!items.is_empty())
                + items
                    .windows(2)
                    .filter(|pair| pair[1].0 != pair[0].0 + 1)
                    .count()
        };
        if runs(&tri) + runs(&wire) + runs(&edges) > 1024 {
            return false;
        }
        if let Some(mesh) = &mut self.tris {
            mesh.uv.stream_sparse(rs, &tri);
        }
        if let Some(mesh) = &mut self.wire {
            mesh.uv.stream_sparse(rs, &wire);
        }
        if let Some(mesh) = &mut self.edges {
            mesh.uv.stream_sparse(rs, &edges);
        }
        true
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
            light: [p.light[0], p.light[1], p.light[2], f32::from(u8::from(p.heatmap))],
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
                    pass.set_vertex_buffer(0, m.geometry.buffer.slice(..));
                    pass.set_vertex_buffer(1, m.uv.buffer.slice(..));
                    pass.set_vertex_buffer(2, m.color.buffer.slice(..));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn distortion_shader_compiles_for_both_viewports() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY));
        let Some(adapter) = adapters.into_iter().next() else { return; };
        let Ok((device, _queue)) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor::default())) else { return; };
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let _module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("uvcraft distortion shader validation"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let error = pollster::block_on(scope.pop());
        assert!(error.is_none(), "shader validation: {error:?}");
    }

    #[test]
    fn sparse_uv_map_updates_only_affected_slots_on_million_triangle_mesh() {
        let sources: Vec<u32> = (0..3_000_000).collect();
        let map = UvSlotMap::new(&sources, sources.len());
        let changes = map.updates(&[(5, [0.25, 0.5]), (2_999_995, [0.75, 0.5])]);
        assert_eq!(changes, vec![(5, [0.25, 0.5]), (2_999_995, [0.75, 0.5])]);
        assert_eq!(changes.len() * std::mem::size_of::<UvAttribute>(), 16);

        let shared = UvSlotMap::new(&[0, 1, 0, 2, 0], 3);
        assert_eq!(
            shared.updates(&[(0, [1.0, 2.0])]),
            vec![(0, [1.0, 2.0]), (2, [1.0, 2.0]), (4, [1.0, 2.0]),]
        );
    }

    #[test]
    fn million_triangle_uv_edit_skips_geometry_and_color_uploads() {
        // Three million rendered corners; only the 8-byte UV attribute changes.
        let corners = 3_000_000;
        let geometry = vec![0_u8; corners * std::mem::size_of::<TriGeometry>()];
        let color = vec![0_u8; corners * std::mem::size_of::<ColorAttribute>()];
        let mut uv = vec![0_u8; corners * std::mem::size_of::<UvAttribute>()];
        let geometry_hash = fingerprint(&geometry);
        let color_hash = fingerprint(&color);
        let uv_hash = fingerprint(&uv);
        uv[0] = 1;
        assert!(!upload_changed(geometry.len(), geometry_hash, &geometry).0);
        assert!(!upload_changed(color.len(), color_hash, &color).0);
        assert!(upload_changed(uv.len(), uv_hash, &uv).0);
        assert_eq!(uv.len(), 24_000_000);
        assert_eq!(geometry.len() + color.len(), 120_000_000);
    }

    /// Run with `cargo test -p uvcraft-app --release -- --ignored --nocapture`.
    /// Measures the CPU preparation/hash work immediately before queue upload;
    /// actual GPU draw timing depends on the host adapter and viewport size.
    #[test]
    #[ignore]
    fn benchmark_million_triangle_uv_upload_preparation() {
        let corners = 3_000_000;
        let vertices = vec![
            TriVertex {
                pos: [0.0; 3],
                uv: [0.0; 2],
                normal: [0.0, 0.0, 1.0],
                color: [1.0; 4],
            };
            corners
        ];
        let uv_stream = vec![[0.0_f32; 2]; corners];
        let start = Instant::now();
        let (geometry, uv, color) = split_tri_attributes(&vertices);
        let _ = fingerprint(bytemuck::cast_slice(&geometry));
        let _ = fingerprint(bytemuck::cast_slice(&uv));
        let _ = fingerprint(bytemuck::cast_slice(&color));
        let full = start.elapsed();
        let start = Instant::now();
        let _ = fingerprint(bytemuck::cast_slice(&uv_stream));
        let uv_only = start.elapsed();
        eprintln!(
            "1M triangles CPU upload prep: full={full:?}, UV-only={uv_only:?}, static bytes skipped={}",
            geometry.len() * std::mem::size_of::<TriGeometry>()
                + color.len() * std::mem::size_of::<ColorAttribute>()
        );
        assert!(uv_only < full);
    }

    /// Offscreen draw through UvCraft's actual renderer. Requires a physical
    /// Vulkan/Metal/DX12 adapter; prints the adapter and synchronized frame time.
    #[test]
    #[ignore]
    fn benchmark_million_triangle_physical_gpu_draw() {
        use std::sync::Arc;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY));
        let Some(adapter) = adapters
            .iter()
            .find(|adapter| {
                matches!(
                    adapter.get_info().device_type,
                    wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu,
                )
            })
            .cloned()
        else {
            eprintln!(
                "No physical GPU adapter available; available: {:?}",
                adapters.iter().map(|a| a.get_info()).collect::<Vec<_>>()
            );
            return;
        };
        let info = adapter.get_info();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("physical GPU device");
        let ui_renderer = egui::epaint::mutex::RwLock::new(eframe::egui_wgpu::Renderer::new(
            &device,
            COLOR_FORMAT,
            Default::default(),
        ));
        let rs = RenderState {
            adapter,
            available_adapters: adapters,
            instance,
            device,
            queue,
            target_format: COLOR_FORMAT,
            renderer: Arc::new(ui_renderer),
        };
        let mut renderer = Renderer::new(&rs);
        let mut tris = Vec::with_capacity(3_000_000);
        for index in 0..1_000_000 {
            let x = -1.0 + 2.0 * (index % 1000) as f32 / 1000.0;
            let y = -1.0 + 2.0 * (index / 1000) as f32 / 1000.0;
            for [dx, dy] in [[0.0, 0.0], [0.0015, 0.0], [0.0, 0.0015]] {
                tris.push(TriVertex {
                    pos: [x + dx, y + dy, 0.5],
                    uv: [x, y],
                    normal: [0.0, 0.0, 1.0],
                    color: [0.8, 0.6, 0.4, 1.0],
                });
            }
        }
        let upload_start = Instant::now();
        renderer.set_scene(&rs, &tris, &[], &[], &[]);
        rs.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let upload = upload_start.elapsed();
        let view = ViewParams {
            view_proj: Mat4::IDENTITY,
            uv_mode: false,
            tint: 0.0,
            checker_cells: 10.0,
            light: [0.0, 0.0, 1.0],
            wireframe: false,
            heatmap: false,
        };
        for _ in 0..2 {
            renderer.render(&rs, 0, [1024, 1024], &view);
            rs.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        }
        let mut frames = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            renderer.render(&rs, 0, [1024, 1024], &view);
            rs.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            frames.push(start.elapsed());
        }
        frames.sort();
        let mut uv_stream: Vec<[f32; 2]> = tris.iter().map(|vertex| vertex.uv).collect();
        let mut uv_updates = Vec::new();
        for sample in 0..5 {
            uv_stream[0][0] = sample as f32 + 0.5;
            let start = Instant::now();
            assert!(renderer.set_scene_uvs(&rs, &uv_stream, &[], &[]));
            rs.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            uv_updates.push(start.elapsed());
        }
        uv_updates.sort();
        let sources: Vec<u32> = (0..3_000_000).collect();
        let layout = UvStreamLayout::new(&sources, &[], &[], sources.len());
        let mut sparse_updates = Vec::new();
        for sample in 0..5 {
            let start = Instant::now();
            assert!(renderer.set_scene_uvs_sparse(
                &rs,
                &layout,
                &[(0, [sample as f32 + 10.5, 0.0])],
            ));
            rs.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            sparse_updates.push(start.elapsed());
        }
        sparse_updates.sort();
        eprintln!(
            "1M triangles physical GPU {:?} {:?}: upload={upload:?}; 1024² MSAA4 synchronized frame median={:?}, range={:?}..{:?}",
            info.name, info.backend, frames[2], frames[0], frames[4]
        );
        eprintln!(
            "1M triangles UV-only GPU upload 24 MB: median={:?}, range={:?}..{:?}",
            uv_updates[2], uv_updates[0], uv_updates[4]
        );
        eprintln!(
            "1M triangles sparse UV GPU upload 8 B: median={:?}, range={:?}..{:?}",
            sparse_updates[2], sparse_updates[0], sparse_updates[4]
        );
        assert_eq!(
            renderer.tris.as_ref().map(|mesh| mesh.count),
            Some(3_000_000)
        );
    }
}
