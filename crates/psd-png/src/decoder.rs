//! PNG decoding.

use crate::common::{
    ADAM7_PASSES, BitDepth, ColorType, Info, Interlacing, SIGNATURE, adam7_pass_size,
    row_bytes_for, zeroed_vec,
};
use crate::crc32::crc32;
use crate::error::Error;
use crate::filter::unfilter_image;
use crate::inflate::{ChecksumHook, InflateError, Inflater, OUTPUT_SLACK};
use crate::transform::{RowConverter, RowSample};

/// A decoded image and the description of its pixel layout.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Image {
    /// The image's dimensions and layout, including its palette and `tRNS` payload.
    pub info: Info,
    /// Pixel data in the image's own format, tightly packed, with no filter bytes and no
    /// padding between rows beyond what a sub-byte bit depth requires.
    pub data: Vec<u8>,
}

/// One scanline of a PNG, delivered by [`Decoder::decode_to`] and its converting
/// variants, in file order.
///
/// `index` counts from zero. `bytes` is one scanline with its filters already reversed:
/// for [`decode_to`](Decoder::decode_to) it is the file's own layout, as
/// [`Info::row_bytes`] describes it, and for the converting methods it is the converted
/// layout those methods document. `bytes.len()` is always the authority on which one it is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row<'a> {
    /// The row's index in the output image: `0` is the top scanline.
    pub index: usize,
    /// The row's bytes: `row_bytes` of pixel data, as [`Info::row_bytes`] describes.
    pub bytes: &'a [u8],
}

impl Image {
    /// Width in pixels.
    #[inline]
    pub fn width(&self) -> u32 {
        self.info.width
    }

    /// Height in pixels.
    #[inline]
    pub fn height(&self) -> u32 {
        self.info.height
    }

    /// How samples are laid out within a pixel of [`Image::data`].
    #[inline]
    pub fn color_type(&self) -> ColorType {
        self.info.color_type
    }

    /// Bits per sample in [`Image::data`].
    #[inline]
    pub fn bit_depth(&self) -> BitDepth {
        self.info.bit_depth
    }
}

/// Which integrity checks the decoder performs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Checks {
    /// Verify the CRC of every chunk. This is the default.
    ///
    /// A chunk that fails is an error when it is critical and dropped when it is ancillary,
    /// since nothing the decoder returns is built from an ancillary chunk.
    ///
    /// The compressed stream also carries an Adler-32 over the data it expands to, but that
    /// covers the same bytes a second time: if the `IDAT` payload is intact and the
    /// decompressor is correct, so is its output. Checking only the CRC therefore detects
    /// the same file corruption for one pass over the data instead of two.
    Crc,
    /// Verify chunk CRCs *and* the Adler-32 of the decompressed data.
    ///
    /// Worth the second pass when the decompressed data must be guarded against faults that
    /// arise after the CRC has been checked, such as memory errors.
    Full,
    /// Verify nothing, and accept whatever the file contains.
    None,
}

/// Which ancillary chunks a decode retains.
///
/// Nothing the decoder returns is built from an ancillary chunk, so by default they are
/// skipped without being copied. Retaining one costs an allocation and a copy of its
/// payload, which is why it is asked for rather than assumed.
///
/// A chunk that fails its CRC is dropped before this is consulted, so a retained chunk has
/// The decoder's default ceiling on [`Info::decompressed_size`], in bytes.
///
/// Half a gigabyte admits any photograph or screen capture a caller is likely to have meant
/// to decode: a 16-bit RGBA image of 8000 by 8000 pixels fits inside it. What it refuses is
/// the class of file that only exists to be refused.
pub const DEFAULT_MAX_DECOMPRESSED_SIZE: usize = 512 << 20;

/// A reusable PNG decoder.
///
/// Reusing one decoder across images keeps the Huffman decoding tables allocated, which is
/// worth roughly 20 KiB of allocation and initialisation per image.
pub struct Decoder {
    inflater: Inflater,
    checks: Checks,
    max_decompressed_size: Option<usize>,
}

impl core::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decoder")
            .field("checks", &self.checks)
            .field("max_decompressed_size", &self.max_decompressed_size)
            .finish_non_exhaustive()
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder that checks chunk CRCs and refuses an image declaring more than
    /// [`DEFAULT_MAX_DECOMPRESSED_SIZE`] bytes.
    pub fn new() -> Self {
        Self {
            inflater: Inflater::new(),
            checks: Checks::Crc,
            max_decompressed_size: Some(DEFAULT_MAX_DECOMPRESSED_SIZE),
        }
    }

    /// Selects which integrity checks to perform. See [`Checks`].
    pub fn checks(&mut self, checks: Checks) -> &mut Self {
        self.checks = checks;
        self
    }

    /// Sets the largest [`Info::decompressed_size`] this decoder will accept, or `None` to
    /// accept any size the platform can address.
    ///
    /// `IHDR` is thirteen bytes and states the image's dimensions, and the decoder needs a
    /// buffer of the size they imply before it can read a single compressed byte. Nothing in
    /// the file has to justify that number: a seventy-byte PNG can name a width and height
    /// whose product runs to petabytes, and a decoder that believes it will ask the allocator
    /// for petabytes. PNG is a format that arrives from elsewhere far more often than it is
    /// written locally, so an unbounded decoder is a way to take a process down from across
    /// a network, and the ceiling is [`DEFAULT_MAX_DECOMPRESSED_SIZE`] rather than absent.
    ///
    /// The limit is on the decompressed data, filter bytes included, and not on pixels: the
    /// same pixel count spans a sixty-four-fold range of buffer sizes across PNG's colour
    /// types and bit depths, from one bit a pixel to sixty-four, so a pixel count does not
    /// bound what gets allocated.
    ///
    /// It bounds the largest single buffer, not the decode's peak. An interlaced image is
    /// unfiltered pass by pass into a second buffer of its own, so Adam7 holds close to
    /// twice the limit at once; every other image holds it once.
    ///
    /// The streaming methods ([`decode_to`](Self::decode_to) and its converting variants)
    /// are held to the same rule, applied to what they allocate instead. A non-interlaced
    /// image streams through a stage whose size follows the row width, not the height —
    /// the match window, a 256 KiB segment and a few rows — plus, when converting, one
    /// converted row; each of those must fit. So a tall image far over the ceiling still
    /// streams, while a header naming rows gigabytes wide is refused as it is by
    /// [`decode`](Self::decode). An interlaced image streams from a whole-image buffer and
    /// is limited exactly as `decode` limits it.
    ///
    /// A header over the limit is [`Error::SizeLimitExceeded`], reported before anything
    /// image-sized is allocated. Raising it for a caller who really does read hundred-
    /// megapixel images is a one-liner, and passing `None` restores the unbounded behaviour
    /// outright:
    ///
    /// ```
    /// use psd_png::Decoder;
    ///
    /// let mut decoder = Decoder::new();
    /// decoder.max_decompressed_size(Some(4 << 30));
    /// ```
    ///
    /// [`read_info`](Self::read_info) is not bounded by this, because it allocates nothing
    /// that depends on the dimensions. A caller wanting to apply a policy of its own can
    /// read the header, decide, and only then decode.
    pub fn max_decompressed_size(&mut self, bytes: Option<usize>) -> &mut Self {
        self.max_decompressed_size = bytes;
        self
    }

    /// Decodes a PNG into its native pixel format.
    pub fn decode(&mut self, png: &[u8]) -> Result<Image, Error> {
        let parsed = self.parse(png, Plan::Whole)?;
        self.decode_parsed(parsed)
    }

    /// Decodes a PNG one scanline at a time, in file order, in the file's native layout.
    ///
    /// `sink` receives each row as it is reconstructed. For a non-interlaced image the
    /// decoder holds only a stage sized by the row width — the DEFLATE match window, a
    /// segment of filtered rows and a few rows of headroom, a few hundred kilobytes for
    /// ordinary images — whatever the height, so images far beyond
    /// [`max_decompressed_size`](Self::max_decompressed_size) decode here that
    /// [`decode`](Self::decode) must refuse. The ceiling bounds the stage instead: see
    /// `max_decompressed_size`. Interlaced images are decoded into a buffer first, as they
    /// are by `decode`, emitted from there, and limited as `decode` limits them.
    ///
    /// A sink error aborts the decode and is returned. Rows already delivered stay
    /// delivered.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let png = std::fs::read("asset.png")?;
    /// let mut decoder = psd_png::Decoder::new();
    /// decoder.decode_to(&png, |row: psd_png::Row<'_>| {
    ///     println!("row {}: {} bytes", row.index, row.bytes.len());
    ///     Ok::<(), psd_png::Error>(())
    /// })?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn decode_to<E, F>(&mut self, png: &[u8], sink: F) -> Result<(), E>
    where
        F: FnMut(Row<'_>) -> Result<(), E>,
        E: From<Error>,
    {
        let parsed = self.parse(png, Plan::Stream { converted_pixel: 0 })?;
        self.dispatch_rows(parsed, sink)
    }

    /// Decodes a PNG one scanline at a time as tightly packed 8-bit RGBA.
    ///
    /// The conversion happens as each row is reconstructed, so the concatenated rows equal
    /// [`to_rgba8`](Image::to_rgba8) on the whole image without the whole-image buffer
    /// existing: sub-byte grey levels are scaled to the full range, 16-bit samples are
    /// truncated to their high byte, palette indices are resolved, and `tRNS` becomes real
    /// alpha. Rows are `width * 4` bytes, interleaved, in file order.
    ///
    /// Neither the memory a whole-image decode needs nor its size ceiling applies: see
    /// [`decode_to`](Self::decode_to). The ceiling bounds the stage and the one converted
    /// row instead of the image.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let png = std::fs::read("asset.png")?;
    /// let mut decoder = psd_png::Decoder::new();
    /// decoder.decode_to_rgba8(&png, |row: psd_png::Row<'_>| {
    ///     // Always four bytes per pixel, filters reversed, palette and tRNS resolved.
    ///     consume(row.index, row.bytes);
    ///     Ok::<(), psd_png::Error>(())
    /// })?;
    /// # fn consume(_index: usize, _bytes: &[u8]) {}
    /// # Ok(())
    /// # }
    /// ```
    pub fn decode_to_rgba8<E, F>(&mut self, png: &[u8], sink: F) -> Result<(), E>
    where
        F: FnMut(Row<'_>) -> Result<(), E>,
        E: From<Error>,
    {
        self.decode_to_converted::<u8, 4, E, F>(png, sink)
    }

    /// Decodes a PNG one scanline at a time as tightly packed 8-bit RGB, dropping alpha.
    ///
    /// As [`decode_to_rgba8`](Self::decode_to_rgba8), with three bytes per pixel: a
    /// grayscale source still expands to three equal channels, and a `tRNS` match is
    /// resolved and then discarded with the rest of the alpha.
    pub fn decode_to_rgb8<E, F>(&mut self, png: &[u8], sink: F) -> Result<(), E>
    where
        F: FnMut(Row<'_>) -> Result<(), E>,
        E: From<Error>,
    {
        self.decode_to_converted::<u8, 3, E, F>(png, sink)
    }

    /// Decodes a PNG one scanline at a time as tightly packed 16-bit RGBA.
    ///
    /// A 16-bit source is passed through untouched, so a consumer whose samples are
    /// [`u16`] keeps every bit the file had. A narrower source is scaled up to fill the
    /// 16-bit range rather than sitting in the bottom of it: an 8-bit sample `b` becomes
    /// `b * 257`, and 1-, 2- and 4-bit samples reach the top of the range exactly. Rows are
    /// `width * 4` big-endian `u16` samples, interleaved, in file order.
    pub fn decode_to_rgba16<E, F>(&mut self, png: &[u8], sink: F) -> Result<(), E>
    where
        F: FnMut(Row<'_>) -> Result<(), E>,
        E: From<Error>,
    {
        self.decode_to_converted::<u16, 4, E, F>(png, sink)
    }

    /// Decodes a PNG one scanline at a time as tightly packed 16-bit RGB, dropping alpha.
    ///
    /// As [`decode_to_rgba16`](Self::decode_to_rgba16), with three big-endian [`u16`]
    /// samples per pixel.
    pub fn decode_to_rgb16<E, F>(&mut self, png: &[u8], sink: F) -> Result<(), E>
    where
        F: FnMut(Row<'_>) -> Result<(), E>,
        E: From<Error>,
    {
        self.decode_to_converted::<u16, 3, E, F>(png, sink)
    }

    /// The shared body of the converting streaming methods.
    ///
    /// The image's colour facts are resolved once, each row is converted into a scratch
    /// buffer reused for the whole image, and routing is left to
    /// [`dispatch_rows`](Self::dispatch_rows) so the interlaced fallback, the segment
    /// driver and the sink-error contract are the same code the native path uses. Only one
    /// row of converted output is ever live, whatever the image's height.
    fn decode_to_converted<S: RowSample, const CHANNELS: usize, E, F>(
        &mut self,
        png: &[u8],
        mut sink: F,
    ) -> Result<(), E>
    where
        F: FnMut(Row<'_>) -> Result<(), E>,
        E: From<Error>,
    {
        let parsed = self.parse(png, Plan::Stream { converted_pixel: CHANNELS * S::WIDTH })?;
        let converter = RowConverter::new(&parsed.info)?;
        // Rows already in the requested layout go to the sink as they are: copying each one
        // into a scratch row first would only move bytes that come out the same.
        if converter.passes_through::<CHANNELS, S>() {
            return self.dispatch_rows(parsed, sink);
        }
        let scratch_len = parsed.info.width as usize * CHANNELS * S::WIDTH;
        let mut scratch =
            zeroed_vec(scratch_len).ok_or(Error::OutOfMemory { bytes: scratch_len })?;

        let mut emit = |row: Row<'_>| -> Result<(), E> {
            converter.convert::<CHANNELS, S>(row.bytes, &mut scratch).map_err(E::from)?;
            sink(Row { index: row.index, bytes: &scratch })
        };
        self.dispatch_rows(parsed, &mut emit)
    }

    /// Routes parsed rows to `sink` in the way the image's interlace mode allows.
    fn dispatch_rows<E, F>(&mut self, parsed: Parsed<'_>, mut sink: F) -> Result<(), E>
    where
        F: FnMut(Row<'_>) -> Result<(), E>,
        E: From<Error>,
    {
        let info = &parsed.info;

        match info.interlacing {
            Interlacing::None => self.stream_rows(parsed, sink),
            Interlacing::Adam7 => {
                // The buffered fallback: streaming an interlaced image needs a
                // caller-owned scatter target, which this API does not have.
                let image = self.decode_parsed(parsed)?;
                let row_bytes = image.info.row_bytes();
                for index in 0..image.info.height as usize {
                    sink(Row {
                        index,
                        bytes: &image.data[index * row_bytes..(index + 1) * row_bytes],
                    })?;
                }
                Ok(())
            }
        }
    }

    /// The streaming core of [`decode_to`](Self::decode_to) for non-interlaced images.
    ///
    /// The stage holds the match window plus a segment of filtered rows; the driver slides
    /// the window by one segment per [`Filled`](crate::inflate::SegmentOutcome) outcome,
    /// extending a segment by up to two rows when the frontier has not caught up to the
    /// slide point yet (see `filter::StreamingFrontier::done_floor`).
    fn stream_rows<E, F>(&mut self, parsed: Parsed<'_>, mut sink: F) -> Result<(), E>
    where
        F: FnMut(Row<'_>) -> Result<(), E>,
        E: From<Error>,
    {
        let info = parsed.info;
        let row_bytes = info.row_bytes();
        let height = info.height as usize;
        let window = crate::filter::MAX_MATCH_DISTANCE;

        let StageLayout { pitch, cap, len: stage_len } = StageLayout::new(row_bytes);
        // The furthest any segment may inflate to: the stage less the copy slack.
        let max_budget = stage_len - OUTPUT_SLACK;
        let mut stage = zeroed_vec(stage_len).ok_or(Error::OutOfMemory { bytes: stage_len })?;

        let mut wrapped =
            |index: usize, bytes: &[u8]| -> Result<(), E> { sink(Row { index, bytes }) };
        let mut frontier = crate::filter::StreamingFrontier::new(
            row_bytes,
            height,
            info.filter_stride(),
            &mut wrapped,
        );

        let data: &[u8] = match &parsed.idat {
            IdatData::Contiguous(data) => data,
            IdatData::Joined(data) => data,
        };

        let verify_adler = self.checks == Checks::Full;
        let mut adler = verify_adler.then(crate::adler32::Adler32::new);
        let mut pause: Option<crate::inflate::SegmentPause> = None;
        let mut base = 0usize;
        let mut budget = window + cap;
        let mut seg_start = 0usize;
        let mut resume_at: Option<usize> = None;
        // Consecutive segments that ended where the previous one did.
        let mut stalls = 0u32;
        let mut last_written = 0usize;
        let written_total;

        loop {
            frontier.set_base(base);
            // The Adler-32 covers the filtered bytes, and the frontier rewrites them in
            // place as it goes, so the hash has to run inside the hook — ahead of each
            // reconstruction — rather than over the stage once the segment returns. The
            // tail after the hook's last call is still filtered, and is hashed before the
            // slide can move it.
            let (end, consumed) = match adler.as_mut() {
                Some(checksum) => {
                    let mut hook = ChecksumHook::new(frontier, checksum, seg_start);
                    let outcome = self
                        .inflater
                        .zlib_segment(data, &mut stage, budget, resume_at, &mut pause, &mut hook)
                        .map_err(|error| E::from(Error::from(error)))?;
                    let (end, consumed) = match outcome {
                        crate::inflate::SegmentOutcome::Filled { written } => (written, None),
                        crate::inflate::SegmentOutcome::StreamEnd { written, consumed } => {
                            (written, Some(consumed))
                        }
                    };
                    hook.hash_rest(&stage, end);
                    frontier = hook.into_inner();
                    (end, consumed)
                }
                None => {
                    let outcome = self
                        .inflater
                        .zlib_segment(
                            data,
                            &mut stage,
                            budget,
                            resume_at,
                            &mut pause,
                            &mut frontier,
                        )
                        .map_err(|error| E::from(Error::from(error)))?;
                    match outcome {
                        crate::inflate::SegmentOutcome::Filled { written } => (written, None),
                        crate::inflate::SegmentOutcome::StreamEnd { written, consumed } => {
                            (written, Some(consumed))
                        }
                    }
                }
            };
            if let Some(error) = frontier.take_error() {
                return Err(error);
            }
            if let Some(row) = frontier.failed_row() {
                return Err(Error::InvalidFilter { row }.into());
            }

            if let Some(consumed) = consumed {
                if verify_adler {
                    let trailer = &data[2 + consumed..];
                    if trailer.len() < 4 {
                        return Err(Error::from(InflateError::UnexpectedEof).into());
                    }
                    let expected =
                        u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
                    if adler.is_none_or(|a| a.finish() != expected) {
                        return Err(Error::from(InflateError::WrongChecksum).into());
                    }
                }
                written_total = end;
                break;
            }

            let abs_written = base + end;
            // A segment can legitimately end without new output (a stored block waiting
            // for room, or a segment extended by a row), but only a couple of times in a
            // row: the slide and the runway below always make room. Should the sizing
            // ever fail to, report a stream that does not fit rather than spin.
            if abs_written == last_written {
                stalls += 1;
                if stalls > 4 {
                    return Err(Error::from(InflateError::OutputOverflow).into());
                }
            } else {
                stalls = 0;
                last_written = abs_written;
            }

            let floor = frontier.done_floor();
            if floor >= base + pitch {
                // Slide, but keep the reconstructed lookback row (row `rows_done - 1`)
                // above the next reconstruction: row r reads its predecessor at one pitch
                // below its first byte. The match window rides along automatically,
                // because the frontier trails the cursor by the window plus less than a
                // row: done_floor >= abs_written - window - pitch + 1, so the retained
                // region [new_base, abs_written) is at least the window and less than the
                // window plus two rows.
                //
                // That bound holds because `zlib_segment` reports the paused position to
                // the frontier before returning `Filled`. Were the frontier left at the
                // top of the last decode iteration, it could trail by a whole match (up to
                // 258 bytes), and for rows narrower than about 130 bytes the retained
                // region would outgrow the four rows of headroom the stage carries.
                let new_base = (floor - pitch + 1).min(abs_written);
                let delta = new_base - base;
                stage.copy_within(delta..end, 0);
                base = new_base;
                let resume = end - delta;
                seg_start = resume;
                resume_at = Some(resume);
                debug_assert!(resume + cap <= max_budget, "retained region outgrew the stage");
                budget = (resume + cap).min(max_budget);
            } else {
                // Row zero (or a row wider than the retained region) is not reconstructed
                // yet; nothing can be dropped. Extend the segment by up to four rows of
                // runway, by which point readiness always arrives.
                budget = (budget + 2 * pitch).min(max_budget);
                seg_start = end;
                resume_at = Some(end);
            }
        }

        if base + written_total != info.decompressed_size() {
            return Err(Error::from(InflateError::OutputUnderflow).into());
        }
        frontier.finish(&mut stage, written_total);
        if let Some(error) = frontier.take_error() {
            return Err(error);
        }
        if let Some(row) = frontier.failed_row() {
            return Err(Error::InvalidFilter { row }.into());
        }
        Ok(())
    }

    fn decode_parsed(&mut self, parsed: Parsed<'_>) -> Result<Image, Error> {
        let Parsed { info, idat } = parsed;

        self.inflater.verify_checksum(self.checks == Checks::Full);
        let request = info.decompressed_size() + OUTPUT_SLACK;
        let mut buffer = zeroed_vec(request).ok_or(Error::OutOfMemory { bytes: request })?;

        let data = match info.interlacing {
            Interlacing::None => {
                let row_bytes = info.row_bytes();
                let height = info.height as usize;

                // Below one match window plus a row, the frontier's first-row trigger is
                // past the end of the stream: reconstruction cannot begin before inflation
                // finishes, so the fused path would be the two-pass path plus its own
                // polling. Take the plain path, which is then identical to the unfused
                // decoder.
                if info.decompressed_size() <= crate::filter::MAX_MATCH_DISTANCE + 1 + row_bytes {
                    match &idat {
                        IdatData::Contiguous(data) => self.inflater.zlib(data, &mut buffer)?,
                        IdatData::Joined(data) => self.inflater.zlib(data, &mut buffer)?,
                    };
                    unfilter_image(&mut buffer, row_bytes, height, info.filter_stride())
                        .map_err(|row| Error::InvalidFilter { row })?;
                } else {
                    // Reverse the scanline filters as inflation writes, behind its match
                    // window, so the second pass walks cache-hot rows instead of the whole
                    // cold image. See `filter::ReconstructionFrontier`.
                    let mut frontier = crate::filter::ReconstructionFrontier::new(
                        row_bytes,
                        height,
                        info.filter_stride(),
                    );
                    let written = match &idat {
                        IdatData::Contiguous(data) => {
                            self.inflater.zlib_progress(data, &mut buffer, &mut frontier)?
                        }
                        IdatData::Joined(data) => {
                            self.inflater.zlib_progress(data, &mut buffer, &mut frontier)?
                        }
                    };
                    frontier.finish(&mut buffer, written);
                    if let Some(row) = frontier.failed_row() {
                        return Err(Error::InvalidFilter { row });
                    }
                }
                buffer.truncate(info.output_size());
                buffer
            }
            Interlacing::Adam7 => {
                match &idat {
                    IdatData::Contiguous(data) => self.inflater.zlib(data, &mut buffer)?,
                    IdatData::Joined(data) => self.inflater.zlib(data, &mut buffer)?,
                };
                deinterlace(&info, &mut buffer)?
            }
        };

        if info.color_type == ColorType::Indexed {
            let palette = info.palette.as_ref().ok_or(Error::MissingPalette)?;
            validate_palette_indices(&data, &info, palette.len() / 3)?;
        }

        Ok(Image { info, data })
    }

    fn parse<'a>(&self, png: &'a [u8], plan: Plan) -> Result<Parsed<'a>, Error> {
        let (mut info, mut chunks) = open(png, self.checks)?;

        // Checked here rather than at the allocation, so a header naming a petabyte costs
        // the thirty-three bytes already read and not a scan of whatever follows it.
        if let Some(limit) = self.max_decompressed_size {
            let size = plan.largest_buffer(&info);
            if size > limit {
                return Err(Error::SizeLimitExceeded { size, limit });
            }
        }

        // The first IDAT is remembered separately so that the common single-chunk case can
        // borrow the compressed bytes instead of copying them.
        let mut first_idat: Option<&[u8]> = None;
        let mut joined: Option<Vec<u8>> = None;

        while let Some(chunk) = chunks.next()? {
            match &chunk.kind {
                b"IEND" => break,
                b"IDAT" => match (&mut joined, first_idat) {
                    (Some(buffer), _) => buffer.extend_from_slice(chunk.data),
                    (None, Some(first)) => {
                        let mut buffer = Vec::with_capacity(first.len() * 2 + chunk.data.len());
                        buffer.extend_from_slice(first);
                        buffer.extend_from_slice(chunk.data);
                        joined = Some(buffer);
                    }
                    (None, None) => first_idat = Some(chunk.data),
                },
                _ => absorb(&mut info, &chunk, first_idat.is_none())?,
            }
        }

        if info.color_type == ColorType::Indexed && info.palette.is_none() {
            return Err(Error::MissingPalette);
        }

        let idat = match joined {
            Some(buffer) => IdatData::Joined(buffer),
            None => IdatData::Contiguous(first_idat.ok_or(Error::MissingImageData)?),
        };

        Ok(Parsed { info, idat })
    }

    /// Reads everything a file says about its image, without decoding any of it.
    ///
    /// Parsing stops at the first `IDAT`, so the cost is the header and whatever colour
    /// chunks precede the image data, and nothing is allocated whose size depends on the
    /// dimensions. That makes this the seam for a caller who wants to decide something about
    /// an image before committing to decoding it, whether that is a size policy stricter
    /// than [`max_decompressed_size`](Self::max_decompressed_size), a colour type it has no
    /// use for, or simply the dimensions.
    ///
    /// Everything the decode would read before the pixels is present, `PLTE` and `tRNS`
    /// included, so [`Info::has_alpha`] answers correctly here. What is absent is any
    /// ancillary chunk that trails the image data, since reaching one would mean reading
    /// the whole file.
    ///
    /// Only the chunks up to the first `IDAT` are examined, so this is the weaker check of
    /// the two in both directions. Nothing that only the pixels could contradict is caught,
    /// such as a palette an index runs past; neither is a structural fault in the tail of
    /// the file, such as an unknown critical chunk, a bad CRC, or a truncated trailer after
    /// the image data. A header this accepts can still fail to decode. A file this rejects
    /// would never have decoded.
    ///
    /// ```no_run
    /// # let png: Vec<u8> = Vec::new(); // pretend this came from a file
    /// let info = psd_png::Decoder::new().read_info(&png)?;
    /// assert_eq!((info.width, info.height), (2, 2));
    /// assert!(info.has_alpha());
    /// # Ok::<(), psd_png::Error>(())
    /// ```
    pub fn read_info(&self, png: &[u8]) -> Result<Info, Error> {
        read_header(png, self.checks)
    }
}

/// Reads the signature and `IHDR`, leaving the reader positioned on the chunk after them.
///
/// Every entry point starts this way, and the two that follow it differ only in what they do
/// with the rest of the file, so sharing the prologue is what keeps them from drifting apart
/// on what counts as a valid header.
fn open(png: &[u8], checks: Checks) -> Result<(Info, ChunkReader<'_>), Error> {
    if png.len() < SIGNATURE.len() || png[..SIGNATURE.len()] != SIGNATURE {
        return Err(Error::NotAPng);
    }

    let mut chunks =
        ChunkReader { data: png, pos: SIGNATURE.len(), verify: checks != Checks::None };

    let header = chunks.next()?.ok_or(Error::MissingHeader)?;
    if &header.kind != b"IHDR" {
        return Err(Error::MissingHeader);
    }
    Ok((parse_ihdr(header.data)?, chunks))
}

/// Folds one chunk that is neither `IDAT` nor `IEND` into the header being built.
///
/// `before_idat` says whether the image data has started. `PLTE` and `tRNS` that follow it
/// are dropped rather than recorded: the specification puts both ahead of the image data,
/// and honouring a late one would mean the pixels depended on an ordering this decoder does
/// not otherwise respect.
fn absorb(info: &mut Info, chunk: &RawChunk<'_>, before_idat: bool) -> Result<(), Error> {
    match &chunk.kind {
        b"PLTE" => {
            if chunk.data.len() > 256 * 3 || !chunk.data.len().is_multiple_of(3) {
                return Err(Error::InvalidChunkLength {
                    chunk: chunk.kind,
                    length: chunk.data.len(),
                });
            }
            if before_idat {
                info.palette = Some(chunk.data.to_vec());
            }
        }
        b"tRNS" => {
            if before_idat {
                validate_trns(info, chunk.data)?;
                info.transparency = Some(chunk.data.to_vec());
            }
        }
        kind if is_critical(*kind) => {
            // Critical chunks we do not understand may change how the image should be
            // interpreted, so decoding cannot safely continue.
            return Err(Error::UnknownCriticalChunk { chunk: *kind });
        }
        _ => {}
    }
    Ok(())
}

/// Reads the header and the colour chunks ahead of the image data. See
/// [`Decoder::read_info`].
///
/// Free of the [`Decoder`] so that the standalone [`read_info`] can call it without building
/// one: a `Decoder` owns an [`Inflater`], and reading a header should not cost the twenty
/// kilobytes of Huffman tables that decoding one does.
fn read_header(png: &[u8], checks: Checks) -> Result<Info, Error> {
    let (mut info, mut chunks) = open(png, checks)?;

    while let Some(chunk) = chunks.next()? {
        match &chunk.kind {
            b"IDAT" => {
                if info.color_type == ColorType::Indexed && info.palette.is_none() {
                    return Err(Error::MissingPalette);
                }
                return Ok(info);
            }
            b"IEND" => break,
            _ => absorb(&mut info, &chunk, true)?,
        }
    }

    Err(Error::MissingImageData)
}

/// Which buffers a decode is about to allocate, so that [`Decoder::parse`] can hold the
/// largest of them to the ceiling before reading past the header.
#[derive(Clone, Copy)]
enum Plan {
    /// [`Decoder::decode`]: the whole decompressed image.
    Whole,
    /// A streaming decode. `converted_pixel` is the bytes per pixel of a converted row, or
    /// zero when rows go out in the file's own layout and no scratch row exists.
    Stream { converted_pixel: usize },
}

impl Plan {
    /// Bytes in the largest single buffer this decode allocates for `info`.
    fn largest_buffer(self, info: &Info) -> usize {
        match self {
            Plan::Whole => info.decompressed_size(),
            Plan::Stream { converted_pixel } => {
                // An interlaced image falls back to a whole-image decode (see
                // `dispatch_rows`), so it needs what `decode` needs.
                let working = match info.interlacing {
                    Interlacing::None => StageLayout::new(info.row_bytes()).len,
                    Interlacing::Adam7 => info.decompressed_size(),
                };
                let scratch = (info.width as usize).saturating_mul(converted_pixel);
                working.max(scratch)
            }
        }
    }
}

/// How the streaming decoder sizes its stage for rows of a given width.
///
/// The stage holds the match window, a segment of filtered rows, and four rows of headroom
/// for the slide to keep. Its size depends on the row width and not on the height, so it is
/// the memory a streaming decode needs whatever the image's height — and, for a very wide
/// image, what the size ceiling has to bound.
struct StageLayout {
    /// Bytes per filtered row, filter byte included.
    pitch: usize,
    /// Bytes of filtered rows one segment inflates beyond the retained region.
    cap: usize,
    /// Bytes in the whole stage, [`OUTPUT_SLACK`] included.
    len: usize,
}

impl StageLayout {
    fn new(row_bytes: usize) -> Self {
        const SEGMENT_TARGET: usize = 256 * 1024;

        let pitch = row_bytes.saturating_add(1);
        let rows_per_segment = (SEGMENT_TARGET / pitch).max(1);
        // A segment after a slide must be able to hold a whole stored block (at most
        // 65535 bytes) beyond the retained region, however narrow the rows are.
        let cap = (rows_per_segment * pitch).max(pitch.saturating_add(65536));
        // The retained region is the match window plus less than two rows (see the slide
        // in `stream_rows`); four rows of headroom cover it and the segment extension.
        let len = crate::filter::MAX_MATCH_DISTANCE
            .saturating_add(cap)
            .saturating_add(pitch.saturating_mul(4))
            .saturating_add(OUTPUT_SLACK);
        Self { pitch, cap, len }
    }
}

struct Parsed<'a> {
    info: Info,
    idat: IdatData<'a>,
}

enum IdatData<'a> {
    /// A single `IDAT` chunk, borrowed straight from the input.
    Contiguous(&'a [u8]),
    /// Several `IDAT` chunks, concatenated.
    Joined(Vec<u8>),
}

/// A chunk borrowed from the input, as the reader yields it.
struct RawChunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
}

struct ChunkReader<'a> {
    data: &'a [u8],
    pos: usize,
    verify: bool,
}

impl<'a> ChunkReader<'a> {
    /// Yields the next chunk that passes its CRC.
    ///
    /// A bad CRC on a critical chunk is an error, because the image depends on it. On an
    /// ancillary chunk it is not: nothing the decoder produces depends on one, and files
    /// exist whose pixels are perfectly intact but whose colour profile or text metadata was
    /// rewritten without recomputing its checksum. Refusing those would lose an image over
    /// four stale bytes describing something that is discarded anyway, so the chunk is
    /// dropped and reading continues, as libpng and the `png` crate both do.
    ///
    /// Skipping still trusts the chunk's length field to find the next header, since the CRC
    /// does not cover it. A length corrupted along with the body resynchronises somewhere
    /// arbitrary, which then fails as a truncated or unknown critical chunk.
    fn next(&mut self) -> Result<Option<RawChunk<'a>>, Error> {
        loop {
            if self.pos == self.data.len() {
                return Ok(None);
            }
            if self.pos + 8 > self.data.len() {
                return Err(Error::TruncatedChunk);
            }

            let length = u32::from_be_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
            // The specification caps chunk lengths at 2^31 - 1.
            if length > i32::MAX as u32 {
                return Err(Error::TruncatedChunk);
            }
            let length = length as usize;

            let kind: [u8; 4] = self.data[self.pos + 4..self.pos + 8].try_into().unwrap();
            let body_start = self.pos + 8;
            let end = body_start + length + 4;
            if end > self.data.len() {
                return Err(Error::TruncatedChunk);
            }

            let data = &self.data[body_start..body_start + length];
            if self.verify {
                let stored =
                    u32::from_be_bytes(self.data[body_start + length..end].try_into().unwrap());
                if crc32(&self.data[self.pos + 4..body_start + length]) != stored {
                    if is_critical(kind) {
                        return Err(Error::BadChunkCrc { chunk: kind });
                    }
                    self.pos = end;
                    continue;
                }
            }

            self.pos = end;
            return Ok(Some(RawChunk { kind, data }));
        }
    }
}

/// A chunk is critical when the first letter of its type is upper case.
fn is_critical(kind: [u8; 4]) -> bool {
    kind[0].is_ascii_uppercase()
}

fn parse_ihdr(data: &[u8]) -> Result<Info, Error> {
    if data.len() != 13 {
        return Err(Error::InvalidChunkLength { chunk: *b"IHDR", length: data.len() });
    }

    let width = u32::from_be_bytes(data[0..4].try_into().unwrap());
    let height = u32::from_be_bytes(data[4..8].try_into().unwrap());
    let bit_depth = BitDepth::from_byte(data[8])
        .ok_or(Error::InvalidBitDepth { color_type: data[9], bit_depth: data[8] })?;
    let color_type = ColorType::from_byte(data[9])?;

    if data[10] != 0 {
        return Err(Error::UnsupportedMethod { field: "compression method", value: data[10] });
    }
    if data[11] != 0 {
        return Err(Error::UnsupportedMethod { field: "filter method", value: data[11] });
    }
    let interlacing = match data[12] {
        0 => Interlacing::None,
        1 => Interlacing::Adam7,
        other => return Err(Error::InvalidInterlaceMethod(other)),
    };

    let mut info = Info::new(width, height, color_type, bit_depth);
    info.interlacing = interlacing;
    info.validate()?;
    Ok(info)
}

/// Checks a `tRNS` chunk against the colour type it accompanies.
///
/// Shared with the encoder, which must not write a chunk it would itself reject.
pub(crate) fn validate_trns(info: &Info, data: &[u8]) -> Result<(), Error> {
    let expected = match info.color_type {
        ColorType::Grayscale => 2,
        ColorType::Rgb => 6,
        // For indexed images tRNS gives one alpha per palette entry, and may be short.
        ColorType::Indexed => {
            if data.len() > 256 {
                return Err(Error::InvalidChunkLength { chunk: *b"tRNS", length: data.len() });
            }
            return Ok(());
        }
        ColorType::GrayscaleAlpha | ColorType::Rgba => {
            return Err(Error::InvalidChunkLength { chunk: *b"tRNS", length: data.len() });
        }
    };
    if data.len() != expected {
        return Err(Error::InvalidChunkLength { chunk: *b"tRNS", length: data.len() });
    }
    Ok(())
}

/// Rejects palette indices with no matching `PLTE` entry, so later conversions can index the
/// palette without a per-pixel range check.
fn validate_palette_indices(data: &[u8], info: &Info, entries: usize) -> Result<(), Error> {
    if entries == 0 {
        return Err(Error::MissingPalette);
    }
    match info.bit_depth {
        BitDepth::Eight => {
            if data.iter().any(|&index| index as usize >= entries) {
                return Err(Error::PaletteIndexOutOfRange);
            }
        }
        depth => {
            // Sub-byte indices can only exceed the palette if the palette is smaller than
            // the depth's full range, which is rare enough to check the cheap way first.
            let max = (1usize << depth.bits()) - 1;
            if max >= entries {
                let width = info.width as usize;
                let bits = depth.bits();
                let row_bytes = info.row_bytes();
                for row in 0..info.height as usize {
                    let line = &data[row * row_bytes..(row + 1) * row_bytes];
                    for x in 0..width {
                        let bit = x * bits;
                        let index = (line[bit / 8] >> (8 - bits - bit % 8)) & max as u8;
                        if index as usize >= entries {
                            return Err(Error::PaletteIndexOutOfRange);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Unfilters each Adam7 pass in place and scatters the passes into a single image.
fn deinterlace(info: &Info, buffer: &mut [u8]) -> Result<Vec<u8>, Error> {
    let width = info.width as usize;
    let height = info.height as usize;
    let bits = info.bits_per_pixel();
    let row_bytes = info.row_bytes();
    let stride = info.filter_stride();

    let request = row_bytes * height;
    let mut image = zeroed_vec(request).ok_or(Error::OutOfMemory { bytes: request })?;
    let mut offset = 0usize;
    let mut row_counter = 0usize;

    for (pass, &(x_start, y_start, x_step, y_step)) in ADAM7_PASSES.iter().enumerate() {
        let (pass_width, pass_height) = adam7_pass_size(pass, width, height);
        if pass_width == 0 || pass_height == 0 {
            continue;
        }
        let pass_row_bytes = row_bytes_for(pass_width, bits);
        let region = &mut buffer[offset..offset + pass_height * (1 + pass_row_bytes)];

        unfilter_image(region, pass_row_bytes, pass_height, stride)
            .map_err(|row| Error::InvalidFilter { row: row_counter + row })?;

        for row in 0..pass_height {
            let source = &region[row * pass_row_bytes..(row + 1) * pass_row_bytes];
            let target_row = y_start + row * y_step;
            let target = &mut image[target_row * row_bytes..(target_row + 1) * row_bytes];
            scatter_row(source, target, pass_width, x_start, x_step, bits);
        }

        offset += pass_height * (1 + pass_row_bytes);
        row_counter += pass_height;
    }

    Ok(image)
}

/// Writes the pixels of one interlace pass row into their positions in the full row.
fn scatter_row(
    source: &[u8],
    target: &mut [u8],
    pass_width: usize,
    x_start: usize,
    x_step: usize,
    bits: usize,
) {
    if bits >= 8 {
        let pixel = bits / 8;
        for k in 0..pass_width {
            let to = (x_start + k * x_step) * pixel;
            target[to..to + pixel].copy_from_slice(&source[k * pixel..(k + 1) * pixel]);
        }
    } else {
        let mask = (1u8 << bits) - 1;
        for k in 0..pass_width {
            let from_bit = k * bits;
            let value = (source[from_bit / 8] >> (8 - bits - from_bit % 8)) & mask;
            let to_bit = (x_start + k * x_step) * bits;
            let shift = 8 - bits - to_bit % 8;
            let slot = &mut target[to_bit / 8];
            *slot = (*slot & !(mask << shift)) | (value << shift);
        }
    }
}

/// Decodes a PNG into its native pixel format.
pub fn decode(png: &[u8]) -> Result<Image, Error> {
    Decoder::new().decode(png)
}

/// Reads a PNG's header and colour chunks without decoding its pixels.
///
/// See [`Decoder::read_info`].
pub fn read_info(png: &[u8]) -> Result<Info, Error> {
    read_header(png, Checks::Crc)
}
