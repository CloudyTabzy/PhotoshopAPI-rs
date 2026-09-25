//! PNG scanline filters (RFC 2083 section 6).
//!
//! Each scanline is prefixed by a filter byte selecting one of five predictors. Decoding
//! reverses the predictor; encoding applies it. Both directions are specialized on the pixel
//! stride with a const generic, because the stride is what determines the serial dependency
//! distance and therefore how the loops schedule.

/// The five filter types a scanline may use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Filter {
    /// No prediction; the bytes are stored as they are.
    None = 0,
    /// Predicts from the byte one pixel to the left.
    Sub = 1,
    /// Predicts from the byte directly above.
    Up = 2,
    /// Predicts from the mean of the left and upper bytes.
    Average = 3,
    /// Predicts from whichever of left, upper and upper-left is nearest their linear
    /// estimate, which follows an edge rather than smearing across it.
    Paeth = 4,
}

impl Filter {
    /// Every filter, in the order their type bytes are numbered.
    pub const ALL: [Filter; 5] =
        [Filter::None, Filter::Sub, Filter::Up, Filter::Average, Filter::Paeth];

    /// Reads a filter from a scanline's prefix byte, which is the discriminant itself.
    /// `None` for anything outside `0..=4`.
    #[inline]
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Filter::None),
            1 => Some(Filter::Sub),
            2 => Some(Filter::Up),
            3 => Some(Filter::Average),
            4 => Some(Filter::Paeth),
            _ => None,
        }
    }
}

/// The Paeth predictor: whichever of the three neighbours the linear estimate `a + b - c`
/// lands closest to.
///
/// Written as a running minimum rather than the specification's nested comparison. The two
/// forms select the same neighbour, including at ties, but this one is a chain of compares
/// and selects with no branches, and maps onto vector min/select when the surrounding loop
/// gets unrolled across a pixel.
#[inline(always)]
pub(crate) fn paeth(a: i16, b: i16, c: i16) -> u8 {
    // `p - a` is `b - c` and `p - b` is `a - c`, so the three distances need only two
    // differences between them.
    let da = b - c;
    let db = a - c;
    let pa = da.abs();
    let pb = db.abs();
    let pc = (da + db).abs();

    let mut nearest = a;
    let mut smallest = pa;
    if pb < smallest {
        nearest = b;
        smallest = pb;
    }
    if pc < smallest {
        nearest = c;
    }
    nearest as u8
}

/// The Paeth predictor over plain bytes, for callers outside the filter loops.
#[inline(always)]
pub fn paeth_predictor(left: u8, above: u8, upper_left: u8) -> u8 {
    paeth(left as i16, above as i16, upper_left as i16)
}

// ---------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------
//
// Every reconstruction loop below walks a whole pixel at a time and carries the neighbours
// it needs in fixed-size arrays. The obvious formulation, `row[i] += row[i - stride]`,
// reloads a byte the previous iteration has just stored, and store-to-load forwarding makes
// that dependency several cycles long; keeping the pixel in registers reduces it to a single
// add, and lets the compiler unroll the per-channel work across the pixel.
//
// A scanline's length is always a whole number of strides: for bit depths of 8 and above the
// stride is the pixel size, and below that the stride is one byte.

/// Reverses the filter on the first scanline of an image or interlace pass.
///
/// The row above is defined to be all zeros, which collapses `Up` and `None` to no-ops and
/// reduces `Paeth` to `Sub`.
fn unfilter_first_row<const BPP: usize>(filter: Filter, row: &mut [u8]) {
    match filter {
        Filter::None | Filter::Up => {}
        Filter::Sub | Filter::Paeth => {
            let mut left = [0u8; BPP];
            for pixel in row.as_chunks_mut::<BPP>().0 {
                for k in 0..BPP {
                    left[k] = pixel[k].wrapping_add(left[k]);
                }
                pixel.copy_from_slice(&left);
            }
        }
        Filter::Average => {
            let mut left = [0u8; BPP];
            for pixel in row.as_chunks_mut::<BPP>().0 {
                for k in 0..BPP {
                    left[k] = pixel[k].wrapping_add(left[k] >> 1);
                }
                pixel.copy_from_slice(&left);
            }
        }
    }
}

/// Reverses the filter on a scanline, given the already-reconstructed row above it.
///
/// Each filter reconstructs into `left`, its own running left neighbour, and copies that out
/// to the row. Every lane reads only its own index, so writing in place is sound, and it
/// matters: a version building each pixel in a separate temporary and assigning that to
/// `left` reconstructs a stride-8 row at an eighth of the speed, LLVM taking the assignment
/// for a memory move rather than a register one.
fn unfilter_row<const BPP: usize>(filter: Filter, prev: &[u8], row: &mut [u8]) {
    debug_assert_eq!(prev.len(), row.len());

    match filter {
        Filter::None => {}
        Filter::Sub => {
            let mut left = [0u8; BPP];
            for pixel in row.as_chunks_mut::<BPP>().0 {
                for k in 0..BPP {
                    left[k] = pixel[k].wrapping_add(left[k]);
                }
                pixel.copy_from_slice(&left);
            }
        }
        Filter::Up => {
            // No serial dependency at all, so this vectorizes to a plain packed add.
            for (x, &b) in row.iter_mut().zip(prev.iter()) {
                *x = x.wrapping_add(b);
            }
        }
        Filter::Average => {
            let mut left = [0u8; BPP];
            for (pixel, above) in
                row.as_chunks_mut::<BPP>().0.iter_mut().zip(prev.as_chunks::<BPP>().0)
            {
                for k in 0..BPP {
                    let sum = left[k] as u16 + above[k] as u16;
                    left[k] = pixel[k].wrapping_add((sum >> 1) as u8);
                }
                pixel.copy_from_slice(&left);
            }
        }
        Filter::Paeth => {
            // The SIMD kernel takes the whole row when it claims this stride; it declines
            // otherwise, leaving the row untouched for the scalar path below.
            if crate::simd::paeth_row(row, prev, BPP) {
                return;
            }
            let mut left = [0u8; BPP];
            let mut upper_left = [0u8; BPP];
            for (pixel, above) in
                row.as_chunks_mut::<BPP>().0.iter_mut().zip(prev.as_chunks::<BPP>().0)
            {
                for k in 0..BPP {
                    left[k] = pixel[k].wrapping_add(paeth(
                        left[k] as i16,
                        above[k] as i16,
                        upper_left[k] as i16,
                    ));
                }
                pixel.copy_from_slice(&left);
                upper_left.copy_from_slice(above);
            }
        }
    }
}

/// One step of the two-row Paeth wavefront: both predictions, from state settled before
/// the step began.
///
/// The four operands are laid out one array per role rather than one per lane so that the
/// two lanes sit adjacent in memory. Both loop bounds are constants, so this unrolls into
/// `2 * BPP` independent lane computations that pack into single vector operations.
#[inline(always)]
fn paeth_step<const BPP: usize>(
    residual: &[[u8; BPP]; 2],
    left: &[[u8; BPP]; 2],
    up: &[[u8; BPP]; 2],
    upper_left: &[[u8; BPP]; 2],
) -> [[u8; BPP]; 2] {
    let mut value = [[0u8; BPP]; 2];
    for lane in 0..2 {
        for k in 0..BPP {
            value[lane][k] = residual[lane][k].wrapping_add(paeth(
                left[lane][k] as i16,
                up[lane][k] as i16,
                upper_left[lane][k] as i16,
            ));
        }
    }
    value
}

/// Reverses `Paeth` on two adjacent rows at once, as a wavefront one pixel deep.
///
/// Reconstruction is serial along a row, and the Paeth predictor is a long enough chain of
/// compares and selects that a single row leaves the machine mostly waiting rather than
/// working: the loop is bound by the latency of that chain, not by its throughput. Taking
/// the two rows on an anti-diagonal fills the idle slots. Row `r` pixel `x` needs row `r`
/// pixel `x - 1`, and row `r + 1` pixel `x - 1` needs row `r + 1` pixel `x - 2` along with
/// row `r` pixels `x - 1` and `x - 2`; every one of those settled before this step, so the
/// two predictions are independent of each other and issue together.
///
/// `above` is the reconstructed row `r - 1`. `first` and `second` hold rows `r` and `r + 1`,
/// filtered on entry and reconstructed on return.
fn unfilter_paeth_pair<const BPP: usize>(above: &[u8], first: &mut [u8], second: &mut [u8]) {
    debug_assert_eq!(above.len(), first.len());
    debug_assert_eq!(first.len(), second.len());

    let count = first.len() / BPP;
    if count == 0 {
        return;
    }

    // Lane 0 is row `r` at pixel `x`, lane 1 is row `r + 1` at pixel `x - 1`.
    let mut left = [[0u8; BPP]; 2];
    let mut up = [[0u8; BPP]; 2];
    let mut upper_left = [[0u8; BPP]; 2];
    let mut residual = [[0u8; BPP]; 2];

    // Row `r` pixel `x - 2`: the upper-left neighbour of lane 1, one step further back than
    // `left[0]` and so not recoverable from it.
    let mut behind = [0u8; BPP];

    // Every neighbour off the left edge or above the first row is defined to be zero, and
    // `paeth(0, b, 0)` is `b`, so the edges need no special case beyond starting from zeros.
    residual[0].copy_from_slice(&first[..BPP]);
    up[0].copy_from_slice(&above[..BPP]);
    let mut value = paeth_step::<BPP>(&residual, &left, &up, &upper_left);
    first[..BPP].copy_from_slice(&value[0]);
    left[0] = value[0];

    for x in 1..count {
        let at = x * BPP;
        let back = at - BPP;

        residual[0].copy_from_slice(&first[at..at + BPP]);
        residual[1].copy_from_slice(&second[back..at]);
        up[0].copy_from_slice(&above[at..at + BPP]);
        up[1] = left[0];
        upper_left[0].copy_from_slice(&above[back..at]);
        upper_left[1] = behind;

        value = paeth_step::<BPP>(&residual, &left, &up, &upper_left);

        first[at..at + BPP].copy_from_slice(&value[0]);
        second[back..at].copy_from_slice(&value[1]);

        behind = left[0];
        left = value;
    }

    // The second row trails the first by a pixel, so its last one is left over.
    let back = (count - 1) * BPP;
    residual[1].copy_from_slice(&second[back..back + BPP]);
    up[1] = left[0];
    upper_left[1] = behind;
    value = paeth_step::<BPP>(&residual, &left, &up, &upper_left);
    second[back..back + BPP].copy_from_slice(&value[1]);
}

/// Reverses filtering across a whole image and compacts it in place.
///
/// `buffer` holds `height` records of `1 + row_bytes` bytes, exactly as the compressed
/// stream produced them. On return the reconstructed image occupies `buffer[..height *
/// row_bytes]` with the filter bytes removed.
///
/// Each row is first moved down over the filter bytes that precede it, which is always a
/// forward move, and is then reconstructed against the row already sitting immediately
/// before it. That keeps the previous row where the next one needs it without a scratch
/// buffer.
pub fn unfilter_image(
    buffer: &mut [u8],
    row_bytes: usize,
    height: usize,
    bpp: usize,
) -> Result<(), usize> {
    match bpp {
        1 => unfilter_image_bpp::<1>(buffer, row_bytes, height),
        2 => unfilter_image_bpp::<2>(buffer, row_bytes, height),
        3 => unfilter_image_bpp::<3>(buffer, row_bytes, height),
        4 => unfilter_image_bpp::<4>(buffer, row_bytes, height),
        6 => unfilter_image_bpp::<6>(buffer, row_bytes, height),
        8 => unfilter_image_bpp::<8>(buffer, row_bytes, height),
        _ => unreachable!("PNG pixel strides are 1, 2, 3, 4, 6 or 8 bytes"),
    }
}

fn unfilter_image_bpp<const BPP: usize>(
    buffer: &mut [u8],
    row_bytes: usize,
    height: usize,
) -> Result<(), usize> {
    debug_assert!(buffer.len() >= height * (1 + row_bytes));

    let mut row = 0;
    while row < height {
        let filter = Filter::from_byte(buffer[row * (1 + row_bytes)]).ok_or(row)?;

        // Two adjacent `Paeth` rows are worth reconstructing together; see
        // [`unfilter_paeth_pair`]. The first row of the image is excluded because it has no
        // row above it, and a row whose successor uses a different filter falls through to
        // the single-row path, which the next iteration then takes for the successor. A
        // stride the SIMD kernel claims does not pair at all: the kernel reconstructs a whole
        // row on its own, and the two schemes are alternatives rather than a pair.
        let pair = filter == Filter::Paeth
            && !crate::simd::claims_stride(BPP)
            && row > 0
            && row + 1 < height
            && buffer[(row + 1) * (1 + row_bytes)] == Filter::Paeth as u8;

        reconstruct_rows::<BPP>(buffer, row_bytes, row, pair)?;
        row += if pair { 2 } else { 1 };
    }

    Ok(())
}

/// Reconstructs rows `row..` in place, compacting each over its filter byte: one row, or
/// the pair `row, row + 1` through the two-row wavefront when `pair` is set. The caller
/// establishes the pairing conditions; everything else is shared with
/// [`ReconstructionFrontier`], which reconstructs rows behind a live output cursor.
fn reconstruct_rows<const BPP: usize>(
    buffer: &mut [u8],
    row_bytes: usize,
    row: usize,
    pair: bool,
) -> Result<(), usize> {
    let source = row * (1 + row_bytes) + 1;
    let dest = row * row_bytes;
    let filter = Filter::from_byte(buffer[source - 1]).ok_or(row)?;

    if pair {
        let next = (row + 1) * (1 + row_bytes) + 1;
        // In that order: compacting the second row first would overwrite the tail of the
        // first row's filtered bytes, which are still where they were written.
        buffer.copy_within(source..source + row_bytes, dest);
        buffer.copy_within(next..next + row_bytes, dest + row_bytes);

        let (above, rest) = buffer.split_at_mut(dest);
        let (first, second) = rest.split_at_mut(row_bytes);
        unfilter_paeth_pair::<BPP>(&above[dest - row_bytes..], first, &mut second[..row_bytes]);
        Ok(())
    } else {
        buffer.copy_within(source..source + row_bytes, dest);

        if row == 0 {
            unfilter_first_row::<BPP>(filter, &mut buffer[..row_bytes]);
        } else {
            let (above, current) = buffer.split_at_mut(dest);
            unfilter_row::<BPP>(filter, &above[dest - row_bytes..], &mut current[..row_bytes]);
        }
        Ok(())
    }
}

/// Reconstructs rows `row..` where they sit in `buffer`, without compaction: the filter
/// byte stays in place as garbage and the row is reversed over itself. The previous row —
/// at one pitch below the row's first byte — must already be reconstructed.
///
/// `base` is the absolute position of `buffer[0]`; all indexing is absolute minus `base`,
/// which is how the streaming decoder addresses a bounded stage window that slides under
/// a much larger logical stream.
fn reconstruct_in_place<const BPP: usize>(
    buffer: &mut [u8],
    row_bytes: usize,
    row: usize,
    base: usize,
    pair: bool,
) -> Result<(), usize> {
    let pitch = 1 + row_bytes;
    let filter_at = row * pitch - base;
    let filter = Filter::from_byte(buffer[filter_at]).ok_or(row)?;
    let start = filter_at + 1;

    if pair {
        // `above` is the reconstructed previous row at one pitch below. `first` is this
        // row's bytes where they sit; the second row's bytes sit one pitch below, at
        // `start + pitch` — not adjacent to `first`, because the filter byte between them
        // stays in place.
        let (head, tail) = buffer.split_at_mut(start);
        let above = &head[start - pitch..start - pitch + row_bytes];
        let (first, rest) = tail.split_at_mut(pitch);
        unfilter_paeth_pair::<BPP>(above, &mut first[..row_bytes], &mut rest[..row_bytes]);
        Ok(())
    } else {
        let (head, tail) = buffer.split_at_mut(start);
        if row == 0 {
            unfilter_first_row::<BPP>(filter, &mut tail[..row_bytes]);
        } else {
            unfilter_row::<BPP>(
                filter,
                &head[start - pitch..start - pitch + row_bytes],
                &mut tail[..row_bytes],
            );
        }
        Ok(())
    }
}

/// DEFLATE's maximum match distance: the distance alphabet's largest code is 24577 with 13
/// extra bits (RFC 1951 §3.2.5), so inflate never reads an output position further back
/// than this, whatever the stream.
pub(crate) const MAX_MATCH_DISTANCE: usize = 32768;

/// Reverses scanline filters a row at a time as inflation's output cursor advances, fusing
/// the decoder's second pass into its first.
///
/// `buffer` holds the filtered stream exactly as inflation writes it. Each row is
/// reconstructed in place — compacted over its filter byte, as [`unfilter_image`] does —
/// as soon as the whole row, plus one match window behind it, has been written, so the row
/// is still cache-resident when its predictor is undone.
///
/// # Soundness
///
/// Inflate's match copies read `buffer` at `pos - distance` for any `distance` up to
/// [`MAX_MATCH_DISTANCE`], so a byte may be rewritten only once no future match can
/// reference it: when the output cursor has passed it by at least the match window. Row
/// `r` (or the pair `r, r + 1`) is therefore reconstructed only when
///
/// ```text
/// cursor >= (r + take) * (1 + row_bytes) + MAX_MATCH_DISTANCE
/// ```
///
/// which places everything the reconstruction writes — at or below `(r + take) *
/// row_bytes`, since compaction only moves rows forward — at least
/// [`MAX_MATCH_DISTANCE`] bytes behind the cursor, outside every future match's reach. The
/// pair is taken only under the strictly stronger two-row condition, because it also
/// writes row `r + 1`'s compacted bytes. The window lag defers the final
/// `MAX_MATCH_DISTANCE / (1 + row_bytes)`-ish rows; [`finish`](Self::finish) drains them
/// once inflation has completed, so `decode` returns byte-identical output to
/// [`unfilter_image`].
pub(crate) struct ReconstructionFrontier {
    row_bytes: usize,
    height: usize,
    stride: usize,
    rows_done: usize,
    failed_row: Option<usize>,
    /// Output position at which the next row's lag precondition first holds. Reconstructed
    /// after every landing, so the steady-state check is a single compare.
    next_trigger: usize,
}

impl crate::inflate::ProgressHook for ReconstructionFrontier {
    const ENABLED: bool = true;

    #[inline(always)]
    fn on_progress(&mut self, output: &mut [u8], pos: usize) {
        ReconstructionFrontier::on_progress(self, output, pos);
    }
}

impl ReconstructionFrontier {
    pub(crate) fn new(row_bytes: usize, height: usize, stride: usize) -> Self {
        Self {
            row_bytes,
            height,
            stride,
            rows_done: 0,
            failed_row: None,
            next_trigger: row_bytes.saturating_add(1).saturating_add(MAX_MATCH_DISTANCE),
        }
    }

    /// Rows reconstructed so far. Test-only: production callers track progress through
    /// `failed_row` and the drained buffer.
    #[cfg(test)]
    pub(crate) fn rows_done(&self) -> usize {
        self.rows_done
    }

    pub(crate) fn failed_row(&self) -> Option<usize> {
        self.failed_row
    }

    /// Reconstructs every row whose lag precondition `pos` — the current output cursor —
    /// now satisfies.
    #[inline(always)]
    pub(crate) fn on_progress(&mut self, buffer: &mut [u8], pos: usize) {
        if pos >= self.next_trigger && self.failed_row.is_none() {
            self.advance(buffer, pos, MAX_MATCH_DISTANCE);
        }
    }

    /// Drains the rows the window lag deferred, after inflation has finished. `pos` is the
    /// stream's final length. Inflation being over, no match will read the buffer again
    /// and the lag is dropped: only whether a row's bytes are present matters.
    pub(crate) fn finish(&mut self, buffer: &mut [u8], pos: usize) {
        self.advance(buffer, pos, 0);
    }

    fn advance(&mut self, buffer: &mut [u8], pos: usize, lag: usize) {
        match self.stride {
            1 => self.advance_bpp::<1>(buffer, pos, lag),
            2 => self.advance_bpp::<2>(buffer, pos, lag),
            3 => self.advance_bpp::<3>(buffer, pos, lag),
            4 => self.advance_bpp::<4>(buffer, pos, lag),
            6 => self.advance_bpp::<6>(buffer, pos, lag),
            8 => self.advance_bpp::<8>(buffer, pos, lag),
            _ => unreachable!("PNG pixel strides are 1, 2, 3, 4, 6 or 8 bytes"),
        }
    }

    #[inline(never)]
    fn advance_bpp<const BPP: usize>(&mut self, buffer: &mut [u8], pos: usize, lag: usize) {
        let pitch = 1 + self.row_bytes;
        while self.rows_done < self.height {
            let row = self.rows_done;

            // Single-row precondition; `pair` strengthens it by one row of pitch. The lag
            // is `MAX_MATCH_DISTANCE` while inflation runs and zero in `finish`.
            let ready = (row + 1).saturating_mul(pitch).saturating_add(lag) <= pos;
            if !ready {
                break;
            }

            // The pair decision mirrors `unfilter_image_bpp`; an invalid filter byte is
            // not `Paeth`, so it falls to the single-row path, which reports it.
            let mut pair = buffer[row * pitch] == Filter::Paeth as u8
                && !crate::simd::claims_stride(BPP)
                && row > 0
                && row + 1 < self.height
                && buffer[(row + 1) * pitch] == Filter::Paeth as u8;
            if pair && (row + 2).saturating_mul(pitch).saturating_add(lag) > pos {
                pair = false;
            }

            if let Err(bad) = reconstruct_rows::<BPP>(buffer, self.row_bytes, row, pair) {
                self.failed_row = Some(bad);
                return;
            }
            self.rows_done = row + if pair { 2 } else { 1 };
        }
        self.next_trigger =
            (self.rows_done + 1).saturating_mul(pitch).saturating_add(MAX_MATCH_DISTANCE);
    }
}

/// The streaming counterpart of [`ReconstructionFrontier`]: it reverses scanline filters a
/// row at a time as a bounded stage window slides under the filtered stream, emitting each
/// reconstructed row through a sink instead of retaining it.
///
/// Where [`ReconstructionFrontier`] compacts rows into the decode buffer, this frontier
/// reverses rows where they sit (`reconstruct_in_place`) and hands the bytes to `sink`.
/// The caller plays the driver: refill the stage with the next window+segment of filtered
/// bytes, [`set_base`](Self::set_base) to the absolute position of `buffer[0]`, and report
/// the cursor through `on_progress`. The same match-window lag applies: a row is reversed
/// only once the cursor has passed its end by [`MAX_MATCH_DISTANCE`], and `finish` drains
/// the tail once inflation is over and the lag no longer matters.
///
/// Emission is eager and in row order. A sink error is stored (see
/// [`take_error`](Self::take_error)) and freezes the frontier; the driver surfaces the
/// error and stops, usually before starting the next segment.
pub(crate) struct StreamingFrontier<'a, E, S: FnMut(usize, &[u8]) -> Result<(), E>> {
    row_bytes: usize,
    height: usize,
    stride: usize,
    rows_done: usize,
    failed_row: Option<usize>,
    /// Absolute position of `buffer[0]`.
    base: usize,
    /// Absolute cursor position at which the next row's lag precondition first holds.
    next_trigger: usize,
    sink: &'a mut S,
    pending: Option<E>,
}

impl<'a, E, S: FnMut(usize, &[u8]) -> Result<(), E>> crate::inflate::ProgressHook
    for StreamingFrontier<'a, E, S>
{
    const ENABLED: bool = true;

    #[inline(always)]
    fn on_progress(&mut self, output: &mut [u8], pos: usize) {
        StreamingFrontier::on_progress(self, output, pos);
    }
}

impl<'a, E, S: FnMut(usize, &[u8]) -> Result<(), E>> StreamingFrontier<'a, E, S> {
    pub(crate) fn new(row_bytes: usize, height: usize, stride: usize, sink: &'a mut S) -> Self {
        let pitch = 1 + row_bytes;
        Self {
            row_bytes,
            height,
            stride,
            rows_done: 0,
            failed_row: None,
            base: 0,
            next_trigger: pitch.saturating_add(MAX_MATCH_DISTANCE),
            sink,
            pending: None,
        }
    }

    pub(crate) fn set_base(&mut self, base: usize) {
        self.base = base;
    }

    /// Absolute position one past the last fully reconstructed row; the streaming driver
    /// must not slide its window past this point.
    pub(crate) fn done_floor(&self) -> usize {
        self.rows_done * (1 + self.row_bytes)
    }

    pub(crate) fn failed_row(&self) -> Option<usize> {
        self.failed_row
    }

    pub(crate) fn take_error(&mut self) -> Option<E> {
        self.pending.take()
    }

    /// Reconstructs and emits every row whose lag precondition the stage-relative cursor
    /// `pos` now satisfies.
    #[inline(always)]
    pub(crate) fn on_progress(&mut self, buffer: &mut [u8], pos: usize) {
        let abs = self.base.saturating_add(pos);
        if abs >= self.next_trigger && self.failed_row.is_none() && self.pending.is_none() {
            self.advance(buffer, abs, MAX_MATCH_DISTANCE);
        }
    }

    /// Drains the rows the window lag deferred, after inflation has finished. `end` is the
    /// stage-relative stream length.
    pub(crate) fn finish(&mut self, buffer: &mut [u8], end: usize) {
        self.advance(buffer, self.base.saturating_add(end), 0);
    }

    fn advance(&mut self, buffer: &mut [u8], abs: usize, lag: usize) {
        match self.stride {
            1 => self.advance_bpp::<1>(buffer, abs, lag),
            2 => self.advance_bpp::<2>(buffer, abs, lag),
            3 => self.advance_bpp::<3>(buffer, abs, lag),
            4 => self.advance_bpp::<4>(buffer, abs, lag),
            6 => self.advance_bpp::<6>(buffer, abs, lag),
            8 => self.advance_bpp::<8>(buffer, abs, lag),
            _ => unreachable!("PNG pixel strides are 1, 2, 3, 4, 6 or 8 bytes"),
        }
    }

    #[inline(never)]
    fn advance_bpp<const BPP: usize>(&mut self, buffer: &mut [u8], abs: usize, lag: usize) {
        let pitch = 1 + self.row_bytes;
        while self.rows_done < self.height {
            let row = self.rows_done;

            // Single-row precondition; `pair` strengthens it by one row of pitch. The lag
            // is `MAX_MATCH_DISTANCE` while inflation runs and zero in `finish`.
            let ready = (row + 1).saturating_mul(pitch).saturating_add(lag) <= abs;
            if !ready {
                break;
            }

            // The pair decision mirrors `unfilter_image_bpp`; an invalid filter byte is
            // not `Paeth`, so it falls to the single-row path, which reports it.
            let mut pair = buffer[row * pitch - self.base] == Filter::Paeth as u8
                && !crate::simd::claims_stride(BPP)
                && row > 0
                && row + 1 < self.height
                && buffer[(row + 1) * pitch - self.base] == Filter::Paeth as u8;
            if pair && (row + 2).saturating_mul(pitch).saturating_add(lag) > abs {
                pair = false;
            }

            let take = if pair { 2 } else { 1 };
            if let Err(bad) =
                reconstruct_in_place::<BPP>(buffer, self.row_bytes, row, self.base, pair)
            {
                self.failed_row = Some(bad);
                return;
            }
            for index in row..row + take {
                let start = index * pitch + 1 - self.base;
                if let Err(error) = (self.sink)(index, &buffer[start..start + self.row_bytes]) {
                    self.pending = Some(error);
                    self.rows_done = index;
                    return;
                }
            }
            self.rows_done = row + take;
        }
        self.next_trigger =
            (self.rows_done + 1).saturating_mul(pitch).saturating_add(MAX_MATCH_DISTANCE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies `filter` to `row`, writing the residuals to `out`, the way the forward
    /// direction of the format defines it. The encoder is gone from the crate, so the
    /// round-trip tests compute the filtered stream themselves: this is the specification's
    /// own arithmetic, kept beside the pins that use it.
    ///
    /// `prev` is the unfiltered row above, or an all-zero slice for the first row.
    fn filter_forward<const BPP: usize>(filter: Filter, prev: &[u8], row: &[u8], out: &mut [u8]) {
        debug_assert_eq!(row.len(), out.len());
        debug_assert_eq!(prev.len(), row.len());
        let len = row.len();
        let head = BPP.min(len);

        match filter {
            Filter::None => out.copy_from_slice(row),
            Filter::Sub => {
                out[..head].copy_from_slice(&row[..head]);
                for i in BPP..len {
                    out[i] = row[i].wrapping_sub(row[i - BPP]);
                }
            }
            Filter::Up => {
                for i in 0..len {
                    out[i] = row[i].wrapping_sub(prev[i]);
                }
            }
            Filter::Average => {
                for i in 0..head {
                    out[i] = row[i].wrapping_sub(prev[i] >> 1);
                }
                for i in BPP..len {
                    let sum = row[i - BPP] as u16 + prev[i] as u16;
                    out[i] = row[i].wrapping_sub((sum >> 1) as u8);
                }
            }
            Filter::Paeth => {
                for i in 0..head {
                    out[i] = row[i].wrapping_sub(prev[i]);
                }
                for i in BPP..len {
                    out[i] = row[i].wrapping_sub(paeth(
                        row[i - BPP] as i16,
                        prev[i] as i16,
                        prev[i - BPP] as i16,
                    ));
                }
            }
        }
    }

    fn round_trip<const BPP: usize>(filter: Filter, width_pixels: usize, height: usize) {
        let row_bytes = width_pixels * BPP;
        let image: Vec<u8> = (0..row_bytes * height)
            .map(|i| (i.wrapping_mul(97).wrapping_add(i / row_bytes * 13) % 256) as u8)
            .collect();

        // Build the filtered stream the way an encoder would.
        let mut stream = vec![0u8; height * (1 + row_bytes)];
        let zero_row = vec![0u8; row_bytes];
        for row in 0..height {
            let prev = if row == 0 {
                &zero_row[..]
            } else {
                &image[(row - 1) * row_bytes..row * row_bytes]
            };
            let base = row * (1 + row_bytes);
            stream[base] = filter as u8;
            let (before, after) = stream.split_at_mut(base + 1);
            let _ = before;
            filter_forward::<BPP>(
                filter,
                prev,
                &image[row * row_bytes..(row + 1) * row_bytes],
                &mut after[..row_bytes],
            );
        }

        unfilter_image(&mut stream, row_bytes, height, BPP).unwrap();
        assert_eq!(&stream[..row_bytes * height], &image[..], "bpp {BPP} {filter:?}");
    }

    #[test]
    fn every_filter_round_trips() {
        for filter in Filter::ALL {
            round_trip::<1>(filter, 17, 5);
            round_trip::<2>(filter, 13, 4);
            round_trip::<3>(filter, 11, 6);
            round_trip::<4>(filter, 9, 7);
            round_trip::<6>(filter, 5, 3);
            round_trip::<8>(filter, 4, 3);
        }
    }

    /// Narrower than one pixel stride: the whole row is "off the left edge".
    #[test]
    fn rows_narrower_than_the_stride() {
        for filter in Filter::ALL {
            round_trip::<8>(filter, 1, 3);
            round_trip::<4>(filter, 1, 2);
        }
    }

    /// Round-trips an image whose rows use `filters[row % filters.len()]`.
    ///
    /// Reconstruction takes adjacent `Paeth` rows two at a time, so what matters is where
    /// the runs of them start and stop: an odd-length run leaves a row for the single-row
    /// path, and a run reaching the last row has no successor to pair with.
    fn round_trip_mixed<const BPP: usize>(filters: &[Filter], width_pixels: usize, height: usize) {
        let row_bytes = width_pixels * BPP;
        let image: Vec<u8> = (0..row_bytes * height)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i / row_bytes * 7) % 251) as u8)
            .collect();

        let mut stream = vec![0u8; height * (1 + row_bytes)];
        let zero_row = vec![0u8; row_bytes];
        for row in 0..height {
            let filter = filters[row % filters.len()];
            let prev = if row == 0 {
                &zero_row[..]
            } else {
                &image[(row - 1) * row_bytes..row * row_bytes]
            };
            let base = row * (1 + row_bytes);
            stream[base] = filter as u8;
            let (_, after) = stream.split_at_mut(base + 1);
            filter_forward::<BPP>(
                filter,
                prev,
                &image[row * row_bytes..(row + 1) * row_bytes],
                &mut after[..row_bytes],
            );
        }

        unfilter_image(&mut stream, row_bytes, height, BPP).unwrap();
        assert_eq!(
            &stream[..row_bytes * height],
            &image[..],
            "bpp {BPP} {filters:?} {width_pixels}x{height}"
        );
    }

    /// Every arrangement of `Paeth` runs the two-row wavefront has to cope with.
    #[test]
    fn paeth_runs_of_every_length_and_alignment() {
        use Filter::{Paeth as P, Sub as S, Up as U};

        let patterns: &[&[Filter]] = &[
            &[P],             // every row, so runs bounded only by the image
            &[P, S],          // no run longer than one
            &[P, P, S],       // an even run, then a break
            &[P, P, P, S],    // an odd run, so one row is left for the single-row path
            &[P, P, P, P, U], // a longer run whose remainder lands differently each time
            &[S, P, P],       // a run that does not start at the top
            &[U, U, P],       // isolated rows between other filters
        ];

        // Heights either side of each pattern's period, so runs end mid-pattern as well as
        // on it, and in particular so a run sometimes reaches the last row of the image.
        for pattern in patterns {
            for height in 1..=9 {
                round_trip_mixed::<1>(pattern, 13, height);
                round_trip_mixed::<2>(pattern, 7, height);
                round_trip_mixed::<3>(pattern, 11, height);
                round_trip_mixed::<4>(pattern, 9, height);
                round_trip_mixed::<6>(pattern, 5, height);
                round_trip_mixed::<8>(pattern, 3, height);
            }
        }
    }

    /// A single pixel per row leaves the wavefront with nothing but its prologue and tail.
    #[test]
    fn paeth_rows_one_pixel_wide() {
        for height in 1..=5 {
            round_trip_mixed::<3>(&[Filter::Paeth], 1, height);
            round_trip_mixed::<4>(&[Filter::Paeth], 1, height);
            round_trip_mixed::<1>(&[Filter::Paeth], 2, height);
        }
    }

    #[test]
    fn paeth_matches_the_specification() {
        // The reference formulation from RFC 2083, written out literally.
        fn reference(a: u8, b: u8, c: u8) -> u8 {
            let p = a as i32 + b as i32 - c as i32;
            let pa = (p - a as i32).abs();
            let pb = (p - b as i32).abs();
            let pc = (p - c as i32).abs();
            if pa <= pb && pa <= pc {
                a
            } else if pb <= pc {
                b
            } else {
                c
            }
        }

        for a in 0..=255u8 {
            for b in [0u8, 1, 63, 127, 128, 200, 255] {
                for c in [0u8, 1, 63, 127, 128, 200, 255] {
                    assert_eq!(
                        paeth(a as i16, b as i16, c as i16),
                        reference(a, b, c),
                        "{a} {b} {c}"
                    );
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Reconstruction frontier (fused decode)
    //
    // The frontier reverses filters row by row as inflation's output cursor advances, lagging
    // it by at least DEFLATE's 32 KiB match window so no future match can read the bytes a
    // row is reconstructed into. These tests build filtered streams the way an encoder would,
    // feed them to the frontier in chunks that imitate a live output cursor, and require the
    // result to equal `unfilter_image` on the same bytes.
    // ---------------------------------------------------------------------------------------

    /// Builds the filtered scanline stream for `image` with `filters[row % filters.len()]`.
    fn filtered_stream<const BPP: usize>(
        image: &[u8],
        row_bytes: usize,
        height: usize,
        filters: &[Filter],
    ) -> Vec<u8> {
        let mut stream = vec![0u8; height * (1 + row_bytes)];
        let zero_row = vec![0u8; row_bytes];
        for row in 0..height {
            let filter = filters[row % filters.len()];
            let prev = if row == 0 {
                &zero_row[..]
            } else {
                &image[(row - 1) * row_bytes..row * row_bytes]
            };
            let base = row * (1 + row_bytes);
            stream[base] = filter as u8;
            let (_, after) = stream.split_at_mut(base + 1);
            filter_forward::<BPP>(
                filter,
                prev,
                &image[row * row_bytes..(row + 1) * row_bytes],
                &mut after[..row_bytes],
            );
        }
        stream
    }

    /// Feeds a filtered stream to a frontier in chunks of `chunk` bytes, finishes it, and
    /// requires the reconstructed prefix to equal `unfilter_image`'s.
    fn frontier_agrees_with_unfilter_image<const BPP: usize>(
        width_pixels: usize,
        height: usize,
        filters: &[Filter],
        chunk: usize,
    ) {
        let row_bytes = width_pixels * BPP;
        let image: Vec<u8> = (0..row_bytes * height)
            .map(|i| (i.wrapping_mul(97).wrapping_add(i / row_bytes * 13) % 256) as u8)
            .collect();
        let stream = filtered_stream::<BPP>(&image, row_bytes, height, filters);

        let mut reference = stream.clone();
        unfilter_image(&mut reference, row_bytes, height, BPP).unwrap();

        let mut buffer = stream.clone();
        let mut frontier = ReconstructionFrontier::new(row_bytes, height, BPP);
        let mut pos = 0;
        while pos < stream.len() {
            pos = (pos + chunk).min(stream.len());
            frontier.on_progress(&mut buffer, pos);
        }
        frontier.finish(&mut buffer, stream.len());

        assert_eq!(frontier.rows_done(), height, "frontier must drain every row");
        assert_eq!(frontier.failed_row(), None);
        assert_eq!(&buffer[..row_bytes * height], &reference[..row_bytes * height]);
    }

    #[test]
    fn frontier_matches_unfilter_image_every_filter_and_stride() {
        for filter in Filter::ALL {
            frontier_agrees_with_unfilter_image::<1>(17, 5, &[filter], 977);
            frontier_agrees_with_unfilter_image::<2>(13, 4, &[filter], 977);
            frontier_agrees_with_unfilter_image::<3>(11, 6, &[filter], 977);
            frontier_agrees_with_unfilter_image::<4>(9, 7, &[filter], 977);
            frontier_agrees_with_unfilter_image::<6>(5, 3, &[filter], 977);
            frontier_agrees_with_unfilter_image::<8>(4, 3, &[filter], 977);
        }
    }

    /// Every `Paeth` run alignment, fed in chunks, including chunk boundaries that land
    /// inside a row so the frontier pauses mid-row.
    #[test]
    fn frontier_matches_unfilter_image_paeth_run_alignments() {
        use Filter::{Average as A, None as N, Paeth as P, Sub as S, Up as U};

        let patterns: &[&[Filter]] = &[
            &[P],
            &[P, S],
            &[P, P, S],
            &[P, P, P, S],
            &[P, P, P, P, U],
            &[S, P, P],
            &[U, U, P],
            &[N, S, U, A, P, P, P],
        ];

        for pattern in patterns {
            for height in 1..=9 {
                frontier_agrees_with_unfilter_image::<1>(13, height, pattern, 977);
                frontier_agrees_with_unfilter_image::<3>(11, height, pattern, 977);
                frontier_agrees_with_unfilter_image::<4>(9, height, pattern, 977);
                frontier_agrees_with_unfilter_image::<8>(3, height, pattern, 977);
            }
        }
    }

    /// Rows narrow and wide, images one row tall and one pixel wide.
    #[test]
    fn frontier_shape_edges() {
        for filter in Filter::ALL {
            frontier_agrees_with_unfilter_image::<8>(1, 9, &[filter], 31);
            frontier_agrees_with_unfilter_image::<4>(1, 1, &[filter], 31);
            frontier_agrees_with_unfilter_image::<1>(37, 1, &[filter], 31);
        }
    }

    /// On a stream wider than the match window, reconstruction must start while inflation
    /// is still running: after the first row's trigger the frontier has reconstructed row 0
    /// and stops; a jump large enough for the pair trigger reconstructs two `Paeth` rows at
    /// once, exercising the wavefront against a live output cursor.
    #[test]
    fn frontier_reconstructs_behind_the_match_window() {
        use Filter::Paeth as P;

        let row_bytes: usize = 4096 * 4;
        let height: usize = 8;
        let image: Vec<u8> = (0..row_bytes * height)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i / row_bytes * 7) % 251) as u8)
            .collect();
        let stream = filtered_stream::<4>(&image, row_bytes, height, &[P]);

        let mut reference = stream.clone();
        unfilter_image(&mut reference, row_bytes, height, 4).unwrap();

        let mut buffer = stream.clone();
        let mut frontier = ReconstructionFrontier::new(row_bytes, height, 4);
        let pitch = 1 + row_bytes;

        // First-row trigger: row 0 is never paired (it has no row above), so one row lands.
        frontier.on_progress(&mut buffer, MAX_MATCH_DISTANCE + pitch);
        assert_eq!(frontier.rows_done(), 1);

        // The next row's trigger alone is not enough for the pair: row r pairs with r+1 only
        // once row r+1's bytes are also behind the window.
        frontier.on_progress(&mut buffer, MAX_MATCH_DISTANCE + 2 * pitch);
        assert_eq!(frontier.rows_done(), 2);

        // At this cursor a single-row path could only reach row 2; the two-row wavefront
        // covers rows 2 and 3 in one landing, which only pairing explains.
        frontier.on_progress(&mut buffer, MAX_MATCH_DISTANCE + 4 * pitch);
        assert_eq!(frontier.rows_done(), 4);

        frontier.on_progress(&mut buffer, MAX_MATCH_DISTANCE + 6 * pitch);
        assert_eq!(frontier.rows_done(), 6);

        frontier.finish(&mut buffer, stream.len());
        assert_eq!(frontier.rows_done(), height);
        assert_eq!(&buffer[..row_bytes * height], &reference[..row_bytes * height]);
    }

    /// An invalid filter byte mid-stream freezes the frontier at that row, exactly as
    /// `unfilter_image` reports it, and later progress calls change nothing.
    #[test]
    fn frontier_freezes_on_an_invalid_filter_byte() {
        let row_bytes: usize = 4096 * 4;
        let height: usize = 8;
        let image: Vec<u8> = (0..row_bytes * height)
            .map(|i| (i.wrapping_mul(13).wrapping_add(i / row_bytes * 5) % 247) as u8)
            .collect();
        let mut stream = filtered_stream::<4>(&image, row_bytes, height, &[Filter::Paeth]);
        stream[5 * (1 + row_bytes)] = 9;

        let mut reference = stream.clone();
        assert_eq!(unfilter_image(&mut reference, row_bytes, height, 4), Err(5));

        let mut buffer = stream.clone();
        let mut frontier = ReconstructionFrontier::new(row_bytes, height, 4);
        frontier.on_progress(&mut buffer, MAX_MATCH_DISTANCE + 7 * (1 + row_bytes));
        assert_eq!(frontier.rows_done(), 5);
        assert_eq!(frontier.failed_row(), Some(5));

        frontier.on_progress(&mut buffer, stream.len());
        frontier.finish(&mut buffer, stream.len());
        assert_eq!(frontier.rows_done(), 5, "frozen at the bad row");
        assert_eq!(frontier.failed_row(), Some(5));
    }

    // ---------------------------------------------------------------------------------------
    // Streaming frontier
    //
    // A StreamingFrontier emits reconstructed rows through a sink while a bounded stage
    // window slides beneath it: the caller refills the stage with the next window+segment
    // of filtered bytes, advances the base, and the frontier reverses rows whose lag
    // precondition the cursor satisfies. These tests simulate the driver's slide with the
    // same rule the decoder uses (base = written - window) and require the emitted rows to
    // equal `unfilter_image`'s output.
    // ---------------------------------------------------------------------------------------

    /// Feeds `stream` to a StreamingFrontier in driver-shaped segments and requires the
    /// emitted rows to equal the reference reconstruction.
    fn streaming_matches_unfilter_image<const BPP: usize>(
        width_pixels: usize,
        height: usize,
        filters: &[Filter],
        rows_per_segment: usize,
    ) {
        let row_bytes = width_pixels * BPP;
        let pitch = 1 + row_bytes;
        let image: Vec<u8> = (0..row_bytes * height)
            .map(|i| (i.wrapping_mul(97).wrapping_add(i / row_bytes * 13) % 256) as u8)
            .collect();
        let stream = filtered_stream::<BPP>(&image, row_bytes, height, filters);

        let mut reference = stream.clone();
        unfilter_image(&mut reference, row_bytes, height, BPP).unwrap();

        let window = MAX_MATCH_DISTANCE;
        let cap = rows_per_segment * pitch;
        let mut stage = vec![0u8; window + cap + 4 * pitch + 16];

        let mut emitted: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut sink = |index: usize, bytes: &[u8]| -> Result<(), &'static str> {
            emitted.push((index, bytes.to_vec()));
            Ok(())
        };
        let mut frontier = StreamingFrontier::new(row_bytes, height, BPP, &mut sink);

        // Driver simulation, mirroring the streaming decoder exactly: the stage keeps the
        // retained region across slides (reconstructed rows included), and only the range
        // after it is refilled — as inflate's writes would produce it.
        let mut base = 0usize;
        let mut resume = 0usize;
        let mut budget = window + cap;
        loop {
            let fill = budget.min(stream.len() - base);
            stage[resume..fill].copy_from_slice(&stream[base + resume..base + fill]);
            frontier.set_base(base);
            frontier.on_progress(&mut stage, fill);
            if base + fill == stream.len() {
                frontier.finish(&mut stage, fill);
                break;
            }
            let abs_written = base + fill;
            let floor = frontier.done_floor();
            if floor >= base + pitch {
                let new_base = (floor - pitch + 1).min(abs_written);
                stage.copy_within(new_base - base..fill, 0);
                resume = abs_written - new_base;
                base = new_base;
                budget = resume + cap;
            } else {
                budget = (budget + 2 * pitch).min(window + cap + 4 * pitch);
            }
        }

        assert_eq!(frontier.failed_row(), None);
        assert_eq!(emitted.len(), height, "every row emitted exactly once");
        for (index, bytes) in &emitted {
            assert_eq!(
                bytes[..],
                reference[*index * row_bytes..(*index + 1) * row_bytes],
                "row {index}"
            );
        }
    }

    #[test]
    fn streaming_matches_unfilter_image_every_filter_and_stride() {
        for filter in Filter::ALL {
            streaming_matches_unfilter_image::<1>(17, 5, &[filter], 3);
            streaming_matches_unfilter_image::<2>(13, 4, &[filter], 1);
            streaming_matches_unfilter_image::<3>(11, 6, &[filter], 7);
            streaming_matches_unfilter_image::<4>(9, 7, &[filter], 2);
            streaming_matches_unfilter_image::<6>(5, 3, &[filter], 5);
            streaming_matches_unfilter_image::<8>(4, 3, &[filter], 1);
        }
    }

    #[test]
    fn streaming_matches_unfilter_image_paeth_run_alignments() {
        use Filter::{Average as A, None as N, Paeth as P, Sub as S, Up as U};

        let patterns: &[&[Filter]] = &[
            &[P],
            &[P, S],
            &[P, P, S],
            &[P, P, P, S],
            &[P, P, P, P, U],
            &[S, P, P],
            &[U, U, P],
            &[N, S, U, A, P, P, P],
        ];

        for pattern in patterns {
            for height in 1..=9 {
                streaming_matches_unfilter_image::<1>(13, height, pattern, 1);
                streaming_matches_unfilter_image::<3>(11, height, pattern, 4);
                streaming_matches_unfilter_image::<4>(9, height, pattern, 1);
                streaming_matches_unfilter_image::<8>(3, height, pattern, 2);
            }
        }
    }

    /// A row wider than one segment: the stage must still emit it whole, after its tail
    /// has been reconstructed, without losing the bytes that slid beneath it.
    #[test]
    fn streaming_with_rows_wider_than_a_segment() {
        use Filter::Paeth as P;
        streaming_matches_unfilter_image::<4>(9000, 6, &[P], 1); // 36 KB rows, one per segment
        streaming_matches_unfilter_image::<4>(9000, 6, &[P], 3);
    }

    /// A failing sink stops emission, surfaces its error, and later progress is a no-op.
    #[test]
    fn streaming_freezes_on_a_failing_sink() {
        let row_bytes: usize = 4096 * 4;
        let height: usize = 8;
        let image: Vec<u8> = (0..row_bytes * height)
            .map(|i| (i.wrapping_mul(13).wrapping_add(i / row_bytes * 5) % 247) as u8)
            .collect();
        let stream = filtered_stream::<4>(&image, row_bytes, height, &[Filter::Paeth]);

        let calls = std::cell::Cell::new(0);
        let mut sink = |_: usize, _: &[u8]| -> Result<(), &'static str> {
            calls.set(calls.get() + 1);
            Err("boom")
        };
        let mut frontier = StreamingFrontier::new(row_bytes, height, 4, &mut sink);
        let mut stage = stream.clone();
        frontier.on_progress(&mut stage, stream.len());
        assert_eq!(calls.get(), 1);
        // The error freezes further progress; the driver takes it once it notices.
        frontier.on_progress(&mut stage, stream.len());
        assert_eq!(calls.get(), 1, "frozen after the sink error");
        assert_eq!(frontier.take_error(), Some("boom"));
        // Taking the error unfreezes: the row re-reconstructs and the sink is called again.
        frontier.on_progress(&mut stage, stream.len());
        assert_eq!(calls.get(), 2);
    }
}
