//! Match and transform rule DSLs (MATCH-01, XFORM-01; docs/01-feature-list.md).
//!
//! - [`match_rule`]: 14-rule dataset predicate DSL for query filtering.
//! - [`transform`]: 16-rule dataset mutation DSL, with optional match conditions
//!   and recording of original values into a Modified Attributes Sequence item.

pub mod match_rule;
pub mod transform;

pub use match_rule::{dicom_wildcard_match, MatchOperator, MatchRule, MatchRuleSet};
pub use transform::{TransformRule, TransformRuleSet, TrimPosition};
