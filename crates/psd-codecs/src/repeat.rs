//! Long repeated byte patterns in bounded blocks.

/// Bytes of pattern copies assembled per block.
const BLOCK_BYTES: usize = 32 * 1024;

/// Feed `pattern` repeated `count` times to `sink` as blocks of about 32 KiB
/// (at least one copy each), so a uniform payload of any length is produced
/// without a buffer of its full length. Nothing is fed for an empty pattern or
/// a zero count; the first error from `sink` stops the walk.
pub fn for_each_block<E>(
    pattern: &[u8],
    count: u64,
    mut sink: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E> {
    if pattern.is_empty() || count == 0 {
        return Ok(());
    }
    let copies = (BLOCK_BYTES / pattern.len()).max(1) as u64;
    let copies = copies.min(count);
    let block = pattern.repeat(copies as usize);
    let mut remaining = count;
    while remaining >= copies {
        sink(&block)?;
        remaining -= copies;
    }
    if remaining > 0 {
        sink(&block[..remaining as usize * pattern.len()])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_concatenate_to_the_repeated_pattern_for_every_tail() {
        for pattern in [&[7u8][..], &[1, 2, 3], &[9; 40_000]] {
            for count in [0u64, 1, 2, 10_922, 10_923, 32_769, 100_000] {
                if pattern.len() as u64 * count > 8 * 1024 * 1024 {
                    continue;
                }
                let mut joined = Vec::new();
                let mut largest = 0;
                for_each_block(pattern, count, |block| {
                    largest = largest.max(block.len());
                    joined.extend_from_slice(block);
                    Ok::<_, ()>(())
                })
                .unwrap();
                assert_eq!(joined, pattern.repeat(count as usize));
                assert!(largest <= BLOCK_BYTES.max(pattern.len()));
            }
        }
    }

    #[test]
    fn the_first_sink_error_stops_the_walk() {
        let mut calls = 0;
        let result = for_each_block(&[1], 1_000_000, |_| {
            calls += 1;
            if calls == 2 {
                Err("stop")
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err("stop"));
        assert_eq!(calls, 2);
    }
}
