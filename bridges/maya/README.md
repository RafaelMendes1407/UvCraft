# UvCraft Maya bridge

Install the `uvcraft` Python extension in Maya's Python environment and add this
directory to Maya's Python path. In Maya's Python Script Editor:

```python
import uvcraft_maya

uvcraft_maya.run({"cmd": "unfold", "border_weight": 2.0})
uvcraft_maya.run([
    {"cmd": "auto_seams", "angle_deg": 60},
    {"cmd": "unfold"},
    {"cmd": "pack", "resolution": 2048},
])
```

The bridge exchanges selected polygon meshes through a temporary OBJ, runs
UvCraft commands, then writes the resulting corner UVs to each mesh's current
UV set. Geometry, face order, materials, other UV sets, and transforms stay in
Maya. To target a named set or specific mesh paths, pass `uv_set="map1"` or
`meshes=["|group|meshShape"]`.

Maya's Python API 2.0 is required. UvCraft works in each mesh's object space,
so transform nodes are not baked into the temporary OBJ. Meshes with partially
unassigned UV corners keep those corners unassigned; a mesh with no UVs receives
UVs when a command creates them.
