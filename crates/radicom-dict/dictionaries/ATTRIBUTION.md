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
