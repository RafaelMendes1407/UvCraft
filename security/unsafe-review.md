# `unsafe` and FFI review map

UvCraft's own crates contain no `unsafe` code: the workspace sets
`unsafe_code = "forbid"` and every crate inherits it with `[lints] workspace = true`.
The `static` audit check (`forbid-unsafe-policy`) fails if either side of that
rule is removed, and the `unsafe` check writes a lexical inventory of own
sources to `unsafe-own-code.json`, which is expected to stay empty.

Memory safety therefore rests on dependencies. `cargo-geiger` produces the
machine-readable inventory for `uvcraft-app` and everything it pulls in. The
groups below are where a manual review is worth the most; the counts are from
the introduction run and only indicate scale, not risk.

| Area | Crates (largest first) | Why it matters | Review priority |
| --- | --- | --- | --- |
| GPU backends | `ash`, `wgpu-hal`, `glow`, `wgpu-core` | Vulkan/GL FFI and driver-facing memory; reached by every frame of the GUI and by GPU packing. | High |
| Mesh decoders | Draco and meshopt decoders used by `uvcraft-io::gltf` | Parse attacker-controlled compressed buffers. Covered indirectly by the `gltf_parse` fuzz target. | High |
| Linear algebra / SIMD | `faer`, `gemm-*`, `fearless_simd`, `glam` | Unchecked indexing and SIMD intrinsics on sizes derived from the mesh. | Medium |
| Image/colour | `image`, `moxcms`, `pxfm` | Decode user-chosen textures (checker/texture overlay). | Medium |
| Platform / windowing | `rustix`, `winit`, `objc2*`, `windows-*` | OS calls; Linux is built in CI, macOS is not. | Medium |
| Python module | `pyo3` (feature `python`, not part of the app binary) | CPython ABI; only built by maturin. | Medium |

Embedded C/C++ code and operating-system libraries (Vulkan loaders, GPU
drivers, libxkbcommon, Wayland/X11) are outside `cargo-geiger`'s Rust source
scan. This map does not change the implementation or add lint suppressions.
