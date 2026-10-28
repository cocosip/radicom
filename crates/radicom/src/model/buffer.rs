//! Byte buffer abstraction (BUF-01, BUF-02; docs/01-architecture.md §7).
//!
//! All large values are buffer references, never immediate copies. This
//! module ships the memory-domain implementations; file/stream windows and
//! `BulkDataUri` arrive with the parser (M3) and JSON (M7).

use bytes::Bytes;
use std::io;
use std::sync::Arc;

/// A reference to a possibly lazily materialized byte sequence.
pub trait ByteBuffer: Send + Sync + std::fmt::Debug {
    fn size(&self) -> u64;

    /// True when [`ByteBuffer::data`] is cheap (already in memory).
    fn is_memory(&self) -> bool;

    /// Materializes the whole value (may trigger a lazy load).
    fn data(&self) -> std::borrow::Cow<'_, [u8]>;

    /// The bulk-data URI this buffer stands for, when it is a [`BulkDataUri`]
    /// (JSON `BulkDataURI` carrier, docs/08 §1.1). Defaults to `None`.
    fn bulk_uri(&self) -> Option<&str> {
        None
    }

    /// Copies `buf.len()` bytes starting at `offset`; out-of-bounds is a
    /// caller bug and asserts in debug builds.
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) {
        let data = self.data();
        let start = offset as usize;
        let end = start + buf.len();
        debug_assert!(end <= data.len(), "range {start}..{end} out of bounds {}", data.len());
        buf.copy_from_slice(&data[start..end.min(data.len())]);
    }

    /// Streams the value into `w` in chunks; returns the byte count.
    fn copy_to(&self, w: &mut dyn io::Write) -> io::Result<u64> {
        let data = self.data();
        w.write_all(&data)?;
        Ok(data.len() as u64)
    }
}

/// A reference to bytes that live outside the dataset, addressed by URI
/// (fo-dicom `BulkDataUriByteBuffer`; JSON `BulkDataURI` carrier, docs/08 §1.1).
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
        BulkDataUri { uri: uri.into(), fallback: None }
    }

    /// A URI buffer backed by already-loaded bytes (e.g. after a fetch).
    pub fn with_data(uri: impl Into<String>, data: impl Into<Bytes>) -> Self {
        BulkDataUri { uri: uri.into(), fallback: Some(data.into()) }
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
        true
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        match &self.fallback {
            Some(bytes) => std::borrow::Cow::Borrowed(bytes),
            None => std::borrow::Cow::Borrowed(&[]),
        }
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
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(&self.0)
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
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(&[])
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
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        let data = self.inner.data();
        match data {
            std::borrow::Cow::Borrowed(b) => {
                std::borrow::Cow::Borrowed(&b[self.offset as usize..(self.offset + self.len) as usize])
            }
            std::borrow::Cow::Owned(b) => {
                std::borrow::Cow::Owned(b[self.offset as usize..(self.offset + self.len) as usize].to_vec())
            }
        }
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) {
        self.inner.get_byte_range(self.offset + offset, buf);
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
        CompositeBuffer { parts, concat: once_cell::sync::OnceCell::new() }
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
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        let bytes = self.concat.get_or_init(|| {
            let mut out = Vec::with_capacity(self.size() as usize);
            for p in &self.parts {
                out.extend_from_slice(&p.data());
            }
            Bytes::from(out)
        });
        std::borrow::Cow::Borrowed(bytes)
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) {
        let mut remaining = offset;
        let mut filled = 0usize;
        for p in &self.parts {
            let size = p.size();
            if remaining >= size {
                remaining -= size;
                continue;
            }
            let take = (buf.len() - filled).min((size - remaining) as usize);
            p.get_byte_range(remaining, &mut buf[filled..filled + take]);
            filled += take;
            remaining = 0;
            if filled == buf.len() {
                return;
            }
        }
        debug_assert_eq!(filled, buf.len(), "range out of bounds");
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
}

impl EndianBuffer {
    pub fn new(inner: SharedBuffer, unit: usize) -> Self {
        EndianBuffer { inner, unit }
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
            Arc::new(EndianBuffer { inner, unit })
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
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        let mut data = self.inner.data().into_owned();
        crate::model::endian::Endian::swap_buffer(&mut data, self.unit);
        std::borrow::Cow::Owned(data)
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) {
        let mut tmp = vec![0u8; buf.len()];
        self.inner.get_byte_range(offset, &mut tmp);
        crate::model::endian::Endian::swap_buffer(&mut tmp, self.unit);
        buf.copy_from_slice(&tmp);
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
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        if self.inner.size().is_multiple_of(2) {
            return self.inner.data();
        }
        let mut out = self.inner.data().into_owned();
        out.push(0);
        std::borrow::Cow::Owned(out)
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) {
        let inner_len = self.inner.size();
        if offset + buf.len() as u64 <= inner_len {
            self.inner.get_byte_range(offset, buf);
        } else {
            let data = self.data();
            buf.copy_from_slice(&data[offset as usize..(offset as usize + buf.len()).min(data.len())]);
        }
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
        f.debug_struct("LazyBuffer").field("materialized", &self.cell.get().is_some()).finish()
    }
}

impl<F: Fn() -> Vec<u8> + Send + Sync> LazyBuffer<F> {
    pub fn new(produce: F) -> Self {
        LazyBuffer { cell: Arc::new(once_cell::sync::OnceCell::new()), produce }
    }
}

impl<F: Fn() -> Vec<u8> + Send + Sync> ByteBuffer for LazyBuffer<F> {
    fn size(&self) -> u64 {
        self.data().len() as u64
    }
    fn is_memory(&self) -> bool {
        true
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        let bytes = self.cell.get_or_init(|| Bytes::from((self.produce)()));
        std::borrow::Cow::Borrowed(bytes)
    }
}

// -- file / stream domain implementations (BUF-02) -----------------------
//
// These keep large values out of the in-memory dataset: a value is a window
// into a file (or a temp-file spill of a non-seekable stream) and bytes are
// read on demand. `data()` materializes the whole value once into a cache,
// while `get_byte_range`/`copy_to` stream without ever holding it all.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Mutex;

/// A process-unique temp path (never collides across concurrent callers/PIDs).
fn temp_path(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    std::env::temp_dir().join(format!("radicom-buf-{}-{seq}-{tag}.tmp", std::process::id()))
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

/// A lazy read-only window over `[offset, offset+len)` of a file
/// (fo-dicom file byte window; BUF-02).
///
/// `get_byte_range` and `copy_to` read straight from disk, so a consumer that
/// only needs part of a large value never materializes it. `data()` does
/// materialize — once, into the `OnceCell` — because the buffer API hands out
/// `&[u8]`; that cache is the ceiling of this design, not a bug.
///
/// Each window owns its own file handle (fo-dicom's `FileByteBuffer` shares
/// one); a file with hundreds of large values therefore holds hundreds of
/// handles while it lives. ponytail: per-window handle, share one `Arc<File>`
/// per parsed file if the handle count ever becomes a problem.
#[derive(Debug)]
pub struct FileWindow {
    file: Mutex<std::fs::File>,
    offset: u64,
    len: u64,
    cache: once_cell::sync::OnceCell<Bytes>,
}

impl FileWindow {
    /// Opens `path` and windows `len` bytes starting at `offset`.
    pub fn open(path: impl AsRef<Path>, offset: u64, len: u64) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).open(path.as_ref())?;
        Ok(FileWindow { file: Mutex::new(file), offset, len, cache: once_cell::sync::OnceCell::new() })
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
        let mut file = self.file.lock().expect("file window mutex poisoned");
        file.seek(SeekFrom::Start(self.offset + offset))?;
        file.read_exact(buf)
    }
}

impl ByteBuffer for FileWindow {
    fn size(&self) -> u64 {
        self.len
    }
    fn is_memory(&self) -> bool {
        false
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        let bytes = self.cache.get_or_init(|| Bytes::from(self.read_all().expect("file window read")));
        std::borrow::Cow::Borrowed(bytes)
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) {
        debug_assert!(offset + buf.len() as u64 <= self.len, "range out of bounds");
        self.read_into(offset, buf).expect("file window range read");
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
        let bytes = bytes.as_ref();
        let path = temp_path("spill");
        std::fs::write(&path, bytes)?;
        let window = FileWindow::open(&path, 0, bytes.len() as u64)?;
        Ok(TempFileSpill { window, _temp: TempFile { path } })
    }

    /// Reads exactly `len` bytes from `reader` into a temp file, then windows it.
    pub fn from_reader<R: Read>(mut reader: R, len: u64) -> io::Result<Self> {
        use std::io::Write as _;
        let path = temp_path("spill");
        let mut file = std::fs::File::create(&path)?;
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
        Ok(TempFileSpill { window, _temp: TempFile { path } })
    }
}

impl ByteBuffer for TempFileSpill {
    fn size(&self) -> u64 {
        self.window.size()
    }
    fn is_memory(&self) -> bool {
        false
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        self.window.data()
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) {
        self.window.get_byte_range(offset, buf);
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
    spill: once_cell::sync::OnceCell<TempFileSpill>,
}

impl StreamWindow {
    /// Wraps `reader`, which will yield exactly `len` bytes.
    pub fn new(reader: Box<dyn Read + Send>, len: u64) -> Self {
        StreamWindow { len, source: Mutex::new(Some(reader)), spill: once_cell::sync::OnceCell::new() }
    }

    fn ensure(&self) -> &TempFileSpill {
        self.spill.get_or_init(|| {
            let mut guard = self.source.lock().expect("stream window mutex poisoned");
            let reader = guard.take().expect("stream window source already consumed");
            TempFileSpill::from_reader(reader, self.len).expect("spill stream to temp file")
        })
    }
}

impl std::fmt::Debug for StreamWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamWindow").field("len", &self.len).field("spilled", &self.spill.get().is_some()).finish()
    }
}

impl ByteBuffer for StreamWindow {
    fn size(&self) -> u64 {
        self.len
    }
    fn is_memory(&self) -> bool {
        false
    }
    fn data(&self) -> std::borrow::Cow<'_, [u8]> {
        self.ensure().data()
    }
    fn get_byte_range(&self, offset: u64, buf: &mut [u8]) {
        self.ensure().get_byte_range(offset, buf);
    }
    fn copy_to(&self, w: &mut dyn io::Write) -> io::Result<u64> {
        self.ensure().copy_to(w)
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
        assert_eq!(&*m.data(), &[1, 2, 3, 4]);

        let e = EmptyBuffer;
        assert_eq!(e.size(), 0);
        assert_eq!(EmptyBuffer::shared().size(), 0);
    }

    #[test]
    fn range_view_is_zero_copy() {
        let m = shared(MemoryBuffer::from(vec![0, 1, 2, 3, 4, 5]));
        let r = RangeBuffer::new(m.clone(), 2, 3);
        assert_eq!(r.size(), 3);
        assert_eq!(&*r.data(), &[2, 3, 4]);
        let mut out = [0u8; 2];
        r.get_byte_range(1, &mut out);
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
        assert_eq!(&*c.data(), &[1, 2, 3, 4, 5, 6]);
        let mut out = [0u8; 3];
        c.get_byte_range(1, &mut out);
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
        fn data(&self) -> std::borrow::Cow<'_, [u8]> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::borrow::Cow::Borrowed(&self.bytes)
        }
    }

    #[test]
    fn composite_data_is_concatenated_once() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let a = shared(CountingBuffer { bytes: vec![1, 2], calls: calls.clone() });
        let b = shared(CountingBuffer { bytes: vec![3], calls: calls.clone() });
        let c = CompositeBuffer::new(vec![a, b]);
        assert_eq!(&*c.data(), &[1, 2, 3]);
        assert_eq!(&*c.data(), &[1, 2, 3], "second data() reuses the cached concat");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2, "each part read once");
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
            fn data(&self) -> std::borrow::Cow<'_, [u8]> {
                panic!("data() must not be called for a range read");
            }
            fn get_byte_range(&self, offset: u64, buf: &mut [u8]) {
                let start = offset as usize;
                buf.copy_from_slice(&self.0[start..start + buf.len()]);
            }
        }
        let c = CompositeBuffer::new(vec![
            shared(MemoryBuffer::from(vec![0, 1])),
            shared(RangeOnly(vec![2, 3, 4])),
        ]);
        let mut out = [0u8; 3];
        c.get_byte_range(1, &mut out); // spans part 0 and part 1
        assert_eq!(out, [1, 2, 3]);
    }

    #[test]
    fn endian_view_swaps_units() {
        let le = shared(MemoryBuffer::from(vec![0x01, 0x02, 0x03, 0x04]));
        let swapped = EndianBuffer::new(le.clone(), 2);
        assert_eq!(&*swapped.data(), &[0x02, 0x01, 0x04, 0x03]);
        // unit 1 wrapping is a no-op (returns the inner buffer)
        let same = EndianBuffer::wrap_if_needed(le.clone(), 1);
        assert!(Arc::ptr_eq(&same, &le));
        let mut out = [0u8; 2];
        swapped.get_byte_range(2, &mut out);
        assert_eq!(out, [0x04, 0x03]);
    }

    #[test]
    fn even_length_pads_odd() {
        let odd = shared(MemoryBuffer::from(vec![1, 2, 3]));
        let padded = EvenLengthBuffer::wrap_if_needed(odd);
        assert_eq!(padded.size(), 4);
        assert_eq!(&*padded.data(), &[1, 2, 3, 0]);
        let even = shared(MemoryBuffer::from(vec![1, 2]));
        assert!(Arc::ptr_eq(&EvenLengthBuffer::wrap_if_needed(even.clone()), &even));
    }

    #[test]
    fn bulk_data_uri_carries_the_uri_not_bytes() {
        let uri = BulkDataUri::new("http://example/x");
        assert_eq!(uri.uri(), "http://example/x");
        assert_eq!(uri.bulk_uri(), Some("http://example/x"));
        assert_eq!(uri.size(), 0, "no bytes loaded");
        assert!(uri.data().is_empty());

        // a resolved buffer reports the bytes but still exposes the URI
        let resolved = BulkDataUri::with_data("http://example/y", vec![1, 2, 3]);
        assert_eq!(resolved.bulk_uri(), Some("http://example/y"));
        assert_eq!(resolved.size(), 3);
        assert_eq!(&*resolved.data(), &[1, 2, 3]);

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
        assert_eq!(&*lazy.data(), &[9, 8, 7]);
        assert_eq!(&*lazy.data(), &[9, 8, 7], "second access uses the cache");
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
            assert_eq!(&*w.data(), &payload[10..30]);
            assert_eq!(&*w.data(), &payload[10..30], "second read uses the cache");
            let mut out = [0u8; 5];
            w.get_byte_range(3, &mut out);
            assert_eq!(out, payload[13..18]);
            let mut sink = Vec::new();
            assert_eq!(w.copy_to(&mut sink).unwrap(), 20);
            assert_eq!(sink, payload[10..30]);
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn temp_file_spill_round_trips() {
        let payload: Vec<u8> = (0..50u8).collect();
        let spill = TempFileSpill::from_bytes(&payload).unwrap();
        assert_eq!(spill.size(), 50);
        assert!(!spill.is_memory());
        assert_eq!(&*spill.data(), &payload[..]);
        let mut out = [0u8; 4];
        spill.get_byte_range(10, &mut out);
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
        assert_eq!(&*sw.data(), &payload[..]);
        assert_eq!(&*sw.data(), &payload[..], "spill happens once, then cached");
        let mut out = [0u8; 3];
        sw.get_byte_range(5, &mut out);
        assert_eq!(out, payload[5..8]);
    }
}
