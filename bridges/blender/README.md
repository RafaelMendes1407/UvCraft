# UvCraft bridge for Blender

This add-on sends the selected mesh objects to UvCraft, runs UV commands, and
copies the resulting UV coordinates back to Blender. It preserves each mesh's
vertices, faces, polygon order, and loop order. Linked objects sharing one mesh
datablock are processed once.

## Optional local HTTP bridge (first protocol version)

The **Connection** section talks to the running UvCraft desktop application
without installing the `uvcraft` Python extension. Open UvCraft, choose
**File → Copy bridge token**, paste the token into **Session token** in Blender,
then click **Connect** and **Send Selected**. Edit the received mesh in the
UvCraft window, then click **Receive UVs** in Blender. **Refresh Status** checks
the selected objects' connection and topology. **Update Mesh** sends
Blender's current mesh and UVs to its existing session. **Disconnect** ends
the sessions. Each selected mesh datablock gets its own session; linked
objects sharing one datablock are sent once. The most recently sent object is
shown in the UvCraft window. To edit another object, select it in Blender and
use **Update Mesh** (or send it again).

The server defaults to `http://127.0.0.1:8766`. Set
`UVCRAFT_BRIDGE_ADDR=127.0.0.1:PORT` before launching the desktop application
to choose another loopback port, then enter that URL in Blender. It accepts
up to 16 MiB per request. The protocol validates the full face/corner vertex
order before applying UVs or seams to Blender. **Preserve Existing UVs**
controls whether sent meshes retain their current UV coordinates. Topology
validation stays enabled for safe transfers. Auto Sync is reserved for a
future version; use **Receive UVs** after editing.

For headless operations, run `cargo run -p uvcraft-cli -- bridge` and use its
printed token. That process has a `/commands` endpoint. Run the CLI bridge
and the desktop bridge on different ports if both are needed. Closing Blender
or the server ends the association; re-send the mesh to start a new session.
The in-process **Run UvCraft** operator remains available for one-step
operations with the Python extension.

## Install

From the UvCraft desktop application, open **File → Install Blender add-on…**
and choose a detected Blender version. If Blender uses a custom scripts
directory, choose its `addons` folder instead. UvCraft copies the bundled
add-on files to `uvcraft_blender`; restart Blender, then enable **UvCraft UV
Bridge** in Blender's add-on preferences. This installs the local HTTP bridge
without the optional Python extension.

Alternatively, install the files manually:

1. Copy this directory to Blender's `scripts/addons/uvcraft_blender` directory
   (or zip it with `uvcraft_blender/__init__.py` at the archive root and use
   Blender's **Install from Disk**).
2. Enable **UvCraft UV Bridge** in Blender's add-on preferences.

The local HTTP connection needs no Python extension. For the separate
**Run UvCraft** operator, build the `uvcraft` Python wheel from
`crates/uvcraft-py` with Maturin and install it into **Blender's Python
interpreter**. The interpreter used by a system terminal may be different
from Blender's.

The wheel must support Blender's Python version and platform. The UvCraft
package is built with `abi3` for Python 3.9 and newer.

## Use

Select one or more mesh objects. Open **3D View → Sidebar → UvCraft**, choose
**Pack**, **Unfold + Pack**, **Auto Seams + Unfold + Pack**, or **Custom JSON**,
then click **Run UvCraft**. The custom field accepts one command object or an
array, for example:

```json
[{"cmd":"auto_seams"},{"cmd":"unfold"},{"cmd":"pack","padding_px":8}]
```

**Selected faces** and **Selected UVs** send all mesh faces to UvCraft so
topology remains stable, limit commands with an `islands` option to UV islands
touching the selection, and write back only selected face loops or UV corners.
In Edit Mode, the add-on
temporarily enters Object Mode to read and write mesh data, then restores Edit
Mode. Blender's Undo reverses the UV changes.

The bridge uses the mesh's local vertex coordinates. Apply object transforms
before using physical measurements or real-scale commands if world-space size
matters. It does not evaluate modifiers; the original mesh topology is kept.

## Test without Blender

```sh
python3 -m unittest discover -s bridges/blender/tests -v
```
