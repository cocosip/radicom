//! Geometry and statistics primitives (REND-11; docs/01-architecture.md §10).
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
    pub const ORIGIN: Point3 = Point3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Point3 { x, y, z }
    }

    /// Distance to another point (fo-dicom `Point3.Distance`).
    pub fn distance(&self, other: &Point3) -> f64 {
        Vector3::new(self.x - other.x, self.y - other.y, self.z - other.z).length()
    }

    /// Moves the point `distance` mm along `axis` (fo-dicom `Point3.Move`).
    pub fn moved(&self, axis: &Vector3, distance: f64) -> Point3 {
        let axis = axis.normalize();
        Point3::new(
            self.x + axis.x * distance,
            self.y + axis.y * distance,
            self.z + axis.z * distance,
        )
    }

    /// This point as a vector from the origin.
    pub fn to_vector(&self) -> Vector3 {
        Vector3::new(self.x, self.y, self.z)
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
    pub const ZERO: Vector3 = Vector3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    pub const AXIS_X: Vector3 = Vector3 {
        x: 1.0,
        y: 0.0,
        z: 0.0,
    };
    pub const AXIS_Y: Vector3 = Vector3 {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };
    pub const AXIS_Z: Vector3 = Vector3 {
        x: 0.0,
        y: 0.0,
        z: 1.0,
    };

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

    /// Distance to another vector's tip (fo-dicom `Vector3.Distance`).
    pub fn distance(&self, other: &Vector3) -> f64 {
        Vector3::new(self.x - other.x, self.y - other.y, self.z - other.z).length()
    }

    /// True when the two vectors are (quasi) perpendicular (fo-dicom
    /// `Vector3.IsPerpendicular`).
    pub fn is_perpendicular(&self, other: &Vector3) -> bool {
        self.dot(other).abs() <= 1e-9
    }

    /// Rotates this vector about `axis` by `angle` radians (Rodrigues' formula,
    /// fo-dicom `Vector3.Rotate`; the zero axis leaves the vector unchanged).
    pub fn rotate(&self, axis: &Vector3, angle: f64) -> Vector3 {
        let axis = axis.normalize();
        if axis.is_zero() {
            return *self;
        }
        let dot = self.dot(&axis);
        let parallel = Vector3::new(axis.x * dot, axis.y * dot, axis.z * dot);
        let perpendicular = Vector3::new(
            self.x - parallel.x,
            self.y - parallel.y,
            self.z - parallel.z,
        );
        let mutual = axis.cross(&perpendicular);
        let (sin, cos) = angle.sin_cos();
        Vector3::new(
            perpendicular.x * cos + mutual.x * sin + parallel.x,
            perpendicular.y * cos + mutual.y * sin + parallel.y,
            perpendicular.z * cos + mutual.z * sin + parallel.z,
        )
    }

    /// Reflects this vector off the plane whose unit normal is `normal`
    /// (fo-dicom `Vector3.Reflect`).
    pub fn reflect(&self, normal: &Vector3) -> Vector3 {
        let dot = self.dot(normal);
        Vector3::new(
            self.x - 2.0 * dot * normal.x,
            self.y - 2.0 * dot * normal.y,
            self.z - 2.0 * dot * normal.z,
        )
    }

    /// The signed unit axis with the largest absolute component (fo-dicom
    /// `Vector3.NearestAxis`).
    pub fn nearest_axis(&self) -> Vector3 {
        let (x, y, z) = (self.x.abs(), self.y.abs(), self.z.abs());
        if x >= y && x >= z {
            Vector3::new(self.x.signum(), 0.0, 0.0)
        } else if y >= z {
            Vector3::new(0.0, self.y.signum(), 0.0)
        } else {
            Vector3::new(0.0, 0.0, self.z.signum())
        }
    }

    /// This vector as a point.
    pub fn to_point(&self) -> Point3 {
        Point3::new(self.x, self.y, self.z)
    }
}

/// An infinite line: a point and a direction vector (fo-dicom `Line3`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Line3 {
    pub point: Point3,
    pub vector: Vector3,
}

impl Line3 {
    pub fn new(point: Point3, vector: Vector3) -> Self {
        Line3 { point, vector }
    }

    /// The line through two points (fo-dicom `Line3(p1, p2)`).
    pub fn from_points(a: Point3, b: Point3) -> Self {
        Line3 {
            point: a,
            vector: Vector3::new(b.x - a.x, b.y - a.y, b.z - a.z),
        }
    }

    /// Where the line crosses `plane`, or `None` when it is parallel to it.
    pub fn intersect_plane(&self, plane: &Plane3) -> Option<Point3> {
        let denominator = self.vector.dot(&plane.normal);
        if denominator.abs() <= 1e-9 {
            return None;
        }
        let offset = Vector3::new(
            plane.point.x - self.point.x,
            plane.point.y - self.point.y,
            plane.point.z - self.point.z,
        );
        let t = offset.dot(&plane.normal) / denominator;
        Some(Point3::new(
            self.point.x + self.vector.x * t,
            self.point.y + self.vector.y * t,
            self.point.z + self.vector.z * t,
        ))
    }
}

/// A line segment between two points (fo-dicom `Segment3`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Segment3 {
    pub a: Point3,
    pub b: Point3,
}

impl Segment3 {
    pub fn new(a: Point3, b: Point3) -> Self {
        Segment3 { a, b }
    }

    /// Segment length in mm.
    pub fn length(&self) -> f64 {
        self.a.distance(&self.b)
    }

    /// The direction and length of the segment (fo-dicom `Vector`).
    pub fn vector(&self) -> Vector3 {
        Vector3::new(
            self.b.x - self.a.x,
            self.b.y - self.a.y,
            self.b.z - self.a.z,
        )
    }

    /// The unit direction of the segment (fo-dicom `NormalVector`).
    pub fn normal_vector(&self) -> Vector3 {
        self.vector().normalize()
    }

    /// The point halfway along the segment.
    pub fn midpoint(&self) -> Point3 {
        Point3::new(
            (self.a.x + self.b.x) / 2.0,
            (self.a.y + self.b.y) / 2.0,
            (self.a.z + self.b.z) / 2.0,
        )
    }
}

/// A plane: a unit normal and one point on it (fo-dicom `Plane3`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plane3 {
    pub normal: Vector3,
    pub point: Point3,
}

impl Plane3 {
    /// Builds a plane, normalising `normal`; a zero normal yields the XY plane.
    pub fn new(normal: Vector3, point: Point3) -> Self {
        let normal = if normal.is_zero() {
            Vector3::AXIS_Z
        } else {
            normal.normalize()
        };
        Plane3 { normal, point }
    }

    /// The plane through three points (fo-dicom `Plane3(a, b, c)`); collinear
    /// points yield the XY plane through `a`.
    pub fn from_points(a: Point3, b: Point3, c: Point3) -> Self {
        let normal = Vector3::new(b.x - a.x, b.y - a.y, b.z - a.z).cross(&Vector3::new(
            c.x - a.x,
            c.y - a.y,
            c.z - a.z,
        ));
        Self::new(normal, a)
    }

    /// Signed distance from `point` to the plane; positive on the normal side.
    pub fn signed_distance(&self, point: &Point3) -> f64 {
        self.normal.dot(&Vector3::new(
            point.x - self.point.x,
            point.y - self.point.y,
            point.z - self.point.z,
        ))
    }

    /// The orthogonal projection of `point` onto the plane.
    pub fn project(&self, point: &Point3) -> Point3 {
        let distance = self.signed_distance(point);
        Point3::new(
            point.x - self.normal.x * distance,
            point.y - self.normal.y * distance,
            point.z - self.normal.z * distance,
        )
    }
}

/// An oriented sample grid on a plane (fo-dicom `Slice3`, expressed like this
/// crate's [`Slice`](crate::render::Slice) and DICOM `ImageOrientationPatient`):
/// `row_direction` steps along a row across columns, `column_direction` down a
/// column across rows, and their cross product is the slice normal.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Slice3 {
    pub top_left: Point3,
    pub row_direction: Vector3,
    pub column_direction: Vector3,
    pub rows: u32,
    pub columns: u32,
    pub spacing: f64,
}

impl Slice3 {
    pub fn new(
        top_left: Point3,
        row_direction: Vector3,
        column_direction: Vector3,
        rows: u32,
        columns: u32,
        spacing: f64,
    ) -> Self {
        Slice3 {
            top_left,
            row_direction,
            column_direction,
            rows,
            columns,
            spacing,
        }
    }

    /// The patient-space point of sample `(row, column)`.
    pub fn point_at(&self, row: u32, column: u32) -> Point3 {
        Point3::new(
            self.top_left.x
                + self.row_direction.x * column as f64 * self.spacing
                + self.column_direction.x * row as f64 * self.spacing,
            self.top_left.y
                + self.row_direction.y * column as f64 * self.spacing
                + self.column_direction.y * row as f64 * self.spacing,
            self.top_left.z
                + self.row_direction.z * column as f64 * self.spacing
                + self.column_direction.z * row as f64 * self.spacing,
        )
    }

    /// The slice's normal (row × column).
    pub fn normal(&self) -> Vector3 {
        self.row_direction.cross(&self.column_direction).normalize()
    }

    /// The plane this slice lies on.
    pub fn plane(&self) -> Plane3 {
        Plane3::new(self.normal(), self.top_left)
    }
}

/// The three standard anatomical slice orientations (fo-dicom `Orientation3`):
/// `row` runs along a row (increasing column index), `column` down a column
/// (increasing row index), `normal` is their cross product.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Orientation3 {
    pub row: Vector3,
    pub column: Vector3,
    pub normal: Vector3,
}

impl Orientation3 {
    pub fn new(row: Vector3, column: Vector3) -> Self {
        let row = row.normalize();
        let column = column.normalize();
        let normal = row.cross(&column).normalize();
        Orientation3 {
            row,
            column,
            normal,
        }
    }

    /// Axial: rows along +X, columns along +Y, normal +Z.
    pub fn axial() -> Self {
        Orientation3 {
            row: Vector3::AXIS_X,
            column: Vector3::AXIS_Y,
            normal: Vector3::AXIS_Z,
        }
    }

    /// Coronal: rows along +X, columns along +Z, normal −Y.
    pub fn coronal() -> Self {
        Orientation3 {
            row: Vector3::AXIS_X,
            column: Vector3::AXIS_Z,
            normal: Vector3::new(0.0, -1.0, 0.0),
        }
    }

    /// Sagittal: rows along +Y, columns along +Z, normal +X.
    pub fn sagittal() -> Self {
        Orientation3 {
            row: Vector3::AXIS_Y,
            column: Vector3::AXIS_Z,
            normal: Vector3::AXIS_X,
        }
    }

    /// The standard orientation whose normal is closest to `normal`.
    pub fn from_normal(normal: &Vector3) -> Self {
        let axis = normal.nearest_axis();
        if axis.x != 0.0 {
            Self::sagittal()
        } else if axis.y != 0.0 {
            Self::coronal()
        } else {
            Self::axial()
        }
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
        Rect {
            x,
            y,
            width: width.max(0.0),
            height: height.max(0.0),
        }
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

    /// Removes every count at `value` (fo-dicom `Histogram.Clear`), so a padding
    /// spike does not dominate a window derived from the histogram.
    pub fn clear(&mut self, value: i32) {
        let pos = self.position(value);
        if pos < 0 || pos as usize >= self.values.len() {
            return;
        }
        let removed = self.values[pos as usize];
        self.values[pos as usize] = 0;
        self.total -= removed;
        if pos as usize >= self.wstart && pos as usize <= self.wend {
            self.wtotal -= removed;
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
        let len = self.values.len();
        if len == 0 {
            self.wstart = 0;
            self.wend = 0;
            self.wtotal = 0;
            self.window = 0;
            return;
        }
        let last = len as i32 - 1;
        let start_pos = start + self.offset;
        let end_pos = end + self.offset;
        // An entirely-out-of-range or inverted window is empty, not a panic
        // (PR-08: a negative `end` used to become a huge usize).
        if end_pos < 0 || start_pos > last || start_pos > end_pos {
            self.wstart = 0;
            self.wend = 0;
            self.wtotal = 0;
            self.window = 0;
            return;
        }
        self.wstart = start_pos.max(0) as usize;
        self.wend = end_pos.min(last) as usize;
        if self.wstart == 0 && self.wend == len - 1 {
            self.window = 100;
            self.wtotal = self.total;
            return;
        }
        self.wtotal = self.values[self.wstart..=self.wend].iter().sum();
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
        MovingAverage {
            window,
            values: vec![0; window],
            count: 0,
        }
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
        Matrix {
            rows,
            cols,
            data: vec![0; rows * cols],
        }
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
        Some(Matrix {
            rows: self.rows,
            cols: self.cols,
            data: self
                .data
                .iter()
                .zip(&rhs.data)
                .map(|(a, b)| a + b)
                .collect(),
        })
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
    fn geometry3_utilities_rotate_reflect_and_intersect() {
        // rotate +X 90° about +Z → +Y
        let rotated = Vector3::AXIS_X.rotate(&Vector3::AXIS_Z, std::f64::consts::FRAC_PI_2);
        assert!((rotated.y - 1.0).abs() < 1e-9);
        // reflect +Y off a plane whose normal is +Y → -Y
        let reflected = Vector3::AXIS_Y.reflect(&Vector3::AXIS_Y);
        assert!((reflected.y + 1.0).abs() < 1e-9);
        assert_eq!(
            Vector3::new(0.2, -3.0, 0.1).nearest_axis(),
            Vector3::new(0.0, -1.0, 0.0)
        );
        assert!(Vector3::AXIS_X.is_perpendicular(&Vector3::AXIS_Y));
        assert_eq!(
            Point3::new(0.0, 0.0, 0.0).distance(&Point3::new(3.0, 4.0, 0.0)),
            5.0
        );

        // the z = 2 plane through the origin's normal +Z
        let plane = Plane3::new(Vector3::AXIS_Z, Point3::new(0.0, 0.0, 2.0));
        assert_eq!(plane.signed_distance(&Point3::new(1.0, 1.0, 5.0)), 3.0);
        assert_eq!(
            plane.project(&Point3::new(1.0, 1.0, 5.0)),
            Point3::new(1.0, 1.0, 2.0)
        );
        let line = Line3::from_points(Point3::new(0.0, 0.0, 0.0), Point3::new(0.0, 0.0, 4.0));
        assert_eq!(
            line.intersect_plane(&plane).unwrap(),
            Point3::new(0.0, 0.0, 2.0)
        );
        assert!(
            Line3::from_points(Point3::new(0.0, 0.0, 4.0), Point3::new(1.0, 0.0, 4.0))
                .intersect_plane(&plane)
                .is_none()
        );

        let segment = Segment3::new(Point3::new(0.0, 0.0, 0.0), Point3::new(0.0, 2.0, 0.0));
        assert_eq!(segment.length(), 2.0);
        assert_eq!(segment.midpoint(), Point3::new(0.0, 1.0, 0.0));

        let slice = Slice3::new(
            Point3::new(0.0, 0.0, 3.0),
            Vector3::AXIS_X,
            Vector3::AXIS_Y,
            4,
            4,
            0.5,
        );
        assert_eq!(slice.point_at(2, 1), Point3::new(0.5, 1.0, 3.0));
        assert_eq!(slice.normal(), Vector3::AXIS_Z);
        assert_eq!(
            Orientation3::from_normal(&Vector3::new(0.1, -0.2, 0.9)),
            Orientation3::axial()
        );
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
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (5.0, 5.0, 30.0, 30.0)
        );
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
        assert_eq!(
            h.window_total(),
            2,
            "only the two values in [0,5] are counted"
        );
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
        assert!(
            (&a * &b).unwrap() == a,
            "multiplying by identity is a no-op"
        );
        assert!((&a + &b).unwrap() == Matrix::from_rows(2, 2, vec![2, 2, 3, 5]).unwrap());
        assert_eq!(
            a.transpose(),
            Matrix::from_rows(2, 2, vec![1, 3, 2, 4]).unwrap()
        );
        // dimension mismatch yields None instead of panicking
        assert!((&a + &Matrix::zero(3, 3)).is_none());
        assert!((&a * &Matrix::zero(3, 3)).is_none());
    }

    #[test]
    fn matrix_determinant() {
        assert_eq!(
            Matrix::from_rows(2, 2, vec![1, 2, 3, 4])
                .unwrap()
                .determinant(),
            Some(-2)
        );
        assert_eq!(Matrix::identity(3).determinant(), Some(1));
        assert_eq!(
            Matrix::zero(3, 2).determinant(),
            None,
            "non-square has no determinant"
        );
    }

    #[test]
    fn histogram_window_and_clear_cover_edge_values() {
        let mut histogram = Histogram::new(0, 10);
        for value in [0, 1, 2, 2, 3, 10] {
            histogram.add(value);
        }
        assert_eq!(histogram.count(2), 2);
        assert_eq!(histogram.count(99), 0, "out-of-range value counts as zero");
        // clearing an out-of-range value is a no-op
        histogram.clear(99);
        // clearing an in-window bin removes it from the window total too
        let before = histogram.total();
        histogram.clear(2);
        assert_eq!(histogram.total(), before - 2);

        // a full window holds the whole histogram
        let mut histogram = Histogram::new(0, 4);
        for value in 0..=4 {
            histogram.add(value);
        }
        histogram.apply_window(0, 4);
        assert_eq!(histogram.window_total(), histogram.total());
        assert_eq!(histogram.window_start(), 0);
        assert_eq!(histogram.window_end(), 4);

        // a shrinking percent window stops at the dense centre
        let mut histogram = Histogram::new(0, 4);
        for value in 0..=4 {
            histogram.add(value);
        }
        histogram.apply_window_percent(40);
        assert!(histogram.window_end() >= histogram.window_start());
        assert!(histogram.window_total() <= histogram.total());

        // an empty histogram has no percentage to apply
        let mut empty = Histogram::new(0, 2);
        empty.apply_window_percent(50);
        assert_eq!(empty.window_total(), 0);

        // asking for the window total before any window applies 100%
        let mut fresh = Histogram::new(0, 1);
        assert_eq!(fresh.window_total(), 0);
    }

    #[test]
    fn explicit_window_out_of_range_or_inverted_is_empty_not_a_panic() {
        // PR-08: a negative `end` previously cast to usize::MAX and panicked.
        let mut histogram = Histogram::new(0, 10);
        for value in 0..=10 {
            histogram.add(value);
        }
        histogram.apply_window(-100, -50);
        assert_eq!(histogram.window_total(), 0, "entirely below the range");

        let mut histogram = Histogram::new(0, 10);
        for value in 0..=10 {
            histogram.add(value);
        }
        histogram.apply_window(5, 2);
        assert_eq!(histogram.window_total(), 0, "inverted window is empty");

        let mut histogram = Histogram::new(0, 10);
        for value in 0..=10 {
            histogram.add(value);
        }
        histogram.apply_window(0, 10);
        assert_eq!(histogram.window_total(), histogram.total(), "full window");
    }

    #[test]
    fn bit_list_grows_and_reads_beyond_storage_as_false() {
        let mut bits = BitList::new();
        assert_eq!(bits.capacity(), 0);
        assert!(!bits.get(5), "unset storage reads as false");
        bits.set(9, true);
        assert!(bits.capacity() >= 10);
        assert!(bits.get(9));
        bits.set(9, false);
        assert!(!bits.get(9));
    }

    #[test]
    fn matrix_accessors_validate_and_report_identity() {
        assert!(Matrix::from_rows(2, 2, vec![1, 2]).is_none());
        let matrix = Matrix::from_rows(2, 2, vec![1, 0, 0, 1]).unwrap();
        assert_eq!(matrix.rows(), 2);
        assert_eq!(matrix.cols(), 2);
        assert_eq!(matrix.get(1, 1), 1);
        assert!(matrix.is_identity());
        assert_eq!(matrix.transpose(), matrix);
        // determinant of the 0x0 and 1x1 matrices
        assert_eq!(Matrix::zero(0, 0).determinant(), Some(1));
        assert_eq!(
            Matrix::from_rows(1, 1, vec![7]).unwrap().determinant(),
            Some(7)
        );
    }

    #[test]
    fn moving_average_counts_and_averages() {
        let mut average = MovingAverage::new(2);
        assert_eq!(average.count(), 0);
        assert_eq!(average.next(2), 2);
        assert_eq!(average.next(4), 3);
        assert_eq!(average.count(), 2);
        assert_eq!(average.next(10), 7, "the oldest sample drops out");
    }

    #[test]
    fn remaining_geometry_helpers_cover_their_branches() {
        // Point3::moved along a unit axis and to_vector.
        assert_eq!(
            Point3::new(0.0, 0.0, 0.0).moved(&Vector3::AXIS_Z, 2.0),
            Point3::new(0.0, 0.0, 2.0)
        );
        assert_eq!(
            Point3::new(1.0, 2.0, 3.0).to_vector(),
            Vector3::new(1.0, 2.0, 3.0)
        );
        // Vector3::distance.
        assert_eq!(Vector3::ZERO.distance(&Vector3::new(0.0, 3.0, 4.0)), 5.0);
        // Rotating about the zero axis is a no-op.
        assert_eq!(Vector3::AXIS_X.rotate(&Vector3::ZERO, 1.0), Vector3::AXIS_X);
        // nearest_axis hits the x- and z-dominant branches.
        assert_eq!(
            Vector3::new(-2.0, 0.1, 0.1).nearest_axis(),
            Vector3::new(-1.0, 0.0, 0.0)
        );
        assert_eq!(
            Vector3::new(0.1, 0.1, -2.0).nearest_axis(),
            Vector3::new(0.0, 0.0, -1.0)
        );
        // Segment3::normal_vector and Slice3::plane.
        let segment = Segment3::new(Point3::new(0.0, 0.0, 0.0), Point3::new(3.0, 0.0, 0.0));
        assert_eq!(segment.normal_vector(), Vector3::AXIS_X);
        let slice = Slice3::new(
            Point3::new(0.0, 0.0, 0.0),
            Vector3::AXIS_X,
            Vector3::AXIS_Y,
            2,
            2,
            1.0,
        );
        assert_eq!(slice.plane().normal, Vector3::AXIS_Z);
        // Orientation3 coronal/sagittal and Plane3::from_points.
        assert_eq!(Orientation3::coronal().normal, Vector3::new(0.0, -1.0, 0.0));
        assert_eq!(Orientation3::sagittal().normal, Vector3::AXIS_X);
        let plane = Plane3::from_points(
            Point3::new(0.0, 0.0, 0.0),
            Point3::new(1.0, 0.0, 0.0),
            Point3::new(0.0, 1.0, 0.0),
        );
        assert_eq!(plane.normal, Vector3::AXIS_Z);
        // Rect::new clamps negative sizes to zero.
        let rect = Rect::new(0.0, 0.0, -3.0, -4.0);
        assert_eq!((rect.width, rect.height), (0.0, 0.0));
    }
}
