//! Person Name (PN) component model (ELEM-01; docs/01-architecture.md §3).
//!
//! Value shape: up to three `=`-separated groups (alphabetic, ideographic,
//! phonetic), each with up to five `^`-separated components:
//! `Family^Given^Middle^Prefix^Suffix`.

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct PersonName {
    groups: [Option<String>; 3],
}

const COMPONENT_NAMES: [&str; 5] = ["family", "given", "middle", "prefix", "suffix"];

impl PersonName {
    /// Parses the full PN value (all groups).
    pub fn parse(s: &str) -> Self {
        let mut groups: [Option<String>; 3] = [None, None, None];
        for (i, g) in s.split('=').take(3).enumerate() {
            groups[i] = Some(g.to_string());
        }
        PersonName { groups }
    }

    fn component(&self, group: usize, index: usize) -> Option<&str> {
        self.groups[group]
            .as_deref()
            .and_then(|g| g.split('^').nth(index))
            .filter(|c| !c.is_empty())
    }

    fn named_component(&self, group: usize, name: &str) -> Option<&str> {
        let index = COMPONENT_NAMES.iter().position(|n| *n == name)?;
        self.component(group, index)
    }

    /// Alphabetic-group component accessors (fo-dicom reads the first group).
    pub fn family(&self) -> Option<&str> {
        self.component(0, 0)
    }
    pub fn given(&self) -> Option<&str> {
        self.component(0, 1)
    }
    pub fn middle(&self) -> Option<&str> {
        self.component(0, 2)
    }
    pub fn prefix(&self) -> Option<&str> {
        self.component(0, 3)
    }
    pub fn suffix(&self) -> Option<&str> {
        self.component(0, 4)
    }

    /// Named component from any group (`"family"|"given"|"middle"|"prefix"|"suffix"`).
    pub fn component_by_name(&self, group: usize, name: &str) -> Option<&str> {
        self.named_component(group, name)
    }

    /// The alphabetic group text.
    pub fn alphabetic(&self) -> &str {
        self.groups[0].as_deref().unwrap_or("")
    }

    /// The ideographic group text (second `=` group), if present.
    pub fn ideographic(&self) -> Option<&str> {
        self.groups[1].as_deref()
    }

    /// The phonetic group text (third `=` group), if present.
    pub fn phonetic(&self) -> Option<&str> {
        self.groups[2].as_deref()
    }

    /// Builds from components (missing components stay empty).
    pub fn from_components(
        family: &str,
        given: &str,
        middle: &str,
        prefix: &str,
        suffix: &str,
    ) -> Self {
        PersonName::parse(&format!("{family}^{given}^{middle}^{prefix}^{suffix}"))
    }

    /// Case-insensitive comparison of every component in every group
    /// (fo-dicom `PersonName.HaveSameContent`).
    pub fn have_same_content(a: &PersonName, b: &PersonName) -> bool {
        for g in 0..3 {
            for c in 0..5 {
                let x = a.component(g, c).map(str::to_lowercase);
                let y = b.component(g, c).map(str::to_lowercase);
                if x != y {
                    return false;
                }
            }
        }
        true
    }
}

impl std::fmt::Display for PersonName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        for g in &self.groups {
            let Some(text) = g else {
                parts.push(String::new());
                continue;
            };
            let mut comps: Vec<&str> = text.split('^').collect();
            while comps.last().is_some_and(|c| c.is_empty()) {
                comps.pop();
            }
            parts.push(comps.join("^"));
        }
        while parts.last().is_some_and(|p| p.is_empty()) {
            parts.pop();
        }
        f.write_str(&parts.join("="))
    }
}

/// Parses a DS value: invariant float with fo-dicom's comma-decimal
/// tolerance (`"1,5"` → 1.5).
pub fn parse_ds(s: &str) -> Result<f64, crate::error::Error> {
    let trimmed = s.trim();
    if let Ok(value) = trimmed.parse::<f64>() {
        return Ok(value);
    }
    let normalized = trimmed.replace(',', ".");
    normalized.parse::<f64>().map_err(|_| {
        crate::error::Error::Data(crate::error::DataError::Other {
            message: format!("invalid DS value \"{s}\""),
        })
    })
}

/// Formats an f64 as a DS string: at most 16 characters; longer outputs
/// demote to 10 significant digits (fo-dicom `ToDecimalString`).
///
/// Small magnitudes need scientific notation: `{:.10}` would round e.g.
/// 1e-20 to `0.0000000000` and silently lose the value.
pub fn format_ds(v: f64) -> String {
    let full = format!("{v}");
    if full.len() <= 16 {
        return full;
    }
    if v != 0.0 && v.abs() < 1e-4 {
        let scientific = format!("{v:.6e}");
        if scientific.len() <= 16 {
            return scientific;
        }
    }
    let demoted = format!("{v:.10}");
    if demoted.len() <= 16 {
        demoted
    } else {
        format!("{v:.3e}")
    }
}

/// Parses an IS value: i32 with fo-dicom's lenient decimal-fraction
/// truncation (`"3.0"` → 3).
pub fn parse_is(s: &str) -> Result<i32, crate::error::Error> {
    let t = s.trim();
    let invalid = || {
        crate::error::Error::Data(crate::error::DataError::Other {
            message: format!("invalid IS value \"{s}\""),
        })
    };
    if let Ok(v) = t.parse::<i32>() {
        return Ok(v);
    }
    // lenient: an all-digit decimal fraction truncates ("3.0" -> 3)
    let Some((head, tail)) = t.split_once('.') else {
        return Err(invalid());
    };
    if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    head.parse::<i32>().map_err(|_| invalid())
}

/// Formats an i32 as an IS string.
pub fn format_is(v: i32) -> String {
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pn_components() {
        let pn = PersonName::parse("Doe^John^Quincy^Jr^MD");
        assert_eq!(pn.family(), Some("Doe"));
        assert_eq!(pn.given(), Some("John"));
        assert_eq!(pn.middle(), Some("Quincy"));
        assert_eq!(pn.prefix(), Some("Jr"));
        assert_eq!(pn.suffix(), Some("MD"));
        assert_eq!(pn.to_string(), "Doe^John^Quincy^Jr^MD");

        let partial = PersonName::parse("Adams^John");
        assert_eq!(partial.family(), Some("Adams"));
        assert_eq!(partial.given(), Some("John"));
        assert_eq!(partial.middle(), None);
        assert_eq!(partial.to_string(), "Adams^John");

        let from = PersonName::from_components("Kim", "Min", "", "Dr", "");
        assert_eq!(from.family(), Some("Kim"));
        assert_eq!(from.prefix(), Some("Dr"));
        assert_eq!(from.to_string(), "Kim^Min^^Dr");
    }

    #[test]
    fn pn_groups() {
        let tri = PersonName::parse("Hong^Gildong=洪吉洞=洪吉洞");
        assert_eq!(tri.alphabetic(), "Hong^Gildong");
        assert_eq!(tri.ideographic(), Some("洪吉洞"));
        assert_eq!(tri.phonetic(), Some("洪吉洞"));
        assert_eq!(tri.component_by_name(1, "family"), Some("洪吉洞"));
        assert_eq!(tri.to_string(), "Hong^Gildong=洪吉洞=洪吉洞");

        let single = PersonName::parse("Anonymous");
        assert_eq!(single.family(), Some("Anonymous"));
        assert_eq!(single.ideographic(), None);
    }

    #[test]
    fn pn_content_equality() {
        let a = PersonName::parse("Doe^John");
        let b = PersonName::parse("Doe^JOHN^");
        assert!(
            PersonName::have_same_content(&a, &b),
            "case-insensitive, trailing empties ignored"
        );
        let c = PersonName::parse("Doe^Jane");
        assert!(!PersonName::have_same_content(&a, &c));
    }

    #[test]
    fn ds_parsing_and_formatting() {
        assert_eq!(parse_ds("1.5").unwrap(), 1.5);
        assert_eq!(parse_ds("1,5").unwrap(), 1.5, "comma decimal tolerated");
        assert_eq!(parse_ds(" -2.5e2 ").unwrap(), -250.0);
        assert!(parse_ds("abc").is_err());

        assert_eq!(format_ds(1.5), "1.5");
        let long = format_ds(123456789012345678.9_f64);
        assert!(long.len() <= 16, "{long}");
    }

    #[test]
    fn is_parsing_and_formatting() {
        assert_eq!(parse_is("42").unwrap(), 42);
        assert_eq!(parse_is("-7").unwrap(), -7);
        assert_eq!(parse_is("3.0").unwrap(), 3, "decimal fraction truncates");
        assert_eq!(parse_is(" 12 ").unwrap(), 12);
        assert!(parse_is("abc").is_err());
        assert!(parse_is("3.5xyz").is_err());
        assert_eq!(format_is(-7), "-7");
    }

    #[test]
    fn format_ds_preserves_small_magnitudes() {
        // `{:.10}` would render 1e-20 as 0.0000000000 and lose the value.
        let text = format_ds(1e-20);
        assert!(text.len() <= 16, "{text}");
        assert_ne!(text, "0.0000000000");
        assert_eq!(parse_ds(&text).unwrap(), 1e-20);
        // ordinary values keep the plain decimal form
        assert_eq!(format_ds(2.5), "2.5");
    }
}
