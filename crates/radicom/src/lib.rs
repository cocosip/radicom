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
    use std::path::{Path, PathBuf};

    /// Files whose job is to *define* UID constants. Everything else must
    /// reference them; the generated tables are skipped wholesale.
    const DEFINITION_SITES: &[&str] = &[
        "crates/radicom/src/model/uid.rs",
        "crates/radicom/src/model/transfer_syntax.rs",
        "crates/radicom-network/src/association.rs",
        "crates/radicom-network/src/pdu.rs",
        "crates/radicom-network/src/print_sop.rs",
    ];

    /// FEAT-26 guard: production code in every crate must use the generated
    /// `crate::uids` constants, not hard-coded `1.2.840.10008…` strings (which
    /// drift and are invisible to UID-based tooling).
    ///
    /// The whole workspace is scanned, so the rule holds outside the crate that
    /// owns the constants. Test modules are allowed to spell UIDs out; only the
    /// text before the first `#[cfg(test)]` is inspected.
    #[test]
    fn production_sources_do_not_hard_code_dicom_uids() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace: PathBuf = manifest
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf();

        let mut scanned = 0usize;
        if !workspace.join("crates").is_dir() {
            // Running from a packaged crate: the sibling sources are not part of
            // the tarball, so there is nothing to scan.
            return;
        }
        for krate in ["radicom", "radicom-dict", "radicom-json", "radicom-network"] {
            let src = workspace.join("crates").join(krate).join("src");
            for path in rust_sources(&src) {
                let relative = path
                    .strip_prefix(&workspace)
                    .expect("inside the workspace")
                    .to_string_lossy()
                    .replace('\\', "/");
                if relative.contains("/generated/") || DEFINITION_SITES.contains(&&*relative) {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("read source");
                let production = match source.find("#[cfg(test)]") {
                    Some(tests) => &source[..tests],
                    None => &source[..],
                };
                for (index, line) in production.lines().enumerate() {
                    assert!(
                        !line.contains("\"1.2.840.10008"),
                        "{relative}:{} hard-codes a DICOM UID; use the uids:: constants: {line}",
                        index + 1
                    );
                }
                scanned += 1;
            }
        }
        assert!(scanned > 50, "the guard scanned only {scanned} files");
    }

    /// Every `.rs` file below `dir`, recursively.
    fn rust_sources(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&next) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        out
    }
}
