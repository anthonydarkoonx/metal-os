//! A random-number generator for the kernel's address-space layout randomisation. `no_std`, written from RFC 8439.
//!
//! * `chacha20_block` is the block function of RFC 8439 section 2.3 (checked against the RFC's test vector).
//! * `Rng` is the "fast key erasure" construction: each refill runs one ChaCha20 block, keeps the first 32 bytes of
//!   it as the *next key* and hands out the other 32 bytes, so an attacker who learns the state after a draw cannot
//!   recover earlier outputs, and one output reveals nothing about the next (unlike SplitMix64, whose output is a
//!   bijective function of its state).
//! * `mix` folds more entropy (timestamps, device data) into the key at any time.
//!
//! This is not a vetted cryptographic RNG; it is meant to make layouts unpredictable to programs that can see their own.
#![no_std]

const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]; // "expand 32-byte k"

#[inline(always)]
fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// RFC 8439 section 2.3: the 64-byte block for `key`, block `counter` and `nonce`.
pub fn chacha20_block(key: &[u32; 8], counter: u32, nonce: &[u32; 3]) -> [u8; 64] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&SIGMA);
    init[4..12].copy_from_slice(key);
    init[12] = counter;
    init[13..16].copy_from_slice(nonce);
    let mut s = init;
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

fn words(b: &[u8]) -> [u32; 8] {
    let mut k = [0u32; 8];
    for i in 0..8 {
        k[i] = u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
    }
    k
}

pub struct Rng {
    key: [u32; 8],
    /// Output not handed out yet (32 bytes per refill), and how much of it is used.
    buf: [u8; 32],
    used: usize,
    /// Counts refills and mixes, so that no two blocks are made from the same (key, counter).
    counter: u32,
}

impl Rng {
    pub fn from_seed(seed: [u8; 32]) -> Rng {
        Rng { key: words(&seed), buf: [0; 32], used: 32, counter: 0 }
    }

    /// Fold `data` into the key: for every 32-byte chunk (the last one zero-padded), the new key is the first half of
    /// the ChaCha20 block of (old key XOR chunk), under a counter no draw uses.
    pub fn mix(&mut self, data: &[u8]) {
        let mut i = 0;
        loop {
            let mut chunk = [0u8; 32];
            let n = (data.len() - i).min(32);
            chunk[..n].copy_from_slice(&data[i..i + n]);
            let c = words(&chunk);
            let mut k = self.key;
            for j in 0..8 {
                k[j] ^= c[j];
            }
            let block = chacha20_block(&k, 0xFFFF_FFFF, &[data.len() as u32, i as u32, 0x6d69_7821]);
            self.key = words(&block[..32]);
            self.used = 32; // (what was buffered came from the old key: discard it)
            i += 32;
            if i >= data.len() {
                break;
            }
        }
    }

    fn refill(&mut self) {
        let block = chacha20_block(&self.key, self.counter, &[0, 0, 0x6e65_7874]);
        self.counter = self.counter.wrapping_add(1);
        self.key = words(&block[..32]); // the old key is gone
        self.buf.copy_from_slice(&block[32..]);
        self.used = 0;
    }

    pub fn fill(&mut self, out: &mut [u8]) {
        for b in out.iter_mut() {
            if self.used == 32 {
                self.refill();
            }
            *b = self.buf[self.used];
            self.buf[self.used] = 0; // (handed out: not kept)
            self.used += 1;
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill(&mut b);
        u64::from_le_bytes(b)
    }

    /// A value in `0..n`, without modulo bias.
    pub fn below(&mut self, n: u64) -> u64 {
        if n <= 1 {
            return 0;
        }
        let zone = u64::MAX - (u64::MAX % n); // draws at or above this would favour small values
        loop {
            let v = self.next_u64();
            if v < zone {
                return v % n;
            }
        }
    }
}

#[cfg(test)]
mod tests;
