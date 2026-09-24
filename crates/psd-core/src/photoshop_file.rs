//! The whole-file aggregate (`PhotoshopFile/PhotoshopFile.{h,cpp}`).
//!
//! Section order: [`FileHeader`] | [`ColorModeData`] | [`ImageResources`] |
//! [`LayerAndMaskInformation`] | [`ImageData`].
//!
//! Reading stops after the layer-and-mask section: the trailing merged
//! composite is never parsed (upstream does the same — the port always
//! synthesizes a zeroed one on write).

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
    /// Set the channel count before writing; read leaves it empty because the
    /// section is never parsed.
    pub image_data: ImageData,
}

impl<'a> PhotoshopFile<'a> {
    /// Read the four leading sections from the current reader position.
    pub fn read(reader: &mut BeReader) -> Result<Self> {
        let header = FileHeader::read(reader)?;
        let color_mode_data = ColorModeData::read(reader)?;
        let image_resources = ImageResources::read(reader)?;
        let layer_and_mask_info = LayerAndMaskInformation::read(reader, &header)?;
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
