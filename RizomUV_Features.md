# Feature Survey — RizomUV

> Reference for the **UvCraft** project (UV unwrapping tool).
> Basis: RizomUV up to version **2027.0** (Oct 2026), plus the history of versions 2020–2025.
> Survey date: 2026-11-10.
> Note: the official changelog at rizomuv.com is the source of truth for fine details. Items marked *(version)* indicate when a feature appeared or changed significantly.

---

## 1. Product overview

| Item | Description |
|---|---|
| Origin | Successor to **Unfold3D** (Polygonal Design / Rizom-Lab, France) |
| Editions | **Virtual Spaces (VS)**: games/VFX, normalized 0–1 UV space and UDIMs. **Real Space (RS)**: product design, textiles and manufacturing, 1:1 scale between 3D and UV, real-world units, no normalization |
| Platforms | Windows, macOS and Linux (GPU packing on Windows with NVIDIA/CUDA only) *(2025)* |
| Versioning | Autodesk-style: 2020, 2022, 2023, 2024, 2025, **2027** (there was no 2026) |
| Licensing | Node-locked, floating, subscription/perpetual, Indie/Educational editions |

---

## 2. Input / Output (I/O) and integration

### 2.1 Formats
- Import and export of **OBJ** and **FBX**, including existing UVs, normals, smoothing groups and materials.
- Preserves the original topology and vertex order on re-export (essential for pipelines).
- Multiple UV channels/sets: read, write, create and swap.
- Import of multiple objects and scenes with hierarchy.
- Option to load UVs only (update UVs on an already loaded mesh).
- Detection of problems on import (duplicate vertices, non-manifold meshes).

### 2.2 Bridges / Integrations (official and community plugins)
- **Autodesk Maya**, **3ds Max**, **Blender**, **Cinema 4D**, **Houdini**, **Modo**, **ZBrush** (via GoZ or scripts).
- One-click round-trip workflow: send the mesh → edit in RizomUV → return the UVs.
- Command-line / headless mode for batch processing.

### 2.3 Scripting and automation
- Full **Lua API**: every UI action generates Lua commands visible in the log/console.
- **Python bridge** (`RizomUVLink`) for external control over TCP *(2022+)*.
- Macro recording from the command history.
- Batch execution of scripts over multiple files.

---

## 3. Interface and navigation

- **3D viewport** and **UV viewport** side by side and synchronized (mirrored selection).
- Configurable, savable window layouts.
- **Scene Outliner** for managing objects and visibility *(2025)*.
- Redesigned **Preferences** panel, with the option to save all settings as session defaults *(2025)*.
- Customizable hotkeys with presets (Maya, Max, Blender).
- Camera navigation: orbit, pan, zoom, frame selection and isolate selection.
- Display modes: wireframe, shaded, checker texture, per-island colors and distortion shading.
- Configurable **checker textures** and grids (size and color), plus loading a user texture.
- Display of **open edges (seams)**, island borders, overlapping islands and flipped islands.
- Unlimited **Undo/Redo history**.
- Contextual help messages and tooltips, with built-in documentation.
- Light and dark interface themes.

---

## 4. Selection

### 4.1 Modes
- Vertex, edge, polygon, island, object/mesh.
- Selection conversion between modes.

### 4.2 Selection tools
- Click, box/lasso and paint selection (brush).
- Grow / Shrink.
- Edge **Loop** and **Ring**.
- **Shortest path** selection between two vertices or edges.
- Selection of **open edges** and island borders.
- Selection by **angle** (sharp edges) and by smoothing groups or materials.
- **Selection by distortion** and **by texel density** (islands or polygons) *(2022)*.
- Select overlapping, flipped or out-of-0–1 islands.
- Select similar islands (same shape or topology).
- Invert selection and select all/none.
- Saved selection sets and groups.
- Hide/unhide and isolate.

---

## 5. Cuts / Seams

### 5.1 Manual
- **Cut** and **Weld** edges.
- **Cut & Unfold** in a single operation.
- Brush painting of seams along edges.
- Cutting by loop, by shortest path and along the border of a polygon selection.
- Interactive **Seam** tool directly in the 3D viewport *(2024)*.
- Weld islands along shared edges (stitch).
- Automatically weld nearby matching edges.

### 5.2 Automatic
- **Auto Seams** with algorithms based on:
  - **Mosaic** (segmentation by planarity or curvature, with control over island count);
  - **Pelt** (for organic shapes, e.g. characters);
  - **Box/Planar** (by angle between faces, a good option for hard-surface);
  - **Sharp edges** (by corner angle);
  - **Geometry-based hierarchy/segmentation**.
- Auto seams that respect existing borders and cuts.
- Control over distortion tolerance, maximum seam length and island count.
- Automatic cuts to reduce distortion (**"Cut to reduce stretch"**).
- **Hard edges ↔ seams**: convert smoothing groups or hard edges to seams and vice versa.
- Existing seams preserved as constraints.

---

## 6. Unfold / Optimize

### 6.1 Core algorithm
- Unfold based on distortion minimization (the Unfold3D foundation): fast, robust, no local overlaps.
- **Optimize** (relax while preserving shape) with iteration control.
- Configurable balance between **angular** and **area** distortion (Mix/Angle-Area).
- **Self-overlap prevention** during unfold.
- **Flipped-triangle prevention**.
- **Real-time interactive** unfold while the user drags pins or edits.
- Works on very dense meshes (millions of polygons) with multi-threaded performance.

### 6.2 Constraints
- **Pins** (fix vertices or borders).
- Lock islands or polygons during unfold, optimize or pack.
- **Straight borders** (straighten) and horizontal/vertical border alignment.
- Constrain borders to keep their original length or shape.
- Preserve island orientation.

### 6.3 Density maps *(2024–2027)*
- **Density map / Density brush**: paint where texel resolution should be higher or lower; influences unfold and packing scale.
- Optimization updates in real time while the artist paints.
- **Automatic density map generation from Ambient Occlusion** (reduces resolution in hidden areas) *(2027)*.
- Map-based texel density workflow *(2027)*.

### 6.4 Initial projections
- Planar, cylindrical, spherical and box.
- **Projection from camera** or from an axis.
- Projection by average normal.
- Unfold from existing UVs (refine instead of reprojecting).

---

## 7. UV editing and transformation

- Move, rotate and scale **gizmo/manipulator** in UV space (with snapping).
- Transformation by numeric values (offset, angle, scale).
- Horizontal and vertical mirror/flip.
- Stepped rotation (90°, 45° or custom).
- Automatic **island orientation**: by world axis, by longest edge, by minimum bounding box, aligned to the selected edge.
- **Straighten**: straighten edge selections or loops, and turn an island into a rectangle (**"Rectangularize" / grid**).
- Island alignment and distribution (left, right, top, center, spacing).
- **Stack / Unstack** similar islands (stack duplicates to share texture).
- Island matching: align identical topology onto a reference island.
- Snapping to grid, vertex and pixel.
- Brush editing of UV vertices (move/smooth/relax locally).
- **Localized relax** over part of an island.
- Copy/paste UVs between objects with the same topology (UV transfer).

---

## 8. Texel Density (TD)

- TD measurement per island, selection or object (px/unit).
- **Set a target TD** and apply it (Set TD) to selected islands.
- **Get TD** from one island and apply it to others (Get/Set).
- Single, linked TD setting shared between the Layout and Packing panels *(2024)*.
- Texture resolution (512, 1K, 2K, 4K, 8K, custom) for pixel-based calculation.
- Uniform scaling of all islands by 3D area (consistency across objects).
- TD heatmap visualization in the viewport.
- **Real Space**: 1:1 scale with physical units (mm, cm, m, inches).

---

## 9. Packing

### 9.1 Algorithm
- Highly efficient packing with island rotation, translation and scaling.
- **GPU packing** (CUDA/NVIDIA, Windows), about 4× faster and able to process 500K+ islands in under 1 minute *(2025)*.
- **Packing strategies** *(2025)*:
  - **Efficient** (maximum space usage);
  - **Padding Perfect** (guaranteed exact padding);
  - **Pixel Aligned** (borders and islands aligned to the pixel grid).
- Quality vs. time control (iterations and precision level).
- Allowed orientations: free, 90° steps, custom angle; improved orientation optimization *(2024–2025)*.
- Allow or disallow mirroring (flip) for a better fit.
- Packing into islands with holes (use the inner space of islands, *packing inside holes*).

### 9.2 Parameters
- **Padding/Spacing** between islands and tile-border **margin**, in pixels or UV units.
- Map resolution (affects pixel padding).
- Keep TD vs. maximize scale (fill).
- Per-island scale from the density map or multipliers.
- Lock islands in place (pack around fixed islands).
- Pack the selection only, or everything.
- Pack into a specific **region/rectangle** of UV space.
- **Packing groups** (keep a group's islands together).
- Stacking of similar islands before packing.

### 9.3 UDIM / Multi-tile
- Packing across **multiple UDIM tiles**.
- Distribution of islands per tile by group, material or object.
- UDIM workflow improvements *(2025)*.
- Define target tiles and limits (number of rows/columns).
- UDIM tile display and numbering (1001, 1002…).

---

## 10. Grouping and organization

- Hierarchical **island groups** (Group/Ungroup), with per-group operations.
- **Island Groups Tree** / hierarchy panel.
- Island colors per group.
- Organization by material, object or smoothing group.
- Lock, visibility and selection per group.
- Scene Outliner for objects *(2025)*.

---

## 11. Analysis and quality

- Color-coded **distortion map** (stretch): angular, area and combined.
- Statistics: island count, UV area usage (% of space used), average and maximum distortion.
- Detection of **overlaps** between islands and self-overlaps.
- Detection of **flipped islands**.
- Islands outside the 0–1 space or outside tiles.
- Detection of minimum padding violations.
- Per-island TD indicator (heatmap).
- Display of seam length and number of cuts.

---

## 12. Real Space (RS) edition specifics

- 1:1 physical scale (UVs in real units, no normalization).
- **Textile/sewing** workflows: generate flat patterns from 3D.
- Export of flat patterns for manufacturing (cutting, printing, packaging).
- Packing into a rectangular area of physical size (e.g. fabric roll width).
- Length and area measurements in real units.

---

## 13. Performance and robustness

- Multi-threaded algorithms, plus GPU for packing *(2025)*.
- Support for meshes with millions of polygons and hundreds of thousands of islands.
- Tolerance for problematic geometry: n-gons, non-manifold meshes, duplicate vertices.
- Mesh cleanup and repair tools on import.
- Interruptible operations with visible progress.

---

## 14. Release timeline summary

| Version | Highlights |
|---|---|
| Unfold3D → RizomUV (2018) | Rebranding; VS/RS split; new UI |
| 2020 | Auto seams, packing and bridge integration improvements |
| 2022 (2022.0/2022.1) | Selection by distortion/TD, Python API (RizomUVLink), UI improvements |
| 2023 | Packing, UDIM and performance refinements |
| 2024 | "Biggest upgrade since 2018": density map/brush, new seam tool, improved packing orientation, unified TD |
| 2025 | **GPU packing**, Efficient/Padding Perfect/Pixel Aligned strategies, Scene Outliner, saved defaults, new Preferences, UDIM |
| 2027 | **Map-based texel density**, **density map from Ambient Occlusion** |

---

## 15. Suggested prioritization for UvCraft

### MVP (v0.1)
1. Import/export OBJ (FBX later), preserving topology.
2. Synchronized 3D and UV viewports with a checker texture.
3. Selection: vertex, edge, face, island, loop and shortest path.
4. Manual seam Cut/Weld.
5. Unfold (e.g. LSCM/ABF++ → later SLIM/ARAP) with pins and anti-flip.
6. Distortion map.
7. Basic packing (90° rotation, padding, margin).
8. Texel density: measure and set.
9. Undo/Redo.

### v0.2–v0.5
- Auto seams (sharp edges, mosaic, pelt).
- Iterative, real-time interactive optimize.
- Straighten, rectangularize, island alignment and orientation.
- Stack/Unstack similar islands.
- UDIM and packing groups.
- Overlap and flip detection.
- Scripting API (Python) and headless CLI.

### v1.0+
- Density map and brush, plus density from AO.
- Advanced packing (strategies, holes, free orientation) with GPU acceleration (cross-platform CUDA/Vulkan/compute, an edge over RizomUV's Windows-only limitation).
- Bridges (Blender, Maya, Houdini).
- "Real Space" mode (physical units and textile patterns).

---

## Sources
- CG Channel: RizomUV 2025: https://www.cgchannel.com/2025/09/rizom-lab-releases-rizomuv-2025/
- Digital Production: RizomUV 2025 (GPU/CUDA): https://digitalproduction.com/2025/09/15/rizomuv-2025-faster-packing-but-only-if-youre-on-windows-with-cuda/
- Digital Production: RizomUV 2027: https://digitalproduction.com/2026/10/13/rizomuv-2027-more-uvs-less-wrestling/
- CG Channel: RizomUV 2027: https://www.cgchannel.com/?p=179230
- CG Channel: RizomUV 2024: https://www.cgchannel.com/2024/06/rizom-lab-releases-rizomuv-2024
- befores & afters: RizomUV 2024: https://beforesandafters.com/2024/06/21/rizomuv-2024-biggest-upgrade-since-2018/
- CG Channel: RizomUV 2022 VS and RS: https://www.cgchannel.com/2021/11/rizom-lab-ships-rizomuv-2022-vs-and-rs/
- Motion Media: RizomUV 2025 Packing: https://www.motionmedia.com/mm-blog/rizomuv-2025-gpupowered-uv-packing-smarter-strategies-cleaner-results/
- Official documentation: https://www.rizom-lab.com / https://docs.rizom-lab.com
