//! `BitDepth`: allocated/stored/high-bit/signedness description (REND-06,
//! docs/10-rendering.md §6). Mirrors fo-dicom `Imaging.BitDepth`.

/// Describes how a pixel value is bit-packed (REND-06 `BitDepth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitDepth {
    /// Bits occupied per sample in storage (`(0028,0100)`).
    pub allocated: u16,
    /// Significant bits (`(0028,0102)`); 0 means "unspecified".
    pub stored: u16,
    /// Position of the most significant stored bit (`(0028,0103)`).
    pub high_bit: u16,
    /// `true` when Pixel Representation (`(0028,0103)` … signed) is 1.
    pub signed: bool,
}

impl BitDepth {
    pub const fn new(allocated: u16, stored: u16, high_bit: u16, signed: bool) -> Self {
        BitDepth { allocated, stored, high_bit, signed }
    }

    /// The smallest representable stored value.
    pub fn min_value(&self) -> i64 {
        if self.stored == 0 {
            return 0;
        }
        if self.signed {
            -(1i64 << (self.stored.min(63) - 1))
        } else {
            0
        }
    }

    /// The largest representable stored value.
    pub fn max_value(&self) -> i64 {
        if self.stored == 0 {
            return 0;
        }
        let bits = self.stored.min(63) as u32;
        if self.signed {
            (1i64 << (bits - 1)) - 1
        } else {
            (1i64 << bits) - 1
        }
    }

    /// The next power of two ≥ the allocated storage width (buffer size bucket).
    pub fn next_power_of_two(&self) -> u32 {
        (self.allocated.max(1) as u32).next_power_of_two()
    }

    /// Normalises a raw stored value to the low `stored` bits: left-align on the
    /// high bit, then drop the sub-bit padding. The common low-aligned case
    /// (`high_bit + 1 == stored`) leaves the value unchanged; malformed depth
    /// metadata (stored 0, stored > allocated, high_bit ≥ allocated) passes
    /// through unchanged (docs §5).
    pub fn normalize(&self, value: u16) -> u16 {
        if self.stored == 0 || self.stored > self.allocated || self.high_bit >= self.allocated {
            return value;
        }
        let up = self.allocated - self.high_bit - 1;
        let down = self.allocated - self.stored;
        value.checked_shl(up as u32).unwrap_or(0).wrapping_shr(down as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_and_max_follow_signedness() {
        let unsigned = BitDepth::new(16, 12, 11, false);
        assert_eq!((unsigned.min_value(), unsigned.max_value()), (0, 4095));
        let signed = BitDepth::new(16, 12, 11, true);
        assert_eq!((signed.min_value(), signed.max_value()), (-2048, 2047));
        // unspecified stored depth → (0, 0)
        assert_eq!((BitDepth::new(16, 0, 0, false).min_value(), 0), (0, 0));
    }

    #[test]
    fn normalize_is_identity_for_low_aligned_values() {
        // allocated 16, stored 12, high bit 11 → low-aligned, value unchanged
        let d = BitDepth::new(16, 12, 11, false);
        assert_eq!(d.normalize(0x0FFF), 0x0FFF);
        assert_eq!(d.normalize(0), 0);
    }

    #[test]
    fn normalize_drops_high_bit_padding() {
        // stored 12 bits left-aligned at the top of 16 → shift right by 4
        let d = BitDepth::new(16, 12, 15, false);
        assert_eq!(d.normalize(0xF000), 0x0F00);
    }

    #[test]
    fn next_power_of_two_buckets_the_allocation() {
        assert_eq!(BitDepth::new(8, 8, 7, false).next_power_of_two(), 8);
        assert_eq!(BitDepth::new(12, 12, 11, false).next_power_of_two(), 16);
        assert_eq!(BitDepth::new(16, 16, 15, false).next_power_of_two(), 16);
    }
}
