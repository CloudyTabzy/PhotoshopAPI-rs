//! PackBits run-length codec — mirrors `Compress_RLE.h` / `Decompress_RLE.h`.
//!
//! Packet semantics:
//! - header `0..=127`: literal — copy the next `header + 1` bytes verbatim
//! - header `128`:     no-op (used to pad compressed scanlines to even length)
//! - header `129..=255`: run — repeat the next byte `257 - header` times
//!
//! [`pack_bits_compress`] is byte-exact against upstream `CompressPackBits`
//! (pinned by the Wikipedia vector in the tests): runs start at any 2-byte
//! repeat, literal runs flush when a repeat begins, both caps are 128 bytes,
//! and odd-length output is padded with a `0x80` no-op.

use rayon::prelude::*;

use crate::error::{CodecError, Result};

/// Maximum bytes one run or literal packet can hold (header is 1 byte).
const MAX_PACKET_LEN: usize = 128;

/// Channels below this size decode/compress sequentially: rayon task
/// overhead outweighs the win on small payloads (the 64×64 corpus, thumbnails).
pub const PARALLEL_MIN_BYTES: usize = 64 * 1024;

/// Compress one scanline with PackBits, padded to even length with `0x80`.
///
/// Faithful port of upstream `RLE_Impl::CompressPackBits`
/// (`Core/Compression/Compress_RLE.h`): a run/literal state machine where
/// `run_len` counts repeat *matches* and both counters flush at 128.
pub fn pack_bits_compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    pack_bits_compress_into(data, &mut out);
    out
}

/// [`pack_bits_compress`] appending to `out`, so a caller compressing many
/// scanlines can fill one buffer instead of allocating one per row. Returns
/// the number of bytes appended, padding included; the padding is per row (it
/// aligns this row's packets, not the buffer), and an empty row appends
/// nothing.
pub fn pack_bits_compress_into(data: &[u8], out: &mut Vec<u8>) -> usize {
    let start = out.len();
    visit_packets(data, |packet| match packet {
        Packet::Literal(bytes) => write_literal(out, bytes),
        Packet::Run { len, byte } => write_run(out, len, byte),
    });
    if !(out.len() - start).is_multiple_of(2) {
        out.push(128);
    }
    out.len() - start
}

/// Exact PackBits length, including row alignment, without allocating or
/// copying packets. It shares packet boundaries with the encoder.
pub fn pack_bits_encoded_len(data: &[u8]) -> usize {
    let mut len = 0usize;
    visit_packets(data, |packet| {
        len += match packet {
            Packet::Literal(bytes) => bytes.len() + 1,
            Packet::Run { .. } => 2,
        };
    });
    len + len % 2
}

#[derive(Clone, Copy)]
enum Packet<'a> {
    Literal(&'a [u8]),
    Run { len: usize, byte: u8 },
}

#[inline]
fn visit_packets(data: &[u8], mut emit: impl FnMut(Packet<'_>)) {
    let mut cursor = 0usize;
    while cursor < data.len() {
        if cursor + 1 < data.len() && data[cursor] == data[cursor + 1] {
            let byte = data[cursor];
            let mut run = equal_prefix(&data[cursor..], byte);
            while run >= MAX_PACKET_LEN {
                emit(Packet::Run {
                    len: MAX_PACKET_LEN,
                    byte,
                });
                cursor += MAX_PACKET_LEN;
                run -= MAX_PACKET_LEN;
            }
            if run >= 2 {
                emit(Packet::Run { len: run, byte });
                cursor += run;
                continue;
            }
            // A one-byte remainder belongs to the next literal, exactly as
            // upstream's run-match counter leaves its current byte pending.
            if run == 0 {
                continue;
            }
        }
        let end = data.len().min(cursor.saturating_add(MAX_PACKET_LEN + 1));
        let literals = first_repeat(&data[cursor..end])
            .unwrap_or(end - cursor)
            .min(MAX_PACKET_LEN);
        emit(Packet::Literal(&data[cursor..cursor + literals]));
        cursor += literals;
    }
}

#[inline]
fn first_repeat(bytes: &[u8]) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let mut offset = 0;
    while offset + 8 < bytes.len() {
        let left = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let right = u64::from_le_bytes(bytes[offset + 1..offset + 9].try_into().unwrap());
        let different = left ^ right;
        let zeros = different.wrapping_sub(ONES) & !different & HIGH;
        if zeros != 0 {
            // Borrow propagation can mark later bytes, but the first marked
            // byte is always the first actual zero. Never count these bits.
            return Some(offset + zeros.trailing_zeros() as usize / 8);
        }
        offset += 8;
    }
    bytes[offset..]
        .windows(2)
        .position(|pair| pair[0] == pair[1])
        .map(|i| offset + i)
}

#[inline]
fn equal_prefix(bytes: &[u8], byte: u8) -> usize {
    let repeated = u64::from_le_bytes([byte; 8]);
    let mut offset = 0;
    while offset + 8 <= bytes.len() {
        let different =
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) ^ repeated;
        if different != 0 {
            return offset + different.trailing_zeros() as usize / 8;
        }
        offset += 8;
    }
    offset
        + bytes[offset..]
            .iter()
            .position(|&value| value != byte)
            .unwrap_or(bytes.len() - offset)
}

/// Exact table and packet length for a complete channel. Large images count
/// independent rows in parallel; no compressed channel is staged merely to
/// decide whether storing the raw bytes would be smaller.
pub fn encoded_scanlines_len(
    data: &[u8],
    scanline_bytes: usize,
    size_width: usize,
) -> Result<usize> {
    Ok(count_scanlines(data, scanline_bytes, size_width)?.encoded_len())
}

/// The packed length of every scanline of one channel, measured once.
///
/// The counts size the scanline table and the packet bytes exactly, so a
/// caller can decide between Raw and RLE without a compressed buffer, and
/// [`compress_counted_scanlines`] then writes every row straight into its
/// final position: one output allocation, no per-block staging or copy.
#[derive(Debug, Clone)]
pub struct ScanlineCounts {
    sizes: Vec<usize>,
    data_len: usize,
    scanline_bytes: usize,
    size_width: usize,
    encoded_len: usize,
}

impl ScanlineCounts {
    /// Bytes of the encoded channel: the size table plus every packed row.
    pub fn encoded_len(&self) -> usize {
        self.encoded_len
    }

    /// Whether every packed row fits a size-table entry (2 bytes for PSD,
    /// 4 for PSB). A row that does not can only be stored raw.
    pub fn fits_table(&self) -> bool {
        self.oversized_row().is_none()
    }

    fn oversized_row(&self) -> Option<usize> {
        let limit = table_entry_limit(self.size_width);
        self.sizes.iter().copied().find(|&size| size > limit)
    }
}

fn table_entry_limit(size_width: usize) -> usize {
    if size_width == 2 {
        u16::MAX as usize
    } else {
        u32::MAX as usize
    }
}

fn check_scanline_geometry(
    data_len: usize,
    scanline_bytes: usize,
    size_width: usize,
) -> Result<()> {
    if scanline_bytes == 0 {
        return Err(CodecError::InvalidInput("scanline length must be non-zero"));
    }
    if size_width != 2 && size_width != 4 {
        return Err(CodecError::InvalidInput(
            "scanline size width must be 2 (PSD) or 4 (PSB)",
        ));
    }
    if !data_len.is_multiple_of(scanline_bytes) {
        return Err(CodecError::InvalidInput(
            "channel data does not divide evenly into scanlines",
        ));
    }
    Ok(())
}

/// Measure every scanline's PackBits length; large channels count rows in
/// parallel. Rows too long for a size-table entry are reported by
/// [`ScanlineCounts::fits_table`], not rejected, so an automatic codec choice
/// can still store such a channel raw.
pub fn count_scanlines(
    data: &[u8],
    scanline_bytes: usize,
    size_width: usize,
) -> Result<ScanlineCounts> {
    check_scanline_geometry(data.len(), scanline_bytes, size_width)?;
    let sizes: Vec<usize> = if data.len() >= PARALLEL_MIN_BYTES {
        data.par_chunks(scanline_bytes)
            .map(pack_bits_encoded_len)
            .collect()
    } else {
        data.chunks(scanline_bytes)
            .map(pack_bits_encoded_len)
            .collect()
    };
    let overflow = || CodecError::InvalidInput("compressed length overflows");
    let packets = sizes
        .iter()
        .try_fold(0usize, |sum, &size| sum.checked_add(size))
        .ok_or_else(overflow)?;
    let encoded_len = sizes
        .len()
        .checked_mul(size_width)
        .and_then(|table| table.checked_add(packets))
        .ok_or_else(overflow)?;
    Ok(ScanlineCounts {
        sizes,
        data_len: data.len(),
        scanline_bytes,
        size_width,
        encoded_len,
    })
}

/// Run packet: header `257 - len`, then the repeated byte.
#[inline]
fn write_run(out: &mut Vec<u8>, len: usize, byte: u8) {
    debug_assert!((1..=MAX_PACKET_LEN).contains(&len));
    out.push((257 - len) as u8);
    out.push(byte);
}

/// Literal packet: header `len - 1`, then the bytes verbatim.
#[inline]
fn write_literal(out: &mut Vec<u8>, bytes: &[u8]) {
    debug_assert!((1..=MAX_PACKET_LEN).contains(&bytes.len()));
    out.push(bytes.len() as u8 - 1);
    out.extend_from_slice(bytes);
}

/// Pack one scanline into `out`, which must be exactly its counted length
/// (padding included). The packets are the ones [`pack_bits_compress_into`]
/// emits; a length mismatch means the counts describe other data.
fn pack_bits_write(data: &[u8], out: &mut [u8]) -> Result<()> {
    let mut cursor = 0usize;
    let mut fits = true;
    visit_packets(data, |packet| {
        let need = match packet {
            Packet::Literal(bytes) => bytes.len() + 1,
            Packet::Run { .. } => 2,
        };
        let Some(dest) = out.get_mut(cursor..cursor + need).filter(|_| fits) else {
            fits = false;
            return;
        };
        match packet {
            Packet::Literal(bytes) => {
                dest[0] = bytes.len() as u8 - 1;
                dest[1..].copy_from_slice(bytes);
            }
            Packet::Run { len, byte } => {
                dest[0] = (257 - len) as u8;
                dest[1] = byte;
            }
        }
        cursor += need;
    });
    if fits && cursor % 2 == 1 {
        if let Some(pad) = out.get_mut(cursor) {
            *pad = 128;
            cursor += 1;
        }
    }
    if !fits || cursor != out.len() {
        return Err(CodecError::InvalidInput(
            "scanline counts do not describe this data",
        ));
    }
    Ok(())
}

/// Decompress exactly `out_len` bytes from a PackBits stream.
///
/// Mirrors `RLE_Impl::DecompressPackBits`: the produced byte count must match
/// `out_len` exactly — overshoot, early end-of-stream, or trailing data after
/// the target is reached are all errors.
pub fn pack_bits_decompress(data: &[u8], out_len: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; out_len];
    pack_bits_decompress_into(data, &mut out)?;
    Ok(out)
}

/// Decompress into a caller-provided buffer (no per-call allocation).
/// `out.len()` is the exact number of bytes to produce.
pub fn pack_bits_decompress_into(data: &[u8], out: &mut [u8]) -> Result<()> {
    let out_len = out.len();
    let mut produced = 0usize;
    let mut pos = 0usize;

    while produced < out_len {
        let Some(&header) = data.get(pos) else {
            return Err(CodecError::InvalidPackBits(
                "stream ended before the declared output length",
            ));
        };
        pos += 1;

        match header {
            // Literal: copy the next `header + 1` bytes.
            0..=127 => {
                let count = header as usize + 1;
                if produced + count > out_len {
                    return Err(CodecError::InvalidPackBits(
                        "literal packet overshoots the declared output length",
                    ));
                }
                let Some(chunk) = data.get(pos..pos + count) else {
                    return Err(CodecError::InvalidPackBits(
                        "literal packet runs past the end of the stream",
                    ));
                };
                out[produced..produced + count].copy_from_slice(chunk);
                produced += count;
                pos += count;
            }
            // No-op (even padding).
            128 => {}
            // Run: repeat the next byte `257 - header` times.
            _ => {
                let count = 257 - header as usize;
                if produced + count > out_len {
                    return Err(CodecError::InvalidPackBits(
                        "run packet overshoots the declared output length",
                    ));
                }
                let Some(&byte) = data.get(pos) else {
                    return Err(CodecError::InvalidPackBits(
                        "run packet runs past the end of the stream",
                    ));
                };
                pos += 1;
                out[produced..produced + count].fill(byte);
                produced += count;
            }
        }
    }

    // Any bytes left after reaching `out_len` (other than the padding no-op)
    // indicate a corrupt or mismatched stream.
    for &b in &data[pos..] {
        if b != 128 {
            return Err(CodecError::InvalidPackBits(
                "trailing data after the declared output length",
            ));
        }
    }

    debug_assert_eq!(produced, out_len);
    Ok(())
}

/// Decompress one PackBits row the way Photoshop recovers damaged image data.
///
/// Real legacy files carry corrupt scanlines and Photoshop still opens them,
/// so the image-channel path cannot reject what the strict decoder rejects: a
/// run or literal that overruns the row is clipped to it, a stream that ends
/// early leaves the rest of the row at zero, and a `128` header is a no-op.
/// Each row is positioned by its own declared size in the scanline table, so
/// a damaged row cannot desync the rows after it. [`pack_bits_decompress_into`]
/// remains the exact-length contract everywhere else (patterns, brushes and
/// the crate's own tests), where a mismatch is a genuine misparse.
pub fn pack_bits_decompress_row_lenient(data: &[u8], out: &mut [u8]) {
    let out_len = out.len();
    let mut produced = 0usize;
    let mut pos = 0usize;
    while produced < out_len {
        let Some(&header) = data.get(pos) else {
            break; // A short stream leaves the rest of the row at zero.
        };
        pos += 1;
        match header {
            0..=127 => {
                let count = (header as usize + 1).min(out_len - produced);
                match data.get(pos..pos + count) {
                    Some(chunk) => {
                        out[produced..produced + count].copy_from_slice(chunk);
                        pos += count;
                    }
                    None => {
                        // The literal runs past the stream: copy what exists.
                        let rest = &data[pos..];
                        let count = rest.len().min(out_len - produced);
                        out[produced..produced + count].copy_from_slice(&rest[..count]);
                        break;
                    }
                }
                produced += count;
            }
            128 => {}
            _ => {
                let count = (257 - header as usize).min(out_len - produced);
                let Some(&byte) = data.get(pos) else { break };
                pos += 1;
                out[produced..produced + count].fill(byte);
                produced += count;
            }
        }
    }
}

/// Compress an image channel as PSD/PSB RLE: a big-endian scanline-size table
/// (2-byte entries for PSD, 4 for PSB) followed by the PackBits data of every
/// scanline. Mirrors upstream `CompressRLE` (`Compress_RLE.h`).
///
/// `scanline_bytes` is the uncompressed length of one scanline (width ×
/// bytes-per-sample); `data.len()` must be a multiple of it.
pub fn compress_scanlines(
    data: &[u8],
    scanline_bytes: usize,
    size_width: usize,
) -> Result<Vec<u8>> {
    compress_counted_scanlines(data, &count_scanlines(data, scanline_bytes, size_width)?)
}

/// [`compress_scanlines`] with the row lengths already measured by
/// [`count_scanlines`] over the same `data`.
///
/// The output is allocated once at its exact length: the size table is
/// written from the counts, then blocks of rows pack (in parallel for large
/// channels) directly into disjoint ranges of that buffer. A row too long for
/// its size-table entry is an [`CodecError::OutputLength`] error; counts that
/// do not describe `data` are an [`CodecError::InvalidInput`] error.
pub fn compress_counted_scanlines(data: &[u8], counts: &ScanlineCounts) -> Result<Vec<u8>> {
    let scanline_bytes = counts.scanline_bytes;
    let size_width = counts.size_width;
    if data.len() != counts.data_len {
        return Err(CodecError::InvalidInput(
            "scanline counts do not describe this data",
        ));
    }
    if let Some(size) = counts.oversized_row() {
        return Err(CodecError::OutputLength {
            expected: table_entry_limit(size_width),
            actual: size,
        });
    }
    let rows = counts.sizes.len();
    let mut out = vec![0u8; counts.encoded_len];
    let (table, mut packets) = out.split_at_mut(rows * size_width);
    for (entry, &size) in table.chunks_exact_mut(size_width).zip(&counts.sizes) {
        if size_width == 2 {
            entry.copy_from_slice(&(size as u16).to_be_bytes());
        } else {
            entry.copy_from_slice(&(size as u32).to_be_bytes());
        }
    }

    // Blocks keep rayon's task count proportional to threads rather than to
    // rows (a large 8-bit document has hundreds of thousands of scanlines).
    let parallel = data.len() >= PARALLEL_MIN_BYTES;
    let block_rows = block_rows(rows, scanline_bytes, parallel);
    let mut blocks = Vec::with_capacity(rows.div_ceil(block_rows));
    for (input, sizes) in data
        .chunks(block_rows * scanline_bytes)
        .zip(counts.sizes.chunks(block_rows))
    {
        let (block, rest) = std::mem::take(&mut packets).split_at_mut(sizes.iter().sum());
        packets = rest;
        blocks.push((input, sizes, block));
    }
    let pack_block = |(input, sizes, block): (&[u8], &[usize], &mut [u8])| -> Result<()> {
        let mut offset = 0;
        for (row, &size) in input.chunks(scanline_bytes).zip(sizes) {
            pack_bits_write(row, &mut block[offset..offset + size])?;
            offset += size;
        }
        Ok(())
    };
    if parallel {
        blocks.into_par_iter().try_for_each(pack_block)?;
    } else {
        blocks.into_iter().try_for_each(pack_block)?;
    }
    Ok(out)
}

/// Rows per compression block. Sequential compression is one block. Parallel
/// compression aims for several blocks per thread so the work balances, but
/// keeps a block within about 1 MiB of input so its buffer stays small, and
/// never splits a row.
fn block_rows(rows: usize, scanline_bytes: usize, parallel: bool) -> usize {
    if !parallel {
        return rows.max(1);
    }
    const MAX_BLOCK_BYTES: usize = 1 << 20;
    let balanced = rows.div_ceil(rayon::current_num_threads() * 4);
    balanced
        .min((MAX_BLOCK_BYTES / scanline_bytes).max(1))
        .max(1)
}

/// Decompress a PSD/PSB RLE channel produced by [`compress_scanlines`].
///
/// The scanline table is structural and stays strict (a row that runs past
/// the stream, or trailing bytes after the last row, are errors), but each
/// row's content is decoded leniently — see
/// [`pack_bits_decompress_row_lenient`] — because Photoshop opens files whose
/// scanlines are damaged.
pub fn decompress_scanlines(
    data: &[u8],
    scanline_bytes: usize,
    size_width: usize,
    height: usize,
) -> Result<Vec<u8>> {
    if size_width != 2 && size_width != 4 {
        return Err(CodecError::InvalidInput(
            "scanline size width must be 2 (PSD) or 4 (PSB)",
        ));
    }
    let table_len = height
        .checked_mul(size_width)
        .ok_or(CodecError::InvalidInput("scanline table length overflows"))?;
    if data.len() < table_len {
        return Err(CodecError::InvalidInput(
            "stream is shorter than its scanline size table",
        ));
    }

    let (table, mut rest) = data.split_at(table_len);
    let total = height
        .checked_mul(scanline_bytes)
        .ok_or(CodecError::InvalidInput("decompressed size overflows"))?;
    let mut out = vec![0u8; total];

    // Walk the size table sequentially (each entry's offset depends on the
    // previous), then decode the independent scanlines in place.
    let mut scanlines = Vec::with_capacity(height);
    for y in 0..height {
        let entry = &table[y * size_width..(y + 1) * size_width];
        let size = if size_width == 2 {
            u16::from_be_bytes(entry.try_into().unwrap()) as usize
        } else {
            u32::from_be_bytes(entry.try_into().unwrap()) as usize
        };
        if rest.len() < size {
            return Err(CodecError::InvalidInput(
                "scanline data runs past the end of the stream",
            ));
        }
        let (scanline, tail) = rest.split_at(size);
        scanlines.push(scanline);
        rest = tail;
    }
    if !rest.is_empty() {
        return Err(CodecError::InvalidInput(
            "trailing bytes after the last scanline",
        ));
    }

    if total >= PARALLEL_MIN_BYTES {
        scanlines
            .into_par_iter()
            .zip(out.par_chunks_mut(scanline_bytes))
            .for_each(|(scanline, dst)| pack_bits_decompress_row_lenient(scanline, dst));
    } else {
        for (scanline, dst) in scanlines.iter().zip(out.chunks_mut(scanline_bytes)) {
            pack_bits_decompress_row_lenient(scanline, dst);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    fn reference(data: &[u8]) -> Vec<u8> {
        if data.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let start = 0;
        let mut run_len: usize = 0;
        let mut lit_len: usize = 0;

        // Mirrors the upstream loop which inspects (prev, curr) pairs.
        for i in 1..data.len() {
            let prev = data[i - 1];
            let curr = data[i];

            if prev == curr {
                // A repeat starts: flush any pending literal run first
                // (upstream flushes `data[i - nonRunLen - 1 ..= i - 2]`).
                if lit_len != 0 {
                    write_literal(&mut out, &data[i - lit_len - 1..i - 1]);
                    lit_len = 0;
                }

                run_len += 1;
                if run_len == MAX_PACKET_LEN {
                    write_run(&mut out, run_len, curr);
                    run_len = 0;
                }
            } else {
                // Run ended (or never started).
                if run_len != 0 {
                    run_len += 1;
                    write_run(&mut out, run_len, prev);
                    run_len = 0;
                } else {
                    lit_len += 1;
                }

                if lit_len == MAX_PACKET_LEN {
                    // Upstream flushes `data[i - nonRunLen ..= i - 1]` here;
                    // the current byte stays pending for the next iteration.
                    write_literal(&mut out, &data[i - lit_len..i]);
                    lit_len = 0;
                }
            }
        }

        // Flush the tail (upstream "encode the last item" epilogue).
        let n = data.len();
        if run_len != 0 {
            run_len += 1;
            write_run(&mut out, run_len, data[n - 1]);
        } else {
            lit_len += 1;
            write_literal(&mut out, &data[n - lit_len..]);
        }

        // Pad this row to 2-byte alignment with the no-op packet.
        if !(out.len() - start).is_multiple_of(2) {
            out.push(128);
        }
        out
    }

    #[test]
    fn packet_walk_and_exact_lengths_match_the_original_state_machine() {
        let mut state = 0x1234_5678u32;
        for len in [0, 1, 7, 8, 9, 31, 32, 127, 128, 129, 255, 256, 257, 4097] {
            for range in [1, 2, 3, 16, 256] {
                let data: Vec<_> = (0..len)
                    .map(|_| {
                        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                        ((state >> 24) % range) as u8
                    })
                    .collect();
                let expected = reference(&data);
                assert_eq!(
                    pack_bits_compress(&data),
                    expected,
                    "{len} bytes, range {range}"
                );
                assert_eq!(pack_bits_encoded_len(&data), expected.len());
            }
        }
        // Long runs with a one-byte remainder followed by literals or another run.
        for run in [127, 128, 129, 255, 256, 257, 4096] {
            let mut data = vec![7; run];
            data.extend_from_slice(&[8, 9, 9, 9, 10]);
            assert_eq!(pack_bits_compress(&data), reference(&data));
        }
        // Verify the byte-zero trick when a zero is followed by a one.
        assert_eq!(first_repeat(&[1, 1, 0, 2, 3, 4, 5, 6, 7]), Some(0));
    }

    use super::*;

    /// The Wikipedia PackBits example, pinned byte-for-byte by upstream
    /// `TestRLECompression.cpp` ("Test Wikipedia Example"), including the
    /// trailing `128` no-op that pads the 15-byte encoding to even.
    const WIKIPEDIA_DATA: [u8; 24] = [
        170, 170, 170, 128, 0, 42, 170, 170, 170, 170, 128, 0, 42, 34, 170, 170, 170, 170, 170,
        170, 170, 170, 170, 170,
    ];
    const WIKIPEDIA_ENCODED: [u8; 16] = [
        254, 170, 2, 128, 0, 42, 253, 170, 3, 128, 0, 42, 34, 247, 170, 128,
    ];

    #[test]
    fn wikipedia_example_compresses_byte_exact() {
        assert_eq!(pack_bits_compress(&WIKIPEDIA_DATA), WIKIPEDIA_ENCODED);
    }

    #[test]
    fn constant_scanlines_preserve_packets_at_every_run_boundary() {
        for len in [1, 2, 31, 32, 33, 127, 128, 129, 255, 256, 257, 4097] {
            let data = vec![42; len];
            let mut expected = Vec::new();
            for _ in 0..len / 128 {
                expected.extend_from_slice(&[129, 42]);
            }
            let tail = len % 128;
            if tail != 0 {
                expected.extend_from_slice(&[((257 - tail) & 255) as u8, 42]);
            }
            assert_eq!(pack_bits_compress(&data), expected, "length {len}");
            assert_eq!(pack_bits_decompress(&expected, len).unwrap(), data);
        }
    }

    #[test]
    fn wikipedia_example_decompresses_byte_exact() {
        assert_eq!(
            pack_bits_decompress(&WIKIPEDIA_ENCODED, WIKIPEDIA_DATA.len()).unwrap(),
            WIKIPEDIA_DATA
        );
    }

    /// Two identical bytes become a run packet in upstream's state machine
    /// (a literal flushes the moment any repeat begins), even though a literal
    /// would be the same size. This pins that fidelity quirk.
    #[test]
    fn two_byte_repeat_encodes_as_run() {
        // [1, 2, 2] -> literal "1" (header 0), run of two 2s (header 255),
        // then pad to even.
        assert_eq!(pack_bits_compress(&[1, 2, 2]), [0, 1, 255, 2]);
    }

    #[test]
    fn round_trips_edge_patterns() {
        // Alternating bytes: the worst case for RLE (literal packets only).
        let alternating: Vec<u8> = (0..300).map(|i| (i % 2) as u8).collect();
        assert_eq!(
            pack_bits_decompress(&pack_bits_compress(&alternating), alternating.len()).unwrap(),
            alternating
        );

        // Constant data: run packets at the 128 cap (128 + 128 + 44).
        let constant = vec![0xABu8; 300];
        assert_eq!(
            pack_bits_decompress(&pack_bits_compress(&constant), constant.len()).unwrap(),
            constant
        );

        // Single byte and two bytes.
        for data in [&[7u8][..], &[7, 8][..], &[7, 7][..]] {
            assert_eq!(
                pack_bits_decompress(&pack_bits_compress(data), data.len()).unwrap(),
                data
            );
        }

        // Empty input compresses to nothing.
        assert_eq!(pack_bits_compress(&[]), Vec::<u8>::new());
    }

    #[test]
    fn run_capped_at_128_bytes() {
        // 300 identical bytes: 128-run + 128-run + 44-run.
        let compressed = pack_bits_compress(&[0xCDu8; 300]);
        assert_eq!(compressed, [129, 0xCD, 129, 0xCD, 213, 0xCD]);
    }

    #[test]
    fn literal_capped_at_128_bytes() {
        // Strictly increasing bytes: no repeats, literal packets at the 128 cap.
        let data: Vec<u8> = (0..200u16).map(|i| i as u8).collect();
        let compressed = pack_bits_compress(&data);
        assert_eq!(compressed[0], 127); // first literal header: 128 bytes
        assert_eq!(pack_bits_decompress(&compressed, data.len()).unwrap(), data);
    }

    #[test]
    fn decompress_rejects_corrupt_streams() {
        // Stream ends early.
        assert!(matches!(
            pack_bits_decompress(&[0, 1], 10),
            Err(CodecError::InvalidPackBits(_))
        ));
        // Run packet overshoots the declared output length.
        assert!(matches!(
            pack_bits_decompress(&[200, 5], 3),
            Err(CodecError::InvalidPackBits(_))
        ));
        // Literal packet references bytes past the end.
        assert!(matches!(
            pack_bits_decompress(&[5, 1, 2], 6),
            Err(CodecError::InvalidPackBits(_))
        ));
        // Trailing non-padding garbage after the target length is reached.
        assert!(matches!(
            pack_bits_decompress(&[0, 9, 42], 1),
            Err(CodecError::InvalidPackBits(_))
        ));
        // Trailing no-op padding, however, is fine.
        assert_eq!(pack_bits_decompress(&[0, 9, 128], 1).unwrap(), [9]);
    }

    #[test]
    fn scanline_table_round_trips_both_widths() {
        // 3 scanlines of 4 bytes each: a 4-run, a 4-literal, a 4-run.
        let data: Vec<u8> = vec![1, 1, 1, 1, 2, 3, 4, 5, 9, 9, 9, 9];
        // Packed scanlines: [253,1] (2B), [3,2,3,4,5,128] (6B), [253,9] (2B).
        let compressed = compress_scanlines(&data, 4, 2).unwrap();
        assert_eq!(&compressed[..6], &[0, 2, 0, 6, 0, 2]);
        assert_eq!(compressed.len(), 6 + 2 + 6 + 2);
        assert_eq!(decompress_scanlines(&compressed, 4, 2, 3).unwrap(), data);

        let compressed = compress_scanlines(&data, 4, 4).unwrap();
        assert_eq!(&compressed[..12], &[0, 0, 0, 2, 0, 0, 0, 6, 0, 0, 0, 2]);
        assert_eq!(decompress_scanlines(&compressed, 4, 4, 3).unwrap(), data);
    }

    #[test]
    fn scanline_table_rejects_malformed_inputs() {
        assert!(matches!(
            compress_scanlines(&[1, 2, 3], 2, 2),
            Err(CodecError::InvalidInput(_))
        ));
        assert!(matches!(
            compress_scanlines(&[1, 2], 2, 3),
            Err(CodecError::InvalidInput(_))
        ));
        // Table claims a scanline longer than the remaining data.
        assert!(matches!(
            decompress_scanlines(&[0, 200, 1, 2], 4, 2, 1),
            Err(CodecError::InvalidInput(_))
        ));
        // Trailing bytes after the last scanline.
        assert!(matches!(
            decompress_scanlines(&[0, 2, 0xFF, 0x00, 0xAA], 2, 2, 1),
            Err(CodecError::InvalidInput(_))
        ));
    }

    #[test]
    fn lenient_rows_clip_overruns_and_zero_fill_short_streams() {
        // A run of 5 (header 0xFC) in a 4-byte row: clipped to the row.
        let mut out = [9u8; 4];
        pack_bits_decompress_row_lenient(&[0xFC, 0x2A], &mut out);
        assert_eq!(out, [0x2A; 4]);

        // A literal that claims 4 bytes but carries 2: copies what exists and
        // leaves the rest of the caller's (zeroed) row alone.
        let mut out = [0u8; 4];
        pack_bits_decompress_row_lenient(&[3, 1, 2], &mut out);
        assert_eq!(out, [1, 2, 0, 0]);

        // A stream that ends after one run leaves the rest of the row alone.
        let mut out = [0u8; 4];
        pack_bits_decompress_row_lenient(&[0x00, 0x11], &mut out);
        assert_eq!(out, [0x11, 0x00, 0x00, 0x00]);

        // A no-op 128 header between packets changes nothing.
        let mut out = [0u8; 4];
        pack_bits_decompress_row_lenient(&[128, 0x01, 7, 7, 0x01, 8, 8], &mut out);
        assert_eq!(out, [7, 7, 8, 8]);
    }

    #[test]
    fn large_channel_round_trip_exercises_parallel_path() {
        // 256 scanlines of 512 bytes — over PARALLEL_MIN_BYTES, so both
        // compress and decompress take the rayon path.
        const { assert!(256 * 512 > PARALLEL_MIN_BYTES) };
        let mut data = Vec::with_capacity(256 * 512);
        for y in 0..256u32 {
            for x in 0..512u32 {
                data.push((((x ^ y) % 23 == 0) as u8 * 0xAB) | ((x + y) % 7) as u8);
            }
        }
        let compressed = compress_scanlines(&data, 512, 2).unwrap();
        assert_eq!(
            decompress_scanlines(&compressed, 512, 2, 256).unwrap(),
            data
        );
        let compressed = compress_scanlines(&data, 512, 4).unwrap();
        assert_eq!(
            decompress_scanlines(&compressed, 512, 4, 256).unwrap(),
            data
        );
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;

    /// The definition `compress_scanlines` must keep: a size table, then every row
    /// packed on its own.
    fn reference(data: &[u8], scanline_bytes: usize, size_width: usize) -> Vec<u8> {
        let packed: Vec<Vec<u8>> = data
            .chunks(scanline_bytes)
            .map(pack_bits_compress)
            .collect();
        let mut out = Vec::new();
        for row in &packed {
            if size_width == 2 {
                out.extend_from_slice(&(row.len() as u16).to_be_bytes());
            } else {
                out.extend_from_slice(&(row.len() as u32).to_be_bytes());
            }
        }
        for row in &packed {
            out.extend_from_slice(row);
        }
        out
    }

    /// Rows that mix runs, literals and noise, so packed lengths vary (and are
    /// sometimes odd before padding).
    fn image(width: usize, height: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_u32;
        let mut data = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                data.push(match y % 4 {
                    0 => 7,
                    1 => (x / 5) as u8,
                    2 => (state >> 24) as u8,
                    _ => {
                        if x % 9 < 4 {
                            200
                        } else {
                            (state >> 24) as u8
                        }
                    }
                });
            }
        }
        data
    }

    #[test]
    fn into_appends_a_padded_row_and_reports_its_length() {
        let mut out = vec![0xAA, 0xBB, 0xCC]; // odd prefix: padding is per row, not per buffer
        let first = pack_bits_compress_into(&[1, 2, 3], &mut out);
        assert_eq!(first, 4); // literal header + 3 bytes = 4, already even
        let second = pack_bits_compress_into(&[9, 9, 9], &mut out);
        assert_eq!(second, 2); // one run packet
        let third = pack_bits_compress_into(&[1, 2], &mut out);
        assert_eq!(third, 4); // literal header + 2 bytes = 3, padded with 0x80
        assert_eq!(&out[..3], [0xAA, 0xBB, 0xCC]);
        assert_eq!(&out[3..], [2, 1, 2, 3, 254, 9, 1, 1, 2, 128]);
        assert_eq!(pack_bits_compress_into(&[], &mut out), 0);
        assert_eq!(out.len(), 13);
        assert_eq!(pack_bits_compress(&[1, 2]), [1, 1, 2, 128]);
    }

    #[test]
    fn blocked_compression_equals_row_by_row_at_every_shape() {
        // Sequential (small) and parallel (>= PARALLEL_MIN_BYTES) shapes, odd and
        // single-column widths, single-row images, PSD and PSB size tables.
        for (width, height) in [
            (1, 1),
            (1, 700),
            (3, 5),
            (63, 61),
            (255, 300),
            (1000, 9),
            (4099, 41),
            (600, 1000),
        ] {
            let data = image(width, height);
            for size_width in [2, 4] {
                assert_eq!(
                    compress_scanlines(&data, width, size_width).unwrap(),
                    reference(&data, width, size_width),
                    "{width}x{height}, size width {size_width}"
                );
            }
        }
    }

    #[test]
    fn a_block_never_splits_a_row() {
        assert_eq!(block_rows(0, 10, false), 1);
        assert_eq!(block_rows(5, 10, false), 5);
        for (rows, scanline_bytes) in [(1, 5_000_000), (10, 2_000_000), (100_000, 4), (7, 300_000)]
        {
            let per_block = block_rows(rows, scanline_bytes, true);
            assert!(per_block >= 1);
            // A row larger than the block budget still gets a block of its own.
            assert!(per_block * scanline_bytes <= (1 << 20).max(scanline_bytes));
        }
    }

    #[test]
    fn a_row_too_long_for_a_psd_size_entry_is_an_error() {
        // 70,000 incompressible bytes pack to more than u16::MAX.
        let noisy = image(70_000, 4).split_off(70_000 * 2);
        assert!(matches!(
            compress_scanlines(&noisy, 70_000, 2),
            Err(CodecError::OutputLength { .. })
        ));
        assert!(compress_scanlines(&noisy, 70_000, 4).is_ok());
        // Counting never rejects the row; it reports that only Raw can hold it.
        let counts = count_scanlines(&noisy, 70_000, 2).unwrap();
        assert!(!counts.fits_table());
        assert!(count_scanlines(&noisy, 70_000, 4).unwrap().fits_table());
    }

    #[test]
    fn counted_encode_writes_the_reference_layout_in_one_exact_buffer() {
        // Sequential and parallel sizes, both table widths, odd row lengths.
        for (width, height) in [(1, 1), (3, 7), (129, 33), (1000, 300), (4097, 40)] {
            let data = image(width, height);
            for size_width in [2, 4] {
                let counts = count_scanlines(&data, width, size_width).unwrap();
                let packed = compress_counted_scanlines(&data, &counts).unwrap();
                assert_eq!(packed, reference(&data, width, size_width));
                assert_eq!(packed.len(), counts.encoded_len());
                assert_eq!(packed.capacity(), packed.len());
                assert_eq!(
                    encoded_scanlines_len(&data, width, size_width).unwrap(),
                    packed.len()
                );
            }
        }
        assert!(compress_scanlines(&[], 4, 2).unwrap().is_empty());
    }

    #[test]
    fn counts_from_other_data_are_rejected_instead_of_misplacing_rows() {
        let data = image(300, 12);
        let counts = count_scanlines(&data, 300, 2).unwrap();
        // Same length, different content: row lengths no longer match.
        let other = vec![9u8; data.len()];
        assert!(matches!(
            compress_counted_scanlines(&other, &counts),
            Err(CodecError::InvalidInput(_))
        ));
        assert!(matches!(
            compress_counted_scanlines(&data[..300], &counts),
            Err(CodecError::InvalidInput(_))
        ));
    }
}
