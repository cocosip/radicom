//! Match and transform rule DSLs.
//!
//! - [`match_rule`]: 11-rule dataset predicate DSL for query filtering (12 with
//!   the `regex-rules` feature).
//! - [`transform`]: 14-rule dataset mutation DSL, with optional match conditions
//!   and recording of original values into a Modified Attributes Sequence item
//!   (15 with the `regex-rules` feature).

pub mod match_rule;
pub mod transform;

pub use match_rule::{dicom_wildcard_match, MatchOperator, MatchRule, MatchRuleSet};
pub use transform::{TransformRule, TransformRuleSet, TrimPosition};
