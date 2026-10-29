//! Spatial 2D transform for the output stage (REND-11; docs/01-architecture.md §10).
//!
//! Mirrors fo-dicom `SpatialTransform`: scale / rotation (90° steps) / flip_x /
//! flip_y / pan / reset. Rendering applies rotation first, then flips (fo-dicom
//! `ImageBase.ToBytes`). Applied to a [`RawImage`] via
//! [`SpatialTransform::apply_to`].

use super::math::Point2;
use super::RawImage;

/// A spatial 2D transform (fo-dicom `SpatialTransform`).
#[derive(Debug, Clone, PartialEq)]
pub struct SpatialTransform {
    pub scale: f64,
    pub rotation: i32,
    pub flip_x: bool,
    pub flip_y: bool,
    pub pan: Point2,
}

impl Default for SpatialTransform {
    fn default() -> Self {
        Self::new()
    }
}

impl SpatialTransform {
    pub fn new() -> Self {
        SpatialTransform {
            scale: 1.0,
            rotation: 0,
            flip_x: false,
            flip_y: false,
            pan: Point2::ORIGIN,
        }
    }

    /// Adds `angle` degrees to the accumulated rotation (fo-dicom `Rotate`).
    pub fn rotate(&mut self, angle: i32) {
        self.rotation += angle;
    }

    /// Resets to the identity transform (fo-dicom `Reset`).
    pub fn reset(&mut self) {
        *self = SpatialTransform::new();
    }

    /// True when any transform is set (fo-dicom `IsTransformed`). Scale differs
    /// from 1, a non-zero rotation or non-origin pan qualifies; flips alone do
    /// not (matching fo-dicom, which omits flips from the check).
    pub fn is_transformed(&self) -> bool {
        self.scale != 1.0 || self.rotation != 0 || self.pan != Point2::ORIGIN
    }

    /// Rotate, flip and pan a [`RawImage`] into a new image (fo-dicom `Rotate`
    /// then `Flip`, then pan). Scale is not pixel-geometric here and is left to
    /// the caller; rotation-by-90°, flips and integer pan reshape/translate the
    /// buffer (exposed pan areas are filled black).
    pub fn apply_to(&self, image: &RawImage) -> RawImage {
        let (width, height) = (image.width(), image.height());
        let mut pixels = pad_rgb(image);

        // rotation first (fo-dicom order). Only right angles are meaningful:
        // fo-dicom's GetRotateFlipType matches 90/180/270 and treats anything
        // else as no rotation, so a 45° request is ignored (with a warning)
        // rather than silently rendered as 90°.
        let angle = ((self.rotation % 360) + 360) % 360;
        if !matches!(angle, 0 | 90 | 180 | 270) {
            tracing::warn!(
                rotation = angle,
                "only 90/180/270 degree rotations are applied"
            );
        }
        let (width, height) = match angle {
            90 => {
                let rotated = rotate_90(&pixels, width, height);
                pixels = rotated;
                (height, width)
            }
            180 => {
                pixels = rotate_180(&pixels, width, height);
                (width, height)
            }
            270 => {
                let rotated = rotate_270(&pixels, width, height);
                pixels = rotated;
                (height, width)
            }
            _ => (width, height),
        };

        // then flips
        if self.flip_x {
            pixels = flip_horizontal(&pixels, width, height);
        }
        if self.flip_y {
            pixels = flip_vertical(&pixels, width, height);
        }

        // then pan: integer pixel translation, exposed area filled black
        let dx = self.pan.x.round() as i32;
        let dy = self.pan.y.round() as i32;
        if dx != 0 || dy != 0 {
            let (w, h) = (width as i32, height as i32);
            let mut moved = vec![0u32; pixels.len()];
            for y in 0..h {
                for x in 0..w {
                    let (sx, sy) = (x - dx, y - dy);
                    if sx >= 0 && sx < w && sy >= 0 && sy < h {
                        moved[(y * w + x) as usize] = pixels[(sy * w + sx) as usize];
                    }
                }
            }
            pixels = moved;
        }

        to_raw_image(&pixels, width, height)
    }
}

/// Views a BGRA `RawImage` as packed RGB `u32` (0x00RRGGBB) for rotation algebra.
fn pad_rgb(image: &RawImage) -> Vec<u32> {
    let bytes = image.as_bytes();
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| (p[2] as u32) << 16 | (p[1] as u32) << 8 | p[0] as u32)
        .collect()
}

fn to_raw_image(rgb: &[u32], width: u32, height: u32) -> RawImage {
    let mut bytes = Vec::with_capacity(rgb.len() * 4);
    for &p in rgb {
        let r = ((p >> 16) & 0xFF) as u8;
        let g = ((p >> 8) & 0xFF) as u8;
        let b = (p & 0xFF) as u8;
        bytes.extend_from_slice(&[b, g, r, 0xFF]);
    }
    RawImage::new(width, height, bytes)
}

/// 90° clockwise (fo-dicom: new[i] = old[y*width + x] scanning x outward,
/// y inward; width/height swap).
fn rotate_90(src: &[u32], width: u32, height: u32) -> Vec<u32> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u32; w * h];
    let mut i = 0;
    for x in 0..w {
        for y in (0..h).rev() {
            out[i] = src[y * w + x];
            i += 1;
        }
    }
    out
}

/// 180° (fo-dicom: reverse scan both axes).
fn rotate_180(src: &[u32], width: u32, height: u32) -> Vec<u32> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u32; w * h];
    let mut i = 0;
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            out[i] = src[y * w + x];
            i += 1;
        }
    }
    out
}

/// 270° clockwise (fo-dicom: x inward, y outward; width/height swap).
fn rotate_270(src: &[u32], width: u32, height: u32) -> Vec<u32> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u32; w * h];
    let mut i = 0;
    for x in (0..w).rev() {
        for y in 0..h {
            out[i] = src[y * w + x];
            i += 1;
        }
    }
    out
}

fn flip_horizontal(src: &[u32], width: u32, height: u32) -> Vec<u32> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u32; w * h];
    let mut i = 0;
    for y in 0..h {
        for x in (0..w).rev() {
            out[i] = src[y * w + x];
            i += 1;
        }
    }
    out
}

fn flip_vertical(src: &[u32], width: u32, height: u32) -> Vec<u32> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u32; w * h];
    let mut i = 0;
    for y in (0..h).rev() {
        for x in 0..w {
            out[i] = src[y * w + x];
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a RawImage from an RGB grid (row-major), given as packed colours.
    fn image(width: u32, height: u32, rgb: &[u32]) -> RawImage {
        to_raw_image(rgb, width, height)
    }

    fn red() -> u32 {
        0xFF0000
    }
    fn green() -> u32 {
        0x00FF00
    }
    fn blue() -> u32 {
        0x0000FF
    }

    fn pixel(image: &RawImage, x: u32, y: u32) -> u32 {
        let i = ((y * image.width() + x) * 4) as usize;
        let b = image.as_bytes();
        (b[i + 2] as u32) << 16 | (b[i + 1] as u32) << 8 | b[i] as u32
    }

    #[test]
    fn identity_leaves_the_image_unchanged() {
        let img = image(2, 2, &[red(), green(), blue(), 0xFFFFFF]);
        let out = SpatialTransform::new().apply_to(&img);
        assert_eq!((out.width(), out.height()), (2, 2));
        assert_eq!(pixel(&out, 0, 0), red());
        assert_eq!(pixel(&out, 1, 0), green());
    }

    #[test]
    fn a_ninety_degree_rotation_swaps_dimensions() {
        // 2x1 image becomes 1x2
        let img = image(2, 1, &[red(), green()]);
        let mut t = SpatialTransform::new();
        t.rotation = 90;
        let out = t.apply_to(&img);
        assert_eq!((out.width(), out.height()), (1, 2));
        // after 90° clockwise, the left pixel (red) ends up at the top
        assert_eq!(pixel(&out, 0, 0), red());
        assert_eq!(pixel(&out, 0, 1), green());
    }

    #[test]
    fn a_180_degree_rotation_reverses_without_swapping() {
        let img = image(2, 1, &[red(), green()]);
        let mut t = SpatialTransform::new();
        t.rotation = 180;
        let out = t.apply_to(&img);
        assert_eq!((out.width(), out.height()), (2, 1));
        assert_eq!(pixel(&out, 0, 0), green(), "the order is reversed");
        assert_eq!(pixel(&out, 1, 0), red());
    }

    #[test]
    fn flip_x_mirrors_columns() {
        let img = image(2, 1, &[red(), green()]);
        let mut t = SpatialTransform::new();
        t.flip_x = true;
        let out = t.apply_to(&img);
        assert_eq!(pixel(&out, 0, 0), green());
        assert_eq!(pixel(&out, 1, 0), red());
    }

    #[test]
    fn flip_y_mirrors_rows() {
        let img = image(1, 2, &[red(), green()]);
        let mut t = SpatialTransform::new();
        t.flip_y = true;
        let out = t.apply_to(&img);
        assert_eq!(pixel(&out, 0, 0), green());
        assert_eq!(pixel(&out, 0, 1), red());
    }

    #[test]
    fn rotate_accumulates_and_wraps() {
        let mut t = SpatialTransform::new();
        t.rotate(90);
        t.rotate(90);
        assert_eq!(t.rotation, 180);
        t.rotate(360);
        assert_eq!(t.rotation, 540, "the raw accumulated value is kept");
        // applying wraps 540 % 360 = 180
        let img = image(2, 1, &[red(), green()]);
        let out = t.apply_to(&img);
        assert_eq!(
            (out.width(), out.height()),
            (2, 1),
            "540° == 180°, dimensions preserved"
        );
    }

    #[test]
    fn is_transformed_and_reset() {
        let mut t = SpatialTransform::new();
        assert!(!t.is_transformed());
        t.rotation = 90;
        assert!(t.is_transformed());
        t.reset();
        assert!(!t.is_transformed());
        // a flip alone is not counted (matching fo-dicom)
        t.flip_x = true;
        assert!(!t.is_transformed());
    }

    #[test]
    fn pan_translates_right_and_fills_exposed_area_black() {
        let img = image(2, 1, &[red(), green()]);
        let mut t = SpatialTransform::new();
        t.pan = Point2 { x: 1.0, y: 0.0 };
        assert!(t.is_transformed());
        let out = t.apply_to(&img);
        assert_eq!(
            (out.width(), out.height()),
            (2, 1),
            "pan preserves dimensions"
        );
        assert_eq!(pixel(&out, 0, 0), 0, "left edge exposed → black");
        assert_eq!(pixel(&out, 1, 0), red(), "content shifted right");
    }

    #[test]
    fn pan_down_shifts_rows() {
        let img = image(1, 2, &[red(), green()]);
        let mut t = SpatialTransform::new();
        t.pan = Point2 { x: 0.0, y: 1.0 };
        let out = t.apply_to(&img);
        assert_eq!(pixel(&out, 0, 0), 0, "top row exposed → black");
        assert_eq!(pixel(&out, 0, 1), red(), "content shifted down");
    }
}
