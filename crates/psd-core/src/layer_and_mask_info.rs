//! The LayerAndMaskInformation section: layer records, channel image data,
//! masks, blending ranges, and the document-level tagged blocks.
//!
//! Mirrors `PhotoshopFile/LayerAndMaskInformation.{h,cpp}` (the `LayerRecords`
//! namespace structs, `LayerRecord`, `LayerInfo`, `GlobalLayerMaskInfo`, and
//! the section itself).
//!
//! Section order: `length` (u32 PSD / u64 PSB) | `LayerInfo` (its own marker)
//! | `GlobalLayerMaskInfo` (u32 marker + bytes, [`GlobalMaskSettings`] typed) | tagged blocks
//! (4-byte aligned). 16- and 32-bit documents keep their layer data inside an
//! `Lr16`/`Lr32` tagged block instead of the main `LayerInfo` (which is then a
//! zero-length section); this port moves it into [`LayerInfo`] on read and
//! regenerates the block on write, so callers always see one layer tree. The block
//! itself is read as an empty placeholder that keeps its position, so the layer data
//! is held once, parsed, and not a second time as raw bytes.
//!
//! Deviations from upstream:
//! - Channel image data is stored as the raw *compressed* bytes (compression
//!   marker parsed, payload untouched). Decompression is `psd-codecs`' job and
//!   belongs to the document layer.
//! - The layer-count sign (negative = first alpha channel holds the merged
//!   image alpha) is preserved via [`LayerInfo::has_merged_alpha`]; upstream
//!   always writes it back positive.
//! - Flag bytes (layer record flags, mask flags, mask params) are stored
//!   verbatim, so unknown bits round-trip. Upstream drops some bits and has a
//!   copy-paste bug where two "unknown" mask-flag bits share one bit.
//! - `LayerBlendingRanges` reads exactly the ranges on disk. Upstream appends
//!   them to five default ranges on every read, then writes a marker for 5 but
//!   10 ranges of data — corrupting any file it round-trips.
//! - `Lr16`/`Lr32` payloads are written without an inner length marker,
//!   matching what Photoshop writes and what upstream *reads* (upstream's
//!   write adds a marker its own reader would choke on).

use std::borrow::Cow;
use std::io::Write;

use crate::enums::{BitDepth, BlendMode, ChannelId, Compression, Version};
use crate::error::{PsdError, Result};
use crate::header::FileHeader;
use crate::io::{BeReader, BeWriter};
use crate::strings::PascalString;
use crate::tagged_blocks::{AdditionalLayerInfo, TaggedBlock, TaggedBlockKey};

/// Whether a plausible global-mask length sits at `start`: one that fits
/// inside the section. A layer-info length that over-counts by a couple of
/// bytes leaves a garbage length here, which is how the fallback to the real
/// content end is chosen.
fn global_mask_fits(reader: &mut BeReader, start: usize, end: usize) -> bool {
    if start + 4 > end {
        return false;
    }
    let saved = reader.position();
    if reader.seek(start).is_err() {
        return false;
    }
    let fits = reader
        .u32()
        .map(|length| (length as usize) <= end - (start + 4))
        .unwrap_or(false);
    let _ = reader.seek(saved);
    fits
}

/// Photoshop's documented per-layer channel cap.
pub const MAX_LAYER_CHANNELS: usize = 56;

/// The LayerAndMaskInformation section.
///
/// The tagged blocks borrow (`Cow`) so document-layer write staging can
/// serialize a layer's blocks without deep-copying every payload per save;
/// parse results own their data via `Cow::Owned`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LayerAndMaskInformation<'a> {
    pub layer_info: LayerInfo<'a>,
    /// Undocumented legacy section; preserved opaquely (upstream skips it and
    /// writes an empty one on save).
    pub global_layer_mask_info: GlobalLayerMaskInfo,
    /// Document-level tagged blocks. For 16/32-bit documents this is where the
    /// `Lr16`/`Lr32` block lives.
    pub additional_layer_info: Option<Cow<'a, AdditionalLayerInfo>>,
}

impl<'a> LayerAndMaskInformation<'a> {
    pub fn read(reader: &mut BeReader, header: &FileHeader) -> Result<Self> {
        Self::read_with_channels(reader, header, |bytes| Cow::Owned(bytes.to_vec()))
    }

    pub(crate) fn read_with_channels<'data>(
        reader: &mut BeReader<'data>,
        header: &FileHeader,
        payload: fn(&'data [u8]) -> Cow<'a, [u8]>,
    ) -> Result<Self> {
        let offset = reader.position() as u64;
        let section_len =
            usize::try_from(reader.len(header.version)?).map_err(|_| PsdError::InvalidData {
                offset,
                message: "layer and mask info length does not fit the platform",
            })?;
        let end = reader
            .position()
            .checked_add(section_len)
            .ok_or(PsdError::InvalidData {
                offset,
                message: "layer and mask info length overflows",
            })?;
        // A zero-length section carries no subsections at all. Files written
        // without layers declare it empty, and parsing must not run into the
        // image data that follows.
        if section_len == 0 {
            return Ok(Self::default());
        }

        let (mut layer_info, content_end) =
            LayerInfo::read_tracking_content_end(reader, header, payload)?;

        // 16/32-bit documents keep their layers in an `Lr16`/`Lr32` block after
        // an empty main section. That block is most of the file, so it is parsed
        // where it lies and never copied: the block is kept with empty data (it
        // fixes where the block is written, and the writer regenerates its
        // content from the layer tree).
        let nested_key = match header.depth {
            BitDepth::Sixteen => Some(TaggedBlockKey::LR16),
            BitDepth::ThirtyTwo => Some(TaggedBlockKey::LR32),
            BitDepth::One | BitDepth::Eight => None,
        }
        .filter(|_| layer_info.layer_records.is_empty());

        // The global-mask info is optional in the wild: some writers end the
        // section right after the layer info, and some declare a layer-info
        // length a couple of bytes longer than its content, which puts the
        // tail at the content end instead of the declared one. Prefer the
        // declared end when a plausible mask length sits there, and fall back
        // to the content end otherwise.
        let declared_end = reader.position();
        let mut tail_start = declared_end;
        if !global_mask_fits(reader, declared_end, end) && content_end < declared_end {
            tail_start = content_end;
        }
        reader.seek(tail_start)?;

        let mut global_layer_mask_info = GlobalLayerMaskInfo::default();
        let mut additional_layer_info = None;
        // One to three trailing bytes cannot hold a mask length; they are
        // padding and are consumed so the position lands on the section end.
        if end.saturating_sub(reader.position()) < 4 {
            let padding = end - reader.position();
            reader.skip(padding)?;
        }
        if reader.position() < end {
            let glmi_len = reader.u32()? as usize;
            // A mask length that runs past the section is clamped rather than
            // rejected; Photoshop reads these files.
            let available = end - reader.position();
            global_layer_mask_info = GlobalLayerMaskInfo {
                data: reader.take(glmi_len.min(available))?.to_vec(),
            };

            let remaining = end - reader.position();
            additional_layer_info = if remaining >= 12 {
                let (mut blocks, _, nested) =
                    AdditionalLayerInfo::read_eliding(reader, header, remaining, 4, nested_key)?;
                if let (Some(key), Some(bytes)) = (nested_key, nested) {
                    let mut sub = BeReader::new(bytes);
                    let parsed = LayerInfo::read_content(&mut sub, header, bytes.len(), payload)?.0;
                    if parsed.layer_records.is_empty() {
                        // Nothing to regenerate a block from: keep it as it was.
                        if let Some(block) = blocks.get_mut(key) {
                            block.data = bytes.to_vec();
                        }
                    } else {
                        layer_info = parsed;
                    }
                }
                Some(Cow::Owned(blocks))
            } else {
                reader.skip(remaining)?;
                None
            };
        }

        Ok(Self {
            layer_info,
            global_layer_mask_info,
            additional_layer_info,
        })
    }

    pub fn write(&self, writer: &mut BeWriter, header: &FileHeader) -> Result<()> {
        let marker = writer.reserve_len_marker(header.version);

        if header.depth == BitDepth::Eight {
            self.layer_info.write_section(writer, header)?;
        } else {
            // The real layer data lives in the Lr16/Lr32 block below.
            writer.len(header.version, 0)?;
        }

        writer.u32(
            u32::try_from(self.global_layer_mask_info.data.len()).map_err(|_| {
                PsdError::LengthOverflow {
                    actual: self.global_layer_mask_info.data.len() as u64,
                    width: 4,
                }
            })?,
        );
        writer.bytes(&self.global_layer_mask_info.data);

        self.write_additional_layer_info(writer, header)?;

        writer.pad_to_relative(marker, 4);
        let end = writer.position();
        writer.patch_len(header.version, marker, end, false)
    }

    /// Stream the section to `sink`: the same bytes as [`write`](Self::write),
    /// without assembling the section in memory first, and each channel payload
    /// is released once it has been written.
    ///
    /// Every length is worked out before the first byte goes out (layer records
    /// and other small parts are staged, payload sizes are known), so a section
    /// that cannot be written, one whose length overflows a PSD field, fails
    /// before any of the section reaches `sink`.
    ///
    /// When a 16/32-bit document carries several `Lr16`/`Lr32` blocks, the layer
    /// data is written once, into the first, and the others are dropped (as
    /// `write` does): a second copy of the layer tree can only be stale.
    /// Upstream writes the same layer data into each of them.
    pub(crate) fn write_to<W: Write>(&mut self, sink: &mut W, header: &FileHeader) -> Result<()> {
        let version = header.version;
        let records = &self.layer_info.layer_records;
        let mut pieces: Vec<Piece<'_>> = Vec::new();

        // The layer records, and the length of records plus channel data.
        let mut head = BeWriter::new();
        self.layer_info.write_head(&mut head, header)?;
        let head = head.into_inner();
        let content_len = head.len() as u64 + self.layer_info.channel_data_len();

        // LayerInfo: real data only in 8-bit documents; the 16/32-bit layer
        // data goes into the `Lr16`/`Lr32` block below.
        if header.depth == BitDepth::Eight {
            if records.is_empty() {
                tracing::warn!("writing a document without layers (empty layer info section)");
                pieces.push(Piece::Bytes(length_field(version, 0)?));
            } else {
                let pad = pad_to_four(content_len);
                pieces.push(Piece::Bytes(length_field(version, content_len + pad)?));
                pieces.push(Piece::Head);
                pieces.push(Piece::ChannelData);
                pieces.push(Piece::Bytes(zeros(pad)));
            }
        } else {
            pieces.push(Piece::Bytes(length_field(version, 0)?));
        }

        // GlobalLayerMaskInfo.
        let mask = &self.global_layer_mask_info.data;
        let mask_len = u32::try_from(mask.len()).map_err(|_| PsdError::LengthOverflow {
            actual: mask.len() as u64,
            width: 4,
        })?;
        let mut mask_bytes = mask_len.to_be_bytes().to_vec();
        mask_bytes.extend_from_slice(mask);
        pieces.push(Piece::Bytes(mask_bytes));

        // Document-level tagged blocks, with the layer data of a 16/32-bit
        // document in its `Lr16`/`Lr32` block (first, for a new document, like
        // Photoshop).
        let nested_key = match header.depth {
            BitDepth::Sixteen => Some(TaggedBlockKey::LR16),
            BitDepth::ThirtyTwo => Some(TaggedBlockKey::LR32),
            BitDepth::One | BitDepth::Eight => None,
        }
        .filter(|_| !records.is_empty());
        let blocks = self
            .additional_layer_info
            .as_ref()
            .map(|ali| ali.blocks.as_slice())
            .unwrap_or(&[]);
        let mut nested_placed = false;
        let nested = |template: TaggedBlock, pieces: &mut Vec<Piece<'_>>| -> Result<()> {
            // The payload has no inner length marker, and the block pads to four
            // outside its declared length.
            pieces.push(Piece::Bytes(template.header_bytes(header, content_len)?));
            pieces.push(Piece::Head);
            pieces.push(Piece::ChannelData);
            pieces.push(Piece::Bytes(zeros(pad_to_four(content_len))));
            Ok(())
        };
        if let Some(key) = nested_key {
            if !blocks.iter().any(|block| block.key == key) {
                nested(TaggedBlock::new(key, Vec::new()), &mut pieces)?;
                nested_placed = true;
            }
        }
        for block in blocks {
            if nested_key == Some(block.key) {
                if !nested_placed {
                    // Only its signature and key are needed; cloning the block
                    // would copy the whole layer data it carried from the read.
                    let template = TaggedBlock {
                        signature: block.signature,
                        key: block.key,
                        data: Vec::new(),
                    };
                    nested(template, &mut pieces)?;
                    nested_placed = true;
                }
            } else if !is_emptied_layer_data(block, header) {
                pieces.push(Piece::Block(block));
            }
        }

        // The section length covers everything above, padded to four.
        let total: u64 = pieces
            .iter()
            .map(|piece| match piece {
                Piece::Bytes(bytes) => bytes.len() as u64,
                Piece::Head => head.len() as u64,
                Piece::Block(block) => block.encoded_len(header, 4),
                Piece::ChannelData => self.layer_info.channel_data_len(),
            })
            .sum();
        let pad = pad_to_four(total);
        sink.write_all(&length_field(version, total + pad)?)?;
        for piece in &pieces {
            match piece {
                Piece::Bytes(bytes) => sink.write_all(bytes)?,
                Piece::Head => sink.write_all(&head)?,
                Piece::Block(block) => block.write_to(sink, header, 4)?,
                Piece::ChannelData => self.layer_info.stream_channel_data(sink)?,
            }
        }
        sink.write_all(&zeros(pad))?;
        Ok(())
    }

    fn write_additional_layer_info(
        &self,
        writer: &mut BeWriter,
        header: &FileHeader,
    ) -> Result<()> {
        let nested_key = match header.depth {
            BitDepth::Sixteen => Some(TaggedBlockKey::LR16),
            BitDepth::ThirtyTwo => Some(TaggedBlockKey::LR32),
            BitDepth::One | BitDepth::Eight => None,
        };

        // Regenerate the nested layer data when the document is 16/32-bit and
        // has layers to store (the block payload has no inner length marker).
        let nested_data = match nested_key {
            Some(_) if !self.layer_info.layer_records.is_empty() => {
                let mut buffer = BeWriter::new();
                self.layer_info.write_content(&mut buffer, header)?;
                Some(buffer.into_inner())
            }
            _ => None,
        };

        let blocks = self
            .additional_layer_info
            .as_ref()
            .map(|ali| ali.blocks.as_slice())
            .unwrap_or(&[]);
        let has_nested_block = nested_key.is_some_and(|key| blocks.iter().any(|b| b.key == key));

        // The layer data goes into the first block with the key. A repeat can only
        // be a stale second copy of the layer tree, so it is dropped.
        let regenerating = nested_data.is_some();
        let mut nested_data = nested_data;
        // New documents place the nested block first, like Photoshop does.
        if let (Some(key), Some(data)) = (nested_key, nested_data.take_if(|_| !has_nested_block)) {
            TaggedBlock::new(key, data).write(writer, header, 4)?;
        }
        for block in blocks {
            if regenerating && nested_key == Some(block.key) {
                if let Some(data) = nested_data.take() {
                    let replacement = TaggedBlock {
                        signature: block.signature,
                        key: block.key,
                        data,
                    };
                    replacement.write(writer, header, 4)?;
                }
            } else if !is_emptied_layer_data(block, header) {
                block.write(writer, header, 4)?;
            }
        }
        Ok(())
    }
}

/// The LayerInfo section: layer records plus their channel image data, index
/// aligned (record `i` owns channel data `i`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LayerInfo<'a> {
    pub layer_records: Vec<LayerRecord<'a>>,
    pub channel_image_data: Vec<ChannelImageData<'a>>,
    /// On-disk layer count was negative: the first alpha channel of the layer
    /// records holds the merged image's alpha (upstream drops this).
    pub has_merged_alpha: bool,
}

impl<'a> LayerInfo<'a> {
    /// Read the standalone LayerInfo section including its length marker.
    /// A zero length (16/32-bit documents) yields an empty `LayerInfo`.
    pub fn read(reader: &mut BeReader, header: &FileHeader) -> Result<Self> {
        Ok(Self::read_tracking_content_end(reader, header, |bytes| Cow::Owned(bytes.to_vec()))?.0)
    }

    /// Read the section and report where its actual content ended, which can
    /// be up to four bytes before the declared end (alignment padding, or a
    /// declared length that over-counts by a couple of bytes).
    fn read_tracking_content_end<'data>(
        reader: &mut BeReader<'data>,
        header: &FileHeader,
        payload: fn(&'data [u8]) -> Cow<'a, [u8]>,
    ) -> Result<(Self, usize)> {
        let offset = reader.position() as u64;
        let section_len =
            usize::try_from(reader.len(header.version)?).map_err(|_| PsdError::InvalidData {
                offset,
                message: "layer info length does not fit the platform",
            })?;
        if section_len == 0 {
            return Ok((Self::default(), reader.position()));
        }
        Self::read_content(reader, header, section_len, payload)
    }

    /// Read `content_len` bytes of layer count + records + channel data
    /// without a leading length marker (the payload of `Lr16`/`Lr32`).
    /// Returns the parsed info and the position where its content ended.
    fn read_content<'data>(
        reader: &mut BeReader<'data>,
        header: &FileHeader,
        content_len: usize,
        payload: fn(&'data [u8]) -> Cow<'a, [u8]>,
    ) -> Result<(Self, usize)> {
        let offset = reader.position() as u64;
        let end = reader
            .position()
            .checked_add(content_len)
            .ok_or(PsdError::InvalidData {
                offset,
                message: "layer info content length overflows",
            })?;

        let raw_count = reader.i16()?;
        let count = raw_count.unsigned_abs() as usize;
        let mut layer_records = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            layer_records.push(LayerRecord::read(reader, header)?);
        }
        if reader.position() > end {
            return Err(PsdError::InvalidData {
                offset,
                message: "layer records exceed the layer info section",
            });
        }

        let mut channel_image_data = Vec::with_capacity(count.min(1024));
        for record in &layer_records {
            channel_image_data.push(ChannelImageData::read_with_channels(
                reader, record, payload,
            )?);
        }

        let position = reader.position();
        if position > end {
            return Err(PsdError::InvalidData {
                offset,
                message: "channel image data exceeds the layer info section",
            });
        }
        // The section is 4-byte aligned; the Lr16/Lr32 payload may carry the
        // same alignment padding.
        let trailing = end - position;
        if trailing > 4 {
            return Err(PsdError::InvalidData {
                offset,
                message: "unexpected trailing bytes in the layer info section",
            });
        }
        reader.skip(trailing)?;

        Ok((
            Self {
                layer_records,
                channel_image_data,
                has_merged_alpha: raw_count < 0,
            },
            position,
        ))
    }

    /// Write the standalone LayerInfo section (with its own marker). 16/32-bit
    /// documents write an empty section; their data goes into `Lr16`/`Lr32`.
    pub fn write_section(&self, writer: &mut BeWriter, header: &FileHeader) -> Result<()> {
        if self.layer_records.is_empty() {
            // Flattened documents (e.g. the indexed fixtures) carry a
            // zero-length section; Photoshop accepts but dislikes this.
            tracing::warn!("writing a document without layers (empty layer info section)");
            writer.len(header.version, 0)?;
            return Ok(());
        }
        let marker = writer.reserve_len_marker(header.version);
        self.write_content(writer, header)?;
        writer.pad_to_relative(marker, 4);
        let end = writer.position();
        writer.patch_len(header.version, marker, end, false)
    }

    /// Write count + records + channel data without a leading length marker
    /// (the `Lr16`/`Lr32` payload).
    pub fn write_content(&self, writer: &mut BeWriter, header: &FileHeader) -> Result<()> {
        self.write_head(writer, header)?;
        for channels in &self.channel_image_data {
            channels.write(writer)?;
        }
        Ok(())
    }

    /// Write the layer count and the layer records: everything that precedes
    /// the channel image data in [`write_content`](Self::write_content).
    fn write_head(&self, writer: &mut BeWriter, header: &FileHeader) -> Result<()> {
        if self.layer_records.len() != self.channel_image_data.len() {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "layer records and channel image data count mismatch",
            });
        }
        let count =
            i16::try_from(self.layer_records.len()).map_err(|_| PsdError::LengthOverflow {
                actual: self.layer_records.len() as u64,
                width: 2,
            })?;
        writer.i16(if self.has_merged_alpha { -count } else { count });

        for record in &self.layer_records {
            record.write(writer, header)?;
        }
        Ok(())
    }

    /// Bytes of every channel's compression marker and payload.
    fn channel_data_len(&self) -> u64 {
        self.channel_image_data
            .iter()
            .flat_map(|layer| &layer.channels)
            .map(channel_disk_size)
            .sum()
    }

    /// Stream every channel to `sink`, releasing each payload as soon as it is
    /// written so the compressed data does not stay resident beside the file
    /// being produced.
    fn stream_channel_data<W: Write>(&mut self, sink: &mut W) -> Result<()> {
        for layer in &mut self.channel_image_data {
            for channel in &mut layer.channels {
                sink.write_all(&channel.compression.as_raw().to_be_bytes())?;
                sink.write_all(&channel.data)?;
                channel.data = Cow::Borrowed(&[]);
            }
        }
        Ok(())
    }
}

/// A piece of the layer-and-mask section in the order it is written, for
/// [`LayerAndMaskInformation::write_to`].
enum Piece<'b> {
    /// Small bytes staged in memory: markers, records, padding.
    Bytes(Vec<u8>),
    /// A tagged block whose payload is written straight from where it lives.
    Block(&'b TaggedBlock),
    /// The layer count and layer records.
    Head,
    /// The channel image data.
    ChannelData,
}

/// Whether `block` is the `Lr16`/`Lr32` block of a 16/32-bit document with its
/// layer data taken out (a reader keeps such a block empty to remember its
/// position and drops the copy of the data, which the writer regenerates from the
/// layer tree). When there are no layers left to regenerate it from, the block
/// has nothing to say and is not written.
fn is_emptied_layer_data(block: &TaggedBlock, header: &FileHeader) -> bool {
    let key = match header.depth {
        BitDepth::Sixteen => TaggedBlockKey::LR16,
        BitDepth::ThirtyTwo => TaggedBlockKey::LR32,
        BitDepth::One | BitDepth::Eight => return false,
    };
    block.key == key && block.data.is_empty()
}

/// `len` zero bytes.
fn zeros(len: u64) -> Vec<u8> {
    vec![0; len as usize]
}

/// Zero bytes that align a section of `len` bytes to a multiple of four.
fn pad_to_four(len: u64) -> u64 {
    (4 - len % 4) % 4
}

/// A length field of the width the file's version uses.
fn length_field(version: Version, value: u64) -> Result<Vec<u8>> {
    let mut writer = BeWriter::new();
    writer.len(version, value)?;
    Ok(writer.into_inner())
}

/// A single layer record.
///
/// `additional_layer_info` borrows so write staging does not deep-copy every
/// tagged block per save; parses own their data (`Cow::Owned`).
#[derive(Debug, Clone, PartialEq)]
pub struct LayerRecord<'a> {
    /// Layer name, Pascal string padded to 4 bytes.
    pub name: PascalString,
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
    pub channels: Vec<ChannelInfo>,
    pub blend_mode: BlendMode,
    /// 0–255.
    pub opacity: u8,
    /// 0 or 1.
    pub clipping: u8,
    pub flags: LayerFlags,
    pub mask_data: Option<LayerMaskData>,
    pub blending_ranges: LayerBlendingRanges,
    pub additional_layer_info: Option<Cow<'a, AdditionalLayerInfo>>,
}

impl<'a> LayerRecord<'a> {
    /// Layer width in pixels (may be negative for malformed bounds).
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    /// Layer height in pixels.
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn read(reader: &mut BeReader, header: &FileHeader) -> Result<Self> {
        let offset = reader.position() as u64;
        let top = reader.i32()?;
        let left = reader.i32()?;
        let bottom = reader.i32()?;
        let right = reader.i32()?;

        let channel_count = reader.u16()? as usize;
        if channel_count > MAX_LAYER_CHANNELS {
            tracing::warn!(
                "layer record declares {channel_count} channels; Photoshop's limit is {MAX_LAYER_CHANNELS}"
            );
        }
        let mut channels = Vec::with_capacity(channel_count.min(64));
        for _ in 0..channel_count {
            let index = reader.i16()?;
            let size = reader.len(header.version)?;
            channels.push(ChannelInfo {
                id: ChannelId::from_index(index, header.color_mode),
                index,
                size,
            });
        }

        reader.signature("8BIM")?;
        let mut blend_bytes = [0u8; 4];
        blend_bytes.copy_from_slice(reader.take(4)?);
        let blend_mode = BlendMode::from_bytes(blend_bytes);

        let opacity = reader.u8()?;
        let clipping = reader.u8()?;
        let flags = LayerFlags::from_bits(reader.u8()?);
        reader.skip(1)?; // filler byte

        let extra_len = reader.u32()? as usize;
        let extra_end = reader
            .position()
            .checked_add(extra_len)
            .ok_or(PsdError::InvalidData {
                offset,
                message: "layer record extra data length overflows",
            })?;

        let has_real_mask = channels.iter().any(|info| info.index == -3);
        let mask_data = LayerMaskData::read(reader, extra_end, has_real_mask)?;
        let blending_ranges = LayerBlendingRanges::read(reader, extra_end)?;
        let name = PascalString::read(reader, 4)?;
        if reader.position() > extra_end {
            return Err(PsdError::InvalidData {
                offset,
                message: "layer record extra data exceeds its declared length",
            });
        }

        let remaining = extra_end - reader.position();
        let (additional_layer_info, content_end) = if remaining >= 12 {
            // Per-layer blocks are padded to an even length. Photoshop counts
            // that pad *inside* the declared length (every block in the corpus
            // reads that way), but some writers declare the unpadded length and
            // put the pad byte outside it — so round the length up to even
            // rather than assuming, which accepts both shapes.
            let (blocks, content_end) =
                AdditionalLayerInfo::read_tracking_end(reader, header, remaining, 2)?;
            (Some(Cow::Owned(blocks)), content_end)
        } else {
            // Too short for a block; the whole remainder is a candidate gap.
            (None, reader.position())
        };
        // A declared extra-data length can over-count by a few bytes: the gap
        // then holds the next section's first bytes, not padding. Padding is
        // zeros, so a non-zero gap means the record really ends at the content
        // end. (One real fixture declares two bytes too many and is only
        // consistent when the channel data that follows starts at the content
        // end.)
        let gap = extra_end - content_end;
        let over_counts = (1..=4).contains(&gap) && {
            let saved = reader.position();
            let non_zero = reader
                .seek(content_end)
                .and_then(|()| reader.take(gap))
                .map(|bytes| bytes.iter().any(|&byte| byte != 0))
                .unwrap_or(false);
            reader.seek(saved)?;
            non_zero
        };
        reader.seek(if over_counts { content_end } else { extra_end })?;

        Ok(Self {
            name,
            top,
            left,
            bottom,
            right,
            channels,
            blend_mode,
            opacity,
            clipping,
            flags,
            mask_data,
            blending_ranges,
            additional_layer_info,
        })
    }

    pub fn write(&self, writer: &mut BeWriter, header: &FileHeader) -> Result<()> {
        writer.i32(self.top);
        writer.i32(self.left);
        writer.i32(self.bottom);
        writer.i32(self.right);

        let channel_count =
            u16::try_from(self.channels.len()).map_err(|_| PsdError::LengthOverflow {
                actual: self.channels.len() as u64,
                width: 2,
            })?;
        writer.u16(channel_count);
        for channel in &self.channels {
            writer.i16(channel.index);
            writer.len(header.version, channel.size)?;
        }

        writer.bytes(b"8BIM");
        writer.bytes(&self.blend_mode.as_bytes());
        writer.u8(self.opacity);
        writer.u8(self.clipping);
        writer.u8(self.flags.bits());
        writer.u8(0); // filler

        // Extra data is 2-byte aligned; its length is always a u32.
        let marker = writer.reserve_len_marker(Version::Psd);
        let content_start = writer.position();
        match &self.mask_data {
            Some(mask) => mask.write(writer)?,
            None => writer.u32(0),
        }
        self.blending_ranges.write(writer)?;
        self.name.write(writer)?;
        if let Some(ali) = &self.additional_layer_info {
            ali.write_layer_blocks(writer, header)?;
        }
        writer.pad_to_relative(content_start, 2);
        let end = writer.position();
        writer.patch_len(Version::Psd, marker, end, false)
    }
}

/// Raw layer-record flag byte with accessors for the documented bits.
///
/// Stored verbatim so unknown bits survive round-trips (upstream drops bit 2
/// and never stores bits 5–7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerFlags(u8);

impl LayerFlags {
    pub const TRANSPARENCY_PROTECTED: u8 = 1 << 0;
    pub const HIDDEN: u8 = 1 << 1;
    /// Bit 3: whether bit 4 carries meaning at all.
    pub const BIT4_USEFUL: u8 = 1 << 3;
    pub const PIXEL_DATA_IRRELEVANT: u8 = 1 << 4;

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn transparency_protected(self) -> bool {
        self.0 & Self::TRANSPARENCY_PROTECTED != 0
    }

    pub const fn hidden(self) -> bool {
        self.0 & Self::HIDDEN != 0
    }

    /// True when bit 4 is set *and* bit 3 marks it useful (upstream semantics).
    pub const fn pixel_data_irrelevant(self) -> bool {
        self.0 & Self::PIXEL_DATA_IRRELEVANT != 0 && self.0 & Self::BIT4_USEFUL != 0
    }

    pub fn set_transparency_protected(&mut self, value: bool) {
        self.set_bit(Self::TRANSPARENCY_PROTECTED, value);
    }

    pub fn set_hidden(&mut self, value: bool) {
        self.set_bit(Self::HIDDEN, value);
    }

    /// Sets or clears bit 4, keeping bit 3 ("bit 4 useful") in sync.
    pub fn set_pixel_data_irrelevant(&mut self, value: bool) {
        self.set_bit(Self::BIT4_USEFUL, value);
        self.set_bit(Self::PIXEL_DATA_IRRELEVANT, value);
    }

    fn set_bit(&mut self, mask: u8, value: bool) {
        if value {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }
}

/// Per-channel entry of a layer record: interpreted id plus the raw index and
/// the compressed channel size (including its 2-byte compression marker).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: ChannelId,
    /// The raw `i16` on disk; authoritative for round-trips.
    pub index: i16,
    /// Compressed payload size *including* the 2-byte compression marker.
    pub size: u64,
}

/// The mask section of a layer record (up to two masks: vector + pixel).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LayerMaskData {
    /// Pixel/user mask (channel id `-2`).
    pub pixel_mask: Option<LayerMask>,
    /// Vector mask; stored first on disk when both are present.
    pub vector_mask: Option<LayerMask>,
}

impl LayerMaskData {
    /// Read the mask section (its own u32 marker). Returns `None` for the
    /// common empty section.
    pub fn read(
        reader: &mut BeReader,
        section_end: usize,
        has_real_mask: bool,
    ) -> Result<Option<Self>> {
        let offset = reader.position() as u64;
        let len = reader.u32()? as usize;
        if len == 0 {
            return Ok(None);
        }
        let end = reader
            .position()
            .checked_add(len)
            .ok_or(PsdError::InvalidData {
                offset,
                message: "layer mask data length overflows",
            })?;
        if end > section_end {
            return Err(PsdError::InvalidData {
                offset,
                message: "layer mask data exceeds the layer record",
            });
        }
        // One mask record is 18 bytes; anything shorter is structurally
        // invalid. Checking here also keeps `end - reader.position()` below
        // from underflowing on a short section (upstream uses a signed
        // `int64_t toRead` and cannot underflow).
        if len < 18 {
            return Err(PsdError::InvalidData {
                offset,
                message: "layer mask data is shorter than one mask record",
            });
        }

        let mut first = LayerMask::read_forward(reader)?;
        let first_has_params_flag = first.flags.has_mask_params();

        // The reverse-ordered header after the first carries the real user
        // mask (`-3`) geometry, so it is present only when the record stores
        // that channel. Deciding by the remaining length alone reads a large
        // parameter block as a header (or drops the parameters entirely),
        // which Photoshop-written files do exhibit.
        let mut second = None;
        if has_real_mask && end.saturating_sub(reader.position()) >= 18 {
            second = Some(LayerMask::read_reverse(reader)?);
        }

        // Parameters follow the header(s) when a flag asks for them; the first
        // mask's flag is the documented one and a second mask's flag is
        // honored too, since some writers set it there.
        if first_has_params_flag
            || second
                .as_ref()
                .is_some_and(|mask| mask.flags.has_mask_params())
        {
            let params = MaskParams::read(reader)?;
            match &mut second {
                Some(mask) => mask.params = Some(params),
                None => first.params = Some(params),
            }
        }

        let (vector_mask, pixel_mask) = if first.flags.is_vector() {
            (Some(first), second)
        } else {
            (None, Some(second.unwrap_or(first)))
        };

        let position = reader.position();
        if position > end {
            return Err(PsdError::InvalidData {
                offset,
                message: "layer mask data exceeds its declared length",
            });
        }
        let trailing = end - position;
        if trailing > 2 {
            tracing::warn!("layer mask data has {trailing} trailing bytes, expected 0 or 2");
        }
        reader.skip(trailing)?;

        Ok(Some(Self {
            pixel_mask,
            vector_mask,
        }))
    }

    /// Write the mask section. The vector mask (if any) comes first, the
    /// pixel mask second in reverse field order — the layout Photoshop writes
    /// and upstream reads.
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        let marker = writer.reserve_len_marker(Version::Psd);
        let content_start = writer.position();
        if let Some(first) = self.vector_mask.or(self.pixel_mask) {
            first.write_forward(writer)?;
        }
        if self.vector_mask.is_some() {
            if let Some(pixel) = self.pixel_mask {
                pixel.write_reverse(writer)?;
            }
        }
        writer.pad_to_relative(content_start, 4);
        let end = writer.position();
        writer.patch_len(Version::Psd, marker, end, false)
    }

    /// The record describing channel `index`'s geometry.
    ///
    /// Channel `-2` (the user mask) is described by the first on-disk record:
    /// the vector-rendered mask when one exists, otherwise the pixel mask.
    /// Channel `-3` (the *real* user mask Photoshop writes when both masks
    /// exist) is the pixel mask. Other indices fall back to the first record.
    pub fn record_for_channel(&self, index: i16) -> Option<&LayerMask> {
        match index {
            -3 => self.pixel_mask.as_ref().or(self.vector_mask.as_ref()),
            _ => self.vector_mask.as_ref().or(self.pixel_mask.as_ref()),
        }
    }

    /// Mutable form of [`record_for_channel`](Self::record_for_channel).
    pub fn record_for_channel_mut(&mut self, index: i16) -> Option<&mut LayerMask> {
        match index {
            -3 => match self.pixel_mask {
                Some(ref mut mask) => Some(mask),
                None => self.vector_mask.as_mut(),
            },
            _ => match self.vector_mask {
                Some(ref mut mask) => Some(mask),
                None => self.pixel_mask.as_mut(),
            },
        }
    }

    /// User (pixel) mask density, `0..=255`.
    pub fn user_mask_density(&self) -> Option<u8> {
        self.params_owner()
            .and_then(|mask| mask.params)
            .and_then(|params| params.user_mask_density)
    }

    /// User (pixel) mask feather in pixels.
    pub fn user_mask_feather(&self) -> Option<f64> {
        self.params_owner()
            .and_then(|mask| mask.params)
            .and_then(|params| params.user_mask_feather)
    }

    /// Set or clear the user mask density. Fails when there is no mask record
    /// to attach the parameters to.
    pub fn set_user_mask_density(&mut self, value: Option<u8>) -> Result<()> {
        self.edit_params(|params| params.set_user_mask_density(value))
    }

    /// Set or clear the user mask feather; `value` must be finite.
    pub fn set_user_mask_feather(&mut self, value: Option<f64>) -> Result<()> {
        if value.is_some_and(|feather| !feather.is_finite()) {
            return Err(PsdError::InvalidData {
                offset: 0,
                message: "mask feather must be finite",
            });
        }
        self.edit_params(|params| params.set_user_mask_feather(value))
    }

    /// Photoshop stores the parameter block after the pixel mask record (the
    /// second record when both masks exist) but raises the "has parameters"
    /// flag on the first record; `read` mirrors that layout.
    fn params_owner(&self) -> Option<&LayerMask> {
        self.pixel_mask.as_ref().or(self.vector_mask.as_ref())
    }

    fn edit_params(&mut self, edit: impl FnOnce(&mut MaskParams)) -> Result<()> {
        let owner = match self.pixel_mask {
            Some(ref mut mask) => mask,
            None => self.vector_mask.as_mut().ok_or(PsdError::InvalidData {
                offset: 0,
                message: "mask parameters require a mask record",
            })?,
        };
        let mut params = owner.params.unwrap_or_default();
        edit(&mut params);
        owner.params = (!params.is_empty()).then_some(params);
        if owner.params.is_some() {
            let first = match self.vector_mask {
                Some(ref mut mask) => mask,
                None => self.pixel_mask.as_mut().expect("owner exists"),
            };
            first.flags.set_has_mask_params(true);
        } else {
            // A stale bit would make `read` consume padding as parameters.
            for mask in [self.vector_mask.as_mut(), self.pixel_mask.as_mut()]
                .into_iter()
                .flatten()
            {
                mask.flags.set_has_mask_params(false);
            }
        }
        Ok(())
    }
}

/// A single layer mask's geometry and flags.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerMask {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
    /// 0 or 255.
    pub default_color: u8,
    pub flags: LayerMaskFlags,
    /// Present when [`LayerMaskFlags::has_mask_params`] is set.
    pub params: Option<MaskParams>,
}

impl LayerMask {
    fn read_forward(reader: &mut BeReader) -> Result<Self> {
        let top = reader.i32()?;
        let left = reader.i32()?;
        let bottom = reader.i32()?;
        let right = reader.i32()?;
        let default_color = reader.u8()?;
        let flags = LayerMaskFlags::from_bits(reader.u8()?);
        Ok(Self {
            top,
            left,
            bottom,
            right,
            default_color,
            flags,
            params: None,
        })
    }

    fn read_reverse(reader: &mut BeReader) -> Result<Self> {
        let flags = LayerMaskFlags::from_bits(reader.u8()?);
        let default_color = reader.u8()?;
        let top = reader.i32()?;
        let left = reader.i32()?;
        let bottom = reader.i32()?;
        let right = reader.i32()?;
        Ok(Self {
            top,
            left,
            bottom,
            right,
            default_color,
            flags,
            params: None,
        })
    }

    fn write_forward(&self, writer: &mut BeWriter) -> Result<()> {
        writer.i32(self.top);
        writer.i32(self.left);
        writer.i32(self.bottom);
        writer.i32(self.right);
        writer.u8(self.default_color);
        writer.u8(self.flags.bits());
        if let Some(params) = &self.params {
            params.write(writer)?;
        }
        Ok(())
    }

    fn write_reverse(&self, writer: &mut BeWriter) -> Result<()> {
        writer.u8(self.flags.bits());
        writer.u8(self.default_color);
        writer.i32(self.top);
        writer.i32(self.left);
        writer.i32(self.bottom);
        writer.i32(self.right);
        if let Some(params) = &self.params {
            params.write(writer)?;
        }
        Ok(())
    }
}

/// Raw layer-mask flag byte with accessors for the documented bits.
///
/// Stored verbatim so unknown bits round-trip (upstream reads bits 2, 5, 6, 7
/// into separate booleans but has two of them mapped to the same bit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayerMaskFlags(u8);

impl LayerMaskFlags {
    pub const POSITION_RELATIVE_TO_LAYER: u8 = 1 << 0;
    pub const DISABLED: u8 = 1 << 1;
    pub const IS_VECTOR: u8 = 1 << 3;
    pub const HAS_MASK_PARAMS: u8 = 1 << 4;

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn position_relative_to_layer(self) -> bool {
        self.0 & Self::POSITION_RELATIVE_TO_LAYER != 0
    }

    pub const fn disabled(self) -> bool {
        self.0 & Self::DISABLED != 0
    }

    pub const fn is_vector(self) -> bool {
        self.0 & Self::IS_VECTOR != 0
    }

    pub const fn has_mask_params(self) -> bool {
        self.0 & Self::HAS_MASK_PARAMS != 0
    }

    pub fn set_disabled(&mut self, value: bool) {
        self.set_bit(Self::DISABLED, value);
    }

    pub fn set_position_relative_to_layer(&mut self, value: bool) {
        self.set_bit(Self::POSITION_RELATIVE_TO_LAYER, value);
    }

    pub fn set_has_mask_params(&mut self, value: bool) {
        self.set_bit(Self::HAS_MASK_PARAMS, value);
    }

    fn set_bit(&mut self, mask: u8, value: bool) {
        if value {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }
}

/// Mask density/feather parameters (present on one mask of a layer).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MaskParams {
    /// Raw parameter flag byte (bit 0–3 select which fields are present).
    pub flags: u8,
    pub user_mask_density: Option<u8>,
    pub user_mask_feather: Option<f64>,
    pub vector_mask_density: Option<u8>,
    pub vector_mask_feather: Option<f64>,
}

impl MaskParams {
    const USER_MASK_DENSITY: u8 = 1 << 0;
    const USER_MASK_FEATHER: u8 = 1 << 1;
    const VECTOR_MASK_DENSITY: u8 = 1 << 2;
    const VECTOR_MASK_FEATHER: u8 = 1 << 3;

    /// Whether no parameter is present (the block may then be omitted).
    pub fn is_empty(&self) -> bool {
        self.user_mask_density.is_none()
            && self.user_mask_feather.is_none()
            && self.vector_mask_density.is_none()
            && self.vector_mask_feather.is_none()
    }

    /// Set or clear the user (pixel) mask density, keeping the presence bit
    /// in `flags` consistent with the field. Unknown flag bits are kept.
    pub fn set_user_mask_density(&mut self, value: Option<u8>) {
        self.user_mask_density = value;
        self.sync_flag(Self::USER_MASK_DENSITY, value.is_some());
    }

    /// Set or clear the user (pixel) mask feather; see
    /// [`set_user_mask_density`](Self::set_user_mask_density).
    pub fn set_user_mask_feather(&mut self, value: Option<f64>) {
        self.user_mask_feather = value;
        self.sync_flag(Self::USER_MASK_FEATHER, value.is_some());
    }

    fn sync_flag(&mut self, bit: u8, present: bool) {
        if present {
            self.flags |= bit;
        } else {
            self.flags &= !bit;
        }
    }

    fn read(reader: &mut BeReader) -> Result<Self> {
        let flags = reader.u8()?;
        // A block can end mid-parameters: real files carry a flag byte that
        // promises more than the block holds (a third-party writer, a real
        // bug report). Read what is there, leave the rest absent, and warn —
        // failing the whole file over it would be worse.
        let mut truncated = false;
        fn field<T>(
            present: bool,
            truncated: &mut bool,
            read: impl FnOnce() -> Result<T>,
        ) -> Option<T> {
            if !present {
                return None;
            }
            match read() {
                Ok(value) => Some(value),
                Err(_) => {
                    *truncated = true;
                    None
                }
            }
        }
        let user_mask_density = field(flags & Self::USER_MASK_DENSITY != 0, &mut truncated, || {
            reader.u8()
        });
        let user_mask_feather = field(flags & Self::USER_MASK_FEATHER != 0, &mut truncated, || {
            reader.f64()
        });
        let vector_mask_density = field(
            flags & Self::VECTOR_MASK_DENSITY != 0,
            &mut truncated,
            || reader.u8(),
        );
        let vector_mask_feather = field(
            flags & Self::VECTOR_MASK_FEATHER != 0,
            &mut truncated,
            || reader.f64(),
        );
        if truncated {
            tracing::warn!(
                "truncated mask parameters (flags {flags:#04x}); some fields are missing"
            );
        }
        Ok(Self {
            flags,
            user_mask_density,
            user_mask_feather,
            vector_mask_density,
            vector_mask_feather,
        })
    }

    fn write(&self, writer: &mut BeWriter) -> Result<()> {
        writer.u8(self.flags);
        if let Some(value) = self.user_mask_density {
            writer.u8(value);
        }
        if let Some(value) = self.user_mask_feather {
            writer.f64(value);
        }
        if let Some(value) = self.vector_mask_density {
            writer.u8(value);
        }
        if let Some(value) = self.vector_mask_feather {
            writer.f64(value);
        }
        Ok(())
    }
}

/// One blending range: four source and four destination bytes (low/high pairs
/// for each of the two split halves; identical when the range was not split).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlendingRange {
    pub source: [u8; 4],
    pub destination: [u8; 4],
}

impl Default for BlendingRange {
    fn default() -> Self {
        Self {
            source: [0, 0, 255, 255],
            destination: [0, 0, 255, 255],
        }
    }
}

/// The blending ranges of a layer record. Photoshop always writes five
/// (combined + one per color channel, plus an unused alpha slot) regardless of
/// color mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerBlendingRanges {
    pub ranges: Vec<BlendingRange>,
}

impl Default for LayerBlendingRanges {
    fn default() -> Self {
        Self {
            ranges: vec![BlendingRange::default(); 5],
        }
    }
}

impl LayerBlendingRanges {
    pub fn read(reader: &mut BeReader, section_end: usize) -> Result<Self> {
        let offset = reader.position() as u64;
        let len = reader.u32()? as usize;
        let end = reader
            .position()
            .checked_add(len)
            .ok_or(PsdError::InvalidData {
                offset,
                message: "layer blending ranges length overflows",
            })?;
        if end > section_end {
            return Err(PsdError::InvalidData {
                offset,
                message: "layer blending ranges exceed the layer record",
            });
        }

        let mut ranges = Vec::with_capacity(len / 8);
        while end - reader.position() >= 8 {
            let mut source = [0u8; 4];
            source.copy_from_slice(reader.take(4)?);
            let mut destination = [0u8; 4];
            destination.copy_from_slice(reader.take(4)?);
            ranges.push(BlendingRange {
                source,
                destination,
            });
        }
        // Upstream leaves a sub-8-byte remainder unread; stay aligned instead.
        let trailing = end - reader.position();
        if trailing > 0 {
            tracing::warn!("layer blending ranges have {trailing} trailing bytes");
        }
        reader.skip(trailing)?;
        Ok(Self { ranges })
    }

    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        let len = self.ranges.len() * 8;
        writer.u32(u32::try_from(len).map_err(|_| PsdError::LengthOverflow {
            actual: len as u64,
            width: 4,
        })?);
        for range in &self.ranges {
            writer.bytes(&range.source);
            writer.bytes(&range.destination);
        }
        Ok(())
    }
}

/// Which mask display setting a [`GlobalMaskSettings`] payload describes (its
/// trailing kind byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalMaskKind {
    /// The overlay color shows the selected color, inverted. Written only by
    /// beta-era Photoshop; kept for old files.
    ColorSelected,
    /// The overlay color shows the protected color (beta-era Photoshop).
    ColorProtected,
    /// Each layer's mask data carries its own value — what modern Photoshop
    /// writes.
    PerLayer,
    /// Any other byte, meaning undocumented. Preserved verbatim.
    Unknown(u8),
}

impl GlobalMaskKind {
    const fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::ColorSelected,
            1 => Self::ColorProtected,
            128 => Self::PerLayer,
            other => Self::Unknown(other),
        }
    }

    const fn as_byte(self) -> u8 {
        match self {
            Self::ColorSelected => 0,
            Self::ColorProtected => 1,
            Self::PerLayer => 128,
            Self::Unknown(byte) => byte,
        }
    }
}

/// The typed contents of a non-empty [`GlobalLayerMaskInfo`]: five big-endian
/// `u16` overlay color values, a `u16` opacity, and a kind byte — a 13-byte
/// record Photoshop pads with zeros to a 4-byte boundary (usually 16 bytes
/// total). The overlay is mask-display state, not compositing input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalMaskSettings {
    /// Undocumented overlay color space id followed by its four components,
    /// as stored (component values use the full `u16` range).
    pub overlay_color: [u16; 5],
    /// Overlay opacity as a percent, 0 (transparent) to 100 (opaque).
    pub opacity: u16,
    /// Which mask setting the overlay describes.
    pub kind: GlobalMaskKind,
}

/// GlobalLayerMaskInfo payload: empty when the document never set mask
/// display options, otherwise the 13-byte [`GlobalMaskSettings`] record
/// zero-padded to a 4-byte boundary. The bytes are authoritative for writing
/// so unusual lengths round-trip; [`parse`](Self::parse) and
/// [`set`](Self::set) cover the typed view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GlobalLayerMaskInfo {
    pub data: Vec<u8>,
}

impl GlobalLayerMaskInfo {
    /// The typed settings when the payload carries the 13-byte record
    /// (padded or not); `None` for an empty or truncated payload.
    pub fn parse(&self) -> Option<GlobalMaskSettings> {
        if self.data.len() < 13 {
            return None;
        }
        let read_u16 = |at: usize| u16::from_be_bytes([self.data[at], self.data[at + 1]]);
        Some(GlobalMaskSettings {
            overlay_color: [
                read_u16(0),
                read_u16(2),
                read_u16(4),
                read_u16(6),
                read_u16(8),
            ],
            opacity: read_u16(10),
            kind: GlobalMaskKind::from_byte(self.data[12]),
        })
    }

    /// Replace the payload with the 13-byte settings record padded to four
    /// bytes, the shape Photoshop writes.
    pub fn set(&mut self, settings: GlobalMaskSettings) {
        let mut data = Vec::with_capacity(16);
        for component in settings.overlay_color {
            data.extend_from_slice(&component.to_be_bytes());
        }
        data.extend_from_slice(&settings.opacity.to_be_bytes());
        data.push(settings.kind.as_byte());
        data.resize(16, 0);
        self.data = data;
    }
}

/// The compressed channel data of one layer, index aligned with the layer's
/// [`LayerRecord::channels`].
///
/// Like [`LayerRecord`], it borrows what a document already holds so that
/// staging a write does not copy it: a parse owns its payloads, a write may
/// borrow them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChannelImageData<'a> {
    pub channels: Vec<ChannelData<'a>>,
}

impl<'a> ChannelImageData<'a> {
    /// Read one layer's channels using the sizes declared in its record. The
    /// payloads are copied out of the reader, so the result owns them.
    pub fn read(reader: &mut BeReader, record: &LayerRecord) -> Result<Self> {
        Self::read_with_channels(reader, record, |bytes| Cow::Owned(bytes.to_vec()))
    }

    fn read_with_channels<'data>(
        reader: &mut BeReader<'data>,
        record: &LayerRecord,
        payload: fn(&'data [u8]) -> Cow<'a, [u8]>,
    ) -> Result<Self> {
        let mut channels = Vec::with_capacity(record.channels.len().min(64));
        for info in &record.channels {
            let offset = reader.position() as u64;
            // Old Photoshop writes empty layers (a 0x0 rectangle) with
            // zero-length channel data: no payload and no two-byte
            // compression marker at all. Treat that as an empty channel
            // instead of erroring, the way Photoshop reads it back. A rewrite
            // normalizes it to the marker-only form.
            if info.size == 0 {
                channels.push(ChannelData {
                    compression: Compression::Raw,
                    data: Cow::Borrowed(&[]),
                });
                continue;
            }
            // A length of one is malformed — the spec requires zero or at
            // least two, for the compression marker — but files carry it.
            // Consume the stray byte so the channels after it stay aligned
            // and read the channel as empty.
            if info.size == 1 {
                reader.skip(1)?;
                channels.push(ChannelData {
                    compression: Compression::Raw,
                    data: Cow::Borrowed(&[]),
                });
                continue;
            }
            let compression = Compression::from_raw(reader.u16()?)?;
            let payload_len = info.size.checked_sub(2).ok_or(PsdError::InvalidData {
                offset,
                message: "channel size is smaller than its compression marker",
            })?;
            let payload_len = usize::try_from(payload_len).map_err(|_| PsdError::InvalidData {
                offset,
                message: "channel size does not fit the platform",
            })?;
            channels.push(ChannelData {
                compression,
                data: payload(reader.take(payload_len)?),
            });
        }
        Ok(Self { channels })
    }

    /// Write each channel as its 2-byte compression marker plus payload.
    /// The record's `size` fields must already match (`payload + 2`).
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        for channel in &self.channels {
            writer.u16(channel.compression.as_raw());
            writer.bytes(&channel.data);
        }
        Ok(())
    }
}

/// One channel's compressed payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelData<'a> {
    pub compression: Compression,
    /// Compressed bytes, excluding the 2-byte compression marker. Owned by a
    /// parse; borrowed when a write stages a payload that a document keeps
    /// compressed (a lazy read), so that writing does not copy it.
    pub data: Cow<'a, [u8]>,
}

impl<'a> ChannelData<'a> {
    /// A channel that owns its payload.
    pub fn owned(compression: Compression, data: Vec<u8>) -> Self {
        Self {
            compression,
            data: Cow::Owned(data),
        }
    }

    /// A channel that borrows its payload.
    pub fn borrowed(compression: Compression, data: &'a [u8]) -> Self {
        Self {
            compression,
            data: Cow::Borrowed(data),
        }
    }
}

/// Convenience helper for consumers that need to know how many bytes a
/// channel occupies on disk (marker included).
pub fn channel_disk_size(channel: &ChannelData) -> u64 {
    channel.data.len() as u64 + 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{ColorMode, SectionDivider};

    fn header(depth: BitDepth) -> FileHeader {
        FileHeader::new(Version::Psd, 3, 64, 64, depth, ColorMode::Rgb).unwrap()
    }

    fn sample_record() -> LayerRecord<'static> {
        LayerRecord {
            name: PascalString::new("Layer 0", 4),
            top: 0,
            left: 0,
            bottom: 64,
            right: 64,
            channels: vec![
                ChannelInfo {
                    id: ChannelId::Alpha,
                    index: -1,
                    size: 10,
                },
                ChannelInfo {
                    id: ChannelId::Red,
                    index: 0,
                    size: 6,
                },
            ],
            blend_mode: BlendMode::NORMAL,
            opacity: 255,
            clipping: 0,
            flags: LayerFlags::from_bits(0x08),
            mask_data: None,
            blending_ranges: LayerBlendingRanges::default(),
            additional_layer_info: None,
        }
    }

    #[test]
    fn layer_record_round_trips() {
        let record = sample_record();
        let mut w = BeWriter::new();
        record.write(&mut w, &header(BitDepth::Eight)).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = LayerRecord::read(&mut r, &header(BitDepth::Eight)).unwrap();
        assert_eq!(back, record);
        assert!(r.is_empty());
        assert_eq!(back.width(), 64);
        assert_eq!(back.height(), 64);
    }

    #[test]
    fn single_pixel_mask_round_trips_with_params() {
        let mut record = sample_record();
        record.mask_data = Some(LayerMaskData {
            pixel_mask: Some(LayerMask {
                top: 0,
                left: 0,
                bottom: 64,
                right: 64,
                default_color: 255,
                flags: LayerMaskFlags::from_bits(
                    LayerMaskFlags::DISABLED | LayerMaskFlags::HAS_MASK_PARAMS,
                ),
                params: Some(MaskParams {
                    flags: MaskParams::USER_MASK_FEATHER,
                    user_mask_density: None,
                    user_mask_feather: Some(2.5),
                    vector_mask_density: None,
                    vector_mask_feather: None,
                }),
            }),
            vector_mask: None,
        });

        let mut w = BeWriter::new();
        record.write(&mut w, &header(BitDepth::Eight)).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = LayerRecord::read(&mut r, &header(BitDepth::Eight)).unwrap();
        assert_eq!(back, record);
        let mask = back.mask_data.unwrap().pixel_mask.unwrap();
        assert!(mask.flags.disabled());
        assert_eq!(mask.params.unwrap().user_mask_feather, Some(2.5));
    }

    #[test]
    fn two_masks_round_trip_in_photoshop_order() {
        let vector = LayerMask {
            top: 1,
            left: 2,
            bottom: 3,
            right: 4,
            default_color: 0,
            flags: LayerMaskFlags::from_bits(LayerMaskFlags::IS_VECTOR),
            params: None,
        };
        let pixel = LayerMask {
            top: 5,
            left: 6,
            bottom: 7,
            right: 8,
            default_color: 255,
            flags: LayerMaskFlags::from_bits(0),
            params: None,
        };
        let mask_data = LayerMaskData {
            pixel_mask: Some(pixel),
            vector_mask: Some(vector),
        };
        let mut w = BeWriter::new();
        mask_data.write(&mut w).unwrap();
        let mut r = BeReader::new(w.as_slice());
        // Two masks: the second header is the real user mask slot.
        let back = LayerMaskData::read(&mut r, w.position(), true)
            .unwrap()
            .unwrap();
        assert_eq!(back, mask_data);
        assert!(r.is_empty());
    }

    #[test]
    fn mask_parameter_setters_follow_photoshops_two_record_layout() {
        let vector = LayerMask {
            top: 1,
            left: 2,
            bottom: 3,
            right: 4,
            default_color: 0,
            flags: LayerMaskFlags::from_bits(LayerMaskFlags::IS_VECTOR),
            params: None,
        };
        let pixel = LayerMask {
            top: 5,
            left: 6,
            bottom: 17,
            right: 18,
            default_color: 255,
            flags: LayerMaskFlags::from_bits(0),
            params: None,
        };
        let mut mask_data = LayerMaskData {
            pixel_mask: Some(pixel),
            vector_mask: Some(vector),
        };
        mask_data.set_user_mask_density(Some(64)).unwrap();
        mask_data.set_user_mask_feather(Some(5.0)).unwrap();
        // Parameters follow the pixel record; the flag sits on the first one.
        assert!(mask_data.vector_mask.unwrap().flags.has_mask_params());
        assert!(mask_data.vector_mask.unwrap().params.is_none());
        assert_eq!(mask_data.user_mask_density(), Some(64));

        let mut w = BeWriter::new();
        mask_data.write(&mut w).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = LayerMaskData::read(&mut r, w.position(), true)
            .unwrap()
            .unwrap();
        assert_eq!(back, mask_data);
        assert_eq!(back.user_mask_feather(), Some(5.0));

        // Clearing every parameter drops the block and the flag again.
        let mut cleared = back;
        cleared.set_user_mask_density(None).unwrap();
        cleared.set_user_mask_feather(None).unwrap();
        assert!(cleared.pixel_mask.unwrap().params.is_none());
        assert!(!cleared.vector_mask.unwrap().flags.has_mask_params());
        let mut w = BeWriter::new();
        cleared.write(&mut w).unwrap();
        let mut r = BeReader::new(w.as_slice());
        assert_eq!(
            LayerMaskData::read(&mut r, w.position(), true)
                .unwrap()
                .unwrap(),
            cleared
        );
        assert!(cleared.set_user_mask_feather(Some(f64::NAN)).is_err());
    }

    #[test]
    fn channel_records_map_user_and_real_user_masks() {
        let first = LayerMask {
            top: 0,
            left: 0,
            bottom: 2,
            right: 2,
            default_color: 0,
            flags: LayerMaskFlags::from_bits(LayerMaskFlags::IS_VECTOR),
            params: None,
        };
        let second = LayerMask {
            bottom: 8,
            right: 8,
            flags: LayerMaskFlags::from_bits(0),
            ..first
        };
        let both = LayerMaskData {
            pixel_mask: Some(second),
            vector_mask: Some(first),
        };
        assert_eq!(both.record_for_channel(-2), Some(&first));
        assert_eq!(both.record_for_channel(-3), Some(&second));
        let pixel_only = LayerMaskData {
            pixel_mask: Some(second),
            vector_mask: None,
        };
        assert_eq!(pixel_only.record_for_channel(-2), Some(&second));
        let mut empty = LayerMaskData::default();
        assert!(empty.set_user_mask_density(Some(1)).is_err());
    }

    #[test]
    fn empty_mask_section_is_none() {
        let bytes = 0u32.to_be_bytes();
        let mut r = BeReader::new(&bytes);
        assert!(LayerMaskData::read(&mut r, 4, false).unwrap().is_none());
    }

    #[test]
    fn short_mask_section_is_a_typed_error_not_a_panic() {
        // A declared length of 4 is shorter than one 18-byte mask record.
        // This must be a typed error; it used to underflow
        // `end - reader.position()` in debug builds.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(&[0; 8]);
        let mut r = BeReader::new(&bytes);
        assert!(matches!(
            LayerMaskData::read(&mut r, 12, false),
            Err(PsdError::InvalidData { .. })
        ));
    }

    #[test]
    fn blending_ranges_round_trip_arbitrary_count() {
        let mut ranges = LayerBlendingRanges::default();
        assert_eq!(ranges.ranges.len(), 5);
        ranges.ranges.truncate(3);
        let mut w = BeWriter::new();
        ranges.write(&mut w).unwrap();
        assert_eq!(&w.as_slice()[..4], &24u32.to_be_bytes());
        let mut r = BeReader::new(w.as_slice());
        let back = LayerBlendingRanges::read(&mut r, w.position()).unwrap();
        assert_eq!(back, ranges);
        assert!(r.is_empty());
    }

    #[test]
    fn zero_length_channels_read_as_empty() {
        // No bytes at all for the channel: no payload, no compression marker.
        let mut record = sample_record();
        record.channels.truncate(1);
        record.channels[0].size = 0;
        let mut reader = BeReader::new(&[]);
        let data = ChannelImageData::read(&mut reader, &record).unwrap();
        assert_eq!(data.channels.len(), 1);
        assert_eq!(data.channels[0].compression, Compression::Raw);
        assert!(data.channels[0].data.is_empty());
        assert!(reader.is_empty());
    }

    #[test]
    fn a_long_parameter_block_is_not_read_as_a_real_mask_header() {
        // A single pixel mask whose parameter block pushes the record past 36
        // bytes: the length alone must not invent a real-mask header — that
        // header follows the record's `-3` channel, and there is none here.
        let params = MaskParams {
            flags: 0x0F,
            user_mask_density: Some(200),
            user_mask_feather: Some(3.5),
            vector_mask_density: Some(100),
            vector_mask_feather: Some(7.25),
        };
        let pixel = LayerMask {
            top: 0,
            left: 0,
            bottom: 2,
            right: 2,
            default_color: 255,
            flags: LayerMaskFlags::from_bits(LayerMaskFlags::HAS_MASK_PARAMS),
            params: Some(params),
        };
        let mask_data = LayerMaskData {
            pixel_mask: Some(pixel),
            vector_mask: None,
        };

        let mut w = BeWriter::new();
        mask_data.write(&mut w).unwrap();
        assert!(
            w.position() >= 36,
            "the block must reach the ambiguous length to exercise the case"
        );
        let mut r = BeReader::new(w.as_slice());
        let back = LayerMaskData::read(&mut r, w.position(), false)
            .unwrap()
            .unwrap();
        assert_eq!(back, mask_data);
        assert_eq!(back.user_mask_feather(), Some(3.5));
        let params = back.pixel_mask.unwrap().params.unwrap();
        assert_eq!(params.vector_mask_density, Some(100));
        assert_eq!(params.vector_mask_feather, Some(7.25));
    }

    #[test]
    fn truncated_mask_parameters_keep_the_fields_that_fit() {
        // The flag byte promises a feather `f64`, the block holds two bytes:
        // the fields that fit survive, the rest are absent, and the read
        // succeeds rather than failing the file.
        let bytes = [0x01 | 0x02, 200, 0x40, 0x00];
        let mut reader = BeReader::new(&bytes);
        let params = MaskParams::read(&mut reader).unwrap();
        assert_eq!(params.flags, 0x03);
        assert_eq!(params.user_mask_density, Some(200));
        assert_eq!(params.user_mask_feather, None);
    }

    #[test]
    fn a_one_byte_channel_reads_as_empty_and_keeps_alignment() {
        // The spec allows a channel length of zero or at least two (the
        // compression marker); one is malformed but appears in files. The
        // stray byte is consumed so the following channel stays aligned.
        let mut record = sample_record();
        record.channels.truncate(2);
        record.channels[0].size = 1;
        record.channels[1].size = 6;
        let mut bytes = BeWriter::new();
        bytes.u8(0xAB); // the stray byte
        bytes.u16(1); // second channel: RLE marker
        bytes.bytes(&[0x00, 0x02, 0x00, 0xFF]);
        let payload = bytes.into_inner();
        let mut reader = BeReader::new(&payload);
        let data = ChannelImageData::read(&mut reader, &record).unwrap();
        assert_eq!(data.channels.len(), 2);
        assert!(data.channels[0].data.is_empty());
        assert_eq!(data.channels[1].compression, Compression::Rle);
        assert_eq!(data.channels[1].data, vec![0x00, 0x02, 0x00, 0xFF]);
        assert!(reader.is_empty());
    }

    #[test]
    fn merged_alpha_flag_round_trips() {
        let mut info = LayerInfo {
            layer_records: vec![sample_record()],
            channel_image_data: vec![ChannelImageData {
                channels: vec![
                    ChannelData {
                        compression: Compression::Raw,
                        data: vec![0; 8].into(),
                    },
                    ChannelData {
                        compression: Compression::Rle,
                        data: vec![1, 2, 3, 4].into(),
                    },
                ],
            }],
            has_merged_alpha: true,
        };
        // Keep the declared channel sizes consistent with the payloads.
        info.layer_records[0].channels[0].size = 10;
        info.layer_records[0].channels[1].size = 6;

        let mut w = BeWriter::new();
        info.write_section(&mut w, &header(BitDepth::Eight))
            .unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = LayerInfo::read(&mut r, &header(BitDepth::Eight)).unwrap();
        assert_eq!(back, info);
        assert!(back.has_merged_alpha);
    }

    #[test]
    fn section_round_trips_with_nested_lr16() {
        let info = LayerInfo {
            layer_records: vec![sample_record()],
            channel_image_data: vec![ChannelImageData {
                channels: vec![
                    ChannelData {
                        compression: Compression::Raw,
                        data: vec![0; 8].into(),
                    },
                    ChannelData {
                        compression: Compression::Raw,
                        data: vec![0; 4].into(),
                    },
                ],
            }],
            has_merged_alpha: false,
        };
        let section = LayerAndMaskInformation {
            layer_info: info,
            global_layer_mask_info: GlobalLayerMaskInfo::default(),
            additional_layer_info: None,
        };

        let mut w = BeWriter::new();
        section.write(&mut w, &header(BitDepth::Sixteen)).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = LayerAndMaskInformation::read(&mut r, &header(BitDepth::Sixteen)).unwrap();
        assert_eq!(back.layer_info, section.layer_info);
        assert_eq!(back.global_layer_mask_info, section.global_layer_mask_info);
        // The nested block was written with the layer content, and reads back as
        // a placeholder: its layers are in `layer_info`, not copied a second time.
        let block = back
            .additional_layer_info
            .as_ref()
            .unwrap()
            .get(TaggedBlockKey::LR16)
            .unwrap();
        assert!(block.data.is_empty());

        // A second round-trip of the parsed form is stable.
        let mut w = BeWriter::new();
        back.write(&mut w, &header(BitDepth::Sixteen)).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let again = LayerAndMaskInformation::read(&mut r, &header(BitDepth::Sixteen)).unwrap();
        assert_eq!(again, back);
    }

    #[test]
    fn a_nested_block_with_no_layers_keeps_its_bytes() {
        // Nothing parses out of it, so nothing can regenerate it: it must come
        // back as it was, not as an empty placeholder.
        let mut ali = AdditionalLayerInfo::new();
        ali.push(TaggedBlock::new(TaggedBlockKey::LR16, vec![0, 0, 0, 0]));
        ali.push(TaggedBlock::new(TaggedBlockKey::LUNI, vec![1, 2, 3, 4]));
        let section = LayerAndMaskInformation {
            layer_info: LayerInfo::default(),
            global_layer_mask_info: GlobalLayerMaskInfo::default(),
            additional_layer_info: Some(Cow::Owned(ali.clone())),
        };
        let mut w = BeWriter::new();
        section.write(&mut w, &header(BitDepth::Sixteen)).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = LayerAndMaskInformation::read(&mut r, &header(BitDepth::Sixteen)).unwrap();
        assert!(back.layer_info.layer_records.is_empty());
        assert_eq!(back.additional_layer_info.as_deref(), Some(&ali));
    }

    #[test]
    fn a_repeated_nested_block_is_written_once() {
        // The layer data goes into the first `Lr16`; a repeat could only be a
        // stale second copy of it, so it is dropped, and the blocks after it stay.
        let info = LayerInfo {
            layer_records: vec![sample_record()],
            channel_image_data: vec![ChannelImageData {
                channels: vec![ChannelData::owned(Compression::Raw, vec![0; 8])],
            }],
            has_merged_alpha: false,
        };
        let mut ali = AdditionalLayerInfo::new();
        ali.push(TaggedBlock::new(TaggedBlockKey::LR16, Vec::new()));
        ali.push(TaggedBlock::new(TaggedBlockKey::LR16, vec![9, 9, 9, 9]));
        ali.push(TaggedBlock::new(TaggedBlockKey::LUNI, vec![1, 2, 3, 4]));
        let mut section = LayerAndMaskInformation {
            layer_info: info,
            global_layer_mask_info: GlobalLayerMaskInfo::default(),
            additional_layer_info: Some(Cow::Owned(ali)),
        };
        section.layer_info.layer_records[0].channels = vec![ChannelInfo {
            id: ChannelId::Red,
            index: 0,
            size: 10,
        }];
        let mut w = BeWriter::new();
        section.write(&mut w, &header(BitDepth::Sixteen)).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = LayerAndMaskInformation::read(&mut r, &header(BitDepth::Sixteen)).unwrap();
        assert_eq!(back.layer_info.layer_records.len(), 1);
        let blocks = &back.additional_layer_info.as_ref().unwrap().blocks;
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].key, TaggedBlockKey::LR16);
        assert!(blocks[0].data.is_empty());
        assert_eq!(blocks[1].key, TaggedBlockKey::LUNI);

        // The streaming writer agrees with the buffered one.
        let mut streamed = Vec::new();
        section
            .write_to(&mut streamed, &header(BitDepth::Sixteen))
            .unwrap();
        assert_eq!(streamed, w.as_slice());
    }

    #[test]
    fn section_divider_block_is_carried_through() {
        let mut record = sample_record();
        let mut ali = AdditionalLayerInfo::new();
        let mut data = Vec::new();
        data.extend_from_slice(&SectionDivider::OpenFolder.as_raw().to_be_bytes());
        data.extend_from_slice(b"8BIM");
        data.extend_from_slice(b"norm");
        ali.push(TaggedBlock::new(TaggedBlockKey::LSCT, data));
        record.additional_layer_info = Some(Cow::Owned(ali));

        let mut w = BeWriter::new();
        record.write(&mut w, &header(BitDepth::Eight)).unwrap();
        let mut r = BeReader::new(w.as_slice());
        let back = LayerRecord::read(&mut r, &header(BitDepth::Eight)).unwrap();
        let block = back
            .additional_layer_info
            .as_ref()
            .unwrap()
            .get(TaggedBlockKey::LSCT)
            .unwrap();
        assert_eq!(
            SectionDivider::from_raw(u32::from_be_bytes(block.data[..4].try_into().unwrap())),
            SectionDivider::OpenFolder
        );
    }

    #[test]
    fn global_layer_mask_info_parses_the_settings_record() {
        // The 16-byte payload Photoshop actually writes (space 0, red overlay
        // at 50%, per-layer kind) — the shape the corpus carries.
        let info = GlobalLayerMaskInfo {
            data: vec![
                0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x32, 0x80, 0x00,
                0x00, 0x00,
            ],
        };
        assert_eq!(
            info.parse(),
            Some(GlobalMaskSettings {
                overlay_color: [0, 0xffff, 0, 0, 0],
                opacity: 50,
                kind: GlobalMaskKind::PerLayer,
            })
        );

        // A 13-byte body padded to only 14 bytes is in the corpus too; the
        // first 13 bytes still decode and the payload stays authoritative.
        let short = GlobalLayerMaskInfo {
            data: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0],
        };
        assert_eq!(
            short.parse(),
            Some(GlobalMaskSettings {
                overlay_color: [0; 5],
                opacity: 0,
                kind: GlobalMaskKind::PerLayer,
            })
        );
        assert_eq!(GlobalLayerMaskInfo::default().parse(), None);
        assert_eq!(GlobalLayerMaskInfo { data: vec![0; 12] }.parse(), None);
    }

    #[test]
    fn global_layer_mask_info_set_writes_the_photoshop_shape() {
        let mut info = GlobalLayerMaskInfo::default();
        info.set(GlobalMaskSettings {
            overlay_color: [1, 2, 3, 4, 5],
            opacity: 75,
            kind: GlobalMaskKind::Unknown(7),
        });
        assert_eq!(info.data.len(), 16);
        assert_eq!(
            info.parse(),
            Some(GlobalMaskSettings {
                overlay_color: [1, 2, 3, 4, 5],
                opacity: 75,
                kind: GlobalMaskKind::Unknown(7),
            })
        );
        assert!(info.data[13..].iter().all(|&byte| byte == 0));
    }
}
