//! Reading values out of DICOM elements, shared by the metadata, pixel and
//! volume code.

use std::str::FromStr;

use dicom_object::DefaultDicomObject;

/// The trimmed text of a DICOM element, or `None` when it is absent or empty.
pub(super) fn element_text(dicom_object: &DefaultDicomObject, keyword: &str) -> Option<String> {
    let text = dicom_object
        .element_by_name(keyword)
        .ok()?
        .to_str()
        .ok()?
        .trim()
        .trim_matches('\0')
        .trim()
        .to_owned();

    (!text.is_empty()).then_some(text)
}

/// The first value of a possibly multi-valued DICOM element.
pub(super) fn first_parsed<T>(dicom_object: &DefaultDicomObject, keyword: &str) -> Option<T>
where
    T: FromStr,
{
    element_text(dicom_object, keyword)?
        .split('\\')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// All `N` numeric values of a DICOM element, or `None` unless every one of
/// them parses and is finite.
pub(super) fn parsed_values<const N: usize>(
    dicom_object: &DefaultDicomObject,
    keyword: &str,
) -> Option<[f64; N]> {
    let text = element_text(dicom_object, keyword)?;
    let values: Vec<_> = text
        .split('\\')
        .map(str::trim)
        .map(str::parse::<f64>)
        .collect::<Result<_, _>>()
        .ok()?;

    let values: [f64; N] = values.try_into().ok()?;
    values
        .iter()
        .all(|value| value.is_finite())
        .then_some(values)
}
