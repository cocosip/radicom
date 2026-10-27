//! radicom — a DICOM processing library for Rust.
//!
//! Modeled on fo-dicom (Fellow Oak DICOM). See `docs/` for the design documents.
//!
//! Crate layout (workspace):
//! - `radicom` (this crate): data model, file IO, pixel data facade, codec
//!   interfaces and transcoder, media/SR/printing/anonymizer data models.
//! - `radicom-dict`: generated DICOM dictionary tables (tags, UIDs, entries).
//! - `radicom-network`: DIMSE network communication (tokio based).
//! - `radicom-json`: DICOM JSON (PS3.18) serialization.
//!
//! Image codec implementations live in the separate `radicom-codecs` project;
//! this crate only defines the [`pixel::codec`] interfaces and registry.

pub mod error;
pub mod model;

pub use error::{DataError, Error, FileError, ReaderError, ValidationFailure};
pub use model::tag::Tag;
pub use model::vr::Vr;
