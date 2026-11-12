//! DICOM file container: format detection, file meta, open and save
//! (FILE-01..06; docs/01-architecture.md §7).

use crate::error::{Error, FileError};
use crate::io::byte_source::ByteSourceReader;
use crate::io::dataset_builder::{parse_dataset_bytes, DatasetBuilder};
use crate::io::reader::{DicomReader, LargeValuePolicy};
use crate::io::writer::{DicomWriter, WriteOptions};
use crate::model::dataset::Dataset;
use crate::model::endian::Endian;
use crate::model::tag::Tag;
use crate::model::transfer_syntax::{syntaxes, TransferSyntax};
use crate::model::uid::Uid;
use crate::model::vr::Vr;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The directory `save_with` writes its sibling temp file into.
///
/// A path with no parent (a bare filename) resolves to the current directory.
fn save_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Implementation identity written into the file meta group (FILE-04).
///
/// The version name embeds the workspace version at compile time, so a release
/// bump renames the identity a peer sees without a second edit. The space and
/// dots are inside the SH repertoire (PS3.5 §6.2), matching the
/// `PYDICOM 3.0.1` / `fo-dicom 5.16.7` convention; the network crate reuses
/// this constant for its user information (§11.2), so both identities agree.
pub const IMPLEMENTATION_CLASS_UID: &str = "1.2.826.0.1.3680043.10.1330.1";
pub const IMPLEMENTATION_VERSION_NAME: &str = concat!("RADICOM ", env!("CARGO_PKG_VERSION"));

// (0002,0013) is SH: 16 bytes maximum. A pre-release version does not fit
// ("RADICOM 1.0.0-rc.1"), so fail the build instead of writing a non-conformant
// value to every saved file.
const _: () = assert!(
    IMPLEMENTATION_VERSION_NAME.len() <= 16,
    "Implementation Version Name exceeds the SH limit of 16 bytes"
);

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
    /// Character encoding for text in a dataset that carries no (0008,0005),
    /// e.g. legacy Latin-1 or Shift-JIS archives (H1; fo-dicom
    /// `DicomFile.Open(..., Encoding fallbackEncoding)`). `None` decodes the
    /// default repertoire (ISO-IR 6).
    pub fallback_encoding: Option<&'static encoding_rs::Encoding>,
    /// Stream input up to this many bytes is parsed from memory; anything larger
    /// is spooled to a temporary file and parsed with the file's lazy value
    /// windows, so the whole instance is never held in RAM (M2, default 8 MiB).
    pub stream_spool_threshold: u64,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions {
            read: FileReadOption::Default,
            large_object_size: 64 * 1024,
            fallback_encoding: None,
            stream_spool_threshold: 8 * 1024 * 1024,
        }
    }
}

impl ReadOptions {
    /// These options with a fallback character encoding (H1).
    pub fn with_fallback_encoding(mut self, encoding: &'static encoding_rs::Encoding) -> Self {
        self.fallback_encoding = Some(encoding);
        self
    }

    /// The fallback list handed to the parser and stored on the dataset, so a
    /// save re-encodes the text the same way it was decoded (H1).
    fn fallback_encodings(&self) -> Vec<&'static encoding_rs::Encoding> {
        self.fallback_encoding.into_iter().collect()
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
    /// The 128-byte preamble as read from disk (zeroes when there was none), so
    /// a write preserves it instead of always emitting an empty preamble.
    preamble: [u8; 128],
    /// Identity of the source, used in error context.
    source: Option<PathBuf>,
    /// Owns the temporary file a stream was spooled into (M2). It must outlive
    /// the dataset's value windows, which the field order guarantees: `dataset`
    /// (and its windows) drops first, then this deletes the file.
    spooled: Option<Arc<crate::model::buffer::TempFileSpill>>,
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
            preamble: [0u8; 128],
            source: None,
            spooled: None,
        }
    }

    /// Builds a dataset-backed file that owns the temporary spill containing
    /// its lazy value windows.
    pub fn from_dataset_with_spool(
        dataset: Dataset,
        spooled: std::sync::Arc<crate::model::buffer::TempFileSpill>,
    ) -> Self {
        let mut file = Self::from_dataset(dataset);
        file.spooled = Some(spooled);
        file
    }

    /// Opens and parses a file from disk (FILE-01, default read options).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with_options(path, &ReadOptions::default())
    }

    /// Opens a file whose text carries no (0008,0005) with `encoding` (H1;
    /// fo-dicom `DicomFile.Open(path, fallbackEncoding)`).
    pub fn open_with_encoding(
        path: impl AsRef<Path>,
        encoding: &'static encoding_rs::Encoding,
    ) -> Result<Self, Error> {
        Self::open_with_options(
            path,
            &ReadOptions::default().with_fallback_encoding(encoding),
        )
    }

    /// Opens a file honoring the [`ReadOptions`] large-value policy (FILE-02).
    ///
    /// The header and meta group are read with a bounded scan; the dataset is
    /// then parsed directly from the file so large values stay as lazy
    /// [`crate::model::buffer::FileWindow`]s instead of being copied into memory. A window serves
    /// range reads and `copy_to` streaming straight from disk; only `data()`
    /// materializes the value, once, because the buffer API returns `&[u8]`.
    pub fn open_with_options(path: impl AsRef<Path>, options: &ReadOptions) -> Result<Self, Error> {
        let path = path.as_ref();
        let mut file = std::fs::File::open(path).map_err(|e| file_err(path, "open", e))?;
        let file_len = file
            .metadata()
            .map_err(|e| file_err(path, "stat", e))?
            .len();
        if file_len < 8 {
            return Err(Error::File(FileError::InvalidFormat(
                "file is too short to be DICOM".into(),
            )));
        }

        // preamble / header
        let head_len = file_len.min(132) as usize;
        let mut head = vec![0u8; head_len];
        file.read_exact(&mut head)
            .map_err(|e| file_err(path, "read", e))?;
        let mut preamble = [0u8; 128];
        let (mut format, meta_start) = if head_len >= 132 && &head[128..132] == b"DICM" {
            preamble.copy_from_slice(&head[..128]);
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
        let syntax = match file_meta
            .try_get_str(Tag::TRANSFER_SYNTAX_UID)
            .and_then(|uid| crate::model::transfer_syntax::parse(uid.trim()).ok())
        {
            Some(syntax) => syntax,
            None => {
                let probe = read_probe(&mut file, dataset_offset, file_len);
                dataset_syntax(format, &probe, file_len - dataset_offset)?
            }
        };

        let (mut dataset, outcome) = if syntax.is_deflate() {
            // Stream the inflate straight from the file: the compressed body is
            // never held in memory, so the peak is one full (inflated) copy plus
            // the codec window rather than compressed + inflated.
            let plain = crate::io::reader::inflate_stream(&mut file, dataset_offset)?;
            parse_slice(&plain, &syntax, options)?
        } else {
            file.seek(SeekFrom::Start(dataset_offset))
                .map_err(|e| file_err(path, "seek", e))?;
            // Buffered reads: the parser issues several small reads per element,
            // and each one used to be its own syscall. This is safe now that the
            // source tracks its own position instead of asking the stream, so the
            // buffer is never consulted for offsets.
            let buffered = std::io::BufReader::with_capacity(64 * 1024, file);
            // one shared handle for every lazy window over this file, rather
            // than one fd per large value
            let shared = Arc::new(
                buffered
                    .get_ref()
                    .try_clone()
                    .map_err(|e| file_err(path, "clone", e))?,
            );
            let mut source =
                ByteSourceReader::new(buffered, syntax.endian()).with_shared_file(shared);
            let reader = DicomReader {
                large_object_size: options.large_object_size,
                large_policy: policy_of(options.read),
                partial_on_eof: true,
                ..DicomReader::default()
            };
            let mut builder = DatasetBuilder::with_transfer_syntax_and_fallback(
                syntax.clone(),
                options.fallback_encodings(),
            );
            let outcome = reader.read(&mut source, &syntax, &mut builder, None)?;
            (builder.into_dataset(), outcome)
        };
        dataset.set_transfer_syntax(syntax.clone());

        // the ACR-NEMA recognition code lives in the *dataset* (0008,0010)
        let format = match dataset.try_get_str_ref(Tag::RECOGNITION_CODE) {
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
            preamble,
            source: Some(path.to_path_buf()),
            spooled: None,
        })
    }

    /// Parses from a byte slice.
    ///
    /// The slice is parsed in place: it is not copied into a `Vec` first, which
    /// matters for the C-STORE sink that already holds the dataset in memory.
    pub fn from_bytes(bytes: &[u8], options: &ReadOptions) -> Result<Self, Error> {
        Self::parse_raw(bytes, options)
    }

    /// Parses from any reader (FILE-01, M2).
    ///
    /// A stream up to [`ReadOptions::stream_spool_threshold`] bytes is parsed
    /// from memory. Anything larger is spooled to a temporary file — never
    /// `read_to_end`ed into RAM — and parsed with the file's lazy value windows;
    /// the temporary file is owned by the returned [`DicomFile`] and deleted when
    /// it (and every value window in it) is dropped.
    pub fn from_reader<R: Read>(reader: &mut R, options: &ReadOptions) -> Result<Self, Error> {
        let limit = options.stream_spool_threshold;
        // read one byte past the threshold: that is enough to tell which path to
        // take, and the prefix is reused by the spool. The `Take` is scoped, so
        // the spool continues from the reader itself and never loses its tail.
        let mut head = Vec::new();
        {
            let mut window = (&mut *reader).take(limit.saturating_add(1));
            window
                .read_to_end(&mut head)
                .map_err(|e| Error::File(FileError::Other(format!("read: {e}"))))?;
        }
        if head.len() as u64 <= limit {
            let mut file = Self::parse_raw(&head, options)?;
            file.source = None;
            return Ok(file);
        }
        let (path, len) = spool_stream(&head, reader)?;
        let mut file = Self::open_with_options(&path, options)?;
        // a stream has no path of its own: the spool is an implementation detail
        file.source = None;
        file.spooled = Some(Arc::new(
            crate::model::buffer::TempFileSpill::from_temp_file(path, len)
                .map_err(|e| Error::File(FileError::Other(format!("spool: {e}"))))?,
        ));
        Ok(file)
    }

    /// Parses a complete file image already in memory.
    fn parse_raw(raw: &[u8], options: &ReadOptions) -> Result<Self, Error> {
        let (format, meta_bytes, dataset_bytes) = detect(raw)?;
        let mut preamble = [0u8; 128];
        if format == FileFormat::Dicom3 {
            preamble.copy_from_slice(&raw[..128]);
        }

        // file meta is always Explicit VR Little Endian
        let file_meta = match meta_bytes {
            Some(meta) => parse_dataset_bytes(meta, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN)?,
            None => Dataset::new(),
        };

        let syntax = match file_meta
            .try_get_str(Tag::TRANSFER_SYNTAX_UID)
            .and_then(|uid| crate::model::transfer_syntax::parse(uid.trim()).ok())
        {
            Some(syntax) => syntax,
            None => dataset_syntax(format, dataset_bytes, dataset_bytes.len() as u64)?,
        };

        let (mut dataset, outcome) = parse_dataset_with(dataset_bytes, &syntax, options)?;
        dataset.set_transfer_syntax(syntax);
        // the ACR-NEMA recognition code lives in the *dataset* (0008,0010)
        let format = match dataset.try_get_str_ref(Tag::RECOGNITION_CODE) {
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
            preamble,
            source: None,
            spooled: None,
        })
    }

    /// Parses until `stop` returns true, leaving the rest unread (FILE-01).
    pub fn open_partial(
        path: impl AsRef<Path>,
        stop: &dyn Fn(&crate::io::reader::ParseState) -> bool,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| file_err(path, "read", e))?;
        let (format, meta_bytes, dataset_bytes) = detect(&bytes).map_err(|e| attach(e, path))?;
        let mut preamble = [0u8; 128];
        if format == FileFormat::Dicom3 {
            preamble.copy_from_slice(&bytes[..128]);
        }
        let file_meta = match meta_bytes {
            Some(meta) => parse_dataset_bytes(meta, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN)?,
            None => Dataset::new(),
        };
        let syntax = match file_meta
            .try_get_str(Tag::TRANSFER_SYNTAX_UID)
            .and_then(|uid| crate::model::transfer_syntax::parse(uid.trim()).ok())
        {
            Some(syntax) => syntax,
            None => dataset_syntax(format, dataset_bytes, dataset_bytes.len() as u64)?,
        };

        // A deflated transfer syntax stores the dataset as raw DEFLATE. Inflate
        // it before parsing; otherwise the compressed bytes would be misread as
        // Explicit VR elements (RD-04, matching the streaming open path).
        let inflated;
        let dataset_bytes: &[u8] = if syntax.is_deflate() {
            inflated = crate::io::reader::inflate(dataset_bytes)?;
            &inflated
        } else {
            dataset_bytes
        };
        let mut source = ByteSourceReader::new(Cursor::new(dataset_bytes), syntax.endian());
        let mut builder = DatasetBuilder::with_transfer_syntax(syntax.clone());
        let outcome =
            DicomReader::default().read(&mut source, &syntax, &mut builder, Some(stop))?;
        Ok(DicomFile {
            file_meta,
            dataset: builder.into_dataset(),
            format,
            is_partial: outcome != crate::io::reader::ReadOutcome::Success,
            preamble,
            source: Some(path.to_path_buf()),
            spooled: None,
        })
    }

    /// Writes the file to disk (FILE-05).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        self.save_with(path, &WriteOptions::default())
    }
    /// Writes the file to disk using the given write options
    /// (fo-dicom `Save(string, DicomWriteOptions)`).
    ///
    /// The file is streamed through the writer (M2): no `Vec` of the whole file
    /// is built. It goes to a sibling temporary file that is renamed into place,
    /// so a failure mid-write never destroys an existing destination.
    ///
    /// ponytail: the rename replaces a destination symlink instead of writing
    /// through it; follow the link here if that ever matters to a caller.
    pub fn save_with(&self, path: impl AsRef<Path>, options: &WriteOptions) -> Result<(), Error> {
        let path = path.as_ref();
        let dir = save_directory(path);
        let (temp_path, mut file) = crate::io::temp::create_unique_file(dir, "radicom-save", "tmp")
            .map_err(|e| file_err(path, "create", e))?;
        let result = self
            .write_stream(&mut file, options)
            .and_then(|()| file.flush().map_err(|e| file_err(path, "write", e)));
        drop(file);
        match result {
            Ok(()) => std::fs::rename(&temp_path, path).map_err(|e| {
                let _ = std::fs::remove_file(&temp_path);
                file_err(path, "rename", e)
            }),
            Err(error) => {
                let _ = std::fs::remove_file(&temp_path);
                Err(error)
            }
        }
    }

    /// Serializes preamble, meta group and dataset into memory (FILE-05).
    ///
    /// Kept for callers that need the whole image as bytes; `save_with`/`write`
    /// stream instead (M2).
    pub fn to_bytes(&self, options: &WriteOptions) -> Result<Vec<u8>, Error> {
        #[cfg(test)]
        TO_BYTES_CALLS.with(|count| count.set(count.get() + 1));
        let mut out = Vec::new();
        self.write_stream(&mut out, options)?;
        Ok(out)
    }

    /// The one streaming implementation behind [`Self::to_bytes`], [`Self::write`]
    /// and [`Self::save_with`] (M2): preamble, meta group, then the dataset —
    /// deflated in flight when the transfer syntax asks for it.
    pub(crate) fn write_stream<W: Write>(
        &self,
        out: &mut W,
        options: &WriteOptions,
    ) -> Result<(), Error> {
        if !self.format.is_saveable() {
            return Err(Error::File(FileError::FormatNotSaveable(format!(
                "{:?}",
                self.format
            ))));
        }
        out.write_all(&self.preamble)
            .and_then(|()| out.write_all(b"DICM"))
            .map_err(|e| Error::File(FileError::Other(format!("write: {e}"))))?;
        let meta = self.refreshed_meta()?;
        // PS3.10 Table 7.1-1: (0002,0000) File Meta Information Group Length is
        // Type 1 (required), so always recalculate it for the meta group whatever
        // the caller's dataset option says.
        let meta_options = WriteOptions {
            keep_group_lengths: true,
            ..options.clone()
        };
        DicomWriter::new(meta_options).write_dataset(
            &meta,
            &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN,
            out,
        )?;
        // the dataset keeps its own syntax; deflated bodies are compressed
        let syntax = self.dataset.transfer_syntax();
        if syntax.is_deflate() {
            let mut counted = CountingWriter {
                inner: out,
                written: 0,
            };
            {
                let mut encoder = flate2::write::DeflateEncoder::new(
                    &mut counted,
                    flate2::Compression::default(),
                );
                DicomWriter::new(options.clone()).write_dataset(
                    &self.dataset,
                    &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN,
                    &mut encoder,
                )?;
                encoder.finish().map_err(|e| {
                    Error::Writer(crate::error::WriterError(format!("deflate: {e}")))
                })?;
            }
            // PS3.5 A.5: a deflated file is padded to an even length
            if counted.written % 2 == 1 {
                counted
                    .inner
                    .write_all(&[0])
                    .map_err(|e| Error::File(FileError::Other(format!("write: {e}"))))?;
            }
        } else {
            DicomWriter::new(options.clone()).write_dataset(&self.dataset, &syntax, out)?;
        }
        Ok(())
    }

    /// Length of the 128-byte preamble, `DICM` magic and encoded file meta
    /// group, without serializing the dataset. Used to place record offsets
    /// while writing a media storage directory (FILE-05).
    pub(crate) fn encoded_prefix_len(&self, options: &WriteOptions) -> Result<u64, Error> {
        if !self.format.is_saveable() {
            return Err(Error::File(FileError::FormatNotSaveable(format!(
                "{:?}",
                self.format
            ))));
        }
        let meta = self.refreshed_meta()?;
        let meta_options = WriteOptions {
            keep_group_lengths: true,
            ..options.clone()
        };
        let meta_len = DicomWriter::new(meta_options)
            .dataset_length(&meta, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN)?;
        Ok(132 + meta_len)
    }

    /// Writes to an arbitrary writer, streaming (M2).
    pub fn write<W: Write>(&self, out: &mut W, options: &WriteOptions) -> Result<(), Error> {
        self.write_stream(out, options)
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
        // PS3.10 Table 7.1-1: FileMetaInformationVersion is Type 1 and always
        // 00\01, even when an incoming meta group omitted it (fo-dicom
        // DicomFileMetaInformation always sets Version).
        if !meta.contains(&Tag::FILE_META_INFORMATION_VERSION) {
            meta.insert_value(Tag::FILE_META_INFORMATION_VERSION, vec![0x00u8, 0x01u8]);
        }
        let syntax = self.dataset.transfer_syntax();
        meta.insert_str(Tag::TRANSFER_SYNTAX_UID, Vr::Ui, syntax.uid());
        meta.insert_str(
            Tag::IMPLEMENTATION_CLASS_UID,
            Vr::Ui,
            IMPLEMENTATION_CLASS_UID,
        );
        meta.insert_str(
            Tag::IMPLEMENTATION_VERSION_NAME,
            Vr::Sh,
            IMPLEMENTATION_VERSION_NAME,
        );
        // The media storage SOP class must describe the dataset's own SOP, never
        // a guessed secondary-capture default; an inconsistent meta group is an
        // error, and a dataset with no SOP class cannot build a file (review #8).
        let dataset_sop_class = self.dataset.try_get_str(Tag::SOP_CLASS_UID);
        let existing_sop_class = meta.try_get_str(Tag::MEDIA_STORAGE_SOP_CLASS_UID);
        match (dataset_sop_class, existing_sop_class) {
            (Some(class), Some(existing)) => {
                if existing != class.as_str() {
                    return Err(Error::File(FileError::Other(format!(
                        "file meta MediaStorageSOPClassUID {existing} disagrees with dataset SOPClassUID {class}"
                    ))));
                }
            }
            (Some(class), None) => {
                meta.insert_str(Tag::MEDIA_STORAGE_SOP_CLASS_UID, Vr::Ui, class);
            }
            (None, Some(_)) => {}
            (None, None) => {
                return Err(Error::File(FileError::Other(
                    "dataset has no SOPClassUID; cannot build file meta information".into(),
                )));
            }
        }
        // IO-02: the Media Storage SOP Instance must describe the dataset's own
        // SOP instance on every save (PS3.10 Table 7.1-1), exactly like the class
        // UID above. Only filling it when absent left a stale instance UID after a
        // re-save that changed (e.g. regenerated) the dataset's SOPInstanceUID.
        match (
            self.dataset.try_get_str(Tag::SOP_INSTANCE_UID),
            meta.try_get_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID),
        ) {
            (Some(instance), Some(existing)) => {
                if existing != instance.as_str() {
                    meta.insert_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID, Vr::Ui, instance);
                }
            }
            (Some(instance), None) => {
                meta.insert_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID, Vr::Ui, instance);
            }
            (None, Some(_)) => {}
            (None, None) => {
                meta.insert_str(
                    Tag::MEDIA_STORAGE_SOP_INSTANCE_UID,
                    Vr::Ui,
                    Uid::generate_value(),
                );
            }
        }
        // the file meta group is always Explicit VR LE; drop the stale length and
        // seed a placeholder, because the writer recalculates a present
        // (0002,0000) but no longer invents a missing one (L2)
        meta.insert_value_with_vr(Tag::FILE_META_INFORMATION_GROUP_LENGTH, Vr::Ul, 0u32);
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
    let mut builder = DatasetBuilder::with_transfer_syntax_and_fallback(
        Arc::new(syntax.clone()),
        options.fallback_encodings(),
    );
    let outcome = reader.read(&mut source, syntax, &mut builder, None)?;
    Ok((builder.into_dataset(), outcome))
}

/// Builds lazy [`crate::model::buffer::FileWindow`]s over a file for the parser (FILE-02).
///
/// Holds one shared `Arc<File>`; every window clone-references it and reads
/// with positional I/O, so a file with many large values still holds one fd.
/// Reads up to 4 KiB at `start` for transfer-syntax probing.
fn read_probe(file: &mut std::fs::File, start: u64, file_len: u64) -> Vec<u8> {
    let take = file_len.saturating_sub(start).min(4096) as usize;
    let mut buf = vec![0u8; take];
    if file
        .seek(SeekFrom::Start(start))
        .and_then(|_| file.read_exact(&mut buf))
        .is_err()
    {
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
fn read_meta_block(
    file: &mut std::fs::File,
    start: u64,
    file_len: u64,
) -> std::io::Result<(Vec<u8>, u64)> {
    // CORE-22: never truncate the remaining file size into `usize`
    let avail = usize::try_from(file_len.saturating_sub(start)).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file is larger than the platform address space",
        )
    })?;
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
            MetaScan::Run(len) => {
                if len == buf.len() && take < avail {
                    // IO-08: the window ended exactly on an element boundary but
                    // more bytes remain, so the meta group may continue past the
                    // window; grow instead of declaring it complete.
                    want = take.saturating_mul(2).min(avail);
                } else {
                    return Ok((buf[..len].to_vec(), len as u64));
                }
            }
            MetaScan::None => return Ok((Vec::new(), 0)),
            MetaScan::More => {
                if take == avail {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "meta group is truncated",
                    ));
                }
                // A bogus element length makes `scan_meta` ask for more on every
                // pass; cap the growth so a crafted file cannot force the whole
                // file into memory (a real meta group is tiny).
                const MAX_META_BYTES: usize = 4 * 1024 * 1024;
                if take >= MAX_META_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "meta group exceeds the maximum size",
                    ));
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
            (
                u32::from_le_bytes([buf[pos + 8], buf[pos + 9], buf[pos + 10], buf[pos + 11]])
                    as usize,
                12,
            )
        };
        let end = match pos.checked_add(header).and_then(|p| p.checked_add(len)) {
            Some(end) => end,
            // CORE-22: overflow means "more bytes needed", never a wrapped
            // length that would slice past the buffer
            None => return MetaScan::More,
        };
        if end > buf.len() {
            return MetaScan::More;
        }
        pos = end;
    }
    // all buffered bytes consumed; if that is exactly an element boundary the
    // run is complete (the caller decides whether more bytes remain), otherwise
    // a partial header/value is still outstanding
    if pos == buf.len() && pos > 0 {
        MetaScan::Run(pos)
    } else {
        MetaScan::More
    }
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
    let format = if meta.is_some() {
        FileFormat::Dicom3NoPreamble
    } else {
        FileFormat::Dicom3NoFileMetaInfo
    };
    Ok((format, meta, dataset))
}

/// Splits leading group 0002 elements from the dataset that follows.
fn split_meta(raw: &[u8]) -> Result<(Option<&[u8]>, &[u8]), Error> {
    if raw.len() < 8 {
        return Err(Error::File(FileError::InvalidFormat(
            "truncated header".into(),
        )));
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
            Error::File(FileError::InvalidFormat(format!(
                "bad meta VR at offset {pos}"
            )))
        })?;
        let (len, header) = if vr.is_16bit_length() {
            (u16::from_le_bytes([raw[pos + 6], raw[pos + 7]]) as usize, 8)
        } else {
            if pos + 12 > raw.len() {
                break;
            }
            (
                u32::from_le_bytes([raw[pos + 8], raw[pos + 9], raw[pos + 10], raw[pos + 11]])
                    as usize,
                12,
            )
        };
        let end = match pos.checked_add(header).and_then(|p| p.checked_add(len)) {
            Some(end) => end,
            // CORE-22: a declared length near u32::MAX must not wrap `usize`
            None => {
                return Err(Error::File(FileError::InvalidFormat(
                    "meta group element length overflows the address space".into(),
                )))
            }
        };
        if end > raw.len() {
            return Err(Error::File(FileError::InvalidFormat(
                "meta group is truncated".into(),
            )));
        }
        pos = end;
    }
    if pos == 0 {
        return Ok((None, raw));
    }
    Ok((Some(&raw[..pos]), &raw[pos..]))
}

/// Fallback syntax for a dataset with no meta information (FILE-03).
///
/// The first tag's group is not a reliable byte-order signal: a dataset may
/// legitimately start with a large group such as `(7FE0,XXXX)` or a private
/// group, which a "group > 0x00FF ⇒ Big Endian" heuristic misreads and turns
/// into an empty dataset. Instead, each plausible syntax is checked against the
/// first element header; an unparsable bare dataset is an explicit error.
fn dataset_syntax(
    format: FileFormat,
    dataset: &[u8],
    total_len: u64,
) -> Result<Arc<TransferSyntax>, Error> {
    let explicit = looks_explicit(dataset);
    // ACR-NEMA 2.0 is big-endian; otherwise little-endian is the norm.
    let order = if format == FileFormat::AcrNema2 {
        [true, false]
    } else {
        [false, true]
    };
    for big_endian in order {
        let syntax = match (explicit, big_endian) {
            (true, true) => syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone(),
            (true, false) => syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            (false, true) => syntaxes::IMPLICIT_VR_BIG_ENDIAN.clone(),
            (false, false) => syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(),
        };
        if first_element_fits(dataset, &syntax, total_len) {
            return Ok(syntax);
        }
    }
    Err(Error::File(FileError::InvalidFormat(
        "cannot determine the transfer syntax of a dataset without file meta information".into(),
    )))
}

/// True when the first element header under `syntax` is structurally consistent
/// with the available byte count. `total_len` is the whole dataset length (the
/// `dataset` slice may be only a bounded probe).
fn first_element_fits(dataset: &[u8], syntax: &TransferSyntax, total_len: u64) -> bool {
    if dataset.len() < 8 || total_len < 8 {
        return false;
    }
    let endian = syntax.endian();
    let (value_len, header) = if syntax.is_explicit_vr() {
        let Some(vr) = Vr::try_parse(&[dataset[4], dataset[5]]) else {
            return false;
        };
        if vr.is_16bit_length() {
            (read_u16(dataset, 6, endian) as u64, 8u64)
        } else {
            if dataset.len() < 12 {
                return false;
            }
            (read_u32(dataset, 8, endian) as u64, 12u64)
        }
    } else {
        (read_u32(dataset, 4, endian) as u64, 8u64)
    };
    if value_len == 0xFFFF_FFFF {
        return true;
    }
    header + value_len <= total_len
}

fn read_u16(bytes: &[u8], at: usize, endian: Endian) -> u16 {
    let pair = [bytes[at], bytes[at + 1]];
    match endian {
        Endian::Little => u16::from_le_bytes(pair),
        Endian::Big => u16::from_be_bytes(pair),
    }
}

fn read_u32(bytes: &[u8], at: usize, endian: Endian) -> u32 {
    let quad = [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
    match endian {
        Endian::Little => u32::from_le_bytes(quad),
        Endian::Big => u32::from_be_bytes(quad),
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

// Counts in-memory serializations on the current thread, so a test can prove
// that the streaming save path never enters `to_bytes` (M2 seam). Thread-local
// so parallel tests cannot inflate it.
#[cfg(test)]
thread_local! {
    pub(crate) static TO_BYTES_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A writer that counts the bytes it forwards, so a deflated body can be padded
/// to an even length without ever buffering it (M2).
struct CountingWriter<'a, W: Write> {
    inner: &'a mut W,
    written: u64,
}

impl<W: Write> Write for CountingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Copies an already-read prefix plus the rest of a stream into a fresh
/// temporary file, returning its path and length (M2). The caller owns deletion.
fn spool_stream<R: Read>(head: &[u8], stream: &mut R) -> Result<(PathBuf, u64), Error> {
    let (path, mut file) =
        crate::io::temp::create_unique_file(&std::env::temp_dir(), "radicom-stream", "tmp")
            .map_err(|e| Error::File(FileError::Other(format!("spool create: {e}"))))?;
    let result = (|| -> Result<u64, Error> {
        file.write_all(head).map_err(spool_err)?;
        let mut len = head.len() as u64;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let read = stream.read(&mut chunk).map_err(spool_err)?;
            if read == 0 {
                break;
            }
            file.write_all(&chunk[..read]).map_err(spool_err)?;
            len += read as u64;
        }
        file.flush().map_err(spool_err)?;
        Ok(len)
    })();
    match result {
        Ok(len) => Ok((path, len)),
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            Err(error)
        }
    }
}

/// Maps a stream/copy failure while spooling (M2).
fn spool_err(e: std::io::Error) -> Error {
    Error::File(FileError::Other(format!("spool: {e}")))
}

fn file_err(path: &Path, action: &str, e: std::io::Error) -> Error {
    Error::File(FileError::Other(format!(
        "{action} {}: {e}",
        path.display()
    )))
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
///
/// Only the header and the leading group-0002 bytes are read; the dataset is
/// never loaded (review CORE-10).
pub fn read_file_meta(path: impl AsRef<Path>) -> Result<Dataset, Error> {
    let path = path.as_ref();
    let mut file = std::fs::File::open(path).map_err(|e| file_err(path, "open", e))?;
    let file_len = file
        .metadata()
        .map_err(|e| file_err(path, "stat", e))?
        .len();
    if file_len < 8 {
        return Err(Error::File(FileError::InvalidFormat(
            "file is too short to be DICOM".into(),
        )));
    }
    let head_len = file_len.min(132) as usize;
    let mut head = vec![0u8; head_len];
    file.read_exact(&mut head)
        .map_err(|e| file_err(path, "read", e))?;
    let meta_start = if head_len >= 132 && &head[128..132] == b"DICM" {
        132
    } else {
        0
    };
    let (meta_block, meta_len) = read_meta_block(&mut file, meta_start, file_len).map_err(|e| {
        attach(
            Error::File(FileError::InvalidFormat(format!("meta group: {e}"))),
            path,
        )
    })?;
    if meta_len == 0 {
        return Ok(Dataset::new());
    }
    parse_dataset_bytes(&meta_block, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN)
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
            preamble: [0u8; 128],
            source: None,
            spooled: None,
        }
    }

    #[test]
    fn save_and_reopen_round_trip() {
        let file = sample_file();
        let bytes = file.to_bytes(&WriteOptions::default()).unwrap();
        assert_eq!(&bytes[128..132], b"DICM", "preamble + magic");
        // literal encoded values, independent of the reader: a wrong writer that
        // the reader happens to mirror would fail these
        assert!(
            bytes.windows(8).any(|w| w == b"Doe^John"),
            "PN value written verbatim"
        );
        assert!(
            bytes.windows(5).any(|w| w == b"PID-1"),
            "LO value written verbatim"
        );
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.format, FileFormat::Dicom3);
        assert_eq!(back.get_str(Tag::PATIENT_NAME), "Doe^John");
        assert_eq!(back.get_str(Tag::PATIENT_ID), "PID-1");
    }

    #[test]
    fn preamble_round_trips_through_from_bytes_and_to_bytes() {
        // IO-B-03: the 128-byte preamble is preserved on re-save instead of
        // always being replaced with zeroes.
        let mut bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        bytes[..128].fill(0xAB);
        let parsed = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        let rewritten = parsed.to_bytes(&WriteOptions::default()).unwrap();
        assert!(
            rewritten[..128].iter().all(|&b| b == 0xAB),
            "the preamble survives a parse→write round-trip"
        );
        assert_eq!(&rewritten[128..132], b"DICM");
    }

    #[test]
    fn fallback_encoding_decodes_and_reencodes_a_file_without_a_charset() {
        use crate::encoding::charset::charset_encoding;

        // A Shift-JIS PN with no (0008,0005): the legacy/misbehaving-archive
        // case. The raw element is appended by hand because the writer
        // re-encodes string VRs from their decoded text — a real such file has
        // these bytes on disk. 0x93FA 0x967B is "日本" in Shift-JIS.
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5");
        let mut bytes = DicomFile::from_dataset(dataset)
            .to_bytes(&WriteOptions::default())
            .unwrap();
        bytes.extend_from_slice(&[
            0x10, 0x00, 0x10, 0x00, // (0010,0010)
            b'P', b'N', 0x04, 0x00, // PN, 4 bytes
            0x93, 0xFA, 0x96, 0x7B, // "日本"
        ]);
        assert!(
            !bytes.windows(6).any(|w| w == b"ISO_IR"),
            "the fixture carries no SpecificCharacterSet"
        );

        // without a fallback the bytes are decoded with the default repertoire,
        // so the value is mojibake
        let default = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_ne!(default.get_str(Tag::PATIENT_NAME), "日本");

        // with ISO-IR 13 the text comes back intact (H1)
        let shift_jis = charset_encoding("ISO_IR 13");
        let with_fallback = DicomFile::from_bytes(
            &bytes,
            &ReadOptions::default().with_fallback_encoding(shift_jis),
        )
        .unwrap();
        assert_eq!(with_fallback.get_str(Tag::PATIENT_NAME), "日本");

        // the fallback is remembered, so saving keeps the original bytes instead
        // of emitting unlabelled UTF-8
        let again = with_fallback.to_bytes(&WriteOptions::default()).unwrap();
        assert!(
            again.windows(4).any(|w| w == [0x93, 0xFA, 0x96, 0x7B]),
            "re-encoded with the fallback encoding"
        );
    }

    #[test]
    fn save_with_honours_explicit_length_sequence_options() {
        let mut file = sample_file();
        let mut item = Dataset::new();
        item.insert_str(Tag::STUDY_DATE, Vr::Da, "20240101");
        file.dataset
            .insert_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
            .push(item);

        let implicit = temp_file("save-implicit");
        file.save_with(&implicit, &WriteOptions::default()).unwrap();
        let explicit = temp_file("save-explicit");
        let options = WriteOptions {
            explicit_length_sequences: true,
            explicit_length_sequence_items: true,
            ..Default::default()
        };
        file.save_with(&explicit, &options).unwrap();

        let a = std::fs::read(&implicit).unwrap();
        let b = std::fs::read(&explicit).unwrap();
        assert_ne!(
            a, b,
            "explicit-length options reach the encoded file, not just to_bytes"
        );
        let reopened = DicomFile::open(&explicit).unwrap();
        assert_eq!(
            reopened
                .dataset
                .try_get_sequence(Tag::REFERENCED_STUDY_SEQUENCE)
                .unwrap()
                .items()
                .len(),
            1
        );

        std::fs::remove_file(&implicit).ok();
        std::fs::remove_file(&explicit).ok();
    }

    #[test]
    fn meta_group_is_refreshed_on_save() {
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(
            back.file_meta.get_str(Tag::TRANSFER_SYNTAX_UID).unwrap(),
            "1.2.840.10008.1.2.1"
        );
        assert_eq!(
            back.file_meta
                .get_str(Tag::IMPLEMENTATION_CLASS_UID)
                .unwrap(),
            IMPLEMENTATION_CLASS_UID
        );
        assert_eq!(
            back.file_meta
                .get_str(Tag::IMPLEMENTATION_VERSION_NAME)
                .unwrap(),
            IMPLEMENTATION_VERSION_NAME
        );
        assert_eq!(
            back.file_meta
                .get_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID)
                .unwrap(),
            "1.2.3.4.5",
            "media storage instance UID comes from the dataset"
        );
    }

    #[test]
    fn resave_derives_media_storage_instance_uid_from_the_dataset() {
        // IO-02: changing (or regenerating) the dataset SOPInstanceUID and saving
        // again must refresh (0002,0003), not leave a stale meta instance UID.
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        let mut reopened = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(
            reopened
                .file_meta
                .get_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID)
                .unwrap(),
            "1.2.3.4.5"
        );
        reopened
            .dataset
            .insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "9.9.9.9");
        let out = reopened.to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&out, &ReadOptions::default()).unwrap();
        assert_eq!(
            back.dataset.get_str(Tag::SOP_INSTANCE_UID).unwrap(),
            "9.9.9.9"
        );
        assert_eq!(
            back.file_meta
                .get_str(Tag::MEDIA_STORAGE_SOP_INSTANCE_UID)
                .unwrap(),
            "9.9.9.9",
            "meta instance UID must track the dataset on every save"
        );
    }

    #[test]
    fn saved_files_carry_file_meta_information_version() {
        // PS3.10 Table 7.1-1: (0002,0001) is Type 1 and must be 00\01.
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        let version = back
            .file_meta
            .get_element(Tag::FILE_META_INFORMATION_VERSION)
            .expect("FileMetaInformationVersion is present");
        let buffer = version.value().as_buffer().expect("OB value");
        assert_eq!(buffer.data().unwrap().as_ref(), &[0x00, 0x01]);

        // and a meta group that omitted it gets repaired on save
        let mut file = sample_file();
        file.file_meta.remove(Tag::FILE_META_INFORMATION_VERSION);
        assert!(!file.file_meta.contains(&Tag::FILE_META_INFORMATION_VERSION));
        let bytes = file.to_bytes(&WriteOptions::default()).unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert!(back.file_meta.contains(&Tag::FILE_META_INFORMATION_VERSION));
    }

    #[test]
    fn read_file_meta_does_not_need_the_dataset() {
        // FILE-04: only the header and group-0002 bytes are read. The dataset is
        // a *truncated deflate stream*, which `open` rejects with a typed error:
        // that is what separates this from reading the whole file (CORE-10).
        let mut file = sample_file();
        file.dataset
            .insert_str(Tag::STUDY_DESCRIPTION, Vr::Lo, "study");
        file.dataset
            .set_transfer_syntax(syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone());
        let bytes = file.to_bytes(&WriteOptions::default()).unwrap();
        let bytes = &bytes[..bytes.len() - 4];

        let path = std::env::temp_dir().join(format!(
            "radicom-readmeta-{}-{}.dcm",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, bytes).unwrap();

        assert!(
            DicomFile::open(&path).is_err(),
            "the dataset body is deliberately unreadable"
        );
        let meta = read_file_meta(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(
            meta.get_str(Tag::TRANSFER_SYNTAX_UID).unwrap(),
            "1.2.840.10008.1.2.1.99",
            "the meta group is read even when the dataset is not"
        );
        assert!(!meta.contains(&Tag::STUDY_DESCRIPTION));
    }

    #[test]
    fn open_reports_short_files_and_records_the_source() {
        let path = temp_file("shortfile");
        std::fs::write(&path, b"abc").unwrap();
        assert!(matches!(
            DicomFile::open(&path),
            Err(Error::File(FileError::InvalidFormat(_)))
        ));
        std::fs::remove_file(&path).ok();

        let path = temp_file("withsource");
        sample_file().save(&path).unwrap();
        let file = DicomFile::open(&path).unwrap();
        assert_eq!(file.source(), Some(path.as_path()));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parses_without_preamble_and_without_meta() {
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        // drop the 128-byte preamble and "DICM": meta still starts at offset 0
        let no_preamble = DicomFile::from_bytes(&bytes[132..], &ReadOptions::default()).unwrap();
        assert_eq!(no_preamble.format, FileFormat::Dicom3NoPreamble);
        assert_eq!(
            no_preamble
                .file_meta
                .get_str(Tag::TRANSFER_SYNTAX_UID)
                .unwrap(),
            "1.2.840.10008.1.2.1"
        );

        // a bare explicit-LE dataset: no preamble and no meta group at all
        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID");
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
        let mut raw = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&ds, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN, &mut raw)
            .unwrap();
        let file = DicomFile::from_bytes(&raw, &ReadOptions::default()).unwrap();
        assert_eq!(file.format, FileFormat::Dicom3NoFileMetaInfo);
        assert!(file.file_meta.is_empty());
        assert_eq!(file.dataset.get_str(Tag::PATIENT_ID).unwrap(), "PID");
    }

    #[test]
    fn acr_nema_recognition_codes_and_format_display() {
        for (code, expected) in [
            ("ACR-NEMA 1.0", FileFormat::AcrNema1),
            ("ACR-NEMA 2.0", FileFormat::AcrNema2),
        ] {
            let mut ds = Dataset::new();
            ds.insert_str(Tag::RECOGNITION_CODE, Vr::Lo, code);
            let mut raw = Vec::new();
            DicomWriter::new(WriteOptions::default())
                .write_dataset(&ds, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN, &mut raw)
                .unwrap();
            let file = DicomFile::from_bytes(&raw, &ReadOptions::default()).unwrap();
            assert_eq!(file.format, expected, "{code}");
        }
        assert_eq!(FileFormat::Dicom3.to_string(), "DICOM 3 (preamble + meta)");
        assert_eq!(
            FileFormat::Dicom3NoPreamble.to_string(),
            "DICOM 3 (meta, no preamble)"
        );
        assert_eq!(
            FileFormat::Dicom3NoFileMetaInfo.to_string(),
            "DICOM 3 (no meta information)"
        );
        assert_eq!(FileFormat::AcrNema1.to_string(), "ACR-NEMA 1.0");
        assert_eq!(FileFormat::AcrNema2.to_string(), "ACR-NEMA 2.0");
    }

    #[test]
    fn save_and_meta_build_reject_inconsistent_files() {
        let mut disagree = sample_file();
        disagree
            .file_meta
            .insert_str(Tag::MEDIA_STORAGE_SOP_CLASS_UID, Vr::Ui, "1.2.3.4");
        assert!(
            disagree.to_bytes(&WriteOptions::default()).is_err(),
            "meta/dataset SOP class disagreement must be an error"
        );

        let mut no_sop = sample_file();
        no_sop.dataset.remove(Tag::SOP_CLASS_UID);
        assert!(
            no_sop.to_bytes(&WriteOptions::default()).is_err(),
            "a dataset without SOPClassUID cannot build meta"
        );

        let mut acr = sample_file();
        acr.format = FileFormat::AcrNema1;
        let path = temp_file("acr");
        assert!(matches!(
            acr.save(&path),
            Err(Error::File(FileError::FormatNotSaveable(_)))
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_file_meta_reports_short_meta_less_and_truncated_files() {
        let path = temp_file("meta-short");
        std::fs::write(&path, b"ab").unwrap();
        assert!(matches!(
            read_file_meta(&path),
            Err(Error::File(FileError::InvalidFormat(_)))
        ));
        std::fs::remove_file(&path).ok();

        let mut ds = Dataset::new();
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID");
        let mut raw = Vec::new();
        DicomWriter::new(WriteOptions::default())
            .write_dataset(&ds, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN, &mut raw)
            .unwrap();
        let path = temp_file("meta-none");
        std::fs::write(&path, &raw).unwrap();
        assert!(read_file_meta(&path).unwrap().is_empty());
        std::fs::remove_file(&path).ok();

        let mut truncated = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        truncated.truncate(140);
        let path = temp_file("meta-trunc");
        std::fs::write(&path, &truncated).unwrap();
        let err = read_file_meta(&path).unwrap_err();
        assert!(format!("{err}").contains("meta"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn scan_meta_reports_more_none_and_complete_runs() {
        assert!(matches!(scan_meta(&[]), MetaScan::More));
        assert!(matches!(
            scan_meta(&[0, 0, 0, 0, 0, 0, 0, 0]),
            MetaScan::None
        ));
        // a group-0002 element whose declared value runs past the buffer needs more
        let mut partial = vec![0x02, 0x00, 0x10, 0x00, b'U', b'I', 0x04, 0x00];
        partial.extend_from_slice(b"1."); // declares 4, only 2 present
        assert!(matches!(scan_meta(&partial), MetaScan::More));
        // a complete meta element followed by a non-0002 group ends the run
        let mut complete = vec![0x02, 0x00, 0x10, 0x00, b'U', b'I', 0x02, 0x00];
        complete.extend_from_slice(b"1.");
        complete.extend_from_slice(&[0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert!(matches!(scan_meta(&complete), MetaScan::Run(10)));
    }

    #[test]
    fn file_meta_group_length_is_written_and_correct() {
        // PS3.10 Table 7.1-1: (0002,0000) FileMetaInformationGroupLength is Type 1.
        let bytes = sample_file().to_bytes(&WriteOptions::default()).unwrap();
        let m = 132usize; // after the 128-byte preamble + "DICM"
        assert_eq!(
            &bytes[m..m + 2],
            &[0x02, 0x00],
            "first meta element is group 0002"
        );
        assert_eq!(&bytes[m + 2..m + 4], &[0x00, 0x00], "element is 0000");
        assert_eq!(&bytes[m + 4..m + 6], b"UL", "(0002,0000) carries UL");
        let group_len =
            u32::from_le_bytes([bytes[m + 8], bytes[m + 9], bytes[m + 10], bytes[m + 11]]) as usize;
        assert!(group_len > 0, "group length is non-zero");
        // the dataset must begin exactly after the declared meta remainder
        let dataset_start = m + 12 + group_len;
        let g = u16::from_le_bytes([bytes[dataset_start], bytes[dataset_start + 1]]);
        assert_ne!(
            g, 0x0002,
            "the bytes after the declared meta length are the dataset, not more meta"
        );
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
    fn bare_dataset_starting_with_a_large_group_is_not_misread_as_big_endian() {
        // IO-B-06: a first tag in a group > 0x00FF (here PixelData, 7FE0) used
        // to be treated as a big-endian signal, yielding an empty dataset. It is
        // valid Implicit VR Little Endian and must parse its first element.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x7FE0u16.to_le_bytes()); // group
        bytes.extend_from_slice(&0x0010u16.to_le_bytes()); // element
        bytes.extend_from_slice(&4u32.to_le_bytes()); // implicit length
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let file = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert!(
            file.dataset.item(&Tag::PIXEL_DATA).is_some(),
            "the (7FE0,0010) element is parsed, not silently dropped"
        );
        assert_eq!(file.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2");
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
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
        dataset.set_transfer_syntax(syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone());
        let bytes = DicomFile::from_dataset(dataset)
            .to_bytes(&WriteOptions::default())
            .unwrap();

        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(back.dataset.transfer_syntax().uid(), "1.2.840.10008.1.2.2");
        assert_eq!(back.get_str(Tag::PATIENT_ID), "PID-1");
        let rows: u16 = back.dataset.get(Tag::ROWS).unwrap();
        let cols: u16 = back.dataset.get(Tag::COLUMNS).unwrap();
        assert_eq!(
            (rows, cols),
            (512, 256),
            "US values survive the big-endian file round-trip"
        );
    }

    #[test]
    fn file_meta_sop_class_comes_from_the_dataset() {
        // review #8: a CT dataset must not be labelled Secondary Capture.
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4");
        let bytes = DicomFile::from_dataset(dataset)
            .to_bytes(&WriteOptions::default())
            .unwrap();
        let back = DicomFile::from_bytes(&bytes, &ReadOptions::default()).unwrap();
        assert_eq!(
            back.file_meta.try_get_str(Tag::MEDIA_STORAGE_SOP_CLASS_UID),
            Some("1.2.840.10008.5.1.4.1.1.2".to_string())
        );
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
        assert!(
            matches!(
                file.to_bytes(&WriteOptions::default()),
                Err(Error::File(FileError::FormatNotSaveable(_)))
            ),
            "ACR-NEMA cannot be saved"
        );
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
                assert!(
                    message.contains("meta group"),
                    "typed meta-group error, got {message}"
                );
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
        file.dataset
            .set_transfer_syntax(syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone());
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
        deflated
            .dataset
            .set_transfer_syntax(syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone());
        std::fs::write(
            &deflated_path,
            deflated.to_bytes(&WriteOptions::default()).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &plain_path,
            sample_file().to_bytes(&WriteOptions::default()).unwrap(),
        )
        .unwrap();

        // `open` goes through the streaming inflate path; both must yield the same data
        let from_disk = DicomFile::open(&deflated_path).unwrap();
        let plain = DicomFile::open(&plain_path).unwrap();
        assert!(from_disk.dataset.transfer_syntax().is_deflate());
        assert_eq!(
            from_disk.get_str(Tag::PATIENT_NAME),
            plain.get_str(Tag::PATIENT_NAME)
        );
        assert_eq!(
            from_disk.get_str(Tag::PATIENT_ID),
            plain.get_str(Tag::PATIENT_ID)
        );
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
        std::fs::write(
            &good,
            sample_file().to_bytes(&WriteOptions::default()).unwrap(),
        )
        .unwrap();
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
        assert_eq!(
            read_file_meta(&path)
                .unwrap()
                .get_str(Tag::TRANSFER_SYNTAX_UID)
                .unwrap(),
            "1.2.840.10008.1.2.1"
        );
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
        assert!(
            file.dataset.contains(&Tag::SOP_CLASS_UID),
            "elements before the stop tag"
        );
        assert!(
            !file.dataset.contains(&Tag::PATIENT_NAME),
            "stop tag not read"
        );
        assert!(
            !file.dataset.contains(&Tag::PATIENT_ID),
            "nothing past the stop tag"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn deflated_file_rewrite_is_stable() {
        let mut file = sample_file();
        file.dataset
            .set_transfer_syntax(syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone());
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
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
        let file = DicomFile::from_dataset(ds);
        let path = temp_file("large");
        file.save(&path).unwrap();
        (path, big)
    }

    fn large_value_buffer(ds: &Dataset) -> &crate::model::buffer::SharedBuffer {
        match ds.item(&Tag::new(0x0009, 0x1001)) {
            Some(crate::model::item::DicomItem::Element(e)) => {
                e.value().as_buffer().expect("value has a buffer")
            }
            other => panic!("expected element, got {other:?}"),
        }
    }

    #[test]
    fn read_large_on_demand_yields_a_lazy_window() {
        let (path, big) = file_with_large_value(5000);
        {
            let opts = ReadOptions {
                read: FileReadOption::ReadLargeOnDemand,
                large_object_size: 1000,
                ..ReadOptions::default()
            };
            let file = DicomFile::open_with_options(&path, &opts).unwrap();
            let buffer = large_value_buffer(&file.dataset);
            assert_eq!(buffer.size(), 5000);
            assert!(
                !buffer.is_memory(),
                "large value must stay a file window, not in memory"
            );
            // range reads and streaming go straight to disk; only data() caches
            let mut slice = [0u8; 16];
            buffer.get_byte_range(1000, &mut slice).unwrap();
            assert_eq!(
                slice,
                big[1000..1016],
                "a range read returns the file's bytes"
            );
            let mut streamed = Vec::new();
            assert_eq!(buffer.copy_to(&mut streamed).unwrap(), 5000);
            assert_eq!(streamed, big, "streaming returns the whole value in chunks");
            assert_eq!(
                &*buffer.data().unwrap(),
                &big[..],
                "window reads the real bytes"
            );
            assert_eq!(file.dataset.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_all_materializes_large_values() {
        let (path, big) = file_with_large_value(5000);
        {
            let opts = ReadOptions {
                read: FileReadOption::ReadAll,
                large_object_size: 1000,
                ..ReadOptions::default()
            };
            let file = DicomFile::open_with_options(&path, &opts).unwrap();
            let buffer = large_value_buffer(&file.dataset);
            assert_eq!(buffer.size(), 5000);
            assert!(buffer.is_memory(), "ReadAll keeps the value in memory");
            assert_eq!(&*buffer.data().unwrap(), &big[..]);
        }
        std::fs::remove_file(&path).ok();
    }

    /// An `(0009,1001)` OB of `big_len` bytes plus a small (0010,0020).
    ///
    /// Carries a SOP instance UID because `refreshed_meta` generates a random
    /// (0002,0003) otherwise, which would make two writes of the same file
    /// differ byte-wise.
    fn large_file(big_len: usize) -> (DicomFile, Vec<u8>) {
        let big: Vec<u8> = (0..big_len).map(|i| (i % 251) as u8).collect();
        let mut ds = Dataset::new();
        ds.insert_or_update(crate::model::element::DicomElement::new_bytes(
            Tag::new(0x0009, 0x1001),
            Vr::Ob,
            std::sync::Arc::new(MemoryBuffer::from(big.clone())),
        ));
        ds.insert_str(Tag::PATIENT_ID, Vr::Lo, "PID-1");
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.2");
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5.6.7");
        (DicomFile::from_dataset(ds), big)
    }

    #[test]
    fn save_streams_a_large_value_and_reopens_it_windowed() {
        // (a) M2: an 8 MiB value goes through the writer to the file, not through
        // a `to_bytes()` image of the whole instance
        let (file, big) = large_file(8 * 1024 * 1024);
        let path = temp_file("stream-large");
        file.save_with(&path, &WriteOptions::default()).unwrap();

        let written = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            written,
            file.to_bytes(&WriteOptions::default()).unwrap().len() as u64,
            "the streamed file is byte-identical to the in-memory image"
        );

        let opts = ReadOptions {
            read: FileReadOption::ReadLargeOnDemand,
            ..ReadOptions::default()
        };
        let reopened = DicomFile::open_with_options(&path, &opts).unwrap();
        let buffer = large_value_buffer(&reopened.dataset);
        assert_eq!(buffer.size(), big.len() as u64);
        assert!(!buffer.is_memory(), "the reopened value is a file window");
        let mut streamed = Vec::new();
        assert_eq!(buffer.copy_to(&mut streamed).unwrap(), big.len() as u64);
        assert_eq!(streamed, big, "the 8 MiB value survives the streamed write");
        assert_eq!(reopened.get_str(Tag::PATIENT_ID), "PID-1");
        std::fs::remove_file(&path).ok();
    }

    /// A `Read` with no `Seek`: the shape of a socket or pipe (M2).
    struct StreamOnly<'a> {
        inner: Cursor<&'a [u8]>,
    }

    impl Read for StreamOnly<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }

    fn spool_files() -> Vec<PathBuf> {
        std::fs::read_dir(std::env::temp_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("radicom-stream-"))
            })
            .collect()
    }

    #[test]
    fn from_reader_spools_a_non_seekable_stream() {
        // (b) M2: no Seek bound, no read_to_end; large streams spool to a temp
        // file and window their large values out of it
        let (file, big) = large_file(256 * 1024);
        let bytes = file.to_bytes(&WriteOptions::default()).unwrap();
        let before = spool_files().len();
        let options = ReadOptions {
            read: FileReadOption::ReadLargeOnDemand,
            large_object_size: 1024,
            stream_spool_threshold: 4096,
            ..ReadOptions::default()
        };
        let mut stream = StreamOnly {
            inner: Cursor::new(bytes.as_slice()),
        };
        let parsed = DicomFile::from_reader(&mut stream, &options).unwrap();
        assert_eq!(
            parsed.dataset.try_get_str_ref(Tag::PATIENT_ID),
            Some("PID-1"),
            "the spooled stream keeps every element (partial={})",
            parsed.is_partial
        );
        let buffer = large_value_buffer(&parsed.dataset);
        assert!(
            !buffer.is_memory(),
            "a spooled stream windows the value instead of holding it in RAM"
        );
        assert_eq!(&*buffer.data().unwrap(), &big[..]);
        assert_eq!(
            spool_files().len(),
            before + 1,
            "the stream went to exactly one temp file"
        );
        drop(parsed);
        assert_eq!(
            spool_files().len(),
            before,
            "the spool is deleted with the file it backs"
        );

        // a stream below the threshold is parsed from memory and touches no disk
        let small = large_file(16).0.to_bytes(&WriteOptions::default()).unwrap();
        let before = spool_files().len();
        let mut stream = StreamOnly {
            inner: Cursor::new(small.as_slice()),
        };
        let parsed = DicomFile::from_reader(&mut stream, &ReadOptions::default()).unwrap();
        assert_eq!(parsed.get_str(Tag::PATIENT_ID), "PID-1");
        assert_eq!(spool_files().len(), before, "small streams stay in memory");
    }

    #[test]
    fn save_and_write_stream_without_the_in_memory_path() {
        // (a) M2 seam: only `to_bytes` increments the counter, and neither
        // `save_with` nor `write` reaches it
        let (file, _) = large_file(1024 * 1024);
        let path = temp_file("stream-seam");
        let before = TO_BYTES_CALLS.with(|count| count.get());
        file.save_with(&path, &WriteOptions::default()).unwrap();
        let mut sink = Vec::new();
        file.write(&mut sink, &WriteOptions::default()).unwrap();
        assert_eq!(
            TO_BYTES_CALLS.with(|count| count.get()),
            before,
            "save_with/write stream through the writer instead of materializing the file"
        );

        // ...and the streaming path produces exactly the in-memory image
        assert_eq!(sink, file.to_bytes(&WriteOptions::default()).unwrap());
        assert_eq!(
            TO_BYTES_CALLS.with(|count| count.get()),
            before + 1,
            "to_bytes is the only in-memory path"
        );
        assert_eq!(std::fs::read(&path).unwrap(), sink);

        // an existing destination is replaced rather than appended to
        std::fs::write(&path, b"stale").unwrap();
        file.save(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), sink);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn skip_large_tags_omits_the_attribute_and_advances() {
        let (path, _big) = file_with_large_value(5000);
        {
            let opts = ReadOptions {
                read: FileReadOption::SkipLargeTags,
                large_object_size: 1000,
                ..ReadOptions::default()
            };
            let file = DicomFile::open_with_options(&path, &opts).unwrap();
            // fo-dicom's FileByteSource hands the observer no buffer for a
            // skipped value, so the attribute is absent, not present-but-empty
            // (review CORE-07).
            assert!(!file.dataset.contains(&Tag::new(0x0009, 0x1001)));
            // the parser still advanced past the skipped value to the next element
            assert_eq!(file.dataset.get_str(Tag::PATIENT_ID).unwrap(), "PID-1");
        }
        std::fs::remove_file(&path).ok();
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;
    use crate::io::writer::WriteOptions;

    fn sample() -> DicomFile {
        let mut dataset = Dataset::new();
        dataset.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        dataset.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5");
        dataset.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        DicomFile {
            file_meta: Dataset::new(),
            dataset,
            format: FileFormat::Dicom3,
            is_partial: false,
            preamble: [0u8; 128],
            source: None,
            spooled: None,
        }
    }

    #[test]
    fn from_reader_write_and_prefix_lengths() {
        let mut bytes = Vec::new();
        sample()
            .write(&mut bytes, &WriteOptions::default())
            .unwrap();
        let mut cursor = std::io::Cursor::new(bytes);
        let parsed = DicomFile::from_reader(&mut cursor, &ReadOptions::default()).unwrap();
        assert_eq!(
            parsed.dataset.get_str(Tag::PATIENT_NAME).unwrap().trim(),
            "Doe^John"
        );

        // a non-saveable format is refused up front
        let mut retired = sample();
        retired.format = FileFormat::AcrNema1;
        assert!(!retired.format.is_saveable());
        assert!(retired
            .encoded_prefix_len(&WriteOptions::default())
            .is_err());
        assert!(retired.to_bytes(&WriteOptions::default()).is_err());
    }
}

#[cfg(test)]
mod io08_tests {
    use super::*;

    /// One Explicit-VR-LE group-0002 element with the given element tag and value.
    fn meta_ui(elem: u16, value: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0x0002u16.to_le_bytes());
        v.extend_from_slice(&elem.to_le_bytes());
        v.extend_from_slice(b"UI");
        v.extend_from_slice(&(value.len() as u16).to_le_bytes());
        v.extend_from_slice(value);
        v
    }

    #[test]
    fn meta_group_ending_exactly_at_eof_is_accepted() {
        // IO-08: a file whose meta group ends at EOF (empty data set) is complete,
        // not "truncated".
        let mut bytes = vec![0u8; 128];
        bytes.extend_from_slice(b"DICM");
        bytes.extend_from_slice(&meta_ui(0x0010, b"1.2.840.10008.1.2.1\0"));
        let path =
            std::env::temp_dir().join(format!("radicom-io08-eof-{}.dcm", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let file = DicomFile::open(&path).expect("meta-only file is accepted");
        assert_eq!(
            file.file_meta.get_str(Tag::TRANSFER_SYNTAX_UID).unwrap(),
            "1.2.840.10008.1.2.1"
        );
        assert!(file.dataset.items().next().is_none(), "empty data set");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn meta_window_ending_on_a_boundary_is_grown_not_truncated() {
        // IO-08: the first 4 KiB window ends exactly on an element boundary; the
        // scanner must grow and still see the following dataset element.
        let meta_len = 4096usize;
        let mut meta = vec![0u8; meta_len];
        // (0002,0010) UI with a 4088-byte value: 8-byte header + 4088 = 4096
        let value = vec![b'1'; meta_len - 8];
        let elem = meta_ui(0x0010, &value);
        assert_eq!(elem.len(), meta_len);
        meta.copy_from_slice(&elem);
        let mut dataset_elem = Vec::new();
        dataset_elem.extend_from_slice(&0x0008u16.to_le_bytes());
        dataset_elem.extend_from_slice(&0x0005u16.to_le_bytes());
        dataset_elem.extend_from_slice(b"CS");
        dataset_elem.extend_from_slice(&2u16.to_le_bytes());
        dataset_elem.extend_from_slice(b"QT");

        let mut bytes = vec![0u8; 128];
        bytes.extend_from_slice(b"DICM");
        bytes.extend_from_slice(&meta);
        bytes.extend_from_slice(&dataset_elem);
        let path =
            std::env::temp_dir().join(format!("radicom-io08-window-{}.dcm", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        let (block, len) = read_meta_block(&mut f, 132, bytes.len() as u64).unwrap();
        assert_eq!(len as usize, meta_len, "meta window grew past the boundary");
        assert_eq!(block.len(), meta_len);
        std::fs::remove_file(&path).ok();
    }
}

#[cfg(test)]
mod core22_tests {
    use super::*;

    #[test]
    fn oversized_meta_lengths_are_errors_not_wrapped_offsets() {
        // (0002,0001) OB declaring 4 GiB - 1 bytes: the computed end must be
        // checked, and the meta group reported as truncated (CORE-22).
        let mut raw = vec![0x02, 0x00, 0x01, 0x00, b'O', b'B', 0x00, 0x00];
        raw.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        raw.extend_from_slice(&[0x08, 0x00, 0x05, 0x00]);
        assert!(matches!(
            split_meta(&raw),
            Err(Error::File(FileError::InvalidFormat(_)))
        ));
        // the streaming scanner asks for more bytes instead of slicing
        assert!(matches!(scan_meta(&raw), MetaScan::More));
        // a valid element run still terminates at the first non-0002 group
        let mut ok = vec![0x02, 0x00, 0x01, 0x00, b'O', b'B', 0x00, 0x00];
        ok.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        ok.extend_from_slice(&[0x08, 0x00, 0x05, 0x00, b'C', b'S', 0x02, 0x00, b'Q', b'T']);
        assert!(matches!(scan_meta(&ok), MetaScan::Run(12)));
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::io::writer::WriteOptions;

    /// One Explicit VR Little Endian element (short-form length).
    fn explicit_elem(group: u16, element: u16, vr: &[u8; 2], value: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&group.to_le_bytes());
        v.extend_from_slice(&element.to_le_bytes());
        v.extend_from_slice(vr);
        v.extend_from_slice(&(value.len() as u16).to_le_bytes());
        v.extend_from_slice(value);
        v
    }

    /// One group-0002 element with a long-form value (OB).
    fn meta_ob(element: u16, value: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&0x0002u16.to_le_bytes());
        v.extend_from_slice(&element.to_le_bytes());
        v.extend_from_slice(b"OB");
        v.extend_from_slice(&[0, 0]);
        v.extend_from_slice(&(value.len() as u32).to_le_bytes());
        v.extend_from_slice(value);
        v
    }

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("radicom-cov-{name}-{}.dcm", std::process::id()))
    }

    fn sample_dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::SOP_CLASS_UID, Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        ds.insert_str(Tag::SOP_INSTANCE_UID, Vr::Ui, "1.2.3.4.5");
        ds.insert_str(Tag::PATIENT_NAME, Vr::Pn, "Doe^John");
        ds
    }

    fn sample_with_name() -> Vec<u8> {
        DicomFile::from_dataset(sample_dataset())
            .to_bytes(&WriteOptions::default())
            .unwrap()
    }

    #[test]
    fn open_with_encoding_uses_the_fallback_encoding() {
        let path = tmp_path("enc");
        std::fs::write(&path, sample_with_name()).unwrap();
        let file = DicomFile::open_with_encoding(&path, encoding_rs::WINDOWS_1252).unwrap();
        assert_eq!(
            file.dataset.get_str(Tag::PATIENT_NAME).unwrap().trim(),
            "Doe^John"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn bare_dataset_has_no_file_meta_group() {
        let path = tmp_path("bare");
        let bytes = explicit_elem(0x0010, 0x0010, b"PN", b"AB");
        std::fs::write(&path, &bytes).unwrap();
        let file = DicomFile::open(&path).unwrap();
        assert_eq!(file.format, FileFormat::Dicom3NoFileMetaInfo);
        assert!(file.file_meta.items().next().is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn meta_without_preamble_or_transfer_syntax_is_probed() {
        let path = tmp_path("nopreamble");
        let mut bytes = meta_ob(0x0001, &[0, 1]); // (0002,0001) version
        bytes.extend_from_slice(&explicit_elem(0x0010, 0x0010, b"PN", b"AB"));
        std::fs::write(&path, &bytes).unwrap();
        let file = DicomFile::open(&path).unwrap();
        assert_eq!(file.format, FileFormat::Dicom3NoPreamble);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn recognition_code_selects_the_acr_nema_format() {
        let bytes = explicit_elem(0x0008, 0x0010, b"LO", b"ACR-NEMA 1.0");
        let path = tmp_path("acr");
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(DicomFile::open(&path).unwrap().format, FileFormat::AcrNema1);
        std::fs::remove_file(&path).ok();

        // the in-memory parse path applies the same recognition rule
        let mut cursor = std::io::Cursor::new(bytes);
        let file = DicomFile::from_reader(&mut cursor, &ReadOptions::default()).unwrap();
        assert_eq!(file.format, FileFormat::AcrNema1);
    }

    #[test]
    fn open_partial_inflates_a_deflated_dataset() {
        // A deflated file must be inflated before parsing in open_partial, just
        // as the streaming open path does; otherwise it yields compressed
        // garbage. The real fixture under tests-data is a raw-DEFLATE sample.
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests-data/deflated/GH227.dcm");
        let full = DicomFile::open(&path).expect("open deflated file");
        let partial =
            DicomFile::open_partial(&path, &|_| false).expect("open_partial deflated file");
        let expected = full
            .dataset
            .try_get_str(Tag::SOP_INSTANCE_UID)
            .expect("full parse has a SOP Instance UID");
        assert_eq!(
            partial.dataset.try_get_str(Tag::SOP_INSTANCE_UID),
            Some(expected),
            "the decompressed dataset exposes a known element"
        );
    }

    #[test]
    fn open_partial_without_meta_uses_probe_syntax() {
        let path = tmp_path("partial");
        std::fs::write(&path, explicit_elem(0x0010, 0x0010, b"PN", b"AB")).unwrap();
        let file = DicomFile::open_partial(&path, &|_| false).unwrap();
        assert_eq!(file.format, FileFormat::Dicom3NoFileMetaInfo);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_with_bare_filename_and_on_a_rename_failure() {
        let file = DicomFile::from_dataset(sample_dataset());
        // a bare relative path has an empty parent, resolved to "."
        assert_eq!(save_directory(Path::new("report.dcm")), Path::new("."));
        assert_eq!(
            save_directory(Path::new("/tmp/report.dcm")),
            Path::new("/tmp")
        );

        // renaming the temp file onto an existing directory fails cleanly
        let dir = std::env::temp_dir().join(format!("radicom-cov-save-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(file.save_with(&dir, &WriteOptions::default()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recognition_code_2_and_unknown_forms() {
        let path = tmp_path("acr2");
        std::fs::write(&path, explicit_elem(0x0008, 0x0010, b"LO", b"ACR-NEMA 2.0")).unwrap();
        assert_eq!(DicomFile::open(&path).unwrap().format, FileFormat::AcrNema2);

        std::fs::write(&path, explicit_elem(0x0008, 0x0010, b"LO", b"NOT-A-CODE")).unwrap();
        assert_eq!(
            DicomFile::open(&path).unwrap().format,
            FileFormat::Dicom3NoFileMetaInfo
        );
        std::fs::remove_file(&path).ok();

        // the in-memory parse path leaves an unknown recognition code alone too
        let mut cursor = std::io::Cursor::new(explicit_elem(0x0008, 0x0010, b"LO", b"OTHER"));
        let file = DicomFile::from_reader(&mut cursor, &ReadOptions::default()).unwrap();
        assert_eq!(file.format, FileFormat::Dicom3NoFileMetaInfo);
    }
}
