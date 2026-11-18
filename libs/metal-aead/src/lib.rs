//! ChaCha20-Poly1305 authenticated encryption (RFC 8439 section 2.8), `no_std`, no heap, in place.
//!
//! The kernel uses it to seal the pages it hands to the pager (pager.rs): the pager stores ciphertext it cannot read or
//! change without the kernel noticing. The block function is `metal-rand`'s (checked there against the RFC);
//! Poly1305 is the 26-bit-limb form of RFC 8439 section 2.5, and the whole is checked against vectors made by an
//! independent implementation (`tests.rs`).
#![forbid(unsafe_code)]
#![no_std]

use metal_rand::chacha20_block;

pub const KEY_LEN: usize = 32;
pub const TAG_LEN: usize = 16;

pub type Key = [u32; 8];
pub type Tag = [u8; TAG_LEN];

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Key words from 32 bytes (little endian).
pub fn key_from_bytes(b: &[u8; KEY_LEN]) -> Key {
    core::array::from_fn(|i| le32(&b[4 * i..]))
}

fn nonce_words(nonce: &[u8; 12]) -> [u32; 3] {
    [le32(&nonce[0..]), le32(&nonce[4..]), le32(&nonce[8..])]
}

/// XOR the ChaCha20 keystream (block counter starting at `counter`) into `data`.
fn stream(key: &Key, nonce: &[u32; 3], counter: u32, data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(64).enumerate() {
        let block = chacha20_block(key, counter + i as u32, nonce);
        for (d, k) in chunk.iter_mut().zip(block.iter()) {
            *d ^= k;
        }
    }
}

struct Poly1305 {
    r: [u32; 5],
    h: [u32; 5],
    pad: [u32; 4],
}

impl Poly1305 {
    fn new(key: &[u8; 32]) -> Poly1305 {
        Poly1305 {
            r: [
                le32(&key[0..]) & 0x3ff_ffff,
                (le32(&key[3..]) >> 2) & 0x3ff_ff03,
                (le32(&key[6..]) >> 4) & 0x3ff_c0ff,
                (le32(&key[9..]) >> 6) & 0x3f0_3fff,
                (le32(&key[12..]) >> 8) & 0x00f_ffff,
            ],
            h: [0; 5],
            pad: [le32(&key[16..]), le32(&key[20..]), le32(&key[24..]), le32(&key[28..])],
        }
    }

    /// One full 16-byte block (the hash input is always padded to whole blocks here).
    fn block(&mut self, m: &[u8]) {
        let [r0, r1, r2, r3, r4] = self.r.map(u64::from);
        let (s1, s2, s3, s4) = (r1 * 5, r2 * 5, r3 * 5, r4 * 5);
        let h0 = self.h[0] as u64 + (le32(&m[0..]) & 0x3ff_ffff) as u64;
        let h1 = self.h[1] as u64 + ((le32(&m[3..]) >> 2) & 0x3ff_ffff) as u64;
        let h2 = self.h[2] as u64 + ((le32(&m[6..]) >> 4) & 0x3ff_ffff) as u64;
        let h3 = self.h[3] as u64 + ((le32(&m[9..]) >> 6) & 0x3ff_ffff) as u64;
        let h4 = self.h[4] as u64 + ((le32(&m[12..]) >> 8) | (1 << 24)) as u64;
        let d0 = h0 * r0 + h1 * s4 + h2 * s3 + h3 * s2 + h4 * s1;
        let mut d1 = h0 * r1 + h1 * r0 + h2 * s4 + h3 * s3 + h4 * s2;
        let mut d2 = h0 * r2 + h1 * r1 + h2 * r0 + h3 * s4 + h4 * s3;
        let mut d3 = h0 * r3 + h1 * r2 + h2 * r1 + h3 * r0 + h4 * s4;
        let mut d4 = h0 * r4 + h1 * r3 + h2 * r2 + h3 * r1 + h4 * r0;
        const M: u64 = 0x3ff_ffff;
        let mut c = d0 >> 26;
        let mut n0 = d0 & M;
        d1 += c;
        c = d1 >> 26;
        let mut n1 = d1 & M;
        d2 += c;
        c = d2 >> 26;
        let n2 = d2 & M;
        d3 += c;
        c = d3 >> 26;
        let n3 = d3 & M;
        d4 += c;
        c = d4 >> 26;
        let n4 = d4 & M;
        n0 += c * 5;
        c = n0 >> 26;
        n0 &= M;
        n1 += c;
        self.h = [n0 as u32, n1 as u32, n2 as u32, n3 as u32, n4 as u32];
    }

    /// Feed `data` padded with zeros to a multiple of 16 bytes.
    fn padded(&mut self, data: &[u8]) {
        let mut chunks = data.chunks_exact(16);
        for c in &mut chunks {
            self.block(c);
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut last = [0u8; 16];
            last[..rest.len()].copy_from_slice(rest);
            self.block(&last);
        }
    }

    fn finish(self) -> Tag {
        const M: u32 = 0x3ff_ffff;
        let mut h = self.h;
        let mut c = h[1] >> 26;
        h[1] &= M;
        h[2] += c;
        c = h[2] >> 26;
        h[2] &= M;
        h[3] += c;
        c = h[3] >> 26;
        h[3] &= M;
        h[4] += c;
        c = h[4] >> 26;
        h[4] &= M;
        h[0] += c * 5;
        c = h[0] >> 26;
        h[0] &= M;
        h[1] += c;
        // h - p, and keep it if there was no borrow
        let mut g = [0u32; 5];
        g[0] = h[0] + 5;
        c = g[0] >> 26;
        g[0] &= M;
        for i in 1..4 {
            g[i] = h[i] + c;
            c = g[i] >> 26;
            g[i] &= M;
        }
        g[4] = h[4].wrapping_add(c).wrapping_sub(1 << 26);
        let keep_g = (g[4] >> 31).wrapping_sub(1); // all ones when h >= p (no borrow)
        for i in 0..5 {
            h[i] = (h[i] & !keep_g) | (g[i] & keep_g);
        }
        let w0 = h[0] | h[1] << 26;
        let w1 = h[1] >> 6 | h[2] << 20;
        let w2 = h[2] >> 12 | h[3] << 14;
        let w3 = h[3] >> 18 | h[4] << 8;
        let mut f = w0 as u64 + self.pad[0] as u64;
        let t0 = f as u32;
        f = w1 as u64 + self.pad[1] as u64 + (f >> 32);
        let t1 = f as u32;
        f = w2 as u64 + self.pad[2] as u64 + (f >> 32);
        let t2 = f as u32;
        f = w3 as u64 + self.pad[3] as u64 + (f >> 32);
        let t3 = f as u32;
        let mut tag = [0u8; 16];
        tag[0..4].copy_from_slice(&t0.to_le_bytes());
        tag[4..8].copy_from_slice(&t1.to_le_bytes());
        tag[8..12].copy_from_slice(&t2.to_le_bytes());
        tag[12..16].copy_from_slice(&t3.to_le_bytes());
        tag
    }
}

fn mac(key: &Key, nonce: &[u32; 3], aad: &[u8], ciphertext: &[u8]) -> Tag {
    // the one-time Poly1305 key is the first 32 bytes of block 0
    let block0 = chacha20_block(key, 0, nonce);
    let mut otk = [0u8; 32];
    otk.copy_from_slice(&block0[..32]);
    let mut p = Poly1305::new(&otk);
    p.padded(aad);
    p.padded(ciphertext);
    let mut lens = [0u8; 16];
    lens[..8].copy_from_slice(&(aad.len() as u64).to_le_bytes());
    lens[8..].copy_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    p.block(&lens);
    p.finish()
}

/// Encrypt `data` in place and return the tag over `aad` and the ciphertext. A `(key, nonce)` pair must never be used twice.
pub fn seal(key: &Key, nonce: &[u8; 12], aad: &[u8], data: &mut [u8]) -> Tag {
    let n = nonce_words(nonce);
    stream(key, &n, 1, data);
    mac(key, &n, aad, data)
}

/// Check `tag` over `aad` and the ciphertext `data`, and decrypt `data` in place if it is right. On a wrong tag `data` is left as
/// it was (still ciphertext) and `false` is returned.
pub fn open(key: &Key, nonce: &[u8; 12], aad: &[u8], data: &mut [u8], tag: &Tag) -> bool {
    let n = nonce_words(nonce);
    let want = mac(key, &n, aad, data);
    let mut diff = 0u8;
    for (a, b) in want.iter().zip(tag.iter()) {
        diff |= a ^ b;
    }
    if diff != 0 {
        return false;
    }
    stream(key, &n, 1, data);
    true
}

#[cfg(test)]
mod tests;
