//! Construction, sorting, and querying of the in-memory DICOM index.

use std::cmp::Ordering;
use std::collections::{HashMap, hash_map::Entry};
use std::path::Path;
use std::str::FromStr;

use dicom_core::{PrimitiveValue, Tag};
use dicom_dictionary_std::tags;

use super::model::{PatientGroup, SeriesGroup, SliceItem, StudyGroup};

#[derive(Default)]
pub(in crate::dicom) struct DicomIndexBuilder {
    patients: Vec<PatientGroup>,
    patient_indices: HashMap<String, usize>,
    study_indices: HashMap<(usize, String), usize>,
    series_indices: HashMap<(usize, usize, String), usize>,
}

#[derive(Default)]
pub(in crate::dicom) struct DicomIndexMetadata {
    patient_id: Option<String>,
    patient_name: Option<String>,
    study_instance_uid: Option<String>,
    study_description: Option<String>,
    study_date: Option<String>,
    study_time: Option<String>,
    series_instance_uid: Option<String>,
    series_description: Option<String>,
    series_number: Option<i32>,
    instance_number: Option<i32>,
    image_position_patient: Option<[f64; 3]>,
    image_orientation_patient: Option<[f64; 6]>,
    number_of_frames: Option<u32>,
    rows: Option<u32>,
    columns: Option<u32>,
    samples_per_pixel: Option<u32>,
    bits_allocated: Option<u32>,
    photometric_interpretation: Option<String>,
}

impl DicomIndexMetadata {
    pub(in crate::dicom) fn includes_tag(tag: Tag) -> bool {
        matches!(
            tag,
            tags::PATIENT_ID
                | tags::PATIENT_NAME
                | tags::STUDY_INSTANCE_UID
                | tags::STUDY_DESCRIPTION
                | tags::STUDY_DATE
                | tags::STUDY_TIME
                | tags::SERIES_INSTANCE_UID
                | tags::SERIES_DESCRIPTION
                | tags::SERIES_NUMBER
                | tags::INSTANCE_NUMBER
                | tags::IMAGE_POSITION_PATIENT
                | tags::IMAGE_ORIENTATION_PATIENT
                | tags::NUMBER_OF_FRAMES
                | tags::ROWS
                | tags::COLUMNS
                | tags::SAMPLES_PER_PIXEL
                | tags::BITS_ALLOCATED
                | tags::PHOTOMETRIC_INTERPRETATION
        )
    }

    pub(in crate::dicom) fn put_primitive(&mut self, tag: Tag, value: &PrimitiveValue) {
        match tag {
            tags::PATIENT_ID => self.patient_id = text_value(value),
            tags::PATIENT_NAME => self.patient_name = text_value(value),
            tags::STUDY_INSTANCE_UID => self.study_instance_uid = text_value(value),
            tags::STUDY_DESCRIPTION => self.study_description = text_value(value),
            tags::STUDY_DATE => self.study_date = text_value(value),
            tags::STUDY_TIME => self.study_time = text_value(value),
            tags::SERIES_INSTANCE_UID => self.series_instance_uid = text_value(value),
            tags::SERIES_DESCRIPTION => self.series_description = text_value(value),
            tags::SERIES_NUMBER => self.series_number = first_parsed_value(value),
            tags::INSTANCE_NUMBER => self.instance_number = first_parsed_value(value),
            tags::IMAGE_POSITION_PATIENT => self.image_position_patient = parsed_array_value(value),
            tags::IMAGE_ORIENTATION_PATIENT => {
                self.image_orientation_patient = parsed_array_value(value)
            }
            tags::NUMBER_OF_FRAMES => self.number_of_frames = first_parsed_value(value),
            tags::ROWS => self.rows = first_parsed_value(value),
            tags::COLUMNS => self.columns = first_parsed_value(value),
            tags::SAMPLES_PER_PIXEL => self.samples_per_pixel = first_parsed_value(value),
            tags::BITS_ALLOCATED => self.bits_allocated = first_parsed_value(value),
            tags::PHOTOMETRIC_INTERPRETATION => self.photometric_interpretation = text_value(value),
            _ => {}
        }
    }

    fn has_image_pixel_metadata(&self) -> bool {
        self.rows.is_some_and(|value| value > 0)
            && self.columns.is_some_and(|value| value > 0)
            && self.samples_per_pixel.is_some_and(|value| value > 0)
            && self.bits_allocated.is_some_and(|value| value > 0)
            && self.photometric_interpretation.is_some()
    }
}

pub(in crate::dicom) struct DicomIndexEntry {
    patient_key: String,
    patient_display_name: String,
    study_key: String,
    study_display_name: String,
    study_date: Option<String>,
    study_time: Option<String>,
    series_key: String,
    series_display_name: String,
    series_number: Option<i32>,
    file_path: std::path::PathBuf,
    instance_number: Option<i32>,
    sort_position: Option<f64>,
    number_of_frames: u32,
}

impl DicomIndexEntry {
    pub(in crate::dicom) fn from_metadata(
        file_path: &Path,
        metadata: DicomIndexMetadata,
        has_pixel_data: bool,
    ) -> Option<Self> {
        if !has_pixel_data || !metadata.has_image_pixel_metadata() {
            return None;
        }

        let DicomIndexMetadata {
            patient_id,
            patient_name,
            study_instance_uid,
            study_description,
            study_date,
            study_time,
            series_instance_uid,
            series_description,
            series_number,
            instance_number,
            image_position_patient,
            image_orientation_patient,
            number_of_frames,
            ..
        } = metadata;
        let sort_position = compute_slice_sort_position_from_values(
            image_position_patient,
            image_orientation_patient,
        );
        let number_of_frames = number_of_frames.unwrap_or(1).max(1);

        let patient_key = patient_id.clone().unwrap_or_else(|| {
            synthetic_patient_key(
                file_path,
                study_instance_uid.as_deref(),
                series_instance_uid.as_deref(),
            )
        });
        let study_key = study_instance_uid.clone().unwrap_or_else(|| {
            synthetic_hierarchy_key("study", file_path, series_instance_uid.as_deref())
        });
        let series_key = series_instance_uid
            .clone()
            .unwrap_or_else(|| synthetic_hierarchy_key("series", file_path, None));
        let patient_display_name =
            build_patient_display_name(patient_name.as_deref(), patient_id.as_deref());
        let study_display_name = build_study_display_name(
            study_description.as_deref(),
            study_date.as_deref(),
            study_time.as_deref(),
            study_instance_uid.as_deref(),
        );
        let series_display_name =
            build_series_display_name(series_number, series_description.as_deref());

        Some(Self {
            patient_key,
            patient_display_name,
            study_key,
            study_display_name,
            study_date,
            study_time,
            series_key,
            series_display_name,
            series_number,
            file_path: file_path.to_path_buf(),
            instance_number,
            sort_position,
            number_of_frames,
        })
    }
}

impl DicomIndexBuilder {
    pub(in crate::dicom) fn add_entry(&mut self, entry: DicomIndexEntry) {
        let DicomIndexEntry {
            patient_key,
            patient_display_name,
            study_key,
            study_display_name,
            study_date,
            study_time,
            series_key,
            series_display_name,
            series_number,
            file_path,
            instance_number,
            sort_position,
            number_of_frames,
        } = entry;
        let patient_index = self.get_or_insert_patient(patient_key, patient_display_name);
        let study_index = self.get_or_insert_study(
            patient_index,
            study_key,
            study_display_name,
            study_date,
            study_time,
        );
        let series_index = self.get_or_insert_series(
            patient_index,
            study_index,
            series_key,
            series_display_name,
            series_number,
        );

        for frame_index in 0..number_of_frames {
            let slice_display_name = build_slice_display_name(
                &file_path,
                instance_number,
                frame_index,
                number_of_frames,
            );

            self.patients[patient_index].studies[study_index].series_groups[series_index]
                .slices
                .push(SliceItem {
                    path: file_path.clone(),
                    display_name: slice_display_name,
                    frame_index,
                    instance_number,
                    sort_position,
                });
        }
    }

    pub(in crate::dicom) fn into_patients(mut self) -> Vec<PatientGroup> {
        sort_index(&mut self.patients);
        self.patients
    }

    fn get_or_insert_patient(&mut self, patient_key: String, display_name: String) -> usize {
        match self.patient_indices.entry(patient_key) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let index = self.patients.len();
                entry.insert(index);
                self.patients.push(PatientGroup {
                    display_name,
                    studies: Vec::new(),
                });
                index
            }
        }
    }

    fn get_or_insert_study(
        &mut self,
        patient_index: usize,
        study_key: String,
        display_name: String,
        study_date: Option<String>,
        study_time: Option<String>,
    ) -> usize {
        match self.study_indices.entry((patient_index, study_key)) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let index = self.patients[patient_index].studies.len();
                entry.insert(index);
                self.patients[patient_index].studies.push(StudyGroup {
                    display_name,
                    study_date,
                    study_time,
                    series_groups: Vec::new(),
                });
                index
            }
        }
    }

    fn get_or_insert_series(
        &mut self,
        patient_index: usize,
        study_index: usize,
        series_key: String,
        display_name: String,
        series_number: Option<i32>,
    ) -> usize {
        match self
            .series_indices
            .entry((patient_index, study_index, series_key))
        {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let series_groups =
                    &mut self.patients[patient_index].studies[study_index].series_groups;
                let index = series_groups.len();
                entry.insert(index);
                series_groups.push(SeriesGroup {
                    display_name,
                    series_number,
                    slices: Vec::new(),
                });
                index
            }
        }
    }
}

fn synthetic_patient_key(
    file_path: &Path,
    study_instance_uid: Option<&str>,
    series_instance_uid: Option<&str>,
) -> String {
    if let Some(study_instance_uid) = study_instance_uid {
        return synthetic_hierarchy_key("patient-study", file_path, Some(study_instance_uid));
    }
    if let Some(series_instance_uid) = series_instance_uid {
        return synthetic_hierarchy_key("patient-series", file_path, Some(series_instance_uid));
    }

    synthetic_hierarchy_key("patient", file_path, None)
}

fn synthetic_hierarchy_key(kind: &str, file_path: &Path, uid: Option<&str>) -> String {
    match uid {
        Some(uid) => format!("\u{1f}dicron-{kind}-uid:{uid}"),
        None => format!("\u{1f}dicron-{kind}-path:{}", file_path.display()),
    }
}

fn build_patient_display_name(patient_name: Option<&str>, patient_id: Option<&str>) -> String {
    match (patient_name, patient_id) {
        (Some(patient_name), Some(patient_id)) => format!("{patient_name} ({patient_id})"),
        (Some(patient_name), None) => patient_name.to_owned(),
        (None, Some(patient_id)) => patient_id.to_owned(),
        (None, None) => "Unknown Patient".to_owned(),
    }
}

fn build_study_display_name(
    study_description: Option<&str>,
    study_date: Option<&str>,
    study_time: Option<&str>,
    study_instance_uid: Option<&str>,
) -> String {
    let description = study_description.unwrap_or("Unknown Study");

    match (study_date, study_time) {
        (Some(study_date), Some(study_time)) => {
            format!("{study_date} {study_time} - {description}")
        }
        (Some(study_date), None) => format!("{study_date} - {description}"),
        _ if description == "Unknown Study" => study_instance_uid.unwrap_or(description).to_owned(),
        _ => description.to_owned(),
    }
}

fn build_series_display_name(
    series_number: Option<i32>,
    series_description: Option<&str>,
) -> String {
    let description = series_description.unwrap_or("Unknown Series");

    match series_number {
        Some(series_number) => format!("Series {series_number} - {description}"),
        None => description.to_owned(),
    }
}

fn build_slice_display_name(
    file_path: &Path,
    instance_number: Option<i32>,
    frame_index: u32,
    number_of_frames: u32,
) -> String {
    let file_name = file_path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or("unknown");

    let file_label = match instance_number {
        Some(instance_number) => format!("#{instance_number} - {file_name}"),
        None => file_name.to_owned(),
    };

    if number_of_frames > 1 {
        format!(
            "{file_label} [frame {} / {number_of_frames}]",
            frame_index + 1
        )
    } else {
        file_label
    }
}

pub(in crate::dicom) fn sort_index(patients: &mut [PatientGroup]) {
    patients.sort_by(|left, right| left.display_name.cmp(&right.display_name));

    for patient in patients {
        patient.studies.sort_by(|left, right| {
            left.study_date
                .cmp(&right.study_date)
                .then_with(|| left.study_time.cmp(&right.study_time))
                .then_with(|| left.display_name.cmp(&right.display_name))
        });

        for study in &mut patient.studies {
            study.series_groups.sort_by(|left, right| {
                left.series_number
                    .unwrap_or(i32::MAX)
                    .cmp(&right.series_number.unwrap_or(i32::MAX))
                    .then_with(|| left.display_name.cmp(&right.display_name))
            });

            for series in &mut study.series_groups {
                series.slices.sort_by(compare_slice_items);
            }
        }
    }
}

fn compare_slice_items(left: &SliceItem, right: &SliceItem) -> Ordering {
    compare_optional_f64(left.sort_position, right.sort_position)
        .then_with(|| {
            left.instance_number
                .unwrap_or(i32::MAX)
                .cmp(&right.instance_number.unwrap_or(i32::MAX))
        })
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.frame_index.cmp(&right.frame_index))
}

fn compare_optional_f64(left: Option<f64>, right: Option<f64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Through-plane sort key for a slice. Prefers the projection of
/// `ImagePositionPatient` (0020,0032) onto the slice normal derived from
/// `ImageOrientationPatient` (0020,0037) — which is correct for axial,
/// sagittal, coronal, and oblique acquisitions alike. Falls back to the raw Z
/// component when orientation is absent, and to `None` when position is absent.
fn compute_slice_sort_position_from_values(
    position: Option<[f64; 3]>,
    orientation: Option<[f64; 6]>,
) -> Option<f64> {
    let position = position?;
    let Some(orientation) = orientation else {
        return Some(position[2]);
    };

    Some(project_onto_slice_normal(position, orientation))
}

/// Projection of an `ImagePositionPatient` point onto the slice normal
/// (`row x column` of `ImageOrientationPatient`). For axial orientation this
/// equals the raw Z; for sagittal/coronal/oblique it is the true through-plane
/// coordinate.
fn project_onto_slice_normal(position: [f64; 3], orientation: [f64; 6]) -> f64 {
    let row = [orientation[0], orientation[1], orientation[2]];
    let column = [orientation[3], orientation[4], orientation[5]];
    let normal = [
        row[1] * column[2] - row[2] * column[1],
        row[2] * column[0] - row[0] * column[2],
        row[0] * column[1] - row[1] * column[0],
    ];

    position[0] * normal[0] + position[1] * normal[1] + position[2] * normal[2]
}

fn text_value(value: &PrimitiveValue) -> Option<String> {
    let raw = value.to_str();
    let value = raw.trim().trim_matches('\0').replace('\\', ", ");
    (!value.is_empty()).then_some(value)
}

fn first_parsed_value<T>(value: &PrimitiveValue) -> Option<T>
where
    T: FromStr,
{
    value
        .to_str()
        .trim()
        .trim_matches('\0')
        .split('\\')
        .next()?
        .trim()
        .parse()
        .ok()
}

fn parsed_array_value<const N: usize>(value: &PrimitiveValue) -> Option<[f64; N]> {
    let raw = value.to_str();
    let mut parsed_values = raw.trim().trim_matches('\0').split('\\');
    let mut values = [0.0; N];
    for slot in &mut values {
        *slot = parsed_values.next()?.trim().parse().ok()?;
    }
    Some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AXIAL: [f64; 6] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    const SAGITTAL: [f64; 6] = [0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    const CORONAL: [f64; 6] = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0];

    #[test]
    fn axial_projection_is_raw_z() {
        assert_eq!(project_onto_slice_normal([10.0, 20.0, 30.0], AXIAL), 30.0);
    }

    #[test]
    fn sagittal_projection_follows_x() {
        // Sagittal slices progress along X, which raw-Z ordering would miss.
        assert_eq!(project_onto_slice_normal([5.0, 99.0, 99.0], SAGITTAL), 5.0);
    }

    #[test]
    fn coronal_projection_follows_y() {
        // Coronal normal is (0,-1,0); ordering is monotonic in Y.
        assert_eq!(project_onto_slice_normal([99.0, 7.0, 99.0], CORONAL), -7.0);
    }

    fn image_metadata() -> DicomIndexMetadata {
        DicomIndexMetadata {
            rows: Some(16),
            columns: Some(16),
            samples_per_pixel: Some(1),
            bits_allocated: Some(16),
            photometric_interpretation: Some("MONOCHROME2".to_owned()),
            ..Default::default()
        }
    }

    fn add_metadata(
        index_builder: &mut DicomIndexBuilder,
        path: &Path,
        metadata: DicomIndexMetadata,
        has_pixel_data: bool,
    ) -> bool {
        let Some(entry) = DicomIndexEntry::from_metadata(path, metadata, has_pixel_data) else {
            return false;
        };
        index_builder.add_entry(entry);
        true
    }

    #[test]
    fn non_image_objects_do_not_become_slices() {
        let mut index_builder = DicomIndexBuilder::default();

        assert!(!add_metadata(
            &mut index_builder,
            Path::new("structure-set"),
            DicomIndexMetadata::default(),
            false,
        ));
        let patients = index_builder.into_patients();
        assert!(patients.is_empty());
    }

    #[test]
    fn compressed_multiframe_image_headers_become_slices_without_pixel_decoding() {
        let mut metadata = image_metadata();
        metadata.number_of_frames = Some(3);
        let mut index_builder = DicomIndexBuilder::default();

        assert!(add_metadata(
            &mut index_builder,
            Path::new("compressed-image"),
            metadata,
            true,
        ));
        let patients = index_builder.into_patients();
        assert_eq!(patients[0].studies[0].series_groups[0].slices.len(), 3);
    }

    #[test]
    fn objects_missing_all_hierarchy_ids_do_not_merge() {
        let mut index_builder = DicomIndexBuilder::default();

        assert!(add_metadata(
            &mut index_builder,
            Path::new("first/image"),
            image_metadata(),
            true,
        ));
        assert!(add_metadata(
            &mut index_builder,
            Path::new("second/image"),
            image_metadata(),
            true,
        ));
        let patients = index_builder.into_patients();

        assert_eq!(patients.len(), 2);
        assert!(
            patients
                .iter()
                .all(|patient| patient.display_name == "Unknown Patient")
        );
        assert!(
            patients
                .iter()
                .all(|patient| patient.studies[0].display_name == "Unknown Study")
        );
        assert!(patients.iter().all(|patient| {
            patient.studies[0].series_groups[0].display_name == "Unknown Series"
        }));
    }

    #[test]
    fn complete_hierarchy_ids_keep_normal_grouping_behavior() {
        let mut index_builder = DicomIndexBuilder::default();
        let metadata = || DicomIndexMetadata {
            patient_id: Some("patient-1".to_owned()),
            study_instance_uid: Some("2.25.301".to_owned()),
            series_instance_uid: Some("2.25.302".to_owned()),
            ..image_metadata()
        };

        assert!(add_metadata(
            &mut index_builder,
            Path::new("first-image"),
            metadata(),
            true,
        ));
        assert!(add_metadata(
            &mut index_builder,
            Path::new("second-image"),
            metadata(),
            true,
        ));
        let patients = index_builder.into_patients();

        assert_eq!(patients.len(), 1);
        assert_eq!(patients[0].studies.len(), 1);
        assert_eq!(patients[0].studies[0].series_groups.len(), 1);
        assert_eq!(patients[0].studies[0].series_groups[0].slices.len(), 2);
    }
}
