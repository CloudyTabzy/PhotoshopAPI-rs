//! Text-tool (`TySh`) and text-engine-data (`Txt2`) tagged blocks.
//!
//! Mirrors `Core/TaggedBlocks/TypeToolTaggedBlock.{h,cpp}`: `TySh` carries a
//! 2D transform matrix plus a text descriptor (`TxLr`, whose `EngineData`
//! raw item holds the PostScript-style text payload — see
//! [`crate::engine_data`]) and a warp descriptor (`warp`), followed by
//! preserved trailing bytes.
//!
//! Like the other typed views, this parses on demand from the preserved
//! block bytes; the document layer keeps the raw block, so a malformed `TySh`
//! never breaks reading — the view just reports an error.
//!
//! `Txt2` payloads are bare EngineData documents; see
//! [`parse_text_engine_data`] for a convenience parser.

use crate::descriptor::{Descriptor, DescriptorIntegerSpan};
use crate::engine_data;
use crate::error::{PsdError, Result};
use crate::io::{BeReader, BeWriter};
use std::ops::Range;

/// Exact source spans for the two editable TySh text payloads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TypeToolTextPayloadSpans {
    /// EngineData bytes, excluding the descriptor's `u32` length.
    pub engine_data: Option<Range<usize>>,
    /// The `Txt ` UnicodeString value, including its unit-count prefix.
    pub text: Option<Range<usize>>,
}

/// Exact source spans of the two descriptor bodies inside a TySh payload.
/// Each range starts at the descriptor's class-name string (after its
/// version fields) and ends after its last item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeToolDescriptorSpans {
    /// The `TxLr` text descriptor.
    pub text: Range<usize>,
    /// The `warp` descriptor.
    pub warp: Range<usize>,
}

/// A parsed `TySh` block payload.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeToolTaggedBlock {
    /// TySh format version (Photoshop writes 1).
    pub tysh_version: u16,
    /// 2D affine transform matrix of the text layer (6 doubles).
    pub transform: [f64; 6],
    /// Text descriptor version (Photoshop writes 50).
    pub text_version: u16,
    /// Descriptor format version (16).
    pub text_descriptor_version: u32,
    /// The `TxLr` text descriptor; its `EngineData` item is the raw text
    /// payload parseable with [`engine_data`].
    pub text: Descriptor,
    /// Warp descriptor version (1).
    pub warp_version: u16,
    /// Warp descriptor format version (16).
    pub warp_descriptor_version: u32,
    /// The `warp` warp descriptor.
    pub warp: Descriptor,
    /// Undocumented trailing bytes, preserved verbatim.
    pub trailing: Vec<u8>,
}

impl TypeToolTaggedBlock {
    /// Locate the `EngineData` byte payload inside a raw TySh block payload.
    /// The returned range excludes the descriptor's four-byte data length.
    /// Unlike parsing and writing the full descriptor tree, this span lets a
    /// caller replace EngineData while preserving all other TySh bytes exactly.
    pub fn engine_data_payload_range(data: &[u8]) -> Result<Option<Range<usize>>> {
        Ok(Self::text_payload_spans(data)?.engine_data)
    }

    /// Locate EngineData and `Txt ` payloads without serializing descriptors.
    pub fn text_payload_spans(data: &[u8]) -> Result<TypeToolTextPayloadSpans> {
        let mut reader = BeReader::new(data);
        reader.skip(56)?; // version, six transform doubles, text and descriptor versions
        let (_, spans) =
            Descriptor::read_tracking_payloads(&mut reader, Some(b"EngineData"), Some(b"Txt "))?;
        Ok(TypeToolTextPayloadSpans {
            engine_data: spans.raw_data,
            text: spans.unicode_string,
        })
    }

    /// Offset of the text descriptor body: `u16` version, six `f64`
    /// transform components, `u16` text version, `u32` descriptor version.
    pub const TEXT_DESCRIPTOR_OFFSET: usize = 56;

    /// Byte range of the six transform doubles inside a TySh payload.
    pub const TRANSFORM_RANGE: Range<usize> = 2..50;

    /// Read the affine transform from its fixed offset without parsing either
    /// descriptor (the header layout is fixed, so this works even when a
    /// descriptor holds a value this port cannot parse).
    pub fn transform_of(data: &[u8]) -> Option<[f64; 6]> {
        if data.len() < Self::TEXT_DESCRIPTOR_OFFSET {
            return None;
        }
        let mut transform = [0.0; 6];
        for (component, bytes) in transform
            .iter_mut()
            .zip(data[Self::TRANSFORM_RANGE].chunks_exact(8))
        {
            *component = f64::from_be_bytes(bytes.try_into().ok()?);
        }
        Some(transform)
    }

    /// Locate the `Txt ` and `EngineData` payloads: exactly via the typed
    /// descriptor parse, or — when the text descriptor holds a value whose
    /// OSType this port cannot parse — via [`scan_text_payload_spans`].
    ///
    /// Upstream always locates these payloads by scanning raw bytes, so it
    /// keeps text editable next to unknown values; the typed parse stays the
    /// first choice here because it cannot be fooled by look-alike bytes.
    ///
    /// [`scan_text_payload_spans`]: Self::scan_text_payload_spans
    pub fn locate_text_payloads(data: &[u8]) -> Result<TypeToolTextPayloadSpans> {
        match Self::text_payload_spans(data) {
            Ok(spans) => Ok(spans),
            Err(error) => {
                let scanned = Self::scan_text_payload_spans(data);
                if scanned.engine_data.is_none() && scanned.text.is_none() {
                    Err(error)
                } else {
                    Ok(scanned)
                }
            }
        }
    }

    /// Find the `Txt ` and `EngineData` items by their complete item
    /// signatures — key length, key, and OSType (`Txt ` + `TEXT` with a zero
    /// or four-byte key length; `EngineData` + `tdta` with a ten-byte key
    /// length) — searching the descriptor region only. Each span is
    /// bounds-checked against its own length field. Stricter than upstream's
    /// scan, which matches the key and OSType without the length prefix.
    pub fn scan_text_payload_spans(data: &[u8]) -> TypeToolTextPayloadSpans {
        const TXT_IMPLICIT: &[u8] = b"\0\0\0\0Txt TEXT";
        const TXT_EXPLICIT: &[u8] = b"\0\0\0\x04Txt TEXT";
        const ENGINE_DATA: &[u8] = b"\0\0\0\x0AEngineDatatdta";

        /// Offsets just past every occurrence of `signature` at or after `from`.
        fn find(data: &[u8], from: usize, signature: &[u8]) -> Vec<usize> {
            data.get(from..)
                .unwrap_or_default()
                .windows(signature.len())
                .enumerate()
                .filter(|(_, window)| *window == signature)
                .map(|(start, _)| from + start + signature.len())
                .collect()
        }
        let from = Self::TEXT_DESCRIPTOR_OFFSET;
        let length_at = |offset: usize| {
            let bytes = data.get(offset..offset.checked_add(4)?)?;
            Some(u32::from_be_bytes(bytes.try_into().ok()?) as usize)
        };

        let mut text_candidates = find(data, from, TXT_IMPLICIT);
        text_candidates.extend(find(data, from, TXT_EXPLICIT));
        text_candidates.sort_unstable();
        let text = text_candidates.into_iter().find_map(|count_at| {
            let end = count_at
                .checked_add(4)?
                .checked_add(length_at(count_at)?.checked_mul(2)?)?;
            (end <= data.len()).then_some(count_at..end)
        });
        let engine_data = find(data, from, ENGINE_DATA)
            .into_iter()
            .find_map(|length_offset| {
                let start = length_offset.checked_add(4)?;
                let end = start.checked_add(length_at(length_offset)?)?;
                (end <= data.len()).then_some(start..end)
            });
        TypeToolTextPayloadSpans { engine_data, text }
    }

    /// Locate the text and warp descriptor bodies so one of them can be
    /// re-serialized while every other TySh byte (transform, the other
    /// descriptor, trailing bytes) stays verbatim.
    pub fn descriptor_spans(data: &[u8]) -> Result<TypeToolDescriptorSpans> {
        let mut reader = BeReader::new(data);
        reader.skip(56)?; // version, six transform doubles, text and descriptor versions
        let text_start = reader.position();
        Descriptor::read(&mut reader)?;
        let text_end = reader.position();
        reader.skip(6)?; // warp version + descriptor version
        let warp_start = reader.position();
        Descriptor::read(&mut reader)?;
        let warp_end = reader.position();
        Ok(TypeToolDescriptorSpans {
            text: text_start..text_end,
            warp: warp_start..warp_end,
        })
    }

    /// Locate legacy Descriptor `From`/`T   ` integer range values in the
    /// TySh text descriptor, including values nested in descriptors/arrays.
    pub fn legacy_range_integer_spans(data: &[u8]) -> Result<Vec<DescriptorIntegerSpan>> {
        let mut reader = BeReader::new(data);
        reader.skip(56)?;
        let (_, spans) = Descriptor::read_tracking_integer_items(&mut reader, &[b"From", b"T   "])?;
        Ok(spans)
    }

    /// Parse a `TySh` payload (the reader must span the block data).
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let offset = reader.position() as u64;

        let tysh_version = reader.u16()?;
        let mut transform = [0.0f64; 6];
        for component in &mut transform {
            *component = reader.f64()?;
        }

        let text_version = reader.u16()?;
        let text_descriptor_version = reader.u32()?;
        let text = Descriptor::read(reader)?;
        let text_end = reader.position();

        let warp_version = reader.u16()?;
        let warp_descriptor_version = reader.u32()?;
        let warp = Descriptor::read(reader)?;
        let warp_end = reader.position();

        if warp_end < text_end {
            return Err(PsdError::InvalidData {
                offset,
                message: "text tool warp descriptor precedes the text descriptor",
            });
        }

        let trailing = reader.take(reader.remaining())?.to_vec();
        Ok(Self {
            tysh_version,
            transform,
            text_version,
            text_descriptor_version,
            text,
            warp_version,
            warp_descriptor_version,
            warp,
            trailing,
        })
    }

    /// Serialize the payload (no length marker; the tagged-block wrapper
    /// adds it).
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        writer.u16(self.tysh_version);
        for component in self.transform {
            writer.f64(component);
        }
        writer.u16(self.text_version);
        writer.u32(self.text_descriptor_version);
        self.text.write(writer)?;
        writer.u16(self.warp_version);
        writer.u32(self.warp_descriptor_version);
        self.warp.write(writer)?;
        writer.bytes(&self.trailing);
        Ok(())
    }

    /// The raw `EngineData` payload of the text descriptor, if present.
    pub fn engine_data_bytes(&self) -> Option<&[u8]> {
        match self.text.get("EngineData") {
            Some(crate::DescriptorValue::RawData { data, .. }) => Some(data),
            _ => None,
        }
    }

    /// Parse the text descriptor's `EngineData` payload.
    pub fn engine_data(&self) -> Result<Option<engine_data::EngineValue>> {
        match self.engine_data_bytes() {
            Some(bytes) => engine_data::parse(bytes).map(Some),
            None => Ok(None),
        }
    }
}

/// Parse a `Txt2` payload. Photoshop CC writes the document-level text
/// engine cache as a bare dictionary body with numeric keys (`/98 << … >>
/// /0 << … >>`, no enclosing `<<`/`>>`); a payload that is a single
/// delimited value is parsed as such.
pub fn parse_text_engine_data(reader: &mut BeReader) -> Result<engine_data::EngineValue> {
    let data = reader.take(reader.remaining())?;
    engine_data::parse(data)
        .or_else(|error| engine_data::parse_dictionary_body(data).map_err(|_| error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_text() -> Descriptor {
        let mut descriptor = Descriptor {
            name: crate::UnicodeString::new("", 1).unwrap(),
            class_id: crate::DescriptorKey::new("TxLr"),
            items: Vec::new(),
        };
        descriptor.insert(
            "EngineData",
            crate::DescriptorValue::RawData {
                os_key: *b"tdta",
                data: b"\n\n<< /EngineDict << /Text (x) >>\n>>\n".to_vec(),
            },
        );
        descriptor
    }

    #[test]
    fn round_trips_with_trailing_bytes() {
        let warp = Descriptor {
            name: crate::UnicodeString::new("", 1).unwrap(),
            class_id: crate::DescriptorKey::new("warp"),
            items: Vec::new(),
        };
        let block = TypeToolTaggedBlock {
            tysh_version: 1,
            transform: [1.0, 0.0, 0.0, 1.0, 12.0, -34.5],
            text_version: 50,
            text_descriptor_version: 16,
            text: sample_text(),
            warp_version: 1,
            warp_descriptor_version: 16,
            warp,
            trailing: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };

        let mut writer = BeWriter::new();
        block.write(&mut writer).unwrap();
        let mut reader = BeReader::new(writer.as_slice());
        let back = TypeToolTaggedBlock::read(&mut reader).unwrap();
        assert!(reader.is_empty());
        assert_eq!(back, block);
        assert_eq!(back.transform[4], 12.0);

        // EngineData is reachable through the typed view.
        let engine = back.engine_data().unwrap().unwrap();
        assert!(engine.get("EngineDict").is_some());
    }

    /// A TySh whose text descriptor has an item with an OSType no parser
    /// knows, placed after `Txt `/`EngineData`, followed by a warp descriptor.
    fn tysh_with_unknown_ostype() -> Vec<u8> {
        let mut text = Descriptor {
            name: crate::UnicodeString::new("\0", 1).unwrap(),
            class_id: crate::DescriptorKey::char_id(*b"TxLr"),
            items: Vec::new(),
        };
        text.items.push(crate::DescriptorItem {
            key: crate::DescriptorKey::char_id(*b"Txt "),
            value: crate::DescriptorValue::String(crate::UnicodeString::new("Hi\0", 1).unwrap()),
        });
        text.insert(
            "EngineData",
            crate::DescriptorValue::RawData {
                os_key: *b"tdta",
                data: b"<< /EngineDict << >> >>".to_vec(),
            },
        );
        let mut body = BeWriter::new();
        text.write(&mut body).unwrap();
        let mut body = body.into_inner();
        // Bump the item count (after the 6-byte name and 8-byte class id) and
        // append `/Zzzz` with an OSType `Qqqq` whose length nobody knows.
        let count_at = 6 + 8;
        let count = u32::from_be_bytes(body[count_at..count_at + 4].try_into().unwrap());
        body[count_at..count_at + 4].copy_from_slice(&(count + 1).to_be_bytes());
        body.extend_from_slice(b"\0\0\0\0ZzzzQqqq\x01\x02\x03");

        let mut writer = BeWriter::new();
        writer.u16(1);
        for value in [1.0, 0.0, 0.0, 1.0, 7.5, -3.0] {
            writer.f64(value);
        }
        writer.u16(50);
        writer.u32(16);
        writer.bytes(&body);
        writer.bytes(&[0, 1, 0, 0, 0, 16]);
        writer.into_inner()
    }

    #[test]
    fn unknown_ostype_falls_back_to_exact_signature_scan() {
        let data = tysh_with_unknown_ostype();
        assert!(TypeToolTaggedBlock::read(&mut BeReader::new(&data)).is_err());
        assert!(TypeToolTaggedBlock::text_payload_spans(&data).is_err());

        let spans = TypeToolTaggedBlock::locate_text_payloads(&data).unwrap();
        let text = spans.text.clone().unwrap();
        assert_eq!(&data[text.start..text.start + 4], &3u32.to_be_bytes());
        assert_eq!(&data[text.start + 4..text.end], b"\0H\0i\0\0");
        assert_eq!(
            &data[spans.engine_data.unwrap()],
            b"<< /EngineDict << >> >>"
        );
        assert_eq!(
            TypeToolTaggedBlock::transform_of(&data),
            Some([1.0, 0.0, 0.0, 1.0, 7.5, -3.0])
        );
    }

    #[test]
    fn signature_scan_matches_the_typed_spans_on_parseable_blocks() {
        let block = TypeToolTaggedBlock {
            tysh_version: 1,
            transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            text_version: 50,
            text_descriptor_version: 16,
            text: {
                let mut text = sample_text();
                text.items.insert(
                    0,
                    crate::DescriptorItem {
                        key: crate::DescriptorKey::new("Txt "),
                        value: crate::DescriptorValue::String(
                            crate::UnicodeString::new("x\0", 1).unwrap(),
                        ),
                    },
                );
                text
            },
            warp_version: 1,
            warp_descriptor_version: 16,
            warp: Descriptor {
                name: crate::UnicodeString::new("", 1).unwrap(),
                class_id: crate::DescriptorKey::new("warp"),
                items: Vec::new(),
            },
            trailing: Vec::new(),
        };
        let mut writer = BeWriter::new();
        block.write(&mut writer).unwrap();
        let data = writer.into_inner();
        assert_eq!(
            TypeToolTaggedBlock::scan_text_payload_spans(&data),
            TypeToolTaggedBlock::text_payload_spans(&data).unwrap()
        );
        // Truncated length fields never yield out-of-bounds spans.
        let truncated = &data[..data.len() - 30];
        let spans = TypeToolTaggedBlock::scan_text_payload_spans(truncated);
        assert!(spans.engine_data.is_none());
        assert!(TypeToolTaggedBlock::transform_of(&data[..40]).is_none());
    }

    #[test]
    fn rejects_warp_before_text() {
        // Hand-craft a payload whose warp descriptor starts inside the text
        // descriptor by truncating mid-descriptor.
        let mut writer = BeWriter::new();
        writer.u16(1);
        for _ in 0..6 {
            writer.f64(0.0);
        }
        writer.u16(50);
        writer.u32(16);
        writer.bytes(b"\x00\x00garbage-not-a-descriptor");
        let bytes = writer.into_inner();
        let mut reader = BeReader::new(&bytes);
        assert!(TypeToolTaggedBlock::read(&mut reader).is_err());
    }
}
