#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_EVENTS: usize = 1_024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    let Ok(root) = tempfile::tempdir() else {
        return;
    };
    let session = root.path().join("sessions/fuzz/journal");
    if std::fs::create_dir_all(&session).is_err()
        || std::fs::write(session.join("active.jsonl"), data).is_err()
        || std::fs::write(session.join("writer.lock"), []).is_err()
    {
        return;
    }
    let _ = rw_store::session::SessionEventLog::load_existing_bounded::<serde_json::Value>(
        root.path(),
        "fuzz",
        MAX_INPUT_BYTES as u64,
        MAX_EVENTS,
    );
});
