//! Optional wgpu compute search for expensive raster placement queries.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;
use std::{future::poll_fn, task::Poll};

use wgpu::util::DeviceExt;

use super::{Grid, Mask};

pub(super) struct Finder {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
}

struct MapState {
    result: Option<Result<(), String>>,
    waker: Option<std::task::Waker>,
}

impl Finder {
    #[cfg(not(target_arch = "wasm32"))]
    fn new(software: bool) -> Result<Self, String> {
        pollster::block_on(Self::new_async(software))
    }

    pub(super) async fn new_async(software: bool) -> Result<Self, String> {
        #[cfg(target_arch = "wasm32")]
        let instance = wgpu::util::new_instance_with_webgpu_detection(
            wgpu::InstanceDescriptor::new_without_display_handle(),
        )
        .await;
        #[cfg(not(target_arch = "wasm32"))]
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: software,
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        #[cfg(test)]
        eprintln!(
            "UvCraft pack adapter: {} ({:?})",
            adapter.get_info().name,
            adapter.get_info().device_type
        );
        if !software && adapter.get_info().device_type == wgpu::DeviceType::Cpu {
            return Err("software compute adapter is slower than CPU raster search".into());
        }
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .map_err(|error| error.to_string())?;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("UvCraft raster placement search"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu.wgsl").into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("UvCraft raster placement search"),
            layout: None,
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        Ok(Self {
            device,
            queue,
            pipeline,
        })
    }

    fn dispatch(&self, grid: &Grid, mask: &Mask) -> Result<Option<(wgpu::Buffer, usize)>, String> {
        if mask.w > grid.gx || mask.h > grid.gy {
            return Ok(None);
        }
        let max_x = grid.gx - mask.w;
        let max_y = grid.gy - mask.h;
        let columns = max_x + 1;
        let count = columns * (max_y + 1);
        if count > u32::MAX as usize {
            return Err("candidate grid is too large for GPU search".into());
        }
        let split = |words: &[u64]| -> Vec<u32> {
            words
                .iter()
                .flat_map(|&word| [word as u32, (word >> 32) as u32])
                .collect()
        };
        let grid_words = split(&grid.bits);
        let mask_words = split(&mask.bits);
        let params = [
            grid.gw * 2,
            mask.mw * 2,
            mask.h,
            mask.fx,
            max_x,
            max_y,
            columns,
            count,
        ]
        .map(|value| value as u32);
        let make_storage = |label, bytes: &[u8]| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some(label),
                    contents: bytes,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                })
        };
        let grid_buffer = make_storage("UvCraft placement grid", bytemuck::cast_slice(&grid_words));
        let mask_buffer = make_storage("UvCraft placement mask", bytemuck::cast_slice(&mask_words));
        let result = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("UvCraft placement result"),
                contents: bytemuck::bytes_of(&u32::MAX),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            });
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("UvCraft placement search parameters"),
                contents: bytemuck::cast_slice(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("UvCraft placement readback"),
            size: 4,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = self.pipeline.get_bind_group_layout(0);
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("UvCraft placement inputs"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: grid_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: mask_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: result.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("UvCraft placement search"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups((count as u32).div_ceil(256), 1, 1);
        }
        encoder.copy_buffer_to_buffer(&result, 0, &staging, 0, 4);
        self.queue.submit([encoder.finish()]);
        Ok(Some((staging, columns)))
    }

    fn read_winner(
        staging: &wgpu::Buffer,
        columns: usize,
    ) -> Result<Option<(usize, usize)>, String> {
        let bytes = staging.get_mapped_range(..).map_err(|e| e.to_string())?;
        let winner = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        drop(bytes);
        staging.unmap();
        Ok((winner != u32::MAX)
            .then_some(((winner as usize) % columns, (winner as usize) / columns)))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn find(&self, grid: &Grid, mask: &Mask) -> Result<Option<(usize, usize)>, String> {
        let Some((staging, columns)) = self.dispatch(grid, mask)? else {
            return Ok(None);
        };
        let (tx, rx) = mpsc::channel();
        staging.map_async(wgpu::MapMode::Read, .., move |status| {
            let _ = tx.send(status);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        Self::read_winner(&staging, columns)
    }

    pub(super) async fn find_async(
        &self,
        grid: &Grid,
        mask: &Mask,
    ) -> Result<Option<(usize, usize)>, String> {
        let Some((staging, columns)) = self.dispatch(grid, mask)? else {
            return Ok(None);
        };
        let state = Arc::new(Mutex::new(MapState {
            result: None,
            waker: None,
        }));
        let callback_state = Arc::clone(&state);
        staging.map_async(wgpu::MapMode::Read, .., move |status| {
            let mut state = callback_state.lock().unwrap();
            state.result = Some(status.map_err(|error| error.to_string()));
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        });
        // Native wgpu requires polling. WebGPU invokes the callback from the
        // browser event loop; blocking there would deadlock.
        #[cfg(not(target_arch = "wasm32"))]
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| error.to_string())?;
        poll_fn(|cx| {
            let mut state = state.lock().unwrap();
            if let Some(result) = state.result.take() {
                Poll::Ready(result)
            } else {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await?;
        Self::read_winner(&staging, columns)
    }
}

#[cfg(not(target_arch = "wasm32"))]
static FINDER: OnceLock<Option<Finder>> = OnceLock::new();

/// Small masks and candidate grids are faster on the CPU.
pub(super) fn eligible(grid: &Grid, mask: &Mask) -> bool {
    !cfg!(target_arch = "wasm32") && eligible_async(grid, mask)
}

pub(super) fn eligible_async(grid: &Grid, mask: &Mask) -> bool {
    std::env::var_os("UVCRAFT_GPU_PACK").as_deref() != Some(std::ffi::OsStr::new("0"))
        && grid.gx * grid.gy >= 256 * 256
        && mask.h >= 8
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn find(grid: &Grid, mask: &Mask) -> Option<Option<(usize, usize)>> {
    if !eligible(grid, mask) {
        return None;
    }
    let finder = FINDER.get_or_init(|| Finder::new(false).ok());
    let finder = finder.as_ref()?;
    finder.find(grid, mask).ok()
}

#[cfg(target_arch = "wasm32")]
pub(super) fn find(_: &Grid, _: &Mask) -> Option<Option<(usize, usize)>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::raster::{Shape, Xform};
    use glam::DVec2;

    #[test]
    fn gpu_find_matches_cpu_when_software_adapter_exists() {
        let Ok(finder) = Finder::new(true) else {
            eprintln!("no software compute adapter; GPU comparison skipped");
            return;
        };
        let grid_size = (256, 256);
        let mut grid = Grid::new(grid_size);
        let shape = Shape {
            pts: vec![DVec2::ZERO, DVec2::new(0.17, 0.0), DVec2::new(0.1, 0.11)],
            tris: vec![[0, 1, 2]],
        };
        let mask = Mask::new(&shape, Xform::IDENTITY, 1.0, 1.0 / 256.0, 2, grid_size).unwrap();
        for &(x, y) in &[(0, 0), (60, 0), (0, 80), (150, 100)] {
            grid.insert(&mask, x, y, 2);
            assert_eq!(finder.find(&grid, &mask).unwrap(), grid.find_cpu(&mask));
            assert_eq!(
                pollster::block_on(finder.find_async(&grid, &mask)).unwrap(),
                grid.find_cpu(&mask)
            );
            assert_eq!(grid.find(&mask), grid.find_cpu(&mask));
        }
    }

    #[test]
    fn gpu_search_handles_hard_no_fit_grid() {
        let Ok(finder) = Finder::new(true) else {
            eprintln!("no software compute adapter; hard-grid comparison skipped");
            return;
        };
        let size = (256, 256);
        let mut grid = Grid::new(size);
        for y in (8..256).step_by(16) {
            for x in (8..256).step_by(16) {
                grid.bits[y * grid.gw + x / 64] |= 1 << (x % 64);
            }
        }
        let shape = Shape {
            pts: vec![
                DVec2::ZERO,
                DVec2::new(0.12, 0.0),
                DVec2::new(0.12, 0.12),
                DVec2::new(0.0, 0.12),
            ],
            tris: vec![[0, 1, 2], [0, 2, 3]],
        };
        let mask = Mask::new(&shape, Xform::IDENTITY, 1.0, 1.0 / 256.0, 0, size).unwrap();
        assert!(grid.find_cpu_budget(&mask, 2048).is_err());
        let cpu = grid.find_cpu(&mask);
        let gpu = finder.find(&grid, &mask).unwrap();
        assert_eq!(gpu, cpu);
        match Finder::new(false) {
            Ok(physical) => {
                assert_eq!(physical.find(&grid, &mask).unwrap(), cpu);
                let median = |mut samples: Vec<Duration>| {
                    samples.sort_unstable();
                    samples[samples.len() / 2]
                };
                let mut cpu_times = Vec::new();
                let mut gpu_times = Vec::new();
                for _ in 0..7 {
                    let started = std::time::Instant::now();
                    assert_eq!(grid.find_cpu(&mask), cpu);
                    cpu_times.push(started.elapsed());
                    let started = std::time::Instant::now();
                    assert_eq!(physical.find(&grid, &mask).unwrap(), cpu);
                    gpu_times.push(started.elapsed());
                }
                eprintln!(
                    "hard placement search median: CPU {:?}, physical compute {:?}",
                    median(cpu_times),
                    median(gpu_times)
                );
            }
            Err(error) => eprintln!("physical GPU unavailable: {error}"),
        }
    }
}
