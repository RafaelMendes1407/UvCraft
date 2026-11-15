//! Untrusted `.gltf` JSON or `.glb` container bytes. External buffer URIs are
//! resolved against a directory that does not exist, so only embedded data
//! (GLB BIN chunk, data URIs) is ever read.
#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 * 1024 {
        return;
    }
    let base = Path::new("/nonexistent-uvcraft-fuzz-dir");
    let _ = uvcraft_io::gltf::parse(data.to_vec(), base);
});
