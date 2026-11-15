//! Untrusted Wavefront OBJ bytes: parsing must return an error, never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 * 1024 {
        return;
    }
    let _ = uvcraft_io::obj::parse(data.to_vec());
});
