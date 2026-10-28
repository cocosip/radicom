//! 256-entry colour tables (REND-06, docs/10-rendering.md §6).
//!
//! Mirrors fo-dicom `ColorTable`: a 256-entry `Color32` lookup that maps an
//! 8-bit grey level to a display colour. MONOCHROME2 is the ascending grey ramp,
//! MONOCHROME1 the descending one. Colour tables round-trip through the classic
//! 768-byte R,G,B `.lut` layout (256 entries × 3 channels).

use crate::pixel::Color32;

/// A 256-entry colour lookup table (REND-06 `ColorTable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorTable {
    entries: [Color32; 256],
}

impl ColorTable {
    /// The ascending grey ramp `0..=255` (MONOCHROME2).
    pub fn monochrome2() -> Self {
        let mut entries = [Color32::new(0, 0, 0); 256];
        for (i, entry) in entries.iter_mut().enumerate() {
            let v = i as u8;
            *entry = Color32::new(v, v, v);
        }
        ColorTable { entries }
    }

    /// The descending grey ramp `255..=0` (MONOCHROME1).
    pub fn monochrome1() -> Self {
        let mut table = Self::monochrome2();
        table.reverse();
        table
    }

    /// Builds a table from explicit entries.
    pub fn from_entries(entries: [Color32; 256]) -> Self {
        ColorTable { entries }
    }

    /// The raw 256 entries.
    pub fn entries(&self) -> &[Color32; 256] {
        &self.entries
    }

    /// The colour for an 8-bit index.
    pub fn get(&self, index: u8) -> Color32 {
        self.entries[index as usize]
    }

    /// Reverses the table in place (fo-dicom `ColorTable.Reverse`).
    pub fn reverse(&mut self) {
        self.entries.reverse();
    }

    /// Builds a table from a 768-byte R,G,B lookup table (256 × 3); `None` when
    /// the slice is shorter than 768 bytes.
    pub fn from_rgb768(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 768 {
            return None;
        }
        let mut entries = [Color32::new(0, 0, 0); 256];
        for (i, entry) in entries.iter_mut().enumerate() {
            *entry = Color32::new(bytes[i * 3], bytes[i * 3 + 1], bytes[i * 3 + 2]);
        }
        Some(ColorTable { entries })
    }

    /// Serialises to a 768-byte R,G,B lookup table (alpha dropped).
    pub fn to_rgb768(&self) -> [u8; 768] {
        let mut out = [0u8; 768];
        for (i, c) in self.entries.iter().enumerate() {
            out[i * 3] = c.r;
            out[i * 3 + 1] = c.g;
            out[i * 3 + 2] = c.b;
        }
        out
    }
}

impl Default for ColorTable {
    fn default() -> Self {
        Self::monochrome2()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome2_is_an_ascending_grey_ramp() {
        let t = ColorTable::monochrome2();
        assert_eq!(t.get(0), Color32::new(0, 0, 0));
        assert_eq!(t.get(128), Color32::new(128, 128, 128));
        assert_eq!(t.get(255), Color32::new(255, 255, 255));
    }

    #[test]
    fn monochrome1_is_the_reversed_ramp() {
        let t = ColorTable::monochrome1();
        assert_eq!(t.get(0), Color32::new(255, 255, 255));
        assert_eq!(t.get(255), Color32::new(0, 0, 0));
    }

    #[test]
    fn reverse_flips_the_endpoints() {
        let mut t = ColorTable::monochrome2();
        t.reverse();
        assert_eq!(t.get(0), Color32::new(255, 255, 255));
        t.reverse();
        assert_eq!(t.get(0), Color32::new(0, 0, 0));
    }

    #[test]
    fn rgb768_round_trips() {
        let t = ColorTable::monochrome2();
        let bytes = t.to_rgb768();
        assert_eq!(bytes.len(), 768);
        assert_eq!(ColorTable::from_rgb768(&bytes), Some(t));
        // short slices are rejected
        assert!(ColorTable::from_rgb768(&bytes[..767]).is_none());
    }
}
