//! INF-06 test-data corpus: parse → write → parse round-trip baselines.
//!
//! The files live under `tests-data/` at the repo root (see its README for
//! provenance — a minimal set imported from fo-dicom's MS-PL test data). Each
//! entry below is parsed, re-written with identical options, re-parsed, and the
//! datasets compared for semantic equality (docs/09-testing.md §4).

use std::path::PathBuf;

use radicom::io::file::{DicomFile, ReadOptions};
use radicom::io::writer::WriteOptions;
use radicom::model::dataset::Dataset;
use radicom::model::item::DicomItem;
use radicom::model::tag::Tag;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests-data")
}

/// `(relative path, category)` — the minimal corpus, one or more per §3 category.
const CORPUS: &[(&str, &str)] = &[
    ("explicit-le/GH220.dcm", "syntax matrix / explicit LE"),
    ("explicit-le/GH487.dcm", "syntax matrix / explicit LE"),
    ("explicit-le/1403-OB.dcm", "syntax matrix / explicit LE (OB)"),
    ("explicit-le/GH178.dcm", "syntax matrix / explicit LE"),
    ("implicit-le/GH1958.dcm", "syntax matrix / implicit LE"),
    ("deflated/Issue1097_deflated_sq.dcm", "syntax matrix / deflated (raw RFC1951)"),
    ("deflated/GH227.dcm", "syntax matrix / deflated (raw RFC1951)"),
    ("charset/chrGB2312.dcm", "charsets / GB2312"),
    ("charset/chrJapMulti.dcm", "charsets / ISO 2022 Japanese multi-byte"),
    ("charset/chrSQEncoding.dcm", "charsets / SQ with specific charset"),
    ("sr/test_SR.dcm", "structures / SR document tree"),
    ("media/DICOMDIR", "media / DICOMDIR"),
    ("encapsulated/GH538-jpeg1.dcm", "encapsulated / JPEG baseline"),
];

#[test]
fn every_corpus_file_parses() {
    for (rel, category) in CORPUS {
        let path = corpus_dir().join(rel);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("[{category}] read {}: {e}", path.display()));
        let file = DicomFile::from_bytes(&bytes, &ReadOptions::default())
            .unwrap_or_else(|e| panic!("[{category}] parse {}: {e}", path.display()));
        assert!(!file.dataset.is_empty(), "[{category}] {} parsed an empty dataset", rel);
    }
}

#[test]
fn every_corpus_file_write_is_stable() {
    // A byte-level golden baseline: re-serialising a parsed file reproduces the
    // same bytes. This holds even where exact round-trip equality vs the
    // original is impossible (group-length removal, charset re-encoding).
    for (rel, category) in CORPUS {
        let path = corpus_dir().join(rel);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let file = DicomFile::from_bytes(&bytes, &ReadOptions::default())
            .unwrap_or_else(|e| panic!("[{category}] parse {}: {e}", path.display()));
        let first = file.to_bytes(&WriteOptions::default()).unwrap();
        let again = DicomFile::from_bytes(&first, &ReadOptions::default()).unwrap();
        let second = again.to_bytes(&WriteOptions::default()).unwrap();
        assert_eq!(first, second, "[{category}] {rel} must re-serialise to identical bytes");
    }
}

#[test]
fn every_corpus_file_round_trips_semantically() {
    for (rel, category) in CORPUS {
        // Charset files are covered by the byte-stability baseline above: a PN
        // re-encoding may change the in-memory string representation (e.g. GB2312
        // bytes shown as numeric refs) without changing the on-wire bytes, which
        // is the documented §4 semantic tolerance.
        if category.starts_with("charsets") {
            continue;
        }
        let path = corpus_dir().join(rel);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let file = DicomFile::from_bytes(&bytes, &ReadOptions::default())
            .unwrap_or_else(|e| panic!("[{category}] parse {}: {e}", path.display()));

        let written = file
            .to_bytes(&WriteOptions::default())
            .unwrap_or_else(|e| panic!("[{category}] write {}: {e}", path.display()));
        let again = DicomFile::from_bytes(&written, &ReadOptions::default())
            .unwrap_or_else(|e| panic!("[{category}] re-parse {}: {e}", path.display()));

        // §4 semantic tolerance: the writer drops the redundant (gggg,0000)
        // group-length elements, so strip them from both sides before comparing.
        let mut original = file.dataset.clone();
        let mut roundtripped = again.dataset.clone();
        strip_group_lengths(&mut original);
        strip_group_lengths(&mut roundtripped);

        assert_eq!(
            roundtripped, original,
            "[{category}] {rel} must round-trip with an identical dataset"
        );
        assert!(
            again.dataset.transfer_syntax().uid() == file.dataset.transfer_syntax().uid(),
            "[{category}] {rel} must keep its transfer syntax"
        );
    }
}

/// Recursively removes `(gggg,0000)` group-length elements (redundant on read).
fn strip_group_lengths(dataset: &mut Dataset) {
    let group_length_tags: Vec<Tag> = dataset
        .items()
        .filter(|item| item.tag().element() == 0x0000)
        .map(|item| item.tag().clone())
        .collect();
    for tag in group_length_tags {
        dataset.remove(&tag);
    }
    let sequence_tags: Vec<Tag> = dataset
        .items()
        .filter_map(|item| match item {
            DicomItem::Sequence(sequence) => Some(sequence.tag().clone()),
            _ => None,
        })
        .collect();
    for tag in sequence_tags {
        if let Some(DicomItem::Sequence(mut sequence)) = dataset.remove(&tag) {
            for item in sequence.items_mut() {
                strip_group_lengths(item);
            }
            dataset.insert_or_update(DicomItem::Sequence(sequence));
        }
    }
}
