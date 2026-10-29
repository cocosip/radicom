//! Byte buffer abstraction (BUF-01, BUF-02; docs/01-architecture.md §7).
//!
//! All large values are buffer references, never immediate copies. This
//! module ships the memory-domain implementations; file/stream windows and
//! `BulkDataUri` arrive with the parser (M3) and JSON (M7).

use bytes::Bytes;
use std::io;
use std::sync::{Arc, OnceLock};

/// A reference to a possibly lazily materialized byte sequence.
///
/// Materialization is fallible: a [`FileWindow`] or [`StreamWindow`] fails when
/// the underlying file is truncated, removed or unreadable, and a temp-file
/// spill can fail. There is deliberately no infallible `data()` twin — safety
/// cannot be type-checked through `dyn ByteBuffer`, so callers that know the
/// value is already in memory use [`ByteBuffer::as_slice`] instead.
pub trait ByteBuffer: Send + Sync + std::fmt::Debug {
    /// Encoded length in bytes. Must be cheap and must not materialize.
    fn size(&self) -> u64;

    /// True when [`ByteBuffer::as_slice`] yields the bytes (already in memory).
    fn is_memory(&self) -> bool;

    /// Materializes the whole value (may trigger a lazy load or a file read).
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>>;

    /// A zero-copy view when the value is one contiguous in-memory slice.
    /// Defaults to `None`; callers fall back to [`ByteBuffer::data`].
    fn as_slice(&self) -> Option<&[u8]> {
        None
    }

    /// The bulk-data URI this buffer stands for, when it is a [`BulkDataUri`]
    /// (JSON `BulkDataURI` carrier, docs/01-architecture.md §12.1). Defaults to `None`.
    fn bulk_uri(&self) -> Option<&str> {
        None
    }

    /// Copies `buf.len()` bytes starting at `offset`. An out-of-range request
    /// is an `InvalidInput` error, never a panic.
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let data = self.data()?;
        let range = checked_range(offset, buf.len(), data.len() as u64)?;
        buf.copy_from_slice(&data[range]);
        Ok(())
    }

    /// Streams the value into `w` in chunks; returns the byte count.
    fn copy_to(&self, w: &mut dyn io::Write) -> io::Result<u64> {
        let data = self.data()?;
        w.write_all(&data)?;
        Ok(data.len() as u64)
    }
}

/// Validates `[offset, offset+len)` against `size` and returns it as a `usize`
/// range (narrowing is safe once the end fits in `size`).
fn checked_range(offset: u64, len: usize, size: u64) -> io::Result<std::ops::Range<usize>> {
    let end = offset
        .checked_add(len as u64)
        .filter(|end| *end <= size)
        .ok_or_else(|| out_of_range(offset, len, size))?;
    Ok(offset as usize..end as usize)
}

/// Content equality for fallible buffers (deep `PartialEq` on elements and
/// fragments). A value that cannot be materialized is never equal to anything:
/// equality has no error channel, and "unreadable" is not "same bytes".
pub(crate) fn same_content(a: &SharedBuffer, b: &SharedBuffer) -> bool {
    if a.size() != b.size() {
        return false;
    }
    match (a.data(), b.data()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// An out-of-range read: a caller bug, reported as a typed error.
fn out_of_range(offset: u64, len: usize, size: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("read of {len} bytes at offset {offset} is outside a {size}-byte buffer"),
    )
}

/// A reference to bytes that live outside the dataset, addressed by URI
/// (fo-dicom `BulkDataUriByteBuffer`; JSON `BulkDataURI` carrier, docs/01-architecture.md §12.1).
///
/// The data itself is not fetched here — the buffer only carries the URI and,
/// when the caller supplied it, an already-materialized fallback so consumers
/// that need bytes still work. `data()` returns the fallback (empty when the
/// repository has not been asked to resolve it).
#[derive(Debug, Clone)]
pub struct BulkDataUri {
    uri: String,
    fallback: Option<Bytes>,
}

impl BulkDataUri {
    /// A URI-only buffer (no local bytes).
    pub fn new(uri: impl Into<String>) -> Self {
        BulkDataUri {
            uri: uri.into(),
            fallback: None,
        }
    }

    /// A URI buffer backed by already-loaded bytes (e.g. after a fetch).
    pub fn with_data(uri: impl Into<String>, data: impl Into<Bytes>) -> Self {
        BulkDataUri {
            uri: uri.into(),
            fallback: Some(data.into()),
        }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }
}

impl ByteBuffer for BulkDataUri {
    fn size(&self) -> u64 {
        self.fallback.as_ref().map_or(0, |b| b.len() as u64)
    }
    fn is_memory(&self) -> bool {
        // Only a URI buffer with a materialized fallback really holds its
        // bytes; a URI-only buffer has nothing in memory (fo-dicom's
        // `BulkDataUriByteBuffer.IsMemory`), which lets content comparison
        // skip it instead of comparing empty placeholders.
        self.fallback.is_some()
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        Ok(match &self.fallback {
            Some(bytes) => std::borrow::Cow::Borrowed(bytes),
            None => std::borrow::Cow::Borrowed(&[]),
        })
    }
    fn as_slice(&self) -> Option<&[u8]> {
        // only a resolved URI is in memory; a URI-only buffer is not, which
        // keeps `is_memory` and `as_slice` consistent
        self.fallback.as_deref()
    }
    fn bulk_uri(&self) -> Option<&str> {
        Some(&self.uri)
    }
}

/// Blanket deref helper: any `Arc<dyn ByteBuffer>` behaves like its target.
pub type SharedBuffer = Arc<dyn ByteBuffer>;

// -- memory-domain implementations ---------------------------------------

/// Owning in-memory bytes (the common case after parsing).
#[derive(Debug, Clone)]
pub struct MemoryBuffer(Bytes);

impl MemoryBuffer {
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        MemoryBuffer(bytes.into())
    }
}

impl From<Vec<u8>> for MemoryBuffer {
    fn from(v: Vec<u8>) -> Self {
        MemoryBuffer(Bytes::from(v))
    }
}

impl ByteBuffer for MemoryBuffer {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }
    fn is_memory(&self) -> bool {
        true
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        Ok(std::borrow::Cow::Borrowed(&self.0))
    }
    fn as_slice(&self) -> Option<&[u8]> {
        Some(self.0.as_ref())
    }
}

/// The empty value singleton (fo-dicom `EmptyBuffer.Value`).
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyBuffer;

impl EmptyBuffer {
    pub fn shared() -> SharedBuffer {
        static EMPTY: std::sync::OnceLock<SharedBuffer> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| Arc::new(EmptyBuffer)).clone()
    }
}

impl ByteBuffer for EmptyBuffer {
    fn size(&self) -> u64 {
        0
    }
    fn is_memory(&self) -> bool {
        true
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        Ok(std::borrow::Cow::Borrowed(&[]))
    }
}

/// Zero-copy window over another buffer (fo-dicom `RangeByteBuffer`).
#[derive(Debug, Clone)]
pub struct RangeBuffer {
    inner: SharedBuffer,
    offset: u64,
    len: u64,
}

impl RangeBuffer {
    pub fn new(inner: SharedBuffer, offset: u64, len: u64) -> Self {
        debug_assert!(offset + len <= inner.size(), "range out of bounds");
        RangeBuffer { inner, offset, len }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn inner(&self) -> &SharedBuffer {
        &self.inner
    }
}

impl ByteBuffer for RangeBuffer {
    fn size(&self) -> u64 {
        self.len
    }
    fn is_memory(&self) -> bool {
        self.inner.is_memory()
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        let data = self.inner.data()?;
        let range = checked_range(self.offset, self.len as usize, data.len() as u64)?;
        Ok(match data {
            std::borrow::Cow::Borrowed(b) => std::borrow::Cow::Borrowed(&b[range]),
            std::borrow::Cow::Owned(b) => std::borrow::Cow::Owned(b[range].to_vec()),
        })
    }
    fn as_slice(&self) -> Option<&[u8]> {
        if !self.inner.is_memory() {
            return None;
        }
        match self.inner.data().ok()? {
            std::borrow::Cow::Borrowed(b) => {
                let range = checked_range(self.offset, self.len as usize, b.len() as u64).ok()?;
                Some(&b[range])
            }
            std::borrow::Cow::Owned(_) => None,
        }
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        checked_range(offset, buf.len(), self.len)?;
        self.inner.get_byte_range(self.offset + offset, buf)
    }
}

/// Concatenation view over several buffers (fo-dicom `CompositeByteBuffer`).
///
/// `data()` is the one place that has to materialise the whole value; the
/// concatenation is cached on first use so repeated `data()`/equality checks do
/// not copy again. Range reads and `copy_to` stay streaming over the parts and
/// never touch the cache.
#[derive(Debug, Clone, Default)]
pub struct CompositeBuffer {
    parts: Vec<SharedBuffer>,
    concat: once_cell::sync::OnceCell<Bytes>,
}

impl CompositeBuffer {
    pub fn new(parts: Vec<SharedBuffer>) -> Self {
        CompositeBuffer {
            parts,
            concat: once_cell::sync::OnceCell::new(),
        }
    }

    pub fn parts(&self) -> &[SharedBuffer] {
        &self.parts
    }
}

impl ByteBuffer for CompositeBuffer {
    fn size(&self) -> u64 {
        self.parts.iter().map(|p| p.size()).sum()
    }
    fn is_memory(&self) -> bool {
        self.parts.iter().all(|p| p.is_memory())
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        let bytes = self.concat.get_or_try_init(|| {
            let mut out = Vec::with_capacity(self.size() as usize);
            for p in &self.parts {
                out.extend_from_slice(&p.data()?);
            }
            Ok::<_, io::Error>(Bytes::from(out))
        })?;
        Ok(std::borrow::Cow::Borrowed(bytes))
    }
    fn as_slice(&self) -> Option<&[u8]> {
        if !self.is_memory() {
            return None;
        }
        match self.data().ok()? {
            std::borrow::Cow::Borrowed(bytes) => Some(bytes),
            std::borrow::Cow::Owned(_) => None,
        }
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        checked_range(offset, buf.len(), self.size())?;
        if buf.is_empty() {
            return Ok(());
        }
        let mut remaining = offset;
        let mut filled = 0usize;
        for p in &self.parts {
            let size = p.size();
            if remaining >= size {
                remaining -= size;
                continue;
            }
            let take = (buf.len() - filled).min((size - remaining) as usize);
            p.get_byte_range(remaining, &mut buf[filled..filled + take])?;
            filled += take;
            remaining = 0;
            if filled == buf.len() {
                return Ok(());
            }
        }
        Err(out_of_range(offset, buf.len(), self.size()))
    }
    fn copy_to(&self, w: &mut dyn io::Write) -> io::Result<u64> {
        let mut total = 0u64;
        for p in &self.parts {
            total += p.copy_to(w)?;
        }
        Ok(total)
    }
}

/// Byte-swap view over another buffer, swapping `unit`-wide groups
/// (fo-dicom `EndianByteBuffer`/`SwapByteBuffer`; unit 1 = no-op).
#[derive(Debug, Clone)]
pub struct EndianBuffer {
    inner: SharedBuffer,
    unit: usize,
    /// Swapped copy, computed on first whole-value read. Big-endian numeric
    /// values are read one element at a time by `FromElement`, so without this
    /// cache an n-element array costs O(n²) bytes of copying/swapping.
    swapped: OnceLock<Vec<u8>>,
}

impl EndianBuffer {
    pub fn new(inner: SharedBuffer, unit: usize) -> Self {
        EndianBuffer {
            inner,
            unit,
            swapped: OnceLock::new(),
        }
    }

    pub fn unit(&self) -> usize {
        self.unit
    }

    pub fn inner(&self) -> &SharedBuffer {
        &self.inner
    }

    /// Wraps unless the unit is 1 (where byte order is irrelevant).
    pub fn wrap_if_needed(inner: SharedBuffer, unit: usize) -> SharedBuffer {
        if unit <= 1 {
            inner
        } else {
            Arc::new(EndianBuffer::new(inner, unit))
        }
    }
}

impl ByteBuffer for EndianBuffer {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn is_memory(&self) -> bool {
        self.inner.is_memory()
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        if let Some(cached) = self.swapped.get() {
            return Ok(std::borrow::Cow::Borrowed(cached));
        }
        let mut data = self.inner.data()?.into_owned();
        crate::model::endian::Endian::swap_buffer(&mut data, self.unit);
        // A concurrent first read may win the race; both compute the same bytes,
        // so keeping theirs is fine.
        let _ = self.swapped.set(data);
        Ok(std::borrow::Cow::Borrowed(
            self.swapped.get().expect("swapped bytes just set"),
        ))
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        if let Some(cached) = self.swapped.get() {
            let start = usize::try_from(offset)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
            let end = start
                .checked_add(buf.len())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "range overflow"))?;
            let slice = cached.get(start..end).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "byte range is past the swapped value",
                )
            })?;
            buf.copy_from_slice(slice);
            return Ok(());
        }
        let mut tmp = vec![0u8; buf.len()];
        self.inner.get_byte_range(offset, &mut tmp)?;
        crate::model::endian::Endian::swap_buffer(&mut tmp, self.unit);
        buf.copy_from_slice(&tmp);
        Ok(())
    }
}

/// Appends one padding byte when the inner size is odd
/// (fo-dicom `EvenLengthBuffer`).
#[derive(Debug, Clone)]
pub struct EvenLengthBuffer {
    inner: SharedBuffer,
}

impl EvenLengthBuffer {
    pub fn new(inner: SharedBuffer) -> Self {
        EvenLengthBuffer { inner }
    }

    /// Wraps only when the inner size is odd.
    pub fn wrap_if_needed(inner: SharedBuffer) -> SharedBuffer {
        if inner.size().is_multiple_of(2) {
            inner
        } else {
            Arc::new(EvenLengthBuffer { inner })
        }
    }
}

impl ByteBuffer for EvenLengthBuffer {
    fn size(&self) -> u64 {
        self.inner.size() + (self.inner.size() % 2)
    }
    fn is_memory(&self) -> bool {
        self.inner.is_memory()
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        if self.inner.size().is_multiple_of(2) {
            return self.inner.data();
        }
        let mut out = self.inner.data()?.into_owned();
        out.push(0);
        Ok(std::borrow::Cow::Owned(out))
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        checked_range(offset, buf.len(), self.size())?;
        let inner_len = self.inner.size();
        // bytes at or past the inner length are the single padding byte
        let taken = (inner_len.saturating_sub(offset) as usize).min(buf.len());
        if taken > 0 {
            self.inner.get_byte_range(offset, &mut buf[..taken])?;
        }
        buf[taken..].fill(0);
        Ok(())
    }
}

/// Lazily materialized bytes (fo-dicom `LazyByteBuffer`): the producer
/// closure runs once, on first access.
#[derive(Clone)]
pub struct LazyBuffer<F: Fn() -> Vec<u8> + Send + Sync> {
    cell: Arc<once_cell::sync::OnceCell<Bytes>>,
    produce: F,
}

impl<F: Fn() -> Vec<u8> + Send + Sync> std::fmt::Debug for LazyBuffer<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyBuffer")
            .field("materialized", &self.cell.get().is_some())
            .finish()
    }
}

impl<F: Fn() -> Vec<u8> + Send + Sync> LazyBuffer<F> {
    pub fn new(produce: F) -> Self {
        LazyBuffer {
            cell: Arc::new(once_cell::sync::OnceCell::new()),
            produce,
        }
    }

    /// The producer cannot fail, so size and data share one infallible load.
    fn bytes(&self) -> &Bytes {
        self.cell.get_or_init(|| Bytes::from((self.produce)()))
    }
}

impl<F: Fn() -> Vec<u8> + Send + Sync> ByteBuffer for LazyBuffer<F> {
    fn size(&self) -> u64 {
        self.bytes().len() as u64
    }
    fn is_memory(&self) -> bool {
        true
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        Ok(std::borrow::Cow::Borrowed(self.bytes()))
    }
}

// -- file / stream domain implementations (BUF-02) -----------------------
//
// These keep large values out of the in-memory dataset: a value is a window
// into a file (or a temp-file spill of a non-seekable stream) and bytes are
// read on demand. `data()` materializes the whole value once into a cache,
// while `get_byte_range`/`copy_to` stream without ever holding it all.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Mutex;

/// A process-unique temp path with a non-guessable suffix.
///
/// A fixed name in the shared temp directory would let another local user
/// pre-create a symlink (the classic temp-file race); `create_temp` opens the
/// file with `create_new` so an existing path is never followed.
fn temp_path(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ ((seq as u128) << 32);
    std::env::temp_dir().join(format!(
        "radicom-buf-{}-{seq}-{nonce:x}-{tag}.tmp",
        std::process::id()
    ))
}

/// Creates a fresh temp file, never following an existing path.
///
/// On Unix the file is created `0600` so a spilled value (possibly PHI) is not
/// readable by other local users.
fn create_temp(tag: &str) -> io::Result<(PathBuf, std::fs::File)> {
    loop {
        let path = temp_path(tag);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Deletes the temp file on drop (after the owning file handle has closed).
#[derive(Debug)]
struct TempFile {
    path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A lazy read-only window over `[offset, offset+len)` of a shared file
/// (BUF-02).
///
/// All windows over one parsed file share a single `Arc<File>`; reads use
/// positional I/O (`read_at` on Unix, `seek_read` on Windows), so a window
/// needs no seek cursor and no per-window handle. `get_byte_range`/`copy_to`
/// read straight from disk (a consumer that only needs part of a large value
/// never materializes it); `data()` materializes once into the `OnceCell`,
/// which is the ceiling of the `&[u8]` API.
#[derive(Debug)]
pub struct FileWindow {
    file: Arc<std::fs::File>,
    offset: u64,
    len: u64,
    cache: once_cell::sync::OnceCell<Bytes>,
}

impl FileWindow {
    /// Opens `path` and windows `len` bytes starting at `offset`.
    pub fn open(path: impl AsRef<Path>, offset: u64, len: u64) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).open(path.as_ref())?;
        Ok(Self::from_shared(Arc::new(file), offset, len))
    }

    /// Windows an already-open shared file, so many windows cost one handle.
    pub fn from_shared(file: Arc<std::fs::File>, offset: u64, len: u64) -> Self {
        FileWindow {
            file,
            offset,
            len,
            cache: once_cell::sync::OnceCell::new(),
        }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn read_all(&self) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; self.len as usize];
        self.read_into(0, &mut buf)?;
        Ok(buf)
    }

    fn read_into(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        read_exact_at(&self.file, self.offset + offset, buf)
    }
}

/// Fills `buf` from `file` at `offset` without moving a shared cursor, so one
/// handle serves any number of concurrent windows.
#[cfg(unix)]
fn read_exact_at(file: &std::fs::File, mut offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset) {
            Ok(0) => return Err(eof_error()),
            Ok(n) => {
                done += n;
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &std::fs::File, mut offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while done < buf.len() {
        match file.seek_read(&mut buf[done..], offset) {
            Ok(0) => return Err(eof_error()),
            Ok(n) => {
                done += n;
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn read_exact_at(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut handle = file.try_clone()?;
    handle.seek(SeekFrom::Start(offset))?;
    handle.read_exact(buf)
}

fn eof_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "file window read past its end",
    )
}

impl ByteBuffer for FileWindow {
    fn size(&self) -> u64 {
        self.len
    }
    fn is_memory(&self) -> bool {
        false
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        let bytes = self
            .cache
            .get_or_try_init(|| self.read_all().map(Bytes::from))?;
        Ok(std::borrow::Cow::Borrowed(bytes))
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        checked_range(offset, buf.len(), self.len)?;
        self.read_into(offset, buf)
    }
    fn copy_to(&self, w: &mut dyn io::Write) -> io::Result<u64> {
        const CHUNK: usize = 1024 * 1024;
        let mut chunk = vec![0u8; CHUNK];
        let mut offset = 0u64;
        while offset < self.len {
            let take = ((self.len - offset) as usize).min(CHUNK);
            self.read_into(offset, &mut chunk[..take])?;
            w.write_all(&chunk[..take])?;
            offset += take as u64;
        }
        Ok(self.len)
    }
}

/// A temp-file spill of a large value (fo-dicom temp-file byte buffer; BUF-02).
///
/// The file is removed when the buffer is dropped. Field order matters: the
/// [`FileWindow`] (and its open handle) must drop before `TempFile` deletes
/// the path, which is required on Windows.
#[derive(Debug)]
pub struct TempFileSpill {
    window: FileWindow,
    _temp: TempFile,
}

impl TempFileSpill {
    /// Writes `bytes` to a temp file and windows the whole file.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> io::Result<Self> {
        use std::io::Write as _;
        let bytes = bytes.as_ref();
        let (path, mut file) = create_temp("spill")?;
        file.write_all(bytes)?;
        file.flush()?;
        drop(file);
        let window = FileWindow::open(&path, 0, bytes.len() as u64)?;
        Ok(TempFileSpill {
            window,
            _temp: TempFile { path },
        })
    }

    /// Reads exactly `len` bytes from `reader` into a temp file, then windows it.
    pub fn from_reader<R: Read>(mut reader: R, len: u64) -> io::Result<Self> {
        use std::io::Write as _;
        let (path, mut file) = create_temp("spill")?;
        let mut remaining = len;
        let mut chunk = vec![0u8; 8192];
        while remaining > 0 {
            let take = remaining.min(chunk.len() as u64) as usize;
            reader.read_exact(&mut chunk[..take])?;
            file.write_all(&chunk[..take])?;
            remaining -= take as u64;
        }
        file.flush()?;
        drop(file);
        let window = FileWindow::open(&path, 0, len)?;
        Ok(TempFileSpill {
            window,
            _temp: TempFile { path },
        })
    }
}

impl ByteBuffer for TempFileSpill {
    fn size(&self) -> u64 {
        self.window.size()
    }
    fn is_memory(&self) -> bool {
        false
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        self.window.data()
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.window.get_byte_range(offset, buf)
    }
    fn copy_to(&self, w: &mut dyn io::Write) -> io::Result<u64> {
        self.window.copy_to(w)
    }
}

/// A serialized window over a non-seekable stream (fo-dicom stream byte buffer;
/// BUF-02). The stream is spilled to a temp file on first access, once.
pub struct StreamWindow {
    len: u64,
    source: Mutex<Option<Box<dyn Read + Send>>>,
    spill: once_cell::sync::OnceCell<Result<TempFileSpill, SpillFailure>>,
}

impl StreamWindow {
    /// Wraps `reader`, which will yield exactly `len` bytes.
    pub fn new(reader: Box<dyn Read + Send>, len: u64) -> Self {
        StreamWindow {
            len,
            source: Mutex::new(Some(reader)),
            spill: once_cell::sync::OnceCell::new(),
        }
    }

    fn ensure(&self) -> io::Result<&TempFileSpill> {
        match self.spill.get_or_init(|| self.spill_source()) {
            Ok(spill) => Ok(spill),
            // the source is consumed, so a failed spill is permanent: report the
            // same failure on every later call instead of panicking or retrying
            Err(failure) => Err(failure.to_error()),
        }
    }

    fn spill_source(&self) -> Result<TempFileSpill, SpillFailure> {
        let mut guard = self.source.lock().map_err(|_| SpillFailure {
            kind: io::ErrorKind::Other,
            message: "stream window mutex poisoned".into(),
        })?;
        let reader = guard.take().ok_or_else(|| SpillFailure {
            kind: io::ErrorKind::Other,
            message: "stream window source already consumed".into(),
        })?;
        TempFileSpill::from_reader(reader, self.len).map_err(|e| SpillFailure {
            kind: e.kind(),
            message: e.to_string(),
        })
    }
}

/// A sticky spill failure: `io::Error` is not `Clone`, so the kind and message
/// are kept and a fresh error is built for every caller.
#[derive(Debug)]
struct SpillFailure {
    kind: io::ErrorKind,
    message: String,
}

impl SpillFailure {
    fn to_error(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
}

impl std::fmt::Debug for StreamWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let spilled = matches!(self.spill.get(), Some(Ok(_)));
        f.debug_struct("StreamWindow")
            .field("len", &self.len)
            .field("spilled", &spilled)
            .finish()
    }
}

impl ByteBuffer for StreamWindow {
    fn size(&self) -> u64 {
        self.len
    }
    fn is_memory(&self) -> bool {
        false
    }
    fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
        self.ensure()?.data()
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        checked_range(offset, buf.len(), self.len)?;
        self.ensure()?.get_byte_range(offset, buf)
    }
    fn copy_to(&self, w: &mut dyn io::Write) -> io::Result<u64> {
        self.ensure()?.copy_to(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared<B: ByteBuffer + 'static>(b: B) -> SharedBuffer {
        Arc::new(b)
    }

    #[test]
    fn memory_and_empty() {
        let m = MemoryBuffer::from(vec![1, 2, 3, 4]);
        assert_eq!(m.size(), 4);
        assert!(m.is_memory());
        assert_eq!(&*m.data().unwrap(), &[1, 2, 3, 4]);

        let e = EmptyBuffer;
        assert_eq!(e.size(), 0);
        assert_eq!(EmptyBuffer::shared().size(), 0);
    }

    #[test]
    fn range_view_is_zero_copy() {
        let m = shared(MemoryBuffer::from(vec![0, 1, 2, 3, 4, 5]));
        let r = RangeBuffer::new(m.clone(), 2, 3);
        assert_eq!(r.size(), 3);
        assert_eq!(&*r.data().unwrap(), &[2, 3, 4]);
        assert_eq!(r.as_slice(), Some(&[2, 3, 4][..]));
        let mut out = [0u8; 2];
        r.get_byte_range(1, &mut out).unwrap();
        assert_eq!(out, [3, 4]);
    }

    #[test]
    fn composite_spans_parts() {
        let c = CompositeBuffer::new(vec![
            shared(MemoryBuffer::from(vec![1, 2])),
            shared(MemoryBuffer::from(vec![3])),
            shared(MemoryBuffer::from(vec![4, 5, 6])),
        ]);
        assert_eq!(c.size(), 6);
        assert_eq!(&*c.data().unwrap(), &[1, 2, 3, 4, 5, 6]);
        assert_eq!(c.as_slice(), Some(&[1, 2, 3, 4, 5, 6][..]));
        let mut out = [0u8; 3];
        c.get_byte_range(1, &mut out).unwrap();
        assert_eq!(out, [2, 3, 4]);
        // copy_to streams part by part
        let mut sink = Vec::new();
        assert_eq!(c.copy_to(&mut sink).unwrap(), 6);
        assert_eq!(sink, vec![1, 2, 3, 4, 5, 6]);
    }

    /// Counts how many times each part is materialised, to prove `data()` caches.
    #[derive(Debug)]
    struct CountingBuffer {
        bytes: Vec<u8>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ByteBuffer for CountingBuffer {
        fn size(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn is_memory(&self) -> bool {
            true
        }
        fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(std::borrow::Cow::Borrowed(&self.bytes))
        }
    }

    #[test]
    fn composite_data_is_concatenated_once() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let a = shared(CountingBuffer {
            bytes: vec![1, 2],
            calls: calls.clone(),
        });
        let b = shared(CountingBuffer {
            bytes: vec![3],
            calls: calls.clone(),
        });
        let c = CompositeBuffer::new(vec![a, b]);
        assert_eq!(&*c.data().unwrap(), &[1, 2, 3]);
        assert_eq!(
            &*c.data().unwrap(),
            &[1, 2, 3],
            "second data() reuses the cached concat"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "each part read once"
        );
        // size() still reflects the parts after caching
        assert_eq!(c.size(), 3);
    }

    #[test]
    fn composite_range_read_does_not_materialize_a_part() {
        // `data()` panics so a range read that materialises the part fails loudly.
        #[derive(Debug)]
        struct RangeOnly(Vec<u8>);
        impl ByteBuffer for RangeOnly {
            fn size(&self) -> u64 {
                self.0.len() as u64
            }
            fn is_memory(&self) -> bool {
                true
            }
            fn data(&self) -> io::Result<std::borrow::Cow<'_, [u8]>> {
                panic!("data() must not be called for a range read");
            }
            fn get_byte_range(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
                let start = offset as usize;
                buf.copy_from_slice(&self.0[start..start + buf.len()]);
                Ok(())
            }
        }
        let c = CompositeBuffer::new(vec![
            shared(MemoryBuffer::from(vec![0, 1])),
            shared(RangeOnly(vec![2, 3, 4])),
        ]);
        let mut out = [0u8; 3];
        c.get_byte_range(1, &mut out).unwrap(); // spans part 0 and part 1
        assert_eq!(out, [1, 2, 3]);
    }

    #[test]
    fn endian_view_swaps_units() {
        let le = shared(MemoryBuffer::from(vec![0x01, 0x02, 0x03, 0x04]));
        let swapped = EndianBuffer::new(le.clone(), 2);
        assert_eq!(&*swapped.data().unwrap(), &[0x02, 0x01, 0x04, 0x03]);
        // unit 1 wrapping is a no-op (returns the inner buffer)
        let same = EndianBuffer::wrap_if_needed(le.clone(), 1);
        assert!(Arc::ptr_eq(&same, &le));
        let mut out = [0u8; 2];
        swapped.get_byte_range(2, &mut out).unwrap();
        assert_eq!(out, [0x04, 0x03]);
    }

    #[test]
    fn even_length_pads_odd() {
        let odd = shared(MemoryBuffer::from(vec![1, 2, 3]));
        let padded = EvenLengthBuffer::wrap_if_needed(odd);
        assert_eq!(padded.size(), 4);
        assert_eq!(&*padded.data().unwrap(), &[1, 2, 3, 0]);
        let even = shared(MemoryBuffer::from(vec![1, 2]));
        assert!(Arc::ptr_eq(
            &EvenLengthBuffer::wrap_if_needed(even.clone()),
            &even
        ));
    }

    #[test]
    fn bulk_data_uri_carries_the_uri_not_bytes() {
        let uri = BulkDataUri::new("http://example/x");
        assert_eq!(uri.uri(), "http://example/x");
        assert_eq!(uri.bulk_uri(), Some("http://example/x"));
        assert_eq!(uri.size(), 0, "no bytes loaded");
        assert!(uri.data().unwrap().is_empty());

        // a resolved buffer reports the bytes but still exposes the URI
        let resolved = BulkDataUri::with_data("http://example/y", vec![1, 2, 3]);
        assert_eq!(resolved.bulk_uri(), Some("http://example/y"));
        assert_eq!(resolved.size(), 3);
        assert_eq!(&*resolved.data().unwrap(), &[1, 2, 3]);

        // an ordinary buffer has no bulk URI
        assert_eq!(MemoryBuffer::from(vec![1]).bulk_uri(), None);
    }

    #[test]
    fn lazy_materializes_once() {
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = counter.clone();
        let lazy = LazyBuffer::new(move || {
            c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vec![9, 8, 7]
        });
        assert_eq!(lazy.size(), 3);
        assert_eq!(&*lazy.data().unwrap(), &[9, 8, 7]);
        assert_eq!(
            &*lazy.data().unwrap(),
            &[9, 8, 7],
            "second access uses the cache"
        );
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn file_window_reads_lazily_and_in_ranges() {
        let path = std::env::temp_dir().join(format!("radicom-test-fw-{}.bin", std::process::id()));
        let payload: Vec<u8> = (0..100u8).collect();
        std::fs::write(&path, &payload).unwrap();
        {
            let w = FileWindow::open(&path, 10, 20).unwrap();
            assert_eq!(w.size(), 20);
            assert!(!w.is_memory(), "a file window is not in memory");
            assert_eq!(&*w.data().unwrap(), &payload[10..30]);
            assert_eq!(
                &*w.data().unwrap(),
                &payload[10..30],
                "second read uses the cache"
            );
            let mut out = [0u8; 5];
            w.get_byte_range(3, &mut out).unwrap();
            assert_eq!(out, payload[13..18]);
            let mut sink = Vec::new();
            assert_eq!(w.copy_to(&mut sink).unwrap(), 20);
            assert_eq!(sink, payload[10..30]);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn windows_over_one_shared_file_read_independently() {
        let path =
            std::env::temp_dir().join(format!("radicom-test-fw-shared-{}.bin", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        {
            let file = Arc::new(std::fs::File::open(&path).unwrap());
            let first = FileWindow::from_shared(file.clone(), 2, 3);
            let second = FileWindow::from_shared(file, 7, 3);
            assert_eq!(&*first.data().unwrap(), b"234");
            assert_eq!(&*second.data().unwrap(), b"789");
            let mut out = [0u8; 2];
            first.get_byte_range(1, &mut out).unwrap();
            assert_eq!(
                &out, b"34",
                "positional reads are independent of the other window"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn temp_file_spill_round_trips() {
        let payload: Vec<u8> = (0..50u8).collect();
        let spill = TempFileSpill::from_bytes(&payload).unwrap();
        assert_eq!(spill.size(), 50);
        assert!(!spill.is_memory());
        assert_eq!(&*spill.data().unwrap(), &payload[..]);
        let mut out = [0u8; 4];
        spill.get_byte_range(10, &mut out).unwrap();
        assert_eq!(out, payload[10..14]);
        let mut sink = Vec::new();
        assert_eq!(spill.copy_to(&mut sink).unwrap(), 50);
        assert_eq!(sink, payload);
    }

    #[test]
    fn stream_window_spills_a_non_seekable_stream_once() {
        let payload: Vec<u8> = (0..40u8).collect();
        let reader = std::io::Cursor::new(payload.clone());
        let sw = StreamWindow::new(Box::new(reader), payload.len() as u64);
        assert_eq!(sw.size(), 40);
        assert!(!sw.is_memory());
        assert_eq!(&*sw.data().unwrap(), &payload[..]);
        assert_eq!(
            &*sw.data().unwrap(),
            &payload[..],
            "spill happens once, then cached"
        );
        let mut out = [0u8; 3];
        sw.get_byte_range(5, &mut out).unwrap();
        assert_eq!(out, payload[5..8]);
    }

    /// A file shorter than the window is an error, not a panic (the open path
    /// cannot know the length up front).
    #[test]
    fn truncated_file_window_reports_eof_instead_of_panicking() {
        let path =
            std::env::temp_dir().join(format!("radicom-test-fw-short-{}.bin", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        {
            let w = FileWindow::open(&path, 0, 20).unwrap();
            let err = w.data().unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
            // the failure is not cached: a second read reports it again
            assert_eq!(w.data().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
            let mut out = [0u8; 4];
            assert_eq!(
                w.get_byte_range(16, &mut out).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
            let mut sink = Vec::new();
            assert_eq!(
                w.copy_to(&mut sink).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
        std::fs::remove_file(&path).ok();
    }

    /// A read outside the buffer is `InvalidInput`, never a panic (release
    /// builds used to panic in the default implementation).
    #[test]
    fn out_of_range_reads_are_errors() {
        let m = MemoryBuffer::from(vec![1, 2, 3]);
        let mut out = [0u8; 4];
        assert_eq!(
            m.get_byte_range(1, &mut out).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            m.get_byte_range(u64::MAX, &mut out).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        let odd = shared(MemoryBuffer::from(vec![1, 2, 3]));
        let padded = EvenLengthBuffer::wrap_if_needed(odd);
        let mut five = [0u8; 5];
        assert_eq!(
            padded.get_byte_range(0, &mut five).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        // the padding byte itself is readable
        let mut pad = [0u8; 1];
        padded.get_byte_range(3, &mut pad).unwrap();
        assert_eq!(pad, [0]);
    }

    /// A failed spill cannot be retried (the source is gone), so the error is
    /// sticky and every later access reports it instead of panicking.
    #[test]
    fn stream_window_spill_failure_is_sticky() {
        let payload = vec![1u8, 2, 3];
        let sw = StreamWindow::new(Box::new(std::io::Cursor::new(payload)), 10);
        assert_eq!(sw.size(), 10, "size() never spills");
        let first = sw.data().unwrap_err();
        assert_eq!(first.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(sw.data().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(
            sw.get_byte_range(0, &mut [0u8; 2]).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            sw.copy_to(&mut Vec::new()).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn temp_files_use_fresh_paths_and_never_follow_an_existing_one() {
        let (first, file1) = create_temp("spill").unwrap();
        let (second, file2) = create_temp("spill").unwrap();
        assert_ne!(first, second, "concurrent spills must not share a path");
        // the name is not reused: create_new on an existing path fails instead
        // of following it (temp-file symlink race)
        assert!(std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&first)
            .is_err());
        drop(file1);
        drop(file2);
        std::fs::remove_file(&first).ok();
        std::fs::remove_file(&second).ok();
    }

    #[test]
    fn window_accessors_endian_edges_and_lazy_debug() {
        let path = std::env::temp_dir().join(format!("radicom-acc-{}.bin", std::process::id()));
        std::fs::write(&path, b"0123456789").unwrap();
        {
            let fw = FileWindow::open(&path, 2, 4).unwrap();
            assert_eq!(fw.offset(), 2);
            assert_eq!(fw.len(), 4);
            assert!(!fw.is_empty());
            assert!(!fw.is_memory());
            assert_eq!(&*fw.data().unwrap(), b"2345");
            let mut out = [0u8; 2];
            assert!(fw.get_byte_range(10, &mut out).is_err());
            let shared_file = Arc::new(std::fs::File::open(&path).unwrap());
            let from_shared = FileWindow::from_shared(shared_file, 0, 3);
            assert_eq!(&*from_shared.data().unwrap(), b"012");
            let empty =
                FileWindow::from_shared(Arc::new(std::fs::File::open(&path).unwrap()), 0, 0);
            assert!(empty.is_empty());
        }
        std::fs::remove_file(&path).ok();

        let composite = CompositeBuffer::new(vec![shared(MemoryBuffer::from(vec![1u8]))]);
        assert_eq!(composite.parts().len(), 1);
        assert!(composite.is_memory());

        let inner = shared(MemoryBuffer::from(vec![1u8, 2, 3, 4, 5, 6]));
        let endian = EndianBuffer::new(inner.clone(), 4);
        assert_eq!(endian.unit(), 4);
        assert_eq!(endian.inner().size(), 6);
        assert!(endian.is_memory());
        let mut out = [0u8; 2];
        endian.get_byte_range(1, &mut out).unwrap();
        assert!(endian.get_byte_range(100, &mut out).is_err());

        let even = EvenLengthBuffer::new(shared(MemoryBuffer::from(vec![7u8, 8])));
        assert!(even.is_memory());
        assert_eq!(&*even.data().unwrap(), &[7, 8]);
        let mut out = [0u8; 1];
        even.get_byte_range(1, &mut out).unwrap();
        assert_eq!(out, [8]);

        let lazy = LazyBuffer::new(|| vec![1u8]);
        assert!(lazy.is_memory());
        assert!(format!("{lazy:?}").contains("materialized"));

        let sw = StreamWindow::new(Box::new(std::io::Cursor::new(b"abcdefgh".to_vec())), 8);
        assert!(format!("{sw:?}").contains("len"));
        assert_eq!(&*sw.data().unwrap(), b"abcdefgh");

        let uri = BulkDataUri::new("file:///x");
        assert!(!uri.is_memory());
        assert!(uri.as_slice().is_none());
    }

    #[test]
    fn endian_buffer_caches_the_swapped_bytes() {
        let inner = shared(MemoryBuffer::from(vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06]));
        let endian = EndianBuffer::new(inner, 2);
        let first = endian.data().unwrap();
        assert_eq!(&*first, &[0x02, 0x01, 0x04, 0x03, 0x06, 0x05]);
        let first_ptr = first.as_ptr();
        drop(first);

        // the second whole-value read reuses the cache instead of copying again
        let second = endian.data().unwrap();
        assert_eq!(second.as_ptr(), first_ptr);
        // and range reads are served from it
        let mut range = [0u8; 4];
        endian.get_byte_range(2, &mut range).unwrap();
        assert_eq!(range, [0x04, 0x03, 0x06, 0x05]);
        assert!(endian.get_byte_range(5, &mut range).is_err());
    }

    #[test]
    fn range_and_memory_buffer_accessors() {
        let memory = MemoryBuffer::from(vec![1u8, 2, 3]);
        assert!(memory.is_memory());
        assert_eq!(memory.as_slice(), Some(&[1, 2, 3][..]));
        let range = RangeBuffer::new(shared(memory), 1, 2);
        assert_eq!(range.offset(), 1);
        assert_eq!(range.len(), 2);
        assert!(!range.is_empty());
        assert_eq!(range.inner().size(), 3);
        assert_eq!(&*range.data().unwrap(), &[2, 3]);
    }
}
