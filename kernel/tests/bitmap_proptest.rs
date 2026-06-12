//! Property-based tests for `RtlBitmap` — the run-finding bitmap that is
//! the brain of the PFN physical-page allocator.
//!
//! The strategy is differential: run a randomized sequence of operations
//! against both `RtlBitmap` and a dead-simple `Vec<bool>` reference model,
//! and assert they agree after every step. Allocators routinely pass
//! hand-written examples and then fail on operation 40,000 of a random
//! sequence; this is the test that finds that.

use kernel::rtl::bitmap::RtlBitmap;
use proptest::prelude::*;

/// Reference model: the obviously-correct bitmap. `true` == set/in-use.
struct Model {
    bits: Vec<bool>,
}

impl Model {
    fn new(size: usize) -> Self {
        Model {
            bits: vec![false; size],
        }
    }

    fn count_set(&self) -> usize {
        self.bits.iter().filter(|b| **b).count()
    }

    /// Mirror of `find_clear_bits_and_set` with the same hint+wrap policy:
    /// search [hint, end) then [0, hint+count); set and return the run.
    fn find_clear_bits_and_set(&mut self, count: usize, hint: usize) -> Option<usize> {
        if count == 0 || count > self.bits.len() {
            return None;
        }
        let hint = if hint >= self.bits.len() { 0 } else { hint };
        let n = self.bits.len();
        let scan = |from: usize, to: usize| -> Option<usize> {
            let to = to.min(n);
            let mut run = 0;
            let mut start = from;
            let mut i = from;
            while i < to {
                if self.bits[i] {
                    run = 0;
                    start = i + 1;
                } else {
                    run += 1;
                    if run == count {
                        return Some(start);
                    }
                }
                i += 1;
            }
            None
        };
        let found = scan(hint, n).or_else(|| scan(0, hint + count));
        if let Some(start) = found {
            for b in &mut self.bits[start..start + count] {
                *b = true;
            }
        }
        found
    }

    fn clear_bits(&mut self, start: usize, count: usize) {
        for b in &mut self.bits[start..start + count] {
            *b = false;
        }
    }
}

/// One operation in a randomized sequence.
#[derive(Debug, Clone)]
enum Op {
    Alloc { count: usize, hint: usize },
    Free { which: usize },
}

fn op_strategy(size: usize) -> impl Strategy<Value = Op> {
    prop_oneof![
        (1usize..=size.min(40), 0usize..size).prop_map(|(count, hint)| Op::Alloc { count, hint }),
        (0usize..64).prop_map(|which| Op::Free { which }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// RtlBitmap agrees with the reference model across long random
    /// alloc/free sequences, and never hands out an overlapping run.
    #[test]
    fn bitmap_matches_model(
        size in 1usize..512,
        ops in prop::collection::vec(0u8..2, 1..600),
        seeds in prop::collection::vec((1usize..40, 0usize..512), 1..600),
    ) {
        let words = size.div_ceil(64);
        let mut storage = vec![0u64; words];
        let mut bm = RtlBitmap::new(&mut storage, size);
        let mut model = Model::new(size);

        // Track live allocations so frees are valid and overlap is checkable.
        let mut live: Vec<(usize, usize)> = Vec::new();

        for (k, &which) in ops.iter().enumerate() {
            let op = if which == 0 {
                let (mut count, hint) = seeds[k % seeds.len()];
                count = count.min(size);
                Op::Alloc { count, hint: hint % size }
            } else {
                Op::Free { which: k }
            };

            match op {
                Op::Alloc { count, hint } => {
                    let got = bm.find_clear_bits_and_set(count, hint);
                    let exp = model.find_clear_bits_and_set(count, hint);
                    prop_assert_eq!(got, exp, "alloc(count={}, hint={}) diverged", count, hint);
                    if let Some(start) = got {
                        // No overlap with any live allocation.
                        for &(s, c) in &live {
                            let disjoint = start + count <= s || s + c <= start;
                            prop_assert!(disjoint, "overlap: new [{},{}) vs live [{},{})",
                                start, start + count, s, s + c);
                        }
                        live.push((start, count));
                    }
                }
                Op::Free { which } => {
                    if !live.is_empty() {
                        let idx = which % live.len();
                        let (s, c) = live.swap_remove(idx);
                        bm.clear_bits(s, c);
                        model.clear_bits(s, c);
                    }
                }
            }

            prop_assert_eq!(bm.count_set(), model.count_set(), "set-count diverged at step {}", k);
        }
    }

    /// Allocated runs are always within bounds and exactly `count` wide.
    #[test]
    fn allocated_runs_are_in_bounds(
        size in 1usize..256,
        reqs in prop::collection::vec(1usize..32, 1..100),
    ) {
        let words = size.div_ceil(64);
        let mut storage = vec![0u64; words];
        let mut bm = RtlBitmap::new(&mut storage, size);
        for count in reqs {
            if let Some(start) = bm.find_clear_bits_and_set(count, 0) {
                prop_assert!(start + count <= size);
                for i in start..start + count {
                    prop_assert!(bm.test_bit(i));
                }
            }
        }
    }
}
