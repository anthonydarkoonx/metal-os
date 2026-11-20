//! The QEMU `virt` machine: its early devices (`pl011`, `gicv2` or `gicv3` as the device tree says, `psci`, the PL031 clock below), its fixed parameters and its test-harness switches.
//! A new machine is a sibling of this file (`platform/<machine>.rs`) selected by a cargo feature in `platform.rs`; the shared kernel never names QEMU.
//!
//! The harness switches come from QEMU's documented `fw_cfg`
//! interface: a data register at the base and a big-endian 16-bit selector at base + 8.
//! The harness passes the RAM size with `-fw_cfg name=opt/metal/ram-bytes,string=<n>`;
//! on real hardware this would come from the firmware's memory map instead.
use crate::hal::{PowerControl, Rtc};
use core::ptr::{read_volatile, write_volatile};

mod gicv2;
mod gicv3;
mod pl011;
mod psci;

/// The machine's early devices, as the shared kernel names them (see `hal.rs`).
pub type Uart = pl011::Pl011;
pub type Power = psci::Psci;
pub type Clock = Pl031;

const FW_CFG: usize = 0x0902_0000;
const KEY_SIGNATURE: u16 = 0x0000;
const KEY_FILE_DIR: u16 = 0x0019;
const RAM_FILE: &[u8] = b"opt/metal/ram-bytes";

fn select(key: u16) {
    // SAFETY: FW_CFG+8 is the fw_cfg selector register (2 bytes) in the device block (l1[0] of `map_devices`); volatile MMIO write.
    unsafe { write_volatile((FW_CFG + 8) as *mut u16, key.to_be()) };
}

fn read_bytes(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: FW_CFG+0 is the fw_cfg data register (byte-wide) in the mapped device block; volatile MMIO read.
        *b = unsafe { read_volatile(FW_CFG as *const u8) };
    }
}

fn read_be32() -> u32 {
    let mut b = [0u8; 4];
    read_bytes(&mut b);
    u32::from_be_bytes(b)
}

fn present() -> bool {
    let mut sig = [0u8; 4];
    select(KEY_SIGNATURE);
    read_bytes(&mut sig);
    &sig == b"QEMU"
}

/// Selector and size of a named fw_cfg file. The directory is a big-endian count followed
/// by 64-byte entries: size (u32), selector (u16), reserved (u16), NUL-padded name (56 bytes).
fn find_file(name: &[u8]) -> Option<(u16, u32)> {
    select(KEY_FILE_DIR);
    let count = read_be32();
    for _ in 0..count.min(256) {
        let size = read_be32();
        let mut sel = [0u8; 2];
        read_bytes(&mut sel);
        let mut skip = [0u8; 2];
        read_bytes(&mut skip);
        let mut n = [0u8; 56];
        read_bytes(&mut n);
        let len = n.iter().position(|&c| c == 0).unwrap_or(56);
        if &n[..len] == name {
            return Some((u16::from_be_bytes(sel), size));
        }
    }
    None
}

// ---- the device tree -------------------------------------------------------------------------------------------------------------------

use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use metal_fdt::{Conduit, Fdt, Gic};

/// Where firmware put the device tree (x0 at entry); 0 if it did not say.
static DTB: AtomicUsize = AtomicUsize::new(0);
/// The most bytes read from it (the QEMU tree is about 10 KiB).
const DTB_LIMIT: usize = 1 << 20;

/// Boot code: the address firmware gave in x0. Called with the MMU off (a plain store), and again once it is on, because the copy of the image
/// `kaslr` makes starts with zeroed statics. QEMU loads an ELF kernel without passing the tree in x0 but still puts it at the start of RAM, so
/// that is tried when x0 does not point at one.
pub fn set_dtb(addr: usize) {
    // a tree lies in RAM and starts with its magic number; anything else (no tree, a stray register value) is ignored rather than read
    let is_tree = |a: usize| {
        let ok = a % 8 == 0 && (RAM_BASE..RAM_BASE + (1 << 30) - 64).contains(&a);
        // SAFETY: `a` is an 8-aligned address inside the first GiB of RAM (checked just above); single-byte volatile loads, valid with the MMU off.
        ok && [0xd0u8, 0x0d, 0xfe, 0xed].iter().enumerate().all(|(i, &m)| unsafe { read_volatile((a + i) as *const u8) } == m)
    };
    DTB.store(if is_tree(addr) { addr } else if is_tree(RAM_BASE) { RAM_BASE } else { 0 }, Ordering::Relaxed);
}

fn dtb_ram() -> Option<(u64, u64)> {
    let d = DTB.load(Ordering::Relaxed);
    if d == 0 {
        return None;
    }
    // SAFETY: `set_dtb` accepted `d` as an 8-aligned address inside RAM, and the limit keeps the read inside the first GiB of it; every load is a
    // single-byte volatile one (see `early_memory`), so it is also safe with the MMU off.
    unsafe { metal_fdt::early_memory(d as *const u8, DTB_LIMIT.min(RAM_BASE + (1 << 30) - d)) }
}

/// What the tree says about the machine, kept in plain atomics once read (`init_machine`).
static NCPUS: AtomicUsize = AtomicUsize::new(0);
/// Each core's `capacity-dmips-mhz` (0 if the tree gave none), in the core order of `MPIDRS`.
static CAPACITY: [core::sync::atomic::AtomicU32; crate::smp::MAX_CPUS] = [const { core::sync::atomic::AtomicU32::new(0) }; crate::smp::MAX_CPUS];
static MPIDRS: [AtomicU64; crate::smp::MAX_CPUS] = [const { AtomicU64::new(0) }; crate::smp::MAX_CPUS];
static CONDUIT_SMC: AtomicU8 = AtomicU8::new(0);
/// 2 or 3: the interrupt controller's version (2 until the tree says otherwise).
static GIC_VERSION: AtomicU8 = AtomicU8::new(2);

fn read_machine() -> Option<metal_fdt::Machine> {
    let d = DTB.load(Ordering::Relaxed);
    if d == 0 {
        return None;
    }
    let max = RAM_BASE + (1 << 30) - d;
    // SAFETY: as in `dtb_ram`; the MMU is on here, so ordinary loads are fine. The header's own size field bounds the slice, `Fdt::new` checks it.
    let head = unsafe { core::slice::from_raw_parts(d as *const u8, 8.min(max)) };
    let total = u32::from_be_bytes(head.get(4..8)?.try_into().ok()?) as usize;
    // SAFETY: `total` bytes at `d` are inside the first GiB of RAM (checked against `max`), and nothing writes the tree.
    let blob = unsafe { core::slice::from_raw_parts(d as *const u8, total.min(max).min(DTB_LIMIT)) };
    Some(metal_fdt::machine(&Fdt::new(blob)?))
}

/// Read the tree (MMU on, boot core, before anything that needs the interrupt controller or the core list): sets the controller's version and
/// addresses, the number of cores and their MPIDRs (the boot core first), and how firmware is called.
pub fn init_machine() {
    let boot: u64;
    // SAFETY: reads MPIDR_EL1, a read-only register; no memory is accessed.
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) boot, options(nomem, nostack)) };
    let boot = boot & 0xFF_00FF_FFFF;
    MPIDRS[0].store(boot, Ordering::Relaxed);
    CAPACITY[0].store(0, Ordering::Relaxed);
    NCPUS.store(0, Ordering::Relaxed);
    let Some(m) = read_machine() else { return };
    match m.gic {
        Some(Gic::V2 { dist, cpu }) => {
            gicv2::GICD_BASE.store(dist as usize, Ordering::Relaxed);
            gicv2::GICC_BASE.store(cpu as usize, Ordering::Relaxed);
        }
        Some(Gic::V3 { dist, redist, redist_size }) => {
            GIC_VERSION.store(3, Ordering::Relaxed);
            gicv3::GICD.store(dist as usize, Ordering::Relaxed);
            gicv3::GICR.store(redist as usize, Ordering::Relaxed);
            gicv3::GICR_SIZE.store(redist_size as usize, Ordering::Relaxed);
        }
        None => {}
    }
    CONDUIT_SMC.store((m.conduit == Some(Conduit::Smc)) as u8, Ordering::Relaxed);
    // the cores: this one first (it is core 0 whatever its place in the tree), then the rest in tree order
    let mut n = 1;
    for (i, &c) in m.cpus[..m.ncpus].iter().enumerate() {
        if c == boot {
            CAPACITY[0].store(m.capacity[i], Ordering::Relaxed);
        } else if n < crate::smp::MAX_CPUS {
            MPIDRS[n].store(c, Ordering::Relaxed);
            CAPACITY[n].store(m.capacity[i], Ordering::Relaxed);
            n += 1;
        }
    }
    NCPUS.store(if m.ncpus == 0 { 0 } else { n }, Ordering::Relaxed);
}

/// How many cores the machine has, as far as the tree says (at most `MAX_CPUS`); `None` if it did not say.
pub fn cpu_count() -> Option<usize> {
    Some(NCPUS.load(Ordering::Relaxed)).filter(|&n| n != 0)
}

/// Core `core`'s MPIDR affinity (the number PSCI and the interrupt controller name it by). Without a tree a core's number is its Aff0.
pub fn cpu_mpidr(core: usize) -> u64 {
    if NCPUS.load(Ordering::Relaxed) == 0 && core != 0 {
        return core as u64;
    }
    MPIDRS[core].load(Ordering::Relaxed)
}

/// The memory the device tree occupies, which nothing may be placed over until it has been read: (start, bytes), or `None` without one.
/// (Readable with the MMU off: single-byte volatile loads.)
pub fn dtb_span() -> Option<(usize, usize)> {
    let d = DTB.load(Ordering::Relaxed);
    if d == 0 {
        return None;
    }
    // SAFETY: `set_dtb` accepted `d`: 8-aligned, inside RAM, with the magic number in its first four bytes; the size field is the next four.
    let size = (4..8).fold(0usize, |v, i| v << 8 | unsafe { read_volatile((d + i) as *const u8) } as usize);
    Some((d, size.clamp(64, DTB_LIMIT)))
}

/// Where the device tree was found (0: none).
pub fn dtb_addr() -> usize {
    DTB.load(Ordering::Relaxed)
}

/// Whether core `core` is an efficiency core, if the tree says the cores differ in speed (`capacity-dmips-mhz`: the slower ones are); `None` if it
/// does not, and the kernel uses its fixed model.
pub fn core_is_little(core: usize) -> Option<bool> {
    let n = NCPUS.load(Ordering::Relaxed);
    let caps = || CAPACITY[..n].iter().map(|c| c.load(Ordering::Relaxed));
    let (lo, hi) = (caps().min()?, caps().max()?);
    (core < n && lo != hi && lo != 0).then(|| CAPACITY[core].load(Ordering::Relaxed) < hi)
}

pub fn intc_name() -> &'static str {
    if GIC_VERSION.load(Ordering::Relaxed) == 3 {
        "GICv3"
    } else {
        "GICv2"
    }
}

/// The interrupt controller: whichever the tree described (GICv2 if it did not), chosen at run time by one load.
pub struct Intc;

macro_rules! gic_call {
    ($f:ident($($a:expr),*)) => {
        if GIC_VERSION.load(Ordering::Relaxed) == 3 { gicv3::GicV3::$f($($a),*) } else { gicv2::GicV2::$f($($a),*) }
    };
}

impl crate::hal::IrqController for Intc {
    fn init_dist() {
        gic_call!(init_dist())
    }
    fn init_cpu() {
        gic_call!(init_cpu())
    }
    fn send_sgi_to(mask: u32, id: u32) {
        gic_call!(send_sgi_to(mask, id))
    }
    fn enable(id: u32) {
        gic_call!(enable(id))
    }
    fn disable(id: u32) {
        gic_call!(disable(id))
    }
    fn configure_spi(id: u32) {
        gic_call!(configure_spi(id))
    }
    fn set_urgent(id: u32) {
        gic_call!(set_urgent(id))
    }
    fn set_edge(id: u32) {
        gic_call!(set_edge(id))
    }
    fn ack() -> u32 {
        gic_call!(ack())
    }
    fn eoi(iar: u32) {
        gic_call!(eoi(iar))
    }
}

/// RAM size in bytes: the harness's number if it gave one, else the tree's.
pub fn ram_size() -> Option<u64> {
    harness_ram_size().or_else(|| dtb_ram().filter(|&(base, _)| base == RAM_BASE as u64).map(|(_, size)| size))
}

fn harness_ram_size() -> Option<u64> {
    if !present() {
        return None;
    }
    let (sel, size) = find_file(RAM_FILE)?;
    if size == 0 || size > 20 {
        return None;
    }
    select(sel);
    let mut buf = [0u8; 20];
    read_bytes(&mut buf[..size as usize]);
    let mut v = 0u64;
    for &c in &buf[..size as usize] {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

/// Is the named fw_cfg file present and does it start with '1'? (Boolean switches from the harness.)
pub fn flag(name: &[u8]) -> bool {
    if !present() {
        return false;
    }
    let Some((sel, size)) = find_file(name) else { return false };
    if size == 0 {
        return false;
    }
    select(sel);
    let mut b = [0u8; 1];
    read_bytes(&mut b);
    b[0] == b'1'
}

/// QEMU's PL031 real-time clock: its data register is the time in seconds since 1970.
const RTC: usize = 0x0901_0000;

pub struct Pl031;

impl Rtc for Pl031 {
    fn seconds() -> Option<u64> {
        if !present() {
            return None;
        }
        // SAFETY: RTC is the PL031 data register (4-aligned) in the mapped device block; `present()` was checked above; volatile MMIO read.
        match unsafe { read_volatile(RTC as *const u32) } {
            0 => None,
            s => Some(s as u64),
        }
    }
}

pub fn rtc_seconds() -> Option<u64> {
    Clock::seconds()
}

static INTERACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Read the harness's switches once at boot.
pub fn init_flags() {
    // the test harness types into the guest: the conductor must keep the system up for it
    INTERACTIVE.store(flag(b"opt/metal/interactive"), core::sync::atomic::Ordering::Relaxed);
    // a non-zero seed switches the system into chaos-test mode (see user/src/chaos.rs)
    CHAOS_SEED.store(number(b"opt/metal/chaos").unwrap_or(0), core::sync::atomic::Ordering::Relaxed);
    MAP_LOG.store(flag(b"opt/metal/map-log"), core::sync::atomic::Ordering::Relaxed);
    // development: run just one group of the conductor's tests (1 = the IPC benchmark, 2 = the scheduler tests)
    ONLY.store(number(b"opt/metal/only").unwrap_or(0), core::sync::atomic::Ordering::Relaxed);
    // a non-zero seed starts the syscall fuzzer and runs only its checks (user/src/fuzz.rs)
    let fuzz = number(b"opt/metal/fuzz").unwrap_or(0);
    FUZZ_SEED.store(fuzz, core::sync::atomic::Ordering::Relaxed);
    if fuzz != 0 {
        ONLY.store(6, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Test aid (`opt/metal/map-log=1`): the kernel logs where it places every mapping it chooses an address for.
static MAP_LOG: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn map_log() -> bool {
    MAP_LOG.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn interactive() -> bool {
    INTERACTIVE.load(core::sync::atomic::Ordering::Relaxed)
}

/// Up to `out.len()` bytes of a named fw_cfg file (binary), if present; how many were read.
pub fn bytes(name: &[u8], out: &mut [u8]) -> usize {
    if !present() {
        return 0;
    }
    let Some((sel, size)) = find_file(name) else { return 0 };
    let n = (size as usize).min(out.len());
    select(sel);
    read_bytes(&mut out[..n]);
    n
}

/// A decimal number stored in a named fw_cfg file, if present.
pub fn number(name: &[u8]) -> Option<u64> {
    if !present() {
        return None;
    }
    let (sel, size) = find_file(name)?;
    if size == 0 || size > 20 {
        return None;
    }
    select(sel);
    let mut buf = [0u8; 20];
    read_bytes(&mut buf[..size as usize]);
    buf[..size as usize].iter().try_fold(0u64, |v, &c| if c.is_ascii_digit() { v.checked_mul(10)?.checked_add((c - b'0') as u64) } else { None })
}

static ONLY: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn only() -> u64 {
    ONLY.load(core::sync::atomic::Ordering::Relaxed)
}

static FUZZ_SEED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn fuzz_seed() -> u64 {
    FUZZ_SEED.load(core::sync::atomic::Ordering::Relaxed)
}

static CHAOS_SEED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn chaos_seed() -> u64 {
    CHAOS_SEED.load(core::sync::atomic::Ordering::Relaxed)
}

// ---- the machine's fixed parameters and its firmware calls ---------------------------------------------------------------------------
// Everything below is what differs between machines (QEMU `virt` here; a board gives its own `platform.rs`): the shared kernel files call these and
// never contain a machine's constants or instructions themselves.

/// Where the RAM the kernel manages starts, and where the kernel image is linked (linker.ld says the same).
pub const RAM_BASE: usize = 0x4000_0000;
pub const LINK_BASE: usize = 0x4008_0000;
/// Physical addresses below this are not mapped (firmware keeps them); 0: all RAM is.
pub const RAM_UNMAPPED_BELOW: usize = 0;
/// The first banner line ends with this; the interrupt controller's and the entropy source's names for the log.
pub const BANNER_SUFFIX: &str = "";
pub const SEED_SOURCE: &str = "fw_cfg ";
/// Whether the machine has the SMMUv3 the kernel confines devices with.
pub const HAS_SMMU: bool = true;
/// The compressed swap store: bytes of pool, and entries (a platform with memory to spare gives a bigger one; the memory daemon reads it from SYSINFO page 17).
pub const SWAP_POOL_BYTES: usize = 2 * 1024 * 1024;
pub const SWAP_SLOTS: usize = 2048;
/// The most memory the console task may hold (pages).
pub const CONSOLE_HEAP_PAGES: usize = 16000;

/// The device windows of the kernel's page tables (the first-level table, 1 GiB entries).
pub fn map_devices(l1: &mut [u64; 512]) {
    l1[0] = crate::mmu::DEVICE_BLOCK; // UART, GIC, PCI BARs
    l1[256] = 0x40_0000_0000 | crate::mmu::DEVICE_BLOCK; // PCIe ECAM
}

/// Switch the machine off (`reset` false) or restart it through the firmware (PSCI). Returns only if the firmware refused.
pub fn psci_power(reset: bool) {
    Power::power(reset)
}

/// PSCI CPU_ON for `core`: it starts at `entry` with `core` as its context id. Returns the PSCI status.
///
/// # Safety
/// `entry` must be the secondary core's entry point.
pub unsafe fn psci_cpu_on(core: usize, entry: u64) -> i64 {
    // SAFETY: the caller's contract (`entry` is the secondary core's entry point) is exactly `cpu_on`'s.
    unsafe { Power::cpu_on(core, entry) }
}

/// End the run with `code` where the machine can (QEMU: semihosting); returns if it cannot.
pub fn exit_machine(code: u32) {
    let block = [0x20026u64, code as u64]; // ADP_Stopped_ApplicationExit
    // SAFETY: ARM semihosting SYS_EXIT (op 0x18) under QEMU: x1 points to `block`, a live stack array of two
    // u64s for the duration of the call; the host reads it. Memory may be touched, so no `nomem`.
    unsafe { core::arch::asm!("hlt #0xf000", in("x0") 0x18u64, in("x1") block.as_ptr(), options(nostack)) };
}

/// Before anything is set up, right after the first line (a board can wait here for a kernel sent over a serial line).
pub fn early_boot() {}

/// A report on the entropy source, before it is used.
pub fn entropy_report() {}

/// The machine's own devices (display, USB, network, ...) are probed and started; after the timer and scheduler, before the objects are made.
pub fn init_devices() {}

/// A device interrupt the platform handles itself (a UART receive, say); `true` if it was one.
pub fn irq(_id: u32) -> bool {
    false
}

/// Find the machine's devices and make the kernel objects for them (QEMU: the virtio devices on the PCI bus).
pub fn probe_devices() {
    crate::devices::probe_pci();
}

/// The capabilities a boot-time task of `role` gets for this machine's own devices, on top of the general ones. `grant(slot, kind, object, rights)`.
pub fn boot_caps(_role: u8, _grant: &mut dyn FnMut(usize, crate::cap::Kind, u16, crate::cap::Rights)) {}

/// Tasks this machine has and others do not (a board's audio service, say), spawned after all the others.
pub fn spawn_extra_tasks() {}
