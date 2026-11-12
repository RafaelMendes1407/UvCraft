#!/usr/bin/env bash
set -euo pipefail

version=${1:?provide a version or tag}
name="UvCraft-${version}-linux-x86_64"
bundle="dist/${name}"
mkdir -p "$bundle/packaging/linux" "$bundle/blender-addon/uvcraft_blender"
cp target/release/uvcraft-app "$bundle/"
cp README.md LICENSE-MIT LICENSE-APACHE icon.png "$bundle/"
cp packaging/linux/install.sh "$bundle/packaging/linux/"
cp bridges/blender/__init__.py bridges/blender/bridge.py bridges/blender/remote.py "$bundle/blender-addon/uvcraft_blender/"
tar -czf "dist/${name}.tar.gz" -C dist "$name"
rm -r "$bundle"
