//! DICOM file container: format detection, file meta, open and save
//! (FILE-01..06; docs/01-architecture.md §7, §5.3).

use crate::error::{Error, FileError};
use crate::io::byte_source::{ByteSourceReader, WindowSource};
use crate::io::dataset_builder::{parse_dataset_bytes, DatasetBuilder};
use crate::io::reader::{DicomReader, LargeValuePolicy};
use crate::io::writer::{DicomWriter, WriteOptions};
use crate::model::buffer::{FileWindow, SharedBuffer};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::transfer_syntax::{syntaxes, TransferSyntax};
use crate::model::uid::Uid;
use crate::model::vr::Vr;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
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
    /// The path this file was opened from, when it came from disk (FILE-01).
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

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

    /// Opens and parses a file from disk (FILE-01, default read options).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with_options(path, &ReadOptions::default())
    }

    /// Opens a file honoring the [`ReadOptions`] large-value policy (FILE-02).
    ///
    /// The header and meta group are read with a bounded scan; the dataset is
    /// then parsed directly from the file so large values stay as lazy
    /// [`FileWindow`]s instead of being copied into memory. A window serves
    /// range reads and `copy_to` streaming straight from disk; only `data()`
    /// materializes the value, once, because the buffer API returns `&[u8]`.
    pub fn open_with_options(path: impl AsRef<Path>, options: &ReadOptions) -> Result<Self, Error> {
        let path = path.as_ref();
        let mut file = std::fs::File::open(path).map_err(|e| file_err(path, "open", e))?;
        let file_len = file.metadata().map_err(|e| file_err(path, "stat", e))?.len();
        if file_len < 8 {
            return Err(Error::File(FileError::InvalidFormat("file is too short to be DICOM".into())));
        }

        // preamble / header
        let head_len = file_len.min(132) as usize;
        let mut head = vec![0u8; head_len];
        file.read_exact(&mut head).map_err(|e| file_err(path, "read", e))?;
        let (mut format, meta_start) = if head_len >= 132 && &head[128..132] == b"DICM" {
            (FileFormat::Dicom3, 132u64)
        } else {
            (FileFormat::Dicom3NoFileMetaInfo, 0u64)
        };

        // leading group 0002 (bounded, growing read)
        let (meta_block, meta_len) = read_meta_block(&mut file, meta_start, file_len)
            .map_err(|e| Error::File(FileError::InvalidFormat(format!("meta group: {e}"))))?;
        let file_meta = if meta_len > 0 {
            parse_dataset_bytes(&meta_block, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN)?
        } else {
            Dataset::new()
        };
        if meta_len > 0 && meta_start == 0 {
            format = FileFormat::Dicom3NoPreamble;
        }

        let dataset_offset = meta_start + meta_len;
        let syntax = file_meta
            .try_get_str(Tag::TRANSFER_SYNTAX_UID)
            .and_then(|uid| crate::model::transfer_syntax::parse(uid.trim()).ok())
            .unwrap_or_else(|| {
                let probe = read_probe(&mut file, dataset_offset, file_len);
                dataset_syntax(format, &probe)
            });

        let (mut dataset, outcome) = if syntax.is_deflate() {
            // Stream the inflate straight from the file: the compressed body is
            // never held in memory, so the peak is one full (inflated) copy plus
            // the codec window rather than compressed + inflated.
            let plain = crate::io::reader::inflate_stream(&mut file, dataset_offset)?;
            parse_slice(&plain, &syntax, options)?
        } else {
            file.seek(SeekFrom::Start(dataset_offset)).map_err(|e| file_err(path, "seek", e))?;
            // Buffered reads: the parser issues several small reads per element,
            // and each one used to be its own syscall. This is safe now that the
            // source tracks its own position instead of asking the stream, so the
            // buffer is never consulted for offsets (docs/fo-dicom-alignment/05 §3.4).
            let buffered = std::io::BufReader::with_capacity(64 * 1024, file);
            let mut source = ByteSourceReader::new(buffered, syntax.endian())
                .with_window_source(Arc::new(PathWindow { path: path.to_path_buf() }));
            let reader = DicomReader {
                large_object_size: options.large_object_size,
                large_policy: policy_of(options.read),
                partial_on_eof: true,
                ..DicomReader::default()
            };
            let mut builder = DatasetBuilder::with_transfer_syntax(syntax.clone());
            let outcome = reader.read(&mut source, &syntax, &mut builder, None)?;
            (builder.into_dataset(), outcome)
        };
        dataset.set_transfer_syntax(syntax.clone());

        // the ACR-NEMA recognition code lives in the *dataset* (0008,0010)
        let format = match dataset.try_get_str(Tag::RECOGNITION_CODE) {
            Some(code) => match code.trim() {
                "ACR-NEMA 1.0" => FileFormat::AcrNema1,
                "ACR-NEMA 2.0" => FileFormat::AcrNema2,
                _ => format,
            },
            None => format,
        };

        Ok(DicomFile {
            file_meta,
            dataset,
            format,
            is_partial: outcome == crate::io::reader::ReadOutcome::Suspended,
            source: Some(path.to_path_buf()),
        })
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
            .try_get_str(Tag::TRANSFER_SYNTAX_UID)
            .and_then(|uid| crate::model::transfer_syntax::parse(uid.trim()).ok())
            .unwrap_or_else(|| dataset_syntax(format, dataset_bytes));

        let (mut dataset, outcome) = parse_dataset_with(dataset_bytes, &syntax, options)?;
        dataset.set_transfer_syntax(syntax);
        // the ACR-NEMA recognition code lives in the *dataset* (0008,0010)
        let format = match dataset.try_get_str(Tag::RECOGNITION_CODE) {
            Some(code) => match code.trim() {
                "ACR-NEMA 1.0" => FileFormat::AcrNema1,
                "ACR-NEMA 2.0" => FileFormat::AcrNema2,
                _ => format,
            },
            None => format,
        };

        Ok(DicomFile {
            file_meta,
            dataset,
            format,
            is_partial: outcome == crate::io::reader::ReadOutcome::Suspended,
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
            .try_get_str(Tag::TRANSFER_SYNTAX_UID)
            .and_then(|uid| crate::model::transfer_syntax::parse(uid.trim()).ok())
            .unwrap_or_else(|| dataset_syntax(format, dataset_bytes));

        let mut source = ByteSourceReader::new(Cursor::new(dataset_bytes.to_vec()), syntax.endian());
        let mut builder = DatasetBuilder::with_transfer_syntax(syntax.clone());
        let outcome = DicomReader::default().read(&mut source, &syntax, &mut builder, Some(stop))?;
        Ok(DicomFile {
            file_meta,
            dataset: builder.into_dataset(),
            format,
            is_partial: outcome != crate::io::reader::ReadOutcome::Success,
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
        // PS3.10 Table 7.1-1: (0002,0000) File Meta Information Group Length is
        // Type 1 (required), so always recalculate it for the meta group whatever
        // the caller's dataset option says.
        let meta_options = WriteOptions { keep_group_lengths: true, ..options.clone() };
        DicomWriter::new(meta_options).write_dataset(
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
        meta.insert_str(Tag::TRANSFER_SYNTAX_UID, Vr::Ui, syntax.uid());
        meta.insert_str(Tag::IMPLEMENTATION_CLASS_UID, Vr::Ui, IMPLEMENTATION_CLASS_UID);
        meta.insert_str(Tag::IMPLEMENTATION_VERSION_NAME, Vr::Sh, IMPLEMENTATION_VERSION_NAME);
        if !meta.contains(&Tag::MEDIA_STORAGE_SOP_CLASS_UID) {
            meta.insert_str(Tag::MEDIA_STORAGE_SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        }
        if !meta.contains(&Tag::MEDIA_STORAGE_SOP_INSTANCE_UID) {
            let uid = self
                .dataset
                .try_get_str(Tag::SOP_INSTANCE_UID)
                .unwrap_or_else(Uid::generate_value);
            meta.insert_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID, Vr::Ui, uid);
        }
        // the file meta group is always Explicit VR LE; drop stale lengths
        meta.remove(Tag::FILE_META_INFORMATION_GROUP_LENGTH);
        Ok(meta)
    }
}

/// Raw file split: format, optional meta bytes, dataset bytes (FILE-03).
type Split<'a> = (FileFormat, Option<&'a [u8]>, &'a [u8]);

/// Splits a raw file into (format, meta bytes, dataset bytes) (FILE-03).
/// Maps the file-level read policy to the parser's large-value policy.
fn policy_of(read: FileReadOption) -> LargeValuePolicy {
    match read {
        FileReadOption::SkipLargeTags => LargeValuePolicy::Skip,
        FileReadOption::ReadAll => LargeValuePolicy::Materialize,
        FileReadOption::Default | FileReadOption::ReadLargeOnDemand => LargeValuePolicy::OnDemand,
    }
}

/// Parses a dataset byte slice honoring the large-value policy (FILE-02).
/// Deflated bodies are inflated first; there is no window source here, so
/// `ReadLargeOnDemand` falls back to materializing. The caller-owned slice stays
/// live alongside the inflated copy here (slice input cannot be freed); the
/// file path uses [`crate::io::reader::inflate_stream`] instead.
fn parse_dataset_with(
    bytes: &[u8],
    syntax: &TransferSyntax,
    options: &ReadOptions,
) -> Result<(Dataset, crate::io::reader::ReadOutcome), Error> {
    if syntax.is_deflate() {
        let plain = crate::io::reader::inflate(bytes)?;
        parse_slice(&plain, syntax, options)
    } else {
        parse_slice(bytes, syntax, options)
    }
}

/// Parses an already-inflated dataset slice without copying it again
/// (`Cursor<&[u8]>` borrows the bytes).
fn parse_slice(
    bytes: &[u8],
    syntax: &TransferSyntax,
    options: &ReadOptions,
) -> Result<(Dataset, crate::io::reader::ReadOutcome), Error> {
    let mut source = ByteSourceReader::new(Cursor::new(bytes), syntax.endian());
    let reader = DicomReader {
        large_object_size: options.large_object_size,
        large_policy: policy_of(options.read),
        partial_on_eof: true,
        ..DicomReader::default()
    };
    let mut builder = DatasetBuilder::with_transfer_syntax(Arc::new(syntax.clone()));
    let outcome = reader.read(&mut source, syntax, &mut builder, None)?;
    Ok((builder.into_dataset(), outcome))
}

/// Builds lazy [`FileWindow`]s over a file for the parser (FILE-02).
struct PathWindow {
    path: PathBuf,
}

impl WindowSource for PathWindow {
    fn window(&self, absolute_offset: u64, len: u64) -> std::io::Result<SharedBuffer> {
        Ok(Arc::new(FileWindow::open(&self.path, absolute_offset, len)?))
    }
}

/// Reads up to 4 KiB at `start` for transfer-syntax probing.
fn read_probe(file: &mut std::fs::File, start: u64, file_len: u64) -> Vec<u8> {
    let take = file_len.saturating_sub(start).min(4096) as usize;
    let mut buf = vec![0u8; take];
    if file.seek(SeekFrom::Start(start)).and_then(|_| file.read_exact(&mut buf)).is_err() {
        return Vec::new();
    }
    buf
}

enum MetaScan {
    /// A group-0002 run ended at `usize` bytes.
    Run(usize),
    /// The first group is not 0x0002 (no meta group).
    None,
    /// Ran out of buffered bytes while still inside group 0002.
    More,
}

/// Reads the leading group 0002 element run, growing the buffer as needed so a
/// large file is never read whole. Returns the meta bytes and their length.
///
/// The buffer grows geometrically; each growth reads only the newly needed
/// bytes from the one already-seeked file position, so the bytes read are
/// O(n) rather than re-reading the prefix on every step.
fn read_meta_block(file: &mut std::fs::File, start: u64, file_len: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let avail = file_len.saturating_sub(start) as usize;
    file.seek(SeekFrom::Start(start))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut want = avail.min(4096);
    loop {
        let take = want.min(avail);
        if buf.len() < take {
            let old = buf.len();
            buf.resize(take, 0);
            file.read_exact(&mut buf[old..])?;
        }
        match scan_meta(&buf) {
            MetaScan::Run(len) => return Ok((buf[..len].to_vec(), len as u64)),
            MetaScan::None => return Ok((Vec::new(), 0)),
            MetaScan::More => {
                if take == avail {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "meta group is truncated"));
                }
                want = (take.saturating_mul(2)).min(avail);
            }
        }
    }
}

/// Walks group-0002 elements (always Explicit VR LE) over `buf`.
fn scan_meta(buf: &[u8]) -> MetaScan {
    if buf.len() < 8 {
        return MetaScan::More;
    }
    if u16::from_le_bytes([buf[0], buf[1]]) != 0x0002 {
        return MetaScan::None;
    }
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        if u16::from_le_bytes([buf[pos], buf[pos + 1]]) != 0x0002 {
            return MetaScan::Run(pos);
        }
        let vr = match Vr::try_parse(&[buf[pos + 4], buf[pos + 5]]) {
            Some(v) => v,
            None => return MetaScan::More,
        };
        let (len, header) = if vr.is_16bit_length() {
            (u16::from_le_bytes([buf[pos + 6], buf[pos + 7]]) as usize, 8)
        } else {
            if pos + 12 > buf.len() {
                return MetaScan::More;
            }
            (u32::from_le_bytes([buf[pos + 8], buf[pos + 9], buf[pos + 10], buf[pos + 11]]) as usize, 12)
        };
        let end = pos + header + len;
        if end > buf.len() {
            return MetaScan::More;
        }
        pos = end;
    }
    // all buffered bytes consumed and still inside group 0002 (or exactly at the
    // end); ask for more unless the caller already read to EOF
    MetaScan::More
}

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
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5");
        dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        dataset.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
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
        // literal encoded values, independent of the reader: a wrong writer that
        // the reader happens to mirror would fail these
        assert!(bytes.windows(8).any(|w| w == b"Doe^John"), "PN value written verbatim");
        assert!(bytes.windows(5).any(|w| w == b"PID-1"), "LO value written verbatim");
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.format, FileFormat::Dicom3);
        assert_eq!(back.get_str(Tag::PATIENT_NAME), "Doe^John");
        assert_eq!(back.get_str(Tag::PATIENT_ID), "PID-1");
    }

    #[test]
    fn meta_group_is_refreshed_on_save() {
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.file_meta.get_str(Tag::TRANSFER_SYNTAX_UID).unwrap(), "1.2.840.10008.1.2.1");
        assert_eq!(back.file_meta.get_str(Tag::IMPLEMENTATION_CLASS_UID).unwrap(), IMPLEMENTATION_CLASS_UID);
        assert_eq!(back.file_meta.get_str(Tag::IMPLEMENTATION_VERSION_NAME).unwrap(), IMPLEMENTATION_VERSION_NAME);
        assert_eq!(
            back.file_meta.get_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID).unwrap(),
            "1.2.3.4.5",
            "media storage instance UID comes from the dataset"
        );
    }

    #[test]
    fn file_meta_group_length_is_written_and_correct() {
        // PS3.10 Table 7.1-1: (0002,0000) FileMetaInformationGroupLength is Type 1.
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        let m = 132usize; // after the 128-byte preamble + "DICM"
        assert_eq!(&bytes[m..m + 2], &[0x02, 0x00], "first meta element is group 0002");
        assert_eq!(&bytes[m + 2..m + 4], &[0x00, 0x00], "element is 0000");
        assert_eq!(&bytes[m + 4..m + 6], b"UL", "(0002,0000) carries UL");
        let group_len = u32::from_le_bytes([bytes[m + 8], bytes[m + 9], bytes[m + 10], bytes[m + 11]]) as usize;
        assert!(group_len > 0, "group length is non-zero");
        // the dataset must begin exactly after the declared meta remainder
        let dataset_start = m + 12 + group_len;
        let g = u16::from_le_bytes([bytes[dataset_start], bytes[dataset_start + 1]]);
        assert_ne!(g, 0x0002, "the bytes after the declared meta length are the dataset, not more meta");
    }

    #[test]
    fn file_without_preamble_is_detected() {
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        // drop the preamble and DICM magic: meta group now leads
        let body = &bytes[132..];
        let back = DicomFile::from_bytes(body, &ReadOptions::default()).unwrap();
        assert_eq!(back.format, FileFormat::Dicom3NoPreamble);
        assert_eq!(back.get_str(Tag::PATIENT_NAME), "Doe^John");
    }

    #[test]
    fn dataset_only_file_falls_back_to_explicit_le() {
        // a bare dataset with no meta group at all
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        let mut bytes = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&dataset, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN, &mut bytes)
            .unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.format, FileFormat::Dicom3NoFileMetaInfo);
        assert_eq!(back.get_str(Tag::PATIENT_ID), "PID-1");
    }

    #[test]
    fn implicit_dataset_without_meta_is_detected() {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        let mut bytes = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&dataset, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN, &mut bytes)
            .unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::PATIENT_ID), "PID-1");
        assert_eq!(back.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2");
    }

    #[test]
    fn big_endian_dataset_without_meta_is_detected() {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        let mut bytes = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&dataset, &syntaxes::EXPLICIT_VR_BIG_ENDIAN, &mut bytes)
            .unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::PATIENT_ID), "PID-1");
        assert_eq!(back.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2.2");
    }

    #[test]
    fn big_endian_file_round_trips_numeric_and_text() {
        // Full Part-10 file (preamble + ELE meta + BE dataset): numeric US values
        // must survive the reader's byte swap and the writer's target-endian swap.
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        dataset.insert_value(Tag::ROWS, 512u16); // Rows (US)
        dataset.insert_value(Tag::COLUMNS, 256u16); // Columns (US)
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone());
        let bytes = DicomFile::from_dataset(dataset).to_bytes(&WriteOptions::default()).unwrap();

        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2.2");
        assert_eq!(back.get_str(Tag::PATIENT_ID), "PID-1");
        let rows: u16 = back.dataset.get(Tag::ROWS).unwrap();
        let cols: u16 = back.dataset.get(Tag::COLUMNS).unwrap();
        assert_eq!((rows, cols), (512, 256), "US values survive the big-endian file round-trip");
    }

    #[test]
    fn acr_nema_recognition_code_marks_unsaveable() {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::RECOGNITION_CODE, Vr::Sh, "ACR-NEMA 2.0");
        dataset.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
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
    fn open_reports_a_truncated_meta_group_as_an_invalid_format() {
        // 128-byte preamble + DICM, then a group-0002 element that declares 100
        // bytes but the file ends after 4: a typed format error, never a panic.
        let dir = std::env::temp_dir().join(format!("radicom-truncmeta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trunc-meta.dcm");
        let mut bytes = vec![0u8; 128];
        bytes.extend_from_slice(b"DICM");
        bytes.extend_from_slice(&[0x02, 0x00, 0x10, 0x00, b'U', b'I', 0x64, 0x00]);
        bytes.extend_from_slice(b"1.2."); // only 4 of the declared 100 bytes
        std::fs::write(&path, &bytes).unwrap();
        match DicomFile::open(&path) {
            Err(Error::File(FileError::InvalidFormat(message))) => {
                assert!(message.contains("meta group"), "typed meta-group error, got {message}");
            }
            other => panic!("expected FileError::InvalidFormat(meta group), got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_meta_block_grows_in_one_pass_and_rejects_truncation() {
        let dir = std::env::temp_dir().join(format!("radicom-metablock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dataset = [0x08u8, 0x00, 0x05, 0x00, b'C', b'S', 0x02, 0x00, b'A', b' '];

        // well-formed: one UI element, then a group-0008 dataset element
        let ok_path = dir.join("ok.dcm");
        let mut bytes = vec![0x02, 0x00, 0x10, 0x00, b'U', b'I', 0x04, 0x00];
        bytes.extend_from_slice(b"1.2.");
        bytes.extend_from_slice(&dataset);
        std::fs::write(&ok_path, &bytes).unwrap();
        {
            let mut f = std::fs::File::open(&ok_path).unwrap();
            let (block, len) = read_meta_block(&mut f, 0, bytes.len() as u64).unwrap();
            assert_eq!(len as usize, 12);
            assert_eq!(&block[..], &bytes[..12]);
        }

        // a meta group larger than the first 4 KiB window exercises the growing pass
        let big_path = dir.join("big.dcm");
        let mut big = vec![0x02, 0x00, 0x10, 0x00, b'U', b'I'];
        big.extend_from_slice(&5000u16.to_le_bytes());
        big.extend_from_slice(&vec![b'7'; 5000]);
        let meta_len = big.len();
        big.extend_from_slice(&dataset);
        std::fs::write(&big_path, &big).unwrap();
        {
            let mut f = std::fs::File::open(&big_path).unwrap();
            let (block, len) = read_meta_block(&mut f, 0, big.len() as u64).unwrap();
            assert_eq!(len as usize, meta_len);
            assert_eq!(block.len(), meta_len);
            assert_eq!(&block[..], &big[..meta_len]);
        }

        // truncated: the element declares 100 bytes but the file ends after 4
        let trunc_path = dir.join("trunc.dcm");
        let mut trunc = vec![0x02, 0x00, 0x10, 0x00, b'U', b'I', 0x64, 0x00];
        trunc.extend_from_slice(b"abcd");
        std::fs::write(&trunc_path, &trunc).unwrap();
        {
            let mut f = std::fs::File::open(&trunc_path).unwrap();
            let err = read_meta_block(&mut f, 0, trunc.len() as u64).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(err.to_string(), "meta group is truncated");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_inputs_do_not_panic() {
        // Parsing untrusted bytes must never panic — only return Ok/Err.
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0u8; 4],
            vec![0xFFu8; 128],
            {
                let mut v = vec![0u8; 128];
                v.extend_from_slice(b"DICM");
                v
            },
            {
                let mut v = vec![0u8; 128];
                v.extend_from_slice(b"DICM");
                v.extend_from_slice(&[0x02, 0x00, 0x00, 0x00]); // truncated meta tag
                v
            },
            {
                let mut v = vec![0u8; 128];
                v.extend_from_slice(b"DICM");
                v.extend_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x55, 0x4C, 0x04, 0x00, 0, 0, 0, 0]);
                v.extend_from_slice(&[0x08, 0x00, 0x05, 0x00, 0x4C, 0x4F, 0x10, 0x00, 0x41]); // lies about length
                v
            },
        ];
        for bytes in &cases {
            let _ = DicomFile::from_bytes(bytes, &ReadOptions::default());
        }
        // truncating a real file at every byte length must not panic either
        let good = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        for n in 0..good.len() {
            let _ = DicomFile::from_bytes(&good[..n], &ReadOptions::default());
        }
    }

    #[test]
    fn deflated_file_round_trips() {
        let mut file = sample_file();
        file.dataset.set_transfer_syntax(syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone());
        let bytes = file.to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.get_str(Tag::PATIENT_NAME), "Doe^John");
        assert!(back.dataset.transfer_syntax().is_deflate());
    }

    #[test]
    fn deflated_file_opens_from_disk_like_its_uncompressed_twin() {
        let dir = std::env::temp_dir().join(format!("radicom-deflate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let deflated_path = dir.join("deflated.dcm");
        let plain_path = dir.join("plain.dcm");

        let mut deflated = sample_file();
        deflated.dataset.set_transfer_syntax(syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone());
        std::fs::write(&deflated_path, deflated.to_bytes(&WriteOptions::default()).unwrap()).unwrap();
        std::fs::write(&plain_path, sample_file().to_bytes(&WriteOptions::default()).unwrap()).unwrap();

        // `open` goes through the streaming inflate path; both must yield the same data
        let from_disk = DicomFile::open(&deflated_path).unwrap();
        let plain = DicomFile::open(&plain_path).unwrap();
        assert!(from_disk.dataset.transfer_syntax().is_deflate());
        assert_eq!(from_disk.get_str(Tag::PATIENT_NAME), plain.get_str(Tag::PATIENT_NAME));
        assert_eq!(from_disk.get_str(Tag::PATIENT_ID), plain.get_str(Tag::PATIENT_ID));
        assert_eq!(from_disk.dataset.len(), plain.dataset.len());

        // a truncated deflate stream is a typed error, never a panic
        let bytes = std::fs::read(&deflated_path).unwrap();
        let cut = dir.join("cut.dcm");
        std::fs::write(&cut, &bytes[..bytes.len() - 4]).unwrap();
        match DicomFile::open(&cut) {
            Err(Error::Reader(crate::error::ReaderError::Other(message))) => {
                assert!(message.contains("deflate"), "typed error, got {message}");
            }
            other => panic!("expected ReaderError::Other(deflate), got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sequences_and_fragments_survive_a_file_round_trip() {
        let mut file = sample_file();
        let seq = file.dataset.insert_sequence(Tag::REFERENCED_IMAGE_SEQUENCE);
        let mut item = Dataset::new();
        item.insert_str(Tag::REFERENCED_SOP_CLASS_UID, Vr::Ui, "1.2.3");
        seq.push(item);
        let frags = file.dataset.insert_fragments(Tag::PIXEL_DATA, Vr::Ob);
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
        assert_eq!(back.get_str(Tag::PATIENT_ID), "PID-1");
        assert_eq!(read_file_meta(&path).unwrap().get_str(Tag::TRANSFER_SYNTAX_UID).unwrap(), "1.2.840.10008.1.2.1");
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
        assert!(file.dataset.contains(&Tag::SOP_CLASS_UID), "elements before the stop tag");
        assert!(!file.dataset.contains(&Tag::PATIENT_NAME), "stop tag not read");
        assert!(!file.dataset.contains(&Tag::PATIENT_ID), "nothing past the stop tag");
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

    fn temp_file(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "radicom-filetest-{}-{}-{name}.dcm",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Writes a file whose (0009,1001) OB value is `big_len` bytes (>= any test
    /// threshold) followed by a small (0010,0020). Returns (path, big bytes).
    fn file_with_large_value(big_len: usize) -> (PathBuf, Vec<u8>) {
        let big: Vec<u8> = (0..big_len).map(|i| (i % 251) as u8).collect();
        let mut ds = Dataset::new();
        ds.insert_or_update(crate::model::element::DicomElement::new_bytes(
            Tag::new(0x0009, 0x1001),
            Vr::Ob,
            std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(big.clone())),
        ));
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        let file = DicomFile::from_dataset(ds);
        let path = temp_file("large");
        file.save(&path).unwrap();
        (path, big)
    }

    fn large_value_buffer(ds: &Dataset) -> &crate::model::buffer::SharedBuffer {
        match ds.item(&Tag::new(0x0009, 0x1001)) {
            Some(crate::model::item::DicomItem::Element(e)) => e.value().as_buffer().expect("value has a buffer"),
            other => panic!("expected element, got {other:?}"),
        }
    }

    #[test]
    fn read_large_on_demand_yields_a_lazy_window() {
        let (path, big) = file_with_large_value(5000);
        {
            let opts = ReadOptions { read: FileReadOption::ReadLargeOnDemand, large_object_size: 1000 };
            let file = DicomFile::open_with_options(&path, &opts).unwrap();
            let buffer = large_value_buffer(&file.dataset);
            assert_eq!(buffer.size(), 5000);
            assert!(!buffer.is_memory(), "large value must stay a file window, not in memory");
            // range reads and streaming go straight to disk; only data() caches
            let mut slice = [0u8; 16];
            buffer.get_byte_range(1000, &mut slice);
            assert_eq!(slice, big[1000..1016], "a range read returns the file's bytes");
            let mut streamed = Vec::new();
            assert_eq!(buffer.copy_to(&mut streamed).unwrap(), 5000);
            assert_eq!(streamed, big, "streaming returns the whole value in chunks");
            assert_eq!(&*buffer.data(), &big[..], "window reads the real bytes");
            assert_eq!(file.dataset.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_all_materializes_large_values() {
        let (path, big) = file_with_large_value(5000);
        {
            let opts = ReadOptions { read: FileReadOption::ReadAll, large_object_size: 1000 };
            let file = DicomFile::open_with_options(&path, &opts).unwrap();
            let buffer = large_value_buffer(&file.dataset);
            assert_eq!(buffer.size(), 5000);
            assert!(buffer.is_memory(), "ReadAll keeps the value in memory");
            assert_eq!(&*buffer.data(), &big[..]);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn skip_large_tags_leaves_a_placeholder_and_advances() {
        let (path, _big) = file_with_large_value(5000);
        {
            let opts = ReadOptions { read: FileReadOption::SkipLargeTags, large_object_size: 1000 };
            let file = DicomFile::open_with_options(&path, &opts).unwrap();
            let buffer = large_value_buffer(&file.dataset);
            assert_eq!(buffer.size(), 0, "skipped value is an empty placeholder");
            assert!(buffer.data().is_empty());
            // the parser still advanced past the skipped value to the next element
            assert_eq!(file.dataset.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        }
        std::fs::remove_file(&path).ok();
    }
}
