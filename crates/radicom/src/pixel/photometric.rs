//! Photometric interpretation values (PIX-01; docs/06-pixel-codec.md §1).
//!
//! Only the well-known values are typed; anything else round-trips as the
//! raw string so unknown vendor values never fail a read.

use crate::model::vr::Vr;

/// How the samples of a pixel map to colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhotometricInterpretation {
    Monochrome1,
    Monochrome2,
    PaletteColor,
    Rgb,
    YbrFull,
    YbrFull422,
    YbrPartial422,
    YbrPartial420,
    YbrIct,
    YbrRct,
    Hsv,
    Argb,
    Cmyk,
    Other,
}

impl PhotometricInterpretation {
    /// Parses the `(0028,0004)` value, trimming padding.
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "MONOCHROME1" => Self::Monochrome1,
            "MONOCHROME2" => Self::Monochrome2,
            "PALETTE COLOR" => Self::PaletteColor,
            "RGB" => Self::Rgb,
            "YBR_FULL" => Self::YbrFull,
            "YBR_FULL_422" => Self::YbrFull422,
            "YBR_PARTIAL_422" => Self::YbrPartial422,
            "YBR_PARTIAL_420" => Self::YbrPartial420,
            "YBR_ICT" => Self::YbrIct,
            "YBR_RCT" => Self::YbrRct,
            "HSV" => Self::Hsv,
            "ARGB" => Self::Argb,
            "CMYK" => Self::Cmyk,
            _ => Self::Other,
        }
    }

    /// The wire form; `Other` is not encodable, callers keep the original
    /// text instead.
    pub fn as_str(self) -> Option<&'static str> {
        Some(match self {
            Self::Monochrome1 => "MONOCHROME1",
            Self::Monochrome2 => "MONOCHROME2",
            Self::PaletteColor => "PALETTE COLOR",
            Self::Rgb => "RGB",
            Self::YbrFull => "YBR_FULL",
            Self::YbrFull422 => "YBR_FULL_422",
            Self::YbrPartial422 => "YBR_PARTIAL_422",
            Self::YbrPartial420 => "YBR_PARTIAL_420",
            Self::YbrIct => "YBR_ICT",
            Self::YbrRct => "YBR_RCT",
            Self::Hsv => "HSV",
            Self::Argb => "ARGB",
            Self::Cmyk => "CMYK",
            Self::Other => return None,
        })
    }

    /// VR of `(0028,0004)`.
    pub const fn vr() -> Vr {
        Vr::Cs
    }

    /// True for the YCbCr-family values that need colour conversion.
    pub const fn is_ybr(self) -> bool {
        matches!(
            self,
            Self::YbrFull
                | Self::YbrFull422
                | Self::YbrPartial422
                | Self::YbrPartial420
                | Self::YbrIct
                | Self::YbrRct
        )
    }

    /// True for the subsampled YCbCr values (PIX-03 frame-size rules).
    pub const fn is_subsampled(self) -> bool {
        matches!(self, Self::YbrFull422 | Self::YbrPartial422 | Self::YbrPartial420)
    }

    /// True for 4:2:0, where both dimensions are chroma-halved.
    pub const fn is_420(self) -> bool {
        matches!(self, Self::YbrPartial420)
    }
}

impl std::fmt::Display for PhotometricInterpretation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str().unwrap_or("OTHER"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_round_trip() {
        for value in [
            "MONOCHROME1",
            "MONOCHROME2",
            "PALETTE COLOR",
            "RGB",
            "YBR_FULL",
            "YBR_FULL_422",
            "YBR_PARTIAL_422",
            "YBR_PARTIAL_420",
            "YBR_ICT",
            "YBR_RCT",
            "HSV",
            "ARGB",
            "CMYK",
        ] {
            let pi = PhotometricInterpretation::parse(value);
            assert_eq!(pi.as_str(), Some(value), "{value} round-trips");
        }
    }

    #[test]
    fn unknown_and_padded_values() {
        assert_eq!(PhotometricInterpretation::parse("VENDOR "), PhotometricInterpretation::Other);
        assert_eq!(PhotometricInterpretation::parse("RGB "), PhotometricInterpretation::Rgb, "padding trimmed");
        assert_eq!(PhotometricInterpretation::Other.as_str(), None);
    }

    #[test]
    fn ybr_classification() {
        assert!(PhotometricInterpretation::YbrFull422.is_ybr());
        assert!(PhotometricInterpretation::YbrFull422.is_subsampled());
        assert!(PhotometricInterpretation::YbrPartial420.is_420());
        assert!(!PhotometricInterpretation::Rgb.is_ybr());
        assert!(!PhotometricInterpretation::YbrFull.is_subsampled(), "4:4:4 is not subsampled");
    }
}
