//! The tolerant per-row PackBits decode the brush and pattern payloads use.
//!
//! [`psd_codecs::rle::pack_bits_decompress_into`] is the PSD contract: a row
//! must decompress to exactly its declared length. The payloads inside `.abr`
//! samples and pattern channels are looser — a run or literal packet may
//! overshoot the row's width and is clipped, the stream may end early leaving
//! the rest of the row at zero, the 128 header is a no-op, and trailing bytes
//! inside the row's own length are ignored. Photoshop writes exact rows, so
//! the tolerance only shows on odd files; matching the reference's reading of
//! them keeps those files decodable instead of rejected.

/// Decodes one PackBits row into `out`, clipping and tolerating the deviations
/// described above. Bytes the stream never reaches stay at whatever `out`
/// held — callers pass a zeroed row.
pub(crate) fn decode_packbits_row(data: &[u8], out: &mut [u8]) {
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
