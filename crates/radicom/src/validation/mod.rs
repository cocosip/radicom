//! Value validation (VAL-01..03; docs/architecture.md §5).
//!
//! Layering: global switch × per-dataset flag × RAII scope guard. File
//! parsing runs with validation off (read-tolerant policy).

use crate::error::{Error, TagRef, ValidationFailure};
use crate::model::dicom_datetime::{parse_da, parse_dt, parse_tm};
use crate::model::element::DicomElement;
use crate::model::vr::Vr;
use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};

static VALIDATION_ENABLED: AtomicBool = AtomicBool::new(true);

// IO-07: a scope guard disables validation for the *current thread* only. Keeping
// a depth here (rather than saving/restoring the global flag) means overlapping
// guards on different threads cannot clobber the shared switch or leave it off.
thread_local! {
    static SCOPE_DISABLE_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Global validation switch (default on).
pub fn set_validation_enabled(on: bool) {
    VALIDATION_ENABLED.store(on, Ordering::SeqCst);
}

pub fn validation_enabled() -> bool {
    if SCOPE_DISABLE_DEPTH.with(Cell::get) > 0 {
        return false;
    }
    VALIDATION_ENABLED.load(Ordering::SeqCst)
}

/// RAII scope guard: disables validation on this thread until dropped (VAL-03).
#[derive(Debug)]
pub struct ValidationScopeGuard {
    // a thread-local depth must be unwound on the thread that set it
    _not_send: PhantomData<*const ()>,
}

impl ValidationScopeGuard {
    pub fn validation_off() -> Self {
        SCOPE_DISABLE_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        ValidationScopeGuard {
            _not_send: PhantomData,
        }
    }
}

impl Drop for ValidationScopeGuard {
    fn drop(&mut self) {
        SCOPE_DISABLE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
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
/// components joined by a single `-`, trailing spaces tolerated, and every
/// component a real calendar date. Partial dates (`YYYY`, `YYYYMM`), `.`/`/`
/// separators, a missing first component and month/day `00` are not conformant,
/// even though the tolerant `parse_da` accepts the partial forms.
fn validate_da(content: &str) -> Result<(), String> {
    fn component(part: &str) -> Result<(), String> {
        if part.len() != 8 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err("expected YYYYMMDD".into());
        }
        // IO-09: `parse_da` validates the real calendar (month 1-12, day valid
        // for the month/leap year), rejecting 20240000 / 20240231.
        parse_da(part)
            .map(|_| ())
            .map_err(|_| "not a valid calendar date".to_string())
    }

    let mut parts = content.split('-');
    let first = parts.next().unwrap_or("").trim_end_matches(' ');
    let second = parts.next().map(|p| p.trim_end_matches(' '));
    if parts.next().is_some() {
        return Err("more than one range separator".into());
    }
    if first.is_empty() {
        // rejects "-" and "-YYYYMMDD": a range needs its first component
        return Err("missing date component".into());
    }
    component(first)?;
    // a trailing separator ("YYYYMMDD-") is still tolerated as an open range
    if let Some(second) = second {
        if !second.is_empty() {
            component(second)?;
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

/// One TM component per PS3.5 §6.2: `HH[MM[SS[.F{1,6}]]]`. The ACR-NEMA forms
/// `HH:MM:SS` and `HH.MM.SS` are not conformant and are rejected here even
/// though the tolerant `parse_tm` accepts them.
fn validate_tm_component(part: &str) -> Result<(), String> {
    let part = part.trim_end_matches(' ');
    let (main, fraction) = match part.split_once('.') {
        Some((main, fraction)) => (main, Some(fraction)),
        None => (part, None),
    };
    if !matches!(main.len(), 2 | 4 | 6) || !main.bytes().all(|b| b.is_ascii_digit()) {
        return Err("expected HH[MM[SS]]".into());
    }
    if let Some(fraction) = fraction {
        // PS3.5: the fractional part only follows seconds and has 1-6 digits.
        if main.len() != 6
            || fraction.is_empty()
            || fraction.len() > 6
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("fraction needs SS and 1-6 digits".into());
        }
    }
    parse_tm(main)
        .map(|_| ())
        .map_err(|_| "not a valid time".to_string())
}

/// TM per PS3.5 §6.2: a single value or a `-` range of single values.
fn validate_tm(content: &str) -> Result<(), String> {
    let trimmed = content.trim_end_matches(' ');
    let mut parts = trimmed.split('-');
    let first = parts.next().unwrap_or("");
    let second = parts.next();
    if parts.next().is_some() {
        return Err("more than one range separator".into());
    }
    if first.is_empty() {
        return Err("missing time component".into());
    }
    validate_tm_component(first)?;
    match second {
        // a trailing separator ("0900-") is an open range
        Some("") | None => Ok(()),
        Some(second) => validate_tm_component(second),
    }
}

/// One DT component per PS3.5 §6.2: `YYYY[MM[DD[HH[MM[SS[.F{1,6}]]]]]]`
/// followed by an optional `&ZZXX` UTC offset. Dotted/slashed dates and a bare
/// `±HHMM` offset are ACR-NEMA forms, not conformant DT.
fn validate_dt_component(part: &str) -> Result<(), String> {
    let part = part.trim_end_matches(' ');
    let (body, offset) = match part.split_once('&') {
        Some((body, offset)) => (body, Some(offset)),
        None => (part, None),
    };
    let (digits, fraction) = match body.split_once('.') {
        Some((digits, fraction)) => (digits, Some(fraction)),
        None => (body, None),
    };
    if !matches!(digits.len(), 4 | 6 | 8 | 10 | 12 | 14)
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("expected YYYY[MM[DD[HH[MM[SS]]]]]".into());
    }
    if let Some(fraction) = fraction {
        if digits.len() != 14
            || fraction.is_empty()
            || fraction.len() > 6
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("fraction needs seconds and 1-6 digits".into());
        }
    }
    // the tolerant parser validates the calendar date and the clock ranges
    parse_dt(digits).map_err(|_| "not a valid date or time".to_string())?;
    if let Some(offset) = offset {
        let mut chars = offset.chars();
        let sign = match chars.next() {
            Some('+') => 1i32,
            Some('-') => -1i32,
            _ => return Err("UTC offset must start with + or -".into()),
        };
        let nums: String = chars.collect();
        if !matches!(nums.len(), 2 | 4) || !nums.bytes().all(|b| b.is_ascii_digit()) {
            return Err("UTC offset needs 2 or 4 digits".into());
        }
        let hour: i32 = nums[..2]
            .parse()
            .map_err(|_| "UTC offset hour".to_string())?;
        let minute: i32 = if nums.len() == 4 {
            nums[2..]
                .parse()
                .map_err(|_| "UTC offset minute".to_string())?
        } else {
            0
        };
        // PS3.3 §6.2: positive offsets cap at 14h, negative at 12h.
        let max = if sign > 0 { 14 } else { 12 };
        if hour > max || minute > 59 || (hour == max && minute != 0) {
            return Err("UTC offset out of range".into());
        }
    }
    Ok(())
}

/// DT per PS3.5 §6.2: a single value or a `-` range. A `-` inside `&ZZXX` is
/// the offset sign, not the range separator.
fn validate_dt(content: &str) -> Result<(), String> {
    let trimmed = content.trim_end_matches(' ');
    let bytes = trimmed.as_bytes();
    let is_separator =
        |index: usize| bytes[index] == b'-' && (index == 0 || bytes[index - 1] != b'&');
    match bytes.iter().enumerate().find(|(i, _)| is_separator(*i)) {
        None => validate_dt_component(trimmed),
        Some((index, _)) => {
            let first = &trimmed[..index];
            let second = &trimmed[index + 1..];
            if second
                .bytes()
                .enumerate()
                .any(|(i, _)| second.as_bytes()[i] == b'-' && is_separator(index + 1 + i))
            {
                return Err("more than one range separator".into());
            }
            validate_dt_component(first)?;
            if second.is_empty() {
                return Ok(());
            }
            validate_dt_component(second)?;
            // DT values are big-endian, so the raw digits order chronologically
            // (a shorter partial value sorts before its extension).
            let first_body = first.split('&').next().unwrap_or(first);
            let second_body = second.split('&').next().unwrap_or(second);
            if first_body > second_body {
                return Err("range ends before it begins".into());
            }
            Ok(())
        }
    }
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
            // 0x7F (DEL) is a control character too (PS3.5 §6.1.3).
            code < 0x20 || code == 0x7F
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
            if b.len() == 4
                && b[..3].iter().all(|c| c.is_ascii_digit())
                && matches!(b[3], b'D' | b'W' | b'M' | b'Y')
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
        Vr::Dt => validate_dt(content).map_err(|reason| fail(vr, content, reason)),
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
                if has_control(g, true) {
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
        Vr::Tm => validate_tm(content).map_err(|reason| fail(vr, content, reason)),
        Vr::Ui => {
            if content.chars().count() > 64 {
                return Err(fail(vr, content, "length exceeds 64"));
            }
            if !crate::model::uid::Uid::is_valid(content) {
                return Err(fail(vr, content, "not a valid UID"));
            }
            Ok(())
        }
        // Unlimited Characters: free text, but no control characters
        Vr::Uc => validate_text_controls(vr, content),
        Vr::Ur => {
            // a URI/URL: no control characters and no embedded whitespace
            validate_text_controls(vr, content)?;
            if content.chars().any(char::is_whitespace) {
                return Err(fail(vr, content, "URI contains whitespace"));
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
        // PS3.5 §6.1.1: every Specific Character Set value must be a Defined
        // Term. A zero-length first value means the default repertoire; any
        // other unrecognized term cannot be encoded or decoded.
        if element.tag() == &crate::model::tag::Tag::SPECIFIC_CHARACTER_SET {
            for (index, term) in s.split('\\').enumerate() {
                if term.is_empty() && index == 0 {
                    continue;
                }
                if !crate::encoding::charset::is_known_charset(term) {
                    return Err(Error::Validation(ValidationFailure {
                        tag: tag_ref,
                        vr: element.vr().code(),
                        content: s.clone(),
                        reason: format!("undefined Specific Character Set term {term:?}"),
                    }));
                }
            }
        }
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
    use std::sync::Mutex;

    // tests that touch the process-global switch must not run concurrently
    static GLOBAL_SWITCH_LOCK: Mutex<()> = Mutex::new(());

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
        assert!(
            validate_single(Vr::As, "034").is_err(),
            "AS needs a D/W/M/Y suffix"
        );
        assert!(validate_single(Vr::As, "034X").is_err());
        assert!(validate_single(Vr::As, "34Y").is_err());

        assert!(validate_single(Vr::Cs, "ORIGINAL PRIMARY").is_ok());
        assert!(validate_single(Vr::Cs, "lower case").is_err());
        assert!(validate_single(Vr::Cs, "HAS-DASH").is_err());
    }

    #[test]
    fn date_time_rules_are_strict() {
        assert!(validate_single(Vr::Da, "20240305").is_ok());
        assert!(validate_single(Vr::Da, "20241301").is_err());
        assert!(
            validate_single(Vr::Da, "20240101-20241231").is_ok(),
            "ranges valid"
        );
        assert!(validate_single(Vr::Tm, "103015").is_ok());
        assert!(validate_single(Vr::Tm, "240000").is_err());

        // PS3.5 §6.2: DT offsets use `&ZZXX`; a bare ±HHMM is ACR-NEMA.
        assert!(validate_single(Vr::Dt, "20240305103015&+0530").is_ok());
        assert!(
            validate_single(Vr::Dt, "20240305103015+0530").is_err(),
            "a bare offset is not a DT value"
        );
        assert!(
            validate_single(Vr::Dt, "20240305103015&+1500").is_err(),
            "offset hour 15"
        );
        // PS3.3 §6.2: negative offsets cap at 12h, positive at 14h.
        assert!(
            validate_single(Vr::Dt, "20240305103015&-1300").is_err(),
            "negative offset hour 13"
        );
        assert!(
            validate_single(Vr::Dt, "20240305103015&-1200").is_ok(),
            "negative offset hour 12"
        );
        assert!(
            validate_single(Vr::Dt, "20240305103015&+1400").is_ok(),
            "positive offset hour 14"
        );

        // ACR-NEMA `HH:MM:SS`, `HH.MM.SS` and dotted dates are not conformant.
        assert!(validate_single(Vr::Tm, "10:30:15").is_err());
        assert!(validate_single(Vr::Tm, "10.30.15").is_err());
        assert!(validate_single(Vr::Dt, "2024.03.05").is_err());
        assert!(validate_single(Vr::Tm, "10").is_ok());
        assert!(validate_single(Vr::Tm, "1030").is_ok());
        assert!(validate_single(Vr::Tm, "103015.123456").is_ok());
        assert!(validate_single(Vr::Tm, "103015-110000").is_ok());
        assert!(validate_single(Vr::Dt, "20240305103015&+0530-20240306103015&+0530").is_ok());
        assert!(validate_single(Vr::Dt, "20240306103015-20240305103015").is_err());
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
        // PN is charset-sensitive: ISO 2022 CJK names legitimately carry ESC
        // (0x1B), so it is allowed; other control characters are still rejected.
        assert!(
            validate_single(Vr::Pn, "山田^\u{1B}$B太郎").is_ok(),
            "ESC allowed for ISO 2022 PN"
        );
        assert!(validate_single(Vr::Pn, "a\u{7}b").is_err(), "BEL rejected");
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
        let _lock = GLOBAL_SWITCH_LOCK.lock().unwrap();
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
    fn da_rejects_empty_components_and_impossible_calendar_dates() {
        // IO-09: these used to pass the old month<=12/day<=31 check.
        assert!(validate_single(Vr::Da, "-").is_err(), "lone separator");
        assert!(
            validate_single(Vr::Da, "-20240305").is_err(),
            "leading separator"
        );
        assert!(validate_single(Vr::Da, "20240000").is_err(), "month/day 00");
        assert!(validate_single(Vr::Da, "20240230").is_err(), "Feb 30");
        assert!(
            validate_single(Vr::Da, "20230229").is_err(),
            "not a leap year"
        );
        assert!(validate_single(Vr::Da, "20240229").is_ok(), "real leap day");
        assert!(validate_single(Vr::Da, "20241301").is_err(), "month 13");
    }

    #[test]
    fn validation_scope_guard_is_thread_local() {
        // IO-07: a guard disables validation only on its own thread and does not
        // clobber the global switch.
        let _lock = GLOBAL_SWITCH_LOCK.lock().unwrap();
        set_validation_enabled(true);
        let guard = ValidationScopeGuard::validation_off();
        assert!(!validation_enabled(), "current thread disabled");
        let other = std::thread::spawn(validation_enabled).join().unwrap();
        assert!(
            other,
            "a scope guard on one thread must not disable validation elsewhere"
        );
        drop(guard);
        assert!(
            validation_enabled(),
            "dropping the guard restores this thread"
        );
    }

    #[test]
    fn nested_validation_guards_restore_at_the_outermost_drop() {
        let _lock = GLOBAL_SWITCH_LOCK.lock().unwrap();
        set_validation_enabled(true);
        let outer = ValidationScopeGuard::validation_off();
        assert!(!validation_enabled());
        {
            let _inner = ValidationScopeGuard::validation_off();
            assert!(!validation_enabled());
        }
        assert!(
            !validation_enabled(),
            "the outer scope still disables after the inner one drops"
        );
        drop(outer);
        assert!(validation_enabled());
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
    fn uc_and_ur_reject_control_and_whitespace() {
        // Uc is free text but may not carry control characters
        assert!(validate_single(Vr::Uc, "free text").is_ok());
        assert!(validate_single(Vr::Uc, "bad\u{7}text").is_err());
        // a URI has no embedded whitespace
        assert!(validate_single(Vr::Ur, "https://example.org/a?b=c").is_ok());
        assert!(validate_single(Vr::Ur, "http://x y").is_err());
        assert!(validate_single(Vr::Ur, "http://x\u{0}").is_err());
    }

    #[test]
    fn validate_element_reports_a_typed_failure() {
        let err = validate_element(&el(Vr::Lo, "a\\b")).unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }
}
