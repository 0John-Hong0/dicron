//! UI-independent extraction and formatting of DICOM metadata.

use dicom_core::header::{HasLength, Header};
use dicom_core::value::Value;
use dicom_core::{
    DataDictionary, Tag,
    dictionary::{DataDictionaryEntry, UidDictionary, UidDictionaryEntry},
};
use dicom_dictionary_std::{StandardDataDictionary, StandardSopClassDictionary, tags};
use dicom_object::DefaultDicomObject;
use dicom_transfer_syntax_registry::{TransferSyntaxIndex, TransferSyntaxRegistry};

const MAX_INLINE_METADATA_VALUE_BYTES: u32 = 4096;

#[derive(Clone)]
pub(crate) struct MetadataItem {
    pub(crate) tag: String,
    pub(crate) description: String,
    pub(crate) value: String,
    // Lowercased "tag\ndescription\nvalue", precomputed once so filtering does
    // not re-lowercase every field of every tag on every UI frame.
    search_haystack: String,
}

impl MetadataItem {
    pub(crate) fn new(tag: String, description: String, value: String) -> Self {
        let search_haystack = format!("{tag}\n{description}\n{value}").to_lowercase();

        Self {
            tag,
            description,
            value,
            search_haystack,
        }
    }

    /// `search_text` is expected to already be lowercased by the caller.
    pub(crate) fn matches_search(&self, search_text: &str) -> bool {
        self.search_haystack.contains(search_text)
    }
}

#[derive(Clone)]
pub(crate) struct DicomMetadata {
    pub(crate) curated_items: Vec<MetadataItem>,
    pub(crate) all_items: Vec<MetadataItem>,
}

pub(crate) fn extract_dicom_metadata(dicom_object: &DefaultDicomObject) -> DicomMetadata {
    DicomMetadata {
        curated_items: extract_curated_dicom_metadata(dicom_object),
        all_items: extract_all_dicom_metadata(dicom_object),
    }
}

fn extract_curated_dicom_metadata(dicom_object: &DefaultDicomObject) -> Vec<MetadataItem> {
    let mut metadata_items = vec![
        MetadataItem::new("-".to_owned(), "File Type".to_owned(), "DICOM".to_owned()),
        MetadataItem::new(
            "(0002,0010)".to_owned(),
            "Transfer Syntax UID".to_owned(),
            format_dicom_text_value(
                tags::TRANSFER_SYNTAX_UID,
                dicom_object.meta().transfer_syntax(),
            ),
        ),
        MetadataItem::new(
            "(0002,0002)".to_owned(),
            "Media Storage SOP Class UID".to_owned(),
            format_dicom_text_value(
                tags::MEDIA_STORAGE_SOP_CLASS_UID,
                dicom_object.meta().media_storage_sop_class_uid(),
            ),
        ),
        MetadataItem::new(
            "(0002,0003)".to_owned(),
            "Media Storage SOP Instance UID".to_owned(),
            dicom_object
                .meta()
                .media_storage_sop_instance_uid()
                .to_owned(),
        ),
    ];

    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0010,0010)",
        "Patient Name",
        "PatientName",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0010,0020)",
        "Patient ID",
        "PatientID",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0010,0040)",
        "Patient Sex",
        "PatientSex",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0010,0030)",
        "Patient Birth Date",
        "PatientBirthDate",
    );

    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0008,0060)",
        "Modality",
        "Modality",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0008,0020)",
        "Study Date",
        "StudyDate",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0008,0030)",
        "Study Time",
        "StudyTime",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0008,1030)",
        "Study Description",
        "StudyDescription",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0008,103E)",
        "Series Description",
        "SeriesDescription",
    );

    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0020,000D)",
        "Study Instance UID",
        "StudyInstanceUID",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0020,000E)",
        "Series Instance UID",
        "SeriesInstanceUID",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0008,0018)",
        "SOP Instance UID",
        "SOPInstanceUID",
    );

    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0020,0011)",
        "Series Number",
        "SeriesNumber",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0020,0013)",
        "Instance Number",
        "InstanceNumber",
    );

    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0010)",
        "Rows",
        "Rows",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0011)",
        "Columns",
        "Columns",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0008)",
        "Number of Frames",
        "NumberOfFrames",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0002)",
        "Samples Per Pixel",
        "SamplesPerPixel",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0004)",
        "Photometric Interpretation",
        "PhotometricInterpretation",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0100)",
        "Bits Allocated",
        "BitsAllocated",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0101)",
        "Bits Stored",
        "BitsStored",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0102)",
        "High Bit",
        "HighBit",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0103)",
        "Pixel Representation",
        "PixelRepresentation",
    );

    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,0030)",
        "Pixel Spacing",
        "PixelSpacing",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0018,0050)",
        "Slice Thickness",
        "SliceThickness",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0020,0032)",
        "Image Position Patient",
        "ImagePositionPatient",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0020,0037)",
        "Image Orientation Patient",
        "ImageOrientationPatient",
    );

    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,1050)",
        "Window Center",
        "WindowCenter",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,1051)",
        "Window Width",
        "WindowWidth",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,1052)",
        "Rescale Intercept",
        "RescaleIntercept",
    );
    push_dicom_tag(
        &mut metadata_items,
        dicom_object,
        "(0028,1053)",
        "Rescale Slope",
        "RescaleSlope",
    );

    metadata_items
}

fn extract_all_dicom_metadata(dicom_object: &DefaultDicomObject) -> Vec<MetadataItem> {
    let mut metadata_items: Vec<MetadataItem> = dicom_object
        .meta()
        .to_element_iter()
        .map(|element| {
            MetadataItem::new(
                element.tag().to_string(),
                build_dicom_tag_description(
                    element.tag(),
                    &format!("File Meta ({})", element.vr()),
                ),
                get_dicom_element_text_value(&element),
            )
        })
        .chain(dicom_object.iter().map(|element| {
            MetadataItem::new(
                element.tag().to_string(),
                build_dicom_tag_description(element.tag(), element.vr().to_string()),
                get_dicom_element_text_value(element),
            )
        }))
        .collect();

    metadata_items.sort_by(|left, right| left.tag.cmp(&right.tag));

    metadata_items
}

fn build_dicom_tag_description(tag: dicom_core::Tag, fallback: &str) -> String {
    StandardDataDictionary
        .by_tag(tag)
        .map(|entry| entry.alias().to_owned())
        .unwrap_or_else(|| fallback.to_owned())
}

fn push_dicom_tag(
    metadata_items: &mut Vec<MetadataItem>,
    dicom_object: &DefaultDicomObject,
    tag: &str,
    description: &str,
    dicom_keyword: &str,
) {
    metadata_items.push(MetadataItem::new(
        tag.to_owned(),
        description.to_owned(),
        get_dicom_text_value(dicom_object, dicom_keyword),
    ));
}

fn get_dicom_text_value(dicom_object: &DefaultDicomObject, dicom_keyword: &str) -> String {
    let Ok(element) = dicom_object.element_by_name(dicom_keyword) else {
        return "-".to_owned();
    };

    get_dicom_element_text_value(element)
}

fn get_dicom_element_text_value<I, P>(element: &dicom_core::DataElement<I, P>) -> String
where
    I: HasLength,
{
    let value_length = element.length();

    if element.tag() == dicom_core::Tag(0x7FE0, 0x0010) {
        return summarize_large_value("Pixel Data", value_length);
    }

    match element.value() {
        Value::Sequence(sequence) => {
            return format!("<sequence: {} items>", sequence.items().len());
        }
        Value::PixelSequence(pixel_sequence) => {
            return format!(
                "<pixel sequence: {} fragments>",
                pixel_sequence.fragments().len()
            );
        }
        Value::Primitive(_) => {}
    }

    if value_length
        .get()
        .is_some_and(|byte_count| byte_count > MAX_INLINE_METADATA_VALUE_BYTES)
    {
        return summarize_large_value("large value", value_length);
    }

    let Ok(raw_value) = element.to_str() else {
        return "<non-text value>".to_owned();
    };

    format_dicom_text_value(element.tag(), raw_value.as_ref())
}

fn format_dicom_text_value(tag: Tag, raw_value: &str) -> String {
    let cleaned_value = clean_text_value(raw_value);

    if cleaned_value.is_empty() {
        return "-".to_owned();
    }

    match known_dicom_value_name(tag, &cleaned_value) {
        Some(name) => format!("{name} ({cleaned_value})"),
        None => cleaned_value,
    }
}

fn known_dicom_value_name(tag: Tag, raw_value: &str) -> Option<String> {
    let name = match tag {
        tags::TRANSFER_SYNTAX_UID => {
            return TransferSyntaxRegistry
                .get(raw_value)
                .map(|transfer_syntax| transfer_syntax.name().to_owned());
        }
        tags::MEDIA_STORAGE_SOP_CLASS_UID | tags::SOP_CLASS_UID => {
            return StandardSopClassDictionary
                .by_uid(raw_value)
                .map(|entry| entry.name().to_owned());
        }
        tags::PIXEL_REPRESENTATION => match raw_value {
            "0" => "Unsigned integer",
            "1" => "Signed two's-complement integer",
            _ => return None,
        },
        tags::PLANAR_CONFIGURATION => match raw_value {
            "0" => "Color-by-pixel",
            "1" => "Color-by-plane",
            _ => return None,
        },
        tags::LOSSY_IMAGE_COMPRESSION => match raw_value {
            "00" => "Has not undergone lossy compression",
            "01" => "Has undergone lossy compression",
            _ => return None,
        },
        _ => return None,
    };

    Some(name.to_owned())
}

fn summarize_large_value(label: &str, length: dicom_core::Length) -> String {
    match length.get() {
        Some(byte_count) => format!("<{label}: {byte_count} bytes>"),
        None => format!("<{label}: undefined length>"),
    }
}

fn clean_text_value(raw_value: &str) -> String {
    raw_value.trim().trim_matches('\0').replace('\\', ", ")
}

#[cfg(test)]
mod tests {
    use dicom_core::{DataElement, VR, dicom_value};
    use dicom_dictionary_std::uids;
    use dicom_object::{FileDicomObject, FileMetaTableBuilder};

    use super::*;

    const EXPECTED_CURATED_ROWS: &[(&str, &str)] = &[
        ("-", "File Type"),
        ("(0002,0010)", "Transfer Syntax UID"),
        ("(0002,0002)", "Media Storage SOP Class UID"),
        ("(0002,0003)", "Media Storage SOP Instance UID"),
        ("(0010,0010)", "Patient Name"),
        ("(0010,0020)", "Patient ID"),
        ("(0010,0040)", "Patient Sex"),
        ("(0010,0030)", "Patient Birth Date"),
        ("(0008,0060)", "Modality"),
        ("(0008,0020)", "Study Date"),
        ("(0008,0030)", "Study Time"),
        ("(0008,1030)", "Study Description"),
        ("(0008,103E)", "Series Description"),
        ("(0020,000D)", "Study Instance UID"),
        ("(0020,000E)", "Series Instance UID"),
        ("(0008,0018)", "SOP Instance UID"),
        ("(0020,0011)", "Series Number"),
        ("(0020,0013)", "Instance Number"),
        ("(0028,0010)", "Rows"),
        ("(0028,0011)", "Columns"),
        ("(0028,0008)", "Number of Frames"),
        ("(0028,0002)", "Samples Per Pixel"),
        ("(0028,0004)", "Photometric Interpretation"),
        ("(0028,0100)", "Bits Allocated"),
        ("(0028,0101)", "Bits Stored"),
        ("(0028,0102)", "High Bit"),
        ("(0028,0103)", "Pixel Representation"),
        ("(0028,0030)", "Pixel Spacing"),
        ("(0018,0050)", "Slice Thickness"),
        ("(0020,0032)", "Image Position Patient"),
        ("(0020,0037)", "Image Orientation Patient"),
        ("(0028,1050)", "Window Center"),
        ("(0028,1051)", "Window Width"),
        ("(0028,1052)", "Rescale Intercept"),
        ("(0028,1053)", "Rescale Slope"),
    ];

    fn metadata_test_object() -> DefaultDicomObject {
        let meta = FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(uids::CT_IMAGE_STORAGE)
            .media_storage_sop_instance_uid("2.25.101")
            .build()
            .unwrap();
        let mut object = FileDicomObject::new_empty_with_meta(meta);

        object.put_element(DataElement::new(
            tags::PATIENT_NAME,
            VR::PN,
            dicom_value!(Str, "Doe^Jane"),
        ));
        object.put_element(DataElement::new(
            tags::PIXEL_REPRESENTATION,
            VR::US,
            dicom_value!(U16, 1),
        ));

        object
    }

    #[test]
    fn formats_known_transfer_syntax_with_raw_uid() {
        assert_eq!(
            format_dicom_text_value(tags::TRANSFER_SYNTAX_UID, "1.2.840.10008.1.2.4.50\0"),
            "JPEG Baseline (Process 1) (1.2.840.10008.1.2.4.50)"
        );
    }

    #[test]
    fn formats_known_sop_class_but_not_instance_uid() {
        let ct_image_storage_uid = "1.2.840.10008.5.1.4.1.1.2";

        assert_eq!(
            format_dicom_text_value(tags::SOP_CLASS_UID, ct_image_storage_uid),
            "CT Image Storage (1.2.840.10008.5.1.4.1.1.2)"
        );
        assert_eq!(
            format_dicom_text_value(tags::SOP_INSTANCE_UID, ct_image_storage_uid),
            ct_image_storage_uid
        );
    }

    #[test]
    fn formats_small_standard_value_sets() {
        let cases = [
            (
                tags::PIXEL_REPRESENTATION,
                "1",
                "Signed two's-complement integer (1)",
            ),
            (tags::PLANAR_CONFIGURATION, "0", "Color-by-pixel (0)"),
            (
                tags::LOSSY_IMAGE_COMPRESSION,
                "00",
                "Has not undergone lossy compression (00)",
            ),
        ];

        for (tag, raw_value, expected) in cases {
            assert_eq!(format_dicom_text_value(tag, raw_value), expected);
        }
    }

    #[test]
    fn leaves_unknown_values_unchanged() {
        assert_eq!(
            format_dicom_text_value(tags::TRANSFER_SYNTAX_UID, "1.2.3.4.5"),
            "1.2.3.4.5"
        );
        assert_eq!(
            format_dicom_text_value(tags::PIXEL_REPRESENTATION, "2"),
            "2"
        );
    }

    #[test]
    fn friendly_and_raw_values_are_searchable() {
        let item = MetadataItem::new(
            "(0002,0010)".to_owned(),
            "Transfer Syntax UID".to_owned(),
            format_dicom_text_value(tags::TRANSFER_SYNTAX_UID, "1.2.840.10008.1.2.1"),
        );

        assert!(item.matches_search("explicit vr little endian"));
        assert!(item.matches_search("1.2.840.10008.1.2.1"));
    }

    #[test]
    fn curated_metadata_preserves_order_labels_and_placeholders() {
        let metadata = extract_dicom_metadata(&metadata_test_object());
        let row_identity: Vec<_> = metadata
            .curated_items
            .iter()
            .map(|item| (item.tag.as_str(), item.description.as_str()))
            .collect();

        assert_eq!(row_identity, EXPECTED_CURATED_ROWS);

        let leading_rows: Vec<_> = metadata
            .curated_items
            .iter()
            .take(6)
            .map(|item| {
                (
                    item.tag.as_str(),
                    item.description.as_str(),
                    item.value.as_str(),
                )
            })
            .collect();

        assert_eq!(
            leading_rows,
            [
                ("-", "File Type", "DICOM"),
                (
                    "(0002,0010)",
                    "Transfer Syntax UID",
                    "Explicit VR Little Endian (1.2.840.10008.1.2.1)",
                ),
                (
                    "(0002,0002)",
                    "Media Storage SOP Class UID",
                    "CT Image Storage (1.2.840.10008.5.1.4.1.1.2)",
                ),
                ("(0002,0003)", "Media Storage SOP Instance UID", "2.25.101"),
                ("(0010,0010)", "Patient Name", "Doe^Jane"),
                ("(0010,0020)", "Patient ID", "-"),
            ]
        );

        let pixel_representation = metadata
            .curated_items
            .iter()
            .find(|item| item.tag == "(0028,0103)")
            .unwrap();
        assert_eq!(
            pixel_representation.value,
            "Signed two's-complement integer (1)"
        );
    }

    #[test]
    fn all_metadata_combines_file_meta_and_dataset_in_tag_order() {
        let metadata = extract_dicom_metadata(&metadata_test_object());

        assert!(
            metadata
                .all_items
                .windows(2)
                .all(|items| items[0].tag <= items[1].tag)
        );

        let transfer_syntax = metadata
            .all_items
            .iter()
            .find(|item| item.tag == "(0002,0010)")
            .unwrap();
        assert_eq!(
            transfer_syntax.value,
            "Explicit VR Little Endian (1.2.840.10008.1.2.1)"
        );

        let patient_name = metadata
            .all_items
            .iter()
            .find(|item| item.tag == "(0010,0010)")
            .unwrap();
        assert_eq!(patient_name.description, "PatientName");
        assert_eq!(patient_name.value, "Doe^Jane");
    }
}
