// NOTE: plain `//` comments, not `//!` module docs, and no inner attributes:
// this file is `include!`d by the AOT runtime, which can carry neither.
//
// The 256-bit `#{char}` value — one bit per `char`, implemented and tested on
// its own before anything is switched over to it.
//
// **Not wired up yet.** Switching a type's representation is atomic: the
// runtime, codegen, and every checked-in `.clif` have to agree, so there is no
// way to land it a piece at a time. What *can* be de-risked first is this — the
// layout, its invariants, and the operations that read and build it, proven by
// ordinary Rust tests with no compiler in the loop. `str24.rs` was staged the
// same way and says more about why.
//
// # One copy, two runtimes
//
// Shared verbatim with the AOT runtime, which `include!`s this file inside a
// `mod charset`, for the reason `str24.rs` is shared: a layout that disagrees
// between the two runtimes is silent memory corruption rather than a failing
// test. So it compiles under `no_std` and must not reach for `std`, `Vec` or
// `std::alloc` — which costs nothing here, because a char set never allocates.
//
// # The value
//
// ```text
//   words[0]        words[1]        words[2]        words[3]
//   chars 0..64     chars 64..128   chars 128..192  chars 192..256
// ```
//
// Bit `c & 63` of word `c >> 6` is set exactly when `c` is a member. A `char`
// in AIPL is one byte, so 256 bits covers the type *totally*: every value is
// representable, there is no fallback path, and no set of chars can overflow
// it. That totality is the whole point — it is what lets membership be a shift
// and a mask with no bounds check, no hash, and no pointer to chase.
//
// The value is 32 bytes and carries no pointer, so it is copied rather than
// refcounted: a `#{char}` never allocates, never frees, and two of them are
// equal exactly when their four words are. Set algebra is the matching bitwise
// operation on all four words — a union is four `or`s, whatever the sizes.
//
// # Order
//
// A bit scan visits members in ascending `char` order, which is what `#<{char}`
// promises and what `#{char}` (which promises nothing) is free to do. `#>{char}`
// scans the other way. So one representation serves all three orders, and
// `next_from` / `prev_from` are the two directions a `for` over a set needs.

/// Bytes a `#{char}` occupies inline — as an array element, a struct field, an
/// optional's payload. Must agree with what `abi_elem_size` reports for it.
pub(crate) const CHARSET_SIZE: usize = 32;

/// How many chars there are, and so how many bits a set has.
const CHARSET_BITS: usize = 256;

/// A `#{char}`: one bit per char, no heap, no refcount.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CharSet {
    pub words: [u64; 4],
}

impl CharSet {
    /// The empty set. Also what zeroed memory reads as, which matters because
    /// fresh array slots and sret buffers arrive zero-filled.
    pub(crate) fn empty() -> CharSet {
        CharSet { words: [0; 4] }
    }

    /// Every char — the complement of the empty set.
    pub(crate) fn full() -> CharSet {
        CharSet { words: [!0; 4] }
    }

    /// The set of exactly the bytes in `bytes`. Duplicates are free: setting a
    /// bit twice is setting it once, which is why building a set from a literal
    /// needs no de-duplication pass.
    pub(crate) fn from_bytes(bytes: &[u8]) -> CharSet {
        let mut s = CharSet::empty();
        for &b in bytes {
            s.insert(b);
        }
        s
    }

    /// Whether `c` is a member: one shift and one mask.
    pub(crate) fn contains(self, c: u8) -> bool {
        let c = c as usize;
        self.words[c >> 6] >> (c & 63) & 1 != 0
    }

    pub(crate) fn insert(&mut self, c: u8) {
        let c = c as usize;
        self.words[c >> 6] |= 1u64 << (c & 63);
    }

    pub(crate) fn remove(&mut self, c: u8) {
        let c = c as usize;
        self.words[c >> 6] &= !(1u64 << (c & 63));
    }

    pub(crate) fn union(self, other: CharSet) -> CharSet {
        self.zip(other, |a, b| a | b)
    }

    pub(crate) fn intersect(self, other: CharSet) -> CharSet {
        self.zip(other, |a, b| a & b)
    }

    /// The members of `self` that are not in `other`.
    pub(crate) fn difference(self, other: CharSet) -> CharSet {
        self.zip(other, |a, b| a & !b)
    }

    pub(crate) fn is_subset_of(self, other: CharSet) -> bool {
        self.difference(other).is_empty()
    }

    fn zip(self, other: CharSet, f: impl Fn(u64, u64) -> u64) -> CharSet {
        let mut out = CharSet::empty();
        for i in 0..4 {
            out.words[i] = f(self.words[i], other.words[i]);
        }
        out
    }

    /// How many members — a popcount per word, not a walk.
    pub(crate) fn len(self) -> usize {
        let mut n = 0;
        for i in 0..4 {
            n += self.words[i].count_ones() as usize;
        }
        n
    }

    pub(crate) fn is_empty(self) -> bool {
        self.words == [0; 4]
    }

    /// The lowest member at or above `from`, or `None` — ascending iteration.
    /// `from` past the last char answers `None`, so a loop can hand back
    /// `member + 1` without a bounds test of its own.
    pub(crate) fn next_from(self, from: usize) -> Option<u8> {
        if from >= CHARSET_BITS {
            return None;
        }
        let mut i = from >> 6;
        // The bits at or above `from` within its own word; whole words after.
        let mut w = self.words[i] & (!0u64 << (from & 63));
        loop {
            if w != 0 {
                return Some(((i << 6) + w.trailing_zeros() as usize) as u8);
            }
            i += 1;
            if i == 4 {
                return None;
            }
            w = self.words[i];
        }
    }

    /// The highest member at or below `from`, or `None` — descending
    /// iteration, and the mirror of `next_from`. The cursor is signed because a
    /// descending walk steps to `member - 1`, and from char 0 that is "below
    /// the first char", not a huge unsigned one. Above the last char clamps.
    pub(crate) fn prev_from(self, from: i64) -> Option<u8> {
        if from < 0 {
            return None;
        }
        let from = if from >= CHARSET_BITS as i64 {
            CHARSET_BITS - 1
        } else {
            from as usize
        };
        let mut i = from >> 6;
        let bit = from & 63;
        // The bits at or below `from` within its own word; whole words before.
        let mask = if bit == 63 {
            !0u64
        } else {
            (1u64 << (bit + 1)) - 1
        };
        let mut w = self.words[i] & mask;
        loop {
            if w != 0 {
                return Some(((i << 6) + (63 - w.leading_zeros() as usize)) as u8);
            }
            if i == 0 {
                return None;
            }
            i -= 1;
            w = self.words[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every char round-trips, which is the totality claim: 256 bits cover the
    // type, so there is no value the representation cannot hold and no
    // fallback path to get wrong.
    #[test]
    fn every_char_is_representable() {
        for c in 0..=255u8 {
            let mut s = CharSet::empty();
            assert!(!s.contains(c));
            s.insert(c);
            assert!(s.contains(c), "{c} not a member after insert");
            assert_eq!(s.len(), 1);
            // Only that one: no neighbouring bit was disturbed.
            for other in 0..=255u8 {
                assert_eq!(s.contains(other), other == c);
            }
            s.remove(c);
            assert!(s.is_empty());
        }
    }

    #[test]
    fn empty_is_zeroed() {
        // Fresh array slots and sret buffers arrive zero-filled, so the empty
        // set has to be what zeroed memory already reads as.
        assert_eq!(CharSet::empty().words, [0u64; 4]);
        assert_eq!(CharSet::empty().len(), 0);
        assert!(CharSet::empty().is_empty());
        assert_eq!(core::mem::size_of::<CharSet>(), CHARSET_SIZE);
    }

    #[test]
    fn full_holds_every_char() {
        let all = CharSet::full();
        assert_eq!(all.len(), 256);
        for c in 0..=255u8 {
            assert!(all.contains(c));
        }
    }

    #[test]
    fn insert_is_idempotent() {
        // Setting a bit twice is setting it once — which is what lets a set
        // literal skip de-duplicating its elements.
        let mut s = CharSet::empty();
        s.insert(b'x');
        s.insert(b'x');
        assert_eq!(s.len(), 1);
        assert_eq!(CharSet::from_bytes(b"aabbcc"), CharSet::from_bytes(b"abc"));
    }

    #[test]
    fn set_algebra() {
        let lo = CharSet::from_bytes(b"abcd");
        let hi = CharSet::from_bytes(b"cdef");
        assert_eq!(lo.union(hi), CharSet::from_bytes(b"abcdef"));
        assert_eq!(lo.intersect(hi), CharSet::from_bytes(b"cd"));
        assert_eq!(lo.difference(hi), CharSet::from_bytes(b"ab"));
        assert_eq!(hi.difference(lo), CharSet::from_bytes(b"ef"));
        assert!(CharSet::from_bytes(b"ab").is_subset_of(lo));
        assert!(!hi.is_subset_of(lo));
        // A set is its own subset, and everything is a subset of `full`.
        assert!(lo.is_subset_of(lo));
        assert!(lo.is_subset_of(CharSet::full()));
        assert!(CharSet::empty().is_subset_of(lo));
    }

    #[test]
    fn algebra_spans_word_boundaries() {
        // The four words are only ever an implementation detail if the
        // operations cross them, so drive members either side of each edge.
        let edges = [0u8, 63, 64, 127, 128, 191, 192, 255];
        let s = CharSet::from_bytes(&edges);
        assert_eq!(s.len(), edges.len());
        for &c in &edges {
            assert!(s.contains(c), "{c} lost across a word boundary");
        }
        assert_eq!(s.union(CharSet::empty()), s);
        assert_eq!(s.intersect(CharSet::full()), s);
        assert_eq!(s.difference(s), CharSet::empty());
        assert_eq!(CharSet::full().difference(s).len(), 256 - edges.len());
    }

    // Ascending iteration: `next_from` walks the members in char order and
    // stops, which is what a `for` over a `#{char}` compiles to.
    #[test]
    fn next_from_walks_ascending() {
        let s = CharSet::from_bytes(b"cab");
        let mut seen = [0u8; 8];
        let mut n = 0;
        let mut at = 0usize;
        while let Some(c) = s.next_from(at) {
            seen[n] = c;
            n += 1;
            at = c as usize + 1;
        }
        assert_eq!(&seen[..n], b"abc");
    }

    #[test]
    fn prev_from_walks_descending() {
        let s = CharSet::from_bytes(b"cab");
        let mut seen = [0u8; 8];
        let mut n = 0;
        let mut at = 255i64;
        while let Some(c) = s.prev_from(at) {
            seen[n] = c;
            n += 1;
            // Stepping below char 0 is what ends the walk.
            at = c as i64 - 1;
        }
        assert_eq!(&seen[..n], b"cba");
    }

    #[test]
    fn iteration_edges() {
        let empty = CharSet::empty();
        assert_eq!(empty.next_from(0), None);
        assert_eq!(empty.prev_from(255), None);

        // The first and last chars are members, which is where an off-by-one
        // in the word masks shows up.
        let ends = CharSet::from_bytes(&[0, 255]);
        assert_eq!(ends.next_from(0), Some(0));
        assert_eq!(ends.next_from(1), Some(255));
        assert_eq!(ends.next_from(255), Some(255));
        assert_eq!(ends.next_from(256), None);
        assert_eq!(ends.prev_from(255), Some(255));
        assert_eq!(ends.prev_from(254), Some(0));
        assert_eq!(ends.prev_from(0), Some(0));
        assert_eq!(ends.prev_from(-1), None);
        // Past the last char clamps rather than answering nothing.
        assert_eq!(ends.prev_from(999), Some(255));

        // Every member is found from its own index, in both directions.
        let all = CharSet::full();
        for c in 0..=255u8 {
            assert_eq!(all.next_from(c as usize), Some(c));
            assert_eq!(all.prev_from(c as i64), Some(c));
        }
    }

    #[test]
    fn iteration_agrees_with_membership() {
        // A walk visits exactly the members, in order, however they are spread.
        for seed in 0..64u32 {
            let mut s = CharSet::empty();
            for c in 0..=255u8 {
                if (c as u32).wrapping_mul(2654435761) % 64 < seed {
                    s.insert(c);
                }
            }
            let mut count = 0;
            let mut at = 0usize;
            let mut last: Option<u8> = None;
            while let Some(c) = s.next_from(at) {
                assert!(s.contains(c));
                if let Some(p) = last {
                    assert!(p < c, "ascending walk went backwards");
                }
                last = Some(c);
                count += 1;
                at = c as usize + 1;
            }
            assert_eq!(count, s.len());
        }
    }

    #[test]
    fn equality_is_the_four_words() {
        // Two sets are equal exactly when their members are — no ordering, no
        // capacity, nothing else in the value to differ on.
        assert_eq!(CharSet::from_bytes(b"abc"), CharSet::from_bytes(b"cba"));
        assert_ne!(CharSet::from_bytes(b"abc"), CharSet::from_bytes(b"abd"));
        assert_eq!(CharSet::empty(), CharSet::from_bytes(b""));
    }

    // The classes the lexer scans linearly today, as the sets they want to be.
    #[test]
    fn identifier_classes() {
        let start = CharSet::from_bytes(b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_");
        let cont = start.union(CharSet::from_bytes(b"0123456789"));
        assert_eq!(start.len(), 53);
        assert_eq!(cont.len(), 63);
        assert!(start.is_subset_of(cont));
        assert!(cont.contains(b'z') && cont.contains(b'0') && cont.contains(b'_'));
        assert!(!start.contains(b'0'));
        assert!(!cont.contains(b'-') && !cont.contains(b' '));
    }
}

// The two entry points a walk over a `#{char}` calls, one per step. Membership,
// length, equality and union are emitted inline — they are the hot ones, and
// each is a handful of instructions — but a walk is inherently per-element, so
// it goes through a call rather than an unrolled four-word scan at every loop
// site.
//
// Both answer -1 for "no more", which is what lets the emitted loop test one
// signed value instead of carrying a separate done flag.

#[no_mangle]
pub(crate) extern "C" fn aipl_charset_next(set: *const CharSet, from: i64) -> i64 {
    let s = unsafe { *set };
    let from = if from < 0 { 0 } else { from as usize };
    match s.next_from(from) {
        Some(c) => c as i64,
        None => -1,
    }
}

#[no_mangle]
pub(crate) extern "C" fn aipl_charset_prev(set: *const CharSet, from: i64) -> i64 {
    let s = unsafe { *set };
    match s.prev_from(from) {
        Some(c) => c as i64,
        None => -1,
    }
}
