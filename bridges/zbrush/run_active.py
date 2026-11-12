"""Run from ZBrush 2026's ZScript > Python > Load Script command."""

import uvcraft_zbrush

commands = [
    {"cmd": "auto_seams"},
    {"cmd": "unfold"},
    {"cmd": "pack", "padding_px": 8},
]

count = uvcraft_zbrush.run(commands)
print("UvCraft updated UVs for %d polygons" % count)
