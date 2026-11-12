param(
    [Parameter(Mandatory = $true)]
    [string]$Version
)

$ErrorActionPreference = 'Stop'
$name = "UvCraft-$Version-windows-x86_64"
$bundle = Join-Path 'dist' $name
$addon = Join-Path $bundle 'blender-addon/uvcraft_blender'
New-Item -ItemType Directory -Force -Path $addon | Out-Null
Copy-Item 'target/release/uvcraft-app.exe', 'README.md', 'LICENSE-MIT', 'LICENSE-APACHE', 'icon.png' -Destination $bundle
Copy-Item 'bridges/blender/__init__.py', 'bridges/blender/bridge.py', 'bridges/blender/remote.py' -Destination $addon
Compress-Archive -Path $bundle -DestinationPath "dist/$name.zip" -CompressionLevel Optimal -Force
Remove-Item -Recurse -Force $bundle
