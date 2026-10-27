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

pub mod encoding;
pub mod dump;
pub mod error;
pub mod generated;
pub mod io;
pub mod logging;
pub mod media;
pub mod model;
pub mod pixel;
pub mod print;
#[cfg(feature = "render")]
pub mod render;
pub mod rules;
pub mod sr;
pub mod validation;

pub mod anonymizer;

pub use error::{DataError, Error, FileError, ReaderError, ValidationFailure};
pub use logging::LogLevel;
pub use model::dictionary::{
    default_dictionary, load_xml, set_default_dictionary, Dictionary, DictionaryEntry, RuntimeEntry,
};
pub use model::dataset::{Dataset, DatasetWalker};
pub use model::dicom_datetime::DtPoint;
pub use model::element::DicomElement;
pub use model::item::{DicomFragmentSequence, DicomItem, DicomSequence};
pub use model::endian::Endian;
pub use model::person_name::PersonName;
pub use model::masked_tag::MaskedTag;
pub use model::tag::Tag;
pub use model::transfer_syntax::{self as transfer_syntax_registry, TransferSyntax};
pub use model::uid::{StorageCategory, Uid, UidGenerator, UidType};
pub use model::vm::Vm;
pub use model::vr::Vr;
