//! ZRAM-style compressed page store. All storage is caller-provided (static
//! arenas in the kernel), so there is no heap and no runtime allocation.
#![forbid(unsafe_code)]
#![no_std]

use metal_lz4 as lz4;
use metal_zstd::lite;

pub const PAGE: usize = 4096;

/// Workspace for the Zstandard codec (see `Codec`).
pub use metal_zstd::encode::PageWorkspace;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No free slot, or pool full even after compaction.
    Full,
    BadHandle,
}

#[derive(Clone, Copy)]
pub struct Slot {
    off: u32,
    /// 0 = free. `PAGE` = stored raw (incompressible).
    len: u16,
}

impl Slot {
    pub const EMPTY: Slot = Slot { off: 0, len: 0 };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Handle(pub u32);

/// How pages are compressed. Zstandard needs a workspace (about 25 KiB) that the caller provides, since
/// there is no heap; its frames are ordinary Zstandard frames, read back by the allocation-free decoder.
pub enum Codec<'a> {
    Lz4,
    Zstd(&'a mut PageWorkspace),
}

pub struct Zram<'a> {
    pool: &'a mut [u8],
    slots: &'a mut [Slot],
    top: usize,
    stored_bytes: usize,
    codec: Codec<'a>,
}

impl<'a> Zram<'a> {
    /// A store that compresses with LZ4.
    pub fn new(pool: &'a mut [u8], slots: &'a mut [Slot]) -> Self {
        Self::with_codec(pool, slots, Codec::Lz4)
    }

    /// A store that compresses with Zstandard (better ratio, a little slower).
    pub fn with_zstd(pool: &'a mut [u8], slots: &'a mut [Slot], ws: &'a mut PageWorkspace) -> Self {
        Self::with_codec(pool, slots, Codec::Zstd(ws))
    }

    pub fn with_codec(pool: &'a mut [u8], slots: &'a mut [Slot], codec: Codec<'a>) -> Self {
        slots.fill(Slot::EMPTY);
        Zram { pool, slots, top: 0, stored_bytes: 0, codec }
    }

    /// Bytes of pool currently holding live compressed data.
    pub fn stored_bytes(&self) -> usize {
        self.stored_bytes
    }

    /// Compress and store a page; returns a handle to load it back with.
    pub fn store(&mut self, page: &[u8; PAGE]) -> Result<Handle, Error> {
        let mut tmp = [0u8; PAGE + 64];
        let compressed = match &mut self.codec {
            Codec::Lz4 => lz4::compress(page, &mut tmp).map_err(|_| ()),
            Codec::Zstd(ws) => ws.compress(page, &mut tmp, false).map_err(|_| ()),
        };
        let clen = match compressed {
            Ok(n) if n < PAGE => n,
            _ => PAGE, // incompressible (or encoder overflow): keep raw
        };
        let data: &[u8] = if clen == PAGE { page } else { &tmp[..clen] };
        let idx = self.slots.iter().position(|s| s.len == 0).ok_or(Error::Full)?;
        if self.pool.len() - self.top < clen {
            self.compact();
            if self.pool.len() - self.top < clen {
                return Err(Error::Full);
            }
        }
        self.pool[self.top..self.top + clen].copy_from_slice(data);
        self.slots[idx] = Slot { off: self.top as u32, len: clen as u16 };
        self.top += clen;
        self.stored_bytes += clen;
        Ok(Handle(idx as u32))
    }

    /// Decompress into `out` and keep the slot.
    pub fn peek(&mut self, h: Handle, out: &mut [u8; PAGE]) -> Result<(), Error> {
        let s = *self.slots.get(h.0 as usize).ok_or(Error::BadHandle)?;
        if s.len == 0 {
            return Err(Error::BadHandle);
        }
        let src = &self.pool[s.off as usize..s.off as usize + s.len as usize];
        if s.len as usize == PAGE {
            out.copy_from_slice(src);
            return Ok(());
        }
        let n = match &self.codec {
            Codec::Lz4 => lz4::decompress(src, out).map_err(|_| Error::BadHandle)?,
            Codec::Zstd(_) => lite::decompress(src, out).map_err(|_| Error::BadHandle)?,
        };
        if n == PAGE {
            Ok(())
        } else {
            Err(Error::BadHandle)
        }
    }

    /// Decompress into `out` and free the slot (swap-in semantics).
    pub fn load(&mut self, h: Handle, out: &mut [u8; PAGE]) -> Result<(), Error> {
        let s = *self.slots.get(h.0 as usize).ok_or(Error::BadHandle)?;
        if s.len == 0 {
            return Err(Error::BadHandle);
        }
        let src = &self.pool[s.off as usize..s.off as usize + s.len as usize];
        if s.len as usize == PAGE {
            out.copy_from_slice(src);
        } else {
            let n = match &self.codec {
                Codec::Lz4 => lz4::decompress(src, out).map_err(|_| Error::BadHandle)?,
                Codec::Zstd(_) => lite::decompress(src, out).map_err(|_| Error::BadHandle)?,
            };
            if n != PAGE {
                return Err(Error::BadHandle);
            }
        }
        self.slots[h.0 as usize] = Slot::EMPTY;
        self.stored_bytes -= s.len as usize;
        Ok(())
    }

    /// Drop a stored page without decompressing it (the page it held is no longer wanted).
    pub fn discard(&mut self, h: Handle) -> Result<(), Error> {
        let s = *self.slots.get(h.0 as usize).ok_or(Error::BadHandle)?;
        if s.len == 0 {
            return Err(Error::BadHandle);
        }
        self.slots[h.0 as usize] = Slot::EMPTY;
        self.stored_bytes -= s.len as usize;
        Ok(())
    }

    /// Slide live entries toward the start of the pool (in offset order).
    fn compact(&mut self) {
        let mut cursor = 0usize;
        loop {
            // next live slot at or after `cursor`
            let mut best: Option<usize> = None;
            for (i, s) in self.slots.iter().enumerate() {
                if s.len != 0 && s.off as usize >= cursor {
                    if best.map_or(true, |b| s.off < self.slots[b].off) {
                        best = Some(i);
                    }
                }
            }
            let Some(i) = best else { break };
            let s = self.slots[i];
            self.pool.copy_within(s.off as usize..s.off as usize + s.len as usize, cursor);
            self.slots[i].off = cursor as u32;
            cursor += s.len as usize;
        }
        self.top = cursor;
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;
    use std::string::{String, ToString};
    use std::vec;
    use std::vec::Vec;
    use std::format;

    fn page(mut f: impl FnMut(usize) -> u8) -> [u8; PAGE] {
        let mut p = [0u8; PAGE];
        for (i, b) in p.iter_mut().enumerate() {
            *b = f(i);
        }
        p
    }

    fn both(f: impl Fn(&mut Zram<'_>)) {
        let (mut pool, mut slots) = (vec![0u8; 64 * 1024], vec![Slot::EMPTY; 32]);
        f(&mut Zram::new(&mut pool, &mut slots));
        let (mut pool, mut slots, mut ws) = (vec![0u8; 64 * 1024], vec![Slot::EMPTY; 32], Box::new(PageWorkspace::new()));
        f(&mut Zram::with_zstd(&mut pool, &mut slots, &mut ws));
    }

    #[test]
    fn store_load_and_savings() {
        both(|z| {
        let zero = page(|_| 0);
        let text = page(|i| b"cold page data "[i % 15]);
        let (a, b) = (z.store(&zero).unwrap(), z.store(&text).unwrap());
        assert!(z.stored_bytes() < 600);
        let mut out = [0u8; PAGE];
        z.load(b, &mut out).unwrap();
        assert_eq!(out, text);
        z.load(a, &mut out).unwrap();
        assert_eq!(out, zero);
        assert_eq!(z.stored_bytes(), 0);
        assert_eq!(z.load(a, &mut out), Err(Error::BadHandle));
        });
    }

    #[test]
    fn discard_frees_without_decompressing() {
        both(|z| {
            let text = page(|i| b"discard me "[i % 11]);
            let h = z.store(&text).unwrap();
            assert!(z.stored_bytes() > 0);
            assert_eq!(z.discard(h), Ok(()));
            assert_eq!(z.stored_bytes(), 0);
            assert_eq!(z.discard(h), Err(Error::BadHandle));
            let mut out = [0u8; PAGE];
            assert_eq!(z.load(h, &mut out), Err(Error::BadHandle));
        });
    }

    #[test]
    fn incompressible_is_kept_raw_and_compaction_reclaims() {
        let mut pool = vec![0u8; PAGE * 2 + 100];
        let mut slots = vec![Slot::EMPTY; 8];
        let mut z = Zram::new(&mut pool, &mut slots);
        let mut x = 99u32;
        let noise = page(|_| {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            (x >> 24) as u8
        });
        let h1 = z.store(&noise).unwrap();
        let _h2 = z.store(&noise).unwrap();
        assert_eq!(z.store(&noise), Err(Error::Full));
        let mut out = [0u8; PAGE];
        z.load(h1, &mut out).unwrap(); // frees a hole at the front
        let h3 = z.store(&noise).unwrap(); // only fits after compaction
        z.load(h3, &mut out).unwrap();
        assert_eq!(out, noise);
    }

    /// Random stores, loads and discards against a small pool, so compaction runs constantly: whatever is
    /// stored must always come back byte for byte.
    #[test]
    fn random_churn_with_constant_compaction() {
        for codec in 0..2 {
            let (mut pool, mut slots, mut ws) = (vec![0u8; 40 * 1024], vec![Slot::EMPTY; 64], Box::new(PageWorkspace::new()));
            let mut z = if codec == 0 { Zram::new(&mut pool, &mut slots) } else { Zram::with_zstd(&mut pool, &mut slots, &mut ws) };
            let mut live: Vec<(Handle, [u8; PAGE])> = Vec::new();
            let mut rng = 0x1234_5678u64 + codec as u64;
            let mut next = move || {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (rng >> 33) as usize
            };
            let (mut stored, mut compactions_possible) = (0usize, 0usize);
            for step in 0..6000 {
                match next() % 5 {
                    0 | 1 | 2 => {
                        // pages of mixed compressibility
                        let kind = next() % 4;
                        let seed = next();
                        let p = page(|i| match kind {
                            0 => b"cold anonymous page "[(i + seed) % 20],
                            1 => (i / 64 + seed) as u8,
                            2 => (i.wrapping_mul(2654435761).wrapping_add(seed) >> 7) as u8,
                            _ => if i % 97 == 0 { seed as u8 } else { 0 },
                        });
                        match z.store(&p) {
                            Ok(h) => {
                                live.push((h, p));
                                stored += 1;
                            }
                            Err(Error::Full) => compactions_possible += 1,
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                    3 if !live.is_empty() => {
                        let (h, p) = live.swap_remove(next() % live.len());
                        let mut out = [0u8; PAGE];
                        z.load(h, &mut out).unwrap();
                        assert!(out == p, "step {step}: a page came back different (codec {codec})");
                    }
                    _ if !live.is_empty() => {
                        let i = next() % live.len();
                        if next() % 2 == 0 {
                            let (h, p) = &live[i];
                            let mut out = [0u8; PAGE];
                            z.peek(*h, &mut out).unwrap();
                            assert!(out == *p, "step {step}: peek differs (codec {codec})");
                        } else {
                            let (h, _) = live.swap_remove(i);
                            z.discard(h).unwrap();
                        }
                    }
                    _ => {}
                }
            }
            // everything still stored is intact
            for (h, p) in &live {
                let mut out = [0u8; PAGE];
                z.load(*h, &mut out).unwrap();
                assert!(out == *p, "final check differs (codec {codec})");
            }
            assert_eq!(z.stored_bytes(), 0);
            assert!(stored > 1000 && compactions_possible > 0, "the pool was meant to run full ({stored} stores, {compactions_possible} refusals)");
        }
    }

    /// Not an assertion of superiority: a measurement, printed with `cargo test -- --nocapture`.
    /// The only hard requirement is that both codecs round-trip every page.
    #[test]
    fn compare_codecs_on_typical_pages() {
        fn mk(name: &str, f: impl FnMut(usize) -> u8) -> (String, [u8; PAGE]) {
            (name.into(), page(f))
        }
        let mut seed = 12345u32;
        let mut noise = move |_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 24) as u8
        };
        let lorem: Vec<u8> = (0..).flat_map(|i: usize| format!("Metal OS system image, line {i:05}: the quick brown fox\n").into_bytes()).take(PAGE).collect();
        let source: Vec<u8> = (0..).flat_map(|i: usize| format!("    let value_{} = compute(state, {});\n", i % 13, i * 7).into_bytes()).take(PAGE).collect();
        let pages = [
            mk("zeros", |_| 0),
            mk("repeated phrase", |i| b"cold anonymous page "[i % 20]),
            ("numbered text".to_string(), page(|i| lorem[i])),
            ("source code".to_string(), page(|i| source[i])),
            mk("array of small ints", |i| if i % 4 == 0 { (i / 4 % 200) as u8 } else { 0 }),
            mk("noise", &mut noise),
        ];
        for (name, p) in &pages {
            let mut sizes = [0usize; 2];
            let (mut pool, mut slots) = (vec![0u8; 16 * 1024], vec![Slot::EMPTY; 4]);
            let mut ws = Box::new(PageWorkspace::new());
            for (k, mut z) in [Zram::new(&mut pool, &mut slots), Zram::with_zstd(&mut vec![0u8; 16 * 1024], &mut vec![Slot::EMPTY; 4], &mut ws)].into_iter().enumerate() {
                let h = z.store(p).unwrap();
                sizes[k] = z.stored_bytes();
                let mut out = [0u8; PAGE];
                z.load(h, &mut out).unwrap();
                assert_eq!(&out, p, "{name}");
            }
            std::println!("{name:>20}: lz4 {:>5} B   zstd {:>5} B", sizes[0], sizes[1]);
        }
    }
}
