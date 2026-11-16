//! Huffman-coded literals (RFC 8878 §4.2), allocation-free: building a length-limited code from byte
//! counts, writing its description (direct 4-bit weights, or FSE-compressed weights), encoding a stream, and
//! the matching reader for the small decoder.
//!
//! A code is described by weights: weight `w` means a code of `maxbits + 1 - w` bits (0 = unused). The last
//! symbol's weight is left out and implied by the rule that the codes fill the space exactly. Codes are
//! assigned canonically: by weight ascending, then symbol. Streams are read backwards, so the encoder writes
//! the last symbol first.
use crate::bits::{BitW, Rev};
use crate::fse::{self, EncTable};
use crate::Error;

/// Longest code the compressor makes and the small decoder accepts (the format allows 11).
pub(crate) const MAX_BITS: u32 = 10;

// ---------------------------------------------------------------------------------------------- reading

/// A decoding table: the next `maxbits` bits of the stream index it; each entry is `symbol << 8 | length`.
pub(crate) struct HufDec {
    pub maxbits: u32,
    pub table: [u16; 1 << MAX_BITS],
}

impl HufDec {
    pub const EMPTY: HufDec = HufDec { maxbits: 0, table: [0; 1 << MAX_BITS] };
}

/// Read the tree description at the start of `data` into `dec`; returns the bytes it used.
pub(crate) fn read_tree(data: &[u8], dec: &mut HufDec) -> Result<usize, Error> {
    let hb = *data.first().ok_or(Error::Truncated)? as usize;
    let mut weights = [0u8; 260]; // at most 255 transmitted, two pushed per step, one implied
    let mut n = 0usize;
    let used;
    if hb >= 128 {
        n = hb - 127;
        let bytes = n.div_ceil(2);
        let body = data.get(1..1 + bytes).ok_or(Error::Truncated)?;
        for i in 0..n {
            let b = body[i / 2];
            weights[i] = if i % 2 == 0 { b >> 4 } else { b & 15 };
        }
        used = 1 + bytes;
    } else {
        let body = data.get(1..1 + hb).ok_or(Error::Truncated)?;
        let mut norm = [0i16; fse::MAX_SYMS];
        let (nsym, al, consumed) = fse::read_ncount(body, 12, 6, &mut norm)?;
        let t = fse::try_build(&norm[..nsym], al).ok_or(Error::Corrupt)?;
        let mut rev = Rev::new(body.get(consumed..).ok_or(Error::Truncated)?)?;
        let mut s1 = rev.read(al) as usize;
        let mut s2 = rev.read(al) as usize;
        // two interleaved states; the stream ending mid-way decides which symbol is last
        loop {
            if n >= 255 {
                return Err(Error::Corrupt);
            }
            let c = t.cells[s1];
            weights[n] = c.sym;
            n += 1;
            s1 = c.base as usize + rev.read(c.nbits as u32) as usize;
            if rev.overflowed() {
                weights[n] = t.cells[s2 & (fse::CAP - 1)].sym;
                n += 1;
                break;
            }
            let c = t.cells[s2];
            weights[n] = c.sym;
            n += 1;
            s2 = c.base as usize + rev.read(c.nbits as u32) as usize;
            if rev.overflowed() {
                weights[n] = t.cells[s1 & (fse::CAP - 1)].sym;
                n += 1;
                break;
            }
            if s1 >= t.size() || s2 >= t.size() {
                return Err(Error::Corrupt);
            }
        }
        used = 1 + hb;
    }
    if n == 0 || n > 256 || weights[..n].iter().any(|&w| w > 11) {
        return Err(Error::Corrupt);
    }
    // the last weight is implied: it must bring the total up to a power of two
    let total: u32 = weights[..n].iter().filter(|&&w| w > 0).map(|&w| 1u32 << (w - 1)).sum();
    if total == 0 {
        return Err(Error::Corrupt);
    }
    let maxbits = 32 - total.leading_zeros();
    if maxbits > MAX_BITS {
        return Err(if maxbits <= 11 { Error::Unsupported } else { Error::Corrupt });
    }
    let rest = (1u32 << maxbits) - total;
    if rest == 0 || !rest.is_power_of_two() {
        return Err(Error::Corrupt);
    }
    weights[n] = (rest.trailing_zeros() + 1) as u8;
    n += 1;
    let mut count = [0usize; 13];
    for &w in &weights[..n] {
        count[w as usize] += 1;
    }
    let mut rank_start = [0usize; 13];
    let mut start = 0usize;
    for w in 1..=maxbits as usize {
        rank_start[w] = start;
        start += count[w] << (w - 1);
    }
    dec.maxbits = maxbits;
    for (sym, &w) in weights[..n].iter().enumerate() {
        if w == 0 {
            continue;
        }
        let len = 1usize << (w - 1);
        let s = rank_start[w as usize];
        for e in dec.table.get_mut(s..s + len).ok_or(Error::Corrupt)? {
            *e = (sym as u16) << 8 | (maxbits + 1 - w as u32) as u16;
        }
        rank_start[w as usize] += len;
    }
    Ok(used)
}

/// Decode `out.len()` symbols from one backward stream; it must be used up exactly.
pub(crate) fn decode_stream(dec: &HufDec, data: &[u8], out: &mut [u8]) -> Result<(), Error> {
    let mut rev = Rev::new(data)?;
    for o in out.iter_mut() {
        let e = dec.table[rev.peek(dec.maxbits) as usize];
        if e & 0xff == 0 {
            return Err(Error::Corrupt);
        }
        rev.pos -= (e & 0xff) as i64;
        *o = (e >> 8) as u8;
    }
    if rev.pos != 0 {
        return Err(Error::Corrupt); // not all bits used, or read past the start
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------- writing

pub(crate) struct HufEnc {
    len: [u8; 256],
    code: [u16; 256],
    weights: [u8; 256],
    /// index of the highest symbol in use (its weight is not transmitted)
    last: usize,
}

impl HufEnc {
    pub const EMPTY: HufEnc = HufEnc { len: [0; 256], code: [0; 256], weights: [0; 256], last: 0 };

    /// Build a code for the byte counts in `hist`, at most `MAX_BITS` long. `None` with fewer than two symbols.
    pub fn build(&mut self, hist: &[u32; 256]) -> Option<()> {
        // symbols in use, by count ascending (ties by symbol)
        let mut syms = [0u8; 256];
        let mut n = 0;
        for (s, &c) in hist.iter().enumerate() {
            if c > 0 {
                syms[n] = s as u8;
                n += 1;
            }
        }
        if n < 2 {
            return None;
        }
        syms[..n].sort_unstable_by_key(|&s| (hist[s as usize], s));
        // the two-queue method: leaves in order, merged nodes appended in order
        let mut weight = [0u64; 512];
        let mut parent = [0u16; 512];
        for i in 0..n {
            weight[i] = hist[syms[i] as usize] as u64;
        }
        let (mut li, mut ni, mut next) = (0usize, n, n);
        for _ in 0..n - 1 {
            let mut pick = [0usize; 2];
            for p in pick.iter_mut() {
                *p = if li < n && (ni >= next || weight[li] <= weight[ni]) {
                    li += 1;
                    li - 1
                } else {
                    ni += 1;
                    ni - 1
                };
            }
            weight[next] = weight[pick[0]] + weight[pick[1]];
            parent[pick[0]] = next as u16;
            parent[pick[1]] = next as u16;
            next += 1;
        }
        let root = next - 1;
        let mut depth = [0u8; 512];
        for node in (0..root).rev() {
            depth[node] = depth[parent[node] as usize].saturating_add(1);
        }
        let mut lens = [0u8; 256]; // by position in `syms`
        lens[..n].copy_from_slice(&depth[..n]);
        limit_lengths(&mut lens[..n], &syms[..n], hist)?;

        self.len = [0; 256];
        let mut maxbits = 0;
        for i in 0..n {
            self.len[syms[i] as usize] = lens[i];
            maxbits = maxbits.max(lens[i] as u32);
        }
        self.last = syms[..n].iter().map(|&s| s as usize).max()?;
        // weights, then canonical codes: weight ascending, symbol ascending
        let mut count = [0usize; 13];
        self.weights = [0; 256];
        for s in 0..256 {
            if self.len[s] > 0 {
                let w = maxbits + 1 - self.len[s] as u32;
                self.weights[s] = w as u8;
                count[w as usize] += 1;
            }
        }
        let mut rank_start = [0usize; 13];
        let mut start = 0usize;
        for w in 1..=maxbits as usize {
            rank_start[w] = start;
            start += count[w] << (w - 1);
        }
        for s in 0..256 {
            let w = self.weights[s] as usize;
            if w > 0 {
                self.code[s] = (rank_start[w] >> (w - 1)) as u16;
                rank_start[w] += 1 << (w - 1);
            }
        }
        Some(())
    }

    /// Write the tree description into `out`; returns its length.
    pub fn write_tree(&self, out: &mut [u8]) -> Option<usize> {
        let n = self.last; // weights of symbols 0..last are transmitted
        if n <= 128 {
            let bytes = n.div_ceil(2);
            let body = out.get_mut(..1 + bytes)?;
            body[0] = (127 + n) as u8;
            for b in body[1..].iter_mut() {
                *b = 0;
            }
            for i in 0..n {
                body[1 + i / 2] |= self.weights[i] << if i % 2 == 0 { 4 } else { 0 };
            }
            return Some(1 + bytes);
        }
        self.write_fse_weights(out)
    }

    fn write_fse_weights(&self, out: &mut [u8]) -> Option<usize> {
        let w = &self.weights[..self.last];
        let mut hist = [0u32; 13];
        for &x in w {
            hist[x as usize] += 1;
        }
        let mut best: Option<usize> = None;
        let mut scratch = [0u8; 160];
        for al in 5..=6u32 {
            let mut norm = [0i16; 13];
            if !fse::normalize(&hist, al, &mut norm) {
                continue;
            }
            let Some(table) = fse::try_build(&norm, al) else { continue };
            let enc = EncTable::new(&table);
            let Some(head) = fse::write_ncount(&norm, al, &mut scratch) else { continue };
            let Some(body) = weight_stream(&enc, w, &mut scratch[head..]) else { continue };
            let total = head + body;
            if total < 128 && best.map_or(true, |b| total < b) {
                best = Some(total);
                let dst = out.get_mut(..1 + total)?;
                dst[0] = total as u8;
                dst[1..].copy_from_slice(&scratch[..total]);
            }
        }
        best.map(|t| 1 + t)
    }

    /// Encode `data` as one backward stream; returns its length in bytes.
    pub fn encode_stream(&self, data: &[u8], out: &mut [u8]) -> Option<usize> {
        let mut w = BitW::new(out);
        for &b in data.iter().rev() {
            w.put(self.code[b as usize] as u32, self.len[b as usize] as u32);
        }
        w.finish()
    }

    /// Size in bits of `hist` coded with this code.
    pub fn bits(&self, hist: &[u32; 256]) -> u64 {
        hist.iter().enumerate().map(|(s, &c)| c as u64 * self.len[s] as u64).sum()
    }
}

/// The weights as an FSE stream with two interleaved states (the decoder alternates them; see `read_tree`).
fn weight_stream(t: &EncTable, w: &[u8], out: &mut [u8]) -> Option<usize> {
    let n = w.len();
    if n < 2 {
        return None;
    }
    let mut bw = BitW::new(out);
    let mut state = [0usize; 2]; // states of the two chains, by parity of the symbol index
    state[(n - 1) & 1] = t.first_state_reading(w[n - 1] as usize)?;
    state[(n - 2) & 1] = t.first_state_reading(w[n - 2] as usize)?;
    // the transition bits, last first: symbol k's state is the one that leads to symbol k+2's
    let mut k = n - 2;
    while k > 0 {
        k -= 1;
        let (c, bits, nb) = t.cover(w[k] as usize, state[k & 1])?;
        bw.put(bits, nb);
        state[k & 1] = c;
    }
    bw.put(state[1] as u32, t.al());
    bw.put(state[0] as u32, t.al());
    bw.finish()
}

/// Shorten any code longer than `MAX_BITS` and make the lengths fill the code space exactly.
fn limit_lengths(lens: &mut [u8], syms: &[u8], hist: &[u32; 256]) -> Option<()> {
    let limit = MAX_BITS as u8;
    let full = 1u32 << limit;
    let kraft = |l: &[u8]| l.iter().map(|&x| full >> x).sum::<u32>();
    for l in lens.iter_mut() {
        *l = (*l).min(limit);
    }
    // too full: lengthen the cheapest symbols that can still grow
    while kraft(lens) > full {
        let pick = (0..lens.len()).filter(|&i| lens[i] < limit).min_by_key(|&i| (hist[syms[i] as usize], core::cmp::Reverse(lens[i])))?;
        lens[pick] += 1;
    }
    // too empty: shorten the most frequent symbols that still fit
    loop {
        let k = kraft(lens);
        if k == full {
            return Some(());
        }
        let pick = (0..lens.len()).filter(|&i| lens[i] > 1 && k + (full >> lens[i]) <= full).max_by_key(|&i| hist[syms[i] as usize])?;
        lens[pick] -= 1;
    }
}
