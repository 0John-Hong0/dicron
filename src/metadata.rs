use dicom_core::header::{HasLength, Header};
use dicom_core::value::Value;
use dicom_core::{DataDictionary, dictionary::DataDictionaryEntry};
use dicom_dictionary_std::StandardDataDictionary;
use dicom_object::DefaultDicomObject;

use crate::dicom::value as dicom_value;

const MAX_INLINE_METADATA_VALUE_BYTES: u32 = 4096;

#[derive(Clone)]
pub struct MetadataItem {
    pub tag: String,
    pub description: String,
    pub value: String,
    // Lowercased "tag\ndescription\nvalue", precomputed once so filtering does
    // not re-lowercase every field of every tag on every UI frame.
    search_haystack: String,
}

impl MetadataItem {
    pub fn new(tag: String, description: String, value: String) -> Self {
        let search_haystack = format!("{tag}\n{description}\n{value}").to_lowercase();

        Self {
            tag,
            description,
            value,
            search_haystack,
        }
    }

    /// `search_text` is expected to already be lowercased by the caller.
    pub fn matches_search(&self, search_text: &str) -> bool {
        self.search_haystack.contains(search_text)
    }
}

#[derive(Clone)]
pub struct DicomMetadata {
    pub curated_items: Vec<MetadataItem>,
    pub all_items: Vec<MetadataItem>,
}

pub fn extract_dicom_metadata(dicom_object: &DefaultDicomObject) -> DicomMetadata {
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
            dicom_object.meta().transfer_syntax().to_owned(),
        ),
        MetadataItem::new(
            "(0002,0002)".to_owned(),
            "Media Storage SOP Class UID".to_owned(),
            dicom_object.meta().media_storage_sop_class_uid().to_owned(),
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

    let cleaned_value = dicom_value::clean_text_value(raw_value.as_ref());

    if cleaned_value.is_empty() {
        "-".to_owned()
    } else {
        cleaned_value
    }
}

fn summarize_large_value(label: &str, length: dicom_core::Length) -> String {
    match length.get() {
        Some(byte_count) => format!("<{label}: {byte_count} bytes>"),
        None => format!("<{label}: undefined length>"),
    }
}
