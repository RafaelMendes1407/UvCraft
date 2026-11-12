# UvCraft

UvCraft is a Rust desktop UV editor with a local Blender bridge. It can mark seams, unwrap UV islands, edit and pack them, visualize distortion, and send UV coordinates and seams back to Blender without changing the 3D mesh geometry.

## Start here

The [user guide](docs/user-guide.md) covers installation, the UV workflow, and the complete Blender round trip. Once the add-on is installed:

1. Open UvCraft and Blender.
2. In UvCraft, click **File → Copy bridge token**.
3. In Blender, select a mesh and open **3D View → Sidebar (`N`) → UvCraft**. Paste the token into **Session token** and click **Connect**.
4. Click **Send Selected**. The mesh appears automatically in UvCraft.
5. Edit the UVs and seams in UvCraft.
6. In Blender, keep the object selected and click **Receive UVs**. The UVs and seams are applied to the original mesh.

Transfers are manual: **Connect** alone does not send a mesh.

## Run from source

Rust 1.95 or newer is required. From the repository root:

```sh
cargo run -p uvcraft-app
```

To open a mesh directly:

```sh
cargo run -p uvcraft-app -- path/to/mesh.obj
```

The editor opens OBJ, glTF, and GLB files. The command line tool supports analysis and automation:

```sh
cargo run -p uvcraft-cli -- analyze path/to/mesh.obj --resolution 2048
cargo run -p uvcraft-cli -- run commands.jsonl
```

See also [Blender add-on setup](bridges/blender/README.md), [UV groups](docs/group-workspace.md), and [Linux installation](packaging/linux/README.md).

## Download a release

The [GitHub Releases page](https://github.com/RafaelMendes1407/UvCraft/releases) provides Linux (`.tar.gz`) and Windows (`.zip`) x86-64 packages. Extract the package and run `uvcraft-app` on Linux or `uvcraft-app.exe` on Windows. On Linux, the extracted `packaging/linux/install.sh` can install a per-user desktop launcher with `sh packaging/linux/install.sh ./uvcraft-app`. Every package includes the Blender add-on source; the application can install it through **File → Install Blender add-on…**.

## Repository layout

- `crates/uvcraft-app`: desktop interface.
- `crates/uvcraft-core`, `uvcraft-algo`, and `uvcraft-commands`: data model, algorithms, and operations.
- `crates/uvcraft-io`: mesh import and export.
- `crates/uvcraft-bridge`: local Blender communication.
- `bridges/blender`: Blender add-on.
- `docs`: user guides; `FEATURES.md` and `features.md`: feature tracking and requirements.

Reference images are kept in `docs/images/`. The application icon remains at the repository root because the binary and Linux packaging embed or install it from there.

License: MIT or Apache-2.0.
