//! Install the bundled Blender add-on into a user add-ons directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const ADDON_FILES: [(&str, &str); 3] = [
    ("__init__.py", include_str!("../../../bridges/blender/__init__.py")),
    ("bridge.py", include_str!("../../../bridges/blender/bridge.py")),
    ("remote.py", include_str!("../../../bridges/blender/remote.py")),
];

/// A Blender profile's version and its `scripts/addons` directory.
pub fn profiles_in(root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(root) else { return Vec::new(); };
    let mut profiles: Vec<_> = entries.flatten().filter_map(|entry| {
        let version = entry.file_name().to_string_lossy().into_owned();
        let parts: Vec<u32> = version.split('.').map(str::parse).collect::<Result<_, _>>().ok()?;
        if parts.len() < 2 || !entry.file_type().ok()?.is_dir() { return None; }
        Some((parts, version, entry.path().join("scripts/addons")))
    }).collect();
    profiles.sort_by(|a, b| b.0.cmp(&a.0));
    profiles.into_iter().map(|(_, version, path)|
        (format!("Blender {version}"), path)).collect()
}

fn config_root() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        return std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config")))
            .map(|root| root.join("blender"));
    }
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("APPDATA").filter(|value| !value.is_empty()).map(|root|
            PathBuf::from(root).join("Blender Foundation/Blender"));
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME").filter(|value| !value.is_empty()).map(|home|
            PathBuf::from(home).join("Library/Application Support/Blender"));
    }
    #[allow(unreachable_code)]
    None
}

pub fn discover_targets() -> Vec<(String, PathBuf)> {
    let mut targets = Vec::new();
    if let Some(scripts) = std::env::var_os("BLENDER_USER_SCRIPTS")
        .filter(|value| !value.is_empty()) {
        targets.push(("Blender custom scripts".to_owned(),
            PathBuf::from(scripts).join("addons")));
    }
    if let Some(root) = config_root() {
        targets.extend(profiles_in(&root));
    }
    targets
}

pub fn install_into(addons_dir: &Path) -> io::Result<PathBuf> {
    let destination = addons_dir.join("uvcraft_blender");
    fs::create_dir_all(&destination)?;
    for (name, contents) in ADDON_FILES {
        fs::write(destination.join(name), contents)?;
    }
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_versioned_profiles_and_installs_the_bundled_addon() {
        let root = std::env::temp_dir().join(format!(
            "uvcraft-blender-addon-test-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap().as_nanos()));
        fs::create_dir_all(root.join("4.2")).unwrap();
        fs::create_dir_all(root.join("5.3")).unwrap();
        fs::create_dir_all(root.join("notes")).unwrap();
        let profiles = profiles_in(&root);
        assert_eq!(profiles.iter().map(|entry| entry.0.as_str()).collect::<Vec<_>>(),
            ["Blender 5.3", "Blender 4.2"]);
        let destination = install_into(&profiles[0].1).unwrap();
        assert!(destination.join("__init__.py").is_file());
        assert!(fs::read_to_string(destination.join("bridge.py")).unwrap()
            .contains("import"));
        assert!(destination.join("remote.py").is_file());
        fs::remove_dir_all(root).unwrap();
    }
}
