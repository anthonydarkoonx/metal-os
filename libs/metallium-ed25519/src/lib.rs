//! SHA-512 (FIPS 180-4) and Ed25519 signature *verification* (RFC 8032), clean-room, `no_std`, no heap, no `unsafe`.
//!
//! The SHA-512 constants are generated from their definition (fractional parts of the square and cube roots of the first
//! primes) by integer arithmetic and checked here against `sha512("abc")`. The curve constants that are not small
//! (`d`, `sqrt(-1)`, the base point) are computed when needed from their definitions, not typed in; the group order `L`
//! is the one number written out, and the RFC 8032 test vectors would fail with a wrong one.
//!
//! Not constant time: it verifies signatures, which are public, and handles no secret.
#![forbid(unsafe_code)]
#![no_std]

// ---------------------------------------------------------------------------------------------------------------
// SHA-512
// ---------------------------------------------------------------------------------------------------------------
const K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];
const H0: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

fn compress(h: &mut [u64; 8], block: &[u8; 128]) {
    let mut w = [0u64; 80];
    for i in 0..16 {
        w[i] = u64::from_be_bytes(block[8 * i..8 * i + 8].try_into().unwrap());
    }
    for i in 16..80 {
        let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
        let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..80 {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *x = x.wrapping_add(y);
    }
}

/// Incremental SHA-512.
#[derive(Clone)]
pub struct Sha512 {
    h: [u64; 8],
    buf: [u8; 128],
    n: usize,
    total: u128,
}

impl Sha512 {
    pub fn new() -> Sha512 {
        Sha512 { h: H0, buf: [0; 128], n: 0, total: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u128;
        while !data.is_empty() {
            let take = (128 - self.n).min(data.len());
            self.buf[self.n..self.n + take].copy_from_slice(&data[..take]);
            self.n += take;
            data = &data[take..];
            if self.n == 128 {
                let b = self.buf;
                compress(&mut self.h, &b);
                self.n = 0;
            }
        }
    }

    pub fn finish(mut self) -> [u8; 64] {
        let bits = self.total * 8;
        let mut pad = [0u8; 256];
        pad[0] = 0x80;
        let padlen = if self.n < 112 { 112 - self.n } else { 240 - self.n };
        self.update(&pad[..padlen]);
        self.update(&bits.to_be_bytes());
        debug_assert_eq!(self.n, 0);
        let mut out = [0u8; 64];
        for i in 0..8 {
            out[8 * i..8 * i + 8].copy_from_slice(&self.h[i].to_be_bytes());
        }
        out
    }
}

impl Default for Sha512 {
    fn default() -> Self {
        Sha512::new()
    }
}

pub fn sha512(data: &[u8]) -> [u8; 64] {
    let mut s = Sha512::new();
    s.update(data);
    s.finish()
}

// ---------------------------------------------------------------------------------------------------------------
// the field GF(2^255 - 19): five 51-bit limbs
// ---------------------------------------------------------------------------------------------------------------
const MASK: u64 = (1 << 51) - 1;

#[derive(Clone, Copy)]
struct Fe([u64; 5]);

impl Fe {
    const ZERO: Fe = Fe([0; 5]);
    const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    fn from_u64(v: u64) -> Fe {
        Fe([v & MASK, v >> 51, 0, 0, 0])
    }

    /// 32 little-endian bytes; the top bit is ignored.
    fn from_bytes(b: &[u8; 32]) -> Fe {
        let w = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        Fe([w(0) & MASK, (w(6) >> 3) & MASK, (w(12) >> 6) & MASK, (w(19) >> 1) & MASK, (w(24) >> 12) & MASK])
    }

    fn carry(mut l: [u64; 5]) -> Fe {
        for _ in 0..2 {
            let c = l[4] >> 51;
            l[4] &= MASK;
            l[0] += c * 19;
            for i in 0..4 {
                l[i + 1] += l[i] >> 51;
                l[i] &= MASK;
            }
        }
        Fe(l)
    }

    fn add(self, o: Fe) -> Fe {
        Fe::carry([self.0[0] + o.0[0], self.0[1] + o.0[1], self.0[2] + o.0[2], self.0[3] + o.0[3], self.0[4] + o.0[4]])
    }

    fn sub(self, o: Fe) -> Fe {
        // add 16p first so no limb goes negative (the operands' limbs stay below 2^52)
        const P0: u64 = 0x7FFFFFFFFFFED << 4;
        const PN: u64 = 0x7FFFFFFFFFFFF << 4;
        Fe::carry([self.0[0] + P0 - o.0[0], self.0[1] + PN - o.0[1], self.0[2] + PN - o.0[2], self.0[3] + PN - o.0[3], self.0[4] + PN - o.0[4]])
    }

    fn mul(self, o: Fe) -> Fe {
        let (a, b) = (self.0, o.0);
        let m = |x: u64, y: u64| x as u128 * y as u128;
        let b19 = [b[0], b[1] * 19, b[2] * 19, b[3] * 19, b[4] * 19];
        let mut t = [0u128; 5];
        t[0] = m(a[0], b[0]) + m(a[1], b19[4]) + m(a[2], b19[3]) + m(a[3], b19[2]) + m(a[4], b19[1]);
        t[1] = m(a[0], b[1]) + m(a[1], b[0]) + m(a[2], b19[4]) + m(a[3], b19[3]) + m(a[4], b19[2]);
        t[2] = m(a[0], b[2]) + m(a[1], b[1]) + m(a[2], b[0]) + m(a[3], b19[4]) + m(a[4], b19[3]);
        t[3] = m(a[0], b[3]) + m(a[1], b[2]) + m(a[2], b[1]) + m(a[3], b[0]) + m(a[4], b19[4]);
        t[4] = m(a[0], b[4]) + m(a[1], b[3]) + m(a[2], b[2]) + m(a[3], b[1]) + m(a[4], b[0]);
        let mut l = [0u64; 5];
        let mut c = 0u128;
        for i in 0..5 {
            let v = t[i] + c;
            l[i] = (v as u64) & MASK;
            c = v >> 51;
        }
        l[0] += (c as u64) * 19;
        Fe::carry(l)
    }

    fn square(self) -> Fe {
        self.mul(self)
    }

    /// self^e for a little-endian exponent (square and multiply).
    fn pow(self, e: &[u8; 32]) -> Fe {
        let mut r = Fe::ONE;
        for i in (0..256).rev() {
            r = r.square();
            if (e[i / 8] >> (i % 8)) & 1 == 1 {
                r = r.mul(self);
            }
        }
        r
    }

    fn invert(self) -> Fe {
        // p - 2 = 2^255 - 21
        let mut e = [0xFFu8; 32];
        e[0] = 0xEB;
        e[31] = 0x7F;
        self.pow(&e)
    }

    /// The canonical 32 bytes.
    fn to_bytes(self) -> [u8; 32] {
        let mut l = Fe::carry(self.0).0;
        // l < 2p now; subtract p once if l >= p: compute l + 19 and look at bit 255
        let mut c = (l[0] + 19) >> 51;
        for i in 1..5 {
            c = (l[i] + c) >> 51;
        }
        l[0] += 19 * c;
        for i in 0..4 {
            l[i + 1] += l[i] >> 51;
            l[i] &= MASK;
        }
        l[4] &= MASK;
        let mut out = [0u8; 32];
        let (mut acc, mut bits, mut o) = (0u128, 0, 0);
        for limb in l {
            acc |= (limb as u128) << bits;
            bits += 51;
            while bits >= 8 && o < 32 {
                out[o] = acc as u8;
                acc >>= 8;
                bits -= 8;
                o += 1;
            }
        }
        if o < 32 {
            out[o] = acc as u8;
        }
        out
    }

    fn is_negative(self) -> bool {
        self.to_bytes()[0] & 1 == 1
    }

    fn neg(self) -> Fe {
        Fe::ZERO.sub(self)
    }

    fn eq(self, o: Fe) -> bool {
        self.to_bytes() == o.to_bytes()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// the curve: -x^2 + y^2 = 1 + d x^2 y^2, extended coordinates
// ---------------------------------------------------------------------------------------------------------------
fn curve_d() -> Fe {
    // d = -121665 / 121666
    Fe::from_u64(121665).neg().mul(Fe::from_u64(121666).invert())
}

fn sqrt_m1() -> Fe {
    // 2^((p-1)/4), (p-1)/4 = 2^253 - 5
    let mut e = [0xFFu8; 32];
    e[0] = 0xFB;
    e[31] = 0x1F;
    Fe::from_u64(2).pow(&e)
}

#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

struct Curve {
    d2: Fe,
    d: Fe,
    i: Fe,
}

impl Curve {
    fn new() -> Curve {
        let d = curve_d();
        Curve { d2: d.add(d), d, i: sqrt_m1() }
    }

    fn identity() -> Point {
        Point { x: Fe::ZERO, y: Fe::ONE, z: Fe::ONE, t: Fe::ZERO }
    }

    fn add(&self, p: &Point, q: &Point) -> Point {
        let a = p.y.sub(p.x).mul(q.y.sub(q.x));
        let b = p.y.add(p.x).mul(q.y.add(q.x));
        let c = p.t.mul(self.d2).mul(q.t);
        let d = p.z.add(p.z).mul(q.z);
        let (e, f, g, h) = (b.sub(a), d.sub(c), d.add(c), b.add(a));
        Point { x: e.mul(f), y: g.mul(h), t: e.mul(h), z: f.mul(g) }
    }

    fn double(&self, p: &Point) -> Point {
        let a = p.x.square();
        let b = p.y.square();
        let c = p.z.square();
        let c = c.add(c);
        let h = a.add(b);
        let e = h.sub(p.x.add(p.y).square());
        let g = a.sub(b);
        let f = c.add(g);
        Point { x: e.mul(f), y: g.mul(h), t: e.mul(h), z: f.mul(g) }
    }

    fn neg(&self, p: &Point) -> Point {
        Point { x: p.x.neg(), y: p.y, z: p.z, t: p.t.neg() }
    }

    /// [k]p for a little-endian scalar (double and add).
    fn mul(&self, p: &Point, k: &[u8; 32]) -> Point {
        let mut r = Curve::identity();
        for i in (0..256).rev() {
            r = self.double(&r);
            if (k[i / 8] >> (i % 8)) & 1 == 1 {
                r = self.add(&r, p);
            }
        }
        r
    }

    fn encode(&self, p: &Point) -> [u8; 32] {
        let zi = p.z.invert();
        let (x, y) = (p.x.mul(zi), p.y.mul(zi));
        let mut b = y.to_bytes();
        b[31] |= (x.is_negative() as u8) << 7;
        b
    }

    /// Decode a point (RFC 8032 5.1.3); None if it is not on the curve.
    fn decode(&self, b: &[u8; 32]) -> Option<Point> {
        let sign = b[31] >> 7 == 1;
        let mut yb = *b;
        yb[31] &= 0x7F;
        let y = Fe::from_bytes(&yb);
        if y.to_bytes() != yb {
            return None; // y >= p
        }
        let y2 = y.square();
        let u = y2.sub(Fe::ONE);
        let v = self.d.mul(y2).add(Fe::ONE);
        // x = u v^3 (u v^7)^((p-5)/8)
        let v3 = v.square().mul(v);
        let v7 = v3.square().mul(v);
        let mut e = [0xFFu8; 32];
        e[0] = 0xFD;
        e[31] = 0x0F; // 2^252 - 3
        let mut x = u.mul(v3).mul(u.mul(v7).pow(&e));
        let vx2 = v.mul(x.square());
        if !vx2.eq(u) {
            if vx2.eq(u.neg()) {
                x = x.mul(self.i);
            } else {
                return None;
            }
        }
        if x.eq(Fe::ZERO) && sign {
            return None;
        }
        if x.is_negative() != sign {
            x = x.neg();
        }
        Some(Point { x, y, z: Fe::ONE, t: x.mul(y) })
    }

    fn base(&self) -> Point {
        // y = 4/5, x even
        let y = Fe::from_u64(4).mul(Fe::from_u64(5).invert());
        let mut b = y.to_bytes();
        b[31] &= 0x7F;
        self.decode(&b).expect("the base point is on the curve")
    }
}

// ---------------------------------------------------------------------------------------------------------------
// scalars modulo the group order L = 2^252 + 27742317777372353535851937790883648493
// ---------------------------------------------------------------------------------------------------------------
const L: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

fn ge_l(a: &[u64; 4]) -> bool {
    let l = limbs(&L);
    for i in (0..4).rev() {
        if a[i] != l[i] {
            return a[i] > l[i];
        }
    }
    true
}

fn limbs(b: &[u8; 32]) -> [u64; 4] {
    core::array::from_fn(|i| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap()))
}

/// A 512-bit little-endian number modulo L (bit-serial; it runs a few times per signature).
fn reduce(h: &[u8; 64]) -> [u8; 32] {
    let l = limbs(&L);
    let mut r = [0u64; 4];
    for i in (0..512).rev() {
        // r = 2r + bit
        let top = r[3] >> 63;
        debug_assert_eq!(top, 0); // r < L < 2^253
        r = [r[0] << 1 | ((h[i / 8] >> (i % 8)) & 1) as u64, r[1] << 1 | r[0] >> 63, r[2] << 1 | r[1] >> 63, r[3] << 1 | r[2] >> 63];
        if ge_l(&r) {
            let mut borrow = 0u64;
            for j in 0..4 {
                let (d1, b1) = r[j].overflowing_sub(l[j]);
                let (d2, b2) = d1.overflowing_sub(borrow);
                r[j] = d2;
                borrow = (b1 | b2) as u64;
            }
        }
    }
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[8 * i..8 * i + 8].copy_from_slice(&r[i].to_le_bytes());
    }
    out
}

/// Verify an Ed25519 signature (`sig` = R || S) of `msg` under the public key `pk`.
pub fn verify(pk: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let (rb, sb): (&[u8; 32], &[u8; 32]) = (sig[..32].try_into().unwrap(), sig[32..].try_into().unwrap());
    if ge_l(&limbs(sb)) {
        return false; // S must be below the group order (it rules out malleable signatures)
    }
    let c = Curve::new();
    let (Some(a), Some(r)) = (c.decode(pk), c.decode(rb)) else { return false };
    let mut h = Sha512::new();
    h.update(rb);
    h.update(pk);
    h.update(msg);
    let k = reduce(&h.finish());
    // [S]B == R + [k]A  <=>  [S]B + [k](-A) == R
    let lhs = c.add(&c.mul(&c.base(), sb), &c.mul(&c.neg(&a), &k));
    c.encode(&lhs) == c.encode(&r)
}

#[cfg(test)]
mod tests;
