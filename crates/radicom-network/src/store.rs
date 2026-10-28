//! C-STORE receive path (SRV-03; docs/01-architecture.md §11, §7.2).
//!
//! The SCP spools the incoming dataset to a temporary file and opens it as a
//! [`DicomFile`], so memory stays bounded for large instances. The sink is
//! injectable.

use crate::dimse::{status, Command, CommandField};
use crate::error::{NetworkError, Result};
use crate::services::{CStoreRequest, CStoreResponse};
use radicom::io::file::{DicomFile, ReadOptions};
use radicom::model::transfer_syntax::TransferSyntax;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where an incoming instance is put before the provider sees it (§5.2).
#[derive(Debug, Clone, Default)]
pub enum StoreSink {
    /// Spool to a temporary file that is removed once the instance is done
    /// with (the default).
    #[default]
    SpoolToTemp,
    /// Spool into a named directory, keeping the file afterwards.
    SpoolToDir(PathBuf),
    /// Keep the bytes in memory and parse them directly.
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
        ReceivedInstance { file, path: None, temp: None }
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
    pub fn materialize(&self, bytes: &[u8], syntax: &Arc<TransferSyntax>) -> Result<ReceivedInstance> {
        match self {
            StoreSink::Memory => {
                let mut file = DicomFile::from_bytes(bytes, &ReadOptions::default()).map_err(parse_err)?;
                // the wire syntax wins: the spooled bytes carry no file meta
                file.dataset.set_transfer_syntax(syntax.clone());
                Ok(ReceivedInstance::from_file(file))
            }
            StoreSink::SpoolToTemp => {
                let path = temp_path();
                // the guard removes the file on every error path below
                let temp = TempSpool(path.clone());
                let file = write_and_parse(&path, bytes, syntax)?;
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
                let path = dir.join(format!("{}.dcm", uuid_like()));
                write_and_parse(&path, bytes, syntax)
                    .map(|file| ReceivedInstance { file, path: Some(path), temp: None })
            }
        }
    }
}

/// Writes the dataset to `path` and opens it, removing the file on failure.
fn write_and_parse(path: &Path, bytes: &[u8], syntax: &Arc<TransferSyntax>) -> Result<DicomFile> {
    let mut file = std::fs::File::create(path).map_err(NetworkError::Io)?;
    file.write_all(bytes).map_err(NetworkError::Io)?;
    file.flush().map_err(NetworkError::Io)?;
    drop(file);

    match DicomFile::from_bytes(bytes, &ReadOptions::default()) {
        Ok(mut parsed) => {
            // the wire syntax wins: the spooled bytes carry no file meta
            parsed.dataset.set_transfer_syntax(syntax.clone());
            Ok(parsed)
        }
        Err(e) => {
            let _ = std::fs::remove_file(path);
            Err(parse_err(e))
        }
    }
}

fn parse_err(e: radicom::Error) -> NetworkError {
    NetworkError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// A process-unique temporary path (no extra dependency).
fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!("radicom-store-{}.dcm", uuid_like()))
}

/// A cheap unique token from the clock and a counter.
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{n:x}")
}

/// Builds the [`CStoreRequest`] from a received command and its dataset (§7.2).
pub fn c_store_request_from_message(
    command: &Command,
    instance: ReceivedInstance,
) -> Result<CStoreRequest> {
    if command.field != CommandField::CStoreRq {
        return Err(NetworkError::protocol("not a C-STORE request"));
    }
    let affected_sop_class_uid = command
        .affected_sop_class_uid
        .clone()
        .ok_or(NetworkError::protocol("C-STORE request has no affected SOP class UID"))?;
    let affected_sop_instance_uid = command
        .affected_sop_instance_uid
        .clone()
        .ok_or(NetworkError::protocol("C-STORE request has no affected SOP instance UID"))?;
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

/// Maps a provider failure onto a C-STORE response (§7.2).
///
/// The connection survives: a rejected instance becomes a failure status with
/// a truncated comment, never a disconnect.
pub fn failure_response(request: &CStoreRequest, error: &NetworkError) -> CStoreResponse {
    let code = match error {
        NetworkError::SopClassNotSupported { .. } => status::STORAGE_DATA_SET_DOES_NOT_MATCH_SOP_CLASS,
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
    fn memory_sink_parses_the_instance() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let bytes = dataset_bytes(&syntax);
        let instance = StoreSink::Memory.materialize(&bytes, &syntax).unwrap();
        assert_eq!(instance.file.dataset.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^John");
        assert_eq!(instance.file.dataset.transfer_syntax().uid(), syntax.uid());
        assert!(instance.path.is_none(), "no file kept");
    }

    #[test]
    fn spool_to_temp_removes_its_file_with_the_instance() {
        let syntax = syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone();
        let bytes = dataset_bytes(&syntax);
        let instance = StoreSink::SpoolToTemp.materialize(&bytes, &syntax).unwrap();
        assert_eq!(instance.file.dataset.get_str(Tag::SOP_INSTANCE_UID).unwrap(), "1.2.3.4.5");
        assert!(instance.path.is_none(), "temporary spool files are not reported");
        let spooled = instance.temp.as_ref().expect("a temp spool file").0.clone();
        assert!(spooled.exists(), "the instance was spooled to disk");
        drop(instance);
        assert!(!spooled.exists(), "the temp spool file must not outlive the instance");
    }

    #[test]
    fn spool_to_dir_keeps_the_file() {
        let dir = std::env::temp_dir().join(format!("radicom-sink-{}", std::process::id()));
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let bytes = dataset_bytes(&syntax);
        let instance = StoreSink::SpoolToDir(dir.clone()).materialize(&bytes, &syntax).unwrap();
        let path = instance.path.expect("directory sink reports its path");
        assert!(path.exists(), "the spooled file is kept");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_bytes_are_a_typed_error() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        match StoreSink::Memory.materialize(&[0xFF; 3], &syntax) {
            Err(NetworkError::Io(_)) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn wire_syntax_overrides_the_parsed_one() {
        // bytes encoded as implicit but announced as explicit big endian: the
        // announced syntax is what the caller must trust
        let bytes = dataset_bytes(&syntaxes::IMPLICIT_VR_LITTLE_ENDIAN);
        let announced = syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone();
        let instance = StoreSink::Memory.materialize(&bytes, &announced).unwrap();
        assert_eq!(instance.file.dataset.transfer_syntax().uid(), announced.uid());
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
        assert_eq!(request.file.dataset.get_str(Tag::PATIENT_NAME).unwrap(), "Doe^John");
    }

    #[test]
    fn wrong_command_field_is_rejected() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory.materialize(&dataset_bytes(&syntax), &syntax).unwrap();
        let command = Command::request(CommandField::CEchoRq, 1);
        assert!(c_store_request_from_message(&command, instance).is_err());
    }

    #[test]
    fn missing_instance_uid_is_rejected() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory.materialize(&dataset_bytes(&syntax), &syntax).unwrap();
        let mut command = c_store_command();
        command.affected_sop_instance_uid = None;
        assert!(c_store_request_from_message(&command, instance).is_err());
    }

    #[test]
    fn provider_failures_map_to_storage_statuses() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory.materialize(&dataset_bytes(&syntax), &syntax).unwrap();
        let request = c_store_request_from_message(&c_store_command(), instance).unwrap();

        let response = failure_response(
            &request,
            &NetworkError::SopClassNotSupported { sop_class: "9.9".into() },
        );
        assert_eq!(response.status, status::STORAGE_DATA_SET_DOES_NOT_MATCH_SOP_CLASS);

        let response = failure_response(&request, &NetworkError::protocol("bad"));
        assert_eq!(response.status, status::PROCESSING_FAILURE);
        assert!(response.error_comment.is_some(), "the comment carries the reason");
    }

    #[test]
    fn long_failure_comments_are_truncated() {
        let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
        let instance = StoreSink::Memory.materialize(&dataset_bytes(&syntax), &syntax).unwrap();
        let request = c_store_request_from_message(&c_store_command(), instance).unwrap();
        let response = failure_response(
            &request,
            &NetworkError::SopClassNotSupported { sop_class: "x".repeat(500) },
        );
        assert!(response.error_comment.unwrap().len() <= 64);
    }
}
