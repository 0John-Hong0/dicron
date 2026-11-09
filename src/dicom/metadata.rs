//! UI-independent extraction and formatting of DICOM metadata.

mod model {
    use std::collections::HashMap;

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
        /// The height of one pixel over its width, or `None` for square pixels.
        pub(crate) pixel_aspect_ratio: Option<f64>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum MetadataItemKind {
        Element,
        Sequence,
        Item,
    }

    #[derive(Clone)]
    pub(crate) struct MetadataItem {
        pub(crate) tag: String,
        pub(crate) description: String,
        pub(crate) value: String,
        pub(crate) path: String,
        pub(crate) kind: MetadataItemKind,
        pub(crate) children: Vec<MetadataItem>,
        /// How many items before this one in its tree share its path. A file
        /// can repeat an element, such as a File Meta element inside its data
        /// set, so only the path together with this names one item.
        pub(crate) occurrence: usize,
        // Precompute the normalized text once so filtering stays cheap per UI frame.
        search_haystack: String,
    }

    impl MetadataItem {
        pub(crate) fn new(tag: String, description: String, value: String) -> Self {
            let path = tag.clone();
            Self::with_structure(
                tag,
                description,
                value,
                path,
                MetadataItemKind::Element,
                Vec::new(),
            )
        }

        pub(crate) fn with_structure(
            tag: String,
            description: String,
            value: String,
            path: String,
            kind: MetadataItemKind,
            children: Vec<MetadataItem>,
        ) -> Self {
            let search_haystack = format!("{tag}\n{description}\n{value}\n{path}").to_lowercase();

            Self {
                tag,
                description,
                value,
                path,
                kind,
                children,
                occurrence: 0,
                search_haystack,
            }
        }

        /// Numbers the items of a tree that share a path, in tree order.
        pub(crate) fn number_repeated_paths(items: &mut [MetadataItem]) {
            fn number(items: &mut [MetadataItem], seen: &mut HashMap<String, usize>) {
                for item in items {
                    let count = seen.entry(item.path.clone()).or_default();
                    item.occurrence = *count;
                    *count += 1;
                    number(&mut item.children, seen);
                }
            }
            number(items, &mut HashMap::new());
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
        use super::{MetadataItem, MetadataItemKind};

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

        /// Items repeat a path when a repeated element brings its sequence
        /// items along; each repeat counts on from the ones before it, in tree
        /// order, while distinct paths stay first.
        #[test]
        fn repeated_paths_are_numbered_in_tree_order_at_every_depth() {
            let sequence = || {
                let leaf = MetadataItem::with_structure(
                    "(0008,0100)".to_owned(),
                    "CodeValue".to_owned(),
                    String::new(),
                    "(0008,1032) / Item 1 / (0008,0100)".to_owned(),
                    MetadataItemKind::Element,
                    Vec::new(),
                );
                MetadataItem::with_structure(
                    "(0008,1032)".to_owned(),
                    "Sequence".to_owned(),
                    String::new(),
                    "(0008,1032)".to_owned(),
                    MetadataItemKind::Sequence,
                    vec![MetadataItem::with_structure(
                        String::new(),
                        "Item 1".to_owned(),
                        String::new(),
                        "(0008,1032) / Item 1".to_owned(),
                        MetadataItemKind::Item,
                        vec![leaf],
                    )],
                )
            };
            let other = MetadataItem::new(
                "(0010,0010)".to_owned(),
                "PatientName".to_owned(),
                String::new(),
            );
            let mut items = vec![sequence(), other, sequence()];
            MetadataItem::number_repeated_paths(&mut items);

            fn flatten(items: &[MetadataItem], result: &mut Vec<(String, usize)>) {
                for item in items {
                    result.push((item.path.clone(), item.occurrence));
                    flatten(&item.children, result);
                }
            }
            let mut numbered = Vec::new();
            flatten(&items, &mut numbered);
            let expected = [
                ("(0008,1032)", 0),
                ("(0008,1032) / Item 1", 0),
                ("(0008,1032) / Item 1 / (0008,0100)", 0),
                ("(0010,0010)", 0),
                ("(0008,1032)", 1),
                ("(0008,1032) / Item 1", 1),
                ("(0008,1032) / Item 1 / (0008,0100)", 1),
            ]
            .map(|(path, occurrence)| (path.to_owned(), occurrence));
            assert_eq!(numbered, expected);
        }
    }
}

mod overlay {
    use dicom_object::DefaultDicomObject;

    use super::super::elements::{element_text, first_parsed, parsed_values};
    use super::model::DicomOverlayMetadata;

    /// The most times taller than wide, or wider than tall, a pixel is shown.
    /// Proportions beyond this are not a real pixel's, and would overflow or
    /// vanish in the viewer's layout.
    const MAX_PIXEL_ASPECT_RATIO: f64 = 1000.0;

    pub(super) fn extract_overlay_metadata(
        dicom_object: &DefaultDicomObject,
    ) -> DicomOverlayMetadata {
        let patient_name =
            element_text(dicom_object, "PatientName").map(|name| name.replace('^', " "));
        let patient_id = element_text(dicom_object, "PatientID");
        let patient_label = match (patient_name, patient_id) {
            (Some(name), Some(id)) => Some(format!("{name} ({id})")),
            (Some(name), None) => Some(name),
            (None, Some(id)) => Some(id),
            (None, None) => None,
        };

        let slice_thickness =
            first_parsed::<f64>(dicom_object, "SliceThickness").filter(|value| value.is_finite());
        let slice_location =
            first_parsed::<f64>(dicom_object, "SliceLocation").filter(|value| value.is_finite());
        // Both list the vertical value first: the distance between rows, then
        // between columns.
        let pixel_aspect_ratio =
            ["PixelSpacing", "PixelAspectRatio"]
                .into_iter()
                .find_map(|keyword| {
                    let [row_value, column_value] = parsed_values(dicom_object, keyword)?;
                    let ratio = row_value / column_value;

                    (row_value > 0.0
                        && column_value > 0.0
                        && (1.0 / MAX_PIXEL_ASPECT_RATIO..=MAX_PIXEL_ASPECT_RATIO).contains(&ratio))
                    .then_some(ratio)
                });

        DicomOverlayMetadata {
            patient_label,
            study_description: element_text(dicom_object, "StudyDescription"),
            series_description: element_text(dicom_object, "SeriesDescription"),
            manufacturer: element_text(dicom_object, "Manufacturer"),
            modality: element_text(dicom_object, "Modality"),
            study_date: element_text(dicom_object, "StudyDate"),
            study_time: element_text(dicom_object, "StudyTime"),
            slice_thickness,
            slice_location,
            image_orientation: parsed_values(dicom_object, "ImageOrientationPatient"),
            pixel_aspect_ratio,
        }
    }

    #[cfg(test)]
    mod tests {
        use dicom_core::{DataElement, PrimitiveValue, VR};
        use dicom_dictionary_std::{tags, uids};
        use dicom_object::{FileDicomObject, FileMetaTableBuilder};

        use super::extract_overlay_metadata;

        fn pixel_aspect_ratio(spacing: Option<&str>, aspect_ratio: Option<&str>) -> Option<f64> {
            let meta = FileMetaTableBuilder::new()
                .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                .media_storage_sop_class_uid(uids::SECONDARY_CAPTURE_IMAGE_STORAGE)
                .media_storage_sop_instance_uid("2.25.102")
                .build()
                .unwrap();
            let mut object = FileDicomObject::new_empty_with_meta(meta);
            if let Some(spacing) = spacing {
                object.put_element(DataElement::new(
                    tags::PIXEL_SPACING,
                    VR::DS,
                    PrimitiveValue::from(spacing),
                ));
            }
            if let Some(aspect_ratio) = aspect_ratio {
                object.put_element(DataElement::new(
                    tags::PIXEL_ASPECT_RATIO,
                    VR::IS,
                    PrimitiveValue::from(aspect_ratio),
                ));
            }

            extract_overlay_metadata(&object).pixel_aspect_ratio
        }

        #[test]
        fn pixel_proportions_come_from_spacing_then_aspect_ratio() {
            assert_eq!(
                pixel_aspect_ratio(Some("0.5\\0.25"), Some("1\\3")),
                Some(2.0)
            );
            assert_eq!(pixel_aspect_ratio(None, Some("4\\3")), Some(4.0 / 3.0));
            // Spacing that cannot be a pixel's size gives way to the ratio.
            for unusable in ["0\\1", "-1\\1", "1", "1\\x"] {
                assert_eq!(
                    pixel_aspect_ratio(Some(unusable), Some("1\\2")),
                    Some(0.5),
                    "{unusable}"
                );
            }
            assert_eq!(pixel_aspect_ratio(Some("0\\0"), None), None);
            // Values that are not finite sizes, or that are not one pair,
            // also give way.
            for unusable in [
                "NaN\\1",
                "1\\inf",
                "1\\2\\3",
                "1\\-2",
                // Both negative: their ratio is in range, but no pixel has
                // a negative size.
                "-1\\-2",
                "1e300\\1e-300",
                "1e-300\\1e300",
            ] {
                assert_eq!(
                    pixel_aspect_ratio(Some(unusable), Some("3\\1")),
                    Some(3.0),
                    "{unusable}"
                );
            }
            assert_eq!(pixel_aspect_ratio(Some("1\\1"), Some("1\\2")), Some(1.0));
            assert_eq!(pixel_aspect_ratio(None, Some("0\\1")), None);
            // Pixels up to a thousand times taller than wide, or wider than
            // tall, are shown as they are.
            assert_eq!(pixel_aspect_ratio(Some("1000\\1"), None), Some(1000.0));
            assert_eq!(pixel_aspect_ratio(Some("1\\1000"), None), Some(0.001));
            // Beyond that, the source gives way to the next one, then to
            // square pixels.
            for unusable in ["1001\\1", "1\\1001", "1e39\\1", "1e-50\\1"] {
                assert_eq!(
                    pixel_aspect_ratio(Some(unusable), Some("3\\1")),
                    Some(3.0),
                    "{unusable}"
                );
                assert_eq!(pixel_aspect_ratio(Some(unusable), None), None, "{unusable}");
                assert_eq!(pixel_aspect_ratio(None, Some(unusable)), None, "{unusable}");
            }
            assert_eq!(pixel_aspect_ratio(None, None), None);
        }
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

    use super::super::elements::g1_read_as_latin_1;
    use super::super::text_encoding::decode_iso_2022_text;

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

        get_dicom_element_text_value(element, g1_read_as_latin_1(dicom_object))
    }

    /// `g1_read_as_latin_1` tells how the parser read ISO 2022 G1 bytes in the element's data
    /// set, see `text_encoding::iso_2022_g1_read_as_latin_1`.
    pub(super) fn get_dicom_element_text_value<I, P>(
        element: &dicom_core::DataElement<I, P>,
        g1_read_as_latin_1: bool,
    ) -> String
    where
        I: HasLength,
    {
        let value_length = element.length();

        if value_length.get() == Some(0) {
            return "-".to_owned();
        }

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
            && !matches!(
                element.vr(),
                dicom_core::VR::AE
                    | dicom_core::VR::AS
                    | dicom_core::VR::CS
                    | dicom_core::VR::DA
                    | dicom_core::VR::DS
                    | dicom_core::VR::DT
                    | dicom_core::VR::IS
                    | dicom_core::VR::LO
                    | dicom_core::VR::LT
                    | dicom_core::VR::PN
                    | dicom_core::VR::SH
                    | dicom_core::VR::ST
                    | dicom_core::VR::TM
                    | dicom_core::VR::UC
                    | dicom_core::VR::UI
                    | dicom_core::VR::UR
                    | dicom_core::VR::UT
            )
        {
            return summarize_large_value("large value", value_length);
        }

        let Ok(raw_value) = element.to_str() else {
            return "<non-text value>".to_owned();
        };
        let raw_value = decode_iso_2022_text(&raw_value, g1_read_as_latin_1);

        // These single-valued text VRs may contain literal backslashes and significant line
        // breaks. Keep their contents intact for display, search, and copying.
        if matches!(
            element.vr(),
            dicom_core::VR::LT | dicom_core::VR::ST | dicom_core::VR::UT | dicom_core::VR::UR
        ) {
            let text = raw_value.trim_end_matches(['\0', ' ']);
            return if text.is_empty() {
                "-".to_owned()
            } else {
                text.to_owned()
            };
        }

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

        #[test]
        fn zero_length_values_stay_empty_including_sequences_and_pixel_data() {
            use dicom_core::{DataElement, VR};
            use dicom_object::InMemDicomObject;

            for (tag, vr) in [
                (tags::PIXEL_DATA, VR::OW),
                (tags::REFERENCED_IMAGE_SEQUENCE, VR::SQ),
                (tags::ROWS, VR::US),
                (tags::PATIENT_NAME, VR::PN),
            ] {
                let element: DataElement<InMemDicomObject> = DataElement::empty(tag, vr);
                assert_eq!(super::get_dicom_element_text_value(&element, false), "-");
            }
        }
    }
}

mod curated {
    use dicom_dictionary_std::tags;
    use dicom_object::{DefaultDicomObject, InMemDicomObject};

    use super::format::{get_dicom_element_text_value, get_dicom_text_value};
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
        original_meta: Option<&InMemDicomObject>,
    ) -> Vec<MetadataItem> {
        let mut metadata_items = Vec::with_capacity(4 + CURATED_TAGS.len());

        metadata_items.push(MetadataItem::new(
            "-".to_owned(),
            "File Type".to_owned(),
            "DICOM".to_owned(),
        ));
        for (tag, description) in [
            (tags::TRANSFER_SYNTAX_UID, "Transfer Syntax UID"),
            (
                tags::MEDIA_STORAGE_SOP_CLASS_UID,
                "Media Storage SOP Class UID",
            ),
            (
                tags::MEDIA_STORAGE_SOP_INSTANCE_UID,
                "Media Storage SOP Instance UID",
            ),
        ] {
            let value = original_meta.and_then(|meta| meta.get(tag)).map_or_else(
                || "-".to_owned(),
                |element| get_dicom_element_text_value(element, false),
            );
            metadata_items.push(MetadataItem::new(
                tag.to_string(),
                description.to_owned(),
                value,
            ));
        }

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
    use dicom_core::value::Value;
    use dicom_object::mem::InMemElement;
    use dicom_object::{DefaultDicomObject, InMemDicomObject};

    use super::super::elements::g1_read_as_latin_1;
    use super::curated::extract_curated_dicom_metadata;
    use super::format::{build_dicom_tag_description, get_dicom_element_text_value};
    use super::model::{DicomMetadata, MetadataItem, MetadataItemKind};
    use super::overlay::extract_overlay_metadata;

    pub(crate) fn extract_dicom_metadata(
        dicom_object: &DefaultDicomObject,
        original_meta: Option<&InMemDicomObject>,
    ) -> DicomMetadata {
        DicomMetadata {
            curated_items: extract_curated_dicom_metadata(dicom_object, original_meta),
            all_items: extract_all_dicom_metadata(dicom_object, original_meta),
            overlay: extract_overlay_metadata(dicom_object),
        }
    }

    fn extract_all_dicom_metadata(
        dicom_object: &DefaultDicomObject,
        original_meta: Option<&InMemDicomObject>,
    ) -> Vec<MetadataItem> {
        let g1_read_as_latin_1 = g1_read_as_latin_1(dicom_object);
        let mut metadata_items: Vec<MetadataItem> = original_meta
            .into_iter()
            .flat_map(InMemDicomObject::iter)
            .map(|element| extract_element(element, "", false))
            .chain(
                dicom_object
                    .iter()
                    .map(|element| extract_element(element, "", g1_read_as_latin_1)),
            )
            .collect();

        metadata_items.sort_by(|left, right| left.tag.cmp(&right.tag));
        MetadataItem::number_repeated_paths(&mut metadata_items);

        metadata_items
    }

    fn extract_element(
        element: &InMemElement,
        parent_path: &str,
        g1_read_as_latin_1: bool,
    ) -> MetadataItem {
        let tag = element.tag().to_string();
        let path = if parent_path.is_empty() {
            tag.clone()
        } else {
            format!("{parent_path} / {tag}")
        };
        let description = build_dicom_tag_description(element.tag(), element.vr().to_string());
        let Value::Sequence(sequence) = element.value() else {
            return MetadataItem::with_structure(
                tag,
                description,
                get_dicom_element_text_value(element, g1_read_as_latin_1),
                path,
                MetadataItemKind::Element,
                Vec::new(),
            );
        };
        let children = sequence
            .items()
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let label = format!("Item {}", index + 1);
                let item_path = format!("{path} / {label}");
                let elements: Vec<_> = item
                    .iter()
                    .map(|child| extract_element(child, &item_path, g1_read_as_latin_1))
                    .collect();
                MetadataItem::with_structure(
                    String::new(),
                    label,
                    format!("{} elements", elements.len()),
                    item_path,
                    MetadataItemKind::Item,
                    elements,
                )
            })
            .collect();
        MetadataItem::with_structure(
            tag,
            description,
            format!("{} items", sequence.items().len()),
            path,
            MetadataItemKind::Sequence,
            children,
        )
    }

    #[cfg(test)]
    mod tests {
        use dicom_core::header::Header;
        use dicom_core::{DataElement, VR, dicom_value};
        use dicom_dictionary_std::{tags, uids};
        use dicom_object::{
            DefaultDicomObject, FileDicomObject, FileMetaTableBuilder, InMemDicomObject,
        };

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

        fn metadata_test_file_meta(object: &DefaultDicomObject) -> InMemDicomObject {
            InMemDicomObject::from_element_iter(object.meta().to_element_iter().map(|element| {
                DataElement::new(
                    element.tag(),
                    element.vr(),
                    element.into_value().into_primitive().unwrap(),
                )
            }))
        }

        #[test]
        fn curated_metadata_preserves_order_labels_and_placeholders() {
            let object = metadata_test_object();
            let original_meta = metadata_test_file_meta(&object);
            let metadata = extract_dicom_metadata(&object, Some(&original_meta));
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
            let object = metadata_test_object();
            let original_meta = metadata_test_file_meta(&object);
            let metadata = extract_dicom_metadata(&object, Some(&original_meta));

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

        /// A data set that repeats a File Meta element lists both, and the
        /// occurrence tells the two rows apart; every other row is the first
        /// with its path.
        #[test]
        fn all_metadata_numbers_a_file_meta_element_repeated_in_the_dataset() {
            let mut object = metadata_test_object();
            object.put_element(DataElement::new(
                tags::IMPLEMENTATION_VERSION_NAME,
                VR::SH,
                dicom_value!(Str, "DATASETCOPY"),
            ));
            let mut original_meta = metadata_test_file_meta(&object);
            original_meta.put_element(DataElement::new(
                tags::IMPLEMENTATION_VERSION_NAME,
                VR::SH,
                dicom_value!(Str, "METACOPY"),
            ));
            let metadata = extract_dicom_metadata(&object, Some(&original_meta));

            let repeated: Vec<_> = metadata
                .all_items
                .iter()
                .filter(|item| item.path == "(0002,0013)")
                .map(|item| (item.value.as_str(), item.occurrence))
                .collect();
            assert_eq!(repeated, [("METACOPY", 0), ("DATASETCOPY", 1)]);
            assert!(
                metadata
                    .all_items
                    .iter()
                    .filter(|item| item.path != "(0002,0013)")
                    .all(|item| item.occurrence == 0)
            );
        }

        #[test]
        fn nested_metadata_preserves_item_order_paths_empty_values_and_full_text() {
            use super::super::model::MetadataItemKind;
            use dicom_core::{Length, Tag, value::Value};

            let mut object = metadata_test_object();
            let long_text = format!(
                "{}\\literal\nlast searchable needle",
                "Long text ".repeat(600)
            );
            let deep = InMemDicomObject::from_element_iter([DataElement::new(
                tags::REFERENCED_SOP_INSTANCE_UID,
                VR::UI,
                "2.25.303",
            )]);
            let first = InMemDicomObject::from_element_iter([
                DataElement::new(tags::CODE_VALUE, VR::SH, "first"),
                DataElement::empty(tags::PATIENT_ID, VR::LO),
                DataElement::new(
                    tags::REFERENCED_IMAGE_SEQUENCE,
                    VR::SQ,
                    Value::new_sequence(vec![deep], Length::UNDEFINED),
                ),
                DataElement::new(Tag(0x7777, 0x1001), VR::UT, long_text.clone()),
            ]);
            let third = InMemDicomObject::from_element_iter([DataElement::new(
                tags::CODE_VALUE,
                VR::SH,
                "third",
            )]);
            object.put_element(DataElement::new(
                tags::PROCEDURE_CODE_SEQUENCE,
                VR::SQ,
                Value::new_sequence(
                    vec![first, InMemDicomObject::new_empty(), third],
                    Length::UNDEFINED,
                ),
            ));
            object.put_element(DataElement::empty(tags::REFERENCED_STUDY_SEQUENCE, VR::SQ));
            object.put_element(DataElement::new(
                tags::REFERENCED_SERIES_SEQUENCE,
                VR::SQ,
                Value::new_sequence(Vec::<InMemDicomObject>::new(), Length::UNDEFINED),
            ));
            let mut original = Vec::new();
            object.write_dataset(&mut original).unwrap();

            let metadata = extract_dicom_metadata(&object, None);
            let sequence = metadata
                .all_items
                .iter()
                .find(|item| item.tag == "(0008,1032)")
                .unwrap();
            assert_eq!(sequence.kind, MetadataItemKind::Sequence);
            assert_eq!(sequence.value, "3 items");
            assert_eq!(sequence.children.len(), 3);
            assert_eq!(
                sequence
                    .children
                    .iter()
                    .map(|item| item.description.as_str())
                    .collect::<Vec<_>>(),
                ["Item 1", "Item 2", "Item 3"]
            );
            assert!(
                sequence
                    .children
                    .iter()
                    .all(|item| item.kind == MetadataItemKind::Item && item.tag.is_empty())
            );

            let first = &sequence.children[0];
            let third = &sequence.children[2];
            let code = |item: &super::MetadataItem| {
                item.children
                    .iter()
                    .find(|child| child.tag == "(0008,0100)")
                    .unwrap()
                    .value
                    .clone()
            };
            assert_eq!(code(first), "first");
            assert_eq!(code(third), "third");
            let first_code = first
                .children
                .iter()
                .find(|item| item.tag == "(0008,0100)")
                .unwrap();
            let third_code = third
                .children
                .iter()
                .find(|item| item.tag == "(0008,0100)")
                .unwrap();
            assert_eq!(first_code.path, "(0008,1032) / Item 1 / (0008,0100)");
            assert_ne!(first_code.path, third_code.path);
            assert!(
                first
                    .children
                    .iter()
                    .any(|item| item.tag == "(0010,0020)" && item.value == "-")
            );
            assert!(sequence.children[1].children.is_empty());
            assert_eq!(sequence.children[1].value, "0 elements");

            let inner = first
                .children
                .iter()
                .find(|item| item.tag == "(0008,1140)")
                .unwrap();
            let reference = &inner.children[0].children[0];
            assert_eq!(reference.value, "2.25.303");
            assert_eq!(
                reference.path,
                "(0008,1032) / Item 1 / (0008,1140) / Item 1 / (0008,1155)"
            );
            let text = first
                .children
                .iter()
                .find(|item| item.tag == "(7777,1001)")
                .unwrap();
            assert_eq!(text.value, long_text);
            assert!(text.matches_search("last searchable needle"));
            assert!(text.matches_search("\\literal\n"));
            for tag in ["(0008,1110)", "(0008,1115)"] {
                let empty = metadata
                    .all_items
                    .iter()
                    .find(|item| item.tag == tag)
                    .unwrap();
                assert_eq!(empty.kind, MetadataItemKind::Sequence);
                assert_eq!(empty.value, "0 items");
                assert!(empty.children.is_empty());
            }
            assert!(
                metadata
                    .curated_items
                    .iter()
                    .any(|item| item.tag == "(0010,0020)" && item.value == "-")
            );
            let mut after = Vec::new();
            object.write_dataset(&mut after).unwrap();
            assert_eq!(after, original);
        }
    }
}

pub(crate) use extract::extract_dicom_metadata;
pub(crate) use model::{DicomMetadata, DicomOverlayMetadata, MetadataItem, MetadataItemKind};

fn clean_text_value(raw_value: &str) -> String {
    raw_value.trim().trim_matches('\0').replace('\\', ", ")
}
