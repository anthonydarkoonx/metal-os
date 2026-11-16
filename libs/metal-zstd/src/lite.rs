//! An allocation-free Zstandard decompressor for a subset of the format, written from RFC 8878.
//!
//! It reads what `encode` writes (and any frame of the same shape): raw and RLE blocks, compressed
//! blocks whose literals are raw, RLE or Huffman-coded (codes up to 10 bits, one or four streams) and
//! whose sequences use the predefined, RLE or transmitted FSE tables (accuracy log up to 8), with
//! explicit offsets. Anything else (a Huffman table reused from an earlier block, repeated sequence
//! tables, repeat-offset codes, dictionaries) is reported as `Unsupported`, never misread. The whole frame
//! must fit in `dst`, which doubles as the match window. All input is untrusted: every access is
//! bounds-checked and malformed data yields an `Error`, never a panic.
use crate::bits::Rev;
use crate::fse::{self, Table, MAX_AL, MAX_SYMS};
use crate::huff::{self, HufDec};
use crate::tables::*;
use crate::xxh::xxh64;
use crate::Error;

const MAGIC: u32 = 0xFD2F_B528;
const BLOCK_MAX: usize = 128 * 1024;
/// Literals of one block that `decompress` can hold on its stack when they are Huffman-coded (a page's
/// literals never exceed 4 KiB); `decompress_with` takes a scratch buffer of any size.
const STACK_LITERALS: usize = 4096;

enum Literals<'a> {
    Raw(&'a [u8]),
    Rle(u8, usize),
}

impl Literals<'_> {
    fn len(&self) -> usize {
        match self {
            Literals::Raw(s) => s.len(),
            Literals::Rle(_, n) => *n,
        }
    }

    fn copy(&self, from: usize, dst: &mut [u8]) {
        match self {
            Literals::Raw(s) => dst.copy_from_slice(&s[from..from + dst.len()]),
            Literals::Rle(b, _) => dst.fill(*b),
        }
    }
}

fn take<'a>(src: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8], Error> {
    let end = at.checked_add(n).ok_or(Error::Truncated)?;
    let s = src.get(*at..end).ok_or(Error::Truncated)?;
    *at = end;
    Ok(s)
}

/// Decompress one frame from `src` into `dst`; returns the number of bytes produced.
pub fn decompress(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    let mut scratch = [0u8; STACK_LITERALS];
    decompress_with(src, dst, &mut scratch)
}

/// Like `decompress`, with the buffer that Huffman-coded literals are decoded into supplied by the caller:
/// it must be as large as the largest literals section in the frame (at most 128 KiB).
pub fn decompress_with(src: &[u8], dst: &mut [u8], scratch: &mut [u8]) -> Result<usize, Error> {
    let mut at = 0;
    let head = take(src, &mut at, 5)?;
    if u32::from_le_bytes([head[0], head[1], head[2], head[3]]) != MAGIC {
        return Err(Error::BadMagic);
    }
    let fhd = head[4];
    if fhd & 0x08 != 0 {
        return Err(Error::Corrupt); // reserved bit
    }
    if fhd & 3 != 0 {
        return Err(Error::Unsupported); // dictionary
    }
    let (fcs_flag, single, has_checksum) = (fhd >> 6, fhd >> 5 & 1 == 1, fhd >> 2 & 1 == 1);
    if !single {
        take(src, &mut at, 1)?; // window descriptor: the whole frame is the window here
    }
    let fcs_len = match (fcs_flag, single) {
        (0, true) => 1,
        (0, false) => 0,
        (1, _) => 2,
        (2, _) => 4,
        _ => 8,
    };
    let fcs = if fcs_len > 0 {
        let b = take(src, &mut at, fcs_len)?;
        let mut v = 0u64;
        for (i, &x) in b.iter().enumerate() {
            v |= (x as u64) << (8 * i);
        }
        Some(if fcs_flag == 1 { v + 256 } else { v })
    } else {
        None
    };
    if fcs.is_some_and(|n| n > dst.len() as u64) {
        return Err(Error::OutputTooSmall);
    }

    let tables = fse::predefined();
    let mut out = 0usize;
    loop {
        let h = take(src, &mut at, 3)?;
        let h = h[0] as u32 | (h[1] as u32) << 8 | (h[2] as u32) << 16;
        let (last, ty, size) = (h & 1 == 1, h >> 1 & 3, (h >> 3) as usize);
        if size > BLOCK_MAX {
            return Err(Error::Corrupt);
        }
        match ty {
            0 => {
                let raw = take(src, &mut at, size)?;
                dst.get_mut(out..out + size).ok_or(Error::OutputTooSmall)?.copy_from_slice(raw);
                out += size;
            }
            1 => {
                let b = take(src, &mut at, 1)?[0];
                dst.get_mut(out..out + size).ok_or(Error::OutputTooSmall)?.fill(b);
                out += size;
            }
            2 => {
                let content = take(src, &mut at, size)?;
                out = compressed_block(content, &tables, dst, out, scratch)?;
            }
            _ => return Err(Error::Corrupt),
        }
        if last {
            break;
        }
    }
    if has_checksum {
        let c = take(src, &mut at, 4)?;
        if u32::from_le_bytes([c[0], c[1], c[2], c[3]]) != xxh64(&dst[..out]) as u32 {
            return Err(Error::ChecksumMismatch);
        }
    }
    if fcs.is_some_and(|n| n != out as u64) {
        return Err(Error::Corrupt);
    }
    Ok(out)
}

/// The table for one sequence stream. Mode 0: the predefined table; 1: RLE (one symbol byte follows);
/// 2: a distribution description follows, built into `slot`; 3 (repeat the previous block's) is not supported.
fn seq_table<'a>(mode: u8, data: &[u8], max_sym: usize, pre: &'a Table, slot: &'a mut Table) -> Result<(&'a Table, usize), Error> {
    match mode {
        0 => Ok((pre, 0)),
        1 => {
            let sym = *data.first().ok_or(Error::Truncated)?;
            if sym as usize > max_sym {
                return Err(Error::Corrupt);
            }
            *slot = Table::rle(sym);
            Ok((slot, 1))
        }
        2 => {
            let mut norm = [0i16; MAX_SYMS];
            let (n, al, used) = fse::read_ncount(data, max_sym, MAX_AL, &mut norm)?;
            if n > max_sym + 1 {
                return Err(Error::Corrupt);
            }
            *slot = fse::try_build(&norm[..n], al).ok_or(Error::Corrupt)?;
            Ok((slot, used))
        }
        _ => Err(Error::Unsupported),
    }
}

/// Decode one compressed block, appending to `dst` after `start` bytes; returns the new length.
fn compressed_block(c: &[u8], pre: &fse::Predefined, dst: &mut [u8], start: usize, scratch: &mut [u8]) -> Result<usize, Error> {
    // Literals_Section (§3.1.1.3.1)
    let b0 = *c.first().ok_or(Error::Truncated)?;
    let byte = |i: usize| c.get(i).copied().ok_or(Error::Truncated);
    let mut at;
    let lits = if b0 & 3 < 2 {
        let rle = b0 & 3 == 1;
        let (hdr, size) = match b0 >> 2 & 3 {
            0 | 2 => (1, (b0 >> 3) as usize),
            1 => (2, (b0 >> 4) as usize | (byte(1)? as usize) << 4),
            _ => (3, (b0 >> 4) as usize | (byte(1)? as usize) << 4 | (byte(2)? as usize) << 12),
        };
        at = hdr;
        if rle {
            let b = byte(at)?;
            at += 1;
            Literals::Rle(b, size)
        } else {
            Literals::Raw(take(c, &mut at, size)?)
        }
    } else if b0 & 3 == 2 {
        let (four, hbytes, bits) = match b0 >> 2 & 3 {
            0 => (false, 3, 10),
            1 => (true, 3, 10),
            2 => (true, 4, 14),
            _ => (true, 5, 18),
        };
        let h = c.get(..hbytes).ok_or(Error::Truncated)?;
        let mut v = 0u64;
        for (i, &b) in h.iter().enumerate() {
            v |= (b as u64) << (8 * i);
        }
        let v = v >> 4;
        let mask = (1u64 << bits) - 1;
        let (regen, comp) = ((v & mask) as usize, (v >> bits & mask) as usize);
        if regen > BLOCK_MAX || comp == 0 {
            return Err(Error::Corrupt);
        }
        if regen > scratch.len() {
            return Err(Error::Unsupported); // more literals than the caller's buffer holds
        }
        at = hbytes;
        let payload = take(c, &mut at, comp)?;
        let mut dec = HufDec::EMPTY;
        let used = huff::read_tree(payload, &mut dec)?;
        let body = payload.get(used..).ok_or(Error::Truncated)?;
        let out = &mut scratch[..regen];
        if !four {
            huff::decode_stream(&dec, body, out)?;
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
                huff::decode_stream(&dec, &rest[bounds[i]..bounds[i + 1]], &mut out[lo..hi])?;
            }
        }
        Literals::Raw(&scratch[..regen])
    } else {
        return Err(Error::Unsupported); // a Huffman table reused from an earlier block
    };

    // Sequences_Section (§3.1.1.3.2)
    let n0 = byte(at)?;
    at += 1;
    let nseq = match n0 {
        0 => 0,
        1..=127 => n0 as usize,
        128..=254 => ((n0 as usize - 128) << 8) + byte(at)? as usize,
        _ => byte(at)? as usize + ((byte(at + 1)? as usize) << 8) + 0x7F00,
    };
    at += match n0 {
        0..=127 => 0,
        128..=254 => 1,
        _ => 2,
    };
    let mut out = start;
    if nseq == 0 {
        let room = dst.get_mut(out..out + lits.len()).ok_or(Error::OutputTooSmall)?;
        lits.copy(0, room);
        return Ok(out + lits.len());
    }
    let modes = byte(at)?;
    at += 1;
    if modes & 3 != 0 {
        return Err(Error::Corrupt); // reserved bits
    }
    let (mut dl, mut dof, mut dml) = (Table::EMPTY, Table::EMPTY, Table::EMPTY);
    let (t_ll, used) = seq_table(modes >> 6, c.get(at..).ok_or(Error::Truncated)?, 35, &pre.ll, &mut dl)?;
    at += used;
    let (t_of, used) = seq_table(modes >> 4 & 3, c.get(at..).ok_or(Error::Truncated)?, 31, &pre.of, &mut dof)?;
    at += used;
    let (t_ml, used) = seq_table(modes >> 2 & 3, c.get(at..).ok_or(Error::Truncated)?, 52, &pre.ml, &mut dml)?;
    at += used;
    let mut rev = Rev::new(c.get(at..).ok_or(Error::Truncated)?)?;
    let (mut s_ll, mut s_of, mut s_ml) = (rev.read(t_ll.al) as usize, rev.read(t_of.al) as usize, rev.read(t_ml.al) as usize);
    if s_ll >= t_ll.size() || s_of >= t_of.size() || s_ml >= t_ml.size() {
        return Err(Error::Corrupt);
    }
    let mut lit_pos = 0;
    for i in 0..nseq {
        let (cl, co, cm) = (t_ll.cells[s_ll], t_of.cells[s_of], t_ml.cells[s_ml]);
        let of_value = (1u64 << co.sym) + rev.read(co.sym as u32);
        let ml = ML_BASE[cm.sym as usize] as usize + rev.read(ML_BITS[cm.sym as usize] as u32) as usize;
        let ll = LL_BASE[cl.sym as usize] as usize + rev.read(LL_BITS[cl.sym as usize] as u32) as usize;
        if of_value <= 3 {
            return Err(Error::Unsupported); // repeat-offset codes: this decoder reads explicit offsets only
        }
        let offset = (of_value - 3) as usize;
        // execute: literals, then the match (§3.1.1.4)
        if lit_pos + ll > lits.len() {
            return Err(Error::Corrupt);
        }
        lits.copy(lit_pos, dst.get_mut(out..out + ll).ok_or(Error::OutputTooSmall)?);
        lit_pos += ll;
        out += ll;
        if offset > out {
            return Err(Error::Corrupt); // reaches before the start of the frame
        }
        if out + ml > dst.len() {
            return Err(Error::OutputTooSmall);
        }
        for k in 0..ml {
            dst[out + k] = dst[out + k - offset]; // may overlap itself: byte by byte
        }
        out += ml;
        if out - start > BLOCK_MAX {
            return Err(Error::Corrupt);
        }
        if i + 1 < nseq {
            // next states: literal length, match length, offset (§3.1.1.3.2.1.2)
            s_ll = cl.base as usize + rev.read(cl.nbits as u32) as usize;
            s_ml = cm.base as usize + rev.read(cm.nbits as u32) as usize;
            s_of = co.base as usize + rev.read(co.nbits as u32) as usize;
            if s_ll >= t_ll.size() || s_ml >= t_ml.size() || s_of >= t_of.size() {
                return Err(Error::Corrupt);
            }
        }
    }
    if rev.pos != 0 {
        return Err(Error::Corrupt); // bits left over, or read past the start
    }
    let rest = lits.len() - lit_pos;
    lits.copy(lit_pos, dst.get_mut(out..out + rest).ok_or(Error::OutputTooSmall)?);
    out += rest;
    if out - start > BLOCK_MAX {
        return Err(Error::Corrupt);
    }
    Ok(out)
}
