//! Byte sources and targets over `Read+Seek` / `Write`
//! (IO-02; docs/05-file-io.md §2).

use crate::error::ReaderError;
use crate::model::buffer::{MemoryBuffer, SharedBuffer};
use crate::model::endian::Endian;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Arc;

/// Supplies a lazy window over `[absolute_offset, absolute_offset+len)` of the
/// underlying seekable source (FILE-02). A file-backed reader attaches one so
/// the parser can leave large values on disk.
pub trait WindowSource: Send + Sync {
    fn window(&self, absolute_offset: u64, len: u64) -> io::Result<SharedBuffer>;
}

/// Sequential/seekable byte reader with endianness-aware scalar reads.
pub struct ByteSourceReader<R: Read + Seek> {
    inner: R,
    pub endian: Endian,
    window: Option<Arc<dyn WindowSource>>,
}

impl<R: Read + Seek> ByteSourceReader<R> {
    pub fn new(inner: R, endian: Endian) -> Self {
        ByteSourceReader { inner, endian, window: None }
    }

    /// Attaches a window source so large values can stay off-heap (FILE-02).
    pub fn with_window_source(mut self, window: Arc<dyn WindowSource>) -> Self {
        self.window = Some(window);
        self
    }

    /// A lazy window over the value about to be read (does not advance).
    /// `None` when this reader has no window source.
    pub fn window_at_current(&mut self, len: u64) -> io::Result<Option<SharedBuffer>> {
        let pos = self.inner.stream_position()?;
        match &self.window {
            Some(source) => Ok(Some(source.window(pos, len)?)),
            None => Ok(None),
        }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    pub fn position(&mut self) -> io::Result<u64> {
        self.inner.stream_position()
    }

    /// Absolute seek (parser rewind for stop predicates).
    pub fn goto(&mut self, pos: u64) -> io::Result<()> {
        self.inner.seek(SeekFrom::Start(pos))?;
        Ok(())
    }

    /// Relative skip.
    pub fn skip(&mut self, n: u64) -> io::Result<()> {
        self.inner.seek(SeekFrom::Current(n as i64))?;
        Ok(())
    }

    /// True when fewer than `n` bytes remain.
    pub fn require(&mut self, n: u64) -> Result<(), ReaderError> {
        let pos = self.inner.stream_position().map_err(|e| ReaderError::Other(e.to_string()))?;
        let end = self
            .inner
            .seek(SeekFrom::End(0))
            .map_err(|e| ReaderError::Other(e.to_string()))?;
        self.inner.seek(SeekFrom::Start(pos)).map_err(|e| ReaderError::Other(e.to_string()))?;
        if end - pos < n {
            return Err(ReaderError::Eof { position: pos, needed: n });
        }
        Ok(())
    }

    pub fn is_eof(&mut self) -> io::Result<bool> {
        let pos = self.inner.stream_position()?;
        let end = self.inner.seek(SeekFrom::End(0))?;
        self.inner.seek(SeekFrom::Start(pos))?;
        Ok(pos >= end)
    }

    pub fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_exact(buf)
    }

    pub fn read_u8(&mut self) -> io::Result<u8> {
        let mut b = [0u8; 1];
        self.inner.read_exact(&mut b)?;
        Ok(b[0])
    }

    pub fn read_u16(&mut self) -> io::Result<u16> {
        let mut b = [0u8; 2];
        self.inner.read_exact(&mut b)?;
        Ok(match self.endian {
            Endian::Little => u16::from_le_bytes(b),
            Endian::Big => u16::from_be_bytes(b),
        })
    }

    pub fn read_u32(&mut self) -> io::Result<u32> {
        let mut b = [0u8; 4];
        self.inner.read_exact(&mut b)?;
        Ok(match self.endian {
            Endian::Little => u32::from_le_bytes(b),
            Endian::Big => u32::from_be_bytes(b),
        })
    }

    pub fn read_u64(&mut self) -> io::Result<u64> {
        let mut b = [0u8; 8];
        self.inner.read_exact(&mut b)?;
        Ok(match self.endian {
            Endian::Little => u64::from_le_bytes(b),
            Endian::Big => u64::from_be_bytes(b),
        })
    }

    pub fn read_f32(&mut self) -> io::Result<f32> {
        Ok(f32::from_bits(self.read_u32()?))
    }

    pub fn read_f64(&mut self) -> io::Result<f64> {
        Ok(f64::from_bits(self.read_u64()?))
    }

    pub fn read_bytes(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; n];
        self.inner.read_exact(&mut v)?;
        Ok(v)
    }

    /// Reads `n` bytes as an owning memory buffer. Large-object windows
    /// (`File`/`Stream` buffers, ReadLargeOnDemand) arrive with the
    /// file-level reader; the parser routes through this hook.
    pub fn read_buffer(&mut self, n: u64) -> io::Result<SharedBuffer> {
        Ok(std::sync::Arc::new(MemoryBuffer::from(self.read_bytes(n as usize)?)))
    }
}

/// Endianness-aware writer with chunked buffer streaming.
pub struct ByteTargetWriter<W: Write> {
    inner: W,
    pub endian: Endian,
}

/// Chunk size for streaming buffers to the target.
pub const WRITE_CHUNK: usize = 1024 * 1024;

impl<W: Write> ByteTargetWriter<W> {
    pub fn new(inner: W, endian: Endian) -> Self {
        ByteTargetWriter { inner, endian }
    }

    pub fn into_inner(self) -> W {
        self.inner
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_all(bytes)
    }

    pub fn write_u8(&mut self, v: u8) -> io::Result<()> {
        self.inner.write_all(&[v])
    }

    pub fn write_u16(&mut self, v: u16) -> io::Result<()> {
        self.inner.write_all(&match self.endian {
            Endian::Little => v.to_le_bytes(),
            Endian::Big => v.to_be_bytes(),
        })
    }

    pub fn write_u32(&mut self, v: u32) -> io::Result<()> {
        self.inner.write_all(&match self.endian {
            Endian::Little => v.to_le_bytes(),
            Endian::Big => v.to_be_bytes(),
        })
    }

    pub fn write_u64(&mut self, v: u64) -> io::Result<()> {
        self.inner.write_all(&match self.endian {
            Endian::Little => v.to_le_bytes(),
            Endian::Big => v.to_be_bytes(),
        })
    }

    pub fn write_f32(&mut self, v: f32) -> io::Result<()> {
        self.write_u32(v.to_bits())
    }

    pub fn write_f64(&mut self, v: f64) -> io::Result<()> {
        self.write_u64(v.to_bits())
    }

    /// Streams a buffer in 1 MiB chunks (spec: large values never buffer
    /// wholesale).
    pub fn write_buffer(&mut self, buffer: &dyn crate::model::buffer::ByteBuffer) -> io::Result<u64> {
        let mut written = 0u64;
        let mut chunk = vec![0u8; WRITE_CHUNK];
        let mut offset = 0u64;
        loop {
            let take = (buffer.size() - offset).min(WRITE_CHUNK as u64) as usize;
            if take == 0 {
                break;
            }
            buffer.get_byte_range(offset, &mut chunk[..take]);
            self.inner.write_all(&chunk[..take])?;
            offset += take as u64;
            written += take as u64;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_scalar_reads_both_endians() {
        let mut data = Vec::new();
        data.extend_from_slice(&0x1234u16.to_le_bytes());
        data.extend_from_slice(&0x5678u16.to_le_bytes());
        data.extend_from_slice(&13.5f64.to_le_bytes());
        let mut le = ByteSourceReader::new(io::Cursor::new(data.clone()), Endian::Little);
        assert_eq!(le.read_u16().unwrap(), 0x1234);
        assert_eq!(le.read_u16().unwrap(), 0x5678);
        le.goto(4).unwrap();
        assert_eq!(le.read_u32().unwrap(), 0, "low half of the f64 payload");
        le.goto(4).unwrap();
        assert_eq!(le.read_f64().unwrap(), 13.5);

        let mut be = ByteSourceReader::new(io::Cursor::new(data), Endian::Big);
        assert_eq!(be.read_u16().unwrap(), 0x3412);
        assert_eq!(be.read_u32().unwrap(), 0x7856_0000);
    }

    #[test]
    fn skip_require_and_eof() {
        let mut src = ByteSourceReader::new(io::Cursor::new(vec![1, 2, 3, 4]), Endian::Little);
        src.require(4).unwrap();
        src.skip(2).unwrap();
        assert_eq!(src.position().unwrap(), 2);
        src.require(2).unwrap();
        assert!(src.require(3).is_err(), "only 2 bytes remain");
        assert!(!src.is_eof().unwrap());
        src.skip(2).unwrap();
        assert!(src.is_eof().unwrap());
        assert!(src.require(1).is_err());
    }

    #[test]
    fn byte_reads_and_buffers() {
        let mut src = ByteSourceReader::new(io::Cursor::new(vec![0xFF, 10, 20, 30]), Endian::Little);
        assert_eq!(src.read_u8().unwrap(), 0xFF);
        let bytes = src.read_bytes(3).unwrap();
        assert_eq!(bytes, vec![10, 20, 30]);
        src.goto(1).unwrap();
        let buf = src.read_buffer(3).unwrap();
        assert_eq!(&*buf.data(), &[10, 20, 30]);
    }

    #[test]
    fn target_scalar_and_buffer_writes() {
        let mut le = ByteTargetWriter::new(Vec::new(), Endian::Little);
        le.write_u16(0x1234).unwrap();
        le.write_u32(0x5678_9ABC).unwrap();
        le.write_f32(1.0).unwrap();
        let out = le.into_inner();
        assert_eq!(&out[..2], &[0x34, 0x12]);
        assert_eq!(&out[2..6], &[0xBC, 0x9A, 0x78, 0x56]);
        assert_eq!(&out[6..10], &[0, 0, 0x80, 0x3F]);

        let mut be = ByteTargetWriter::new(Vec::new(), Endian::Big);
        be.write_u16(0x1234).unwrap();
        assert_eq!(be.into_inner(), vec![0x12, 0x34]);

        // chunked buffer write
        let payload: Vec<u8> = (0..=255u8).collect();
        let buf = MemoryBuffer::from(payload.clone());
        let mut t = ByteTargetWriter::new(Vec::new(), Endian::Little);
        let n = t.write_buffer(&buf).unwrap();
        assert_eq!(n, 256);
        assert_eq!(t.into_inner(), payload);
    }
}
