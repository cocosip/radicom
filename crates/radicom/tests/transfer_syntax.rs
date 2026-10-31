use radicom::model::transfer_syntax::{builtin_entries, parse, query};
use radicom::{syntaxes, Endian, Uid, UidType};

#[test]
fn builtin_metadata_matches_fo_dicom_reference() {
    let mut expected_count = 0;
    for line in include_str!("fixtures/fo-dicom-transfer-syntaxes.tsv").lines() {
        if line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 10);
        let uid = fields[0];
        let ts = query(uid).unwrap_or_else(|| panic!("missing built-in {uid}"));
        assert_eq!(ts.name(), fields[1], "{uid}: name");
        for (actual, expected, flag) in [
            (ts.is_retired(), fields[2], "retired"),
            (ts.is_explicit_vr(), fields[3], "explicit VR"),
            (ts.is_encapsulated(), fields[4], "encapsulated"),
            (ts.is_lossy(), fields[5], "lossy"),
            (ts.is_deflate(), fields[6], "deflate"),
            (ts.swap_pixel_data(), fields[7], "pixel swap"),
        ] {
            assert_eq!(actual, expected.parse::<bool>().unwrap(), "{uid}: {flag}");
        }
        assert_eq!(
            ts.endian(),
            if fields[8] == "Big" {
                Endian::Big
            } else {
                Endian::Little
            },
            "{uid}: endian"
        );
        assert_eq!(
            ts.lossy_compression_method().unwrap_or("-"),
            fields[9],
            "{uid}: lossy method"
        );
        assert_eq!(parse(uid).unwrap(), ts);
        expected_count += 1;
    }
    // fo-dicom exposes this virtual syntax but does not include it in KnownEntries.
    assert_eq!(builtin_entries().len(), expected_count + 1);
}

#[test]
fn htj2k_constants_resolve_standard_wire_uids() {
    for (ts, uid, lossy) in [
        (&*syntaxes::HTJ2K_LOSSLESS, "1.2.840.10008.1.2.4.201", false),
        (
            &*syntaxes::HTJ2K_LOSSLESS_RPCL,
            "1.2.840.10008.1.2.4.202",
            false,
        ),
        (&*syntaxes::HTJ2K_LOSSY, "1.2.840.10008.1.2.4.203", true),
    ] {
        let parsed = parse(uid).unwrap();
        assert_eq!(&parsed, ts);
        assert_eq!(ts.uid(), uid);
        assert!(parsed.is_encapsulated());
        assert_eq!(parsed.is_lossy(), lossy);
    }
    assert!(query("1.2.840.10008.1.2.4.109").is_none());
    assert_eq!(
        parse("1.2.840.10008.1.2.4.108")
            .unwrap()
            .lossy_compression_method(),
        Some("ISO_23008_2")
    );
    assert!(!parse("1.2.840.10008.1.2.4.110").unwrap().is_lossy());
}

#[test]
fn unlisted_transfer_syntax_preserves_uid_metadata() {
    let jpeg_xl = parse("1.2.840.10008.1.2.4.110").unwrap();
    assert_eq!(jpeg_xl.name(), "JPEG XL Lossless");
    assert!(!jpeg_xl.is_retired());

    Uid::register(
        "1.2.999.718",
        "Retired custom syntax",
        UidType::TransferSyntax,
        true,
    );
    let custom = parse("1.2.999.718").unwrap();
    assert_eq!(custom.name(), "Retired custom syntax");
    assert!(custom.is_retired());
    assert!(custom.is_explicit_vr() && custom.is_encapsulated());
    assert_eq!(custom.endian(), Endian::Little);
}
