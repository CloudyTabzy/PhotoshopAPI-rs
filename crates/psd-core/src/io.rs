//! Big-endian primitive IO over byte slices and growable buffers.
//!
//! Replaces upstream `Core/FileIO/Read.h`, `Write.h`, `BytesIO.h`, and the
//! length-marker machinery of `LengthMarkers.h`. Upstream writes directly into
//! a seekable file and backpatches markers (`ScopedLengthBlock`); here sections
//! are built in a `Vec<u8>` and markers patched in place before the section is
//! appended — the same guarantee without requiring a seekable sink.

use crate::enums::Version;
use crate::error::{PsdError, Result};

/// Round `value` up to a multiple of `multiple` (upstream `RoundUpToMultiple`).
///
/// Used for the 2-byte alignment of image-resource blocks and Pascal strings.
pub(crate) fn round_up(value: usize, multiple: usize) -> usize {
    debug_assert!(multiple >= 1);
    value.div_ceil(multiple) * multiple
}

/// Cursor-style big-endian reader over a byte slice with absolute offsets in errors.
///
/// One reader serves both mmap-backed and memory inputs (upstream's `File` vs
/// `ByteStream` duality collapses into this type.
#[derive(Debug, Clone)]
pub struct BeReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BeReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Absolute offset of the cursor.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Total length of the underlying slice.
    pub fn total_len(&self) -> usize {
        self.data.len()
    }

    /// Reposition the cursor absolutely (used for section jumps after headers).
    pub fn seek(&mut self, offset: usize) -> Result<()> {
        if offset > self.data.len() {
            return Err(PsdError::UnexpectedEof {
                offset: offset as u64,
            });
        }
        self.pos = offset;
        Ok(())
    }

    /// Skip `n` bytes.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        let target = self
            .pos
            .checked_add(n)
            .ok_or(PsdError::UnexpectedEof { offset: u64::MAX })?;
        self.seek(target)
    }

    /// Consume `n` bytes, returning them as a sub-slice.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(PsdError::UnexpectedEof {
                offset: self.pos as u64,
            });
        }
        let start = self.pos;
        self.pos += n;
        Ok(&self.data[start..self.pos])
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn i8(&mut self) -> Result<i8> {
        Ok(self.take(1)?[0] as i8)
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// Version-dependent length field: 4 bytes for PSD, 8 for PSB.
    ///
    /// Replaces upstream's `ReadBinaryDataVariadic<uint32_t, uint64_t>` +
    /// `SwapPsdPsb` + `ExtractWidestValue` with a single helper.
    /// Used for the layer-and-mask-info / layer-info / `Lr16`/`Lr32` markers.
    pub fn len(&mut self, version: Version) -> Result<u64> {
        match version {
            Version::Psd => Ok(u64::from(self.u32()?)),
            Version::Psb => Ok(self.u64()?),
        }
    }

    /// Read a hard-validated 4-byte signature (`8BPS`, `8BIM`, `8B64`).
    pub fn signature(&mut self, expected: &'static str) -> Result<()> {
        let offset = self.pos as u64;
        let bytes = self.take(4)?;
        if bytes != expected.as_bytes() {
            let mut found = [0u8; 4];
            found.copy_from_slice(bytes);
            return Err(PsdError::InvalidSignature {
                expected,
                found,
                offset,
            });
        }
        Ok(())
    }
}

/// Growable big-endian buffer with length-marker patch support.
///
/// Sections are serialized into a writer; when a section needs a length prefix,
/// reserve the marker, write the payload, then [`patch_len`](Self::patch_len).
/// The finished section is then appended to the parent writer with
/// [`bytes`](Self::bytes) — no seeking, no I/O.
#[derive(Debug, Default)]
pub struct BeWriter {
    buf: Vec<u8>,
}

impl BeWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Current write position (bytes written so far).
    pub fn position(&self) -> usize {
        self.buf.len()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub fn bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub fn i8(&mut self, value: i8) {
        self.buf.push(value as u8);
    }

    pub fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn i16(&mut self, value: i16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn i32(&mut self, value: i32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn f32(&mut self, value: f32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn f64(&mut self, value: f64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Pad with zero bytes until the position is a multiple of `multiple`.
    pub fn pad_to(&mut self, multiple: usize) {
        debug_assert!(multiple > 0);
        let rem = self.buf.len() % multiple;
        if rem != 0 {
            self.buf.resize(self.buf.len() + (multiple - rem), 0);
        }
    }

    /// Pad with zero bytes until `position() - start` is a multiple of
    /// `multiple` (upstream `ScopedLengthBlock` padding is relative to the
    /// section, not to the file).
    pub fn pad_to_relative(&mut self, start: usize, multiple: usize) {
        debug_assert!(multiple > 0);
        debug_assert!(start <= self.buf.len());
        let len = self.buf.len() - start;
        let rem = len % multiple;
        if rem != 0 {
            self.buf.resize(self.buf.len() + (multiple - rem), 0);
        }
    }

    /// Version-dependent length field width in bytes (4 for PSD, 8 for PSB).
    pub const fn len_width(version: Version) -> usize {
        match version {
            Version::Psd => 4,
            Version::Psb => 8,
        }
    }

    /// Write a version-dependent length value, rejecting narrowing.
    pub fn len(&mut self, version: Version, value: u64) -> Result<()> {
        match version {
            Version::Psd => {
                let narrowed = u32::try_from(value).map_err(|_| PsdError::LengthOverflow {
                    actual: value,
                    width: 4,
                })?;
                self.u32(narrowed);
            }
            Version::Psb => self.u64(value),
        }
        Ok(())
    }

    /// Reserve a zeroed length marker, returning its byte offset for
    /// a later [`patch_len`](Self::patch_len).
    pub fn reserve_len_marker(&mut self, version: Version) -> usize {
        let offset = self.buf.len();
        self.buf.resize(offset + Self::len_width(version), 0);
        offset
    }

    /// Reserve a zeroed 8-byte length marker (for sections whose length is
    /// always a `u64`, e.g. linked-layer records).
    pub fn reserve_len_marker64(&mut self) -> usize {
        let offset = self.buf.len();
        self.buf.resize(offset + 8, 0);
        offset
    }

    /// Patch an 8-byte marker reserved with
    /// [`reserve_len_marker64`](Self::reserve_len_marker64).
    pub fn patch_len64(&mut self, marker_offset: usize, value: u64) {
        debug_assert!(marker_offset + 8 <= self.buf.len());
        self.buf[marker_offset..marker_offset + 8].copy_from_slice(&value.to_be_bytes());
    }

    /// Patch a marker reserved with [`reserve_len_marker`](Self::reserve_len_marker).
    ///
    /// The written value counts the bytes between the end of the marker and
    /// `end` (usually `self.position()`); `include_marker` additionally counts
    /// the marker itself (needed by the tagged-block convention where the
    /// length includes the marker). Callers pass `end`
    /// explicitly so nested sections patch outer markers after inner content.
    pub fn patch_len(
        &mut self,
        version: Version,
        marker_offset: usize,
        end: usize,
        include_marker: bool,
    ) -> Result<()> {
        let width = Self::len_width(version);
        let marker_end = marker_offset + width;
        if end < marker_end {
            return Err(PsdError::InvalidData {
                offset: end as u64,
                message: "length marker end precedes marker",
            });
        }
        let mut value = (end - marker_end) as u64;
        if include_marker {
            value += width as u64;
        }
        let bytes = match version {
            Version::Psd => u32::try_from(value)
                .map_err(|_| PsdError::LengthOverflow {
                    actual: value,
                    width: 4,
                })?
                .to_be_bytes()
                .to_vec(),
            Version::Psb => value.to_be_bytes().to_vec(),
        };
        self.buf[marker_offset..marker_end].copy_from_slice(&bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip_big_endian() {
        let mut w = BeWriter::new();
        w.u8(0xAB);
        w.u16(0x1234);
        w.i16(-2);
        w.u32(0xDEAD_BEEF);
        w.i32(-70000);
        w.u64(0x0102_0304_0506_0708);
        w.f32(1.0);
        w.f64(-2.5);
        let bytes = w.into_inner();

        // On-disk order is big-endian.
        assert_eq!(&bytes[1..3], &[0x12, 0x34]);
        assert_eq!(&bytes[5..9], &[0xDE, 0xAD, 0xBE, 0xEF]);

        let mut r = BeReader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 0xAB);
        assert_eq!(r.u16().unwrap(), 0x1234);
        assert_eq!(r.i16().unwrap(), -2);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.i32().unwrap(), -70000);
        assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.f32().unwrap(), 1.0);
        assert_eq!(r.f64().unwrap(), -2.5);
        assert!(r.is_empty());
    }

    #[test]
    fn version_dependent_length_width() {
        // PSD: 4-byte marker; PSB: 8-byte marker.
        let mut w = BeWriter::new();
        w.len(Version::Psd, 0x1122_3344).unwrap();
        w.len(Version::Psb, 0x1122_3344).unwrap();
        let bytes = w.into_inner();
        assert_eq!(bytes.len(), 12);
        assert_eq!(&bytes[..4], &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(&bytes[4..], &[0, 0, 0, 0, 0x11, 0x22, 0x33, 0x44]);

        let mut r = BeReader::new(&bytes);
        assert_eq!(r.len(Version::Psd).unwrap(), 0x1122_3344);
        assert_eq!(r.len(Version::Psb).unwrap(), 0x1122_3344);
    }

    #[test]
    fn psd_length_overflow_is_typed_error() {
        let mut w = BeWriter::new();
        assert!(matches!(
            w.len(Version::Psd, u64::from(u32::MAX) + 1),
            Err(PsdError::LengthOverflow { .. })
        ));
    }

    #[test]
    fn reserved_marker_patches_payload_len() {
        let mut w = BeWriter::new();
        let marker = w.reserve_len_marker(Version::Psd);
        w.bytes(&[0xAA; 10]);
        let end = w.position();
        w.patch_len(Version::Psd, marker, end, false).unwrap();
        assert_eq!(&w.as_slice()[..4], &10u32.to_be_bytes());

        // include_marker additionally counts the marker width itself.
        let mut w = BeWriter::new();
        let marker = w.reserve_len_marker(Version::Psd);
        w.bytes(&[0xBB; 6]);
        let end = w.position();
        w.patch_len(Version::Psd, marker, end, true).unwrap();
        assert_eq!(&w.as_slice()[..4], &10u32.to_be_bytes()); // 6 payload + 4 marker

        // Round-trip: reader sees the marker then the payload.
        let bytes = w.into_inner();
        let mut r = BeReader::new(&bytes);
        assert_eq!(r.len(Version::Psd).unwrap(), 10);
        assert_eq!(r.take(6).unwrap(), &[0xBB; 6]);
    }

    #[test]
    fn eof_and_signature_errors_carry_offsets() {
        let mut r = BeReader::new(&[0x38, 0x42]);
        assert!(matches!(
            r.u32(),
            Err(PsdError::UnexpectedEof { offset: 0 })
        ));

        let mut r = BeReader::new(b"8BIMrest");
        r.signature("8BIM").unwrap();
        assert!(matches!(
            r.signature("8BIM"),
            Err(PsdError::InvalidSignature { offset: 4, .. })
        ));
    }

    #[test]
    fn seek_skip_and_pad() {
        let mut r = BeReader::new(&[1, 2, 3, 4, 5, 6]);
        r.skip(2).unwrap();
        assert_eq!(r.u8().unwrap(), 3);
        r.seek(0).unwrap();
        assert_eq!(r.u8().unwrap(), 1);

        let mut w = BeWriter::new();
        w.bytes(&[1, 2, 3]);
        w.pad_to(4);
        assert_eq!(w.as_slice(), &[1, 2, 3, 0]);
    }
}
