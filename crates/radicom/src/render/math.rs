//! Geometry and statistics primitives (REND-11; docs/10-rendering.md §6).
//!
//! Mirrors fo-dicom `Imaging/Mathematics`: `Point2`/`Point3`/`Vector3`, an
//! integer `Matrix`, `Rect`, `Interval`, `Histogram`, `BitList` and
//! `MovingAverage`. Kept dependency-free so the render pipeline and the future
//! MPR/geometry work can build on it.

/// A 2D point / vector with `f64` coordinates (fo-dicom `Point2<T>`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point2 {
    pub x: f64,
    pub y: f64,
}

impl Point2 {
    pub const ORIGIN: Point2 = Point2 { x: 0.0, y: 0.0 };

    pub fn new(x: f64, y: f64) -> Self {
        Point2 { x, y }
    }

    /// Component-wise rounding to an integer grid point.
    pub fn round(&self) -> (i64, i64) {
        (self.x.round() as i64, self.y.round() as i64)
    }
}

/// A 3D point with `f64` coordinates (fo-dicom `Point3<T>`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Point3 {
    pub const ORIGIN: Point3 = Point3 { x: 0.0, y: 0.0, z: 0.0 };

    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Point3 { x, y, z }
    }
}

/// A 3D vector with `f64` components and the usual vector algebra
/// (fo-dicom `Vector3<T>`: length, normalize, dot, cross).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vector3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vector3 {
    pub const ZERO: Vector3 = Vector3 { x: 0.0, y: 0.0, z: 0.0 };
    pub const AXIS_X: Vector3 = Vector3 { x: 1.0, y: 0.0, z: 0.0 };
    pub const AXIS_Y: Vector3 = Vector3 { x: 0.0, y: 1.0, z: 0.0 };
    pub const AXIS_Z: Vector3 = Vector3 { x: 0.0, y: 0.0, z: 1.0 };

    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Vector3 { x, y, z }
    }

    /// Euclidean length.
    pub fn length(&self) -> f64 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    /// True when every component is (quasi) zero.
    pub fn is_zero(&self) -> bool {
        self.length() <= 1e-9
    }

    /// Unit vector in the same direction; the zero vector is returned as-is.
    pub fn normalize(&self) -> Vector3 {
        let len = self.length();
        if len <= 1e-9 {
            Vector3::ZERO
        } else {
            Vector3::new(self.x / len, self.y / len, self.z / len)
        }
    }

    /// Dot product.
    pub fn dot(&self, other: &Vector3) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// Cross product.
    pub fn cross(&self, other: &Vector3) -> Vector3 {
        Vector3::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }
}

/// A rectangular region: start corner plus (non-negative) size
/// (fo-dicom `Rect<T>`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    /// Creates a rectangle, clamping negative sizes to zero (fo-dicom rejects
    /// them; clamping is the safe choice at a trust boundary).
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Rect { x, y, width: width.max(0.0), height: height.max(0.0) }
    }

    /// Grows the rectangle by `(dx, dy)` on each side (fo-dicom `Inflate`).
    pub fn inflate(&mut self, dx: f64, dy: f64) {
        // never shrink past zero width/height
        let dx = dx.max(-self.width / 2.0);
        let dy = dy.max(-self.height / 2.0);
        self.x -= dx;
        self.y -= dy;
        self.width += 2.0 * dx;
        self.height += 2.0 * dy;
    }
}

/// A closed interval `[min, max]` (fo-dicom `Interval<T>`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    pub min: f64,
    pub max: f64,
}

impl Interval {
    pub fn new(min: f64, max: f64) -> Self {
        Interval { min, max }
    }

    /// True when `value` lies within the closed interval.
    pub fn contains(&self, value: f64) -> bool {
        self.min <= value && value <= self.max
    }

    pub fn center(&self) -> f64 {
        (self.min + self.max) / 2.0
    }

    pub fn width(&self) -> f64 {
        self.max - self.min
    }
}

/// A histogram of integer bin counts over `[min, max]` (fo-dicom `Histogram`).
///
/// Bins are addressed by their absolute value (not position). `apply_window`
/// shrinks the active window from both ends until it holds at most `percent`%
/// of the total, matching fo-dicom's `ApplyWindow(int percent)`.
#[derive(Debug, Clone)]
pub struct Histogram {
    values: Vec<i32>,
    offset: i32,
    total: i32,
    window: i32,
    wstart: usize,
    wend: usize,
    wtotal: i32,
}

impl Histogram {
    pub fn new(min: i32, max: i32) -> Self {
        let range = (max - min + 1).max(1) as usize;
        Histogram {
            values: vec![0; range],
            offset: -min,
            total: 0,
            window: -1,
            wstart: 0,
            wend: range - 1,
            wtotal: 0,
        }
    }

    fn position(&self, value: i32) -> i32 {
        value + self.offset
    }

    /// Count at `value` (0 when out of range).
    pub fn count(&self, value: i32) -> i32 {
        let pos = self.position(value);
        if pos < 0 || pos as usize >= self.values.len() {
            0
        } else {
            self.values[pos as usize]
        }
    }

    /// Increments the bin for `value` (no-op when out of range).
    pub fn add(&mut self, value: i32) {
        let pos = self.position(value);
        if pos < 0 || pos as usize >= self.values.len() {
            return;
        }
        self.values[pos as usize] += 1;
        self.total += 1;
        if pos as usize >= self.wstart && pos as usize <= self.wend {
            self.wtotal += 1;
        }
    }

    /// Total number of values added.
    pub fn total(&self) -> i32 {
        self.total
    }

    /// The current window's start value.
    pub fn window_start(&self) -> i32 {
        self.wstart as i32 - self.offset
    }

    /// The current window's end value.
    pub fn window_end(&self) -> i32 {
        self.wend as i32 - self.offset
    }

    /// Total count currently inside the window.
    pub fn window_total(&mut self) -> i32 {
        if self.window == -1 {
            self.apply_window_percent(100);
        }
        self.wtotal
    }

    /// Shrinks the window until it holds ≤ `percent`% of the total
    /// (fo-dicom `ApplyWindow(int percent)`).
    pub fn apply_window_percent(&mut self, percent: i32) {
        self.wstart = 0;
        self.wend = self.values.len() - 1;
        self.window = percent;
        self.wtotal = self.total;
        if percent == 100 || self.total == 0 {
            return;
        }
        let target = (self.total as f64 * (percent as f64 / 100.0)) as i32;
        while self.wtotal > target {
            if self.values[self.wstart] >= self.values[self.wend] {
                let next = self.wtotal - self.values[self.wstart];
                if next < target {
                    break;
                }
                self.wstart += 1;
                self.wtotal = next;
            } else {
                let next = self.wtotal - self.values[self.wend];
                if next < target {
                    break;
                }
                self.wend -= 1;
                self.wtotal = next;
            }
            if self.wstart == self.wend {
                break;
            }
        }
    }

    /// Applies an explicit window `[start, end]` in value space
    /// (fo-dicom `ApplyWindow(int start, int end)`).
    pub fn apply_window(&mut self, start: i32, end: i32) {
        self.wstart = (start + self.offset).max(0) as usize;
        self.wend = (end + self.offset).min(self.values.len() as i32 - 1) as usize;
        if self.wstart == 0 && self.wend == self.values.len() - 1 {
            self.window = 100;
            self.wtotal = self.total;
            return;
        }
        self.wtotal = 0;
        for i in self.wstart..=self.wend {
            self.wtotal += self.values[i];
        }
        self.window = if self.total > 0 {
            (self.wtotal as f64 / self.total as f64) as i32
        } else {
            0
        };
    }
}

/// A growable bit vector, LSB-first within each byte (fo-dicom `BitList`).
#[derive(Debug, Clone, Default)]
pub struct BitList {
    bytes: Vec<u8>,
}

impl BitList {
    pub fn new() -> Self {
        BitList { bytes: Vec::new() }
    }

    /// Number of bits the storage can currently hold.
    pub fn capacity(&self) -> usize {
        self.bytes.len() * 8
    }

    /// Ensures room for at least `bits` bits.
    pub fn set_capacity(&mut self, bits: usize) {
        let needed = bits.div_ceil(8);
        while self.bytes.len() < needed {
            self.bytes.push(0);
        }
    }

    /// The bit at `pos` (false when beyond the current storage).
    pub fn get(&self, pos: usize) -> bool {
        let byte = pos / 8;
        let mask = 1u8 << (pos % 8);
        self.bytes.get(byte).map(|b| b & mask != 0).unwrap_or(false)
    }

    /// Sets the bit at `pos`, growing the storage as needed.
    pub fn set(&mut self, pos: usize, value: bool) {
        if pos / 8 >= self.bytes.len() {
            self.set_capacity(pos + 1);
        }
        let byte = pos / 8;
        let mask = 1u8 << (pos % 8);
        if value {
            self.bytes[byte] |= mask;
        } else {
            self.bytes[byte] &= !mask;
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// A fixed-window moving average over integer samples (fo-dicom
/// `MovingAverage`). Each `next` returns the average of the last ≤ `window`
/// values, using integer division.
#[derive(Debug, Clone)]
pub struct MovingAverage {
    window: usize,
    values: Vec<i32>,
    count: usize,
}

impl MovingAverage {
    pub fn new(window: usize) -> Self {
        let window = window.max(1);
        MovingAverage { window, values: vec![0; window], count: 0 }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Feeds one sample and returns the current integer average.
    pub fn next(&mut self, value: i32) -> i32 {
        self.values[self.count % self.window] = value;
        self.count += 1;
        let sum: i32 = self.values.iter().sum();
        let divisor = self.count.min(self.window) as i32;
        sum / divisor.max(1)
    }
}

/// A dense integer matrix (fo-dicom integer `Matrix`): add, subtract, multiply,
/// scalar ops, transpose, determinant and identity. Row-major storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matrix {
    rows: usize,
    cols: usize,
    data: Vec<i64>,
}

impl Matrix {
    /// A zero-filled `rows × cols` matrix.
    pub fn zero(rows: usize, cols: usize) -> Self {
        Matrix { rows, cols, data: vec![0; rows * cols] }
    }

    /// The `n × n` identity matrix.
    pub fn identity(n: usize) -> Self {
        let mut m = Matrix::zero(n, n);
        for i in 0..n {
            m.data[i * n + i] = 1;
        }
        m
    }

    /// Builds a matrix from row-major `data`; `None` when lengths disagree.
    pub fn from_rows(rows: usize, cols: usize, data: Vec<i64>) -> Option<Self> {
        if data.len() == rows * cols {
            Some(Matrix { rows, cols, data })
        } else {
            None
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn get(&self, row: usize, col: usize) -> i64 {
        self.data[row * self.cols + col]
    }

    pub fn is_identity(&self) -> bool {
        self.rows == self.cols && *self == Matrix::identity(self.rows)
    }

    /// The transpose.
    pub fn transpose(&self) -> Matrix {
        let mut out = Matrix::zero(self.cols, self.rows);
        for r in 0..self.rows {
            for c in 0..self.cols {
                out.data[c * self.rows + r] = self.data[r * self.cols + c];
            }
        }
        out
    }

    /// The determinant (Laplace expansion); only square matrices are valid.
    pub fn determinant(&self) -> Option<i64> {
        if self.rows != self.cols {
            return None;
        }
        Some(det(&self.data, self.rows))
    }
}

/// Determinant of an `n × n` row-major matrix via cofactor expansion.
fn det(data: &[i64], n: usize) -> i64 {
    if n == 0 {
        return 1;
    }
    if n == 1 {
        return data[0];
    }
    if n == 2 {
        return data[0] * data[3] - data[1] * data[2];
    }
    let mut result = 0i64;
    for col in 0..n {
        // build the minor matrix (skip row 0, column `col`)
        let mut minor = Vec::with_capacity((n - 1) * (n - 1));
        for r in 1..n {
            for c in 0..n {
                if c != col {
                    minor.push(data[r * n + c]);
                }
            }
        }
        let sign = if col % 2 == 0 { 1 } else { -1 };
        result += sign * data[col] * det(&minor, n - 1);
    }
    result
}

impl std::ops::Add for &Matrix {
    type Output = Option<Matrix>;
    fn add(self, rhs: &Matrix) -> Option<Matrix> {
        if self.rows != rhs.rows || self.cols != rhs.cols {
            return None;
        }
        Some(Matrix { rows: self.rows, cols: self.cols, data: self.data.iter().zip(&rhs.data).map(|(a, b)| a + b).collect() })
    }
}

impl std::ops::Mul for &Matrix {
    type Output = Option<Matrix>;
    fn mul(self, rhs: &Matrix) -> Option<Matrix> {
        if self.cols != rhs.rows {
            return None;
        }
        let mut out = Matrix::zero(self.rows, rhs.cols);
        for r in 0..self.rows {
            for c in 0..rhs.cols {
                let mut sum = 0i64;
                for k in 0..self.cols {
                    sum += self.data[r * self.cols + k] * rhs.data[k * rhs.cols + c];
                }
                out.data[r * rhs.cols + c] = sum;
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point2_rounds_to_the_grid() {
        assert_eq!(Point2::new(1.4, 2.6).round(), (1, 3));
    }

    #[test]
    fn vector3_length_dot_and_cross() {
        let a = Vector3::new(3.0, 0.0, 0.0);
        let b = Vector3::new(0.0, 4.0, 0.0);
        assert_eq!(a.length(), 3.0);
        assert_eq!(a.dot(&b), 0.0);
        // x̂ × ŷ = ẑ
        let c = a.normalize().cross(&b.normalize());
        assert!((c.z - 1.0).abs() < 1e-9);
        assert_eq!(Vector3::ZERO.normalize(), Vector3::ZERO);
    }

    #[test]
    fn interval_contains_center_width() {
        let interval = Interval::new(10.0, 20.0);
        assert!(interval.contains(10.0) && interval.contains(20.0) && interval.contains(15.0));
        assert!(!interval.contains(9.9));
        assert_eq!(interval.center(), 15.0);
        assert_eq!(interval.width(), 10.0);
    }

    #[test]
    fn rect_inflate_grows_symmetrically() {
        let mut rect = Rect::new(10.0, 10.0, 20.0, 20.0);
        rect.inflate(5.0, 5.0);
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (5.0, 5.0, 30.0, 30.0));
        // shrinking past zero clamps width/height to zero
        let mut r = Rect::new(0.0, 0.0, 4.0, 4.0);
        r.inflate(-100.0, -100.0);
        assert_eq!((r.width, r.height), (0.0, 0.0));
    }

    #[test]
    fn histogram_counts_and_shrinks_the_window() {
        let mut h = Histogram::new(0, 9);
        // 100 values: 90 at bin 5, 5 at bin 0, 5 at bin 9
        for _ in 0..90 {
            h.add(5);
        }
        for _ in 0..5 {
            h.add(0);
            h.add(9);
        }
        assert_eq!(h.total(), 100);
        assert_eq!(h.count(5), 90);
        // a 90% window should trim the sparse tails toward the dense centre
        h.apply_window_percent(90);
        assert!(h.window_total() >= 85, "the window keeps most of the mass");
        assert!(h.window_start() >= 0 && h.window_end() <= 9);
    }

    #[test]
    fn histogram_explicit_window_sums_its_bins() {
        let mut h = Histogram::new(0, 9);
        h.add(2);
        h.add(2);
        h.add(7);
        h.apply_window(0, 5);
        assert_eq!(h.window_total(), 2, "only the two values in [0,5] are counted");
        assert_eq!((h.window_start(), h.window_end()), (0, 5));
    }

    #[test]
    fn histogram_ignores_out_of_range_values() {
        let mut h = Histogram::new(10, 20);
        h.add(5); // below min
        h.add(25); // above max
        assert_eq!(h.total(), 0, "out-of-range values are dropped");
    }

    #[test]
    fn bitlist_sets_and_reads_bits_lsb_first() {
        let mut bits = BitList::new();
        bits.set(0, true);
        bits.set(9, true);
        assert!(bits.get(0));
        assert!(!bits.get(1));
        assert!(bits.get(9));
        assert!(!bits.get(100), "unset high bit reads false");
        // bit 0 is the low bit of byte 0; bit 9 is bit 1 of byte 1
        assert_eq!(bits.as_bytes()[0], 0b0000_0001);
        assert_eq!(bits.as_bytes()[1], 0b0000_0010);
    }

    #[test]
    fn moving_average_smooths_a_window() {
        let mut avg = MovingAverage::new(3);
        assert_eq!(avg.next(3), 3); // count 1 → 3/1
        assert_eq!(avg.next(6), 4); // (3+6)/2 = 4
        assert_eq!(avg.next(9), 6); // (3+6+9)/3 = 6
        assert_eq!(avg.next(12), 9); // window slides: (6+9+12)/3 = 9
    }

    #[test]
    fn matrix_multiply_add_and_transpose() {
        let a = Matrix::from_rows(2, 2, vec![1, 2, 3, 4]).unwrap();
        let b = Matrix::identity(2);
        assert!((&a * &b).unwrap() == a, "multiplying by identity is a no-op");
        assert!((&a + &b).unwrap() == Matrix::from_rows(2, 2, vec![2, 2, 3, 5]).unwrap());
        assert_eq!(a.transpose(), Matrix::from_rows(2, 2, vec![1, 3, 2, 4]).unwrap());
        // dimension mismatch yields None instead of panicking
        assert!((&a + &Matrix::zero(3, 3)).is_none());
        assert!((&a * &Matrix::zero(3, 3)).is_none());
    }

    #[test]
    fn matrix_determinant() {
        assert_eq!(Matrix::from_rows(2, 2, vec![1, 2, 3, 4]).unwrap().determinant(), Some(-2));
        assert_eq!(Matrix::identity(3).determinant(), Some(1));
        assert_eq!(Matrix::zero(3, 2).determinant(), None, "non-square has no determinant");
    }
}
