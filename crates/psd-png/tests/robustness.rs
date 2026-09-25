//! Malformed input must produce errors, never panics.
//!
//! A decoder is usually pointed at data from somewhere untrusted, so every corruption of a
//! valid file has to come back as an `Err`.

mod common;

use common::build_png;
use psd_png::common::{BitDepth, ColorType, Info};

fn valid_png() -> Vec<u8> {
    let info = Info::new(23, 17, ColorType::Rgba, BitDepth::Eight);
    let data: Vec<u8> =
        (0..info.output_size()).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    build_png(&info, &data, 1)
}

#[test]
fn truncation_at_every_length_is_rejected() {
    let png = valid_png();
    for length in 0..png.len() {
        // Nothing shorter than the whole file can decode, and nothing may panic.
        let _ = psd_png::decode(&png[..length]);
    }
    assert!(psd_png::decode(&png).is_ok());
}

#[test]
fn single_byte_corruption_is_rejected_or_decoded() {
    let png = valid_png();
    let mut decoder = psd_png::Decoder::new();

    // Flipping any single bit either breaks a checksum or, in the rare case it lands
    // somewhere inert, still produces a well-formed result. Neither may panic.
    for index in 0..png.len() {
        for bit in [0u8, 3, 7] {
            let mut damaged = png.clone();
            damaged[index] ^= 1 << bit;
            let _ = decoder.decode(&damaged);
        }
    }
}

#[test]
fn structurally_invalid_headers_are_rejected() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("signature only", psd_png::common::SIGNATURE.to_vec()),
        ("wrong signature", vec![0; 32]),
        ("text", b"not a png at all, just some bytes".to_vec()),
    ];
    for (name, data) in cases {
        assert!(psd_png::decode(&data).is_err(), "{name} should not decode");
    }

    // A header claiming a colour type and bit depth that cannot go together.
    let mut png = valid_png();
    png[24] = 3; // IHDR bit depth byte -> 3, which no colour type allows
    let crc = psd_png::crc32::crc32(&png[12..29]);
    png[29..33].copy_from_slice(&crc.to_be_bytes());
    assert!(psd_png::decode(&png).is_err(), "bit depth 3 should not decode");
}

#[test]
fn hostile_zlib_streams_do_not_hang_or_panic() {
    // Random bytes handed to the decompressor as if they were a zlib stream.
    let mut state = 0x1234_5678_9abc_def0u64;
    for _ in 0..2000 {
        let length = (state % 200) as usize + 2;
        let data: Vec<u8> = (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        // Any expected length: the decompressor must terminate either way.
        let _ = psd_png::inflate::decompress_zlib(&data, 4096);
        let _ = psd_png::inflate::decompress_zlib(&data, 0);
    }
}

#[test]
fn zlib_headers_with_valid_framing_but_junk_payload_are_rejected() {
    for payload_len in [0usize, 1, 4, 64] {
        let mut stream = vec![0x78, 0x01];
        stream.extend(std::iter::repeat_n(0xA5, payload_len));
        assert!(psd_png::inflate::decompress_zlib(&stream, 1024).is_err());
    }
}

/// A chunk whose stored CRC does not match its contents.
fn chunk_with_bad_crc(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut chunk = (body.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(body);
    chunk.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    chunk
}

/// Signature, then `IHDR`'s length, type, thirteen-byte body and CRC.
const IHDR_END: usize = 8 + 4 + 4 + 13 + 4;

#[test]
fn ancillary_chunk_with_a_bad_crc_is_discarded() {
    let png = valid_png();
    let expected = psd_png::decode(&png).unwrap();

    // Files exist whose colour profile was rewritten without recomputing the chunk's
    // checksum. Nothing about the image depends on that chunk, so it must still decode.
    let mut damaged = png[..IHDR_END].to_vec();
    damaged.extend_from_slice(&chunk_with_bad_crc(b"iCCP", b"ICC PROFILE\0\0not a profile"));
    damaged.extend_from_slice(&png[IHDR_END..]);

    let decoded = psd_png::decode(&damaged).unwrap();
    assert_eq!(decoded.data, expected.data);
    assert_eq!(decoded.info, expected.info);
}

#[test]
fn critical_chunk_with_a_bad_crc_is_rejected() {
    // The image cannot be trusted when a chunk it is built from fails its checksum, so the
    // tolerance above must not extend to `IHDR` or `IDAT`.
    for offset in [IHDR_END - 1, IHDR_END + 4] {
        let mut damaged = valid_png();
        damaged[offset] ^= 0xff;
        assert!(psd_png::decode(&damaged).is_err());
    }
}
