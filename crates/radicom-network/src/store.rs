//! C-STORE receive path (SRV-03; docs/01-architecture.md §11).
//!
//! The receiver streams the inbound dataset PDVs into a temporary file (H3,
//! fo-dicom `DicomService.CreateCStoreReceiveStream`) and parses that file into
//! a [`DicomFile`] only once the last fragment arrived. [`StoreSink::Memory`]
//! parses and deletes the temp file; [`StoreSink::SpoolToTemp`] keeps the
//! received file as the instance's spool; [`StoreSink::SpoolToDir`] keeps a copy
//! under the configured directory.
//!
//! **Ceiling:** the *receive* phase is bounded by the PDU size
//! ([`crate::assembler::SpoolConfig`]) instead of the instance size. Spool sinks
//! parse through the core reader's file windows, matching fo-dicom's temporary
//! receive stream without reading the entire instance back into memory.

use crate::assembler::MessageBody;
use crate::dimse::{status, Command, CommandField};
use crate::error::{NetworkError, Result};
use crate::services::{CStoreRequest, CStoreResponse};
use radicom::io::byte_source::ByteSourceReader;
use radicom::io::dataset_builder::DatasetBuilder;
use radicom::io::file::DicomFile;
use radicom::io::reader::{DicomReader, LargeValuePolicy};
use radicom::io::temp::create_unique_file;
use radicom::model::dataset::Dataset;
use radicom::model::transfer_syntax::TransferSyntax;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where an incoming instance is put before the provider sees it (§11.2).
///
/// The default parses the already-buffered bytes directly: the C-STORE receiver
/// has the whole dataset in memory before the sink is consulted, so writing a
/// temporary file only to parse the same bytes back doubles the work without
/// bounding memory (audit P-3/§3.1).
#[derive(Debug, Clone, Default)]
pub enum StoreSink {
    /// Spool to a temporary file that is removed once the instance is done
    /// with. Kept for callers that want an on-disk artifact during processing.
    SpoolToTemp,
    /// Spool into a named directory, keeping the file afterwards.
    SpoolToDir(PathBuf),
    /// Keep the bytes in memory and parse them directly (the default).
    #[default]
    Memory,
}

/// A received instance plus the spooled path when one was used.
#[derive(Debug)]
pub struct ReceivedInstance {
    pub file: DicomFile,
    /// The spooled file, when the sink kept it (only [`StoreSink::SpoolToDir`]).
    pub path: Option<PathBuf>,
    /// A temporary spool file that must not outlive the instance
    /// ([`StoreSink::SpoolToTemp`]). Drop-only: it is never read.
    #[allow(dead_code)]
    temp: Option<TempSpool>,
}

impl ReceivedInstance {
    /// An instance that owns no spool file.
    pub fn from_file(file: DicomFile) -> Self {
        ReceivedInstance {
            file,
            path: None,
            temp: None,
        }
    }
}

/// Removes its spool file on drop, so no code path can leak it.
#[derive(Debug)]
struct TempSpool(PathBuf);

impl Drop for TempSpool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl StoreSink {
    /// Materializes the instance from an already-received body (H3).
    ///
    /// A body that was streamed to a temporary file is parsed from that file,
    /// and the sink decides whether the file survives: `Memory` deletes it,
    /// `SpoolToTemp` adopts it (no second copy), `SpoolToDir` keeps a copy under
    /// the directory as before.
    pub fn materialize_body(
        &self,
        body: &MessageBody,
        syntax: &Arc<TransferSyntax>,
    ) -> Result<ReceivedInstance> {
        self.materialize_body_with_fallback(body, syntax, None)
    }

    /// Materializes the instance, decoding text with no (0008,0005) under
    /// `fallback` (H1, a DICOM Defined Term such as `"ISO_IR 100"`).
    pub fn materialize_body_with_fallback(
        &self,
        body: &MessageBody,
        syntax: &Arc<TransferSyntax>,
        fallback: Option<&str>,
    ) -> Result<ReceivedInstance> {
        self.materialize_body_with_fallback_limited(
            body,
            syntax,
            fallback,
            body.spool().map_or(
                crate::assembler::SpoolConfig::DEFAULT_MAX_SPILLED_LENGTH,
                |spool| spool.max_message_length(),
            ),
        )
    }

    /// Materializes a received body with a hard cap on decoded dataset bytes.
    pub fn materialize_body_with_fallback_limited(
        &self,
        body: &MessageBody,
        syntax: &Arc<TransferSyntax>,
        fallback: Option<&str>,
        max_materialized_length: u64,
    ) -> Result<ReceivedInstance> {
        let MessageBody::Spilled(spool) = body else {
            let bytes = body.in_memory().expect("a non-spilled body is in memory");
            return self.materialize_with_fallback_limited(
                bytes,
                syntax,
                fallback,
                max_materialized_length,
            );
        };
        let max_materialized_length = max_materialized_length.min(spool.max_message_length());
        if spool.len() > max_materialized_length {
            return Err(NetworkError::protocol(
                "DIMSE dataset exceeds the maximum size",
            ));
        }
        match self {
            StoreSink::Memory => {
                let file =
                    file_from_spooled_file(spool, syntax, fallback, max_materialized_length)?;
                spool.keep();
                Ok(ReceivedInstance {
                    file,
                    path: None,
                    temp: None,
                })
            }
            StoreSink::SpoolToTemp => {
                let file =
                    file_from_spooled_file(spool, syntax, fallback, max_materialized_length)?;
                spool.keep();
                Ok(ReceivedInstance {
                    file,
                    path: None,
                    temp: None,
                })
            }
            StoreSink::SpoolToDir(dir) => {
                std::fs::create_dir_all(dir).map_err(NetworkError::Io)?;
                let (path, mut file) =
                    create_unique_file(dir, "radicom-store", "dcm").map_err(NetworkError::Io)?;
                let mut source = std::fs::File::open(spool.path()).map_err(NetworkError::Io)?;
                std::io::copy(&mut source, &mut file).map_err(NetworkError::Io)?;
                file.flush().map_err(NetworkError::Io)?;
                drop(file);
                let dataset =
                    match decode_spooled_file(&path, syntax, fallback, max_materialized_length) {
                        Ok(dataset) => dataset,
                        Err(error) => {
                            let _ = std::fs::remove_file(&path);
                            return Err(error);
                        }
                    };
                Ok(ReceivedInstance {
                    file: DicomFile::from_dataset(dataset),
                    path: Some(path),
                    temp: None,
                })
            }
        }
    }

    /// Materializes the instance from the dataset bytes.
    pub fn materialize(
        &self,
        bytes: &[u8],
        syntax: &Arc<TransferSyntax>,
    ) -> Result<ReceivedInstance> {
        self.materialize_with_fallback(bytes, syntax, None)
    }

    /// Materializes the instance from the dataset bytes, decoding text with no
    /// (0008,0005) under `fallback` (H1).
    pub fn materialize_with_fallback(
        &self,
        bytes: &[u8],
        syntax: &Arc<TransferSyntax>,
        fallback: Option<&str>,
    ) -> Result<ReceivedInstance> {
        self.materialize_with_fallback_limited(
            bytes,
            syntax,
            fallback,
            crate::assembler::SpoolConfig::DEFAULT_MAX_SPILLED_LENGTH,
        )
    }

    fn materialize_with_fallback_limited(
        &self,
        bytes: &[u8],
        syntax: &Arc<TransferSyntax>,
        fallback: Option<&str>,
        max_materialized_length: u64,
    ) -> Result<ReceivedInstance> {
        match self {
            StoreSink::Memory => {
                // PS3.7 §9.3: the dataset is encoded in the negotiated transfer
                // syntax and carries no file meta, so it must be parsed *with*
                // that syntax instead of being sniffed (NET-24)
                let dataset = decode_limited(bytes, syntax, fallback, max_materialized_length)?;
                Ok(ReceivedInstance::from_file(DicomFile::from_dataset(
                    dataset,
                )))
            }
            StoreSink::SpoolToTemp => {
                // `create_unique_file` is `create_new` + a random name + 0600 on
                // Unix, so a symlink planted in the shared temp directory is
                // never followed and the spooled PHI is not world-readable
                // (CORE-17, the same rule the FileWindow spill uses).
                let (path, file) =
                    create_unique_file(&std::env::temp_dir(), "radicom-store", "dcm")
                        .map_err(NetworkError::Io)?;
                // the guard removes the file on every error path below
                let temp = TempSpool(path.clone());
                let file = write_and_parse(
                    &path,
                    file,
                    bytes,
                    syntax,
                    fallback,
                    max_materialized_length,
                )?;
                // the instance is parsed from memory, so the spool file has no
                // reader: the guard deletes it as soon as the instance is done
                Ok(ReceivedInstance {
                    file,
                    path: None,
                    temp: Some(temp),
                })
            }
            StoreSink::SpoolToDir(dir) => {
                std::fs::create_dir_all(dir).map_err(NetworkError::Io)?;
                // same rule: a random name plus `create_new` under the operator's
                // directory, so a planted path cannot be followed
                let (path, file) =
                    create_unique_file(dir, "radicom-store", "dcm").map_err(NetworkError::Io)?;
                write_and_parse(
                    &path,
                    file,
                    bytes,
                    syntax,
                    fallback,
                    max_materialized_length,
                )
                .map(|file| ReceivedInstance {
                    file,
                    path: Some(path),
                    temp: None,
                })
            }
        }
    }
}

/// Parses a bare dataset directly from a spool file using lazy value windows.
fn decode_spooled_file(
    path: &Path,
    syntax: &Arc<TransferSyntax>,
    fallback: Option<&str>,
    max_materialized_length: u64,
) -> Result<Dataset> {
    if syntax.is_deflate() {
        // Deflated transfer syntax needs an inflated byte stream before the
        // existing dataset parser can run. Stream the compressed file instead
        // of first materialising a second compressed copy in memory.
        let mut file = std::fs::File::open(path).map_err(NetworkError::Io)?;
        let bytes =
            radicom::io::reader::inflate_stream_limited(&mut file, 0, max_materialized_length)
                .map_err(|error| {
                    NetworkError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        error.to_string(),
                    ))
                })?;
        let mut dataset = decode(
            &bytes,
            &radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN,
            fallback,
        )?;
        dataset.set_transfer_syntax(syntax.clone());
        return Ok(dataset);
    }
    let file = std::fs::File::open(path).map_err(NetworkError::Io)?;
    let shared = Arc::new(file.try_clone().map_err(NetworkError::Io)?);
    let mut source =
        ByteSourceReader::new(BufReader::new(file), syntax.endian()).with_shared_file(shared);
    let fallback = fallback
        .filter(|term| radicom::encoding::charset::is_known_charset(term))
        .map(radicom::encoding::charset::charset_encoding);
    let fallbacks = fallback.into_iter().collect();
    let mut builder = DatasetBuilder::with_transfer_syntax_and_fallback(syntax.clone(), fallbacks);
    DicomReader {
        large_object_size: 64 * 1024,
        large_policy: LargeValuePolicy::OnDemand,
        partial_on_eof: false,
        ..DicomReader::default()
    }
    .read(&mut source, syntax, &mut builder, None)
    .map_err(|error| {
        NetworkError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        ))
    })?;
    Ok(builder.into_dataset())
}

fn file_from_spooled_file(
    spool: &crate::assembler::SpoolFile,
    syntax: &Arc<TransferSyntax>,
    fallback: Option<&str>,
    max_materialized_length: u64,
) -> Result<DicomFile> {
    let dataset = decode_spooled_file(spool.path(), syntax, fallback, max_materialized_length)?;
    let owner = radicom::model::buffer::TempFileSpill::from_temp_file(
        spool.path().to_path_buf(),
        spool.len(),
    )
    .map_err(NetworkError::Io)?;
    Ok(DicomFile::from_dataset_with_spool(dataset, Arc::new(owner)))
}

/// Writes the dataset through the already-open handle, then parses it.
///
/// The handle comes from [`create_unique_file`], so this must not re-open the
/// path: a second open could follow a swapped-in path and would defeat the
/// `create_new` guarantee.
fn write_and_parse(
    path: &Path,
    mut file: std::fs::File,
    bytes: &[u8],
    syntax: &Arc<TransferSyntax>,
    fallback: Option<&str>,
    max_materialized_length: u64,
) -> Result<DicomFile> {
    file.write_all(bytes).map_err(NetworkError::Io)?;
    file.flush().map_err(NetworkError::Io)?;
    drop(file);

    // the spooled bytes are a bare dataset: parse them with the negotiated
    // syntax, never by sniffing (NET-24)
    match decode_limited(bytes, syntax, fallback, max_materialized_length) {
        Ok(dataset) => Ok(DicomFile::from_dataset(dataset)),
        Err(e) => {
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

/// Parses a received bare dataset, applying the configured fallback charset
/// when it carries no (0008,0005) (H1).
fn decode(
    bytes: &[u8],
    syntax: &Arc<TransferSyntax>,
    fallback: Option<&str>,
) -> Result<radicom::model::dataset::Dataset> {
    crate::message::decode_dataset_with_fallback(bytes, syntax, fallback)
}

/// Reads and decodes a received body with a hard bound on inflated output.
///
/// This performs blocking file IO and parsing; async callers must offload it.
pub fn decode_body_with_fallback_limited(
    body: &MessageBody,
    syntax: &Arc<TransferSyntax>,
    fallback: Option<&str>,
    max_materialized_length: u64,
) -> Result<Dataset> {
    let limit = body.spool().map_or(max_materialized_length, |spool| {
        max_materialized_length.min(spool.max_message_length())
    });
    if body.len() > limit {
        return Err(NetworkError::protocol(
            "DIMSE dataset exceeds the maximum size",
        ));
    }
    let bytes = body.read_all()?;
    decode_limited(&bytes, syntax, fallback, limit)
}

fn decode_limited(
    bytes: &[u8],
    syntax: &Arc<TransferSyntax>,
    fallback: Option<&str>,
    max_materialized_length: u64,
) -> Result<Dataset> {
    if bytes.len() as u64 > max_materialized_length {
        return Err(NetworkError::protocol(
            "DIMSE dataset exceeds the maximum size",
        ));
    }
    if !syntax.is_deflate() {
        return decode(bytes, syntax, fallback);
    }
    let bytes = radicom::io::reader::inflate_stream_limited(
        &mut std::io::Cursor::new(bytes),
        0,
        max_materialized_length,
    )
    .map_err(|error| {
        NetworkError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        ))
    })?;
    let mut dataset = decode(
        &bytes,
        &radicom::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN,
        fallback,
    )?;
    dataset.set_transfer_syntax(syntax.clone());
    Ok(dataset)
}

/// The status for a dataset that disagrees with its C-STORE command (§11.4).
pub const DATASET_MISMATCH: u16 = status::STORAGE_DATA_SET_DOES_NOT_MATCH_SOP_CLASS;

/// Checks the received dataset against the C-STORE command (§11.4, NET-23).
///
/// PS3.4 §F.1 requires the SOP Class UID and SOP Instance UID in the dataset to
/// equal the ones in the command. A peer that disagrees gets 0xA900 and the
/// connection stays usable; a dataset that simply omits them is left to the
/// provider (some peers omit UIDs in the dataset of an encapsulated object).
pub fn dataset_mismatch(request: &CStoreRequest) -> Option<String> {
    let dataset = &request.file.dataset;
    if let Some(class) = dataset.try_get_str_ref(radicom::model::tag::Tag::SOP_CLASS_UID) {
        let class = class.trim();
        if class != request.affected_sop_class_uid.trim() {
            return Some(format!(
                "dataset SOP class {class} does not match the command's {}",
                request.affected_sop_class_uid
            ));
        }
    }
    if let Some(instance) = dataset.try_get_str_ref(radicom::model::tag::Tag::SOP_INSTANCE_UID) {
        let instance = instance.trim();
        if instance != request.affected_sop_instance_uid.trim() {
            return Some(format!(
                "dataset SOP instance {instance} does not match the command's {}",
                request.affected_sop_instance_uid
            ));
        }
    }
    None
}

/// Builds the [`CStoreRequest`] from a received command and its dataset (§11.4).
pub fn c_store_request_from_message(
    command: &Command,
    instance: ReceivedInstance,
) -> Result<CStoreRequest> {
    if command.field != CommandField::CStoreRq {
        return Err(NetworkError::protocol("not a C-STORE request"));
    }
    let affected_sop_class_uid =
        command
            .affected_sop_class_uid
            .clone()
            .ok_or(NetworkError::protocol(
                "C-STORE request has no affected SOP class UID",
            ))?;
    let affected_sop_instance_uid =
        command
            .affected_sop_instance_uid
            .clone()
            .ok_or(NetworkError::protocol(
                "C-STORE request has no affected SOP instance UID",
            ))?;
    Ok(CStoreRequest {
        message_id: command.message_id,
        affected_sop_class_uid,
        affected_sop_instance_uid,
        priority: command.priority,
        file: instance.file,
        move_originator_ae_title: command.move_originator_ae_title.clone(),
        move_originator_message_id: command.move_originator_message_id,
    })
}

/// Maps a provider failure onto a C-STORE response (§11.4).
///
/// The connection survives: a rejected instance becomes a failure status with
/// a truncated comment, never a disconnect.
pub fn failure_response(request: &CStoreRequest, error: &NetworkError) -> CStoreResponse {
    let code = match error {
        // the provider cannot store this SOP class at all: "Refused: SOP class
        // not supported" (PS3.4 Table B.2-1), not a dataset/command mismatch
        // (NET-18)
        NetworkError::SopClassNotSupported { .. } => status::SOP_CLASS_NOT_SUPPORTED,
        NetworkError::Io(_) => status::STORAGE_FAILURE,
        _ => status::PROCESSING_FAILURE,
    };
    CStoreResponse::failure(request, code, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use radicom::model::dataset::Dataset;
    use radicom::model::tag::Tag;
    use radicom::model::transfer_syntax::syntaxes;
    use radicom::model::vr::Vr;

    fn dataset_bytes(syntax: &TransferSyntax) -> Vec<u8> {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        crate::message::encode_dataset(&ds, syntax).unwrap()
    }

    fn c_store_command() -> Command {
        let mut command = Command::request(CommandField::CStoreRq, 11);
        command.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.7".into());
        command.affected_sop_instance_uid = Some("1.2.3.4.5".into());
        command.no_dataset = false;
        command
    }

    #[test]
    fn default_sink_parses_in_memory_instead_of_spooling() {
        // The C-STORE receiver already holds the bytes; the shipped default
        // must not write a temporary file only to parse the same bytes back.
        assert!(matches!(StoreSink::default(), StoreSink::Memory));
    }

    #[test]
    fn memory_sink_parses_the_instance() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let bytes = dataset_bytes(&syntax);
        let instance = StoreSink::Memory.materialize(&bytes, &syntax).unwrap();
        assert_eq!(
            instance.file.dataset.get_str(Tag::PATIENT_NAME).unwrap(),
            "Doe^John"
        );
        assert_eq!(instance.file.dataset.transfer_syntax().uid(), syntax.uid());
        assert!(instance.path.is_none(), "no file kept");
    }

    #[test]
    fn spool_to_temp_removes_its_file_with_the_instance() {
        let syntax = syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone();
        let bytes = dataset_bytes(&syntax);
        let instance = StoreSink::SpoolToTemp.materialize(&bytes, &syntax).unwrap();
        assert_eq!(
            instance
                .file
                .dataset
                .get_str(Tag::SOP_INSTANCE_UID)
                .unwrap(),
            "1.2.3.4.5"
        );
        assert!(
            instance.path.is_none(),
            "temporary spool files are not reported"
        );
        let spooled = instance.temp.as_ref().expect("a temp spool file").0.clone();
        assert!(spooled.exists(), "the instance was spooled to disk");
        drop(instance);
        assert!(
            !spooled.exists(),
            "the temp spool file must not outlive the instance"
        );
    }

    #[test]
    fn a_spooled_file_survives_until_the_c_store_request_is_dropped() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5");
        dataset.insert_value(Tag::PIXEL_DATA, vec![0u8; 128 * 1024]);
        let bytes = crate::message::encode_dataset(&dataset, &syntax).unwrap();
        let dir = std::env::temp_dir().join(format!("radicom-store-owner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut assembler =
            crate::assembler::PdvAssembler::with_spool(crate::assembler::SpoolConfig {
                memory_threshold: 0,
                dir: Some(dir.clone()),
                ..crate::assembler::SpoolConfig::default()
            });
        assembler.arm_spill(1);
        let unit = assembler
            .push_pdv(&crate::pdu::Pdv {
                context_id: 1,
                is_command: false,
                is_last: true,
                data: bytes::Bytes::from(bytes),
            })
            .unwrap()
            .expect("the dataset completes");
        let body = unit.body();
        let path = body.spool().unwrap().path().to_path_buf();
        let instance = StoreSink::SpoolToTemp
            .materialize_body(&body, &syntax)
            .unwrap();
        let request = c_store_request_from_message(&c_store_command(), instance).unwrap();
        assert!(path.exists(), "the request still owns its spool file");
        drop(request);
        assert!(
            !path.exists(),
            "the spool is removed after the request drops"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_spooled_instance_gets_a_private_file_with_a_fresh_name() {
        // CORE-17: the spool used `File::create` on a clock+counter name in the
        // shared temp directory, so a pre-planted symlink was followed and the
        // PHI landed world-readable. It now goes through the same
        // `create_unique_file` helper as the FileWindow spill.
        let syntax = syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone();
        let bytes = dataset_bytes(&syntax);

        let first = StoreSink::SpoolToTemp.materialize(&bytes, &syntax).unwrap();
        let first_path = first.temp.as_ref().expect("a temp spool file").0.clone();
        let name = first_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            name.starts_with("radicom-store-") && name.ends_with(".dcm"),
            "unexpected spool name {name}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&first_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "spooled PHI must not be world-readable");
        }

        let second = StoreSink::SpoolToTemp.materialize(&bytes, &syntax).unwrap();
        let second_path = second.temp.as_ref().unwrap().0.clone();
        assert_ne!(
            first_path, second_path,
            "two spools must never share a path"
        );
    }

    #[test]
    fn spool_to_dir_keeps_the_file() {
        let dir = std::env::temp_dir().join(format!("radicom-sink-{}", std::process::id()));
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let bytes = dataset_bytes(&syntax);
        let instance = StoreSink::SpoolToDir(dir.clone())
            .materialize(&bytes, &syntax)
            .unwrap();
        let path = instance.path.expect("directory sink reports its path");
        assert!(path.exists(), "the spooled file is kept");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_bytes_are_a_typed_error() {
        // an explicit-VR element whose VR is not a VR: the negotiated-syntax
        // parser rejects it (NET-24 kept the error typed)
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let bytes = [0x08, 0x00, 0x00, 0x00, b'Z', b'Z', 0x02, 0x00, 0x00, 0x00];
        match StoreSink::Memory.materialize(&bytes, &syntax) {
            Err(NetworkError::Io(_)) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn a_dataset_in_another_syntax_is_not_sniffed_into_place() {
        // NET-24: bytes encoded as implicit cannot be read as the negotiated
        // explicit syntax. The old auto-detect path accepted them and merely
        // relabelled the dataset, which is how a peer could smuggle a dataset
        // in a syntax it never negotiated.
        let bytes = dataset_bytes(&syntaxes::IMPLICIT_VR_LITTLE_ENDIAN);
        let announced = syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone();
        let error = StoreSink::Memory
            .materialize(&bytes, &announced)
            .expect_err("implicit bytes are not explicit big endian");
        assert!(matches!(error, NetworkError::Io(_)), "got {error:?}");
    }

    #[test]
    fn request_is_built_from_the_command() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let bytes = dataset_bytes(&syntax);
        let instance = StoreSink::Memory.materialize(&bytes, &syntax).unwrap();
        let request = c_store_request_from_message(&c_store_command(), instance).unwrap();
        assert_eq!(request.message_id, 11);
        assert_eq!(request.affected_sop_class_uid, "1.2.840.10008.5.1.4.1.1.7");
        assert_eq!(request.affected_sop_instance_uid, "1.2.3.4.5");
        assert_eq!(
            request.file.dataset.get_str(Tag::PATIENT_NAME).unwrap(),
            "Doe^John"
        );
    }

    #[test]
    fn wrong_command_field_is_rejected() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory
            .materialize(&dataset_bytes(&syntax), &syntax)
            .unwrap();
        let command = Command::request(CommandField::CEchoRq, 1);
        assert!(c_store_request_from_message(&command, instance).is_err());
    }

    #[test]
    fn missing_instance_uid_is_rejected() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory
            .materialize(&dataset_bytes(&syntax), &syntax)
            .unwrap();
        let mut command = c_store_command();
        command.affected_sop_instance_uid = None;
        assert!(c_store_request_from_message(&command, instance).is_err());
    }

    #[test]
    fn provider_failures_map_to_storage_statuses() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory
            .materialize(&dataset_bytes(&syntax), &syntax)
            .unwrap();
        let request = c_store_request_from_message(&c_store_command(), instance).unwrap();

        // NET-18: an unsupported SOP class is 0x0122, not the 0xA900 mismatch
        let response = failure_response(
            &request,
            &NetworkError::SopClassNotSupported {
                sop_class: "9.9".into(),
            },
        );
        assert_eq!(response.status, status::SOP_CLASS_NOT_SUPPORTED);
        assert_ne!(
            response.status,
            status::STORAGE_DATA_SET_DOES_NOT_MATCH_SOP_CLASS
        );

        let response = failure_response(&request, &NetworkError::protocol("bad"));
        assert_eq!(response.status, status::PROCESSING_FAILURE);
        assert!(
            response.error_comment.is_some(),
            "the comment carries the reason"
        );
    }

    #[test]
    fn long_failure_comments_are_truncated() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory
            .materialize(&dataset_bytes(&syntax), &syntax)
            .unwrap();
        let request = c_store_request_from_message(&c_store_command(), instance).unwrap();
        let response = failure_response(
            &request,
            &NetworkError::SopClassNotSupported {
                sop_class: "x".repeat(500),
            },
        );
        assert!(response.error_comment.unwrap().len() <= 64);
    }

    #[test]
    fn a_dataset_must_describe_the_command_s_instance() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory
            .materialize(&dataset_bytes(&syntax), &syntax)
            .unwrap();
        let request = c_store_request_from_message(&c_store_command(), instance).unwrap();
        assert!(dataset_mismatch(&request).is_none(), "they agree");

        // the command names another instance: 0xA900 (NET-23)
        let instance = StoreSink::Memory
            .materialize(&dataset_bytes(&syntax), &syntax)
            .unwrap();
        let mut command = c_store_command();
        command.affected_sop_instance_uid = Some("9.9.9".into());
        let request = c_store_request_from_message(&command, instance).unwrap();
        let comment = dataset_mismatch(&request).expect("instance mismatch");
        assert!(comment.contains("9.9.9"), "{comment}");

        // the command names another SOP class
        let instance = StoreSink::Memory
            .materialize(&dataset_bytes(&syntax), &syntax)
            .unwrap();
        let mut command = c_store_command();
        command.affected_sop_class_uid = Some("1.2.840.10008.5.1.4.1.1.2".into());
        let request = c_store_request_from_message(&command, instance).unwrap();
        let comment = dataset_mismatch(&request).expect("class mismatch");
        assert!(comment.contains("SOP class"), "{comment}");
    }

    #[test]
    fn a_dataset_is_parsed_with_the_negotiated_syntax() {
        // NET-24: the bytes carry no file meta, so an explicit-VR big-endian
        // dataset must be read with the negotiated syntax instead of sniffed
        let announced = syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone();
        let bytes = dataset_bytes(&announced);
        let instance = StoreSink::Memory.materialize(&bytes, &announced).unwrap();
        assert_eq!(
            instance
                .file
                .dataset
                .get_str(Tag::PATIENT_NAME)
                .unwrap()
                .trim(),
            "Doe^John"
        );
        assert_eq!(
            instance.file.dataset.transfer_syntax().uid(),
            announced.uid()
        );
    }

    #[test]
    fn an_io_failure_maps_onto_storage_failure() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory
            .materialize(&dataset_bytes(&syntax), &syntax)
            .unwrap();
        let request = c_store_request_from_message(&c_store_command(), instance).unwrap();
        let response = failure_response(
            &request,
            &NetworkError::Io(std::io::Error::other("disk full")),
        );
        assert_eq!(response.status, status::STORAGE_FAILURE);
    }

    fn spilled_body(
        syntax: &Arc<TransferSyntax>,
        tag: &str,
    ) -> (crate::assembler::MessageBody, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("radicom-store-spill-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut assembler =
            crate::assembler::PdvAssembler::with_spool(crate::assembler::SpoolConfig {
                memory_threshold: 0,
                dir: Some(dir.clone()),
                ..crate::assembler::SpoolConfig::default()
            });
        assembler.arm_spill(1);
        let pdv = crate::pdu::Pdv {
            context_id: 1,
            is_command: false,
            is_last: true,
            data: bytes::Bytes::from(dataset_bytes(syntax)),
        };
        let unit = assembler
            .push_pdv(&pdv)
            .unwrap()
            .expect("one PDV completes the dataset");
        (unit.body(), dir)
    }

    #[test]
    fn materialize_body_handles_an_in_memory_body() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let body =
            crate::assembler::MessageBody::Memory(bytes::Bytes::from(dataset_bytes(&syntax)));
        let instance = StoreSink::Memory.materialize_body(&body, &syntax).unwrap();
        assert_eq!(
            instance.file.dataset.get_str(Tag::PATIENT_NAME).unwrap(),
            "Doe^John"
        );
    }

    #[test]
    fn materialize_body_handles_a_spilled_body_for_every_sink() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();

        // Memory parses the spool file and keeps it alive for lazy value windows.
        let (body, dir) = spilled_body(&syntax, "mem");
        let instance = StoreSink::Memory.materialize_body(&body, &syntax).unwrap();
        assert_eq!(
            instance.file.dataset.get_str(Tag::PATIENT_NAME).unwrap(),
            "Doe^John"
        );
        assert!(instance.path.is_none());
        drop(instance);
        std::fs::remove_dir_all(&dir).ok();

        // SpoolToTemp adopts the received file instead of copying it.
        let (body, dir) = spilled_body(&syntax, "temp");
        let instance = StoreSink::SpoolToTemp
            .materialize_body(&body, &syntax)
            .unwrap();
        assert!(instance.path.is_none());
        let temp = body.spool().unwrap().path().to_path_buf();
        assert!(temp.exists(), "the instance adopted the received file");
        drop(instance);
        assert!(!temp.exists(), "the adopted file dies with the instance");
        std::fs::remove_dir_all(&dir).ok();

        // SpoolToDir keeps a copy under the directory.
        let (body, dir) = spilled_body(&syntax, "dir");
        let out = std::env::temp_dir().join(format!("radicom-store-out-{}", std::process::id()));
        let instance = StoreSink::SpoolToDir(out.clone())
            .materialize_body(&body, &syntax)
            .unwrap();
        let kept = instance.path.expect("directory sink reports its path");
        assert!(kept.exists(), "the spooled copy is kept");
        std::fs::remove_dir_all(&out).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_spooled_large_value_stays_file_backed() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let mut dataset = Dataset::new();
        dataset.insert_value(Tag::PIXEL_DATA, vec![0u8; 128 * 1024]);
        let bytes = crate::message::encode_dataset(&dataset, &syntax).unwrap();
        let dir = std::env::temp_dir().join(format!("radicom-store-window-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut assembler =
            crate::assembler::PdvAssembler::with_spool(crate::assembler::SpoolConfig {
                memory_threshold: 0,
                dir: Some(dir.clone()),
                ..crate::assembler::SpoolConfig::default()
            });
        assembler.arm_spill(1);
        let unit = assembler
            .push_pdv(&crate::pdu::Pdv {
                context_id: 1,
                is_command: false,
                is_last: true,
                data: bytes::Bytes::from(bytes),
            })
            .unwrap()
            .expect("the dataset completes");
        let instance = StoreSink::SpoolToTemp
            .materialize_body(&unit.body(), &syntax)
            .unwrap();
        let buffer = instance
            .file
            .dataset
            .get_element(Tag::PIXEL_DATA)
            .unwrap()
            .value()
            .as_buffer()
            .unwrap();
        assert!(!buffer.is_memory(), "large values stay file-backed");
        drop(instance);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_deflated_spool_has_an_inflated_size_limit() {
        let syntax = syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5");
        dataset.insert_value(Tag::PIXEL_DATA, vec![0u8; 4096]);
        let bytes = crate::message::encode_dataset(&dataset, &syntax).unwrap();
        let dir =
            std::env::temp_dir().join(format!("radicom-store-deflate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut assembler =
            crate::assembler::PdvAssembler::with_spool(crate::assembler::SpoolConfig {
                memory_threshold: 0,
                max_message_length: 1024,
                dir: Some(dir.clone()),
            });
        assembler.arm_spill(1);
        let unit = assembler
            .push_pdv(&crate::pdu::Pdv {
                context_id: 1,
                is_command: false,
                is_last: true,
                data: bytes::Bytes::from(bytes),
            })
            .unwrap()
            .expect("the dataset completes");
        let error = StoreSink::SpoolToTemp
            .materialize_body(&unit.body(), &syntax)
            .expect_err("inflated output must obey the spool limit");
        assert!(error.to_string().contains("maximum inflated size"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_malformed_spooled_dataset_is_removed_and_reported() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let dir = std::env::temp_dir().join(format!("radicom-store-bad-{}", std::process::id()));
        let bytes = [0x08u8, 0x00, 0x00, 0x00, b'Z', b'Z', 0x02, 0x00, 0x00, 0x00];
        let error = StoreSink::SpoolToDir(dir.clone())
            .materialize(&bytes, &syntax)
            .expect_err("a bad VR is rejected");
        assert!(matches!(error, NetworkError::Io(_)), "{error:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_request_without_a_class_uid_is_rejected() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory
            .materialize(&dataset_bytes(&syntax), &syntax)
            .unwrap();
        let mut command = c_store_command();
        command.affected_sop_class_uid = None;
        assert!(c_store_request_from_message(&command, instance).is_err());
    }
}
