//! `PixelDataConverter`: bit-order helpers mirroring fo-dicom
//! `Imaging/PixelDataConverter` (REND-06, docs/01-architecture.md §10).

/// Byte-oriented pixel data converters (REND-06).
pub struct PixelDataConverter;

impl PixelDataConverter {
    /// Reverses the bit order within every byte (fo-dicom `ReverseBits`). Used
    /// to turn an MSB-first 1-bit stream into an LSB-first one for indexing.
    pub fn reverse_bits(data: &[u8]) -> Vec<u8> {
        data.iter().map(|b| b.reverse_bits()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_bits_flips_bit_order() {
        assert_eq!(PixelDataConverter::reverse_bits(&[0b1000_0000]), vec![0b0000_0001]);
        assert_eq!(PixelDataConverter::reverse_bits(&[0b0000_1111]), vec![0b1111_0000]);
        assert_eq!(PixelDataConverter::reverse_bits(&[0x00, 0xFF]), vec![0x00, 0xFF]);
    }
}
