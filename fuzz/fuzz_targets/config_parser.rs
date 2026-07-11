#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    if let Ok(input) = std::str::from_utf8(data) {
        let _ = toml::from_str::<rw_types::config::ConfigFile>(input);
    }
});
