//! Untrusted binary or ASCII FBX bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 * 1024 {
        return;
    }
    let _ = uvcraft_io::fbx::parse(data);
    let _ = uvcraft_io::fbx::parse_layers(data);
});
