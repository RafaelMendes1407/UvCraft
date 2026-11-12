# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

UvCraft is an open-source UV unwrapping tool written in Rust, modeled on RizomUV. It is licensed `MIT OR Apache-2.0`. Every dependency must use an OSI license compatible with that, so proprietary SDKs (e.g. the Autodesk FBX SDK) are off-limits.

Planning documents:
- `FEATURES.md`: the full feature checklist, each item tagged with milestone (M0–M4), difficulty and approach. Tick items (`- [x]`) when they are fully implemented.
- `UvCraft_Technical_Analysis.md`: how to build each feature (algorithms, papers, crate choices, risks, build order).
- `RizomUV_Features.md`: the source survey of what RizomUV does.

Current state: M0 is mostly done. Most of M1 is done: cut/weld, auto seams, projections (planar/best/box/cylindrical/spherical), LSCM, raster + shelf packing, texel density, glTF vertex splitting, plus a first desktop GUI (`uvcraft-app`). Also done:
- pins;
- optimize (SLIM flip-free by default, or ARAP);
- handle (genus) cutting;
- packing around fixed islands;
- `.gltf` ↔ `.glb` conversion;
- OBJ ↔ glTF conversion;
- occlusion-aware 3D selection.

Current gaps are tracked in `FEATURES.md`; notable ones include full camera perspective projection, large-mesh rendering, GPU picking/packing, and remaining format bridges.

## Commands

```sh
cargo build
cargo test                                  # all crates
cargo test -p uvcraft-io                    # one crate
cargo test -p uvcraft-io reports_line_numbers   # tests whose name contains this
cargo clippy --all-targets                  # must be warning-free
cargo fmt
cargo run -p uvcraft-cli -- analyze mesh.obj|.gltf|.glb --resolution 2048 [--padding 4] [--json]
cargo run -p uvcraft-cli -- run script.jsonl [--log out.jsonl] [--macro m.jsonl] [--json]
cargo run -p uvcraft-cli -- serve [--bind 127.0.0.1:8765] # line-delimited JSON-RPC 2.0; one session per client
(cd crates/uvcraft-py && maturin develop)   # Python module `uvcraft` (needs the `python` feature, which maturin enables)
cargo run -p uvcraft-app -- [mesh.glb] [--run script.jsonl] [--screenshot out.png]   # GUI
```

The TCP server accepts one JSON-RPC request per line. `execute` takes `{"command":{"cmd":"load","path":"a.obj"}}` in `params`; `status` and `log` need no params. Every client has its own session. The default address listens on loopback only.

Scripts are JSON Lines (or a JSON array) of commands tagged by `cmd`, e.g. `{"cmd":"load","path":"a.obj"}`, `{"cmd":"transform","islands":[0],"rotate_deg":90}`, `{"cmd":"auto_seams","angle_deg":60}`, `{"cmd":"project","mode":"box"}` (or `best`, `{"planar":"x"}`, `{"cylindrical":"y"}`, `{"spherical":"y"}`), `{"cmd":"unfold"}` (or `"method":"abf"` for ABF++), `{"cmd":"optimize","method":"slim","iterations":20}` (or `arap`), `{"cmd":"pack","padding_px":4,"method":"raster","rescale":true,"around_others":true}` (or `shelf`; optional `"region":[u0,v0,u1,v1]`, `"padding_uv"`/`"margin_uv"`, `"rotation_step_deg":45`, `"mirror":true`, `"quality":"low|normal|high"`, `"texel_density":512`, UDIM `"tiles":4` / `"tiles_u":10`), `{"cmd":"set_texel_density","density":512,"resolution":2048}`, `{"cmd":"cut","edges":[..]}` / `weld`, `{"cmd":"pin","vertices":[..],"pinned":true}` / `clear_pins`, `{"cmd":"mirror","islands":[0],"axis":"u","at":0.5}` (`at` omitted: each island's center), `{"cmd":"orient","mode":"min_bbox","vertical":false}` (or `longest_edge`, `world_axis`, `{"edge":N}`), `{"cmd":"align","axis":"u","to":"min"}` (`max`/`center`, optional `value`), `{"cmd":"distribute","axis":"u","by":"gaps"}` (or `centers`, optional fixed `gap`), `{"cmd":"snap","to":"pixel","resolution":2048}` (or `pixel_center`, `bbox`, `vertex` with `tolerance_px`; optional `step`), `{"cmd":"undo"}`, `{"cmd":"analyze","resolution":2048,"padding_px":4}` (padding check skipped when 0, the default; the report includes overlaps, padding violations and `mesh_problems`), `{"cmd":"load_uvs","path":"other.obj"}` (UVs from a file with the same face/corner layout; undoable), `{"cmd":"mosaic","charts":8,"max_error":0.1,"iterations":10,"unfold":true}` (all optional; no `charts` = automatic), `{"cmd":"save_macro","path":"m.jsonl"}`, `{"cmd":"similar","islands":[0],"mode":"similar","tolerance":0.05}`, `{"cmd":"stack","islands":[0],"mode":"similar","tolerance":0.05}`, `{"cmd":"unstack"}`, `{"cmd":"match","source":0,"targets":[1,2]}`, `{"cmd":"cut_unfold","edges":[..]}`, `{"cmd":"cut_loop","edge":N}` / `{"cmd":"cut_path","vertices":[a,b,..]}` / `{"cmd":"cut_border","faces":[..]}` (each with optional `"unfold":true`), `{"cmd":"save","path":"b.obj"}`. Lines starting with `#` or `//` are ignored. Paths are relative to the working directory.

## Architecture

Cargo workspace in `crates/`. Dependency direction: `core ← algo`, `core ← io`, `{core, algo, io} ← commands ← {cli, app, py}` (app = eframe/egui + wgpu; py = pyo3, see below).

The central rule: **the 3D mesh is immutable; only the UV layer changes.**
- `uvcraft-core::Mesh` stores faces as CSR arrays. A corner is also a half-edge (`corner_vertex(c)` → `corner_vertex(next(c))`). Twins and edges are built once at construction. Face and corner order are exactly the input order.
- `UvLayer` = `corner_uv` (UV vertex index per corner) + `uvs`. A seam is an edge whose two faces reference different UV indices (`UvLayer::edge_kind`). So cut/weld only rewrite `corner_uv`, and topology-preserving export is automatic.
- `Islands` are derived (union-find over interior edges). `Document` caches them and drops the cache whenever `corner_uv` changes.
- Invariant: each UV vertex belongs to exactly one island and one mesh vertex. `Islands::split_shared_uvs` enforces it on load and after every topology-changing edit (`Session::edit`). Without it, moving one island drags another, and projections overwrite each other. Exporters must therefore compare UV *mappings* (`UvLayer::same_mapping`), not indices, to detect "unchanged".

`uvcraft-commands` is the only entry point for actions. Every action is a serde-serializable `Command` executed by `Session::execute`, which drives undo/redo (full `UvLayer` snapshots), the replayable command log and the CLI. UI, Python and macros must go through it too, never call `algo` directly. New actions = new `Command` variant + `apply` arm. Mutating ones go through `Session::edit` so they get an undo snapshot. Failed commands are not logged.
- Pins are stored per mesh vertex in `Document`. Mesh vertices never change, so pins survive cut, weld and compact unchanged.
- Undo snapshots hold the UV layer plus the pins.
- `unfold`:
  - two or more pins become hard LSCM constraints at their current UVs (no rescaling);
  - with one pin, the island is placed around it.

Macros (`history.rs`): `Session` keeps the command behind each undo/redo snapshot (`history`/`undone`, same lengths as the stacks). `macro_commands()` / `save_macro` / CLI `--macro` give the edits behind the current UVs since the last load: undone edits, no-op edits, load/save/analyze are left out. `--log` is the raw log (everything, including undo/redo).

`uvcraft-py` (lib name `uvcraft`, cdylib + rlib, abi3-py39) is the Python module. `lib.rs` is Python-free (JSON in/out over `Session::execute`, tested by plain `cargo test`). `python.rs` holds the pyo3 class behind the off-by-default `python` feature, so workspace builds never need Python or libpython. It converts Python values through the `json` module. `Session` has `execute(dict|str)`, `run_script`/`run_file`, one convenience method per command (keyword options pass through), `log()`, `macro_commands()`, `islands()`, `uvs()`, `corner_uv()`, counts. Command errors raise `uvcraft.UvCraftError`; bad JSON raises `ValueError`. To check it: `PYO3_BUILD_EXTENSION_MODULE=1 cargo clippy -p uvcraft-py --features python --all-targets`; `tests/smoke.py` runs against a built module.

`uvcraft-algo` holds pure functions over `(mesh, uv, islands, params)` with no I/O. The modules:
- `seams`: cut/weld, sharp edges, shortest paths and `cut_to_disk`. Weld aligns the smaller island rigidly (Procrustes) before merging.
  - `cut_to_disk` slits closed islands, joins boundary loops, then opens handles with `handle_cuts`.
  - `handle_cuts` cuts the complement of a maximum spanning tree of the faces, then prunes dangling edges.
- `projection`: planar, best-fit and box projection.
- `mosaic`: VSA (L2,1 normals, Lloyd + priority-queue flood, seeds at worst faces) over interior edges only (existing seams are kept), then merges smooth neighbors that fit a cylinder (or cheapest merges down to `charts`), smooths teeth, cuts chart borders and `cut_to_disk`.
- `unfold`: LSCM through a faer sparse Cholesky, parallel per island with rayon. It pins two far-apart boundary vertices and keeps the island's area and center.
- `abf`: ABF++ angle optimization (Newton with the ABF++ block elimination, one sparse Cholesky per step). `unfold` with `method: abf` rebuilds UVs from the angles by angle-based LSCM, with the same pins and placement; it falls back to LSCM (reported) if the angles do not converge.
- `projection`'s cylindrical and spherical modes put the wrap seam in each island's largest angular gap. Corners on the axis (poles) each get their own UV.
- `pack`: two methods.
  - `pack::raster` (default): conservative bitmask rasterization; the grid stores islands dilated by the padding; bottom-left fit trying 4 rotations. Several scales are tried in parallel, on a coarse grid and then a fine one.
  - Shelf: bounding boxes on shelves; it is also the raster fallback.
  - When packing a subset with `around_others`, the other islands are pre-rasterized into the grid as obstacles.
  - `rescale: false` caps the scale at 1.
  - Options (all optional, defaults = old behavior): `region` (rectangle; margin inside it), `padding_uv`/`margin_uv`, `rotation_step_deg` (raster transforms are `raster::Xform`: quarter turns stay exact), `mirror`, `quality` (grid cells + `raster::Effort` trials; `normal` = old), `texel_density` (set TD first, then keep size).
  - Multi-tile (`tiles`/`tiles_u`): raster only, scale capped at 1; first-fit across tiles opened row-major (`tiles_u` per row, default 10), tile `t` offset by `(t % cols, t / cols)`; `pack::udim` names them.
  - Also holds texel density.
- `slim`: SLIM on the symmetric Dirichlet energy.
  - It starts from the current UVs if they are flip-free, else from a Tutte embedding (disks only).
  - The line search is capped below the first triangle inversion, so no triangle ever flips.
  - It takes a `method` from `optimize::OptimizeMethod` (the default).
- `optimize`: ARAP (local/global). It uses a clamped cotangent Laplacian, factorized once; the 2D rotations are closed-form. It keeps the island's area and center, and honors pins like `unfold` does.
- `validate`: mesh problems (non-manifold edges/vertices, duplicate vertices, degenerate faces, repeated vertices, inconsistent winding, isolated vertices, components) as sorted index lists. `Document` computes them once at load; the `analyze` command attaches them to its report.
- `layout`: rigid island layout ops: mirror, orient (min-area box via `pack::min_area_rect_angle`, longest/selected edge, world axis projected through the triangle Jacobians), align, distribute, snap (grid/texel center/bbox/other islands' vertices).
- `select`: selection queries over boolean masks per `Elem` (vertex/edge/face/island): grow/shrink, conversion (touching or contained), edge loop/ring (quad traversal, straightest-edge fallback on triangles), paths, borders, flood by angle. The GUI keeps the masks (and hidden faces) itself; the `cut_*` commands use these queries.
- `analysis`: gets distortion from the singular values of each triangle's 3D→UV Jacobian, normalized by island scale, and fan-triangulates n-gons.
- `checks`: overlap detection (uniform-grid broad phase over fan triangles, SAT with a touch tolerance; triangles sharing a UV vertex are skipped), padding violations (boundary-segment distances, grid accelerated; overlapping island pairs count as distance 0), flipped faces, out-of-tile islands. `analysis::analyze_with_padding` folds them into the report.
- `stack`: similar islands. An exact match is a face/UV-vertex isomorphism found by BFS from a seed face pair (all rotations and mirrors; the seed is the face with the rarest local invariant); the best 3D fit wins, and UVs are copied exactly. Otherwise the target's longest UV boundary loop is fitted onto the reference's (2D Kabsch with scale). `unstack` moves overlapping islands into a row.

`uvcraft-io::obj` keeps the original bytes. With unchanged UVs, export is byte-identical. Otherwise it drops all `vt` lines, writes one `vt` block before the first `vt`/`f` line, and rewrites only the `vt` index of `f` tokens (`v` and `vn` text kept verbatim, including relative indices). `l` lines lose their `vt` refs. Corners without `vt` get a shared placeholder UV per vertex at (0, 0).

`uvcraft-io::gltf` (own code over `serde_json` with `preserve_order`, no glTF crate) handles `.gltf` (external or data-URI buffers) and `.glb`:
- Mesh vertices are welded by exact POSITION per glTF mesh. UV vertices are welded by (mesh vertex, UV value), so normal-only splits are not seams.
- Each glTF vertex is a *slot* holding one UV. Export writes changed slots into the TEXCOORD_0 bytes in place, updates accessor min/max if present, and adds a new accessor for primitives that had no UVs.
- If two corners of one slot need different UVs, export duplicates the slot and every vertex attribute before writing the changed UVs.
- V is flipped (`v = 1 - v_gltf`). Values are f32 on disk.
- When saving to another directory, modified buffers are written as `<stem>.bin` next to the output, and image/buffer URIs are re-pointed relative to it. Kept buffers are never overwritten.

`uvcraft_io::read`/`write` pick the format by extension.
- Save can convert between `.gltf` and `.glb`.
  - To `.glb`: all buffers are merged into the BIN chunk, 4-byte aligned, and the bufferViews are re-pointed.
  - To `.gltf`: the BIN chunk is written as `<stem>.bin`.
  - Unknown GLB chunks survive only a `.glb` → `.glb` save.
- OBJ ↔ glTF (`uvcraft-io::convert`) writes a fresh file.
  - OBJ → `.gltf`/`.glb`: one glTF mesh per run of consecutive faces of the same `o`, one primitive per run of the same `usemtl` (so face order is kept). Polygons are fan-triangulated. One glTF vertex per unique (vertex, UV, normal) per primitive; missing `vn` get the face normal. Materials carry only their name. Coincident-UV seams and f32-equal positions merge on reload.
  - glTF → `.obj`: `v` in mesh order, `vt` = the UV layer, unique `vn`, `f` in the exact face/corner order, `o` per glTF mesh, `usemtl` per material (no `.mtl`). A mesh used by exactly one non-skinned node with a positive-determinant world transform is written in world space; otherwise in local space. Point/line primitives are dropped.

`uvcraft-app` is the GUI.
- `app.rs`: egui 0.37 panels; every action goes through `Session::execute`.
- `render.rs`: wgpu offscreen MSAA targets shown via `register_native_texture`.
- `shader.wgsl`: one vertex layout for both views. `params.x` selects 3D or UV placement.
- `scene.rs`: render buffers and CPU picking. `IdBuffer` is a CPU depth-tested face-ID raster, used for occlusion-aware box/lasso selection in 3D (unless X-ray is on).
- `camera.rs`: uses `glam::camera::rh::proj::directx`, because glam 0.34 has no `Mat4::perspective_rh`.

The GUI keeps a command queue.
- Heavy commands (`app::is_heavy`: load, save, project, auto_seams, unfold, pack) run on a worker thread. The worker takes the `Session` with `mem::take`, runs the command, builds the new `Scene`, then hands both back.
- While a command runs, `session.document()` is `None`, so the panels are disabled.
- A failed command clears the queue, which stops a script.
- The GUI rebuilds all GPU buffers after each command.

## Conventions

- Storage uses `u32` indices (`NONE = u32::MAX` sentinel); public APIs take and return `usize`.
- Geometry is `f64` (`glam::DVec2`/`DVec3`).
- Algorithms must be deterministic: never let `HashMap`/`HashSet` iteration order reach geometry (use `BTree*` or sort). Packing amplifies 1e-8 differences into different layouts.
- Errors use `thiserror` enums per crate; `unsafe` is forbidden workspace-wide.

