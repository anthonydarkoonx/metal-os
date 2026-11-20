//! The program loader. The kernel does not read executable formats: `SPAWN` creates an empty task (an *embryo*) with
//! its stack, arguments and capabilities, `TASK_LOAD` puts fresh pages with given contents and permissions into it,
//! and `TASK_START` makes it runnable. Everything about *formats* lives here, in ordinary user code.
//!
//! Three formats:
//!  * the flat image of `programs/link.ld` (32-byte header: magic, size of the read-execute part, total memory size, 8 bytes
//!    of name; entry right after the header), and
//!  * a statically linked AArch64 ELF executable (`ET_EXEC`) whose `PT_LOAD` segments lie between the image base and the
//!    stack. Segments that share a page get the union of their permissions. `PT_INTERP`, `PT_DYNAMIC` and
//!    `PT_DYNAMIC` are refused;
//!  * a position-independent AArch64 ELF executable (`ET_DYN`, linked at address 0, `-pie`): the kernel picks a random base for
//!    the image (`FLAG_PIE`), and the loader applies the `R_AARCH64_RELATIVE` relocations of its `DT_RELA` table to the pages
//!    it loads. Any other relocation, `DT_NEEDED` or a PLT is refused: there is no dynamic linking, only relocation.
//!
//! Either way a page may not be writable and executable at once (the kernel refuses such a page too: `TASK_LOAD` has no
//! way to ask for one).
#![cfg_attr(not(test), forbid(unsafe_code))]
#![no_std]

pub const PAGE: usize = 4096;
/// Where programs are loaded, and the end of the image area (the stack starts there).
pub const USER_BASE: usize = 0x8000_0000;
pub const MAX_IMAGE: usize = 0x8010_0000 - 16 * PAGE - USER_BASE;
/// A position-independent image may be larger (it is not squeezed in below the first mappings): 8 MiB.
pub const MAX_PIE_IMAGE: usize = 8 << 20;
const IMAGE_PAGES: usize = MAX_PIE_IMAGE / PAGE;
pub const MAX_SEGS: usize = 16;
/// Largest file a program may be (a file can hold more than ends up in memory, e.g. debug information).
/// (an unsigned package is at most `metallium_map::MAX_PACKAGE_UNSIGNED` = 4 MiB, so only a program a trusted key vouched for can have a file this big)
pub const MAX_FILE: usize = 8 << 20;

pub const E_INVAL: i64 = -5;
pub const E_NOMEM: i64 = -8;

pub const X: u8 = 1;
pub const W: u8 = 2;
pub const R: u8 = 4;
const PRESENT: u8 = 8;

#[derive(Clone, Copy, Debug)]
pub struct Seg {
    pub va: usize,
    /// Offset of the segment's bytes in the image.
    pub off: usize,
    pub filesz: usize,
    pub memsz: usize,
    pub flags: u8,
}

#[derive(Debug)]
pub struct Plan {
    pub segs: [Seg; MAX_SEGS],
    pub nsegs: usize,
    /// Per page from the image base: `PRESENT` and the union of the flags of the segments on it.
    pages: [u8; IMAGE_PAGES],
    /// The entry point as linked (add `bias`; `entry_addr` does).
    pub entry: usize,
    pub name: [u8; 8],
    /// Address of the first page as linked: `USER_BASE`, or 0 for a position-independent image.
    lo: usize,
    /// Where the image really goes minus `lo` (0 unless `rebase` was called).
    bias: usize,
    pie: bool,
    /// The largest image of this kind (`MAX_IMAGE` or `MAX_PIE_IMAGE`).
    max: usize,
    /// File offset and count of the `R_AARCH64_RELATIVE` entries (24 bytes each), checked by `plan`.
    rela_off: usize,
    rela_n: usize,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Error {
    /// Not an image we can load (wrong format, a segment outside the program area, writable+executable, ...).
    Format,
}

fn le16(b: &[u8], o: usize) -> usize {
    u16::from_le_bytes([b[o], b[o + 1]]) as usize
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> usize {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) as usize
}

/// Read and check an image.
pub fn plan(image: &[u8]) -> Result<Plan, Error> {
    let len = image.len();
    let mut plan = Plan { segs: [Seg { va: 0, off: 0, filesz: 0, memsz: 0, flags: 0 }; MAX_SEGS], nsegs: 0, pages: [0; IMAGE_PAGES], entry: 0, name: [0; 8], lo: USER_BASE, bias: 0, pie: false, max: MAX_IMAGE, rela_off: 0, rela_n: 0 };
    if len < 32 || len > MAX_FILE {
        return Err(Error::Format);
    }
    if image[..4] == [0x7F, b'E', b'L', b'F'] {
        load_elf(image, &mut plan)?;
    } else {
        let (code_end, mem_size) = (le64(image, 8), le64(image, 16));
        if le64(image, 0) != 0x4D4F_5355 || code_end < PAGE || code_end % PAGE != 0 || mem_size % PAGE != 0 || code_end > mem_size || mem_size > MAX_IMAGE || len > mem_size {
            return Err(Error::Format);
        }
        plan.segs[0] = Seg { va: USER_BASE, off: 0, filesz: len.min(code_end), memsz: code_end, flags: X | R };
        plan.nsegs = 1;
        if mem_size > code_end {
            plan.segs[1] = Seg { va: USER_BASE + code_end, off: code_end, filesz: len.saturating_sub(code_end), memsz: mem_size - code_end, flags: W | R };
            plan.nsegs = 2;
        }
        plan.entry = USER_BASE + 0x20;
        plan.name.copy_from_slice(&image[24..32]);
    }
    for sg in plan.segs[..plan.nsegs].iter() {
        let first = (sg.va - plan.lo) / PAGE;
        let last = (sg.va + sg.memsz - 1 - plan.lo) / PAGE;
        for pg in first..=last {
            plan.pages[pg] |= PRESENT | sg.flags;
        }
    }
    if plan.pages.iter().any(|&f| f & W != 0 && f & X != 0) {
        return Err(Error::Format);
    }
    let e = plan.entry;
    if e % 4 != 0 || e < plan.lo || e >= plan.lo + plan.max || plan.pages[(e - plan.lo) / PAGE] & X == 0 {
        return Err(Error::Format);
    }
    Ok(plan)
}

fn load_elf(h: &[u8], plan: &mut Plan) -> Result<(), Error> {
    let len = h.len();
    // class 64-bit (2), little endian (1), version 1; ET_EXEC (2) or ET_DYN (3); EM_AARCH64 (183); 56-byte program headers
    if len < 64 || h[4] != 2 || h[5] != 1 || h[6] != 1 || (le16(h, 16) != 2 && le16(h, 16) != 3) || le16(h, 18) != 183 || le16(h, 54) != 56 {
        return Err(Error::Format);
    }
    let (entry, phoff, phnum) = (le64(h, 24), le64(h, 32), le16(h, 56));
    if phnum == 0 || phnum > 16 || phoff.checked_add(phnum * 56).map_or(true, |e| e > len) {
        return Err(Error::Format);
    }
    let pie = le16(h, 16) == 3;
    let lo = if pie { 0 } else { USER_BASE };
    plan.lo = lo;
    plan.pie = pie;
    plan.max = if pie { MAX_PIE_IMAGE } else { MAX_IMAGE };
    let limit = lo + plan.max;
    let mut dynamic = None;
    for i in 0..phnum {
        let p = &h[phoff + i * 56..phoff + (i + 1) * 56];
        let (ty, fl) = (le32(p, 0), le32(p, 4));
        let (off, va, filesz, memsz) = (le64(p, 8), le64(p, 16), le64(p, 32), le64(p, 40));
        match ty {
            1 => {
                // PT_LOAD
                if memsz == 0 {
                    continue;
                }
                if plan.nsegs == MAX_SEGS || filesz > memsz || va < lo || va.checked_add(memsz).map_or(true, |e| e > limit) || off.checked_add(filesz).map_or(true, |e| e > len) {
                    return Err(Error::Format);
                }
                let flags = (if fl & 1 != 0 { X } else { 0 }) | (if fl & 2 != 0 { W } else { 0 }) | R;
                plan.segs[plan.nsegs] = Seg { va, off, filesz, memsz, flags };
                plan.nsegs += 1;
            }
            2 if pie && dynamic.is_none() && off.checked_add(filesz).map_or(false, |e| e <= len) => dynamic = Some((off, filesz)),
            3 | 2 => return Err(Error::Format), // PT_INTERP; PT_DYNAMIC of a fixed executable (or a second one): no dynamic linking
            _ => {}
        }
    }
    if plan.nsegs == 0 {
        return Err(Error::Format);
    }
    plan.name[..3].copy_from_slice(b"elf");
    if pie {
        read_relocations(h, plan, dynamic)?;
    } else if dynamic.is_some() {
        return Err(Error::Format);
    }
    plan.entry = entry;
    Ok(())
}

const DT_NULL: usize = 0;
const DT_NEEDED: usize = 1;
const DT_PLTRELSZ: usize = 2;
const DT_STRTAB: usize = 5;
const DT_RELA: usize = 7;
const DT_RELASZ: usize = 8;
const DT_RELAENT: usize = 9;
const DT_SONAME: usize = 14;
const DT_REL: usize = 17;
const DT_RELSZ: usize = 18;
const DT_JMPREL: usize = 23;
const DT_RELR: usize = 36;
const R_AARCH64_RELATIVE: usize = 1027;

/// Find and check the relocation table of a position-independent image: only `R_AARCH64_RELATIVE` entries, each patching
/// 8 aligned bytes that the file provides.
fn read_relocations(h: &[u8], plan: &mut Plan, dynamic: Option<(usize, usize)>) -> Result<(), Error> {
    let Some((doff, dsz)) = dynamic else { return Ok(()) };
    let (mut rela, mut relasz, mut relaent, mut strtab, mut soname) = (None, 0, 24, None, None);
    for e in h[doff..doff + dsz].chunks_exact(16) {
        let (tag, val) = (le64(e, 0), le64(e, 8));
        match tag {
            DT_NULL => break,
            DT_RELA => rela = Some(val),
            DT_RELASZ => relasz = val,
            DT_RELAENT => relaent = val,
            DT_STRTAB => strtab = Some(val),
            DT_SONAME => soname = Some(val),
            DT_NEEDED | DT_REL | DT_RELSZ | DT_RELR => return Err(Error::Format),
            DT_JMPREL | DT_PLTRELSZ if val != 0 => return Err(Error::Format),
            _ => {}
        }
    }
    // the program's name, if the linker was given one (`-soname`): the first 8 bytes of the string
    if let (Some(tab), Some(n)) = (strtab, soname) {
        if let Some(sg) = plan.segs[..plan.nsegs].iter().find(|s| tab >= s.va && tab - s.va < s.filesz) {
            let from = sg.off + (tab - sg.va) + n;
            if n < sg.filesz - (tab - sg.va) {
                let end = (sg.off + sg.filesz).min(h.len());
                let mut name = [0u8; 8];
                for (d, &b) in name.iter_mut().zip(h[from..end].iter().take_while(|&&b| b != 0)) {
                    *d = b;
                }
                plan.name = name;
            }
        }
    }
    let Some(rva) = rela else { return if relasz == 0 { Ok(()) } else { Err(Error::Format) } };
    if relaent != 24 || relasz % 24 != 0 {
        return Err(Error::Format);
    }
    // the table's address is a virtual one: find the segment that holds it
    let Some(sg) = plan.segs[..plan.nsegs].iter().find(|s| rva >= s.va && rva - s.va < s.filesz) else { return Err(Error::Format) };
    let off = sg.off + (rva - sg.va);
    if relasz > sg.filesz - (rva - sg.va) || off.checked_add(relasz).map_or(true, |e| e > h.len()) {
        return Err(Error::Format);
    }
    let mut last = 0;
    for r in h[off..off + relasz].chunks_exact(24) {
        let (at, info) = (le64(r, 0), le64(r, 8));
        if at < last {
            return Err(Error::Format); // (the table must be in address order: `pages` searches it)
        }
        last = at;
        let ok = info == R_AARCH64_RELATIVE && at % 8 == 0 && plan.segs[..plan.nsegs].iter().any(|s| at >= s.va && at - s.va + 8 <= s.filesz);
        if !ok {
            return Err(Error::Format);
        }
    }
    plan.rela_off = off;
    plan.rela_n = relasz / 24;
    Ok(())
}

impl Plan {
    /// Is this a position-independent image (the kernel must choose its base: spawn with `FLAG_PIE`)?
    pub fn is_pie(&self) -> bool {
        self.pie
    }

    /// Put the image at `base` (a page-aligned address; only for position-independent images).
    pub fn rebase(&mut self, base: usize) {
        self.bias = base - self.lo;
    }

    /// Where execution starts, once rebased.
    pub fn entry_addr(&self) -> usize {
        self.entry + self.bias
    }

    /// Every page of the program, in order: (address, contents, permission flags `X`/`W`/`R`).
    pub fn pages<'a>(&'a self, image: &'a [u8]) -> impl Iterator<Item = (usize, [u8; PAGE], u8)> + 'a {
        self.pages.iter().enumerate().filter(|(_, &f)| f & PRESENT != 0).map(move |(idx, &f)| {
            let va = self.lo + idx * PAGE; // (as linked)
            let mut page = [0u8; PAGE];
            // every segment that touches this page puts its file bytes in (segments may share a page)
            for sg in self.segs[..self.nsegs].iter() {
                let (lo, hi) = (va.max(sg.va), (va + PAGE).min(sg.va + sg.filesz));
                if lo < hi {
                    let from = sg.off + (lo - sg.va);
                    page[lo - va..hi - va].copy_from_slice(&image[from..from + (hi - lo)]);
                }
            }
            // a position-independent image: add its base to the places that hold its own addresses
            let table = &image[self.rela_off..self.rela_off + 24 * self.rela_n];
            let (mut lo, mut hi) = (0, self.rela_n);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if le64(table, 24 * mid) < va {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            for r in table[24 * lo..].chunks_exact(24).take_while(|r| le64(r, 0) < va + PAGE) {
                let at = le64(r, 0);
                page[at - va..at - va + 8].copy_from_slice(&((le64(r, 16) + self.bias) as u64).to_le_bytes());
            }
            (va + self.bias, page, f & (X | W | R))
        })
    }
}

// ---- starting a program ------------------------------------------------------------------------------------------------

/// A system call: `(id, a1, a2, a3, a4) -> x0`.
pub type Syscall = fn(u64, u64, u64, u64, u64) -> u64;

const SPAWN: u64 = 24;
const WAIT: u64 = 25;
const KILL: u64 = 26;
const TASK_LOAD: u64 = 28;
const TASK_START: u64 = 29;
const TASK_BASE: u64 = 39;
/// Spawn flag: the program is position-independent, the kernel chooses where its image goes.
const FLAG_PIE: u32 = 8;
/// Version of the request block `SPAWN` takes (creates an embryo; bytes 12..24, the old image fields, must be zero; 8..12 is the program's memory budget).
pub const REQ_VERSION: u32 = 2;
pub const REQ_SIZE: usize = 96;

/// Create and start a program from `image`. `request` is the 96-byte block described in docs/ABI.md (version and the
/// image fields are filled in here; a zero name takes the image's). Returns the task id, or a negative error.
pub fn spawn(sys: Syscall, spawn_slot: u64, request: &mut [u8; REQ_SIZE], image: &[u8]) -> i64 {
    spawn_with(sys, spawn_slot, request, image, &mut |_| 0)
}

/// Like `spawn`, but calls `before_start(task id)` once the program is completely loaded and *before it runs its first
/// instruction*: a parent that has to prepare something for the new program (confine its file access, say) does it there,
/// with no window in which the program runs unprepared. A negative result cancels the start: the embryo is condemned and
/// collected and `spawn_with` returns that error.
pub fn spawn_with(sys: Syscall, spawn_slot: u64, request: &mut [u8; REQ_SIZE], image: &[u8], before_start: &mut dyn FnMut(u64) -> i64) -> i64 {
    let Ok(mut plan) = plan(image) else { return E_INVAL };
    if plan.is_pie() {
        request[4] |= FLAG_PIE as u8;
    }
    request[0..4].copy_from_slice(&REQ_VERSION.to_le_bytes());
    request[12..24].fill(0); // (8..12 is the program's budget: the caller's)
    if request[56..64].iter().all(|&b| b == 0) {
        request[56..64].copy_from_slice(&plan.name);
    }
    let tid = sys(SPAWN, spawn_slot, request.as_ptr() as u64, 0, 0) as i64;
    if tid < 0 {
        return tid;
    }
    let fail = |e: i64| -> i64 {
        // an embryo that is not started is condemned and collected, so its slot and memory come back
        sys(KILL, tid as u64, 0, 0, 0);
        sys(WAIT, tid as u64, 0, 0, 0);
        e
    };
    if plan.is_pie() {
        let base = sys(TASK_BASE, tid as u64, 0, 0, 0) as i64;
        if base <= 0 {
            return fail(E_INVAL);
        }
        plan.rebase(base as usize);
    }
    for (va, page, flags) in plan.pages(image) {
        let perm = if flags & X != 0 {
            0
        } else if flags & W != 0 {
            1
        } else {
            2
        };
        let mut lr = [0u8; 24];
        lr[0..8].copy_from_slice(&(va as u64).to_le_bytes());
        lr[8..16].copy_from_slice(&(page.as_ptr() as u64).to_le_bytes());
        lr[16..20].copy_from_slice(&1u32.to_le_bytes());
        lr[20..24].copy_from_slice(&(perm as u32).to_le_bytes());
        let r = sys(TASK_LOAD, tid as u64, lr.as_ptr() as u64, 0, 0) as i64;
        if r < 0 {
            return fail(r);
        }
    }
    let r = before_start(tid as u64);
    if r < 0 {
        return fail(r);
    }
    let r = sys(TASK_START, tid as u64, plan.entry_addr() as u64, 0, 0) as i64;
    if r < 0 {
        return fail(r);
    }
    tid
}

#[cfg(test)]
mod tests;
