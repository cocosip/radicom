//! Byte buffer abstraction (BUF-01, BUF-02; docs/05-file-io.md §1).
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
#[derive(Debug, Clone, Default)]
pub struct CompositeBuffer {
    parts: Vec<SharedBuffer>,
}

impl CompositeBuffer {
    pub fn new(parts: Vec<SharedBuffer>) -> Self {
        CompositeBuffer { parts }
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
        let mut out = Vec::with_capacity(self.size() as usize);
        for p in &self.parts {
            out.extend_from_slice(&p.data());
        }
        std::borrow::Cow::Owned(out)
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
        if inner.size() % 2 == 0 {
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
        if self.inner.size() % 2 == 0 {
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
}
