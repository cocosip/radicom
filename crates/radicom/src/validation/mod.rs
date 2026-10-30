//! Value validation (VAL-01..03; docs/01-architecture.md §5).
//!
//! Layering: global switch × per-dataset flag × RAII scope guard. File
//! parsing runs with validation off (read-tolerant policy).

use crate::error::{Error, TagRef, ValidationFailure};
use crate::model::dicom_datetime::{parse_dt, parse_tm};
use crate::model::element::DicomElement;
use crate::model::vr::Vr;
use std::sync::atomic::{AtomicBool, Ordering};

static VALIDATION_ENABLED: AtomicBool = AtomicBool::new(true);

/// Global validation switch (default on).
pub fn set_validation_enabled(on: bool) {
    VALIDATION_ENABLED.store(on, Ordering::SeqCst);
}

pub fn validation_enabled() -> bool {
    VALIDATION_ENABLED.load(Ordering::SeqCst)
}

/// RAII scope guard: disables validation until dropped (VAL-03), restoring
/// the previous global state.
#[derive(Debug)]
pub struct ValidationScopeGuard {
    prev: bool,
}

impl ValidationScopeGuard {
    pub fn validation_off() -> Self {
        let prev = validation_enabled();
        set_validation_enabled(false);
        ValidationScopeGuard { prev }
    }
}

impl Drop for ValidationScopeGuard {
    fn drop(&mut self) {
        set_validation_enabled(self.prev);
    }
}

/// Whether validation should run given the dataset-level flag.
pub fn should_validate(dataset_flag: bool) -> bool {
    dataset_flag && validation_enabled()
}

fn fail(vr: Vr, content: &str, reason: impl Into<String>) -> String {
    // include the VR and a bounded prefix of the value: "not a valid DA value"
    // alone does not tell a caller which element or what was in it
    let shown: String = content.chars().take(32).collect();
    let ellipsis = if content.chars().count() > 32 {
        "…"
    } else {
        ""
    };
    format!("{vr:?} \"{shown}{ellipsis}\": {}", reason.into())
}

/// DA per PS3.3 C.3.1 / fo-dicom `ValidateDA`: one or two `YYYYMMDD`
/// components joined by a single `-`, trailing spaces tolerated, month ≤ 12,
/// day ≤ 31. Partial dates (`YYYY`, `YYYYMM`) and `.`/`/` separators are not
/// conformant, even though the tolerant date parser accepts them.
fn validate_da(content: &str) -> Result<(), String> {
    let mut parts = content.split('-');
    let first = parts.next().unwrap_or("");
    let second = parts.next();
    if parts.next().is_some() {
        return Err("more than one range separator".into());
    }
    for part in std::iter::once(first).chain(second) {
        let trimmed = part.trim_end_matches(' ');
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.len() != 8 || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return Err("expected YYYYMMDD".into());
        }
        let month: u32 = trimmed[4..6].parse().map_err(|_| "bad month")?;
        let day: u32 = trimmed[6..8].parse().map_err(|_| "bad day")?;
        if month > 12 {
            return Err("month exceeds 12".into());
        }
        if day > 31 {
            return Err("day exceeds 31".into());
        }
    }
    Ok(())
}

/// DS per PS3.3 C.3.4 / fo-dicom `DecimalStringRegex`:
/// `[+-]?((\d+(\.\d*)?)|(\.\d+))([eE][-+]?\d+)?`.
fn validate_ds(content: &str) -> Result<(), String> {
    let s = content.trim();
    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let before = count_digits(bytes, &mut i);
    let mut after = 0;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        after = count_digits(bytes, &mut i);
    }
    if before == 0 && after == 0 {
        return Err("no digits".into());
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        if count_digits(bytes, &mut i) == 0 {
            return Err("empty exponent".into());
        }
    }
    if i != bytes.len() {
        return Err("trailing characters".into());
    }
    Ok(())
}

fn count_digits(bytes: &[u8], i: &mut usize) -> usize {
    let start = *i;
    while *i < bytes.len() && bytes[*i].is_ascii_digit() {
        *i += 1;
    }
    *i - start
}

/// IS per PS3.3 C.3.5 / fo-dicom `ValidateIS`: optional sign, digits only,
/// at most 12 bytes, fits a 32-bit integer.
fn validate_is(content: &str) -> Result<(), String> {
    if content.len() > 12 {
        return Err("length exceeds 12".into());
    }
    let s = content.trim();
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
        return Err("expected [+-]?digits".into());
    }
    if s.parse::<i32>().is_err() {
        return Err("does not fit a 32-bit integer".into());
    }
    Ok(())
}

/// Validates one `\`-separated value per its VR rules (VAL-01).
pub fn validate_single(vr: Vr, content: &str) -> Result<(), String> {
    if content.is_empty() {
        return Ok(()); // empty values always validate
    }
    let has_control = |s: &str, allow_esc: bool| {
        s.chars().any(|c| {
            let code = c as u32;
            if allow_esc && code == 0x1B {
                return false;
            }
            code < 0x20
        })
    };
    let no_backslash = |s: &str| !s.contains('\\');
    match vr {
        Vr::Ae => {
            if content.chars().count() > 16 {
                return Err(fail(vr, content, "length exceeds 16"));
            }
            if content.trim().is_empty() {
                return Err(fail(vr, content, "all-blank value"));
            }
            if content.contains('\\') || has_control(content, false) {
                return Err(fail(vr, content, "backslash or control character"));
            }
            Ok(())
        }
        Vr::As => {
            let b = content.as_bytes();
            if b.len() == 3 && b.iter().all(|c| c.is_ascii_digit())
                || (b.len() == 4
                    && b[..3].iter().all(|c| c.is_ascii_digit())
                    && matches!(b[3], b'D' | b'W' | b'M' | b'Y'))
            {
                Ok(())
            } else {
                Err(fail(vr, content, "expected \\d{3}[DWMY]"))
            }
        }
        Vr::Cs => {
            if content.chars().count() > 16 {
                return Err(fail(vr, content, "length exceeds 16"));
            }
            if !content
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == ' ')
            {
                return Err(fail(vr, content, "characters outside [A-Z0-9_ ]"));
            }
            Ok(())
        }
        Vr::Da => validate_da(content).map_err(|reason| fail(vr, content, reason)),
        Vr::Ds => {
            if content.chars().count() > 16 {
                return Err(fail(vr, content, "length exceeds 16"));
            }
            validate_ds(content).map_err(|reason| fail(vr, content, reason))
        }
        Vr::Dt => {
            if crate::model::dicom_datetime::has_invalid_dt_offset(content) {
                return Err(fail(vr, content, "UTC offset hour out of range"));
            }
            parse_dt(content)
                .map(|_| ())
                .map_err(|_| fail(vr, content, "not a valid DT value or range"))
        }
        Vr::Is => validate_is(content).map_err(|reason| fail(vr, content, reason)),
        Vr::Lo => {
            if content.chars().count() > 64 {
                return Err(fail(vr, content, "length exceeds 64"));
            }
            if !no_backslash(content) || has_control(content, true) {
                return Err(fail(vr, content, "backslash or control character"));
            }
            Ok(())
        }
        Vr::Lt => {
            if content.chars().count() > 10240 {
                return Err(fail(vr, content, "length exceeds 10240"));
            }
            validate_text_controls(vr, content)
        }
        Vr::Pn => {
            let groups: Vec<&str> = content.split('=').collect();
            if groups.len() > 3 {
                return Err(fail(vr, content, "more than three name groups"));
            }
            for g in &groups {
                if g.chars().count() > 64 {
                    return Err(fail(vr, content, "group length exceeds 64"));
                }
                let comps: Vec<&str> = g.split('^').collect();
                if comps.len() > 5 {
                    return Err(fail(vr, content, "more than five components"));
                }
                if has_control(g, false) {
                    return Err(fail(vr, content, "control character"));
                }
            }
            Ok(())
        }
        Vr::Sh => {
            if content.chars().count() > 16 {
                return Err(fail(vr, content, "length exceeds 16"));
            }
            if !no_backslash(content) || has_control(content, true) {
                return Err(fail(vr, content, "backslash or control character"));
            }
            Ok(())
        }
        Vr::St => {
            if content.chars().count() > 1024 {
                return Err(fail(vr, content, "length exceeds 1024"));
            }
            validate_text_controls(vr, content)
        }
        Vr::Ut => validate_text_controls(vr, content),
        Vr::Tm => parse_tm(content)
            .map(|_| ())
            .map_err(|_| fail(vr, content, "not a valid TM value or range")),
        Vr::Ui => {
            if content.chars().count() > 64 {
                return Err(fail(vr, content, "length exceeds 64"));
            }
            if !crate::model::uid::Uid::is_valid(content) {
                return Err(fail(vr, content, "not a valid UID"));
            }
            Ok(())
        }
        // VRs without string validators: AE-family rules cover the rest
        _ => Ok(()),
    }
}

fn validate_text_controls(vr: Vr, content: &str) -> Result<(), String> {
    if content.chars().any(|c| {
        (c < '\u{20}' || c == '\u{7f}') && !matches!(c, '\r' | '\n' | '\t' | '\u{c}' | '\u{1b}')
    }) {
        return Err(fail(vr, content, "control character"));
    }
    Ok(())
}

/// Validates every value of an element's string form, respecting single-value VRs.
pub fn validate_string(vr: Vr, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    for part in value.split(|c| c == '\\' && vr.is_multi_value()) {
        validate_single(vr, part)?;
    }
    Ok(())
}

/// VM check against the dictionary entry (VAL-02): private tags, the Other
/// family (OB..OV), UC, UN and empty elements are exempt.
pub fn validate_vm(element: &DicomElement) -> Result<(), String> {
    if element.is_empty() {
        return Ok(());
    }
    let tag = element.tag();
    if tag.is_private() {
        return Ok(());
    }
    if matches!(
        element.vr(),
        Vr::Ob | Vr::Od | Vr::Of | Vr::Ol | Vr::Ov | Vr::Ow | Vr::Uc | Vr::Un
    ) {
        return Ok(());
    }
    let entry = crate::model::dictionary::dictionary().entry(tag);
    let count = element.count();
    if entry.vm.matches(count) {
        Ok(())
    } else {
        Err(format!(
            "value count {count} violates VM {} for {}",
            entry.vm, entry.name
        ))
    }
}

/// Full element validation: string rules (VAL-01) + VM rules (VAL-02).
/// Returns a [`ValidationFailure`]-shaped error.
/// Byte-backed strings are materialized so their values and VM are checked.
pub fn validate_element(element: &DicomElement) -> Result<(), Error> {
    if element.vr().is_string()
        && matches!(
            element.value(),
            crate::model::element::ElementValue::Bytes(_)
        )
    {
        let decoded = DicomElement::new_str(
            element.tag().clone(),
            element.vr(),
            element.get::<String>(None)?,
        );
        return validate_element(&decoded);
    }
    let tag_ref = TagRef(element.tag().group(), element.tag().element());
    if let crate::model::element::ElementValue::Str(s) = element.value() {
        validate_string(element.vr(), s).map_err(|reason| {
            Error::Validation(ValidationFailure {
                tag: tag_ref,
                vr: element.vr().code(),
                content: s.clone(),
                reason,
            })
        })?;
    }
    validate_vm(element).map_err(|reason| {
        Error::Validation(ValidationFailure {
            tag: tag_ref,
            vr: element.vr().code(),
            content: element.string_value().to_string(),
            reason,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tag::Tag;

    fn el(vr: Vr, text: &str) -> DicomElement {
        DicomElement::new_str(Tag::PATIENT_NAME, vr, text)
    }

    #[test]
    fn ae_rules() {
        assert!(validate_single(Vr::Ae, "SCPMED").is_ok());
        assert!(
            validate_single(Vr::Ae, "EXACTLY16CHARS").is_ok(),
            "exactly 16"
        );
        assert!(
            validate_single(Vr::Ae, "SEVENTEEN_CHARS_X").is_err(),
            "17 chars"
        );
        assert!(validate_single(Vr::Ae, "   ").is_err(), "all blank");
        assert!(validate_single(Vr::Ae, "A\\B").is_err(), "backslash");
        assert!(validate_single(Vr::Ae, "A\nB").is_err(), "control char");
    }

    #[test]
    fn as_cs_rules() {
        assert!(validate_single(Vr::As, "034Y").is_ok());
        assert!(validate_single(Vr::As, "034").is_ok());
        assert!(validate_single(Vr::As, "034X").is_err());
        assert!(validate_single(Vr::As, "34Y").is_err());

        assert!(validate_single(Vr::Cs, "ORIGINAL PRIMARY").is_ok());
        assert!(validate_single(Vr::Cs, "lower case").is_err());
        assert!(validate_single(Vr::Cs, "HAS-DASH").is_err());
    }

    #[test]
    fn date_time_rules_delegate_to_parsers() {
        assert!(validate_single(Vr::Da, "20240305").is_ok());
        assert!(validate_single(Vr::Da, "20241301").is_err());
        assert!(
            validate_single(Vr::Da, "20240101-20241231").is_ok(),
            "ranges valid"
        );
        assert!(validate_single(Vr::Tm, "103015").is_ok());
        assert!(validate_single(Vr::Tm, "240000").is_err());
        assert!(validate_single(Vr::Dt, "20240305103015+0530").is_ok());
        assert!(
            validate_single(Vr::Dt, "20240305103015-1500").is_err(),
            "offset hour 15"
        );
        // PS3.3 §6.2: negative offsets cap at 12h, positive at 14h.
        assert!(
            validate_single(Vr::Dt, "20240305103015-1300").is_err(),
            "negative offset hour 13"
        );
        assert!(
            validate_single(Vr::Dt, "20240305103015-1200").is_ok(),
            "negative offset hour 12"
        );
        assert!(
            validate_single(Vr::Dt, "20240305103015+1400").is_ok(),
            "positive offset hour 14"
        );
    }

    #[test]
    fn ds_is_rules() {
        assert!(validate_single(Vr::Ds, "-1.5E2").is_ok());
        assert!(
            validate_single(Vr::Ds, "1234567890123456").is_ok(),
            "16 chars"
        );
        assert!(
            validate_single(Vr::Ds, "12345678901234567").is_err(),
            "17 chars"
        );
        assert!(validate_single(Vr::Ds, "abc").is_err());
        assert!(validate_single(Vr::Is, "+42").is_ok());
        assert!(validate_single(Vr::Is, "4 2").is_err());
    }

    #[test]
    fn da_requires_full_yyyymmdd() {
        assert!(validate_single(Vr::Da, "20240305").is_ok());
        assert!(
            validate_single(Vr::Da, "20240305-20241231").is_ok(),
            "range"
        );
        assert!(
            validate_single(Vr::Da, "2024").is_err(),
            "partial date is not conformant"
        );
        assert!(
            validate_single(Vr::Da, "202403").is_err(),
            "partial date is not conformant"
        );
        assert!(
            validate_single(Vr::Da, "2024.03.05").is_err(),
            "dots are not conformant"
        );
        assert!(
            validate_single(Vr::Da, "2024/03/05").is_err(),
            "slashes are not conformant"
        );
    }

    #[test]
    fn is_rejects_a_fraction_and_overflow() {
        assert!(validate_single(Vr::Is, "-42").is_ok());
        assert!(
            validate_single(Vr::Is, "3.0").is_err(),
            "an IS has no fraction"
        );
        assert!(
            validate_single(Vr::Is, "999999999999").is_err(),
            "over 32-bit"
        );
    }

    #[test]
    fn ds_rejects_a_comma_decimal_separator() {
        assert!(validate_single(Vr::Ds, "1.5").is_ok());
        assert!(validate_single(Vr::Ds, "1,5").is_err(), "DICOM uses '.'");
        assert!(validate_single(Vr::Ds, ".").is_err(), "no digits");
        assert!(validate_single(Vr::Ds, "1e").is_err(), "empty exponent");
    }

    #[test]
    fn a_failure_names_the_vr_and_the_value() {
        let message = validate_single(Vr::Is, "3.0").unwrap_err();
        assert!(message.contains("Is"), "VR in message: {message}");
        assert!(message.contains("3.0"), "value in message: {message}");
    }

    #[test]
    fn review_single_text_accepts_formatting_and_checks_the_whole_value() {
        for (vr, limit) in [(Vr::St, 1024), (Vr::Lt, 10240)] {
            assert!(validate_single(vr, "A\\B\r\nC\tD\u{c}E\u{1b}").is_ok());
            assert!(validate_string(vr, "A\\B\r\nC").is_ok());
            for bad in ['\0', '\u{1}', '\u{7}', '\u{b}', '\u{1f}', '\u{7f}'] {
                assert!(
                    validate_single(vr, &format!("A{bad}B")).is_err(),
                    "{vr:?} {bad:?}"
                );
            }
            let oversized = format!("{}\\{}", "A".repeat(limit / 2), "B".repeat(limit / 2));
            assert!(validate_string(vr, &oversized).is_err());
        }
    }

    #[test]
    fn byte_backed_text_rejects_the_same_controls_as_strings() {
        for vr in [Vr::St, Vr::Lt, Vr::Ut] {
            let tag = Tag::new(0x0029, 0x1001);
            let string = DicomElement::new_str(tag.clone(), vr, "A\0B");
            let bytes = DicomElement::new_bytes(
                tag,
                vr,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(b"A\0B".to_vec())),
            );
            assert!(validate_element(&string).is_err(), "{vr:?} string");
            assert!(validate_element(&bytes).is_err(), "{vr:?} bytes");
        }
    }

    #[test]
    fn byte_backed_single_text_keeps_multiline_and_literal_backslashes() {
        for vr in [Vr::St, Vr::Lt, Vr::Ut] {
            let tag = Tag::new(0x0029, 0x1001);
            let text = "A\\B\r\nC";
            let string = DicomElement::new_str(tag.clone(), vr, text);
            let bytes = DicomElement::new_bytes(
                tag,
                vr,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(
                    text.as_bytes().to_vec(),
                )),
            );
            assert!(validate_element(&string).is_ok(), "{vr:?} string");
            assert!(validate_element(&bytes).is_ok(), "{vr:?} bytes");
        }
    }

    #[test]
    fn byte_backed_dates_use_decoded_values_for_rules_and_vm() {
        for (text, valid) in [
            ("20240305", true),
            ("x", false),
            ("20240305\\20240306", false),
        ] {
            let element = DicomElement::new_bytes(
                Tag::PATIENT_BIRTH_DATE,
                Vr::Da,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(
                    text.as_bytes().to_vec(),
                )),
            );
            assert_eq!(validate_element(&element).is_ok(), valid, "{text:?}");
        }
    }

    #[test]
    fn byte_backed_string_validation_propagates_read_errors() {
        let unreadable = crate::model::buffer::StreamWindow::new(
            Box::new(std::io::Cursor::new(Vec::<u8>::new())),
            1,
        );
        let element = DicomElement::new_bytes(
            Tag::PATIENT_BIRTH_DATE,
            Vr::Da,
            std::sync::Arc::new(unreadable),
        );
        assert!(matches!(validate_element(&element), Err(Error::Io(_))));
    }

    #[test]
    fn text_length_rules() {
        let lo64 = "L".repeat(64);
        let lo65 = "L".repeat(65);
        assert!(validate_single(Vr::Lo, &lo64).is_ok());
        assert!(validate_single(Vr::Lo, &lo65).is_err());
        assert!(validate_single(Vr::St, &"S".repeat(1024)).is_ok());
        assert!(validate_single(Vr::St, &"S".repeat(1025)).is_err());
        assert!(validate_single(Vr::Lt, &"L".repeat(10240)).is_ok());
        assert!(validate_single(Vr::Lt, &"L".repeat(10241)).is_err());
        // LO forbids backslash but allows escape (0x1B).
        assert!(validate_single(Vr::Lo, "a\\b").is_err());
        assert!(validate_single(Vr::Lo, "a\u{1B}b").is_ok());
    }

    #[test]
    fn pn_rules() {
        assert!(validate_single(Vr::Pn, "Doe^John^Quincy^Jr^MD").is_ok());
        assert!(validate_single(Vr::Pn, "A=B=C").is_ok());
        assert!(validate_single(Vr::Pn, "A=B=C=D").is_err(), "four groups");
        assert!(
            validate_single(Vr::Pn, "a^b^c^d^e^f").is_err(),
            "six components"
        );
        let long = format!("{}^x", "N".repeat(65));
        assert!(validate_single(Vr::Pn, &long).is_err(), "group over 64");
    }

    #[test]
    fn ui_rules() {
        assert!(validate_single(Vr::Ui, "1.2.840.10008.1.2").is_ok());
        assert!(validate_single(Vr::Ui, "01.2").is_err(), "leading zero");
        assert!(validate_single(Vr::Ui, "1..2").is_err(), "empty component");
        assert!(
            validate_single(Vr::Ui, &"1".repeat(65)).is_err(),
            "too long"
        );
    }

    #[test]
    fn multi_value_validates_each_part() {
        assert!(validate_string(Vr::Is, "1\\2\\3").is_ok());
        assert!(validate_string(Vr::Is, "1\\x\\3").is_err());
        assert!(validate_string(Vr::Lo, "").is_ok(), "empty always valid");
    }

    #[test]
    fn vm_check_with_exemptions() {
        // PatientID is LO VM 1
        let one = el(Vr::Lo, "ID1");
        assert!(validate_vm(&one).is_ok());
        let many = el(Vr::Lo, "ID1\\ID2");
        assert!(validate_vm(&many).is_err(), "LO VM 1 rejects two values");

        // private tag exempt
        let priv_tag = Tag::private(
            0x0029,
            0x1001,
            std::sync::Arc::new(crate::model::private_creator::PrivateCreator::new("X")),
        );
        let priv_el = DicomElement::new_str(priv_tag, Vr::Lo, "a\\b");
        assert!(validate_vm(&priv_el).is_ok(), "private tags exempt");

        // Other family exempt
        let ob = DicomElement::new_bytes(
            Tag::PIXEL_DATA,
            Vr::Ob,
            std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![1, 2, 3])),
        );
        assert!(validate_vm(&ob).is_ok());

        // empty exempt
        let empty = el(Vr::Lo, "");
        assert!(validate_vm(&empty).is_ok());
    }

    #[test]
    fn validation_element_and_switches() {
        // full validation produces a ValidationFailure with context
        let bad = el(Vr::Sh, "WAY_TOO_LONG_ACCESSION_NUMBER");
        let err = validate_element(&bad).unwrap_err();
        assert!(err.to_string().contains("does not validate"), "{err}");

        // global switch off → should_validate false
        let guard = ValidationScopeGuard::validation_off();
        assert!(!should_validate(true));
        drop(guard);
        assert!(should_validate(true));
        assert!(!should_validate(false), "dataset flag still gates");

        set_validation_enabled(false);
        assert!(!should_validate(true));
        set_validation_enabled(true);
        assert!(should_validate(true));
    }

    #[test]
    fn character_limits_count_characters_not_utf8_bytes() {
        // 40 CJK characters are 120 UTF-8 bytes but still one valid PN group
        // (PS3.5 §6.2 limits are in characters).
        let valid = "患".repeat(40);
        assert!(validate_string(Vr::Pn, &valid).is_ok());
        assert!(validate_string(Vr::Pn, &"患".repeat(65)).is_err());
        assert!(validate_string(Vr::Lo, &"检".repeat(64)).is_ok());
        assert!(validate_string(Vr::Lo, &"检".repeat(65)).is_err());
    }

    #[test]
    fn da_rules_cover_ranges_and_day_limits() {
        assert!(validate_single(Vr::Da, "20240305").is_ok());
        assert!(validate_single(Vr::Da, "20240305-20240307").is_ok());
        assert!(validate_single(Vr::Da, "20240305-").is_ok());
        assert!(validate_single(Vr::Da, "a-b-c").is_err());
        assert!(validate_single(Vr::Da, "20240332").is_err(), "day > 31");
        assert!(validate_single(Vr::Da, "2024030X").is_err());
    }

    #[test]
    fn is_ds_and_text_limits_cover_every_guard() {
        assert!(validate_single(Vr::Is, "1234567890123").is_err());
        assert!(validate_single(Vr::Ds, "12345678901234567").is_err());
        assert!(validate_single(Vr::Lo, "a\\b").is_err());
        assert!(validate_single(Vr::Pn, "a\u{7}b").is_err());
        assert!(validate_single(Vr::Sh, "abc\\def").is_err());
        assert!(validate_single(Vr::St, "abc\u{7}").is_err());
        // a VR without a string validator is accepted, and empties always are
        assert!(validate_single(Vr::Ur, "http://x").is_ok());
        assert!(validate_single(Vr::Lo, "").is_ok());
    }

    #[test]
    fn validate_element_reports_a_typed_failure() {
        let err = validate_element(&el(Vr::Lo, "a\\b")).unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }
}
