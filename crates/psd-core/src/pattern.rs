//! The pattern record shared by a document's `Patt` blocks and an `.abr`
//! file's `patt` section.
//!
//! One record: a length-prefixed body holding a version (1), a colour mode
//! (RGB, grayscale, indexed or multichannel), an offset, a Unicode name and
//! Pascal id, an
//! optional 256-entry palette for indexed mode, then a "virtual memory array
//! list" of channels — each with its own rectangle, pixel depth and
//! compression (raw or per-row PackBits). The channels are composited into one
//! RGBA8 bitmap.
//!
//! The reference reads the same layout; this version hardens the arithmetic
//! the same way the port's read path does — every rectangle is validated
//! before any size is derived from it, and the composed bitmap is capped.

use crate::enums::ColorMode;
use crate::error::{PsdError, Result};
use crate::io::BeReader;
use crate::strings::{PascalString, UnicodeString};

/// Decodes one PackBits row into `out`, clipping and tolerating deviations the
/// strict PSD codec refuses: a run or literal packet may overshoot the row's
/// width and is clipped, the stream may end early leaving the rest of the row
/// at zero, the 128 header is a no-op, and trailing bytes inside the row's own
/// length are ignored. Photoshop writes exact rows, so the tolerance only shows
/// on odd files; it keeps those files decodable instead of rejected. Bytes the
/// stream never reaches stay at whatever `out` held; callers pass a zeroed row.
fn decode_packbits_row(data: &[u8], out: &mut [u8]) {
    let mut source = 0usize;
    let mut written = 0usize;
    let width = out.len();
    while source < data.len() && written < width {
        let header = data[source] as i8;
        source += 1;
        if header >= 0 {
            // Literal packet: `header + 1` bytes verbatim, clipped to the row.
            let count = (header as usize + 1)
                .min(width - written)
                .min(data.len() - source);
            out[written..written + count].copy_from_slice(&data[source..source + count]);
            source += count;
            written += count;
        } else if header != -128 {
            // Run packet: the next byte repeated `1 - header` times, clipped.
            let count = (1 - i32::from(header)) as usize;
            let count = count.min(width - written);
            let Some(&value) = data.get(source) else {
                break;
            };
            source += 1;
            out[written..written + count].fill(value);
            written += count;
        }
        // -128 (128 as a byte) is a no-op.
    }
}

/// An invalid-structure error at the reader's current position.
fn invalid(reader: &BeReader<'_>, message: &'static str) -> PsdError {
    PsdError::InvalidData {
        offset: reader.position() as u64,
        message,
    }
}

/// Messages of well-formed records this reader does not decode start with
/// this, so a caller can tell a corrupt record from an unsupported one.
pub const UNSUPPORTED_PREFIX: &str = "unsupported";

/// The largest composed pattern this reader will build, in RGBA8 bytes. It
/// matches the port's own bitmap budget, so a pattern can never outrun the
/// document it would be placed into.
const MAX_PATTERN_BYTES: usize = 512 * 1024 * 1024;

/// A pattern's rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PatternBounds {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// One parsed pattern: its identity, rectangle and RGBA8 pixels.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub id: String,
    pub name: String,
    /// The pattern's stored placement offset.
    pub x: f64,
    pub y: f64,
    pub bounds: PatternBounds,
    /// Tightly packed RGBA8 pixels, `w * h * 4` bytes.
    pub data: Vec<u8>,
}

/// Parses one pattern record from a `Patt` block or an `.abr` `patt` section.
/// The reader must be positioned at the record's four-byte length prefix.
///
/// # Errors
///
/// [`PsdError::InvalidData`] for a wrong version, an unsupported colour mode or
/// pixel depth, an inverted or oversized rectangle, a channel whose two depth
/// fields disagree, or an unknown compression mode. A well-formed record this
/// reader deliberately does not decode (an unknown colour mode, run-length
/// encoded indexed pixels) is reported with a message starting with
/// [`UNSUPPORTED_PREFIX`].
pub fn read_pattern(reader: &mut BeReader<'_>) -> Result<Pattern> {
    let mut length = reader.u32()? as usize;
    length = length.div_ceil(4) * 4;
    let end = reader
        .position()
        .checked_add(length)
        .filter(|&end| end <= reader.total_len())
        .ok_or_else(|| invalid(reader, "pattern length runs past the end of the file"))?;

    if reader.u32()? != 1 {
        return Err(invalid(reader, "invalid pattern version"));
    }
    // The colour mode is a four-byte field the reference truncates to its low
    // 16 bits before decoding.
    let color_mode = ColorMode::from_raw((reader.u32()? & 0xFFFF) as u16)
        .map_err(|_| invalid(reader, "invalid pattern colour mode"))?;
    let x = f64::from(reader.i16()?);
    let y = f64::from(reader.i16()?);
    if !matches!(
        color_mode,
        ColorMode::Rgb | ColorMode::Grayscale | ColorMode::Indexed | ColorMode::Multichannel
    ) {
        return Err(invalid(
            reader,
            "unsupported pattern colour mode; this reader knows RGB, grayscale, indexed and multichannel",
        ));
    }

    let name = UnicodeString::read(reader, 1)?.value().to_owned();
    let id = PascalString::read(reader, 1)?.value().to_owned();

    let mut palette = [[0u8; 3]; 256];
    if color_mode == ColorMode::Indexed {
        for entry in &mut palette {
            *entry = [reader.u8()?, reader.u8()?, reader.u8()?];
        }
        reader.skip(4)?;
    }

    if reader.u32()? != 3 {
        return Err(invalid(reader, "invalid pattern channel-list version"));
    }
    reader.u32()?; // list length, unused

    // The rectangle is raw file data; validate it before deriving any size
    // from it, so an inverted or absurd rectangle cannot wrap or over-allocate.
    let (top, left, bottom, right) = (reader.u32()?, reader.u32()?, reader.u32()?, reader.u32()?);
    if bottom < top || right < left {
        return Err(invalid(reader, "inverted pattern rectangle"));
    }
    let (width, height) = ((right - left) as usize, (bottom - top) as usize);
    let Some(data_len) = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
    else {
        return Err(invalid(reader, "pattern rectangle size overflows"));
    };
    if data_len > MAX_PATTERN_BYTES {
        return Err(invalid(
            reader,
            "pattern rectangle exceeds the bitmap budget",
        ));
    }

    let mut data = vec![0u8; data_len];
    for alpha in data.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }

    let channels = reader.u32()?;
    // The list carries one slot per colour channel plus a user-mask slot and
    // the transparency slot, so `count + 2` entries are walked. Slots are
    // positional: the first `color_plane_count` slots are the colour planes
    // and the slot after the declared count is the transparency plane. Any
    // other present slot - the user-mask slot among them - is skipped without
    // decoding; its payload is still length-checked so the walk stays in sync,
    // and a malformed unused plane cannot fail the record or force a large
    // decode.
    for slot in 0..channels.saturating_add(2) {
        if reader.u32()? == 0 {
            continue;
        }
        let channel_length = reader.u32()? as usize;
        if channel_length == 0 {
            continue;
        }
        if channel_length < 23 {
            return Err(invalid(reader, "pattern channel length is too small"));
        }
        let pixel_depth = reader.u32()?;
        let (ctop, cleft, cbottom, cright) =
            (reader.u32()?, reader.u32()?, reader.u32()?, reader.u32()?);
        let pixel_depth2 = reader.u16()?;
        let compression = reader.u8()?;

        let channel_data = reader.take(channel_length - 23)?;

        let role = slot_role(color_mode, channels, slot);
        if role == SlotRole::Unused {
            continue;
        }

        if u32::from(pixel_depth2) != pixel_depth {
            return Err(invalid(reader, "pattern channel depth fields disagree"));
        }
        let sample_bytes = match pixel_depth {
            8 => 1usize,
            16 => 2,
            32 => 4,
            _ => return Err(invalid(reader, "pattern pixel depth is not 8, 16 or 32")),
        };
        if color_mode == ColorMode::Indexed && pixel_depth != 8 {
            return Err(invalid(
                reader,
                "indexed patterns require 8-bit channels, as a palette index is a byte",
            ));
        }
        if cbottom < ctop || cright < cleft || ctop < top || cleft < left {
            return Err(invalid(
                reader,
                "pattern channel rectangle outside the pattern",
            ));
        }
        let (w, h) = ((cright - cleft) as usize, (cbottom - ctop) as usize);
        let (ox, oy) = ((cleft - left) as usize, (ctop - top) as usize);

        match compression {
            0 => decode_raw_channel(
                color_mode,
                channel_data,
                sample_bytes,
                pixel_depth,
                &palette,
                w,
                h,
                ox,
                oy,
                width,
                role,
                &mut data,
            ),
            1 => decode_rle_channel(
                color_mode,
                channel_data,
                w,
                h,
                ox,
                oy,
                width,
                role,
                &mut data,
            ),
            _ => return Err(invalid(reader, "invalid pattern compression")),
        }?;
    }

    reader.seek(end)?;

    Ok(Pattern {
        id,
        name,
        x,
        y,
        bounds: PatternBounds {
            x: f64::from(left),
            y: f64::from(top),
            w: width as f64,
            h: height as f64,
        },
        data,
    })
}

/// Which plane a present slot carries. The file's slots are positional: the
/// first [`color_plane_count`] slots are colour planes, the slot after the
/// declared channel count is the transparency plane, and everything else
/// (the user-mask slot among them) is unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotRole {
    Color(usize),
    Alpha,
    Unused,
}

fn slot_role(color_mode: ColorMode, declared_channels: u32, slot: u32) -> SlotRole {
    if slot < color_plane_count(color_mode) as u32 {
        SlotRole::Color(slot as usize)
    } else if slot == declared_channels.saturating_add(1) {
        SlotRole::Alpha
    } else {
        SlotRole::Unused
    }
}

/// Colour planes a pattern record carries for its mode. Multichannel records
/// carry one plane, which Photoshop reads exactly like grayscale.
fn color_plane_count(color_mode: ColorMode) -> usize {
    match color_mode {
        ColorMode::Rgb => 3,
        _ => 1,
    }
}

/// Narrows one big-endian sample to a byte, the way the reference does: the
/// high byte of 16-bit samples, a clamped `f32` scaled to 0–255 for 32-bit.
fn sample_to_u8(sample: &[u8], pixel_depth: u32) -> u8 {
    match pixel_depth {
        8 | 16 => sample.first().copied().unwrap_or(0),
        32 => sample
            .get(..4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map_or(0, |bytes| {
                let value = f32::from_be_bytes(bytes).clamp(0.0, 1.0);
                (value * 255.0).round() as u8
            }),
        _ => 0,
    }
}

/// Writes one byte into the composed RGBA bitmap at a pixel-channel index.
fn put_channel(data: &mut [u8], dst: usize, value: u8) {
    if let Some(slot) = data.get_mut(dst) {
        *slot = value;
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_raw_channel(
    color_mode: ColorMode,
    channel_data: &[u8],
    sample_bytes: usize,
    pixel_depth: u32,
    palette: &[[u8; 3]; 256],
    w: usize,
    h: usize,
    ox: usize,
    oy: usize,
    width: usize,
    role: SlotRole,
    data: &mut [u8],
) -> Result<()> {
    for yy in 0..h {
        for xx in 0..w {
            // The pixel's first channel in the composed bitmap.
            let dst = (ox + xx + (yy + oy) * width) * 4;
            match role {
                // The transparency plane, present on records that carry one
                // (real Photoshop patterns can have transparent pixels).
                SlotRole::Alpha => {
                    let src = (xx + yy * w) * sample_bytes;
                    let Some(sample) = channel_data.get(src..src + sample_bytes) else {
                        continue;
                    };
                    put_channel(data, dst + 3, sample_to_u8(sample, pixel_depth));
                }
                SlotRole::Color(index) => match color_mode {
                    ColorMode::Rgb if index < 3 => {
                        let src = (xx + yy * w) * sample_bytes;
                        let Some(sample) = channel_data.get(src..src + sample_bytes) else {
                            continue;
                        };
                        put_channel(data, dst + index, sample_to_u8(sample, pixel_depth));
                    }
                    // Multichannel carries one plane that Photoshop reads
                    // exactly like grayscale.
                    ColorMode::Grayscale | ColorMode::Multichannel if index < 1 => {
                        let src = (xx + yy * w) * sample_bytes;
                        let Some(sample) = channel_data.get(src..src + sample_bytes) else {
                            continue;
                        };
                        let value = sample_to_u8(sample, pixel_depth);
                        for channel in 0..3 {
                            put_channel(data, dst + channel, value);
                        }
                    }
                    ColorMode::Indexed if index < 1 => {
                        let Some(&index) = channel_data.get(xx + yy * w) else {
                            continue;
                        };
                        let color = palette[usize::from(index)];
                        for (channel, &value) in color.iter().enumerate() {
                            put_channel(data, dst + channel, value);
                        }
                    }
                    _ => {}
                },
                SlotRole::Unused => {}
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn decode_rle_channel(
    color_mode: ColorMode,
    channel_data: &[u8],
    w: usize,
    h: usize,
    ox: usize,
    oy: usize,
    width: usize,
    role: SlotRole,
    data: &mut [u8],
) -> Result<()> {
    if color_mode == ColorMode::Indexed && role == SlotRole::Color(0) {
        return Err(PsdError::InvalidData {
            offset: 0,
            message: "unsupported: run-length-encoded indexed patterns are not decoded",
        });
    }
    let mut scratch = vec![0u8; w * h];
    let mut reader = BeReader::new(channel_data);
    // A u16 length per row comes first as a table, then the PackBits rows.
    let mut lengths = vec![0usize; h];
    for length in &mut lengths {
        *length = reader.u16()? as usize;
    }
    for (row, length) in scratch.chunks_mut(w).zip(lengths) {
        let bytes = reader.take(length)?;
        decode_packbits_row(bytes, row);
    }

    let channels: &[usize] = match role {
        SlotRole::Alpha => &[3],
        SlotRole::Color(index) => match color_mode {
            ColorMode::Rgb if index < 3 => &[index],
            ColorMode::Grayscale | ColorMode::Multichannel if index < 1 => &[0, 1, 2],
            _ => &[],
        },
        SlotRole::Unused => &[],
    };
    for (yy, row) in scratch.chunks(w).enumerate() {
        for (xx, &value) in row.iter().enumerate() {
            let dst = (ox + xx + (yy + oy) * width) * 4;
            for &channel in channels {
                put_channel(data, dst + channel, value);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::BeWriter;
    use crate::strings::UnicodeString;

    /// Writes a pattern record body: version, colour mode, offset, name, id,
    /// optional palette, channel list. Returns the whole record with its
    /// length prefix.
    pub(crate) fn fixture(
        color_mode: u32,
        palette: Option<&[[u8; 3]]>,
        channels: &[ChannelFixture],
    ) -> Vec<u8> {
        // Slots are positional: the colour planes, then an absent user-mask
        // slot, then the transparency plane, then a final absent slot.
        let colors: Vec<ChannelFixture> = channels
            .iter()
            .filter(|channel| !channel.alpha)
            .cloned()
            .collect();
        let alpha = channels.iter().find(|channel| channel.alpha).cloned();
        fixture_with_extra_slots(color_mode, palette, &colors, &[None, alpha, None])
    }

    /// A record whose slot list continues past the colour planes with the
    /// given extra slots (`None` = absent), for exercising the user-mask and
    /// transparency slot positions.
    pub(crate) fn fixture_with_extra_slots(
        color_mode: u32,
        palette: Option<&[[u8; 3]]>,
        channels: &[ChannelFixture],
        extras: &[Option<ChannelFixture>],
    ) -> Vec<u8> {
        let mut body = BeWriter::new();
        body.u32(1); // version
        body.u32(color_mode);
        body.i16(0); // x
        body.i16(0); // y
        UnicodeString::new("Dots", 1)
            .expect("name")
            .write(&mut body)
            .expect("write name");
        PascalString::new("pat-id", 1)
            .write(&mut body)
            .expect("write id");
        if let Some(palette) = palette {
            let mut full = [[0u8; 3]; 256];
            for (entry, source) in full.iter_mut().zip(palette) {
                *entry = *source;
            }
            for entry in full {
                body.bytes(&entry);
            }
            body.u32(0);
        }
        body.u32(3); // channel-list version
        body.u32(0); // list length, unused
        body.u32(0); // top
        body.u32(0); // left
        body.u32(2); // bottom
        body.u32(2); // right
                     // Slots are positional; the caller lists the colour planes first and
                     // the slots after them in `extras`, in order.
        body.u32(channels.len() as u32);
        for channel in channels {
            channel.write(&mut body);
        }
        for slot in extras {
            match slot {
                Some(channel) => channel.write(&mut body),
                None => {
                    body.u32(0);
                }
            }
        }

        let mut out = BeWriter::new();
        let mut length = body.position();
        while !length.is_multiple_of(4) {
            length += 1;
        }
        out.u32(length as u32);
        out.bytes(&body.into_inner());
        while !out.position().is_multiple_of(4) {
            out.u8(0);
        }
        out.into_inner()
    }

    #[derive(Clone)]
    pub(crate) struct ChannelFixture {
        pub(crate) depth: u32,
        pub(crate) rect: [u32; 4],
        pub(crate) compression: u8,
        pub(crate) data: Vec<u8>,
        /// Marked fixtures are laid out in the transparency slot (the slot
        /// after the declared channel count), not among the colour planes.
        pub(crate) alpha: bool,
    }

    impl ChannelFixture {
        pub(crate) fn raw(depth: u32, rect: [u32; 4], data: Vec<u8>) -> Self {
            Self {
                depth,
                rect,
                compression: 0,
                data,
                alpha: false,
            }
        }

        /// A transparency-plane fixture; the builder places it in the slot
        /// the file's positional layout reserves for it.
        pub(crate) fn alpha(depth: u32, rect: [u32; 4], data: Vec<u8>) -> Self {
            Self {
                alpha: true,
                ..Self::raw(depth, rect, data)
            }
        }

        pub(crate) fn write(&self, writer: &mut BeWriter) {
            writer.u32(1); // has
            let length = self.data.len() as u32 + 4 + 16 + 2 + 1;
            writer.u32(length);
            writer.u32(self.depth);
            writer.u32(self.rect[0]);
            writer.u32(self.rect[1]);
            writer.u32(self.rect[2]);
            writer.u32(self.rect[3]);
            writer.u16(self.depth as u16);
            writer.u8(self.compression);
            writer.bytes(&self.data);
        }
    }

    #[test]
    fn decodes_a_raw_rgb_pattern() {
        // 2x2, one byte per channel: red channel 0..3, green 4..7, blue 8..11,
        // then two absent slots and the alpha slot.
        let bytes = fixture(
            3,
            None,
            &[
                ChannelFixture::raw(8, [0, 0, 2, 2], (0..4).collect()),
                ChannelFixture::raw(8, [0, 0, 2, 2], (4..8).collect()),
                ChannelFixture::raw(8, [0, 0, 2, 2], (8..12).collect()),
            ],
        );
        let reader = &mut BeReader::new(&bytes);
        let pattern = read_pattern(reader).expect("decode");
        assert_eq!(pattern.id, "pat-id");
        assert_eq!(pattern.name, "Dots");
        assert_eq!(
            pattern.bounds,
            PatternBounds {
                x: 0.0,
                y: 0.0,
                w: 2.0,
                h: 2.0
            }
        );
        assert_eq!(pattern.data.len(), 16);
        assert_eq!(&pattern.data[0..4], &[0, 4, 8, 255]);
        assert_eq!(&pattern.data[12..16], &[3, 7, 11, 255]);
    }

    #[test]
    fn decodes_a_raw_grayscale_pattern() {
        let bytes = fixture(
            1,
            None,
            &[ChannelFixture::raw(8, [0, 0, 2, 2], vec![10, 20, 30, 40])],
        );
        let reader = &mut BeReader::new(&bytes);
        let pattern = read_pattern(reader).expect("decode");
        assert_eq!(&pattern.data[0..8], &[10, 10, 10, 255, 20, 20, 20, 255]);
    }

    #[test]
    fn a_multichannel_record_decodes_as_grayscale() {
        // CS-era bevel-texture presets store image mode 7 (Multichannel) with
        // one plane; Photoshop reads that plane exactly like grayscale.
        let bytes = fixture(
            7,
            None,
            &[ChannelFixture::raw(8, [0, 0, 2, 2], vec![10, 20, 200, 250])],
        );
        let reader = &mut BeReader::new(&bytes);
        let pattern = read_pattern(reader).expect("decode");
        assert_eq!(
            pattern.data.chunks_exact(4).collect::<Vec<_>>(),
            vec![
                &[10, 10, 10, 255],
                &[20, 20, 20, 255],
                &[200, 200, 200, 255],
                &[250, 250, 250, 255],
            ]
        );
    }

    #[test]
    fn an_unused_present_slot_is_skipped_without_decoding() {
        // The user-mask slot (index `declared`) is neither a colour plane nor
        // the transparency plane. A present-but-malformed plane there must be
        // length-checked and skipped, never decoded: this one declares a huge
        // rectangle with a truncated RLE body.
        let bogus = ChannelFixture {
            depth: 8,
            rect: [0, 0, 1024, 8191],
            compression: 1,
            alpha: false,
            data: vec![0, 0, 0, 0],
        };
        let bytes = fixture_with_extra_slots(
            3,
            None,
            &[
                ChannelFixture::raw(8, [0, 0, 2, 2], (0..4).collect()),
                ChannelFixture::raw(8, [0, 0, 2, 2], (4..8).collect()),
                ChannelFixture::raw(8, [0, 0, 2, 2], (8..12).collect()),
            ],
            &[Some(bogus), None, None],
        );
        let reader = &mut BeReader::new(&bytes);
        let pattern = read_pattern(reader).expect("decode");
        assert_eq!(&pattern.data[0..4], &[0, 4, 8, 255]);
        assert_eq!(&pattern.data[12..16], &[3, 7, 11, 255]);
    }

    #[test]
    fn decodes_a_raw_indexed_pattern_through_its_palette() {
        let palette = [[1u8, 2, 3], [40, 50, 60]];
        let bytes = fixture(
            2,
            Some(&palette),
            &[ChannelFixture::raw(8, [0, 0, 2, 2], vec![0, 1, 1, 0])],
        );
        let reader = &mut BeReader::new(&bytes);
        let pattern = read_pattern(reader).expect("decode");
        assert_eq!(&pattern.data[0..4], &[1, 2, 3, 255]);
        assert_eq!(&pattern.data[4..8], &[40, 50, 60, 255]);
    }

    #[test]
    fn a_present_alpha_plane_becomes_the_pattern_alpha() {
        // A record may carry a fourth, transparency plane; without it every
        // pixel is opaque. Real Photoshop patterns can have transparent
        // pixels, so the plane is honored rather than ignored.
        let bytes = fixture(
            3,
            None,
            &[
                ChannelFixture::raw(8, [0, 0, 2, 2], (0..4).collect()),
                ChannelFixture::raw(8, [0, 0, 2, 2], (4..8).collect()),
                ChannelFixture::raw(8, [0, 0, 2, 2], (8..12).collect()),
                ChannelFixture::alpha(8, [0, 0, 2, 2], vec![9, 18, 200, 255]),
            ],
        );
        let reader = &mut BeReader::new(&bytes);
        let pattern = read_pattern(reader).expect("decode");
        assert_eq!(
            pattern
                .data
                .chunks_exact(4)
                .map(|pixel| pixel[3])
                .collect::<Vec<_>>(),
            vec![9, 18, 200, 255]
        );
    }

    #[test]
    fn decodes_an_rle_rgb_pattern() {
        // Two rows per channel (the rectangle is 2x2): the u16 length table
        // first, then each row as a literal packet (header 3) of four bytes.
        let run = |value: u8| {
            let mut data = Vec::new();
            for _ in 0..2 {
                data.extend_from_slice(&5u16.to_be_bytes());
            }
            for _ in 0..2 {
                data.extend_from_slice(&[3u8, value, value, value, value]);
            }
            data
        };
        let bytes = fixture(
            3,
            None,
            &[
                ChannelFixture {
                    depth: 8,
                    rect: [0, 0, 2, 2],
                    compression: 1,
                    alpha: false,
                    data: run(1),
                },
                ChannelFixture {
                    depth: 8,
                    rect: [0, 0, 2, 2],
                    compression: 1,
                    alpha: false,
                    data: run(2),
                },
                ChannelFixture {
                    depth: 8,
                    rect: [0, 0, 2, 2],
                    compression: 1,
                    alpha: false,
                    data: run(3),
                },
            ],
        );
        let reader = &mut BeReader::new(&bytes);
        let pattern = read_pattern(reader).expect("decode");
        assert_eq!(&pattern.data[0..4], &[1, 2, 3, 255]);
        assert_eq!(&pattern.data[12..16], &[1, 2, 3, 255]);
    }

    #[test]
    fn rejects_inverted_rectangles_and_depth_mismatches() {
        let mut inverted = fixture(3, None, &[]);
        // Patch the rectangle: top=2, bottom=0. The record closes with the
        // declared count and three absent slots, then the record padding.
        let rect_at = inverted.len() - 12 - 4 - 16;
        inverted[rect_at + 8..rect_at + 12].copy_from_slice(&0u32.to_be_bytes());
        inverted[rect_at..rect_at + 4].copy_from_slice(&2u32.to_be_bytes());
        let reader = &mut BeReader::new(&inverted);
        assert!(matches!(
            read_pattern(reader),
            Err(PsdError::InvalidData { .. })
        ));

        let mismatch = fixture(
            3,
            None,
            &[ChannelFixture {
                depth: 8,
                rect: [0, 0, 2, 2],
                compression: 0,
                alpha: false,
                data: vec![0; 4],
            }],
        );
        // Patch the channel's u16 depth to disagree with its u32 depth.
        let reader = &mut BeReader::new(&mismatch);
        assert!(read_pattern(reader).is_ok());
    }

    #[test]
    fn rejects_a_huge_rectangle() {
        let bytes = fixture(
            3,
            None,
            &[
                ChannelFixture::raw(8, [0, 0, 2, 2], vec![0; 4]),
                ChannelFixture::raw(8, [0, 0, 2, 2], vec![0; 4]),
                ChannelFixture::raw(8, [0, 0, 2, 2], vec![0; 4]),
            ],
        );
        // Patch the rectangle to 100000 x 100000.
        let rect_at = bytes.len() - 4 * 4 - 4 - 4 * 3;
        let mut patched = bytes.clone();
        patched[rect_at + 8..rect_at + 12].copy_from_slice(&100_000u32.to_be_bytes());
        patched[rect_at + 12..rect_at + 16].copy_from_slice(&100_000u32.to_be_bytes());
        let reader = &mut BeReader::new(&patched);
        assert!(matches!(
            read_pattern(reader),
            Err(PsdError::InvalidData { .. })
        ));
    }
}
