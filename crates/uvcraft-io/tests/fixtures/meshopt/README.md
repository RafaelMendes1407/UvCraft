# MeshoptCubeTest fixture

`MeshoptCubeTest.gltf` and `MeshoptCubeTest.bin` are from the Khronos glTF
Sample Assets repository, `Models/MeshoptCubeTest/glTF-Meshopt`. The optional
variant's `MeshoptCubeTest.gltf` is stored as `MeshoptCubeTestOptional.gltf`,
along with its `MeshoptCubeTestFallback.bin`:

https://github.com/KhronosGroup/glTF-Sample-Assets/tree/main/Models/MeshoptCubeTest/glTF-Meshopt

The model is licensed CC0 1.0. The omitted PNG files are not needed to test
geometry decoding. The required variant has no uncompressed fallback buffer.
The optional variant's fallback gives an independent geometry reference. The
asset exercises mixed compressed and plain views, vertex and index modes,
filters, and a missing UV channel.
