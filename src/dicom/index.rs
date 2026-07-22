//! Construction, sorting, and querying of the in-memory DICOM index.

use std::cmp::Ordering;
use std::path::Path;
use std::str::FromStr;

use dicom_object::DefaultDicomObject;

use super::model::{PatientGroup, SeriesGroup, SliceItem, StudyGroup};

pub(in crate::dicom) fn add_dicom_object_to_index(
    patients: &mut Vec<PatientGroup>,
    file_path: &Path,
    dicom_object: &DefaultDicomObject,
) {
    let patient_id = text(dicom_object, "PatientID");
    let patient_name = text(dicom_object, "PatientName");

    let study_instance_uid =
        text(dicom_object, "StudyInstanceUID").unwrap_or_else(|| "Unknown Study".to_owned());
    let study_description = text(dicom_object, "StudyDescription");
    let study_date = text(dicom_object, "StudyDate");
    let study_time = text(dicom_object, "StudyTime");

    let series_instance_uid =
        text(dicom_object, "SeriesInstanceUID").unwrap_or_else(|| "Unknown Series".to_owned());
    let series_description = text(dicom_object, "SeriesDescription");
    let series_number = first_parsed(dicom_object, "SeriesNumber");

    let instance_number = first_parsed(dicom_object, "InstanceNumber");
    let sort_position = compute_slice_sort_position(dicom_object);
    let number_of_frames = first_parsed::<u32>(dicom_object, "NumberOfFrames")
        .unwrap_or(1)
        .max(1);

    let patient_key = patient_id
        .clone()
        .or_else(|| patient_name.clone())
        .unwrap_or_else(|| "Unknown Patient".to_owned());

    let patient_display_name =
        build_patient_display_name(patient_name.as_deref(), patient_id.as_deref());

    let study_display_name = build_study_display_name(
        study_description.as_deref(),
        study_date.as_deref(),
        study_time.as_deref(),
        &study_instance_uid,
    );

    let series_display_name =
        build_series_display_name(series_number, series_description.as_deref());

    let patient_index = get_or_insert_patient(patients, patient_key, patient_display_name);

    let study_index = get_or_insert_study(
        &mut patients[patient_index].studies,
        study_instance_uid,
        study_display_name,
        study_date,
        study_time,
    );

    let series_index = get_or_insert_series(
        &mut patients[patient_index].studies[study_index].series_groups,
        series_instance_uid,
        series_display_name,
        series_number,
    );

    for frame_index in 0..number_of_frames {
        let slice_display_name =
            build_slice_display_name(file_path, instance_number, frame_index, number_of_frames);

        patients[patient_index].studies[study_index].series_groups[series_index]
            .slices
            .push(SliceItem {
                path: file_path.to_path_buf(),
                display_name: slice_display_name,
                frame_index,
                instance_number,
                sort_position,
            });
    }
}

fn get_or_insert_patient(
    patients: &mut Vec<PatientGroup>,
    patient_key: String,
    display_name: String,
) -> usize {
    if let Some(patient_index) = patients
        .iter()
        .position(|patient| patient.patient_key == patient_key)
    {
        return patient_index;
    }

    patients.push(PatientGroup {
        patient_key,
        display_name,
        studies: Vec::new(),
    });

    patients.len() - 1
}

fn get_or_insert_study(
    studies: &mut Vec<StudyGroup>,
    study_key: String,
    display_name: String,
    study_date: Option<String>,
    study_time: Option<String>,
) -> usize {
    if let Some(study_index) = studies
        .iter()
        .position(|study| study.study_key == study_key)
    {
        return study_index;
    }

    studies.push(StudyGroup {
        study_key,
        display_name,
        study_date,
        study_time,
        series_groups: Vec::new(),
    });

    studies.len() - 1
}

fn get_or_insert_series(
    series_groups: &mut Vec<SeriesGroup>,
    series_key: String,
    display_name: String,
    series_number: Option<i32>,
) -> usize {
    if let Some(series_index) = series_groups
        .iter()
        .position(|series| series.series_key == series_key)
    {
        return series_index;
    }

    series_groups.push(SeriesGroup {
        series_key,
        display_name,
        series_number,
        slices: Vec::new(),
    });

    series_groups.len() - 1
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
    study_instance_uid: &str,
) -> String {
    let description = study_description.unwrap_or("Unknown Study");

    match (study_date, study_time) {
        (Some(study_date), Some(study_time)) => {
            format!("{study_date} {study_time} - {description}")
        }
        (Some(study_date), None) => format!("{study_date} - {description}"),
        _ => {
            if description == "Unknown Study" {
                study_instance_uid.to_owned()
            } else {
                description.to_owned()
            }
        }
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

/// Through-plane sort key for a slice. Prefers the projection of
/// `ImagePositionPatient` (0020,0032) onto the slice normal derived from
/// `ImageOrientationPatient` (0020,0037) — which is correct for axial,
/// sagittal, coronal, and oblique acquisitions alike. Falls back to the raw Z
/// component when orientation is absent, and to `None` (caller then orders by
/// `InstanceNumber`) when position is absent too.
fn compute_slice_sort_position(dicom_object: &DefaultDicomObject) -> Option<f64> {
    let position = get_image_position_patient(dicom_object)?;

    let Some(orientation) = get_image_orientation_patient(dicom_object) else {
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

fn get_image_position_patient(dicom_object: &DefaultDicomObject) -> Option<[f64; 3]> {
    let mut values = [0.0_f64; 3];

    for (index, slot) in values.iter_mut().enumerate() {
        *slot = parsed_at(dicom_object, "ImagePositionPatient", index)?;
    }

    Some(values)
}

fn get_image_orientation_patient(dicom_object: &DefaultDicomObject) -> Option<[f64; 6]> {
    let mut values = [0.0_f64; 6];

    for (index, slot) in values.iter_mut().enumerate() {
        *slot = parsed_at(dicom_object, "ImageOrientationPatient", index)?;
    }

    Some(values)
}

fn text(dicom_object: &DefaultDicomObject, keyword: &str) -> Option<String> {
    let raw_value = dicom_object.element_by_name(keyword).ok()?.to_str().ok()?;
    let value = raw_value.trim().trim_matches('\0').replace('\\', ", ");

    (!value.is_empty()).then_some(value)
}

fn first_parsed<T>(dicom_object: &DefaultDicomObject, keyword: &str) -> Option<T>
where
    T: FromStr,
{
    parsed_at(dicom_object, keyword, 0)
}

fn parsed_at<T>(dicom_object: &DefaultDicomObject, keyword: &str, index: usize) -> Option<T>
where
    T: FromStr,
{
    dicom_object
        .element_by_name(keyword)
        .ok()?
        .to_str()
        .ok()?
        .trim()
        .trim_matches('\0')
        .split('\\')
        .nth(index)?
        .trim()
        .parse()
        .ok()
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
}
