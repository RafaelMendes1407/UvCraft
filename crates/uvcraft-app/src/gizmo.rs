//! UV island gizmo math. The UI commits one Transform command per drag.

use glam::DVec2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    Move,
    Rotate,
    Scale,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub translate: [f64; 2],
    pub rotate_deg: f64,
    pub scale: [f64; 2],
}

impl Transform {
    pub fn is_noop(self) -> bool {
        self.translate.iter().all(|v| v.abs() < 1e-10)
            && self.rotate_deg.abs() < 1e-8
            && self.scale.iter().all(|v| (v - 1.0).abs() < 1e-10)
    }
}

/// Derive a transform about a shared UV center. Snaps movement to texel
/// increments, rotation to 15 degrees and uniform scale to 10% increments.
pub fn transform(
    handle: Handle,
    start: DVec2,
    current: DVec2,
    center: DVec2,
    snap: bool,
    resolution: u32,
) -> Transform {
    let mut translation = DVec2::ZERO;
    let mut angle = 0.0;
    let mut scale = 1.0;
    match handle {
        Handle::Move => {
            translation = current - start;
            if snap && resolution > 0 {
                let step = 1.0 / resolution as f64;
                translation = (translation / step).round() * step;
            }
        }
        Handle::Rotate => {
            let a = start - center;
            let b = current - center;
            if a.length_squared() > 1e-18 && b.length_squared() > 1e-18 {
                angle = a.perp_dot(b).atan2(a.dot(b)).to_degrees();
                if snap {
                    angle = (angle / 15.0).round() * 15.0;
                }
            }
        }
        Handle::Scale => {
            let a = (start - center).length();
            if a > 1e-9 {
                scale = ((current - center).length() / a).clamp(0.01, 100.0);
                if snap {
                    scale = (scale * 10.0).round().max(1.0) / 10.0;
                }
            }
        }
    }
    about_center(center, translation, angle, scale)
}

/// Compose a uniform transform around one shared center, keeping the group's
/// internal spacing and orientation intact.
pub fn about_center(center: DVec2, offset: DVec2, angle: f64, scale: f64) -> Transform {
    let (sin, cos) = angle.to_radians().sin_cos();
    let transformed_center = DVec2::new(
        scale * (center.x * cos - center.y * sin),
        scale * (center.x * sin + center.y * cos),
    );
    Transform {
        translate: (center + offset - transformed_center).to_array(),
        rotate_deg: angle,
        scale: [scale; 2],
    }
}

pub fn apply(point: DVec2, transform: Transform) -> DVec2 {
    let radians = transform.rotate_deg.to_radians();
    let (sin, cos) = radians.sin_cos();
    let x = point.x * transform.scale[0];
    let y = point.y * transform.scale[1];
    DVec2::new(x * cos - y * sin, x * sin + y * cos) + DVec2::from(transform.translate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_snaps_to_texels() {
        let t = transform(
            Handle::Move,
            DVec2::ZERO,
            DVec2::new(0.126, 0.249),
            DVec2::ZERO,
            true,
            8,
        );
        assert_eq!(t.translate, [0.125, 0.25]);
    }

    #[test]
    fn rotation_and_scale_keep_shared_center_fixed() {
        let center = DVec2::new(0.5, 0.5);
        let rotate = transform(
            Handle::Rotate,
            center + DVec2::X,
            center + DVec2::Y,
            center,
            true,
            1024,
        );
        assert_eq!(rotate.rotate_deg, 90.0);
        let c = DVec2::new(-center.y, center.x) + DVec2::from(rotate.translate);
        assert!((c - center).length() < 1e-12);

        let scaled = transform(
            Handle::Scale,
            center + DVec2::X,
            center + DVec2::X * 2.0,
            center,
            false,
            1024,
        );
        assert_eq!(scaled.scale, [2.0, 2.0]);
        assert!((center * 2.0 + DVec2::from(scaled.translate) - center).length() < 1e-12);
        assert!((apply(center, scaled) - center).length() < 1e-12);
    }

    #[test]
    fn group_transform_preserves_relative_offsets() {
        let center = DVec2::new(2.0, 3.0);
        let a = DVec2::new(1.0, 3.0);
        let b = DVec2::new(3.0, 3.0);
        let offset = DVec2::new(0.25, -0.5);
        let t = about_center(center, offset, 90.0, 2.0);
        assert!((apply(center, t) - center - offset).length() < 1e-12);
        assert!(((apply(a, t) - apply(b, t)).length() - 4.0).abs() < 1e-12);
    }
}
