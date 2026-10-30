//! C-STORE receive path (SRV-03; docs/01-architecture.md §11).
//!
//! The receiver parses the inbound dataset bytes into a [`DicomFile`]. The
//! bytes are already buffered in memory by the time the sink runs, so the
//! default [`StoreSink::Memory`] parses them directly; [`StoreSink::SpoolToTemp`]
//! and [`StoreSink::SpoolToDir`] additionally keep an on-disk copy for callers
//! that want one.
//!
//! **Ceiling:** a dataset arrives whole. The assembler buffers up to
//! [`crate::assembler::PdvAssembler::DEFAULT_MAX_MESSAGE_LENGTH`] (1 GiB) and
//! parses it into a `DicomFile`, so peak memory tracks the instance size rather
//! than the PDU size. Streaming a dataset into the sink as PDVs arrive is a
//! documented backlog item (docs/06-backlog.md §1, NET-13).

use crate::dimse::{status, Command, CommandField};
use crate::error::{NetworkError, Result};
use crate::services::{CStoreRequest, CStoreResponse};
use radicom::io::file::DicomFile;
use radicom::io::temp::create_unique_file;
use radicom::model::transfer_syntax::TransferSyntax;
use std::io::Write;
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
    /// Materializes the instance from the dataset bytes.
    pub fn materialize(
        &self,
        bytes: &[u8],
        syntax: &Arc<TransferSyntax>,
    ) -> Result<ReceivedInstance> {
        match self {
            StoreSink::Memory => {
                // PS3.7 §9.3: the dataset is encoded in the negotiated transfer
                // syntax and carries no file meta, so it must be parsed *with*
                // that syntax instead of being sniffed (NET-24)
                let dataset = crate::message::decode_dataset(bytes, syntax)?;
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
                let file = write_and_parse(&path, file, bytes, syntax)?;
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
                write_and_parse(&path, file, bytes, syntax).map(|file| ReceivedInstance {
                    file,
                    path: Some(path),
                    temp: None,
                })
            }
        }
    }
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
) -> Result<DicomFile> {
    file.write_all(bytes).map_err(NetworkError::Io)?;
    file.flush().map_err(NetworkError::Io)?;
    drop(file);

    // the spooled bytes are a bare dataset: parse them with the negotiated
    // syntax, never by sniffing (NET-24)
    match crate::message::decode_dataset(bytes, syntax) {
        Ok(dataset) => Ok(DicomFile::from_dataset(dataset)),
        Err(e) => {
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
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
}
