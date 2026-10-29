//! Film layout rendering (PRINT-02; docs/01-architecture.md §12).
//!
//! `FilmBox::render_to_image`: fill a background with the EmptyImageDensity,
//! draw the Trim border, render each ImageBox's first image through the render
//! pipeline (docs/01-architecture.md §10) and scale it into its layout cell;
//! Polarity REVERSE inverts the cell. The film box's Presentation LUT is applied
//! next: a `(2050,0010)` sequence maps output bytes through its table, otherwise
//! `IDENTITY` is a no-op and `INVERSE` inverts the cell (`LIN OD` is validated
//! by the model but not applied here). Built on the REND-07 `RawImage` backend,
//! so it is the cross-platform replacement for fo-dicom's GDI+ print extensions.

use super::layout::LayoutCell;
use super::models::FilmBox;
use super::presentation_lut::{PresentationLutShape, PresentationLutTable};
use crate::error::{DataError, Error};
use crate::pixel::Color32;
use crate::render::{bilinear_bgra32, render_frame, RawImage};

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

/// Empty-image density (2010,0110): BLACK, WHITE or an optical-density string.
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
            // Trim box border ≈ 2 mm (fo-dicom ImageBox BORDER = 100*2/25.4 in
            // 1/100 inch), scaled to the rasterisation, so it is a real band and
            // not a 1-px hairline.
            let border_px = ((2.0 * scale.pixels_per_mm).round() as i64).max(1);
            canvas.draw_border(Color32::new(0, 0, 0), border_px);
        }

        // Parse/validate once: a malformed shape or truncated LUT fails the render.
        let presentation_lut = self.presentation_lut()?;
        let cells = self.layout(margins);
        let boxes = self.image_boxes();
        for (index, cell) in cells.iter().enumerate() {
            // PS3.3 C.11.1: ImageBoxPosition selects the cell (1-based). Fall back
            // to list order when a box omits the attribute.
            let position = (index + 1) as u16;
            let image_box = match boxes.iter().find(|b| b.position() == Some(position)) {
                Some(b) => b,
                None => match boxes.get(index) {
                    Some(b) => b,
                    None => continue,
                },
            };
            let Some(source) = image_box.images().first() else {
                continue;
            };
            let mut patch = match render_frame(source, 0) {
                Ok(image) => image,
                Err(error) => {
                    // an unrenderable cell stays background, but silently: a
                    // blank film is indistinguishable from an empty cell
                    tracing::warn!(cell = ?cell, %error, "film cell left empty");
                    continue;
                }
            };
            if let Some(table) = presentation_lut.table() {
                apply_lut(&mut patch, table);
            } else if presentation_lut.shape() == Some(PresentationLutShape::Inverse) {
                invert(&mut patch);
            }
            if image_box.polarity() {
                invert(&mut patch);
            }
            canvas.blit_scaled(cell, &patch, scale.pixels_per_mm, image_box.magnification_type().as_deref());
        }

        Ok(canvas.into_raw_image())
    }
}

/// Maps each pixel's grey channel through the presentation LUT and rebuilds the
/// BGRA patch (alpha preserved).
fn apply_lut(image: &mut RawImage, table: &PresentationLutTable) {
    let bytes = image.as_bytes();
    let mapped: Vec<u8> = bytes
        .as_chunks::<4>().0.iter()
        .flat_map(|px| {
            let out = table.map_byte(px[2] as i32);
            [out, out, out, px[3]]
        })
        .collect();
    *image = RawImage::new(image.width(), image.height(), mapped);
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

    fn draw_border(&mut self, color: Color32, thickness: i64) {
        let (w, h) = (self.width as i64, self.height as i64);
        for t in 0..thickness {
            let (x0, y0, x1, y1) = (t, t, w - 1 - t, h - 1 - t);
            if x1 < x0 || y1 < y0 {
                break;
            }
            for x in x0..=x1 {
                self.set(x, y0, color);
                self.set(x, y1, color);
            }
            for y in y0..=y1 {
                self.set(x0, y, color);
                self.set(x1, y, color);
            }
        }
    }

    fn blit_scaled(&mut self, cell: &LayoutCell, patch: &RawImage, px_per_mm: f64, magnification: Option<&str>) {
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

        // Resample the BGRA patch directly into one full-image buffer, then copy
        // its rows into the canvas — no RGB round trip and no per-pixel set (§3.7).
        let rescaled = if magnification.is_some_and(|value| value.eq_ignore_ascii_case("REPLICATE")) {
            nearest_bgra32(
                patch.as_bytes(),
                patch.width() as usize,
                patch.height() as usize,
                dst_w as usize,
                dst_h as usize,
            )
        } else {
            bilinear_bgra32(
                patch.as_bytes(),
                patch.width() as usize,
                patch.height() as usize,
                dst_w as usize,
                dst_h as usize,
            )
        };

        let off_x = cell_x + (cell_w - dst_w) / 2;
        let off_y = cell_y + (cell_h - dst_h) / 2;
        self.blit_bgra(off_x, off_y, dst_w, dst_h, &rescaled);
    }

    /// Copies a `dst_w`×`dst_h` BGRA patch at (`off_x`,`off_y`), clipping to the
    /// canvas exactly like the per-pixel [`Canvas::set`] loop did.
    fn blit_bgra(&mut self, off_x: i64, off_y: i64, dst_w: i64, dst_h: i64, src: &[u8]) {
        let x0 = (-off_x).max(0).min(dst_w);
        let x1 = (self.width as i64 - off_x).min(dst_w);
        let y0 = (-off_y).max(0).min(dst_h);
        let y1 = (self.height as i64 - off_y).min(dst_h);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let run = ((x1 - x0) * 4) as usize;
        for dy in y0..y1 {
            let src_start = ((dy * dst_w + x0) * 4) as usize;
            let dst_start = (((off_y + dy) * self.width as i64 + (off_x + x0)) * 4) as usize;
            self.bytes[dst_start..dst_start + run].copy_from_slice(&src[src_start..src_start + run]);
        }
    }

    fn into_raw_image(self) -> RawImage {
        RawImage::new(self.width, self.height, self.bytes)
    }
}

/// Nearest-neighbour BGRA resampling used by the DICOM `REPLICATE` mode.
fn nearest_bgra32(input: &[u8], input_width: usize, input_height: usize, output_width: usize, output_height: usize) -> Vec<u8> {
    let mut output = vec![0u8; output_width * output_height * 4];
    let x_ratio = if output_width <= 1 {
        0.0
    } else {
        (input_width.saturating_sub(1)) as f64 / (output_width - 1) as f64
    };
    let y_ratio = if output_height <= 1 {
        0.0
    } else {
        (input_height.saturating_sub(1)) as f64 / (output_height - 1) as f64
    };
    for y in 0..output_height {
        let source_y = (y as f64 * y_ratio).round() as usize;
        for x in 0..output_width {
            let source_x = (x as f64 * x_ratio).round() as usize;
            let source = (source_y * input_width + source_x) * 4;
            let target = (y * output_width + x) * 4;
            output[target..target + 4].copy_from_slice(&input[source..source + 4]);
        }
    }
    output
}

fn invert(image: &mut RawImage) {
    // invert by rebuilding through the public API (RawImage is read-only)
    let bytes = image.as_bytes();
    let inverted: Vec<u8> = bytes
        .as_chunks::<4>().0.iter()
        .flat_map(|px| [255 - px[0], 255 - px[1], 255 - px[2], px[3]])
        .collect();
    *image = RawImage::new(image.width(), image.height(), inverted);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dataset::Dataset;
    use crate::model::tag::Tag;
    use crate::model::vr::Vr;

    fn grayscale_image(fill: u8) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::SAMPLES_PER_PIXEL, 1u16);
        ds.insert_str(Tag::PHOTOMETRIC_INTERPRETATION, Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::ROWS, 4u16);
        ds.insert_value(Tag::COLUMNS, 4u16);
        ds.insert_value(Tag::BITS_ALLOCATED, 8u16);
        ds.insert_value(Tag::BITS_STORED, 8u16);
        ds.insert_value(Tag::HIGH_BIT, 7u16);
        ds.insert_value(Tag::PIXEL_REPRESENTATION, 0u16);
        ds.insert_str(Tag::WINDOW_CENTER, Vr::Ds, "128");
        ds.insert_str(Tag::WINDOW_WIDTH, Vr::Ds, "256");
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::PIXEL_DATA,
                Vr::Ob,
                std::sync::Arc::new(crate::model::buffer::MemoryBuffer::from(vec![fill; 16])),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    /// Builds a FilmBox dataset directly with one image box holding `fill`.
    ///
    /// The tags are the PS3.6 numbers rather than `print::models::tags`, so a
    /// wrong constant in the model cannot make this test agree with itself.
    fn film_with_one_box(polarity: &str, fill: u8) -> FilmBox {
        let image_display_format = Tag::new(0x2010, 0x0010);
        let film_size_id = Tag::new(0x2010, 0x0050);
        let referenced_image_box_sequence = Tag::new(0x2010, 0x0510);
        let image_box_position = Tag::new(0x2020, 0x0010);
        let polarity_tag = Tag::new(0x2020, 0x0020);

        let mut boxed = Dataset::new();
        boxed.insert_value(image_box_position, 1u16);
        if !polarity.is_empty() {
            boxed.insert_str(polarity_tag, Vr::Cs, polarity);
        }
        boxed.insert_sequence(Tag::BASIC_GRAYSCALE_IMAGE_SEQUENCE).push(grayscale_image(fill));

        let mut film = Dataset::new();
        film.insert_str(image_display_format, Vr::Cs, "STANDARD\\1,1");
        film.insert_str(film_size_id, Vr::Cs, "A4");
        film.insert_sequence(referenced_image_box_sequence).push(boxed);
        FilmBox::from_dataset(film)
    }

    /// The grey channel of pixel (x, y) in a rendered BGRA image.
    fn grey_at(image: &RawImage, x: u32, y: u32) -> u8 {
        image.as_bytes()[((y * image.width() + x) * 4) as usize]
    }

    #[test]
    fn rendering_produces_a_canvas_of_the_film_size() {
        let film = film_with_one_box("", 128);
        let image = film.render_to_image(10.0, RenderScale { pixels_per_mm: 1.0 }, EmptyDensity::Black).unwrap();
        assert_eq!(image.width(), 210); // A4 width mm at 1 px/mm
        assert_eq!(image.height(), 297);
        // the 4x4 grey-128 patch is scaled into the central cell: the canvas
        // centre must carry the source grey, not the black background.
        assert_eq!(grey_at(&image, 105, 148), 128, "the cell content is actually blitted");
    }

    #[test]
    fn the_background_uses_the_empty_density() {
        let film = film_with_one_box("", 128);
        let white = film.render_to_image(10.0, RenderScale { pixels_per_mm: 1.0 }, EmptyDensity::White).unwrap();
        // every corner is the white background
        for (x, y) in [(0, 0), (209, 0), (0, 296), (209, 296)] {
            assert_eq!(grey_at(&white, x, y), 255, "corner ({x},{y}) is white background");
        }
        // and the drawn cell differs from the background, so the test cannot
        // pass with background-only output.
        assert_eq!(grey_at(&white, 105, 148), 128, "the cell is drawn over the background");
    }

    #[test]
    fn trim_draws_a_border() {
        let mut film = film_with_one_box("", 128);
        film.set_trim(true);
        // 2 px/mm → a 2 mm trim border is 4 px wide (fo-dicom BORDER 100*2/25.4).
        let image = film.render_to_image(10.0, RenderScale { pixels_per_mm: 2.0 }, EmptyDensity::White).unwrap();
        for i in 0..4 {
            assert_eq!(grey_at(&image, 100, i), 0, "top border is 4 px thick (row {i})");
            assert_eq!(grey_at(&image, i, 100), 0, "left border is 4 px thick (column {i})");
        }
        assert_eq!(grey_at(&image, 100, 4), 255, "past the 4 px border is background (not a 1 px hairline)");
        assert_eq!(grey_at(&image, 140, 148), 128, "the cell is drawn inside the border");
    }

    /// Builds a FilmBox with two boxes in reverse list order (position 2 then 1),
    /// position 2 black and position 1 white.
    fn film_with_two_boxes_reversed() -> FilmBox {
        let image_display_format = Tag::new(0x2010, 0x0010);
        let film_size_id = Tag::new(0x2010, 0x0050);
        let referenced_image_box_sequence = Tag::new(0x2010, 0x0510);
        let image_box_position = Tag::new(0x2020, 0x0010);
        let build = |position: u16, fill: u8| {
            let mut boxed = Dataset::new();
            boxed.insert_value(image_box_position.clone(), position);
            boxed.insert_sequence(Tag::BASIC_GRAYSCALE_IMAGE_SEQUENCE).push(grayscale_image(fill));
            boxed
        };
        let mut film = Dataset::new();
        film.insert_str(image_display_format, Vr::Cs, "STANDARD\\1,2");
        film.insert_str(film_size_id, Vr::Cs, "A4");
        let seq = film.insert_sequence(referenced_image_box_sequence);
        seq.push(build(2, 0));
        seq.push(build(1, 255));
        FilmBox::from_dataset(film)
    }

    #[test]
    fn image_box_position_selects_the_cell() {
        let film = film_with_two_boxes_reversed();
        let cells = film.layout(10.0);
        let image = film.render_to_image(10.0, RenderScale { pixels_per_mm: 2.0 }, EmptyDensity::Black).unwrap();
        let centre = |cell: &LayoutCell| {
            let cx = ((cell.x + cell.width / 2.0) * 2.0) as u32;
            let cy = ((cell.y + cell.height / 2.0) * 2.0) as u32;
            grey_at(&image, cx, cy)
        };
        assert_eq!(centre(&cells[0]), 255, "position 1 (white) lands in the first cell, despite list order");
        assert_eq!(centre(&cells[1]), 0, "position 2 (black) lands in the second cell");
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

    fn centre_grey(image: &RawImage) -> u8 {
        let cx = image.width() / 2;
        let cy = image.height() / 2;
        image.as_bytes()[((cy * image.width() + cx) * 4) as usize]
    }

    #[test]
    fn presentation_lut_identity_and_inverse_render_differently() {
        let mut identity = film_with_one_box("", 64);
        identity.set_presentation_lut_shape(PresentationLutShape::Identity);
        let lit = identity
            .render_to_image(10.0, RenderScale { pixels_per_mm: 2.0 }, EmptyDensity::Black)
            .unwrap();

        let mut inverse = film_with_one_box("", 64);
        inverse.set_presentation_lut_shape(PresentationLutShape::Inverse);
        let inverted = inverse
            .render_to_image(10.0, RenderScale { pixels_per_mm: 2.0 }, EmptyDensity::Black)
            .unwrap();

        let (a, b) = (centre_grey(&lit), centre_grey(&inverted));
        assert!(b > a + 100, "INVERSE ({b}) must be much brighter than IDENTITY ({a}) for a dark cell");
    }

    #[test]
    fn rendering_without_a_film_size_errors() {
        let mut film = FilmBox::new();
        film.set_image_display_format("STANDARD\\1,1").unwrap();
        assert!(film.render_to_image(10.0, RenderScale::default(), EmptyDensity::Black).is_err());
    }

    #[test]
    fn blit_scaled_matches_the_previous_rgb_round_trip_bytes() {
        // §3.7: the new direct-BGRA blit must produce the same canvas bytes as the
        // old to_rgb24 → bilinear_color24 → per-pixel set path (incl. clipping).
        let patch = RawImage::new(
            3,
            2,
            vec![
                10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, //
                100, 110, 120, 255, 130, 140, 150, 255, 160, 170, 180, 255,
            ],
        );
        // a cell placed partly off the top-left of the canvas to exercise clipping
        let cell = LayoutCell { x: -0.5, y: -0.5, width: 5.0, height: 4.0 };
        let (cw, ch, ppm) = (30u32, 25u32, 3.0f64);
        let bg = crate::pixel::Color32::new(5, 6, 7);
        let mut actual = Canvas::filled(cw, ch, bg);
        actual.blit_scaled(&cell, &patch, ppm, None);

        // reference implementation: the previous RGB round-trip + per-pixel set
        let mut expected = Canvas::filled(cw, ch, bg);
        let bytes = patch.as_bytes();
        let mut rgb = Vec::new();
        for px in bytes.as_chunks::<4>().0 {
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
        let cell_x = (cell.x * ppm).round() as i64;
        let cell_y = (cell.y * ppm).round() as i64;
        let cell_w = (cell.width * ppm).round() as i64;
        let cell_h = (cell.height * ppm).round() as i64;
        let fit = (cell_w as f64 / patch.width() as f64).min(cell_h as f64 / patch.height() as f64);
        let dst_w = ((patch.width() as f64 * fit).round() as i64).max(1);
        let dst_h = ((patch.height() as f64 * fit).round() as i64).max(1);
        let rescaled = crate::render::bilinear_color24(&rgb, patch.width() as usize, patch.height() as usize, dst_w as usize, dst_h as usize);
        let off_x = cell_x + (cell_w - dst_w) / 2;
        let off_y = cell_y + (cell_h - dst_h) / 2;
        for dy in 0..dst_h {
            for dx in 0..dst_w {
                let si = ((dy * dst_w + dx) * 3) as usize;
                expected.set(off_x + dx, off_y + dy, crate::pixel::Color32::new(rescaled[si], rescaled[si + 1], rescaled[si + 2]));
            }
        }

        assert_eq!(actual.bytes, expected.bytes, "direct BGRA blit matches the RGB round trip byte for byte");
    }

    #[test]
    fn replicate_magnification_uses_nearest_neighbour() {
        let patch = RawImage::new(
            2,
            1,
            vec![
                0, 0, 0, 255, // black
                255, 255, 255, 255, // white
            ],
        );
        let cell = LayoutCell { x: 0.0, y: 0.0, width: 4.0, height: 2.0 };
        let mut canvas = Canvas::filled(4, 2, crate::pixel::Color32::new(0, 0, 0));
        canvas.blit_scaled(&cell, &patch, 1.0, Some("REPLICATE"));

        let row = &canvas.bytes[..16];
        assert_eq!(&row[0..4], &[0, 0, 0, 255]);
        assert_eq!(&row[4..8], &[0, 0, 0, 255]);
        assert_eq!(&row[8..12], &[255, 255, 255, 255]);
        assert_eq!(&row[12..16], &[255, 255, 255, 255]);
    }
}
