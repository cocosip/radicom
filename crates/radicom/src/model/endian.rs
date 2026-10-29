//! Byte order shared by the model and IO layers (IO-03).

/// DICOM uses little- and big-endian encodings selected per transfer syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    /// The platform's native byte order (used to skip swaps on write).
    pub fn native() -> Endian {
        if cfg!(target_endian = "big") {
            Endian::Big
        } else {
            Endian::Little
        }
    }

    pub fn is_native(self) -> bool {
        self == Endian::native()
    }

    /// Swaps `size`-wide units in place within `buf` (len must be a
    /// multiple of `size`); no-op for `size <= 1`.
    pub fn swap_buffer(buf: &mut [u8], size: usize) {
        if size <= 1 {
            return;
        }
        debug_assert_eq!(
            buf.len() % size,
            0,
            "buffer length must be a multiple of the unit size"
        );
        for chunk in buf.chunks_exact_mut(size) {
            chunk.reverse();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_units() {
        let mut buf = [0x01u8, 0x02, 0x03, 0x04];
        Endian::swap_buffer(&mut buf, 2);
        assert_eq!(buf, [0x02, 0x01, 0x04, 0x03]);

        let mut buf = [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        Endian::swap_buffer(&mut buf, 8);
        assert_eq!(buf, [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);

        let mut buf = [1u8, 2, 3];
        Endian::swap_buffer(&mut buf, 1);
        assert_eq!(buf, [1, 2, 3], "unit 1 is a no-op");
    }

    #[test]
    fn native_is_consistent() {
        assert!(Endian::native().is_native());
    }
}
