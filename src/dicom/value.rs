use std::str::FromStr;

use dicom_object::DefaultDicomObject;

pub fn text(dicom_object: &DefaultDicomObject, dicom_keyword: &str) -> Option<String> {
    let raw_value = raw_text(dicom_object, dicom_keyword)?;
    let cleaned_value = clean_text_value(&raw_value);

    if cleaned_value.is_empty() {
        None
    } else {
        Some(cleaned_value)
    }
}

pub fn first_parsed<T>(dicom_object: &DefaultDicomObject, dicom_keyword: &str) -> Option<T>
where
    T: FromStr,
{
    parsed_at(dicom_object, dicom_keyword, 0)
}

pub fn parsed_at<T>(
    dicom_object: &DefaultDicomObject,
    dicom_keyword: &str,
    value_index: usize,
) -> Option<T>
where
    T: FromStr,
{
    raw_text(dicom_object, dicom_keyword)?
        .trim()
        .trim_matches('\0')
        .split('\\')
        .nth(value_index)
        .and_then(|value| value.trim().parse::<T>().ok())
}

pub fn clean_text_value(raw_value: &str) -> String {
    raw_value.trim().trim_matches('\0').replace('\\', ", ")
}

fn raw_text(dicom_object: &DefaultDicomObject, dicom_keyword: &str) -> Option<String> {
    dicom_object
        .element_by_name(dicom_keyword)
        .ok()?
        .to_str()
        .ok()
        .map(|value| value.into_owned())
}
