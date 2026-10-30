//! Structured Reports (SR-01/02; docs/01-architecture.md §12).
//!
//! An SR is a tree of content items over a `DicomFile`. This module builds and
//! reads that tree with the value type checked at construction time, and
//! provides the reusable `CodeItem` support type plus `StructuredReport`
//! document open/save.

pub mod code_item;
pub mod content_item;
pub mod structured_report;

pub use code_item::CodeItem;
pub use content_item::{
    ContentItem, ContentValue, GraphicType, GraphicType3D, MeasuredValue, ReferencedSop,
    Relationship, SpatialCoordinates, SpatialCoordinates3D, TemporalCoordinates, TemporalRange,
    TemporalRangeType, ValueType,
};
pub use structured_report::StructuredReport;
