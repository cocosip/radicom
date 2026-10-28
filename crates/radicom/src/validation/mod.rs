//! Value validation (VAL-01..03; docs/04-data-model.md §9).
//!
//! Layering: global switch × per-dataset flag × RAII scope guard. File
//! parsing runs with validation off (read-tolerant policy).

use crate::error::{Error, TagRef, ValidationFailure};
use crate::model::element::DicomElement;
use crate::model::person_name::{parse_ds, parse_is};
use crate::model::dicom_datetime::{parse_da, parse_dt, parse_tm};
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

fn fail(_vr: Vr, _content: &str, reason: impl Into<String>) -> String {
    reason.into()
}

/// Validates one `\`-separated value per its VR rules (VAL-01).
pub fn validate_single(vr: Vr, content: &str) -> Result<(), String> {
    let empty_ok = true;
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
            if content.len() > 16 {
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
            if b.len() == 3
                && b.iter().all(|c| c.is_ascii_digit())
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
            if content.len() > 16 {
                return Err(fail(vr, content, "length exceeds 16"));
            }
            if !content.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == ' ') {
                return Err(fail(vr, content, "characters outside [A-Z0-9_ ]"));
            }
            Ok(())
        }
        Vr::Da => parse_da(content)
            .map(|_| ())
            .map_err(|_| fail(vr, content, "not a valid DA value or range")),
        Vr::Ds => {
            if content.len() > 16 {
                return Err(fail(vr, content, "length exceeds 16"));
            }
            parse_ds(content).map(|_| ()).map_err(|_| fail(vr, content, "not a decimal string"))
        }
        Vr::Dt => {
            if crate::model::dicom_datetime::has_invalid_dt_offset(content) {
                return Err(fail(vr, content, "UTC offset hour out of range"));
            }
            parse_dt(content)
                .map(|_| ())
                .map_err(|_| fail(vr, content, "not a valid DT value or range"))
        }
        Vr::Is => parse_is(content)
            .map(|_| ())
            .map_err(|_| fail(vr, content, "not an integer string")),
        Vr::Lo => {
            if content.len() > 64 {
                return Err(fail(vr, content, "length exceeds 64"));
            }
            if !no_backslash(content) || has_control(content, true) {
                return Err(fail(vr, content, "backslash or control character"));
            }
            Ok(())
        }
        Vr::Lt => {
            if content.len() > 10240 {
                return Err(fail(vr, content, "length exceeds 10240"));
            }
            if !no_backslash(content) || has_control(content, true) {
                return Err(fail(vr, content, "backslash or control character"));
            }
            Ok(())
        }
        Vr::Pn => {
            let groups: Vec<&str> = content.split('=').collect();
            if groups.len() > 3 {
                return Err(fail(vr, content, "more than three name groups"));
            }
            for g in &groups {
                if g.len() > 64 {
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
            if content.len() > 16 {
                return Err(fail(vr, content, "length exceeds 16"));
            }
            if !no_backslash(content) || has_control(content, true) {
                return Err(fail(vr, content, "backslash or control character"));
            }
            Ok(())
        }
        Vr::St => {
            if content.len() > 1024 {
                return Err(fail(vr, content, "length exceeds 1024"));
            }
            if !no_backslash(content) || has_control(content, true) {
                return Err(fail(vr, content, "backslash or control character"));
            }
            Ok(())
        }
        Vr::Tm => parse_tm(content)
            .map(|_| ())
            .map_err(|_| fail(vr, content, "not a valid TM value or range")),
        Vr::Ui => {
            if content.len() > 64 {
                return Err(fail(vr, content, "length exceeds 64"));
            }
            if !crate::model::uid::Uid::is_valid(content) {
                return Err(fail(vr, content, "not a valid UID"));
            }
            Ok(())
        }
        // VRs without string validators: AE-family rules cover the rest
        _ => {
            let _ = empty_ok;
            Ok(())
        }
    }
}

/// Validates every `\`-separated value of an element's string form.
pub fn validate_string(vr: Vr, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    for part in value.split('\\') {
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
    if matches!(element.vr(), Vr::Ob | Vr::Od | Vr::Of | Vr::Ol | Vr::Ov | Vr::Ow | Vr::Uc | Vr::Un) {
        return Ok(());
    }
    let entry = crate::model::dictionary::default_dictionary().entry(tag);
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
pub fn validate_element(element: &DicomElement) -> Result<(), Error> {
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
        DicomElement::new_str(Tag::new(0x0010, 0x0010), vr, text)
    }

    #[test]
    fn ae_rules() {
        assert!(validate_single(Vr::Ae, "SCPMED").is_ok());
        assert!(validate_single(Vr::Ae, "EXACTLY16CHARS").is_ok(), "exactly 16");
        assert!(validate_single(Vr::Ae, "SEVENTEEN_CHARS_X").is_err(), "17 chars");
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
        assert!(validate_single(Vr::Da, "20240101-20241231").is_ok(), "ranges valid");
        assert!(validate_single(Vr::Tm, "103015").is_ok());
        assert!(validate_single(Vr::Tm, "240000").is_err());
        assert!(validate_single(Vr::Dt, "20240305103015+0530").is_ok());
        assert!(validate_single(Vr::Dt, "20240305103015-1500").is_err(), "offset hour 15");
        // PS3.3 §6.2: negative offsets cap at 12h, positive at 14h.
        assert!(validate_single(Vr::Dt, "20240305103015-1300").is_err(), "negative offset hour 13");
        assert!(validate_single(Vr::Dt, "20240305103015-1200").is_ok(), "negative offset hour 12");
        assert!(validate_single(Vr::Dt, "20240305103015+1400").is_ok(), "positive offset hour 14");
    }

    #[test]
    fn ds_is_rules() {
        assert!(validate_single(Vr::Ds, "-1.5E2").is_ok());
        assert!(validate_single(Vr::Ds, "1234567890123456").is_ok(), "16 chars");
        assert!(validate_single(Vr::Ds, "12345678901234567").is_err(), "17 chars");
        assert!(validate_single(Vr::Ds, "abc").is_err());
        assert!(validate_single(Vr::Is, "+42").is_ok());
        assert!(validate_single(Vr::Is, "4 2").is_err());
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
        // backslash forbidden in LO/ST/LT but escape (0x1B) allowed
        assert!(validate_single(Vr::Lo, "a\\b").is_err());
        assert!(validate_single(Vr::Lo, "a\u{1B}b").is_ok());
    }

    #[test]
    fn pn_rules() {
        assert!(validate_single(Vr::Pn, "Doe^John^Quincy^Jr^MD").is_ok());
        assert!(validate_single(Vr::Pn, "A=B=C").is_ok());
        assert!(validate_single(Vr::Pn, "A=B=C=D").is_err(), "four groups");
        assert!(validate_single(Vr::Pn, "a^b^c^d^e^f").is_err(), "six components");
        let long = format!("{}^x", "N".repeat(65));
        assert!(validate_single(Vr::Pn, &long).is_err(), "group over 64");
    }

    #[test]
    fn ui_rules() {
        assert!(validate_single(Vr::Ui, "1.2.840.10008.1.2").is_ok());
        assert!(validate_single(Vr::Ui, "01.2").is_err(), "leading zero");
        assert!(validate_single(Vr::Ui, "1..2").is_err(), "empty component");
        assert!(validate_single(Vr::Ui, &"1".repeat(65)).is_err(), "too long");
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
        let priv_tag = Tag::private(0x0029, 0x1001, std::sync::Arc::new(crate::model::private_creator::PrivateCreator::new("X")));
        let priv_el = DicomElement::new_str(priv_tag, Vr::Lo, "a\\b");
        assert!(validate_vm(&priv_el).is_ok(), "private tags exempt");

        // Other family exempt
        let ob = DicomElement::new_bytes(Tag::new(0x7FE0, 0x0010), Vr::Ob, std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![1, 2, 3])));
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
}
