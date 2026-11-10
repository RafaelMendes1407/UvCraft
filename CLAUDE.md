# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

UvCraft is an open-source UV unwrapping tool written in Rust, modeled on RizomUV. It is licensed `MIT OR Apache-2.0`. Every dependency must use an OSI license compatible with that, so proprietary SDKs (e.g. the Autodesk FBX SDK) are off-limits.

Planning documents:
- `FEATURES.md`: the full feature checklist, each item tagged with milestone (M0–M4), difficulty and approach. Tick items (`- [x]`) when they are fully implemented.
- `UvCraft_Technical_Analysis.md`: how to build each feature (algorithms, papers, crate choices, risks, build order).
- `RizomUV_Features.md`: the source survey of what RizomUV does.

Current state: M0 is mostly done. Most of M1 is done: cut/weld, auto seams, projections (planar/best/box/cylindrical/spherical), LSCM, raster + shelf packing, texel density, glTF vertex splitting, plus a first desktop GUI (`uvcraft-app`). Still missing: user pins, SLIM, genus handling in `cut_to_disk`.

## Commands

```sh
cargo build
cargo test                                  # all crates
cargo test -p uvcraft-io                    # one crate
cargo test -p uvcraft-io reports_line_numbers   # tests whose name contains this
cargo clippy --all-targets                  # must be warning-free
cargo fmt
cargo run -p uvcraft-cli -- analyze mesh.obj|.gltf|.glb --resolution 2048 [--json]
cargo run -p uvcraft-cli -- run script.jsonl [--log out.jsonl] [--json]
cargo run -p uvcraft-app -- [mesh.glb] [--run script.jsonl] [--screenshot out.png]   # GUI
```

Scripts are JSON Lines (or a JSON array) of commands tagged by `cmd`, e.g. `{"cmd":"load","path":"a.obj"}`, `{"cmd":"transform","islands":[0],"rotate_deg":90}`, `{"cmd":"auto_seams","angle_deg":60}`, `{"cmd":"project","mode":"box"}` (or `best`, `{"planar":"x"}`, `{"cylindrical":"y"}`, `{"spherical":"y"}`), `{"cmd":"unfold"}`, `{"cmd":"pack","padding_px":4,"method":"raster"}` (or `shelf`), `{"cmd":"set_texel_density","density":512,"resolution":2048}`, `{"cmd":"cut","edges":[..]}` / `weld`, `{"cmd":"undo"}`, `{"cmd":"analyze"}`, `{"cmd":"save","path":"b.obj"}`. Lines starting with `#` or `//` are ignored. Paths are relative to the working directory.

## Architecture

Cargo workspace in `crates/`. Dependency direction: `core ← algo`, `core ← io`, `{core, algo, io} ← commands ← {cli, app}` (app = eframe/egui + wgpu). Later also `py` (pyo3).

The central rule: **the 3D mesh is immutable; only the UV layer changes.**
- `uvcraft-core::Mesh` stores faces as CSR arrays. A corner is also a half-edge (`corner_vertex(c)` → `corner_vertex(next(c))`). Twins and edges are built once at construction. Face and corner order are exactly the input order.
- `UvLayer` = `corner_uv` (UV vertex index per corner) + `uvs`. A seam is an edge whose two faces reference different UV indices (`UvLayer::edge_kind`). So cut/weld only rewrite `corner_uv`, and topology-preserving export is automatic.
- `Islands` are derived (union-find over interior edges). `Document` caches them and drops the cache whenever `corner_uv` changes.
- Invariant: each UV vertex belongs to exactly one island and one mesh vertex. `Islands::split_shared_uvs` enforces it on load and after every topology-changing edit (`Session::edit`). Without it, moving one island drags another, and projections overwrite each other. Exporters must therefore compare UV *mappings* (`UvLayer::same_mapping`), not indices, to detect "unchanged".

`uvcraft-commands` is the only entry point for actions. Every action is a serde-serializable `Command` executed by `Session::execute`, which drives undo/redo (full `UvLayer` snapshots), the replayable command log and the CLI. UI, Python and macros must go through it too, never call `algo` directly. New actions = new `Command` variant + `apply` arm. Mutating ones go through `Session::edit` so they get an undo snapshot. Failed commands are not logged.

`uvcraft-algo` holds pure functions over `(mesh, uv, islands, params)` with no I/O. The modules:
- `seams`: cut/weld, sharp edges, shortest paths and `cut_to_disk`. Weld aligns the smaller island rigidly (Procrustes) before merging.
- `projection`: planar, best-fit and box projection.
- `unfold`: LSCM through a faer sparse Cholesky, parallel per island with rayon. It pins two far-apart boundary vertices and keeps the island's area and center.
- `projection`'s cylindrical and spherical modes put the wrap seam in each island's largest angular gap. Corners on the axis (poles) each get their own UV.
- `pack`: two methods.
  - `pack::raster` (default): conservative bitmask rasterization; the grid stores islands dilated by the padding; bottom-left fit trying 4 rotations. Several scales are tried in parallel, on a coarse grid and then a fine one.
  - Shelf: bounding boxes on shelves; it is also the raster fallback.
  - Also holds texel density.
- `analysis`: gets distortion from the singular values of each triangle's 3D→UV Jacobian, normalized by island scale, and fan-triangulates n-gons.

`uvcraft-io::obj` keeps the original bytes. With unchanged UVs, export is byte-identical. Otherwise it drops all `vt` lines, writes one `vt` block before the first `vt`/`f` line, and rewrites only the `vt` index of `f` tokens (`v` and `vn` text kept verbatim, including relative indices). `l` lines lose their `vt` refs. Corners without `vt` get a shared placeholder UV per vertex at (0, 0).

`uvcraft-io::gltf` (own code over `serde_json` with `preserve_order`, no glTF crate) handles `.gltf` (external or data-URI buffers) and `.glb`:
- Mesh vertices are welded by exact POSITION per glTF mesh. UV vertices are welded by (mesh vertex, UV value), so normal-only splits are not seams.
- Each glTF vertex is a *slot* holding one UV. Export writes changed slots into the TEXCOORD_0 bytes in place, updates accessor min/max if present, and adds a new accessor for primitives that had no UVs.
- If two corners of one slot need different UVs, export fails with a "vertex splitting" error. Cut/weld in M1 must implement that splitting.
- V is flipped (`v = 1 - v_gltf`). Values are f32 on disk.
- When saving to another directory, modified buffers are written as `<stem>.bin` next to the output, and image/buffer URIs are re-pointed relative to it. Kept buffers are never overwritten.

`uvcraft_io::read`/`write` pick the format by extension. Save must use the same format as load (no conversion yet).

`uvcraft-app` is the GUI.
- `app.rs`: egui 0.37 panels; every action goes through `Session::execute`.
- `render.rs`: wgpu offscreen MSAA targets shown via `register_native_texture`.
- `shader.wgsl`: one vertex layout for both views. `params.x` selects 3D or UV placement.
- `scene.rs`: render buffers and CPU picking.
- `camera.rs`: uses `glam::camera::rh::proj::directx`, because glam 0.34 has no `Mat4::perspective_rh`.

The GUI rebuilds all GPU buffers after each command.

## Conventions

- Storage uses `u32` indices (`NONE = u32::MAX` sentinel); public APIs take and return `usize`.
- Geometry is `f64` (`glam::DVec2`/`DVec3`).
- Algorithms must be deterministic: never let `HashMap`/`HashSet` iteration order reach geometry (use `BTree*` or sort). Packing amplifies 1e-8 differences into different layouts.
- Errors use `thiserror` enums per crate; `unsafe` is forbidden workspace-wide.

