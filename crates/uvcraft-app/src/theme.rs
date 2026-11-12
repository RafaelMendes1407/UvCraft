//! UvCraft's graphite and orange desktop palette.

use eframe::egui::{self, Color32, CornerRadius, Stroke};

pub(crate) const ORANGE: Color32 = Color32::from_rgb(255, 122, 24);
pub(crate) const GRAPHITE: Color32 = Color32::from_rgb(23, 25, 29);
pub(crate) const SURFACE: Color32 = Color32::from_rgb(29, 31, 36);
pub(crate) const BORDER: Color32 = Color32::from_rgb(53, 56, 63);

#[derive(Clone, Copy)]
pub(crate) enum ToolbarIcon {
    Open,
    Save,
    SaveAs,
    Undo,
    Redo,
    Select,
    Lasso,
    Brush,
    Model,
    Uv,
    Fit,
    Wire,
    Scene,
    Settings,
}

pub(crate) fn icon_button(
    ui: &mut egui::Ui,
    icon: ToolbarIcon,
    tooltip: &str,
    active: bool,
) -> egui::Response {
    let response = ui
        .add(
            egui::Button::new("")
                .min_size(egui::vec2(34.0, 32.0))
                .fill(if active { ORANGE } else { SURFACE })
                .stroke(Stroke::new(1.0, BORDER)),
        )
        .on_hover_text(tooltip);
    let color = if active {
        Color32::from_rgb(24, 24, 26)
    } else {
        Color32::from_rgb(222, 224, 229)
    };
    let stroke = Stroke::new(1.7, color);
    let center = response.rect.center();
    let point = |x: f32, y: f32| center + egui::vec2(x, y);
    let line = |coords: &[(f32, f32)]| {
        ui.painter().add(egui::Shape::line(
            coords.iter().map(|&(x, y)| point(x, y)).collect(),
            stroke,
        ));
    };
    match icon {
        ToolbarIcon::Open => {
            line(&[
                (-10.0, -5.0),
                (-10.0, -8.0),
                (-3.0, -8.0),
                (-1.0, -5.0),
                (10.0, -5.0),
            ]);
            line(&[
                (-10.0, -5.0),
                (10.0, -5.0),
                (8.0, 8.0),
                (-9.0, 8.0),
                (-10.0, -5.0),
            ]);
        }
        ToolbarIcon::Save | ToolbarIcon::SaveAs => {
            line(&[
                (-9.0, -9.0),
                (6.0, -9.0),
                (9.0, -6.0),
                (9.0, 9.0),
                (-9.0, 9.0),
                (-9.0, -9.0),
            ]);
            line(&[(-5.0, -9.0), (-5.0, -2.0), (5.0, -2.0), (5.0, -9.0)]);
            line(&[(-5.0, 9.0), (-5.0, 2.0), (5.0, 2.0), (5.0, 9.0)]);
            if matches!(icon, ToolbarIcon::SaveAs) {
                line(&[(10.0, 4.0), (15.0, 4.0)]);
                line(&[(12.5, 1.5), (12.5, 6.5)]);
            }
        }
        ToolbarIcon::Undo => {
            line(&[
                (-9.0, -2.0),
                (-4.0, -7.0),
                (-4.0, -3.0),
                (2.0, -3.0),
                (7.0, 0.0),
                (7.0, 5.0),
                (4.0, 8.0),
            ]);
            line(&[(-9.0, -2.0), (-4.0, 2.0)]);
        }
        ToolbarIcon::Redo => {
            line(&[
                (9.0, -2.0),
                (4.0, -7.0),
                (4.0, -3.0),
                (-2.0, -3.0),
                (-7.0, 0.0),
                (-7.0, 5.0),
                (-4.0, 8.0),
            ]);
            line(&[(9.0, -2.0), (4.0, 2.0)]);
        }
        ToolbarIcon::Select => {
            line(&[
                (-7.0, -10.0),
                (-7.0, 9.0),
                (-2.0, 3.0),
                (2.0, 10.0),
                (5.0, 8.0),
                (1.0, 1.0),
                (8.0, 0.0),
                (-7.0, -10.0),
            ]);
        }
        ToolbarIcon::Lasso => {
            line(&[
                (-9.0, 0.0),
                (-7.0, -5.0),
                (-2.0, -8.0),
                (5.0, -7.0),
                (9.0, -2.0),
                (8.0, 4.0),
                (3.0, 7.0),
                (-4.0, 6.0),
                (-9.0, 0.0),
            ]);
            line(&[(3.0, 7.0), (6.0, 10.0)]);
        }
        ToolbarIcon::Brush => {
            line(&[
                (-8.0, 8.0),
                (-4.0, 9.0),
                (-1.0, 6.0),
                (-3.0, 3.0),
                (-8.0, 8.0),
            ]);
            line(&[(-3.0, 3.0), (6.0, -8.0), (9.0, -5.0), (-1.0, 6.0)]);
        }
        ToolbarIcon::Model => {
            line(&[
                (0.0, -10.0),
                (9.0, -5.0),
                (9.0, 5.0),
                (0.0, 10.0),
                (-9.0, 5.0),
                (-9.0, -5.0),
                (0.0, -10.0),
            ]);
            line(&[(-9.0, -5.0), (0.0, 0.0), (9.0, -5.0)]);
            line(&[(0.0, 0.0), (0.0, 10.0)]);
        }
        ToolbarIcon::Uv => {
            for x in [-8.0, 0.0, 8.0] {
                line(&[(x, -8.0), (x, 8.0)]);
            }
            for y in [-8.0, 0.0, 8.0] {
                line(&[(-8.0, y), (8.0, y)]);
            }
        }
        ToolbarIcon::Fit => {
            line(&[(-9.0, -3.0), (-9.0, -9.0), (-3.0, -9.0)]);
            line(&[(3.0, -9.0), (9.0, -9.0), (9.0, -3.0)]);
            line(&[(9.0, 3.0), (9.0, 9.0), (3.0, 9.0)]);
            line(&[(-3.0, 9.0), (-9.0, 9.0), (-9.0, 3.0)]);
        }
        ToolbarIcon::Wire => {
            line(&[
                (0.0, -10.0),
                (9.0, 0.0),
                (0.0, 10.0),
                (-9.0, 0.0),
                (0.0, -10.0),
            ]);
            line(&[(0.0, -10.0), (0.0, 10.0)]);
            line(&[(-9.0, 0.0), (9.0, 0.0)]);
        }
        ToolbarIcon::Scene => {
            line(&[
                (-10.0, -5.0),
                (0.0, -10.0),
                (10.0, -5.0),
                (0.0, 0.0),
                (-10.0, -5.0),
            ]);
            line(&[(-10.0, 0.0), (0.0, 5.0), (10.0, 0.0)]);
            line(&[(-10.0, 5.0), (0.0, 10.0), (10.0, 5.0)]);
        }
        ToolbarIcon::Settings => {
            ui.painter().circle_stroke(center, 7.0, stroke);
            ui.painter().circle_stroke(center, 2.3, stroke);
            for &(x, y) in &[(-10.0, 0.0), (10.0, 0.0), (0.0, -10.0), (0.0, 10.0)] {
                let inner = point(x * 0.7, y * 0.7);
                ui.painter().line_segment([inner, point(x, y)], stroke);
            }
        }
    }
    response
}

pub(crate) fn apply(ctx: &egui::Context, dark: bool) {
    let mut visuals = if dark {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = GRAPHITE;
        visuals.window_fill = SURFACE;
        visuals.extreme_bg_color = Color32::from_rgb(17, 19, 23);
        visuals.faint_bg_color = Color32::from_rgb(35, 37, 43);
        visuals.widgets.noninteractive.bg_fill = GRAPHITE;
        visuals.widgets.noninteractive.weak_bg_fill = GRAPHITE;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(37, 39, 45);
        visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(34, 36, 42);
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(63, 48, 38);
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(63, 48, 38);
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ORANGE);
        visuals.widgets.active.bg_fill = Color32::from_rgb(132, 64, 24);
        visuals.widgets.active.weak_bg_fill = Color32::from_rgb(132, 64, 24);
        visuals.widgets.active.bg_stroke = Stroke::new(1.0, ORANGE);
        // egui also uses this stroke as the text color on selected buttons.
        visuals.selection.bg_fill = ORANGE;
        visuals.selection.stroke = Stroke::new(1.5, Color32::BLACK);
        visuals.warn_fg_color = ORANGE;
        visuals
    } else {
        egui::Visuals::light()
    };
    visuals.widgets.inactive.corner_radius = CornerRadius::same(5);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(5);
    visuals.widgets.active.corner_radius = CornerRadius::same(5);
    visuals.window_corner_radius = CornerRadius::same(7);
    ctx.set_visuals(visuals);
    ctx.style_mut_of(
        if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        },
        |style| {
            style.spacing.item_spacing = egui::vec2(6.0, 5.0);
            style.spacing.button_padding = egui::vec2(9.0, 6.0);
        },
    );
}
