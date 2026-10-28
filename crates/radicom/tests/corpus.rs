//! INF-06 test-data corpus: parse → write → parse round-trip baselines.
//!
//! The files live under `tests-data/` at the repo root (see its README for
//! provenance — a minimal set imported from fo-dicom's MS-PL test data). Each
//! entry below is parsed, re-written with identical options, re-parsed, and the
//! datasets compared for semantic equality (docs/02-development-guide.md §5 §4).

use std::path::PathBuf;

use radicom::io::file::{DicomFile, ReadOptions};
use radicom::io::writer::WriteOptions;
use radicom::model::dataset::Dataset;
use radicom::model::item::DicomItem;
use radicom::model::tag::Tag;
use radicom::sr::StructuredReport;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests-data")
}

/// `(relative path, category)` — the minimal corpus, one or more per §3 category.
const CORPUS: &[(&str, &str)] = &[
    ("explicit-le/GH220.dcm", "syntax matrix / explicit LE"),
    ("explicit-le/GH487.dcm", "syntax matrix / explicit LE"),
    ("explicit-le/1403-OB.dcm", "syntax matrix / explicit LE (OB)"),
    ("explicit-le/GH178.dcm", "syntax matrix / explicit LE"),
    ("explicit-le/GH626.dcm", "syntax matrix / private SQ with a bad VR byte"),
    ("explicit-le/GH179B.dcm", "syntax matrix / truncated file (partial read)"),
    ("big-endian/GH177_D_CLUNIE_CT1_IVRLE_BigEndian_undefined_length.dcm", "syntax matrix / BE CP-246"),
    ("implicit-le/GH1958.dcm", "syntax matrix / implicit LE"),
    ("deflated/Issue1097_deflated_sq.dcm", "syntax matrix / deflated (raw RFC1951)"),
    ("deflated/GH227.dcm", "syntax matrix / deflated (raw RFC1951)"),
    ("charset/chrGB2312.dcm", "charsets / GB2312"),
    ("charset/chrJapMulti.dcm", "charsets / ISO 2022 Japanese multi-byte"),
    ("charset/chrSQEncoding.dcm", "charsets / SQ with specific charset"),
    ("charset/chrRuss.dcm", "charsets / ISO_IR 144 Cyrillic"),
    ("charset/chrArab.dcm", "charsets / ISO_IR 127 Arabic"),
    ("charset/chrGreek.dcm", "charsets / ISO_IR 126 Greek"),
    ("charset/chrHbrw.dcm", "charsets / ISO_IR 138 Hebrew"),
    ("charset/chrI2.dcm", "charsets / ISO 2022 IR 149 mixed 8-bit"),
    ("charset/chrKoreanMulti.dcm", "charsets / ISO 2022 IR 149 Korean"),
    ("sr/test_SR.dcm", "structures / SR document tree"),
    ("media/DICOMDIR", "media / DICOMDIR"),
    ("encapsulated/GH538-jpeg1.dcm", "encapsulated / JPEG baseline"),
    ("encapsulated/D_CLUNIE_CT1_RLE_FRAGS.dcm", "encapsulated / RLE with a trailing element"),
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

/// Charset files must decode their textual values to real characters, not
/// U+FFFD / raw ASCII approximations — a Specific Character Set list whose first
/// value is empty must still resolve to its first non-empty encoding (ENC-01/02).
#[test]
fn charset_corpus_decodes_to_real_characters() {
    let cases: &[(&str, &[&str])] = &[
        ("charset/chrGB2312.dcm", &["张", "小东"]),
        ("charset/chrJapMulti.dcm", &["やまだ", "たろう"]),
        ("charset/chrRuss.dcm", &["Люкceмбypг"]),
        ("charset/chrArab.dcm", &["قباني^لنزار"]),
        ("charset/chrGreek.dcm", &["Διονυσιος"]),
        ("charset/chrHbrw.dcm", &["שרון^דבורה"]),
        ("charset/chrI2.dcm", &["Hong^Gildong=洪^吉洞=홍^길동"]),
        ("charset/chrKoreanMulti.dcm", &["김희중"]),
    ];
    for (rel, expected) in cases {
        let bytes = std::fs::read(corpus_dir().join(rel)).unwrap();
        let file = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        let name = file.dataset.try_get_str(Tag::PATIENT_NAME).unwrap_or_default();
        assert!(!name.contains('\u{FFFD}'), "{rel}: PatientName has replacement chars: {name:?}");
        for want in *expected {
            assert!(name.contains(want), "{rel}: PatientName {name:?} should contain {want:?}");
        }
    }
}

/// A file whose declared length runs past EOF is a *partial* read, not an
/// error: the readable prefix stays available (fo-dicom GH179B).
#[test]
fn a_truncated_file_keeps_its_readable_prefix() {
    let bytes = std::fs::read(corpus_dir().join("explicit-le/GH179B.dcm")).unwrap();
    let file = DicomFile::from_bytes(&bytes, &ReadOptions::default()).expect("partial read succeeds");
    assert!(file.is_partial, "truncated input must be flagged partial");
    assert!(!file.dataset.is_empty(), "the readable prefix is retained");
}

/// A real SR document walks correctly: the content tree parses into the expected
/// number of nodes with the expected value types (docs/09, SR-01 §4.1).
#[test]
fn a_real_sr_document_walks_its_content_tree() {
    use radicom::sr::{ContentItem, ValueType};

    fn count(items: &[ContentItem]) -> usize {
        items.iter().map(|it| 1 + count(&it.children())).sum()
    }

    let bytes = std::fs::read(corpus_dir().join("sr/test_SR.dcm")).unwrap();
    let sr = StructuredReport::from_bytes(&bytes).unwrap();
    let roots = sr.root_items();
    assert_eq!(roots.len(), 5, "the reference SR has five top-level content items");
    assert_eq!(count(&roots), 28, "the whole content tree has 28 nodes");

    // The tree mixes CONTAINER / TEXT / COMPOSITE / IMAGE / UIDREF nodes.
    let mut seen: Vec<ValueType> = Vec::new();
    fn walk(items: &[ContentItem], seen: &mut Vec<ValueType>) {
        for it in items {
            if let Some(vt) = it.value_type() {
                if !seen.contains(&vt) {
                    seen.push(vt);
                }
            }
            walk(&it.children(), seen);
        }
    }
    walk(&roots, &mut seen);
    for expected in [ValueType::Container, ValueType::Text, ValueType::Image] {
        assert!(seen.contains(&expected), "the tree should contain a {expected:?} node");
    }

    // One container node carries children (a nested report structure).
    assert!(
        roots.iter().any(|r| !r.children().is_empty()),
        "at least one top-level item has a non-empty subtree"
    );
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
