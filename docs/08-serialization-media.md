# Module Spec: Serialization, Media, Structured Reports, Printing, Anonymizer

> Status: design phase (milestone M7 for the P1 parts; printing and DSLs are P2/M8 — see [03-development-plan.md](03-development-plan.md) §3)
> Feature IDs covered: JSON-01..02, XML-01, MEDIA-01..03, SR-01..02, PRINT-01, ANON-01, INF-03..04
> Behavior baseline: fo-dicom `Serialization/` (DicomJson, DicomXML), `Media/` (DicomDirectory, IIconGenerator), `StructuredReport/`, `Printing/`, `DicomAnonymizer*`

---

## 1. DICOM JSON (PS3.18) — radicom-json (JSON-01/02)

### 1.1 Model

Object per attribute: key = 8-hex-digit tag `GGGGEEEE`; value object always contains `"vr"` plus value carriers:

| Carrier | Used for |
|---|---|
| `"Value": [...]` | all string/numeric VRs (empty element or empty string ⇒ omitted or null per rules below) |
| `"InlineBinary": "<base64>"` | OB/OD/OF/OL/OV/OW/UN |
| `"BulkDataURI": "..."` | when the buffer is a `BulkDataUri` buffer (bidirectional, JSON-02) |
| `"Value": [ {Alphabetic/Ideographic/Phonetic} ]` | PN groups |

- SQ: `"Value": [ <nested attribute object>, ... ]` (recursion).
- AT: 8-hex-digit strings.
- Group-length `(GGGG,0000)` elements are skipped on write.
- Fragment sequences are **not representable**: serialization returns an error suggesting transcoding (match fo-dicom).

### 1.2 Options

```rust
pub struct JsonOptions { pub tags_as_keywords: bool /*non-standard: "PatientID" keys*/,
                          pub validate_on_read: bool, pub numbers: NumberMode,
                          pub format_indented: bool }
pub enum NumberMode { AsNumber, AsString, PreferablyAsNumber }  // applies to IS/DS/SV/UV
```

DS string form preserves trailing zeros exactly; `PreferablyAsNumber` writes numbers when lossless, strings otherwise; NaN/Infinity always serialize as strings.

### 1.3 Reading rules

- `"vr"` may appear at any property position — locate it first on a token copy, then iterate.
- Accept numbers or strings for numeric VRs; accept unknown extra properties (skip by depth); tolerate property duplicates by last-wins.
- Private creator elements `(gggg,00xx)` are applied to build private tags of that group on the fly.
- Missing `"vr"` ⇒ hard error for that attribute.

### 1.4 API

```rust
pub fn dataset_to_json(ds: &Dataset, opts: &JsonOptions) -> Result<String>;
pub fn json_to_dataset(json: &str, opts: &JsonOptions) -> Result<Dataset>;
// serde integration: DicomDatasetJsonConverter implementing serde_json serialize/deserialize
```

## 2. DICOM XML Export (XML-01, P2)

One-way export: root `NativeDicomModel`; per attribute `DicomAttribute Keyword/Tag/VR[/PrivateCreator]`; values as `<Value number="n">`; PN expands to component elements; binary as `<InlineBinary>` base64 (fragments: repeated InlineBinary elements). No reader.

## 3. Media — DICOMDIR (MEDIA-01..03)

### 3.1 Structure

`DicomDirectory : DicomFile` over SOP class Media Storage Directory. Records live in `(0004,1220)` DirectoryRecordSequence; each record carries `DirectoryRecordType` (PATIENT/STUDY/SERIES/IMAGE / "SR DOCUMENT" / PRESENTATION), next-sibling offset `(0004,1400)` and first-child offset `(0004,1420)`.

### 3.2 Reading

A dedicated `ParseObserver` records every sequence item keyed by its **file byte offset**; tree stitching follows the offset links from the root first/last record offsets `(0004,1200)/(0004,1512)`; a missing offset falls back to the nearest recorded offset (dirty-DICOMDIR tolerance). Exposes `root_records()`, iteration helpers per level, and lookup by key (PatientID / StudyUID / SeriesUID).

### 3.3 Writing (two-pass offsets)

1. Serialize all records to items with placeholder offsets while computing each record's exact encoded size via the length calculator (item header 8 + payload + delimiter 8 when undefined).
2. Patch `next/lower` offsets and root first/last; group lengths recalculated.
3. `FileSetID`, descriptor `(0004,1202..1212)` maintained on the dataset.

### 3.4 AddFile

`add_file(dicom_file, referenced_file_id)`: find-or-create PATIENT (by PatientID) → STUDY (StudyInstanceUID) → SERIES (SeriesInstanceUID) records (sibling-chain walk), then append an IMAGE / "SR DOCUMENT" / PRESENTATION record per storage category with `(0004,1511..1517)` references (FileID, SOPClass, SOPInstance, TransferSyntax). Optional icon: `(0000,9113)` IconImageSequence via `IconGenerator` (MEDIA-02: trait + built-in ≤128×128 8-bit MONOCHROME2 builder; frame selection: RepresentativeFrameNumber else midpoint).

### 3.5 File scanner (MEDIA-03, P2)

Async directory walk with glob filter; events `on_progress/on_file`; output feeds `DicomDirectoryBuilder`. Recursion depth and error policy configurable.

## 4. Structured Reports (SR-01/02)

### 4.1 Content tree

`ContentItem` wraps one dataset: `Code` (ConceptNameCodeSequence), `ValueType` (14: CONTAINER/TEXT/CODE/NUM/PNAME/DATE/TIME/DATETIME/UIDREF/COMPOSITE/IMAGE/WAVEFORM/SCOORD/TCOORD), `Relationship` (7: CONTAINS/HAS PROPERTIES/INFERRED FROM/SELECTED FROM/HAS OBS CONTEXT/HAS ACQ CONTEXT/HAS CONCEPT MOD), `Continuity`. Children = ContentSequence items; add/remove with relationship maintenance.

Type-checked constructors write the correct value tag per type (TextValue / PersonName / Date/Time/DateTime / UID / ConceptCodeSequence / MeasuredValueSequence / ReferencedSOPSequence).

### 4.2 Support types (SR-02)

- `CodeItem`: auto-dispatch `(0018,0090..)` short (≤16) / long / URN code value by length/prefix; equality = value + scheme + version.
- `MeasuredValue` { value, units: CodeItem }; `ReferencedSop { class_uid, instance_uid }`.

### 4.3 Open/Save

`StructuredReport::open(path/stream)` wraps `DicomFile::open`; `save` forces explicit-length SQ and items (fo-dicom behavior).

## 5. Printing Data Models and Layout Rendering (PRINT-01/02, P2)

- `FilmSession : Dataset` — film destination/label/medium type/priority/copies/color; manages child FilmBoxes and PresentationLuts (`create/find/delete`).
- `FilmBox : Dataset` — `ImageDisplayFormat` parser (`STANDARD\C,R`, `ROW\R1,R2..`, `COL\C1,C2..`, `SLIDE`, `SUPERSLIDE`, `CUSTOM\i`), film size/orientation/magnification/density/trim/resolution; `initialize()` lays out ImageBoxes (BasicGrayscale/BasicColorImageBox SOP choice) filling ReferencedImageBoxSequence; layout rectangle computation for the three composite formats:
  - `STANDARD\C,R`: a C×R grid of equal cells within the film bounds minus margins;
  - `ROW\R1,R2,..`: row-major cells with per-row column counts;
  - `COL\C1,C2,..`: column-major cells with per-column row counts;
  - film size → physical dimensions: `8INX10IN`-style inches, `A4/A3`, `24CMX30CM`-style cm conversions.
- `ImageBox : Dataset` — position, polarity, per-box overrides with FilmBox fallbacks, `ImageSequence` payload.
- `PresentationLut : Dataset` — PresentationLUTSequence / LUT descriptor+data / shape (IDENTITY, LIN OD).
- Persistence: save/load session graph (`.dcm` per box + text dump + images folder) for debugging/print workflows.
- **PRINT-02 layout rendering**: `FilmBox::render_to_image(&self, margins) -> Image` — fill background with EmptyImageDensity (BLACK/WHITE/OD value), draw the Trim border when enabled, render each ImageBox's `ImageSequence` through the render pipeline ([10-rendering.md](10-rendering.md) §1) and scale it into its layout cell; Polarity REVERSE inverts the cell. Cross-platform replacement for fo-dicom's GDI+ print extensions, built on the REND-07 backend.

No network print SCU/SCP in radicom (matches fo-dicom Core; N-services SRV-07 cover the DIMSE layer).

## 6. Anonymizer (ANON-01)

### 6.1 Profile format

Built-in profile: the PS3.15 BasicProfile table generated into `radicom-dict` (source: fo-dicom `DicomAnonymizerGenerated` CSV). Row shape:

```
tag-regex ; action(BasicProfile) ; action(RetainSafePrivate) ; ... ; action(CleanGraph)
```

Options (11) in column order: BasicProfile, RetainSafePrivate, RetainUIDs, RetainDeviceIdent, RetainInstitutionIdent, RetainPatientChars, RetainLongFullDates, RetainLongModifDates, CleanDesc, CleanStructdCont, CleanGraph. Custom profiles load from the same CSV shape.

Actions: **D** replace with non-zero dummy, **Z** zero-length, **X** remove, **K** keep, **C** clean to safe equivalent, **U** consistent new UID.

### 6.2 Application algorithm

```
anonymize_in_place(dataset):
  for each item (recursive into SQ unless the matched action is K):
    match tag string against profile regex rows (first match wins; catch-all rows cover
      all odd groups, curve 50xx, overlay 60xx 3000/4000 as in the built-in table)
    D/U/C: UI elements → consistent regenerated UID; string → profile.PatientName/PatientID if the
           tag is (0010,0010)/(0010,0020) else "ANONYMOUS"; other typed values → type-aware dummy
    Z: blank (SQ → empty; DA/DT/TM → empty or minimum per type; binary → empty; numeric → zero-values
       non-zero-length dummy per fo-dicom semantics)
    X: collect, remove after traversal
    K: untouched
anonymize_in_place(file): also sync FileMeta.MediaStorageSOPInstanceUID with the mapped instance UID
anonymize(file|dataset) = clone + in-place
```

The generator instance keeps `replaced_uids` across calls for consistency.

## 7. Dump and Logging (INF-03/04)

> Implemented: `radicom::dump` holds the formatter (`dump_dataset`/`DumpOptions`).
> `radicom::logging` is the `tracing` facade: `targets::{NETWORK,CODEC,IO,ENCODING,PRINT}`,
> `Dataset::to_dump_string()` / `to_dump_string_with(options)` / `write_to_log(level)`.
> `radicom::io::trace` provides `TraceObserver` (a `ParseObserver` logging every
> parse event at debug on `radicom::io`) and a `trace(bytes, syntax)` helper.

- `dataset.to_dump_string(opts)` / `write_to_log(level)`: hierarchical indentation, `TAG VR Name : value` per line; string truncation (default 64 chars), binary as hex (first N bytes), SQ depth markers; console colorization optional.
- Parser trace: an observer that logs every parse event at debug level (`radicom::io::trace`).
- Log target names: `radicom::network`, `radicom::codec`, `radicom::io`, `radicom::encoding`, `radicom::print`.

## 8. Test Requirements Checklist

- [ ] JSON: golden files from fo-dicom output; round-trip (read fo-dicom JSON → radicom dataset → JSON → equal); PN groups; numeric modes; BulkDataURI both directions; fragment rejection message; unordered/unknown properties on read.
- [ ] XML: golden export snapshot tests (P2).
- [ ] DICOMDIR: build from a file set → byte-verify offsets against fo-dicom-produced DICOMDIR; re-read tree equality; nearest-offset tolerance case; icon builder output validation.
- [ ] SR: walk a real SR document; type-checked constructor matrix; save with explicit lengths re-opens.
- [ ] Printing: display-format parser table; layout rectangle goldens (P2).
- [ ] Anonymizer: apply the full built-in profile to a corpus; assert per-tag actions (sampled), UID consistency across calls, meta sync, clone-vs-inplace equivalence; custom CSV profile load.
- [ ] Dump: stable snapshot for a reference dataset.
