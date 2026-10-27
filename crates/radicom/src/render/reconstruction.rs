//! Multi-planar reconstruction core (REND-09; docs/10-rendering.md §9).
//!
//! Stacks native grayscale slices into a [`VolumeData`] and resamples an
//! arbitrary plane through it with trilinear interpolation (bilinear in-plane,
//! linear between the two bracketing slices), producing a [`Slice`]. This mirrors
//! fo-dicom `Imaging/Reconstruction` (`ImageData`, `VolumeData.GetCut`, `Slice`)
//! for the common native MONOCHROME case.
//!
//! Scope: `get_cut`/`Slice`, `VolumeData::from_datasets`, `Stack` spacing
//! resampling and `DicomGenerator` series write-back are all implemented here.

use crate::error::{DataError, Error};
use crate::model::dataset::Dataset;
use crate::model::tag::Tag;
use crate::model::uid::Uid;
use crate::model::vr::Vr;
use crate::pixel::PixelData;
use crate::render::geometry::FrameGeometry;
use crate::render::math::{Point2, Point3, Vector3};

/// One source slice: its dataset, geometry and decoded native grayscale samples.
#[derive(Debug, Clone)]
pub struct ImageData {
    geometry: FrameGeometry,
    /// Row-major stored grayscale values (already rescaled by slope/intercept).
    pixels: Vec<f64>,
    width: u32,
    height: u32,
    instance_number: i32,
    /// Ordering key along the slice normal: `normal · top_left`.
    sorting_value: f64,
}

impl ImageData {
    /// Builds an [`ImageData`] from a dataset holding one native MONOCHROME frame.
    pub fn from_dataset(dataset: &Dataset) -> Result<ImageData, Error> {
        let geometry = FrameGeometry::from_dataset(dataset)?;
        if !geometry.has_geometry_data() {
            return Err(invalid("slice has no geometry (needs orientation + position)"));
        }
        let pixels = read_grayscale(dataset)?;
        let width = geometry.width();
        let height = geometry.height();
        if pixels.len() as u32 != width * height {
            return Err(invalid("pixel count does not match width*height"));
        }
        let instance_number = dataset.try_get::<i32>(crate::model::tag::Tag::new(0x0020, 0x0013)).unwrap_or(0);
        let sorting_value = dot(&geometry.direction_normal(), &point_to_vector(&geometry.point_top_left()));
        Ok(ImageData { geometry, pixels, width, height, instance_number, sorting_value })
    }

    /// The slice geometry.
    pub fn geometry(&self) -> &FrameGeometry {
        &self.geometry
    }

    /// The slice's frame of reference UID.
    pub fn frame_of_reference_uid(&self) -> &str {
        self.geometry.frame_of_reference_uid()
    }

    /// The slice's InstanceNumber (0 when absent).
    pub fn instance_number(&self) -> i32 {
        self.instance_number
    }

    /// The ordering value along the slice normal.
    pub fn sorting_value(&self) -> f64 {
        self.sorting_value
    }

    fn pixel(&self, x: u32, y: u32) -> f64 {
        self.pixels[(y * self.width + x) as usize]
    }

    /// Bilinear interpolation at a fractional image-space point, or `None` when
    /// the point falls outside the sample grid (fo-dicom `Interpolate`).
    fn interpolate(&self, image: Point2) -> Option<f64> {
        let (x, y) = (image.x, image.y);
        if !(x >= 0.0 && x < self.width as f64 - 1.0 && y >= 0.0 && y < self.height as f64 - 1.0) {
            return None;
        }
        let px = x.floor();
        let ax = x - px;
        let py = y.floor();
        let ay = y - py;
        let (px, py) = (px as u32, py as u32);
        Some(
            (1.0 - ax) * ((1.0 - ay) * self.pixel(px, py) + ay * self.pixel(px, py + 1))
                + ax * ((1.0 - ay) * self.pixel(px + 1, py) + ay * self.pixel(px + 1, py + 1)),
        )
    }
}

/// A stack of coplanar source slices for arbitrary-plane resampling.
#[derive(Debug, Clone)]
pub struct VolumeData {
    slices: Vec<ImageData>,
    sort_orders: Vec<f64>,
    slices_normal: Vector3,
    bounding_min: Point3,
    bounding_max: Point3,
}

impl VolumeData {
    /// Stacks datasets into a volume (fo-dicom `VolumeData(IEnumerable<ImageData>)`).
    ///
    /// All slices must share one frame of reference; at least one non-empty
    /// group of slices is required. Slices are sorted along the normal.
    pub fn from_datasets(datasets: &[Dataset]) -> Result<VolumeData, Error> {
        let mut images: Vec<ImageData> = Vec::new();
        for dataset in datasets {
            if let Ok(image) = ImageData::from_dataset(dataset) {
                images.push(image);
            }
        }
        if images.is_empty() {
            return Err(invalid("no slices with usable geometry"));
        }
        let first_uid = images[0].frame_of_reference_uid().to_string();
        if images.iter().any(|i| i.frame_of_reference_uid() != first_uid) {
            return Err(invalid("slices are mixed up from different stacks"));
        }

        images.sort_by(|a, b| a.sorting_value.partial_cmp(&b.sorting_value).unwrap());
        let sort_orders: Vec<f64> = images.iter().map(|i| i.sorting_value()).collect();
        let slices_normal = images[0].geometry.direction_normal();

        // union of every slice's bounding box
        let mut bounding_min = Point3::new(f64::INFINITY, f64::INFINITY, f64::INFINITY);
        let mut bounding_max = Point3::new(f64::NEG_INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for image in &images {
            let (min, max) = image.geometry.bounding_box();
            bounding_min = Point3::new(
                bounding_min.x.min(min.x),
                bounding_min.y.min(min.y),
                bounding_min.z.min(min.z),
            );
            bounding_max = Point3::new(
                bounding_max.x.max(max.x),
                bounding_max.y.max(max.y),
                bounding_max.z.max(max.z),
            );
        }

        Ok(VolumeData { slices: images, sort_orders, slices_normal, bounding_min, bounding_max })
    }

    /// Volume bounding-box minimum corner (patient space).
    pub fn bounding_min(&self) -> Point3 {
        self.bounding_min
    }

    /// Volume bounding-box maximum corner (patient space).
    pub fn bounding_max(&self) -> Point3 {
        self.bounding_max
    }

    /// Number of slices in the volume.
    pub fn len(&self) -> usize {
        self.slices.len()
    }

    /// True when the volume has no slices.
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }

    /// Resamples an arbitrary plane and returns a [`Slice`] (fo-dicom `GetCut`).
    ///
    /// `top_left` is the patient-space start corner, `row_dir`/`col_dir` the unit
    /// in-plane directions, and `spacing` the mm step in both directions.
    pub fn get_cut(
        &self,
        top_left: Point3,
        row_dir: Vector3,
        col_dir: Vector3,
        rows: u32,
        cols: u32,
        spacing: f64,
    ) -> Slice {
        let mut output = vec![0.0f64; (rows * cols) as usize];

        let delta_x = scale(&row_dir, spacing);
        let delta_y = scale(&col_dir, spacing);
        let ordered_delta_x = dot(&self.slices_normal, &delta_x);
        let ordered_delta_y = dot(&self.slices_normal, &delta_y);
        let ordered_row_start = dot(&self.slices_normal, &point_to_vector(&top_left));

        for x in 0..cols {
            let point_x = Point3::new(
                top_left.x + delta_x.x * x as f64,
                top_left.y + delta_x.y * x as f64,
                top_left.z + delta_x.z * x as f64,
            );
            let mut point = point_x;
            let mut ordered = ordered_row_start + ordered_delta_x * x as f64;
            let mut last_index = 0usize;
            for y in 0..rows {
                if let Some(index) = self.sorting_index(ordered, last_index) {
                    let index = index.max(1);
                    last_index = index;
                    let next_slice = &self.slices[index];
                    let prev_slice = &self.slices[index - 1];
                    if let (Ok(next_img), Ok(prev_img)) = (
                        next_slice.geometry.transform_patient_to_image(point),
                        prev_slice.geometry.transform_patient_to_image(point),
                    ) {
                        if let (Some(next_pixel), Some(prev_pixel)) =
                            (next_slice.interpolate(next_img), prev_slice.interpolate(prev_img))
                        {
                            let alpha1 = next_slice.sorting_value() - ordered;
                            let alpha2 = ordered - prev_slice.sorting_value();
                            if alpha1 + alpha2 != 0.0 {
                                output[(x + y * cols) as usize] =
                                    (prev_pixel * alpha1 + next_pixel * alpha2) / (alpha1 + alpha2);
                            }
                        }
                    }
                }
                point = Point3::new(point.x + delta_y.x, point.y + delta_y.y, point.z + delta_y.z);
                ordered += ordered_delta_y;
            }
        }

        Slice {
            top_left,
            row_direction: row_dir,
            column_direction: col_dir,
            rows,
            columns: cols,
            spacing,
            output,
        }
    }

    /// First slice index whose sorting value is `>= value` starting the search at
    /// `guess` (fo-dicom `SortingIndex`), or `None` when nothing is large enough.
    fn sorting_index(&self, value: f64, guess: usize) -> Option<usize> {
        let len = self.sort_orders.len();
        let mut guess = guess.min(len.saturating_sub(1));
        while self.sort_orders[guess] >= value && guess > 0 {
            guess -= 1;
        }
        while guess < len {
            if self.sort_orders[guess] >= value {
                return Some(guess);
            }
            guess += 1;
        }
        None
    }
}

/// A calculated cut through a volume (fo-dicom `Slice`).
#[derive(Debug, Clone)]
pub struct Slice {
    top_left: Point3,
    row_direction: Vector3,
    column_direction: Vector3,
    rows: u32,
    columns: u32,
    spacing: f64,
    output: Vec<f64>,
}

impl Slice {
    /// The cut's top-left patient corner.
    pub fn top_left(&self) -> Point3 {
        self.top_left
    }
    /// The cut's row direction.
    pub fn row_direction(&self) -> Vector3 {
        self.row_direction
    }
    /// The cut's column direction.
    pub fn column_direction(&self) -> Vector3 {
        self.column_direction
    }
    /// The cut's row count.
    pub fn rows(&self) -> u32 {
        self.rows
    }
    /// The cut's column count.
    pub fn columns(&self) -> u32 {
        self.columns
    }
    /// The cut's mm spacing.
    pub fn spacing(&self) -> f64 {
        self.spacing
    }

    /// The resampled sample values, row-major (`columns` per row).
    pub fn pixels(&self) -> &[f64] {
        &self.output
    }

    /// The `(min, max)` of the sampled values (fo-dicom `GetMinMaxValue`).
    pub fn min_max(&self) -> (f64, f64) {
        let min = self.output.iter().copied().fold(f64::INFINITY, f64::min);
        let max = self.output.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (min, max)
    }

    /// Renders the cut to an 8-bit buffer, mapping `[min, max]` across `0..=255`
    /// (fo-dicom `RenderIntoByteArray`, without the source LUT).
    pub fn render_into_byte_array(&self) -> Vec<u8> {
        let (min, max) = self.min_max();
        let range = (max - min).max(f64::EPSILON);
        self.output
            .iter()
            .map(|&v| (((v - min) / range) * 255.0).round().clamp(0.0, 255.0) as u8)
            .collect()
    }

    /// Renders the raw sample values as little-endian bytes of `bytes_per_pixel`
    /// width (fo-dicom `RenderRawData`): 1 byte truncates, 2 bytes little-endian.
    pub fn render_raw_data(&self, bytes_per_pixel: u8) -> Vec<u8> {
        match bytes_per_pixel {
            1 => self.output.iter().map(|&v| v.round() as u8).collect(),
            2 => {
                let mut out = Vec::with_capacity(self.output.len() * 2);
                for &v in &self.output {
                    out.extend_from_slice(&(v.round() as u16).to_le_bytes());
                }
                out
            }
            _ => Vec::new(),
        }
    }
}

/// Reads native MONOCHROME samples, rescaled by RescaleSlope/Intercept (0028,1053/1052).
fn read_grayscale(dataset: &Dataset) -> Result<Vec<f64>, Error> {
    let mut owned = dataset.clone();
    let pixel = PixelData::from_dataset(&mut owned)?;
    let slots = (pixel.width() * pixel.height() * pixel.samples_per_pixel() as u32) as usize;
    let frame = pixel.get_frame(0)?;
    let bytes = frame.data();
    let bytes = bytes.as_ref();
    let bits = pixel.bits_allocated()?;
    let signed = pixel.pixel_representation() == 1;

    let raw: Vec<f64> = match (bits, signed) {
        (8, false) => bytes.iter().take(slots).map(|&b| b as f64).collect(),
        (8, true) => bytes.iter().take(slots).map(|&b| (b as i8) as f64).collect(),
        (16, false) => bytes
            .chunks_exact(2)
            .take(slots)
            .map(|c| u16::from_le_bytes([c[0], c[1]]) as f64)
            .collect(),
        (16, true) => bytes
            .chunks_exact(2)
            .take(slots)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64)
            .collect(),
        _ => return Err(invalid("MPR supports 8- or 16-bit native MONOCHROME data")),
    };

    // apply rescale slope/intercept so stacked slices are comparable
    let slope = dataset
        .try_get_str(crate::model::tag::Tag::new(0x0028, 0x1053))
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(1.0);
    let intercept = dataset
        .try_get_str(crate::model::tag::Tag::new(0x0028, 0x1052))
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    Ok(raw.into_iter().map(|v| v * slope + intercept).collect())
}

fn dot(a: &Vector3, b: &Vector3) -> f64 {
    a.dot(b)
}

fn scale(v: &Vector3, factor: f64) -> Vector3 {
    Vector3::new(v.x * factor, v.y * factor, v.z * factor)
}

fn point_to_vector(p: &Point3) -> Vector3 {
    Vector3::new(p.x, p.y, p.z)
}

fn invalid(message: &str) -> Error {
    Error::Data(DataError::Other { message: format!("MPR: {message}") })
}

/// The orthogonal plane a new [`Stack`] is cut along (fo-dicom `StackType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackType {
    Axial,
    Coronal,
    Sagittal,
}

/// A regular stack of slices resampled from a volume along a `StackType` axis
/// (fo-dicom `Stack`).
#[derive(Debug, Clone)]
pub struct Stack {
    slices: Vec<Slice>,
    slice_distance: f64,
}

impl Stack {
    /// Cuts a new stack out of `volume` at `spacing` in-plane and `slice_distance`
    /// between slices.
    pub fn new(volume: &VolumeData, stack_type: StackType, spacing: f64, slice_distance: f64) -> Stack {
        let min = volume.bounding_min();
        let max = volume.bounding_max();
        let (dx, dy, dz) = (max.x - min.x, max.y - min.y, max.z - min.z);
        let top_left0 = Point3::new(min.x, min.y, max.z);

        // (per-slice step, row dir, col dir, rows, cols, axis extent)
        let (step, row_dir, col_dir, rows, cols, extent) = match stack_type {
            StackType::Axial => (
                Vector3::new(0.0, 0.0, -slice_distance),
                Vector3::new(1.0, 0.0, 0.0),
                Vector3::new(0.0, 1.0, 0.0),
                (dy / spacing) as u32,
                (dx / spacing) as u32,
                dz,
            ),
            StackType::Coronal => (
                Vector3::new(0.0, slice_distance, 0.0),
                Vector3::new(1.0, 0.0, 0.0),
                Vector3::new(0.0, 0.0, -1.0),
                (dz / spacing) as u32,
                (dx / spacing) as u32,
                dy,
            ),
            StackType::Sagittal => (
                Vector3::new(slice_distance, 0.0, 0.0),
                Vector3::new(0.0, 1.0, 0.0),
                Vector3::new(0.0, 0.0, -1.0),
                (dz / spacing) as u32,
                (dy / spacing) as u32,
                dx,
            ),
        };

        let count = (extent / slice_distance).floor() as i64 + 1;
        let mut slices = Vec::new();
        let mut top_left = top_left0;
        for _ in 0..count.max(1) {
            slices.push(volume.get_cut(top_left, row_dir, col_dir, rows, cols, spacing));
            top_left = Point3::new(top_left.x + step.x, top_left.y + step.y, top_left.z + step.z);
        }
        Stack { slices, slice_distance }
    }

    /// The generated slices.
    pub fn slices(&self) -> &[Slice] {
        &self.slices
    }

    /// The spacing between slices.
    pub fn slice_distance(&self) -> f64 {
        self.slice_distance
    }
}

/// Writes a [`Stack`] back as a new series of datasets (fo-dicom `DicomGenerator`).
///
/// Each output dataset is a clone of the common source dataset with fresh series /
/// SOP Instance UIDs, recomputed geometry and appends its slice pixels through the
/// PixelData facade.
pub struct DicomGenerator {
    common_dataset: Dataset,
}

impl DicomGenerator {
    pub fn new(common_dataset: Dataset) -> Self {
        DicomGenerator { common_dataset }
    }

    /// Stores the stack as one dataset per slice, described by `new_series_description`.
    pub fn store_as_dicom(&self, stack: &Stack, new_series_description: &str) -> Vec<Dataset> {
        let new_series_uid = Uid::generate_value();
        let mut out = Vec::with_capacity(stack.slices().len());
        for (index, slice) in stack.slices().iter().enumerate() {
            let mut ds = self.common_dataset.clone();
            let rows = slice.rows() as u16;
            let cols = slice.columns() as u16;

            ds.insert_str(Tag::new(0x0008, 0x103E), Vr::Lo, new_series_description);
            ds.insert_str(Tag::new(0x0020, 0x000E), Vr::Ui, new_series_uid.clone());
            ds.insert_str(Tag::new(0x0008, 0x0018), Vr::Ui, Uid::generate_value());
            ds.insert_value(Tag::new(0x0020, 0x0013), (index + 1) as i32);

            ds.insert_value(Tag::new(0x0028, 0x0010), rows);
            ds.insert_value(Tag::new(0x0028, 0x0011), cols);
            let tl = slice.top_left();
            ds.insert_str(Tag::new(0x0020, 0x0032), Vr::Ds, format!("{}\\{}\\{}", tl.x, tl.y, tl.z));
            let rd = slice.row_direction();
            let cd = slice.column_direction();
            ds.insert_str(
                Tag::new(0x0020, 0x0037),
                Vr::Ds,
                format!("{}\\{}\\{}\\{}\\{}\\{}", rd.x, rd.y, rd.z, cd.x, cd.y, cd.z),
            );
            ds.insert_str(Tag::new(0x0028, 0x0030), Vr::Ds, format!("{}\\{}", slice.spacing(), slice.spacing()));
            ds.insert_str(Tag::new(0x0018, 0x0050), Vr::Ds, format!("{}", stack.slice_distance()));

            // infer the smallest storage width from the observed maximum
            let (_, max_value) = slice.min_max();
            let bits_stored = if max_value <= 255.0 {
                8u16
            } else {
                16
            };
            ds.insert_value(Tag::new(0x0028, 0x0100), bits_stored); // BitsAllocated
            ds.insert_value(Tag::new(0x0028, 0x0101), bits_stored); // BitsStored
            ds.insert_value(Tag::new(0x0028, 0x0102), bits_stored - 1); // HighBit
            ds.insert_value(Tag::new(0x0028, 0x0103), 0u16); // PixelRepresentation unsigned
            ds.insert_value(Tag::new(0x0028, 0x0002), 1u16); // SamplesPerPixel
            if !ds.contains(&Tag::new(0x0028, 0x0004)) {
                ds.insert_str(Tag::new(0x0028, 0x0004), Vr::Cs, "MONOCHROME2");
            }

            let bytes_per_pixel = if bits_stored <= 8 { 1u8 } else { 2u8 };
            let frame = slice.render_raw_data(bytes_per_pixel);
            {
                let mut pixel = PixelData::create_new(&mut ds).expect("create pixel data");
                pixel.add_frame(frame).expect("append frame");
            }

            out.push(ds);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tag::Tag;
    use crate::model::vr::Vr;
    use std::sync::Arc;

    /// An axial slice at a given patient z, 4x4, filled with `fill`.
    fn axial_slice(z: f64, fill: u16) -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_value(Tag::new(0x0028, 0x0002), 1u16); // samples
        ds.insert_str(Tag::new(0x0028, 0x0004), Vr::Cs, "MONOCHROME2");
        ds.insert_value(Tag::new(0x0028, 0x0010), 4u16); // rows
        ds.insert_value(Tag::new(0x0028, 0x0011), 4u16); // cols
        ds.insert_value(Tag::new(0x0028, 0x0100), 16u16); // bits allocated
        ds.insert_value(Tag::new(0x0028, 0x0101), 16u16); // bits stored
        ds.insert_value(Tag::new(0x0028, 0x0102), 15u16); // high bit
        ds.insert_value(Tag::new(0x0028, 0x0103), 0u16); // unsigned
        ds.insert_str(Tag::new(0x0028, 0x0030), Vr::Ds, "1\\1"); // pixel spacing
        // image orientation: rows=+y, cols=+x, normal=+z
        ds.insert_str(Tag::new(0x0020, 0x0037), Vr::Ds, "1\\0\\0\\0\\1\\0");
        ds.insert_str(Tag::new(0x0020, 0x0032), Vr::Ds, format!("0\\0\\{z}"));
        ds.insert_str(Tag::new(0x0020, 0x0052), Vr::Ui, "1.2.3.4");
        ds.insert_value(Tag::new(0x0020, 0x0013), 1i32);
        let mut bytes = Vec::new();
        for _ in 0..16 {
            bytes.extend_from_slice(&fill.to_le_bytes());
        }
        ds.insert_or_update(crate::model::item::DicomItem::Element(
            crate::model::element::DicomElement::new_bytes(
                Tag::new(0x7FE0, 0x0010),
                Vr::Ow,
                Arc::new(crate::model::buffer::MemoryBuffer::from(bytes)),
            ),
        ));
        ds.set_transfer_syntax(crate::model::transfer_syntax::syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone());
        ds
    }

    #[test]
    fn a_volume_stacks_and_sorts_slices_by_position() {
        let slices = [axial_slice(2.0, 200), axial_slice(0.0, 100), axial_slice(1.0, 150)];
        let volume = VolumeData::from_datasets(&slices).unwrap();
        assert_eq!(volume.len(), 3);
        // sorted ascending by z
        assert_eq!(volume.slices[0].sorting_value(), 0.0);
        assert_eq!(volume.slices[2].sorting_value(), 2.0);
    }

    #[test]
    fn mixing_frame_of_reference_is_rejected() {
        let mut a = axial_slice(0.0, 1);
        let mut b = axial_slice(1.0, 2);
        b.insert_str(Tag::new(0x0020, 0x0052), Vr::Ui, "9.9.9");
        let slices = [a.clone(), b];
        assert!(VolumeData::from_datasets(&slices).is_err());
        let _ = &mut a; // silence unused-mut if any
    }

    #[test]
    fn an_axial_cut_through_constant_slices_is_constant() {
        // all slices fill 500 → any cut value is 500
        let slices: Vec<Dataset> = (0..4).map(|z| axial_slice(z as f64, 500)).collect();
        let volume = VolumeData::from_datasets(&slices).unwrap();
        // cut an axial plane at z=1.5 (between slice 1 and 2), 2x2, spacing 1
        let cut = volume.get_cut(
            Point3::new(0.5, 0.5, 1.5),
            Vector3::new(1.0, 0.0, 0.0),
            Vector3::new(0.0, 1.0, 0.0),
            2,
            2,
            1.0,
        );
        assert_eq!(cut.pixels(), &[500.0, 500.0, 500.0, 500.0]);
        assert_eq!(cut.rows(), 2);
        assert_eq!(cut.columns(), 2);
    }

    #[test]
    fn a_cut_between_slices_interpolates_linearly() {
        // slice z=0 fill 0, z=1 fill 100; a cut at z=0.5 → ~50
        let slices = [axial_slice(0.0, 0), axial_slice(1.0, 100)];
        let volume = VolumeData::from_datasets(&slices).unwrap();
        let cut = volume.get_cut(
            Point3::new(0.5, 0.5, 0.5),
            Vector3::new(1.0, 0.0, 0.0),
            Vector3::new(0.0, 1.0, 0.0),
            1,
            1,
            1.0,
        );
        let v = cut.pixels()[0];
        assert!((40.0..=60.0).contains(&v), "mid-slice value is ~50, got {v}");
    }

    #[test]
    fn min_max_and_byte_rendering_span_the_range() {
        let slices = [axial_slice(0.0, 10), axial_slice(1.0, 20)];
        let volume = VolumeData::from_datasets(&slices).unwrap();
        let cut = volume.get_cut(
            Point3::new(0.0, 0.0, 0.0),
            Vector3::new(1.0, 0.0, 0.0),
            Vector3::new(0.0, 1.0, 0.0),
            2,
            2,
            1.0,
        );
        let (min, max) = cut.min_max();
        assert!(max >= min);
        let bytes = cut.render_into_byte_array();
        assert_eq!(bytes.len(), 4);
        assert!(bytes.contains(&0) || bytes.contains(&255), "the range is spread to the byte domain");
    }

    #[test]
    fn a_slice_without_geometry_is_skipped() {
        let mut flat = Dataset::new();
        flat.insert_value(Tag::new(0x0028, 0x0010), 4u16);
        flat.insert_value(Tag::new(0x0028, 0x0011), 4u16);
        let good = [axial_slice(0.0, 10), axial_slice(1.0, 20)];
        let mut all = good.to_vec();
        all.push(flat);
        let volume = VolumeData::from_datasets(&all).unwrap();
        assert_eq!(volume.len(), 2, "the geometry-less slice is dropped");
    }

    #[test]
    fn a_stack_generates_a_series_of_slices() {
        // 4x4 slices at z = 0..3, spacing 1 mm → an axial stack with spacing 1
        let slices: Vec<Dataset> = (0..4).map(|z| axial_slice(z as f64, 100)).collect();
        let volume = VolumeData::from_datasets(&slices).unwrap();
        let stack = Stack::new(&volume, StackType::Axial, 1.0, 1.0);
        assert!(!stack.slices().is_empty(), "at least one slice is produced");
        assert_eq!(stack.slice_distance(), 1.0);
        // every generated cut has the requested geometry
        for slice in stack.slices() {
            assert!(slice.rows() > 0 && slice.columns() > 0);
        }
    }

    #[test]
    fn dicom_generator_writes_one_dataset_per_slice() {
        let slices: Vec<Dataset> = (0..4).map(|z| axial_slice(z as f64, 100)).collect();
        let volume = VolumeData::from_datasets(&slices).unwrap();
        let stack = Stack::new(&volume, StackType::Axial, 1.0, 1.0);

        let generator = DicomGenerator::new(slices[0].clone());
        let outputs = generator.store_as_dicom(&stack, "MPR axial");
        assert_eq!(outputs.len(), stack.slices().len());

        let mut sops = std::collections::HashSet::new();
        for (i, ds) in outputs.iter().enumerate() {
            // fresh SOP Instance UID per slice
            let sop = ds.try_get_str(Tag::new(0x0008, 0x0018)).unwrap();
            assert!(Uid::is_valid(&sop));
            assert!(sops.insert(sop), "SOP Instance UIDs are unique");
            // geometry + pixel tags present
            assert!(ds.get::<u16>(Tag::new(0x0028, 0x0010)).is_ok(), "Rows set");
            assert!(ds.get::<u16>(Tag::new(0x0028, 0x0011)).is_ok(), "Columns set");
            assert_eq!(ds.get::<i32>(Tag::new(0x0020, 0x0013)).unwrap(), (i + 1) as i32);
            // the pixel data decodes back through the facade
            let mut owned = ds.clone();
            let pixel = PixelData::from_dataset(&mut owned).unwrap();
            assert!(pixel.get_frame(0).is_ok(), "pixel data written");
        }
    }
}
