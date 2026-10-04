//! Planned channel encoding, independent of metadata and sink ownership.

use std::borrow::Cow;
use std::io::Write;

use psd_core::{ChannelData, Compression, PsdError, Result, Version};

use crate::{BitDepth, CompressionPolicy};

pub(crate) enum ChannelSource<'a, T> {
    Stored(Compression, &'a [u8]),
    Samples(&'a [T]),
    Uniform(T),
}

pub(crate) struct ChannelJob<'a, T> {
    pub source: ChannelSource<'a, T>,
    pub width: usize,
    pub height: usize,
    pub version: Version,
    pub forced: Option<Compression>,
}

pub(crate) struct EncodedChannel<'a> {
    codec: Compression,
    payload: Payload<'a>,
}

enum Payload<'a> {
    Bytes(Cow<'a, [u8]>),
    Repeat {
        pattern: Vec<u8>,
        count: usize,
    },
    RleRepeat {
        row: Vec<u8>,
        rows: usize,
        size_width: usize,
    },
}

impl<T: BitDepth> ChannelJob<'_, T> {
    /// Exact byte-plane reservation also fixes the automatic codec choice, so
    /// the subsequent encode does not repeat the counting pass.
    pub(crate) fn prepare_byte_hint(&mut self) -> Result<Option<usize>> {
        let samples = self
            .width
            .checked_mul(self.height)
            .ok_or(invalid("channel dimensions overflow"))?;
        match &self.source {
            ChannelSource::Stored(_, bytes) => Ok(Some(bytes.len())),
            ChannelSource::Samples(data) if T::DEPTH == 8 => {
                let bytes = psd_codecs::endian::be_bytes(data);
                if samples == 0 {
                    return Ok(Some(0));
                }
                match self.forced {
                    Some(Compression::Raw) => Ok(Some(bytes.len())),
                    None | Some(Compression::Rle) => {
                        let width = if self.version == Version::Psd { 2 } else { 4 };
                        let rle = psd_codecs::rle::encoded_scanlines_len(&bytes, self.width, width)
                            .map_err(|error| PsdError::Compression(error.to_string()))?;
                        if self.forced.is_none() {
                            self.forced = Some(if rle < bytes.len() {
                                Compression::Rle
                            } else {
                                Compression::Raw
                            });
                        }
                        Ok(Some(if self.forced == Some(Compression::Raw) {
                            bytes.len()
                        } else {
                            rle
                        }))
                    }
                    _ => Ok(None),
                }
            }
            ChannelSource::Uniform(_) if T::DEPTH == 8 => {
                let raw = samples;
                let table = if self.version == Version::Psd { 2 } else { 4 };
                let rle = self
                    .width
                    .div_ceil(128)
                    .checked_mul(2)
                    .and_then(|row| row.checked_add(table))
                    .and_then(|row| row.checked_mul(self.height))
                    .ok_or(invalid("RLE length overflows"))?;
                Ok(match self.forced {
                    Some(Compression::Raw) => Some(raw),
                    Some(Compression::Rle) => Some(rle),
                    None => Some(raw.min(rle)),
                    _ => None,
                })
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn work_bytes(&self) -> usize {
        match self.source {
            ChannelSource::Stored(_, _) => 0,
            ChannelSource::Samples(data) => data.len().saturating_mul(T::SIZE).saturating_mul(2),
            ChannelSource::Uniform(_) => self
                .width
                .saturating_mul(T::SIZE)
                .saturating_mul(3)
                .saturating_add(
                    self.width
                        .saturating_mul(self.height)
                        .saturating_mul(T::SIZE)
                        / 128,
                )
                .saturating_add(256 * 1024),
        }
    }
}

impl<'a, T: BitDepth> ChannelJob<'a, T> {
    pub(crate) fn encode(self, policy: CompressionPolicy) -> Result<EncodedChannel<'a>> {
        let (codec, payload) = match self.source {
            ChannelSource::Stored(codec, bytes) => (codec, Payload::Bytes(Cow::Borrowed(bytes))),
            ChannelSource::Samples(data) => {
                let (codec, bytes) = crate::channels::encode_channel(
                    data,
                    self.width,
                    self.height,
                    self.version,
                    self.forced,
                    policy,
                )?;
                (codec, Payload::Bytes(bytes))
            }
            ChannelSource::Uniform(value) => {
                let samples = self
                    .width
                    .checked_mul(self.height)
                    .ok_or(invalid("uniform channel dimensions overflow"))?;
                if samples == 0 {
                    (Compression::Raw, Payload::Bytes(Cow::Borrowed(&[])))
                } else {
                    let mut codec = self.forced.unwrap_or(if T::DEPTH >= 16 {
                        Compression::ZipPrediction
                    } else {
                        Compression::Rle
                    });
                    if T::DEPTH == 32 && codec == Compression::Zip {
                        codec = Compression::ZipPrediction;
                    }
                    let pattern = psd_codecs::endian::encode_be_bytes(&[value]);
                    match codec {
                        Compression::Raw => (
                            codec,
                            Payload::Repeat {
                                pattern,
                                count: samples,
                            },
                        ),
                        Compression::Rle => {
                            let mut row = Vec::new();
                            repeat_into(&mut row, &pattern, self.width)?;
                            let row = psd_codecs::rle::pack_bits_compress(&row);
                            let size_width = if self.version == Version::Psd { 2 } else { 4 };
                            if size_width == 2 && row.len() > u16::MAX as usize {
                                return Err(PsdError::LengthOverflow {
                                    actual: row.len() as u64,
                                    width: 2,
                                });
                            }
                            let encoded = Payload::RleRepeat {
                                row,
                                rows: self.height,
                                size_width,
                            };
                            let raw_len = samples
                                .checked_mul(T::SIZE)
                                .ok_or(invalid("uniform channel byte count overflows"))?;
                            if self.forced.is_none() && encoded.len()? >= raw_len {
                                (
                                    Compression::Raw,
                                    Payload::Repeat {
                                        pattern,
                                        count: samples,
                                    },
                                )
                            } else {
                                (codec, encoded)
                            }
                        }
                        Compression::Zip => {
                            let bytes = psd_codecs::zip::compress_repeated(&pattern, samples)
                                .map_err(|error| PsdError::Compression(error.to_string()))?;
                            (codec, Payload::Bytes(Cow::Owned(bytes)))
                        }
                        Compression::ZipPrediction => {
                            let bytes =
                                T::zip_prediction_compress_constant(value, self.width, self.height)
                                    .map_err(|error| PsdError::Compression(error.to_string()))?;
                            (codec, Payload::Bytes(Cow::Owned(bytes)))
                        }
                    }
                }
            }
        };
        Ok(EncodedChannel { codec, payload })
    }
}

impl EncodedChannel<'_> {
    pub(crate) fn disk_len(&self) -> Result<u64> {
        Ok(self
            .payload
            .len()?
            .checked_add(2)
            .ok_or(invalid("channel disk length overflows"))? as u64)
    }
    pub(crate) fn write_to(&self, sink: &mut dyn Write) -> Result<()> {
        sink.write_all(&self.codec.as_raw().to_be_bytes())?;
        self.payload.write_to(sink)
    }
}

impl<'a> EncodedChannel<'a> {
    pub(crate) fn into_data(self) -> Result<ChannelData<'a>> {
        let payload = match self.payload {
            Payload::Bytes(bytes) => bytes,
            repeated => {
                let mut bytes = Vec::with_capacity(repeated.len()?);
                repeated.write_to(&mut bytes)?;
                Cow::Owned(bytes)
            }
        };
        Ok(ChannelData {
            compression: self.codec,
            data: payload,
        })
    }
}

impl Payload<'_> {
    fn len(&self) -> Result<usize> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.len()),
            Self::Repeat { pattern, count } => pattern
                .len()
                .checked_mul(*count)
                .ok_or(invalid("repeated channel length overflows")),
            Self::RleRepeat {
                row,
                rows,
                size_width,
            } => row
                .len()
                .checked_add(*size_width)
                .and_then(|length| length.checked_mul(*rows))
                .ok_or(invalid("RLE channel length overflows")),
        }
    }
    fn write_to(&self, sink: &mut dyn Write) -> Result<()> {
        match self {
            Self::Bytes(bytes) => {
                sink.write_all(bytes)?;
                Ok(())
            }
            Self::Repeat { pattern, count } => repeat_into(sink, pattern, *count),
            Self::RleRepeat {
                row,
                rows,
                size_width,
            } => {
                let length = row.len() as u32;
                let size = if *size_width == 2 {
                    (length as u16).to_be_bytes().to_vec()
                } else {
                    length.to_be_bytes().to_vec()
                };
                repeat_into(sink, &size, *rows)?;
                repeat_into(sink, row, *rows)
            }
        }
    }
}

fn repeat_into(sink: &mut dyn Write, pattern: &[u8], mut count: usize) -> Result<()> {
    if pattern.is_empty() || count == 0 {
        return Ok(());
    }
    let copies = (32 * 1024 / pattern.len()).max(1).min(count);
    let mut block = Vec::with_capacity(pattern.len() * copies);
    for _ in 0..copies {
        block.extend_from_slice(pattern);
    }
    while count >= copies {
        sink.write_all(&block)?;
        count -= copies;
    }
    sink.write_all(&block[..count * pattern.len()])?;
    Ok(())
}

fn invalid(message: &'static str) -> PsdError {
    PsdError::InvalidData { offset: 0, message }
}
