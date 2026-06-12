//! `RTL_BITMAP` — the kernel's run-finding bitmap.
//!
//! Mm uses bitmaps to track free physical pages and system PTE ranges; the
//! executive handle table uses them for handle slots. The hot operation is
//! not "test bit" but **"find N contiguous clear bits and set them"**
//! (`RtlFindClearBitsAndSet`), which is what makes it an allocator
//! primitive rather than just a bit array.
//!
//! The implementation deliberately stores `u64` words (the C one uses
//! ULONGs) — same semantics, fewer iterations on 64-bit hardware.

/// A borrowed bitmap over caller-owned storage, like `RTL_BITMAP` whose
/// `Buffer` points into pool. Bit `i` lives in word `i / 64`, bit `i % 64`;
/// set == allocated/in-use, clear == free.
pub struct RtlBitmap<'a> {
    words: &'a mut [u64],
    /// Number of *valid* bits (may be less than `words.len() * 64`).
    size_in_bits: usize,
}

impl<'a> RtlBitmap<'a> {
    /// `RtlInitializeBitMap` — wrap caller storage. Bits beyond
    /// `size_in_bits` are treated as permanently set (never allocatable),
    /// which the constructor enforces immediately so find-runs never has to
    /// special-case the tail word.
    pub fn new(words: &'a mut [u64], size_in_bits: usize) -> Self {
        assert!(size_in_bits <= words.len() * 64, "bitmap storage too small");
        let bm = RtlBitmap { words, size_in_bits };
        // Mask off the unusable tail.
        let tail_bits = size_in_bits % 64;
        let full_words = size_in_bits / 64;
        if tail_bits != 0 {
            bm.words[full_words] |= !0u64 << tail_bits;
        }
        for w in &mut bm.words[(full_words + if tail_bits != 0 { 1 } else { 0 })..] {
            *w = !0;
        }
        bm
    }

    /// Number of valid bits in the map.
    pub fn len(&self) -> usize {
        self.size_in_bits
    }

    /// `RtlSetBits` — mark `[start, start+count)` as in-use.
    pub fn set_bits(&mut self, start: usize, count: usize) {
        debug_assert!(start + count <= self.size_in_bits);
        for i in start..start + count {
            self.words[i / 64] |= 1u64 << (i % 64);
        }
    }

    /// `RtlClearBits` — mark `[start, start+count)` as free.
    pub fn clear_bits(&mut self, start: usize, count: usize) {
        debug_assert!(start + count <= self.size_in_bits);
        for i in start..start + count {
            self.words[i / 64] &= !(1u64 << (i % 64));
        }
    }

    /// `RtlTestBit` — is bit `i` set?
    pub fn test_bit(&self, i: usize) -> bool {
        debug_assert!(i < self.size_in_bits);
        self.words[i / 64] & (1u64 << (i % 64)) != 0
    }

    /// `RtlNumberOfSetBits`.
    pub fn count_set(&self) -> usize {
        // Subtract the artificial tail-set bits added by `new`.
        let padding = self.words.len() * 64 - self.size_in_bits;
        self.words.iter().map(|w| w.count_ones() as usize).sum::<usize>() - padding
    }

    /// `RtlFindClearBitsAndSet` — find `count` contiguous clear bits at or
    /// after `hint`, set them, and return the starting index. Wraps around
    /// to the beginning if nothing is free above the hint. Returns `None`
    /// when no run exists (C returns 0xFFFFFFFF).
    ///
    /// The hint is what makes the PFN allocator mostly O(1): Mm passes the
    /// index just past the previous allocation, so sequential allocations
    /// scan forward without re-walking the densely used low memory.
    pub fn find_clear_bits_and_set(&mut self, count: usize, hint: usize) -> Option<usize> {
        if count == 0 || count > self.size_in_bits {
            return None;
        }
        let hint = if hint >= self.size_in_bits { 0 } else { hint };
        self.find_run_from(hint, self.size_in_bits, count)
            .or_else(|| self.find_run_from(0, hint + count.min(self.size_in_bits), count))
            .map(|start| {
                self.set_bits(start, count);
                start
            })
    }

    /// Scan `[from, to)` for a run of `count` clear bits.
    ///
    /// Word-at-a-time fast path: a fully-set word can never contain the
    /// start of a run, so skip it in one comparison instead of 64.
    fn find_run_from(&self, from: usize, to: usize, count: usize) -> Option<usize> {
        let to = to.min(self.size_in_bits);
        let mut run_start = from;
        let mut run_len = 0usize;
        let mut i = from;
        while i < to {
            if run_len == 0 && i % 64 == 0 && i + 64 <= to && self.words[i / 64] == !0 {
                i += 64; // fast-skip a fully allocated word
                run_start = i;
                continue;
            }
            if self.test_bit(i) {
                run_len = 0;
                run_start = i + 1;
            } else {
                run_len += 1;
                if run_len == count {
                    return Some(run_start);
                }
            }
            i += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_clear_test_roundtrip() {
        let mut storage = [0u64; 4];
        let mut bm = RtlBitmap::new(&mut storage, 200);
        assert_eq!(bm.count_set(), 0);
        bm.set_bits(10, 5);
        assert!(bm.test_bit(10) && bm.test_bit(14) && !bm.test_bit(15));
        assert_eq!(bm.count_set(), 5);
        bm.clear_bits(10, 5);
        assert_eq!(bm.count_set(), 0);
    }

    #[test]
    fn find_clear_bits_and_set_with_hint_and_wrap() {
        let mut storage = [0u64; 2];
        let mut bm = RtlBitmap::new(&mut storage, 128);

        // Carve up the space: first 64 bits busy.
        bm.set_bits(0, 64);
        // Hint past the busy region finds the free space immediately.
        assert_eq!(bm.find_clear_bits_and_set(8, 64), Some(64));
        // Hint near the end wraps around and still fails only when truly full.
        assert_eq!(bm.find_clear_bits_and_set(56, 120), Some(72));
        assert_eq!(bm.count_set(), 128);
        assert_eq!(bm.find_clear_bits_and_set(1, 0), None);

        // Free a hole in the middle and find it from a wrapping hint.
        bm.clear_bits(100, 4);
        assert_eq!(bm.find_clear_bits_and_set(4, 120), Some(100));
    }

    #[test]
    fn tail_bits_are_never_allocatable() {
        let mut storage = [0u64; 1];
        let mut bm = RtlBitmap::new(&mut storage, 10); // 54 tail bits masked
        assert_eq!(bm.find_clear_bits_and_set(11, 0), None); // > size
        assert_eq!(bm.find_clear_bits_and_set(10, 0), Some(0)); // exactly size
        assert_eq!(bm.find_clear_bits_and_set(1, 0), None); // now full
    }
}
