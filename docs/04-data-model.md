# Module Spec: Data Model (Identifiers, Dataset, Elements, Charsets, Validation)

> Status: design phase (milestones M1–M2; see [03-development-plan.md](03-development-plan.md) §3)
> Feature IDs covered: TAG-01..05, VR-01..02, UID-01..04, TS-01..03, DICT-01..03, VM-01, DS-01..10, ELEM-01..05, ENC-01..04, VAL-01..03
> Behavior baseline: fo-dicom `DicomTag.cs`, `DicomVR.cs`, `DicomElement.cs`, `DicomDataset.cs`, `DicomUID*.cs`, `DicomTransferSyntax.cs`, `DicomDictionary*.cs`, `DicomEncoding.cs`, `DicomValidation.cs` (v5.2.x / 6.0.0-alpha)

---

## 1. Tag

### 1.1 Representation

```rust
pub struct Tag { group: u16, element: u16, private_creator: Option<Arc<PrivateCreator>> }
```

- `is_private()` = odd group.
- Conversion `u32 ↔ Tag` (big-endian packing: `(group << 16) | element`).
- Constant `Tag::UNKNOWN = (FFFF,FFFF)`.

### 1.2 Ordering and equality semantics (critical — Dataset storage depends on it)

| Comparison | Rule |
|---|---|
| Both public | compare `group`, then `element` |
| Both private, same group, same creator | compare only the **low 8 bits** of `element` (block-relative ordering) |
| Otherwise | compare `group`, then `element`, then creator string |

`Hash` follows the same normalization (private tags hash by group + low byte + creator hash). This mirrors fo-dicom and guarantees the sorted container orders private blocks stably regardless of which concrete block a private tag was resolved to.

### 1.3 String formats

| Format | Example | Producer/Consumer |
|---|---|---|
| General `(GGGG,EEEE)` / `(GGGG,EEEE:CREATOR)` | `(0028,0010)`, `(0029,1001: Vendor X)` | Display/Parse default |
| No parentheses | `0028,0010` | Parse accepted |
| JSON `GGGGEEEE` (8 hex digits) | `00280010` | DICOM JSON |
| Masked | `(60xx,4000)` | MaskedTag only |

Parse failures return `Error::Data` with the offending input; the creator string is interned through the dictionary's creator cache (see §6.4).

### 1.4 Masked tags (TAG-05)

`MaskedTag { card: u32, mask: u32 }`. `parse("(60xx,4000)")`: each hex nibble maps to `0xF` in the mask, each `x` to `0x0`. `is_match(tag)` = `(tag.to_u32() & mask) == card`. Display re-derives the `x` positions from the mask. Used by: overlay repeating groups, anonymizer wildcard rules, dictionary masked entries.

### 1.5 Interning index (TAG-04)

Global `LazyLock<HashMap<u32, Tag>>` built from the generated constants (radicom-dict). `Tag::lookup_or_create(group, element)` returns the interned instance for known tags or constructs a fresh `Tag`. The parser and AT-element decoding must route through this to avoid per-element allocations.

## 2. VR

### 2.1 Enum and metadata

```rust
pub enum Vr { /* 34 standard + */ Item, ItemDelim, SeqDelim, Implicit }
```

Per-VR attributes (compile-time constants): `code`, `name`, `is_string`, `is_character_encoded`, `is_16bit_length`, `is_multi_value`, `padding`, `max_value_length`, `unit_size`, `byte_swap` (unit for endianness conversion).

### 2.2 Encoding attribute table (authoritative)

| VR | string | charset-sensitive | length field | pad | max len/value | unit bytes | validator |
|---|---|---|---|---|---|---|---|
| AE | yes | no | 16 | SP | 16 | 1 | AE |
| AS | yes | no | 16 | SP | 4 | 1 | AS |
| AT | no | – | 16 | 0x00 | 4 | 4 (swap 2) | – |
| CS | yes | no | 16 | SP | 16 | 1 | CS |
| DA | yes | no | 16 | SP | 18 (range) | 1 | DA |
| DS | yes | no | 16 | SP | 16 | 1 | DS |
| DT | yes | no | 16 | SP | 54 | 1 | DT |
| FD | no | – | 16 | 0x00 | 8 | 8 | – |
| FL | no | – | 16 | 0x00 | 4 | 4 | – |
| IS | yes | no | 16 | SP | 12 | 1 | IS |
| LO | yes | **yes** | 16 | SP | 64 | 1 | LO |
| LT | yes | yes | 16 | SP | 10240 | 1 | LT |
| OB | no | – | **32** | 0x00 | ∞ | 1 | – |
| OD | no | – | 32 | 0x00 | ∞ | 8 | – |
| OF | no | – | 32 | 0x00 | ∞ | 4 | – |
| OL | no | – | 32 | 0x00 | ∞ | 4 | – |
| OV | no | – | 32 | 0x00 | ∞ | 8 | – |
| OW | no | – | 32 | 0x00 | ∞ | 2 | – |
| PN | yes | yes | 16 | SP | 64 per group | 1 | PN |
| SH | yes | yes | 16 | SP | 16 | 1 | SH |
| SL | no | – | 16 | 0x00 | 4 | 4 | – |
| SQ | no | – | 32 | SP | – | 0 | – |
| SS | no | – | 16 | 0x00 | 2 | 2 | – |
| ST | yes | yes | 16 | SP | 1024 | 1 | ST |
| SV | no | – | **32** | 0x00 | 8 | 8 | – |
| TM | yes | no | 16 | SP | 16 | 1 | TM |
| UC | yes | yes | 32 | SP | ∞ | 1 | – |
| UI | yes | no | 16 | **0x00** | 64 | 1 | UI |
| UL | no | – | 16 | 0x00 | 4 | 4 | – |
| UN | no | – | 32 | 0x00 | ∞ | 1 | – |
| UR | yes | yes | 32 | SP | ∞ | 1 | – |
| UT | yes | yes | 32 | SP | ∞ | 1 | – |
| US | no | – | 16 | 0x00 | 2 | 2 | – |
| UV | no | – | **32** | 0x00 | 8 | 8 | – |

### 2.3 Parsing (VR-02)

- `Vr::parse(&[u8;2])`: dispatch on the **first byte** via a static lookup table (hot path); only ambiguous first bytes check the second byte. Unknown codes → `Error::Data`.
- `Vr::parse_str("PN")` for string input.

## 3. UID

### 3.1 Structure and typing

```rust
pub struct Uid { value: Arc<str>, name: Option<&'static str>, uid_type: UidType, retired: bool }
pub enum UidType { TransferSyntax, SopClass, MetaSopClass, ServiceClass, SopInstance,
                   ApplicationContextName, ApplicationHostingModel, CodingScheme,
                   FrameOfReference, Ldap, MappingResource, ContextGroupName, Unknown }
```

- Registry: generated static table (~1928 entries) + runtime `Register`/`Query`/`Enumerate` over a `RwLock<HashMap>`-style extension map.
- `Parse(s)`: trims spaces and `\0`; registry hits return the interned entry; misses construct an `Unknown`-type instance (not an error).
- `is_valid_uid(s)` delegates to the UI validator (§9.2).
- Storage-category heuristic (`UidType::SopClass → StorageCategory`): private root (not `1.2.840.10008.*`) → `Private`; name contains "Image Storage"/"Volume Storage" → `Image`/`Volume`; curated whitelist for PresentationState/StructuredReport/Waveform/Document/Raw.

### 3.2 Generation (UID-03)

| Function | Rule |
|---|---|
| `Uid::generate()` | UUID v4 → 128-bit big integer (little-endian byte layout, sign byte 0) → `"2.25." + decimal` (PS3.5 B.2 / ISO 9834-8) |
| `Uid::append(base, seq)` | Dot-append a monotonically increasing component |
| `UidGenerator::map(source)` | Instance-scoped cache source→new UID so repeated regeneration of the same source is consistent (used by anonymizer, `regenerate_all`) |
| `regenerate_all(dataset)` | Recursively walk SQ; replace every UI element whose resolved type is SopInstance or Unknown |

`RootUID` for sequence-based generation is configurable (global).

## 4. Transfer Syntax

### 4.1 Registry rules (TS-01/02)

- Static registry of ~57 built-in syntaxes as constants (`TransferSyntax::IMPLICIT_VR_LE`, `JPEG_PROCESS_1`, ...), each with: uid, explicit/implicit VR, encapsulated, lossy + ISO compression method, deflate flag, endianness, swap_pixel_data, retired.
- `Lookup(uid)`: registry hit → entry; miss but the UID's type is TransferSyntax → **construct on the fly** assuming Little Endian / explicit per fo-dicom fallback (encapsulated = true when the UID is not one of the known-uncompressed ones is NOT assumed — mirror fo-dicom: default explicit VR LE, encapsulated `true`); miss and unknown type → `Error::Data`.
- `Query(uid)` returns `Option` without constructing.
- `Register/Unregister`: extension point used by radicom-codecs; registering must not collide with built-ins unless replacing explicitly.

### 4.2 Virtual syntaxes (TS-03)

| Syntax | Purpose |
|---|---|
| `ImplicitVRBigEndian` (pseudo-UID `1.2.840.10008.1.2.2.123456`) | Reading files wrongly encoded as big-endian implicit VR (legal to parse, never to write) |
| `GEPrivateImplicitVRBigEndian` | Little-endian syntax with `swap_pixel_data = true` (GE export quirk) |

## 5. Dictionary

### 5.1 Entry structure

```rust
pub struct DictionaryEntry { tag: MaskedTag, name: &'static str, keyword: &'static str,
                             vrs: &'static [Vr], vm: Vm, retired: bool, private_creator: Option<...> }
pub struct Vm { min: u32, max: Option<u32>, multiplicity: u32 } // "1", "1-n", "2-2n", "1-3"
```

`Vm::parse`/`Display` round-trip; `matches(count)` implements min/max/step logic.

### 5.2 Lookup algorithm (DICT-02) — order matters

```
lookup(tag):
  1. if tag is private and has a creator:
       a. private sub-dictionary of creator: exact entry, then masked entries (xx element patterns)
       b. if element in [0x0010, 0x00FF]: return the PrivateCreator placeholder entry (LO, VM 1)
  2. exact table lookup (public)
  3. masked stack (e.g. overlay groups) in registration order
  4. fallback UnknownTag entry (any VR, VM 1-n) — never fails
```

Reverse lookup `by_keyword(keyword)` searches public then all private sub-dictionaries.

### 5.3 Loading (DICT-01/03)

- Built-in: `radicom-dict` static tables (generated; see architecture §9). No runtime XML parsing needed for the default path.
- External: `Dictionary::load_xml(reader)` accepting the fo-dicom dictionary XML shape (`<dictionaries>`/`<dictionary creator="...">`/`<tag group element keyword vr vm retired>`; multi-VR separators `_ / \ , |`; `x` nibbles → masked entries; `.gz` auto-detected). Loaded entries merge per §5.2 precedence; a one-shot `set_default_dictionary()` replaces the default (idempotent guard, mismatched repeat calls error).

## 6. Dataset

### 6.1 Storage

`items: BTreeMap<TagOrdKey, DicomItem>` where `TagOrdKey` encodes the §1.2 ordering. Invariant: **iteration order is always ascending tag order** (file write correctness). `transfer_syntax` field with cascade-to-nested-items on set; `fallback_encodings`; per-dataset `validate` flag.

### 6.2 Read API semantics (DS-02)

| Method | Missing tag | Wrong type | Empty value |
|---|---|---|---|
| `get<T>(tag)` | `Err(Data::TagNotFound)` | `Err(Data::UnexpectedType)` | numeric → `Err`; string → `Ok("")` |
| `try_get<T>(tag)` | `None` | `None` (any conversion failure → `None`) | as above but `None` on error |
| `get_or<T>(tag, default)` | `default` | `default` | `default` on error |

- `T: FromElement` implementors: `i32/i16/u16/u32/f32/f64`, `String`, `bool`(from CS YES/NO helpers is app-level, not core), `Uid`, `TransferSyntax`, `DateTime` types, `Vec<T>`, `Option<T>`-via-try, `ByteBuffer` (raw, zero-copy), enums (`FromElement` for `C:CsEnum` via macro).
- `get_str(tag)`: unified string view (multi-value joined with `\`), empty element → `""`.
- `get_sequence(tag)` / `try_get_non_empty_sequence(tag)`; `value_count(tag)` (values, or SQ item count).
- Multi-value strings: `get_strings(tag) -> Vec<String>` splits on `\`.

### 6.3 Write API (DS-03)

- `insert(item)` errors on duplicate tag (`Data::TagAlreadyExists`); `insert_or_update` replaces.
- `insert_value<V: IntoElement>(tag, value)` dispatch matrix:

| Rust value | VRs it can produce |
|---|---|
| `&str` / `String` | any string VR; multi-value VRs accept `\`-separated input parsed per-value |
| `i16/i32/u16/u32/i64/u64` | IS (decimal string) or SL/SS/UL/US/SV/UV depending on declared/target VR |
| `f32/f64` | DS (string form, ≤16 chars, precision demotion beyond 16 → 10 significant digits) or FL/FD |
| `DateTime`/`NaiveDate`/`NaiveTime` | DA/DT/TM with canonical output formats |
| `Uid` / `&TransferSyntax` | UI |
| `&[u8]` / `ByteBuffer` | OB/OW/OD/OF/OL/OV/UN |
| `Dataset` / `Sequence` | SQ |
| enum | CS (uppercase variant name) |

- Without an explicit VR, the VR is resolved from the dictionary: entry's VR whose value type matches, else the first VR; unknown public tag or private tag without creator/entry → `Err`.
- `None`-equivalent: `insert_empty(vr, tag)` writes a zero-length element (fo-dicom's null→empty semantics).
- `remove(tag) -> Option<DicomItem>`, `retain(pred)`, `clear`.
- All tag arguments pass through private-tag normalization (§6.4) first.

### 6.4 Private tag normalization (DS-04) — algorithm

```
resolve_private(tag, create):
  if tag.group even or tag.element > 0xFF: return tag unchanged (already a block tag)
  scan group's blocks 0x10..0xFF:
     read (group, block<<8 | 0x10) LO element value == tag.creator?
       hit  → block tag (group, block<<8 | element)
  no hit and create:
     allocate first free block; write the PrivateCreator LO element; return block tag
  group exhausted → None
```

Creators are interned via the dictionary creator cache so equality by pointer/string is consistent.

### 6.5 Derived operations

- `functional_groups(frame)` (DS-06): build a transient dataset = SharedFunctionalGroups items + PerFrameFunctionalGroups[frame] items, skipping empty sequences; `ReferencedImageSequence` merged with `insert_or_update` semantics (whole-item replacement like fo-dicom).
- `copy_to(dest, selector)` where selector = all | tags | masked (DS-05).
- `Clone` deep-copies with validation off (matching fo-dicom clone semantics).
- Walker (DS-09): `walk(&impl DatasetWalker)` with callbacks `on_element/on_sequence_begin/item_begin/item_end/sequence_end/on_fragment_begin/fragment/on_fragment_end`; used by the writer, dump, JSON.
- Date-time composition (DS-08): `get_datetime(date_tag, time_tag)`; `get_datetime_offset` honoring `(0008,0201)` `±hhmm`.
- Equality (DS-07): `PartialEq` = deep content comparison (recursive SQ, byte-equal values). The underlying comparers are exposed as public utilities (`tag_comparer`, `value_comparer`, `dataset_comparer` — fo-dicom `DicomItemComparer`); BulkDataUri buffers compare by tag only while unloaded.

## 7. Elements

### 7.1 Value model

```rust
pub enum ElementValue {
    Str { text: String, encodings: CharEncodings },     // set-by-user, not yet encoded
    Bytes(ByteBuffer),                                   // parsed or binary
    Dates { values: Box<[DicomDateTime]>, fmt: DateFmt },// parse cache for DA/DT/TM
}
```

- String elements encode lazily at serialization time (`to_bytes` applies target encodings + even-length padding with the VR's pad byte).
- Numeric strings (DS/IS) cache parsed values after first access (`OnceCell`).
- Binary elements expose typed reads honoring the current endianness (`ByteBuffer` Endian wrapper set at parse time).

### 7.2 Per-VR behavior notes

| VR group | Behaviors |
|---|---|
| Multi-string (AE/AS/CS/LO/SH/UC) | values split/join on `\`; per-value validation; UC skips VM count checks |
| PN | component model `Family^Given^Middle^Prefix^Suffix` per `=`-separated group (alphabetic/ideographic/phonetic); accessor `PersonName::family()` etc.; content-equality compares all components case-insensitively |
| DS | comma decimal tolerated on parse; values parsed as `f64`; serialization trims to ≤16 chars (demote to 10 significant digits when longer) |
| IS | parse as `i32`; a decimal fraction in the string is accepted (truncation like fo-dicom's lenient parse) |
| DA | accept 6 input formats (`yyyyMMdd`, `yyyyMM`, `yyyy`, `yyyy-MM-dd` variants per fo-dicom format table); ranges `min-max` |
| DT | 26 accepted formats incl. fractional seconds and `±hhmm` offsets; output includes fraction only when non-zero, trailing zeros trimmed |
| TM | 36 accepted formats; seconds ≤ 60; ranges |
| AT | each value = 2×u16 little-endian; decode via the interning index |
| OB/OW/OL/OD/OF/OV/UN | no VM validation; OB additionally reinterpretable as i16/u16/i32/u32/f32/f64 arrays |
| UI | parse → `Uid` (interned when known); → `TransferSyntax` via lookup |
| Fragments (encapsulated pixel data) | `DicomFragmentSequence { offset_table: Vec<u32>, fragments: Vec<ByteBuffer> }`; **fragment[0] is the offset table by construction convention** |

### 7.3 Conversion traits

```rust
pub trait FromElement: Sized { fn from_element(e: &DicomElement, idx: Option<usize>) -> Result<Self>; }
pub trait IntoElement { fn vr_hint() -> Option<Vr>; fn into_element(self, vr: Vr) -> DicomElement; }
```

## 8. Charsets (ENC-01..04)

### 8.1 Charset table (built-in mapping)

Default: ASCII. Registered terms (case/space tolerant: `ISO_IR 192`, `ISO-IR 192`, `ISO_IR192` all hit): `ISO_IR 100/101/109/110/126/127/138/144/148/149/166/192`, `ISO 2022 IR 6/13/87/100/101/109/110/126/127/138/144/148/149/159/166/58`, `GBK`, `GB18030`, `ISO 2022 GBK`. Unknown terms: warn (tracing) + ASCII fallback. `register_charset(term, encoding)` is the user extension point; a strict (no-replacement) encoder pair is kept for probing.

### 8.2 Decoding multi-charset strings (ENC-02)

Algorithm: scan bytes; split at `ESC (0x1B)` boundaries; map known ISO 2022 escape sequences (`ESC ( B`, `ESC $ ( D`, `ESC - A`, etc.) to the target encoding for the following segment; reset to the first charset at each CR/LF/TAB/FF for text VRs and at `^`/`=` for PN. `iso-2022-jp` delegates to the codec (escape bytes remain in-stream).

### 8.3 Encoding multi-charset strings (ENC-03)

Try the strict encoder of charset #1 for the whole value; on failure split at VR-appropriate delimiters and find, per segment, the first charset that encodes strictly, emitting the matching escape prefix before that segment; if a segment fails everywhere, encode with charset #1 + replacement character and warn.

## 9. Validation (VAL-01..03)

### 9.1 Switch layering

Global `VALIDATION_ENABLED` (atomic) AND per-dataset `validate` flag AND scoped guard (`validation::scope_off()` returning a RAII guard restoring the previous state). File parsing runs entirely with validation off (read-tolerant policy).

### 9.2 Per-VR string rules

| VR | Rule summary |
|---|---|
| AE | ≤16; not all blank; no `\` or control chars (incl. LF/FF/CR/ESC) |
| AS | `^\d{3}[DWMY]$` (empty allowed) |
| CS | ≤16; `[A-Z0-9_ ]*` |
| DA | ≤2 segments split on `-`; each `^\d{8}$`; month ≤12, day ≤31; trailing spaces tolerated |
| DS | ≤16; decimal number grammar incl. exponent |
| DT | `-` ranges (incl. negative-UTC ambiguity resolution; `-0000` forbidden); suffix `&ZZZZ`-equivalent offsets (positive hour ≤14, negative ≤12, minute ≤59); `YYYY[MM[DD[HH[MM[SS[.F{1-6}]]]]]]`; nonzero month/day; components in range |
| IS | `[+-]?\d+` parseable as i32 |
| LO/SH/ST/LT | length caps (64/16/1024/10240); no `\`/control chars (ESC allowed) |
| PN | ≤3 `=` groups; ≤64 chars per group; ≤5 `^` components; no control chars |
| TM | ≤2 segments; `HH[MM[SS[.F{1-6}]]]`; h≤23, m≤59, s≤60 |
| UI | ≤64; `[0-9.]*`; no leading zeros in components (including a bare first `0`); no empty components; trailing spaces tolerated |
| TZ | `^[+-]\d{4}$` (timezone offset helper) |

VM check (VAL-02): against dictionary entry min/max/step; exemptions: private tags, Other family, UC, UN, empty elements. Violations produce `Error::Validation { tag, vr, content, reason }`.

## 10. Errors (module-local view)

`Error::Data { kind, tag?, context }` covers: TagNotFound, TagAlreadyExists, UnexpectedType, ValueCount, PrivateTagResolution, Encoding(charset) failures. `Error::Validation { tag, vr, content }`. All carry the tag where available.

Note: fo-dicom's global static `DicomException.OnException` event (fired on every exception construction) has no direct port — the equivalent observability is the `tracing` error-level logging at every error construction site (INF-03), which subscribers can aggregate globally.

## 11. Test Requirements Checklist

- [ ] Tag ordering table tests: public vs private same/different creator, block-relative ordering.
- [ ] Tag parse/display round-trip for all 4 formats; masked tag parse/match/display.
- [ ] VR attribute table test-generated from a golden table (single source of truth with codegen).
- [ ] UID: parse interning, generation determinism (`2.25.` algorithm golden vectors), map-consistency, regenerate_all on nested SQ.
- [ ] Transfer syntax: lookup fallback construction, register/unregister, virtual syntax flags.
- [ ] Dictionary: lookup precedence (private→exact→masked→unknown), keyword reverse, external XML load, one-shot default replacement.
- [ ] Dataset: three-tier read semantics matrix; insert dispatch matrix (typed → VR); private block allocation incl. exhaustion; functional groups flatten golden cases.
- [ ] Elements: per-VR parse/format round-trips; DS/IS lenient cases; DT 26 formats (golden vector file); PN component access; AT interning.
- [ ] Charsets: decode with escape sequences (CJK sample vectors from fo-dicom tests); encode fallback chain; reset positions.
- [ ] Validation: one test per rule row above; switch layering (global × dataset × scope guard).
