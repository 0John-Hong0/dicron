//! Text interpretation selected for opened DICOM data.

use dicom_encoding::text::{SpecificCharacterSet, TextCodec};

pub(super) const MAX_DETECTION_TEXT_VALUE_BYTES: usize = 4096;
const MAX_DETECTION_TOTAL_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum TextEncoding {
    #[default]
    Auto,
    DicomDefault,
    KoreanEucKr,
    Utf8,
    JapaneseShiftJis,
}

impl TextEncoding {
    pub(crate) fn assumed_character_set(self) -> SpecificCharacterSet {
        match self {
            Self::Auto | Self::DicomDefault => SpecificCharacterSet::ISO_IR_6,
            Self::KoreanEucKr => SpecificCharacterSet::from_code("ISO 2022 IR 149")
                .expect("the DICOM Korean character set is supported"),
            Self::Utf8 => SpecificCharacterSet::ISO_IR_192,
            // dicom-encoding implements ISO_IR 13 with its Windows-31J codec.
            Self::JapaneseShiftJis => SpecificCharacterSet::from_code("ISO_IR 13")
                .expect("the DICOM Japanese character set is supported"),
        }
    }
}

/// Raw charset-sensitive values. Decode them only after confirming no charset was declared.
#[derive(Default)]
pub(super) struct AutoEncodingEvidence {
    values: Vec<Vec<u8>>,
    total_bytes: usize,
    rejected: bool,
}

impl AutoEncodingEvidence {
    pub(super) fn observe(&mut self, bytes: &[u8]) {
        if self.rejected || bytes.is_ascii() {
            return;
        }

        let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        if !bytes.iter().any(|byte| !byte.is_ascii()) {
            return;
        }

        // Buffer raw values until the parser has confirmed that no charset was declared.
        if bytes.len() > MAX_DETECTION_TEXT_VALUE_BYTES
            || self.total_bytes + bytes.len() > MAX_DETECTION_TOTAL_BYTES
        {
            self.rejected = true;
            return;
        }
        self.total_bytes += bytes.len();
        self.values.push(bytes.to_vec());
    }

    pub(super) fn detect(&self) -> Option<TextEncoding> {
        if self.rejected {
            return None;
        }

        let candidates = [
            (
                TextEncoding::Utf8,
                strict_utf8_count as fn(&[u8]) -> Option<usize>,
            ),
            (TextEncoding::KoreanEucKr, strict_hangul_count),
            (TextEncoding::JapaneseShiftJis, strict_japanese_kana_count),
        ];
        let mut detected = None;
        for (encoding, validator) in candidates {
            let mut count = 0;
            for bytes in &self.values {
                let Some(value_count) = validator(bytes) else {
                    count = 0;
                    break;
                };
                count += value_count;
            }
            if count >= 3 {
                if detected.is_some() {
                    return None;
                }
                detected = Some(encoding);
            }
        }
        detected
    }
}

fn strict_utf8_count(bytes: &[u8]) -> Option<usize> {
    let decoded = std::str::from_utf8(bytes).ok()?;
    if decoded
        .chars()
        .any(|ch| ch.is_control() || (!ch.is_ascii() && !ch.is_alphabetic()))
    {
        return None;
    }
    Some(decoded.chars().filter(|ch| !ch.is_ascii()).count())
}

fn strict_hangul_count(bytes: &[u8]) -> Option<usize> {
    // A valid UTF-8 value is ambiguous even if its bytes could form EUC-KR pairs.
    if std::str::from_utf8(bytes).is_ok() {
        return None;
    }

    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii() {
            if !(0x20..=0x7e).contains(&byte) {
                return None;
            }
            index += 1;
        } else if (0xa1..=0xfe).contains(&byte)
            && bytes
                .get(index + 1)
                .is_some_and(|next| (0xa1..=0xfe).contains(next))
        {
            index += 2;
        } else {
            return None;
        }
    }

    let codec = TextEncoding::KoreanEucKr.assumed_character_set();
    let decoded = codec.decode(bytes).ok()?;
    let encoded = codec.encode(&decoded).ok()?;
    if encoded != bytes
        || decoded
            .chars()
            .any(|ch| !ch.is_ascii() && !(('\u{ac00}'..='\u{d7a3}').contains(&ch)))
    {
        return None;
    }
    Some(
        decoded
            .chars()
            .filter(|ch| ('\u{ac00}'..='\u{d7a3}').contains(ch))
            .count(),
    )
}

fn strict_japanese_kana_count(bytes: &[u8]) -> Option<usize> {
    if std::str::from_utf8(bytes).is_ok() {
        return None;
    }

    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii() {
            if !(0x20..=0x7e).contains(&byte) {
                return None;
            }
            index += 1;
        } else if ((0x81..=0x9f).contains(&byte) || (0xe0..=0xfc).contains(&byte))
            && bytes
                .get(index + 1)
                .is_some_and(|next| (0x40..=0x7e).contains(next) || (0x80..=0xfc).contains(next))
        {
            index += 2;
        } else {
            // Single-byte half-width kana are too easy to confuse with Latin-1.
            return None;
        }
    }

    let codec = TextEncoding::JapaneseShiftJis.assumed_character_set();
    let decoded = codec.decode(bytes).ok()?;
    if codec.encode(&decoded).ok()? != bytes {
        return None;
    }
    let mut kana_count = 0;
    let mut non_ascii_count = 0;
    for ch in decoded.chars().filter(|ch| !ch.is_ascii()) {
        non_ascii_count += 1;
        if ('\u{3040}'..='\u{30ff}').contains(&ch) {
            kana_count += 1;
        } else if !(('\u{3000}'..='\u{303f}').contains(&ch)
            || ('\u{4e00}'..='\u{9fff}').contains(&ch))
        {
            return None;
        }
    }
    (kana_count * 2 >= non_ascii_count).then_some(kana_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_strict_korean_utf8_and_japanese_text() {
        let korean = TextEncoding::KoreanEucKr.assumed_character_set();
        let japanese = TextEncoding::JapaneseShiftJis.assumed_character_set();
        for (bytes, expected) in [
            (korean.encode("홍길동").unwrap(), TextEncoding::KoreanEucKr),
            ("Иванов".as_bytes().to_vec(), TextEncoding::Utf8),
            (
                japanese.encode("やまだたろう").unwrap(),
                TextEncoding::JapaneseShiftJis,
            ),
        ] {
            let mut evidence = AutoEncodingEvidence::default();
            evidence.observe(b"Plain ASCII");
            evidence.observe(&bytes);
            assert_eq!(evidence.detect(), Some(expected));
        }
    }

    #[test]
    fn rejects_ambiguous_or_weak_text() {
        let korean = TextEncoding::KoreanEucKr.assumed_character_set();
        let japanese = TextEncoding::JapaneseShiftJis.assumed_character_set();
        for bytes in [
            b"Plain ASCII".to_vec(),
            "é".as_bytes().to_vec(),
            vec![0xc8, 0xab, 0xc8, 0xab, 0xc8, 0x20],
            korean.encode("洪吉洞").unwrap(),
            japanese.encode("山田太郎").unwrap(),
        ] {
            let mut evidence = AutoEncodingEvidence::default();
            evidence.observe(&bytes);
            assert_eq!(evidence.detect(), None);
        }
    }
}
