//! UI-independent extraction and formatting of DICOM metadata.

mod model {
    #[derive(Clone, Default)]
    pub(crate) struct DicomOverlayMetadata {
        pub(crate) patient_label: Option<String>,
        pub(crate) study_description: Option<String>,
        pub(crate) series_description: Option<String>,
        pub(crate) manufacturer: Option<String>,
        pub(crate) modality: Option<String>,
        pub(crate) study_date: Option<String>,
        pub(crate) study_time: Option<String>,
        pub(crate) slice_thickness: Option<f64>,
        pub(crate) slice_location: Option<f64>,
        pub(crate) image_orientation: Option<[f64; 6]>,
    }

    #[derive(Clone)]
    pub(crate) struct MetadataItem {
        pub(crate) tag: String,
        pub(crate) description: String,
        pub(crate) value: String,
        // Precompute the normalized text once so filtering stays cheap per UI frame.
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

        /// search_text is expected to already be lowercased by the caller.
        pub(crate) fn matches_search(&self, search_text: &str) -> bool {
            self.search_haystack.contains(search_text)
        }
    }

    #[derive(Clone)]
    pub(crate) struct DicomMetadata {
        pub(crate) curated_items: Vec<MetadataItem>,
        pub(crate) all_items: Vec<MetadataItem>,
        pub(crate) overlay: DicomOverlayMetadata,
    }

    #[cfg(test)]
    mod tests {
        use super::MetadataItem;

        #[test]
        fn friendly_and_raw_values_are_searchable() {
            let item = MetadataItem::new(
                "(0002,0010)".to_owned(),
                "Transfer Syntax UID".to_owned(),
                "Explicit VR Little Endian (1.2.840.10008.1.2.1)".to_owned(),
            );

            assert!(item.matches_search("explicit vr little endian"));
            assert!(item.matches_search("1.2.840.10008.1.2.1"));
        }
    }
}

mod overlay {
    use std::str::FromStr;

    use dicom_object::DefaultDicomObject;

    use super::model::DicomOverlayMetadata;

    pub(super) fn extract_overlay_metadata(
        dicom_object: &DefaultDicomObject,
    ) -> DicomOverlayMetadata {
        let patient_name =
            optional_text(dicom_object, "PatientName").map(|name| name.replace('^', " "));
        let patient_id = optional_text(dicom_object, "PatientID");
        let patient_label = match (patient_name, patient_id) {
            (Some(name), Some(id)) => Some(format!("{name} ({id})")),
            (Some(name), None) => Some(name),
            (None, Some(id)) => Some(id),
            (None, None) => None,
        };

        let image_position = parsed_values::<3>(dicom_object, "ImagePositionPatient");

        let slice_thickness =
            first_parsed::<f64>(dicom_object, "SliceThickness").filter(|value| value.is_finite());
        let slice_location = first_parsed::<f64>(dicom_object, "SliceLocation")
            .filter(|value| value.is_finite())
            .or_else(|| image_position.map(|position| position[2]));

        DicomOverlayMetadata {
            patient_label,
            study_description: optional_text(dicom_object, "StudyDescription"),
            series_description: optional_text(dicom_object, "SeriesDescription"),
            manufacturer: optional_text(dicom_object, "Manufacturer"),
            modality: optional_text(dicom_object, "Modality"),
            study_date: optional_text(dicom_object, "StudyDate"),
            study_time: optional_text(dicom_object, "StudyTime"),
            slice_thickness,
            slice_location,
            image_orientation: parsed_values(dicom_object, "ImageOrientationPatient"),
        }
    }

    fn optional_text(dicom_object: &DefaultDicomObject, keyword: &str) -> Option<String> {
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

    fn first_parsed<T>(dicom_object: &DefaultDicomObject, keyword: &str) -> Option<T>
    where
        T: FromStr,
    {
        optional_text(dicom_object, keyword)?
            .split('\\')
            .next()?
            .trim()
            .parse()
            .ok()
    }

    fn parsed_values<const N: usize>(
        dicom_object: &DefaultDicomObject,
        keyword: &str,
    ) -> Option<[f64; N]> {
        let text = optional_text(dicom_object, keyword)?;
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
}

mod format {
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

    pub(super) fn build_dicom_tag_description(tag: Tag, fallback: &str) -> String {
        StandardDataDictionary
            .by_tag(tag)
            .map(|entry| entry.alias().to_owned())
            .unwrap_or_else(|| fallback.to_owned())
    }

    pub(super) fn get_dicom_text_value(
        dicom_object: &DefaultDicomObject,
        dicom_keyword: &str,
    ) -> String {
        let Ok(element) = dicom_object.element_by_name(dicom_keyword) else {
            return "-".to_owned();
        };

        get_dicom_element_text_value(element)
    }

    pub(super) fn get_dicom_element_text_value<I, P>(
        element: &dicom_core::DataElement<I, P>,
    ) -> String
    where
        I: HasLength,
    {
        let value_length = element.length();

        if element.tag() == tags::PIXEL_DATA {
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

    pub(super) fn format_dicom_text_value(tag: Tag, raw_value: &str) -> String {
        let cleaned_value = super::clean_text_value(raw_value);

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

    #[cfg(test)]
    mod tests {
        use dicom_dictionary_std::tags;

        use super::format_dicom_text_value;

        #[test]
        fn formats_known_transfer_syntax_with_raw_uid() {
            assert_eq!(
                format_dicom_text_value(tags::TRANSFER_SYNTAX_UID, "1.2.840.10008.1.2.4.50\0",),
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
    }
}

mod curated {
    use dicom_dictionary_std::tags;
    use dicom_object::DefaultDicomObject;

    use super::format::{format_dicom_text_value, get_dicom_text_value};
    use super::model::MetadataItem;

    struct CuratedTag {
        tag: &'static str,
        description: &'static str,
        keyword: &'static str,
    }

    const CURATED_TAGS: &[CuratedTag] = &[
        CuratedTag {
            tag: "(0010,0010)",
            description: "Patient Name",
            keyword: "PatientName",
        },
        CuratedTag {
            tag: "(0010,0020)",
            description: "Patient ID",
            keyword: "PatientID",
        },
        CuratedTag {
            tag: "(0010,0040)",
            description: "Patient Sex",
            keyword: "PatientSex",
        },
        CuratedTag {
            tag: "(0010,0030)",
            description: "Patient Birth Date",
            keyword: "PatientBirthDate",
        },
        CuratedTag {
            tag: "(0008,0060)",
            description: "Modality",
            keyword: "Modality",
        },
        CuratedTag {
            tag: "(0008,0020)",
            description: "Study Date",
            keyword: "StudyDate",
        },
        CuratedTag {
            tag: "(0008,0030)",
            description: "Study Time",
            keyword: "StudyTime",
        },
        CuratedTag {
            tag: "(0008,1030)",
            description: "Study Description",
            keyword: "StudyDescription",
        },
        CuratedTag {
            tag: "(0008,103E)",
            description: "Series Description",
            keyword: "SeriesDescription",
        },
        CuratedTag {
            tag: "(0020,000D)",
            description: "Study Instance UID",
            keyword: "StudyInstanceUID",
        },
        CuratedTag {
            tag: "(0020,000E)",
            description: "Series Instance UID",
            keyword: "SeriesInstanceUID",
        },
        CuratedTag {
            tag: "(0008,0018)",
            description: "SOP Instance UID",
            keyword: "SOPInstanceUID",
        },
        CuratedTag {
            tag: "(0020,0011)",
            description: "Series Number",
            keyword: "SeriesNumber",
        },
        CuratedTag {
            tag: "(0020,0013)",
            description: "Instance Number",
            keyword: "InstanceNumber",
        },
        CuratedTag {
            tag: "(0028,0010)",
            description: "Rows",
            keyword: "Rows",
        },
        CuratedTag {
            tag: "(0028,0011)",
            description: "Columns",
            keyword: "Columns",
        },
        CuratedTag {
            tag: "(0028,0008)",
            description: "Number of Frames",
            keyword: "NumberOfFrames",
        },
        CuratedTag {
            tag: "(0028,0002)",
            description: "Samples Per Pixel",
            keyword: "SamplesPerPixel",
        },
        CuratedTag {
            tag: "(0028,0004)",
            description: "Photometric Interpretation",
            keyword: "PhotometricInterpretation",
        },
        CuratedTag {
            tag: "(0028,0100)",
            description: "Bits Allocated",
            keyword: "BitsAllocated",
        },
        CuratedTag {
            tag: "(0028,0101)",
            description: "Bits Stored",
            keyword: "BitsStored",
        },
        CuratedTag {
            tag: "(0028,0102)",
            description: "High Bit",
            keyword: "HighBit",
        },
        CuratedTag {
            tag: "(0028,0103)",
            description: "Pixel Representation",
            keyword: "PixelRepresentation",
        },
        CuratedTag {
            tag: "(0028,0030)",
            description: "Pixel Spacing",
            keyword: "PixelSpacing",
        },
        CuratedTag {
            tag: "(0018,0050)",
            description: "Slice Thickness",
            keyword: "SliceThickness",
        },
        CuratedTag {
            tag: "(0020,0032)",
            description: "Image Position Patient",
            keyword: "ImagePositionPatient",
        },
        CuratedTag {
            tag: "(0020,0037)",
            description: "Image Orientation Patient",
            keyword: "ImageOrientationPatient",
        },
        CuratedTag {
            tag: "(0028,1050)",
            description: "Window Center",
            keyword: "WindowCenter",
        },
        CuratedTag {
            tag: "(0028,1051)",
            description: "Window Width",
            keyword: "WindowWidth",
        },
        CuratedTag {
            tag: "(0028,1052)",
            description: "Rescale Intercept",
            keyword: "RescaleIntercept",
        },
        CuratedTag {
            tag: "(0028,1053)",
            description: "Rescale Slope",
            keyword: "RescaleSlope",
        },
    ];

    pub(super) fn extract_curated_dicom_metadata(
        dicom_object: &DefaultDicomObject,
    ) -> Vec<MetadataItem> {
        let mut metadata_items = Vec::with_capacity(4 + CURATED_TAGS.len());

        metadata_items.push(MetadataItem::new(
            "-".to_owned(),
            "File Type".to_owned(),
            "DICOM".to_owned(),
        ));
        metadata_items.push(MetadataItem::new(
            "(0002,0010)".to_owned(),
            "Transfer Syntax UID".to_owned(),
            format_dicom_text_value(
                tags::TRANSFER_SYNTAX_UID,
                dicom_object.meta().transfer_syntax(),
            ),
        ));
        metadata_items.push(MetadataItem::new(
            "(0002,0002)".to_owned(),
            "Media Storage SOP Class UID".to_owned(),
            format_dicom_text_value(
                tags::MEDIA_STORAGE_SOP_CLASS_UID,
                dicom_object.meta().media_storage_sop_class_uid(),
            ),
        ));
        metadata_items.push(MetadataItem::new(
            "(0002,0003)".to_owned(),
            "Media Storage SOP Instance UID".to_owned(),
            dicom_object
                .meta()
                .media_storage_sop_instance_uid()
                .to_owned(),
        ));

        metadata_items.extend(CURATED_TAGS.iter().map(|curated_tag| {
            MetadataItem::new(
                curated_tag.tag.to_owned(),
                curated_tag.description.to_owned(),
                get_dicom_text_value(dicom_object, curated_tag.keyword),
            )
        }));

        metadata_items
    }
}

mod extract {
    use dicom_core::header::Header;
    use dicom_object::DefaultDicomObject;

    use super::curated::extract_curated_dicom_metadata;
    use super::format::{build_dicom_tag_description, get_dicom_element_text_value};
    use super::model::{DicomMetadata, MetadataItem};
    use super::overlay::extract_overlay_metadata;

    pub(crate) fn extract_dicom_metadata(dicom_object: &DefaultDicomObject) -> DicomMetadata {
        DicomMetadata {
            curated_items: extract_curated_dicom_metadata(dicom_object),
            all_items: extract_all_dicom_metadata(dicom_object),
            overlay: extract_overlay_metadata(dicom_object),
        }
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

    #[cfg(test)]
    mod tests {
        use dicom_core::{DataElement, VR, dicom_value};
        use dicom_dictionary_std::{tags, uids};
        use dicom_object::{DefaultDicomObject, FileDicomObject, FileMetaTableBuilder};

        use super::extract_dicom_metadata;

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
                    ("(0002,0003)", "Media Storage SOP Instance UID", "2.25.101",),
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
}

pub(crate) use extract::extract_dicom_metadata;
pub(crate) use model::{DicomMetadata, DicomOverlayMetadata, MetadataItem};

fn clean_text_value(raw_value: &str) -> String {
    raw_value.trim().trim_matches('\0').replace('\\', ", ")
}
