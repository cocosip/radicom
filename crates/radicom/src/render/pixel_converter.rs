//! `PixelDataConverter`: planar/interleaved 24-bit layout conversion and 1-bit
//! bit reversal (REND-06, docs/01-architecture.md §10). Static helpers mirroring
//! fo-dicom `Imaging/PixelDataConverter`.

/// Byte-oriented pixel data converters (REND-06).
pub struct PixelDataConverter;

impl PixelDataConverter {
    /// Interleaved `R G B R G B …` → planar `R…R G…G B…B` for `width*height`
    /// pixels. Short input is zero-padded rather than panicking.
    pub fn interleaved_to_planar_24(data: &[u8], width: usize, height: usize) -> Vec<u8> {
        let count = width * height;
        let mut out = vec![0u8; count * 3];
        for i in 0..count {
            for c in 0..3 {
                out[c * count + i] = *data.get(i * 3 + c).unwrap_or(&0);
            }
        }
        out
    }

    /// Planar `R…R G…G B…B` → interleaved `R G B R G B …` (inverse of
    /// [`interleaved_to_planar_24`](Self::interleaved_to_planar_24)).
    pub fn planar_to_interleaved_24(data: &[u8], width: usize, height: usize) -> Vec<u8> {
        let count = width * height;
        let mut out = vec![0u8; count * 3];
        for i in 0..count {
            for c in 0..3 {
                out[i * 3 + c] = *data.get(c * count + i).unwrap_or(&0);
            }
        }
        out
    }

    /// Reverses the bit order within every byte (fo-dicom `ReverseBits`), used
    /// to normalise 1-bit data stored LSB-first (e.g. Multiframe Single Bit
    /// Secondary Capture). Inverts an 8-bit bitmask, not the pixel values.
    pub fn reverse_bits(data: &[u8]) -> Vec<u8> {
        data.iter().map(|b| b.reverse_bits()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_and_planar_are_inverses() {
        // 2 pixels: (10,20,30) and (40,50,60)
        let interleaved = [10u8, 20, 30, 40, 50, 60];
        let planar = PixelDataConverter::interleaved_to_planar_24(&interleaved, 2, 1);
        assert_eq!(planar, vec![10, 40, 20, 50, 30, 60], "R plane, G plane, B plane");
        let back = PixelDataConverter::planar_to_interleaved_24(&planar, 2, 1);
        assert_eq!(back, interleaved.to_vec());
    }

    #[test]
    fn planar_round_trips_a_2d_image() {
        // 2x2 image, four RGB pixels
        let interleaved: Vec<u8> = (0..12).collect();
        let planar = PixelDataConverter::interleaved_to_planar_24(&interleaved, 2, 2);
        assert_eq!(planar.len(), 12);
        // first plane is every pixel's R channel: 0,3,6,9
        assert_eq!(&planar[..4], &[0, 3, 6, 9]);
        assert_eq!(PixelDataConverter::planar_to_interleaved_24(&planar, 2, 2), interleaved);
    }

    #[test]
    fn reverse_bits_flips_bit_order() {
        assert_eq!(PixelDataConverter::reverse_bits(&[0b1000_0000]), vec![0b0000_0001]);
        assert_eq!(PixelDataConverter::reverse_bits(&[0b0000_1111]), vec![0b1111_0000]);
        assert_eq!(PixelDataConverter::reverse_bits(&[0x00, 0xFF]), vec![0x00, 0xFF]);
    }

    #[test]
    fn short_input_is_zero_padded() {
        // one full pixel plus a truncated one → trailing channels are zero
        let out = PixelDataConverter::interleaved_to_planar_24(&[1, 2, 3], 2, 1);
        assert_eq!(out, vec![1, 0, 2, 0, 3, 0]);
    }
}
