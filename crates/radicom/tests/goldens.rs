//! Golden snapshot baselines (docs/02-development-guide.md §5 §4).
//!
//! The golden files under `tests-data/goldens/` are produced by
//! `cargo xtask regen-goldens` from deterministic corpus files. These tests fail
//! when the produced output drifts from the committed golden.

use std::path::PathBuf;

use radicom::io::file::{DicomFile, ReadOptions};

fn tests_data() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests-data")
}

#[test]
fn dump_goldens_are_current() {
    let cases = [
        ("explicit-le/GH178.dcm", "GH178.dump.txt"),
        ("media/DICOMDIR", "dicomdir.dump.txt"),
    ];
    for (source, golden) in cases {
        let bytes = std::fs::read(tests_data().join(source))
            .unwrap_or_else(|e| panic!("read {source}: {e}"));
        let file = DicomFile::from_bytes(&bytes, &ReadOptions::default())
            .unwrap_or_else(|e| panic!("parse {source}: {e}"));
        let produced = file.dataset.to_dump_string();
        let expected = std::fs::read_to_string(tests_data().join("goldens").join(golden))
            .unwrap_or_else(|e| panic!("read golden {golden}: {e}"));
        assert_eq!(
            produced, expected,
            "dump golden {golden} is stale; run `cargo xtask regen-goldens`"
        );
    }
}
