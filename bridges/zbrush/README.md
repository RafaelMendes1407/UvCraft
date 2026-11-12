# UvCraft bridge for ZBrush 2026

The bridge exports the active SubTool as OBJ using the official
`zbrush.commands` Python API, runs commands through `uvcraft.Session`, checks
vertex and polygon corner order, and imports an OBJ with the original geometry
records and new UV records. This follows Maxon's workflow for creating UVs
outside ZBrush and importing the model at subdivision level 1.

## Install and run

1. Build `uvcraft` from `crates/uvcraft-py` for ZBrush 2026's Python version
   and install it into the interpreter used by ZBrush.
2. Copy this folder as `uvcraft_zbrush` onto ZBrush's Python path.
3. Select the target polygon SubTool and set **Tool > Geometry > SDiv** to 1.
   Save a separate copy of the ZBrush project before the first import. The
   bridge requires a readable SDiv control and identifiable active SubTool;
   it checks the Tool path, SubTool index and SubTool ID before export and
   again immediately before import.
4. Run `run_active.py` with **ZScript > Python > Load Script**. Edit its command
   list as needed. The bridge processes only the active SubTool.

You can also call `uvcraft_zbrush.run([{"cmd": "unfold"}, {"cmd": "pack"}])`
from a ZBrush Python script. For an offline handoff, call
`process_obj(source_obj, destination_obj, commands)` from regular Python and
import `destination_obj` manually in ZBrush.

The generated OBJ retains original `v` records, polygon vertex order, groups,
materials and normals where present; it replaces `vt` records and face UV
references. It cannot guarantee ZBrush will preserve all sculpt details,
layers, masks, Polygroups or an existing Morph Target during `Tool:Import`.
Maxon's documented method for restoring any changed point positions after UV
import uses **Tool > Morph Target > StoreMT** before importing and **Switch**
afterward. The bridge does not manipulate Morph Targets because a SubTool may
already use its single Morph Target. If the SubTool has no Morph Target and you
need this recovery path, store one at SDiv 1 before running the bridge, then
inspect the result before using Switch. Never overwrite an existing Morph
Target. Verify the imported UVs and the highest subdivision's sculpt, layers,
masks and Polygroups on the saved project copy.

The in-host prerequisites are ZBrush 2026 desktop with `zbrush.commands`
(`get_subtool_count`, `get_active_tool_path`, `get_active_subtool_index`,
`get_subtool_id`, `exists`, `get`,
`set_next_filename`, `press`), a `uvcraft` Python extension built for ZBrush's
Python ABI, a selected SubTool whose SDiv control reads exactly 1, and a saved
project copy. The bridge aborts if the active SubTool or SDiv changes while
UvCraft processes the OBJ.

## Status and tests

```sh
python3 -m unittest discover -s bridges/zbrush/tests -v
```

The OBJ processing and mocked ZBrush command calls are tested. An in-host
roundtrip test on a SubTool with subdivision levels and sculpt data is still
required before claiming a topology-preserving production bridge; the feature
checkbox remains open.

References: [Maxon Python scripting](https://help.maxon.net/zbr/en-us/Content/html/reference-guide/zscript/zscript-python.html), [external UV workflow](https://help.maxon.net/zbr/en-us/Content/html/user-guide/3d-modeling/painting-your-model/texture-maps/texture-maps.html), and [OBJ import/export support](https://help.maxon.net/zbr/en-us/Content/html/features/main-features/import-export/import-export.html).
