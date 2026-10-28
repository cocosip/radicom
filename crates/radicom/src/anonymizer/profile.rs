//! PS3.15 Basic Profile table and its matching grammar (ANON-01;
//! docs/01-architecture.md §12).
//!
//! The profile is the fo-dicom PS3.15 de-identification map, extracted to
//! `ps3.15.csv` (MS-PL, see `ATTRIBUTION.md`). Each row is:
//!
//! ```text
//! tag-pattern ; action[11] ; ...
//! ```
//!
//! where `action[i]` is the action under option `i`. The tag patterns use a
//! deliberately tiny regex subset — literals, `[chars]` classes, and fixed
//! `{n}` repetition — so matching needs no regex dependency (keeps the core
//! crate light per architecture §"heavy deps isolated").

use crate::error::{DataError, Error};

/// The 11 de-identification options, in CSV column order (§6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnonOption {
    BasicProfile = 0,
    RetainSafePrivate = 1,
    RetainUids = 2,
    RetainDeviceIdent = 3,
    RetainInstitutionIdent = 4,
    RetainPatientChars = 5,
    RetainLongFullDates = 6,
    RetainLongModifDates = 7,
    CleanDesc = 8,
    CleanStructdCont = 9,
    CleanGraph = 10,
}

impl AnonOption {
    pub const ALL: [AnonOption; 11] = [
        AnonOption::BasicProfile,
        AnonOption::RetainSafePrivate,
        AnonOption::RetainUids,
        AnonOption::RetainDeviceIdent,
        AnonOption::RetainInstitutionIdent,
        AnonOption::RetainPatientChars,
        AnonOption::RetainLongFullDates,
        AnonOption::RetainLongModifDates,
        AnonOption::CleanDesc,
        AnonOption::CleanStructdCont,
        AnonOption::CleanGraph,
    ];
}

/// One de-identification action (§6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// D: replace with a non-zero dummy consistent with the VR.
    Dummy,
    /// Z: replace with a zero-length (or minimal) value.
    ZeroLength,
    /// X: remove the element.
    Remove,
    /// K: keep untouched.
    Keep,
    /// C: clean to a safe equivalent.
    Clean,
    /// U: replace with an internally consistent UID.
    Uid,
}

impl Action {
    /// Parses an action cell. Conditional cells like `X/Z`, `Z/D`, `X/Z/U*`
    /// take only their **first** letter, matching fo-dicom's
    /// `ToCharArray().FirstOrDefault()` — so the fallback (non-retain) action
    /// is the leftmost one.
    pub fn parse(cell: &str) -> Option<Action> {
        let first = cell.trim().chars().next()?;
        match first.to_ascii_uppercase() {
            'D' => Some(Action::Dummy),
            'Z' => Some(Action::ZeroLength),
            'X' => Some(Action::Remove),
            'K' => Some(Action::Keep),
            'C' => Some(Action::Clean),
            'U' => Some(Action::Uid),
            _ => None,
        }
    }
}

/// A compiled tag pattern: literal hex + classes + `{n}` repetition (§6.1).
#[derive(Debug, Clone)]
pub struct TagPattern {
    text: String,
    tokens: Vec<Token>,
}

#[derive(Debug, Clone)]
enum Token {
    /// A literal character that must match exactly.
    Literal(char),
    /// A single character drawn from this set.
    Class(Vec<char>),
}

impl TagPattern {
    /// Compiles a pattern string; the supported grammar is tiny by design.
    pub fn new(text: &str) -> Result<TagPattern, Error> {
        let mut tokens = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            match chars[index] {
                '[' => {
                    let mut class = Vec::new();
                    index += 1;
                    while index < chars.len() && chars[index] != ']' {
                        // expand `a-b` ranges; the grammar only needs hex ranges
                        if chars.get(index + 1) == Some(&'-') && chars.get(index + 2).is_some_and(|c| *c != ']') {
                            let start = chars[index];
                            let end = chars[index + 2];
                            let mut c = start;
                            while c <= end {
                                class.push(c.to_ascii_uppercase());
                                if c == char::MAX {
                                    break;
                                }
                                c = char::from_u32(c as u32 + 1).unwrap_or(char::MAX);
                            }
                            index += 3;
                        } else {
                            class.push(chars[index].to_ascii_uppercase());
                            index += 1;
                        }
                    }
                    if index >= chars.len() {
                        return Err(pattern_error(text, "unterminated character class"));
                    }
                    index += 1; // skip ']'
                    // expansion: `{n}` repeats the just-closed class
                    let (repeat, consumed) = parse_repeat(&chars[index..]).map_err(|e| pattern_error(text, &e))?;
                    index += consumed;
                    let count = repeat.unwrap_or(1);
                    for _ in 0..count {
                        tokens.push(Token::Class(class.clone()));
                    }
                }
                ch => {
                    let literal = ch.to_ascii_uppercase();
                    index += 1;
                    let (repeat, consumed) = parse_repeat(&chars[index..]).map_err(|e| pattern_error(text, &e))?;
                    index += consumed;
                    let count = repeat.unwrap_or(1);
                    for _ in 0..count {
                        tokens.push(Token::Literal(literal));
                    }
                }
            }
        }
        Ok(TagPattern { text: text.to_string(), tokens })
    }

    /// True when `tag` (as `GGGG,EEEE`, uppercase) matches this pattern.
    pub fn matches(&self, tag: &str) -> bool {
        let tag = tag.to_ascii_uppercase();
        let chars: Vec<char> = tag.chars().collect();
        if chars.len() != self.tokens.len() {
            return false;
        }
        for (ch, token) in chars.iter().zip(&self.tokens) {
            let ok = match token {
                Token::Literal(expected) => ch.to_ascii_uppercase() == *expected,
                Token::Class(set) => set.contains(&ch.to_ascii_uppercase()),
            };
            if !ok {
                return false;
            }
        }
        true
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Parses an optional `{n}` suffix; returns the count and chars consumed.
fn parse_repeat(rest: &[char]) -> Result<(Option<usize>, usize), String> {
    if rest.first() != Some(&'{') {
        return Ok((None, 0));
    }
    let mut index = 1;
    let mut digits = String::new();
    while index < rest.len() && rest[index].is_ascii_digit() {
        digits.push(rest[index]);
        index += 1;
    }
    if rest.get(index) != Some(&'}') || digits.is_empty() {
        return Err("malformed {n} repetition".into());
    }
    let count = digits.parse::<usize>().map_err(|_| "bad repetition count".to_string())?;
    Ok((Some(count), index + 1))
}

fn pattern_error(text: &str, reason: &str) -> Error {
    Error::Data(DataError::Other { message: format!("bad profile pattern \"{text}\": {reason}") })
}

/// One profile row: a tag pattern and its per-option action (§6.1).
#[derive(Debug, Clone)]
pub struct ProfileRow {
    pub pattern: TagPattern,
    /// `None` means the row leaves the tag untouched under that option.
    pub actions: [Option<Action>; 11],
}

impl ProfileRow {
    /// The action under one option.
    pub fn action_for(&self, option: AnonOption) -> Option<Action> {
        self.actions[option as usize]
    }
}

/// A parsed de-identification profile (§6.1).
#[derive(Debug, Clone)]
pub struct SecurityProfile {
    rows: Vec<ProfileRow>,
}

impl SecurityProfile {
    /// Parses the CSV embedded in this module (rows of `tag;action x11`).
    pub fn basic() -> Self {
        Self::parse(include_str!("ps3.15.csv")).expect("the built-in PS3.15 profile must parse")
    }

    /// Parses a CSV profile string; used for the built-in table and custom ones.
    pub fn parse(csv: &str) -> Result<Self, Error> {
        let mut rows = Vec::new();
        for (line_number, line) in csv.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split(';');
            let tag_pattern = fields.next().unwrap_or("").trim();
            if tag_pattern.is_empty() {
                continue;
            }
            let pattern = TagPattern::new(tag_pattern)?;
            let mut actions = [None; 11];
            for (slot, cell) in actions.iter_mut().zip(fields) {
                *slot = Action::parse(cell);
            }
            rows.push(ProfileRow { pattern, actions });
            let _ = line_number;
        }
        if rows.is_empty() {
            return Err(Error::Data(DataError::Other { message: "profile has no rows".into() }));
        }
        Ok(SecurityProfile { rows })
    }

    /// The action for `tag` under `option` (§6.2).
    ///
    /// fo-dicom builds one table per option and inserts a row only when that
    /// option's cell is filled, so a row whose cell is empty for the selected
    /// option does not shadow the rows below it. Matching the first row and
    /// *then* looking up the cell (the old behaviour) let the leading
    /// odd-private-group row swallow every later row for options it does not
    /// take part in, which is why `CleanGraph` did nothing.
    pub fn action_for(&self, tag: &str, option: AnonOption) -> Option<Action> {
        self.rows
            .iter()
            .find_map(|row| row.pattern.matches(tag).then(|| row.action_for(option)).flatten())
    }

    pub fn rows(&self) -> &[ProfileRow] {
        &self.rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_profile_parses_and_is_large() {
        let profile = SecurityProfile::basic();
        assert!(profile.rows().len() > 600, "the PS3.15 table has ~655 rows, got {}", profile.rows().len());
    }

    #[test]
    fn a_literal_tag_matches_and_gets_its_action() {
        let profile = SecurityProfile::basic();
        // (0008,0050) AccessionNumber is Z (blank) under the basic profile
        assert_eq!(profile.action_for("0008,0050", AnonOption::BasicProfile), Some(Action::ZeroLength));
    }

    #[test]
    fn a_class_pattern_matches_curve_tags() {
        let profile = SecurityProfile::basic();
        // 50xx,xxxx curve group is X (remove) under basic profile
        assert_eq!(profile.action_for("5000,0100", AnonOption::BasicProfile), Some(Action::Remove));
        assert_eq!(profile.action_for("5010,ABCD", AnonOption::BasicProfile), Some(Action::Remove));
    }

    #[test]
    fn the_odd_private_group_pattern_matches() {
        let profile = SecurityProfile::basic();
        // odd group + odd element (private) is X under basic profile
        assert_eq!(profile.action_for("0009,0001", AnonOption::BasicProfile), Some(Action::Remove));
        // an even group does not match that row
        assert_ne!(profile.action_for("0008,0001", AnonOption::BasicProfile), Some(Action::Remove));
    }

    #[test]
    fn an_option_skips_rows_that_do_not_take_part_in_it() {
        // The leading row (`[0-9A-F]{3}[13579BDF],...` → X under BasicProfile,
        // C under RetainSafePrivate) matches every odd private tag, but it has
        // no CleanGraph cell; the graph rows below it must still be reachable
        // (fo-dicom: one table per option, rows with an empty cell are absent).
        let profile = SecurityProfile::basic();
        assert_eq!(profile.action_for("5011,1234", AnonOption::CleanGraph), Some(Action::Clean));
        assert_eq!(profile.action_for("6011,3000", AnonOption::CleanGraph), Some(Action::Clean));
        assert_eq!(profile.action_for("50FF,0001", AnonOption::CleanGraph), Some(Action::Clean));

        // ... while the options the leading row does take part in are unchanged
        assert_eq!(profile.action_for("5011,1234", AnonOption::BasicProfile), Some(Action::Remove));
        assert_eq!(profile.action_for("5011,1234", AnonOption::RetainSafePrivate), Some(Action::Clean));

        // a tag no row covers under the option still has no action
        assert_eq!(profile.action_for("0010,0010", AnonOption::CleanGraph), None);
    }

    #[test]
    fn a_pattern_compiles_literals_classes_and_repetition() {
        let pattern = TagPattern::new("50[0-9A-F]{2},[0-9A-F]{4}").unwrap();
        assert!(pattern.matches("50AB,1234"));
        assert!(pattern.matches("50ab,1234"), "matching is case-insensitive");
        assert!(!pattern.matches("60AB,1234"), "the first literal must match");
        assert!(!pattern.matches("50AB,12345"), "length must match exactly");
    }

    #[test]
    fn an_empty_profile_is_rejected() {
        assert!(SecurityProfile::parse("# only a comment\n\n").is_err());
    }

    #[test]
    fn actions_parse_including_the_conditional_form() {
        assert_eq!(Action::parse("D"), Some(Action::Dummy));
        assert_eq!(Action::parse("K"), Some(Action::Keep));
        assert_eq!(Action::parse("U"), Some(Action::Uid));
        assert_eq!(Action::parse("X/Z"), Some(Action::Remove), "conditional cells take the first letter");
        assert_eq!(Action::parse("Z/D"), Some(Action::ZeroLength));
        assert_eq!(Action::parse("X/Z/U*"), Some(Action::Remove));
        assert_eq!(Action::parse(""), None);
        assert_eq!(Action::parse("?"), None);
    }
}
