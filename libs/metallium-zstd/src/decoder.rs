//! The full Zstandard (RFC 8878) frame decompressor, written from the specification
//! (see PROVENANCE.md; the `zstd` command-line tool is used only as a test oracle).
//!
//! `no_std` + `alloc`. All input is untrusted: every index is bounds-checked and
//! malformed data yields an `Error`, never a panic. Supported: raw / RLE /
//! compressed blocks, Huffman-coded literals (1 or 4 streams, treeless reuse),
//! FSE-coded sequences (predefined, RLE, inline and repeated tables), repeat
//! offsets, skippable frames, and the optional content checksum (XXH64).
//! Not supported: dictionaries. The caller provides an output buffer large
//! enough for the whole frame; it doubles as the match window.
use crate::tables::*;
use crate::xxh::xxh64;
use crate::Error;
use alloc::vec;
use alloc::vec::Vec;

type Res<T> = Result<T, Error>;

const MAGIC: u32 = 0xFD2F_B528;
const SKIPPABLE_MASK: u32 = 0xFFFF_FFF0;
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;
const BLOCK_MAX: usize = 128 * 1024;
const MAX_LL: usize = 35;
const MAX_ML: usize = 52;
const MAX_OF: usize = 31;
const HUF_MAX_BITS: u32 = 11;

// ---------------------------------------------------------------------------
// bit readers
// ---------------------------------------------------------------------------

/// Forward, LSB-first reader (FSE table descriptions).
struct Fwd<'a> {
    data: &'a [u8],
    pos: usize, // bit position
}

impl Fwd<'_> {
    fn peek(&self, n: u32) -> u32 {
        let mut v = 0u64;
        let first = self.pos / 8;
        for k in 0..5 {
            v |= (*self.data.get(first + k).unwrap_or(&0) as u64) << (8 * k);
        }
        ((v >> (self.pos % 8)) & ((1u64 << n) - 1)) as u32
    }
    fn read(&mut self, n: u32) -> Res<u32> {
        if self.pos + n as usize > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        let v = self.peek(n);
        self.pos += n as usize;
        Ok(v)
    }
    fn skip(&mut self, n: u32) -> Res<()> {
        if self.pos + n as usize > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        self.pos += n as usize;
        Ok(())
    }
}

/// Backward reader: the stream is consumed from its last bit towards its first.
/// Reading past the start yields zeros and leaves `pos` negative (overflow).
struct Rev<'a> {
    data: &'a [u8],
    pos: i64, // number of unread bits
}

impl<'a> Rev<'a> {
    fn new(data: &'a [u8]) -> Res<Self> {
        let last = *data.last().ok_or(Error::Truncated)?;
        if last == 0 {
            return Err(Error::Corrupt); // the end marker bit must exist
        }
        let hb = 7 - last.leading_zeros() as i64;
        Ok(Rev { data, pos: (data.len() as i64 - 1) * 8 + hb })
    }

    fn bits_at(&self, start: i64, n: u32) -> u64 {
        let first = start.div_euclid(8);
        let sh = start.rem_euclid(8) as u32;
        let mut v = 0u64;
        for k in 0..6i64 {
            let i = first + k;
            let b = if i >= 0 { *self.data.get(i as usize).unwrap_or(&0) } else { 0 };
            v |= (b as u64) << (8 * k);
        }
        (v >> sh) & ((1u64 << n) - 1)
    }

    fn peek(&self, n: u32) -> u64 {
        if n == 0 { 0 } else { self.bits_at(self.pos - n as i64, n) }
    }

    fn read(&mut self, n: u32) -> u64 {
        let v = self.peek(n);
        self.pos -= n as i64;
        v
    }

    fn overflowed(&self) -> bool {
        self.pos < 0
    }
}

// ---------------------------------------------------------------------------
// FSE
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Cell {
    sym: u8,
    nbits: u8,
    base: u16,
}

#[derive(Clone)]
struct Fse {
    al: u32,
    cells: Vec<Cell>,
}

fn log2_floor(v: u32) -> u32 {
    31 - v.leading_zeros()
}

/// Read a normalized-count table description. Returns (counts, accuracy log, bytes consumed).
fn read_ncount(data: &[u8], max_sym: usize, max_al: u32) -> Res<(Vec<i16>, u32, usize)> {
    let mut br = Fwd { data, pos: 0 };
    let al = br.read(4)? + 5;
    if al > max_al {
        return Err(Error::Corrupt);
    }
    let mut remaining: i32 = (1 << al) + 1;
    let mut threshold: i32 = 1 << al;
    let mut nbbits = al + 1;
    let mut norm: Vec<i16> = Vec::new();
    let mut previous0 = false;
    while remaining > 1 && norm.len() <= max_sym {
        if previous0 {
            // a run of zero-probability symbols, coded in 2-bit groups (3 = "more")
            let mut zeros = 0usize;
            loop {
                let r = br.read(2)? as usize;
                zeros += r;
                if r != 3 {
                    break;
                }
            }
            if norm.len() + zeros > max_sym + 1 {
                return Err(Error::Corrupt);
            }
            norm.resize(norm.len() + zeros, 0);
            if norm.len() > max_sym {
                break;
            }
        }
        let max = 2 * threshold - 1 - remaining;
        let low = br.peek(nbbits - 1) as i32;
        let mut count;
        if low < max {
            count = low;
            br.skip(nbbits - 1)?;
        } else {
            let full = br.peek(nbbits) as i32;
            count = if full >= threshold { full - max } else { full };
            br.skip(nbbits)?;
        }
        count -= 1; // stored with +1 so that -1 ("less than one") is representable
        remaining -= count.abs();
        if remaining < 0 {
            return Err(Error::Corrupt);
        }
        norm.push(count as i16);
        previous0 = count == 0;
        while remaining < threshold {
            nbbits -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return Err(Error::Corrupt);
    }
    Ok((norm, al, br.pos.div_ceil(8)))
}

fn build_fse(norm: &[i16], al: u32) -> Res<Fse> {
    let size = 1usize << al;
    let mut cells = vec![Cell::default(); size];
    let mut next = vec![0u32; norm.len()];
    let mut high = size as i64 - 1;
    for (s, &c) in norm.iter().enumerate() {
        if c == -1 {
            if high < 0 {
                return Err(Error::Corrupt);
            }
            cells[high as usize].sym = s as u8;
            high -= 1;
            next[s] = 1;
        } else if c >= 0 {
            next[s] = c as u32;
        } else {
            return Err(Error::Corrupt);
        }
    }
    let step = (size >> 1) + (size >> 3) + 3;
    let mask = size - 1;
    let mut pos = 0usize;
    for (s, &c) in norm.iter().enumerate() {
        for _ in 0..c.max(0) {
            cells[pos].sym = s as u8;
            pos = (pos + step) & mask;
            while pos as i64 > high {
                pos = (pos + step) & mask;
            }
        }
    }
    if pos != 0 {
        return Err(Error::Corrupt);
    }
    for u in 0..size {
        let s = cells[u].sym as usize;
        let ns = next[s];
        next[s] += 1;
        if ns == 0 {
            return Err(Error::Corrupt);
        }
        let nb = al - log2_floor(ns);
        cells[u].nbits = nb as u8;
        cells[u].base = ((ns << nb) as usize).checked_sub(size).ok_or(Error::Corrupt)? as u16;
    }
    Ok(Fse { al, cells })
}

fn rle_fse(sym: u8) -> Fse {
    Fse { al: 0, cells: vec![Cell { sym, nbits: 0, base: 0 }] }
}

// ---------------------------------------------------------------------------
// Huffman
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Huf {
    maxbits: u32,
    /// Indexed by the next `maxbits` bits of the stream: (symbol, code length).
    table: Vec<(u8, u8)>,
}

fn read_huffman(data: &[u8]) -> Res<(Huf, usize)> {
    let hb = *data.first().ok_or(Error::Truncated)? as usize;
    let mut weights: Vec<u8> = Vec::new();
    let used;
    if hb >= 128 {
        let n = hb - 127;
        let bytes = n.div_ceil(2);
        let body = data.get(1..1 + bytes).ok_or(Error::Truncated)?;
        for i in 0..n {
            let b = body[i / 2];
            weights.push(if i % 2 == 0 { b >> 4 } else { b & 15 });
        }
        used = 1 + bytes;
    } else {
        let body = data.get(1..1 + hb).ok_or(Error::Truncated)?;
        let (norm, al, consumed) = read_ncount(body, 255, 6)?;
        let fse = build_fse(&norm, al)?;
        let mut rev = Rev::new(body.get(consumed..).ok_or(Error::Truncated)?)?;
        let mut s1 = rev.read(al) as usize;
        let mut s2 = rev.read(al) as usize;
        // two interleaved states; the stream ending mid-way decides which symbol is last
        loop {
            if weights.len() > 255 {
                return Err(Error::Corrupt);
            }
            let c = fse.cells[s1];
            weights.push(c.sym);
            s1 = c.base as usize + rev.read(c.nbits as u32) as usize;
            if rev.overflowed() {
                weights.push(fse.cells[s2].sym);
                break;
            }
            let c = fse.cells[s2];
            weights.push(c.sym);
            s2 = c.base as usize + rev.read(c.nbits as u32) as usize;
            if rev.overflowed() {
                weights.push(fse.cells[s1].sym);
                break;
            }
        }
        used = 1 + hb;
    }
    if weights.is_empty() || weights.len() > 255 || weights.iter().any(|&w| w > HUF_MAX_BITS as u8 + 1) {
        return Err(Error::Corrupt);
    }
    // the last weight is implied: it must bring the total up to a power of two
    let total: u32 = weights.iter().filter(|&&w| w > 0).map(|&w| 1u32 << (w - 1)).sum();
    if total == 0 {
        return Err(Error::Corrupt);
    }
    let maxbits = log2_floor(total) + 1;
    if maxbits > HUF_MAX_BITS {
        return Err(Error::Corrupt);
    }
    let rest = (1u32 << maxbits) - total;
    if rest == 0 || !rest.is_power_of_two() {
        return Err(Error::Corrupt);
    }
    weights.push((log2_floor(rest) + 1) as u8);
    let mut rank_start = [0usize; 13];
    let mut count = [0usize; 13];
    for &w in &weights {
        count[w as usize] += 1;
    }
    let mut start = 0usize;
    for w in 1..=maxbits as usize {
        rank_start[w] = start;
        start += count[w] << (w - 1);
    }
    let mut table = vec![(0u8, 0u8); 1 << maxbits];
    for (sym, &w) in weights.iter().enumerate() {
        if w == 0 {
            continue;
        }
        let len = 1usize << (w - 1);
        let s = rank_start[w as usize];
        for e in table.get_mut(s..s + len).ok_or(Error::Corrupt)? {
            *e = (sym as u8, (maxbits + 1 - w as u32) as u8);
        }
        rank_start[w as usize] += len;
    }
    Ok((Huf { maxbits, table }, used))
}

fn huf_stream(h: &Huf, data: &[u8], out: &mut [u8]) -> Res<()> {
    let mut rev = Rev::new(data)?;
    for o in out.iter_mut() {
        let (sym, nb) = h.table[rev.peek(h.maxbits) as usize];
        if nb == 0 {
            return Err(Error::Corrupt);
        }
        rev.pos -= nb as i64;
        *o = sym;
    }
    if rev.pos != 0 {
        return Err(Error::Corrupt); // not all bits used, or read past the start
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the decoder
// ---------------------------------------------------------------------------

struct State {
    huf: Option<Huf>,
    ll: Option<Fse>,
    of: Option<Fse>,
    ml: Option<Fse>,
    rep: [u32; 3],
}

struct Out<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Out<'_> {
    fn push(&mut self, s: &[u8]) -> Res<()> {
        let end = self.len.checked_add(s.len()).ok_or(Error::OutputTooSmall)?;
        self.buf.get_mut(self.len..end).ok_or(Error::OutputTooSmall)?.copy_from_slice(s);
        self.len = end;
        Ok(())
    }
    fn push_rle(&mut self, b: u8, n: usize) -> Res<()> {
        let end = self.len.checked_add(n).ok_or(Error::OutputTooSmall)?;
        self.buf.get_mut(self.len..end).ok_or(Error::OutputTooSmall)?.fill(b);
        self.len = end;
        Ok(())
    }
    fn copy_match(&mut self, offset: usize, n: usize) -> Res<()> {
        if offset == 0 || offset > self.len {
            return Err(Error::Corrupt);
        }
        let end = self.len.checked_add(n).ok_or(Error::OutputTooSmall)?;
        if end > self.buf.len() {
            return Err(Error::OutputTooSmall);
        }
        for i in self.len..end {
            self.buf[i] = self.buf[i - offset]; // may overlap: byte-wise on purpose
        }
        self.len = end;
        Ok(())
    }
}

fn parse_literals(st: &mut State, data: &[u8]) -> Res<(Vec<u8>, usize)> {
    let b0 = *data.first().ok_or(Error::Truncated)?;
    let ty = b0 & 3;
    let fmt = (b0 >> 2) & 3;
    match ty {
        0 | 1 => {
            let (regen, hdr) = match fmt {
                0 | 2 => ((b0 >> 3) as usize, 1),
                1 => {
                    let b = data.get(..2).ok_or(Error::Truncated)?;
                    (((b[0] >> 4) as usize) | (b[1] as usize) << 4, 2)
                }
                _ => {
                    let b = data.get(..3).ok_or(Error::Truncated)?;
                    (((b[0] >> 4) as usize) | (b[1] as usize) << 4 | (b[2] as usize) << 12, 3)
                }
            };
            if regen > BLOCK_MAX {
                return Err(Error::Corrupt);
            }
            if ty == 0 {
                let body = data.get(hdr..hdr + regen).ok_or(Error::Truncated)?;
                Ok((body.to_vec(), hdr + regen))
            } else {
                let b = *data.get(hdr).ok_or(Error::Truncated)?;
                Ok((vec![b; regen], hdr + 1))
            }
        }
        _ => {
            let (streams4, hbytes, bits) = match fmt {
                0 => (false, 3, 10),
                1 => (true, 3, 10),
                2 => (true, 4, 14),
                _ => (true, 5, 18),
            };
            let h = data.get(..hbytes).ok_or(Error::Truncated)?;
            let mut v = 0u64;
            for (i, &b) in h.iter().enumerate() {
                v |= (b as u64) << (8 * i);
            }
            let v = v >> 4;
            let mask = (1u64 << bits) - 1;
            let (regen, comp) = ((v & mask) as usize, ((v >> bits) & mask) as usize);
            if regen > BLOCK_MAX || comp == 0 {
                return Err(Error::Corrupt);
            }
            let payload = data.get(hbytes..hbytes + comp).ok_or(Error::Truncated)?;
            let (huf, used) = if ty == 2 {
                let (t, used) = read_huffman(payload)?;
                st.huf = Some(t.clone());
                (t, used)
            } else {
                (st.huf.clone().ok_or(Error::Corrupt)?, 0)
            };
            let body = payload.get(used..).ok_or(Error::Truncated)?;
            let mut out = vec![0u8; regen];
            if !streams4 {
                huf_stream(&huf, body, &mut out)?;
            } else {
                let jt = body.get(..6).ok_or(Error::Truncated)?;
                let sz = [
                    u16::from_le_bytes([jt[0], jt[1]]) as usize,
                    u16::from_le_bytes([jt[2], jt[3]]) as usize,
                    u16::from_le_bytes([jt[4], jt[5]]) as usize,
                ];
                let rest = &body[6..];
                let s4 = rest.len().checked_sub(sz[0] + sz[1] + sz[2]).ok_or(Error::Corrupt)?;
                let seg = regen.div_ceil(4);
                if seg * 3 > regen {
                    return Err(Error::Corrupt);
                }
                let bounds = [0, sz[0], sz[0] + sz[1], sz[0] + sz[1] + sz[2], sz[0] + sz[1] + sz[2] + s4];
                for i in 0..4 {
                    let (lo, hi) = (i * seg, ((i + 1) * seg).min(regen));
                    huf_stream(&huf, &rest[bounds[i]..bounds[i + 1]], &mut out[lo..hi])?;
                }
            }
            Ok((out, hbytes + comp))
        }
    }
}

fn read_table(mode: u8, data: &[u8], max_sym: usize, max_al: u32, default: &[i16], default_al: u32, prev: &Option<Fse>) -> Res<(Fse, usize)> {
    match mode {
        0 => Ok((build_fse(default, default_al)?, 0)),
        1 => {
            let s = *data.first().ok_or(Error::Truncated)?;
            if s as usize > max_sym {
                return Err(Error::Corrupt);
            }
            Ok((rle_fse(s), 1))
        }
        2 => {
            let (norm, al, used) = read_ncount(data, max_sym, max_al)?;
            if norm.len() > max_sym + 1 {
                return Err(Error::Corrupt);
            }
            Ok((build_fse(&norm, al)?, used))
        }
        _ => Ok((prev.clone().ok_or(Error::Corrupt)?, 0)),
    }
}

fn decode_block(st: &mut State, data: &[u8], out: &mut Out) -> Res<()> {
    let (lits, used) = parse_literals(st, data)?;
    let mut p = used;
    let b0 = *data.get(p).ok_or(Error::Truncated)? as usize;
    p += 1;
    let nseq = if b0 == 0 {
        0
    } else if b0 < 128 {
        b0
    } else if b0 < 255 {
        let b1 = *data.get(p).ok_or(Error::Truncated)? as usize;
        p += 1;
        ((b0 - 128) << 8) + b1
    } else {
        let b = data.get(p..p + 2).ok_or(Error::Truncated)?;
        p += 2;
        b[0] as usize + ((b[1] as usize) << 8) + 0x7F00
    };
    if nseq == 0 {
        if p != data.len() {
            return Err(Error::Corrupt);
        }
        return out.push(&lits);
    }
    let modes = *data.get(p).ok_or(Error::Truncated)?;
    p += 1;
    if modes & 3 != 0 {
        return Err(Error::Corrupt); // reserved bits
    }
    let (lm, om, mm) = (modes >> 6, (modes >> 4) & 3, (modes >> 2) & 3);
    let (ll, n) = read_table(lm, &data[p..], MAX_LL, 9, &LL_DEFAULT, 6, &st.ll)?;
    p += n;
    let (of, n) = read_table(om, &data[p..], MAX_OF, 8, &OF_DEFAULT, 5, &st.of)?;
    p += n;
    let (ml, n) = read_table(mm, &data[p..], MAX_ML, 9, &ML_DEFAULT, 6, &st.ml)?;
    p += n;
    let mut bits = Rev::new(data.get(p..).ok_or(Error::Truncated)?)?;
    let (mut s_ll, mut s_of, mut s_ml) = (bits.read(ll.al) as usize, bits.read(of.al) as usize, bits.read(ml.al) as usize);
    let mut lit_pos = 0usize;
    for i in 0..nseq {
        let (cl, co, cm) = (ll.cells[s_ll], of.cells[s_of], ml.cells[s_ml]);
        if co.sym as usize > MAX_OF || cl.sym as usize > MAX_LL || cm.sym as usize > MAX_ML {
            return Err(Error::Corrupt);
        }
        let offv = (1u64 << co.sym) + bits.read(co.sym as u32);
        let mlen = ML_BASE[cm.sym as usize] as usize + bits.read(ML_BITS[cm.sym as usize] as u32) as usize;
        let llen = LL_BASE[cl.sym as usize] as usize + bits.read(LL_BITS[cl.sym as usize] as u32) as usize;
        if i + 1 < nseq {
            s_ll = cl.base as usize + bits.read(cl.nbits as u32) as usize;
            s_ml = cm.base as usize + bits.read(cm.nbits as u32) as usize;
            s_of = co.base as usize + bits.read(co.nbits as u32) as usize;
        }
        if bits.overflowed() {
            return Err(Error::Corrupt);
        }
        let offset: u32 = if offv > 3 {
            let o = (offv - 3) as u32;
            st.rep = [o, st.rep[0], st.rep[1]];
            o
        } else {
            let idx = offv as usize - 1 + (llen == 0) as usize;
            match idx {
                0 => st.rep[0],
                1 => {
                    st.rep.swap(0, 1);
                    st.rep[0]
                }
                2 => {
                    st.rep = [st.rep[2], st.rep[0], st.rep[1]];
                    st.rep[0]
                }
                _ => {
                    let o = st.rep[0].checked_sub(1).filter(|&o| o > 0).ok_or(Error::Corrupt)?;
                    st.rep = [o, st.rep[0], st.rep[1]];
                    o
                }
            }
        };
        let seg = lits.get(lit_pos..lit_pos + llen).ok_or(Error::Corrupt)?;
        out.push(seg)?;
        lit_pos += llen;
        out.copy_match(offset as usize, mlen)?;
    }
    if bits.pos != 0 {
        return Err(Error::Corrupt);
    }
    out.push(&lits[lit_pos..])?;
    st.ll = Some(ll);
    st.of = Some(of);
    st.ml = Some(ml);
    Ok(())
}

/// Decode the first frame of `src` into `dst`; returns the number of bytes produced.
/// Skippable frames before it are skipped. `dst` must be large enough for the whole frame.
pub fn decompress(src: &[u8], dst: &mut [u8]) -> Res<usize> {
    let mut p = 0usize;
    loop {
        let m = u32::from_le_bytes(src.get(p..p + 4).ok_or(Error::Truncated)?.try_into().unwrap());
        if m & SKIPPABLE_MASK == SKIPPABLE_MAGIC {
            let n = u32::from_le_bytes(src.get(p + 4..p + 8).ok_or(Error::Truncated)?.try_into().unwrap()) as usize;
            p = p.checked_add(8 + n).ok_or(Error::Corrupt)?;
            continue;
        }
        if m != MAGIC {
            return Err(Error::BadMagic);
        }
        p += 4;
        break;
    }
    let fhd = *src.get(p).ok_or(Error::Truncated)?;
    p += 1;
    let (fcs_flag, single, checksum, dict_flag) = (fhd >> 6, fhd >> 5 & 1 == 1, fhd >> 2 & 1 == 1, fhd & 3);
    if fhd & 0x08 != 0 {
        return Err(Error::Corrupt); // reserved bit
    }
    if !single {
        p += 1; // window descriptor: the output buffer is the window
    }
    let dict_len = [0, 1, 2, 4][dict_flag as usize];
    let dict_id = src.get(p..p + dict_len).ok_or(Error::Truncated)?;
    if dict_id.iter().any(|&b| b != 0) {
        return Err(Error::Unsupported);
    }
    p += dict_len;
    let fcs_len = match fcs_flag {
        0 => single as usize,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let fcs = src.get(p..p + fcs_len).ok_or(Error::Truncated)?;
    p += fcs_len;
    let content_size = if fcs_len == 0 {
        None
    } else {
        let mut v = 0u64;
        for (i, &b) in fcs.iter().enumerate() {
            v |= (b as u64) << (8 * i);
        }
        Some(if fcs_len == 2 { v + 256 } else { v })
    };
    if content_size.map_or(false, |c| c > dst.len() as u64) {
        return Err(Error::OutputTooSmall);
    }
    let mut st = State { huf: None, ll: None, of: None, ml: None, rep: [1, 4, 8] };
    let mut out = Out { buf: dst, len: 0 };
    loop {
        let h = src.get(p..p + 3).ok_or(Error::Truncated)?;
        p += 3;
        let hv = h[0] as u32 | (h[1] as u32) << 8 | (h[2] as u32) << 16;
        let (last, ty, size) = (hv & 1 == 1, (hv >> 1) & 3, (hv >> 3) as usize);
        if size > BLOCK_MAX {
            return Err(Error::Corrupt);
        }
        match ty {
            0 => out.push(src.get(p..p + size).ok_or(Error::Truncated)?).map(|_| p += size)?,
            1 => {
                out.push_rle(*src.get(p).ok_or(Error::Truncated)?, size)?;
                p += 1;
            }
            2 => {
                decode_block(&mut st, src.get(p..p + size).ok_or(Error::Truncated)?, &mut out)?;
                p += size;
            }
            _ => return Err(Error::Corrupt),
        }
        if last {
            break;
        }
    }
    if content_size.map_or(false, |c| c != out.len as u64) {
        return Err(Error::Corrupt);
    }
    if checksum {
        let want = u32::from_le_bytes(src.get(p..p + 4).ok_or(Error::Truncated)?.try_into().unwrap());
        if xxh64(&out.buf[..out.len]) as u32 != want {
            return Err(Error::ChecksumMismatch);
        }
    }
    Ok(out.len)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::process::{Command, Stdio};
    use std::string::String;
    use std::vec::Vec;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
    }

    fn have_zstd() -> bool {
        Command::new("zstd").arg("--version").output().is_ok()
    }

    fn compress(data: &[u8], args: &[&str]) -> Vec<u8> {
        use std::io::Write;
        let mut c = Command::new("zstd").args(args).arg("-q").arg("-c").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut stdin = c.stdin.take().unwrap();
        let d = data.to_vec();
        let t = std::thread::spawn(move || stdin.write_all(&d).unwrap());
        let out = c.wait_with_output().unwrap();
        t.join().unwrap();
        assert!(out.status.success());
        out.stdout
    }

    fn samples() -> Vec<(String, Vec<u8>)> {
        let mut r = Rng(7);
        let mut v: Vec<(String, Vec<u8>)> = Vec::new();
        v.push(("empty".into(), Vec::new()));
        v.push(("one".into(), std::vec![42]));
        v.push(("zeros 1M".into(), std::vec![0; 1 << 20]));
        v.push(("noise 300k".into(), (0..300_000).map(|_| r.next() as u8).collect()));
        v.push(("text 400k".into(), "the quick brown fox jumps over the lazy dog, again and again\n".repeat(7000).into_bytes()));
        v.push(("lines 250k".into(), (0..6000).flat_map(|i| std::format!("record {i:06} value={} tag={:x}\n", i * 7 % 1000, i * 2654435761u32 as usize).into_bytes()).collect()));
        let mut mixed = Vec::new();
        for i in 0..200 {
            match i % 5 {
                0 => mixed.extend((0..4096).map(|_| r.next() as u8)),
                1 => mixed.extend(std::vec![i as u8; 5000]),
                2 => mixed.extend("abcdefgh".repeat(700).into_bytes()),
                3 => mixed.extend((0..3000).map(|k| (k * k >> 3) as u8)),
                _ => mixed.extend((0..2000).map(|_| (r.next() % 7) as u8 + b'a')),
            }
        }
        v.push(("mixed 700k".into(), mixed));
        // skewed bytes: exercises Huffman with many distinct weights
        v.push(("skewed 200k".into(), (0..200_000).map(|_| { let x = r.next(); (x.trailing_zeros() * 9 + (x >> 20) % 3) as u8 }).collect()));
        for n in [2usize, 3, 31, 32, 33, 127, 128, 129, 4095, 4096, 65535, 65536, 131071, 131072, 131073] {
            v.push((std::format!("size {n}"), (0..n).map(|i| (i % 97) as u8 ^ (i >> 7) as u8).collect()));
        }
        // the zstd binary itself: real machine code
        if let Ok(b) = std::fs::read("/usr/bin/zstd") {
            v.push(("zstd binary".into(), b));
        }
        v
    }

    #[test]
    fn matches_reference_compressor_at_all_levels() {
        if !have_zstd() {
            std::eprintln!("zstd CLI missing: skipped");
            return;
        }
        let mut n = 0;
        for (name, data) in samples() {
            for args in [&["-1"][..], &["-3"], &["-9"], &["-19"], &["--fast=5"], &["-3", "--no-check"], &["-5", "--zstd=wlog=10"], &["-12", "--long=20"]] {
                let z = compress(&data, args);
                let mut out = std::vec![0u8; data.len()];
                let got = decompress(&z, &mut out).unwrap_or_else(|e| panic!("{name} {args:?}: {e:?}"));
                assert_eq!(got, data.len(), "{name} {args:?}");
                assert!(out == data, "{name} {args:?}: content differs");
                n += 1;
            }
        }
        std::eprintln!("{n} reference frames decoded and verified");
    }

    #[test]
    fn small_tables_use_predefined_and_rle_modes() {
        if !have_zstd() {
            return;
        }
        // tiny inputs make the encoder pick predefined / RLE sequence tables
        for len in 1..300usize {
            let data: Vec<u8> = (0..len).map(|i| b"abcabcabdabc"[i % 12]).collect();
            let z = compress(&data, &["-19"]);
            let mut out = std::vec![0u8; len];
            assert_eq!(decompress(&z, &mut out), Ok(len));
            assert_eq!(out, data, "len {len}");
        }
    }

    #[test]
    fn errors_are_reported_not_panicked() {
        let mut out = [0u8; 64];
        assert_eq!(decompress(&[], &mut out), Err(Error::Truncated));
        assert_eq!(decompress(&[1, 2, 3, 4, 5, 6], &mut out), Err(Error::BadMagic));
        let mut hdr = std::vec![0x28, 0xB5, 0x2F, 0xFD];
        hdr.extend([0x24, 0x00]); // checksum flag, no content size ... then truncated blocks
        assert!(decompress(&hdr, &mut out).is_err());
        // raw block that does not fit the output buffer
        let raw = [0x28, 0xB5, 0x2F, 0xFD, 0x20, 0x80, 0x01, 0x00, 0x00];
        let mut tiny = [0u8; 4];
        assert!(decompress(&raw, &mut tiny).is_err());
    }

    #[test]
    fn corrupted_frames_never_panic() {
        if !have_zstd() {
            return;
        }
        let mut r = Rng(99);
        let data: Vec<u8> = "metal os fuzz target with some structure 12345\n".repeat(900).into_bytes();
        let mut frames = Vec::new();
        for a in [&["-1"][..], &["-9"], &["-19"], &["-3", "--no-check"]] {
            frames.push(compress(&data, a));
        }
        let mut ok_by_luck = 0;
        for i in 0..30_000 {
            let mut z = frames[i % frames.len()].clone();
            for _ in 0..1 + r.next() % 3 {
                let at = r.next() as usize % z.len();
                z[at] ^= 1 << (r.next() % 8);
            }
            if r.next() % 5 == 0 {
                let cut = r.next() as usize % z.len();
                z.truncate(cut);
            }
            let mut out = std::vec![0u8; data.len() + 100];
            if decompress(&z, &mut out).is_ok() {
                ok_by_luck += 1; // only possible for no-check frames with harmless flips
            }
        }
        std::eprintln!("fuzz: 30000 mutated frames, {ok_by_luck} still decoded (no checksum to catch them)");
    }

    #[test]
    fn xxh64_known_values() {
        assert_eq!(xxh64(b""), 0xEF46DB3751D8E999);
        assert_eq!(xxh64(b"abc"), 0x44BC2CF5AD770999);
    }

    /// The allocation-free table builder must produce exactly the cells the full decoder builds.
    #[test]
    fn predefined_tables_equal_the_decoders() {
        fn same(norm: &[i16], al: u32) {
            let a = crate::fse::try_build(norm, al).unwrap();
            let b = build_fse(norm, al).unwrap();
            assert_eq!(b.cells.len(), 1 << al);
            for (x, y) in a.cells.iter().zip(b.cells.iter()) {
                assert_eq!((x.sym, x.nbits, x.base), (y.sym, y.nbits, y.base));
            }
        }
        same(&LL_DEFAULT, 6);
        same(&ML_DEFAULT, 6);
        same(&OF_DEFAULT, 5);
    }
}
