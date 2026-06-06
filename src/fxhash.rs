//! a fast, non-cryptographic hasher for our own internal hash-map keys (glyph
//! atlas cache, cluster intern table). the std HashMap defaults to SipHash, which
//! is DoS-resistant but slow — overkill for keys we generate ourselves. this is
//! the well-known FxHash (multiply-rotate-xor, as used by rustc) hand-rolled so
//! no dependency is added. it is NOT collision-resistant against adversarial
//! input and must not be used for untrusted/external keys

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
const ROTATE: u32 = 5;

#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(ROTATE) ^ i).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            self.add(u64::from_le_bytes(bytes[..8].try_into().unwrap()));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            self.add(u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64);
            bytes = &bytes[4..];
        }
        if bytes.len() >= 2 {
            self.add(u16::from_le_bytes(bytes[..2].try_into().unwrap()) as u64);
            bytes = &bytes[2..];
        }
        if let Some(&b) = bytes.first() {
            self.add(b as u64);
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// a HashMap using the fast internal hasher; drop-in for our own-key caches
pub type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hash;

    fn h<T: Hash>(v: &T) -> u64 {
        let mut s = FxHasher::default();
        v.hash(&mut s);
        s.finish()
    }

    #[test]
    fn deterministic_and_distinct() {
        // same input -> same hash (determinism is the only correctness need)
        assert_eq!(h(&(b'A', 0u32, true)), h(&(b'A', 0u32, true)));
        // distinct small keys don't collide (sanity for distribution)
        assert_ne!(h(&(b'A', 0u32, false)), h(&(b'A', 0u32, true)));
        assert_ne!(h(&"abc"), h(&"abd"));
        // the map round-trips as a normal HashMap
        let mut m: FxHashMap<u32, &str> = FxHashMap::default();
        m.insert(7, "seven");
        m.insert(42, "answer");
        assert_eq!(m.get(&7), Some(&"seven"));
        assert_eq!(m.get(&42), Some(&"answer"));
        assert_eq!(m.get(&1), None);
    }
}
