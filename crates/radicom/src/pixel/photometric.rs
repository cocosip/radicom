//! Photometric interpretation values (PIX-01; docs/01-architecture.md §9).
//!
//! Only the well-known values are typed; anything else round-trips as the
//! raw string so unknown vendor values never fail a read.

use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
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

/// Colour space category (REND-06 / docs §6; fo-dicom `ColorSpace.Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorSpaceType {
    OneBit,
    Grayscale,
    Indexed,
    Rgb,
    Bgr,
    Rgba,
    YCbCr,
}

/// A colour-space descriptor: category, component count and per-component
/// `(horizontal, vertical)` chroma subsampling factors (REND-06 / docs §6;
/// fo-dicom `ColorSpace`). Components without chroma subsampling use `(1, 1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorSpace {
    pub kind: ColorSpaceType,
    pub components: u8,
    pub subsampling: [(u8, u8); 3],
}

impl ColorSpace {
    /// The `(horizontal, vertical)` chroma subsampling of the second component.
    pub fn chroma_subsampling(&self) -> (u8, u8) {
        self.subsampling[1]
    }

    /// True when any component is downsampled (4:2:2 or 4:2:0).
    pub fn is_subsampled(&self) -> bool {
        self.subsampling.iter().any(|&(h, v)| h > 1 || v > 1)
    }

    /// True for single-sample monochrome / one-bit data.
    pub fn is_grayscale(&self) -> bool {
        matches!(
            self.kind,
            ColorSpaceType::Grayscale | ColorSpaceType::OneBit
        )
    }

    /// True for palette-indexed data.
    pub fn is_palette(&self) -> bool {
        matches!(self.kind, ColorSpaceType::Indexed)
    }
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

    /// The colour-space descriptor for this PI (REND-06 / docs §6). `None` for
    /// colour spaces without a documented descriptor (HSV/ARGB/CMYK/Other).
    pub fn color_space(self) -> Option<ColorSpace> {
        use ColorSpaceType::*;
        let (kind, components, subsampling) = match self {
            Self::Monochrome1 | Self::Monochrome2 => (Grayscale, 1, [(1u8, 1u8); 3]),
            Self::PaletteColor => (Indexed, 1, [(1, 1); 3]),
            Self::Rgb => (Rgb, 3, [(1, 1); 3]),
            Self::YbrFull | Self::YbrIct | Self::YbrRct => (YCbCr, 3, [(1, 1); 3]),
            Self::YbrFull422 | Self::YbrPartial422 => (YCbCr, 3, [(1, 1), (2, 1), (2, 1)]),
            Self::YbrPartial420 => (YCbCr, 3, [(1, 1), (2, 2), (2, 2)]),
            Self::Hsv | Self::Argb | Self::Cmyk | Self::Other => return None,
        };
        Some(ColorSpace {
            kind,
            components,
            subsampling,
        })
    }

    /// True for the subsampled YCbCr values (PIX-03 frame-size rules).
    pub fn is_subsampled(self) -> bool {
        self.color_space().is_some_and(|cs| cs.is_subsampled())
    }

    /// True for 4:2:0, where both dimensions are chroma-halved.
    pub fn is_420(self) -> bool {
        self.color_space()
            .is_some_and(|cs| cs.chroma_subsampling() == (2, 2))
    }

    /// Infers the photometric interpretation for legacy (ACR-NEMA) files whose
    /// `(0028,0004)` is absent (docs §1.1 / REND-05 `PixelDataFactory`): a palette
    /// LUT (`(0028,1101/1102/1103)`) with ≤1 sample ⇒ PALETTE COLOR, other
    /// single-sample data ⇒ MONOCHROME2, and multi-sample data ⇒ RGB.
    pub fn infer_from_dataset(dataset: &Dataset) -> Self {
        let samples = dataset.try_get::<u16>(Tag::SAMPLES_PER_PIXEL).unwrap_or(1);
        if samples <= 1 {
            let palette = dataset.contains(&Tag::RED_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR)
                && dataset.contains(&Tag::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR)
                && dataset.contains(&Tag::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR);
            if palette {
                Self::PaletteColor
            } else {
                Self::Monochrome2
            }
        } else {
            Self::Rgb
        }
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
        assert_eq!(
            PhotometricInterpretation::parse("VENDOR "),
            PhotometricInterpretation::Other
        );
        assert_eq!(
            PhotometricInterpretation::parse("RGB "),
            PhotometricInterpretation::Rgb,
            "padding trimmed"
        );
        assert_eq!(PhotometricInterpretation::Other.as_str(), None);
    }

    #[test]
    fn ybr_classification() {
        assert!(PhotometricInterpretation::YbrFull422.is_ybr());
        assert!(PhotometricInterpretation::YbrFull422.is_subsampled());
        assert!(PhotometricInterpretation::YbrPartial420.is_420());
        assert!(!PhotometricInterpretation::Rgb.is_ybr());
        assert!(
            !PhotometricInterpretation::YbrFull.is_subsampled(),
            "4:4:4 is not subsampled"
        );
    }

    #[test]
    fn colour_space_descriptors() {
        let mono = PhotometricInterpretation::Monochrome2
            .color_space()
            .unwrap();
        assert_eq!(mono.kind, ColorSpaceType::Grayscale);
        assert_eq!(mono.components, 1);
        assert!(mono.is_grayscale() && !mono.is_subsampled());

        let rgb = PhotometricInterpretation::Rgb.color_space().unwrap();
        assert_eq!((rgb.kind, rgb.components), (ColorSpaceType::Rgb, 3));
        assert!(!rgb.is_subsampled());

        let pal = PhotometricInterpretation::PaletteColor
            .color_space()
            .unwrap();
        assert!(pal.is_palette());

        let y444 = PhotometricInterpretation::YbrFull.color_space().unwrap();
        assert_eq!(y444.chroma_subsampling(), (1, 1));
        assert!(!y444.is_subsampled());

        let y422 = PhotometricInterpretation::YbrFull422.color_space().unwrap();
        assert_eq!(
            (y422.kind, y422.chroma_subsampling()),
            (ColorSpaceType::YCbCr, (2, 1))
        );
        assert!(y422.is_subsampled());

        let y420 = PhotometricInterpretation::YbrPartial420
            .color_space()
            .unwrap();
        assert_eq!(y420.chroma_subsampling(), (2, 2));
        assert!(y420.is_subsampled());

        // undocumented colour spaces have no descriptor
        assert!(PhotometricInterpretation::Hsv.color_space().is_none());
    }

    #[test]
    fn infer_photometric_for_legacy_files() {
        // no PI, single sample, no palette → MONOCHROME2
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        assert_eq!(
            PhotometricInterpretation::infer_from_dataset(&ds),
            PhotometricInterpretation::Monochrome2
        );
        // single sample + all three palette descriptors present → PALETTE COLOR
        for t in [0x1101u16, 0x1102, 0x1103] {
            ds.insert_value(Tag::new(0x0028, t), 0u16);
        }
        assert_eq!(
            PhotometricInterpretation::infer_from_dataset(&ds),
            PhotometricInterpretation::PaletteColor
        );
        // multi-sample → RGB
        let mut rgb = Dataset::new();
        rgb.insert_value(Tag::SAMPLES_PER_PIXEL, 3u16);
        assert_eq!(
            PhotometricInterpretation::infer_from_dataset(&rgb),
            PhotometricInterpretation::Rgb
        );
    }
}
