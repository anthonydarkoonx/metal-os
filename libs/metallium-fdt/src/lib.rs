//! A flattened device tree (devicetree specification, chapter 5) reader, for the facts the kernel needs at boot: the
//! RAM, the cores, the interrupt controller, the firmware call conduit.
//!
//! The blob comes from firmware and is untrusted: every offset and length is checked against the blob's own size
//! before it is used, and a malformed one makes `Fdt::new` or the walk return `None`/stop, never read outside it.
//! Nothing is allocated: the walk is a loop over the structure block with a small fixed stack of node depths.
#![no_std]

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;
/// Deepest nesting the walk follows (real trees are 4 or 5 deep).
const MAX_DEPTH: usize = 16;

/// A validated view of a device tree blob.
#[derive(Clone, Copy)]
pub struct Fdt<'a> {
    blob: &'a [u8],
    structs: &'a [u8],
    strings: &'a [u8],
}

/// One property: its name and its raw (big-endian) value.
#[derive(Clone, Copy)]
pub struct Prop<'a> {
    pub name: &'a [u8],
    pub value: &'a [u8],
}

impl<'a> Prop<'a> {
    /// The value as one big-endian 32-bit cell.
    pub fn u32(&self) -> Option<u32> {
        Some(u32::from_be_bytes(self.value.get(..4)?.try_into().ok()?))
    }

    /// The `i`-th 32-bit cell.
    pub fn cell(&self, i: usize) -> Option<u32> {
        let o = i.checked_mul(4)?;
        Some(u32::from_be_bytes(self.value.get(o..o.checked_add(4)?)?.try_into().ok()?))
    }

    /// `cells` cells (1 or 2) starting at cell `i`, as one number.
    pub fn num(&self, i: usize, cells: usize) -> Option<u64> {
        match cells {
            1 => self.cell(i).map(u64::from),
            2 => Some((self.cell(i)? as u64) << 32 | self.cell(i + 1)? as u64),
            _ => None,
        }
    }

    /// The value as a string (up to the first NUL).
    pub fn str(&self) -> Option<&'a [u8]> {
        let n = self.value.iter().position(|&c| c == 0)?;
        Some(&self.value[..n])
    }

    /// Whether a string list (`compatible`) contains `s`.
    pub fn has_string(&self, s: &[u8]) -> bool {
        self.value.split(|&c| c == 0).any(|e| e == s)
    }
}

/// Where the walk is when it hands a node or property to the caller.
pub struct Node<'a> {
    /// Node name with its unit address (`cpu@1`).
    pub name: &'a [u8],
    pub depth: usize,
}

/// What a walk visitor is told.
pub enum Event<'a> {
    Enter(Node<'a>),
    Prop(Prop<'a>),
    Leave(Node<'a>),
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?))
}

impl<'a> Fdt<'a> {
    /// Check the header and the two blocks the blob says it has. `None` if anything lies outside the blob or the version is not 17-compatible.
    pub fn new(blob: &'a [u8]) -> Option<Fdt<'a>> {
        if be32(blob, 0)? != MAGIC {
            return None;
        }
        let total = be32(blob, 4)? as usize;
        let (off_struct, off_strings) = (be32(blob, 8)? as usize, be32(blob, 12)? as usize);
        let (last_compat, version) = (be32(blob, 24)?, be32(blob, 20)?);
        let (size_strings, size_struct) = (be32(blob, 32)? as usize, be32(blob, 36)? as usize);
        if total > blob.len() || version < 16 || last_compat > 17 {
            return None;
        }
        let blob = &blob[..total];
        let structs = blob.get(off_struct..off_struct.checked_add(size_struct)?)?;
        let strings = blob.get(off_strings..off_strings.checked_add(size_strings)?)?;
        Some(Fdt { blob, structs, strings })
    }

    /// The blob's own size (header field), for a caller that wants to reserve it.
    pub fn size(&self) -> usize {
        self.blob.len()
    }

    fn name_at(&self, off: usize) -> Option<&'a [u8]> {
        let s = self.strings.get(off..)?;
        let n = s.iter().position(|&c| c == 0)?;
        Some(&s[..n])
    }

    /// Visit every node and property in order. The visitor returns `false` to stop. A malformed structure block ends the walk early.
    pub fn walk(&self, mut f: impl FnMut(Event<'a>) -> bool) {
        let b = self.structs;
        let mut o = 0usize;
        let mut names: [&[u8]; MAX_DEPTH] = [&[]; MAX_DEPTH];
        let mut depth = 0usize;
        while let Some(tag) = be32(b, o) {
            o += 4;
            match tag {
                BEGIN_NODE => {
                    let Some(rest) = b.get(o..) else { return };
                    let Some(n) = rest.iter().position(|&c| c == 0) else { return };
                    if depth >= MAX_DEPTH {
                        return;
                    }
                    names[depth] = &rest[..n];
                    o = (o + n + 1 + 3) & !3;
                    depth += 1;
                    if !f(Event::Enter(Node { name: names[depth - 1], depth })) {
                        return;
                    }
                }
                END_NODE => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                    if !f(Event::Leave(Node { name: names[depth], depth: depth + 1 })) {
                        return;
                    }
                }
                PROP => {
                    let (Some(len), Some(nameoff)) = (be32(b, o), be32(b, o + 4)) else { return };
                    o += 8;
                    let len = len as usize;
                    let Some(value) = o.checked_add(len).and_then(|e| b.get(o..e)) else { return };
                    let Some(name) = self.name_at(nameoff as usize) else { return };
                    o = (o + len + 3) & !3;
                    if !f(Event::Prop(Prop { name, value })) {
                        return;
                    }
                }
                NOP => {}
                END => return,
                _ => return,
            }
        }
    }
}

/// How many cores the machine has at most, which the fixed tables inside `Machine` can describe.
pub const MAX_CORES: usize = 64;

/// How the firmware is called (`/psci` `method`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conduit {
    Hvc,
    Smc,
}

/// Which interrupt controller, and where its registers are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gic {
    /// GICv2: distributor, CPU interface.
    V2 { dist: u64, cpu: u64 },
    /// GICv3/v4: distributor, and the redistributor region (all cores' frames in a row) with its size.
    V3 { dist: u64, redist: u64, redist_size: u64 },
}

/// The facts about the machine the kernel takes from the tree.
#[derive(Clone, Copy, Debug)]
pub struct Machine {
    /// The first RAM region (`/memory`): base and size.
    pub ram: Option<(u64, u64)>,
    /// MPIDR affinity values (`/cpus/cpu@N` `reg`) in tree order; the first is the boot core in practice, the caller decides.
    pub cpus: [u64; MAX_CORES],
    /// `capacity-dmips-mhz` of each core of `cpus` (0: the core has none): how fast it is, relative to the others; big.LITTLE machines differ here.
    pub capacity: [u32; MAX_CORES],
    pub ncpus: usize,
    pub gic: Option<Gic>,
    pub conduit: Option<Conduit>,
    /// `/chosen` `stdout-path` is not used; the UART base is (first `arm,pl011` node's `reg`), if any.
    pub uart: Option<u64>,
}

impl Machine {
    const fn empty() -> Machine {
        Machine { ram: None, cpus: [0; MAX_CORES], capacity: [0; MAX_CORES], ncpus: 0, gic: None, conduit: None, uart: None }
    }
}

/// Read the machine's description out of a tree. Missing pieces stay `None`; a core list longer than `MAX_CORES` is cut.
pub fn machine(fdt: &Fdt) -> Machine {
    #[derive(Clone, Copy, Default)]
    struct Cur<'a> {
        reg: Option<&'a [u8]>,
        compatible: Option<Prop<'a>>,
        method: Option<&'a [u8]>,
        capacity: u32,
        disabled: bool,
    }
    let mut m = Machine::empty();
    // per depth: this node's own #address-cells / #size-cells (what its children's `reg` use), and what it has said so far
    let mut cells = [(2usize, 1usize); MAX_DEPTH + 2];
    let mut cur = [Cur::default(); MAX_DEPTH + 2];
    let mut name_at: [&[u8]; MAX_DEPTH + 2] = [&[]; MAX_DEPTH + 2];
    let mut depth = 0usize;
    fdt.walk(|ev| {
        match ev {
            Event::Enter(n) => {
                depth = n.depth;
                cells[depth] = (2, 1);
                cur[depth] = Cur::default();
                name_at[depth] = n.name.split(|&c| c == b'@').next().unwrap_or(n.name);
            }
            Event::Prop(p) => match p.name {
                b"#address-cells" => cells[depth].0 = p.u32().unwrap_or(2) as usize,
                b"#size-cells" => cells[depth].1 = p.u32().unwrap_or(1) as usize,
                b"reg" => cur[depth].reg = Some(p.value),
                b"compatible" => cur[depth].compatible = Some(p),
                b"method" => cur[depth].method = p.str(),
                b"capacity-dmips-mhz" => cur[depth].capacity = p.u32().unwrap_or(0),
                b"status" => cur[depth].disabled = p.str().is_some_and(|s| s != b"okay" && s != b"ok"),
                _ => {}
            },
            Event::Leave(n) => {
                let d = n.depth;
                let c = cur[d];
                let (ac, sc) = cells[d - 1];
                let reg = c.reg.map(|v| Prop { name: b"reg", value: v });
                let compat = |s: &[u8]| c.compatible.is_some_and(|p| p.has_string(s));
                // (a pair of (address, size) at `i`-th entry of `reg`)
                let entry = |i: usize| -> Option<(u64, u64)> {
                    let r = reg?;
                    Some((r.num(i * (ac + sc), ac)?, r.num(i * (ac + sc) + ac, sc)?))
                };
                if d == 3 && name_at[d] == b"cpu" && name_at[2] == b"cpus" {
                    let ac = cells[2].0;
                    if let (Some(r), false) = (reg, c.disabled) {
                        if let Some(v) = r.num(0, ac) {
                            if m.ncpus < MAX_CORES {
                                m.cpus[m.ncpus] = v & 0xFF_00FF_FFFF; // Aff3 Aff2 Aff1 Aff0
                                m.capacity[m.ncpus] = c.capacity;
                                m.ncpus += 1;
                            }
                        }
                    }
                } else if d == 2 && name_at[d] == b"memory" && m.ram.is_none() {
                    m.ram = entry(0).filter(|&(_, size)| size != 0);
                } else if d == 2 && name_at[d] == b"psci" {
                    m.conduit = match c.method {
                        Some(b"hvc") => Some(Conduit::Hvc),
                        Some(b"smc") => Some(Conduit::Smc),
                        _ => None,
                    };
                } else if m.gic.is_none() && compat(b"arm,gic-v3") {
                    if let (Some((dist, _)), Some((redist, redist_size))) = (entry(0), entry(1)) {
                        m.gic = Some(Gic::V3 { dist, redist, redist_size });
                    }
                } else if m.gic.is_none() && (compat(b"arm,cortex-a15-gic") || compat(b"arm,gic-400") || compat(b"arm,cortex-a9-gic")) {
                    if let (Some((dist, _)), Some((cpu, _))) = (entry(0), entry(1)) {
                        m.gic = Some(Gic::V2 { dist, cpu });
                    }
                } else if m.uart.is_none() && compat(b"arm,pl011") {
                    m.uart = entry(0).map(|(a, _)| a);
                }
                depth = d - 1;
            }
        }
        true
    });
    m
}

/// The first `/memory` region, read with nothing but single-byte volatile loads.
///
/// For the code that runs before the MMU is on: with it off every access is Device memory, where a load that is not aligned to its own size
/// faults, and the compiler may turn an ordinary byte loop or slice compare into a call to `memcmp` or a wide load. Here every load is a
/// `read_volatile` of a `u8`, each loop is its own (volatile keeps it from being recognised as `memcmp`), and nothing is copied.
///
/// # Safety
/// `blob` must point to readable memory holding a device tree; the blob's own length fields are checked against `limit` (the most bytes that may be read from `blob`).
pub unsafe fn early_memory(blob: *const u8, limit: usize) -> Option<(u64, u64)> {
    let b = |o: usize| -> Option<u8> {
        if o >= limit {
            return None;
        }
        // SAFETY: `o < limit`, and the caller promises `limit` bytes at `blob` are readable.
        Some(unsafe { core::ptr::read_volatile(blob.add(o)) })
    };
    let w = |o: usize| -> Option<u32> { Some((b(o)? as u32) << 24 | (b(o + 1)? as u32) << 16 | (b(o + 2)? as u32) << 8 | b(o + 3)? as u32) };
    if w(0)? != MAGIC {
        return None;
    }
    let total = (w(4)? as usize).min(limit);
    let (off_struct, off_strings, size_struct) = (w(8)? as usize, w(12)? as usize, w(36)? as usize);
    if off_struct.checked_add(size_struct)? > total || off_strings > total {
        return None;
    }
    let (mut o, end) = (off_struct, off_struct + size_struct);
    let mut depth = 0usize;
    let mut in_memory = false;
    let (mut ac, mut sc) = (2u32, 1u32); // the root's cells (`reg` of /memory uses them)
    while o + 4 <= end {
        let tag = w(o)?;
        o += 4;
        match tag {
            BEGIN_NODE => {
                // name: is it "memory" followed by NUL or '@' (depth 2 = a child of the root)?
                let want = b"memory";
                let mut k = 0;
                let mut matches = true;
                loop {
                    let c = b(o + k)?;
                    if c == 0 {
                        break;
                    }
                    if k < want.len() {
                        matches &= c == want[k];
                    } else if k == want.len() {
                        matches &= c == b'@';
                    }
                    k += 1;
                    if o + k >= end {
                        return None;
                    }
                }
                if k < want.len() {
                    matches = false;
                }
                depth += 1;
                in_memory = depth == 2 && matches;
                o = (o + k + 1 + 3) & !3;
            }
            END_NODE => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                in_memory = false;
            }
            PROP => {
                let (len, nameoff) = (w(o)? as usize, w(o + 4)? as usize);
                o += 8;
                let val = o;
                o = o.checked_add(len)?.checked_add(3)? & !3;
                if o > end {
                    return None;
                }
                let name = off_strings.checked_add(nameoff)?;
                let is = |s: &[u8]| -> Option<bool> {
                    for (i, &c) in s.iter().enumerate() {
                        if b(name + i)? != c {
                            return Some(false);
                        }
                    }
                    Some(b(name + s.len())? == 0)
                };
                if depth == 1 && is(b"#address-cells")? && len == 4 {
                    ac = w(val)?;
                } else if depth == 1 && is(b"#size-cells")? && len == 4 {
                    sc = w(val)?;
                } else if in_memory && is(b"reg")? {
                    let cells = |at: usize, n: u32| -> Option<u64> {
                        match n {
                            1 => Some(w(at)? as u64),
                            2 => Some((w(at)? as u64) << 32 | w(at + 4)? as u64),
                            _ => None,
                        }
                    };
                    let base = cells(val, ac)?;
                    let size = cells(val + 4 * ac as usize, sc)?;
                    return (size != 0).then_some((base, size));
                }
            }
            NOP => {}
            _ => return None,
        }
    }
    None
}
