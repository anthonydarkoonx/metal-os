//! Arm SMMUv3 driver: confines each DMA-capable device to the pages its driver was granted.
//!
//! Written from the Arm SMMUv3 architecture specification (IHI 0070): register map, stream
//! table entry, command and event queue formats, stage-2 translation. A device is identified
//! by its stream id (its PCI requester id). Each attached device gets a *stage-2* translation
//! with its own table, which maps exactly the granted pages and nothing else; a stream with no
//! valid entry is aborted. Addresses are identity-mapped (the driver programs physical
//! addresses, as it learned them through `CAP_INFO`), so the SMMU limits *where* a device can
//! reach, not what address it uses.
//!
//! When the device touches anything else the SMMU raises an event; the interrupt handler logs
//! it and condemns the owning driver, which the scheduler then restarts (sched::doom).
use crate::sync::{SpinLock, R_SMMU};
use crate::{gic, kprintln, mem, obj, task};
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

const BASE: usize = 0x0905_0000;
/// The event queue's wired interrupt (SPI 74 on QEMU `virt`, from its device tree).
pub const EVENT_IRQ: u32 = 32 + 74;

// register offsets (page 0)
const IDR0: usize = 0x00;
const IDR1: usize = 0x04;
const IDR5: usize = 0x14;
const CR0: usize = 0x20;
const CR0ACK: usize = 0x24;
const CR1: usize = 0x28;
const IRQ_CTRL: usize = 0x50;
const IRQ_CTRLACK: usize = 0x54;
const GERROR: usize = 0x60;
const GERRORN: usize = 0x64;
const STRTAB_BASE: usize = 0x80;
const STRTAB_BASE_CFG: usize = 0x88;
const CMDQ_BASE: usize = 0x90;
const CMDQ_PROD: usize = 0x98;
const CMDQ_CONS: usize = 0x9c;
const EVTQ_BASE: usize = 0xa0;
// The event queue's PROD and CONS live in register page 1 (the page 0 locations are only an
// optional alias, RAZ/WI on implementations without it).
const PAGE1: usize = 0x1_0000;
const EVTQ_PROD: usize = PAGE1 + 0xa8;
const EVTQ_CONS: usize = PAGE1 + 0xac;

const CR0_SMMUEN: u32 = 1 << 0;
const CR0_EVTQEN: u32 = 1 << 2;
const CR0_CMDQEN: u32 = 1 << 3;
const IRQ_GERROR: u32 = 1 << 0;
const IRQ_EVTQ: u32 = 1 << 2;

const STREAMS_LOG2: u32 = 6; // 64 stream ids: bus 0, devices 0..7 of function 0..7
const NSTREAMS: usize = 1 << STREAMS_LOG2;
const STE_BYTES: usize = 64;
const CMDQ_LOG2: u32 = 4; // 16 commands (we synchronise after every batch)
const EVTQ_LOG2: u32 = 7; // 128 events
const MAX_TABLES: usize = 12;

fn r32(off: usize) -> u32 {
    // SAFETY: `BASE + off` is an SMMUv3 register (4-/8-aligned by the register map) in the device block (l1[0]); volatile MMIO.
    unsafe { read_volatile((BASE + off) as *const u32) }
}
fn w32(off: usize, v: u32) {
    // SAFETY: `BASE + off` is an SMMUv3 register (4-/8-aligned by the register map) in the device block (l1[0]); volatile MMIO.
    unsafe { write_volatile((BASE + off) as *mut u32, v) }
}
fn w64(off: usize, v: u64) {
    // SAFETY: `BASE + off` is an SMMUv3 register (4-/8-aligned by the register map) in the device block (l1[0]); volatile MMIO.
    unsafe { write_volatile((BASE + off) as *mut u64, v) }
}
fn dsb() {
    // SAFETY: barrier only; orders the queue/table writes before the register write that follows.
    unsafe { asm!("dsb sy", options(nostack)) };
}

#[derive(Clone, Copy)]
struct Stream {
    attached: bool,
    /// The SMMU has already stopped this device after a fault.
    cut_off: bool,
    /// Role of the task that owns the device; it is the one restarted on a fault.
    role: u8,
    l1: usize,
    tables: [usize; MAX_TABLES],
    ntables: usize,
}

struct Smmu {
    ready: bool,
    /// IDR0.COHACC: the SMMU's own accesses to tables and queues snoop the CPU caches
    coherent: bool,
    strtab: usize,
    cmdq: usize,
    evtq: usize,
    cmd_prod: u32,
    streams: [Stream; NSTREAMS],
}

static SMMU: SpinLock<Smmu> = SpinLock::new(Smmu {
    ready: false,
    coherent: true,
    strtab: 0,
    cmdq: 0,
    evtq: 0,
    cmd_prod: 0,
    streams: [Stream { attached: false, cut_off: false, role: 0, l1: 0, tables: [0; MAX_TABLES], ntables: 0 }; NSTREAMS],
});

impl Smmu {
    /// Queue one command and wait until the SMMU has consumed it.
    fn command(&mut self, dw0: u64, dw1: u64) {
        let idx = (self.cmd_prod & ((1 << CMDQ_LOG2) - 1)) as usize;
        let slot = (self.cmdq + 16 * idx) as *mut u64;
        // SAFETY: `slot` is entry `prod & 15` of the command queue (16 entries x 16 B in the one page `init` allocated, 8-aligned) and the
        // SMMU lock is held, so nothing else writes it.
        unsafe {
            write_volatile(slot, dw0);
            write_volatile(slot.add(1), dw1);
        }
        crate::cache::clean(slot as usize, 16); // the SMMU reads the command from memory
        self.cmd_prod = (self.cmd_prod + 1) & ((2 << CMDQ_LOG2) - 1);
        dsb();
        w32(CMDQ_PROD, self.cmd_prod);
    }

    /// CMD_SYNC, then wait for the whole batch to complete. False if the SMMU never got there.
    fn sync(&mut self) -> bool {
        self.command(0x46, 0);
        for _ in 0..2_000_000 {
            if r32(CMDQ_CONS) & ((2 << CMDQ_LOG2) - 1) == self.cmd_prod {
                return true;
            }
        }
        kprintln!("[smmu] command queue stuck (cons {:#x}, prod {:#x}, gerror {:#x})", r32(CMDQ_CONS), self.cmd_prod, r32(GERROR));
        false
    }

    fn ste(&self, sid: usize) -> *mut u64 {
        (self.strtab + sid * STE_BYTES) as *mut u64
    }

    /// Make the SMMU re-read stream `sid`'s table entry and forget cached translations.
    fn invalidate(&mut self, sid: usize) -> bool {
        self.command(0x03 | (sid as u64) << 32, 1); // CFGI_STE, leaf
        self.command(0x30, 0); // TLBI_NSNH_ALL
        self.sync()
    }

    /// Abort the stream and drop its translation tables. If the SMMU cannot confirm it has forgotten
    /// the entry, the tables are *leaked* rather than freed: it might still walk them.
    fn tear_down(&mut self, sid: usize) {
        // SAFETY: `ste(sid)` is inside the stream-table page (64 entries x 64 B = one page, allocated in `init`); callers have `sid < NSTREAMS`; SMMU lock held.
        unsafe { write_volatile(self.ste(sid), 0) };
        crate::cache::clean(self.ste(sid) as usize, STE_BYTES);
        dsb();
        if self.invalidate(sid) {
            self.free_tables(sid);
        } else {
            kprintln!("[smmu] stream {sid:#x}: invalidation not confirmed, its translation tables are leaked");
            self.streams[sid] = Stream { attached: false, cut_off: true, role: 0, l1: 0, tables: [0; MAX_TABLES], ntables: 0 };
        }
    }

    fn free_tables(&mut self, sid: usize) {
        let s = &mut self.streams[sid];
        for &t in &s.tables[..s.ntables] {
            mem::free(t);
        }
        *s = Stream { attached: false, cut_off: false, role: 0, l1: 0, tables: [0; MAX_TABLES], ntables: 0 };
    }

    /// Map one 4 KiB page (identity, read/write) into stream `sid`'s stage-2 tables.
    fn map(&mut self, sid: usize, pa: usize) -> Result<(), ()> {
        let mut table = self.streams[sid].l1;
        for level in [1, 2] {
            let idx = (pa >> (12 + 9 * (3 - level))) & 511;
            let e = (table + 8 * idx) as *mut u64;
            // SAFETY: `e` is entry `idx` of a table page this stream owns (`l1`, or one from `mem::alloc` listed in `tables`); idx < 512; SMMU lock held.
            let mut v = unsafe { read_volatile(e) };
            if v & 1 == 0 {
                let s = &mut self.streams[sid];
                if s.ntables == MAX_TABLES {
                    return Err(());
                }
                let t = mem::alloc(mem::OWNER_KERNEL).ok_or(())?;
                crate::cache::clean(t, mem::PAGE); // the SMMU walks it: its zeroes must be in memory
                s.tables[s.ntables] = t;
                s.ntables += 1;
                v = t as u64 | 0b11; // table descriptor
                // SAFETY: as the read above: `e` is a slot of a table page this stream owns; SMMU lock held.
                unsafe { write_volatile(e, v) };
                crate::cache::clean(e as usize, 8);
            }
            table = (v & 0x0000_ffff_ffff_f000) as usize;
        }
        // level-3 page: normal write-back memory, read/write, inner shareable, access flag set
        const PAGE: u64 = 0b11 | 0b1111 << 2 | 0b11 << 6 | 0b11 << 8 | 1 << 10;
        let leaf = table + 8 * ((pa >> 12) & 511);
        // SAFETY: `leaf` is a slot of the stream's level-3 table (`table` came from a descriptor this function wrote, idx < 512); SMMU lock held.
        unsafe { write_volatile(leaf as *mut u64, pa as u64 | PAGE) };
        crate::cache::clean(leaf, 8);
        Ok(())
    }
}

/// (translation enabled, number of devices currently confined to their driver's pages)
pub fn summary() -> (bool, usize) {
    let s = SMMU.lock(R_SMMU);
    (s.ready, s.streams.iter().filter(|st| st.attached).count())
}

/// Bring the SMMU up with an empty stream table: every device is aborted until attached.
pub fn init() {
    let idr0 = r32(IDR0);
    if idr0 & (1 << 0) == 0 {
        kprintln!("[smmu] no stage-2 support (IDR0={idr0:#x}): devices are not confined");
        return;
    }
    let idr1 = r32(IDR1);
    if (idr1 & 0x3f) < STREAMS_LOG2 {
        kprintln!("[smmu] too few stream id bits (IDR1={idr1:#x})");
        return;
    }
    let (strtab, cmdq, evtq) = (mem::alloc(mem::OWNER_KERNEL), mem::alloc(mem::OWNER_KERNEL), mem::alloc(mem::OWNER_KERNEL));
    let (Some(strtab), Some(cmdq), Some(evtq)) = (strtab, cmdq, evtq) else {
        kprintln!("[smmu] out of memory");
        return;
    };
    let mut s = SMMU.lock(R_SMMU);
    (s.strtab, s.cmdq, s.evtq) = (strtab, cmdq, evtq);
    for page in [strtab, cmdq, evtq] {
        crate::cache::clean(page, mem::PAGE); // their zeroes must be in memory before the SMMU reads them
    }

    w32(CR0, 0); // disabled while configuring
    while r32(CR0ACK) != 0 {}
    // Table and queue memory attributes. A coherent SMMU (IDR0.COHACC) snoops the caches: write-back, inner
    // shareable. One that is not accesses memory as non-cacheable and non-shareable, whatever is programmed (the
    // architecture requires these fields to be 0 then), and only the cache maintenance below keeps it in step.
    // (the harness can pretend the SMMU is not coherent, to exercise the cache-maintenance path under QEMU)
    let coherent = idr0 & (1 << 4) != 0 && !crate::platform::flag(b"opt/metal/dma-noncoherent");
    s.coherent = coherent;
    w32(CR1, if coherent { 0b11 << 10 | 0b01 << 8 | 0b01 << 6 | 0b11 << 4 | 0b01 << 2 | 0b01 } else { 0 });
    w64(STRTAB_BASE, strtab as u64 | 1 << 62); // read-allocate hint
    w32(STRTAB_BASE_CFG, STREAMS_LOG2); // linear table
    w64(CMDQ_BASE, cmdq as u64 | CMDQ_LOG2 as u64);
    w32(CMDQ_PROD, 0);
    w32(CMDQ_CONS, 0);
    w64(EVTQ_BASE, evtq as u64 | EVTQ_LOG2 as u64);
    w32(EVTQ_PROD, 0);
    w32(EVTQ_CONS, 0);

    w32(CR0, CR0_CMDQEN | CR0_EVTQEN);
    while r32(CR0ACK) != CR0_CMDQEN | CR0_EVTQEN {}
    s.command(0x04, 31); // CFGI_ALL
    s.command(0x30, 0); // TLBI_NSNH_ALL
    s.sync();

    gic::configure_spi(EVENT_IRQ);
    gic::set_edge(EVENT_IRQ); // the SMMU pulses its wired interrupt
    // A fault must be seen before any device completion that might be pending with it: otherwise a
    // driver whose request "completed" (with the data it could not read replaced by zeros) is woken
    // and runs before the kernel has condemned it.
    gic::set_urgent(EVENT_IRQ);
    gic::enable(EVENT_IRQ);
    w32(IRQ_CTRL, IRQ_EVTQ | IRQ_GERROR);
    while r32(IRQ_CTRLACK) != IRQ_EVTQ | IRQ_GERROR {}

    w32(CR0, CR0_CMDQEN | CR0_EVTQEN | CR0_SMMUEN);
    while r32(CR0ACK) != CR0_CMDQEN | CR0_EVTQEN | CR0_SMMUEN {}
    s.ready = true;
    kprintln!(
        "[smmu] SMMUv3 enabled (IDR0={idr0:#x}, {} stream ids, output {} bits, {}); unattached devices are aborted",
        NSTREAMS,
        [32, 36, 40, 42, 44, 48, 52, 0][(r32(IDR5) & 7) as usize],
        if coherent { "coherent table walks" } else { "not coherent: tables and queues non-cacheable, caches maintained" }
    );
}

/// Allow stream `sid` (owned by the task with `role`) to reach exactly the pages of `objs`.
pub fn attach(sid: u16, role: u8, objs: &[u8]) {
    let sid = sid as usize;
    // objects first: the OBJ lock ranks below the SMMU lock
    let mut regions = [(0usize, 0usize); 8];
    let mut n = 0;
    for &o in objs {
        if let Some(m) = obj::get(o) {
            regions[n] = (m.pa, m.pages);
            n += 1;
        }
    }
    let mut s = SMMU.lock(R_SMMU);
    if !s.ready {
        return;
    }
    if sid >= NSTREAMS {
        kprintln!("[smmu] stream {sid:#x} is beyond the {NSTREAMS}-entry stream table: the device stays aborted");
        return;
    }
    // Events already queued for *this* stream concern a previous instance and are dropped; those of
    // other streams are handled as usual (a device that faulted a moment ago must still be cut off).
    drain_events(&mut s, true, Some(sid));
    if s.streams[sid].attached {
        s.tear_down(sid); // never rewrite or free a table behind a valid entry
    }
    let Some(l1) = mem::alloc(mem::OWNER_KERNEL) else { return };
    s.streams[sid] = Stream { attached: true, cut_off: false, role, l1, tables: { let mut t = [0; MAX_TABLES]; t[0] = l1; t }, ntables: 1 };
    let mut pages = 0;
    for &(pa, np) in &regions[..n] {
        for p in 0..np {
            if s.map(sid, pa + p * mem::PAGE).is_err() {
                kprintln!("[smmu] stream {sid:#x}: out of translation table space");
                s.free_tables(sid);
                return;
            }
            pages += 1;
        }
    }
    // Stream table entry (STE), stage-2 only. Everything but the valid bit first.
    let ste = s.ste(sid);
    let vmid = sid as u64 + 1;
    let dw2 = vmid
        | 25 << 32            // S2T0SZ: 39-bit input addresses
        | 1 << 38             // S2SL0: walk starts at level 1
        | if s.coherent { 0b01 << 40 | 0b01 << 42 | 0b11 << 44 } else { 0 } // S2IR0 / S2OR0 write-back, S2SH0 inner shareable (non-cacheable and non-shareable if the SMMU is not coherent)
        | 0b00 << 46          // S2TG: 4 KiB granule
        | 0b010 << 48         // S2PS: 40-bit output
        | 1 << 51             // S2AA64
        | 1 << 58; // S2R: record faults
    // SAFETY: `ste` is entry `sid` of the stream-table page (`sid < NSTREAMS` checked in `attach`); the entry is still invalid (V=0) so
    // the SMMU does not read it while words 1..7 are written; SMMU lock held.
    unsafe {
        write_volatile(ste.add(1), 0);
        write_volatile(ste.add(2), dw2);
        write_volatile(ste.add(3), l1 as u64);
        for i in 4..8 {
            write_volatile(ste.add(i), 0);
        }
    }
    crate::cache::clean(ste as usize, STE_BYTES);
    dsb();
    // SAFETY: same entry as above, in bounds; this word-0 write is the one that makes it valid, after its other words were cleaned to memory.
    unsafe { write_volatile(ste, 1 | 0b110 << 1) }; // V, Config = stage 2 translate, stage 1 bypass
    crate::cache::clean(ste as usize, STE_BYTES);
    dsb();
    s.invalidate(sid);
    kprintln!("[smmu] stream {sid:#x} attached: {pages} pages reachable, everything else faults");
}

/// The device's driver is gone: abort the stream and drop its translation tables.
pub fn detach(sid: u16) {
    let sid = sid as usize;
    let mut s = SMMU.lock(R_SMMU);
    if !s.ready || sid >= NSTREAMS || !s.streams[sid].attached {
        return;
    }
    s.tear_down(sid);
}

fn event_name(id: u64) -> &'static str {
    match id {
        0x02 => "C_BAD_STREAMID",
        0x03 => "F_STE_FETCH",
        0x04 => "C_BAD_STE",
        0x06 => "F_STREAM_DISABLED",
        0x0b => "F_WALK_EABT",
        0x10 => "F_TRANSLATION",
        0x11 => "F_ADDR_SIZE",
        0x12 => "F_ACCESS",
        0x13 => "F_PERMISSION",
        _ => "event",
    }
}

/// Consume every queued event. With `act`, log faults and cut off the offending device at once;
/// the driver that owns it is then condemned (restarted by the scheduler).
///
/// A device that keeps issuing the same bad access produces one event per access (QEMU's virtio
/// retries in 4-byte steps), so only the first of a run of identical events is logged.
fn drain_events(s: &mut Smmu, act: bool, skip: Option<usize>) {
    let mask = (2u32 << EVTQ_LOG2) - 1;
    let mut cons = r32(EVTQ_CONS) & mask;
    let (mut last, mut repeats) = ((u64::MAX, usize::MAX), 0u32);
    let mut overflowed = false;
    loop {
        let prod_reg = r32(EVTQ_PROD);
        let prod = prod_reg & mask;
        // PROD.OVFLG (bit 31) flips when the queue was full and events were discarded
        overflowed |= (prod_reg ^ r32(EVTQ_CONS)) >> 31 != 0;
        if prod == cons {
            // acknowledge any overflow by copying OVFLG into CONS.OVACKFLG
            w32(EVTQ_CONS, cons | prod_reg & 1 << 31);
            break;
        }
        let rec = (s.evtq + 32 * (cons & ((1 << EVTQ_LOG2) - 1)) as usize) as *const u64;
        crate::cache::invalidate(rec as usize, 32); // the SMMU wrote the record; do not read a stale cached copy
        // SAFETY: `rec` is event `cons & 127` of the event-queue page (128 x 32 B, 8-aligned, from `init`); reads dw0 and dw2 inside that
        // entry, after `cache::invalidate`, and the SMMU lock is held (`s: &mut Smmu`).
        let (dw0, addr) = unsafe { (read_volatile(rec), read_volatile(rec.add(2))) };
        let (id, sid) = (dw0 & 0xff, (dw0 >> 32) as usize);
        if act && Some(sid) != skip {
            if (id, sid) == last {
                repeats += 1;
            } else {
                if repeats > 0 {
                    kprintln!("[smmu]   ... and {repeats} more like it");
                }
                repeats = 0;
                last = (id, sid);
                kprintln!("[smmu] blocked DMA: {} ({:#x}) from stream {:#x}, address {:#x}", event_name(id), id, sid, addr);
            }
            let st = s.streams.get(sid).copied().filter(|st| st.attached);
            if let Some(st) = st.filter(|_| (0x10..=0x13).contains(&id)) {
                if !s.streams[sid].cut_off {
                    // stop the device now; the stream stays unusable until a new driver is attached
                    s.streams[sid].cut_off = true;
                    // SAFETY: `sid` came from `s.streams.get(sid)` above, so it is < NSTREAMS and `ste(sid)` is inside the stream-table page; SMMU lock held.
                    unsafe { write_volatile(s.ste(sid), 0) };
                    crate::cache::clean(s.ste(sid) as usize, STE_BYTES);
                    dsb();
                    s.invalidate(sid);
                }
                task::condemn_role(st.role, crate::sched::Doom::Iommu);
            }
        }
        cons = (cons + 1) & mask;
        w32(EVTQ_CONS, cons | prod_reg & 1 << 31);
    }
    if act && overflowed {
        kprintln!("[smmu] event queue was full: some fault events were discarded");
    }
    if repeats > 0 {
        kprintln!("[smmu]   ... and {repeats} more like it");
    }
    let gerr = r32(GERROR);
    let new = gerr ^ r32(GERRORN);
    if new != 0 {
        if act && new & (1 << 2) != 0 {
            kprintln!("[smmu] could not write an event to the event queue (EVENTQ_ABT_ERR)");
        } else if act {
            kprintln!("[smmu] global error {new:#x}");
        }
        w32(GERRORN, gerr); // acknowledge
    }
}

/// Event-queue interrupt.
pub fn event_irq() {
    let mut s = SMMU.lock(R_SMMU);
    if s.ready {
        drain_events(&mut s, true, None);
    }
}
