# UvCraft — Feature List

Complete list of planned UvCraft features, based on the RizomUV feature set (`RizomUV_Features.md`). Algorithm details, architecture and risks are in `UvCraft_Technical_Analysis.md`.

**Format:** `- [ ] Feature — Milestone · Difficulty · Approach`

**Milestones:**
- **M0**: foundation (headless)
- **M1**: unfold and pack (headless)
- **M2**: MVP v0.1 with viewports
- **M3**: v0.2–v0.5 quality
- **M4**: v1.0+ differentiators

**Difficulty:**
- **D1**: days, well-known math
- **D3**: weeks, needs care
- **D5**: open research or RizomUV's competitive edge

---

## 1. Foundation

- [x] Immutable 3D mesh with half-edge connectivity — M0 · D2 · built once at import, never modified
- [x] UV layer (corner→UV indices + UV coords), multiple channels — M0 · D1 · glTF and FBX channels can be listed, created, swapped, edited and saved; seams use discontinuous corner indices
- [x] Island extraction and cache — M0 · D1 · connected components across non-seam edges; cached and incrementally updated after seam changes
- [x] Command layer (every action = named, serializable command) — M0 · D2 · single path for UI, undo, script log, macros, Python, CLI
- [x] Unlimited undo/redo — M0 · D1 · snapshot/diff of the UV layer only
- [x] Progress reporting and cancellation for long operations — M0 · D1 · worker threads + cancel token
- [x] Per-island multi-threading — M0 · D1 · rayon in unfold and optimize

## 2. Input / Output

- [x] OBJ import/export preserving topology, vertex order and face order — M0 · D1 · rewrite only `vt` data
- [x] Preserve groups, objects, materials, smoothing groups (OBJ) — M0 · D1
- [x] Multiple objects per scene — M0 · D1
- [x] Load UVs only (update UVs on a loaded mesh) — M1 · D1 · match by topology
- [x] Mesh problem detection (non-manifold, duplicate vertices, degenerate faces) — M1 · D2 · report and isolate, never reorder
- [x] glTF 2.0 import/export (.gltf with external or embedded buffers, .glb) — M0 · D3 · own reader/writer over serde_json; welds split vertices, writes TEXCOORD_0 in place
- [x] glTF vertex splitting on export (needed once cuts move a shared glTF vertex) — M1 · D3 · duplicate every attribute, including skins and morph targets
- [x] glTF node transforms for 3D measurements — M3 · D1 · unique static nodes use world space; instanced, skinned and animated meshes still use local space
- [x] glTF sparse accessors and normalized-integer UVs outside 0–1 — M3 · D2
- [x] glTF Draco / meshopt compressed meshes — M4 · D3 · imports Draco and EXT/KHR meshopt, including Khronos sample assets; edited exports use ordinary glTF buffers
- [x] Format conversion .gltf ↔ .glb — M3 · D2 · merge buffers into the BIN chunk / extract it to `<stem>.bin`
- [x] Format conversion OBJ ↔ glTF — M3 · D2
- [x] FBX import — M4 · D2 · ASCII/binary 7.4–7.7 static meshes, transforms, instances, materials and UV channels; active Layer selection; external Assimp ASCII and binary fixtures match ufbx face counts
- [x] FBX export — M4 · D4 · ASCII 7.4 static meshes, objects, materials and UV channels, normals/smoothing, Layer/Definitions; independently parsed by ufbx, including a 1056-face external binary-FBX roundtrip
- [x] Multiple UV channels: create, swap, read/write — M3 · D1 · glTF `TEXCOORD_n` (OBJ has one UV set by format)

## 3. Scripting, automation and integration

- [x] Command history / log console — M1 · D1
- [x] Headless CLI and batch processing — M1 · D1
- [x] Macro recording from history — M3 · D1
- [x] Python API — M3 · D2 · pyo3 + maturin over the command layer
- [x] Remote control over TCP (JSON-RPC) — M4 · D2
- [x] Blender bridge — M4 · D2
- [x] Maya bridge — M4 · D2
- [x] Houdini bridge — M4 · D2
- [x] 3ds Max bridge — M4+ · D2
- [x] Cinema 4D bridge — M4+ · D2
- [x] Modo bridge — M4+ · D2
- [ ] ZBrush bridge — M4+ · D2

## 4. Interface and navigation

- [x] 3D and UV viewports side by side with synchronized selection — M2 · D2
- [x] Rendering of meshes with millions of polygons — M2 · D2 · 1M triangles drawn offscreen on physical RX 9060 XT: 1024² MSAA4 median 1.39 ms/frame; 24 MB UV-only GPU upload median 1.32 ms (synthetic triangles, no wireframe/window composition)
- [x] Camera: orbit, pan, zoom, frame selection, isolate — M2 · D1
- [x] Display modes: wireframe, shaded, checker, per-island colors, distortion shading — M2 · D1
- [x] Configurable checker / grid / custom texture — M2 · D1
- [x] Overlays: seams, island borders, overlaps, flipped islands — M2 · D1
- [x] UDIM tile display and numbering — M3 · D1
- [x] Customizable hotkeys with Maya / Max / Blender presets — M2 · D1 · key → command name
- [x] Configurable, savable window layouts — M3 · D1
- [x] Preferences panel with saved session defaults — M3 · D1
- [x] Scene outliner — M3 · D2
- [x] Light and dark themes — M3 · D1
- [x] Tooltips and built-in help — M3 · D1

## 5. Selection

- [x] Modes: vertex, edge, face, island, object — M2 · D1
- [x] Conversion between modes — M2 · D1
- [x] Click, box, lasso selection — M2 · D2 · CPU picking + CPU ID buffer for occlusion (X-ray toggle selects through)
- [x] Paint (brush) selection — M3 · D2
- [x] Grow / shrink — M2 · D1
- [x] Edge loop and ring — M2 · D2 · quad traversal, fallback heuristic on triangles
- [x] Shortest path between vertices/edges — M2 · D1 · Dijkstra
- [x] Select open edges / island borders — M2 · D1
- [x] Select by angle, smoothing group, material — M2 · D1
- [x] Select by distortion and by texel density — M3 · D1
- [x] Select overlapping, flipped, out-of-tile islands — M3 · D1
- [x] Select similar islands — M3 · D3 · topology signature + shape match
- [x] Invert, select all / none — M2 · D1
- [x] Saved selection sets — M3 · D1
- [x] Hide / unhide / isolate — M2 · D1

## 6. Seams: manual

- [x] Cut edges — M1 · D2 · split UV vertices along the chain
- [x] Weld / stitch edges — M1 · D2 · align islands (Procrustes) then merge
- [x] Cut & Unfold in one operation — M1 · D1
- [x] Cut by loop, shortest path, selection border — M2 · D1
- [x] Interactive seam brush / tool in the 3D viewport — M3 · D2
- [x] Auto-weld nearby matching edges — M3 · D2 · endpoint distance tolerance in UV units
- [x] Hard edges / smoothing groups ↔ seams conversion — M3 · D1 · OBJ handles cyclic seams with supporting hard edges and rewrites explicit `vn`

## 7. Seams: automatic

- [x] Sharp edges (dihedral angle) — M1 · D1
- [x] Box / planar (by dominant normal + label smoothing) — M3 · D2 · six-axis face labels with a smoothness penalty, exposed by `box_planar_seams`
- [x] Mosaic (planarity/curvature segmentation, island count control) — M3 · D3 · VSA / D-Charts
- [x] Pelt (organic shapes) — M4 · D4 · geodesic cut graph
- [x] Cut to reduce stretch — M4 · D4 · iterative high-distortion slit candidates, unfold and accept only measured improvements
- [x] Respect existing seams and borders as constraints — M3 · D1
- [x] Controls: distortion tolerance, max seam length, island count — M3 · D2 · Mosaic post-unfold UV distortion bound, new-seam length budget, chart count
- [x] Cut closed and multi-loop islands to disks, including handles (genus > 0) — M1 · D3 · slit + loop joins + tree–cotree cut graph with pruning

## 8. Unfold and optimize

- [x] LSCM unfold (conformal, baseline) — M1 · D2 · `faer` sparse solve
- [x] ABF++ unfold (higher quality) — M3 · D3 · Newton + ABF++ block elimination, angle-based LSCM rebuild, LSCM fallback
- [x] Flip-free unfold (Tutte init + SLIM) — M3 · D4
- [x] Optimize / relax with iteration control — M3 · D3 · SLIM (default) or ARAP from current UVs
- [x] Angle ↔ area distortion mix — M3 · D3 · weighted energy on Jacobian singular values
- [x] Real-time interactive unfold while dragging pins — M3 · D3 · asynchronous warm-started ARAP preview, orientation-safe continuation and one undoable commit per drag
- [x] Self-overlap (global) prevention — M4 · D5 · optional global face-pair overlap guard with UV line search in unfold/optimize
- [x] Unfold from existing UVs (refine) — M3 · D1 · SLIM from current flip-free UVs
- [x] Pins (fixed vertices / borders) — M1 · D1 · hard constraints
- [x] Lock islands / polygons during unfold, optimize, pack — M3 · D1 · face masks survive seam edits; polygon vertices stay fixed in solvers
- [x] Straight and horizontal/vertical-aligned borders — M3 · D2 · `align_border` projects a selected boundary path to U or V while honoring pins and locks
- [x] Keep border length / shape — M3 · D2 · soft penalty
- [x] Preserve island orientation — M3 · D1 · optional unfold/optimize alignment to previous UV rotation and mirror

## 9. Initial projections

- [x] Planar (axis) — M1 · D1
- [x] Box — M1 · D1
- [x] Cylindrical — M3 · D1
- [x] Spherical — M3 · D1
- [x] From camera — M3 · D1
- [x] By average normal — M3 · D1

## 10. UV editing and transformation

- [x] Move / rotate / scale gizmo with snapping — M2 · D1
- [x] Numeric transform (offset, angle, scale) — M2 · D1
- [x] Mirror / flip horizontal and vertical — M2 · D1
- [x] Stepped rotation (90°, 45°, custom) — M2 · D1
- [x] Auto-orient: world axis, longest edge, min bounding box, to selected edge — M3 · D1 · rotating calipers
- [x] Straighten edge loops — M3 · D2
- [x] Rectangularize / grid quad islands — M3 · D3
- [x] Align and distribute islands — M3 · D1
- [x] Stack similar islands — M3 · D3 · topology correspondence + Kabsch alignment
- [x] Unstack — M3 · D1
- [x] Match island onto reference island — M3 · D3
- [x] Snap to grid, vertex, pixel — M2 · D1
- [x] Brush editing (move / smooth / relax) — M3 · D2
- [x] Localized relax on part of an island — M3 · D2
- [x] Copy / paste UVs between same-topology objects — M3 · D1

## 11. Texel density

- [x] Measure TD per island, selection, object — M1 · D1 · `res × sqrt(uvArea / area3D)`
- [x] Set target TD — M1 · D1
- [x] Get TD from one island, apply to others — M1 · D1
- [x] Texture resolution presets (512 – 8K, custom) — M1 · D1
- [x] Single TD setting shared by layout and packing — M2 · D1
- [x] Uniform scale of all islands by 3D area — M1 · D1 · equalize_texel_density preserves total UV area
- [x] TD heatmap — M2 · D1

## 12. Packing

- [x] Bounding-box packing with padding and margin (baseline) — M1 · D2
- [x] Raster (bitmask) packing with rotation — M1 · D3
- [x] Padding / margin in pixels or UV units — M1 · D1
- [x] Rotation options: none, 90° steps, custom angle, free — M3 · D2
- [x] Allow mirroring for better fit — M3 · D1
- [x] Keep TD vs. maximize scale (fill) — M3 · D2 · binary search on scale
- [x] Pack selection only / pack around locked islands — M3 · D1
- [x] Pack into a region / rectangle — M3 · D1
- [x] Packing groups — M3 · D2 · pack listed islands as one rigid layout item
- [x] Stack similar before packing — M3 · D1 · `pack` option stacks matching islands as one item
- [x] Quality vs. time control — M3 · D1
- [x] Packing inside holes — M3 · D1 · free with raster packing
- [x] Strategy: Padding Perfect — M4 · D2
- [x] Strategy: Pixel Aligned — M4 · D2 · snap each packed item's UV origin to texel grid with reserved padding
- [x] Strategy: Efficient — M4 · D4 · three deterministic raster placement orders; keeps the best scale
- [x] GPU raster placement search on native Vulkan — M4 · D4 · wgpu compute checks candidate positions in parallel; AMD Radeon RX 9060 XT validated against CPU, with CPU fallback and `UVCRAFT_GPU_PACK=0` override
- [ ] GPU packing in browser WebGPU — M4 · D4 · async `pack_with_progress_async` now uses WebGPU adapter detection and callback driven readback, with CPU fallback; wasm32 target compiles and native async/CPU equivalence passes, but browser runtime validation remains
- [x] Per-island scale from density map / multipliers — M4 · D2 · `pack.island_multipliers` applies explicit per-island factors; map input is tracked separately below

## 13. UDIM / multi-tile

- [x] Pack across multiple UDIM tiles — M3 · D2 · multi-bin packing
- [x] Distribute islands per tile by group, material, object — M3 · D2
- [x] Target tiles and row/column limits — M3 · D1

## 14. Grouping and organization

- [x] Hierarchical island groups (group / ungroup) — M3 · D2
- [x] Island groups tree panel — M3 · D2
- [x] Per-group colors — M3 · D1
- [x] Organize by material, object, smoothing group — M3 · D1
- [x] Lock, visibility, selection per group — M3 · D1

## 15. Analysis and quality

- [x] Distortion map: angle, area, combined — M0 · D1 · Jacobian singular values
- [x] Statistics: island count, coverage %, average / max distortion — M0 · D1
- [x] Flipped triangle / island detection — M0 · D1 · signed area
- [x] Overlap detection (between and within islands) — M3 · D2 · grid / BVH
- [x] Out-of-tile detection — M1 · D1
- [x] Minimum padding violation detection — M3 · D2
- [x] Seam length and cut count — M1 · D1

## 16. Density maps

- [x] Density brush (paint target resolution) — M4 · D3 · per-face target density with viewport stroke, lock filtering and undo
- [x] Real-time optimize while painting — M4 · D3 · live UV preview with pin constraints and one committed edit per stroke
- [x] Density map from ambient occlusion — M4 · D3 · CPU triangle BVH ray casting bakes PGM with progress and cancellation
- [x] Map-based texel density workflow — M4 · D2 · `apply_density_map` samples P2/P5 grayscale PGM per island and scales UVs before packing

## 17. Real Space mode

- [x] 1:1 physical scale, no normalization — M4 · D1 · `set_exact_real_scale` preserves every edge on planar islands; nonplanar islands require cuts/flattening first
- [x] Physical units (mm, cm, m, in) — M4 · D1
- [x] Length and area measurements — M4 · D1
- [x] Pack into a physical rectangle (e.g. fabric width) — M4 · D2
- [x] Flat pattern export for manufacturing (textile, print, packaging) — M4+ · D4 · dimensioned SVG cut contours in physical UV units
