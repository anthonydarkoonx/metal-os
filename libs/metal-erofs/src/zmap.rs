//! Compressed-file support for the EROFS reader.
//!
//! PROVENANCE. This module is a fresh implementation. It was written from
//!  * the EROFS on-disk structure layouts (field offsets and sizes of the
//!    superblock, inodes and the per-inode compression header), and
//!  * measurements of real images: hexdumps of the index area of images made by
//!    the reference `mkfs.erofs`, compared against the extent maps printed by
//!    `dump.erofs -e` (used purely as black-box test oracles; see
//!    `tests/real_images.rs` and `PROVENANCE.md`).
//! No Linux kernel or erofs-utils source was consulted while writing it.
//!
//! WHAT THE INDEX IS. A compressed file is cut into *logical clusters* of
//! `1 << lcbits` bytes. Each one has an index entry saying either "an extent
//! starts in me" (a *head*: with the offset inside the cluster where it
//! starts, and where its compressed data lives) or "I continue an extent that
//! started `back` clusters earlier" (a *tail*). A head is `Plain` (stored raw),
//! `Head1` (compressed with the inode's first algorithm) or `Head2` (second
//! algorithm). An extent runs from its head's start to the next head's start.
//!
//! Two on-disk encodings carry the same information:
//!  * legacy: one 8-byte record per cluster;
//!  * compact: entries bit-packed into small *packs*, each ending in a 32-bit
//!    base block address from which its heads' addresses are derived.
//!
//! This module decodes a whole pack at once, front to back, into [`Lc`] values.
use super::*;

/// One physical cluster is never larger than this many blocks.
const MAX_CBLKS: u64 = 256;
/// One decompressed extent is never larger than this.
const MAX_EXTENT: u64 = 1 << 20;

const ALG_LZ4: u8 = 0;
const ALG_ZSTD: u8 = 3;

// header "advise" bits
const ADV_PACKED_2B: u16 = 0x0001; // compact layout may use 2-byte entry packs
const ADV_BIG: u16 = 0x0002 | 0x0004; // physical clusters may span several blocks
const ADV_NOT_SUPPORTED: u16 = 0x0008 | 0x0010 | 0x0020; // inline data, interlaced, packed fragments
const CLUSTERBITS_FRAGMENT_INODE: u8 = 0x80;

/// Marker bit in a tail's value: the low bits are a block count, not a distance.
const MARKER: u32 = 1 << 11;
const MARKER_MASK: u32 = MARKER - 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Plain,
    Head1,
    Head2,
    Tail,
}

/// A decoded index entry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Lc {
    kind: Kind,
    /// Heads: where inside this logical cluster the extent starts.
    ofs: u64,
    /// Tails: how many clusters back the head is (0 for heads).
    back: u64,
    /// Tails carrying a marker: blocks in the physical cluster of the extent they belong to.
    blocks: u64,
    /// Heads: first block of the extent's compressed data.
    pblk: u64,
}

impl Lc {
    fn is_head(&self) -> bool {
        self.kind != Kind::Tail
    }
}

struct Index {
    legacy: bool,
    big: bool,
    packed_2b: bool,
    lcbits: u32,
    /// Absolute offset of the first index byte.
    first: u64,
    /// Number of logical clusters.
    count: u64,
    /// Algorithm ids of Head1 and Head2 clusters.
    alg: [u8; 2],
}

/// Where a compact pack lives and how it is shaped.
struct Pack {
    at: u64,
    /// Entries in the pack (2 or 16) and bits per entry (16 or 14).
    entries: usize,
    width: usize,
    /// Logical cluster number of its first entry.
    first_lcn: u64,
}

pub(crate) struct Extent {
    nid: u64,
    start: u64,
    data: Vec<u8>,
}

/// One-entry caches: the last decoded pack and the last decompressed extent.
#[derive(Default)]
pub(crate) struct Cache {
    extent: Option<Extent>,
    pack: Option<(u64, [Lc; 16])>,
}

const BLANK: Lc = Lc { kind: Kind::Tail, ofs: 0, back: 0, blocks: 0, pblk: 0 };

impl<S: Source> Erofs<S> {
    fn index_of(&self, ino: &Inode) -> Result<Index, Error> {
        let at = (ino.inline_off + 7) & !7; // header: 8-byte aligned, after the inode and its xattrs
        let mut h = [0u8; 8];
        self.src.read_at(at, &mut h)?;
        let advise = le16(&h, 4);
        if advise & ADV_NOT_SUPPORTED != 0 || h[7] & CLUSTERBITS_FRAGMENT_INODE != 0 {
            return Err(Error::Unsupported);
        }
        let lcbits = self.blksz.trailing_zeros() + (h[7] & 7) as u32;
        let legacy = ino.layout == LAYOUT_COMPRESSED_FULL;
        let packed_2b = !legacy && advise & ADV_PACKED_2B != 0;
        if lcbits > 14 || (packed_2b && lcbits > 12) {
            return Err(Error::Unsupported);
        }
        Ok(Index {
            legacy,
            big: advise & ADV_BIG != 0,
            packed_2b,
            lcbits,
            // the legacy table leaves 8 bytes of padding after the header, the compact one starts right away
            first: at + if legacy { 16 } else { 8 },
            count: ino.size.div_ceil(1 << lcbits),
            alg: [h[6] & 15, h[6] >> 4],
        })
    }

    /// Locate the compact pack holding cluster `lcn`.
    ///
    /// Observed layout: 4-byte-per-entry packs (2 entries, 8 bytes) from the first index byte up
    /// to the first 32-byte-aligned address; then, if the inode allows it, 2-byte-per-entry packs
    /// (16 entries, 32 bytes) covering as many whole packs as fit; then 4-byte packs again.
    fn pack_of(&self, ix: &Index, lcn: u64) -> Pack {
        let lead = ((32 - ix.first % 32) % 32) / 4; // entries before the first aligned address
        let lead = lead.min(ix.count);
        let wide = if ix.packed_2b { (ix.count - lead) / 16 * 16 } else { 0 };
        let (region_start, region_lcn, entries, width) = if lcn < lead {
            (ix.first, 0, 2usize, 16usize)
        } else if lcn < lead + wide {
            (ix.first + lead * 4, lead, 16, 14)
        } else {
            (ix.first + lead * 4 + wide * 2, lead + wide, 2, 16)
        };
        let n = entries as u64;
        let pack_index = (lcn - region_lcn) / n;
        let bytes = if entries == 16 { 32 } else { 8 };
        Pack { at: region_start + pack_index * bytes, entries, width, first_lcn: region_lcn + pack_index * n }
    }

    /// Read `width`-bit entry `j` of a pack: (12-bit value, 2-bit type).
    fn raw_entry(bytes: &[u8; 32], j: usize, width: usize) -> (u32, u8) {
        let bit = j * width;
        let mut w = [0u8; 4];
        let from = bit / 8;
        let n = (bytes.len() - from).min(4);
        w[..n].copy_from_slice(&bytes[from..from + n]);
        let v = u32::from_le_bytes(w) >> (bit % 8);
        (v & 0xFFF, ((v >> 12) & 3) as u8)
    }

    /// Decode every entry of a compact pack, front to back.
    ///
    /// Value meaning: head/plain -> offset in cluster; tail -> distance back to its head. Two
    /// exceptions found in real images: a tail with the marker bit carries a block count
    /// (and is exactly one cluster after its head), and the *last* entry of a pack carries the
    /// distance *forward* instead, so its distance back is its predecessor's plus one.
    ///
    /// Head addresses: each pack stores a base block. For ordinary images a head's block is
    /// base + 1 + (heads before it in the pack). For images that allow big clusters it is
    /// base + the blocks of every earlier physical cluster in the pack, each counted once: at
    /// its marker if it has one (that marker may be the pack's first entry, belonging to a head
    /// in the previous pack), otherwise at its head as one block.
    fn decode_pack(&self, ix: &Index, p: &Pack) -> Result<[Lc; 16], Error> {
        let size = if p.entries == 16 { 32 } else { 8 };
        let mut bytes = [0u8; 32];
        self.src.read_at(p.at, &mut bytes[..size])?;
        let base_blk = le32(&bytes, size - 4) as u64;
        let mut out = [BLANK; 16];
        let (mut heads_before, mut blocks_before) = (0u64, 0u64);
        for j in 0..p.entries {
            let (v, ty) = Self::raw_entry(&bytes, j, p.width);
            out[j] = if ty == 2 {
                let marker = v & MARKER != 0;
                let back = if marker {
                    1
                } else if j + 1 < p.entries {
                    v as u64
                } else if j > 0 {
                    out[j - 1].back + 1
                } else {
                    return Err(Error::Corrupt);
                };
                let blocks = if marker { (v & MARKER_MASK) as u64 } else { 0 };
                if marker && ix.big {
                    blocks_before += blocks;
                }
                Lc { kind: Kind::Tail, ofs: 0, back, blocks, pblk: 0 }
            } else {
                let kind = match ty {
                    0 => Kind::Plain,
                    1 => Kind::Head1,
                    _ => Kind::Head2,
                };
                let marker_follows = j + 1 < p.entries && {
                    let (nv, nt) = Self::raw_entry(&bytes, j + 1, p.width);
                    nt == 2 && nv & MARKER != 0
                };
                let pblk = base_blk + if ix.big { blocks_before } else { 1 + heads_before };
                heads_before += 1;
                if ix.big && !marker_follows {
                    blocks_before += 1;
                }
                Lc { kind, ofs: v as u64, back: 0, blocks: 0, pblk }
            };
        }
        Ok(out)
    }

    fn lcluster(&self, ix: &Index, lcn: u64) -> Result<Lc, Error> {
        if lcn >= ix.count {
            return Err(Error::Corrupt);
        }
        if ix.legacy {
            let mut e = [0u8; 8];
            self.src.read_at(ix.first + lcn * 8, &mut e)?;
            let (adv, ofs, word) = (le16(&e, 0), le16(&e, 2) as u64, le32(&e, 4));
            return Ok(match adv & 3 {
                2 => {
                    let d0 = word & 0xFFFF;
                    let marker = d0 & MARKER != 0;
                    Lc { kind: Kind::Tail, ofs: 0, back: if marker { 1 } else { d0 as u64 }, blocks: if marker { (d0 & MARKER_MASK) as u64 } else { 0 }, pblk: 0 }
                }
                t => Lc { kind: [Kind::Plain, Kind::Head1, Kind::Tail, Kind::Head2][t as usize], ofs, back: 0, blocks: 0, pblk: word as u64 },
            });
        }
        let p = self.pack_of(ix, lcn);
        let mut cache = self.z.borrow_mut();
        let entries = match cache.pack {
            Some((at, e)) if at == p.at => e,
            _ => {
                let e = self.decode_pack(ix, &p)?;
                cache.pack = Some((p.at, e));
                e
            }
        };
        Ok(entries[(lcn - p.first_lcn) as usize])
    }

    /// Cluster number of the head of the extent that cluster `lcn` (decoded as `lc`) belongs to.
    fn head_of(&self, lcn: u64, lc: Lc) -> Result<u64, Error> {
        if lc.is_head() {
            return Ok(lcn);
        }
        if lc.back == 0 || lc.back > lcn {
            return Err(Error::Corrupt);
        }
        Ok(lcn - lc.back)
    }

    /// The extent containing byte `pos`: (start, end, first block, blocks, head kind).
    fn extent_at(&self, ix: &Index, size: u64, pos: u64) -> Result<(u64, u64, u64, u64, Kind), Error> {
        let (cluster, within) = (pos >> ix.lcbits, pos & ((1 << ix.lcbits) - 1));
        let here = self.lcluster(ix, cluster)?;
        let head_lcn = if here.is_head() && within >= here.ofs {
            cluster
        } else if here.is_head() {
            // `pos` precedes the extent that starts in this cluster: it belongs to the previous one
            let before = cluster.checked_sub(1).ok_or(Error::Corrupt)?;
            let prev = self.lcluster(ix, before)?;
            self.head_of(before, prev)?
        } else {
            self.head_of(cluster, here)?
        };
        let head = self.lcluster(ix, head_lcn)?;
        if !head.is_head() {
            return Err(Error::Corrupt);
        }
        let start = (head_lcn << ix.lcbits) + head.ofs;
        // Walk forward to the next head; the cluster right after ours says how many blocks we own.
        let mut blocks = 1;
        let mut next = head_lcn + 1;
        let end = loop {
            if (next << ix.lcbits) >= size {
                break size;
            }
            let lc = self.lcluster(ix, next)?;
            if lc.is_head() {
                break (next << ix.lcbits) + lc.ofs;
            }
            if next == head_lcn + 1 && lc.blocks > 0 {
                blocks = lc.blocks;
            }
            next += 1;
        };
        if !(start <= pos && pos < end) || blocks > MAX_CBLKS {
            return Err(Error::Corrupt);
        }
        Ok((start, end, head.pblk, blocks, head.kind))
    }

    fn decode_extent(&self, ino: &Inode, ix: &Index, pos: u64) -> Result<Extent, Error> {
        let (start, end, pblk, blocks, kind) = self.extent_at(ix, ino.size, pos)?;
        let len = end - start;
        if len > MAX_EXTENT {
            return Err(Error::Unsupported);
        }
        let mut raw = vec![0u8; (blocks * self.blksz) as usize];
        self.src.read_at(pblk * self.blksz, &mut raw)?;
        let data = match kind {
            Kind::Plain => {
                if len > raw.len() as u64 {
                    return Err(Error::Corrupt);
                }
                raw.truncate(len as usize);
                raw
            }
            _ => {
                // The compressed bytes may be preceded by zero padding inside the cluster.
                let skip = raw.iter().position(|&b| b != 0).ok_or(Error::Corrupt)?;
                let mut out = vec![0u8; len as usize];
                let alg = ix.alg[(kind == Kind::Head2) as usize];
                let got = match alg {
                    ALG_LZ4 if self.lz4_0padding => metal_lz4::decompress(&raw[skip..], &mut out).map_err(|_| Error::Corrupt)?,
                    ALG_ZSTD => metal_zstd::decompress(&raw[skip..], &mut out).map_err(|_| Error::Corrupt)?,
                    _ => return Err(Error::Unsupported), // other algorithms
                };
                if got as u64 != len {
                    return Err(Error::Corrupt);
                }
                out
            }
        };
        Ok(Extent { nid: ino.nid, start, data })
    }

    pub(crate) fn read_compressed(&self, ino: &Inode, off: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let ix = self.index_of(ino)?;
        let (mut pos, mut done) = (off, 0usize);
        while done < buf.len() && pos < ino.size {
            let cached = self.z.borrow().extent.as_ref().map_or(false, |e| e.nid == ino.nid && e.start <= pos && pos < e.start + e.data.len() as u64);
            if !cached {
                let e = self.decode_extent(ino, &ix, pos)?;
                self.z.borrow_mut().extent = Some(e);
            }
            let cache = self.z.borrow();
            let e = cache.extent.as_ref().unwrap();
            let rel = (pos - e.start) as usize;
            let n = (buf.len() - done).min(e.data.len() - rel);
            buf[done..done + n].copy_from_slice(&e.data[rel..rel + n]);
            done += n;
            pos += n as u64;
        }
        Ok(done)
    }
}
