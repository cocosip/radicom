//! 2D rescaling: bilinear and nearest-neighbour interpolation (REND-06;
//! docs/01-architecture.md §10).
//!
//! Mirrors fo-dicom `Imaging/Algorithms`: bilinear for grayscale (any integer
//! sample type) and 24-bit colour, nearest-neighbour for 1-bit. Both map output
//! pixel `x` to input `x * (inputWidth-1)/(outputWidth-1)`, matching fo-dicom's
//! endpoint-preserving grid sampling.

/// Bilinear rescale of a single-channel image (grayscale, any integer type).
///
/// Uses `f64` accumulation and truncating cast back to `T` via `round`, matching
/// fo-dicom's `(byte)`/`(ushort)` casts. A single-row or single-column input
/// cannot divide by zero, so `outputWidth`/`outputHeight` of 1 are handled.
pub fn bilinear_grayscale<T>(
    input: &[T],
    input_width: usize,
    input_height: usize,
    output_width: usize,
    output_height: usize,
) -> Vec<T>
where
    T: Copy + Into<f64> + FromF64Clamped,
{
    let mut output = Vec::with_capacity(output_width * output_height);
    let x_f = ratio(input_width, output_width);
    let y_f = ratio(input_height, output_height);
    let x_max = input_width.saturating_sub(1);
    let y_max = input_height.saturating_sub(1);

    for y in 0..output_height {
        let oy0 = y as f64 * y_f;
        let oy1 = oy0 as usize;
        let oy2 = if oy1 == y_max { oy1 } else { oy1 + 1 };
        let dy1 = oy0 - oy1 as f64;
        let dy2 = 1.0 - dy1;
        let row1 = input_width * oy1;
        let row2 = input_width * oy2;

        for x in 0..output_width {
            let ox0 = x as f64 * x_f;
            let ox1 = ox0 as usize;
            let ox2 = if ox1 == x_max { ox1 } else { ox1 + 1 };
            let dx1 = ox0 - ox1 as f64;
            let dx2 = 1.0 - dx1;

            let v = dy2 * (dx2 * input[row1 + ox1].into() + dx1 * input[row1 + ox2].into())
                + dy1 * (dx2 * input[row2 + ox1].into() + dx1 * input[row2 + ox2].into());
            output.push(T::from_f64_clamped(v));
        }
    }
    output
}

/// Bilinear rescale of a 24-bit interleaved RGB image (3 bytes/pixel).
pub fn bilinear_color24(
    input: &[u8],
    input_width: usize,
    input_height: usize,
    output_width: usize,
    output_height: usize,
) -> Vec<u8> {
    let mut output = vec![0u8; output_width * output_height * 3];
    let x_f = ratio(input_width, output_width);
    let y_f = ratio(input_height, output_height);
    let x_max = input_width.saturating_sub(1);
    let y_max = input_height.saturating_sub(1);

    for y in 0..output_height {
        let oy0 = y as f64 * y_f;
        let oy1 = oy0 as usize;
        let oy2 = if oy1 == y_max { oy1 } else { oy1 + 1 };
        let dy1 = oy0 - oy1 as f64;
        let dy2 = 1.0 - dy1;
        let row1 = input_width * oy1 * 3;
        let row2 = input_width * oy2 * 3;

        for x in 0..output_width {
            let ox0 = x as f64 * x_f;
            let ox1 = ox0 as usize;
            let ox2 = if ox1 == x_max { ox1 } else { ox1 + 1 };
            let dx1 = ox0 - ox1 as f64;
            let dx2 = 1.0 - dx1;
            let x1 = ox1 * 3;
            let x2 = ox2 * 3;
            for c in 0..3 {
                let v = dy2 * (dx2 * input[row1 + x1 + c] as f64 + dx1 * input[row1 + x2 + c] as f64)
                    + dy1 * (dx2 * input[row2 + x1 + c] as f64 + dx1 * input[row2 + x2 + c] as f64);
                output[(output_width * y + x) * 3 + c] = v as u8;
            }
        }
    }
    output
}

/// Nearest-neighbour rescale of a single-channel image (for 1-bit data).
///
/// Each output pixel takes the input sample whose fractional coordinate rounds
/// to the nearest source pixel (threshold 0.5), matching fo-dicom.
pub fn nearest_grayscale<T: Copy>(
    input: &[T],
    input_width: usize,
    input_height: usize,
    output_width: usize,
    output_height: usize,
) -> Vec<T> {
    let mut output = Vec::with_capacity(output_width * output_height);
    let x_f = ratio(input_width, output_width);
    let y_f = ratio(input_height, output_height);
    let x_max = input_width.saturating_sub(1);
    let y_max = input_height.saturating_sub(1);

    for y in 0..output_height {
        let iy0 = y as f64 * y_f;
        let iy1 = iy0 as usize;
        let iy2 = if iy1 == y_max { iy1 } else { iy1 + 1 };
        let row = input_width * if (iy0 - iy1 as f64) < 0.5 { iy1 } else { iy2 };
        for x in 0..output_width {
            let ix0 = x as f64 * x_f;
            let ix1 = ix0 as usize;
            let ix2 = if ix1 == x_max { ix1 } else { ix1 + 1 };
            let ix = if (ix0 - ix1 as f64) < 0.5 { ix1 } else { ix2 };
            output.push(input[row + ix]);
        }
    }
    output
}

/// `(input - 1) / (output - 1)`, guarding against an output dimension of 1.
fn ratio(input: usize, output: usize) -> f64 {
    if output <= 1 {
        0.0
    } else {
        (input as f64 - 1.0) / (output as f64 - 1.0)
    }
}

/// Converts an `f64` back to the sample type, truncating like fo-dicom's casts.
pub trait FromF64Clamped {
    fn from_f64_clamped(value: f64) -> Self;
}

impl FromF64Clamped for u8 {
    fn from_f64_clamped(value: f64) -> Self {
        value.clamp(0.0, 255.0) as u8
    }
}

impl FromF64Clamped for u16 {
    fn from_f64_clamped(value: f64) -> Self {
        value.clamp(0.0, 65535.0) as u16
    }
}

impl FromF64Clamped for u32 {
    fn from_f64_clamped(value: f64) -> Self {
        value.clamp(0.0, u32::MAX as f64) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bilinear_to_the_same_size_is_identity() {
        let input: Vec<u8> = vec![10, 20, 30, 40];
        let out = bilinear_grayscale(&input, 2, 2, 2, 2);
        assert_eq!(out, input);
    }

    #[test]
    fn bilinear_upscale_preserves_the_corners() {
        // 2x2 → 3x3; corners stay pinned because the grid endpoints are preserved
        let input: Vec<u8> = vec![0, 255, 255, 0];
        let out = bilinear_grayscale(&input, 2, 2, 3, 3);
        assert_eq!(out.len(), 9);
        assert_eq!(out[0], 0, "top-left corner preserved");
        assert_eq!(out[2], 255, "top-right corner preserved");
        assert_eq!(out[6], 255, "bottom-left corner preserved");
        assert_eq!(out[8], 0, "bottom-right corner preserved");
        // the exact centre of a 0/255 checkerboard is a 50/50 mix
        assert!((127..=128).contains(&out[4]), "centre is mid-grey, got {}", out[4]);
    }

    #[test]
    fn bilinear_works_for_u16_samples() {
        let input: Vec<u16> = vec![0, 1000, 1000, 0];
        let out = bilinear_grayscale(&input, 2, 2, 2, 2);
        assert_eq!(out, input, "identity preserves 16-bit values");
    }

    #[test]
    fn bilinear_single_output_row_does_not_divide_by_zero() {
        let input: Vec<u8> = vec![5, 15, 25];
        let out = bilinear_grayscale(&input, 3, 1, 3, 1);
        assert_eq!(out, input, "a 1-row image passes through");
    }

    #[test]
    fn bilinear_color24_upscales_each_channel() {
        // 2x2 RGB: red, green / blue, white
        let input: Vec<u8> = vec![
            255, 0, 0, 0, 255, 0, // row 0
            0, 0, 255, 255, 255, 255, // row 1
        ];
        let out = bilinear_color24(&input, 2, 2, 3, 3);
        assert_eq!(out.len(), 3 * 3 * 3);
        // top-left corner preserved (red)
        assert_eq!(&out[0..3], &[255, 0, 0]);
        // bottom-right corner preserved (white)
        assert_eq!(&out[24..27], &[255, 255, 255]);
    }

    #[test]
    fn nearest_neighbor_downscale_picks_source_pixels() {
        // 4x1 → 2x1 picks input[0] and input[2] (round-to-nearest of 0 and 2*1.5=3→… )
        let input: Vec<u8> = vec![10, 20, 30, 40];
        let out = nearest_grayscale(&input, 4, 1, 2, 1);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], 10, "leftmost source sample");
        // x=1 → ix0 = 1 * (3/1) = 3.0 → ix1=3, frac 0 → picks 3 → 40
        assert_eq!(out[1], 40, "rightmost source sample");
    }

    #[test]
    fn nearest_neighbor_identity_and_upscale() {
        let input: Vec<u8> = vec![1, 2, 3, 4];
        assert_eq!(nearest_grayscale(&input, 2, 2, 2, 2), input);
        // upscaling 2x1 → 4x1 duplicates the nearest source
        let out = nearest_grayscale(&[7u8, 9], 2, 1, 4, 1);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], 7);
        assert_eq!(out[3], 9);
    }
}
