//! Photoshop's two string types: [`PascalString`] and [`UnicodeString`].
//!
//! Mirrors `Core/Struct/PascalString.h/.cpp` and `UnicodeString.h/.cpp`.
//!
//! **PascalString** — a 1-byte length marker (counting string bytes only) plus
//! a codepage-encoded payload, padded to a section-dependent multiple (2 in
//! image resources, 4 for layer names). Upstream decodes to
//! UTF-8 with Windows-1252/Mac-Roman tables but writes the UTF-8 bytes back
//! raw; this port encodes back through the reverse table so representable
//! names round-trip byte-exactly — a deliberate difference from upstream.
//!
//! **UnicodeString** — a 4-byte code-unit count plus UTF-16BE payload, padded
//! to the given multiple. Write appends a null code unit if missing
//! ("Photoshop gets angry otherwise") and counts it; read strips nulls from
//! the decoded UTF-8 only — the stored UTF-16 stays verbatim.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::error::{PsdError, Result};
use crate::io::{round_up, BeReader, BeWriter};

/// Shared zero buffer for alignment padding: every padding used by the format
/// is ≤ 4, so a pad run never exceeds 3 bytes.
const ZERO_PAD: [u8; 8] = [0u8; 8];

/// Legacy codepage used by [`PascalString`] payloads.
///
/// Windows saves appear to always use Windows-1252; Mac-Roman exists for old
/// documents. Bytes `0x00..=0x7F` are ASCII (= UTF-8) in both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PascalEncoding {
    Windows1252,
    MacRoman,
}

/// Windows-1252 high range `0x80..=0x9F`; undefined slots decode to U+FFFD
/// (matches upstream's table). `0xA0..=0xFF` is plain Latin-1 (byte == codepoint).
const WINDOWS_1252_CONTROL: [char; 32] = [
    '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}', '\u{017D}', '\u{FFFD}',
    '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
];

/// Mac-Roman high range `0x80..=0xFF`, from upstream's `MacRoman_UTF8` table.
#[rustfmt::skip]
const MAC_ROMAN_HIGH: [char; 128] = [
    '\u{00C4}', '\u{00C5}', '\u{00C7}', '\u{00C9}', '\u{00D1}', '\u{00D6}', '\u{00DC}', '\u{00E1}',
    '\u{00E0}', '\u{00E2}', '\u{00E4}', '\u{00E3}', '\u{00E5}', '\u{00E7}', '\u{00E9}', '\u{00E8}',
    '\u{00EA}', '\u{00EB}', '\u{00ED}', '\u{00EC}', '\u{00EE}', '\u{00EF}', '\u{00F1}', '\u{00F3}',
    '\u{00F2}', '\u{00F4}', '\u{00F6}', '\u{00F5}', '\u{00FA}', '\u{00F9}', '\u{00FB}', '\u{00FC}',
    '\u{2020}', '\u{00B0}', '\u{00A2}', '\u{00A3}', '\u{00A7}', '\u{2022}', '\u{00B6}', '\u{00DF}',
    '\u{00AE}', '\u{00A9}', '\u{2122}', '\u{00B4}', '\u{00A8}', '\u{2260}', '\u{00C6}', '\u{00D8}',
    '\u{221E}', '\u{00B1}', '\u{2264}', '\u{2265}', '\u{00A5}', '\u{00B5}', '\u{2202}', '\u{2211}',
    '\u{220F}', '\u{03C0}', '\u{222B}', '\u{00AA}', '\u{00BA}', '\u{03A9}', '\u{00E6}', '\u{00F8}',
    '\u{00BF}', '\u{00A1}', '\u{00AC}', '\u{221A}', '\u{0192}', '\u{2248}', '\u{2206}', '\u{00AB}',
    '\u{00BB}', '\u{2026}', '\u{00A0}', '\u{00C0}', '\u{00C3}', '\u{00D5}', '\u{0152}', '\u{0153}',
    '\u{2013}', '\u{2014}', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}', '\u{00F7}', '\u{25CA}',
    '\u{00FF}', '\u{0178}', '\u{2044}', '\u{20AC}', '\u{2039}', '\u{203A}', '\u{FB01}', '\u{FB02}',
    '\u{2021}', '\u{00B7}', '\u{201A}', '\u{201E}', '\u{2030}', '\u{00C2}', '\u{00CA}', '\u{00C1}',
    '\u{00CB}', '\u{00C8}', '\u{00CD}', '\u{00CE}', '\u{00CF}', '\u{00CC}', '\u{00D3}', '\u{00D4}',
    '\u{F8FF}', '\u{00D2}', '\u{00DA}', '\u{00DB}', '\u{00D9}', '\u{0131}', '\u{02C6}', '\u{02DC}',
    '\u{00AF}', '\u{02D8}', '\u{02D9}', '\u{02DA}', '\u{00B8}', '\u{02DD}', '\u{02DB}', '\u{02C7}',
];

/// Decode one codepage byte to its char. ASCII passes through unchanged.
fn decode_byte(encoding: PascalEncoding, byte: u8) -> char {
    match byte {
        0x00..=0x7F => byte as char,
        high => match encoding {
            PascalEncoding::Windows1252 => match high {
                0x80..=0x9F => WINDOWS_1252_CONTROL[(high - 0x80) as usize],
                // Latin-1 supplement: byte value == codepoint.
                _ => high as char,
            },
            PascalEncoding::MacRoman => MAC_ROMAN_HIGH[(high - 0x80) as usize],
        },
    }
}

/// Reverse lookup tables for encoding, built once from the decode tables.
fn reverse_tables() -> &'static (HashMap<char, u8>, HashMap<char, u8>) {
    static TABLES: OnceLock<(HashMap<char, u8>, HashMap<char, u8>)> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut windows = HashMap::with_capacity(128);
        let mut mac = HashMap::with_capacity(128);
        for byte in 0x80u8..=0xFF {
            // First byte wins for decode-ambiguous entries (e.g. U+FFFD).
            windows
                .entry(decode_byte(PascalEncoding::Windows1252, byte))
                .or_insert(byte);
            mac.entry(decode_byte(PascalEncoding::MacRoman, byte))
                .or_insert(byte);
        }
        (windows, mac)
    })
}

/// Decode a codepage byte string to UTF-8.
pub fn decode(encoding: PascalEncoding, bytes: &[u8]) -> String {
    bytes.iter().map(|&b| decode_byte(encoding, b)).collect()
}

/// Encode a UTF-8 string to a codepage byte string.
///
/// Unrepresentable characters become `?` (0x3F) with a warning — upstream
/// writes raw UTF-8 instead, which corrupts such names outright.
pub fn encode(encoding: PascalEncoding, value: &str) -> Vec<u8> {
    let (windows, mac) = reverse_tables();
    let table = match encoding {
        PascalEncoding::Windows1252 => windows,
        PascalEncoding::MacRoman => mac,
    };
    value
        .chars()
        .map(|c| {
            if c.is_ascii() {
                c as u8
            } else {
                *table.get(&c).unwrap_or_else(|| {
                    tracing::warn!(
                        "character {c:?} is not representable in {encoding:?}, \
                         writing '?' (upstream wrote raw UTF-8 here)"
                    );
                    &0x3F
                })
            }
        })
        .collect()
}

/// A Pascal string: 1-byte length (string bytes only) + codepage payload,
/// zero-padded to a multiple of `padding` (2 in image resources, 4 for layer
/// names). Decoded value is UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PascalString {
    value: String,
    /// Section alignment captured at construction/read; used on write.
    padding: usize,
}

impl PascalString {
    /// Maximum stored payload in bytes: the 1-byte marker caps at 255 and the
    /// padded section must stay inside it (upstream truncates the same way).
    fn max_len(padding: usize) -> usize {
        254 - 254 % padding
    }

    /// Create from a UTF-8 value, truncating to the format's byte limit.
    pub fn new(value: impl Into<String>, padding: usize) -> Self {
        debug_assert!(padding >= 1);
        let mut value = value.into();
        let max = Self::max_len(padding);
        if value.len() > max {
            tracing::warn!(
                "pascal string exceeds {max} bytes, truncating (upstream does the same)"
            );
            let mut boundary = max;
            while !value.is_char_boundary(boundary) {
                boundary -= 1;
            }
            value.truncate(boundary);
        }
        Self { value, padding }
    }

    /// The decoded UTF-8 value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Total on-disk section size in bytes (marker + payload + zero padding).
    pub fn section_size(&self) -> usize {
        round_up(self.value.len() + 1, self.padding)
    }

    /// Read a Pascal string of the given alignment (Windows-1252 payload).
    pub fn read(reader: &mut BeReader, padding: usize) -> Result<Self> {
        let string_size = reader.u8()? as usize;
        let section = round_up(string_size + 1, padding);
        let payload = reader.take(string_size)?;
        let value = decode(PascalEncoding::Windows1252, payload);
        // Skip the zero padding after the payload.
        reader.skip(section - 1 - string_size)?;
        Ok(Self { value, padding })
    }

    /// Write with the alignment captured at construction/read.
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        let payload = encode(PascalEncoding::Windows1252, &self.value);
        let len = u8::try_from(payload.len()).map_err(|_| PsdError::InvalidData {
            offset: 0,
            message: "pascal string payload exceeds 255 bytes",
        })?;
        writer.u8(len);
        writer.bytes(&payload);
        let pad = round_up(payload.len() + 1, self.padding) - payload.len() - 1;
        writer.bytes(&ZERO_PAD[..pad]);
        Ok(())
    }
}

/// A Photoshop Unicode string: 4-byte code-unit count + UTF-16BE payload,
/// zero-padded to `padding`. Keeps the UTF-16 units verbatim so the write-side
/// null-termination rule round-trips faithfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnicodeString {
    value: String,
    utf16: Vec<u16>,
    padding: usize,
}

impl Default for UnicodeString {
    /// An empty string with 2-byte alignment (the common case).
    fn default() -> Self {
        Self {
            value: String::new(),
            utf16: Vec::new(),
            padding: 2,
        }
    }
}

impl UnicodeString {
    /// Create from a UTF-8 value with the given section alignment.
    pub fn new(value: impl Into<String>, padding: usize) -> Result<Self> {
        let value = value.into();
        let utf16: Vec<u16> = value.encode_utf16().collect();
        if utf16.len() * 2 > u32::MAX as usize {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "string exceeds the UnicodeString u32 byte limit",
            });
        }
        Ok(Self {
            value,
            utf16,
            padding,
        })
    }

    /// Create from a UTF-8 value that ends, on disk, in a null code unit, the way Photoshop
    /// writes descriptor names and `TEXT` values.
    ///
    /// The result equals what [`read`](Self::read) makes of such a string: the null is in the
    /// UTF-16 units and counted in the marker, but not part of [`value`](Self::value).
    pub fn terminated(value: impl Into<String>, padding: usize) -> Result<Self> {
        let value: String = value.into().chars().filter(|&c| c != '\0').collect();
        let mut string = Self::new(value, padding)?;
        string.utf16.push(0);
        Ok(string)
    }

    /// The decoded UTF-8 value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The verbatim UTF-16 code units (no trailing null is implied).
    pub fn utf16(&self) -> &[u16] {
        &self.utf16
    }

    /// Total on-disk section size in bytes, including null-termination and
    /// padding as performed by [`write`](Self::write).
    pub fn section_size(&self) -> usize {
        let mut units = self.utf16.len();
        if units == 0 || self.utf16[units - 1] != 0 {
            units += 1;
        }
        round_up(units * 2 + 4, self.padding)
    }

    /// Read a Unicode string of the given alignment.
    pub fn read(reader: &mut BeReader, padding: usize) -> Result<Self> {
        let offset = reader.position() as u64;
        let count = reader.u32()? as usize;
        let byte_len = count.checked_mul(2).ok_or(PsdError::InvalidData {
            offset,
            message: "UnicodeString unit count overflows",
        })?;
        let payload = reader.take(byte_len)?;
        let utf16: Vec<u16> = payload
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        let value = if byte_len == 0 {
            String::new()
        } else {
            // Upstream strips nulls from the UTF-8 view only.
            String::from_utf16(&utf16)
                .map_err(|_| PsdError::InvalidData {
                    offset,
                    message: "invalid UTF-16 in UnicodeString",
                })?
                .chars()
                .filter(|&c| c != '\0')
                .collect()
        };
        reader.skip(round_up(byte_len + 4, padding) - 4 - byte_len)?;
        Ok(Self {
            value,
            utf16,
            padding,
        })
    }

    /// Write with the alignment captured at construction/read. Appends a null
    /// code unit if the string does not end with one and counts it in the
    /// marker (upstream behavior — "Photoshop gets angry otherwise").
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        let mut units = self.utf16.clone();
        if units.last() != Some(&0) {
            units.push(0);
        }
        let count = u32::try_from(units.len()).map_err(|_| PsdError::InvalidData {
            offset: 0,
            message: "UnicodeString exceeds u32 code-unit limit",
        })?;
        writer.u32(count);
        for unit in &units {
            writer.u16(*unit);
        }
        let byte_size = units.len() * 2 + 4;
        writer.bytes(&ZERO_PAD[..round_up(byte_size, self.padding) - byte_size]);
        Ok(())
    }

    /// Write the stored UTF-16 units exactly, without the null-termination
    /// rule of [`write`](Self::write).
    ///
    /// Descriptor and linked-data payloads keep their strings verbatim so
    /// read → write stays byte-exact even when a string has no trailing null;
    /// the null-appending behavior is reserved for places upstream relies on
    /// it (layer names).
    pub fn write_verbatim(&self, writer: &mut BeWriter) -> Result<()> {
        let count = u32::try_from(self.utf16.len()).map_err(|_| PsdError::InvalidData {
            offset: 0,
            message: "UnicodeString exceeds u32 code-unit limit",
        })?;
        writer.u32(count);
        for unit in &self.utf16 {
            writer.u16(*unit);
        }
        let byte_size = self.utf16.len() * 2 + 4;
        writer.bytes(&ZERO_PAD[..round_up(byte_size, self.padding) - byte_size]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_1252_decoding_spot_checks() {
        assert_eq!(decode(PascalEncoding::Windows1252, &[0x80]), "\u{20AC}"); // €
        assert_eq!(decode(PascalEncoding::Windows1252, &[0x9C]), "\u{0153}"); // œ
        assert_eq!(decode(PascalEncoding::Windows1252, &[0xE9]), "\u{00E9}"); // é (Latin-1)
        assert_eq!(decode(PascalEncoding::Windows1252, &[0x81]), "\u{FFFD}"); // undefined
        assert_eq!(decode(PascalEncoding::Windows1252, b"abc"), "abc");
    }

    #[test]
    fn mac_roman_decoding_spot_checks() {
        assert_eq!(decode(PascalEncoding::MacRoman, &[0x80]), "\u{00C4}"); // Ä
        assert_eq!(decode(PascalEncoding::MacRoman, &[0xDE]), "\u{FB01}"); // fi ligature
        assert_eq!(decode(PascalEncoding::MacRoman, &[0xDB]), "\u{20AC}"); // €
    }

    #[test]
    fn encoding_round_trips_representable_chars() {
        for encoding in [PascalEncoding::Windows1252, PascalEncoding::MacRoman] {
            let original = "Layer \u{20AC}\u{00E9} 1";
            let bytes = encode(encoding, original);
            assert_eq!(decode(encoding, &bytes), original);
        }
        // Unrepresentable in Windows-1252: becomes '?'.
        assert_eq!(encode(PascalEncoding::Windows1252, "\u{4E16}"), b"?");
    }

    #[test]
    fn pascal_read_write_padding_rules() {
        // Layer names pad to 4: "Ab" -> marker 2 + 2 bytes + 1 zero = 4.
        let s = PascalString::new("Ab", 4);
        assert_eq!(s.section_size(), 4);
        let mut w = BeWriter::new();
        s.write(&mut w).unwrap();
        assert_eq!(w.as_slice(), &[2, b'A', b'b', 0]);

        // Resource names pad to 2: "" -> marker 0 + 1 zero = 2.
        let s = PascalString::new("", 2);
        let mut w = BeWriter::new();
        s.write(&mut w).unwrap();
        assert_eq!(w.as_slice(), &[0, 0]);

        // Payload of 5: marker + 5 bytes = 6, padded to 8 with 2 zeros.
        let s = PascalString::new("Abcde", 4);
        let mut w = BeWriter::new();
        s.write(&mut w).unwrap();
        assert_eq!(w.as_slice(), &[5, b'A', b'b', b'c', b'd', b'e', 0, 0]);

        // Payload of 3: marker + 3 bytes = 4, already aligned, no padding.
        let s = PascalString::new("Abc", 4);
        let mut aligned = BeWriter::new();
        s.write(&mut aligned).unwrap();
        assert_eq!(aligned.as_slice(), &[3, b'A', b'b', b'c']);

        // Round-trip through the reader.
        let mut r = BeReader::new(w.as_slice());
        let back = PascalString::read(&mut r, 4).unwrap();
        assert_eq!(back.value(), "Abcde");
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn pascal_truncates_to_format_limit() {
        let long = "x".repeat(300);
        let s = PascalString::new(long, 4);
        assert_eq!(s.value().len(), 252); // 254 - 254 % 4
        let s = PascalString::new("x".repeat(300), 2);
        assert_eq!(s.value().len(), 254);
    }

    #[test]
    fn unicode_write_appends_and_counts_null() {
        let s = UnicodeString::new("Ab", 2).unwrap();
        let mut w = BeWriter::new();
        s.write(&mut w).unwrap();
        // count=3 (A, b, null), UTF-16BE units, padded to 2 (no extra).
        assert_eq!(w.as_slice(), &[0, 0, 0, 3, 0, b'A', 0, b'b', 0, 0]);
    }

    #[test]
    fn a_terminated_string_equals_what_reading_one_makes() {
        for text in ["", "Gradient", "グラデーション"] {
            let built = UnicodeString::terminated(text, 1).unwrap();
            let mut writer = BeWriter::new();
            built.write_verbatim(&mut writer).unwrap();
            let bytes = writer.into_inner();
            // The marker counts the null, and the value does not include it.
            let units = text.encode_utf16().count() + 1;
            assert_eq!(
                u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize,
                units
            );
            assert_eq!(built.value(), text);
            let read = UnicodeString::read(&mut BeReader::new(&bytes), 1).unwrap();
            assert_eq!(read, built);
        }
        // A null in the input is not doubled.
        assert_eq!(
            UnicodeString::terminated("a\0", 1).unwrap().utf16(),
            [0x61, 0]
        );
    }

    #[test]
    fn unicode_empty_writes_single_null() {
        let s = UnicodeString::new("", 4).unwrap();
        let mut w = BeWriter::new();
        s.write(&mut w).unwrap();
        // count=1, one null unit, padded to 4 -> 4 + 2 + 2 zeros = 8.
        assert_eq!(w.as_slice(), &[0, 0, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn unicode_round_trip_preserves_verbatim_units() {
        // Simulate a Photoshop 'luni'-style payload: count includes the null.
        let bytes: &[u8] = &[0, 0, 0, 3, 0, b'A', 0, b'b', 0, 0];
        let mut r = BeReader::new(bytes);
        let s = UnicodeString::read(&mut r, 2).unwrap();
        assert_eq!(s.value(), "Ab");
        assert_eq!(s.utf16(), &[0x0041, 0x0062, 0x0000]); // null kept verbatim

        let mut w = BeWriter::new();
        s.write(&mut w).unwrap();
        assert_eq!(w.as_slice(), bytes); // byte-exact round-trip
    }

    #[test]
    fn unicode_read_rejects_lone_surrogate() {
        let bytes: &[u8] = &[0, 0, 0, 1, 0xD8, 0x00]; // unpaired high surrogate
        let mut r = BeReader::new(bytes);
        assert!(matches!(
            UnicodeString::read(&mut r, 2),
            Err(PsdError::InvalidData { .. })
        ));
    }

    #[test]
    fn unicode_supplementary_characters() {
        let s = UnicodeString::new("A\u{1F600}B", 4).unwrap();
        let mut w = BeWriter::new();
        s.write(&mut w).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = UnicodeString::read(&mut r, 4).unwrap();
        assert_eq!(back.value(), "A\u{1F600}B");
    }
}
