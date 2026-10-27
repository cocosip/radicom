//! DICOM file container: format detection, file meta, open and save
//! (FILE-01..06; docs/05-file-io.md §4, §5.3).

use crate::error::{Error, FileError};
use crate::io::byte_source::ByteSourceReader;
use crate::io::dataset_builder::{parse_dataset_bytes, DatasetBuilder};
use crate::io::reader::DicomReader;
use crate::io::writer::{DicomWriter, WriteOptions};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::transfer_syntax::{syntaxes, TransferSyntax};
use crate::model::uid::Uid;
use crate::model::vr::Vr;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Implementation identity written into the file meta group (FILE-04).
pub const IMPLEMENTATION_CLASS_UID: &str = "1.2.826.0.1.3680043.10.1330.1";
pub const IMPLEMENTATION_VERSION_NAME: &str = "RADICOM_0_1";

/// On-disk layout detected while opening (FILE-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    /// 128-byte preamble + `DICM` + meta group.
    Dicom3,
    /// Meta group present, no preamble.
    Dicom3NoPreamble,
    /// Dataset only, no meta group.
    Dicom3NoFileMetaInfo,
    /// ACR-NEMA 1.0 (retired).
    AcrNema1,
    /// ACR-NEMA 2.0 (retired).
    AcrNema2,
}

impl FileFormat {
    /// ACR-NEMA variants cannot be written back (FILE-05).
    pub fn is_saveable(self) -> bool {
        !matches!(self, FileFormat::AcrNema1 | FileFormat::AcrNema2)
    }
}

/// Large-value policy for opening (FILE-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileReadOption {
    /// Alias of [`FileReadOption::ReadLargeOnDemand`].
    Default,
    /// Large values stay as file/stream windows.
    ReadLargeOnDemand,
    /// Large values are skipped entirely.
    SkipLargeTags,
    /// Large values are materialized in memory.
    ReadAll,
}

/// Open options (FILE-02).
#[derive(Debug, Clone)]
pub struct ReadOptions {
    pub read: FileReadOption,
    /// Values at or above this size are "large" (default 64 KiB).
    pub large_object_size: u64,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions { read: FileReadOption::Default, large_object_size: 64 * 1024 }
    }
}

/// A parsed DICOM file (FILE-01/04).
#[derive(Debug, Clone)]
pub struct DicomFile {
    /// Group 0002 elements (always Explicit VR Little Endian on the wire).
    pub file_meta: Dataset,
    pub dataset: Dataset,
    pub format: FileFormat,
    /// True when a stop predicate ended the parse early.
    pub is_partial: bool,
    /// Identity of the source, used in error context.
    source: Option<PathBuf>,
}

impl DicomFile {
    /// Builds a file in memory from an existing dataset (no source path).
    pub fn from_dataset(dataset: Dataset) -> Self {
        DicomFile {
            file_meta: Dataset::new(),
            dataset,
            format: FileFormat::Dicom3,
            is_partial: false,
            source: None,
        }
    }

    /// Opens and parses a file from disk.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| file_err(path, "read", e))?;
        let mut file = Self::from_bytes(&bytes, &ReadOptions::default()).map_err(|e| attach(e, path))?;
        file.source = Some(path.to_path_buf());
        Ok(file)
    }

    /// Parses from a byte slice.
    pub fn from_bytes(bytes: &[u8], options: &ReadOptions) -> Result<Self, Error> {
        let mut cursor = Cursor::new(bytes);
        Self::from_reader(&mut cursor, options)
    }

    /// Parses from any seekable reader (FILE-01).
    pub fn from_reader<R: Read + Seek>(reader: &mut R, options: &ReadOptions) -> Result<Self, Error> {
        let mut raw = Vec::new();
        reader
            .read_to_end(&mut raw)
            .map_err(|e| Error::File(FileError::Other(format!("read: {e}"))))?;
        let (format, meta_bytes, dataset_bytes) = detect(&raw)?;

        // file meta is always Explicit VR Little Endian
        let file_meta = match meta_bytes {
            Some(meta) => parse_dataset_bytes(meta, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN)?,
            None => Dataset::new(),
        };

        let syntax = file_meta
            .try_get_str(Tag::new(0x0002, 0x0010))
            .and_then(|uid| crate::model::transfer_syntax::parse(uid.trim()).ok())
            .unwrap_or_else(|| dataset_syntax(format, dataset_bytes));

        let mut dataset = parse_dataset_bytes(dataset_bytes, &syntax)?;
        dataset.set_transfer_syntax(syntax);
        // the ACR-NEMA recognition code lives in the *dataset* (0008,0010)
        let format = match dataset.try_get_str(Tag::new(0x0008, 0x0010)) {
            Some(code) => match code.trim() {
                "ACR-NEMA 1.0" => FileFormat::AcrNema1,
                "ACR-NEMA 2.0" => FileFormat::AcrNema2,
                _ => format,
            },
            None => format,
        };
        // ponytail: every value is materialized in memory, so `options.read`
        // and `large_object_size` have no effect yet; they take effect once
        // the File/Stream buffer windows land (FILE-02, BUF-02 P1).
        let _ = options;

        Ok(DicomFile {
            file_meta,
            dataset,
            format,
            is_partial: false,
            source: None,
        })
    }

    /// Parses until `stop` returns true, leaving the rest unread (FILE-01).
    pub fn open_partial(path: impl AsRef<Path>, stop: &dyn Fn(&crate::io::reader::ParseState) -> bool) -> Result<Self, Error> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| file_err(path, "read", e))?;
        let (format, meta_bytes, dataset_bytes) = detect(&bytes).map_err(|e| attach(e, path))?;
        let file_meta = match meta_bytes {
            Some(meta) => parse_dataset_bytes(meta, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN)?,
            None => Dataset::new(),
        };
        let syntax = file_meta
            .try_get_str(Tag::new(0x0002, 0x0010))
            .and_then(|uid| crate::model::transfer_syntax::parse(uid.trim()).ok())
            .unwrap_or_else(|| dataset_syntax(format, dataset_bytes));

        let mut source = ByteSourceReader::new(Cursor::new(dataset_bytes.to_vec()), syntax.endian());
        let mut builder = DatasetBuilder::with_transfer_syntax(syntax.clone());
        let outcome = DicomReader::default().read(&mut source, &syntax, &mut builder, Some(stop))?;
        Ok(DicomFile {
            file_meta,
            dataset: builder.into_dataset(),
            format,
            is_partial: outcome == crate::io::reader::ReadOutcome::Stopped,
            source: Some(path.to_path_buf()),
        })
    }

    /// Writes the file to disk (FILE-05).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref();
        let bytes = self.to_bytes(&WriteOptions::default())?;
        std::fs::write(path, bytes).map_err(|e| file_err(path, "write", e))
    }

    /// Serializes preamble, meta group and dataset (FILE-05).
    pub fn to_bytes(&self, options: &WriteOptions) -> Result<Vec<u8>, Error> {
        if !self.format.is_saveable() {
            return Err(Error::File(FileError::FormatNotSaveable(format!("{:?}", self.format))));
        }
        let mut out = Vec::new();
        out.extend_from_slice(&[0u8; 128]);
        out.extend_from_slice(b"DICM");
        let meta = self.refreshed_meta()?;
        DicomWriter::new(options.clone()).write_dataset(
            &meta,
            &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN,
            &mut out,
        )?;
        // the dataset keeps its own syntax; deflated bodies are compressed
        let syntax = self.dataset.transfer_syntax();
        if syntax.is_deflate() {
            let mut plain = Vec::new();
            DicomWriter::new(options.clone()).write_dataset(&self.dataset, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN, &mut plain)?;
            out.extend_from_slice(&deflate(&plain)?);
        } else {
            DicomWriter::new(options.clone()).write_dataset(&self.dataset, &syntax, &mut out)?;
        }
        Ok(out)
    }

    /// Writes to an arbitrary writer.
    pub fn write<W: Write>(&self, out: &mut W, options: &WriteOptions) -> Result<(), Error> {
        let bytes = self.to_bytes(options)?;
        out.write_all(&bytes).map_err(|e| Error::File(FileError::Other(format!("write: {e}"))))
    }

    /// FILE-06: true when `path` carries the 128-byte preamble + `DICM`.
    pub fn has_valid_header(path: impl AsRef<Path>) -> bool {
        let Ok(mut f) = std::fs::File::open(path) else {
            return false;
        };
        let mut magic = [0u8; 4];
        if f.seek(std::io::SeekFrom::Start(128)).is_err() {
            return false;
        }
        f.read_exact(&mut magic).is_ok() && &magic == b"DICM"
    }

    /// Meta group with Implementation identity refreshed and the dataset's
    /// transfer syntax recorded; rebuilt from the dataset when absent.
    fn refreshed_meta(&self) -> Result<Dataset, Error> {
        let mut meta = self.file_meta.clone();
        let syntax = self.dataset.transfer_syntax();
        meta.insert_str(Tag::new(0x0002, 0x0010), Vr::Ui, syntax.uid());
        meta.insert_str(Tag::new(0x0002, 0x0012), Vr::Ui, IMPLEMENTATION_CLASS_UID);
        meta.insert_str(Tag::new(0x0002, 0x0013), Vr::Sh, IMPLEMENTATION_VERSION_NAME);
        if !meta.contains(&Tag::new(0x0002, 0x0002)) {
            meta.insert_str(Tag::new(0x0002, 0x0002), Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        }
        if !meta.contains(&Tag::new(0x0002, 0x0003)) {
            let uid = self
                .dataset
                .try_get_str(Tag::new(0x0008, 0x0018))
                .unwrap_or_else(Uid::generate_value);
            meta.insert_str(Tag::new(0x0002, 0x0003), Vr::Ui, uid);
        }
        // the file meta group is always Explicit VR LE; drop stale lengths
        meta.remove(Tag::new(0x0002, 0x0000));
        Ok(meta)
    }
}

/// Raw file split: format, optional meta bytes, dataset bytes (FILE-03).
type Split<'a> = (FileFormat, Option<&'a [u8]>, &'a [u8]);

/// Splits a raw file into (format, meta bytes, dataset bytes) (FILE-03).
fn detect(raw: &[u8]) -> Result<Split<'_>, Error> {
    if raw.len() < 8 {
        return Err(Error::File(FileError::InvalidFormat(
            "file is too short to be DICOM".into(),
        )));
    }
    // Dicom3: preamble + DICM magic
    if raw.len() >= 132 && &raw[128..132] == b"DICM" {
        let (meta, dataset) = split_meta(&raw[132..])?;
        return Ok((FileFormat::Dicom3, meta, dataset));
    }
    // otherwise the file starts with the meta group or the dataset
    let (meta, dataset) = split_meta(raw)?;
    let format = if meta.is_some() { FileFormat::Dicom3NoPreamble } else { FileFormat::Dicom3NoFileMetaInfo };
    Ok((format, meta, dataset))
}

/// Splits leading group 0002 elements from the dataset that follows.
fn split_meta(raw: &[u8]) -> Result<(Option<&[u8]>, &[u8]), Error> {
    if raw.len() < 8 {
        return Err(Error::File(FileError::InvalidFormat("truncated header".into())));
    }
    let group = u16::from_le_bytes([raw[0], raw[1]]);
    if group != 0x0002 {
        return Ok((None, raw));
    }
    // Walk group 0002 elements (always Explicit VR Little Endian) to find
    // where the dataset starts.
    let mut pos = 0usize;
    while pos + 8 <= raw.len() {
        let g = u16::from_le_bytes([raw[pos], raw[pos + 1]]);
        if g != 0x0002 {
            break;
        }
        let vr = Vr::try_parse(&[raw[pos + 4], raw[pos + 5]]).ok_or_else(|| {
            Error::File(FileError::InvalidFormat(format!("bad meta VR at offset {pos}")))
        })?;
        let (len, header) = if vr.is_16bit_length() {
            (u16::from_le_bytes([raw[pos + 6], raw[pos + 7]]) as usize, 8)
        } else {
            if pos + 12 > raw.len() {
                break;
            }
            (
                u32::from_le_bytes([raw[pos + 8], raw[pos + 9], raw[pos + 10], raw[pos + 11]]) as usize,
                12,
            )
        };
        let end = pos + header + len;
        if end > raw.len() {
            return Err(Error::File(FileError::InvalidFormat("meta group is truncated".into())));
        }
        pos = end;
    }
    if pos == 0 {
        return Ok((None, raw));
    }
    Ok((Some(&raw[..pos]), &raw[pos..]))
}

/// Fallback syntax for a dataset with no meta information (FILE-03).
fn dataset_syntax(format: FileFormat, dataset: &[u8]) -> Arc<TransferSyntax> {
    let big_endian = format == FileFormat::AcrNema2
        || (dataset.len() >= 2 && u16::from_le_bytes([dataset[0], dataset[1]]) > 0x00FF);
    if big_endian {
        // an implicit big-endian dataset has no registered UID
        if looks_explicit(dataset) {
            syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone()
        } else {
            syntaxes::IMPLICIT_VR_BIG_ENDIAN.clone()
        }
    } else if looks_explicit(dataset) {
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone()
    } else {
        syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone()
    }
}

/// Probes the first element's VR bytes against the dictionary (FILE-03).
fn looks_explicit(dataset: &[u8]) -> bool {
    if dataset.len() < 8 {
        return false;
    }
    // byte order is unknown here, so accept either interpretation
    let code_le = [dataset[4], dataset[5]];
    let code_be = [dataset[5], dataset[4]];
    Vr::try_parse(&code_le).is_some() || Vr::try_parse(&code_be).is_some()
}

fn deflate(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    use std::io::Write as _;
    // PS3.5 A.5: the deflated transfer syntax is **raw DEFLATE (RFC 1951)**, no
    // zlib header/trailer; an odd-length stream gets one padding byte.
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    let mut out = encoder
        .write_all(bytes)
        .and_then(|_| encoder.finish())
        .map_err(|e| Error::Writer(crate::error::WriterError(format!("deflate: {e}"))))?;
    if out.len() % 2 == 1 {
        out.push(0);
    }
    Ok(out)
}

fn file_err(path: &Path, action: &str, e: std::io::Error) -> Error {
    Error::File(FileError::Other(format!("{action} {}: {e}", path.display())))
}

fn attach(e: Error, path: &Path) -> Error {
    match e {
        Error::File(_) | Error::Reader(_) => {
            Error::File(FileError::Other(format!("{}: {e}", path.display())))
        }
        other => other,
    }
}

/// Reads a file's meta group without parsing the dataset (FILE-04).
pub fn read_file_meta(path: impl AsRef<Path>) -> Result<Dataset, Error> {
    let bytes = std::fs::read(path.as_ref()).map_err(|e| file_err(path.as_ref(), "read", e))?;
    let (_, meta, _) = detect(&bytes)?;
    match meta {
        Some(meta) => parse_dataset_bytes(meta, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN),
        None => Ok(Dataset::new()),
    }
}

impl std::fmt::Display for FileFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            FileFormat::Dicom3 => "DICOM 3 (preamble + meta)",
            FileFormat::Dicom3NoPreamble => "DICOM 3 (meta, no preamble)",
            FileFormat::Dicom3NoFileMetaInfo => "DICOM 3 (no meta information)",
            FileFormat::AcrNema1 => "ACR-NEMA 1.0",
            FileFormat::AcrNema2 => "ACR-NEMA 2.0",
        };
        f.write_str(name)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::writer::WriteOptions;
    use crate::model::buffer::MemoryBuffer;

    fn sample_file() -> DicomFile {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::new(0x0008, 0x0016), Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::new(0x0008, 0x0018), Vr::Ui, "1.2.3.4.5");
        dataset.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Doe^John");
        dataset.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "PID-1");
        DicomFile {
            file_meta: Dataset::new(),
            dataset,
            format: FileFormat::Dicom3,
            is_partial: false,
            source: None,
        }
    }

    #[test]
    fn save_and_reopen_round_trip() {
        let file = sample_file();
        let bytes = file.to_bytes(&WriteOptions::default()).unwrap();
        assert_eq!(&bytes[128..132], b"DICM", "preamble + magic");
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.format, FileFormat::Dicom3);
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0010)), "Doe^John");
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0020)), "PID-1");
    }

    #[test]
    fn meta_group_is_refreshed_on_save() {
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.file_meta.get_str(Tag::new(0x0002, 0x0010)).unwrap(), "1.2.840.10008.1.2.1");
        assert_eq!(back.file_meta.get_str(Tag::new(0x0002, 0x0012)).unwrap(), IMPLEMENTATION_CLASS_UID);
        assert_eq!(back.file_meta.get_str(Tag::new(0x0002, 0x0013)).unwrap(), IMPLEMENTATION_VERSION_NAME);
        assert_eq!(
            back.file_meta.get_str(Tag::new(0x0002, 0x0003)).unwrap(),
            "1.2.3.4.5",
            "media storage instance UID comes from the dataset"
        );
    }

    #[test]
    fn file_without_preamble_is_detected() {
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        // drop the preamble and DICM magic: meta group now leads
        let body = &bytes[132..];
        let back = DicomFile::from_bytes(body, &ReadOptions::default()).unwrap();
        assert_eq!(back.format, FileFormat::Dicom3NoPreamble);
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0010)), "Doe^John");
    }

    #[test]
    fn dataset_only_file_falls_back_to_explicit_le() {
        // a bare dataset with no meta group at all
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "PID-1");
        let mut bytes = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&dataset, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN, &mut bytes)
            .unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.format, FileFormat::Dicom3NoFileMetaInfo);
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0020)), "PID-1");
    }

    #[test]
    fn implicit_dataset_without_meta_is_detected() {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "PID-1");
        let mut bytes = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&dataset, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN, &mut bytes)
            .unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0020)), "PID-1");
        assert_eq!(back.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2");
    }

    #[test]
    fn big_endian_dataset_without_meta_is_detected() {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "PID-1");
        let mut bytes = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&dataset, &syntaxes::EXPLICIT_VR_BIG_ENDIAN, &mut bytes)
            .unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0020)), "PID-1");
        assert_eq!(back.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2.2");
    }

    #[test]
    fn acr_nema_recognition_code_marks_unsaveable() {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::new(0x0008, 0x0010), Vr::Sh, "ACR-NEMA 2.0");
        dataset.insert_str(Tag::new(0x0010, 0x0020), Vr::Lo, "PID-1");
        let mut bytes = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&dataset, &syntaxes::EXPLICIT_VR_BIG_ENDIAN, &mut bytes)
            .unwrap();
        let file = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(file.format, FileFormat::AcrNema2);
        assert!(matches!(
            file.to_bytes(&WriteOptions::default()),
            Err(Error::File(FileError::FormatNotSaveable(_)))
        ), "ACR-NEMA cannot be saved");
    }

    #[test]
    fn truncated_input_is_rejected() {
        match DicomFile::from_bytes(&[0u8; 4], &ReadOptions::default()) {
            Err(Error::File(FileError::InvalidFormat(_))) => {}
            other => panic!("expected InvalidFormat, got {other:?}"),
        }
    }

    #[test]
    fn deflated_file_round_trips() {
        let mut file = sample_file();
        file.dataset.set_transfer_syntax(syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone());
        let bytes = file.to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0010)), "Doe^John");
        assert!(back.dataset.transfer_syntax().is_deflate());
    }

    #[test]
    fn sequences_and_fragments_survive_a_file_round_trip() {
        let mut file = sample_file();
        let seq = file.dataset.insert_sequence(Tag::new(0x0008, 0x1140));
        let mut item = Dataset::new();
        item.insert_str(Tag::new(0x0008, 0x1150), Vr::Ui, "1.2.3");
        seq.push(item);
        let frags = file.dataset.insert_fragments(Tag::new(0x7FE0, 0x0010), Vr::Ob);
        frags.push_fragment(std::sync::Arc::new(MemoryBuffer::from(vec![1, 2, 3, 4])));

        let bytes = file.to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.dataset, file.dataset, "file round-trip is lossless");
    }

    #[test]
    fn has_valid_header_checks_the_magic() {
        let dir = std::env::temp_dir().join(format!("radicom-hdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.dcm");
        let bad = dir.join("bad.dcm");
        std::fs::write(&good, sample_file().to_bytes(&WriteOptions::default()).unwrap()).unwrap();
        std::fs::write(&bad, b"not a dicom file at all").unwrap();
        assert!(DicomFile::has_valid_header(&good));
        assert!(!DicomFile::has_valid_header(&bad));
        assert!(!DicomFile::has_valid_header(dir.join("missing.dcm")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_and_open_on_disk() {
        let dir = std::env::temp_dir().join(format!("radicom-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.dcm");
        sample_file().save(&path).unwrap();
        let back = DicomFile::open(&path).unwrap();
        assert_eq!(back.get_str(Tag::new(0x0010, 0x0020)), "PID-1");
        assert_eq!(read_file_meta(&path).unwrap().get_str(Tag::new(0x0002, 0x0010)).unwrap(), "1.2.840.10008.1.2.1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stop_predicate_leaves_the_file_partial() {
        let dir = std::env::temp_dir().join(format!("radicom-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("partial.dcm");
        sample_file().save(&path).unwrap();
        // (0008,*) sorts before (0010,*), so it is read before the stop tag
        let stop = |state: &crate::io::reader::ParseState| state.tag == (0x0010, 0x0010);
        let file = DicomFile::open_partial(&path, &stop).unwrap();
        assert!(file.is_partial, "FILE-01: is_partial set when stopped");
        assert!(file.dataset.contains(&Tag::new(0x0008, 0x0016)), "elements before the stop tag");
        assert!(!file.dataset.contains(&Tag::new(0x0010, 0x0010)), "stop tag not read");
        assert!(!file.dataset.contains(&Tag::new(0x0010, 0x0020)), "nothing past the stop tag");
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn deflated_file_rewrite_is_stable() {
        let mut file = sample_file();
        file.dataset.set_transfer_syntax(syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone());
        let first = file.to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&first, &ReadOptions::default()).unwrap();
        let second = back.to_bytes(&WriteOptions::default()).unwrap();
        let again = DicomFile::from_bytes(&second, &ReadOptions::default()).unwrap();
        assert_eq!(again.dataset, file.dataset, "deflate round-trip is stable");
    }

    impl DicomFile {
        fn get_str(&self, tag: Tag) -> String {
            self.dataset.get_str(tag).unwrap()
        }
    }
}
