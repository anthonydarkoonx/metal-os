//! Allocation-free FSE (RFC 8878 §4.1): decoding tables built from a normalised distribution, the
//! distribution's description (reader and writer), and what the compressor needs on top: normalising counts
//! and finding the state that leads to a given next state.
//!
//! Tables are at most `1 << MAX_AL` = 256 cells: the compressor never makes a larger one and the small
//! decoder reports `Unsupported` for a larger one (the format allows 512 for literal and match lengths).
use crate::bits::{BitW, Fwd};
use crate::tables::*;
use crate::Error;

pub(crate) const MAX_AL: u32 = 8;
pub(crate) const CAP: usize = 1 << MAX_AL;
/// Symbols per table: 36 literal-length, 53 match-length, 32 offset codes, 12 Huffman weights.
pub(crate) const MAX_SYMS: usize = 64;

#[derive(Clone, Copy, Default)]
pub(crate) struct Cell {
    pub sym: u8,
    /// Bits to read to get from this state to the next one.
    pub nbits: u8,
    /// Added to those bits to give the next state.
    pub base: u16,
}

#[derive(Clone, Copy)]
pub(crate) struct Table {
    pub al: u32,
    pub cells: [Cell; CAP],
}

impl Table {
    pub const EMPTY: Table = Table { al: 0, cells: [Cell { sym: 0, nbits: 0, base: 0 }; CAP] };

    /// Number of states.
    pub fn size(&self) -> usize {
        1 << self.al
    }

    /// The table of a stream whose every symbol is `sym` (compression mode "RLE"): one state, no bits.
    pub fn rle(sym: u8) -> Table {
        let mut t = Table::EMPTY;
        t.cells[0] = Cell { sym, nbits: 0, base: 0 };
        t
    }
}

/// The decoding table of normalised distribution `norm` (RFC 8878 §4.1.1) with accuracy log `al`, or `None`
/// if the distribution is not valid. Built exactly the way the spec says, so the cells equal any other
/// decoder's.
pub(crate) fn try_build(norm: &[i16], al: u32) -> Option<Table> {
    if al > MAX_AL || norm.len() > MAX_SYMS {
        return None;
    }
    let size = 1usize << al;
    let mut t = Table::EMPTY;
    t.al = al;
    let mut next = [0u32; MAX_SYMS];
    // symbols with probability "less than one" take the highest states, one each
    let mut high = size as isize - 1;
    for (s, &c) in norm.iter().enumerate() {
        if c == -1 {
            if high < 0 {
                return None;
            }
            t.cells[high as usize].sym = s as u8;
            high -= 1;
            next[s] = 1;
        } else if c >= 0 {
            next[s] = c as u32;
        } else {
            return None;
        }
    }
    // the others are spread over the remaining states with a fixed step
    let step = (size >> 1) + (size >> 3) + 3;
    let mask = size - 1;
    let mut pos = 0usize;
    for (s, &c) in norm.iter().enumerate() {
        for _ in 0..c.max(0) {
            if pos > mask {
                return None;
            }
            t.cells[pos].sym = s as u8;
            pos = (pos + step) & mask;
            while pos as isize > high {
                pos = (pos + step) & mask;
            }
        }
    }
    if pos != 0 {
        return None; // the counts do not add up to the table size
    }
    for cell in t.cells[..size].iter_mut() {
        let s = cell.sym as usize;
        let ns = next[s];
        next[s] += 1;
        if ns == 0 {
            return None;
        }
        let nb = al - (31 - ns.leading_zeros());
        cell.nbits = nb as u8;
        cell.base = ((ns << nb) as usize).checked_sub(size)? as u16;
    }
    Some(t)
}

/// The predefined tables for literal lengths, match lengths and offsets (§3.1.1.3.2.2).
#[derive(Clone, Copy)]
pub(crate) struct Predefined {
    pub ll: Table,
    pub ml: Table,
    pub of: Table,
}

pub(crate) fn predefined() -> Predefined {
    // the built-in distributions are valid
    Predefined {
        ll: try_build(&LL_DEFAULT, 6).unwrap_or(Table::EMPTY),
        ml: try_build(&ML_DEFAULT, 6).unwrap_or(Table::EMPTY),
        of: try_build(&OF_DEFAULT, 5).unwrap_or(Table::EMPTY),
    }
}

/// Read a distribution description (§4.1.1). Fills `norm` (`max_sym + 1` entries at most) and returns
/// (number of symbols, accuracy log, bytes used).
pub(crate) fn read_ncount(data: &[u8], max_sym: usize, max_al: u32, norm: &mut [i16; MAX_SYMS]) -> Result<(usize, u32, usize), Error> {
    let mut br = Fwd { data, pos: 0 };
    let al = br.read(4)? + 5;
    if al > max_al {
        return Err(Error::Unsupported);
    }
    let mut remaining: i32 = (1 << al) + 1;
    let mut threshold: i32 = 1 << al;
    let mut nbbits = al + 1;
    let mut n = 0usize;
    let mut previous0 = false;
    while remaining > 1 && n <= max_sym {
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
            if n + zeros > max_sym + 1 {
                return Err(Error::Corrupt);
            }
            for _ in 0..zeros {
                norm[n] = 0;
                n += 1;
            }
            if n > max_sym {
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
        norm[n] = count as i16;
        n += 1;
        previous0 = count == 0;
        while remaining < threshold {
            nbbits -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return Err(Error::Corrupt);
    }
    Ok((n, al, br.pos.div_ceil(8)))
}

/// Write the description of `norm` (no negative counts) with accuracy log `al`; returns its length in bytes.
/// The inverse of `read_ncount`.
pub(crate) fn write_ncount(norm: &[i16], al: u32, out: &mut [u8]) -> Option<usize> {
    let last = norm.iter().rposition(|&c| c != 0)?;
    let mut w = BitW::new(out);
    w.put(al - 5, 4);
    let mut remaining: i32 = (1 << al) + 1;
    let mut threshold: i32 = 1 << al;
    let mut nbbits = al + 1;
    let mut i = 0usize;
    let mut previous0 = false;
    while i <= last {
        if previous0 {
            let mut z = 0;
            while norm[i + z] == 0 {
                z += 1;
            }
            let mut left = z;
            while left >= 3 {
                w.put(3, 2);
                left -= 3;
            }
            w.put(left as u32, 2);
            i += z;
        }
        let c = norm[i] as i32;
        let v = c + 1;
        let max = 2 * threshold - 1 - remaining;
        if v < max {
            w.put(v as u32, nbbits - 1);
        } else {
            w.put(if v >= threshold { v + max } else { v } as u32, nbbits);
        }
        remaining -= c;
        previous0 = c == 0;
        i += 1;
        while remaining < threshold {
            nbbits -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return None;
    }
    w.align()
}

/// Scale `counts` to a distribution of total `1 << al`, every present symbol keeping at least one state.
/// False if there are more symbols than states.
pub(crate) fn normalize(counts: &[u32], al: u32, norm: &mut [i16]) -> bool {
    let size = 1i64 << al;
    let total: i64 = counts.iter().map(|&c| c as i64).sum();
    let nsym = counts.iter().filter(|&&c| c > 0).count() as i64;
    if total == 0 || nsym > size {
        return false;
    }
    let mut sum = 0i64;
    for (i, &c) in counts.iter().enumerate() {
        norm[i] = if c == 0 { 0 } else { (c as i64 * size / total).max(1) as i16 };
        sum += norm[i] as i64;
    }
    // what the rounding gave each symbol, relative to its exact share: positive means it deserves more
    let deserves = |i: usize, norm: &[i16]| counts[i] as i64 * size - norm[i] as i64 * total;
    while sum != size {
        let mut best: Option<usize> = None;
        for i in 0..counts.len() {
            if counts[i] == 0 || (sum > size && norm[i] <= 1) {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) if sum < size => deserves(i, norm) > deserves(b, norm),
                Some(b) => deserves(i, norm) < deserves(b, norm),
            };
            if better {
                best = Some(i);
            }
        }
        let Some(b) = best else { return false };
        if sum < size {
            norm[b] += 1;
            sum += 1;
        } else {
            norm[b] -= 1;
            sum -= 1;
        }
    }
    true
}

/// A decoding table prepared for encoding: for each symbol, its cells ordered by base, so the cell whose
/// interval contains a wanted next state is found by binary search.
pub(crate) struct EncTable {
    pub t: Table,
    order: [u16; CAP],
    start: [u16; MAX_SYMS + 1],
}

impl EncTable {
    pub fn new(t: &Table) -> EncTable {
        let mut e = EncTable { t: *t, order: [0; CAP], start: [0; MAX_SYMS + 1] };
        let size = t.size();
        let mut count = [0u16; MAX_SYMS];
        for c in &t.cells[..size] {
            count[(c.sym as usize).min(MAX_SYMS - 1)] += 1;
        }
        let mut acc = 0u16;
        for s in 0..MAX_SYMS {
            e.start[s] = acc;
            acc += count[s];
        }
        e.start[MAX_SYMS] = acc;
        let mut fill = [0u16; MAX_SYMS];
        for (i, c) in t.cells[..size].iter().enumerate() {
            let s = (c.sym as usize).min(MAX_SYMS - 1);
            e.order[(e.start[s] + fill[s]) as usize] = i as u16;
            fill[s] += 1;
        }
        for s in 0..MAX_SYMS {
            let (a, b) = (e.start[s] as usize, e.start[s + 1] as usize);
            e.order[a..b].sort_unstable_by_key(|&c| t.cells[c as usize].base);
        }
        e
    }

    pub fn al(&self) -> u32 {
        self.t.al
    }

    /// Any state that names `sym`.
    pub fn first_state(&self, sym: usize) -> Option<usize> {
        (sym < MAX_SYMS && self.start[sym] < self.start[sym + 1]).then(|| self.order[self.start[sym] as usize] as usize)
    }

    /// A state that names `sym` and reads at least one bit to leave it (when the symbol has such a state):
    /// the Huffman-weights stream relies on the decoder running out of bits at the update that follows
    /// the last symbol of a chain, which a zero-bit state would not do.
    pub fn first_state_reading(&self, sym: usize) -> Option<usize> {
        if sym >= MAX_SYMS {
            return None;
        }
        let group = &self.order[self.start[sym] as usize..self.start[sym + 1] as usize];
        let best = group.iter().copied().max_by_key(|&c| self.t.cells[c as usize].nbits)?;
        (self.t.cells[best as usize].nbits > 0).then_some(best as usize)
    }

    /// The state that names `sym` and, after `nbits` more bits (the value returned), moves to `next`:
    /// (state, bits value, bit count).
    pub fn cover(&self, sym: usize, next: usize) -> Option<(usize, u32, u32)> {
        if sym >= MAX_SYMS {
            return None;
        }
        let group = &self.order[self.start[sym] as usize..self.start[sym + 1] as usize];
        let idx = group.partition_point(|&c| self.t.cells[c as usize].base as usize <= next);
        let c = *group.get(idx.checked_sub(1)?)? as usize;
        let cell = self.t.cells[c];
        let base = cell.base as usize;
        (next < base + (1usize << cell.nbits)).then(|| (c, (next - base) as u32, cell.nbits as u32))
    }
}

/// Literal-length code and the extra bits that complete it: (code, extra value, extra bit count).
pub(crate) fn ll_code(ll: u32) -> (usize, u32, u32) {
    let c = LL_BASE.iter().rposition(|&b| b <= ll).unwrap_or(0);
    (c, ll - LL_BASE[c], LL_BITS[c] as u32)
}

/// Match-length code (`ml` >= 3), same shape as `ll_code`.
pub(crate) fn ml_code(ml: u32) -> (usize, u32, u32) {
    let c = ML_BASE.iter().rposition(|&b| b <= ml).unwrap_or(0);
    (c, ml - ML_BASE[c], ML_BITS[c] as u32)
}
