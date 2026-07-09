use crate::utils::align_up;
use core::slice;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug)]
pub struct AtomicBitmapCore<'a> {
    bits: &'a [AtomicU64],
    bit_count: usize,
}

impl<'a> AtomicBitmapCore<'a> {
    /// # Safety
    /// The caller is required to ensure that the slice is large enough to hold
    /// the specified number of bits.
    pub unsafe fn new_unchecked(bits: &'a [AtomicU64], bit_count: usize) -> Self {
        Self { bit_count, bits }
    }

    pub fn new(bits: &'a [AtomicU64], bit_count: usize) -> Self {
        assert!(bit_count <= bits.len() & 64);
        // SAFETY: the size check above is sufficient to guarantee safety.
        unsafe { Self::new_unchecked(bits, bit_count) }
    }

    pub fn find_and_set_single_bit(&self, start_hint: usize) -> Option<usize> {
        // Search each word, starting from the hint word, looking for a single
        // clear bit that can be set.
        let mut word = start_hint / 64;
        let word_count = self.bits.len();
        for _attempt in 0..word_count {
            let mut bits = self.bits[word].load(Ordering::Relaxed);
            // Only process words that have at least one clear bit.
            while !bits != 0 {
                let index = (!bits).trailing_zeros() as usize;
                let full_index = (word * 64) + index;
                // Do not return any bits beyond the specified bitmap size.
                if full_index >= self.bit_count {
                    break;
                }
                match self.bits[word].compare_exchange_weak(
                    bits,
                    bits | (1 << index),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Some(full_index),
                    Err(new_bits) => bits = new_bits,
                }
            }

            word += 1;
            if word == word_count {
                word = 0;
            }
        }
        None
    }

    fn find_bits_in_word(
        &self,
        word: usize,
        mut bit_mask: u64,
        bit_count: usize,
    ) -> (usize, usize) {
        let bits = self.bits[word].load(Ordering::Relaxed);
        let mut bit = 0;

        // Check to see whether a contiguous run of the entire length can be
        // found.
        while bit <= 64 - bit_count {
            if (bits & bit_mask) == 0 {
                return (bit_count, bit);
            }
            bit += 1;
            bit_mask <<= 1;
        }

        // Check to see whether a shorter contiguous run of bits can be found
        // at the most significant end of the word.
        bit = bits.leading_zeros() as usize;
        (bit, 64 - bit)
    }

    fn check_clear_bit_run(&self, mut word: usize, mut bit_count: usize) -> bool {
        // Only attempt a search if the bitmap is large enough to contain the
        // requested bits.
        if (word * 64) + bit_count > self.bit_count {
            return false;
        }

        // Check for entire words that must be clear.
        while bit_count >= 64 {
            if self.bits[word].load(Ordering::Relaxed) != 0 {
                return false;
            }

            bit_count -= 64;
            word += 1;
        }

        // Check the final word if required.
        bit_count == 0
            || (self.bits[word].load(Ordering::Relaxed) & Self::mask_below(bit_count)) == 0
    }

    pub fn find_clear_bit_run(&self, length: usize, start_hint: usize) -> Option<usize> {
        // Begin the search from the specified starting hint.  If the end of
        // the bitmap is reached before finding a suitable run, then wrap
        // around to the beginning.
        let mut word = Self::word_index(start_hint);
        let word_count = self.bits.len();

        // Calculate a bit mask for an acceptable contiguous run of bits that
        // can be found in the first word.  This initially places the mask at
        // the least significant bit position, and it is shifted during the
        // search for bits.
        let (first_bit_mask, first_bit_count) = if length < 64 {
            ((1u64 << length) - 1, length)
        } else {
            (!0u64, 64)
        };

        // Calculate the last word that could possibly contain the start of a
        // suitable bit run.
        let last_word = Self::word_index(self.bit_count - length);

        let mut attempt = 0;
        while attempt < word_count {
            if !self.bits[word].load(Ordering::Relaxed) != 0 {
                // Count the number of contiguous bits that can be found in
                // this word.
                let (bit_count, bit_index) =
                    self.find_bits_in_word(word, first_bit_mask, first_bit_count);
                if bit_count == length {
                    // The full bit count was found, so return it now.
                    return Some((word * 64) + bit_index);
                }
                if bit_count != 0 {
                    // At least some suitable bit run was found.  Check to see
                    // whether the next bits in sequence are also clear.
                    if self.check_clear_bit_run(word + 1, length - bit_count) {
                        return Some((word * 64) + bit_index);
                    }
                }
            }

            // Advance to the next word to continue the search.  Wrap around
            // once the remaining space in the bitmap is no longer sufficient
            // to contain the requested number of bits.
            word = if word == last_word {
                attempt = self.bits.len() - Self::word_index(start_hint);
                0
            } else {
                attempt += 1;
                word + 1
            }
        }

        None
    }

    pub fn set_clear_bit_run(&self, start_index: usize, length: usize) -> bool {
        let start_word = Self::word_index(start_index);
        let start_mask_raw = Self::mask_above(start_index);

        let end_index = start_index + length;
        let end_word = Self::word_index(end_index);
        let end_mask_raw = Self::mask_below(end_index);

        // Calculate the mask to apply to the starting and ending words based
        // on whether it overlaps the ending word.
        let (start_mask, end_mask) = if start_word == end_word {
            (start_mask_raw & end_mask_raw, 0)
        } else {
            (start_mask_raw, end_mask_raw)
        };
        let mut bits = self.bits[start_word].load(Ordering::Relaxed);
        loop {
            // Verify that the bits are all clear.
            if (bits & start_mask) != 0 {
                return false;
            }
            // Attempt to set the bits.  If they have changed, then they must
            // be reexamined.
            match self.bits[start_word].compare_exchange_weak(
                bits,
                bits | start_mask,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Err(new_bits) => bits = new_bits,
                Ok(_) => break,
            }
        }

        let mut word = start_word + 1;
        while word < end_word {
            // Each intermediate word must be zero.  If it isn't, then all
            // previous work must be unwound.
            if self.bits[word]
                .compare_exchange(0, !0, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                self.unwind_set_bits(word, start_word, start_mask);
                return false;
            }
            word += 1;
        }

        // If no bits need to be set in the final word, then the work is
        // complete.
        if end_mask == 0 {
            return true;
        }

        // Attempt to set the necessary bits in the final word.
        bits = self.bits[end_word].load(Ordering::Relaxed);
        while (bits & end_mask) == 0 {
            match self.bits[end_word].compare_exchange_weak(
                bits,
                bits | end_mask,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Err(new_bits) => bits = new_bits,
                Ok(_) => return true,
            }
        }

        // Unwind the work that has been done so far.
        self.unwind_set_bits(end_word, start_word, start_mask);
        false
    }

    fn unwind_set_bits(&self, mut end_word: usize, start_word: usize, start_mask: u64) {
        end_word -= 1;
        while end_word > start_word {
            self.bits[end_word].store(0, Ordering::Relaxed);
            end_word -= 1;
        }
        self.bits[start_word].fetch_and(!start_mask, Ordering::Relaxed);
    }

    pub fn clear_bit_run(&self, start_index: usize, length: usize) {
        let mut word = Self::word_index(start_index);
        let start_mask = Self::mask_above(start_index);

        let end_index = start_index + length;
        let end_word = Self::word_index(end_index);
        let end_mask = Self::mask_below(end_index);

        // Handle the case of the start and end positions being in the same
        // word.
        if word == end_word {
            self.bits[word].fetch_and(!(start_mask & end_mask), Ordering::Relaxed);
        } else {
            self.bits[word].fetch_and(!start_mask, Ordering::Relaxed);
            word += 1;
            while word < end_word {
                self.bits[word].store(0, Ordering::Relaxed);
                word += 1;
            }
            if end_mask != 0 {
                self.bits[word].fetch_and(!end_mask, Ordering::Relaxed);
            }
        }
    }

    fn word_index(index: usize) -> usize {
        index / 64
    }

    fn mask_above(index: usize) -> u64 {
        !0 << (index & 63)
    }

    fn mask_below(index: usize) -> u64 {
        (1 << (index & 63)) - 1
    }
}

#[derive(Debug)]
pub struct AtomicBitmap {
    bits: Vec<u64>,
    bit_count: usize,
}

impl AtomicBitmap {
    pub fn new(bit_count: usize) -> Self {
        // Allocate a backing store for the bitmap.
        let word_count = align_up(bit_count, 64) / 64;
        let bits = vec![0; word_count];
        Self { bits, bit_count }
    }

    fn bitmap_core(&self) -> AtomicBitmapCore<'_> {
        // SAFETY: the bit vector is known to be large enough for the bit
        // count, and because it is never used outside of the atomic flows,
        // the slice can safely be treated as a slice of atomics instead of
        // integers..
        unsafe {
            let atomic_bits =
                slice::from_raw_parts(self.bits.as_ptr() as *const AtomicU64, self.bits.len());
            AtomicBitmapCore::new_unchecked(atomic_bits, self.bit_count)
        }
    }

    pub fn find_and_set_single_bit(&self, start_hint: usize) -> Option<usize> {
        self.bitmap_core().find_and_set_single_bit(start_hint)
    }

    pub fn find_clear_bit_run(&self, length: usize, start_hint: usize) -> Option<usize> {
        self.bitmap_core().find_clear_bit_run(length, start_hint)
    }

    pub fn set_clear_bit_run(&self, start_index: usize, length: usize) -> bool {
        self.bitmap_core().set_clear_bit_run(start_index, length)
    }

    pub fn clear_bit_run(&self, start_index: usize, length: usize) {
        self.bitmap_core().clear_bit_run(start_index, length);
    }
}

#[cfg(test)]
mod tests {
    use super::AtomicBitmap;

    #[test]
    fn test_atomic_bitmap() {
        let bitmap = AtomicBitmap::new(1024);

        // Verify that it is possible to find a single bit, a run within
        // 64 bits, and a run exceeding 64 bits.  All of these should be
        // found at bit 0 since the bitmap is still empty.
        assert_eq!(bitmap.find_clear_bit_run(1, 0), Some(0));
        assert_eq!(bitmap.find_clear_bit_run(16, 0), Some(0));
        assert_eq!(bitmap.find_clear_bit_run(70, 0), Some(0));

        // Verify that it is possible to set the top 32 bits of the bitmap,
        // then the 64 bits below that (to span a word boundary).
        assert!(bitmap.set_clear_bit_run(1024 - 32, 32));
        assert!(bitmap.set_clear_bit_run(1024 - 32 - 64, 64));

        // Set the remainder of the bits, and then verify that no bit runs can
        // be set.
        assert!(bitmap.set_clear_bit_run(0, 32));
        assert!(bitmap.set_clear_bit_run(32, 1024 - 32 - 32 - 64));

        assert!(!bitmap.set_clear_bit_run(0, 1));
        assert!(!bitmap.set_clear_bit_run(63, 1));
        assert!(!bitmap.set_clear_bit_run(63, 2));

        // Verify that no single bit can be found.
        assert_eq!(bitmap.find_and_set_single_bit(0), None);
        assert_eq!(bitmap.find_and_set_single_bit(512), None);

        // Clear two bits and attempt to set them using wraparound searches.
        bitmap.clear_bit_run(511, 2);
        assert_eq!(bitmap.find_and_set_single_bit(512), Some(512));
        assert_eq!(bitmap.find_and_set_single_bit(512), Some(511));
        assert_eq!(bitmap.find_and_set_single_bit(512), None);

        // Verify that no single bit can be found with a wraparound search.
        assert_eq!(bitmap.find_clear_bit_run(1, 512), None);

        // Clear two bits and verify that one or both can be found using a
        // wraparound search.
        bitmap.clear_bit_run(511, 2);
        assert_eq!(bitmap.find_clear_bit_run(1, 512), Some(512));
        assert_eq!(bitmap.find_clear_bit_run(1, 512 + 64), Some(511));
        assert_eq!(bitmap.find_clear_bit_run(2, 512), Some(511));

        // Clear a cluster of bits and verify that free bits can be found
        // in a single word or across a span of words.
        bitmap.clear_bit_run(32, 128);
        assert_eq!(bitmap.find_clear_bit_run(8, 0), Some(32));
        assert_eq!(bitmap.find_clear_bit_run(64, 0), Some(32));
        assert_eq!(bitmap.find_clear_bit_run(128, 0), Some(32));

        // Attempt to set a run of bits that is not fully clear and verify that
        // they remain clear.
        assert!(!bitmap.set_clear_bit_run(32, 129));
        assert_eq!(bitmap.find_clear_bit_run(128, 0), Some(32));

        // Verify that the clear bits can be set.
        assert!(bitmap.set_clear_bit_run(32, 128));

        // Clear some bits ahd verify that a run of bits is correctly found
        // even following a deceptive start.
        bitmap.clear_bit_run(32, 128);
        bitmap.clear_bit_run(280, 130);
        assert_eq!(bitmap.find_clear_bit_run(130, 0), Some(280));
        assert_eq!(bitmap.find_clear_bit_run(128, 0), Some(32));
        assert!(bitmap.set_clear_bit_run(64, 1));
        assert_eq!(bitmap.find_clear_bit_run(128, 0), Some(280));
    }
}
