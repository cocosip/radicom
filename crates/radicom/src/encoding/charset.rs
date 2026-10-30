//! Character set handling (ENC-01..04; docs/01-architecture.md §4).
//!
//! Maps Specific Character Set terms to encodings, handles ISO 2022
//! escape-sequence switching on decode, and per-segment fallback encoding
//! with escape injection on encode.

use crate::error::{DataError, Error};
use encoding_rs::Encoding;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::RwLock;

/// The built-in term → encoding table (docs/01-architecture.md §4). ISO 2022 extended
/// terms decode their base encoding; escape switching is handled by
/// [`decode_multi_charset`].
static BUILTINS: &[(&str, &str)] = &[
    ("ISO_IR 6", "ASCII"),
    ("ISO_IR 13", "Shift_JIS"),
    ("ISO_IR 100", "iso-8859-1"),
    ("ISO_IR 101", "iso-8859-2"),
    ("ISO_IR 109", "iso-8859-3"),
    ("ISO_IR 110", "iso-8859-4"),
    ("ISO_IR 126", "iso-8859-7"),
    ("ISO_IR 127", "iso-8859-6"),
    ("ISO_IR 138", "iso-8859-8"),
    ("ISO_IR 144", "iso-8859-5"),
    ("ISO_IR 148", "iso-8859-9"),
    ("ISO_IR 149", "EUC-KR"),
    ("ISO_IR 166", "windows-874"),
    ("ISO_IR 192", "UTF-8"),
    ("GBK", "GBK"),
    ("GB18030", "gb18030"),
    ("ISO 2022 IR 6", "ASCII"),
    ("ISO 2022 IR 13", "Shift_JIS"),
    ("ISO 2022 IR 58", "gb2312"),
    ("ISO 2022 IR 87", "ISO-2022-JP"),
    ("ISO 2022 IR 100", "iso-8859-1"),
    ("ISO 2022 IR 101", "iso-8859-2"),
    ("ISO 2022 IR 109", "iso-8859-3"),
    ("ISO 2022 IR 110", "iso-8859-4"),
    ("ISO 2022 IR 126", "iso-8859-7"),
    ("ISO 2022 IR 127", "iso-8859-6"),
    ("ISO 2022 IR 138", "iso-8859-8"),
    ("ISO 2022 IR 144", "iso-8859-5"),
    ("ISO 2022 IR 148", "iso-8859-9"),
    ("ISO 2022 IR 149", "EUC-KR"),
    ("ISO 2022 IR 159", "ISO-2022-JP"),
    ("ISO 2022 IR 166", "windows-874"),
    ("ISO 2022 GBK", "GBK"),
];

static REGISTRY: Lazy<RwLock<HashMap<String, &'static Encoding>>> = Lazy::new(|| {
    let mut m = HashMap::new();
    for (term, enc) in BUILTINS {
        if let Some(e) = Encoding::for_label(enc.as_bytes()) {
            m.insert(normalize_term(term), e);
        }
    }
    RwLock::new(m)
});

/// Term matching is case/space/punctuation tolerant: `iso-ir 192`,
/// `ISO_IR192` and `ISO IR 192` all hit (ENC-01 tolerance).
fn normalize_term(term: &str) -> String {
    term.chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Calls `f` with the normalized term, using a small stack buffer for the
/// common short ASCII case so a charset switch does not allocate (review §2.13).
fn with_normalized_term<R>(term: &str, f: impl FnOnce(&str) -> R) -> R {
    let mut buffer = [0u8; 32];
    let mut len = 0usize;
    for c in term.chars() {
        if matches!(c, ' ' | '-' | '_') {
            continue;
        }
        if !c.is_ascii() || len == buffer.len() {
            return f(&normalize_term(term));
        }
        buffer[len] = c.to_ascii_uppercase() as u8;
        len += 1;
    }
    f(std::str::from_utf8(&buffer[..len]).expect("normalized ASCII"))
}

/// Registers or remaps a character set term (ENC-04 extension point).
pub fn register_charset(term: &str, encoding_name: &str) -> Result<(), Error> {
    let enc = Encoding::for_label(encoding_name.as_bytes()).ok_or_else(|| {
        Error::Data(DataError::Other {
            message: format!("unknown encoding \"{encoding_name}\""),
        })
    })?;
    // leak the name once: registry values must be 'static
    let leaked: &'static str = Box::leak(encoding_name.to_string().into_boxed_str());
    let enc = Encoding::for_label(leaked.as_bytes()).unwrap_or(enc);
    REGISTRY
        .write()
        .expect("registry lock")
        .insert(normalize_term(term), enc);
    Ok(())
}

/// Resolves a Specific Character Set term to an encoding; unknown terms
/// fall back to windows-1252 (encoding_rs's ASCII superset — callers warn,
/// spec §4).
///
/// ponytail: every call takes the registry read lock. Parsing only pays this on
/// a charset switch (the builder caches), so the cost is confined to the write
/// path; a thread-local term→encoding cache would remove it if that ever shows up
/// in a profile (review §2.13). The normalized term no longer allocates for short
/// ASCII labels.
pub fn charset_encoding(term: &str) -> &'static Encoding {
    ensure_registry();
    with_normalized_term(term, |key| {
        REGISTRY
            .read()
            .expect("registry lock")
            .get(key)
            .copied()
            .unwrap_or(encoding_rs::WINDOWS_1252)
    })
}

/// True when the term is a known charset (used to decide whether to warn).
pub fn is_known_charset(term: &str) -> bool {
    ensure_registry();
    with_normalized_term(term, |key| {
        REGISTRY.read().expect("registry lock").contains_key(key)
    })
}

fn ensure_registry() {
    Lazy::force(&REGISTRY);
}

/// Reverse lookup: encoding → DICOM term (for writing SpecificCharacterSet,
/// ENC-04). Returns the single-byte/Unicode term, not the ISO 2022 variant.
///
/// `encoding_rs` maps both the `ASCII` and `windows-1252` labels to
/// `WINDOWS_1252`, so the bare-ASCII row is skipped: reporting `ISO_IR 6` for
/// a decoder that accepts the full Latin-1 range would misdescribe the bytes
/// (CORE-24). `ISO_IR 100` is the correct term for that encoding.
pub fn charset_for_encoding(encoding: &'static Encoding) -> Option<&'static str> {
    BUILTINS
        .iter()
        .filter(|(term, enc)| !term.starts_with("ISO 2022") && *enc != "ASCII")
        .find(|(_, enc)| Encoding::for_label(enc.as_bytes()) == Some(encoding))
        .map(|(term, _)| *term)
}

// -- decoding (ENC-02) ------------------------------------------------------

/// Decodes a possibly multi-charset string: splits at ESC (0x1B) boundaries
/// and maps known ISO 2022 sequences to their encoding; resets to the first
/// charset at CR/LF/TAB/FF for text VRs and at `^`/`=` for PN.
///
/// `first` is the encoding from SpecificCharacterSet; `pn` selects the PN
/// reset positions.
pub fn decode_multi_charset(bytes: &[u8], first: &'static Encoding, pn: bool) -> String {
    // Pure 7-bit ASCII decodes to itself in every DICOM character set — ASCII is
    // invariant everywhere these tables reach — so skip the segment scan and the
    // per-segment decoder calls. The 0x1B check
    // matters: an ISO 2022 escape designator is itself 7-bit, so a run full of
    // them must still go through the stateful path.
    if bytes.iter().all(|b| *b < 0x80 && *b != 0x1B) {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    // ISO 2022 Japanese: the multibyte (JIS X 0208) designators only decode
    // correctly when their ESC prefixes reach the decoder (encoding_rs's
    // ISO-2022-JP is stateful). Stripping the escapes and decoding the bare JIS
    // bytes would yield ASCII garbage, so hand the whole run to the stateful
    // decoder, which handles the ESC $\"B\"/$@/$(B/(J/(I switches itself.
    if first.name() == "ISO-2022-JP" {
        let (text, _, _) = first.decode(bytes);
        return text.into_owned();
    }
    fn flush(
        out: &mut String,
        bytes: &[u8],
        range: std::ops::Range<usize>,
        enc: &'static Encoding,
        korean: bool,
        designator: Option<&[u8]>,
    ) {
        if range.start >= range.end {
            return;
        }
        let slice = &bytes[range];
        // ISO-2022-JP is a stateful decoder: it only decodes JIS X 0208 bytes
        // correctly when the ESC designator that selected them is part of the
        // input. Replay it, or the bare bytes decode as ASCII garbage
        // (CORE-26).
        if enc.name() == "ISO-2022-JP" {
            let mut input = Vec::with_capacity(slice.len() + 4);
            if let Some(designator) = designator {
                input.extend_from_slice(designator);
            }
            input.extend_from_slice(slice);
            out.push_str(&enc.decode(&input).0);
            return;
        }
        // ISO 2022 Korean: 7-bit double bytes (0x21-0x7E) after `ESC $ ) C`
        // must be shifted to their EUC-KR 8-bit form. Bytes already in 8-bit
        // form (mixed encodings) are passed through unchanged.
        if korean && !slice.iter().any(|b| *b >= 0x80) {
            let shifted: Vec<u8> = slice.iter().map(|b| b + 0x80).collect();
            out.push_str(&enc.decode(&shifted).0);
        } else {
            out.push_str(&enc.decode(slice).0);
        }
    }
    let mut out = String::with_capacity(bytes.len());
    let mut current = first;
    let mut korean = false;
    let mut designator: Option<&[u8]> = None;
    let mut segment_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1B {
            if let Some(len) = escape_sequence_len(bytes, i) {
                flush(
                    &mut out,
                    bytes,
                    segment_start..i,
                    current,
                    korean,
                    designator,
                );
                korean = bytes[i..i + len] == [0x1B, 0x24, 0x29, 0x43];
                designator = Some(&bytes[i..i + len]);
                if let Some(enc) = encoding_for_escape(&bytes[i..i + len]) {
                    current = enc;
                }
                segment_start = i + len;
                i += len;
                continue;
            }
        }
        // JIS graphic characters are byte pairs; their second byte may be a PN delimiter.
        if current.name() == "ISO-2022-JP"
            && (0x21..=0x7e).contains(&b)
            && bytes
                .get(i + 1)
                .is_some_and(|trail| (0x21..=0x7e).contains(trail))
        {
            i += 2;
            continue;
        }
        let reset = (pn && (b == b'^' || b == b'=')) || matches!(b, b'\r' | b'\n' | b'\t' | 0x0C);
        if reset {
            flush(
                &mut out,
                bytes,
                segment_start..i,
                current,
                korean,
                designator,
            );
            segment_start = i;
            current = first;
            korean = false;
            designator = None;
        }
        i += 1;
    }
    flush(
        &mut out,
        bytes,
        segment_start..bytes.len(),
        current,
        korean,
        designator,
    );
    out
}

/// Length of a recognizable ISO 2022 escape sequence at `bytes[i]` (0x1B),
/// or `None` when the bytes do not form a known designator.
fn escape_sequence_len(bytes: &[u8], i: usize) -> Option<usize> {
    let rest = &bytes[i..];
    match rest.first()? {
        0x1B => {}
        _ => return None,
    }
    match rest.get(1)? {
        // 4-byte multibyte designators: ESC $ ( X, ESC $ ) X
        b'$' => match rest.get(2) {
            Some(b'(') | Some(b')') => {
                if rest.len() >= 4 {
                    Some(4)
                } else {
                    None
                }
            }
            _ => {
                if rest.len() >= 3 {
                    Some(3)
                } else {
                    None
                }
            }
        },
        // 3-byte designators: ESC ( X, ESC ) X, ESC - X
        b'(' | b')' | b'-' => {
            if rest.len() >= 3 {
                Some(3)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Maps an ISO 2022 designator to its encoding (spec §4 mapping table).
fn encoding_for_escape(seq: &[u8]) -> Option<&'static Encoding> {
    if seq.first() != Some(&0x1B) {
        return None;
    }
    let designator: String = match seq.len() {
        3 => format!("{}{}", seq[1] as char, seq[2] as char),
        4 => format!("{}{}{}", seq[1] as char, seq[2] as char, seq[3] as char),
        _ => return None,
    };
    let label = match designator.as_str() {
        "(B" => "ASCII",
        "(J" | "(I" => "Shift_JIS", // JIS X 0201 romaji / katakana
        "-A" => "iso-8859-1",       // Latin 1 (ISO 2022 IR 100)
        "-B" => "iso-8859-2",       // Latin 2 (ISO 2022 IR 101)
        "-C" => "iso-8859-3",       // Latin 3 (ISO 2022 IR 109)
        "-D" => "iso-8859-4",       // Latin 4 (ISO 2022 IR 110)
        "-F" => "iso-8859-7",       // Greek (ISO 2022 IR 126)
        "-G" => "iso-8859-6",       // Arabic (ISO 2022 IR 127)
        "-H" => "iso-8859-8",       // Hebrew (ISO 2022 IR 138)
        "-L" => "iso-8859-5",       // Cyrillic (ISO 2022 IR 144)
        "-M" => "iso-8859-9",       // Latin 5 (ISO 2022 IR 148)
        "-T" => "windows-874",      // Thai (ISO 2022 IR 166)
        "$B" | "$@" | "$(D" => "ISO-2022-JP", // JIS X 0208 / 0212
        "$)A" => "gb2312",          // GB 2312 (ISO 2022 IR 58)
        "$)C" => "EUC-KR",          // KS X 1001 (ISO 2022 IR 149)
        _ => return None,
    };
    Encoding::for_label(label.as_bytes())
}

// -- encoding (ENC-03) -------------------------------------------------------

/// Encodes a string into `first` with per-segment fallback to `alternates`
/// when the strict encoder fails; successful alternates get their ISO 2022
/// escape prefix injected. When everything fails, encodes with `first` and
/// replacement characters (documented last resort).
pub fn encode_multi_charset(
    text: &str,
    first: &'static Encoding,
    alternates: &[&'static Encoding],
    pn: bool,
) -> Vec<u8> {
    encode_multi_charset_with(text, first, alternates, pn, None)
}

/// The ISO 2022 designation that a declared charset term requires before its
/// (8-bit) characters (PS3.5 §6.1.2.5.3). `None` for the default repertoire
/// and for stateful ISO-2022-JP, whose encoder emits its own escapes.
pub fn iso2022_designator_for_term(term: &str) -> Option<&'static [u8]> {
    match normalize_term(term).as_str() {
        "ISO2022IR58" => Some(b"\x1B$)A"),
        "ISO2022IR149" => Some(b"\x1B$)C"),
        "ISO2022IR13" => Some(b"\x1B(J"),
        "ISO2022IR100" => Some(b"\x1B-A"),
        "ISO2022IR101" => Some(b"\x1B-B"),
        "ISO2022IR109" => Some(b"\x1B-C"),
        "ISO2022IR110" => Some(b"\x1B-D"),
        "ISO2022IR126" => Some(b"\x1B-F"),
        "ISO2022IR127" => Some(b"\x1B-G"),
        "ISO2022IR138" => Some(b"\x1B-H"),
        "ISO2022IR144" => Some(b"\x1B-L"),
        "ISO2022IR148" => Some(b"\x1B-M"),
        "ISO2022IR166" => Some(b"\x1B-T"),
        _ => None,
    }
}

/// Encodes like [`encode_multi_charset`], additionally emitting
/// `first_designator` before every non-ASCII run.
///
/// PS3.5 §6.1.2.5.3 requires the designation to be (re)established after each
/// PN/text delimiter, so this splits at the same reset positions as the
/// fallback path and writes the ASCII prefix of a segment before the
/// designator. `first_designator` comes from the declared
/// SpecificCharacterSet term ([`iso2022_designator_for_term`]); passing `None`
/// is exactly [`encode_multi_charset`].
pub fn encode_multi_charset_with(
    text: &str,
    first: &'static Encoding,
    alternates: &[&'static Encoding],
    pn: bool,
    first_designator: Option<&'static [u8]>,
) -> Vec<u8> {
    let Some(designator) = first_designator else {
        return encode_multi_charset_plain(text, first, alternates, pn);
    };
    if text.is_ascii() {
        return text.as_bytes().to_vec();
    }

    let reset_points: Vec<usize> = if pn {
        text.char_indices()
            .filter(|(_, c)| *c == '^' || *c == '=')
            .map(|(i, _)| i)
            .collect()
    } else {
        text.char_indices()
            .filter(|(_, c)| matches!(c, '\r' | '\n' | '\t' | '\u{0C}'))
            .map(|(i, _)| i)
            .collect()
    };
    let mut segments: Vec<&str> = Vec::new();
    let mut start = 0usize;
    for p in reset_points {
        segments.push(&text[start..p]);
        start = p;
    }
    segments.push(&text[start..]);

    let mut bytes = Vec::with_capacity(text.len() + 8);
    // a G0 designation (JIS X 0201) must be undone before ASCII runs
    let g0_designation = designator.starts_with(b"\x1B(");
    let mut g0_active = false;
    for segment in segments {
        match segment.find(|c: char| !c.is_ascii()) {
            None => {
                if g0_active {
                    bytes.extend_from_slice(b"\x1B(B");
                    g0_active = false;
                }
                bytes.extend_from_slice(segment.as_bytes());
            }
            Some(split) => {
                bytes.extend_from_slice(&segment.as_bytes()[..split]);
                if g0_active {
                    bytes.extend_from_slice(b"\x1B(B");
                }
                let rest = &segment[split..];
                if segment_can_encode(first, rest) {
                    bytes.extend_from_slice(designator);
                    g0_active = g0_designation;
                    let (encoded, _, _) = first.encode(rest);
                    bytes.extend_from_slice(&encoded);
                } else if let Some(enc) = alternates
                    .iter()
                    .copied()
                    .find(|enc| segment_can_encode(enc, rest))
                {
                    if let Some(esc) = escape_for_encoding(enc) {
                        bytes.extend_from_slice(esc);
                        g0_active = esc.starts_with(b"\x1B(");
                    }
                    let (encoded, _, _) = enc.encode(rest);
                    bytes.extend_from_slice(&encoded);
                } else {
                    bytes.extend_from_slice(designator);
                    g0_active = g0_designation;
                    let (encoded, _, _) = first.encode(rest);
                    bytes.extend_from_slice(&encoded);
                }
            }
        }
    }
    bytes
}

/// The original per-segment fallback encoder (no first-charset designation).
fn encode_multi_charset_plain(
    text: &str,
    first: &'static Encoding,
    alternates: &[&'static Encoding],
    pn: bool,
) -> Vec<u8> {
    // Try the whole string with the first encoding; `encode` reports
    // replacement-character usage via the error flag.
    let (encoded, _, had_errors) = first.encode(text);
    if !had_errors {
        return encoded.into_owned();
    }

    // Per-segment fallback: split at VR-appropriate delimiters, pick the
    // first encoding that encodes the segment strictly, inject its escape.
    let reset_points: Vec<usize> = if pn {
        text.char_indices()
            .filter(|(_, c)| *c == '^' || *c == '=')
            .map(|(i, _)| i)
            .collect()
    } else {
        text.char_indices()
            .filter(|(_, c)| matches!(c, '\r' | '\n' | '\t' | '\u{0C}'))
            .map(|(i, _)| i)
            .collect()
    };

    let mut segments: Vec<&str> = Vec::new();
    let mut start = 0usize;
    for p in reset_points {
        segments.push(&text[start..p]);
        start = p;
    }
    segments.push(&text[start..]);

    let candidates: Vec<&'static Encoding> = std::iter::once(first)
        .chain(alternates.iter().copied())
        .collect();
    let mut bytes = Vec::with_capacity(text.len());
    let mut active = first;
    for (i, segment) in segments.iter().enumerate() {
        // reset to the first encoding at reset positions (the reset char
        // itself starts the next segment)
        if i > 0 {
            active = first;
        }
        let mut chosen: Option<&'static Encoding> = None;
        for enc in &candidates {
            if segment_can_encode(enc, segment) {
                chosen = Some(enc);
                break;
            }
        }
        match chosen {
            Some(enc) => {
                if enc != active {
                    if let Some(esc) = escape_for_encoding(enc) {
                        bytes.extend_from_slice(esc);
                    }
                    active = enc;
                }
                let (encoded, _, had_errors) = enc.encode(segment);
                bytes.extend_from_slice(&encoded);
                let _ = had_errors;
            }
            None => {
                // last resort: first encoding with replacement characters
                let (encoded, _, _) = first.encode(segment);
                bytes.extend_from_slice(&encoded);
            }
        }
    }
    bytes
}

fn segment_can_encode(enc: &'static Encoding, segment: &str) -> bool {
    let (_, _, had_errors) = enc.encode(segment);
    !had_errors
}

/// The ISO 2022 escape prefix switching into `enc` (spec §4 table).
fn escape_for_encoding(enc: &'static Encoding) -> Option<&'static [u8]> {
    match enc.name() {
        "windows-1252" => Some(b"\x1B-A"),
        "ISO-8859-2" => Some(b"\x1B-B"),
        "ISO-8859-3" => Some(b"\x1B-C"),
        "ISO-8859-4" => Some(b"\x1B-D"),
        "ISO-8859-7" => Some(b"\x1B-F"),
        "ISO-8859-6" => Some(b"\x1B-G"),
        "ISO-8859-8" => Some(b"\x1B-H"),
        "ISO-8859-5" => Some(b"\x1B-L"),
        "ISO-8859-9" => Some(b"\x1B-M"),
        "windows-874" => Some(b"\x1B-T"),
        "Shift_JIS" => Some(b"\x1B(J"),
        // ISO-2022-JP emits its own switches, including any leading ASCII run.
        "ISO-2022-JP" => None,
        "GBK" => Some(b"\x1B$)A"),
        "EUC-KR" => Some(b"\x1B$)C"),
        // CORE-26: no `UTF-8`/`ASCII` arm - neither is a valid ISO 2022
        // target, and `encoding_rs` has no encoding named "ASCII" (the label
        // resolves to windows-1252, handled above), so those arms were dead.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_text_is_returned_without_the_segment_scan() {
        // the ASCII fast path must agree with the general path for plain
        // text, including PN delimiters and resets.
        for text in [
            "",
            "SMITH^JOHN=DOE",
            "line one\r\nline two",
            "0123456789 ^=",
        ] {
            for encoding in ["ISO_IR 100", "ISO_IR 192", "ISO 2022 IR 87", "GB18030"] {
                let enc = charset_encoding(encoding);
                assert_eq!(
                    decode_multi_charset(text.as_bytes(), enc, true),
                    text,
                    "{encoding} plain ASCII"
                );
            }
        }
    }

    #[test]
    fn decode_iso2022jp_keeps_escape_switching() {
        // JIS X 0208 "あ" as ESC $ B 0x24 0x22, then ESC ( B back to ASCII.
        let base = charset_encoding("ISO 2022 IR 87");
        let bytes = [0x1Bu8, 0x24, 0x42, 0x24, 0x22, 0x1B, 0x28, 0x42];
        let out = decode_multi_charset(&bytes, base, false);
        assert_eq!(
            out, "あ",
            "JIS X 0208 must decode to the Japanese char, not ASCII"
        );
    }

    #[test]
    fn iso2022jp_round_trips_mixed_text() {
        let jp = charset_encoding("ISO 2022 IR 87");
        let text = "patient 山田";
        let bytes = encode_multi_charset(text, jp, &[], false);
        let back = decode_multi_charset(&bytes, jp, false);
        assert_eq!(back, text);
    }

    #[test]
    fn decode_iso2022kr_shifts_double_bytes_to_euc() {
        // "가" = KS X 1001 0x30 0x21 (7-bit), wrapped in ESC $ ) C ... ESC ( B
        let base = charset_encoding("ISO 2022 IR 149");
        let bytes = [0x1Bu8, 0x24, 0x29, 0x43, 0x30, 0x21, 0x1B, 0x28, 0x42];
        let out = decode_multi_charset(&bytes, base, false);
        assert_eq!(
            out, "가",
            "Korean ISO 2022 must decode, not fall back to ASCII"
        );
    }

    #[test]
    fn decode_iso2022kr_mixed_with_ascii() {
        let base = charset_encoding("ISO 2022 IR 149");
        // "A" + ESC $ ) C 가(0x30 0x21) + ESC ( B + "B"
        let bytes = [
            0x41, 0x1B, 0x24, 0x29, 0x43, 0x30, 0x21, 0x1B, 0x28, 0x42, 0x42,
        ];
        let out = decode_multi_charset(&bytes, base, false);
        assert_eq!(out, "A가B");
    }

    #[test]
    fn plain_euc_kr_without_designator_is_unaffected() {
        // native EUC-KR 8-bit bytes, no ESC → the Korean branch does not trigger
        let base = charset_encoding("ISO_IR 149");
        let bytes = [0xB0u8, 0xA1];
        let out = decode_multi_charset(&bytes, base, false);
        assert_eq!(out, "가");
    }

    #[test]
    fn term_matching_is_tolerant() {
        for term in ["ISO_IR 192", "iso-ir 192", "ISOIR192", "ISO_IR192"] {
            assert!(is_known_charset(term), "{term}");
            assert_eq!(charset_encoding(term).name(), "UTF-8");
        }
        assert_eq!(charset_encoding("ISO_IR 149").name(), "EUC-KR");
        assert_eq!(charset_encoding("GBK").name(), "GBK");
        assert_eq!(
            charset_encoding(" Totally Unknown ").name(),
            "windows-1252",
            "unknown falls back"
        );
        assert!(!is_known_charset(" Totally Unknown "));
    }

    #[test]
    fn register_custom_charset() {
        register_charset("CUSTOM_X", "windows-1252").unwrap();
        assert_eq!(charset_encoding("custom-x").name(), "windows-1252");
        assert!(register_charset("BAD", "not-a-real-encoding").is_err());
    }

    #[test]
    fn reverse_lookup() {
        let utf8 = charset_encoding("ISO_IR 192");
        assert_eq!(charset_for_encoding(utf8), Some("ISO_IR 192"));
        let cyrillic = Encoding::for_label(b"iso-8859-5").unwrap();
        assert_eq!(charset_for_encoding(cyrillic), Some("ISO_IR 144"));
        // CORE-24: windows-1252 is Latin-1 on the wire, not bare ASCII
        assert_eq!(
            charset_for_encoding(encoding_rs::WINDOWS_1252),
            Some("ISO_IR 100")
        );
        assert_eq!(charset_for_encoding(encoding_rs::BIG5), None);
    }

    #[test]
    fn iso_ir_144_decodes_cyrillic_as_iso_8859_5() {
        // bytes from fo-dicom's chrRuss.dcm; fo-dicom asserts the same text
        let enc = charset_encoding("ISO_IR 144");
        let bytes = [0xBBu8, 0xEE, 0xDA, 0x63, 0x65, 0xDC, 0xD1, 0x79, 0x70, 0xD3];
        assert_eq!(decode_multi_charset(&bytes, enc, true), "Люкceмбypг");
    }

    #[test]
    fn iso_ir_127_decodes_arabic_as_iso_8859_6() {
        // bytes from fo-dicom's chrArab.dcm
        let enc = charset_encoding("ISO_IR 127");
        let bytes = [
            0xE2u8, 0xC8, 0xC7, 0xE6, 0xEA, 0x5E, 0xE4, 0xE6, 0xD2, 0xC7, 0xD1, 0x20,
        ];
        assert_eq!(
            decode_multi_charset(&bytes, enc, true).trim_end(),
            "قباني^لنزار"
        );
    }

    #[test]
    fn mixed_8bit_korean_and_delimiters_decode_per_fragment() {
        // chrI2.dcm shape: ESC $ ) C 洪 ^ ESC $ ) C 홍; the 8-bit bytes must
        // pass through and the delimiter must reset to the base encoding
        let base = charset_encoding("ISO_IR 6");
        let bytes = [
            0x1Bu8, 0x24, 0x29, 0x43, 0xFB, 0xF3, 0x5E, 0x1B, 0x24, 0x29, 0x43, 0xC8, 0xAB,
        ];
        assert_eq!(decode_multi_charset(&bytes, base, true), "洪^홍");
    }

    #[test]
    fn korean_designator_with_8bit_byte_never_overflows() {
        let base = charset_encoding("EUC-KR");
        let out = decode_multi_charset(&[0x1B, 0x24, 0x29, 0x43, 0xFB], base, false);
        assert!(!out.is_empty(), "deterministic output, no panic");
    }

    #[test]
    fn iso2022_single_byte_designators_switch_encoding() {
        // ESC - L designates ISO-8859-5 (Cyrillic) for the following bytes
        let base = charset_encoding("ISO_IR 6");
        let out = decode_multi_charset(b"\x1B-L\xBB\xEE\xDA", base, false);
        assert_eq!(out, "Люк");
        // ESC - C is Latin-3, not a Chinese designator
        assert_eq!(
            encoding_for_escape(b"\x1B-C").map(|e| e.name()),
            Some("ISO-8859-3")
        );
        assert_eq!(
            encoding_for_escape(b"\x1B$)A").map(|e| e.name()),
            Some("GBK")
        );
    }

    #[test]
    fn plain_decode() {
        let enc = charset_encoding("ISO_IR 192");
        assert_eq!(
            decode_multi_charset("héllo".as_bytes(), enc, false),
            "héllo"
        );
    }

    #[test]
    fn decode_with_escape_switching() {
        // Latin-1 base, then ESC - A switches (treated as latin-1 segment),
        // then CR resets to base
        let bytes = b"AB\x1B-A\xC3\xA9\rCD";
        let base = charset_encoding("ISO_IR 100");
        let out = decode_multi_charset(bytes, base, false);
        assert!(out.contains("AB"), "{out}");
        // reset at CR: CD decodes with the base encoding again
        assert!(out.contains("CD"), "{out}");
    }

    #[test]
    fn decode_pn_resets_at_caret() {
        // PN: 'Doe^José' where José arrives after an escape switch; the
        // caret resets to the base charset
        let bytes = b"Doe^\x1B-AJos\xC3\xA9";
        let base = charset_encoding("ISO_IR 100");
        let out = decode_multi_charset(bytes, base, true);
        assert!(out.starts_with("Doe^"), "{out}");
    }

    #[test]
    fn encode_strict_first_then_fallback() {
        let utf8 = charset_encoding("ISO_IR 192");
        // pure ASCII: no escapes needed
        let out = encode_multi_charset("plain", utf8, &[], false);
        assert_eq!(out, b"plain");

        // Windows-1252 base with a Chinese segment falls back to GBK
        let latin = charset_encoding("ISO_IR 100");
        let gbk = charset_encoding("GBK");
        let out = encode_multi_charset("ok:参数", latin, &[gbk], false);
        assert!(
            out.starts_with(b"\x1B"),
            "escape injected before the GBK segment"
        );
        // round-trips: the escape switch makes the decode recover the text
        let decoded = decode_multi_charset(&out, latin, false);
        assert_eq!(decoded, "ok:参数");
    }

    #[test]
    fn encode_replacement_fallback() {
        let ascii = charset_encoding("ISO_IR 6");
        // no alternates: 'é' becomes a replacement character, never panics
        let out = encode_multi_charset("café", ascii, &[], false);
        let (text, _, _) = encoding_rs::WINDOWS_1252.decode(&out);
        assert!(text.contains("caf"));
    }

    #[test]
    fn iso2022_designator_is_resolved_from_the_declared_term() {
        assert_eq!(
            iso2022_designator_for_term("ISO 2022 IR 58"),
            Some(&b"\x1B$)A"[..])
        );
        assert_eq!(
            iso2022_designator_for_term("ISO 2022 IR 149"),
            Some(&b"\x1B$)C"[..])
        );
        // the default repertoire and stateful JIS need no injected designation
        assert!(iso2022_designator_for_term("ISO_IR 6").is_none());
        assert!(iso2022_designator_for_term("ISO 2022 IR 87").is_none());
    }

    #[test]
    fn encode_emits_iso2022_designation_after_delimiters() {
        // PS3.5 §6.1.2.5.3: the designation must follow every PN delimiter.
        let gbk = charset_encoding("ISO 2022 IR 58");
        let encoded = encode_multi_charset_with(
            "Zhang^XiaoDong=张^小东",
            gbk,
            &[],
            true,
            iso2022_designator_for_term("ISO 2022 IR 58"),
        );
        assert_eq!(
            encoded,
            b"Zhang^XiaoDong=\x1B$)A\xD5\xC5^\x1B$)A\xD0\xA1\xB6\xAB".to_vec()
        );
        // ASCII-only values never gain an escape
        let plain = encode_multi_charset_with(
            "Doe^John",
            gbk,
            &[],
            true,
            iso2022_designator_for_term("ISO 2022 IR 58"),
        );
        assert_eq!(plain, b"Doe^John".to_vec());
    }

    #[test]
    fn escape_sequence_parsing_rejects_truncated_and_unknown_designators() {
        assert_eq!(escape_sequence_len(b"\x1B$", 0), None);
        assert_eq!(escape_sequence_len(b"\x1B$(", 0), None);
        assert_eq!(escape_sequence_len(b"\x1B(", 0), None);
        assert_eq!(escape_sequence_len(b"\x1BXY", 0), None);
        assert_eq!(escape_sequence_len(b"\x1B$)A", 0), Some(4));
        assert_eq!(escape_sequence_len(b"\x1B$B", 0), Some(3));
        assert_eq!(escape_sequence_len(b"\x1B-A", 0), Some(3));

        assert_eq!(encoding_for_escape(b"XY"), None);
        assert_eq!(encoding_for_escape(b"\x1B"), None);
        assert_eq!(encoding_for_escape(b"\x1BZZ"), None);
        assert_eq!(encoding_for_escape(b"\x1B$)Z"), None);
        assert_eq!(encoding_for_escape(b"\x1B-Z"), None);
        assert!(encoding_for_escape(b"\x1B(B").is_some());
        assert!(encoding_for_escape(b"\x1B$)C").is_some());
    }

    #[test]
    fn decode_switches_and_resets_between_segments() {
        let base = charset_encoding("ISO_IR 6");
        // ESC - A switches to Latin 1; CR resets to the base encoding
        let out = decode_multi_charset(b"a\x1B-A\xE9\rb\xE9", base, false);
        assert!(out.starts_with("aé\rb"), "{out}");
        // an unknown designator does not abort the decode
        let out = decode_multi_charset(b"a\x1BZYb", base, false);
        assert!(out.starts_with('a'), "{out}");
    }

    #[test]
    fn malformed_jis_lead_does_not_consume_a_text_reset() {
        let ascii = charset_encoding("ISO 2022 IR 6");
        for control in [b'\r', b'\n', b'\t', 0x0c] {
            let bytes = [b"\x1b$B$".as_slice(), &[control], b"ASCII"].concat();
            assert_eq!(
                decode_multi_charset(&bytes, ascii, true),
                format!("\u{fffd}{}ASCII", control as char)
            );
        }
    }

    #[test]
    fn review_jis_person_name_delimiter_bytes_round_trip() {
        let first = charset_encoding("ISO 2022 IR 6");
        let jis = charset_encoding("ISO 2022 IR 87");
        for text in ["ま^太郎", "十^ま", "ま=ま^太郎", "ま\\太郎"] {
            let bytes = encode_multi_charset(text, first, &[jis], true);
            assert_eq!(
                decode_multi_charset(&bytes, first, true),
                text,
                "{bytes:02x?}"
            );
        }
    }

    #[test]
    fn review_jis_person_name_survives_file_write_and_parse() {
        use crate::io::file::{DicomFile, ReadOptions};
        use crate::io::writer::WriteOptions;
        use crate::model::{dataset::Dataset, tag::Tag, vr::Vr};

        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3");
        dataset.insert_str(
            Tag::SPECIFIC_CHARACTER_SET,
            Vr::Cs,
            "ISO 2022 IR 6\\ISO 2022 IR 87",
        );
        dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "ま^太郎");
        let bytes = DicomFile::from_dataset(dataset)
            .to_bytes(&WriteOptions::default())
            .unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.dataset.get_str(Tag::PATIENT_NAME).unwrap(), "ま^太郎");
    }

    #[test]
    fn a_designated_jis_segment_is_decoded_with_its_escape() {
        // CORE-26: the base charset is not Japanese, so the JIS X 0208 bytes
        // only decode when the designator reaches the stateful decoder.
        let base = charset_encoding("ISO_IR 100");
        let bytes = [0x1Bu8, 0x24, 0x42, 0x24, 0x22];
        assert_eq!(decode_multi_charset(&bytes, base, false), "あ");
    }

    #[test]
    fn designator_encoder_covers_fallback_and_last_resort() {
        let gbk = charset_encoding("ISO 2022 IR 58");
        let designator = iso2022_designator_for_term("ISO 2022 IR 58");
        // first encoding cannot encode → the alternate's escape is used
        let ascii = charset_encoding("ISO_IR 6");
        let greek = charset_encoding("ISO_IR 126");
        let out = encode_multi_charset_with("Ω", ascii, &[greek], false, Some(b"\x1B(B"));
        assert!(
            out.windows(2).any(|w| w == b"\x1B-"),
            "Greek escape present: {out:02X?}"
        );
        // nothing can encode → replacement characters, still designated
        let out = encode_multi_charset_with("张\u{1F600}", gbk, &[], false, designator);
        assert!(out.starts_with(b"\x1B$)A"), "designator still written");
    }

    #[test]
    fn fallback_encoder_covers_reset_and_failure_branches() {
        let latin = charset_encoding("ISO_IR 100");
        let gbk = charset_encoding("GBK");
        // a '^' restarts the segment at the first encoding
        let out = encode_multi_charset("Doe^参数", latin, &[gbk], true);
        assert!(out.contains(&0x1B), "GBK escape injected after the reset");
        // no alternate can encode: replacement characters, no panic
        let out = encode_multi_charset("x\u{1F600}y", latin, &[], false);
        assert!(out.starts_with(b"x"));
        // the escape table covers Korean, and rejects non-ISO-2022 encodings
        assert_eq!(
            escape_for_encoding(charset_encoding("ISO 2022 IR 149")),
            Some(&b"\x1B$)C"[..])
        );
        // UTF-8 is not a valid ISO 2022 target (dead arm removed, CORE-26)
        assert_eq!(escape_for_encoding(charset_encoding("ISO_IR 192")), None);
        assert_eq!(escape_for_encoding(encoding_rs::BIG5), None);
    }
}
