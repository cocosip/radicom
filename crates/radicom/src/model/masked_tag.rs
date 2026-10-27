//! Masked (repeating-group) tags (TAG-05; docs/04-data-model.md §1.4).

use crate::error::{DataError, Error};

/// A tag with wildcard nibbles, e.g. `(60xx,3000)` for overlay groups.
///
/// `card` holds the fixed bits, `mask` has a `0xF` nibble per fixed hex
/// digit and `0x0` per `x`. Matches when `(tag & mask) == card & mask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaskedTag {
    card: u32,
    mask: u32,
}

impl MaskedTag {
    /// Builds from nibble parts; `None` nibbles in `mask` mark wildcards.
    pub const fn new(card: u32, mask: u32) -> Self {
        MaskedTag { card, mask }
    }

    /// Exact tag shape: `(GGGG,EEEE)` with full mask.
    pub const fn exact(tag: u32) -> Self {
        MaskedTag { card: tag, mask: 0xFFFF_FFFF }
    }

    pub const fn card(&self) -> u32 {
        self.card
    }

    pub const fn mask(&self) -> u32 {
        self.mask
    }

    pub fn matches(&self, tag_group: u16, tag_element: u16) -> bool {
        let tag = ((tag_group as u32) << 16) | tag_element as u32;
        (tag & self.mask) == (self.card & self.mask)
    }

    pub fn matches_u32(&self, tag: u32) -> bool {
        (tag & self.mask) == (self.card & self.mask)
    }

    /// Parses `(60xx,3000)`, `60xx,3000` or `60XX3000`; `x` (any case)
    /// marks wildcard nibbles.
    pub fn parse(s: &str) -> Result<Self, Error> {
        let s = s.trim();
        let body = s.strip_prefix('(').unwrap_or(s);
        let body = body.strip_suffix(')').unwrap_or(body);
        let invalid = || {
            Error::Data(DataError::Other { message: format!("invalid masked tag \"{s}\"") })
        };
        let hex = if let Some((g, e)) = body.split_once(',') {
            let g = g.trim();
            let e = e.trim();
            if g.len() != 4 || e.len() != 4 {
                return Err(invalid());
            }
            format!("{g}{e}")
        } else if body.len() == 8 {
            body.to_string()
        } else {
            return Err(invalid());
        };
        if hex.len() != 8 || !hex.chars().all(|c| c == 'x' || c == 'X' || c.is_ascii_hexdigit()) {
            return Err(invalid());
        }
        let mut card = 0u32;
        let mut mask = 0u32;
        for c in hex.chars() {
            card <<= 4;
            mask <<= 4;
            if c == 'x' || c == 'X' {
                mask |= 0x0;
            } else {
                card |= c.to_digit(16).expect("checked hex digit");
                mask |= 0xF;
            }
        }
        Ok(MaskedTag { card, mask })
    }

    /// Display with wildcard nibbles shown as `x`, e.g. `(60xx,3000)`.
    pub fn to_general_string(&self) -> String {
        let group = (self.card >> 16) as u16;
        let element = self.card as u16;
        let gmask = (self.mask >> 16) as u16;
        let emask = self.mask as u16;
        let mut s = String::with_capacity(12);
        s.push('(');
        for i in (0..4).rev() {
            let nib = (group >> (i * 4)) & 0xF;
            if (gmask >> (i * 4)) & 0xF == 0 {
                s.push('x');
            } else {
                s.push_str(&format!("{nib:X}"));
            }
        }
        s.push(',');
        for i in (0..4).rev() {
            let nib = (element >> (i * 4)) & 0xF;
            if (emask >> (i * 4)) & 0xF == 0 {
                s.push('x');
            } else {
                s.push_str(&format!("{nib:X}"));
            }
        }
        s.push(')');
        s
    }
}

impl std::fmt::Display for MaskedTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_general_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_round_trip() {
        let m = MaskedTag::parse("(60xx,3000)").unwrap();
        assert_eq!(m.card(), 0x6000_3000);
        assert_eq!(m.mask(), 0xFF00_FFFF);
        assert_eq!(m.to_general_string(), "(60xx,3000)");
        assert_eq!(m.to_string(), "(60xx,3000)");

        assert_eq!(MaskedTag::parse("60XX,3000").unwrap(), m);
        assert_eq!(MaskedTag::parse("60xx3000").unwrap(), m);
        assert_eq!(MaskedTag::parse("(0028,0010)").unwrap(), MaskedTag::exact(0x0028_0010));
    }

    #[test]
    fn matching() {
        let overlay_data = MaskedTag::parse("(60xx,3000)").unwrap();
        assert!(overlay_data.matches(0x6002, 0x3000));
        assert!(overlay_data.matches(0x60FE, 0x3000));
        assert!(!overlay_data.matches(0x6002, 0x4000));
        assert!(!overlay_data.matches(0x6102, 0x3000), "6102 & FF00 = 6100 ≠ 6000");
        assert!(overlay_data.matches_u32(0x6014_3000));

        // all-wildcard and fully-fixed shapes
        let any = MaskedTag::parse("(xxxx,xxxx)").unwrap();
        assert!(any.matches(0xFFFF, 0xFFFF));
        let fixed = MaskedTag::exact(0x7FE0_0010);
        assert!(fixed.matches(0x7FE0, 0x0010));
        assert!(!fixed.matches(0x7FE0, 0x0011));
    }

    #[test]
    fn parse_errors() {
        assert!(MaskedTag::parse("").is_err());
        assert!(MaskedTag::parse("(60xx,30)").is_err());
        assert!(MaskedTag::parse("(60x,3000)").is_err());
        assert!(MaskedTag::parse("(GGGG,EEEE)").is_err());
        assert!(MaskedTag::parse("60xx300").is_err());
    }
}
