//! Small user preferences file, independent of a mesh or editing session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::app::SelectionSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowLayout {
    pub tools_width: f32,
    pub outliner_width: f32,
    pub viewport_split: f32,
    pub show_outliner: bool,
}

impl WindowLayout {
    pub fn valid(&self) -> bool {
        (170.0..=700.0).contains(&self.tools_width)
            && (140.0..=260.0).contains(&self.outliner_width)
            && (0.15..=0.85).contains(&self.viewport_split)
    }
}
use serde::{Deserialize, Serialize};

pub const HOTKEY_ACTIONS: [&str; 6] = ["Frame", "Cut", "Weld", "Unfold", "Pack", "Optimize"];
pub const EXTRA_ACTIONS: [&str; 16] = [
    "Frame selection",
    "Select all",
    "Invert selection",
    "Grow selection",
    "Shrink selection",
    "Edge ring",
    "Edge loop",
    "Unhide",
    "Isolate",
    "Hide",
    "Cut and unfold",
    "Open",
    "Save",
    "Undo",
    "Redo",
    "Clear selection",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseBinding {
    pub button: MouseButton,
    pub shift: bool,
    pub alt: bool,
    pub command: bool,
}

impl MouseBinding {
    const fn new(button: MouseButton, shift: bool, alt: bool) -> Self {
        Self {
            button,
            shift,
            alt,
            command: false,
        }
    }

    pub fn display(self) -> String {
        format!(
            "{}{}{}{} drag",
            if self.command { "Ctrl+" } else { "" },
            if self.shift { "Shift+" } else { "" },
            if self.alt { "Alt+" } else { "" },
            match self.button {
                MouseButton::Left => "LMB",
                MouseButton::Middle => "MMB",
                MouseButton::Right => "RMB",
            }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseNavigation {
    pub orbit_3d: MouseBinding,
    #[serde(default = "default_alt_orbit")]
    pub orbit_3d_alt: Option<MouseBinding>,
    pub pan_3d: MouseBinding,
    #[serde(default = "default_alt_pan")]
    pub pan_3d_alt: Option<MouseBinding>,
    pub pan_uv: MouseBinding,
    #[serde(default = "default_alt_uv_pan")]
    pub pan_uv_alt: Option<MouseBinding>,
}

fn default_alt_orbit() -> Option<MouseBinding> {
    Some(MouseBinding::new(MouseButton::Left, false, true))
}

fn default_alt_pan() -> Option<MouseBinding> {
    Some(MouseBinding::new(MouseButton::Right, true, false))
}

fn default_alt_uv_pan() -> Option<MouseBinding> {
    Some(MouseBinding::new(MouseButton::Right, false, false))
}

impl Default for MouseNavigation {
    fn default() -> Self {
        Self {
            orbit_3d: MouseBinding::new(MouseButton::Right, false, false),
            orbit_3d_alt: default_alt_orbit(),
            pan_3d: MouseBinding::new(MouseButton::Middle, false, false),
            pan_3d_alt: default_alt_pan(),
            pan_uv: MouseBinding::new(MouseButton::Middle, false, false),
            pan_uv_alt: default_alt_uv_pan(),
        }
    }
}

impl MouseNavigation {
    pub(crate) fn valid(self) -> bool {
        let orbit = [Some(self.orbit_3d), self.orbit_3d_alt];
        let pan = [Some(self.pan_3d), self.pan_3d_alt];
        orbit
            .iter()
            .flatten()
            .all(|o| pan.iter().flatten().all(|p| o != p))
            && [self.orbit_3d, self.pan_3d, self.pan_uv]
                .into_iter()
                .chain(
                    [self.orbit_3d_alt, self.pan_3d_alt, self.pan_uv_alt]
                        .into_iter()
                        .flatten(),
                )
                .all(|b| b.button != MouseButton::Left || b.shift || b.alt || b.command)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub key: String,
    pub command: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Binding {
    fn new(key: &str, command: bool, shift: bool, alt: bool) -> Self {
        Self {
            key: key.into(),
            command,
            shift,
            alt,
        }
    }

    fn valid(&self) -> bool {
        (self.key.len() == 1 && self.key.as_bytes()[0].is_ascii_uppercase())
            || matches!(self.key.as_str(), "Period" | "Comma" | "Escape")
    }

    pub fn display(&self) -> String {
        format!(
            "{}{}{}{}",
            if self.command { "Ctrl+" } else { "" },
            if self.shift { "Shift+" } else { "" },
            if self.alt { "Alt+" } else { "" },
            self.key
        )
    }
}

fn default_extras() -> [Binding; 16] {
    [
        Binding::new("F", false, true, false),
        Binding::new("A", true, false, false),
        Binding::new("I", true, false, false),
        Binding::new("Period", true, false, false),
        Binding::new("Comma", true, false, false),
        Binding::new("L", false, true, false),
        Binding::new("L", false, false, false),
        Binding::new("H", false, false, true),
        Binding::new("H", false, true, false),
        Binding::new("H", false, false, false),
        Binding::new("C", false, true, false),
        Binding::new("O", true, false, false),
        Binding::new("S", true, false, false),
        Binding::new("Z", true, false, false),
        Binding::new("Z", true, true, false),
        Binding::new("Escape", false, false, false),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkeys {
    pub keys: [char; 6],
    #[serde(default = "default_extras")]
    pub extras: [Binding; 16],
    #[serde(default)]
    pub mouse: MouseNavigation,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            keys: ['F', 'C', 'W', 'U', 'P', 'O'],
            extras: default_extras(),
            mouse: MouseNavigation::default(),
        }
    }
}

impl Hotkeys {
    pub fn maya() -> Self {
        let mut keys = Self::default();
        keys.mouse.orbit_3d = MouseBinding::new(MouseButton::Left, false, true);
        keys.mouse.orbit_3d_alt = None;
        keys.mouse.pan_3d = MouseBinding::new(MouseButton::Middle, false, true);
        keys.mouse.pan_3d_alt = None;
        keys.mouse.pan_uv_alt = None;
        keys
    }
    pub fn max() -> Self {
        let mut extras = default_extras();
        extras[0].key = "Z".into();
        Self {
            keys: ['Z', 'C', 'W', 'U', 'P', 'O'],
            extras,
            mouse: MouseNavigation {
                orbit_3d: MouseBinding::new(MouseButton::Middle, false, true),
                orbit_3d_alt: None,
                pan_3d: MouseBinding::new(MouseButton::Middle, false, false),
                pan_3d_alt: None,
                pan_uv_alt: None,
                ..MouseNavigation::default()
            },
        }
    }
    pub fn blender() -> Self {
        let mut extras = default_extras();
        extras[10].key = "X".into();
        Self {
            keys: ['F', 'X', 'S', 'U', 'P', 'O'],
            extras,
            mouse: MouseNavigation {
                orbit_3d: MouseBinding::new(MouseButton::Middle, false, false),
                orbit_3d_alt: None,
                pan_3d: MouseBinding::new(MouseButton::Middle, true, false),
                pan_3d_alt: None,
                pan_uv_alt: None,
                ..MouseNavigation::default()
            },
        }
    }

    /// Swaps an occupied key so every action keeps a unique binding.
    pub fn assign(&mut self, action: usize, key: char) {
        let old = self.keys[action];
        if let Some(other) = self.keys.iter().position(|&k| k == key) {
            self.keys[other] = old;
        }
        self.keys[action] = key;
    }

    pub fn assign_extra(&mut self, action: usize, binding: Binding) {
        if self.keys.iter().any(|&key| {
            !binding.command && !binding.shift && !binding.alt && binding.key == key.to_string()
        }) {
            return;
        }
        let old = self.extras[action].clone();
        if let Some(other) = self.extras.iter().position(|item| *item == binding) {
            self.extras[other] = old;
        }
        self.extras[action] = binding;
    }

    fn valid(&self) -> bool {
        self.mouse.valid()
            && self
                .keys
                .iter()
                .all(|k| k.is_ascii_uppercase() && *k != 'H' && *k != 'L')
            && self
                .keys
                .iter()
                .enumerate()
                .all(|(i, k)| self.keys[..i].iter().all(|other| other != k))
            && self.extras.iter().all(Binding::valid)
            && self.extras.iter().enumerate().all(|(i, binding)| {
                self.extras[..i].iter().all(|other| other != binding)
                    && !self.keys.iter().any(|&key| {
                        !binding.command
                            && !binding.shift
                            && !binding.alt
                            && binding.key == key.to_string()
                    })
            })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub dark_theme: bool,
    pub wireframe: bool,
    pub overlay_seams: bool,
    pub overlay_borders: bool,
    pub show_outliner: bool,
    pub tools_width: f32,
    pub outliner_width: f32,
    pub viewport_split: f32,
    pub brush_radius: f32,
    pub checker_cells: f32,
    pub grid_visible: bool,
    pub grid_divisions: u32,
    pub texture_path: Option<PathBuf>,
    pub resolution: u32,
    pub density: f64,
    pub padding: f64,
    pub margin: f64,
    pub seam_angle: f64,
    pub select_angle: f64,
    pub pack_rotate: bool,
    pub pack_rescale: bool,
    pub pack_keep_density: bool,
    pub pack_keep_groups: bool,
    pub pack_preserve_stacks: bool,
    pub pack_pixel_aligned: bool,
    pub pack_padding_perfect: bool,
    pub hotkeys: Hotkeys,
    pub selection_sets: BTreeMap<String, Vec<SelectionSet>>,
    pub layout_presets: BTreeMap<String, WindowLayout>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            dark_theme: true,
            wireframe: true,
            overlay_seams: true,
            overlay_borders: true,
            show_outliner: false,
            tools_width: 350.0,
            outliner_width: 210.0,
            viewport_split: 0.5,
            brush_radius: 24.0,
            checker_cells: 16.0,
            grid_visible: true,
            grid_divisions: 10,
            texture_path: None,
            resolution: 2048,
            density: 512.0,
            padding: 4.0,
            margin: 4.0,
            seam_angle: 60.0,
            select_angle: 60.0,
            pack_rotate: true,
            pack_rescale: true,
            pack_keep_density: false,
            pack_keep_groups: true,
            pack_preserve_stacks: true,
            pack_pixel_aligned: false,
            pack_padding_perfect: false,
            hotkeys: Hotkeys::default(),
            selection_sets: BTreeMap::new(),
            layout_presets: BTreeMap::new(),
        }
    }
}

impl Preferences {
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))?;
        Some(base.join("uvcraft").join("preferences.json"))
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let prefs: Self = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if prefs.valid() {
            Ok(prefs)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid UvCraft preference value",
            ))
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if !self.valid() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid UvCraft preference value",
            ));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, bytes)?;
        std::fs::rename(temp, path)
    }

    fn valid(&self) -> bool {
        (2.0..=64.0).contains(&self.checker_cells)
            && (2..=64).contains(&self.grid_divisions)
            && self.resolution > 0
            && self.density.is_finite()
            && self.density > 0.0
            && self.padding.is_finite()
            && self.padding >= 0.0
            && self.margin.is_finite()
            && self.margin >= 0.0
            && self.seam_angle.is_finite()
            && (0.0..=180.0).contains(&self.seam_angle)
            && self.select_angle.is_finite()
            && (0.0..=180.0).contains(&self.select_angle)
            && self.hotkeys.valid()
            && (170.0..=700.0).contains(&self.tools_width)
            && (140.0..=260.0).contains(&self.outliner_width)
            && (0.15..=0.85).contains(&self.viewport_split)
            && (4.0..=128.0).contains(&self.brush_radius)
            && self.layout_presets.values().all(WindowLayout::valid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_presets_keep_orbit_and_pan_distinct() {
        let maya = Hotkeys::maya();
        let max = Hotkeys::max();
        let blender = Hotkeys::blender();
        assert!(maya.valid() && max.valid() && blender.valid());
        assert_eq!(maya.mouse.orbit_3d.display(), "Alt+LMB drag");
        assert_eq!(max.mouse.orbit_3d.display(), "Alt+MMB drag");
        assert_eq!(blender.mouse.orbit_3d.display(), "MMB drag");
        assert_eq!(blender.mouse.pan_3d.display(), "Shift+MMB drag");
        assert_eq!(
            Hotkeys::default().mouse.orbit_3d_alt.unwrap().display(),
            "Alt+LMB drag"
        );
    }

    #[test]
    fn old_hotkey_preferences_receive_default_mouse_navigation() {
        let old = r#"{"keys":["F","C","W","U","P","O"]}"#;
        let loaded: Hotkeys = serde_json::from_str(old).unwrap();
        assert_eq!(loaded.mouse, MouseNavigation::default());
        assert!(loaded.valid());
    }

    #[test]
    fn round_trip_and_invalid_values() {
        let path = std::env::temp_dir().join(format!(
            "uvcraft-preferences-test-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut prefs = Preferences {
            dark_theme: false,
            checker_cells: 24.0,
            grid_visible: false,
            grid_divisions: 16,
            texture_path: Some(PathBuf::from("tile.png")),
            hotkeys: Hotkeys::blender(),
            ..Preferences::default()
        };
        prefs.layout_presets.insert(
            "UV focus".into(),
            WindowLayout {
                tools_width: 210.0,
                outliner_width: 160.0,
                viewport_split: 0.3,
                show_outliner: true,
            },
        );
        prefs.save(&path).unwrap();
        assert_eq!(Preferences::load(&path).unwrap(), prefs);
        std::fs::write(&path, r#"{"checker_cells":0}"#).unwrap();
        assert!(Preferences::load(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hotkey_assignment_swaps_duplicates() {
        let mut keys = Hotkeys::default();
        keys.assign(1, 'W');
        assert_eq!(keys.keys[1], 'W');
        assert_eq!(keys.keys[2], 'C');
        assert!(keys.valid());
        keys.assign_extra(6, Binding::new("J", false, false, false));
        assert_eq!(keys.extras[6].key, "J");
        keys.assign_extra(9, Binding::new("J", false, false, false));
        assert_eq!(keys.extras[9].key, "J");
        assert_eq!(keys.extras[6].key, "H");
        assert!(keys.valid());
    }
}
