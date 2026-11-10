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
- [ ] UV layer (corner→UV indices + UV coords), multiple channels — M0 · D1 · seams = edges whose faces reference different UV vertices
- [ ] Island extraction and cache — M0 · D1 · connected components across non-seam edges, updated incrementally
- [x] Command layer (every action = named, serializable command) — M0 · D2 · single path for UI, undo, script log, macros, Python, CLI
- [x] Unlimited undo/redo — M0 · D1 · snapshot/diff of the UV layer only
- [ ] Progress reporting and cancellation for long operations — M0 · D1 · worker threads + cancel token
- [ ] Per-island multi-threading — M0 · D1 · rayon

## 2. Input / Output

- [x] OBJ import/export preserving topology, vertex order and face order — M0 · D1 · rewrite only `vt` data
- [x] Preserve groups, objects, materials, smoothing groups (OBJ) — M0 · D1
- [x] Multiple objects per scene — M0 · D1
- [ ] Load UVs only (update UVs on a loaded mesh) — M1 · D1 · match by topology
- [ ] Mesh problem detection (non-manifold, duplicate vertices, degenerate faces) — M1 · D2 · report and isolate, never reorder
- [x] glTF 2.0 import/export (.gltf with external or embedded buffers, .glb) — M0 · D3 · own reader/writer over serde_json; welds split vertices, writes TEXCOORD_0 in place
- [x] glTF vertex splitting on export (needed once cuts move a shared glTF vertex) — M1 · D3 · duplicate every attribute, including skins and morph targets
- [ ] glTF node transforms for 3D measurements (today meshes are measured in local space) — M3 · D1
- [ ] glTF sparse accessors and normalized-integer UVs outside 0–1 — M3 · D2
- [ ] glTF Draco / meshopt compressed meshes — M4 · D3
- [ ] Format conversion (OBJ ↔ glTF, .gltf ↔ .glb) — M3 · D2
- [ ] FBX import — M4 · D2 · `fbxcel-dom`
- [ ] FBX export — M4 · D4 · own writer (ASCII first)
- [ ] Multiple UV channels: create, swap, read/write — M3 · D1

## 3. Scripting, automation and integration

- [x] Command history / log console — M1 · D1
- [x] Headless CLI and batch processing — M1 · D1
- [ ] Macro recording from history — M3 · D1
- [ ] Python API — M3 · D2 · pyo3 + maturin over the command layer
- [ ] Remote control over TCP (JSON-RPC) — M4 · D2
- [ ] Blender bridge — M4 · D2
- [ ] Maya bridge — M4 · D2
- [ ] Houdini bridge — M4 · D2
- [ ] 3ds Max, Cinema 4D, Modo, ZBrush bridges — M4+ · D2 each

## 4. Interface and navigation

- [x] 3D and UV viewports side by side with synchronized selection — M2 · D2
- [ ] Rendering of meshes with millions of polygons — M2 · D2 · GPU buffers, update only UV buffer on edits
- [ ] Camera: orbit, pan, zoom, frame selection, isolate — M2 · D1
- [x] Display modes: wireframe, shaded, checker, per-island colors, distortion shading — M2 · D1
- [ ] Configurable checker / grid / custom texture — M2 · D1
- [ ] Overlays: seams, island borders, overlaps, flipped islands — M2 · D1
- [ ] UDIM tile display and numbering — M3 · D1
- [ ] Customizable hotkeys with Maya / Max / Blender presets — M2 · D1 · key → command name
- [ ] Configurable, savable window layouts — M3 · D1
- [ ] Preferences panel with saved session defaults — M3 · D1
- [ ] Scene outliner — M3 · D2
- [ ] Light and dark themes — M3 · D1
- [ ] Tooltips and built-in help — M3 · D1

## 5. Selection

- [ ] Modes: vertex, edge, face, island, object — M2 · D1
- [ ] Conversion between modes — M2 · D1
- [ ] Click, box, lasso selection — M2 · D2 · wgpu ID buffer picking
- [ ] Paint (brush) selection — M3 · D2
- [ ] Grow / shrink — M2 · D1
- [ ] Edge loop and ring — M2 · D2 · quad traversal, fallback heuristic on triangles
- [ ] Shortest path between vertices/edges — M2 · D1 · Dijkstra
- [ ] Select open edges / island borders — M2 · D1
- [ ] Select by angle, smoothing group, material — M2 · D1
- [ ] Select by distortion and by texel density — M3 · D1
- [ ] Select overlapping, flipped, out-of-tile islands — M3 · D1
- [ ] Select similar islands — M3 · D3 · topology signature + shape match
- [ ] Invert, select all / none — M2 · D1
- [ ] Saved selection sets — M3 · D1
- [ ] Hide / unhide / isolate — M2 · D1

## 6. Seams: manual

- [x] Cut edges — M1 · D2 · split UV vertices along the chain
- [x] Weld / stitch edges — M1 · D2 · align islands (Procrustes) then merge
- [ ] Cut & Unfold in one operation — M1 · D1
- [ ] Cut by loop, shortest path, selection border — M2 · D1
- [ ] Interactive seam brush / tool in the 3D viewport — M3 · D2
- [ ] Auto-weld nearby matching edges — M3 · D2
- [ ] Hard edges / smoothing groups ↔ seams conversion — M3 · D1

## 7. Seams: automatic

- [x] Sharp edges (dihedral angle) — M1 · D1
- [ ] Box / planar (by dominant normal + label smoothing) — M3 · D2
- [ ] Mosaic (planarity/curvature segmentation, island count control) — M3 · D3 · VSA / D-Charts
- [ ] Pelt (organic shapes) — M4 · D4 · geodesic cut graph
- [ ] Cut to reduce stretch — M4 · D4 · iterative unfold + cut (Autocuts / OptCuts ideas)
- [ ] Respect existing seams and borders as constraints — M3 · D1
- [ ] Controls: distortion tolerance, max seam length, island count — M3 · D2

## 8. Unfold and optimize

- [x] LSCM unfold (conformal, baseline) — M1 · D2 · `faer` sparse solve
- [ ] ABF++ unfold (higher quality) — M3 · D3
- [ ] Flip-free unfold (Tutte init + SLIM) — M3 · D4
- [ ] Optimize / relax with iteration control — M3 · D3 · SLIM / ARAP from current UVs
- [ ] Angle ↔ area distortion mix — M3 · D3 · weighted energy on Jacobian singular values
- [ ] Real-time interactive unfold while dragging pins — M3 · D3 · warm-started iterations per frame
- [ ] Self-overlap (global) prevention — M4 · D5 · scaffold / boundary barrier
- [ ] Unfold from existing UVs (refine) — M3 · D1
- [ ] Pins (fixed vertices / borders) — M1 · D1 · hard constraints
- [ ] Lock islands / polygons during unfold, optimize, pack — M3 · D1
- [ ] Straight and horizontal/vertical-aligned borders — M3 · D2
- [ ] Keep border length / shape — M3 · D2 · soft penalty
- [ ] Preserve island orientation — M3 · D1

## 9. Initial projections

- [x] Planar (axis) — M1 · D1
- [x] Box — M1 · D1
- [ ] Cylindrical — M3 · D1
- [ ] Spherical — M3 · D1
- [ ] From camera — M3 · D1
- [x] By average normal — M3 · D1

## 10. UV editing and transformation

- [ ] Move / rotate / scale gizmo with snapping — M2 · D1
- [ ] Numeric transform (offset, angle, scale) — M2 · D1
- [ ] Mirror / flip horizontal and vertical — M2 · D1
- [ ] Stepped rotation (90°, 45°, custom) — M2 · D1
- [ ] Auto-orient: world axis, longest edge, min bounding box, to selected edge — M3 · D1 · rotating calipers
- [ ] Straighten edge loops — M3 · D2
- [ ] Rectangularize / grid quad islands — M3 · D3
- [ ] Align and distribute islands — M3 · D1
- [ ] Stack similar islands — M3 · D3 · topology correspondence + Kabsch alignment
- [ ] Unstack — M3 · D1
- [ ] Match island onto reference island — M3 · D3
- [ ] Snap to grid, vertex, pixel — M2 · D1
- [ ] Brush editing (move / smooth / relax) — M3 · D2
- [ ] Localized relax on part of an island — M3 · D2
- [ ] Copy / paste UVs between same-topology objects — M3 · D1

## 11. Texel density

- [x] Measure TD per island, selection, object — M1 · D1 · `res × sqrt(uvArea / area3D)`
- [x] Set target TD — M1 · D1
- [x] Get TD from one island, apply to others — M1 · D1
- [x] Texture resolution presets (512 – 8K, custom) — M1 · D1
- [ ] Single TD setting shared by layout and packing — M2 · D1
- [ ] Uniform scale of all islands by 3D area — M1 · D1
- [ ] TD heatmap — M2 · D1

## 12. Packing

- [x] Bounding-box packing with padding and margin (baseline) — M1 · D2
- [ ] Raster (bitmask) packing with rotation — M1 · D3
- [ ] Padding / margin in pixels or UV units — M1 · D1
- [ ] Rotation options: none, 90° steps, custom angle, free — M3 · D2
- [ ] Allow mirroring for better fit — M3 · D1
- [ ] Keep TD vs. maximize scale (fill) — M3 · D2 · binary search on scale
- [ ] Pack selection only / pack around locked islands — M3 · D1
- [ ] Pack into a region / rectangle — M3 · D1
- [ ] Packing groups — M3 · D2
- [ ] Stack similar before packing — M3 · D1
- [ ] Quality vs. time control — M3 · D1
- [ ] Packing inside holes — M3 · D1 · free with raster packing
- [ ] Strategy: Padding Perfect — M4 · D2
- [ ] Strategy: Pixel Aligned — M4 · D2
- [ ] Strategy: Efficient — M4 · D4
- [ ] GPU packing, cross-platform (Vulkan / WebGPU compute) — M4 · D4
- [ ] Per-island scale from density map / multipliers — M4 · D2

## 13. UDIM / multi-tile

- [ ] Pack across multiple UDIM tiles — M3 · D2 · multi-bin packing
- [ ] Distribute islands per tile by group, material, object — M3 · D2
- [ ] Target tiles and row/column limits — M3 · D1

## 14. Grouping and organization

- [ ] Hierarchical island groups (group / ungroup) — M3 · D2
- [ ] Island groups tree panel — M3 · D2
- [ ] Per-group colors — M3 · D1
- [ ] Organize by material, object, smoothing group — M3 · D1
- [ ] Lock, visibility, selection per group — M3 · D1

## 15. Analysis and quality

- [x] Distortion map: angle, area, combined — M0 · D1 · Jacobian singular values
- [x] Statistics: island count, coverage %, average / max distortion — M0 · D1
- [x] Flipped triangle / island detection — M0 · D1 · signed area
- [ ] Overlap detection (between and within islands) — M3 · D2 · grid / BVH
- [x] Out-of-tile detection — M1 · D1
- [ ] Minimum padding violation detection — M3 · D2
- [x] Seam length and cut count — M1 · D1

## 16. Density maps

- [ ] Density brush (paint target resolution) — M4 · D3
- [ ] Real-time optimize while painting — M4 · D3
- [ ] Density map from ambient occlusion — M4 · D3 · own BVH or wgpu compute ray casting
- [ ] Map-based texel density workflow — M4 · D2

## 17. Real Space mode

- [ ] 1:1 physical scale, no normalization — M4 · D1
- [ ] Physical units (mm, cm, m, in) — M4 · D1
- [ ] Length and area measurements — M4 · D1
- [ ] Pack into a physical rectangle (e.g. fabric width) — M4 · D2
- [ ] Flat pattern export for manufacturing (textile, print, packaging) — M4+ · D4
