//! Zero-allocation LZ4 block compressor / decompressor (clean-room, from the
//! public LZ4 block format description). `no_std`, no heap, no `unsafe`.
#![no_std]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Destination buffer too small.
    OutputTooSmall,
    /// Malformed compressed stream, or input too large for this compressor.
    InvalidInput,
}

/// Worst-case compressed size for `n` input bytes.
pub const fn compress_bound(n: usize) -> usize {
    n + n / 255 + 16
}

const MIN_MATCH: usize = 4;
const MF_LIMIT: usize = 12; // last match must start >= 12 bytes before end
const LAST_LITERALS: usize = 5; // last 5 bytes are always literals
const HASH_BITS: u32 = 12;
const NONE: u16 = u16::MAX;

/// Decompress one LZ4 block. Returns the number of bytes written to `dst`.
pub fn decompress(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    let (mut ip, mut op) = (0usize, 0usize);
    while ip < src.len() {
        let token = src[ip];
        ip += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *src.get(ip).ok_or(Error::InvalidInput)?;
                ip += 1;
                lit = lit.checked_add(b as usize).ok_or(Error::InvalidInput)?;
                if b != 255 {
                    break;
                }
            }
        }
        let lit_end = ip.checked_add(lit).ok_or(Error::InvalidInput)?;
        if lit_end > src.len() {
            return Err(Error::InvalidInput);
        }
        if op + lit > dst.len() {
            return Err(Error::OutputTooSmall);
        }
        dst[op..op + lit].copy_from_slice(&src[ip..lit_end]);
        ip = lit_end;
        op += lit;
        if ip == src.len() {
            break; // final sequence carries literals only
        }
        if ip + 2 > src.len() {
            return Err(Error::InvalidInput);
        }
        let off = src[ip] as usize | (src[ip + 1] as usize) << 8;
        ip += 2;
        if off == 0 || off > op {
            return Err(Error::InvalidInput);
        }
        let mut ml = (token & 15) as usize;
        if ml == 15 {
            loop {
                let b = *src.get(ip).ok_or(Error::InvalidInput)?;
                ip += 1;
                ml = ml.checked_add(b as usize).ok_or(Error::InvalidInput)?;
                if b != 255 {
                    break;
                }
            }
        }
        ml += MIN_MATCH;
        if op + ml > dst.len() {
            return Err(Error::OutputTooSmall);
        }
        for i in 0..ml {
            dst[op + i] = dst[op + i - off]; // may overlap: byte-wise on purpose
        }
        op += ml;
    }
    Ok(op)
}

struct Out<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Out<'_> {
    fn byte(&mut self, b: u8) -> Result<(), Error> {
        *self.buf.get_mut(self.pos).ok_or(Error::OutputTooSmall)? = b;
        self.pos += 1;
        Ok(())
    }
    fn bytes(&mut self, s: &[u8]) -> Result<(), Error> {
        let end = self.pos + s.len();
        self.buf
            .get_mut(self.pos..end)
            .ok_or(Error::OutputTooSmall)?
            .copy_from_slice(s);
        self.pos = end;
        Ok(())
    }
    fn ext_len(&mut self, mut n: usize) -> Result<(), Error> {
        while n >= 255 {
            self.byte(255)?;
            n -= 255;
        }
        self.byte(n as u8)
    }
    /// Emit one sequence. `m` is (offset, match_len) or None for the last one.
    fn sequence(&mut self, lits: &[u8], m: Option<(usize, usize)>) -> Result<(), Error> {
        let ll = lits.len();
        let ml = m.map_or(0, |(_, l)| l - MIN_MATCH);
        let tok = ((ll.min(15) as u8) << 4) | (ml.min(15) as u8);
        self.byte(tok)?;
        if ll >= 15 {
            self.ext_len(ll - 15)?;
        }
        self.bytes(lits)?;
        if let Some((off, _)) = m {
            self.byte(off as u8)?;
            self.byte((off >> 8) as u8)?;
            if ml >= 15 {
                self.ext_len(ml - 15)?;
            }
        }
        Ok(())
    }
}

#[inline]
fn hash4(s: &[u8]) -> usize {
    let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    (v.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

/// Compress `src` (at most 65535 bytes) into `dst` as a single LZ4 block.
/// Uses an 8 KiB stack hash table.
pub fn compress(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    let n = src.len();
    if n > u16::MAX as usize {
        return Err(Error::InvalidInput);
    }
    let mut table = [NONE; 1 << HASH_BITS];
    let mut out = Out { buf: dst, pos: 0 };
    let (mut ip, mut anchor) = (0usize, 0usize);
    if n >= MF_LIMIT + 1 {
        let last_start = n - MF_LIMIT;
        let match_limit = n - LAST_LITERALS;
        while ip <= last_start {
            let h = hash4(&src[ip..]);
            let cand = table[h];
            table[h] = ip as u16;
            if cand != NONE {
                let c = cand as usize;
                if src[c..c + 4] == src[ip..ip + 4] {
                    let mut len = MIN_MATCH;
                    while ip + len < match_limit && src[c + len] == src[ip + len] {
                        len += 1;
                    }
                    out.sequence(&src[anchor..ip], Some((ip - c, len)))?;
                    ip += len;
                    anchor = ip;
                    continue;
                }
            }
            ip += 1;
        }
    }
    out.sequence(&src[anchor..], None)?;
    Ok(out.pos)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn roundtrip(data: &[u8]) -> usize {
        let mut c = vec![0u8; compress_bound(data.len())];
        let cn = compress(data, &mut c).unwrap();
        let mut d = vec![0u8; data.len()];
        let dn = decompress(&c[..cn], &mut d).unwrap();
        assert_eq!(&d[..dn], data);
        cn
    }

    #[test]
    fn empty_and_tiny() {
        roundtrip(b"");
        roundtrip(b"a");
        roundtrip(b"hello world");
        roundtrip(&[7u8; 12]);
        roundtrip(&[7u8; 13]);
    }

    #[test]
    fn repetitive_compresses() {
        let data = vec![0xABu8; 4096];
        assert!(roundtrip(&data) < 64);
        let text: Vec<u8> = b"metal os metal os microkernel "
            .iter()
            .cycle()
            .take(4096)
            .copied()
            .collect();
        assert!(roundtrip(&text) < 400);
    }

    #[test]
    fn pseudo_random_roundtrips() {
        let mut x = 0x1234_5678u32;
        for len in [1usize, 15, 16, 255, 256, 1000, 4096, 60000] {
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    (x >> 24) as u8
                })
                .collect();
            roundtrip(&data);
        }
    }

    #[test]
    fn known_vector() {
        // "abcabcabcabc" hand-encoded: literals "abc", match off=3 len=9... last 5 are literals
        // so use the spec's trivially valid literal-only block instead.
        let block = [0x50, b'h', b'e', b'l', b'l', b'o'];
        let mut d = [0u8; 5];
        assert_eq!(decompress(&block, &mut d), Ok(5));
        assert_eq!(&d, b"hello");
    }

    #[test]
    fn rejects_garbage() {
        let mut d = [0u8; 64];
        assert!(decompress(&[0x0F, 0x00, 0x00], &mut d).is_err()); // zero offset
        assert!(decompress(&[0xF0], &mut d).is_err()); // truncated length
        assert!(decompress(&[0x10, b'a', 0x05, 0x00], &mut d).is_err()); // offset > output
        let mut tiny = [0u8; 2];
        assert_eq!(
            decompress(&[0x50, b'h', b'e', b'l', b'l', b'o'], &mut tiny),
            Err(Error::OutputTooSmall)
        );
    }
}
