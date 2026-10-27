//! JSON golden snapshot baseline (docs/09-testing.md §4).
//!
//! Regenerate with `cargo xtask regen-goldens`; this test fails when the JSON
//! output drifts from the committed `tests-data/goldens/GH178.json`.

use std::path::PathBuf;

use radicom::io::file::{DicomFile, ReadOptions};
use radicom_json::{dataset_to_json, JsonOptions};

fn tests_data() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests-data")
}

#[test]
fn json_golden_is_current() {
    let bytes = std::fs::read(tests_data().join("explicit-le/GH178.dcm"))
        .expect("read GH178.dcm");
    let file = DicomFile::from_bytes(&bytes, &ReadOptions::default()).expect("parse GH178.dcm");
    let produced = dataset_to_json(&file.dataset, &JsonOptions::default()).expect("encode json");
    let expected = std::fs::read_to_string(tests_data().join("goldens/GH178.json"))
        .expect("read golden GH178.json");
    assert_eq!(produced, expected, "json golden is stale; run `cargo xtask regen-goldens`");
}
