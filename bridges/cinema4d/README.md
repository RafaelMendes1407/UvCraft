# UvCraft bridge for Cinema 4D

The bridge reads a selected `PolygonObject` and its first `UVWTag`, writes a
temporary OBJ, runs commands through `uvcraft.Session`, validates the point
and polygon topology, and writes UVW coordinates back with `UVWTag.SetSlow`.
It does not edit point positions, polygons, or N-gon grouping.

## Install and run

1. Build the `uvcraft` wheel from `crates/uvcraft-py` with Maturin and install
   it into **Cinema 4D's Python interpreter**. The wheel must match Cinema
   4D's Python version and platform.
2. Copy this directory as `uvcraft_cinema4d` into a directory on Cinema 4D's
   Python path, or add its parent to `PYTHONPATH`.
3. Select a `PolygonObject` and run `run_selected.py` from the Script Manager.
   Edit the `commands` list near the top of the script.

The example runs Auto Seams, Unfold, then Pack. Set `selected_only = True` in
the script to target selected polygon islands and write back only those
polygons. Polygon indices are zero based in the Python bridge. A custom list
can also be passed directly:

```python
import c4d
import uvcraft_cinema4d

doc = c4d.documents.GetActiveDocument()
obj = doc.GetActiveObject()
prepared = uvcraft_cinema4d.prepare(obj, [
    {"cmd": "unfold"}, {"cmd": "pack", "padding_px": 8},
], c4d)
doc.StartUndo()
try:
    uvcraft_cinema4d.apply(obj, prepared, c4d, doc)
finally:
    doc.EndUndo()
c4d.EventAdd()
```

The bridge reads local point positions and the first UVW tag. If no UVW tag
exists, it creates one after UvCraft finishes and records that creation in
Cinema 4D's undo stack. It preserves the third UVW component on existing
tags; for triangles, the fourth UVW corner repeats the third. UV continuity
is inferred from equal coordinates at the same point, since `UVWTag` stores
per-polygon coordinates rather than shared UV vertex indices. A seam whose
two sides happen to have exactly equal coordinates cannot be distinguished
during transfer. The object's N-gon grouping is untouched, but UvCraft sees
the underlying triangles and quads returned by `GetAllPolygons()`.

## Test without Cinema 4D

```sh
python3 -m unittest discover -s bridges/cinema4d/tests -v
```
