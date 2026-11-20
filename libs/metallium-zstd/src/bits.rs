//! Bit readers and writers shared by the compressor and the small decoder.
use crate::Error;

/// Reads a bitstream backwards (the way FSE and Huffman streams are read, RFC 8878 §4.1): `pos` counts the
/// unread bits; reading past the start yields zeros and leaves `pos` negative.
pub(crate) struct Rev<'a> {
    data: &'a [u8],
    pub pos: i64,
}

impl<'a> Rev<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self, Error> {
        let last = *data.last().ok_or(Error::Truncated)?;
        if last == 0 {
            return Err(Error::Corrupt); // the end marker bit must exist
        }
        let hb = 7 - last.leading_zeros() as i64;
        Ok(Rev { data, pos: (data.len() as i64 - 1) * 8 + hb })
    }

    pub fn peek(&self, n: u32) -> u64 {
        if n == 0 {
            return 0;
        }
        let start = self.pos - n as i64;
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

    pub fn read(&mut self, n: u32) -> u64 {
        let v = self.peek(n);
        self.pos -= n as i64;
        v
    }

    pub fn overflowed(&self) -> bool {
        self.pos < 0
    }
}

/// Forward, least-significant-bit-first reader (FSE table descriptions).
pub(crate) struct Fwd<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl Fwd<'_> {
    pub fn peek(&self, n: u32) -> u32 {
        let mut v = 0u64;
        let first = self.pos / 8;
        for k in 0..5 {
            v |= (*self.data.get(first + k).unwrap_or(&0) as u64) << (8 * k);
        }
        ((v >> (self.pos % 8)) & ((1u64 << n) - 1)) as u32
    }

    pub fn read(&mut self, n: u32) -> Result<u32, Error> {
        let v = self.peek(n);
        self.skip(n)?;
        Ok(v)
    }

    pub fn skip(&mut self, n: u32) -> Result<(), Error> {
        if self.pos + n as usize > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        self.pos += n as usize;
        Ok(())
    }
}

/// Forward bit writer, least significant bit first.
pub(crate) struct BitW<'a> {
    out: &'a mut [u8],
    pos: usize,
    acc: u64,
    nb: u32,
    overflow: bool,
}

impl<'a> BitW<'a> {
    pub fn new(out: &'a mut [u8]) -> Self {
        BitW { out, pos: 0, acc: 0, nb: 0, overflow: false }
    }

    fn byte(&mut self, b: u8) {
        match self.out.get_mut(self.pos) {
            Some(slot) => *slot = b,
            None => self.overflow = true,
        }
        self.pos += 1;
    }

    /// Append the low `n` bits of `v` (n <= 32).
    pub fn put(&mut self, v: u32, n: u32) {
        if n == 0 {
            return;
        }
        self.acc |= ((v as u64) & ((1u64 << n) - 1)) << self.nb;
        self.nb += n;
        while self.nb >= 8 {
            self.byte(self.acc as u8);
            self.acc >>= 8;
            self.nb -= 8;
        }
    }

    /// Pad with zeros to a byte boundary (for a forward description, with no end marker).
    pub fn align(mut self) -> Option<usize> {
        if self.nb > 0 {
            self.byte(self.acc as u8);
        }
        if self.overflow {
            None
        } else {
            Some(self.pos)
        }
    }

    /// End a backward stream: a single 1 bit, then zeros up to the byte boundary (§3.1.1.3.2.1.2).
    pub fn finish(mut self) -> Option<usize> {
        self.put(1, 1);
        self.align()
    }
}
