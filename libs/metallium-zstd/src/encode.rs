//! A Zstandard compressor that needs no allocation, written from RFC 8878.
//!
//! Per block it finds matches greedily with a 4-byte hash (offsets stay inside the block). The literals are
//! Huffman-coded (or raw, or RLE, whichever is smallest), and each of the three sequence streams (literal
//! length, offset, match length) picks the cheapest of the predefined FSE distribution, a one-symbol RLE
//! table, or a distribution fitted to this block and transmitted in front. A block that does not shrink is
//! stored raw, and a block of one repeated byte as an RLE block. The result is an ordinary Zstandard
//! frame: any decoder reads it.
//!
//! All working memory is a caller-provided [`Workspace`] (for 4 KiB pages: about 14 KiB), so this runs
//! in a kernel without a heap.
//!
//! How the sequences are written (§3.1.1.3.2.1.2 and §4.1): the decoder reads the bitstream
//! backwards, so the encoder writes the last sequence first. For each of the three streams (literal
//! length, match length, offset) the decoder walks a chain of FSE states, each state naming a symbol
//! and telling how many bits to read to reach the next. Going backwards, the encoder knows the next
//! state `T` and needs the one cell that yields the wanted symbol and whose interval
//! `[base, base + 2^nbits)` contains `T`; the bits it writes are `T - base`.
use crate::bits::BitW;
use crate::fse::{self, ll_code, ml_code, EncTable, Table};
use crate::huff::HufEnc;
use crate::xxh::xxh64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// The output buffer is too small (see `Workspace::max_compressed_len`).
    OutputTooSmall,
    /// The input is larger than a frame header can describe here (4 GiB).
    InputTooLarge,
}

const MAGIC: u32 = 0xFD2F_B528;
const HASH_LOG: u32 = 11;
const HASH: usize = 1 << HASH_LOG;
const MIN_MATCH: usize = 4;
/// How many earlier positions with the same hash are tried for a longer match.
const CHAIN_DEPTH: usize = 12;
const BLOCK_MAX: usize = 128 * 1024;

#[derive(Clone, Copy)]
struct Seq {
    ll: u32,
    ml: u32,
    off: u32,
}

/// Memory for compressing blocks of up to `B` bytes producing at most `S` sequences (`S` = `B / 4 + 1`
/// always suffices, since every match is at least 4 bytes).
pub struct Workspace<const B: usize, const S: usize> {
    hash: [u32; HASH],
    /// for each position, the previous position with the same hash (plus one; 0 = none)
    prev: [u32; B],
    seqs: [Seq; S],
    lits: [u8; B],
    /// the three streams' symbol codes per sequence: literal length, offset, match length
    codes: [[u8; S]; 3],
    /// the predefined tables prepared for encoding, built on first use
    pre: Option<[EncTable; 3]>,
}

/// Workspace for one 4 KiB page.
pub type PageWorkspace = Workspace<4096, 1025>;

fn emit(out: &mut [u8], at: &mut usize, data: &[u8]) -> Result<(), EncodeError> {
    let dst = out.get_mut(*at..*at + data.len()).ok_or(EncodeError::OutputTooSmall)?;
    dst.copy_from_slice(data);
    *at += data.len();
    Ok(())
}

/// Offset code for a real offset `off` (>= 1): the offset value is `off + 3` (values 1..=3 would
/// mean "repeat offset", which this encoder never uses); code = its top bit position.
fn of_code(off: u32) -> (usize, u32, u32) {
    let value = off + 3;
    let code = 31 - value.leading_zeros();
    (code as usize, value - (1 << code), code)
}

impl<const B: usize, const S: usize> Workspace<B, S> {
    const SIZES_OK: () = assert!(B <= BLOCK_MAX && B >= 16 && S >= 1);

    pub const fn new() -> Self {
        let () = Self::SIZES_OK;
        Workspace { hash: [0; HASH], prev: [0; B], seqs: [Seq { ll: 0, ml: 0, off: 0 }; S], lits: [0; B], codes: [[0; S]; 3], pre: None }
    }

    /// An upper bound on the size of `compress`'s output for `n` input bytes (with or without checksum).
    pub const fn max_compressed_len(n: usize) -> usize {
        n + 3 * (n / B + 1) + 16
    }

    /// Compress `input` into one complete Zstandard frame in `out`; returns the frame's length.
    /// With `checksum` the frame carries an XXH64 content checksum.
    pub fn compress(&mut self, input: &[u8], out: &mut [u8], checksum: bool) -> Result<usize, EncodeError> {
        let n = input.len();
        if n > u32::MAX as usize {
            return Err(EncodeError::InputTooLarge);
        }
        let mut w = 0;
        emit(out, &mut w, &MAGIC.to_le_bytes())?;
        // Frame header: single segment (the window is the whole content), content size always present
        let (flag, fcs): (u8, [u8; 4]) = if n <= 255 {
            (0, (n as u32).to_le_bytes())
        } else if n <= 65791 {
            (1, ((n - 256) as u32).to_le_bytes())
        } else {
            (2, (n as u32).to_le_bytes())
        };
        emit(out, &mut w, &[flag << 6 | 1 << 5 | (checksum as u8) << 2])?;
        emit(out, &mut w, &fcs[..[1, 2, 4][flag as usize]])?;

        let mut off = 0;
        loop {
            let len = (n - off).min(B);
            let last = off + len == n;
            self.block(&input[off..off + len], last, out, &mut w)?;
            off += len;
            if last {
                break;
            }
        }
        if checksum {
            emit(out, &mut w, &(xxh64(input) as u32).to_le_bytes())?;
        }
        Ok(w)
    }

    fn block(&mut self, chunk: &[u8], last: bool, out: &mut [u8], w: &mut usize) -> Result<(), EncodeError> {
        let header = |ty: u32, size: usize| ((last as u32) | ty << 1 | (size as u32) << 3).to_le_bytes();
        if chunk.len() >= 2 && chunk.iter().all(|&b| b == chunk[0]) {
            emit(out, w, &header(1, chunk.len())[..3])?; // RLE block: one byte, repeated
            return emit(out, w, &chunk[..1]);
        }
        let start = *w;
        if let Some(body) = out.get_mut(start + 3..) {
            if let Ok(clen) = self.compressed_block(chunk, body) {
                if clen < chunk.len() {
                    out[start..start + 3].copy_from_slice(&header(2, clen)[..3]);
                    *w = start + 3 + clen;
                    return Ok(());
                }
            }
        }
        emit(out, w, &header(0, chunk.len())[..3])?; // did not shrink: store it
        emit(out, w, chunk)
    }

    /// The content of a compressed block: literals section, then sequences section.
    fn compressed_block(&mut self, chunk: &[u8], out: &mut [u8]) -> Result<usize, EncodeError> {
        let (nseq, nlit) = self.find_matches(chunk);
        let mut w = 0;
        self.write_literals(nlit, out, &mut w)?;

        // Sequences_Section
        if nseq == 0 {
            emit(out, &mut w, &[0])?;
            return Ok(w);
        }
        if nseq < 128 {
            emit(out, &mut w, &[nseq as u8])?;
        } else if nseq < 0x7F00 {
            emit(out, &mut w, &[128 + (nseq >> 8) as u8, nseq as u8])?;
        } else {
            let v = nseq - 0x7F00;
            emit(out, &mut w, &[255, v as u8, (v >> 8) as u8])?;
        }
        self.write_sequences(nseq, out, &mut w)?;
        Ok(w)
    }

    /// Literals_Section (§3.1.1.3.1): the smallest of RLE, Huffman-coded and raw.
    fn write_literals(&mut self, nlit: usize, out: &mut [u8], w: &mut usize) -> Result<(), EncodeError> {
        let lits = &self.lits[..nlit];
        // raw and RLE headers: the size in 5, 12 or 20 bits (§3.1.1.3.1.1)
        let simple_header = |ty: u8, out: &mut [u8], w: &mut usize| {
            if nlit < 32 {
                emit(out, w, &[(nlit as u8) << 3 | ty])
            } else if nlit < 4096 {
                emit(out, w, &[(nlit as u8 & 0xF) << 4 | 0b01 << 2 | ty, (nlit >> 4) as u8])
            } else {
                emit(out, w, &[(nlit as u8 & 0xF) << 4 | 0b11 << 2 | ty, (nlit >> 4) as u8, (nlit >> 12) as u8])
            }
        };
        if nlit >= 2 && lits.iter().all(|&b| b == lits[0]) {
            simple_header(1, out, w)?;
            return emit(out, w, &lits[..1]);
        }
        let raw_size = nlit + if nlit < 32 { 1 } else if nlit < 4096 { 2 } else { 3 };
        if nlit >= 32 {
            if let Some(n) = self.huffman_literals(nlit, raw_size, &mut out[*w..]) {
                *w += n;
                return Ok(());
            }
        }
        simple_header(0, out, w)?;
        emit(out, w, &self.lits[..nlit])
    }

    /// Try to write the literals Huffman-coded into `out`; the size written, if it beats `raw_size`.
    fn huffman_literals(&self, nlit: usize, raw_size: usize, out: &mut [u8]) -> Option<usize> {
        let lits = &self.lits[..nlit];
        let mut hist = [0u32; 256];
        for &b in lits {
            hist[b as usize] += 1;
        }
        let mut enc = HufEnc::EMPTY;
        enc.build(&hist)?;
        if (enc.bits(&hist) / 8) as usize + 8 >= raw_size {
            return None; // cannot win even before the description
        }
        // header: type 2, size format by the sizes (single stream below 1 KiB), 10, 14 or 18 bit sizes
        let (format, hbytes, bits) = if nlit < 1024 { (0u32, 3usize, 10u32) } else if nlit < 16384 { (2, 4, 14) } else { (3, 5, 18) };
        let body = out.get_mut(hbytes..)?;
        let tree = enc.write_tree(body)?;
        let mut comp = tree;
        if format == 0 {
            comp += enc.encode_stream(lits, body.get_mut(comp..)?)?;
        } else {
            let seg = nlit.div_ceil(4);
            let jump = comp;
            comp += 6;
            let mut sizes = [0usize; 4];
            for (i, size) in sizes.iter_mut().enumerate() {
                let part = &lits[(i * seg).min(nlit)..((i + 1) * seg).min(nlit)];
                *size = enc.encode_stream(part, body.get_mut(comp..)?)?;
                comp += *size;
            }
            for i in 0..3 {
                body.get_mut(jump + 2 * i..jump + 2 * i + 2)?.copy_from_slice(&u16::try_from(sizes[i]).ok()?.to_le_bytes());
            }
        }
        if hbytes + comp >= raw_size || comp >= 1 << bits {
            return None;
        }
        let v = (nlit as u64 | (comp as u64) << bits) << 4 | 2 | (format as u64) << 2;
        out[..hbytes].copy_from_slice(&v.to_le_bytes()[..hbytes]);
        Some(hbytes + comp)
    }

    /// The modes byte, the three table descriptions and the bitstream.
    fn write_sequences(&mut self, n: usize, out: &mut [u8], w: &mut usize) -> Result<(), EncodeError> {
        for i in 0..n {
            let s = self.seqs[i];
            self.codes[0][i] = ll_code(s.ll).0 as u8;
            self.codes[1][i] = of_code(s.off).0 as u8;
            self.codes[2][i] = ml_code(s.ml).0 as u8;
        }
        let pre = self.pre.get_or_insert_with(|| {
            let p = fse::predefined();
            [EncTable::new(&p.ll), EncTable::new(&p.of), EncTable::new(&p.ml)]
        });
        // per stream: the cheapest of the predefined table (mode 0), RLE (1) and a fitted table (2)
        let mut mode = [0u8; 3];
        let mut fitted: [Option<EncTable>; 3] = [None, None, None];
        let mut desc = [[0u8; 96]; 3];
        let mut desc_len = [0usize; 3];
        for i in 0..3 {
            let codes = &self.codes[i][..n];
            let mut best = chain_cost(&pre[i], codes).unwrap_or(u64::MAX);
            let mut counts = [0u32; fse::MAX_SYMS];
            for &c in codes {
                counts[c as usize] += 1;
            }
            let nsym = counts.iter().filter(|&&c| c > 0).count();
            if nsym == 1 {
                let sym = codes[0];
                if 8 < best {
                    best = 8;
                    mode[i] = 1;
                    desc[i][0] = sym;
                    desc_len[i] = 1;
                    fitted[i] = Some(EncTable::new(&Table::rle(sym)));
                }
                continue;
            }
            let min_al = (usize::BITS - (nsym - 1).leading_zeros()).max(5);
            for al in min_al..=fse::MAX_AL {
                let mut norm = [0i16; fse::MAX_SYMS];
                if !fse::normalize(&counts, al, &mut norm) {
                    continue;
                }
                let Some(table) = fse::try_build(&norm, al) else { continue };
                let mut bytes = [0u8; 96];
                let Some(len) = fse::write_ncount(&norm, al, &mut bytes) else { continue };
                let enc = EncTable::new(&table);
                let Some(cost) = chain_cost(&enc, codes) else { continue };
                let cost = cost + 8 * len as u64;
                if cost < best {
                    best = cost;
                    mode[i] = 2;
                    desc[i] = bytes;
                    desc_len[i] = len;
                    fitted[i] = Some(enc);
                }
            }
        }
        emit(out, w, &[mode[0] << 6 | mode[1] << 4 | mode[2] << 2])?;
        for i in 0..3 {
            emit(out, w, &desc[i][..desc_len[i]])?;
        }
        let pick = |i: usize| -> &EncTable {
            match (&fitted[i], mode[i]) {
                (Some(t), 1 | 2) => t,
                _ => &pre[i],
            }
        };
        let tables = [pick(0), pick(1), pick(2)];
        let body = out.get_mut(*w..).ok_or(EncodeError::OutputTooSmall)?;
        *w += Self::encode_sequences(&self.seqs[..n], tables, body)?;
        Ok(())
    }

    /// Matching with short hash chains and one step of lazy evaluation: at each position the best match among
    /// the last `CHAIN_DEPTH` positions with the same 4-byte hash is found, and it is only taken if the next
    /// position does not offer a clearly longer one. Fills `seqs` and `lits`; returns (sequence count,
    /// literal count).
    fn find_matches(&mut self, chunk: &[u8]) -> (usize, usize) {
        let n = chunk.len();
        self.hash.fill(0);
        let rd = |p: usize| u32::from_le_bytes([chunk[p], chunk[p + 1], chunk[p + 2], chunk[p + 3]]);
        let hash_of = |v: u32| (v.wrapping_mul(2_654_435_761) >> (32 - HASH_LOG)) as usize;
        // insert position p; the chain links it to the previous position with the same hash (0 = none, else p+1)
        macro_rules! insert {
            ($p:expr) => {{
                let (p, h) = ($p, hash_of(rd($p)));
                self.prev[p] = self.hash[h];
                self.hash[h] = (p + 1) as u32;
            }};
        }
        // the longest match at `i` among the chain's candidates: (length, offset)
        let best_at = |this: &Self, i: usize| -> (usize, usize) {
            let mut best = (0usize, 0usize);
            let mut cand = this.hash[hash_of(rd(i))] as usize;
            let mut depth = 0;
            while cand != 0 && depth < CHAIN_DEPTH {
                let c = cand - 1;
                if chunk[c + best.0.min(n - i - 1)] == chunk[i + best.0.min(n - i - 1)] && rd(c) == rd(i) {
                    let mut ml = MIN_MATCH;
                    while i + ml < n && chunk[c + ml] == chunk[i + ml] {
                        ml += 1;
                    }
                    // a longer match wins; at equal length the nearer one (cheaper to code) was seen first
                    if ml > best.0 {
                        best = (ml, i - c);
                    }
                }
                cand = this.prev[c] as usize;
                depth += 1;
            }
            best
        };
        let (mut i, mut lit_start, mut nseq, mut nlit) = (0usize, 0usize, 0usize, 0usize);
        while i + MIN_MATCH <= n {
            let (mut ml, mut off) = if nseq < S { best_at(self, i) } else { (0, 0) };
            if ml < MIN_MATCH {
                insert!(i);
                i += 1;
                continue;
            }
            // lazy: is the match one byte later clearly better (it costs one more literal)?
            insert!(i);
            while i + 1 + MIN_MATCH <= n {
                let (ml2, off2) = best_at(self, i + 1);
                if ml2 > ml + 1 {
                    i += 1;
                    insert!(i);
                    ml = ml2;
                    off = off2;
                } else {
                    break;
                }
            }
            let ll = i - lit_start;
            self.lits[nlit..nlit + ll].copy_from_slice(&chunk[lit_start..i]);
            nlit += ll;
            self.seqs[nseq] = Seq { ll: ll as u32, ml: ml as u32, off: off as u32 };
            nseq += 1;
            // remember the positions inside the match too, so later data can refer to them
            let end = i + ml;
            let mut p = i + 1;
            while p < end && p + MIN_MATCH <= n {
                insert!(p);
                p += 1;
            }
            i = end;
            lit_start = i;
        }
        let rest = n - lit_start;
        self.lits[nlit..nlit + rest].copy_from_slice(&chunk[lit_start..]);
        (nseq, nlit + rest)
    }

    /// The FSE bitstream of `seqs` (at least one), written last sequence first. `t` holds the literal-length,
    /// offset and match-length tables.
    fn encode_sequences(seqs: &[Seq], t: [&EncTable; 3], out: &mut [u8]) -> Result<usize, EncodeError> {
        let bad = EncodeError::OutputTooSmall; // unreachable: every table was checked to cover its symbols
        let mut bw = BitW::new(out);
        let codes = |s: &Seq| (ll_code(s.ll), ml_code(s.ml), of_code(s.off));
        let n = seqs.len();

        // The last sequence: any state that names its symbol; its extra bits go first.
        let ((llc, llx, llb), (mlc, mlx, mlb), (ofc, ofx, ofb)) = codes(&seqs[n - 1]);
        let mut s_ll = t[0].first_state(llc).ok_or(bad)?;
        let mut s_of = t[1].first_state(ofc).ok_or(bad)?;
        let mut s_ml = t[2].first_state(mlc).ok_or(bad)?;
        bw.put(llx, llb);
        bw.put(mlx, mlb);
        bw.put(ofx, ofb);

        for i in (0..n - 1).rev() {
            let ((llc, llx, llb), (mlc, mlx, mlb), (ofc, ofx, ofb)) = codes(&seqs[i]);
            // the states that lead, after the bits written next, to the ones already chosen
            let (c_ll, b_ll, n_ll) = t[0].cover(llc, s_ll).ok_or(bad)?;
            let (c_of, b_of, n_of) = t[1].cover(ofc, s_of).ok_or(bad)?;
            let (c_ml, b_ml, n_ml) = t[2].cover(mlc, s_ml).ok_or(bad)?;
            // the decoder updates literal length, match length, offset, in that order: write the reverse
            bw.put(b_of, n_of);
            bw.put(b_ml, n_ml);
            bw.put(b_ll, n_ll);
            (s_ll, s_of, s_ml) = (c_ll, c_of, c_ml);
            // then this sequence's extra bits, written so the decoder reads offset, match length, literal length
            bw.put(llx, llb);
            bw.put(mlx, mlb);
            bw.put(ofx, ofb);
        }
        // initial states: the decoder reads literal length, offset, match length
        bw.put(s_ml as u32, t[2].al());
        bw.put(s_of as u32, t[1].al());
        bw.put(s_ll as u32, t[0].al());
        bw.finish().ok_or(EncodeError::OutputTooSmall)
    }
}

/// What the state chain of one stream costs in bits with table `t`: the initial state plus every transition.
/// `None` if the table has no state for one of the symbols.
fn chain_cost(t: &EncTable, codes: &[u8]) -> Option<u64> {
    let n = codes.len();
    let mut state = t.first_state(codes[n - 1] as usize)?;
    let mut bits = t.al() as u64;
    for k in (0..n - 1).rev() {
        let (c, _, nb) = t.cover(codes[k] as usize, state)?;
        bits += nb as u64;
        state = c;
    }
    Some(bits)
}

impl<const B: usize, const S: usize> Default for Workspace<B, S> {
    fn default() -> Self {
        Self::new()
    }
}
