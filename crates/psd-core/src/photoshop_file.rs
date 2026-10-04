//! The whole-file aggregate (`PhotoshopFile/PhotoshopFile.{h,cpp}`).
//!
//! Section order: [`FileHeader`] | [`ColorModeData`] | [`ImageResources`] |
//! [`LayerAndMaskInformation`] | [`ImageData`].
//!
//! Reading stops after the layer-and-mask section. The document layer may
//! retain the trailing merged section when a source document has no layer
//! records; ordinary document writes continue to synthesize a zeroed one.

use crate::color_mode_data::ColorModeData;
use crate::error::Result;
use crate::header::FileHeader;
use crate::image_data::ImageData;
use crate::image_resources::ImageResources;
use crate::io::{BeReader, BeWriter};
use crate::layer_and_mask_info::LayerAndMaskInformation;

/// A parsed PSD/PSB file at the raw-format layer.
///
/// The layer/mask section borrows its tagged blocks (`Cow`), so document-layer
/// write staging can assemble a file without deep-copying every block; parse
/// results own their data (`Cow::Owned`).
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoshopFile<'a> {
    pub header: FileHeader,
    pub color_mode_data: ColorModeData,
    pub image_resources: ImageResources,
    pub layer_and_mask_info: LayerAndMaskInformation<'a>,
    /// Set the channel count before writing; the merged section can also be
    /// retained raw by the document layer when the file has no layer tree.
    pub image_data: ImageData,
}

impl<'a> PhotoshopFile<'a> {
    /// Read the four leading sections from the current reader position.
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        Self::read_with_channels(reader, |bytes| bytes.to_vec().into())
    }

    /// Read while borrowing compressed channel payloads from the input.
    /// Metadata remains owned. This avoids staging a second copy of every
    /// compressed channel before an eager document read decodes it.
    pub fn read_borrowed(reader: &mut BeReader<'a>) -> Result<Self> {
        Self::read_with_channels(reader, std::borrow::Cow::Borrowed)
    }

    fn read_with_channels<'data>(
        reader: &mut BeReader<'data>,
        payload: fn(&'data [u8]) -> std::borrow::Cow<'a, [u8]>,
    ) -> Result<Self> {
        let header = FileHeader::read(reader)?;
        let color_mode_data = ColorModeData::read(reader)?;
        let image_resources = ImageResources::read(reader)?;
        let layer_and_mask_info =
            LayerAndMaskInformation::read_with_channels(reader, &header, payload)?;
        Ok(Self {
            header,
            color_mode_data,
            image_resources,
            layer_and_mask_info,
            image_data: ImageData::default(),
        })
    }

    /// Write all five sections, ending with the zeroed merged composite.
    pub fn write(&self, writer: &mut BeWriter) -> Result<()> {
        self.header.write(writer);
        self.color_mode_data.write(writer, &self.header)?;
        self.image_resources.write(writer)?;
        self.layer_and_mask_info.write(writer, &self.header)?;
        self.image_data.write(writer, &self.header)?;
        Ok(())
    }

    /// Stream all five sections to `sink`: the bytes [`write`](Self::write)
    /// produces, without holding the whole file in memory. The layer-and-mask
    /// section, which is nearly all of a real document, is written straight from
    /// the channel payloads, and each payload is released once written, so this
    /// takes `&mut self` and leaves the channel data empty.
    ///
    /// `sink` should be buffered when it is a file. When it is a `Vec`, reserve
    /// [`size_hint`](Self::size_hint) bytes first.
    pub fn write_to<W: std::io::Write>(&mut self, sink: &mut W) -> Result<()> {
        // The small leading sections and the trailing composite are staged.
        let mut leading = BeWriter::new();
        self.header.write(&mut leading);
        self.color_mode_data.write(&mut leading, &self.header)?;
        self.image_resources.write(&mut leading)?;
        let mut composite = BeWriter::new();
        self.image_data.write(&mut composite, &self.header)?;

        sink.write_all(leading.as_slice())?;
        drop(leading);
        self.layer_and_mask_info.write_to(sink, &self.header)?;
        sink.write_all(composite.as_slice())?;
        Ok(())
    }

    /// A close estimate of the file's size in bytes, for reserving space in a
    /// buffer: the channel data plus room for everything else.
    pub fn size_hint(&self) -> usize {
        let payload: usize = self
            .layer_and_mask_info
            .layer_info
            .channel_image_data
            .iter()
            .flat_map(|layer| &layer.channels)
            .map(|channel| channel.data.len() + 2)
            .sum();
        payload + self.image_data.raw_section().map_or(0, <[u8]>::len) + (1 << 20)
    }

    /// Write a metadata outline, producing channel bytes when their positions
    /// are reached. The callback writes compression markers and payloads in
    /// record/channel order and returns their complete lengths in that order.
    /// Length fields are validated and backpatched before returning. The sink
    /// can contain partial data on failure; atomic replacement belongs to the
    /// document layer. Layerless documents do not invoke the callback.
    pub fn write_seekable<W: std::io::Write + std::io::Seek>(
        &mut self,
        sink: &mut W,
        channels: impl FnOnce(&mut dyn std::io::Write) -> Result<Vec<Vec<u64>>>,
    ) -> Result<()> {
        let mut sink = crate::seek_writer::PositionWriter::new(sink)?;
        let mut leading = BeWriter::new();
        self.header.write(&mut leading);
        self.color_mode_data.write(&mut leading, &self.header)?;
        self.image_resources.write(&mut leading)?;
        std::io::Write::write_all(&mut sink, leading.as_slice())?;
        drop(leading);
        crate::seek_writer::write_layer_section(
            &mut self.layer_and_mask_info,
            &mut sink,
            &self.header,
            channels,
        )?;
        self.image_data.write_to(&mut sink, &self.header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::{BitDepth, ColorMode, Version};

    #[test]
    fn aggregate_round_trips_minimal_document() {
        // 16-bit documents may have an empty main LayerInfo section.
        let header =
            FileHeader::new(Version::Psd, 3, 64, 64, BitDepth::Sixteen, ColorMode::Rgb).unwrap();
        let file = PhotoshopFile {
            header,
            color_mode_data: ColorModeData::default(),
            image_resources: ImageResources::new(),
            layer_and_mask_info: LayerAndMaskInformation::default(),
            image_data: ImageData::new(3),
        };

        let mut w = BeWriter::new();
        file.write(&mut w).unwrap();

        let mut r = BeReader::new(w.as_slice());
        let back = PhotoshopFile::read(&mut r).unwrap();
        assert_eq!(back.header, file.header);
        assert_eq!(back.color_mode_data, file.color_mode_data);
        assert_eq!(back.image_resources, file.image_resources);
        assert_eq!(back.layer_and_mask_info, file.layer_and_mask_info);
        // The merged composite is written but not parsed on read.
        assert_eq!(back.image_data, ImageData::default());
        assert!(r.remaining() > 0);
    }
}
