#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_INPUT_BYTES: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let mut decoder = rw_ext::FrameDecoder::new(MAX_FRAME_BYTES);
    for chunk in data.chunks(31) {
        let _ = decoder.push(chunk);
    }
    assert!(decoder.buffered_bytes() <= MAX_FRAME_BYTES);
});
