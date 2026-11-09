//! Reading values out of DICOM elements, shared by the metadata, pixel and
//! volume code.

use std::str::FromStr;

use dicom_object::DefaultDicomObject;

use super::text_encoding::{decode_iso_2022_text, iso_2022_g1_read_as_latin_1};

/// The trimmed text of a DICOM element, or `None` when it is absent or empty.
pub(super) fn element_text(dicom_object: &DefaultDicomObject, keyword: &str) -> Option<String> {
    let raw_text = dicom_object.element_by_name(keyword).ok()?.to_str().ok()?;
    let text = decode_iso_2022_text(&raw_text, g1_read_as_latin_1(dicom_object))
        .trim()
        .trim_matches('\0')
        .trim()
        .to_owned();

    (!text.is_empty()).then_some(text)
}

/// Whether the parser read the ISO 2022 G1 bytes of `dicom_object`'s text as Latin-1.
pub(super) fn g1_read_as_latin_1(dicom_object: &DefaultDicomObject) -> bool {
    dicom_object
        .element_by_name("SpecificCharacterSet")
        .ok()
        .and_then(|element| element.to_str().ok())
        .is_some_and(|declaration| iso_2022_g1_read_as_latin_1(&declaration))
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
