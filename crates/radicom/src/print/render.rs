//! Film layout rendering (PRINT-02; docs/08-serialization-media.md §5).
//!
//! `FilmBox::render_to_image`: fill a background with the EmptyImageDensity,
//! draw the Trim border, render each ImageBox's first image through the render
//! pipeline (docs/10-rendering.md §1) and scale it into its layout cell;
//! Polarity REVERSE inverts the cell. Built on the REND-07 `RawImage` backend, so
//! it is the cross-platform replacement for fo-dicom's GDI+ print extensions.

use super::layout::LayoutCell;
use super::models::FilmBox;
use crate::error::{DataError, Error};
use crate::pixel::Color32;
use crate::render::{bilinear_color24, render_frame, RawImage};

/// How dense to rasterise the film: pixels per millimetre on the printed page.
#[derive(Debug, Clone, Copy)]
pub struct RenderScale {
    pub pixels_per_mm: f64,
}

impl Default for RenderScale {
    fn default() -> Self {
        // a print-friendly ~150 dpi (≈5.9 px/mm)
        RenderScale { pixels_per_mm: 150.0 / 25.4 }
    }
}

/// Empty-image density (2010,0120): BLACK, WHITE or an optical-density string.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EmptyDensity {
    Black,
    White,
    /// An optical-density value in `[0, 2]`; 0 = white, ≥2 = black.
    Optical(f64),
}

impl EmptyDensity {
    /// The background grey level this density maps to.
    pub fn background(self) -> Color32 {
        let level = match self {
            EmptyDensity::Black => 0u8,
            EmptyDensity::White => 255,
            EmptyDensity::Optical(od) => (255.0 * (1.0 - od.clamp(0.0, 2.0) / 2.0)) as u8,
        };
        Color32::new(level, level, level)
    }
}

impl FilmBox {
    /// Renders the film to an 8-bit [`RawImage`] (PRINT-02).
    pub fn render_to_image(
        &self,
        margins: f64,
        scale: RenderScale,
        empty_density: EmptyDensity,
    ) -> Result<RawImage, Error> {
        let film = self
            .film_size()
            .ok_or_else(|| Error::Data(DataError::Other { message: "no FilmSizeID to render".into() }))?;

        let canvas_w = ((film.width_mm * scale.pixels_per_mm).round() as u32).max(1);
        let canvas_h = ((film.height_mm * scale.pixels_per_mm).round() as u32).max(1);
        let mut canvas = Canvas::filled(canvas_w, canvas_h, empty_density.background());

        if self.trim() {
            canvas.draw_border(Color32::new(0, 0, 0));
        }

        let cells = self.layout(margins);
        let boxes = self.image_boxes();
        for (cell, image_box) in cells.iter().zip(boxes.iter()) {
            let Some(source) = image_box.images().first() else {
                continue;
            };
            let mut patch = match render_frame(source, 0) {
                Ok(image) => image,
                Err(_) => continue, // an unrenderable cell stays background
            };
            if image_box.polarity() {
                invert(&mut patch);
            }
            canvas.blit_scaled(cell, &patch, scale.pixels_per_mm);
        }

        Ok(canvas.into_raw_image())
    }
}

/// A BGRA32 canvas under construction.
struct Canvas {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}

impl Canvas {
    fn filled(width: u32, height: u32, color: Color32) -> Self {
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            bytes.extend_from_slice(&[color.b, color.g, color.r, 0xFF]);
        }
        Canvas { width, height, bytes }
    }

    fn set(&mut self, x: i64, y: i64, color: Color32) {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            return;
        }
        let i = ((y as u32 * self.width + x as u32) * 4) as usize;
        self.bytes[i] = color.b;
        self.bytes[i + 1] = color.g;
        self.bytes[i + 2] = color.r;
        self.bytes[i + 3] = 0xFF;
    }

    fn draw_border(&mut self, color: Color32) {
        let (w, h) = (self.width as i64, self.height as i64);
        for x in 0..w {
            self.set(x, 0, color);
            self.set(x, h - 1, color);
        }
        for y in 0..h {
            self.set(0, y, color);
            self.set(w - 1, y, color);
        }
    }

    fn blit_scaled(&mut self, cell: &LayoutCell, patch: &RawImage, px_per_mm: f64) {
        let cell_x = (cell.x * px_per_mm).round() as i64;
        let cell_y = (cell.y * px_per_mm).round() as i64;
        let cell_w = (cell.width * px_per_mm).round() as i64;
        let cell_h = (cell.height * px_per_mm).round() as i64;
        if cell_w <= 0 || cell_h <= 0 || patch.width() == 0 || patch.height() == 0 {
            return;
        }
        // fit preserving aspect ratio
        let scale = (cell_w as f64 / patch.width() as f64).min(cell_h as f64 / patch.height() as f64);
        let dst_w = ((patch.width() as f64 * scale).round() as i64).max(1);
        let dst_h = ((patch.height() as f64 * scale).round() as i64).max(1);

        let rgb = to_rgb24(patch);
        let rescaled = bilinear_color24(&rgb, patch.width() as usize, patch.height() as usize, dst_w as usize, dst_h as usize);

        let off_x = cell_x + (cell_w - dst_w) / 2;
        let off_y = cell_y + (cell_h - dst_h) / 2;
        for dy in 0..dst_h {
            for dx in 0..dst_w {
                let si = ((dy * dst_w + dx) * 3) as usize;
                let color = Color32::new(rescaled[si], rescaled[si + 1], rescaled[si + 2]);
                self.set(off_x + dx, off_y + dy, color);
            }
        }
    }

    fn into_raw_image(self) -> RawImage {
        RawImage::new(self.width, self.height, self.bytes)
    }
}

fn invert(image: &mut RawImage) {
    // invert by rebuilding through the public API (RawImage is read-only)
    let bytes = image.as_bytes();
    let inverted: Vec<u8> = bytes
        .chunks_exact(4)
        .flat_map(|px| [255 - px[0], 255 - px[1], 255 - px[2], px[3]])
        .collect();
    *image = RawImage::new(image.width(), image.height(), inverted);
}

fn to_rgb24(image: &RawImage) -> Vec<u8> {
    let bytes = image.as_bytes();
    let mut rgb = Vec::with_capacity((image.width() * image.height() * 3) as usize);
    for px in bytes.chunks_exact(4) {
        rgb.push(px[2]);
        rgb.push(px[1]);
        rgb.push(px[0]);
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dataset::Dataset;
    use crate::model::tag::Tag;
    use crate::model::vr::Vr;
    use crate::print::models::tags;

    fn grayscale_image(fill: u8) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::new(0x0028, 0x0002), 1u16);
        ds.insert_str(Tag::new(0x0028, 0x0004), Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::new(0x0028, 0x0010), 4u16);
        ds.insert_value(Tag::new(0x0028, 0x0011), 4u16);
        ds.insert_value(Tag::new(0x0028, 0x0100), 8u16);
        ds.insert_value(Tag::new(0x0028, 0x0101), 8u16);
        ds.insert_value(Tag::new(0x0028, 0x0102), 7u16);
        ds.insert_value(Tag::new(0x0028, 0x0103), 0u16);
        ds.insert_str(Tag::new(0x0028, 0x1050), Vr::Ds, "128");
        ds.insert_str(Tag::new(0x0028, 0x1051), Vr::Ds, "256");
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::new(0x7FE0, 0x0010),
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![fill; 16])),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    /// Builds a FilmBox dataset directly with one image box holding `fill`.
    fn film_with_one_box(polarity: &str, fill: u8) -> FilmBox {
        let mut boxed = Dataset::new();
        boxed.insert_value(tags::IMAGE_POSITION, 1u16);
        if !polarity.is_empty() {
            boxed.insert_str(tags::POLARITY, Vr::Cs, polarity);
        }
        boxed.insert_sequence(Tag::new(0x2020, 0x0110)).push(grayscale_image(fill));

        let mut film = Dataset::new();
        film.insert_str(tags::IMAGE_DISPLAY_FORMAT, Vr::Cs, "STANDARD\\1,1");
        film.insert_str(tags::FILM_SIZE_ID, Vr::Cs, "A4");
        film.insert_sequence(tags::REFERENCED_IMAGE_BOX_SEQUENCE).push(boxed);
        FilmBox::from_dataset(film)
    }

    #[test]
    fn rendering_produces_a_canvas_of_the_film_size() {
        let film = film_with_one_box("", 128);
        let image = film.render_to_image(10.0, RenderScale { pixels_per_mm: 1.0 }, EmptyDensity::Black).unwrap();
        assert_eq!(image.width(), 210); // A4 width mm at 1 px/mm
        assert_eq!(image.height(), 297);
    }

    #[test]
    fn the_background_uses_the_empty_density() {
        let film = film_with_one_box("", 128);
        let white = film.render_to_image(10.0, RenderScale { pixels_per_mm: 1.0 }, EmptyDensity::White).unwrap();
        assert_eq!(&white.as_bytes()[0..3], &[255, 255, 255], "corner is white background");
    }

    #[test]
    fn trim_draws_a_border() {
        let mut film = film_with_one_box("", 128);
        film.set_trim(true);
        let image = film.render_to_image(10.0, RenderScale { pixels_per_mm: 1.0 }, EmptyDensity::White).unwrap();
        assert_eq!(&image.as_bytes()[0..3], &[0, 0, 0], "trim border is drawn over the background");
    }

    #[test]
    fn polarity_reverse_darkens_a_white_cell() {
        // white source, REVERSE polarity → the drawn cell is dark
        let film = film_with_one_box("REVERSE", 255);
        let image = film.render_to_image(10.0, RenderScale { pixels_per_mm: 2.0 }, EmptyDensity::Black).unwrap();
        let cx = image.width() / 2;
        let cy = image.height() / 2;
        let i = ((cy * image.width() + cx) * 4) as usize;
        assert!(image.as_bytes()[i] < 60, "REVERSE inverts a white cell to dark, got {}", image.as_bytes()[i]);
    }

    #[test]
    fn a_normal_cell_keeps_its_brightness() {
        let film = film_with_one_box("", 255);
        let image = film.render_to_image(10.0, RenderScale { pixels_per_mm: 2.0 }, EmptyDensity::Black).unwrap();
        let cx = image.width() / 2;
        let cy = image.height() / 2;
        let i = ((cy * image.width() + cx) * 4) as usize;
        assert!(image.as_bytes()[i] > 200, "a white NORMAL cell stays bright, got {}", image.as_bytes()[i]);
    }

    #[test]
    fn rendering_without_a_film_size_errors() {
        let mut film = FilmBox::new();
        film.set_image_display_format("STANDARD\\1,1").unwrap();
        assert!(film.render_to_image(10.0, RenderScale::default(), EmptyDensity::Black).is_err());
    }
}
