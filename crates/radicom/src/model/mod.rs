//! Data model: tags, VRs, elements, datasets, UIDs, transfer syntaxes and
//! dictionaries (docs/04-data-model.md).

pub mod buffer;
pub mod dicom_datetime;
pub mod dictionary;
pub mod masked_tag;
pub mod endian;
pub mod private_creator;
pub mod range;
pub mod tag;
pub mod tag_index;
pub mod transfer_syntax;
pub mod uid;
pub mod vm;
pub mod vr;
