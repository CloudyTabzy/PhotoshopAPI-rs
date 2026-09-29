//! Tagged blocks — Photoshop's "additional layer info" key/value sections.
//!
//! Mirrors `Core/TaggedBlocks/TaggedBlock.{h,cpp}` and the storage/dispatch in
//! `TaggedBlockStorage.cpp` / `PhotoshopFile/AdditionalLayerInfo.cpp`.
//!
//! On disk each block is `signature(4)` | `key(4)` | `length` | `data`, where
//! the length is a `u32` — except for a fixed set of keys (upstream
//! `isTaggedBlockSizeUint64`) in PSB containers, where it is a `u64`. The
//! signature is `8BIM`, or `8B64` for some PSB blocks; it is preserved
//! verbatim.
//!
//! Upstream dispatches known keys to typed parsers and keeps every other
//! *mapped* key as an opaque base `TaggedBlock`; keys absent from its map are
//! read and then dropped. This port keeps every block as [`TaggedBlock`]
//! (key + raw payload) so all of them round-trip byte-exact; typed views over
//! the payloads the document layer needs live with their consumers
//! (e.g. `Lr16`/`Lr32` are parsed by [`crate::layer_and_mask_info`]).

use std::fmt;

use crate::enums::Version;
use crate::error::{PsdError, Result};
use crate::header::FileHeader;
use crate::io::{round_up, BeReader, BeWriter};

/// 4-byte ASCII key identifying a tagged block (`Util/Enum.h`'s
/// `TaggedBlockKey`). Unknown keys are carried through verbatim.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaggedBlockKey([u8; 4]);

impl TaggedBlockKey {
    /// Layer section divider (`lsct`): group start/end markers.
    pub const LSCT: Self = Self(*b"lsct");
    /// Unicode layer name (`luni`).
    pub const LUNI: Self = Self(*b"luni");
    /// 16-bit layer records (`Lr16`).
    pub const LR16: Self = Self(*b"Lr16");
    /// 32-bit layer records (`Lr32`).
    pub const LR32: Self = Self(*b"Lr32");
    /// Protected (locked) settings (`lspf`): a `u32` lock-flag word.
    pub const LSPF: Self = Self(*b"lspf");
    /// Layer id (`lyid`): a `u32`, unique within a document.
    pub const LYID: Self = Self(*b"lyid");
    /// Sheet (display) color setting (`lclr`): `u16` color + 6 padding bytes.
    pub const LCLR: Self = Self(*b"lclr");
    /// Blend fill opacity (`iOpa`): `u8` fill + 3 padding bytes.
    pub const IOPA: Self = Self(*b"iOpa");
    /// Placed-layer data (`SoLd`), the modern smart-object descriptor.
    pub const SOLD: Self = Self(*b"SoLd");

    pub const fn new(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 4] {
        self.0
    }

    /// The key as a (possibly lossy) string, e.g. for tracing.
    pub fn as_str(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

impl fmt::Debug for TaggedBlockKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl fmt::Display for TaggedBlockKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// Keys whose length field is 8 bytes wide in PSB containers
/// (upstream `isTaggedBlockSizeUint64`).
const UINT64_LENGTH_KEYS: [[u8; 4]; 14] = [
    *b"LMsk", *b"Lr16", *b"Lr32", *b"Layr", *b"Mtrn", *b"Mt16", *b"Mt32", *b"Alph", *b"FMsk",
    *b"FXid", *b"FEid", *b"lnk2", *b"PxSD", *b"cinf",
];

/// Whether the key uses a `u64` length field in PSB files.
pub fn uses_uint64_length(key: TaggedBlockKey) -> bool {
    UINT64_LENGTH_KEYS.contains(&key.as_bytes())
}

/// Length-field width decision: an `8B64` signature always carries a `u64`
/// length, and the PSB-only key list covers blocks Photoshop writes with an
/// `8BIM` signature but 8-byte lengths (e.g. `Lr16`).
///
/// Upstream decides purely by key, which misparses `8B64 lnkE`-style blocks
/// that recent Photoshop versions emit for external links (its error handling
/// then silently drops everything after the block).
fn uses_uint64_length_for(signature: [u8; 4], key: TaggedBlockKey, header: &FileHeader) -> bool {
    signature == *b"8B64" || (header.version == Version::Psb && uses_uint64_length(key))
}

const ZEROS: [u8; 8] = [0u8; 8];

/// A single tagged block: 4-byte key plus its opaque payload.
#[derive(Clone, PartialEq, Eq)]
pub struct TaggedBlock {
    /// `8BIM` or `8B64`; preserved verbatim (upstream always re-writes `8BIM`).
    pub signature: [u8; 4],
    pub key: TaggedBlockKey,
    /// Payload at its declared length, excluding any alignment padding.
    pub data: Vec<u8>,
}

impl fmt::Debug for TaggedBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaggedBlock")
            .field("key", &self.key)
            .field("data_len", &self.data.len())
            .finish()
    }
}

impl TaggedBlock {
    /// Create a block with the standard `8BIM` signature.
    pub fn new(key: TaggedBlockKey, data: Vec<u8>) -> Self {
        Self {
            signature: *b"8BIM",
            key,
            data,
        }
    }

    fn length_width(&self, header: &FileHeader) -> usize {
        if uses_uint64_length_for(self.signature, self.key, header) {
            8
        } else {
            4
        }
    }

    /// Read one block. `padding` is the alignment the surrounding section
    /// pads blocks to (1 inside layer records, 4 at document level); padding
    /// bytes are skipped but not part of [`data`](Self::data).
    pub fn read(reader: &mut BeReader, header: &FileHeader, padding: usize) -> Result<Self> {
        Ok(Self::read_eliding(reader, header, padding, None)?.0)
    }

    /// [`read`](Self::read), except that when the block's key is `elide` its
    /// payload is not copied: the block comes back with empty data, and the
    /// payload is returned as a slice of the reader's own bytes for the caller to
    /// parse in place. This is how the layer data inside a 16/32-bit document's
    /// `Lr16`/`Lr32` block, which is most of the file, is read without a second
    /// copy of it existing beside the parsed layers.
    pub(crate) fn read_eliding<'r>(
        reader: &mut BeReader<'r>,
        header: &FileHeader,
        padding: usize,
        elide: Option<TaggedBlockKey>,
    ) -> Result<(Self, Option<&'r [u8]>)> {
        let offset = reader.position() as u64;
        let mut signature = [0u8; 4];
        signature.copy_from_slice(reader.take(4)?);
        if &signature != b"8BIM" && &signature != b"8B64" {
            return Err(PsdError::InvalidSignature {
                expected: "8BIM",
                found: signature,
                offset,
            });
        }
        let mut key = [0u8; 4];
        key.copy_from_slice(reader.take(4)?);
        let key = TaggedBlockKey(key);

        let length = if uses_uint64_length_for(signature, key, header) {
            reader.u64()?
        } else {
            u64::from(reader.u32()?)
        };
        let length = usize::try_from(length).map_err(|_| PsdError::InvalidData {
            offset,
            message: "tagged block length does not fit the platform",
        })?;

        let payload = reader.take(length)?;
        reader.skip(round_up(length, padding) - length)?;
        let (data, elided) = if elide == Some(key) {
            (Vec::new(), Some(payload))
        } else {
            (payload.to_vec(), None)
        };
        Ok((
            Self {
                signature,
                key,
                data,
            },
            elided,
        ))
    }

    /// Write the block with its length marker and alignment padding.
    ///
    /// `padding` aligns the block on disk *outside* its declared length, which
    /// is what the document-level (global) block list uses: those blocks are
    /// four-byte aligned after the declared length. Per-layer blocks follow a
    /// different rule — see [`write_layer_block`](Self::write_layer_block).
    pub fn write(&self, writer: &mut BeWriter, header: &FileHeader, padding: usize) -> Result<()> {
        self.write_to(writer, header, padding)
    }

    /// [`write`](Self::write) to any sink, so a large payload can go straight
    /// to a file without being copied into a staging buffer first.
    pub fn write_to<W: std::io::Write>(
        &self,
        sink: &mut W,
        header: &FileHeader,
        padding: usize,
    ) -> Result<()> {
        let declared = self.data.len();
        let pad = round_up(declared, padding) - declared;
        sink.write_all(&self.header_bytes(header, declared as u64)?)?;
        sink.write_all(&self.data)?;
        sink.write_all(&ZEROS[..pad])?;
        Ok(())
    }

    /// Bytes [`write`](Self::write) produces: signature, key, length field,
    /// payload and its padding to a multiple of `padding`.
    pub fn encoded_len(&self, header: &FileHeader, padding: usize) -> u64 {
        let declared = self.data.len();
        (8 + self.length_width(header) + round_up(declared, padding)) as u64
    }

    /// The signature, key and length field that lead a block whose payload
    /// declares `declared` bytes. A block streamed in pieces (the `Lr16`/`Lr32`
    /// layer data) needs this before its payload exists.
    pub fn header_bytes(&self, header: &FileHeader, declared: u64) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(&self.signature);
        bytes.extend_from_slice(&self.key.as_bytes());
        if self.length_width(header) == 8 {
            bytes.extend_from_slice(&declared.to_be_bytes());
        } else {
            let narrowed = u32::try_from(declared).map_err(|_| PsdError::LengthOverflow {
                actual: declared,
                width: 4,
            })?;
            bytes.extend_from_slice(&narrowed.to_be_bytes());
        }
        Ok(bytes)
    }

    /// Write a per-layer block the way Photoshop's layer-record walk requires:
    /// the declared length is even, and an odd payload carries its pad byte
    /// *inside* that length.
    ///
    /// Photoshop advances by the declared length rounded up to an even count,
    /// and its own files never declare an odd length (a scan of the test
    /// corpora shows zero odd per-layer blocks). An odd declared length makes
    /// it walk one byte past the block, after which every later block in the
    /// record reads as unknown data. Keeping the pad inside the declared
    /// length leaves both an exact-advance reader and Photoshop's rounding
    /// walk on the same layout; even-length blocks — every block Photoshop
    /// writes — are byte-identical to what the input carried.
    pub fn write_layer_block(&self, writer: &mut BeWriter, header: &FileHeader) -> Result<()> {
        let pad = self.data.len() % 2;
        writer.bytes(&self.header_bytes(header, (self.data.len() + pad) as u64)?);
        writer.bytes(&self.data);
        writer.bytes(&ZEROS[..pad]);
        Ok(())
    }
}

/// An ordered list of tagged blocks (`AdditionalLayerInfo`). The same type is
/// used for the document-level list at the end of the layer-and-mask section
/// and for the per-layer list at the end of each layer record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdditionalLayerInfo {
    pub blocks: Vec<TaggedBlock>,
}

impl AdditionalLayerInfo {
    pub fn new() -> Self {
        Self::default()
    }

    /// First block with the given key (upstream assumes keys are unique).
    pub fn get(&self, key: TaggedBlockKey) -> Option<&TaggedBlock> {
        self.blocks.iter().find(|block| block.key == key)
    }

    /// First block with the given key, mutably.
    pub fn get_mut(&mut self, key: TaggedBlockKey) -> Option<&mut TaggedBlock> {
        self.blocks.iter_mut().find(|block| block.key == key)
    }

    pub fn push(&mut self, block: TaggedBlock) {
        self.blocks.push(block);
    }

    /// Read blocks until `max_len` bytes are consumed, then skip the
    /// remaining padding. A single block needs at least 12 bytes
    /// (16 in PSB for `u64`-length keys); a shorter remainder is padding.
    pub fn read(
        reader: &mut BeReader,
        header: &FileHeader,
        max_len: usize,
        padding: usize,
    ) -> Result<Self> {
        Ok(Self::read_tracking_end(reader, header, max_len, padding)?.0)
    }

    /// Like [`read`](Self::read), but also reports where the last block ended,
    /// before any trailing bytes inside `max_len` are skipped. A caller that
    /// sees non-zero bytes in that gap knows the declared length over-counts
    /// and the next section really starts at the content end.
    pub fn read_tracking_end(
        reader: &mut BeReader,
        header: &FileHeader,
        max_len: usize,
        padding: usize,
    ) -> Result<(Self, usize)> {
        let (blocks, content_end, _) = Self::read_eliding(reader, header, max_len, padding, None)?;
        Ok((blocks, content_end))
    }

    /// [`read_tracking_end`](Self::read_tracking_end) that does not copy the
    /// payload of the first block whose key is `elide`: that block is kept with
    /// empty data and its payload is returned, borrowed from the reader's bytes.
    /// See [`TaggedBlock::read_eliding`].
    pub(crate) fn read_eliding<'r>(
        reader: &mut BeReader<'r>,
        header: &FileHeader,
        max_len: usize,
        padding: usize,
        mut elide: Option<TaggedBlockKey>,
    ) -> Result<(Self, usize, Option<&'r [u8]>)> {
        let mut elided = None;
        let start = reader.position();
        let end = start.checked_add(max_len).ok_or(PsdError::InvalidData {
            offset: start as u64,
            message: "additional layer info length overflows",
        })?;
        // PSB blocks with a `u64` length need a 16-byte header; parsing with
        // the 12-byte PSD minimum on a 12–15 byte PSB tail would attempt a
        // block and abort the whole file (upstream catches per-block, warns,
        // and continues). Anything shorter than the minimum is padding.
        let min_block = if header.version == Version::Psb {
            16
        } else {
            12
        };
        let mut blocks = Vec::new();
        while end.saturating_sub(reader.position()) >= min_block {
            let before = reader.position();
            let (block, payload) = TaggedBlock::read_eliding(reader, header, padding, elide)?;
            if payload.is_some() {
                // Only the first block with the key is taken; a repeat is an
                // ordinary block.
                elided = payload;
                elide = None;
            }
            blocks.push(block);
            if reader.position() - start > max_len {
                return Err(PsdError::InvalidData {
                    offset: before as u64,
                    message: "tagged block exceeds the additional layer info section",
                });
            }
        }
        let content_end = reader.position();
        reader.skip(end - reader.position())?;
        Ok((Self { blocks }, content_end, elided))
    }

    /// Write all blocks with the given alignment (`padding`), for the
    /// document-level list. See [`TaggedBlock::write`].
    pub fn write(&self, writer: &mut BeWriter, header: &FileHeader, padding: usize) -> Result<()> {
        for block in &self.blocks {
            block.write(writer, header, padding)?;
        }
        Ok(())
    }

    /// Write a layer record's block list, every block declaring an even
    /// length. See [`TaggedBlock::write_layer_block`].
    pub fn write_layer_blocks(&self, writer: &mut BeWriter, header: &FileHeader) -> Result<()> {
        for block in &self.blocks {
            block.write_layer_block(writer, header)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{BitDepth, ColorMode};

    fn header(version: Version) -> FileHeader {
        FileHeader::new(version, 3, 64, 64, BitDepth::Eight, ColorMode::Rgb).unwrap()
    }

    #[test]
    fn round_trips_unknown_keys_with_padding() {
        // 5-byte payload, aligned to 4: 3 zero pad bytes, declared length 5.
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"xxxx"), vec![1, 2, 3, 4, 5]);
        let mut w = BeWriter::new();
        block.write(&mut w, &header(Version::Psd), 4).unwrap();
        assert_eq!(&w.as_slice()[8..12], &5u32.to_be_bytes());
        assert_eq!(w.as_slice().len(), 12 + 8); // header + payload + 3 pad

        let mut r = BeReader::new(w.as_slice());
        let back = TaggedBlock::read(&mut r, &header(Version::Psd), 4).unwrap();
        assert_eq!(back, block);
        assert!(r.is_empty());
    }

    #[test]
    fn psb_uint64_keys_get_eight_byte_lengths() {
        let block = TaggedBlock::new(TaggedBlockKey::LR16, vec![0xAA; 4]);
        let mut w = BeWriter::new();
        block.write(&mut w, &header(Version::Psb), 4).unwrap();
        assert_eq!(&w.as_slice()[8..16], &4u64.to_be_bytes());

        let mut r = BeReader::new(w.as_slice());
        assert_eq!(
            TaggedBlock::read(&mut r, &header(Version::Psb), 4).unwrap(),
            block
        );

        // The same key in a PSD stays 32-bit.
        let mut w = BeWriter::new();
        block.write(&mut w, &header(Version::Psd), 4).unwrap();
        assert_eq!(&w.as_slice()[8..12], &4u32.to_be_bytes());
    }

    #[test]
    fn preserves_8b64_signature() {
        let mut block = TaggedBlock::new(TaggedBlockKey::LR32, vec![1]);
        block.signature = *b"8B64";
        let mut w = BeWriter::new();
        block.write(&mut w, &header(Version::Psb), 4).unwrap();
        assert_eq!(&w.as_slice()[..4], b"8B64");
        let mut r = BeReader::new(w.as_slice());
        assert_eq!(
            TaggedBlock::read(&mut r, &header(Version::Psb), 4).unwrap(),
            block
        );
    }

    #[test]
    fn eliding_takes_the_first_block_with_the_key_and_borrows_its_payload() {
        let mut ali = AdditionalLayerInfo::new();
        ali.push(TaggedBlock::new(TaggedBlockKey::LUNI, vec![1, 2, 3, 4]));
        ali.push(TaggedBlock::new(TaggedBlockKey::LR16, vec![7; 6]));
        ali.push(TaggedBlock::new(TaggedBlockKey::LR16, vec![8; 5]));
        let mut w = BeWriter::new();
        ali.write(&mut w, &header(Version::Psd), 4).unwrap();
        let total = w.position();

        let mut r = BeReader::new(w.as_slice());
        let (back, end, payload) = AdditionalLayerInfo::read_eliding(
            &mut r,
            &header(Version::Psd),
            total,
            4,
            Some(TaggedBlockKey::LR16),
        )
        .unwrap();
        assert_eq!(payload, Some([7u8; 6].as_slice()));
        assert_eq!(end, total);
        assert_eq!(back.blocks.len(), 3);
        assert!(back.blocks[1].data.is_empty());
        // A repeat of the key is an ordinary block.
        assert_eq!(back.blocks[2].data, vec![8; 5]);
        assert_eq!(back.blocks[0], ali.blocks[0]);

        // No key to elide: identical to a plain read.
        let mut r = BeReader::new(w.as_slice());
        let (plain, _, none) =
            AdditionalLayerInfo::read_eliding(&mut r, &header(Version::Psd), total, 4, None)
                .unwrap();
        assert!(none.is_none());
        assert_eq!(plain, ali);
    }

    #[test]
    fn additional_layer_info_reads_to_the_section_end() {
        let mut ali = AdditionalLayerInfo::new();
        ali.push(TaggedBlock::new(TaggedBlockKey::LUNI, vec![1, 2, 3, 4]));
        ali.push(TaggedBlock::new(TaggedBlockKey::new(*b"zzzz"), vec![9; 7]));

        let mut w = BeWriter::new();
        ali.write(&mut w, &header(Version::Psd), 4).unwrap();
        // Round the section up to 4 with trailing padding bytes.
        w.pad_to(4);
        let total = w.position();
        assert_eq!(total, 12 + 4 + 12 + 8); // 16 + 20

        let mut r = BeReader::new(w.as_slice());
        let back = AdditionalLayerInfo::read(&mut r, &header(Version::Psd), total, 4).unwrap();
        assert_eq!(back, ali);
        assert!(r.is_empty());
    }

    #[test]
    fn eight_b_64_signature_forces_u64_length_even_for_unknown_keys() {
        // Photoshop writes external links as `8B64 lnkE` + u64 length; `lnkE`
        // is not in the key-based u64 list.
        let mut block = TaggedBlock::new(TaggedBlockKey::new(*b"lnkE"), vec![0xAB; 3]);
        block.signature = *b"8B64";
        let mut w = BeWriter::new();
        block.write(&mut w, &header(Version::Psb), 4).unwrap();
        assert_eq!(&w.as_slice()[8..16], &3u64.to_be_bytes());

        let mut r = BeReader::new(w.as_slice());
        assert_eq!(
            TaggedBlock::read(&mut r, &header(Version::Psb), 4).unwrap(),
            block
        );
        assert!(r.is_empty());
    }

    #[test]
    fn layer_blocks_declare_an_even_length_with_the_pad_inside() {
        // An odd payload gains one pad byte, counted in the declared length.
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"tes1"), vec![1, 2, 3]);
        let mut writer = BeWriter::new();
        block
            .write_layer_block(&mut writer, &header(Version::Psd))
            .unwrap();
        let bytes = writer.into_inner();
        assert_eq!(&bytes[..4], b"8BIM");
        assert_eq!(&bytes[4..8], b"tes1");
        assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 4);
        assert_eq!(
            &bytes[12..],
            &[1, 2, 3, 0],
            "the pad byte is inside the declared length"
        );

        // An even payload is written exactly as it was read.
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"tes2"), vec![1, 2, 3, 4]);
        let mut writer = BeWriter::new();
        block
            .write_layer_block(&mut writer, &header(Version::Psd))
            .unwrap();
        let bytes = writer.into_inner();
        assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 4);
        assert_eq!(&bytes[12..], &[1, 2, 3, 4]);

        // The document-level form still aligns outside the declared length.
        let block = TaggedBlock::new(TaggedBlockKey::new(*b"tes3"), vec![1, 2, 3]);
        let mut writer = BeWriter::new();
        block.write(&mut writer, &header(Version::Psd), 4).unwrap();
        let bytes = writer.into_inner();
        assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 3);
        assert_eq!(&bytes[12..], &[1, 2, 3, 0], "global blocks pad outside");
    }

    #[test]
    fn rejects_bad_signature() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"8BXX");
        bytes.extend_from_slice(b"luni");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let mut r = BeReader::new(&bytes);
        assert!(matches!(
            TaggedBlock::read(&mut r, &header(Version::Psd), 1),
            Err(PsdError::InvalidSignature { .. })
        ));
    }
}
