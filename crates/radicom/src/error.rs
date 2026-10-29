//! Error hierarchy (feature INF-01, architecture §5).
//!
//! Every error carries context where available (tag, VR, value fragment,
//! byte position). Network errors are subtyped so reconnect policies can
//! discriminate. Panics are never used to signal data problems.

use std::fmt;

/// A DICOM tag in `(GGGG,EEEE)` form, used for error context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagRef(pub u16, pub u16);

impl fmt::Display for TagRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({:04X},{:04X})", self.0, self.1)
    }
}

/// Data-level failures (dataset reads/writes, value conversion).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataError {
    #[error("tag {tag} not found")]
    TagNotFound { tag: TagRef },
    #[error("tag {tag} already exists")]
    TagAlreadyExists { tag: TagRef },
    #[error("tag {tag}: value of {expected} expected, found {found}")]
    UnexpectedType {
        tag: TagRef,
        expected: &'static str,
        found: &'static str,
    },
    #[error("tag {tag}: value index {index} out of range (count {count})")]
    IndexOutOfRange {
        tag: TagRef,
        index: usize,
        count: usize,
    },
    #[error("tag {tag}: private creator block exhausted")]
    PrivateTagResolution { tag: TagRef },
    #[error("charset error: {message} (tag: {tag_fmt})")]
    Encoding {
        tag: Option<TagRef>,
        message: String,
        tag_fmt: String,
    },
    #[error("{message}")]
    Other { message: String },
}

impl DataError {
    /// Constructor for [`DataError::Encoding`] that derives the display form of the optional tag.
    pub fn encoding(tag: Option<TagRef>, message: impl Into<String>) -> Self {
        DataError::Encoding {
            tag_fmt: tag.map(|t| t.to_string()).unwrap_or_else(|| "none".into()),
            tag,
            message: message.into(),
        }
    }
}

/// Validation failures carry the offending content and VR (VAL-*).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{tag} {vr}: content \"{content}\" does not validate: {reason}")]
pub struct ValidationFailure {
    pub tag: TagRef,
    pub vr: &'static str,
    pub content: String,
    pub reason: String,
}

/// File-level failures (open/save, format detection, meta information).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    #[error("invalid DICOM file: {0}")]
    InvalidFormat(String),
    #[error("format {0} cannot be saved")]
    FormatNotSaveable(String),
    #[error("missing file meta information: {0}")]
    MissingMeta(String),
    #[error("{0}")]
    Other(String),
}

/// Parser failures carry the byte position where parsing failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReaderError {
    #[error("invalid DICOM stream at byte offset {position}: {message}")]
    Invalid { message: String, position: u64 },
    #[error("sequence nesting deeper than {limit} at byte offset {position}")]
    TooDeep { limit: u32, position: u64 },
    #[error("unexpected end of stream at byte offset {position} (needed {needed} more bytes)")]
    Eof { position: u64, needed: u64 },
    #[error("{0}")]
    Other(String),
}

/// Writer failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("write error: {0}")]
pub struct WriterError(pub String);

/// Codec/transcoder failures. The single error channel across the
/// radicom ↔ radicom-codecs boundary (docs/01-architecture.md §9.1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("no codec registered for transfer syntax {0}")]
    NotRegistered(String),
    #[error("cannot transcode {from} to {to}")]
    UnsupportedTranscode { from: String, to: String },
    #[error("codec {codec}: {message}")]
    Operation { codec: String, message: String },
}

/// Pixel data / imaging failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("pixel data error: {0}")]
pub struct ImagingError(pub String);

/// Top-level error type for the whole library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Data(#[from] DataError),
    #[error("validation failed: {0}")]
    Validation(#[from] ValidationFailure),
    #[error(transparent)]
    File(#[from] FileError),
    #[error(transparent)]
    Reader(#[from] ReaderError),
    #[error(transparent)]
    Writer(#[from] WriterError),
    #[error("network error: {0}")]
    Network(String), // typed sub-errors arrive with the network crate (M5)
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Imaging(#[from] ImagingError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Convenience constructor for data errors with free-form context.
    pub fn other(message: impl Into<String>) -> Self {
        Error::Data(DataError::Other {
            message: message.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_carries_context() {
        let e = Error::Data(DataError::TagNotFound {
            tag: TagRef(0x0028, 0x0010),
        });
        assert_eq!(e.to_string(), "tag (0028,0010) not found");

        let v = Error::Validation(ValidationFailure {
            tag: TagRef(0x0008, 0x0050),
            vr: "SH",
            content: "TOOLONGACCESSSIONNUMBERX".into(),
            reason: "length 24 exceeds max 16".into(),
        });
        let s = v.to_string();
        assert!(s.contains("(0008,0050)") && s.contains("SH") && s.contains("does not validate"));
    }

    #[test]
    fn every_variant_has_a_display_form() {
        let tag = TagRef(0x0008, 0x0018);
        let errors = [
            Error::Data(DataError::TagAlreadyExists { tag }),
            Error::Data(DataError::UnexpectedType {
                tag,
                expected: "US",
                found: "IS",
            }),
            Error::Data(DataError::IndexOutOfRange {
                tag,
                index: 2,
                count: 1,
            }),
            Error::Data(DataError::PrivateTagResolution { tag }),
            Error::Data(DataError::Encoding {
                tag: Some(tag),
                message: "bad".into(),
                tag_fmt: tag.to_string(),
            }),
            Error::Data(DataError::Encoding {
                tag: None,
                message: "bad".into(),
                tag_fmt: "none".into(),
            }),
            Error::Data(DataError::Other {
                message: "other".into(),
            }),
            Error::File(FileError::InvalidFormat("bad".into())),
            Error::File(FileError::FormatNotSaveable("bad".into())),
            Error::File(FileError::MissingMeta("bad".into())),
            Error::File(FileError::Other("bad".into())),
            Error::Reader(ReaderError::Invalid {
                message: "bad".into(),
                position: 1,
            }),
            Error::Reader(ReaderError::TooDeep {
                limit: 2,
                position: 3,
            }),
            Error::Reader(ReaderError::Eof {
                position: 4,
                needed: 5,
            }),
            Error::Reader(ReaderError::Other("bad".into())),
            Error::Writer(WriterError("bad".into())),
            Error::Network("bad".into()),
            Error::Codec(CodecError::UnsupportedTranscode {
                from: "a".into(),
                to: "b".into(),
            }),
            Error::Codec(CodecError::Operation {
                codec: "c".into(),
                message: "bad".into(),
            }),
            Error::Imaging(ImagingError("bad".into())),
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn constructors_are_usable() {
        let tag = TagRef(0x0008, 0x0018);
        assert!(DataError::encoding(Some(tag), "bad")
            .to_string()
            .contains("(0008,0018)"));
        assert!(DataError::encoding(None, "bad")
            .to_string()
            .contains("none"));
        assert!(Error::other("bad").to_string().contains("bad"));
    }

    #[test]
    fn conversions_compose() {
        let io: Error = std::io::Error::new(std::io::ErrorKind::NotFound, "gone").into();
        assert!(matches!(io, Error::Io(_)));
        let codec: Error = CodecError::NotRegistered("1.2.840.10008.1.2.4.57".into()).into();
        assert!(codec.to_string().contains("no codec registered"));
    }
}
