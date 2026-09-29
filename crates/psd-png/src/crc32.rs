//! CRC-32/ISO-HDLC, as used by PNG chunk checksums.
//!
//! The computation is `crc32fast`'s: carry-less multiplication where the CPU has it
//! (PCLMULQDQ on x86, PMULL on aarch64), at 60-80 GB/s, and a slice-by-16 table walk where
//! it does not. The crate ran its own slice-by-16, at 3.2 GB/s, plus a hand-written aarch64
//! CRC-instruction path that could not be tested off that architecture; the chunk CRC runs
//! over every compressed byte of a default decode, and no safe code can reach the carry-less
//! multiply, so the checksum is the one place the crate leans on someone else's `unsafe`.

/// Incremental CRC-32 hasher.
#[derive(Clone, Debug, Default)]
pub struct Crc32 {
    hasher: crc32fast::Hasher,
}

impl Crc32 {
    /// A checksum over no bytes.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds `data` into the running checksum. Any split into calls gives the same result.
    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        self.hasher.update(data);
    }

    /// The checksum of everything fed in so far. The hasher is left as it was, so more bytes
    /// can follow.
    #[inline]
    pub fn finish(&self) -> u32 {
        self.hasher.clone().finalize()
    }
}

/// Computes the CRC-32 of `data` in one shot.
#[inline]
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The definition, a bit at a time, so that nothing in the test shares an
    /// implementation with the code under test.
    fn bitwise(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    #[test]
    fn known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"a"), 0xe8b7_be43);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
    }

    /// Lengths around the points where an implementation changes strategy: a byte at a time
    /// below a word, the sixteen-byte table step, the 64-byte carry-less-multiply blocks and
    /// their tails.
    #[test]
    fn matches_the_bitwise_definition_at_every_length() {
        let data: Vec<u8> =
            (0..1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for len in (0..=130).chain([191, 192, 193, 255, 256, 257, 511, 1023, 1024]) {
            assert_eq!(crc32(&data[..len]), bitwise(&data[..len]), "len {len}");
        }
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data: Vec<u8> =
            (0..1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();

        // Splitting the input must not change the result.
        for split in [0, 1, 7, 8, 15, 16, 17, 63, 64, 65, 511, 1023, 1024] {
            let mut hasher = Crc32::new();
            hasher.update(&data[..split]);
            hasher.update(&data[split..]);
            assert_eq!(hasher.finish(), crc32(&data), "split at {split}");
        }
    }

    /// `finish` reads the running value without consuming it.
    #[test]
    fn finish_leaves_the_hasher_usable() {
        let mut hasher = Crc32::new();
        hasher.update(b"1234");
        let _ = hasher.finish();
        hasher.update(b"56789");
        assert_eq!(hasher.finish(), 0xcbf4_3926);
    }
}
