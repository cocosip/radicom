# Dictionary Data Provenance

The files in this directory are verbatim copies of data files from the
[fo-dicom](https://github.com/fo-dicom/fo-dicom) project (Fellow Oak DICOM),
Copyright (c) 2012-2026 fo-dicom contributors, licensed under the
**Microsoft Public License (MS-PL)**.

| File | Origin |
|---|---|
| `DICOMDictionary.xml.gz` | fo-dicom `FO-DICOM.Core/Dictionaries/DICOMDictionary.xml.gz` (dictionary version 2026b, 5347 tags) |
| `PrivateDictionary.xml.gz` | fo-dicom `FO-DICOM.Core/Dictionaries/PrivateDictionary.xml.gz` (4678 tags, 235 private creators) |
| `uids.tsv` | Extracted one-time from fo-dicom `FO-DICOM.Core/DicomUIDGenerated.cs` (1928 registered UIDs; columns: uid, keyword, name, type code, retired). See the type-code mapping in the generated `uids.rs` header. |

These data files are inputs to `xtask gen-dict`, which generates the Rust
tables under `src/generated/`. The generated files retain this MS-PL
attribution in their headers. Regenerate after replacing the data files:

```
cargo xtask gen-dict
```

## Anonymizer profile

`crates/radicom/src/anonymizer/ps3.15.csv` is the PS3.15 de-identification
map, extracted one-time from fo-dicom
`FO-DICOM.Core/DicomAnonymizerGenerated.cs` (`DefaultProfile`, 655 rows).
Columns: `tag-pattern; BasicProfile; RetainSafePrivate; RetainUIDs;
RetainDeviceIdent; RetainInstitutionIdent; RetainPatientChars;
RetainLongFullDates; RetainLongModifDates; CleanDesc; CleanStructdCont;
CleanGraph`. It is compiled into `radicom` with `include_str!` and parsed by
the dependency-free matcher in `radicom::anonymizer::profile`, so the core
crate needs no regex dependency. Same MS-PL attribution applies.
