#!/bin/sh
# Install a per-user UvCraft launcher and the existing project icon.
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
data_dir=${XDG_DATA_HOME:-"$HOME/.local/share"}
bin_dir=${XDG_BIN_HOME:-"$HOME/.local/bin"}
binary=${1:-"$project_dir/target/release/uvcraft-app"}

if [ ! -f "$binary" ]; then
    printf 'UvCraft binary not found: %s\nBuild it with: cargo build --release -p uvcraft-app\n' "$binary" >&2
    exit 1
fi

install -Dm755 "$binary" "$bin_dir/uvcraft-app"
install -Dm644 "$project_dir/icon.png" "$data_dir/pixmaps/uvcraft.png"
install -d "$data_dir/applications"
cat > "$data_dir/applications/uvcraft.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=UvCraft
Comment=UV editing for 3D meshes
Exec="$bin_dir/uvcraft-app" %f
Icon=uvcraft
Terminal=false
Categories=Graphics;3DGraphics;
MimeType=model/obj;model/gltf+json;model/gltf-binary;
EOF
printf 'Installed UvCraft in %s and its launcher in %s\n' "$bin_dir" "$data_dir/applications"
