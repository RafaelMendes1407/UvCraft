//! Untrusted command scripts (JSON Lines or a JSON array), the format accepted
//! by `uvcraft run`, macros and the JSON-RPC server.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() > 16 * 1024 {
        return;
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = uvcraft_commands::parse_script(text);
    }
});
