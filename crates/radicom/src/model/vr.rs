//! Value Representation (VR-01, VR-02; docs/01-architecture.md §2).

use crate::error::{DataError, Error};

/// The 34 standard VRs plus internal markers.
///
/// `Item`, `ItemDelim` and `SeqDelim` mark item-family elements during
/// parsing/writing; `Implicit` is the placeholder used when an element's VR
/// could not be determined in explicit-VR context. Internal VRs never appear
/// in serialized VR fields and cannot be produced by [`Vr::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Vr {
    Ae,
    As,
    At,
    Cs,
    Da,
    Ds,
    Dt,
    Fd,
    Fl,
    Is,
    Lo,
    Lt,
    Ob,
    Od,
    Of,
    Ol,
    Ov,
    Ow,
    Pn,
    Sh,
    Sl,
    Sq,
    Ss,
    St,
    Sv,
    Tm,
    Uc,
    Ui,
    Ul,
    Un,
    Ur,
    Us,
    Ut,
    Uv,
    Item,
    ItemDelim,
    SeqDelim,
    Implicit,
}

impl Vr {
    /// Two-letter code; internal VRs report `"NONE"` (never serialized).
    pub const fn code(self) -> &'static str {
        match self {
            Vr::Ae => "AE",
            Vr::As => "AS",
            Vr::At => "AT",
            Vr::Cs => "CS",
            Vr::Da => "DA",
            Vr::Ds => "DS",
            Vr::Dt => "DT",
            Vr::Fd => "FD",
            Vr::Fl => "FL",
            Vr::Is => "IS",
            Vr::Lo => "LO",
            Vr::Lt => "LT",
            Vr::Ob => "OB",
            Vr::Od => "OD",
            Vr::Of => "OF",
            Vr::Ol => "OL",
            Vr::Ov => "OV",
            Vr::Ow => "OW",
            Vr::Pn => "PN",
            Vr::Sh => "SH",
            Vr::Sl => "SL",
            Vr::Sq => "SQ",
            Vr::Ss => "SS",
            Vr::St => "ST",
            Vr::Sv => "SV",
            Vr::Tm => "TM",
            Vr::Uc => "UC",
            Vr::Ui => "UI",
            Vr::Ul => "UL",
            Vr::Un => "UN",
            Vr::Ur => "UR",
            Vr::Us => "US",
            Vr::Ut => "UT",
            Vr::Uv => "UV",
            _ => "NONE",
        }
    }

    pub const fn is_string(self) -> bool {
        matches!(
            self,
            Vr::Ae
                | Vr::As
                | Vr::Cs
                | Vr::Da
                | Vr::Ds
                | Vr::Dt
                | Vr::Is
                | Vr::Lo
                | Vr::Lt
                | Vr::Pn
                | Vr::Sh
                | Vr::St
                | Vr::Tm
                | Vr::Uc
                | Vr::Ui
                | Vr::Ur
                | Vr::Ut
        )
    }

    /// Affected by Specific Character Set (spec §2 "charset-sensitive").
    pub const fn is_character_encoded(self) -> bool {
        matches!(
            self,
            Vr::Lo | Vr::Lt | Vr::Pn | Vr::Sh | Vr::St | Vr::Uc | Vr::Ur | Vr::Ut
        )
    }

    /// 16-bit (short-form) length field; false = 32-bit (long-form).
    pub const fn is_16bit_length(self) -> bool {
        !matches!(
            self,
            Vr::Ob
                | Vr::Od
                | Vr::Of
                | Vr::Ol
                | Vr::Ov
                | Vr::Ow
                | Vr::Sq
                | Vr::Uc
                | Vr::Un
                | Vr::Ur
                | Vr::Ut
                | Vr::Uv
                | Vr::Sv
        )
    }

    /// Values may repeat for this VR. Matches fo-dicom `DicomVR.IsMultiValue`:
    /// false only for LT/ST/UR/UT/SQ (and the internal item markers); every
    /// other VR — string and binary alike — is multi-valued.
    pub const fn is_multi_value(self) -> bool {
        !matches!(
            self,
            Vr::Lt
                | Vr::St
                | Vr::Ur
                | Vr::Ut
                | Vr::Sq
                | Vr::Item
                | Vr::ItemDelim
                | Vr::SeqDelim
                | Vr::Implicit
        )
    }

    /// Value padding byte (spec §2 "pad").
    pub const fn padding(self) -> u8 {
        if matches!(self, Vr::Ui) {
            0x00
        } else if self.is_string() || matches!(self, Vr::Sq) {
            b' '
        } else {
            0x00
        }
    }

    /// Maximum encoded length per value; 0 = unlimited (spec §2 "max len").
    pub const fn max_value_length(self) -> u32 {
        match self {
            Vr::Ae | Vr::Cs | Vr::Ds | Vr::Sh => 16,
            Vr::As => 4,
            Vr::At => 4,
            Vr::Da => 18,
            Vr::Dt => 54,
            Vr::Fd => 8,
            Vr::Fl => 4,
            Vr::Is => 12,
            Vr::Lo => 64,
            Vr::Lt => 10240,
            Vr::Pn => 64,
            Vr::Sl | Vr::Ul => 4,
            Vr::Ss | Vr::Us => 2,
            Vr::St => 1024,
            Vr::Sv | Vr::Uv => 8,
            Vr::Tm => 16,
            Vr::Ui => 64,
            // OB/OD/OF/OL/OV/OW/SQ/UC/UN/UR/UT: unlimited
            _ => 0,
        }
    }

    /// True for the signed integer VRs (SS/SL/SV). Used so binary widening
    /// sign-extends only for signed source values (CM-02).
    pub const fn is_signed_integer(self) -> bool {
        matches!(self, Vr::Ss | Vr::Sl | Vr::Sv)
    }

    /// Byte size of one value unit (spec §2 "unit"); 0 for SQ and the
    /// internal VRs.
    pub const fn unit_size(self) -> usize {
        match self {
            Vr::At => 4,
            Vr::Fd | Vr::Od | Vr::Ov | Vr::Sv | Vr::Uv => 8,
            Vr::Fl | Vr::Of | Vr::Ol => 4,
            Vr::Ob | Vr::Un => 1,
            Vr::Ow => 2,
            Vr::Sl | Vr::Ul => 4,
            Vr::Ss | Vr::Us => 2,
            Vr::Sq | Vr::Item | Vr::ItemDelim | Vr::SeqDelim | Vr::Implicit => 0,
            _ => 1, // string VRs
        }
    }

    /// Unit size used for endianness conversion (spec §2 "swap"): AT
    /// swaps its two 2-byte components; everything else matches
    /// [`Vr::unit_size`].
    pub const fn byte_swap_size(self) -> usize {
        if matches!(self, Vr::At) {
            2
        } else {
            self.unit_size()
        }
    }

    /// True while an explicit-VR parse of these bytes is possible.
    /// Dispatches on the first byte (hot path, VR-02).
    pub fn try_parse(code: &[u8; 2]) -> Option<Vr> {
        let second = code[1];
        let vr = match code[0] {
            b'A' => match second {
                b'E' => Vr::Ae,
                b'S' => Vr::As,
                b'T' => Vr::At,
                _ => return None,
            },
            b'C' if second == b'S' => Vr::Cs,
            b'D' => match second {
                b'A' => Vr::Da,
                b'S' => Vr::Ds,
                b'T' => Vr::Dt,
                _ => return None,
            },
            b'F' => match second {
                b'D' => Vr::Fd,
                b'L' => Vr::Fl,
                _ => return None,
            },
            b'I' if second == b'S' => Vr::Is,
            b'L' => match second {
                b'O' => Vr::Lo,
                b'T' => Vr::Lt,
                _ => return None,
            },
            b'O' => match second {
                b'B' => Vr::Ob,
                b'D' => Vr::Od,
                b'F' => Vr::Of,
                b'L' => Vr::Ol,
                b'V' => Vr::Ov,
                b'W' => Vr::Ow,
                _ => return None,
            },
            b'P' if second == b'N' => Vr::Pn,
            b'S' => match second {
                b'H' => Vr::Sh,
                b'L' => Vr::Sl,
                b'Q' => Vr::Sq,
                b'S' => Vr::Ss,
                b'T' => Vr::St,
                b'V' => Vr::Sv,
                _ => return None,
            },
            b'T' if second == b'M' => Vr::Tm,
            b'U' => match second {
                b'C' => Vr::Uc,
                b'I' => Vr::Ui,
                b'L' => Vr::Ul,
                b'N' => Vr::Un,
                b'R' => Vr::Ur,
                b'S' => Vr::Us,
                b'T' => Vr::Ut,
                b'V' => Vr::Uv,
                _ => return None,
            },
            _ => return None,
        };
        Some(vr)
    }

    /// Like [`Vr::try_parse`] but returns an [`Error`] on unknown codes.
    pub fn parse(code: &[u8; 2]) -> Result<Vr, Error> {
        Vr::try_parse(code).ok_or_else(|| {
            Error::Data(DataError::Other {
                message: format!("unknown VR code {:02X} {:02X}", code[0], code[1]),
            })
        })
    }

    /// Parses a two-letter string such as `"PN"`.
    pub fn parse_str(s: &str) -> Result<Vr, Error> {
        let b = s.as_bytes();
        if b.len() == 2 {
            Vr::parse(&[b[0], b[1]])
        } else {
            Err(Error::Data(DataError::Other {
                message: format!("invalid VR \"{s}\": expected two letters"),
            }))
        }
    }
}

impl std::fmt::Display for Vr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// Formats an `f64` as a DICOM DS value: at most 16 characters, no lower-case
/// exponent. `format!("{value}")` can print `0.30000000000000004` (19 chars) or
/// drop tiny values to "0", which produces an invalid DS (PR-11).
///
/// Lives in `model` (always compiled) so both the render pipeline and the print
/// models can share it without making `print` depend on the `render` feature
/// (PR-C-07).
pub(crate) fn format_ds(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    let mut fixed = format!("{value:.6}");
    if fixed.contains('.') {
        while fixed.ends_with('0') {
            fixed.pop();
        }
        if fixed.ends_with('.') {
            fixed.pop();
        }
    }
    // A fixed form that collapsed a tiny non-zero value to "0" is not usable.
    let collapsed = value != 0.0
        && fixed
            .parse::<f64>()
            .map(|parsed| parsed == 0.0)
            .unwrap_or(false);
    if !collapsed && fixed.len() <= 16 {
        return fixed;
    }
    // Too long, or the fixed form collapsed a tiny non-zero value to "0".
    for precision in (0..=6).rev() {
        let exponent = format!("{:.prec$E}", value, prec = precision);
        if exponent.len() <= 16 {
            return exponent;
        }
    }
    "0".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The authoritative attribute table from docs/01-architecture.md §2, asserted as one
    /// golden matrix over every standard VR: (code, string, charset, len16,
    /// pad, max_len, unit, swap).
    #[test]
    fn golden_attribute_table() {
        /// (Vr, code, is_string, charset_sensitive, is_16bit_len, pad, max_len, unit, swap)
        type Row = (Vr, &'static str, bool, bool, bool, u8, u32, usize, usize);
        //        VR    code  str  charset 16bit pad    max   unit swap
        let table: &[Row] = &[
            (Vr::Ae, "AE", true, false, true, b' ', 16, 1, 1),
            (Vr::As, "AS", true, false, true, b' ', 4, 1, 1),
            (Vr::At, "AT", false, false, true, 0x00, 4, 4, 2),
            (Vr::Cs, "CS", true, false, true, b' ', 16, 1, 1),
            (Vr::Da, "DA", true, false, true, b' ', 18, 1, 1),
            (Vr::Ds, "DS", true, false, true, b' ', 16, 1, 1),
            (Vr::Dt, "DT", true, false, true, b' ', 54, 1, 1),
            (Vr::Fd, "FD", false, false, true, 0x00, 8, 8, 8),
            (Vr::Fl, "FL", false, false, true, 0x00, 4, 4, 4),
            (Vr::Is, "IS", true, false, true, b' ', 12, 1, 1),
            (Vr::Lo, "LO", true, true, true, b' ', 64, 1, 1),
            (Vr::Lt, "LT", true, true, true, b' ', 10240, 1, 1),
            (Vr::Ob, "OB", false, false, false, 0x00, 0, 1, 1),
            (Vr::Od, "OD", false, false, false, 0x00, 0, 8, 8),
            (Vr::Of, "OF", false, false, false, 0x00, 0, 4, 4),
            (Vr::Ol, "OL", false, false, false, 0x00, 0, 4, 4),
            (Vr::Ov, "OV", false, false, false, 0x00, 0, 8, 8),
            (Vr::Ow, "OW", false, false, false, 0x00, 0, 2, 2),
            (Vr::Pn, "PN", true, true, true, b' ', 64, 1, 1),
            (Vr::Sh, "SH", true, true, true, b' ', 16, 1, 1),
            (Vr::Sl, "SL", false, false, true, 0x00, 4, 4, 4),
            (Vr::Sq, "SQ", false, false, false, b' ', 0, 0, 0),
            (Vr::Ss, "SS", false, false, true, 0x00, 2, 2, 2),
            (Vr::St, "ST", true, true, true, b' ', 1024, 1, 1),
            (Vr::Sv, "SV", false, false, false, 0x00, 8, 8, 8),
            (Vr::Tm, "TM", true, false, true, b' ', 16, 1, 1),
            (Vr::Uc, "UC", true, true, false, b' ', 0, 1, 1),
            (Vr::Ui, "UI", true, false, true, 0x00, 64, 1, 1),
            (Vr::Ul, "UL", false, false, true, 0x00, 4, 4, 4),
            (Vr::Un, "UN", false, false, false, 0x00, 0, 1, 1),
            (Vr::Ur, "UR", true, true, false, b' ', 0, 1, 1),
            (Vr::Us, "US", false, false, true, 0x00, 2, 2, 2),
            (Vr::Ut, "UT", true, true, false, b' ', 0, 1, 1),
            (Vr::Uv, "UV", false, false, false, 0x00, 8, 8, 8),
        ];
        assert_eq!(table.len(), 34, "all standard VRs covered");
        for (vr, code, is_str, charset, len16, pad, max, unit, swap) in table {
            assert_eq!(vr.code(), *code);
            assert_eq!(vr.is_string(), *is_str, "{code} is_string");
            assert_eq!(vr.is_character_encoded(), *charset, "{code} charset");
            assert_eq!(vr.is_16bit_length(), *len16, "{code} len16");
            assert_eq!(vr.padding(), *pad, "{code} pad");
            assert_eq!(vr.max_value_length(), *max, "{code} max");
            assert_eq!(vr.unit_size(), *unit, "{code} unit");
            assert_eq!(vr.byte_swap_size(), *swap, "{code} swap");
        }
    }

    #[test]
    fn multi_value_classification() {
        let multi = [
            Vr::Ae,
            Vr::As,
            Vr::Cs,
            Vr::Da,
            Vr::Ds,
            Vr::Dt,
            Vr::Is,
            Vr::Lo,
            Vr::Pn,
            Vr::Sh,
            Vr::Tm,
            Vr::Ui,
            Vr::Uc,
        ];
        for vr in multi {
            assert!(vr.is_multi_value(), "{}", vr);
        }
        let single = [Vr::Lt, Vr::St, Vr::Ur, Vr::Ut];
        for vr in single {
            assert!(!vr.is_multi_value(), "{}", vr);
        }
        // binary VRs are multi-valued too (fo-dicom DicomVR.IsMultiValue); only
        // SQ (and the internal item markers) is not
        for vr in [Vr::Ob, Vr::Ow, Vr::Us, Vr::Ss, Vr::At, Vr::Sv, Vr::Uv] {
            assert!(vr.is_multi_value(), "{vr}");
        }
        assert!(!Vr::Sq.is_multi_value());
    }

    #[test]
    fn parse_all_codes_round_trip() {
        for vr in [
            Vr::Ae,
            Vr::As,
            Vr::At,
            Vr::Cs,
            Vr::Da,
            Vr::Ds,
            Vr::Dt,
            Vr::Fd,
            Vr::Fl,
            Vr::Is,
            Vr::Lo,
            Vr::Lt,
            Vr::Ob,
            Vr::Od,
            Vr::Of,
            Vr::Ol,
            Vr::Ov,
            Vr::Ow,
            Vr::Pn,
            Vr::Sh,
            Vr::Sl,
            Vr::Sq,
            Vr::Ss,
            Vr::St,
            Vr::Sv,
            Vr::Tm,
            Vr::Uc,
            Vr::Ui,
            Vr::Ul,
            Vr::Un,
            Vr::Ur,
            Vr::Us,
            Vr::Ut,
            Vr::Uv,
        ] {
            let code = vr.code();
            assert_eq!(Vr::parse_str(code).unwrap(), vr, "{}", code);
            let bytes: [u8; 2] = code.as_bytes().try_into().unwrap();
            assert_eq!(Vr::parse(&bytes).unwrap(), vr, "{}", code);
        }
    }

    #[test]
    fn parse_rejects_unknown_and_internal() {
        assert!(Vr::try_parse(b"ZZ").is_none());
        assert!(
            Vr::try_parse(b"OX").is_none(),
            "OX handled by reader logic, not parse"
        );
        assert!(Vr::try_parse(b"AO").is_none());
        assert!(Vr::parse_str("PNX").is_err());
        assert!(Vr::parse_str("").is_err());
        assert!(
            Vr::parse(b"NO").is_err(),
            "internal VR NONE is not parseable"
        );
        assert_eq!(
            Vr::parse(b"??").unwrap_err().to_string(),
            "unknown VR code 3F 3F"
        );
    }

    #[test]
    fn internal_vrs() {
        for vr in [Vr::Item, Vr::ItemDelim, Vr::SeqDelim, Vr::Implicit] {
            assert_eq!(vr.code(), "NONE");
            assert_eq!(vr.unit_size(), 0);
            assert!(!vr.is_string());
        }
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    #[test]
    fn format_ds_non_finite_and_tiny_values() {
        assert_eq!(format_ds(f64::NAN), "0");
        assert_eq!(format_ds(f64::INFINITY), "0");
        // a tiny non-zero value collapses in fixed notation; scientific is used
        let tiny = format_ds(1e-10);
        assert!(tiny.len() <= 16, "DS must fit 16 chars: {tiny}");
        assert!(
            tiny.contains('E') || tiny.contains('e'),
            "scientific: {tiny}"
        );
    }
}
