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

pub mod dump;
pub mod encoding;
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
pub use model::dataset::{Dataset, DatasetWalker};
pub use model::dicom_datetime::DtPoint;
pub use model::dictionary::{
    default_dictionary, load_xml, set_default_dictionary, Dictionary, DictionaryEntry, RuntimeEntry,
};
pub use model::element::DicomElement;
pub use model::endian::Endian;
pub use model::item::{DicomFragmentSequence, DicomItem, DicomSequence};
pub use model::masked_tag::MaskedTag;
pub use model::person_name::PersonName;
pub use model::tag::Tag;
pub use model::transfer_syntax::{self as transfer_syntax_registry, TransferSyntax};
pub use model::uid::{StorageCategory, Uid, UidGenerator, UidType};
pub use model::vm::Vm;
pub use model::vr::Vr;

/// Generated DICOM UID constants: `uids::CT_IMAGE_STORAGE`.
///
/// These are `&'static str`, so they can be passed straight to
/// [`Uid::parse`] or any API taking an implementation of `Into<String>`.
///
/// Tag constants are inherent associated constants on [`Tag`]
/// (`Tag::PATIENT_NAME`, `Tag::ROWS`, ...) and need no import.
pub mod uids {
    pub use radicom_dict::generated::uids::consts::*;
}

/// Built-in transfer syntaxes: `syntaxes::EXPLICIT_VR_LITTLE_ENDIAN`.
pub mod syntaxes {
    pub use crate::model::transfer_syntax::syntaxes::*;
}

#[cfg(test)]
mod uid_literals {
    /// FEAT-26 guard: production code must use the generated `crate::uids`
    /// constants, not hard-coded `1.2.840.10008…` strings (which drift and are
    /// invisible to UID-based tooling). Test modules are allowed to spell tags
    /// and UIDs out.
    #[test]
    fn production_sources_do_not_hard_code_dicom_uids() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for relative in ["render/mod.rs", "media/mod.rs", "sr/structured_report.rs"] {
            let path = src.join(relative);
            let source = std::fs::read_to_string(&path).expect("read source");
            let production = match source.find("#[cfg(test)]") {
                Some(tests) => &source[..tests],
                None => &source[..],
            };
            for (index, line) in production.lines().enumerate() {
                assert!(
                    !line.contains("\"1.2.840.10008"),
                    "{relative}:{} hard-codes a DICOM UID; use crate::uids: {line}",
                    index + 1
                );
            }
        }
    }
}
