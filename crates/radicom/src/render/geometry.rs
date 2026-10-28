//! Frame geometry: image↔patient transforms and orientation (REND-08;
//! docs/01-architecture.md §10).
//!
//! Mirrors fo-dicom `Imaging/FrameGeometry`: derives row/column unit vectors and
//! the normal (row × column), the four image corners, an image↔patient
//! transform, an orientation classification (Axial/Sagittal/Coronal by the
//! normal's nearest axis) and a geometry type (None/Plane/Volume). Incomplete
//! position/orientation data yields an invalid-but-non-panicking geometry
//! (fo-dicom #2067).

use super::math::{Point2, Point3, Vector3};
use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::item::DicomItem;
use crate::model::tag::Tag;

/// The dominant plane of a frame (fo-dicom `FrameOrientation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOrientation {
    None,
    Axial,
    Sagittal,
    Coronal,
}

/// How much geometry a frame carries (fo-dicom `FrameGeometryType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameGeometryType {
    None,
    Plane,
    Volume,
}

/// 3D frame geometry and image↔patient mapping (fo-dicom `FrameGeometry`).
#[derive(Debug, Clone)]
pub struct FrameGeometry {
    geometry_type: FrameGeometryType,
    frame_of_reference_uid: String,
    direction_row: Vector3,
    direction_column: Vector3,
    direction_normal: Vector3,
    width: u32,
    height: u32,
    spacing_between_rows: f64,
    spacing_between_columns: f64,
    point_top_left: Point3,
    point_top_right: Point3,
    point_bottom_left: Point3,
    point_bottom_right: Point3,
    orientation: FrameOrientation,
}

impl FrameGeometry {
    /// Extracts geometry from a dataset for frame 0.
    pub fn from_dataset(dataset: &Dataset) -> Result<Self, Error> {
        Self::from_dataset_frame(dataset, 0)
    }

    /// Extracts geometry from a dataset for one frame (fo-dicom
    /// `FrameGeometry(DicomDataset, int frame)`).
    ///
    /// Spacing precedence: ImagerPixelSpacing, PixelSpacing (0028,0030),
    /// NominalScannedPixelSpacing (0018,2010), then the flattened enhanced
    /// functional groups (Per-Frame `(5200,9230)` → Pixel Measures `(0028,9110)`,
    /// then Shared `(5200,9229)`). Position/orientation use the top-level tags
    /// first, then the same flattened functional groups, so enhanced multi-frame
    /// geometry is read per frame instead of only from frame 0's top level.
    pub fn from_dataset_frame(dataset: &Dataset, frame: u32) -> Result<Self, Error> {
        let width = dataset.try_get::<u16>(Tag::COLUMNS).unwrap_or(0) as u32;
        let height = dataset.try_get::<u16>(Tag::ROWS).unwrap_or(0) as u32;
        let frame_of_reference_uid = dataset
            .try_get_str(Tag::FRAME_OF_REFERENCE_UID)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let functional = functional_group_values(dataset, frame);

        // spacing: top-level ImagerPixelSpacing, PixelSpacing, Nominal... , then
        // the flattened functional-group values (row, column order in each pair).
        let (spacing_between_rows, spacing_between_columns) = read_pair(dataset, Tag::IMAGER_PIXEL_SPACING)
            .or_else(|| read_pair(dataset, Tag::PIXEL_SPACING))
            .or_else(|| read_pair(dataset, Tag::NOMINAL_SCANNED_PIXEL_SPACING))
            .or_else(|| read_pair(&functional, Tag::PIXEL_SPACING))
            .or_else(|| read_pair(&functional, Tag::IMAGER_PIXEL_SPACING))
            .unwrap_or((0.0, 0.0));

        let position = read_vector(dataset, Tag::IMAGE_POSITION_PATIENT)
            .or_else(|| read_vector(&functional, Tag::IMAGE_POSITION_PATIENT));
        let orientation = read_orientation(dataset, Tag::IMAGE_ORIENTATION_PATIENT)
            .or_else(|| read_orientation(&functional, Tag::IMAGE_ORIENTATION_PATIENT));

        Ok(Self::build(
            frame_of_reference_uid,
            width,
            height,
            spacing_between_rows,
            spacing_between_columns,
            position,
            orientation,
        ))
    }

    /// Assembles the geometry from raw values (the fo-dicom second constructor +
    /// InitializeCalculatedVolumeData + transformations).
    pub fn build(
        frame_of_reference_uid: String,
        width: u32,
        height: u32,
        spacing_between_rows: f64,
        spacing_between_columns: f64,
        position: Option<[f64; 3]>,
        orientation: Option<[f64; 6]>,
    ) -> Self {
        let mut geometry = FrameGeometry {
            geometry_type: FrameGeometryType::None,
            frame_of_reference_uid,
            direction_row: Vector3::AXIS_X,
            direction_column: Vector3::AXIS_Y,
            direction_normal: Vector3::ZERO,
            width,
            height,
            spacing_between_rows,
            spacing_between_columns,
            point_top_left: Point3::ORIGIN,
            point_top_right: Point3::ORIGIN,
            point_bottom_left: Point3::ORIGIN,
            point_bottom_right: Point3::ORIGIN,
            orientation: FrameOrientation::None,
        };

        match (position, orientation) {
            (Some(p), Some(o)) => {
                geometry.point_top_left = Point3::new(p[0], p[1], p[2]);
                geometry.direction_row = Vector3::new(o[0], o[1], o[2]);
                geometry.direction_column = Vector3::new(o[3], o[4], o[5]);
                geometry.direction_normal = geometry.direction_row.cross(&geometry.direction_column);
                geometry.orientation = if geometry.direction_normal.is_zero() {
                    FrameOrientation::None
                } else {
                    classify(geometry.direction_normal)
                };
            }
            _ => {
                // incomplete data: 2D defaults, no volume (fo-dicom #2067)
                geometry.orientation = FrameOrientation::None;
                geometry.direction_row = Vector3::AXIS_X;
                geometry.direction_column = Vector3::AXIS_Y;
                geometry.direction_normal = Vector3::ZERO;
            }
        }

        geometry.point_top_right = add_scaled(
            geometry.point_top_left,
            geometry.direction_row,
            geometry.spacing_between_columns * width as f64,
        );
        geometry.point_bottom_left = add_scaled(
            geometry.point_top_left,
            geometry.direction_column,
            geometry.spacing_between_rows * height as f64,
        );
        geometry.point_bottom_right = Point3::new(
            geometry.point_bottom_left.x + (geometry.point_top_right.x - geometry.point_top_left.x),
            geometry.point_bottom_left.y + (geometry.point_top_right.y - geometry.point_top_left.y),
            geometry.point_bottom_left.z + (geometry.point_top_right.z - geometry.point_top_left.z),
        );

        geometry.geometry_type = geometry.compute_geometry_type();
        geometry
    }

    fn compute_geometry_type(&self) -> FrameGeometryType {
        if nearly_zero(self.spacing_between_columns) || nearly_zero(self.spacing_between_rows) {
            return FrameGeometryType::None;
        }
        if self.direction_normal.is_zero()
            && self.point_top_left == Point3::ORIGIN
            && self.direction_row == Vector3::AXIS_X
            && self.direction_column == Vector3::AXIS_Y
        {
            return FrameGeometryType::Plane;
        }
        FrameGeometryType::Volume
    }

    pub fn geometry_type(&self) -> FrameGeometryType {
        self.geometry_type
    }

    pub fn has_geometry_data(&self) -> bool {
        self.geometry_type != FrameGeometryType::None
    }

    /// Frame width in pixels (columns).
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels (rows).
    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn orientation(&self) -> FrameOrientation {
        self.orientation
    }

    pub fn frame_of_reference_uid(&self) -> &str {
        &self.frame_of_reference_uid
    }

    pub fn direction_row(&self) -> Vector3 {
        self.direction_row
    }

    pub fn direction_column(&self) -> Vector3 {
        self.direction_column
    }

    pub fn direction_normal(&self) -> Vector3 {
        self.direction_normal
    }

    pub fn point_top_left(&self) -> Point3 {
        self.point_top_left
    }

    pub fn point_top_right(&self) -> Point3 {
        self.point_top_right
    }

    pub fn point_bottom_left(&self) -> Point3 {
        self.point_bottom_left
    }

    pub fn point_bottom_right(&self) -> Point3 {
        self.point_bottom_right
    }

    /// Column spacing in mm (0028,0030 second value), 0 when absent.
    pub fn spacing_between_columns(&self) -> f64 {
        self.spacing_between_columns
    }

    /// Row spacing in mm (0028,0030 first value), 0 when absent.
    pub fn spacing_between_rows(&self) -> f64 {
        self.spacing_between_rows
    }

    /// The patient-space axis-aligned bounding box of this frame's four
    /// corners, as `(min, max)` (fo-dicom `FrameGeometry.GetBoundingBox`).
    pub fn bounding_box(&self) -> (Point3, Point3) {
        let corners = [
            self.point_top_left,
            self.point_top_right,
            self.point_bottom_left,
            self.point_bottom_right,
        ];
        let min = Point3::new(
            corners.iter().map(|p| p.x).fold(f64::INFINITY, f64::min),
            corners.iter().map(|p| p.y).fold(f64::INFINITY, f64::min),
            corners.iter().map(|p| p.z).fold(f64::INFINITY, f64::min),
        );
        let max = Point3::new(
            corners.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max),
            corners.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max),
            corners.iter().map(|p| p.z).fold(f64::NEG_INFINITY, f64::max),
        );
        (min, max)
    }

    /// Maps an image (column, row) point to patient space (fo-dicom
    /// `TransformImagePointToPatient`). Errors when there is no geometry.
    pub fn transform_image_to_patient(&self, image: Point2) -> Result<Point3, Error> {
        if self.geometry_type == FrameGeometryType::None {
            return Err(no_geometry());
        }
        let col = self.spacing_between_columns;
        let row = self.spacing_between_rows;
        Ok(Point3::new(
            self.point_top_left.x + image.x * col * self.direction_row.x + image.y * row * self.direction_column.x,
            self.point_top_left.y + image.x * col * self.direction_row.y + image.y * row * self.direction_column.y,
            self.point_top_left.z + image.x * col * self.direction_row.z + image.y * row * self.direction_column.z,
        ))
    }

    /// Maps a patient point back to image (column, row) space, projecting onto
    /// the frame plane (fo-dicom `TransformPatientPointToImage`).
    pub fn transform_patient_to_image(&self, patient: Point3) -> Result<Point2, Error> {
        if self.geometry_type == FrameGeometryType::None {
            return Err(no_geometry());
        }
        // offset from the top-left corner, in patient space
        let d = Point3::new(
            patient.x - self.point_top_left.x,
            patient.y - self.point_top_left.y,
            patient.z - self.point_top_left.z,
        );
        let dv = Vector3::new(d.x, d.y, d.z);
        // project onto the (orthonormal, when present) row/column directions
        let row_component = dv.dot(&self.direction_row);
        let col_component = dv.dot(&self.direction_column);
        let column = if nearly_zero(self.spacing_between_columns) {
            0.0
        } else {
            row_component / self.spacing_between_columns
        };
        let row = if nearly_zero(self.spacing_between_rows) {
            0.0
        } else {
            col_component / self.spacing_between_rows
        };
        Ok(Point2::new(column, row))
    }
}

/// True when two frames may display each other's localizer (fo-dicom
/// `ImageLocalizer.CanDrawLocalizer`): valid 3D geometry, different orientations
/// and the same frame of reference.
pub fn can_draw_localizer(source: &FrameGeometry, destination: &FrameGeometry) -> bool {
    if source.orientation == FrameOrientation::None || destination.orientation == FrameOrientation::None {
        return false;
    }
    if source.orientation == destination.orientation {
        return false;
    }
    if source.frame_of_reference_uid.is_empty() || destination.frame_of_reference_uid.is_empty() {
        return false;
    }
    source.frame_of_reference_uid == destination.frame_of_reference_uid
}

/// Projects the four corners of `source` onto `destination`'s image plane and
/// returns them as pixel points (fo-dicom `CalculateProjectionLocalizer`).
///
/// Returns an empty vector when either frame lacks 3D geometry. The four points
/// form the projected rectangle; for orthogonal frames it collapses to a line.
pub fn projection_localizer(source: &FrameGeometry, destination: &FrameGeometry) -> Vec<Point2> {
    if !source.has_geometry_data() || !destination.has_geometry_data() || destination.direction_normal.is_zero() {
        return Vec::new();
    }
    // the four source corners in patient space (TLHC, TRHC, BRHC, BLHC)
    let corners = [
        source.point_top_left,
        source.point_top_right,
        source.point_bottom_right,
        source.point_bottom_left,
    ];
    let mut out = Vec::with_capacity(4);
    for corner in corners {
        // rotate the corner (relative to the destination origin) into the
        // destination's row/column frame, then divide by spacing
        let d = Vector3::new(
            corner.x - destination.point_top_left.x,
            corner.y - destination.point_top_left.y,
            corner.z - destination.point_top_left.z,
        );
        let col = if nearly_zero(destination.spacing_between_columns) {
            0.0
        } else {
            d.dot(&destination.direction_row) / destination.spacing_between_columns
        };
        let row = if nearly_zero(destination.spacing_between_rows) {
            0.0
        } else {
            d.dot(&destination.direction_column) / destination.spacing_between_rows
        };
        out.push(Point2::new(col.round(), row.round()));
    }
    out
}

/// Computes the line where the `source` plane crosses the `destination` plane,
/// in destination pixel space (fo-dicom `CalculateIntersectionLocalizer`).
///
/// Returns `None` when the planes do not cross the source rectangle at exactly
/// two edge points (no, parallel, or tangential intersection).
pub fn intersection_localizer(source: &FrameGeometry, destination: &FrameGeometry) -> Option<(Point2, Point2)> {
    if destination.direction_normal.is_zero() {
        return None;
    }
    let normal = &destination.direction_normal;
    let to_vec = |p: Point3| Vector3::new(p.x, p.y, p.z);

    // signed distance of each source corner from the destination plane
    let n_p = normal.dot(&to_vec(destination.point_top_left));
    let n_a = normal.dot(&to_vec(source.point_top_left));
    let n_b = normal.dot(&to_vec(source.point_top_right));
    let n_c = normal.dot(&to_vec(source.point_bottom_right));
    let n_d = normal.dot(&to_vec(source.point_bottom_left));

    let mut crossings: Vec<Point3> = Vec::with_capacity(2);
    // walk the four edges, recording an in-range crossing on each
    let edges = [
        (source.point_top_left, source.point_top_right, n_a, n_b),
        (source.point_top_right, source.point_bottom_right, n_b, n_c),
        (source.point_bottom_right, source.point_bottom_left, n_c, n_d),
        (source.point_bottom_left, source.point_top_left, n_d, n_a),
    ];
    for (from, to, na, nb) in edges {
        if (nb - na).abs() > 1e-6 {
            let t = (n_p - na) / (nb - na);
            if t > 0.0 && t <= 1.0 {
                crossings.push(Point3::new(
                    from.x + t * (to.x - from.x),
                    from.y + t * (to.y - from.y),
                    from.z + t * (to.z - from.z),
                ));
            }
        }
    }
    if crossings.len() != 2 {
        return None;
    }
    let start = destination.transform_patient_to_image(crossings[0]).ok()?;
    let end = destination.transform_patient_to_image(crossings[1]).ok()?;
    Some((Point2::new(start.x.round(), start.y.round()), Point2::new(end.x.round(), end.y.round())))
}

/// Classifies a normal vector by its nearest axis (fo-dicom `NearestAxis`):
/// X→Sagittal, Y→Coronal, Z→Axial.
fn classify(normal: Vector3) -> FrameOrientation {
    let n = normal.normalize();
    let (ax, ay, az) = (n.x.abs(), n.y.abs(), n.z.abs());
    if ax >= ay && ax >= az {
        FrameOrientation::Sagittal
    } else if ay >= az {
        FrameOrientation::Coronal
    } else {
        FrameOrientation::Axial
    }
}

fn add_scaled(base: Point3, dir: Vector3, scale: f64) -> Point3 {
    Point3::new(base.x + dir.x * scale, base.y + dir.y * scale, base.z + dir.z * scale)
}

fn nearly_zero(v: f64) -> bool {
    v.abs() <= 1e-9
}

fn no_geometry() -> Error {
    Error::Data(DataError::Other {
        message: "cannot transform a point without geometry data".into(),
    })
}

/// Flattens the shared `(5200,9229)` and per-frame `(5200,9230)` functional
/// groups for `frame` into a plain dataset, mirroring fo-dicom
/// `FunctionalGroupValues`: each functional-group macro's first item is expanded
/// one level (so Pixel Measures `(0028,9110)` contributes its PixelSpacing),
/// Shared first then Per-Frame so per-frame values win.
fn functional_group_values(dataset: &Dataset, frame: u32) -> Dataset {
    fn absorb(out: &mut Dataset, group_item: &Dataset) {
        for item in group_item.items() {
            match item {
                DicomItem::Sequence(seq) => {
                    if let Some(first) = seq.items().first() {
                        for inner in first.items() {
                            out.insert_or_update(inner.clone());
                        }
                    }
                }
                other => {
                    out.insert_or_update(other.clone());
                }
            }
        }
    }
    let mut out = Dataset::new();
    if let Some(shared) = dataset.try_get_sequence(Tag::SHARED_FUNCTIONAL_GROUPS_SEQUENCE) {
        if let Some(item) = shared.items().first() {
            absorb(&mut out, item);
        }
    }
    if let Some(per_frame) = dataset.try_get_sequence(Tag::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE) {
        if let Some(item) = per_frame.items().get(frame as usize) {
            absorb(&mut out, item);
        }
    }
    out
}

/// Reads a two-value DS attribute (e.g. PixelSpacing) into `(first, second)`.
fn read_pair(dataset: &Dataset, tag: Tag) -> Option<(f64, f64)> {
    let text = dataset.try_get_str(tag)?;
    let mut parts = text.split('\\');
    let first = parts.next()?.trim().parse::<f64>().ok()?;
    let second = parts.next()?.trim().parse::<f64>().ok()?;
    Some((first, second))
}

/// Reads three DS values (ImagePositionPatient).
fn read_vector(dataset: &Dataset, tag: Tag) -> Option<[f64; 3]> {
    let text = dataset.try_get_str(tag)?;
    let values: Vec<f64> = text.split('\\').filter_map(|p| p.trim().parse::<f64>().ok()).collect();
    if values.len() >= 3 {
        Some([values[0], values[1], values[2]])
    } else {
        None
    }
}

/// Reads six DS values (ImageOrientationPatient).
fn read_orientation(dataset: &Dataset, tag: Tag) -> Option<[f64; 6]> {
    let text = dataset.try_get_str(tag)?;
    let values: Vec<f64> = text.split('\\').filter_map(|p| p.trim().parse::<f64>().ok()).collect();
    if values.len() >= 6 {
        Some([values[0], values[1], values[2], values[3], values[4], values[5]])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vr::Vr;

    fn axial_dataset() -> Dataset {
        // classic axial CT: rows along patient X, columns along patient Y, normal = +Z
        let mut ds = Dataset::new();
        ds.insert_value(Tag::ROWS, 512u16); // rows
        ds.insert_value(Tag::COLUMNS, 512u16); // columns
        ds.insert_str(Tag::PIXEL_SPACING, Vr::Ds, "0.5\\0.5"); // PixelSpacing (row, col)
        ds.insert_str(Tag::IMAGE_POSITION_PATIENT, Vr::Ds, "-100\\-100\\50"); // position
        ds.insert_str(Tag::IMAGE_ORIENTATION_PATIENT, Vr::Ds, "1\\0\\0\\0\\1\\0"); // orientation
        ds.insert_str(Tag::FRAME_OF_REFERENCE_UID, Vr::Ui, "1.2.3.4");
        ds
    }

    #[test]
    fn axial_geometry_classifies_its_orientation() {
        let geometry = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        assert_eq!(geometry.orientation(), FrameOrientation::Axial);
        assert_eq!(geometry.geometry_type(), FrameGeometryType::Volume);
        // normal = X̂ × Ŷ = Ẑ
        let normal = geometry.direction_normal();
        assert!((normal.z - 1.0).abs() < 1e-9);
    }

    #[test]
    fn sagittal_and_coronal_classification() {
        let sag = FrameGeometry::build(String::new(), 1, 1, 1.0, 1.0, Some([0.0; 3]), Some([0.0, 1.0, 0.0, 0.0, 0.0, 1.0]));
        // normal = Ŷ × Ẑ = X̂ → Sagittal
        assert_eq!(sag.orientation(), FrameOrientation::Sagittal);
        let cor = FrameGeometry::build(String::new(), 1, 1, 1.0, 1.0, Some([0.0; 3]), Some([1.0, 0.0, 0.0, 0.0, 0.0, 1.0]));
        // normal = X̂ × Ẑ = -Ŷ → Coronal
        assert_eq!(cor.orientation(), FrameOrientation::Coronal);
    }

    #[test]
    fn corners_are_derived_from_origin_directions_and_spacing() {
        let geometry = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        let tl = geometry.point_top_left();
        assert!((tl.x + 100.0).abs() < 1e-9 && (tl.z - 50.0).abs() < 1e-9);
        // TR = TL + rowDir * colSpacing * width = +0.5*512 along X
        let tr = geometry.point_top_right();
        assert!((tr.x - (-100.0 + 0.5 * 512.0)).abs() < 1e-6, "TR x = {tr_x}", tr_x = tr.x);
        // BL = TL + colDir * rowSpacing * height = +0.5*512 along Y
        let bl = geometry.point_bottom_left();
        assert!((bl.y - (-100.0 + 0.5 * 512.0)).abs() < 1e-6);
    }

    #[test]
    fn image_to_patient_transform_round_trips() {
        let geometry = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        let image_point = Point2::new(10.0, 20.0);
        let patient = geometry.transform_image_to_patient(image_point).unwrap();
        let back = geometry.transform_patient_to_image(patient).unwrap();
        assert!((back.x - image_point.x).abs() < 1e-6, "column round-trips");
        assert!((back.y - image_point.y).abs() < 1e-6, "row round-trips");
    }

    #[test]
    fn a_classic_2d_frame_without_position_is_a_plane() {
        // spacing present, orientation/position absent → default axes, Plane
        let geometry = FrameGeometry::build(String::new(), 100, 100, 0.5, 0.5, None, None);
        assert_eq!(geometry.orientation(), FrameOrientation::None);
        assert_eq!(geometry.geometry_type(), FrameGeometryType::Plane);
    }

    #[test]
    fn missing_spacing_yields_no_geometry_and_transform_errors() {
        let geometry = FrameGeometry::build(String::new(), 10, 10, 0.0, 0.0, Some([0.0; 3]), Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]));
        assert!(!geometry.has_geometry_data());
        assert!(geometry.transform_image_to_patient(Point2::new(0.0, 0.0)).is_err());
    }

    #[test]
    fn localizer_is_drawable_only_across_orientations_in_one_frame_of_reference() {
        let axial = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        let mut sagittal_ds = axial_dataset();
        sagittal_ds.insert_str(Tag::IMAGE_ORIENTATION_PATIENT, Vr::Ds, "0\\1\\0\\0\\0\\1");
        let sagittal = FrameGeometry::from_dataset(&sagittal_ds).unwrap();

        assert!(can_draw_localizer(&axial, &sagittal), "different orientations, same frame of reference");

        // same orientation → not drawn
        assert!(!can_draw_localizer(&axial, &axial), "parallel planes get no localizer");

        // different frame of reference → not drawn
        let mut other = sagittal.clone();
        other.frame_of_reference_uid = "9.9.9".to_string();
        assert!(!can_draw_localizer(&axial, &other));
    }

    /// A sagittal frame centered so its Z extent brackets the axial plane (z=50),
    /// spanning Y (rows) and Z (columns), same frame of reference as the axial helper.
    fn sagittal_at_origin() -> FrameGeometry {
        FrameGeometry::build(
            "1.2.3.4".to_string(),
            512,
            512,
            0.5,
            0.5,
            Some([0.0, -100.0, -100.0]), // z range -100..156 brackets the axial plane z=50
            Some([0.0, 1.0, 0.0, 0.0, 0.0, 1.0]), // row=+Y, col=+Z, normal=+X
        )
    }

    #[test]
    fn projection_localizer_returns_four_points() {
        let axial = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        let sagittal = sagittal_at_origin();
        let points = projection_localizer(&sagittal, &axial);
        assert_eq!(points.len(), 4, "the four projected corners are returned");
    }

    #[test]
    fn projection_localizer_is_empty_without_geometry() {
        let axial = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        let flat = FrameGeometry::build(String::new(), 4, 4, 0.0, 0.0, None, None); // no spacing → no geometry
        assert!(projection_localizer(&flat, &axial).is_empty());
        assert!(projection_localizer(&axial, &flat).is_empty());
    }

    #[test]
    fn intersection_localizer_finds_the_crossing_line() {
        // The sagittal frame sits at patient x = 0; the axial frame spans x in
        // [-100, 156], so the plane x=0 crosses it → a two-point line.
        let axial = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        let sagittal = sagittal_at_origin();
        let line = intersection_localizer(&sagittal, &axial).expect("the planes cross the source rectangle");
        assert_ne!(line.0, line.1, "the crossing is a real segment, not one point");
    }

    #[test]
    fn intersection_localizer_returns_none_for_parallel_planes() {
        // two axial planes (parallel) never cross
        let a = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        let b = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        assert!(intersection_localizer(&a, &b).is_none());
    }

    #[test]
    fn intersection_localizer_returns_none_when_the_frame_is_missed() {
        // move the sagittal plane far outside the axial image (x = 10000)
        let axial = FrameGeometry::from_dataset(&axial_dataset()).unwrap();
        let far = FrameGeometry::build(
            "1.2.3.4".to_string(),
            512,
            512,
            0.5,
            0.5,
            Some([10000.0, -100.0, 50.0]),
            Some([0.0, 1.0, 0.0, 0.0, 0.0, 1.0]),
        );
        assert!(intersection_localizer(&far, &axial).is_none(), "the plane never crosses the source rectangle");
    }

    #[test]
    fn nominal_scanned_pixel_spacing_is_the_spacing_fallback() {
        // (0018,2010) is used when neither PixelSpacing nor ImagerPixelSpacing is
        // present (fo-dicom FrameGeometry.cs, third branch).
        let mut ds = Dataset::new();
        ds.insert_value(Tag::ROWS, 2u16);
        ds.insert_value(Tag::COLUMNS, 2u16);
        ds.insert_str(Tag::NOMINAL_SCANNED_PIXEL_SPACING, Vr::Ds, "0.3\\0.4");
        let geometry = FrameGeometry::from_dataset(&ds).unwrap();
        assert_eq!(geometry.spacing_between_rows(), 0.3, "rows from the first value");
        assert_eq!(geometry.spacing_between_columns(), 0.4, "columns from the second value");
    }

    #[test]
    fn enhanced_per_frame_pixel_measures_give_each_frame_its_spacing() {
        // (5200,9230) per-frame item → Pixel Measures (0028,9110) → PixelSpacing
        let mut ds = Dataset::new();
        ds.insert_value(Tag::ROWS, 2u16);
        ds.insert_value(Tag::COLUMNS, 2u16);
        let per_frame = ds.insert_sequence(Tag::PER_FRAME_FUNCTIONAL_GROUPS_SEQUENCE);
        for spacing in ["0.5\\0.6", "0.7\\0.8"] {
            let mut frame_item = Dataset::new();
            let mut measures = Dataset::new();
            measures.insert_str(Tag::PIXEL_SPACING, Vr::Ds, spacing);
            frame_item.insert_sequence(Tag::PIXEL_MEASURES_SEQUENCE).push(measures);
            per_frame.push(frame_item);
        }
        let f0 = FrameGeometry::from_dataset_frame(&ds, 0).unwrap();
        assert_eq!((f0.spacing_between_rows(), f0.spacing_between_columns()), (0.5, 0.6));
        let f1 = FrameGeometry::from_dataset_frame(&ds, 1).unwrap();
        assert_eq!((f1.spacing_between_rows(), f1.spacing_between_columns()), (0.7, 0.8), "frame 1 uses its own Pixel Measures");
    }
}
