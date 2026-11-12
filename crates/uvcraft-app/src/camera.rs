//! 3D orbit camera and 2D UV camera.

use glam::camera::rh::{proj::directx, view};
use glam::{Mat4, Vec2, Vec3};
use uvcraft_algo::projection::{CameraOrthographic, CameraPerspective};

pub const FOV_Y: f32 = std::f32::consts::FRAC_PI_4;

#[derive(Debug, Clone, Copy)]
pub struct OrbitCamera {
    pub target: Vec3,
    pub distance: f32,
    pub yaw: f32,
    pub pitch: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        OrbitCamera {
            target: Vec3::ZERO,
            distance: 4.0,
            yaw: 0.6,
            pitch: 0.4,
        }
    }
}

impl OrbitCamera {
    pub fn eye(&self) -> Vec3 {
        let dir = Vec3::new(
            self.pitch.cos() * self.yaw.sin(),
            self.pitch.sin(),
            self.pitch.cos() * self.yaw.cos(),
        );
        self.target + dir * self.distance
    }

    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let near = (self.distance * 0.01).max(1e-5);
        // wgpu NDC: Z in [0, 1], Y up (the `directx` convention).
        directx::perspective(FOV_Y, aspect.max(1e-3), near, self.distance * 100.0 + 1.0)
            * view::look_at_mat4(self.eye(), self.target, Vec3::Y)
    }

    pub fn forward(&self) -> Vec3 {
        (self.target - self.eye()).normalize_or_zero()
    }

    pub fn orthographic_projection(&self, aspect: f32) -> CameraOrthographic {
        let forward = self.forward();
        let right = forward.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(forward).normalize_or_zero();
        CameraOrthographic {
            right: [right.x as f64, right.y as f64, right.z as f64],
            up: [up.x as f64, up.y as f64, up.z as f64],
            aspect: aspect.max(1e-3) as f64,
        }
    }

    pub fn perspective_projection(&self, aspect: f32) -> CameraPerspective {
        CameraPerspective {
            view_proj: self.view_proj(aspect).to_cols_array().map(f64::from),
        }
    }

    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * 0.008;
        self.pitch = (self.pitch + dy * 0.008).clamp(-1.55, 1.55);
    }

    /// Moves the target with the cursor; `height` is the viewport height in points.
    pub fn pan(&mut self, dx: f32, dy: f32, height: f32) {
        let scale = 2.0 * self.distance * (FOV_Y / 2.0).tan() / height.max(1.0);
        let fwd = self.forward();
        let right = fwd.cross(Vec3::Y).normalize_or_zero();
        let up = right.cross(fwd);
        self.target += (-right * dx + up * dy) * scale;
    }

    pub fn zoom(&mut self, factor: f32) {
        self.distance = (self.distance * factor).clamp(1e-4, 1e7);
    }

    pub fn frame(&mut self, min: Vec3, max: Vec3) {
        self.target = (min + max) / 2.0;
        let radius = ((max - min).length() / 2.0).max(1e-4);
        self.distance = radius / (FOV_Y / 2.0).sin() * 1.05;
    }
}

/// UV view: `center` in UV units, `zoom` in points per UV unit.
#[derive(Debug, Clone, Copy)]
pub struct UvCamera {
    pub center: Vec2,
    pub zoom: f32,
}

impl Default for UvCamera {
    fn default() -> Self {
        UvCamera {
            center: Vec2::splat(0.5),
            zoom: 400.0,
        }
    }
}

impl UvCamera {
    /// Visible conventional UDIM tiles (10 columns, up to 100 rows).
    pub fn visible_udim_tiles(&self, size: Vec2) -> Vec<(i32, i32, u32)> {
        let a = self.screen_to_uv(Vec2::ZERO, size);
        let b = self.screen_to_uv(size, size);
        let u0 = (a.x.min(b.x).floor() as i32).clamp(0, 9);
        let u1 = (a.x.max(b.x).ceil() as i32 - 1).clamp(0, 9);
        let v0 = (a.y.min(b.y).floor() as i32).clamp(0, 99);
        let v1 = (a.y.max(b.y).ceil() as i32 - 1).clamp(0, 99);
        if a.x.max(b.x) <= 0.0
            || a.y.max(b.y) <= 0.0
            || a.x.min(b.x) >= 10.0
            || a.y.min(b.y) >= 100.0
        {
            return Vec::new();
        }
        (v0..=v1)
            .flat_map(|v| (u0..=u1).map(move |u| (u, v, 1001 + u as u32 + 10 * v as u32)))
            .collect()
    }
    pub fn view_proj(&self, size: Vec2) -> Mat4 {
        let half = size / (2.0 * self.zoom);
        directx::orthographic(
            self.center.x - half.x,
            self.center.x + half.x,
            self.center.y - half.y,
            self.center.y + half.y,
            -1.0,
            1.0,
        )
    }

    /// Position relative to the view's top-left corner (points) → UV.
    pub fn screen_to_uv(&self, p: Vec2, size: Vec2) -> Vec2 {
        let d = p - size / 2.0;
        self.center + Vec2::new(d.x, -d.y) / self.zoom
    }

    pub fn uv_to_screen(&self, uv: Vec2, size: Vec2) -> Vec2 {
        let d = (uv - self.center) * self.zoom;
        size / 2.0 + Vec2::new(d.x, -d.y)
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.center += Vec2::new(-dx, dy) / self.zoom;
    }

    /// Zooms keeping the UV point under the cursor fixed.
    pub fn zoom_at(&mut self, factor: f32, cursor: Vec2, size: Vec2) {
        let before = self.screen_to_uv(cursor, size);
        self.zoom = (self.zoom * factor).clamp(1.0, 1e7);
        let after = self.screen_to_uv(cursor, size);
        self.center += before - after;
    }

    pub fn frame(&mut self, min: Vec2, max: Vec2, size: Vec2) {
        let min = min.min(Vec2::ZERO);
        let max = max.max(Vec2::ONE);
        self.frame_bounds(min, max, size);
    }

    /// Fits the exact bounds, without expanding them to include the 0–1 tile.
    pub fn frame_bounds(&mut self, min: Vec2, max: Vec2, size: Vec2) {
        self.center = (min + max) / 2.0;
        let extent = (max - min).max(Vec2::splat(1e-6));
        self.zoom = ((size.x / extent.x).min(size.y / extent.y) * 0.9).clamp(1.0, 1e7);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uv_round_trip() {
        let mut c = UvCamera::default();
        let size = Vec2::new(800.0, 600.0);
        c.zoom_at(1.7, Vec2::new(100.0, 50.0), size);
        let p = Vec2::new(321.0, 123.0);
        assert!((c.uv_to_screen(c.screen_to_uv(p, size), size) - p).length() < 1e-3);
        // The projection matches the screen mapping.
        let uv = c.screen_to_uv(p, size);
        let ndc = c.view_proj(size).project_point3(uv.extend(0.0));
        let screen = Vec2::new((ndc.x + 1.0) / 2.0 * size.x, (1.0 - ndc.y) / 2.0 * size.y);
        assert!((screen - p).length() < 1e-2, "{screen}");
    }

    #[test]
    fn visible_udim_tiles_follow_pan_and_zoom() {
        let c = UvCamera {
            center: Vec2::new(1.5, 0.5),
            zoom: 200.0,
        };
        assert_eq!(c.visible_udim_tiles(Vec2::splat(200.0)), vec![(1, 0, 1002)]);
        let c = UvCamera {
            center: Vec2::new(0.5, 1.5),
            zoom: 200.0,
        };
        assert_eq!(c.visible_udim_tiles(Vec2::splat(200.0)), vec![(0, 1, 1011)]);
        let c = UvCamera {
            center: Vec2::new(10.5, 0.5),
            zoom: 200.0,
        };
        assert!(c.visible_udim_tiles(Vec2::splat(200.0)).is_empty());
    }

    #[test]
    fn camera_orthographic_axes_match_view_screen_orientation() {
        let camera = OrbitCamera::default();
        let projection = camera.orthographic_projection(16.0 / 9.0);
        assert!(projection.valid());
        let right = Vec3::from_array(projection.right.map(|v| v as f32));
        let up = Vec3::from_array(projection.up.map(|v| v as f32));
        let vp = camera.view_proj(16.0 / 9.0);
        let center = vp.project_point3(camera.target);
        assert!(vp.project_point3(camera.target + right).x > center.x);
        assert!(vp.project_point3(camera.target + up).y > center.y);
        let perspective = camera.perspective_projection(16.0 / 9.0);
        assert!(perspective.valid());
        assert_eq!(perspective.view_proj, vp.to_cols_array().map(f64::from));
    }
}
