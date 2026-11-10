# UvCraft — Technical Analysis of the RizomUV Feature Set

> Companion to `RizomUV_Features.md`. That file lists *what* RizomUV does; this one analyzes *how* each feature can be built, what it depends on, how hard it is, and the order to build it in.
> Date: 2026-11-10.

Difficulty scale: **1** = a few days, well-known math · **3** = weeks, needs care · **5** = open research / RizomUV's competitive edge.

---

## 1. The key architectural insight

A UV tool **never changes the 3D mesh**. Positions, faces, face order, vertex order, normals and materials stay fixed from import to export. Every UvCraft operation, cuts and welds included, changes only the **UV layer**:

- **UV topology**: for each face corner, an index into the UV-vertex array (the `vt` index in OBJ). A *seam* is just an edge whose two adjacent faces reference different UV vertices.
- **UV coordinates**: the 2D positions of the UV vertices.

Consequences:
1. **Topology-preserving export comes for free**: write the original mesh back and replace only the UV arrays.
2. **Undo/redo is cheap**: snapshot or diff only the UV layer (indices + coordinates), never the mesh.
3. **Islands are derived data**: connected components of faces across non-seam edges. They are recomputed (or patched incrementally) after a cut or weld and cached.
4. **Algorithms are naturally parallel**: islands are independent, so unfold, optimize and analysis run per island on a thread pool.

### Proposed layering

```
core/mesh        Immutable 3D mesh (half-edge connectivity built once at import)
                 + UvLayer (corner→uv indices, uv coords) per UV channel
                 + derived caches: islands, seam flags, adjacency
core/algo        Pure functions over (mesh, uv layer, params) → new UV state.
                 No UI dependencies. Each takes a progress callback + cancel token.
core/commands    Command registry: every user action is a named, serializable
                 command (name + args). One path feeds undo/redo, the script log,
                 macros, Python and the headless CLI.
core/selection   One selection model (vertex/edge/face/island) shared by both views.
io/              OBJ (MVP), glTF, FBX (later).
app/             UI: 3D viewport, UV viewport, panels, preferences, hotkeys.
bindings/        Python module + CLI executable, both built on core/commands.
```

Rule: `app/` and `bindings/` only call `core/commands`. Nothing in `core/` knows the UI exists. This is what makes headless batch mode, scripting and tests possible from day one, and it is how RizomUV works too (every UI action is logged as a Lua command).

---

## 2. Feature-by-feature analysis

### 2.1 I/O

| Feature | Approach | Difficulty | Notes |
|---|---|---|---|
| OBJ import/export | Own parser/writer | 1 | Must keep the original `v`/`vn` order and face order byte-for-byte; rewrite only `vt` and the `vt` indices. Keep `g`/`o`/`usemtl`/`s` lines. |
| Multiple UV channels | Several `UvLayer`s per mesh | 1 | OBJ has only one; matters for FBX/glTF. |
| glTF 2.0 | Own reader/writer over `serde_json` (done in M0) | 3 | glTF duplicates vertices at seams and hard normals: rebuild mesh vertices by welding equal positions per mesh, and UV vertices by (vertex, UV) so hard edges don't become seams. V axis is flipped (`v = 1 - v_gltf`). Export writes TEXCOORD_0 bytes in place; a cut through a shared glTF vertex will need vertex splitting (all attributes). |
| FBX import | `fbxcel-dom` (Rust); `ufbx` (C) as reference | 2 | |
| FBX export | Own writer (the proprietary FBX SDK is excluded by the license policy) | 4 | No open-source FBX *writer* is good enough to rely on. Start with ASCII FBX. |
| Load UVs only | Match by topology, replace the UV layer | 1 | Falls out of the architecture. |
| Mesh validation | Non-manifold edges, duplicate vertices, degenerate faces | 2 | Report and isolate; don't silently "fix" (that breaks the vertex order). |

### 2.2 Selection

| Feature | Approach | Difficulty |
|---|---|---|
| Vertex/edge/face/island modes, conversion | Half-edge traversal | 1 |
| Click / box / lasso picking | GPU ID buffer (render primitive IDs to an offscreen target) | 2 |
| Grow / shrink | One-ring expansion | 1 |
| Edge loop / ring | Walk "opposite" edges through valence-4 vertices / quads | 2. Triangulated input breaks this; detect quads or fall back to a dihedral-angle heuristic. |
| Shortest path | Dijkstra on the edge graph, 3D edge length as weight | 1 |
| By angle / material / smoothing group | Per-edge dihedral angle, per-face attributes | 1 |
| By distortion / TD | Threshold on the analysis results (§2.8) | 1 |
| Overlapping / flipped / out-of-tile | Reuses analysis (§2.8) | 1 |
| Similar islands | Topology signature (face/vertex/boundary counts + degree histogram), then shape check (§2.5 Stack) | 3 |

### 2.3 Seams: manual

| Feature | Approach | Difficulty |
|---|---|---|
| Cut | Split UV vertices along the edge chain; recompute affected islands | 2. The subtle part is correctly splitting UV vertices at chain endpoints and junctions. |
| Weld / stitch | Merge UV vertices across the edge; move/rotate/scale the smaller island so the shared edge lines up (Procrustes on the edge points) before merging | 2 |
| Cut & Unfold | Compose commands: Cut → Unfold affected islands | 1 |
| Seam painting in the 3D view | Brush selects edges near the stroke path (snap to shortest path between hits) | 2 |
| Hard edges ↔ seams | Map normal splits / smoothing groups to seam flags | 1 |

### 2.4 Seams: automatic

This is where tools differ in quality. Plan to ship simple methods first and iterate.

| Method | Algorithm | Difficulty |
|---|---|---|
| Sharp edges | Dihedral angle threshold + remove short dangling seams | 1 |
| Box / planar | Cluster faces by dominant normal axis (6 bins) then smooth labels (graph cut / majority filter) to avoid jagged borders | 2 |
| Mosaic | **Variational Shape Approximation** (Cohen-Steiner, Alliez, Desbrun 2004) or **D-Charts** (Julius, Kraevoy, Sheffer 2005), with island count as the parameter | 3 |
| Pelt (organic) | Seams along low-visibility, high-curvature paths: geodesic cut graph joining extremities (e.g. Seamster, Sheffer & Hart 2002) | 4 |
| Cut to reduce stretch | Iterate: unfold → find high-distortion vertices → cut a shortest path from them to the border. Research references: **Autocuts** (Poranne et al. 2017), **OptCuts** (Li et al. 2018) | 4 |

Reference implementation worth studying: **xatlas** (MIT), whose chart segmentation is a solid baseline.

### 2.5 Unfold / Optimize: the core of the product

RizomUV's main strength is a **robust, real-time, iterative** unfold that avoids flips and overlaps. Build it in stages:

| Stage | Algorithm | Properties | Difficulty |
|---|---|---|---|
| A. Baseline | **LSCM** (Lévy et al. 2002) / DCP | One sparse linear solve, conformal, needs 2 pins (or the spectral variant, Mullen et al. 2008). Can flip. | 2 |
| B. Quality | **ABF++** (Sheffer et al. 2005) | Better angles, non-linear, still one-shot | 3 |
| C. Flip-free, interactive | Injective init (**Tutte** embedding on a convex boundary, or LSCM if it has no flips) → **SLIM** (Rabinovich et al. 2017) on symmetric Dirichlet energy | Never flips; iterative so it can update every frame with warm start; scales to large meshes | 4 |
| D. No global overlap | Barrier on boundary contacts (Smith & Schaefer 2015) or **scaffold** triangulation of the empty space (Jiang et al. 2017) | Island cannot self-overlap | 5 |
| Optimize / relax | Same solver (SLIM / ARAP, Liu et al. 2008) started from the current UVs | | Shares C |

Angle ↔ area mix: a weighted energy. Conformal terms (σ1/σ2) plus area terms (σ1·σ2 vs. 1), where σ are the singular values of each triangle's 2D Jacobian. SLIM-style solvers handle this kind of energy.

Constraints:
- **Pins / locked vertices**: eliminate those variables from the system (hard constraints).
- **Straight / aligned borders**: project the selected boundary vertices onto a line each iteration, or add linear constraints.
- **Keep border length**: soft penalty on boundary edge lengths.

Dependencies: **`faer`** for sparse matrices and Cholesky. Reference implementations to learn from: **libigl** (LSCM, ARAP, SLIM) and **Geogram** (LSCM, ABF++).

Performance: factor once per island when the sparsity pattern is fixed and re-solve each iteration. Run islands in parallel. Interactive mode runs N iterations per frame on the islands being edited only.

### 2.6 Initial projections

Planar, box, cylindrical, spherical, from camera, from average normal: all are closed-form per-vertex mappings. **Difficulty 1.** Useful as starting points and as a fallback.

### 2.7 UV editing and transformation

| Feature | Approach | Difficulty |
|---|---|---|
| Move/rotate/scale gizmo, numeric transform, flip, snapping | Affine transform on the selected UVs | 1 |
| Auto-orient (min bounding box) | Convex hull + **rotating calipers** → minimum-area rectangle, O(n log n) | 1 |
| Orient to world axis / to edge | Project the chosen 3D direction into UV through the triangle Jacobian | 1 |
| Align / distribute | Bounding-box math | 1 |
| Straighten edge loop | Place loop vertices on a line at cumulative 3D arc length, then relax the rest with the loop pinned | 2 |
| Rectangularize / grid | Quad-only islands: walk quad strips to assign integer grid coords, scale by average edge lengths, then optionally relax | 3 |
| Stack similar | Topology match (correspondence by BFS from a matched boundary corner), then **Kabsch/Procrustes** alignment onto the reference island | 3 |
| Unstack | Offset stacked islands | 1 |
| UV transfer between same-topology objects | Copy the UV layer by index | 1 |
| Brush move / smooth / local relax | Brush weight × displacement; local relax = run the optimizer on a sub-region with the rest pinned | 2 |

### 2.8 Analysis and quality

| Feature | Approach | Difficulty |
|---|---|---|
| Distortion map | Per-triangle Jacobian singular values: angle = σ1/σ2, area = σ1·σ2 normalized by the island's global scale | 1 |
| Flipped triangles / islands | Signed UV area < 0 (per triangle / summed per island) | 1 |
| Overlaps | Triangle–triangle intersection in 2D accelerated with a uniform grid or BVH | 2 |
| Statistics (coverage %, counts, average/max distortion) | Aggregates | 1 |
| Padding violations | Distance between island borders < padding (use the same raster as packing, §2.10) | 2 |

### 2.9 Texel density

TD (px/unit) = `resolution × sqrt(uvArea / area3D)`. Get/Set TD = scale islands about their center. Heatmap = per-island TD vs. target. **Difficulty 1.** Real Space mode is the same math with resolution replaced by a unit scale.

### 2.10 Packing: the other competitive edge

Optimal packing is NP-hard, so quality comes from good heuristics and compute.

| Approach | Quality | Speed | Notes |
|---|---|---|---|
| Bounding-box shelf/skyline | Poor | Very fast | Fine as a first milestone |
| **Raster (bitmask) packing** | Good to very good | Fast, GPU-friendly | Rasterize each island at the target resolution, dilate by padding, try positions × rotations, place by a score (e.g. lowest-left, minimum bounding extent). Used by xatlas. |
| No-Fit Polygon (Minkowski sums) | Excellent | Slow | Classic nesting (textile/sheet metal); relevant for Real Space |

**Recommendation: raster packing.** It gives RizomUV's three 2025 strategies almost directly:
- **Padding Perfect**: padding is an exact dilation in pixel space.
- **Pixel Aligned**: snap placements and borders to the pixel grid.
- **Efficient**: finer raster + more rotation candidates + iterative improvement.
- **Packing inside holes**: automatic, because holes are empty pixels in the mask.
- **GPU acceleration**: each island × rotation × position test is a bitmask AND, ideal for compute shaders. Use `wgpu` compute (Vulkan/Metal/DX12) to be cross-platform, which beats RizomUV's Windows/CUDA-only GPU packing.

Parameters (padding, margin, resolution, rotation step, allow mirror, lock islands, pack into region, keep TD vs. fill) are all inputs to the same algorithm. Fill = binary search on a global scale until the islands no longer fit.

UDIM / multi-tile = multi-bin packing: assign islands to tiles (by group/material/object or greedily by area), then pack each tile. Packing groups = merge a group's islands into one rigid "super-island" first, or pack them as one unit.

**Difficulty:** basic 2 · high-quality CPU 4 · GPU 4.

### 2.11 Density maps (v1.0+)

Per-face target scale factor `s` (painted or computed). Unfold scales each 3D rest triangle by `s` before computing the energy, and packing scales islands by their average `s`. Density from **AO**: compute per-vertex ambient occlusion by ray casting (own BVH, or a wgpu compute shader) and map AO → `s`. **Difficulty 3** once the solver from §2.5 exists.

### 2.12 Grouping and organization

Hierarchical groups of islands stored as IDs on the UV layer; group colors; lock/visibility/selection per group; outliner = tree view over objects and groups. **Difficulty 1–2**, mostly UI.

### 2.13 Interface and viewports

| Feature | Notes | Difficulty |
|---|---|---|
| Synchronized 3D + UV viewports | Two views of one model + one selection model | 2 |
| Rendering millions of polygons | GPU buffers, update only the UV buffer on edits, wireframe via barycentric shader | 2 |
| Checker / custom texture, per-island colors, distortion and TD shading | Shader modes | 1 |
| Seam / border / overlap / flip overlays | Line and fill passes driven by analysis | 1 |
| Configurable hotkeys with Maya/Max/Blender presets | Map key → command name (works because of `core/commands`) | 1 |
| Layouts, themes, preferences, saved defaults | UI framework features | 1–2 |
| Progress + cancel for long operations | Algorithms run on worker threads with a cancel token | 1 if planned from the start |

### 2.14 Scripting, automation, integration

| Feature | Approach | Difficulty |
|---|---|---|
| Command log / macros | Every command is serialized; a macro is a recorded list | 1 (given the architecture) |
| Python API | Embed or expose `core/commands` via **pyo3** + maturin | 2 |
| Headless CLI / batch | Executable that runs scripts without the UI | 1 |
| Remote control (like RizomUVLink) | Local TCP/JSON-RPC server that executes commands | 2 |
| DCC bridges (Blender, Maya, Houdini) | Each bridge = small plugin that exports the mesh, calls the CLI or TCP server, and reimports the UVs | 2 each |

### 2.15 Real Space mode (v1.0+)

Mostly a mode flag: UVs in physical units, no 0–1 normalization, packing into a physical rectangle (e.g. fabric width), measurements in units. True textile patterns (seam allowances, grain lines, notches, DXF export) are a product of their own. Treat them as out of scope until the VS feature set is solid.

---

## 3. Technology decisions (decided 2026-11-10)

- **Language: Rust**, as a Cargo workspace.
- **License: fully open source.** UvCraft is dual-licensed `MIT OR Apache-2.0` (the Rust ecosystem convention), and every dependency must have an OSI-approved license compatible with that. Proprietary SDKs are excluded, which rules out the Autodesk FBX SDK.

The C/C++ libraries named in §2 (libigl, Geogram, xatlas, ufbx, Embree) are **algorithm references only**. UvCraft implements the algorithms in Rust.

| Need | Crate | License |
|---|---|---|
| Small vector math | `glam` | MIT/Apache-2.0 |
| Sparse linear algebra (LSCM, SLIM, ARAP solves) | `faer` (sparse Cholesky/LU) | MIT |
| Parallelism | `rayon` | MIT/Apache-2.0 |
| OBJ | Own parser/writer (byte-faithful round-trip; generic loaders reorder data) | — |
| glTF | Own reader/writer; `base64` for data URIs | MIT/Apache-2.0 |
| FBX import | `fbxcel` / `fbxcel-dom` | MIT/Apache-2.0 |
| FBX export | Own writer (ASCII first, binary later) | — |
| UI | `egui` + `eframe`, docking via `egui_dock` | MIT/Apache-2.0 |
| Rendering + GPU compute (packing, AO) | `wgpu` (Vulkan / Metal / DX12, cross-platform) | MIT/Apache-2.0 |
| Ray casting for AO (CPU fallback) | Own BVH | — |
| Python bindings | `pyo3` + `maturin` | MIT/Apache-2.0 |
| CLI | `clap` | MIT/Apache-2.0 |
| Command serialization | `serde` + `serde_json` | MIT/Apache-2.0 |
| Errors | `thiserror` | MIT/Apache-2.0 |
| Tests | `cargo test`, golden files (OBJ in → OBJ out) | — |

### Workspace layout

```
crates/
  uvcraft-core      Mesh, UvLayer, islands, selection. Data model only.
  uvcraft-algo      Analysis, projections, unfold, seams, packing. Pure functions.
  uvcraft-io        OBJ and glTF (M0), FBX later.
  uvcraft-commands  Session, command registry, undo/redo, command log.
  uvcraft-cli       Headless executable over uvcraft-commands.
  uvcraft-app       (M2) egui/wgpu desktop app over uvcraft-commands.
  uvcraft-py        (M3) Python module over uvcraft-commands.
```

Dependency direction: `core ← algo`, `core ← io`, `{core, algo, io} ← commands ← {cli, app, py}`.

---

## 4. Main risks

1. **Unfold quality and robustness** (§2.5 C–D): RizomUV's reputation rests on it. Mitigation: start with LSCM, add SLIM early, build a regression set of difficult meshes (thin parts, high genus, n-gons, degenerate triangles).
2. **Packing efficiency**: competitors are mature. Mitigation: raster packing with a benchmark suite tracking coverage % and time.
3. **Performance at millions of polygons**: plan GPU buffers, per-island parallelism and incremental updates from day one; adding them later is expensive.
4. **FBX round-trip**: no proprietary SDK allowed, so the writer is our own. Mitigation: OBJ + glTF first, FBX last.
5. **Auto seams quality**: research territory. Mitigation: ship sharp-edge/box first, mosaic next, pelt and stretch-based cuts last.
6. **Rust ecosystem gaps**: no libigl/xatlas equivalent exists, so unfold, seams and packing are written from scratch. Mitigation: port carefully from the reference implementations and test against their outputs.

---

## 5. Build order (implementation plan)

Each milestone produces something usable and testable.

**M0: Foundation (headless)**
1. Cargo workspace, CI, test setup.
2. `core/mesh`: half-edge mesh, `UvLayer`, island extraction.
3. OBJ import/export with a round-trip test (output identical to input when UVs are unchanged).
4. `core/commands` + undo/redo + command log; CLI that runs a list of commands.
5. Analysis: distortion, flips, TD, statistics (needed to measure everything else).

**M1: Unfold & pack (headless)**
6. Projections (planar/box).
7. Cut / weld commands.
8. LSCM unfold with pins; sharp-edge auto seams.
9. Bounding-box packing with padding/margin, then raster packing.
10. Set/Get TD.
→ First end-to-end result: `uvcraft-cli in.obj --auto-seams sharp --unfold --pack out.obj`.

**M2: Viewports (MVP v0.1)**
11. egui/wgpu app shell, 3D + UV viewports, checker texture, distortion shading.
12. Shared selection: modes, picking, grow/shrink, loop/ring, shortest path.
13. Gizmo transforms, hotkeys bound to commands.

**M3: Quality (v0.2–v0.5)**
14. SLIM optimize (flip-free, interactive with pin dragging).
15. Overlap detection; orient, align, straighten, rectangularize.
16. Stack/unstack, select similar.
17. Box and mosaic auto seams.
18. UDIM, packing groups, rotation/mirror options.
19. Python bindings.

**M4: Differentiators (v1.0+)**
20. Global overlap prevention (scaffold).
21. GPU packing + packing strategies.
22. Density map/brush, density from AO.
23. FBX, DCC bridges, Real Space mode.
