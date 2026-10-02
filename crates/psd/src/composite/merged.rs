//! The merged `ImageData` fallback for documents without a layer tree.
//!
//! The format reader leaves this section raw because `psd-core` must not
//! depend on pixel codecs. The document compositor retains the bounded section
//! here, decodes it on demand, and preserves it verbatim on an untouched save.

use psd_core::{
    BitDepth as CoreBitDepth, ColorMode, Compression, FileHeader, PsdError, Result, Version,
};

use crate::bitdepth::BitDepth;
use crate::channels::decompress_channel;

const MAX_MERGED_SECTION_BYTES: usize = 512 * 1024 * 1024;
const MAX_MERGED_DECODED_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergedImageData {
    pub version: Version,
    pub width: u32,
    pub height: u32,
    pub depth: CoreBitDepth,
    pub color_mode: ColorMode,
    pub channels: u16,
    pub transparency: bool,
    /// Compression marker followed by the original payload.
    section: Vec<u8>,
}

impl MergedImageData {
    /// Convert a retained layerless composite to another sample depth. The
    /// plane order, transparency flag and document geometry are kept; its raw
    /// image payload is rewritten without changing the pixel values beyond
    /// the source and target sample conversion rules.
    pub(crate) fn convert_depth<S: BitDepth, D: BitDepth>(&self) -> Result<Self> {
        if S::DEPTH == D::DEPTH {
            return Ok(self.clone());
        }
        let pixels = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or_else(|| invalid("merged conversion dimensions overflow"))?;
        let decoded_size = pixels
            .checked_mul(usize::from(self.channels))
            .and_then(|samples| samples.checked_mul(D::SIZE))
            .ok_or_else(|| invalid("converted merged image size overflows"))?;
        if decoded_size > MAX_MERGED_DECODED_BYTES {
            return Err(PsdError::ExceededMemoryLimit {
                requested: decoded_size,
                available: MAX_MERGED_DECODED_BYTES,
            });
        }
        let section_size = decoded_size
            .checked_add(2)
            .ok_or_else(|| invalid("converted merged section size overflows"))?;
        if section_size > MAX_MERGED_SECTION_BYTES {
            return Err(PsdError::ExceededMemoryLimit {
                requested: section_size,
                available: MAX_MERGED_SECTION_BYTES,
            });
        }

        // A depth change cannot reuse the packed source stream; write a valid
        // planar Raw section for the target sample width.
        let mut section = Vec::with_capacity(section_size);
        section.extend_from_slice(&Compression::Raw.as_raw().to_be_bytes());
        for plane in self.decode::<S>()? {
            let converted: Vec<D> = plane
                .into_iter()
                .map(crate::bitdepth::convert_sample::<S, D>)
                .collect();
            section.extend_from_slice(&psd_codecs::endian::encode_be_bytes(&converted));
        }
        Ok(Self {
            version: self.version,
            width: self.width,
            height: self.height,
            depth: match D::DEPTH {
                8 => CoreBitDepth::Eight,
                16 => CoreBitDepth::Sixteen,
                32 => CoreBitDepth::ThirtyTwo,
                _ => unreachable!("BitDepth is sealed to supported sample widths"),
            },
            color_mode: self.color_mode,
            channels: self.channels,
            transparency: self.transparency,
            section,
        })
    }

    pub fn read(section: &[u8], header: &FileHeader, transparency: bool) -> Result<Option<Self>> {
        if section.len() < 2 {
            return Ok(None);
        }
        if section.len() > MAX_MERGED_SECTION_BYTES {
            return Err(PsdError::ExceededMemoryLimit {
                requested: section.len(),
                available: MAX_MERGED_SECTION_BYTES,
            });
        }
        let compression = Compression::from_raw(u16::from_be_bytes([section[0], section[1]]))?;
        if header.width == 0 || header.height == 0 || header.num_channels == 0 {
            return Err(invalid(
                "stored merged image has empty dimensions or channels",
            ));
        }
        if compression == Compression::ZipPrediction && header.depth == CoreBitDepth::One {
            return Err(invalid("one-bit merged images cannot use ZIP prediction"));
        }
        Ok(Some(Self {
            version: header.version,
            width: header.width,
            height: header.height,
            depth: header.depth,
            color_mode: header.color_mode,
            channels: header.num_channels,
            transparency,
            section: section.to_vec(),
        }))
    }

    pub fn section(&self) -> &[u8] {
        &self.section
    }

    pub fn decode<T: BitDepth>(&self) -> Result<Vec<Vec<T>>> {
        let width = usize::try_from(self.width)
            .map_err(|_| invalid("stored merged image width is unaddressable"))?;
        let height = usize::try_from(self.height)
            .map_err(|_| invalid("stored merged image height is unaddressable"))?;
        let pixels = width
            .checked_mul(height)
            .ok_or_else(|| invalid("stored merged image dimensions overflow"))?;
        let channels = usize::from(self.channels);
        let output_bytes = pixels
            .checked_mul(channels)
            .and_then(|count| count.checked_mul(T::SIZE))
            .ok_or_else(|| invalid("stored merged image decoded size overflows"))?;
        if output_bytes > MAX_MERGED_DECODED_BYTES {
            return Err(PsdError::ExceededMemoryLimit {
                requested: output_bytes,
                available: MAX_MERGED_DECODED_BYTES,
            });
        }
        let row_bytes = match self.depth {
            CoreBitDepth::One => width.div_ceil(8).max(1),
            _ => width
                .checked_mul(self.depth.bytes_per_sample() as usize)
                .ok_or_else(|| invalid("stored merged image row size overflows"))?,
        };
        let packed_len = row_bytes
            .checked_mul(height)
            .and_then(|count| count.checked_mul(channels))
            .ok_or_else(|| invalid("stored merged image compressed size overflows"))?;
        let payload = &self.section[2..];
        if packed_len > MAX_MERGED_DECODED_BYTES || payload.len() > MAX_MERGED_SECTION_BYTES {
            return Err(PsdError::ExceededMemoryLimit {
                requested: packed_len.max(payload.len()),
                available: MAX_MERGED_DECODED_BYTES,
            });
        }
        let compression =
            Compression::from_raw(u16::from_be_bytes([self.section[0], self.section[1]]))?;
        match compression {
            Compression::Raw => {
                if payload.len() != packed_len {
                    return Err(invalid(
                        "raw merged image length does not match its dimensions",
                    ));
                }
                self.decode_planes::<T>(payload, row_bytes * height)
            }
            Compression::Zip => {
                let bytes = psd_codecs::zip::decompress(payload, packed_len)
                    .map_err(|error| PsdError::Compression(error.to_string()))?;
                self.decode_planes::<T>(&bytes, row_bytes * height)
            }
            Compression::ZipPrediction => {
                let bytes = psd_codecs::zip::decompress(payload, packed_len)
                    .map_err(|error| PsdError::Compression(error.to_string()))?;
                let rows = height
                    .checked_mul(channels)
                    .ok_or_else(|| invalid("merged prediction row count overflows"))?;
                let samples = T::zip_prediction_decode(&bytes, width, rows)
                    .map_err(|error| PsdError::Compression(error.to_string()))?;
                if samples.len() != pixels * channels {
                    return Err(invalid(
                        "predicted merged sample count does not match its dimensions",
                    ));
                }
                Ok(samples.chunks_exact(pixels).map(<[T]>::to_vec).collect())
            }
            Compression::Rle => {
                let count = channels
                    .checked_mul(height)
                    .ok_or_else(|| invalid("merged RLE row count overflows"))?;
                let size_bytes = if self.version == Version::Psd { 2 } else { 4 };
                let table_bytes = count
                    .checked_mul(size_bytes)
                    .ok_or_else(|| invalid("merged RLE table overflows"))?;
                if table_bytes > payload.len() {
                    return Err(invalid("merged RLE row table exceeds the section"));
                }
                let mut reader = psd_core::BeReader::new(payload);
                let mut lengths = Vec::with_capacity(count);
                let mut stream_bytes = 0usize;
                for _ in 0..count {
                    let length = if size_bytes == 2 {
                        usize::from(reader.u16()?)
                    } else {
                        usize::try_from(reader.u32()?)
                            .map_err(|_| invalid("merged RLE row is unaddressable"))?
                    };
                    stream_bytes = stream_bytes
                        .checked_add(length)
                        .ok_or_else(|| invalid("merged RLE payload overflows"))?;
                    lengths.push(length);
                }
                if table_bytes
                    .checked_add(stream_bytes)
                    .is_none_or(|size| size > payload.len())
                {
                    return Err(invalid("merged RLE payload exceeds the section"));
                }
                let mut streams = Vec::with_capacity(channels);
                for channel in 0..channels {
                    let mut encoded = Vec::with_capacity(
                        height * size_bytes
                            + lengths[channel * height..(channel + 1) * height]
                                .iter()
                                .sum::<usize>(),
                    );
                    for length in &lengths[channel * height..(channel + 1) * height] {
                        if size_bytes == 2 {
                            encoded.extend_from_slice(
                                &u16::try_from(*length)
                                    .map_err(|_| {
                                        invalid("merged RLE row length exceeds PSD limit")
                                    })?
                                    .to_be_bytes(),
                            );
                        } else {
                            encoded.extend_from_slice(
                                &u32::try_from(*length)
                                    .map_err(|_| {
                                        invalid("merged RLE row length exceeds PSB limit")
                                    })?
                                    .to_be_bytes(),
                            );
                        }
                    }
                    for length in &lengths[channel * height..(channel + 1) * height] {
                        encoded.extend_from_slice(reader.take(*length)?);
                    }
                    streams.push(encoded);
                }
                if self.depth == CoreBitDepth::One {
                    streams
                        .into_iter()
                        .map(|stream| {
                            crate::channels::decompress_merged_channel::<T>(
                                compression,
                                &stream,
                                width,
                                height,
                                self.version,
                                1,
                            )
                        })
                        .collect()
                } else {
                    streams
                        .into_iter()
                        .map(|stream| {
                            decompress_channel::<T>(
                                compression,
                                &stream,
                                width,
                                height,
                                self.version,
                                self.depth.as_raw(),
                            )
                        })
                        .collect()
                }
            }
        }
    }

    fn decode_planes<T: BitDepth>(&self, bytes: &[u8], plane_stride: usize) -> Result<Vec<Vec<T>>> {
        let count = usize::from(self.channels);
        let size = self.depth.bytes_per_sample() as usize;
        let mut planes = Vec::with_capacity(count);
        for channel in 0..count {
            let start = channel
                .checked_mul(plane_stride)
                .ok_or_else(|| invalid("merged channel offset overflows"))?;
            let end = start
                .checked_add(plane_stride)
                .ok_or_else(|| invalid("merged channel length overflows"))?;
            let data = bytes
                .get(start..end)
                .ok_or_else(|| invalid("merged channel ends early"))?;
            if self.depth == CoreBitDepth::One {
                planes.push(crate::channels::decompress_merged_channel::<T>(
                    Compression::Raw,
                    data,
                    self.width as usize,
                    self.height as usize,
                    self.version,
                    1,
                )?);
            } else {
                let sample_bytes = plane_stride / size;
                planes.push(crate::channels::decompress_channel::<T>(
                    Compression::Raw,
                    data,
                    sample_bytes,
                    1,
                    self.version,
                    self.depth.as_raw(),
                )?);
            }
        }
        Ok(planes)
    }
}

fn invalid(message: &'static str) -> PsdError {
    PsdError::InvalidData { offset: 0, message }
}
