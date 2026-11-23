#![no_main]
use libfuzzer_sys::fuzz_target;
use pullrun_oci::confinement::RelPath;

fuzz_target!(|data: &[u8]| {
    if let Ok(raw) = std::str::from_utf8(data) {
        // Must never panic, never accept absolute/parent-traversal paths.
        let unsafe_input = raw.starts_with('/') || raw.split('/').any(|c| c == "..");
        if unsafe_input {
            assert!(RelPath::parse(raw).is_err(), "unsafe path accepted: {raw:?}");
        } else {
            assert!(RelPath::parse(raw).is_ok(), "safe path rejected: {raw:?}");
        }
    }
});
