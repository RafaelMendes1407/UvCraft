# UvCraft bridge for 3ds Max

The bridge edits a selected **Editable Poly base object** through `pymxs`.
It transfers the polygon faces and a texture map channel to a temporary OBJ,
runs UvCraft commands, validates the returned topology, then updates only the
map vertices and map faces. Mesh vertices and polygon faces are not changed.
N-gons and existing UV seam splits are retained during transfer.

## Install and run

1. Build the `uvcraft` wheel from `crates/uvcraft-py` with Maturin and install
   it into **3ds Max's Python interpreter**. The wheel must match its platform
   and Python version.
2. Copy this directory as `uvcraft_3dsmax` into a directory on 3ds Max's
   Python path, or add its parent directory to `PYTHONPATH`.
3. Select one Editable Poly node and run `run_selected.py` from the Python
   Script Editor. Change the `commands` list and `channel` near the top of
   that script as needed. Channel `1` is the default texture map channel;
   channel `0` is vertex color and is deliberately rejected.

The example runs Auto Seams, Unfold, then Pack. A custom command list may be
passed directly to `prepare` or `run`:

```python
from pymxs import runtime as rt
import uvcraft_3dsmax

poly = rt.selection[0].baseObject
uvcraft_3dsmax.run(poly, [{"cmd": "unfold"}, {"cmd": "pack"}], rt, channel=1)
```

For a subset, pass 1-based Editable Poly face IDs as `selected_faces={1, 2}`.
Commands with an `islands` argument target islands touching those faces, and
unselected map faces and map vertices are kept. The sample script wraps the
write in a 3ds Max undo block. If `pymxs.undo` rolls back due to an error, the
sample detects that and reports a failure.

`polyop.setMapSupport` creates a map face array when a channel has no UV map.
The bridge then sets map vertices and map faces. It does not call
`polyop.setNumMapFaces` on channels 0 or 1. The third UVW component is retained
from the first source corner when possible.

The bridge reads the base Editable Poly object, so modifiers above it are not
evaluated. It does not convert another object type into Editable Poly. The
script handles one selected node; for multiple nodes, call the bridge once
per base object. Geometry and map edits must run on 3ds Max's main thread.

## Test without 3ds Max

```sh
python3 -m unittest discover -s bridges/3dsmax/tests -v
```
