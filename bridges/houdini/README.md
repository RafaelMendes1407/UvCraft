# UvCraft bridge for Houdini

The bridge runs inside a Python SOP. Houdini copies the first input's geometry
to the Python SOP output before cooking; the bridge reads closed polygon
primitives, calls UvCraft, and writes only the `uv` **vertex attribute**. It
does not create or delete points or primitives. Non-polygon primitives pass
through unchanged.

## Install

1. Build the `uvcraft` Python wheel from `crates/uvcraft-py` with Maturin and
   install it into **Houdini's Python interpreter**. The wheel must match
   Houdini's platform and Python version.
2. Copy this directory as `uvcraft_houdini` into a directory on Houdini's
   Python module path (for example a `pythonX.Ylibs` directory), or add its
   parent directory to `PYTHONPATH`.
3. Create a Python SOP, connect the mesh to its first input, and paste the
   contents of `python_sop.py` into its Code field.

The example runs `auto_seams`, `unfold`, then `pack`. You may add a string
parameter named `uvcraft_commands` with a JSON object or array of UvCraft
commands. For example:

```json
[{"cmd":"unfold"},{"cmd":"pack","padding_px":8}]
```

An optional string parameter `uvcraft_group` accepts a primitive group
pattern. Commands that accept `islands` will target UV islands touching the
group, and only those primitives' vertex UVs are written back. The input may
have a two- or three-component vertex `uv` attribute, a point `uv` attribute,
or no UVs. A vertex attribute is created when needed. If input UVs are on
points, the original point attribute is retained under the name
`uvcraft_source_uv` (or a numbered variant) because HOM cannot create a vertex
attribute with the same name. Existing third UV components are preserved.

The Python API can also be called directly from a Python SOP:

```python
import hou
import uvcraft_houdini

uvcraft_houdini.run(hou.pwd().geometry(), [
    {"cmd": "unfold"},
    {"cmd": "pack"},
])
```

The bridge uses SOP point positions. UVs are stored by polygon vertex, so
seams remain independent where UV values differ. It validates point, polygon
and corner order before writing UVs. No OBJ import node is needed; the OBJ is
temporary and only used to transfer geometry to the UvCraft Python session.

## Test without Houdini

```sh
python3 -m unittest discover -s bridges/houdini/tests -v
```
