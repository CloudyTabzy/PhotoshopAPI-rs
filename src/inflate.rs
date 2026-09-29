//! DEFLATE decompression (RFC 1951), with the zlib framing (RFC 1950) that PNG's `IDAT`
//! stream uses.
//!
//! The decompressor is deliberately *not* a resumable state machine. PNG tells us the exact
//! size of the decompressed data up front, so the whole stream can be decoded in one call
//! against one output buffer. That removes the per-iteration state checks and output
//! clamping a streaming decoder needs, and lets the hot loop keep the bit buffer, the output
//! cursor, and the table references in registers.
//!
//! A caller who does not know the size in advance is served by
//! [`decompress_zlib_to_vec`], which pays for that choice by decoding again into a larger
//! buffer rather than resuming into one. The match window *is* the output buffer, so there
//! is no smaller piece of state that could be carried across a reallocation.

use crate::adler32::Adler32;
use crate::common::zeroed_vec;
use crate::tables::{
    CLCL_ORDER, DIST_BASE, DIST_EXTRA, FIXED_DIST_LENGTHS, FIXED_LITLEN_LENGTHS, LEN_BASE,
    LEN_EXTRA,
};

/// Extra bytes the caller must leave at the end of the output buffer.
///
/// Match copies are performed 16 bytes at a time regardless of the match length, so the last
/// copy of a block may write up to 15 bytes past the logical end of the data. Those bytes are
/// scratch and are never part of the result.
pub const OUTPUT_SLACK: usize = 16;

/// An observer for inflate's output cursor, used by the PNG decoder to reverse scanline
/// filters while inflation is still running.
///
/// The hook is called once per decode-loop iteration with the output buffer and the
/// position the cursor has reached. Implementations decide for themselves whether a call
/// does work; the contract is only that the call happens at iteration boundaries, where no
/// match copy or literal store is in flight.
///
/// `ENABLED` exists so the hot loop can be shared between callers that observe progress
/// and callers that do not: for [`()`] it is `false`, the calls compile out, and the loop
/// is byte-for-byte the one a hook-free decompressor would run.
pub(crate) trait ProgressHook {
    /// Whether this hook ever does work. `false` removes the call sites entirely.
    const ENABLED: bool;
    fn on_progress(&mut self, output: &mut [u8], pos: usize);
}

impl ProgressHook for () {
    const ENABLED: bool = false;
    fn on_progress(&mut self, _: &mut [u8], _: usize) {}
}

/// Hashes each region of the output **before** the hook behind it can rewrite it.
///
/// The Adler-32 in a zlib stream covers the filtered bytes the decompressor produces. A
/// hook that reverses scanline filters does so in place, so once inflation returns, the
/// buffer is part reconstructed and part not — and the trailer no longer describes it. This
/// wrapper hashes inside the same call that triggers reconstruction, which is the only
/// order that sees the bytes the checksum was computed over.
///
/// `ENABLED` is unconditionally `true`: this type exists only when verification is on, and
/// the hashing is the point, so the call sites must not compile out.
pub(crate) struct ChecksumHook<'a, H> {
    inner: H,
    checksum: &'a mut Adler32,
    hashed: usize,
}

impl<'a, H: ProgressHook> ChecksumHook<'a, H> {
    /// Wraps `inner`, resuming from `hashed`: the position in the output buffer up to which
    /// the caller has already accounted for these bytes.
    pub(crate) fn new(inner: H, checksum: &'a mut Adler32, hashed: usize) -> Self {
        Self { inner, checksum, hashed }
    }

    /// Hashes what the hook has not seen: the tail after its final call.
    ///
    /// Those bytes are still filtered — the hook reconstructs a region only from inside a
    /// call, and no call happens for the tail until the caller asks for one.
    pub(crate) fn hash_rest(&mut self, output: &[u8], written: usize) {
        self.checksum.update(&output[self.hashed..written]);
        self.hashed = written;
    }

    /// Hands the wrapped hook back, for a caller that reuses it across segments.
    pub(crate) fn into_inner(self) -> H {
        self.inner
    }
}

/// Lets a wrapper own a `&mut` hook as its inner, so a caller can pass either form.
impl<H: ProgressHook + ?Sized> ProgressHook for &mut H {
    const ENABLED: bool = H::ENABLED;

    fn on_progress(&mut self, output: &mut [u8], pos: usize) {
        (**self).on_progress(output, pos);
    }
}

impl<H: ProgressHook> ProgressHook for ChecksumHook<'_, H> {
    const ENABLED: bool = true;

    fn on_progress(&mut self, output: &mut [u8], pos: usize) {
        self.checksum.update(&output[self.hashed..pos]);
        self.hashed = pos;
        self.inner.on_progress(output, pos);
    }
}

/// Non-exhaustive, for the reason [`Error`](crate::Error) is. Match with a fallback arm.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflateError {
    /// The two-byte zlib header is not a valid PNG-compatible header.
    BadZlibHeader,
    /// A zlib stream requested a preset dictionary, which PNG does not permit.
    PresetDictionary,
    /// The input ended in the middle of the stream.
    UnexpectedEof,
    /// A block header used the reserved block type.
    InvalidBlockType,
    /// A stored block's length and its complement disagree.
    InvalidStoredLength,
    /// A dynamic block declared more literal/length codes than DEFLATE allows.
    InvalidHlit,
    /// A dynamic block declared more distance codes than DEFLATE allows.
    InvalidHdist,
    /// A code length repeat ran before the first length or past the last one.
    InvalidCodeLengthRepeat,
    /// The code length code lengths do not form a complete Huffman tree.
    BadCodeLengthTree,
    /// The literal/length code lengths do not form a complete Huffman tree.
    BadLiteralLengthTree,
    /// The distance code lengths do not form a valid Huffman tree.
    BadDistanceTree,
    /// A literal/length symbol outside the declared alphabet was decoded.
    InvalidLiteralLengthCode,
    /// A distance symbol outside the declared alphabet was decoded.
    InvalidDistanceCode,
    /// A match referred further back than the start of the output.
    DistanceTooFarBack,
    /// The stream decoded to more bytes than the caller said to expect.
    OutputOverflow,
    /// The stream decoded to fewer bytes than the caller said to expect.
    OutputUnderflow,
    /// The trailing Adler-32 checksum does not match the decompressed data.
    WrongChecksum,
    /// The allocator could not provide an output buffer of the requested size.
    OutOfMemory {
        /// Size of the allocation the allocator refused.
        bytes: usize,
    },
}

impl core::fmt::Display for InflateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let message = match self {
            Self::BadZlibHeader => "invalid zlib header",
            Self::PresetDictionary => "zlib stream requires a preset dictionary",
            Self::UnexpectedEof => "compressed stream ended unexpectedly",
            Self::InvalidBlockType => "reserved deflate block type",
            Self::InvalidStoredLength => "stored block length does not match its complement",
            Self::InvalidHlit => "too many literal/length codes",
            Self::InvalidHdist => "too many distance codes",
            Self::InvalidCodeLengthRepeat => "code length repeat out of range",
            Self::BadCodeLengthTree => "invalid code length huffman tree",
            Self::BadLiteralLengthTree => "invalid literal/length huffman tree",
            Self::BadDistanceTree => "invalid distance huffman tree",
            Self::InvalidLiteralLengthCode => "invalid literal/length code",
            Self::InvalidDistanceCode => "invalid distance code",
            Self::DistanceTooFarBack => "match distance points before the start of the output",
            Self::OutputOverflow => "compressed stream expands to more data than expected",
            Self::OutputUnderflow => "compressed stream expands to less data than expected",
            Self::WrongChecksum => "adler-32 checksum mismatch",
            Self::OutOfMemory { bytes } => {
                return write!(f, "could not allocate {bytes} bytes of output");
            }
        };
        f.write_str(message)
    }
}

impl std::error::Error for InflateError {}

// ---------------------------------------------------------------------------------------
// Decoding table layout
// ---------------------------------------------------------------------------------------
//
// Both Huffman codes are decoded through a direct-indexed primary table plus, for codes too
// long to fit, a secondary table. Each primary entry is a `u32`:
//
//   bits  0..8   number of code bits this entry consumes
//   bits  8..12  literal count (1 or 2) for literal entries, extra-bit count otherwise
//   bit   12     FLAG_INVALID      symbol is not usable in this alphabet
//   bit   13     FLAG_SECONDARY    payload is an offset into the secondary table
//   bit   14     FLAG_EXCEPTIONAL  entry needs the slow path (end of block, or secondary)
//   bit   15     FLAG_LITERAL      entry yields literal bytes directly
//   bits 16..32  payload: literal bytes, length/distance base, or secondary table offset
//
// Secondary entries are `u16`: the symbol in bits 4..16 and the total code length in bits
// 0..4.

const LITLEN_TABLE_BITS: u32 = 12;
const LITLEN_TABLE_SIZE: usize = 1 << LITLEN_TABLE_BITS;
const DIST_TABLE_BITS: u32 = 9;
const DIST_TABLE_SIZE: usize = 1 << DIST_TABLE_BITS;
const CLCL_TABLE_BITS: u32 = 7;
const CLCL_TABLE_SIZE: usize = 1 << CLCL_TABLE_BITS;

/// Mask applied to an entry when it is used directly as a shift amount.
///
/// A code length never exceeds 15, so the low six bits of an entry are exactly its length.
/// Masking with 63 rather than 255 matches what the shift instruction already does on every
/// target of interest, so the mask folds away and the table load feeds the next shift with
/// nothing in between.
const SHIFT_MASK: u32 = 0x3f;

const FLAG_INVALID: u32 = 0x1000;
const FLAG_SECONDARY: u32 = 0x2000;
const FLAG_EXCEPTIONAL: u32 = 0x4000;
const FLAG_LITERAL: u32 = 0x8000;

/// Payloads for the literal/length alphabet, indexed by symbol.
static LITLEN_ENTRIES: [u32; 288] = {
    let mut entries = [0u32; 288];
    let mut sym = 0;
    while sym < 256 {
        entries[sym] = (sym as u32) << 16 | (1 << 8) | FLAG_LITERAL;
        sym += 1;
    }
    entries[256] = FLAG_EXCEPTIONAL;
    sym += 1;
    while sym <= 285 {
        entries[sym] = (LEN_BASE[sym - 257] as u32) << 16 | (LEN_EXTRA[sym - 257] as u32) << 8;
        sym += 1;
    }
    // 286 and 287 are part of the fixed code's bit assignment but may never be emitted.
    entries[286] = FLAG_EXCEPTIONAL | FLAG_INVALID;
    entries[287] = FLAG_EXCEPTIONAL | FLAG_INVALID;
    entries
};

/// Payloads for the distance alphabet, indexed by symbol.
static DIST_ENTRIES: [u32; 32] = {
    let mut entries = [0u32; 32];
    let mut sym = 0;
    while sym < 30 {
        entries[sym] = (DIST_BASE[sym] as u32) << 16 | (DIST_EXTRA[sym] as u32) << 8 | FLAG_LITERAL;
        sym += 1;
    }
    // 30 and 31 are likewise unusable; leaving them without FLAG_LITERAL makes the decoder
    // reject them.
    entries
};

// ---------------------------------------------------------------------------------------
// Bit reader
// ---------------------------------------------------------------------------------------

/// Little-endian bit reader over a complete input buffer.
///
/// The buffer always holds at least 57 valid bits after a refill, which is more than the 48
/// bits the longest literal/length + distance pair can consume. That lets the decode loop
/// refill once per symbol pair and never check bit availability in between.
#[derive(Clone, Copy)]
struct BitReader<'a> {
    input: &'a [u8],
    /// Number of input bytes already shifted into `buf`.
    pos: usize,
    buf: u64,
    nbits: u32,
    /// Zero bits synthesized past the end of `input` that are currently inside `buf`.
    ///
    /// `padding - nbits` never decreases once the input is exhausted, so comparing the two
    /// at the end of the stream is enough to detect that a symbol read past the real data.
    padding: u32,
}

impl<'a> BitReader<'a> {
    #[inline]
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0, buf: 0, nbits: 0, padding: 0 }
    }

    #[inline(always)]
    fn refill(&mut self) {
        if self.pos + 8 <= self.input.len() {
            let word = u64::from_le_bytes(self.input[self.pos..self.pos + 8].try_into().unwrap());
            self.buf |= word << self.nbits;
            // Only the low `64 - nbits` bits of `word` made it in; advance by that many
            // whole bytes. Equal to `(63 - nbits) >> 3` for every `nbits` this can see, but
            // it needs no constant in a register, which matters across five inlined copies.
            debug_assert!(self.nbits < 64);
            self.pos += 7 - (self.nbits >> 3) as usize;
            self.nbits |= 56;
        } else {
            self.refill_tail();
        }
    }

    #[cold]
    fn refill_tail(&mut self) {
        while self.nbits <= 56 {
            if self.pos < self.input.len() {
                self.buf |= (self.input[self.pos] as u64) << self.nbits;
                self.pos += 1;
            } else {
                self.padding += 8;
            }
            self.nbits += 8;
        }
    }

    #[inline(always)]
    fn peek(&self, count: u32) -> u32 {
        (self.buf & ((1u64 << count) - 1)) as u32
    }

    #[inline(always)]
    fn consume(&mut self, count: u32) {
        debug_assert!(count <= self.nbits);
        self.buf >>= count;
        self.nbits -= count;
    }

    #[inline(always)]
    fn take(&mut self, count: u32) -> u32 {
        let value = self.peek(count);
        self.consume(count);
        value
    }

    /// Discards bits up to the next byte boundary.
    #[inline]
    fn align(&mut self) {
        self.consume(self.nbits % 8);
    }

    /// Byte offset of the next unread byte, or an error if decoding consumed bits that were
    /// never in the input.
    fn byte_position(&self) -> Result<usize, InflateError> {
        if self.padding > self.nbits {
            return Err(InflateError::UnexpectedEof);
        }
        debug_assert_eq!(self.nbits % 8, 0, "byte_position requires an aligned reader");
        Ok(self.pos - ((self.nbits - self.padding) / 8) as usize)
    }

    /// Restarts reading at an absolute byte offset, discarding any buffered bits.
    #[inline]
    fn seek(&mut self, byte_pos: usize) {
        self.pos = byte_pos;
        self.buf = 0;
        self.nbits = 0;
        self.padding = 0;
    }
}

// ---------------------------------------------------------------------------------------
// Table construction
// ---------------------------------------------------------------------------------------

/// Advances to the next canonical codeword in bit-reversed (stream) order.
///
/// Codewords are stored least-significant-bit-first so a table can be indexed with raw bits
/// straight from the stream. Incrementing in that order means carrying from the high end.
#[inline]
fn next_codeword(mut codeword: u16, table_size: u16) -> u16 {
    if codeword == table_size - 1 {
        return codeword;
    }
    let advance = (u16::BITS - 1) - (codeword ^ (table_size - 1)).leading_zeros();
    let bit = 1 << advance;
    codeword &= bit - 1;
    codeword |= bit;
    codeword
}

/// Builds a decoding table from canonical Huffman code lengths.
///
/// Returns `false` if the lengths do not describe a complete Huffman tree. When
/// `allow_degenerate` is set (distance codes only), an alphabet with zero or one used symbol
/// is accepted, matching what real encoders emit for streams with no matches.
///
/// `double_literal` packs two consecutive literal symbols into a single primary entry
/// whenever their combined code length still fits the table. On filtered image data, whose
/// byte distribution is dominated by a handful of short codes, this resolves a large fraction
/// of literals with one lookup instead of two.
// The loops below count in code lengths, which drive shifts and table widths as well as
// indexing the histogram; iterating the histogram instead would obscure that.
#[allow(clippy::needless_range_loop)]
fn build_table(
    lengths: &[u8],
    entries: &[u32],
    codes: &mut [u16; 288],
    primary: &mut [u32],
    secondary: &mut Vec<u16>,
    allow_degenerate: bool,
    double_literal: bool,
) -> bool {
    let mut histogram = [0usize; 16];
    for &length in lengths {
        histogram[length as usize] += 1;
    }

    let mut max_length = 15;
    while max_length > 1 && histogram[max_length] == 0 {
        max_length -= 1;
    }

    if allow_degenerate {
        if histogram[1..].iter().all(|&count| count == 0) {
            // No distances are used at all; any distance code in the stream is an error.
            primary.fill(0);
            secondary.clear();
            return true;
        }
        if max_length == 1 && histogram[1] == 1 {
            // A single one-bit code. The other half of the code space is invalid.
            let symbol = lengths.iter().position(|&l| l == 1).unwrap();
            codes[symbol] = 0;
            let entry = entries.get(symbol).copied().unwrap_or((symbol as u32) << 16) | 1;
            for pair in primary.chunks_mut(2) {
                pair[0] = entry;
                pair[1] = 0;
            }
            secondary.clear();
            return true;
        }
    }

    // Starting index of each code length within the length-sorted symbol list, plus a
    // Kraft sum check that the tree is exactly complete.
    let mut offsets = [0usize; 16];
    let mut codespace_used = 0usize;
    offsets[1] = histogram[0];
    for length in 1..max_length {
        offsets[length + 1] = offsets[length] + histogram[length];
        codespace_used = (codespace_used << 1) + histogram[length];
    }
    codespace_used = (codespace_used << 1) + histogram[max_length];
    if codespace_used != 1 << max_length {
        return false;
    }

    let mut next_index = offsets;
    let mut sorted_symbols = [0u16; 288];
    for (symbol, &length) in lengths.iter().enumerate() {
        sorted_symbols[next_index[length as usize]] = symbol as u16;
        next_index[length as usize] += 1;
    }

    let primary_bits = primary.len().trailing_zeros() as usize;
    let primary_mask = (1u16 << primary_bits) - 1;

    let mut codeword = 0u16;
    let mut cursor = histogram[0];

    // Iterate over every primary table width, not just the lengths in use: the doubling
    // step at the end of each round is what fills the table when the longest code is
    // shorter than the table.
    for length in 1..=primary_bits {
        let table_end = 1usize << length;

        for _ in 0..histogram[length] {
            let symbol = sorted_symbols[cursor] as usize;
            cursor += 1;

            primary[codeword as usize] =
                entries.get(symbol).copied().unwrap_or((symbol as u32) << 16) | length as u32;
            codes[symbol] = codeword;
            codeword = next_codeword(codeword, table_end as u16);
        }

        if double_literal {
            // Every way of splitting `length` bits into two shorter codes gives a pair of
            // literals that can be decoded together.
            for len1 in 1..length.saturating_sub(1) {
                let len2 = length - len1;
                for i1 in offsets[len1]..next_index[len1] {
                    for i2 in offsets[len2]..next_index[len2] {
                        let sym1 = sorted_symbols[i1] as usize;
                        let sym2 = sorted_symbols[i2] as usize;
                        if sym1 < 256 && sym2 < 256 {
                            let combined = codes[sym1] | (codes[sym2] << len1);
                            primary[combined as usize] = (sym1 as u32) << 16
                                | (sym2 as u32) << 24
                                | FLAG_LITERAL
                                | (2 << 8)
                                | length as u32;
                        }
                    }
                }
            }
        }

        // Codes shorter than the table width cover every index sharing their low bits;
        // doubling the filled prefix replicates them into the rest of the table.
        if length < primary_bits {
            primary.copy_within(0..table_end, table_end);
        }
    }

    secondary.clear();
    if max_length > primary_bits {
        let mut subtable_start = 0usize;
        let mut subtable_prefix = u16::MAX;

        for length in (primary_bits + 1)..=max_length {
            let subtable_size = 1usize << (length - primary_bits);

            for _ in 0..histogram[length] {
                if codeword & primary_mask != subtable_prefix {
                    subtable_prefix = codeword & primary_mask;
                    subtable_start = secondary.len();
                    primary[subtable_prefix as usize] = (subtable_start as u32) << 16
                        | FLAG_EXCEPTIONAL
                        | FLAG_SECONDARY
                        | (subtable_size as u32 - 1);
                    secondary.resize(subtable_start + subtable_size, 0);
                }

                let symbol = sorted_symbols[cursor];
                cursor += 1;
                codes[symbol as usize] = codeword;
                secondary[subtable_start + (codeword >> primary_bits) as usize] =
                    (symbol << 4) | length as u16;
                codeword = next_codeword(codeword, 1 << length);
            }

            // Longer codes sharing this prefix need the subtable to grow; the existing
            // entries are replicated so the wider index still resolves them.
            if length < max_length && codeword & primary_mask == subtable_prefix {
                secondary.extend_from_within(subtable_start..);
                let grown = secondary.len() - subtable_start;
                primary[subtable_prefix as usize] = (subtable_start as u32) << 16
                    | FLAG_EXCEPTIONAL
                    | FLAG_SECONDARY
                    | (grown as u32 - 1);
            }
        }
    }

    true
}

// ---------------------------------------------------------------------------------------
// Decompressor
// ---------------------------------------------------------------------------------------

/// A reusable DEFLATE decompressor.
///
/// Holding the decoding tables across calls avoids reallocating roughly 20 KiB per image.
pub struct Inflater {
    litlen: Box<[u32; LITLEN_TABLE_SIZE]>,
    litlen_secondary: Vec<u16>,
    dist: Box<[u32; DIST_TABLE_SIZE]>,
    dist_secondary: Vec<u16>,
    codes: [u16; 288],
    code_lengths: [u8; 320],
    verify_checksum: bool,
}

impl core::fmt::Debug for Inflater {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Inflater")
            .field("verify_checksum", &self.verify_checksum)
            .finish_non_exhaustive()
    }
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Inflater {
    /// A decompressor with its Huffman tables allocated but not yet filled.
    ///
    /// The tables are about twenty kilobytes, so reuse one across streams rather than
    /// building a decompressor per call.
    pub fn new() -> Self {
        Self {
            litlen: vec![0u32; LITLEN_TABLE_SIZE].into_boxed_slice().try_into().unwrap(),
            litlen_secondary: Vec::new(),
            dist: vec![0u32; DIST_TABLE_SIZE].into_boxed_slice().try_into().unwrap(),
            dist_secondary: Vec::new(),
            codes: [0; 288],
            code_lengths: [0; 320],
            verify_checksum: true,
        }
    }

    /// Enables or disables verification of the trailing Adler-32 checksum.
    ///
    /// PNG chunks carry their own CRC, so a caller that already verified those has checked
    /// the same bytes once; skipping the Adler-32 avoids a second pass over the output.
    pub fn verify_checksum(&mut self, verify: bool) -> &mut Self {
        self.verify_checksum = verify;
        self
    }

    /// Decompresses a zlib stream into `output`.
    ///
    /// The stream must expand to exactly `output.len() - OUTPUT_SLACK` bytes; the trailing
    /// slack is scratch space for match copies and never holds result data.
    ///
    /// PNG is the shape this is cut for: `IHDR` states the decompressed size, so a stream
    /// that stops short is a corrupt file and saying so is the useful answer. A caller who
    /// does not know the length in advance has no such expectation to check against and
    /// wants [`zlib_at_most`](Self::zlib_at_most).
    pub fn zlib(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize, InflateError> {
        let written = self.zlib_at_most(input, output)?;
        if written != output.len() - OUTPUT_SLACK {
            return Err(InflateError::OutputUnderflow);
        }
        Ok(written)
    }

    /// Decompresses a zlib stream into `output`, accepting any length that fits.
    ///
    /// Identical to [`zlib`](Self::zlib) except that a stream expanding to less than
    /// `output.len() - OUTPUT_SLACK` bytes is a success rather than
    /// [`OutputUnderflow`](InflateError::OutputUnderflow); the count is returned. One that
    /// expands to more is still [`OutputOverflow`](InflateError::OutputOverflow), since
    /// there is nowhere to put the rest.
    ///
    /// Nothing beyond the returned length is meaningful: the buffer is handed to the
    /// decoder whole, and the bytes past the data are whatever match copies overwrote.
    pub fn zlib_at_most(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize, InflateError> {
        self.zlib_at_most_progress::<()>(input, output, &mut ())
    }

    /// Decompresses a zlib stream into `output`, reporting output-cursor progress to `hook`.
    ///
    /// The stream must expand to exactly `output.len() - OUTPUT_SLACK` bytes, as in
    /// [`zlib`](Self::zlib); the hook observes the buffer as the stream is decoded. See
    /// [`ProgressHook`]: for [`()`] this is `zlib` under another name, and for the decoder's
    /// reconstruction frontier it fuses scanline reversal into inflation.
    pub(crate) fn zlib_progress<H: ProgressHook>(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        hook: &mut H,
    ) -> Result<usize, InflateError> {
        let written = self.zlib_at_most_progress(input, output, hook)?;
        if written != output.len() - OUTPUT_SLACK {
            return Err(InflateError::OutputUnderflow);
        }
        Ok(written)
    }

    fn zlib_at_most_progress<H: ProgressHook>(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        hook: &mut H,
    ) -> Result<usize, InflateError> {
        assert!(output.len() >= OUTPUT_SLACK, "output buffer must include OUTPUT_SLACK");
        let limit = output.len() - OUTPUT_SLACK;

        if input.len() < 2 {
            return Err(InflateError::UnexpectedEof);
        }
        let cmf = input[0];
        let flg = input[1];
        if cmf & 0x0f != 8 || cmf >> 4 > 7 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
            return Err(InflateError::BadZlibHeader);
        }
        if flg & 0x20 != 0 {
            return Err(InflateError::PresetDictionary);
        }

        let mut pause = None;
        // With verification on, the checksum has to be taken region by region ahead of the
        // hook: a hook that reverses filters in place would otherwise leave the buffer
        // partly rewritten by the time a single pass over it ran.
        let mut checksum = self.verify_checksum.then(Adler32::new);
        let (written, consumed) = match checksum.as_mut() {
            Some(checksum) => {
                let mut hook = ChecksumHook::new(hook, checksum, 0);
                let result =
                    self.inflate::<_, false>(&input[2..], output, limit, &mut hook, &mut pause)?;
                hook.hash_rest(output, result.0);
                result
            }
            None => self.inflate::<H, false>(&input[2..], output, limit, hook, &mut pause)?,
        };

        let trailer = &input[2 + consumed..];
        if trailer.len() < 4 {
            return Err(InflateError::UnexpectedEof);
        }
        if let Some(checksum) = checksum {
            let expected = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
            if checksum.finish() != expected {
                return Err(InflateError::WrongChecksum);
            }
        }

        Ok(written)
    }

    /// Decompresses one segment of a zlib stream into `output`, pausing at `budget`.
    ///
    /// The first call must pass `pause` as `None`; it validates the zlib header and starts
    /// the stream. Each call inflates until the stream ends or the output cursor reaches
    /// `budget` (which must leave [`OUTPUT_SLACK`] in `output`), returning the outcome. On
    /// `Filled`, `pause` carries everything needed to continue with the same `input` slice
    /// after the caller has made room via the next call.
    ///
    /// The cursor stops at `budget` or short of it — a match that would cross it waits for
    /// the next call — except that the literal run in flight when it is reached may finish
    /// up to five bytes past it. `written` is the exact position either way.
    ///
    /// On `Filled`, `hook` has observed `written` itself before the call returns. The decode
    /// loop reports progress at the top of each iteration, so without that final report an
    /// iteration that lands on the budget would leave the hook as much as a whole match
    /// behind the cursor, and a caller sizing its next step from the hook's state (the
    /// streaming decoder slides its stage to the frontier) would size it from stale data.
    ///
    /// `resume_at` overrides the checkpoint's output position before decoding. The
    /// streaming decoder passes `None` on the first call, the previous `Filled` position
    /// while extending a segment, and `window` after relocating the match window to the
    /// front of the stage with a `copy_within`.
    ///
    /// This is the seam under the decoder's streaming `decode_to`: a bounded stage holds
    /// the match window plus a segment of filtered rows, and pause/resume never crosses a
    /// symbol boundary the format does not already provide — the state is a bit position,
    /// and tables are rebuilt per block regardless.
    pub(crate) fn zlib_segment<H: ProgressHook>(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        budget: usize,
        resume_at: Option<usize>,
        pause: &mut Option<SegmentPause>,
        hook: &mut H,
    ) -> Result<SegmentOutcome, InflateError> {
        assert!(budget <= output.len() - OUTPUT_SLACK, "budget must leave OUTPUT_SLACK");

        if pause.is_none() {
            if input.len() < 2 {
                return Err(InflateError::UnexpectedEof);
            }
            let cmf = input[0];
            let flg = input[1];
            if cmf & 0x0f != 8 || cmf >> 4 > 7 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0
            {
                return Err(InflateError::BadZlibHeader);
            }
            if flg & 0x20 != 0 {
                return Err(InflateError::PresetDictionary);
            }
            *pause = Some(SegmentPause {
                reader_pos: 0,
                buf: 0,
                nbits: 0,
                padding: 0,
                out_pos: 0,
                mid_block: false,
                last_block: false,
            });
        }
        if let Some(position) = resume_at
            && let Some(state) = pause.as_mut()
        {
            state.out_pos = position;
        }

        let (written, consumed) =
            self.inflate::<H, true>(&input[2..], output, budget, hook, pause)?;

        match pause {
            Some(_) => {
                // Every byte below `written` is final, and no match copy is in flight at a
                // pause, so this is a boundary the hook contract allows.
                if H::ENABLED {
                    hook.on_progress(output, written);
                }
                Ok(SegmentOutcome::Filled { written })
            }
            None => Ok(SegmentOutcome::StreamEnd { written, consumed }),
        }
    }

    /// Decodes a raw DEFLATE stream, returning the bytes written and the input bytes read.
    ///
    /// With `SEGMENTED`, running into `limit` pauses: the reader and cursor are stored in
    /// `pause` (with `mid_block` set when a block is partially decoded) and the caller
    /// resumes by calling again after making room. Without it, the same condition is an
    /// [`OutputOverflow`](InflateError::OutputOverflow) error, as before.
    fn inflate<H: ProgressHook, const SEGMENTED: bool>(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        limit: usize,
        hook: &mut H,
        pause: &mut Option<SegmentPause>,
    ) -> Result<(usize, usize), InflateError> {
        // `decode_block` writes speculatively past `limit`, always below `limit +
        // OUTPUT_SLACK`. Its stores are checked, so this cannot cause an out-of-bounds write;
        // it is what keeps a valid stream from tripping one.
        assert!(
            limit <= output.len().saturating_sub(OUTPUT_SLACK),
            "limit must leave OUTPUT_SLACK in the output buffer"
        );
        let mut reader = BitReader::new(input);
        let mut out_pos = 0usize;

        // Resume from the previous segment's checkpoint. `last_block` replays the final
        // flag of a block paused mid-way: finishing it must not fall into the header loop.
        let mut finished_block = false;
        if let Some(state) = pause.take() {
            reader = BitReader {
                input,
                pos: state.reader_pos,
                buf: state.buf,
                nbits: state.nbits,
                padding: state.padding,
            };
            out_pos = state.out_pos;
            if state.mid_block {
                out_pos = self.decode_block::<H, SEGMENTED>(
                    &mut reader,
                    output,
                    out_pos,
                    limit,
                    hook,
                    pause,
                    state.last_block,
                )?;
                if SEGMENTED && pause.is_some() {
                    return Ok((out_pos, 0));
                }
                finished_block = state.last_block;
            }
        }

        while !finished_block {
            // A segmented pause rewinds to here: the stored block's header is re-read once
            // it fits, so the checkpoint is captured before any header bits are consumed.
            let block_start = reader;
            reader.refill();
            let last_block = reader.take(1) != 0;
            let block_type = reader.take(2);

            match block_type {
                0 => {
                    reader.align();
                    let start = reader.byte_position()?;
                    if start + 4 > input.len() {
                        return Err(InflateError::UnexpectedEof);
                    }
                    let len = u16::from_le_bytes([input[start], input[start + 1]]) as usize;
                    let nlen = u16::from_le_bytes([input[start + 2], input[start + 3]]) as usize;
                    if len ^ 0xffff != nlen {
                        return Err(InflateError::InvalidStoredLength);
                    }
                    let data_start = start + 4;
                    if data_start + len > input.len() {
                        return Err(InflateError::UnexpectedEof);
                    }
                    if out_pos + len > limit {
                        if SEGMENTED {
                            *pause = Some(SegmentPause {
                                reader_pos: block_start.pos,
                                buf: block_start.buf,
                                nbits: block_start.nbits,
                                padding: block_start.padding,
                                out_pos,
                                mid_block: false,
                                last_block: false,
                            });
                            return Ok((out_pos, 0));
                        }
                        return Err(InflateError::OutputOverflow);
                    }
                    output[out_pos..out_pos + len]
                        .copy_from_slice(&input[data_start..data_start + len]);
                    out_pos += len;
                    if H::ENABLED {
                        hook.on_progress(output, out_pos);
                    }
                    reader.seek(data_start + len);
                }
                1 => {
                    self.build_fixed_tables()?;
                    out_pos = self.decode_block::<H, SEGMENTED>(
                        &mut reader,
                        output,
                        out_pos,
                        limit,
                        hook,
                        pause,
                        last_block,
                    )?;
                }
                2 => {
                    self.read_dynamic_header(&mut reader)?;
                    out_pos = self.decode_block::<H, SEGMENTED>(
                        &mut reader,
                        output,
                        out_pos,
                        limit,
                        hook,
                        pause,
                        last_block,
                    )?;
                }
                _ => return Err(InflateError::InvalidBlockType),
            }

            if SEGMENTED && pause.is_some() {
                return Ok((out_pos, 0));
            }
            finished_block = last_block;
        }

        reader.align();
        let consumed = reader.byte_position()?;
        Ok((out_pos, consumed))
    }

    fn build_fixed_tables(&mut self) -> Result<(), InflateError> {
        if !build_table(
            &FIXED_LITLEN_LENGTHS,
            &LITLEN_ENTRIES,
            &mut self.codes,
            &mut self.litlen[..],
            &mut self.litlen_secondary,
            false,
            true,
        ) {
            return Err(InflateError::BadLiteralLengthTree);
        }
        if !build_table(
            &FIXED_DIST_LENGTHS,
            &DIST_ENTRIES,
            &mut self.codes,
            &mut self.dist[..],
            &mut self.dist_secondary,
            true,
            false,
        ) {
            return Err(InflateError::BadDistanceTree);
        }
        Ok(())
    }

    fn read_dynamic_header(&mut self, reader: &mut BitReader) -> Result<(), InflateError> {
        reader.refill();
        let hlit = reader.take(5) as usize + 257;
        let hdist = reader.take(5) as usize + 1;
        let hclen = reader.take(4) as usize + 4;

        if hlit > 286 {
            return Err(InflateError::InvalidHlit);
        }
        if hdist > 30 {
            return Err(InflateError::InvalidHdist);
        }

        let mut clcl_lengths = [0u8; 19];
        for &symbol in CLCL_ORDER.iter().take(hclen) {
            reader.refill();
            clcl_lengths[symbol as usize] = reader.take(3) as u8;
        }

        let mut clcl_table = [0u32; CLCL_TABLE_SIZE];
        let mut clcl_secondary = Vec::new();
        if !build_table(
            &clcl_lengths,
            &[],
            &mut self.codes,
            &mut clcl_table,
            &mut clcl_secondary,
            false,
            false,
        ) {
            return Err(InflateError::BadCodeLengthTree);
        }

        let total = hlit + hdist;
        let lengths = &mut self.code_lengths[..];
        lengths[..total].fill(0);

        let mut index = 0usize;
        while index < total {
            reader.refill();
            let entry = clcl_table[reader.peek(CLCL_TABLE_BITS) as usize];
            let code_bits = entry & 0xff;
            if code_bits == 0 {
                return Err(InflateError::BadCodeLengthTree);
            }
            reader.consume(code_bits);

            match entry >> 16 {
                symbol @ 0..=15 => {
                    lengths[index] = symbol as u8;
                    index += 1;
                }
                16 => {
                    if index == 0 {
                        return Err(InflateError::InvalidCodeLengthRepeat);
                    }
                    let repeat = 3 + reader.take(2) as usize;
                    let previous = lengths[index - 1];
                    if index + repeat > total {
                        return Err(InflateError::InvalidCodeLengthRepeat);
                    }
                    lengths[index..index + repeat].fill(previous);
                    index += repeat;
                }
                17 => {
                    let repeat = 3 + reader.take(3) as usize;
                    if index + repeat > total {
                        return Err(InflateError::InvalidCodeLengthRepeat);
                    }
                    index += repeat;
                }
                _ => {
                    let repeat = 11 + reader.take(7) as usize;
                    if index + repeat > total {
                        return Err(InflateError::InvalidCodeLengthRepeat);
                    }
                    index += repeat;
                }
            }
        }

        // The distance alphabet is always built over 32 symbols so that symbols 30 and 31,
        // which are legal in the code but never usable, are rejected by the decoder.
        let mut dist_lengths = [0u8; 32];
        dist_lengths[..hdist].copy_from_slice(&lengths[hlit..hlit + hdist]);

        if !build_table(
            &lengths[..hlit],
            &LITLEN_ENTRIES,
            &mut self.codes,
            &mut self.litlen[..],
            &mut self.litlen_secondary,
            false,
            true,
        ) {
            return Err(InflateError::BadLiteralLengthTree);
        }
        if !build_table(
            &dist_lengths,
            &DIST_ENTRIES,
            &mut self.codes,
            &mut self.dist[..],
            &mut self.dist_secondary,
            true,
            false,
        ) {
            return Err(InflateError::BadDistanceTree);
        }

        Ok(())
    }

    /// Writes the one or two literal bytes an entry carries, as one halfword store.
    ///
    /// Bounds-checked. The decode loop keeps `pos + 2 <= output.len()` by refusing to enter an
    /// iteration unless `pos <= limit`, where `limit + OUTPUT_SLACK <= output.len()`, and by
    /// advancing `pos` by at most six over the literals it then writes speculatively, so the
    /// check never fires on a stream the decoder accepts. It costs nothing measurable on the
    /// literal path: the earlier form skipped it with `unsafe` and measured the same.
    #[inline(always)]
    fn store_literals(output: &mut [u8], pos: usize, entry: u32) {
        let pair = ((entry >> 16) as u16).to_le_bytes();
        output[pos..pos + 2].copy_from_slice(&pair);
    }

    /// Appends a `length`-byte match to `output[..pos]`, copied from `distance` bytes back.
    ///
    /// The bytes are copied sixteen at a time, so a match writes up to fifteen bytes past its
    /// end: scratch that the next symbol overwrites or [`OUTPUT_SLACK`] holds. When the
    /// distance is shorter than a sixteen-byte step, each pass extends the correctly filled
    /// prefix by `distance` bytes and the run resolves itself after `ceil(length / distance)`
    /// passes, which is the overlapping-copy semantics LZ77 requires.
    ///
    /// This is the decoder's one function that steps outside safe Rust, and it is sound for
    /// any arguments: a single up-front check covers the whole range every pass can touch,
    /// `pos - distance .. pos + length + 15`, so the passes need none of their own. Checked per
    /// pass, the bounds tests measured 4-20% slower on the highly compressible streams whose
    /// matches are long; checked once per match they cost a handful of instructions on a path
    /// that takes ten or more cycles anyway. The caller has already established the same
    /// facts, which the optimiser cannot see through `limit`; here the check is real, and a
    /// bug elsewhere becomes a panic and not an out-of-bounds write.
    ///
    /// # Panics
    /// If `distance` is zero or exceeds `pos`, or the range does not fit in `output`.
    #[inline(always)]
    fn copy_match(output: &mut [u8], pos: usize, distance: usize, length: usize) {
        let end = pos.checked_add(length).and_then(|end| end.checked_add(15));
        assert!(
            distance != 0 && distance <= pos && end.is_some_and(|end| end <= output.len()),
            "match range outside the output buffer"
        );

        if distance == 1 {
            // Byte runs are the most common match in filtered image data. Both fills are
            // safe code; the assertion above has already put `pos + length + 15` in range.
            let byte = output[pos - 1];
            if length <= 16 {
                output[pos..pos + 16].fill(byte);
            } else {
                output[pos..pos + length].fill(byte);
            }
            return;
        }

        let source = pos - distance;
        let step = distance.min(16);
        let base = output.as_mut_ptr();
        let mut offset = 0;
        while offset < length {
            // SAFETY: the assertion above gives `source + offset + 16 <= pos + offset + 16 <=
            // pos + length + 15 + 1 <= output.len()` for every `offset < length`, so both the
            // read and the write are in bounds. Each reads into a register before it writes,
            // which is what an overlapping source needs, and written as fixed-size accesses
            // they stay a register pair rather than becoming a `memmove` call.
            unsafe {
                let chunk = base.add(source + offset).cast::<[u8; 16]>().read_unaligned();
                base.add(pos + offset).cast::<[u8; 16]>().write_unaligned(chunk);
            }
            offset += step;
        }
    }

    /// Decodes one compressed block, returning the new output position.
    ///
    /// The bit reader is copied into a local for the duration of the loop. Left behind a
    /// `&mut` it would have to be spilled to memory after every consume, because the
    /// compiler cannot otherwise rule out aliasing with the output buffer; as a local it
    /// stays in registers.
    ///
    /// `hook` observes the output cursor once per loop iteration. For [`()`] the call
    /// compiles out entirely (see [`ProgressHook`]), leaving the loop identical to a
    /// hook-free decoder.
    ///
    /// `limit` is where output stops: the end of the buffer less [`OUTPUT_SLACK`] for a
    /// one-shot decode, and the caller's budget for a segmented one. The caller guarantees
    /// `limit + OUTPUT_SLACK <= output.len()`, which is what keeps the stores below inside
    /// the buffer for every stream the decoder accepts.
    ///
    /// With `SEGMENTED` the two output-limit checks pause instead of erroring: the bit
    /// reader and the cursor are stored in `pause` and the caller resumes from them after
    /// making room. `Ok` therefore means "block end or pause"; the caller distinguishes
    /// through `pause.is_some()`. `last_block` is recorded so a mid-block pause resumed
    /// into the stream's final block does not decode past it.
    #[allow(clippy::too_many_arguments)]
    fn decode_block<H: ProgressHook, const SEGMENTED: bool>(
        &mut self,
        reader: &mut BitReader,
        output: &mut [u8],
        start_pos: usize,
        limit: usize,
        hook: &mut H,
        pause: &mut Option<SegmentPause>,
        last_block: bool,
    ) -> Result<usize, InflateError> {
        // Binding the tables as fixed-size array references, rather than slices, lets the
        // masked table indices be proven in range and drops the bounds checks from the two
        // hottest loads in the loop.
        let litlen: &[u32; LITLEN_TABLE_SIZE] = &self.litlen;
        let dist_table: &[u32; DIST_TABLE_SIZE] = &self.dist;
        let litlen_secondary = &self.litlen_secondary[..];
        let dist_secondary = &self.dist_secondary[..];

        debug_assert!(limit + OUTPUT_SLACK <= output.len());
        let mut pos = start_pos;
        let mut r = *reader;

        let result = 'block: {
            r.refill();
            let mut entry = litlen[r.peek(LITLEN_TABLE_BITS) as usize];

            loop {
                if SEGMENTED {
                    // Pause at the exact budget: pausing past it would leave speculative
                    // literal bytes inside the window the caller relocates.
                    if pos >= limit {
                        *reader = r;
                        *pause = Some(SegmentPause {
                            reader_pos: r.pos,
                            buf: r.buf,
                            nbits: r.nbits,
                            padding: r.padding,
                            out_pos: pos,
                            mid_block: true,
                            last_block,
                        });
                        break 'block Ok(pos);
                    }
                } else if pos > limit {
                    break 'block Err(InflateError::OutputOverflow);
                }
                if H::ENABLED {
                    hook.on_progress(output, pos);
                }

                let mut bits = r.buf;
                let mut code_bits = entry & 0xff;

                if entry & FLAG_LITERAL != 0 {
                    // Literals dominate filtered image data, so speculatively resolve a
                    // short chain of them. Each entry carries its own code length, so the
                    // next table index is available without waiting for the previous store.
                    //
                    // The bits are shifted cumulatively rather than by a running sum of code
                    // lengths: that keeps the loop between one table load and the next down
                    // to a shift and a mask, which is the whole latency budget of this loop.
                    let bits1 = bits >> (entry & SHIFT_MASK);
                    let entry2 = litlen[(bits1 as u32 & 0xfff) as usize];
                    let bits2 = bits1 >> (entry2 & SHIFT_MASK);
                    let entry3 = litlen[(bits2 as u32 & 0xfff) as usize];
                    let bits3 = bits2 >> (entry3 & SHIFT_MASK);
                    let entry4 = litlen[(bits3 as u32 & 0xfff) as usize];

                    let code_bits2 = entry2 & 0xff;
                    let code_bits3 = entry3 & 0xff;

                    // `pos <= limit` was checked above and `limit + OUTPUT_SLACK <=
                    // output.len()`, so `pos + 2 <= output.len() - 14`. Each store below
                    // advances `pos` by at most two and there are at most three of them, so
                    // none of the checks inside `store_literals` can fail.
                    Self::store_literals(output, pos, entry);
                    pos += ((entry >> 8) & 0xf) as usize;

                    if entry2 & FLAG_LITERAL != 0 {
                        Self::store_literals(output, pos, entry2);
                        pos += ((entry2 >> 8) & 0xf) as usize;

                        if entry3 & FLAG_LITERAL != 0 {
                            Self::store_literals(output, pos, entry3);
                            pos += ((entry3 >> 8) & 0xf) as usize;

                            r.consume(code_bits + code_bits2 + code_bits3);
                            r.refill();
                            entry = entry4;
                            continue;
                        }

                        r.consume(code_bits + code_bits2);
                        r.refill();
                        entry = entry3;
                        code_bits = code_bits3;
                        bits = r.buf;
                    } else {
                        r.consume(code_bits);
                        r.refill();
                        entry = entry2;
                        code_bits = code_bits2;
                        bits = r.buf;
                    }
                }

                // Whatever is left is a match length, the end-of-block marker, or a code too
                // long for the primary table.
                let (length_base, length_extra, code_bits) = if entry & FLAG_EXCEPTIONAL == 0 {
                    (entry >> 16, (entry >> 8) & 0xf, code_bits)
                } else if entry & FLAG_SECONDARY != 0 {
                    let index =
                        (entry >> 16) + ((bits >> LITLEN_TABLE_BITS) as u32 & (entry & 0xff));
                    let secondary = match litlen_secondary.get(index as usize) {
                        Some(&value) => value,
                        None => break 'block Err(InflateError::InvalidLiteralLengthCode),
                    };
                    let symbol = (secondary >> 4) as usize;
                    let secondary_bits = (secondary & 0xf) as u32;

                    match symbol {
                        0..=255 => {
                            r.consume(secondary_bits);
                            r.refill();
                            output[pos] = symbol as u8;
                            pos += 1;
                            entry = litlen[r.peek(LITLEN_TABLE_BITS) as usize];
                            continue;
                        }
                        256 => {
                            r.consume(secondary_bits);
                            break 'block Ok(pos);
                        }
                        257..=285 => (
                            LEN_BASE[symbol - 257] as u32,
                            LEN_EXTRA[symbol - 257] as u32,
                            secondary_bits,
                        ),
                        _ => break 'block Err(InflateError::InvalidLiteralLengthCode),
                    }
                } else if entry & FLAG_INVALID != 0 || code_bits == 0 {
                    break 'block Err(InflateError::InvalidLiteralLengthCode);
                } else {
                    r.consume(code_bits);
                    break 'block Ok(pos);
                };

                bits >>= code_bits;
                let length = (length_base + (bits as u32 & ((1 << length_extra) - 1))) as usize;
                bits >>= length_extra;

                let dist_entry = dist_table[(bits as u32 & 0x1ff) as usize];
                let (dist_base, dist_extra, dist_bits) = if dist_entry & FLAG_LITERAL != 0 {
                    (dist_entry >> 16, (dist_entry >> 8) & 0xf, dist_entry & 0xff)
                } else if dist_entry & FLAG_SECONDARY != 0 {
                    let index = (dist_entry >> 16)
                        + ((bits >> DIST_TABLE_BITS) as u32 & (dist_entry & 0xff));
                    let secondary = match dist_secondary.get(index as usize) {
                        Some(&value) => value,
                        None => break 'block Err(InflateError::InvalidDistanceCode),
                    };
                    let symbol = (secondary >> 4) as usize;
                    if symbol >= 30 {
                        break 'block Err(InflateError::InvalidDistanceCode);
                    }
                    (DIST_BASE[symbol] as u32, DIST_EXTRA[symbol] as u32, (secondary & 0xf) as u32)
                } else {
                    break 'block Err(InflateError::InvalidDistanceCode);
                };

                bits >>= dist_bits;
                let distance = (dist_base + (bits as u32 & ((1 << dist_extra) - 1))) as usize;

                // The match's bits are consumed before its bounds are known; a segmented
                // pause on the length check must rewind to here, or the resume would
                // decode past the match and lose its bytes.
                let symbol_start = r;
                r.consume(code_bits + length_extra + dist_bits + dist_extra);
                r.refill();
                entry = litlen[r.peek(LITLEN_TABLE_BITS) as usize];

                if distance > pos {
                    break 'block Err(InflateError::DistanceTooFarBack);
                }
                if pos + length > limit {
                    if SEGMENTED {
                        *reader = symbol_start;
                        *pause = Some(SegmentPause {
                            reader_pos: symbol_start.pos,
                            buf: symbol_start.buf,
                            nbits: symbol_start.nbits,
                            padding: symbol_start.padding,
                            out_pos: pos,
                            mid_block: true,
                            last_block,
                        });
                        break 'block Ok(pos);
                    }
                    break 'block Err(InflateError::OutputOverflow);
                }

                // `pos + length <= limit` was just checked and the buffer carries
                // `OUTPUT_SLACK` bytes beyond `limit`, so the copy's own range check, which
                // does not depend on this reasoning, passes for every stream accepted here.
                Self::copy_match(output, pos, distance, length);
                pos += length;
            }
        };

        *reader = r;
        result
    }
}

/// Decompresses a zlib stream that is known to expand to exactly `expected_len` bytes.
pub fn decompress_zlib(input: &[u8], expected_len: usize) -> Result<Vec<u8>, InflateError> {
    // `expected_len` is the caller's, and on the paths this exists for it came out of a file.
    // A length that cannot have its slack added is refused like any other unmeetable size.
    let request = expected_len
        .checked_add(OUTPUT_SLACK)
        .ok_or(InflateError::OutOfMemory { bytes: expected_len })?;
    let mut output = zeroed_vec(request).ok_or(InflateError::OutOfMemory { bytes: request })?;
    let written = Inflater::new().zlib(input, &mut output)?;
    output.truncate(written);
    Ok(output)
}

/// Decompresses a zlib stream of unknown length, growing the output buffer as needed and
/// giving up past `max_output` bytes.
///
/// Use this when the length is not recorded anywhere trustworthy. A length field read from
/// an untrusted file is not an answer to that problem, it is a restatement of it: taking one
/// at face value is an instruction to allocate whatever the file asks for, which is why
/// `max_output` is a parameter and not a default. Truncation is caught regardless, by the
/// Adler-32 the zlib framing already carries over the data.
///
/// The cost of not knowing is decoding more than once. The decompressor holds its whole
/// output buffer as the match window and so cannot resume into a larger one; a buffer that
/// fills is therefore discarded and the stream decoded again into a buffer twice the size.
/// The discarded attempts sum to just under the size of the one that succeeds, so the
/// doubling bounds the wasted work at one extra pass over the data. That is not free, and a
/// caller who *does* know the length should say so through [`decompress_zlib`] instead.
///
/// Returns [`OutputOverflow`](InflateError::OutputOverflow) if the stream expands past
/// `max_output`, having allocated no more than that. A corrupt stream reaches the same
/// answer the same way, so `max_output` bounds the work this will do on a hostile input as
/// well as the memory: budget it as two passes over `max_output` bytes, however few bytes of
/// input arrived.
pub fn decompress_zlib_to_vec(input: &[u8], max_output: usize) -> Result<Vec<u8>, InflateError> {
    // Deflate's best case is a little over 1000:1, so no first guess drawn from the input
    // length is safe; four times it settles ordinary data in a single pass, and the floor
    // keeps small streams from starting a doubling run at a handful of bytes.
    let mut capacity = input.len().saturating_mul(4).max(1024).min(max_output);

    let mut inflater = Inflater::new();
    loop {
        let request = capacity
            .checked_add(OUTPUT_SLACK)
            .ok_or(InflateError::OutOfMemory { bytes: capacity })?;
        let mut output = zeroed_vec(request).ok_or(InflateError::OutOfMemory { bytes: request })?;
        match inflater.zlib_at_most(input, &mut output) {
            Ok(written) => {
                output.truncate(written);
                return Ok(output);
            }
            Err(InflateError::OutputOverflow) if capacity < max_output => {
                capacity = capacity.saturating_mul(2).min(max_output);
            }
            Err(error) => return Err(error),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Segmented decompression (streaming decode)
// ---------------------------------------------------------------------------------------

/// Where a segmented decompression stopped, so the next segment can resume exactly.
///
/// A segment ends at a deflate block boundary or mid-block, wherever the output budget ran
/// out; either way the bit reader's position and the output cursor are enough to continue,
/// because the Huffman tables are rebuilt per block and the reader state is `Copy`.
/// `mid_block` says the block header was already consumed and the tables for the current
/// block are built: resume inside the block, not at the header loop.
pub(crate) struct SegmentPause {
    reader_pos: usize,
    buf: u64,
    nbits: u32,
    padding: u32,
    /// Stage-relative output position.
    out_pos: usize,
    mid_block: bool,
    /// Whether the block being decoded is the stream's final one; only meaningful with
    /// `mid_block`, so a resumed final block finishes the stream instead of reading on.
    last_block: bool,
}

/// The outcome of one `zlib_segment` call.
#[derive(Debug)]
pub(crate) enum SegmentOutcome {
    /// The stream is fully decoded. `written` is the stage-relative end of the output and
    /// `consumed` the input bytes the deflate stream occupied, excluding the 2-byte zlib
    /// header and the 4-byte trailer.
    StreamEnd { written: usize, consumed: usize },
    /// The output budget is full; `pause` (passed to the next call) carries the resume state.
    Filled { written: usize },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::filter::MAX_MATCH_DISTANCE;

    /// The zlib stream the decoder-side tests need, built from stored blocks: the crate's
    /// compressor is gone, and a stored stream needs none. The block type bytes and length
    /// words are the format's own; the Adler-32 is the crate's. `block_limit` is the most
    /// bytes one stored block carries, which decides whether a pause can fall mid-block.
    fn stored_stream_for_tests(data: &[u8], block_limit: usize) -> Vec<u8> {
        let mut out = vec![0x78, 0x01];
        let mut rest = data;
        while !rest.is_empty() {
            let take = rest.len().min(block_limit);
            let (block, tail) = rest.split_at(take);
            out.push(if tail.is_empty() { 0x01 } else { 0x00 });
            let len = block.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(block);
            rest = tail;
        }
        out.extend_from_slice(&crate::adler32::adler32(data).to_be_bytes());
        out
    }

    /// Miri cannot enumerate directories on Windows, so alongside the corpus sweep this
    /// fs-free case runs the pause/resume machinery over a stream built by the crate's own
    /// deflate encoder, with a stage smaller than the stream to force real slides.
    #[test]
    fn segmented_resume_matches_one_shot_inline() {
        // Compressible data. The zlib stream is built from one stored block, so the segment
        // budget is smaller than the block and the pause falls mid-block, which is the
        // resume path that matters.
        let expected = vec![0u8; 45_000];
        let compressed = stored_stream_for_tests(&expected, 4_096);
        let mut budget = MAX_MATCH_DISTANCE + 4096;

        let mut reference = vec![0u8; expected.len() + OUTPUT_SLACK];
        Inflater::new().zlib(&compressed, &mut reference).unwrap();

        let mut stage = vec![0u8; MAX_MATCH_DISTANCE + 4096 + 65_536 + OUTPUT_SLACK];
        let mut inflater = Inflater::new();
        let mut pause: Option<SegmentPause> = None;
        let mut output = Vec::new();
        let mut prev_end = 0usize;
        let mut resume_at: Option<usize> = None;
        let mut base = 0usize;
        for iteration in 0.. {
            match inflater
                .zlib_segment(&compressed, &mut stage, budget, resume_at, &mut pause, &mut ())
                .unwrap()
            {
                SegmentOutcome::StreamEnd { written, .. } => {
                    output.extend_from_slice(&stage[prev_end..written]);
                    break;
                }
                SegmentOutcome::Filled { written } => {
                    output.extend_from_slice(&stage[prev_end..written]);
                    // Relocate the window and advance, as the streaming decoder does.
                    // Budget keeps pace with the absolute position (base + budget grows
                    // every fill), so a match that refuses the budget always fits after
                    // the next relocation. A fill with no new bytes is a block that
                    // cannot fit (a stored block beyond the budget); grow once, enough
                    // for any stored block.
                    let keep = MAX_MATCH_DISTANCE.min(written);
                    stage.copy_within(written - keep..written, 0);
                    base += written - keep;
                    prev_end = keep;
                    resume_at = Some(keep);
                    let grown = keep + 4096;
                    budget =
                        if output.is_empty() && iteration > 0 { budget + 65_536 } else { grown };
                }
            }
        }
        let _ = base;

        assert_eq!(output.len(), expected.len());
        assert!(output.len() > budget, "resume path not exercised");
        assert_eq!(output, expected);
    }

    /// A hook that remembers the last cursor position it was shown.
    struct LastSeen(usize);

    impl ProgressHook for LastSeen {
        const ENABLED: bool = true;

        fn on_progress(&mut self, _output: &mut [u8], pos: usize) {
            self.0 = pos;
        }
    }

    /// A paused segment must have shown the hook the exact position it paused at.
    ///
    /// The decode loop reports progress at the top of each iteration, so the iteration that
    /// reaches the budget writes bytes the hook has not been told about. When that iteration
    /// is a match, the gap is up to 258 bytes, and the streaming decoder — which slides its
    /// stage to wherever the hook's frontier stands — once kept that much more than its
    /// headroom allowed and panicked on narrow images. Sweeping the budget across runs of
    /// long matches lands the pause after every kind of iteration: literal chains, matches
    /// that end exactly on the budget, and matches refused for crossing it.
    #[test]
    fn a_paused_segment_has_reported_its_position_to_the_hook() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i / 700) as u8).collect();
        let compressed = stored_stream_for_tests(&data, 4_096);

        for budget in 1..3_000 {
            let mut stage = vec![0u8; budget + OUTPUT_SLACK];
            let mut pause = None;
            let mut hook = LastSeen(usize::MAX);
            let outcome = Inflater::new()
                .zlib_segment(&compressed, &mut stage, budget, None, &mut pause, &mut hook)
                .unwrap();
            let SegmentOutcome::Filled { written } = outcome else {
                panic!("budget {budget}: a stream of {} bytes cannot end here", data.len());
            };
            assert_eq!(hook.0, written, "budget {budget}: the hook was left behind the cursor");
            assert!(written <= budget + 5, "budget {budget}: overran to {written}");
            assert_eq!(&stage[..written], &data[..written], "budget {budget}");
        }
    }

    /// Drives `zlib_segment` with a small stage over every reference vector and requires
    /// the concatenated segments to equal the one-shot decode. A small budget forces the
    /// pause/resume path hundreds of times per vector.
    #[test]
    fn segmented_resume_matches_one_shot() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp/z");
        if !dir.exists() {
            eprintln!("skipping: {} not generated", dir.display());
            return;
        }

        // A segment must hold a whole stored block (65535 bytes) beyond the window, or a
        // vector built from stored blocks can never make progress at this budget.
        const CAP: usize = 70_000;
        let budget = MAX_MATCH_DISTANCE + CAP;

        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("z") {
                continue;
            }
            let compressed = std::fs::read(&path).unwrap();
            let expected = std::fs::read(path.with_extension("raw")).unwrap();

            // One-shot reference.
            let mut reference = vec![0u8; expected.len() + OUTPUT_SLACK];
            Inflater::new().zlib(&compressed, &mut reference).unwrap();
            reference.truncate(expected.len());

            // Segmented with a stage barely larger than the match window, so almost every
            // vector spans several slides. Models the streaming decoder's contract:
            // relocate the window to the front after each Filled and resume there.
            let mut stage = vec![0u8; budget + OUTPUT_SLACK];
            let mut inflater = Inflater::new();
            let mut pause: Option<SegmentPause> = None;
            let mut output = Vec::new();
            let mut prev_end = 0usize;
            let mut resume_at: Option<usize> = None;
            loop {
                match inflater
                    .zlib_segment(&compressed, &mut stage, budget, resume_at, &mut pause, &mut ())
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
                {
                    SegmentOutcome::StreamEnd { written, .. } => {
                        output.extend_from_slice(&stage[prev_end..written]);
                        break;
                    }
                    SegmentOutcome::Filled { written } => {
                        output.extend_from_slice(&stage[prev_end..written]);
                        // Relocate what fits of the match window, as the decoder does;
                        // early segments can be smaller than the window.
                        let keep = MAX_MATCH_DISTANCE.min(written);
                        stage.copy_within(written - keep..written, 0);
                        prev_end = keep;
                        resume_at = Some(keep);
                    }
                }
            }

            assert_eq!(output, expected, "{}", path.display());
            // The whole point: vectors bigger than one segment actually exercised resume.
            if expected.len() > budget {
                assert!(output.len() > budget, "{}: resume path not exercised", path.display());
            }
            checked += 1;
        }
        assert!(checked > 0, "no vectors found in {}", dir.display());
        eprintln!("checked {checked} streams");
    }

    /// The match copy must reproduce the byte-at-a-time definition of an LZ77 copy for every
    /// distance and length, including the distances shorter than its sixteen-byte step, where
    /// each pass depends on the bytes the previous one wrote. Bytes past the match are
    /// scratch by contract and are not compared.
    #[test]
    fn copy_match_matches_the_byte_at_a_time_definition() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut history = Vec::new();
        for _ in 0..600 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            history.push((state >> 24) as u8);
        }

        for distance in [1usize, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 100, 257, 500] {
            for length in [3usize, 4, 5, 15, 16, 17, 18, 31, 32, 33, 64, 100, 257, 258] {
                let pos = 520;
                let mut got = history.clone();
                got.resize(pos + 258 + OUTPUT_SLACK, 0xAA);
                let mut want = got.clone();

                for i in 0..length {
                    want[pos + i] = want[pos + i - distance];
                }
                Inflater::copy_match(&mut got, pos, distance, length);
                assert_eq!(
                    got[..pos + length],
                    want[..pos + length],
                    "distance {distance}, length {length}"
                );
            }
        }
    }

    /// A match whose range does not fit is refused before any byte is written, not partly
    /// performed: the copy is a safe function, and its safety is this check.
    #[test]
    fn copy_match_refuses_a_range_outside_the_buffer() {
        let original: Vec<u8> = (0..64u8).collect();
        let refused = [
            // Distance zero, and further back than the start of the output.
            (32usize, 0usize, 4usize),
            (8, 9, 4),
            // The last pass would write past the end: `pos + length + 15 > len`.
            (60, 3, 4),
            (49, 3, 1),
            // Arithmetic that would wrap.
            (32, 4, usize::MAX),
            (usize::MAX, 4, 4),
        ];
        for (pos, distance, length) in refused {
            let mut buffer = original.clone();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Inflater::copy_match(&mut buffer, pos, distance, length);
            }));
            assert!(outcome.is_err(), "pos {pos}, distance {distance}, length {length}");
            assert_eq!(buffer, original, "a refused match must not write");
        }

        // The tightest range that fits: `pos + length + 15 == len`.
        let mut buffer = original.clone();
        Inflater::copy_match(&mut buffer, 46, 3, 3);
        assert_eq!(buffer[46..49], [43, 44, 45]);
    }
}
