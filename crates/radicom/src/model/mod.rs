//! Data model: tags, VRs, elements, datasets, UIDs, transfer syntaxes and
//! dictionaries (docs/01-architecture.md §3).

pub mod buffer;
pub mod dataset;
pub mod dicom_datetime;
pub mod dictionary;
pub mod element;
pub mod endian;
pub mod item;
pub mod masked_tag;
pub mod person_name;
pub mod private_creator;
pub mod range;
pub mod tag;
pub mod tag_index;
pub mod transfer_syntax;
pub mod uid;
pub mod vm;
pub mod vr;
