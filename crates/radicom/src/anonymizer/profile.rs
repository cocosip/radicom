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
use crate::model::tag::Tag;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Arc;

/// The 11 de-identification options, in CSV column order (§12.5).
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

/// One de-identification action (§12.5).
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

/// A compiled tag pattern: literal hex + classes + `{n}` repetition (§12.5).
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
                        if chars.get(index + 1) == Some(&'-')
                            && chars.get(index + 2).is_some_and(|c| *c != ']')
                        {
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
                    let (repeat, consumed) =
                        parse_repeat(&chars[index..]).map_err(|e| pattern_error(text, &e))?;
                    index += consumed;
                    let count = repeat.unwrap_or(1);
                    for _ in 0..count {
                        tokens.push(Token::Class(class.clone()));
                    }
                }
                ch => {
                    let literal = ch.to_ascii_uppercase();
                    index += 1;
                    let (repeat, consumed) =
                        parse_repeat(&chars[index..]).map_err(|e| pattern_error(text, &e))?;
                    index += consumed;
                    let count = repeat.unwrap_or(1);
                    for _ in 0..count {
                        tokens.push(Token::Literal(literal));
                    }
                }
            }
        }
        Ok(TagPattern {
            text: text.to_string(),
            tokens,
        })
    }

    /// True when `tag` (as `GGGG,EEEE`, uppercase) matches this pattern.
    ///
    /// Compared byte-wise without allocating: patterns only ever hold ASCII
    /// (hex digits, `,`, `[`, `]`, `{n}`), so a byte that is not ASCII can never
    /// equal a literal or class member.
    pub fn matches(&self, tag: &str) -> bool {
        let bytes = tag.as_bytes();
        if bytes.len() != self.tokens.len() {
            return false;
        }
        bytes.iter().zip(&self.tokens).all(|(byte, token)| {
            let upper = byte.to_ascii_uppercase();
            match token {
                Token::Literal(expected) => expected.is_ascii() && upper == *expected as u8,
                Token::Class(set) => set.contains(&char::from(upper)),
            }
        })
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
    let count = digits
        .parse::<usize>()
        .map_err(|_| "bad repetition count".to_string())?;
    Ok((Some(count), index + 1))
}

fn pattern_error(text: &str, reason: &str) -> Error {
    Error::Data(DataError::Other {
        message: format!("bad profile pattern \"{text}\": {reason}"),
    })
}

/// The packed `group << 16 | element` key of a `GGGG,EEEE` tag string — the
/// form [`SecurityProfile::action_for`] takes. `None` when the string is not a
/// plain tag, which also identifies the patterns that are not literal tags.
fn tag_key(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    if bytes.len() != 9 || bytes[4] != b',' {
        return None;
    }
    let group = u16::from_str_radix(std::str::from_utf8(&bytes[..4]).ok()?, 16).ok()?;
    let element = u16::from_str_radix(std::str::from_utf8(&bytes[5..]).ok()?, 16).ok()?;
    Some(u32::from(group) << 16 | u32::from(element))
}

/// One profile row: a tag pattern and its per-option action (§12.5).
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

/// A parsed de-identification profile (§12.5).
///
/// Cloning is cheap: the rows and their lookup index are shared behind an
/// `Arc`, so [`SecurityProfile::basic`] can hand out the one parsed table.
#[derive(Debug, Clone)]
pub struct SecurityProfile {
    inner: Arc<ProfileInner>,
}

/// The parsed rows plus the lookup structures derived from them.
#[derive(Debug)]
struct ProfileInner {
    rows: Vec<ProfileRow>,
    /// Indices of the rows whose pattern is not a single literal tag; they are
    /// the only rows a non-literal tag can match.
    wildcards: Vec<usize>,
    /// Per option: the resolved action of every tag the table names literally,
    /// in row order (the first matching row that has a cell for the option
    /// wins, exactly like the linear scan).
    exact: [HashMap<u32, Action>; 11],
}

impl SecurityProfile {
    /// Parses the CSV embedded in this module (rows of `tag;action x11`).
    ///
    /// The 655-row table is parsed once per process and shared; earlier this
    /// re-parsed the CSV on every call (~316 µs each).
    pub fn basic() -> Self {
        static BASIC: Lazy<SecurityProfile> = Lazy::new(|| {
            SecurityProfile::parse(include_str!("ps3.15.csv"))
                .expect("the built-in PS3.15 profile must parse")
        });
        BASIC.clone()
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
            return Err(Error::Data(DataError::Other {
                message: "profile has no rows".into(),
            }));
        }
        Ok(Self::indexed(rows))
    }

    /// Builds the exact-tag index over the parsed rows.
    ///
    /// For each option the rows are replayed in order: a literal row claims its
    /// own tag, and a wildcard row claims the literal tags below it that it
    /// matches. The result is the same action the row-by-row scan would pick,
    /// but the common case becomes one hash lookup.
    fn indexed(rows: Vec<ProfileRow>) -> Self {
        let literals: Vec<(u32, String)> = rows
            .iter()
            .filter_map(|row| {
                tag_key(row.pattern.text())
                    .map(|key| (key, row.pattern.text().to_ascii_uppercase()))
            })
            .collect();
        let wildcards: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| tag_key(row.pattern.text()).is_none())
            .map(|(index, _)| index)
            .collect();

        let mut exact: [HashMap<u32, Action>; 11] = Default::default();
        for (slot, option) in exact.iter_mut().zip(AnonOption::ALL) {
            for row in &rows {
                let Some(action) = row.action_for(option) else {
                    continue;
                };
                match tag_key(row.pattern.text()) {
                    Some(key) => {
                        slot.entry(key).or_insert(action);
                    }
                    None => {
                        for (key, text) in &literals {
                            if !slot.contains_key(key) && row.pattern.matches(text) {
                                slot.insert(*key, action);
                            }
                        }
                    }
                }
            }
        }
        SecurityProfile {
            inner: Arc::new(ProfileInner {
                rows,
                wildcards,
                exact,
            }),
        }
    }

    /// The action for `tag` under `option` (§12.5).
    ///
    /// fo-dicom builds one table per option and inserts a row only when that
    /// option's cell is filled, so a row whose cell is empty for the selected
    /// option does not shadow the rows below it. Matching the first row and
    /// *then* looking up the cell (the old behaviour) let the leading
    /// odd-private-group row swallow every later row for options it does not
    /// take part in, which is why `CleanGraph` did nothing.
    pub fn action_for(&self, tag: &str, option: AnonOption) -> Option<Action> {
        if let Some(key) = tag_key(tag) {
            if let Some(action) = self.inner.exact[option as usize].get(&key) {
                return Some(*action);
            }
        }
        self.match_wildcard(tag, option)
    }

    /// The action for a typed [`Tag`] under `option`.
    ///
    /// Literal tags resolve from the key directly, so the anonymizer does not
    /// format every element's tag into a `String` and parse it back.
    pub fn action_for_tag(&self, tag: &Tag, option: AnonOption) -> Option<Action> {
        if let Some(action) = self.inner.exact[option as usize].get(&tag.to_u32()) {
            return Some(*action);
        }
        // only a wildcard row can still match; it needs the textual form
        self.match_wildcard(
            &format!("{:04X},{:04X}", tag.group(), tag.element()),
            option,
        )
    }

    fn match_wildcard(&self, tag: &str, option: AnonOption) -> Option<Action> {
        self.inner.wildcards.iter().find_map(|index| {
            let row = &self.inner.rows[*index];
            row.pattern
                .matches(tag)
                .then(|| row.action_for(option))
                .flatten()
        })
    }

    /// The unindexed scan this type used to do; kept to prove the index agrees.
    #[cfg(test)]
    fn linear_action_for(&self, tag: &str, option: AnonOption) -> Option<Action> {
        self.inner.rows.iter().find_map(|row| {
            row.pattern
                .matches(tag)
                .then(|| row.action_for(option))
                .flatten()
        })
    }

    pub fn rows(&self) -> &[ProfileRow] {
        &self.inner.rows
    }

    /// Returns a copy of this profile with `row` appended (fo-dicom lets a
    /// caller `Add` rows to the active profile). Existing rows keep their
    /// precedence: row order is preserved, so a tag already named earlier is
    /// still resolved by that earlier row.
    pub fn with_row(&self, row: ProfileRow) -> Self {
        let mut rows = self.inner.rows.clone();
        rows.push(row);
        Self::indexed(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_profile_parses_and_is_large() {
        let profile = SecurityProfile::basic();
        assert!(
            profile.rows().len() > 600,
            "the PS3.15 table has ~655 rows, got {}",
            profile.rows().len()
        );
    }

    #[test]
    fn a_literal_tag_matches_and_gets_its_action() {
        let profile = SecurityProfile::basic();
        // (0008,0050) AccessionNumber is Z (blank) under the basic profile
        assert_eq!(
            profile.action_for("0008,0050", AnonOption::BasicProfile),
            Some(Action::ZeroLength)
        );
    }

    #[test]
    fn a_row_can_be_added_to_an_existing_profile() {
        let profile = SecurityProfile::parse("0008,0050;Z;;;;;;;;;;").unwrap();
        assert_eq!(
            profile.action_for("0010,0010", AnonOption::BasicProfile),
            None
        );

        let mut actions = [None; 11];
        actions[AnonOption::BasicProfile as usize] = Some(Action::Remove);
        let extended = profile.with_row(ProfileRow {
            pattern: TagPattern::new("0010,0010").unwrap(),
            actions,
        });

        assert_eq!(
            extended.action_for("0010,0010", AnonOption::BasicProfile),
            Some(Action::Remove)
        );
        assert_eq!(
            profile.action_for("0010,0010", AnonOption::BasicProfile),
            None,
            "original unchanged"
        );
        assert_eq!(
            extended.action_for("0008,0050", AnonOption::BasicProfile),
            Some(Action::ZeroLength),
            "existing rows survive"
        );
    }

    #[test]
    fn a_class_pattern_matches_curve_tags() {
        let profile = SecurityProfile::basic();
        // 50xx,xxxx curve group is X (remove) under basic profile
        assert_eq!(
            profile.action_for("5000,0100", AnonOption::BasicProfile),
            Some(Action::Remove)
        );
        assert_eq!(
            profile.action_for("5010,ABCD", AnonOption::BasicProfile),
            Some(Action::Remove)
        );
    }

    #[test]
    fn the_odd_private_group_pattern_matches() {
        let profile = SecurityProfile::basic();
        // odd group + odd element (private) is X under basic profile
        assert_eq!(
            profile.action_for("0009,0001", AnonOption::BasicProfile),
            Some(Action::Remove)
        );
        // an even group does not match that row
        assert_ne!(
            profile.action_for("0008,0001", AnonOption::BasicProfile),
            Some(Action::Remove)
        );
    }

    #[test]
    fn an_option_skips_rows_that_do_not_take_part_in_it() {
        // The leading row (`[0-9A-F]{3}[13579BDF],...` → X under BasicProfile,
        // C under RetainSafePrivate) matches every odd private tag, but it has
        // no CleanGraph cell; the graph rows below it must still be reachable
        // (fo-dicom: one table per option, rows with an empty cell are absent).
        let profile = SecurityProfile::basic();
        assert_eq!(
            profile.action_for("5011,1234", AnonOption::CleanGraph),
            Some(Action::Clean)
        );
        assert_eq!(
            profile.action_for("6011,3000", AnonOption::CleanGraph),
            Some(Action::Clean)
        );
        assert_eq!(
            profile.action_for("50FF,0001", AnonOption::CleanGraph),
            Some(Action::Clean)
        );

        // ... while the options the leading row does take part in are unchanged
        assert_eq!(
            profile.action_for("5011,1234", AnonOption::BasicProfile),
            Some(Action::Remove)
        );
        assert_eq!(
            profile.action_for("5011,1234", AnonOption::RetainSafePrivate),
            Some(Action::Clean)
        );

        // a tag no row covers under the option still has no action
        assert_eq!(
            profile.action_for("0010,0010", AnonOption::CleanGraph),
            None
        );
    }

    #[test]
    fn a_pattern_compiles_literals_classes_and_repetition() {
        let pattern = TagPattern::new("50[0-9A-F]{2},[0-9A-F]{4}").unwrap();
        assert!(pattern.matches("50AB,1234"));
        assert!(pattern.matches("50ab,1234"), "matching is case-insensitive");
        assert!(
            !pattern.matches("60AB,1234"),
            "the first literal must match"
        );
        assert!(!pattern.matches("50AB,12345"), "length must match exactly");
    }

    #[test]
    fn the_builtin_profile_is_parsed_once_and_shared() {
        let first = SecurityProfile::basic();
        let second = SecurityProfile::basic();
        assert!(
            Arc::ptr_eq(&first.inner, &second.inner),
            "basic() shares one parsed table"
        );

        // the two handles describe the same table
        assert_eq!(first.rows().len(), second.rows().len());
        for (left, right) in first.rows().iter().zip(second.rows()) {
            assert_eq!(left.pattern.text(), right.pattern.text());
            assert_eq!(left.actions, right.actions);
        }

        // a custom profile stays its own table and does not disturb the cached one
        let custom = SecurityProfile::parse("0010,0010;D\n").unwrap();
        assert_eq!(custom.rows().len(), 1);
        assert_eq!(
            custom.action_for("0010,0010", AnonOption::BasicProfile),
            Some(Action::Dummy)
        );
        assert_eq!(
            SecurityProfile::basic().action_for("0010,0010", AnonOption::BasicProfile),
            Some(Action::ZeroLength),
            "the built-in row is unchanged"
        );
    }

    /// A tag string that matches `pattern`: literals as they are, the first
    /// character of each class.
    fn sample_tag(pattern: &TagPattern) -> String {
        pattern
            .tokens
            .iter()
            .map(|token| match token {
                Token::Literal(ch) => *ch,
                Token::Class(set) => set[0],
            })
            .collect()
    }

    #[test]
    fn the_exact_tag_index_agrees_with_the_linear_scan() {
        // every literal row's tag, every wildcard row's sample tag, plus tags the
        // table does not mention and a few edge shapes
        let profile = SecurityProfile::basic();
        let mut probes: Vec<String> = Vec::new();
        for row in profile.rows() {
            match tag_key(row.pattern.text()) {
                Some(key) => probes.push(format!("{:04X},{:04X}", key >> 16, key & 0xFFFF)),
                None => probes.push(sample_tag(&row.pattern)),
            }
        }
        probes.extend(
            [
                "0008,0060",
                "0008,0001",
                "0009,0001",
                "5011,1234",
                "60FF,3000",
                "0070,0001",
                "FFFF,FFFF",
            ]
            .map(String::from),
        );

        for option in AnonOption::ALL {
            for tag in &probes {
                assert_eq!(
                    profile.action_for(tag, option),
                    profile.linear_action_for(tag, option),
                    "index and scan disagree for {tag} under {option:?}"
                );
            }
        }
    }

    #[test]
    fn typed_tag_lookup_matches_the_textual_lookup() {
        let profile = SecurityProfile::basic();
        let probes = [
            Tag::PATIENT_NAME,
            Tag::PATIENT_ID,
            Tag::SOP_INSTANCE_UID,
            Tag::new(0x0009, 0x0001),
            Tag::new(0x60FF, 0x3000),
            Tag::new(0xFFFF, 0xFFFF),
        ];
        for option in AnonOption::ALL {
            for tag in &probes {
                let text = format!("{:04X},{:04X}", tag.group(), tag.element());
                assert_eq!(
                    profile.action_for_tag(tag, option),
                    profile.action_for(&text, option),
                    "typed and textual disagree for {tag:?} under {option:?}"
                );
            }
        }
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
        assert_eq!(
            Action::parse("X/Z"),
            Some(Action::Remove),
            "conditional cells take the first letter"
        );
        assert_eq!(Action::parse("Z/D"), Some(Action::ZeroLength));
        assert_eq!(Action::parse("X/Z/U*"), Some(Action::Remove));
        assert_eq!(Action::parse(""), None);
        assert_eq!(Action::parse("?"), None);
    }
    #[test]
    fn malformed_patterns_and_blank_rows_are_handled() {
        assert!(TagPattern::new("(0010,0010{").is_err());
        // a row with no tag pattern is skipped, a comment is ignored
        let profile = SecurityProfile::parse(";Z\n# note\n0010,0010;Z\n").unwrap();
        // an unparseable tag never matches anything
        assert_eq!(
            profile.action_for("not-a-tag", AnonOption::BasicProfile),
            None
        );
        assert_eq!(
            profile.action_for("0010,0010", AnonOption::BasicProfile),
            Some(Action::ZeroLength)
        );
    }
}
