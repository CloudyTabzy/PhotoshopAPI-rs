//! Decompressing a zlib stream whose expanded length is not known in advance.
//!
//! `decompress_zlib` is shaped for PNG, where `IHDR` states the length. A caller without
//! that number must not invent one by trusting a length field in an untrusted file, which
//! is the same instruction to over-allocate wearing different clothes.

mod common;

use common::compress_zlib;
use psd_png::inflate::{InflateError, decompress_zlib_to_vec};

/// Something that compresses, at a few different scales relative to the first guess.
fn sample(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i / 7 % 251) as u8).collect()
}

#[test]
fn a_stream_round_trips_without_being_told_its_length() {
    for len in [0usize, 1, 100, 5_000, 300_000, 3_000_000] {
        let compressed = compress_zlib(&sample(len));
        let decoded = decompress_zlib_to_vec(&compressed, 8 << 20)
            .unwrap_or_else(|e| panic!("{len} bytes: {e}"));
        assert_eq!(decoded, sample(len), "{len} bytes round-tripped incorrectly");
    }
}

#[test]
fn the_ceiling_is_a_ceiling() {
    let compressed = compress_zlib(&sample(100_000));

    // Exactly the right size is enough, and it is the smallest size that is.
    assert_eq!(decompress_zlib_to_vec(&compressed, 100_000).unwrap().len(), 100_000);
    assert_eq!(
        decompress_zlib_to_vec(&compressed, 99_999),
        Err(InflateError::OutputOverflow),
        "a stream that does not fit under the ceiling must be refused, not truncated"
    );
    assert_eq!(decompress_zlib_to_vec(&compressed, 0), Err(InflateError::OutputOverflow));
}

/// A fixed-Huffman stream that decodes to eight megabytes of zeros.
///
/// One block: 254 literal zeros, then 32,513 matches of (length 258, distance 1), which is
/// 254 + 32,513 × 258 = 8 MiB exactly. Huffman codes go into the stream MSB-first, the
/// block header LSB-first, which is why the two pushes differ.
///
/// ```text
/// literal 0   fixed code 00110000, 8 bits
/// length 258  fixed code 11000101, 8 bits, no extra bits
/// distance 1  fixed code 00000,    5 bits, no extra bits
/// ```
fn eight_megabytes_of_zeros() -> Vec<u8> {
    const LITERAL_ZERO: u32 = 0b0011_0000;
    const LENGTH_258: u32 = 0b1100_0101;
    const DISTANCE_1: u32 = 0;

    struct Bits {
        byte: u8,
        used: u32,
        out: Vec<u8>,
    }

    impl Bits {
        /// Adds `count` bits of `value`, least-significant bit first: the order every
        /// non-Huffman field is written in.
        fn push(&mut self, value: u32, count: u32) {
            for i in 0..count {
                if (value >> i) & 1 != 0 {
                    self.byte |= 1 << self.used;
                }
                self.used += 1;
                if self.used == 8 {
                    self.out.push(self.byte);
                    self.byte = 0;
                    self.used = 0;
                }
            }
        }

        /// Adds a Huffman code, most significant bit of the code first.
        fn push_code(&mut self, code: u32, count: u32) {
            for i in (0..count).rev() {
                if (code >> i) & 1 != 0 {
                    self.byte |= 1 << self.used;
                }
                self.used += 1;
                if self.used == 8 {
                    self.out.push(self.byte);
                    self.byte = 0;
                    self.used = 0;
                }
            }
        }

        fn flush(mut self) -> Vec<u8> {
            if self.used > 0 {
                self.out.push(self.byte);
            }
            self.out
        }
    }

    let mut bits = Bits { byte: 0, used: 0, out: Vec::new() };
    bits.push(1, 1); // BFINAL
    bits.push(1, 2); // BTYPE = fixed Huffman
    for _ in 0..254 {
        bits.push_code(LITERAL_ZERO, 8);
    }
    for _ in 0..32_513 {
        bits.push_code(LENGTH_258, 8);
        bits.push_code(DISTANCE_1, 5);
    }
    // End of block: symbol 256, the seven-bit all-zero code.
    bits.push_code(0, 7);

    // zlib header, the block, then the Adler-32 of eight megabytes of zero bytes.
    let mut stream = vec![0x78, 0x01];
    stream.extend(bits.flush());
    stream.extend_from_slice(&0x0780_0001u32.to_be_bytes());
    stream
}

#[test]
fn a_highly_compressible_stream_cannot_talk_its_way_past_the_ceiling() {
    // About sixteen kilobytes of stream expanding to eight megabytes: the case a guess
    // drawn from the compressed size gets wrong, and the case where the ceiling has to be
    // the thing that holds.
    let compressed = eight_megabytes_of_zeros();
    assert!(compressed.len() < 64 * 1024, "the premise: this compresses hard");

    assert_eq!(decompress_zlib_to_vec(&compressed, 1 << 20), Err(InflateError::OutputOverflow));
    assert_eq!(decompress_zlib_to_vec(&compressed, 8 << 20).unwrap(), vec![0u8; 8 << 20]);
}

#[test]
fn corruption_is_still_caught_without_a_length_to_check_against() {
    // The argument for not carrying a length field at all: the zlib framing already covers
    // the data with an Adler-32, so a truncated or altered stream is caught regardless.
    let compressed = compress_zlib(&sample(50_000));

    for cut in [2usize, compressed.len() / 2, compressed.len() - 5] {
        assert!(
            decompress_zlib_to_vec(&compressed[..cut], 1 << 20).is_err(),
            "truncation at {cut} must not pass"
        );
    }

    let mut altered = compressed.clone();
    let middle = altered.len() / 2;
    altered[middle] ^= 0xFF;
    assert!(decompress_zlib_to_vec(&altered, 1 << 20).is_err());
}

#[test]
fn hostile_streams_terminate() {
    // Every payload carries a valid zlib header, so each iteration reaches the decompressor
    // and the retry loop rather than being turned away two bytes in. Random bytes clear that
    // header check about one time in a thousand, which is why they are not left to chance.
    let mut state = 0x0f1e_2d3c_4b5a_6978u64;
    for _ in 0..2000 {
        let length = (state % 200) as usize + 2;
        let mut data = vec![0x78u8, 0x9c];
        data.extend((0..length).map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        }));
        let _ = decompress_zlib_to_vec(&data, 1 << 20);
    }
}

#[test]
fn an_empty_stream_needs_no_room_at_all() {
    // The corner of the ceiling: a stream expanding to nothing fits under a ceiling of
    // nothing, so a zero `max_output` is not by itself a refusal.
    let compressed = compress_zlib(b"");
    assert_eq!(decompress_zlib_to_vec(&compressed, 0).unwrap(), Vec::<u8>::new());
}
