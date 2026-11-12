"""Run from Modo's Python interpreter after installing the uvcraft wheel."""

import lx
import uvcraft_modo

commands = [
    {"cmd": "auto_seams"},
    {"cmd": "unfold"},
    {"cmd": "pack", "padding_px": 8},
]

count = uvcraft_modo.run(commands, lx=lx, map_name="Texture")
lx.out("UvCraft updated UVs for %d polygons" % count)
