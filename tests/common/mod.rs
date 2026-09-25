//! Builds small PNG files by hand, with every scanline stored uncompressed.
//!
//! The encoder is gone from the crate, so the decode tests build their fixtures directly:
//! a zlib stream whose blocks are all *stored* needs no compressor — the stream is the zlib
//! header, then for each block a type byte, a little-endian length and its one's complement,
//! then the raw bytes — closed with the Adler-32 of the data. Every scanline carries filter
//! byte 0 (`None`), which is the forward filter the block already is.
//!
//! Each test binary that includes this module sees only the helpers it names, so the ones
//! it does not use are annotated rather than removed.
#![allow(dead_code)]

use psd_png::adler32;
use psd_png::common::{Info, SIGNATURE};
use psd_png::crc32;

/// CRC-32 over a PNG chunk's type and payload, from the crate's own table.
fn chunk_crc(kind: &[u8; 4], payload: &[u8]) -> [u8; 4] {
    let mut hasher = crc32::Crc32::new();
    hasher.update(kind);
    hasher.update(payload);
    hasher.finish().to_be_bytes()
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    png.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(payload);
    png.extend_from_slice(&chunk_crc(kind, payload));
}

/// Builds a zlib stream holding data in stored blocks, each at most limit bytes.
pub fn stored_zlib(data: &[u8], limit: usize) -> Vec<u8> {
    assert!(limit <= 65_535, "a stored block may not exceed 65_535 bytes");
    let mut out = vec![0x78, 0x01];
    if data.is_empty() {
        // One empty final block; both length words read zero.
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut rest = data;
        while !rest.is_empty() {
            let (block, tail) = rest.split_at(limit.min(rest.len()));
            out.push(if tail.is_empty() { 0x01 } else { 0x00 });
            let len = block.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(block);
            rest = tail;
        }
    }
    let adler = adler32::adler32(data);
    out.extend_from_slice(&adler32_to_be(adler));
    out
}

fn adler32_to_be(value: u32) -> [u8; 4] {
    value.to_be_bytes()
}

/// The shape the deflate round-trip tests used when the crate still had its compressor:
/// one call, one stream. Stored blocks fill the same contract.
pub fn compress_zlib(data: &[u8]) -> Vec<u8> {
    stored_zlib(data, u16::MAX as usize)
}

/// Builds a complete PNG file for `info`, with `pixels` as its unfiltered native-format
/// raster and `idat_chunks` `IDAT` chunks carrying the stream between them.
pub fn build_png(info: &Info, pixels: &[u8], idat_chunks: usize) -> Vec<u8> {
    assert!(idat_chunks >= 1, "a PNG needs at least one IDAT");

    // The filtered stream: each scanline with its filter byte 0 in front of it.
    let row_bytes = info.row_bytes();
    let height = info.height as usize;
    let mut filtered = Vec::with_capacity(height * (1 + row_bytes));
    let mut rest = pixels;
    for _ in 0..height {
        filtered.push(0u8);
        filtered.extend_from_slice(&rest[..row_bytes]);
        rest = &rest[row_bytes..];
    }

    let mut png = SIGNATURE.to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&info.width.to_be_bytes());
    ihdr.extend_from_slice(&info.height.to_be_bytes());
    ihdr.push(info.bit_depth as u8);
    ihdr.push(info.color_type as u8);
    ihdr.extend_from_slice(&[0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    if let Some(palette) = &info.palette {
        chunk(&mut png, b"PLTE", palette);
    }
    if let Some(transparency) = &info.transparency {
        chunk(&mut png, b"tRNS", transparency);
    }

    let zlib = stored_zlib(&filtered, u16::MAX as usize);
    // Spread the stream across the requested number of IDAT chunks.
    let per_chunk = zlib.len().div_ceil(idat_chunks);
    let mut rest = zlib.as_slice();
    while !rest.is_empty() {
        let (payload, tail) = rest.split_at(per_chunk.min(rest.len()));
        chunk(&mut png, b"IDAT", payload);
        rest = tail;
    }
    chunk(&mut png, b"IEND", &[]);
    png
}
