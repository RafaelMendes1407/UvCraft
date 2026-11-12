# UvCraft bridge for Modo

This bridge reads a Modo mesh layer's point positions, polygon winding and
effective UV coordinates for each polygon corner. It passes an OBJ to
`uvcraft.Session`, validates that point and polygon topology is unchanged, then
writes UVs with `PolygonAccessor.SetMapValue(point_id, map_id, buffer)` and
commits through an editable layer scan. It does not write point positions or
polygon definitions.

## Install and run

1. Install a `uvcraft` wheel built from `crates/uvcraft-py` into Modo's Python
   interpreter. The wheel must match Modo's Python version and platform.
2. Copy this directory as `uvcraft_modo` onto Modo's Python path.
3. Open a mesh item in an active editable layer and run `run_primary.py` in
   Modo's Python interpreter. Edit its command list before running.

Programmatic call:

```python
import lx
import uvcraft_modo

uvcraft_modo.run([{"cmd": "unfold"}, {"cmd": "pack", "padding_px": 8}],
                 lx=lx, map_name="Texture", layer_index=0)
```

`layer_index` indexes the active editable mesh layers, starting at zero.
`selected_polygons` optionally takes zero-based polygon indices. Commands
that accept `islands` target the islands touching that selection unless the
command already specifies islands; only selected polygons are written back.
The map name may refer to an existing UV map or a new one. Existing maps must
have UVs assigned to every polygon corner. On a new map, commands must produce
UVs before writing back.

Modo's UV map stores continuous and discontinuous values. The bridge reads the
effective value per polygon/point pair and writes that pair directly. When
building the temporary OBJ, equal coordinates at the same point are treated
as one UV vertex; a seam with exactly equal coordinates on both sides cannot
be distinguished. The editable layer scan is committed once after all UVs
have been staged and validated. A failure during a host `SetMapValue` call is
not transactionally rolled back; use Modo's undo for host-side failures.

## Validation

```sh
python3 -m unittest discover -s bridges/modo/tests -v
```

The tests mock the SDK calls and do not run inside Modo. An in-host smoke test
is still needed for the installed Modo/Python/uvcraft combination.

The bridge follows Foundry's [reading UVs tutorial](https://learn.foundry.com/modo/developers/latest/SDK/pages/tutorials/Reading%20UV%20Values.html), [layer scan edit example](https://learn.foundry.com/modo/developers/latest/sdk/pages/tutorials/Creating%20a%20Selection%20Set.html) and [mesh API reference](https://learn.foundry.com/modo/developers/latest/SDK/cpp/_rst/lxd_mesh.html).
