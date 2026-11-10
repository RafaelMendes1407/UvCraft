//! 3D orbit camera and 2D UV camera.

use glam::camera::rh::{proj::directx, view};
use glam::{Mat4, Vec2, Vec3};

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
        self.center = (min + max) / 2.0;
        let extent = (max - min).max(Vec2::splat(1e-6));
        self.zoom = (size.x / extent.x).min(size.y / extent.y) * 0.9;
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
}
