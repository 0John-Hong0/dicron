//! Text interpretation selected for opened DICOM data.

use std::borrow::Cow;

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

/// The character set named by the extensions of a SpecificCharacterSet value whose first
/// value is empty, such as `\ISO 2022 IR 149`. dicom-parser switches character sets on the
/// first value only, so it keeps decoding such data with the character set it started with.
pub(super) fn declared_extension_character_set(raw_value: &[u8]) -> Option<SpecificCharacterSet> {
    let declaration = std::str::from_utf8(raw_value).ok()?;
    let mut values = declaration
        .trim_end_matches('\0')
        .split('\\')
        .map(str::trim);
    if !values.next()?.is_empty() {
        return None;
    }
    values
        .filter_map(SpecificCharacterSet::from_code)
        .find(|character_set| *character_set != SpecificCharacterSet::ISO_IR_6)
}

/// A multi-byte character set designated by an ISO 2022 escape sequence.
#[derive(Clone, Copy, PartialEq)]
enum Iso2022MultiByteSet {
    /// KS X 1001 in G1 (ESC $ ) C).
    KsX1001,
    /// GB 2312 in G1 (ESC $ ) A).
    Gb2312,
    /// JIS X 0208 in G0 (ESC $ B or ESC $ @).
    JisX0208,
    /// JIS X 0212 in G0 (ESC $ ( D).
    JisX0212,
}

impl Iso2022MultiByteSet {
    /// Decodes characters the parser read one byte each, or `None` when they do not form
    /// whole characters of this set.
    fn decode(self, run: &str) -> Option<String> {
        let bytes = run
            .chars()
            .map(|ch| u8::try_from(ch).ok())
            .collect::<Option<Vec<u8>>>()?;
        let (code, designation) = match self {
            Self::KsX1001 => ("ISO 2022 IR 149", b"".as_slice()),
            Self::Gb2312 => ("ISO 2022 IR 58", b"".as_slice()),
            // dicom-encoding implements ISO 2022 IR 87 with an ISO-2022-JP codec.
            Self::JisX0208 => ("ISO 2022 IR 87", b"\x1b$B".as_slice()),
            Self::JisX0212 => ("ISO 2022 IR 87", b"\x1b$(D".as_slice()),
        };
        let decoded = SpecificCharacterSet::from_code(code)?
            .decode(&[designation, &bytes].concat())
            .ok()?;
        // Every byte pair is one non-ASCII character. Undecodable bytes come out as ASCII
        // escapes such as \375.
        (decoded.chars().count() * 2 == bytes.len() && !decoded.chars().any(|ch| ch.is_ascii()))
            .then_some(decoded)
    }
}

/// Whether dicom-parser read the G1 bytes of text in a data set with this SpecificCharacterSet
/// value one Latin-1 character each. The parser decodes with the set named by the first value
/// and keeps the set it started with when that value is empty or unknown. For an empty first
/// value that is the extension's set (see `declared_extension_character_set`) or the set the
/// user chose, so its reading of G1 bytes is not known to be Latin-1.
pub(super) fn iso_2022_g1_read_as_latin_1(declaration: &str) -> bool {
    let first_value = declaration.split('\\').next().unwrap_or_default();
    SpecificCharacterSet::from_code(first_value.trim_matches([' ', '\0'])).is_some_and(
        |character_set| {
            character_set == SpecificCharacterSet::ISO_IR_6
                || character_set == SpecificCharacterSet::ISO_IR_100
        },
    )
}

/// Decoded DICOM text with ISO 2022 multi-byte segments decoded and escape sequences removed.
///
/// dicom-parser decodes text with the first value of SpecificCharacterSet only. Under a
/// single-byte set such as ISO 2022 IR 6 or IR 13, the bytes after a multi-byte designation
/// come out one character per byte: GL bytes as ASCII, and GR bytes as U+00A1..=U+00FE when
/// the set reads them as Latin-1 (`g1_read_as_latin_1`, see `iso_2022_g1_read_as_latin_1`).
/// Those characters are decoded here with the designated set. Text the parser already decoded,
/// such as Hangul or kanji, keeps its reading, and codecs such as the Korean one leave the
/// escape sequences in the text.
pub(super) fn decode_iso_2022_text(text: &str, g1_read_as_latin_1: bool) -> Cow<'_, str> {
    const ESCAPE: char = '\u{1b}';
    if !text.contains(ESCAPE) {
        return Cow::Borrowed(text);
    }

    let mut g0 = None;
    let mut g1 = None;
    let mut cleaned = String::with_capacity(text.len());
    let mut run = String::new();
    let mut run_set = None;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == ESCAPE {
            // An escape sequence is ESC, intermediate bytes 0x20..=0x2F, then one final byte.
            let mut intermediates = String::new();
            while let Some(intermediate) = chars.next_if(|next| (' '..='/').contains(next)) {
                intermediates.push(intermediate);
            }
            if intermediates.is_empty() {
                continue;
            }
            let final_byte = chars.next_if(|next| ('0'..='~').contains(next));
            match (intermediates.as_str(), final_byte) {
                ("$)", Some('C')) => g1 = Some(Iso2022MultiByteSet::KsX1001),
                ("$)", Some('A')) => g1 = Some(Iso2022MultiByteSet::Gb2312),
                ("$", Some('B' | '@')) => g0 = Some(Iso2022MultiByteSet::JisX0208),
                ("$(", Some('D')) => g0 = Some(Iso2022MultiByteSet::JisX0212),
                ("(" | "$" | "$(", _) => g0 = None,
                (")" | "-" | "$)" | "$-", _) => g1 = None,
                _ => {}
            }
            continue;
        }

        // PS3.5 6.1.2.5.3 makes the value-1 sets active again at value, line and PN group
        // delimiters. None of them can be a byte of a GR pair. G0 JIS X 0208 pairs can hold
        // '\\' and '=', but not control characters. Writers often designate a multi-byte set
        // once for a whole PN group, so both stay across the '^' between its components.
        if matches!(ch, '\\' | '=' | '\r' | '\n' | '\x0c' | '\t') {
            g1 = None;
        }
        if matches!(ch, '\r' | '\n' | '\x0c' | '\t') {
            g0 = None;
        }
        let set = if ('!'..='~').contains(&ch) {
            g0
        } else if g1_read_as_latin_1 && ('\u{a1}'..='\u{fe}').contains(&ch) {
            g1
        } else {
            None
        };
        if set != run_set {
            push_iso_2022_run(&mut cleaned, &run, run_set);
            run.clear();
            run_set = set;
        }
        run.push(ch);
    }
    push_iso_2022_run(&mut cleaned, &run, run_set);
    Cow::Owned(cleaned)
}

fn push_iso_2022_run(text: &mut String, run: &str, set: Option<Iso2022MultiByteSet>) {
    match set.and_then(|set| set.decode(run)) {
        Some(decoded) => text.push_str(&decoded),
        None => text.push_str(run),
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
            (TextEncoding::Utf8, self.count(strict_utf8_count)),
            (TextEncoding::KoreanEucKr, self.korean_count()),
            (
                TextEncoding::JapaneseShiftJis,
                self.count(strict_japanese_kana_count),
            ),
        ];
        let mut detected = None;
        for (encoding, count) in candidates {
            if count >= 3 {
                if detected.is_some() {
                    return None;
                }
                detected = Some(encoding);
            }
        }
        detected
    }

    /// The characters counted in every value, or 0 when a value is not valid.
    fn count(&self, validator: fn(&[u8]) -> Option<usize>) -> usize {
        self.values
            .iter()
            .map(|bytes| validator(bytes))
            .sum::<Option<usize>>()
            .unwrap_or(0)
    }

    /// The Hangul count of a file whose values are all EUC-KR Hangul. A value that is also
    /// valid UTF-8 is ambiguous. One value that cannot be UTF-8 name text, because it is not
    /// valid UTF-8 or reads as unlikely letters, rules UTF-8 out for the file. A value that
    /// reads as UTF-8 symbols does not: the symbols U+00A1..=U+00BF are the same bytes as the
    /// Hangul of EUC-KR row C2, so they favour neither reading.
    fn korean_count(&self) -> usize {
        let utf8_ruled_out = self
            .values
            .iter()
            .any(|bytes| match std::str::from_utf8(bytes) {
                Ok(text) => has_unlikely_name_letter(text),
                Err(_) => true,
            });
        if utf8_ruled_out {
            self.count(strict_hangul_count)
        } else {
            0
        }
    }
}

fn strict_utf8_count(bytes: &[u8]) -> Option<usize> {
    let decoded = std::str::from_utf8(bytes).ok()?;
    if decoded
        .chars()
        .any(|ch| ch.is_control() || (!ch.is_ascii() && !ch.is_alphabetic()))
        || (has_unlikely_name_letter(decoded) && strict_hangul_count(bytes).is_some())
    {
        return None;
    }
    Some(decoded.chars().filter(|ch| !ch.is_ascii()).count())
}

/// Whether UTF-8 text holds a letter that hardly occurs in names: Latin Extended-B (U+0180)
/// and beyond, except the Vietnamese horn letters. EUC-KR Hangul that happens to be valid
/// UTF-8 reads as U+00A1..=U+023F, so this separates such Hangul from European names.
fn has_unlikely_name_letter(text: &str) -> bool {
    text.chars()
        .any(|ch| ch >= '\u{180}' && !matches!(ch, 'Ơ' | 'ơ' | 'Ư' | 'ư'))
}

fn strict_hangul_count(bytes: &[u8]) -> Option<usize> {
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
    fn reads_the_character_set_of_iso_2022_declarations_with_an_empty_first_value() {
        for (declaration, expected) in [
            (b"\\ISO 2022 IR 149".as_slice(), Some("ISO_IR 149")),
            (b"\\ISO 2022 IR 87 ", Some("ISO_IR 87")),
            (b"\\ISO 2022 IR 87\\ISO 2022 IR 159", Some("ISO_IR 87")),
            (b"ISO 2022 IR 6\\ISO 2022 IR 149", None),
            (b"ISO_IR 192", None),
            (b"", None),
            (b"\\", None),
        ] {
            assert_eq!(
                declared_extension_character_set(declaration)
                    .map(|character_set| character_set.name()),
                expected.map(Cow::Borrowed),
                "{declaration:?}"
            );
        }
    }

    #[test]
    fn removes_iso_2022_escape_sequences_from_decoded_text() {
        for (decoded, expected) in [
            ("Hong^Gildong=\u{1b}$)C홍^길동", "Hong^Gildong=홍^길동"),
            ("\u{1b}$B山田\u{1b}(B^\u{1b}(J太郎", "山田^太郎"),
            ("lone\u{1b}escape", "loneescape"),
            ("Hong^Gildong=홍^길동", "Hong^Gildong=홍^길동"),
        ] {
            assert_eq!(decode_iso_2022_text(decoded, false), expected);
        }
    }

    fn detect_values(values: &[&[u8]]) -> Option<TextEncoding> {
        let mut evidence = AutoEncodingEvidence::default();
        for value in values {
            evidence.observe(value);
        }
        evidence.detect()
    }

    #[test]
    fn detects_korean_names_whose_bytes_read_as_unlikely_utf8_letters() {
        let korean = TextEncoding::KoreanEucKr.assumed_character_set();
        let name = korean.encode("황창호").unwrap();
        assert_eq!(std::str::from_utf8(&name), Ok("Ȳâȣ"));

        assert_eq!(detect_values(&[&name]), Some(TextEncoding::KoreanEucKr));
        let not_utf8 = korean.encode("홍길동").unwrap();
        assert_eq!(
            detect_values(&[&name, &not_utf8]),
            Some(TextEncoding::KoreanEucKr)
        );
    }

    #[test]
    fn keeps_undeclared_european_names_as_utf8() {
        for values in [
            ["Müller^Jörg", "Gómez", "Ibáñez"].as_slice(),
            &["Kožená^Eva", "Yılmaz"],
            // Vietnamese horn letters also form EUC-KR Hangul byte pairs.
            &["Trương^Lương"],
        ] {
            let values: Vec<&[u8]> = values.iter().map(|value| value.as_bytes()).collect();
            assert_eq!(
                detect_values(&values),
                Some(TextEncoding::Utf8),
                "{values:?}"
            );
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

    #[test]
    fn reads_padded_and_mixed_iso_2022_extension_declarations() {
        // Leading and trailing spaces of CS values are not significant.
        for (declaration, expected) in [
            (b" \\ISO 2022 IR 149".as_slice(), Some("ISO_IR 149")),
            (b"\\ ISO 2022 IR 149 ", Some("ISO_IR 149")),
            (b"\\ISO 2022 IR 6\\ISO 2022 IR 149", Some("ISO_IR 149")),
            (b"\\ISO 2022 IR 159\\ISO 2022 IR 149", Some("ISO_IR 149")),
            (b"\\ISO 2022 IR 149\0", Some("ISO_IR 149")),
            (b"\\ISO 2022 IR 159", None),
            (b"\\ISO 2022 IR 6", None),
        ] {
            assert_eq!(
                declared_extension_character_set(declaration)
                    .map(|character_set| character_set.name()),
                expected.map(Cow::Borrowed),
                "{declaration:?}"
            );
        }
    }

    #[test]
    fn keeps_undeclared_pinyin_names_with_tone_letters_as_utf8() {
        // Pinyin tone letters lie above U+0180, but their UTF-8 bytes are not EUC-KR Hangul.
        let values: [&[u8]; 2] = ["Lǚ^Xiǎomíng".as_bytes(), "Zhāng^Sān".as_bytes()];
        assert_eq!(detect_values(&values), Some(TextEncoding::Utf8));
    }

    #[test]
    fn removes_escape_sequences_at_value_edges_and_keeps_text_after_partial_ones() {
        for (decoded, expected) in [
            ("\u{1b}$)C", ""),
            ("Kim\u{1b}$", "Kim"),
            ("\u{1b}(홍", "홍"),
            ("\u{1b}$)CA\u{1b}(BC", "AC"),
        ] {
            assert_eq!(decode_iso_2022_text(decoded, true), expected, "{decoded:?}");
        }
    }

    #[test]
    fn korean_detection_survives_a_value_whose_bytes_also_read_as_latin_utf8() {
        // EUC-KR 체크 (also the UTF-8 text "üũ") next to EUC-KR 홍길동, which is not UTF-8,
        // reads as Korean rather than falling back to ISO_IR 6.
        let korean = TextEncoding::KoreanEucKr.assumed_character_set();
        let name = korean.encode("홍길동").unwrap();
        let description = korean.encode("체크").unwrap();
        assert_eq!(std::str::from_utf8(&description), Ok("üũ"));
        assert_eq!(
            detect_values(&[&name, &description]),
            Some(TextEncoding::KoreanEucKr)
        );
    }

    #[test]
    fn decodes_multi_byte_iso_2022_segments_that_a_single_byte_set_read() {
        for (decoded, expected) in [
            // KS X 1001 in G1 read as Latin-1 under ISO 2022 IR 6: EUC-KR 홍^길동.
            (
                "Hong^Gildong=\u{1b}$)C\u{c8}\u{ab}^\u{b1}\u{e6}\u{b5}\u{bf}",
                "Hong^Gildong=홍^길동",
            ),
            // GB 2312 in G1 read as Latin-1: GB 2312 张三.
            (
                "Zhang^San=\u{1b}$)A\u{d5}\u{c5}\u{c8}\u{fd}",
                "Zhang^San=张三",
            ),
            // JIS X 0208 in G0 read as ASCII, designated with ESC $ B and ESC $ @: 山田, 太郎.
            ("\u{1b}$B;3ED\u{1b}(B^\u{1b}$@B@O:\u{1b}(J", "山田^太郎"),
            // JIS X 0212 in G0 read as ASCII: 0x30 0x21 is 丂.
            ("\u{1b}$(D0!\u{1b}(B", "丂"),
            // Half-width katakana that ISO_IR 13 decoded stay next to the kanji.
            ("ﾔﾏﾀﾞ=\u{1b}$B;3ED\u{1b}(J", "ﾔﾏﾀﾞ=山田"),
        ] {
            assert_eq!(decode_iso_2022_text(decoded, true), expected, "{decoded:?}");
        }
    }

    #[test]
    fn keeps_text_that_is_not_a_whole_multi_byte_iso_2022_segment() {
        for (decoded, expected) in [
            // No escape sequence: nothing changes, whatever the characters.
            ("Müller^È«±æ;3ED", "Müller^È«±æ;3ED"),
            // Latin-1 letters before the Korean designation keep their reading.
            ("Müller=\u{1b}$)C\u{c8}\u{ab}", "Müller=홍"),
            // A lone byte of a two-byte set is not a character.
            ("\u{1b}$)C\u{c8}", "\u{c8}"),
            ("\u{1b}$B;3E\u{1b}(B", ";3E"),
            // ASCII after ESC ( B is single-byte text again.
            ("\u{1b}$B;3ED\u{1b}(B;3ED", "山田;3ED"),
        ] {
            assert_eq!(decode_iso_2022_text(decoded, true), expected, "{decoded:?}");
        }
        assert!(matches!(
            decode_iso_2022_text("Hong^Gildong=홍^길동", true),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn korean_detection_counts_values_that_are_also_valid_utf8_once_utf8_is_ruled_out() {
        let korean = TextEncoding::KoreanEucKr.assumed_character_set();
        let name = korean.encode("홍길동").unwrap();
        for description in ["처치", "창", "카", "채천"] {
            let description = korean.encode(description).unwrap();
            assert!(std::str::from_utf8(&description).is_ok());
            assert_eq!(
                detect_values(&[&name, &description]),
                Some(TextEncoding::KoreanEucKr)
            );
        }
        // Without a value that only EUC-KR reads, the low-range value still reads as UTF-8.
        let ambiguous = korean.encode("천창철").unwrap();
        assert_eq!(detect_values(&[&ambiguous]), Some(TextEncoding::Utf8));
        // Two Hangul that rule UTF-8 out, plus the ambiguous 처치, reach the threshold.
        let short_name = korean.encode("홍길").unwrap();
        let description = korean.encode("처치").unwrap();
        assert_eq!(
            detect_values(&[&short_name, &description]),
            Some(TextEncoding::KoreanEucKr)
        );
    }

    #[test]
    fn korean_detection_counts_valid_utf8_values_once_one_value_is_unlikely_utf8() {
        let korean = TextEncoding::KoreanEucKr.assumed_character_set();
        let encode = |text| korean.encode(text).unwrap();
        // EUC-KR 황창호 is the valid UTF-8 text "Ȳâȣ", which no name holds.
        let name = encode("황창호");
        for description in ["처치", "체크"] {
            let description = encode(description);
            assert!(std::str::from_utf8(&description).is_ok());
            assert_eq!(
                detect_values(&[&name, &description]),
                Some(TextEncoding::KoreanEucKr),
                "{description:?}"
            );
        }
        // Hangul of row C2 is valid UTF-8 for the symbols U+00A1..=U+00BF, so on its own
        // it favours neither reading; next to a value that rules UTF-8 out it is Korean.
        let row_c2 = ["짱", "째", "짜"].map(encode);
        for value in &row_c2 {
            let as_utf8 = std::str::from_utf8(value).unwrap();
            assert!(as_utf8.chars().all(|ch| ('¡'..='¿').contains(&ch)));
        }
        let row_c2: Vec<&[u8]> = row_c2.iter().map(Vec::as_slice).collect();
        assert_eq!(detect_values(&row_c2), None);
        let with_name = [row_c2.as_slice(), &[&name]].concat();
        assert_eq!(detect_values(&with_name), Some(TextEncoding::KoreanEucKr));
    }

    /// UTF-8 symbols such as the degree sign share their bytes with EUC-KR Hangul of row
    /// C2, so a UTF-8 file of European names and a temperature is not taken for Korean.
    #[test]
    fn utf8_symbols_do_not_turn_european_names_korean() {
        let values: [&[u8]; 4] = [
            "Müller^Jörg".as_bytes(),
            "Gómez".as_bytes(),
            "Ibáñez".as_bytes(),
            "37°C".as_bytes(),
        ];
        assert_ne!(detect_values(&values), Some(TextEncoding::KoreanEucKr));
    }

    #[test]
    fn keeps_undeclared_turkish_and_vietnamese_names_as_utf8() {
        for values in [
            ["Öztürk^Ayşe", "Çelik^Gül", "Yıldız"].as_slice(),
            &["Nguyễn^Văn", "Trần^Thị"],
        ] {
            let values: Vec<&[u8]> = values.iter().map(|value| value.as_bytes()).collect();
            assert_eq!(
                detect_values(&values),
                Some(TextEncoding::Utf8),
                "{values:?}"
            );
        }
    }

    #[test]
    fn adv_an_explicit_g1_designation_ends_a_korean_segment() {
        // 'ISO 2022 IR 100\ISO 2022 IR 149': a writer switches G1 back to Latin-1 with ESC - A.
        // Latin-1 letters after it keep their reading, and an unknown G1 set (ESC ) I) does too.
        for (decoded, expected) in [
            ("\u{1b}$)C\u{c8}\u{ab}\u{1b}-AGr\u{f6}\u{df}e", "홍Größe"),
            ("\u{1b}$)C\u{c8}\u{ab}\u{1b})I\u{f6}\u{df}", "홍öß"),
            // U+00FF is still a Latin-1 reading, so the Korean segment after it is decoded.
            ("Ha\u{ff}=\u{1b}$)C\u{c8}\u{ab}", "Haÿ=홍"),
            // GB 2312 ends the same way.
            ("\u{1b}$)A\u{d5}\u{c5}\u{1b}-A\u{f6}\u{df}", "张öß"),
        ] {
            assert_eq!(decode_iso_2022_text(decoded, true), expected, "{decoded:?}");
        }
    }

    #[test]
    fn g1_designation_does_not_carry_past_value_and_line_delimiters() {
        // 'ISO 2022 IR 100\ISO 2022 IR 149': PS3.5 6.1.2.5.3 makes the value-1 set active
        // again before '\', before CR/LF/FF/TAB and before PN '^' and '='. The Latin-1 letters
        // after the delimiter keep their reading instead of the KS X 1001 hanja "秤" (F6 DF).
        for (decoded, expected) in [
            ("\u{1b}$)C\u{c8}\u{ab}\\Gr\u{f6}\u{df}e", "홍\\Größe"),
            ("\u{1b}$)C\u{c8}\u{ab}\r\nGr\u{f6}\u{df}e", "홍\r\nGröße"),
            ("\u{1b}$)C\u{c8}\u{ab}\tGr\u{f6}\u{df}e", "홍\tGröße"),
            ("\u{1b}$)C\u{c8}\u{ab}\u{c}Gr\u{f6}\u{df}e", "홍\u{c}Größe"),
            (
                "Hong=\u{1b}$)C\u{c8}\u{ab}=Gr\u{f6}\u{df}e",
                "Hong=홍=Größe",
            ),
            // JIS X 0208 in G0 ends at a line end; ASCII follows.
            ("\u{1b}$B;3ED\r\nABCD", "山田\r\nABCD"),
        ] {
            assert_eq!(decode_iso_2022_text(decoded, true), expected, "{decoded:?}");
        }
    }

    #[test]
    fn korean_codec_symbols_are_not_re_decoded_as_hangul() {
        // '\ISO 2022 IR 149' text is decoded by the Korean codec, which reads the KS X 1001
        // symbols ° (A1 C6) and ± (A1 BE) as U+00B0 and U+00B1. They stay, rather than being
        // mapped back to bytes B0 B1, the Hangul "갚".
        let korean = TextEncoding::KoreanEucKr.assumed_character_set();
        let decoded = korean
            .decode(&[b"\x1b$)C".as_slice(), &korean.encode("Tilt 0°±2").unwrap()].concat())
            .unwrap();
        assert_eq!(decoded, "\u{1b}$)CTilt 0°±2");
        assert_eq!(decode_iso_2022_text(&decoded, false), "Tilt 0°±2");
        assert_eq!(decode_iso_2022_text("\u{1b}$)C½¼", false), "½¼");
        // Hangul the Korean codec decoded, next to a symbol it decoded as U+00B0.
        assert_eq!(decode_iso_2022_text("\u{1b}$)C홍°°", false), "홍°°");
    }

    #[test]
    fn g1_bytes_read_as_latin_1_only_under_a_single_byte_latin_1_first_value() {
        for (declaration, expected) in [
            ("ISO 2022 IR 6\\ISO 2022 IR 149", true),
            ("ISO 2022 IR 100\\ISO 2022 IR 149", true),
            ("ISO_IR 100", true),
            (" ISO 2022 IR 6 \\ISO 2022 IR 58", true),
            // An empty first value leaves the parser on the extension's set or the chosen one.
            ("\\ISO 2022 IR 149", false),
            ("", false),
            // ISO_IR 13 reads GR bytes as katakana, ISO_IR 192 as UTF-8.
            ("ISO 2022 IR 13\\ISO 2022 IR 87", false),
            ("ISO_IR 192", false),
        ] {
            assert_eq!(
                iso_2022_g1_read_as_latin_1(declaration),
                expected,
                "{declaration:?}"
            );
        }
    }

    #[test]
    fn jis_x_0208_segments_keep_bytes_that_are_dicom_delimiters() {
        // 0x5C, 0x5E and 0x3D are valid JIS X 0208 bytes, so '\\', '^' and '=' inside a G0
        // segment are part of its kanji.
        let jis = b"0\\0^0=";
        let expected = SpecificCharacterSet::from_code("ISO 2022 IR 87")
            .unwrap()
            .decode(&[b"\x1b$B".as_slice(), jis].concat())
            .unwrap();
        assert_eq!(expected.chars().count(), 3);
        assert!(!expected.chars().any(|ch| ch.is_ascii()));
        let decoded = "\u{1b}$B0\\0^0=\u{1b}(B";
        for g1_read_as_latin_1 in [true, false] {
            assert_eq!(decode_iso_2022_text(decoded, g1_read_as_latin_1), expected);
        }
    }
}
