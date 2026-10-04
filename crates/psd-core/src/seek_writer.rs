//! Seekable format emission with channel lengths supplied during the write.

use std::io::{Seek, SeekFrom, Write};

use crate::{
    BeWriter, BitDepth, FileHeader, LayerAndMaskInformation, LayerInfo, PsdError, Result,
    TaggedBlock, TaggedBlockKey,
};

/// Counts writes locally, so bookmarks do not flush a buffered sink.
pub(crate) struct PositionWriter<'a, W> {
    inner: &'a mut W,
    position: u64,
}

impl<'a, W: Write + Seek> PositionWriter<'a, W> {
    pub(crate) fn new(inner: &'a mut W) -> Result<Self> {
        let position = inner.stream_position()?;
        Ok(Self { inner, position })
    }

    fn position(&self) -> u64 {
        self.position
    }

    fn patch(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        let end = self.position;
        self.seek(SeekFrom::Start(offset))?;
        self.write_all(bytes)?;
        self.seek(SeekFrom::Start(end))?;
        Ok(())
    }
}

impl<W: Write> Write for PositionWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.inner.write(bytes)?;
        self.position = self
            .position
            .checked_add(count as u64)
            .ok_or_else(|| std::io::Error::other("stream position overflows"))?;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Seek> Seek for PositionWriter<'_, W> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        if from == SeekFrom::Current(0) {
            return Ok(self.position);
        }
        self.position = self.inner.seek(from)?;
        Ok(self.position)
    }
}

type ChannelSizes = Vec<Vec<u64>>;

pub(crate) fn write_layer_section<W: Write + Seek>(
    section: &mut LayerAndMaskInformation<'_>,
    sink: &mut PositionWriter<'_, W>,
    header: &FileHeader,
    channels: impl FnOnce(&mut dyn Write) -> Result<ChannelSizes>,
) -> Result<()> {
    let marker = sink.position();
    sink.write_all(&crate::layer_and_mask_info::length_field(
        header.version,
        0,
    )?)?;
    let start = sink.position();
    let mut channels = Some(channels);
    let has_layers = !section.layer_info.layer_records.is_empty();
    if header.depth == BitDepth::Eight && has_layers {
        let inner_marker = sink.position();
        sink.write_all(&crate::layer_and_mask_info::length_field(
            header.version,
            0,
        )?)?;
        let content = sink.position();
        write_layer_content(
            &mut section.layer_info,
            sink,
            header,
            channels.take().unwrap(),
        )?;
        let length = sink.position() - content;
        write_padding(sink, length)?;
        sink.patch(
            inner_marker,
            &crate::layer_and_mask_info::length_field(header.version, length + padding(length))?,
        )?;
    } else {
        sink.write_all(&crate::layer_and_mask_info::length_field(
            header.version,
            0,
        )?)?;
    }

    let mask = &section.global_layer_mask_info.data;
    let length = u32::try_from(mask.len()).map_err(|_| PsdError::LengthOverflow {
        actual: mask.len() as u64,
        width: 4,
    })?;
    sink.write_all(&length.to_be_bytes())?;
    sink.write_all(mask)?;

    let nested_key = match header.depth {
        BitDepth::Sixteen => Some(TaggedBlockKey::LR16),
        BitDepth::ThirtyTwo => Some(TaggedBlockKey::LR32),
        _ => None,
    }
    .filter(|_| has_layers);
    let blocks = section
        .additional_layer_info
        .as_ref()
        .map(|blocks| blocks.blocks.as_slice())
        .unwrap_or(&[]);
    let mut nested_placed = false;
    if let Some(key) = nested_key {
        if !blocks.iter().any(|block| block.key == key) {
            write_nested(
                &TaggedBlock::new(key, Vec::new()),
                &mut section.layer_info,
                sink,
                header,
                channels.take().unwrap(),
            )?;
            nested_placed = true;
        }
    }
    for block in blocks {
        if nested_key == Some(block.key) {
            if !nested_placed {
                write_nested(
                    block,
                    &mut section.layer_info,
                    sink,
                    header,
                    channels.take().unwrap(),
                )?;
                nested_placed = true;
            }
        } else if !crate::layer_and_mask_info::is_emptied_layer_data(block, header) {
            block.write_to(sink, header, 4)?;
        }
    }
    let length = sink.position() - start;
    write_padding(sink, length)?;
    sink.patch(
        marker,
        &crate::layer_and_mask_info::length_field(header.version, length + padding(length))?,
    )?;
    Ok(())
}

fn write_nested<W: Write + Seek>(
    block: &TaggedBlock,
    info: &mut LayerInfo<'_>,
    sink: &mut PositionWriter<'_, W>,
    header: &FileHeader,
    channels: impl FnOnce(&mut dyn Write) -> Result<ChannelSizes>,
) -> Result<()> {
    let marker = sink.position();
    sink.write_all(&block.header_bytes(header, 0)?)?;
    let content = sink.position();
    write_layer_content(info, sink, header, channels)?;
    let length = sink.position() - content;
    write_padding(sink, length)?;
    // Document-level block padding is outside the declared payload length.
    sink.patch(marker, &block.header_bytes(header, length)?)
}

fn write_layer_content<W: Write + Seek>(
    info: &mut LayerInfo<'_>,
    sink: &mut PositionWriter<'_, W>,
    header: &FileHeader,
    channels: impl FnOnce(&mut dyn Write) -> Result<ChannelSizes>,
) -> Result<()> {
    let count = i16::try_from(info.layer_records.len()).map_err(|_| PsdError::LengthOverflow {
        actual: info.layer_records.len() as u64,
        width: 2,
    })?;
    sink.write_all(&(if info.has_merged_alpha { -count } else { count }).to_be_bytes())?;
    // The records stay staged (they are metadata, small beside the channel
    // data) so every channel length is patched in memory and the block is
    // rewritten with one seek, not one per layer.
    let records_start = sink.position();
    let mut records = BeWriter::new();
    let mut length_offsets = Vec::with_capacity(info.layer_records.len());
    for record in &info.layer_records {
        // Offsets are positions in `records`: each record's channel lengths.
        length_offsets.push(record.write_with_channel_offsets(&mut records, header)?);
    }
    let mut records = records.into_inner();
    sink.write_all(&records)?;
    let start = sink.position();
    let sizes = channels(sink)?;
    if sizes.len() != info.layer_records.len()
        || sizes
            .iter()
            .zip(&info.layer_records)
            .any(|(sizes, record)| sizes.len() != record.channels.len())
    {
        return Err(PsdError::InvalidData {
            offset: start,
            message: "streamed channel counts do not match layer records",
        });
    }
    let actual = sizes
        .iter()
        .flatten()
        .try_fold(0u64, |sum, size| sum.checked_add(*size))
        .ok_or(PsdError::InvalidData {
            offset: start,
            message: "streamed channel lengths overflow",
        })?;
    if sink.position() - start != actual {
        return Err(PsdError::InvalidData {
            offset: start,
            message: "streamed channel lengths do not match bytes written",
        });
    }
    for ((record, sizes), offsets) in info.layer_records.iter_mut().zip(sizes).zip(length_offsets) {
        for ((channel, size), offset) in record.channels.iter_mut().zip(sizes).zip(offsets) {
            channel.size = size;
            let mut field = BeWriter::new();
            field.len(header.version, size)?;
            let field = field.as_slice();
            records[offset..offset + field.len()].copy_from_slice(field);
        }
    }
    if !records.is_empty() {
        sink.patch(records_start, &records)?;
    }
    Ok(())
}

fn padding(length: u64) -> u64 {
    (4 - length % 4) % 4
}
fn write_padding<W: Write>(sink: &mut W, length: u64) -> Result<()> {
    sink.write_all(&[0; 3][..padding(length) as usize])?;
    Ok(())
}
