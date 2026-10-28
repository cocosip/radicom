//! Print layout primitives (PRINT-01; docs/01-architecture.md §12).
//!
//! The film-layout maths that PRINT-02's `render_to_image` will consume: how an
//! `ImageDisplayFormat` turns into a grid of image cells, and how a film size
//! string maps to physical dimensions. Kept free of any rendering backend so it
//! is unit-testable on its own; the session/box DICOM datasets sit on top.

use crate::error::{DataError, Error};

/// A film's physical size in millimetres (§5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilmSize {
    pub width_mm: f64,
    pub height_mm: f64,
}

impl FilmSize {
    pub fn new(width_mm: f64, height_mm: f64) -> Self {
        FilmSize { width_mm, height_mm }
    }

    /// Parses a DICOM film size string (§5).
    ///
    /// Recognises the standard named sizes (`8INX10IN`, `A4`, `A3`, ...) and the
    /// `NNCMXNNCM` form. Returns `None` for an unknown string so the caller can
    /// fall back to a default rather than mis-lay out the film.
    pub fn parse(text: &str) -> Option<FilmSize> {
        let text = text.trim().to_ascii_uppercase();
        // named sizes in millimetres (PS3.3 C.13.3.1 table)
        let named = match text.as_str() {
            "8INX10IN" => Some((203.0, 254.0)),
            "10INX12IN" => Some((254.0, 305.0)),
            "10INX14IN" => Some((254.0, 356.0)),
            "11INX14IN" => Some((279.0, 356.0)),
            "14INX14IN" => Some((356.0, 356.0)),
            "14INX17IN" => Some((356.0, 432.0)),
            "A3" => Some((297.0, 420.0)),
            "A4" => Some((210.0, 297.0)),
            "A4S" => Some((210.0, 297.0)),
            "A5" => Some((148.0, 210.0)),
            _ => None,
        };
        if let Some((w, h)) = named {
            return Some(FilmSize::new(w, h));
        }
        // `NNCMXNNCM` form, e.g. `24CMX30CM`
        if let Some((w, h)) = text.strip_suffix("CM").and_then(|rest| rest.split_once('X')) {
            let w = w.strip_suffix("CM").unwrap_or(w);
            let w = w.parse::<f64>().ok()?;
            let h = h.parse::<f64>().ok()?;
            return Some(FilmSize::new(w * 10.0, h * 10.0));
        }
        None
    }

    /// Aspect ratio (width / height).
    pub fn aspect(&self) -> f64 {
        if self.height_mm == 0.0 {
            1.0
        } else {
            self.width_mm / self.height_mm
        }
    }
}

/// A rectangular cell in millimetres, origin at the film's top-left (§5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutCell {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// How images are arranged on the film (§5, PS3.3 C.13.3.2).
///
/// The doc names six forms; `CUSTOM` and `SLIDE`/`SUPERSLIDE` are accepted by
/// the parser but this round only lays out the three composite forms
/// (`STANDARD`, `ROW`, `COL`) because those have a closed-form grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageDisplayFormat {
    /// `STANDARD\C,R`: `columns × rows` equal grid.
    Standard { columns: u32, rows: u32 },
    /// `ROW\R1,R2,..`: `Ri` cells in the i-th row (row-major).
    Row(Vec<u32>),
    /// `COL\C1,C2,..`: `Ci` cells in the i-th column (column-major).
    Col(Vec<u32>),
    /// `SLIDE`: single image per slide.
    Slide,
    /// `SUPERSLIDE`: super-slide layout (treated as one cell per slide here).
    SuperSlide,
    /// `CUSTOM\i`: a vendor-defined layout, index `i`.
    Custom(u32),
}

impl ImageDisplayFormat {
    /// Parses a DICOM `ImageDisplayFormat` string (§5).
    pub fn parse(text: &str) -> Result<ImageDisplayFormat, Error> {
        let text = text.trim();
        let (kind, rest) = match text.split_once('\\') {
            Some((kind, rest)) => (kind.trim(), rest),
            None => (text, ""),
        };
        match kind {
            "STANDARD" => {
                let (columns, rows) = rest
                    .split_once(',')
                    .ok_or_else(|| format_error(text, "STANDARD needs C,R"))?;
                let columns = parse_count(columns, text)?;
                let rows = parse_count(rows, text)?;
                Ok(ImageDisplayFormat::Standard { columns, rows })
            }
            "ROW" => Ok(ImageDisplayFormat::Row(parse_counts(rest, text)?)),
            "COL" => Ok(ImageDisplayFormat::Col(parse_counts(rest, text)?)),
            "SLIDE" => Ok(ImageDisplayFormat::Slide),
            "SUPERSLIDE" => Ok(ImageDisplayFormat::SuperSlide),
            "CUSTOM" => {
                let index = parse_count(rest, text)?;
                Ok(ImageDisplayFormat::Custom(index))
            }
            other => Err(format_error(text, &format!("unknown display format \"{other}\""))),
        }
    }

    /// The total number of image cells this format defines.
    pub fn cell_count(&self) -> u32 {
        match self {
            ImageDisplayFormat::Standard { columns, rows } => columns * rows,
            ImageDisplayFormat::Row(counts) => counts.iter().sum(),
            ImageDisplayFormat::Col(counts) => counts.iter().sum(),
            ImageDisplayFormat::Slide | ImageDisplayFormat::SuperSlide => 1,
            ImageDisplayFormat::Custom(_) => 0,
        }
    }

    /// Computes each cell's rectangle on `film`, inset by `margin` mm on every
    /// side (§5). `STANDARD`/`ROW`/`COL` are laid out; `SLIDE`, `SUPERSLIDE`
    /// and `CUSTOM` return an empty list (no closed-form grid this round).
    pub fn layout(&self, film: FilmSize, margin: f64) -> Vec<LayoutCell> {
        let left = margin;
        let top = margin;
        let usable_w = (film.width_mm - 2.0 * margin).max(0.0);
        let usable_h = (film.height_mm - 2.0 * margin).max(0.0);

        match self {
            ImageDisplayFormat::Standard { columns, rows } => {
                // `rows` rows, each with `columns` cells
                let per_row = vec![*columns; *rows as usize];
                grid(&per_row, usable_w, usable_h, left, top)
            }
            ImageDisplayFormat::Row(counts) => {
                // `counts` are per-row column counts; rows stack vertically
                grid(counts, usable_w, usable_h, left, top)
            }
            ImageDisplayFormat::Col(counts) => {
                // `counts` are per-column row counts; lay them out column-major
                col_major(counts, usable_w, usable_h, left, top)
            }
            ImageDisplayFormat::Slide | ImageDisplayFormat::SuperSlide => {
                vec![LayoutCell { x: left, y: top, width: usable_w, height: usable_h }]
            }
            ImageDisplayFormat::Custom(_) => Vec::new(),
        }
    }
}

/// Lays out equal cells in a per-row grid.
///
/// `per_row[i]` gives the number of equal cells in row `i`.
fn grid(per_row: &[u32], usable_w: f64, usable_h: f64, left: f64, top: f64) -> Vec<LayoutCell> {
    let row_count = per_row.len();
    if row_count == 0 {
        return Vec::new();
    }
    let row_h = usable_h / row_count as f64;
    let mut cells = Vec::new();
    for (r, &column_count) in per_row.iter().enumerate() {
        if column_count == 0 {
            continue;
        }
        let col_w = usable_w / column_count as f64;
        let y = top + r as f64 * row_h;
        for c in 0..column_count {
            cells.push(LayoutCell { x: left + c as f64 * col_w, y, width: col_w, height: row_h });
        }
    }
    cells
}

/// Lays out a column-major format where column `i` holds `counts[i]` cells.
fn col_major(counts: &[u32], usable_w: f64, usable_h: f64, left: f64, top: f64) -> Vec<LayoutCell> {
    let column_count = counts.len();
    if column_count == 0 {
        return Vec::new();
    }
    let col_w = usable_w / column_count as f64;
    let mut cells = Vec::new();
    for (c, rows_in_col) in counts.iter().enumerate() {
        if *rows_in_col == 0 {
            continue;
        }
        let row_h = usable_h / *rows_in_col as f64;
        let x = left + c as f64 * col_w;
        for r in 0..*rows_in_col {
            cells.push(LayoutCell { x, y: top + r as f64 * row_h, width: col_w, height: row_h });
        }
    }
    cells
}

/// Parses a single positive count, rejecting zero and garbage.
fn parse_count(text: &str, whole: &str) -> Result<u32, Error> {
    let count = text.trim().parse::<u32>().map_err(|_| format_error(whole, "expected a number"))?;
    if count == 0 {
        return Err(format_error(whole, "counts must be positive"));
    }
    Ok(count)
}

/// Parses a comma-separated list of positive counts.
fn parse_counts(rest: &str, whole: &str) -> Result<Vec<u32>, Error> {
    let counts: Result<Vec<u32>, Error> = rest.split(',').map(|part| parse_count(part, whole)).collect();
    let counts = counts?;
    if counts.is_empty() {
        return Err(format_error(whole, "no counts given"));
    }
    Ok(counts)
}

fn format_error(text: &str, reason: &str) -> Error {
    Error::Data(DataError::Other { message: format!("bad ImageDisplayFormat \"{text}\": {reason}") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_parses_to_columns_and_rows() {
        let format = ImageDisplayFormat::parse("STANDARD\\2,3").unwrap();
        assert_eq!(format, ImageDisplayFormat::Standard { columns: 2, rows: 3 });
        assert_eq!(format.cell_count(), 6);
    }

    #[test]
    fn row_parses_per_row_counts() {
        let format = ImageDisplayFormat::parse("ROW\\2,1,2").unwrap();
        assert_eq!(format, ImageDisplayFormat::Row(vec![2, 1, 2]));
        assert_eq!(format.cell_count(), 5);
    }

    #[test]
    fn col_parses_per_column_counts() {
        let format = ImageDisplayFormat::parse("COL\\1,3,1").unwrap();
        assert_eq!(format, ImageDisplayFormat::Col(vec![1, 3, 1]));
        assert_eq!(format.cell_count(), 5);
    }

    #[test]
    fn slide_superslide_and_custom_parse() {
        assert_eq!(ImageDisplayFormat::parse("SLIDE").unwrap(), ImageDisplayFormat::Slide);
        assert_eq!(ImageDisplayFormat::parse("SUPERSLIDE").unwrap(), ImageDisplayFormat::SuperSlide);
        assert_eq!(ImageDisplayFormat::parse("CUSTOM\\3").unwrap(), ImageDisplayFormat::Custom(3));
    }

    #[test]
    fn an_unknown_format_is_rejected() {
        assert!(ImageDisplayFormat::parse("BOGUS\\1,2").is_err());
        assert!(ImageDisplayFormat::parse("STANDARD").is_err(), "STANDARD needs C,R");
        assert!(ImageDisplayFormat::parse("ROW\\").is_err(), "ROW needs counts");
        assert!(ImageDisplayFormat::parse("STANDARD\\0,2").is_err(), "zero counts are invalid");
    }

    #[test]
    fn standard_layout_is_a_full_grid() {
        let format = ImageDisplayFormat::parse("STANDARD\\2,2").unwrap();
        let film = FilmSize::new(200.0, 200.0);
        let cells = format.layout(film, 10.0);
        assert_eq!(cells.len(), 4);
        // usable area is 180x180 starting at (10,10)
        let first = cells[0];
        assert_eq!(first.x, 10.0);
        assert_eq!(first.y, 10.0);
        assert!((first.width - 90.0).abs() < 1e-9);
        assert!((first.height - 90.0).abs() < 1e-9);
        // the last cell sits in the bottom-right quadrant
        let last = cells[3];
        assert!((last.x - 100.0).abs() < 1e-9);
        assert!((last.y - 100.0).abs() < 1e-9);
    }

    #[test]
    fn row_layout_stacks_rows_with_their_own_column_counts() {
        let format = ImageDisplayFormat::parse("ROW\\2,1").unwrap();
        let film = FilmSize::new(200.0, 200.0);
        let cells = format.layout(film, 10.0);
        assert_eq!(cells.len(), 3);
        // two cells in the first row (90 wide), one full-width cell below
        let top: Vec<_> = cells.iter().filter(|c| c.y == 10.0).collect();
        assert_eq!(top.len(), 2);
        let bottom: Vec<_> = cells.iter().filter(|c| c.y == 100.0).collect();
        assert_eq!(bottom.len(), 1);
        assert!((bottom[0].width - 180.0).abs() < 1e-9, "the single cell spans the full width");
    }

    #[test]
    fn col_layout_stacks_columns_with_their_own_row_counts() {
        let format = ImageDisplayFormat::parse("COL\\1,2").unwrap();
        let film = FilmSize::new(200.0, 200.0);
        let cells = format.layout(film, 10.0);
        assert_eq!(cells.len(), 3);
        // left column (x=10) has 1 cell, right column (x=100) has 2
        let left: Vec<_> = cells.iter().filter(|c| c.x == 10.0).collect();
        assert_eq!(left.len(), 1);
        let right: Vec<_> = cells.iter().filter(|c| (c.x - 100.0).abs() < 1e-9).collect();
        assert_eq!(right.len(), 2);
    }

    #[test]
    fn slide_layout_is_one_cell() {
        let cells = ImageDisplayFormat::Slide.layout(FilmSize::new(200.0, 300.0), 5.0);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0], LayoutCell { x: 5.0, y: 5.0, width: 190.0, height: 290.0 });
    }

    #[test]
    fn film_size_parses_named_and_metric_forms() {
        assert_eq!(FilmSize::parse("A4"), Some(FilmSize::new(210.0, 297.0)));
        assert_eq!(FilmSize::parse("a4"), Some(FilmSize::new(210.0, 297.0)), "case-insensitive");
        assert_eq!(FilmSize::parse("8INX10IN"), Some(FilmSize::new(203.0, 254.0)));
        assert_eq!(FilmSize::parse("24CMX30CM"), Some(FilmSize::new(240.0, 300.0)));
        assert_eq!(FilmSize::parse("bogus"), None);
    }

    #[test]
    fn a_margin_larger_than_the_film_yields_zero_area_cells() {
        let format = ImageDisplayFormat::parse("STANDARD\\1,1").unwrap();
        let cells = format.layout(FilmSize::new(10.0, 10.0), 20.0);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].width, 0.0);
        assert_eq!(cells[0].height, 0.0);
    }
}
