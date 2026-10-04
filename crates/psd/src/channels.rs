//! Planar channel storage and per-channel codec plumbing.
//!
//! Mirrors the channel maps of `ImageDataMixins.h` / `Core/Struct/ImageChannel.h`
//! but reshaped: one entry per channel keyed by [`ChannelKey`]. Entries hold
//! decoded samples by default or the original compressed payload during an
//! opt-in lazy read. Replacing a raw-backed key with pixels discards its payload.

use std::collections::BTreeMap;

use psd_codecs::endian::{be_bytes, decode_be_bytes};
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

/// One layer's channels, ordered by key. Entries hold decoded samples by
/// default or a compressed payload after an opt-in lazy read.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelStore<T> {
    channels: BTreeMap<ChannelKey, ChannelEntry<T>>,
}

#[derive(Debug, Clone, PartialEq)]
enum ChannelEntry<T> {
    Decoded(Vec<T>),
    Raw(RawChannelData),
}

/// Compressed bytes retained by an opt-in lazy read, following the raw-channel
/// design used by independent PSD parsers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawChannelData {
    pub(crate) compression: Compression,
    pub(crate) payload: Vec<u8>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) version: Version,
}

impl RawChannelData {
    pub(crate) fn new(
        compression: Compression,
        payload: Vec<u8>,
        width: usize,
        height: usize,
        version: Version,
    ) -> Self {
        Self {
            compression,
            payload,
            width,
            height,
            version,
        }
    }
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

    /// Decoded samples for `key`; returns `None` while the channel is raw.
    pub fn get(&self, key: ChannelKey) -> Option<&[T]> {
        match self.channels.get(&key)? {
            ChannelEntry::Decoded(samples) => Some(samples),
            ChannelEntry::Raw(_) => None,
        }
    }

    /// Mutable decoded samples; raw-backed channels must be decoded first.
    pub fn get_mut(&mut self, key: ChannelKey) -> Option<&mut Vec<T>> {
        match self.channels.get_mut(&key)? {
            ChannelEntry::Decoded(samples) => Some(samples),
            ChannelEntry::Raw(_) => None,
        }
    }

    /// Replace one channel with decoded pixels, discarding any retained raw
    /// payload for the same key.
    pub fn insert(&mut self, key: ChannelKey, data: Vec<T>) -> Option<Vec<T>> {
        match self.channels.insert(key, ChannelEntry::Decoded(data)) {
            Some(ChannelEntry::Decoded(previous)) => Some(previous),
            Some(ChannelEntry::Raw(_)) | None => None,
        }
    }

    /// Remove a channel. Returns its samples when decoded; removing a raw
    /// channel discards its payload and returns `None`.
    pub fn remove(&mut self, key: ChannelKey) -> Option<Vec<T>> {
        match self.channels.remove(&key) {
            Some(ChannelEntry::Decoded(samples)) => Some(samples),
            Some(ChannelEntry::Raw(_)) | None => None,
        }
    }

    /// Whether the channel is present, decoded or raw-backed.
    pub fn contains(&self, key: ChannelKey) -> bool {
        self.channels.contains_key(&key)
    }

    /// Whether `key` is present only as compressed data from a lazy read.
    pub fn is_raw(&self, key: ChannelKey) -> bool {
        matches!(self.channels.get(&key), Some(ChannelEntry::Raw(_)))
    }

    pub(crate) fn insert_raw(&mut self, key: ChannelKey, raw: RawChannelData) {
        self.channels.insert(key, ChannelEntry::Raw(raw));
    }

    pub(crate) fn raw(&self, key: ChannelKey) -> Option<&RawChannelData> {
        match self.channels.get(&key)? {
            ChannelEntry::Raw(raw) => Some(raw),
            ChannelEntry::Decoded(_) => None,
        }
    }

    pub(crate) fn raw_channels(&self) -> impl Iterator<Item = (ChannelKey, &RawChannelData)> {
        self.channels.iter().filter_map(|(key, entry)| match entry {
            ChannelEntry::Raw(raw) => Some((*key, raw)),
            ChannelEntry::Decoded(_) => None,
        })
    }

    pub fn keys(&self) -> impl Iterator<Item = ChannelKey> + '_ {
        self.channels.keys().copied()
    }

    /// Iterate decoded channels. Raw-backed keys are present in [`keys`](Self::keys)
    /// but are omitted until explicitly decoded.
    pub fn iter(&self) -> impl Iterator<Item = (ChannelKey, &[T])> + '_ {
        self.channels.iter().filter_map(|(key, entry)| match entry {
            ChannelEntry::Decoded(samples) => Some((*key, samples.as_slice())),
            ChannelEntry::Raw(_) => None,
        })
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (ChannelKey, &mut [T])> + '_ {
        self.channels
            .iter_mut()
            .filter_map(|(key, entry)| match entry {
                ChannelEntry::Decoded(samples) => Some((*key, samples.as_mut_slice())),
                ChannelEntry::Raw(_) => None,
            })
    }

    /// Number of present channels, including raw-backed channels.
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// Whether no decoded or raw-backed channels are present.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

/// Decode one retained raw channel. `Ok(None)` marks a real-user-mask (`-3`)
/// record that does not decode: older Photoshop files carry marker-only or
/// undersized `-3` records whose payload Photoshop ignores, and an eager read
/// keeps exactly those raw. A later lazy decode must accept the same document
/// rather than fail on it every time; the record round-trips unchanged.
fn decode_retained<T: BitDepth>(
    key: ChannelKey,
    raw: &RawChannelData,
    source_depth: u16,
) -> Result<Option<Vec<T>>> {
    match decompress_channel::<T>(
        raw.compression,
        &raw.payload,
        raw.width,
        raw.height,
        raw.version,
        source_depth,
    ) {
        Ok(samples) => Ok(Some(samples)),
        Err(error) if key == ChannelKey::REAL_USER_MASK => {
            tracing::debug!("real user mask did not decode ({error}); keeping its record raw");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

impl<T: BitDepth> ChannelStore<T> {
    /// Decode independent entries, releasing each successful payload immediately.
    /// Failed entries remain raw; the spent budget and first error are reported.
    /// A real-user-mask record that does not decode stays raw without an error
    /// (see [`decode_retained`]).
    pub(crate) fn decode_raw_in_place(&mut self, source_depth: u16) -> (usize, Option<PsdError>) {
        let mut spent = 0usize;
        let mut first_error = None;
        for (&key, entry) in self.channels.iter_mut() {
            let ChannelEntry::Raw(raw) = entry else {
                continue;
            };
            match decode_retained::<T>(key, raw, source_depth) {
                Ok(Some(samples)) => {
                    spent += samples.len() * T::SIZE;
                    *entry = ChannelEntry::Decoded(samples);
                }
                Ok(None) => {}
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        (spent, first_error)
    }

    /// Stage raw channel decodes without changing the store. Reserving the
    /// full layer budget first keeps a failed decode atomic; the reservation
    /// of a real-user-mask record that stays raw is returned afterwards.
    pub(crate) fn decode_raw(
        &self,
        source_depth: u16,
        remaining: &mut Option<usize>,
    ) -> Result<Vec<(ChannelKey, Vec<T>)>> {
        let mut jobs = Vec::new();
        for (key, raw) in self.raw_channels() {
            crate::layered_file::charge_decoded_bitmap(remaining, raw.width, raw.height, T::SIZE)?;
            let scratch = raw.width.saturating_mul(raw.height).saturating_mul(T::SIZE);
            jobs.push(((key, raw), scratch));
        }
        let mut decoded = Vec::with_capacity(jobs.len());
        let mut refund = 0usize;
        crate::parallel::for_each_ordered(
            jobs,
            |(key, raw)| Ok((key, raw, decode_retained::<T>(key, raw, source_depth)?)),
            |(key, raw, samples)| {
                match samples {
                    Some(samples) => decoded.push((key, samples)),
                    None => refund += raw.width * raw.height * T::SIZE,
                }
                Ok(())
            },
        )?;
        if let Some(remaining) = remaining {
            *remaining += refund;
        }
        Ok(decoded)
    }

    /// Additional decoded-channel memory needed by a depth conversion.
    pub(crate) fn conversion_footprint<U: BitDepth>(
        &self,
        target_bytes: &mut usize,
        raw_scratch_peak: &mut usize,
    ) -> Result<()> {
        for entry in self.channels.values() {
            let (samples, raw) = match entry {
                ChannelEntry::Decoded(samples) => (samples.len(), false),
                ChannelEntry::Raw(data) => (
                    data.width
                        .checked_mul(data.height)
                        .ok_or(PsdError::InvalidData {
                            offset: 0,
                            message: "channel dimensions overflow during bit-depth conversion",
                        })?,
                    true,
                ),
            };
            let output_bytes = samples.checked_mul(U::SIZE).ok_or(PsdError::InvalidData {
                offset: 0,
                message: "converted channel size overflows",
            })?;
            *target_bytes =
                target_bytes
                    .checked_add(output_bytes)
                    .ok_or(PsdError::InvalidData {
                        offset: 0,
                        message: "converted document channel size overflows",
                    })?;
            if raw {
                // Decompression temporarily holds both the decoded source
                // samples and the codec's byte buffer.
                let scratch = samples
                    .checked_mul(T::SIZE)
                    .and_then(|bytes| bytes.checked_mul(2))
                    .ok_or(PsdError::InvalidData {
                        offset: 0,
                        message: "channel decode workspace overflows",
                    })?;
                *raw_scratch_peak = (*raw_scratch_peak).max(scratch);
            }
        }
        Ok(())
    }

    /// Convert every decoded or lazy channel while keeping its key and order.
    pub(crate) fn convert_bit_depth<U: BitDepth>(
        &self,
        source_depth: u16,
    ) -> Result<ChannelStore<U>> {
        let mut converted = ChannelStore::new();
        for (&key, entry) in &self.channels {
            let samples = match entry {
                ChannelEntry::Decoded(samples) => samples.clone(),
                ChannelEntry::Raw(raw) => decompress_channel::<T>(
                    raw.compression,
                    &raw.payload,
                    raw.width,
                    raw.height,
                    raw.version,
                    source_depth,
                )?,
            };
            converted.insert(
                key,
                samples
                    .into_iter()
                    .map(crate::bitdepth::convert_sample::<T, U>)
                    .collect(),
            );
        }
        Ok(converted)
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
    source_depth: u16,
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
    if source_depth == 1 {
        return decode_one_bit_channel::<T>(compression, payload, width, height, version);
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
            return decode_be_bytes::<T>(payload).map_err(codec_error);
        }
        Compression::Rle => {
            let size_width = if version == Version::Psd { 2 } else { 4 };
            rle::decompress_scanlines(payload, width * T::SIZE, size_width, height)
                .map_err(codec_error)?
        }
        Compression::Zip => return T::inflate_be(payload, samples).map_err(codec_error),
        Compression::ZipPrediction => {
            return T::zip_prediction_decompress(payload, width, height).map_err(codec_error);
        }
    };
    T::from_be_vec(bytes).map_err(codec_error)
}

/// Decode a 1-bit (bitmap mode) channel: eight pixels per byte, MSB first,
/// with a row stride of `ceil(width / 8)`. A set bit is *black* — Photoshop
/// writes a bitmap document with the inked pixels set — so the expansion
/// yields `0` for a set bit and full scale for a clear one, one 8-bit sample
/// per pixel, and black padding is all-ones bytes (zero bytes would paint
/// white). The rows go through the same codecs as any other depth; ZIP
/// with prediction is not defined at 1 bit (writers downgrade it to ZIP), so
/// a stream marked that way is read as plain ZIP.
fn decode_one_bit_channel<T: BitDepth>(
    compression: Compression,
    payload: &[u8],
    width: usize,
    height: usize,
    version: Version,
) -> Result<Vec<T>> {
    let row_bytes = width.div_ceil(8).max(1);
    let packed_len = row_bytes * height;
    let packed = match compression {
        Compression::Raw => {
            // A body that ends mid-row cannot have its geometry recovered;
            // the remainder is dropped and the rest zero-filled, which
            // degrades the read rather than failing it.
            let mut bytes = payload[..payload.len().min(packed_len)].to_vec();
            if bytes.len() < packed_len {
                tracing::warn!(
                    "raw 1-bit channel is {} bytes, expected {packed_len}; filling the rest black",
                    payload.len()
                );
                bytes.resize(packed_len, 0xFF);
            }
            bytes
        }
        Compression::Rle => {
            let size_width = if version == Version::Psd { 2 } else { 4 };
            rle::decompress_scanlines(payload, row_bytes, size_width, height)
                .map_err(codec_error)?
        }
        Compression::Zip | Compression::ZipPrediction => {
            let inflated = zip::decompress(payload, packed_len).map_err(codec_error)?;
            let mut bytes = inflated;
            bytes.resize(packed_len, 0xFF);
            bytes
        }
    };

    let black = T::from_f32(0.0);
    let white = T::widen_eight(u8::MAX);
    let mut samples = Vec::with_capacity(width * height);
    for row in packed.chunks_exact(row_bytes) {
        for x in 0..width {
            let bit = (row[x / 8] >> (7 - (x % 8))) & 1;
            samples.push(if bit == 1 { black } else { white });
        }
    }
    Ok(samples)
}

/// Decode a one-channel segment from the merged-image section. The merged RLE
/// table has already been split into this channel's table and row stream.
pub(crate) fn decompress_merged_channel<T: BitDepth>(
    compression: Compression,
    payload: &[u8],
    width: usize,
    height: usize,
    version: Version,
    source_depth: u16,
) -> Result<Vec<T>> {
    if source_depth == 1 {
        decode_one_bit_channel(compression, payload, width, height, version)
    } else {
        decompress_channel(compression, payload, width, height, version, source_depth)
    }
}

/// Compress typed samples into `(codec, payload)` for one channel.
///
/// Codec choice mirrors Photoshop/upstream: 16/32-bit always
/// use ZIP prediction (32-bit never plain ZIP), and 8-bit uses RLE when its
/// exact packed size, counted over every scanline, beats the raw size and
/// every row fits its size-table entry; otherwise the plane is stored raw.
/// An explicit override wins (except that 32-bit ZIP is
/// upgraded to ZIP prediction, as Photoshop insists).
pub fn compress_channel<T: BitDepth>(
    data: &[T],
    width: usize,
    height: usize,
    version: Version,
    forced: Option<Compression>,
) -> Result<(Compression, Vec<u8>)> {
    let (codec, bytes) = encode_channel(
        data,
        width,
        height,
        version,
        forced,
        psd_codecs::zip::CompressionPolicy::Balanced,
        None,
    )?;
    Ok((codec, bytes.into_owned()))
}

/// Whether automatic selection stores a plane raw instead of as RLE: PackBits
/// is not smaller, or a row outgrows its PSD/PSB size-table entry (which only
/// raw storage can hold).
pub(crate) fn rle_loses(counts: &rle::ScanlineCounts, raw_len: usize) -> bool {
    !counts.fits_table() || counts.encoded_len() >= raw_len
}

/// Encode without copying native byte samples merely to write them out.
/// `rle_counts`, when given, are [`rle::count_scanlines`] of these samples'
/// big-endian bytes, measured earlier (to reserve output); they are reused
/// rather than counted again.
pub(crate) fn encode_channel<'a, T: BitDepth>(
    data: &'a [T],
    width: usize,
    height: usize,
    version: Version,
    forced: Option<Compression>,
    policy: psd_codecs::zip::CompressionPolicy,
    rle_counts: Option<rle::ScanlineCounts>,
) -> Result<(Compression, std::borrow::Cow<'a, [u8]>)> {
    // Validate extents first: a non-empty shape with an empty channel must be
    // a typed write-time error, not a corrupt file (Photoshop reads a bare Raw
    // marker for the expected `width * height` samples as corrupt, and the
    // port's own reader would zero-fill it — silently mutating data).
    if width.checked_mul(height) != Some(data.len()) {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "channel sample count does not match its extents",
        });
    }
    if data.is_empty() {
        return Ok((Compression::Raw, std::borrow::Cow::Borrowed(&[])));
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
        Compression::Raw => Ok((Compression::Raw, be_bytes(data))),
        Compression::Rle => {
            // 8-bit samples are their own bytes, so nothing is copied unless
            // the channel turns out not to compress and is stored raw.
            let raw = be_bytes(data);
            let size_width = if version == Version::Psd { 2 } else { 4 };
            // Every row is counted before anything is packed: the counts
            // decide Raw against RLE exactly (sampled rows cannot represent
            // heterogeneous planes) and size the one output buffer.
            let counts = match rle_counts {
                Some(counts) => counts,
                None => {
                    rle::count_scanlines(&raw, width * T::SIZE, size_width).map_err(codec_error)?
                }
            };
            if forced.is_none() && rle_loses(&counts, raw.len()) {
                return Ok((Compression::Raw, raw));
            }
            let packed = rle::compress_counted_scanlines(&raw, &counts).map_err(codec_error)?;
            Ok((Compression::Rle, packed.into()))
        }
        Compression::Zip => Ok((
            Compression::Zip,
            zip::compress_with_policy(&be_bytes(data), policy)
                .map_err(codec_error)?
                .into(),
        )),
        Compression::ZipPrediction => Ok((
            Compression::ZipPrediction,
            T::zip_prediction_compress_with_policy(data, width, height, policy)
                .map_err(codec_error)?
                .into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_rle_measures_heterogeneous_planes_instead_of_trusting_sample_rows() {
        let width = 256;
        let height = 4096;
        let mut data = vec![0u8; width * height];
        let mut state = 0x1234_5678u32;
        // Every row the old sampler inspected is noisy; the rest are flat.
        for index in 0..16 {
            let row = index * (height - 1) / 15;
            for byte in &mut data[row * width..(row + 1) * width] {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                *byte = (state >> 24) as u8;
            }
        }
        let (codec, bytes) = compress_channel(&data, width, height, Version::Psd, None).unwrap();
        assert_eq!(codec, Compression::Rle);
        assert!(bytes.len() < data.len() / 10);
        assert_eq!(
            decompress_channel::<u8>(codec, &bytes, width, height, Version::Psd, 8).unwrap(),
            data
        );
    }

    #[test]
    fn a_real_user_mask_that_stays_raw_returns_its_budget_reservation() {
        let mut store = ChannelStore::<u8>::new();
        let raw =
            |payload: Vec<u8>| RawChannelData::new(Compression::Raw, payload, 2, 2, Version::Psd);
        store.insert_raw(ChannelKey::ALPHA, raw(vec![1, 2, 3, 4]));
        store.insert_raw(ChannelKey::REAL_USER_MASK, raw(vec![9]));
        // Both planes are reserved up front; the undecodable `-3` one is refunded.
        let mut remaining = Some(8);
        let decoded = store.decode_raw(8, &mut remaining).unwrap();
        assert_eq!(decoded, [(ChannelKey::ALPHA, vec![1, 2, 3, 4])]);
        assert_eq!(remaining, Some(4));
        // Bulk decoding charges only what it decoded and reports no error.
        let (spent, error) = store.decode_raw_in_place(8);
        assert_eq!((spent, error.is_none()), (4, true));
        assert!(store.is_raw(ChannelKey::REAL_USER_MASK));
        // Any other channel that does not decode is still an error.
        store.insert_raw(ChannelKey::color(0), raw(vec![9]));
        assert!(store.decode_raw(8, &mut None).is_err());
        assert!(store.decode_raw_in_place(8).1.is_some());
    }

    #[test]
    fn automatic_rle_stores_raw_when_a_row_outgrows_the_psd_size_table() {
        // A 70,000-pixel row packs smaller than raw yet longer than a 2-byte
        // PSD size entry can declare: only raw storage can hold it.
        let width = 70_000;
        let mut state = 0x1234_5678u32;
        let mut data: Vec<u8> = (0..66_000)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        data.resize(width, 0);
        let (codec, bytes) = compress_channel(&data, width, 1, Version::Psd, None).unwrap();
        assert_eq!(codec, Compression::Raw);
        assert_eq!(bytes, data);
        // PSB's 4-byte entries hold it, and an explicit RLE request on PSD
        // still reports the overflow.
        let (codec, _) = compress_channel(&data, width, 1, Version::Psb, None).unwrap();
        assert_eq!(codec, Compression::Rle);
        assert!(compress_channel(&data, width, 1, Version::Psd, Some(Compression::Rle)).is_err());
    }

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
    fn replacing_a_raw_channel_makes_decoded_samples_authoritative() {
        let mut store = ChannelStore::<u8>::new();
        store.insert_raw(
            ChannelKey::color(0),
            RawChannelData::new(Compression::Raw, vec![1, 2], 2, 1, Version::Psd),
        );
        assert!(store.contains(ChannelKey::color(0)));
        assert!(store.is_raw(ChannelKey::color(0)));
        assert_eq!(store.get(ChannelKey::color(0)), None);

        assert_eq!(store.insert(ChannelKey::color(0), vec![7, 8]), None);
        assert!(!store.is_raw(ChannelKey::color(0)));
        assert_eq!(store.get(ChannelKey::color(0)), Some([7u8, 8].as_slice()));
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
                let back = decompress_channel::<u8>(used, &payload, 8, 8, version, 8).unwrap();
                assert_eq!(back, data8, "u8 {codec:?} {version:?}");

                let (used, payload) =
                    compress_channel(&data16, 8, 8, version, Some(codec)).unwrap();
                let back = decompress_channel::<u16>(used, &payload, 8, 8, version, 16).unwrap();
                assert_eq!(back, data16, "u16 {codec:?} {version:?}");

                let (used, payload) =
                    compress_channel(&data32, 8, 8, version, Some(codec)).unwrap();
                let back = decompress_channel::<f32>(used, &payload, 8, 8, version, 32).unwrap();
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
            decompress_channel::<u8>(codec, &payload, 0, 0, Version::Psd, 8)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn one_bit_channels_expand_msb_first_with_black_set() {
        // 10 pixels wide: two packed bytes per row, MSB first, and a set bit
        // is black. This row alternates black and white.
        let payload = [0b1010_1010, 0b1000_0000];
        let samples =
            decompress_channel::<u8>(Compression::Raw, &payload, 10, 1, Version::Psd, 1).unwrap();
        assert_eq!(
            samples,
            vec![0, 255, 0, 255, 0, 255, 0, 255, 0, 255],
            "set bits are black, clear bits are white"
        );

        // A body that ends mid-row degrades the read: the missing pixels are
        // filled black (all-ones), not white.
        let samples =
            decompress_channel::<u8>(Compression::Raw, &[0b1010_1010], 10, 1, Version::Psd, 1)
                .unwrap();
        assert_eq!(samples, vec![0, 255, 0, 255, 0, 255, 0, 255, 0, 0]);

        // A 16-pixel row packs into two bytes, one sample per pixel.
        let samples = decompress_channel::<u8>(
            Compression::Raw,
            &[0b0101_0101, 0b0101_0101],
            16,
            1,
            Version::Psd,
            1,
        )
        .unwrap();
        assert_eq!(
            samples,
            vec![255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0]
        );
    }

    #[test]
    fn a_real_bitmap_mode_stream_decodes_to_its_inked_pixels() {
        // Four rows of a 4x4 bitmap-mode image as Photoshop wrote them
        // (raw 1-bit channel data). The inked pixels are the set bits:
        //   B B W W
        //   B B B B
        //   W B B B
        //   W W B B
        let packed = [0xC0, 0xF0, 0x70, 0x30];
        let samples =
            decompress_channel::<u8>(Compression::Raw, &packed, 4, 4, Version::Psd, 1).unwrap();
        let rendered: Vec<&str> = samples
            .chunks_exact(4)
            .map(|row| {
                if row.iter().all(|&sample| sample == 0) {
                    "BBBB"
                } else if row == [0, 0, 255, 255] {
                    "BBWW"
                } else if row == [255, 0, 0, 0] {
                    "WBBB"
                } else if row == [255, 255, 0, 0] {
                    "WWBB"
                } else {
                    "????"
                }
            })
            .collect();
        assert_eq!(rendered, ["BBWW", "BBBB", "WBBB", "WWBB"]);
    }

    #[test]
    fn one_bit_rle_channels_decode_row_by_row() {
        // Two rows of 10 pixels: 2 packed bytes each. RLE framing is the same
        // as every other depth: a u16 length per row, then the row bodies
        // (a literal packet of two bytes).
        let mut payload = Vec::new();
        payload.extend_from_slice(&3u16.to_be_bytes());
        payload.extend_from_slice(&3u16.to_be_bytes());
        payload.extend_from_slice(&[1, 0b1010_1010, 0b1000_0000]);
        payload.extend_from_slice(&[1, 0b0101_0101, 0b0000_0000]);
        let samples =
            decompress_channel::<u8>(Compression::Rle, &payload, 10, 2, Version::Psd, 1).unwrap();
        assert_eq!(&samples[..10], &[0, 255, 0, 255, 0, 255, 0, 255, 0, 255]);
        // The second row's last two bits are clear, so those pixels are white.
        assert_eq!(&samples[10..], &[255, 0, 255, 0, 255, 0, 255, 0, 255, 255]);
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
