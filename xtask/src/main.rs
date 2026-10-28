//! xtask: workspace code generation tasks (`cargo xtask gen-dict`).

use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use quick_xml::events::Event;
use quick_xml::Reader;

fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("gen-dict") => gen_dict(),
        Some("regen-goldens") => regen_goldens(),
        _ => {
            eprintln!("usage: cargo xtask <gen-dict|regen-goldens>");
            std::process::exit(2);
        }
    }
}

fn root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // xtask/
    p
}

#[derive(Debug, Clone)]
struct RawTag {
    group: u16,
    element: u16,
    group_mask: u16,
    element_mask: u16,
    keyword: String,
    name: String,
    vr: String,
    vm: String,
    retired: bool,
}

/// Parses `x` nibbles into a 4-bit-per-nibble mask (digit → 0xF, x → 0x0).
fn parse_masked(hex: &str) -> Result<(u16, u16), String> {
    if hex.is_empty() || hex.chars().count() > 4 {
        return Err(format!("bad hex field: {hex}"));
    }
    let mut value = 0u16;
    let mut mask = 0u16;
    for c in hex.chars() {
        value <<= 4;
        mask <<= 4;
        let d = if c == 'x' || c == 'X' {
            0
        } else {
            c.to_digit(16).ok_or_else(|| format!("bad hex nibble '{c}'"))? as u16
        };
        value |= d;
        mask |= if c == 'x' || c == 'X' { 0x0 } else { 0xF };
    }
    Ok((value, mask))
}

fn vr_list(attr: &str) -> String {
    // fo-dicom separates multi-VR entries with "/" (OB/OW, US/SS/OW).
    // Normalize to '\' which the runtime dictionary parser accepts.
    attr.trim().split('/').map(str::trim).collect::<Vec<_>>().join("\\")
}

/// Parses the public dictionary XML (root `<dictionary>`, no creator).
fn parse_dictionary_xml(reader: impl Read) -> Result<Vec<RawTag>, String> {
    let mut reader = Reader::from_reader(std::io::BufReader::new(reader));
    reader.config_mut().trim_text(true);
    let mut tags: Vec<RawTag> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.name().as_ref() == b"tag" => {
                let mut group = None;
                let mut element = None;
                let mut keyword = String::new();
                let mut vr = String::new();
                let mut vm = String::new();
                let mut retired = false;
                for attr in e.attributes().flatten() {
                    let val = attr.decode_and_unescape_value(reader.decoder()).map_err(|e| e.to_string())?.into_owned();
                    match attr.key.as_ref() {
                        b"group" => group = Some(parse_masked(&val)?),
                        b"element" => element = Some(parse_masked(&val)?),
                        b"keyword" => keyword = val,
                        b"vr" => vr = vr_list(&val),
                        b"vm" => vm = val,
                        b"retired" => retired = val == "true",
                        _ => {}
                    }
                }
                let (g, gm) = group.ok_or("tag missing group")?;
                let (el, em) = element.ok_or("tag missing element")?;
                tags.push(RawTag {
                    group: g,
                    element: el,
                    group_mask: gm,
                    element_mask: em,
                    keyword,
                    name: String::new(), // filled from the Text event
                    vr,
                    vm,
                    retired,
                });
            }
            Ok(Event::Text(t)) => {
                if let Some(last) = tags.last_mut() {
                    if last.name.is_empty() {
                        last.name = t.unescape().map_err(|e| e.to_string())?.into_owned();
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(e.to_string()),
            _ => {}
        }
        buf.clear();
    }
    Ok(tags)
}

fn screaming_snake(keyword: &str) -> String {
    let chars: Vec<char> = keyword.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let next = chars.get(i + 1).copied();
            let boundary = match prev {
                None => false,
                Some(p) => {
                    p.is_ascii_lowercase()
                        || p.is_ascii_digit()
                        || (p.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase()))
                }
            };
            if boundary && !out.is_empty() {
                out.push('_');
            }
            out.push(c);
        } else if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    out
}

fn emit_dict_source(public: &[RawTag], private: &[(String, RawTag)]) -> String {
    /// Escapes a value for inclusion in a Rust string literal.
    fn lit(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }

    fn entry_row(t: &RawTag, creator: Option<&str>) -> String {
        let creator_lit = creator.map(|c| format!("\"{}\"", lit(c))).unwrap_or_else(|| "\"\"".into());
        format!(
            "        RawDictEntry {{ group: 0x{:04X}, element: 0x{:04X}, group_mask: 0x{:04X}, \
             element_mask: 0x{:04X}, creator: {creator_lit}, keyword: \"{}\", name: \"{}\", \
             vr: \"{}\", vm: \"{}\", retired: {} }},\n",
            t.group,
            t.element,
            t.group_mask,
            t.element_mask,
            lit(&t.keyword),
            lit(&t.name),
            lit(&t.vr),
            lit(&t.vm),
            t.retired
        )
    }

    let mut exact = public.iter().filter(|t| t.group_mask == 0xFFFF && t.element_mask == 0xFFFF).collect::<Vec<_>>();
    exact.sort_by_key(|t| (t.group, t.element));
    let masked = public.iter().filter(|t| t.group_mask != 0xFFFF || t.element_mask != 0xFFFF).collect::<Vec<_>>();

    let mut s = String::new();
    let _ = writeln!(s, "//! GENERATED by `cargo xtask gen-dict` — do not edit by hand.");
    let _ = writeln!(s, "//! Source data: fo-dicom dictionary XML (MS-PL), see dictionaries/ATTRIBUTION.md.");
    let _ = writeln!(s, "//!");
    let _ = writeln!(s, "//! VR values are normalized to '\\'-separated standard codes and VM keeps the");
    let _ = writeln!(s, "//! dictionary spelling (\"1\", \"1-n\", \"2-2n\"); the runtime dictionary parses them.");
    let _ = writeln!(s, "#![allow(clippy::all)]");
    s.push('\n');
    let _ = writeln!(s, "/// One data dictionary row (public rows carry `creator: \"\"`).");
    let _ = writeln!(s, "#[derive(Debug, Clone, Copy)]");
    let _ = writeln!(s, "pub struct RawDictEntry {{");
    let _ = writeln!(s, "    pub group: u16,");
    let _ = writeln!(s, "    pub element: u16,");
    let _ = writeln!(s, "    /// Nibble masks: 0xF per hex digit, 0x0 per 'x'.");
    let _ = writeln!(s, "    pub group_mask: u16,");
    let _ = writeln!(s, "    pub element_mask: u16,");
    let _ = writeln!(s, "    pub creator: &'static str,");
    let _ = writeln!(s, "    pub keyword: &'static str,");
    let _ = writeln!(s, "    pub name: &'static str,");
    let _ = writeln!(s, "    pub vr: &'static str,");
    let _ = writeln!(s, "    pub vm: &'static str,");
    let _ = writeln!(s, "    pub retired: bool,");
    let _ = writeln!(s, "}}");
    s.push('\n');
    let _ = writeln!(s, "/// Public data dictionary, exact entries, sorted by (group, element).");
    let _ = writeln!(s, "pub static DICTIONARY: &[RawDictEntry] = &[");
    for t in &exact {
        s.push_str(&entry_row(t, None));
    }
    let _ = writeln!(s, "];");
    s.push('\n');
    let _ = writeln!(s, "/// Public data dictionary, masked (repeating-group) entries.");
    let _ = writeln!(s, "pub static MASKED_DICTIONARY: &[RawDictEntry] = &[");
    for t in &masked {
        s.push_str(&entry_row(t, None));
    }
    let _ = writeln!(s, "];");
    s.push('\n');
    let _ = writeln!(s, "/// Private dictionary rows grouped by their creator string.");
    let _ = writeln!(s, "pub static PRIVATE_DICTIONARY: &[RawDictEntry] = &[");
    for (creator, t) in private {
        s.push_str(&entry_row(t, Some(creator)));
    }
    let _ = writeln!(s, "];");
    s
}

fn emit_uids_source(uids: &[UsvRow]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "//! GENERATED by `cargo xtask gen-dict` — do not edit by hand.");
    let _ = writeln!(s, "//! Source data: UID registry extracted from fo-dicom DicomUIDGenerated.cs (MS-PL).");
    let _ = writeln!(s, "//!");
    let _ = writeln!(s, "//! type codes: 0=TransferSyntax 1=SopClass 2=MetaSopClass 3=ServiceClass");
    let _ = writeln!(s, "//! 4=SopInstance 5=ApplicationContextName 6=ApplicationHostingModel");
    let _ = writeln!(s, "//! 7=CodingScheme 8=FrameOfReference 9=Ldap 10=MappingResource 11=ContextGroupName");
    let _ = writeln!(s, "#![allow(clippy::all)]");
    s.push('\n');
    let _ = writeln!(s, "/// One registered UID row; `type_code` maps to `UidType` per the header.");
    let _ = writeln!(s, "#[derive(Debug, Clone, Copy)]");
    let _ = writeln!(s, "pub struct RawUid {{");
    let _ = writeln!(s, "    pub uid: &'static str,");
    let _ = writeln!(s, "    pub keyword: &'static str,");
    let _ = writeln!(s, "    pub name: &'static str,");
    let _ = writeln!(s, "    pub type_code: u8,");
    let _ = writeln!(s, "    pub retired: bool,");
    let _ = writeln!(s, "}}");
    s.push('\n');
    let _ = writeln!(s, "pub static UIDS: &[RawUid] = &[");
    for r in uids {
        let name = r.name.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = writeln!(
            s,
            "        RawUid {{ uid: \"{}\", keyword: \"{}\", name: \"{}\", type_code: {}, retired: {} }},",
            r.uid, r.keyword, name, r.type_code, r.retired
        );
    }
    let _ = writeln!(s, "];");
    s.push('\n');
    let _ = writeln!(s, "/// Well-known UID string constants (SCREAMING_SNAKE of the source");
    let _ = writeln!(s, "/// keyword; `_N` suffixes disambiguate keyword collisions).");
    let _ = writeln!(s, "pub mod consts {{");
    let mut used: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for r in uids {
        let base = screaming_snake(&r.keyword);
        let count = used.entry(base.clone()).and_modify(|c| *c += 1).or_insert(1);
        let name = if *count == 1 { base.clone() } else { format!("{base}_{count}") };
        let _ = writeln!(s, "    pub const {name}: &str = \"{}\";", r.uid);
    }
    let _ = writeln!(s, "}}");
    s
}

fn emit_tag_consts(public: &[RawTag]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "//! GENERATED by `cargo xtask gen-dict` — do not edit by hand.");
    let _ = writeln!(s, "//! Source data: fo-dicom dictionary XML (MS-PL), see radicom-dict/dictionaries.");
    let _ = writeln!(s, "#![allow(clippy::all)]");
    s.push('\n');
    let _ = writeln!(s, "use crate::model::tag::Tag;");
    s.push('\n');
    let _ = writeln!(s, "impl Tag {{");
    let mut used: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut exact: Vec<&RawTag> = public.iter().filter(|t| t.group_mask == 0xFFFF && t.element_mask == 0xFFFF).collect();
    exact.sort_by_key(|t| (t.group, t.element));
    for t in &exact {
        let base = screaming_snake(&t.keyword);
        let count = used.entry(base.clone()).and_modify(|c| *c += 1).or_insert(1);
        let name = if *count == 1 { base.clone() } else { format!("{base}_{}", count) };
        let _ = writeln!(s, "    /// {} ({})", t.name.replace('"', "\\\""), t.vr);
        let _ = writeln!(s, "    pub const {name}: Tag = Tag::new(0x{:04X}, 0x{:04X});", t.group, t.element);
    }
    let _ = writeln!(s, "}}");
    s
}

struct UsvRow {
    uid: String,
    keyword: String,
    name: String,
    type_code: u8,
    retired: bool,
}

fn read_usv(path: &Path) -> Result<Vec<UsvRow>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 5 {
            return Err(format!("{}: bad row '{}…'", path.display(), &line[..line.len().min(40)]));
        }
        rows.push(UsvRow {
            uid: parts[0].into(),
            keyword: parts[1].into(),
            name: parts[2].replace("\\\\", "\\"),
            type_code: parts[3].parse().map_err(|_| format!("bad type code {}", parts[3]))?,
            retired: parts[4] == "true",
        });
    }
    Ok(rows)
}

fn read_gz_xml(path: &Path) -> Result<Vec<RawTag>, String> {
    let f = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_dictionary_xml(GzDecoder::new(f))
}

fn gen_dict() {
    let root = root();
    let dict_dir = root.join("crates/radicom-dict/dictionaries");
    let public = read_gz_xml(&dict_dir.join("DICOMDictionary.xml.gz")).expect("public dictionary");

    // Re-parse private XML capturing the creator per <dictionary> block:
    // parse_dictionary_xml returns one creator; the private file nests
    // multiple dictionaries, so walk again block-wise by splitting on the
    // creator attribute via a full parse that tracks context.
    let mut private_rows: Vec<(String, RawTag)> = Vec::new();
    {
        let f = fs::File::open(dict_dir.join("PrivateDictionary.xml.gz")).expect("private dictionary");
        let mut reader = Reader::from_reader(std::io::BufReader::new(GzDecoder::new(f)));
        reader.config_mut().trim_text(true);
        let mut creator = String::new();
        let mut buf = Vec::new();
        let mut in_tag = false;
        let mut cur: Option<RawTag> = None;
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                    b"dictionary" => {
                        creator.clear();
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"creator" {
                                creator = attr.decode_and_unescape_value(reader.decoder()).expect("creator").into_owned();
                            }
                        }
                    }
                    b"tag" => {
                        in_tag = !e.is_empty();
                        let mut g = (0u16, 0xFFFFu16);
                        let mut el = (0u16, 0xFFFFu16);
                        let mut keyword = String::new();
                        let mut vr = String::new();
                        let mut vm = String::new();
                        let mut retired = false;
                        for attr in e.attributes().flatten() {
                            let val = attr.decode_and_unescape_value(reader.decoder()).expect("attr").into_owned();
                            match attr.key.as_ref() {
                                b"group" => g = parse_masked(&val).expect("group"),
                                b"element" => el = parse_masked(&val).expect("element"),
                                b"keyword" => keyword = val,
                                b"vr" => vr = vr_list(&val),
                                b"vm" => vm = val,
                                b"retired" => retired = val == "true",
                                _ => {}
                            }
                        }
                        cur = Some(RawTag { group: g.0, element: el.0, group_mask: g.1, element_mask: el.1, keyword, name: String::new(), vr, vm, retired });
                    }
                    _ => {}
                },
                Ok(Event::Text(t)) => {
                    if in_tag {
                        if let Some(c) = cur.as_mut() {
                            c.name = t.unescape().expect("text").into_owned();
                        }
                    }
                }
                Ok(Event::End(e)) => {
                    if e.name().as_ref() == b"tag" {
                        if let Some(c) = cur.take() {
                            private_rows.push((creator.clone(), c));
                        }
                        in_tag = false;
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => panic!("private XML: {e}"),
                _ => {}
            }
            buf.clear();
        }
    }

    let uids = read_usv(&dict_dir.join("uids.tsv")).expect("uids.tsv");

    // Emit radicom-dict generated sources.
    let gen_dir = root.join("crates/radicom-dict/src/generated");
    fs::create_dir_all(&gen_dir).expect("create generated dir");
    fs::write(gen_dir.join("dict.rs"), emit_dict_source(&public, &private_rows)).expect("write dict.rs");
    fs::write(gen_dir.join("uids.rs"), emit_uids_source(&uids)).expect("write uids.rs");

    // Emit radicom typed tag constants.
    let core_dir = root.join("crates/radicom/src/generated");
    fs::create_dir_all(&core_dir).expect("create core generated dir");
    fs::write(core_dir.join("tags.rs"), emit_tag_consts(&public)).expect("write tags.rs");

    eprintln!(
        "gen-dict: {} public tags ({} masked), {} private rows, {} UIDs",
        public.len(),
        public.iter().filter(|t| t.group_mask != 0xFFFF || t.element_mask != 0xFFFF).count(),
        private_rows.len(),
        uids.len()
    );
}

/// Regenerates the golden snapshot files under `tests-data/goldens/`
/// (docs/02-development-guide.md §5 §4). Sources are deterministic corpus files, so the
/// outputs are reproducible; the integration tests in `radicom`/`radicom-json`
/// fail when they drift.
fn regen_goldens() {
    let root = root();
    let data_dir = root.join("tests-data");
    let golden_dir = data_dir.join("goldens");
    fs::create_dir_all(&golden_dir).expect("create goldens dir");

    // (source relative to tests-data, golden filename) — dump snapshots.
    let dump_cases = [
        ("explicit-le/GH178.dcm", "GH178.dump.txt"),
        ("media/DICOMDIR", "dicomdir.dump.txt"),
    ];
    for (source, golden) in dump_cases {
        let bytes = fs::read(data_dir.join(source)).unwrap_or_else(|e| panic!("read {source}: {e}"));
        let file = radicom::io::file::DicomFile::from_bytes(&bytes, &Default::default())
            .unwrap_or_else(|e| panic!("parse {source}: {e}"));
        fs::write(golden_dir.join(golden), file.dataset.to_dump_string())
            .unwrap_or_else(|e| panic!("write {golden}: {e}"));
    }

    // JSON snapshot of the same rich reference dataset.
    let bytes = fs::read(data_dir.join("explicit-le/GH178.dcm")).expect("read GH178 for json");
    let file = radicom::io::file::DicomFile::from_bytes(&bytes, &Default::default()).expect("parse GH178");
    let json = radicom_json::dataset_to_json(&file.dataset, &Default::default()).expect("encode json");
    fs::write(golden_dir.join("GH178.json"), json).expect("write GH178.json");

    // PDU byte-stream goldens (docs/02-development-guide.md §5 §3 "Network … hex fixtures").
    // Keep the fixtures below in sync with crates/radicom-network/tests/goldens.rs.
    let pdu_cases = [
        ("associate_rq.hex", fixture_associate_rq()),
        ("pdata_cecho.hex", fixture_pdata_cecho()),
        ("associate_rq_user_info.hex", fixture_associate_rq_user_info()),
        ("cmd_n_get_rq.hex", fixture_command_n_get_rq()),
        ("cmd_cget_reverse_store_rq.hex", fixture_command_cget_reverse_store_rq()),
        ("cmd_cfind_pending_rsp.hex", fixture_command_cfind_pending_rsp()),
    ];
    for (name, bytes) in pdu_cases {
        fs::write(golden_dir.join(name), hex_lines(&bytes)).unwrap_or_else(|e| panic!("write {name}: {e}"));
    }

    eprintln!("regen-goldens: wrote dump + json + pdu snapshots to {}", golden_dir.display());
}

/// 16 hex bytes per line, uppercase, trailing newline. Whitespace is ignored by
/// the comparison, so only the byte sequence matters.
fn hex_lines(bytes: &[u8]) -> String {
    let mut s = String::new();
    for chunk in bytes.chunks(16) {
        for (i, b) in chunk.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            let _ = write!(s, "{b:02X}");
        }
        s.push('\n');
    }
    s
}

fn fixture_associate_rq() -> Vec<u8> {
    use radicom_network::pdu::encode;
    use radicom_network::{Associate, PresentationContext, Pdu};
    let mut associate = Associate::new("RADICOM_SCP", "RADICOM_SCU");
    associate.presentation_contexts.push(PresentationContext::new(
        1,
        "1.2.840.10008.1.1",
        vec!["1.2.840.10008.1.2.1".to_string()],
    ));
    encode(&Pdu::AssociateRq(associate)).expect("encode")
}

fn fixture_pdata_cecho() -> Vec<u8> {
    use radicom_network::message::encode_command;
    use radicom_network::pdu::encode;
    use radicom_network::{Command, CommandField, PData, Pdu, Pdv};
    let mut command = Command::request(CommandField::CEchoRq, 1);
    command.affected_sop_class_uid = Some("1.2.840.10008.1.1".to_string());
    let command_bytes = encode_command(&command.to_dataset()).expect("encode command");
    let pdv = Pdv { context_id: 1, is_command: true, is_last: true, data: command_bytes.into() };
    encode(&Pdu::PData(PData { pdvs: vec![pdv] })).expect("encode")
}

fn fixture_associate_rq_user_info() -> Vec<u8> {
    use radicom_network::pdu::encode;
    use radicom_network::{Associate, Pdu, PresentationContext};
    let mut associate = Associate::new("RADICOM_SCP", "RADICOM_SCU");
    associate.presentation_contexts.push(PresentationContext::new(
        1,
        "1.2.840.10008.1.1",
        vec!["1.2.840.10008.1.2.1".to_string()],
    ));
    associate.user_information = radicom_network::association::build_user_information(
        16384,
        "1.2.826.0.1.3680043.10.1330.2",
        "RADICOM_0_1",
        Vec::new(),
    );
    encode(&Pdu::AssociateRq(associate)).expect("encode")
}

fn fixture_command_n_get_rq() -> Vec<u8> {
    use radicom_network::nservice::NServiceRequest;
    use radicom_network::server::command_bytes;
    use radicom_network::CommandField;
    let mut request = NServiceRequest::new(CommandField::NGetRq, 7);
    request.sop_class_uid = Some("1.2.840.10008.1.20.1".to_string());
    request.sop_instance_uid = Some("1.2.3.4.5".to_string());
    command_bytes(&request.to_command()).expect("encode command")
}

fn fixture_command_cget_reverse_store_rq() -> Vec<u8> {
    use radicom_network::server::command_bytes;
    use radicom_network::{Command, CommandField};
    let mut command = Command::request(CommandField::CStoreRq, 1);
    command.affected_sop_class_uid = Some("1.2.840.10008.1.2.4.50".to_string());
    command.affected_sop_instance_uid = Some("1.2.3.4.5.6".to_string());
    command.no_dataset = false;
    command_bytes(&command).expect("encode command")
}

fn fixture_command_cfind_pending_rsp() -> Vec<u8> {
    use radicom::model::dataset::Dataset;
    use radicom_network::cfind::{CFindRequest, CFindResponse};
    use radicom_network::server::command_bytes;
    let request = CFindRequest::study_query(1);
    let response = CFindResponse::pending(&request, Dataset::new());
    command_bytes(&response.to_command()).expect("encode command")
}
