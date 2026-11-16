//! Zstandard (RFC 8878), written from the specification (see PROVENANCE.md; the `zstd` command-line
//! tool is only a test oracle).
//!
//! * `decoder` (feature `decoder`, on by default; needs `alloc`): the full frame decompressor.
//! * `encode`: a compressor that needs no allocation: LZ77 matching, Huffman-coded literals, FSE-coded
//!   sequences with the predefined tables or ones fitted to the block. Its output is an ordinary
//!   Zstandard frame.
//! * `lite`: an allocation-free decompressor for exactly that subset (raw/RLE/Huffman literals,
//!   predefined/RLE/transmitted tables, explicit offsets), which is all a kernel without a heap needs to
//!   read what `encode` wrote.
#![no_std]
#[cfg(feature = "decoder")]
extern crate alloc;

pub(crate) mod bits;
pub mod encode;
pub(crate) mod fse;
pub(crate) mod huff;
pub mod lite;
pub(crate) mod tables;
pub mod xxh;

#[cfg(feature = "decoder")]
mod decoder;
#[cfg(feature = "decoder")]
pub use decoder::decompress;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadMagic,
    Truncated,
    Corrupt,
    Unsupported,
    OutputTooSmall,
    ChecksumMismatch,
}
