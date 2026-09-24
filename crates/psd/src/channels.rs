//! Planar channel storage and per-channel codec plumbing.
//!
//! Mirrors the channel maps of `ImageDataMixins.h` / `Core/Struct/ImageChannel.h`
//! but reshaped: one `Vec<T>` per channel keyed by
//! [`ChannelKey`], decompressed in memory. Compression is a write-time property,
//! not part of the in-memory state.

use std::collections::BTreeMap;

use psd_codecs::endian::{decode_be_bytes, encode_be_bytes};
use psd_codecs::rle;
use psd_codecs::zip;
use psd_core::{Compression, PsdError, Result, Version};

use crate::bitdepth::BitDepth;

/// Key of a layer channel. The raw `i16` index is authoritative on disk:
/// `-1` alpha, `-2` user (pixel) mask, `-3` real user mask, `0..n` color
/// channels in color-mode order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelKey(pub i16);

impl ChannelKey {
    pub const ALPHA: Self = Self(-1);
    pub const USER_MASK: Self = Self(-2);
    pub const REAL_USER_MASK: Self = Self(-3);

    /// A color channel by its mode-relative index.
    pub const fn color(index: u8) -> Self {
        Self(index as i16)
    }

    pub const fn index(self) -> i16 {
        self.0
    }

    /// Whether this channel holds mask pixels (uses the mask bounds on disk).
    pub const fn is_mask(self) -> bool {
        self.0 == -2 || self.0 == -3
    }
}

/// The planar channels of one layer, ordered by key.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelStore<T> {
    channels: BTreeMap<ChannelKey, Vec<T>>,
}

impl<T> Default for ChannelStore<T> {
    fn default() -> Self {
        Self {
            channels: BTreeMap::new(),
        }
    }
}

impl<T> ChannelStore<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: ChannelKey) -> Option<&[T]> {
        self.channels.get(&key).map(Vec::as_slice)
    }

    pub fn get_mut(&mut self, key: ChannelKey) -> Option<&mut Vec<T>> {
        self.channels.get_mut(&key)
    }

    pub fn insert(&mut self, key: ChannelKey, data: Vec<T>) -> Option<Vec<T>> {
        self.channels.insert(key, data)
    }

    pub fn remove(&mut self, key: ChannelKey) -> Option<Vec<T>> {
        self.channels.remove(&key)
    }

    pub fn contains(&self, key: ChannelKey) -> bool {
        self.channels.contains_key(&key)
    }

    pub fn keys(&self) -> impl Iterator<Item = ChannelKey> + '_ {
        self.channels.keys().copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ChannelKey, &[T])> + '_ {
        self.channels
            .iter()
            .map(|(key, data)| (*key, data.as_slice()))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (ChannelKey, &mut [T])> + '_ {
        self.channels
            .iter_mut()
            .map(|(key, data)| (*key, data.as_mut_slice()))
    }

    pub fn len(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

/// Map a codec failure into the workspace error type.
fn codec_error(error: psd_codecs::CodecError) -> PsdError {
    PsdError::Compression(error.to_string())
}

/// Decompress one channel payload into typed samples.
///
/// `width`/`height` are the channel's own extents (the mask's for mask
/// channels); `payload` excludes the 2-byte compression marker.
pub fn decompress_channel<T: BitDepth>(
    compression: Compression,
    payload: &[u8],
    width: usize,
    height: usize,
    version: Version,
) -> Result<Vec<T>> {
    let samples = width * height;
    if samples == 0 {
        return Ok(Vec::new());
    }
    if payload.is_empty() {
        // Photoshop leaves channels without data as a bare compression marker
        // (e.g. the mask channel of a vector-mask layer); upstream decodes
        // those into a zero-filled buffer.
        tracing::warn!("empty channel payload for a {width}x{height} channel, filling with zeros");
        return Ok(vec![T::ZERO; samples]);
    }
    let bytes = match compression {
        Compression::Raw => {
            if payload.len() != samples * T::SIZE {
                return Err(PsdError::Compression(format!(
                    "raw channel is {} bytes, expected {}",
                    payload.len(),
                    samples * T::SIZE
                )));
            }
            payload.to_vec()
        }
        Compression::Rle => {
            let size_width = if version == Version::Psd { 2 } else { 4 };
            rle::decompress_scanlines(payload, width * T::SIZE, size_width, height)
                .map_err(codec_error)?
        }
        Compression::Zip => zip::decompress(payload, samples * T::SIZE).map_err(codec_error)?,
        Compression::ZipPrediction => {
            let inflated = zip::decompress(payload, samples * T::SIZE).map_err(codec_error)?;
            return T::zip_prediction_decode(&inflated, width, height).map_err(codec_error);
        }
    };
    decode_be_bytes::<T>(&bytes).map_err(codec_error)
}

/// Compress typed samples into `(codec, payload)` for one channel.
///
/// Codec choice mirrors Photoshop/upstream: 16/32-bit always
/// use ZIP prediction (32-bit never plain ZIP), 8-bit uses RLE when it beats
/// the raw size, and an explicit override wins (except that 32-bit ZIP is
/// upgraded to ZIP prediction, as Photoshop insists).
pub fn compress_channel<T: BitDepth>(
    data: &[T],
    width: usize,
    height: usize,
    version: Version,
    forced: Option<Compression>,
) -> Result<(Compression, Vec<u8>)> {
    // Validate extents first: a non-empty shape with an empty channel must be
    // a typed write-time error, not a corrupt file (Photoshop reads a bare Raw
    // marker for the expected `width * height` samples as corrupt, and the
    // port's own reader would zero-fill it — silently mutating data).
    if width * height != data.len() {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "channel sample count does not match its extents",
        });
    }
    if data.is_empty() {
        return Ok((Compression::Raw, Vec::new()));
    }

    let mut codec = match forced {
        Some(codec) => codec,
        None if T::DEPTH >= 16 => Compression::ZipPrediction,
        None => Compression::Rle, // re-evaluated against Raw below
    };
    if T::DEPTH == 32 && codec == Compression::Zip {
        codec = Compression::ZipPrediction;
    }

    match codec {
        Compression::Raw => Ok((Compression::Raw, encode_be_bytes(data))),
        Compression::Rle => {
            let raw = encode_be_bytes(data);
            let size_width = if version == Version::Psd { 2 } else { 4 };
            let packed =
                rle::compress_scanlines(&raw, width * T::SIZE, size_width).map_err(codec_error)?;
            if forced.is_none() && packed.len() >= raw.len() {
                Ok((Compression::Raw, raw))
            } else {
                Ok((Compression::Rle, packed))
            }
        }
        Compression::Zip => Ok((
            Compression::Zip,
            zip::compress(&encode_be_bytes(data)).map_err(codec_error)?,
        )),
        Compression::ZipPrediction => {
            let encoded = T::zip_prediction_encode(data, width, height).map_err(codec_error)?;
            Ok((
                Compression::ZipPrediction,
                zip::compress(&encoded).map_err(codec_error)?,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_keys_map_to_raw_indices() {
        assert_eq!(ChannelKey::ALPHA.index(), -1);
        assert_eq!(ChannelKey::USER_MASK.index(), -2);
        assert_eq!(ChannelKey::color(2).index(), 2);
        assert!(ChannelKey::USER_MASK.is_mask());
        assert!(!ChannelKey::ALPHA.is_mask());
    }

    #[test]
    fn store_keeps_channels_ordered_by_key() {
        let mut store = ChannelStore::<u8>::new();
        store.insert(ChannelKey::color(0), vec![1]);
        store.insert(ChannelKey::ALPHA, vec![2]);
        store.insert(ChannelKey::USER_MASK, vec![3]);
        let keys: Vec<i16> = store.keys().map(ChannelKey::index).collect();
        assert_eq!(keys, vec![-2, -1, 0]);
        assert_eq!(store.get(ChannelKey::ALPHA), Some([2u8].as_slice()));
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn all_codecs_round_trip_per_depth() {
        let data8: Vec<u8> = (0..64u32).map(|i| (i * 5) as u8).collect();
        let data16: Vec<u16> = (0..64u32).map(|i| (i * 1000) as u16).collect();
        let data32: Vec<f32> = (0..64).map(|i| i as f32 * 0.5).collect();

        for version in [Version::Psd, Version::Psb] {
            for codec in [
                Compression::Raw,
                Compression::Rle,
                Compression::Zip,
                Compression::ZipPrediction,
            ] {
                let (used, payload) = compress_channel(&data8, 8, 8, version, Some(codec)).unwrap();
                assert_eq!(used, codec);
                let back = decompress_channel::<u8>(used, &payload, 8, 8, version).unwrap();
                assert_eq!(back, data8, "u8 {codec:?} {version:?}");

                let (used, payload) =
                    compress_channel(&data16, 8, 8, version, Some(codec)).unwrap();
                let back = decompress_channel::<u16>(used, &payload, 8, 8, version).unwrap();
                assert_eq!(back, data16, "u16 {codec:?} {version:?}");

                let (used, payload) =
                    compress_channel(&data32, 8, 8, version, Some(codec)).unwrap();
                let back = decompress_channel::<f32>(used, &payload, 8, 8, version).unwrap();
                assert_eq!(back, data32, "f32 {codec:?} {version:?}");
            }
        }
    }

    #[test]
    fn automatic_codec_choice_follows_depth() {
        let data8: Vec<u8> = vec![0; 4096]; // RLE-friendly
        let (codec, _) = compress_channel(&data8, 64, 64, Version::Psd, None).unwrap();
        assert_eq!(codec, Compression::Rle);

        let noise: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761)) as u8)
            .collect();
        let (codec, _) = compress_channel(&noise, 64, 64, Version::Psd, None).unwrap();
        assert_eq!(codec, Compression::Raw);

        let data16: Vec<u16> = vec![7; 4096];
        let (codec, _) = compress_channel(&data16, 64, 64, Version::Psd, None).unwrap();
        assert_eq!(codec, Compression::ZipPrediction);

        let data32: Vec<f32> = vec![0.5; 4096];
        let (codec, _) =
            compress_channel(&data32, 64, 64, Version::Psd, Some(Compression::Zip)).unwrap();
        assert_eq!(codec, Compression::ZipPrediction); // Photoshop insists
    }

    #[test]
    fn empty_channels_are_raw_and_empty() {
        let (codec, payload) = compress_channel::<u8>(&[], 0, 0, Version::Psd, None).unwrap();
        assert_eq!(codec, Compression::Raw);
        assert!(payload.is_empty());
        assert!(
            decompress_channel::<u8>(codec, &payload, 0, 0, Version::Psd)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn empty_data_with_nonempty_extents_is_a_typed_error() {
        // 8×8 extents with zero samples used to slip through the empty
        // shortcut and serialize a bare Raw marker — a file Photoshop reads
        // as corrupt and the port's own reader zero-fills silently.
        assert!(matches!(
            compress_channel::<u8>(&[], 8, 8, Version::Psd, None),
            Err(PsdError::InvalidData { .. })
        ));
    }
}
