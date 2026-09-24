//! The streaming decoder's memory bounds and its size ceiling, on PNGs built by hand.
//!
//! Every image here is black, and every stream is written by a small fixed-Huffman encoder
//! below rather than by the crate's own, so each test controls exactly where the matches
//! fall — which is what the stage's sizing depends on — and none depends on generated
//! fixtures being present.

use psd_png::adler32::adler32;
use psd_png::crc32::crc32;
use psd_png::tables::{LEN_BASE, LEN_EXTRA};
use psd_png::{Decoder, Error, Row};

/// A DEFLATE bit writer: fields least significant bit first, Huffman codes most
/// significant bit first, as RFC 1951 lays them out.
struct Bits {
    out: Vec<u8>,
    acc: u64,
    count: u32,
}

impl Bits {
    fn new() -> Self {
        Self { out: Vec::new(), acc: 0, count: 0 }
    }

    fn put(&mut self, value: u32, bits: u32) {
        self.acc |= u64::from(value) << self.count;
        self.count += bits;
        while self.count >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.count -= 8;
        }
    }

    fn code(&mut self, code: u32, bits: u32) {
        let reversed = code.reverse_bits() >> (32 - bits);
        self.put(reversed, bits);
    }

    /// A zero byte, in the fixed literal/length code.
    fn zero(&mut self) {
        self.code(0x30, 8);
    }

    /// A run of `length` bytes copying the previous byte: distance one.
    fn run(&mut self, length: usize) {
        let index = LEN_BASE.iter().rposition(|&base| usize::from(base) <= length).unwrap();
        let symbol = 257 + index as u32;
        if symbol <= 279 {
            self.code(symbol - 256, 7);
        } else {
            self.code(0xC0 + symbol - 280, 8);
        }
        self.put((length - usize::from(LEN_BASE[index])) as u32, u32::from(LEN_EXTRA[index]));
        self.code(0, 5);
    }

    fn finish(mut self) -> Vec<u8> {
        self.code(0, 7);
        if self.count > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// A zlib stream of `total` zero bytes: `lead` literals, then runs of the longest match
/// DEFLATE has, 258 bytes. Every match after the literals therefore ends at
/// `lead + 258 * k`, so sweeping `lead` over 258 values puts a match end on every
/// position a segment could pause at.
fn zeros(total: usize, lead: usize) -> Vec<u8> {
    let mut bits = Bits::new();
    bits.put(1, 1); // BFINAL
    bits.put(1, 2); // BTYPE = fixed Huffman
    let lead = lead.clamp(1, total);
    for _ in 0..lead {
        bits.zero();
    }
    let mut remaining = total - lead;
    while remaining > 0 {
        let length = remaining.min(258);
        if length >= 3 {
            bits.run(length);
        } else {
            for _ in 0..length {
                bits.zero();
            }
        }
        remaining -= length;
    }

    let mut stream = vec![0x78, 0x01];
    stream.extend(bits.finish());
    stream.extend(adler32(&vec![0u8; total]).to_be_bytes());
    stream
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend(kind);
    png.extend(data);
    let crc = crc32(&png[start..]);
    png.extend(crc.to_be_bytes());
}

/// A PNG with the given header fields and one `IDAT` holding `idat`.
fn png(
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
    idat: &[u8],
) -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([bit_depth, color_type, 0, 0, interlace]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", idat);
    chunk(&mut png, b"IEND", &[]);
    png
}

/// A black image whose zero filtered stream ends a match at every pause position as
/// `lead` sweeps a full match length.
fn black(width: u32, height: u32, bit_depth: u8, color_type: u8, lead: usize) -> Vec<u8> {
    let header = psd_png::Info::new(
        width,
        height,
        match color_type {
            0 => psd_png::ColorType::Grayscale,
            6 => psd_png::ColorType::Rgba,
            other => panic!("colour type {other} is not used here"),
        },
        psd_png::BitDepth::from_byte(bit_depth).unwrap(),
    );
    png(width, height, bit_depth, color_type, 0, &zeros(header.decompressed_size(), lead))
}

/// Narrow rows are where the stage's headroom is tightest: it keeps four rows beyond the
/// match window, and a narrow row is a few bytes. If the frontier is left a whole match
/// behind when a segment pauses, the slide keeps more than that, and the decoder once
/// panicked here on valid files that `decode` read without complaint. Sweeping the lead
/// puts a match end exactly on the pause for every width.
#[test]
fn narrow_images_stream_wherever_a_match_ends() {
    // (width, bit depth, colour type): grey rows of 2 and 4 bytes, RGBA rows of 33.
    for (width, depth, color) in [(1u32, 8u8, 0u8), (3, 8, 0), (8, 8, 6)] {
        let pitch = 1 + width as usize * if color == 6 { 4 } else { 1 };
        // A little past one full stage, so the first pause is followed by a slide.
        let height = (360_000 / pitch) as u32;
        for lead in 1..=258 {
            let png = black(width, height, depth, color, lead);
            let mut rows = 0usize;
            Decoder::new()
                .decode_to(&png, |row: Row<'_>| {
                    assert_eq!(row.index, rows);
                    assert!(row.bytes.iter().all(|&b| b == 0), "row {} is not black", row.index);
                    rows += 1;
                    Ok::<(), Error>(())
                })
                .unwrap_or_else(|e| panic!("width {width}, lead {lead}: {e}"));
            assert_eq!(rows, height as usize, "width {width}, lead {lead}");
        }
    }
}

/// An interlaced image streams from a whole-image buffer, so it is held to the ceiling
/// exactly as `decode` holds it — native and converted alike.
#[test]
fn interlaced_streaming_is_held_to_the_ceiling() {
    let header = {
        let mut info =
            psd_png::Info::new(63, 40, psd_png::ColorType::Rgba, psd_png::BitDepth::Eight);
        info.interlacing = psd_png::Interlacing::Adam7;
        info
    };
    let size = header.decompressed_size();
    let png = png(63, 40, 8, 6, 1, &zeros(size, 1));

    let mut decoder = Decoder::new();
    decoder.max_decompressed_size(Some(1000));
    let expected = Error::SizeLimitExceeded { size, limit: 1000 };
    assert_eq!(decoder.decode(&png).unwrap_err(), expected);
    assert_eq!(decoder.decode_to(&png, |_: Row<'_>| Ok::<(), Error>(())), Err(expected.clone()));
    assert_eq!(decoder.decode_to_rgba8(&png, |_: Row<'_>| Ok::<(), Error>(())), Err(expected));

    // Under the default ceiling the same file streams.
    let mut rows = 0;
    Decoder::new()
        .decode_to(&png, |row: Row<'_>| {
            assert!(row.bytes.iter().all(|&b| b == 0));
            rows += 1;
            Ok::<(), Error>(())
        })
        .unwrap();
    assert_eq!(rows, 40);
}

/// The stage grows with the row width, so a header naming rows gigabytes wide is refused
/// before anything is allocated — the image data is never looked at.
#[test]
fn rows_too_wide_for_the_ceiling_are_refused() {
    // 100 million RGBA16 pixels a row: 800 MB, one row high.
    let png = png(100_000_000, 1, 16, 6, 0, &[0x78, 0x01]);
    for result in [
        Decoder::new().decode_to(&png, |_: Row<'_>| Ok::<(), Error>(())),
        Decoder::new().decode_to_rgba16(&png, |_: Row<'_>| Ok::<(), Error>(())),
    ] {
        match result {
            Err(Error::SizeLimitExceeded { size, limit }) => {
                assert_eq!(limit, psd_png::DEFAULT_MAX_DECOMPRESSED_SIZE);
                assert!(size > 800_000_000, "the stage holds several rows, got {size}");
            }
            other => panic!("expected the ceiling to refuse the stage, got {other:?}"),
        }
    }
}

/// A converting decode also holds one converted row, which can be far wider than the file's
/// own: a 1-bit grey row becomes sixty-four times the bytes as RGBA16. That row is held to
/// the ceiling too, while the native stream of the same file is not refused.
#[test]
fn a_converted_row_is_held_to_the_ceiling() {
    let (width, height) = (100_000u32, 2u32);
    let header =
        psd_png::Info::new(width, height, psd_png::ColorType::Grayscale, psd_png::BitDepth::One);
    let png = png(width, height, 1, 0, 0, &zeros(header.decompressed_size(), 1));
    let limit = 500_000;

    let mut decoder = Decoder::new();
    decoder.max_decompressed_size(Some(limit));
    let mut rows = 0;
    decoder
        .decode_to(&png, |_: Row<'_>| {
            rows += 1;
            Ok::<(), Error>(())
        })
        .expect("the native stage fits the ceiling");
    assert_eq!(rows, height as usize);

    let converted = width as usize * 4 * 2;
    assert_eq!(
        decoder.decode_to_rgba16(&png, |_: Row<'_>| Ok::<(), Error>(())),
        Err(Error::SizeLimitExceeded { size: converted, limit })
    );
}
