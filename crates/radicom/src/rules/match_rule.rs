//! Match rule DSL for query filtering (MATCH-01; docs/02-development-guide.md §2).
//!
//! Ports fo-dicom `DicomMatchRules`: a [`MatchRule`] tests a dataset against a
//! tag/value condition. Rules compose with [`MatchRuleSet`] (And/Or) and
//! [`MatchRule::Negate`]. Regex matching is available behind the dedicated
//! `regex-rules` feature; the rest are dependency-free.

use crate::model::dataset::Dataset;
use crate::model::tag::Tag;

/// How a [`MatchRuleSet`] combines its rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOperator {
    And,
    Or,
}

/// A dataset match predicate (fo-dicom `IDicomMatchRule`).
#[derive(Debug, Clone)]
pub enum MatchRule {
    /// `(tag)` is present.
    Exists(Tag),
    /// `(tag)` is absent or empty.
    IsEmpty(Tag),
    /// `(tag)` equals the string (missing counts as empty).
    Equals(Tag, String),
    StartsWith(Tag, String),
    EndsWith(Tag, String),
    Contains(Tag, String),
    /// DICOM wildcard (`*` = any run, `?` = one char).
    Wildcard(Tag, String),
    /// The value is one of the given strings.
    OneOf(Tag, Vec<String>),
    /// Regex match (`regex-rules` feature).
    #[cfg(feature = "regex-rules")]
    Regex(Tag, String),
    /// Always true / always false.
    Bool(bool),
    /// Logical negation.
    Negate(Box<MatchRule>),
    /// A nested rule set.
    Set(MatchRuleSet),
}

/// A set of rules combined with [`MatchOperator`] (fo-dicom `DicomMatchRuleSet`).
#[derive(Debug, Clone)]
pub struct MatchRuleSet {
    pub operator: MatchOperator,
    pub rules: Vec<MatchRule>,
}

impl Default for MatchRuleSet {
    fn default() -> Self {
        MatchRuleSet {
            operator: MatchOperator::And,
            rules: Vec::new(),
        }
    }
}

impl MatchRuleSet {
    pub fn new(operator: MatchOperator) -> Self {
        MatchRuleSet {
            operator,
            rules: Vec::new(),
        }
    }

    pub fn with(mut self, rule: MatchRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// True when the set matches: empty ⇒ true; And ⇒ all; Or ⇒ any.
    pub fn matches(&self, dataset: &Dataset) -> bool {
        if self.rules.is_empty() {
            return true;
        }
        match self.operator {
            MatchOperator::And => self.rules.iter().all(|r| r.matches(dataset)),
            MatchOperator::Or => self.rules.iter().any(|r| r.matches(dataset)),
        }
    }
}

impl MatchRule {
    /// Evaluates the rule against a dataset (fo-dicom `Match`).
    pub fn matches(&self, dataset: &Dataset) -> bool {
        let value = |tag: &Tag| dataset.try_get_str(tag).unwrap_or_default();
        match self {
            MatchRule::Exists(tag) => dataset.contains(tag),
            MatchRule::IsEmpty(tag) => value(tag).is_empty(),
            MatchRule::Equals(tag, s) => &value(tag) == s,
            MatchRule::StartsWith(tag, s) => value(tag).starts_with(s.as_str()),
            MatchRule::EndsWith(tag, s) => value(tag).ends_with(s.as_str()),
            MatchRule::Contains(tag, s) => value(tag).contains(s.as_str()),
            MatchRule::Wildcard(tag, pattern) => dicom_wildcard_match(&value(tag), pattern),
            MatchRule::OneOf(tag, values) => {
                let v = value(tag);
                values.contains(&v)
            }
            #[cfg(feature = "regex-rules")]
            MatchRule::Regex(tag, pattern) => regex::Regex::new(pattern)
                .map(|re| re.is_match(&value(tag)))
                .unwrap_or(false),
            MatchRule::Bool(b) => *b,
            MatchRule::Negate(inner) => !inner.matches(dataset),
            MatchRule::Set(set) => set.matches(dataset),
        }
    }
}

/// Matches a DICOM wildcard pattern against a value: `*` matches any run of
/// characters, `?` matches exactly one; all other characters are literal.
/// (PS3.4 C.2.2.2 — distinct from filesystem globs, which also use `**`.)
///
/// Iterative single-`*` backtracking (the classic two-pointer glob): no
/// recursion and no `Vec<char>`, so a pathological pattern like
/// `*a*a*a…b` cannot exhaust the stack or re-scan quadratically.
pub fn dicom_wildcard_match(value: &str, pattern: &str) -> bool {
    let value_bytes = value.as_bytes();
    let pattern_bytes = pattern.as_bytes();
    let mut vi = 0;
    let mut pi = 0;
    // most recent `*`: its pattern index and the value offset it may resume at
    let mut star_pi: Option<usize> = None;
    let mut star_vi = 0;
    while vi < value_bytes.len() {
        if pi < pattern_bytes.len() {
            match pattern_bytes[pi] {
                b'*' => {
                    star_pi = Some(pi);
                    star_vi = vi;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    vi += next_char_len(value, vi);
                    pi += 1;
                    continue;
                }
                literal if literal == value_bytes[vi] => {
                    vi += 1;
                    pi += 1;
                    continue;
                }
                _ => {}
            }
        }
        // mismatch (or the pattern ran out): let the last `*` swallow one more
        // character and retry from there
        match star_pi {
            Some(sp) => {
                pi = sp + 1;
                star_vi += next_char_len(value, star_vi);
                vi = star_vi;
            }
            None => return false,
        }
    }
    while pi < pattern_bytes.len() && pattern_bytes[pi] == b'*' {
        pi += 1;
    }
    pi == pattern_bytes.len()
}

/// Byte length of the UTF-8 character at `at` (1 at/after the end).
fn next_char_len(text: &str, at: usize) -> usize {
    text[at..].chars().next().map(char::len_utf8).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vr::Vr;

    fn dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds.insert_str(Tag::MODALITY, Vr::Cs, "CT");
        ds.insert_str(Tag::ACCESSION_NUMBER, Vr::Sh, "ACC123");
        ds
    }

    #[test]
    fn exists_and_is_empty() {
        assert!(MatchRule::Exists(Tag::PATIENT_NAME).matches(&dataset()));
        assert!(!MatchRule::Exists(Tag::PATIENT_ID).matches(&dataset()));
        assert!(
            MatchRule::IsEmpty(Tag::PATIENT_ID).matches(&dataset()),
            "missing is empty"
        );
        assert!(!MatchRule::IsEmpty(Tag::PATIENT_NAME).matches(&dataset()));
    }

    #[test]
    fn string_comparisons() {
        let ds = dataset();
        assert!(MatchRule::Equals(Tag::MODALITY, "CT".into()).matches(&ds));
        assert!(MatchRule::StartsWith(Tag::PATIENT_NAME, "Doe".into()).matches(&ds));
        assert!(MatchRule::EndsWith(Tag::PATIENT_NAME, "John".into()).matches(&ds));
        assert!(MatchRule::Contains(Tag::ACCESSION_NUMBER, "ACC".into()).matches(&ds));
        assert!(!MatchRule::Equals(Tag::MODALITY, "MR".into()).matches(&ds));
    }

    #[test]
    fn one_of_matches_any_candidate() {
        let ds = dataset();
        let rule = MatchRule::OneOf(Tag::MODALITY, vec!["CT".into(), "MR".into()]);
        assert!(rule.matches(&ds));
        let miss = MatchRule::OneOf(Tag::MODALITY, vec!["US".into()]);
        assert!(!miss.matches(&ds));
    }

    #[test]
    fn wildcard_star_and_question() {
        assert!(dicom_wildcard_match("Doe^John", "Doe*"));
        assert!(dicom_wildcard_match("Doe^John", "*John"));
        assert!(dicom_wildcard_match("Doe^John", "D?e^John"));
        assert!(dicom_wildcard_match("CT", "*"));
        assert!(!dicom_wildcard_match("CT", "MR"));
        assert!(!dicom_wildcard_match("CT", "C?T?"), "question is one char");
        assert!(dicom_wildcard_match("ACC123", "ACC*"));
    }

    #[test]
    fn and_or_negate_composition() {
        let ds = dataset();
        let and_set = MatchRuleSet::new(MatchOperator::And)
            .with(MatchRule::Equals(Tag::MODALITY, "CT".into()))
            .with(MatchRule::StartsWith(Tag::PATIENT_NAME, "Doe".into()));
        assert!(and_set.matches(&ds));

        let or_set = MatchRuleSet::new(MatchOperator::Or)
            .with(MatchRule::Equals(Tag::MODALITY, "MR".into()))
            .with(MatchRule::Equals(Tag::MODALITY, "CT".into()));
        assert!(or_set.matches(&ds));

        let negated = MatchRule::Negate(Box::new(MatchRule::Equals(Tag::MODALITY, "MR".into())));
        assert!(negated.matches(&ds));
    }

    #[test]
    fn an_empty_set_matches_everything() {
        assert!(MatchRuleSet::default().matches(&dataset()));
    }

    #[test]
    fn bool_rules_are_constant() {
        assert!(MatchRule::Bool(true).matches(&dataset()));
        assert!(!MatchRule::Bool(false).matches(&dataset()));
    }

    #[cfg(feature = "regex-rules")]
    #[test]
    fn regex_rule_matches() {
        let ds = dataset();
        assert!(MatchRule::Regex(Tag::ACCESSION_NUMBER, "^ACC\\d+$".into()).matches(&ds));
        assert!(!MatchRule::Regex(Tag::ACCESSION_NUMBER, "^MR".into()).matches(&ds));
        // an invalid pattern never matches instead of panicking
        assert!(!MatchRule::Regex(Tag::ACCESSION_NUMBER, "(".into()).matches(&ds));
    }

    #[test]
    fn nested_rule_sets_participate_in_matching() {
        let nested = MatchRuleSet::new(MatchOperator::Or)
            .with(MatchRule::Equals(Tag::MODALITY, "MR".into()))
            .with(MatchRule::Equals(Tag::MODALITY, "CT".into()));
        assert!(MatchRule::Set(nested).matches(&dataset()));
    }

    #[test]
    fn wildcard_with_an_empty_value_and_trailing_star_is_false() {
        assert!(!dicom_wildcard_match("", "*a"));
        assert!(dicom_wildcard_match("", "*"));
    }

    #[test]
    fn a_pathological_wildcard_pattern_does_not_recurse() {
        // 40k single-byte characters would make the old recursive matcher walk
        // tens of thousands of stack frames; the iterative matcher is flat.
        let long = "a".repeat(40_000);
        assert!(dicom_wildcard_match(&long, "*a"));
        assert!(!dicom_wildcard_match(&long, "*a*a*a*a*a*b"));
        assert!(dicom_wildcard_match(&long, "*a*a*a*a*a*a"));
        assert!(dicom_wildcard_match("", "*"));
        assert!(!dicom_wildcard_match("", "*a"));
    }

    #[test]
    fn a_question_mark_matches_one_whole_character() {
        assert!(dicom_wildcard_match("ü", "?"));
        assert!(dicom_wildcard_match("Döe^John", "Dö?^J?hn"));
        assert!(!dicom_wildcard_match("ü", "??"));
    }
}
