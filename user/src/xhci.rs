//! USB host controller (xHCI) driver for the input driver task (role 14): finds keyboards and pointing devices on the controller's ports and hands their
//! reports to `kbd.rs` as the same key and pointer events the virtio input devices produce.
//!
//! Everything the device and driver share (rings, contexts, report buffers) is one DMA object the kernel gave this task (`OBJ_USB_DMA`); the IOMMU lets the
//! controller reach nothing else. Nothing waits for an interrupt: like the virtio input devices, the controller is polled when someone asks for input.
//!
//! Devices may sit on a root port or behind hubs (up to three tiers); they can be plugged and unplugged while the system runs (the controller's port-change
//! events, and a hub's status-change endpoint, say so). A held key repeats (after half a second, thirty times a second: the device does not do it). Not done:
//! SuperSpeed hubs, hubs with a transaction translator per port, more than 16 devices at once.
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use metal_driver_rt::dma;
use metal_driver_rt::*;
use metal_usb::{hid_interfaces, hub_descriptor, hub_interface, port_status, xhci_interval, HidIface, HubInfo, Keyboard, Pointer};

/// Where this task maps the controller's registers and shared memory (a handle in the big window for the memory: it is more than 64 KiB's worth of handle).
const MMIO_H: usize = 0x8027_0000;
const DMA_H: usize = 0xA010_0000;
/// Capability slots (see `devices.rs`).
const SLOT_MMIO: u64 = 7;
const SLOT_DMA: u64 = 8;

// layout of the shared memory (the kernel makes it USB_DMA_PAGES = 80 pages: the end of the scratchpad buffers)
const O_DCBAA: usize = 0x0000; // device context base address array: 256 entries
const O_SCRATCH_ARRAY: usize = 0x0800;
const O_CMD: usize = 0x1000; // command ring (64 TRBs used)
const O_EVT: usize = 0x2000; // event ring (256 TRBs)
const O_ERST: usize = 0x3000;
const O_INPUT: usize = 0x4000; // input context (also the one for every command: commands are one at a time)
const O_DEV0: usize = 0x8000; // per device: 4 pages
const DEV_STRIDE: usize = 0x4000;
const MAX_DEVS: usize = 16; // (16 * 0x4000 ends where the scratchpad buffers start)
const O_SCRATCH: usize = O_DEV0 + MAX_DEVS * DEV_STRIDE; // scratchpad buffers: up to 8 pages
const MAX_SCRATCH: usize = 8;
const EVT_TRBS: usize = 256;
const RING_TRBS: usize = 64;

// TRB types
const T_NORMAL: u32 = 1;
const T_SETUP: u32 = 2;
const T_DATA: u32 = 3;
const T_STATUS: u32 = 4;
const T_LINK: u32 = 6;
const T_ENABLE_SLOT: u32 = 9;
const T_ADDRESS: u32 = 11;
const T_CONFIGURE: u32 = 12;
const T_DISABLE_SLOT: u32 = 10;
const T_EVALUATE: u32 = 13;
const T_PORT_CHANGE: u32 = 34;
const T_TRANSFER_EVENT: u32 = 32;
const T_CMD_COMPLETE: u32 = 33;

const CC_SUCCESS: u32 = 1;
const CC_SHORT: u32 = 13;

fn r32(a: usize) -> u32 {
    // SAFETY: `a` is a register (4-aligned) inside the controller's mapped register window, or a word of the shared memory this task mapped; volatile because the device changes it.
    unsafe { read_volatile(a as *const u32) }
}
fn w32(a: usize, v: u32) {
    // SAFETY: as `r32`, for a write.
    unsafe { write_volatile(a as *mut u32, v) }
}
fn w64(a: usize, v: u64) {
    w32(a, v as u32);
    w32(a + 4, (v >> 32) as u32);
}

/// A transfer or command ring: `RING_TRBS` entries of 16 bytes, the last a link back to the start.
struct Ring {
    va: usize,
    pa: u64,
    enq: usize,
    cycle: u32,
}

impl Ring {
    fn new(va: usize, pa: u64) -> Ring {
        Ring { va, pa, enq: 0, cycle: 1 }
    }

    /// Queue one TRB (`control` without the cycle bit); returns the physical address it has.
    fn push(&mut self, param: u64, status: u32, control: u32) -> u64 {
        let at = self.va + self.enq * 16;
        w64(at, param);
        w32(at + 8, status);
        w32(at + 12, control | self.cycle); // (the cycle bit goes last: it is what hands the TRB over)
        dma::clean(at, 16);
        let pa = self.pa + self.enq as u64 * 16;
        self.enq += 1;
        if self.enq == RING_TRBS - 1 {
            let l = self.va + self.enq * 16;
            w64(l, self.pa);
            w32(l + 8, 0);
            w32(l + 12, T_LINK << 10 | 1 << 1 | self.cycle); // toggle cycle
            dma::clean(l, 16);
            self.enq = 0;
            self.cycle ^= 1;
        }
        pa
    }
}

enum Kind {
    Keyboard(Keyboard),
    Pointer { layout: Pointer, buttons: u8 },
    /// A hub's status-change endpoint: bit n of its report says port n has something to tell.
    Hub,
}

/// One interrupt IN endpoint being read.
struct Ep {
    dci: u8,
    ring: Ring,
    buf_va: usize,
    buf_pa: u64,
    len: usize,
    interval: u8,
    max_packet: u32,
    kind: Kind,
}

struct HubState {
    ports: u32,
    /// The device slot of what sits on each downstream port (index = port number; 0 = nothing).
    children: [u8; 16],
}

struct Dev {
    slot: u8,
    /// Which of the `MAX_DEVS` memory areas it uses.
    area: usize,
    va: usize,
    pa: u64,
    ctrl: Ring,
    eps: Vec<Ep>,
    root_port: u32,
    route: u32,
    /// How many hubs are above it.
    tier: u32,
    speed: u32,
    /// The hub slot and port it hangs off, if not on a root port.
    parent: Option<(u8, u32)>,
    hub: Option<HubState>,
}

/// Where decoded input goes (`kbd.rs`).
pub trait Sink {
    fn key(&mut self, code: u16, value: u32);
    /// An evdev-style pointer event (`type`, `code`, `value`), as the virtio pointer reports them.
    fn pointer(&mut self, ty: u16, code: u16, value: u32);
}

pub struct Usb {
    op: usize,
    db: usize,
    ir: usize,
    csz: usize,
    dma_va: usize,
    dma_pa: u64,
    cmd: Ring,
    ev_deq: usize,
    ev_cycle: u32,
    /// Events that arrived while waiting for something else.
    stash: Vec<[u32; 4]>,
    devs: Vec<Dev>,
    used: [bool; MAX_DEVS],
    /// Pointer position (0..32767) for relative devices.
    px: i32,
    py: i32,
    /// The key being held (not a modifier) and the tick it repeats next.
    held: Option<u16>,
    next_repeat: u64,
    log: bool,
    pub keyboards: usize,
    pub pointers: usize,
}

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const REL_WHEEL: u16 = 8;
const BTN_LEFT: u16 = 0x110;
/// Repeat: after 50 ticks (half a second), then every 3 (a tick is 10 ms).
const REPEAT_DELAY: u64 = 50;
const REPEAT_EVERY: u64 = 3;
const CHANGES: u32 = 0x00FE_0000; // a root port's write-1-to-clear change bits
const PED: u32 = 1 << 1;

fn wait_for(mut done: impl FnMut() -> bool, ms: u64) -> bool {
    let end = ticks() + ms.div_ceil(10) + 1;
    loop {
        if done() {
            return true;
        }
        if ticks() > end {
            return false;
        }
        sleep_us(200);
    }
}

/// Modifier keys and Caps Lock do not repeat.
fn repeats(code: u16) -> bool {
    !matches!(code, 29 | 42 | 54 | 56 | 58 | 97 | 100 | 125 | 126)
}

impl Usb {
    /// Map the controller, reset it, set its rings up and start it, then look at every port and start the keyboards and pointers found.
    pub fn init() -> Result<Usb, &'static str> {
        if mem_map(SLOT_MMIO, MMIO_H, true) != 0 || mem_map(SLOT_DMA, DMA_H, true) != 0 {
            return Err("mapping failed");
        }
        let (mmio, dma_va) = (va(MMIO_H), va(DMA_H));
        let (st, dma_pa, pages, _) = cap_info(SLOT_DMA);
        if st != 0 || pages < 32 {
            return Err("no shared memory");
        }
        // SAFETY: the shared memory is `pages` mapped pages at `dma_va`; nothing else uses it yet, and a predecessor's state is wiped.
        unsafe { core::ptr::write_bytes(dma_va as *mut u8, 0, pages as usize * 4096) };
        dma::clean(dma_va, pages as usize * 4096);
        let caplen = r32(mmio) & 0xFF;
        let (hcs1, hcs2, hcc1) = (r32(mmio + 4), r32(mmio + 8), r32(mmio + 0x10));
        let (db, rt) = (mmio + (r32(mmio + 0x14) & !3) as usize, mmio + (r32(mmio + 0x18) & !0x1F) as usize);
        let op = mmio + caplen as usize;
        let (max_slots, max_ports) = (hcs1 & 0xFF, hcs1 >> 24);
        let scratch = ((hcs2 >> 21 & 0x1F) << 5 | hcs2 >> 27) as usize;
        if scratch > MAX_SCRATCH {
            return Err("controller wants too many scratchpad buffers");
        }
        // stop, reset
        w32(op, r32(op) & !1);
        if !wait_for(|| r32(op + 4) & 1 != 0, 200) {
            return Err("controller will not halt");
        }
        w32(op, 1 << 1);
        if !wait_for(|| r32(op) & 2 == 0 && r32(op + 4) & (1 << 11) == 0, 500) {
            return Err("controller reset timed out");
        }
        w32(op + 0x38, max_slots.min(MAX_DEVS as u32)); // CONFIG
        w64(op + 0x30, dma_pa + O_DCBAA as u64); // DCBAAP
        if scratch > 0 {
            for i in 0..scratch {
                w64(dma_va + O_SCRATCH_ARRAY + 8 * i, dma_pa + (O_SCRATCH + 4096 * i) as u64);
            }
            w64(dma_va + O_DCBAA, dma_pa + O_SCRATCH_ARRAY as u64); // entry 0 is the scratchpad array
        }
        let cmd = Ring::new(dma_va + O_CMD, dma_pa + O_CMD as u64);
        w64(op + 0x18, dma_pa + O_CMD as u64 | 1); // CRCR: the ring and its cycle state
        // event ring: one segment
        w64(dma_va + O_ERST, dma_pa + O_EVT as u64);
        w32(dma_va + O_ERST + 8, EVT_TRBS as u32);
        dma::clean(dma_va, 0x4000);
        dma::flush(dma_va + O_EVT, EVT_TRBS * 16);
        let ir = rt + 0x20;
        w32(ir + 8, 1); // ERSTSZ
        w64(ir + 0x18, dma_pa + O_EVT as u64); // ERDP
        w64(ir + 0x10, dma_pa + O_ERST as u64); // ERSTBA (last: it starts the ring)
        w32(ir, 3); // IMAN: IE (the PCI function's interrupt line is held off by the kernel, so this only makes the controller write events) and clear IP
        w32(op, 1 | 1 << 2); // run, interrupts enabled
        if !wait_for(|| r32(op + 4) & 1 == 0, 200) {
            return Err("controller will not run");
        }
        let log = sysinfo(2)[3] == 10; // (the USB input test: say what arrives; never in normal use, a log of key presses would be a keylogger)
        let mut u = Usb {
            op,
            db,
            ir,
            csz: if hcc1 & 4 != 0 { 64 } else { 32 },
            dma_va,
            dma_pa,
            cmd,
            ev_deq: 0,
            ev_cycle: 1,
            stash: Vec::new(),
            devs: Vec::new(),
            used: [false; MAX_DEVS],
            px: 16384,
            py: 16384,
            held: None,
            next_repeat: 0,
            log,
            keyboards: 0,
            pointers: 0,
        };
        for port in 1..=max_ports.min(16) {
            u.root_port_change(port);
        }
        Ok(u)
    }

    fn port_reg(&self, port: u32) -> usize {
        self.op + 0x400 + 0x10 * (port as usize - 1)
    }

    /// The next event the controller has written, if any.
    fn next_event(&mut self) -> Option<[u32; 4]> {
        let at = self.dma_va + O_EVT + self.ev_deq * 16;
        dma::flush(at, 16);
        let control = r32(at + 12);
        if control & 1 != self.ev_cycle {
            return None;
        }
        let ev = [r32(at), r32(at + 4), r32(at + 8), control];
        self.ev_deq += 1;
        if self.ev_deq == EVT_TRBS {
            self.ev_deq = 0;
            self.ev_cycle ^= 1;
        }
        w64(self.ir + 0x18, self.dma_pa + (O_EVT + self.ev_deq * 16) as u64 | 1 << 3); // ERDP, clearing the busy flag
        Some(ev)
    }

    /// Wait for the event `want` accepts; others are kept for `poll`.
    fn wait_event(&mut self, ms: u64, want: impl Fn(&[u32; 4]) -> bool) -> Option<[u32; 4]> {
        let end = ticks() + ms.div_ceil(10) + 1;
        loop {
            while let Some(e) = self.next_event() {
                if want(&e) {
                    return Some(e);
                }
                if self.stash.len() < 64 {
                    self.stash.push(e);
                }
            }
            if ticks() > end {
                return None;
            }
            sleep_us(200);
        }
    }

    /// Run a command: (completion code, slot id).
    fn command(&mut self, param: u64, control: u32) -> Result<(u32, u32), &'static str> {
        let pa = self.cmd.push(param, 0, control);
        w32(self.db, 0);
        let e = self.wait_event(1000, |e| e[3] >> 10 & 0x3F == T_CMD_COMPLETE && (e[0] as u64 | (e[1] as u64) << 32) == pa).ok_or("command timed out")?;
        Ok((e[2] >> 24, e[3] >> 24))
    }

    fn input_ctx(&self) -> usize {
        self.dma_va + O_INPUT
    }

    fn idx(&self, slot: u8) -> Result<usize, &'static str> {
        self.devs.iter().position(|d| d.slot == slot).ok_or("device is gone")
    }

    /// A control transfer on endpoint 0 of the device in `slot`; `data` is (physical address, length, device-to-host) of the data stage, if any.
    fn control(&mut self, slot: u8, setup: [u8; 8], data: Option<(u64, usize, bool)>) -> Result<(), &'static str> {
        let di = self.idx(slot)?;
        let trt = match data {
            None => 0,
            Some((_, _, true)) => 3,
            Some((_, _, false)) => 2,
        };
        let d = &mut self.devs[di];
        d.ctrl.push(u64::from_le_bytes(setup), 8, T_SETUP << 10 | 1 << 6 | trt << 16);
        if let Some((pa, len, inward)) = data {
            d.ctrl.push(pa, len as u32, T_DATA << 10 | (inward as u32) << 16);
        }
        let status_in = !matches!(data, Some((_, _, true)));
        d.ctrl.push(0, 0, T_STATUS << 10 | 1 << 5 | (status_in as u32) << 16);
        w32(self.db + 4 * slot as usize, 1); // EP0
        let e = self.wait_event(1000, |e| e[3] >> 10 & 0x3F == T_TRANSFER_EVENT && e[3] >> 24 == slot as u32 && e[3] >> 16 & 0x1F == 1).ok_or("control transfer timed out")?;
        match e[2] >> 24 {
            CC_SUCCESS | CC_SHORT => Ok(()),
            _ => Err("control transfer failed"),
        }
    }

    /// A control read into the device's control buffer; the bytes.
    fn read_ctrl(&mut self, slot: u8, request_type: u8, request: u8, value: u16, index: u16, len: usize) -> Result<Vec<u8>, &'static str> {
        let di = self.idx(slot)?;
        let (va, pa) = (self.devs[di].va + 0x3000, self.devs[di].pa + 0x3000);
        let len = len.min(512);
        dma::flush(va, len);
        let setup = [request_type, request, value as u8, (value >> 8) as u8, index as u8, (index >> 8) as u8, len as u8, (len >> 8) as u8];
        self.control(slot, setup, Some((pa, len, true)))?;
        dma::flush(va, len);
        let mut out = alloc::vec![0u8; len];
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: `va..va+len` is the device's control buffer inside the shared memory (len <= 512).
            *b = unsafe { read_volatile((va + i) as *const u8) };
        }
        Ok(out)
    }

    fn set_request(&mut self, slot: u8, request_type: u8, request: u8, value: u16, index: u16) -> Result<(), &'static str> {
        let setup = [request_type, request, value as u8, (value >> 8) as u8, index as u8, (index >> 8) as u8, 0, 0];
        self.control(slot, setup, None)
    }

    // ---- root ports ----------------------------------------------------------------------------------------------------------------------

    /// Something happened at root port `port` (or it is being looked at for the first time): acknowledge it, then start what is connected, or forget what went away.
    fn root_port_change(&mut self, port: u32) {
        let reg = self.port_reg(port);
        let ps = r32(reg);
        w32(reg, (ps & !(CHANGES | PED)) | (ps & CHANGES)); // acknowledge the changes
        let known = self.devs.iter().any(|d| d.root_port == port && d.parent.is_none());
        if ps & 1 == 0 {
            while let Some(d) = self.devs.iter().find(|d| d.root_port == port && d.parent.is_none()).map(|d| d.slot) {
                println!("[usb] port {port}: device removed");
                self.remove_slot(d);
            }
            return;
        }
        if known {
            return;
        }
        match self.root_connect(port) {
            Ok(()) => {}
            Err(e) => println!("[usb] port {port}: {e}"),
        }
    }

    fn root_connect(&mut self, port: u32) -> Result<(), &'static str> {
        let reg = self.port_reg(port);
        let ps = r32(reg);
        if ps & 1 << 9 == 0 {
            w32(reg, (ps & !(CHANGES | PED)) | 1 << 9); // power
            sleep(2);
        }
        w32(reg, (r32(reg) & !(CHANGES | PED)) | 1 << 4); // reset
        if !wait_for(|| r32(reg) & 1 << 21 != 0, 500) {
            return Err("reset timed out");
        }
        w32(reg, (r32(reg) & !(CHANGES | PED)) | 1 << 21); // clear the reset-change bit
        let ps = r32(reg);
        if ps & 1 == 0 || ps & PED == 0 {
            return Err("did not come up after reset");
        }
        self.enumerate(port, ps >> 10 & 0xF, 0, 0, None).map(|_| ())
    }

    // ---- devices -------------------------------------------------------------------------------------------------------------------------

    /// Take a device away: the controller's slot, then the memory it had (zeroed: the next device must not find stale TRBs).
    fn release(&mut self, slot: u8) {
        let Some(di) = self.devs.iter().position(|d| d.slot == slot) else { return };
        let d = self.devs.remove(di);
        for e in &d.eps {
            match e.kind {
                Kind::Keyboard(_) => self.keyboards = self.keyboards.saturating_sub(1),
                Kind::Pointer { .. } => self.pointers = self.pointers.saturating_sub(1),
                Kind::Hub => {}
            }
        }
        let _ = self.command(0, T_DISABLE_SLOT << 10 | (slot as u32) << 24);
        w64(self.dma_va + O_DCBAA + 8 * slot as usize, 0);
        dma::clean(self.dma_va + O_DCBAA, 8 * (MAX_DEVS + 1));
        // SAFETY: the area is `DEV_STRIDE` bytes inside the shared memory, and the slot was just disabled: the controller no longer reads or writes it.
        unsafe { core::ptr::write_bytes(d.va as *mut u8, 0, DEV_STRIDE) };
        dma::clean(d.va, DEV_STRIDE);
        self.used[d.area] = false;
    }

    /// A device went away: it, and everything behind it if it is a hub.
    fn remove_slot(&mut self, slot: u8) {
        while let Some(c) = self.devs.iter().find(|d| d.parent.is_some_and(|(p, _)| p == slot)).map(|d| d.slot) {
            self.remove_slot(c);
        }
        if let Some(di) = self.devs.iter().position(|d| d.slot == slot) {
            if let Some((hub, port)) = self.devs[di].parent {
                if let Some(h) = self.devs.iter_mut().find(|d| d.slot == hub).and_then(|d| d.hub.as_mut()) {
                    h.children[port as usize % 16] = 0;
                }
            }
        }
        if self.held.is_some() && self.devs.iter().all(|d| d.eps.iter().all(|e| !matches!(e.kind, Kind::Keyboard(_)) || d.slot == slot)) {
            self.held = None; // (the keyboard that held the key is going)
        }
        self.release(slot);
    }

    /// Address a new device (on root port `root_port`, speed id `speed`, at `route` behind `tier` hubs, hanging off `parent` = (hub slot, its port, the hub is high speed))
    /// and start what it is: a hub, or keyboards and pointers. Returns its slot.
    fn enumerate(&mut self, root_port: u32, speed: u32, route: u32, tier: u32, parent: Option<(u8, u32, bool)>) -> Result<u8, &'static str> {
        let area = self.used.iter().position(|u| !u).ok_or("too many devices")?;
        let (cc, slot) = self.command(0, T_ENABLE_SLOT << 10)?;
        if cc != CC_SUCCESS || slot == 0 || slot > MAX_DEVS as u32 || self.devs.iter().any(|d| d.slot as u32 == slot) {
            return Err("no device slot");
        }
        let slot = slot as u8;
        let base = O_DEV0 + area * DEV_STRIDE;
        let (va, pa) = (self.dma_va + base, self.dma_pa + base as u64);
        self.used[area] = true;
        let (ic, csz) = (self.input_ctx(), self.csz);
        // SAFETY: the input context page is inside the shared memory; commands are one at a time, so nothing else uses it now.
        unsafe { core::ptr::write_bytes(ic as *mut u8, 0, 4096) };
        let mps: u32 = match speed {
            4 | 5 => 512,
            3 => 64,
            _ => 8, // (a full-speed device may have more: read from its descriptor below)
        };
        w32(ic + 4, 0b11); // add: slot, endpoint 0
        w32(ic + csz, 1 << 27 | speed << 20 | route & 0xF_FFFF);
        w32(ic + csz + 4, root_port << 16);
        if let Some((hub, port, hub_is_high_speed)) = parent {
            if speed < 3 && hub_is_high_speed {
                w32(ic + csz + 8, hub as u32 | port << 8); // behind a high-speed hub: its transaction translator
            }
        }
        w32(ic + 2 * csz + 4, 3 << 1 | 4 << 3 | mps << 16); // CErr 3, control endpoint
        w64(ic + 2 * csz + 8, pa + 0x1000 | 1);
        w32(ic + 2 * csz + 16, 8);
        dma::clean(ic, 4096);
        w64(self.dma_va + O_DCBAA + 8 * slot as usize, pa);
        dma::clean(self.dma_va + O_DCBAA, 8 * (MAX_DEVS + 1));
        dma::flush(va, 4096);
        self.devs.push(Dev {
            slot,
            area,
            va,
            pa,
            ctrl: Ring::new(va + 0x1000, pa + 0x1000),
            eps: Vec::new(),
            root_port,
            route,
            tier,
            speed,
            parent: parent.map(|(h, p, _)| (h, p)),
            hub: None,
        });
        match self.start_device(slot, speed, mps, parent) {
            Ok(()) => Ok(slot),
            Err(e) => {
                self.remove_slot(slot);
                Err(e)
            }
        }
    }

    fn start_device(&mut self, slot: u8, speed: u32, mps: u32, _parent: Option<(u8, u32, bool)>) -> Result<(), &'static str> {
        let (cc, _) = self.command(self.dma_pa + O_INPUT as u64, T_ADDRESS << 10 | (slot as u32) << 24)?;
        if cc != CC_SUCCESS {
            return Err("address device failed");
        }
        // endpoint 0's real packet size: a full-speed device may use 8, 16, 32 or 64 (the first 8 bytes of the device descriptor say)
        let dd = self.read_ctrl(slot, 0x80, 6, 1 << 8, 0, 8)?;
        if speed == 1 && dd[7] as u32 != mps && matches!(dd[7], 8 | 16 | 32 | 64) {
            self.set_ep0_packet_size(slot, dd[7] as u32)?;
        }
        // the configuration descriptor
        let head = self.read_ctrl(slot, 0x80, 6, 2 << 8, 0, 9)?;
        let total = u16::from_le_bytes([head[2], head[3]]) as usize;
        let config = self.read_ctrl(slot, 0x80, 6, 2 << 8, 0, total.clamp(9, 512))?;
        if let Some(h) = hub_interface(&config) {
            return self.start_hub(slot, speed, h);
        }
        let mut ifs = [HidIface::default(); 2];
        let n = hid_interfaces(&config, &mut ifs);
        if n == 0 {
            return Err("not an input device");
        }
        self.start_interfaces(slot, speed, &ifs[..n])
    }

    /// Evaluate Context: endpoint 0's maximum packet size.
    fn set_ep0_packet_size(&mut self, slot: u8, mps: u32) -> Result<(), &'static str> {
        let (ic, csz) = (self.input_ctx(), self.csz);
        let di = self.idx(slot)?;
        let out = self.devs[di].va;
        // SAFETY: the input context page is inside the shared memory, not in use (commands are one at a time).
        unsafe { core::ptr::write_bytes(ic as *mut u8, 0, 4096) };
        dma::flush(out, 2 * csz);
        for off in (0..2 * csz).step_by(4) {
            w32(ic + csz + off, r32(out + off)); // slot and endpoint 0 contexts as the controller has them
        }
        w32(ic + 4, 0b11);
        w32(ic + 2 * csz + 4, r32(ic + 2 * csz + 4) & 0xFFFF | mps << 16);
        dma::clean(ic, 4096);
        let (cc, _) = self.command(self.dma_pa + O_INPUT as u64, T_EVALUATE << 10 | (slot as u32) << 24)?;
        if cc != CC_SUCCESS {
            return Err("evaluate context failed");
        }
        Ok(())
    }

    /// Configure Endpoint: add `eps` (all interrupt IN) to the slot; for a hub also its hub fields.
    fn configure(&mut self, slot: u8, eps: &[Ep], hub: Option<(u32, u32)>) -> Result<(), &'static str> {
        let (ic, csz) = (self.input_ctx(), self.csz);
        let di = self.idx(slot)?;
        let out = self.devs[di].va;
        // SAFETY: the input context page is inside the shared memory, not in use (commands are one at a time).
        unsafe { core::ptr::write_bytes(ic as *mut u8, 0, 4096) };
        dma::flush(out, csz);
        let max_dci = eps.iter().map(|e| e.dci).max().unwrap_or(1) as u32;
        for off in (0..csz).step_by(4) {
            w32(ic + csz + off, r32(out + off)); // the slot context out of the output context
        }
        let mut dw0 = r32(ic + csz) & !(0x1F << 27) | max_dci << 27;
        if let Some((ports, think)) = hub {
            dw0 |= 1 << 26; // Hub
            w32(ic + csz + 4, r32(ic + csz + 4) & 0x00FF_FFFF | ports << 24);
            w32(ic + csz + 8, r32(ic + csz + 8) | think << 16);
        }
        w32(ic + csz, dw0);
        let mut add = 1u32;
        for e in eps {
            let at = ic + (e.dci as usize + 1) * csz;
            add |= 1 << e.dci;
            w32(at, (xhci_interval(self.devs[di].speed as u8, e.interval) as u32) << 16);
            w32(at + 4, 3 << 1 | 7 << 3 | e.max_packet << 16); // CErr 3, interrupt IN
            w64(at + 8, e.ring.pa | 1);
            w32(at + 16, e.max_packet | e.max_packet << 16);
        }
        w32(ic + 4, add);
        dma::clean(ic, 4096);
        let (cc, _) = self.command(self.dma_pa + O_INPUT as u64, T_CONFIGURE << 10 | (slot as u32) << 24)?;
        if cc != CC_SUCCESS {
            return Err("configure endpoint failed");
        }
        Ok(())
    }

    fn new_ep(&self, di: usize, k: usize, dci: u8, max_packet: u16, interval: u8, kind: Kind) -> Ep {
        let d = &self.devs[di];
        Ep {
            dci,
            ring: Ring::new(d.va + 0x2000 + k * 0x400, d.pa + 0x2000 + k as u64 * 0x400),
            buf_va: d.va + 0x3400 + k * 0x40,
            buf_pa: d.pa + 0x3400 + k as u64 * 0x40,
            len: (max_packet as usize).clamp(1, 64),
            interval,
            max_packet: max_packet as u32 & 0x7FF,
            kind,
        }
    }

    fn start_interfaces(&mut self, slot: u8, speed: u32, ifs: &[HidIface]) -> Result<(), &'static str> {
        let di = self.idx(slot)?;
        let mut eps: Vec<Ep> = Vec::new();
        let mut boot_ifaces: Vec<u8> = Vec::new();
        for (k, i) in ifs.iter().enumerate() {
            let kind = if i.subclass == 1 && i.protocol == 1 {
                Kind::Keyboard(Keyboard::new())
            } else if i.subclass == 1 && i.protocol == 2 {
                Kind::Pointer { layout: metal_usb::boot_mouse(), buttons: 0 }
            } else {
                // a descriptor tells where a pointer's fields are
                let desc = self.read_ctrl(slot, 0x81, 6, 0x22 << 8, i.number as u16, i.report_len as usize)?;
                match metal_usb::parse_pointer(&desc) {
                    Some(layout) => Kind::Pointer { layout, buttons: 0 },
                    None => continue,
                }
            };
            if i.subclass == 1 {
                boot_ifaces.push(i.number);
            }
            eps.push(self.new_ep(di, k, (i.ep_addr & 0xF) * 2 + 1, i.ep_max_packet, i.ep_interval, kind));
        }
        if eps.is_empty() {
            return Err("no keyboard or pointer interface");
        }
        let _ = speed;
        self.configure(slot, &eps, None)?;
        // the device: configuration, boot protocol where there is one, then the first read of each endpoint
        self.set_request(slot, 0x00, 9, ifs[0].config_value as u16, 0)?;
        for n in boot_ifaces {
            let _ = self.set_request(slot, 0x21, 0x0B, 0, n as u16); // SET_PROTOCOL: boot
        }
        let (mut kb, mut pt) = (0, 0);
        for e in eps.iter_mut() {
            match e.kind {
                Kind::Keyboard(_) => kb += 1,
                Kind::Pointer { .. } => pt += 1,
                Kind::Hub => {}
            }
            self.queue_read(slot, e);
        }
        self.keyboards += kb;
        self.pointers += pt;
        println!("[usb] device attached: {kb} keyboard(s), {pt} pointer(s)");
        let di = self.idx(slot)?;
        self.devs[di].eps = eps;
        Ok(())
    }

    // ---- hubs ----------------------------------------------------------------------------------------------------------------------------

    fn start_hub(&mut self, slot: u8, speed: u32, h: metal_usb::HubIface) -> Result<(), &'static str> {
        let di = self.idx(slot)?;
        if self.devs[di].tier >= 3 || speed >= 4 {
            return Err("hub too deep, or a SuperSpeed hub");
        }
        let hd = self.read_ctrl(slot, 0xA0, 6, 0x29 << 8, 0, 9)?;
        let info: HubInfo = hub_descriptor(&hd).ok_or("bad hub descriptor")?;
        let ep = self.new_ep(di, 0, (h.ep_addr & 0xF) * 2 + 1, h.ep_max_packet, h.ep_interval, Kind::Hub);
        // (the transaction translator's think time only matters for a high-speed hub)
        self.configure(slot, core::slice::from_ref(&ep), Some((info.ports as u32, if speed == 3 { info.think_time as u32 } else { 0 })))?;
        self.set_request(slot, 0x00, 9, h.config_value as u16, 0)?;
        let mut ep = ep;
        self.devs[di].hub = Some(HubState { ports: info.ports as u32, children: [0; 16] });
        println!("[usb] hub attached: {} ports", info.ports);
        self.queue_read(slot, &mut ep);
        let di = self.idx(slot)?;
        self.devs[di].eps.push(ep);
        // power every port, give it time, then look at what is plugged in
        for p in 1..=info.ports as u16 {
            let _ = self.set_request(slot, 0x23, 3, 8, p); // SET_FEATURE PORT_POWER
        }
        sleep_us(info.power_good_ms as u64 * 1000 + 20_000);
        for p in 1..=info.ports as u32 {
            self.hub_port_event(slot, p);
        }
        Ok(())
    }

    fn hub_port_status(&mut self, slot: u8, port: u32) -> Result<metal_usb::PortStatus, &'static str> {
        let d = self.read_ctrl(slot, 0xA3, 0, 0, port as u16, 4)?;
        port_status(&d).ok_or("short port status")
    }

    /// Something may have happened at downstream port `port` of the hub in `slot`: see what, and start or forget the device there.
    fn hub_port_event(&mut self, slot: u8, port: u32) {
        let Ok(st) = self.hub_port_status(slot, port) else { return };
        if st.connect_changed {
            let _ = self.set_request(slot, 0x23, 1, 16, port as u16); // CLEAR_FEATURE C_PORT_CONNECTION
        }
        let Ok(di) = self.idx(slot) else { return };
        let child = self.devs[di].hub.as_ref().map_or(0, |h| h.children[port as usize % 16]);
        if !st.connected {
            if child != 0 {
                println!("[usb] hub port {port}: device removed");
                self.remove_slot(child);
            }
            return;
        }
        if child != 0 {
            return;
        }
        if let Err(e) = self.hub_connect(slot, port) {
            println!("[usb] hub port {port}: {e}");
        }
    }

    fn hub_connect(&mut self, slot: u8, port: u32) -> Result<(), &'static str> {
        let _ = self.set_request(slot, 0x23, 3, 4, port as u16); // SET_FEATURE PORT_RESET
        let end = ticks() + 60;
        let mut st = self.hub_port_status(slot, port)?;
        while !st.reset_changed && ticks() < end {
            sleep(1);
            st = self.hub_port_status(slot, port)?;
        }
        let _ = self.set_request(slot, 0x23, 1, 20, port as u16); // CLEAR_FEATURE C_PORT_RESET
        let st = self.hub_port_status(slot, port)?;
        if !st.connected || !st.enabled {
            return Err("did not come up after reset");
        }
        let di = self.idx(slot)?;
        let (root, route, tier, hub_speed) = (self.devs[di].root_port, self.devs[di].route, self.devs[di].tier, self.devs[di].speed);
        let child_route = route | port << (4 * tier);
        let child = self.enumerate(root, st.speed as u32, child_route, tier + 1, Some((slot, port, hub_speed == 3)))?;
        let di = self.idx(slot)?;
        if let Some(h) = self.devs[di].hub.as_mut() {
            h.children[port as usize % 16] = child;
        }
        Ok(())
    }

    // ---- input ---------------------------------------------------------------------------------------------------------------------------

    fn queue_read(&self, slot: u8, e: &mut Ep) {
        dma::flush(e.buf_va, 64);
        e.ring.push(e.buf_pa, e.len as u32, T_NORMAL << 10 | 1 << 5 | 1 << 2); // IOC, interrupt on short packet
        w32(self.db + 4 * slot as usize, e.dci as u32);
    }

    /// Take what the controller has reported: decode every finished read, give the reports to `sink`, and start the next read; follow devices coming and
    /// going; repeat a held key.
    pub fn poll(&mut self, sink: &mut dyn Sink) {
        let mut events = core::mem::take(&mut self.stash);
        while let Some(e) = self.next_event() {
            events.push(e);
        }
        let mut ports: Vec<u32> = Vec::new();
        let mut hubs: Vec<(u8, u32)> = Vec::new();
        for e in events {
            let ty = e[3] >> 10 & 0x3F;
            if ty == T_PORT_CHANGE {
                let p = e[0] >> 24;
                if p >= 1 && !ports.contains(&p) && ports.len() < 16 {
                    ports.push(p);
                }
                continue;
            }
            if ty != T_TRANSFER_EVENT {
                continue;
            }
            let (slot, dci, cc) = ((e[3] >> 24) as u8, (e[3] >> 16 & 0x1F) as u8, e[2] >> 24);
            let Some(di) = self.devs.iter().position(|d| d.slot == slot) else { continue };
            let Some(ei) = self.devs[di].eps.iter().position(|x| x.dci == dci) else { continue };
            let mut ep = self.devs[di].eps.swap_remove(ei);
            if cc == CC_SUCCESS || cc == CC_SHORT {
                let got = ep.len.saturating_sub((e[2] & 0xFF_FFFF) as usize);
                dma::flush(ep.buf_va, 64);
                let mut r = [0u8; 64];
                for (i, b) in r.iter_mut().enumerate().take(got.min(64)) {
                    // SAFETY: the report buffer is inside the shared memory (64 bytes at `buf_va`); the controller wrote it.
                    *b = unsafe { read_volatile((ep.buf_va + i) as *const u8) };
                }
                if matches!(ep.kind, Kind::Hub) {
                    let ports = self.devs[di].hub.as_ref().map_or(0, |h| h.ports);
                    for p in 1..=ports.min(15) {
                        if r[p as usize / 8] >> (p % 8) & 1 != 0 && hubs.len() < 32 {
                            hubs.push((slot, p));
                        }
                    }
                } else {
                    self.deliver(&mut ep, &r[..got.min(64)], sink);
                }
            }
            if cc != 6 && cc != 3 {
                // (not a stall, not a babble: read again)
                self.queue_read(slot, &mut ep);
            }
            self.devs[di].eps.push(ep);
        }
        for p in ports {
            self.root_port_change(p);
        }
        for (slot, p) in hubs {
            if self.idx(slot).is_ok() {
                self.hub_port_event(slot, p);
            }
        }
        if let Some(code) = self.held {
            let now = ticks();
            if now >= self.next_repeat {
                if self.log {
                    println!("[usb] key {code} repeat");
                }
                sink.key(code, 2);
                self.next_repeat = now + REPEAT_EVERY;
            }
        }
    }

    fn deliver(&mut self, ep: &mut Ep, r: &[u8], sink: &mut dyn Sink) {
        let log = self.log;
        match &mut ep.kind {
            Kind::Hub => {}
            Kind::Keyboard(k) => {
                let mut changes: Vec<(u16, u32)> = Vec::new();
                k.report(r, |code, value| changes.push((code, value)));
                for (code, value) in changes {
                    if log {
                        println!("[usb] key {code} {}", if value != 0 { "down" } else { "up" });
                    }
                    if value != 0 && repeats(code) {
                        self.held = Some(code);
                        self.next_repeat = ticks() + REPEAT_DELAY;
                    } else if value == 0 && self.held == Some(code) {
                        self.held = None;
                    }
                    sink.key(code, value);
                }
            }
            Kind::Pointer { layout, buttons } => {
                let Some(rep) = layout.report(r) else { return };
                for (axis, field, code, pos) in [(rep.x, layout.x, ABS_X, true), (rep.y, layout.y, ABS_Y, false)] {
                    let (Some(v), Some(f)) = (axis, field) else { continue };
                    let value = if f.relative {
                        let p = if pos { &mut self.px } else { &mut self.py };
                        *p = (*p + v * 48).clamp(0, 32767);
                        *p
                    } else {
                        Pointer::scale_abs(&f, v)
                    };
                    if pos {
                        self.px = value;
                    } else {
                        self.py = value;
                    }
                    sink.pointer(EV_ABS, code, value as u32);
                }
                for b in 0..3u8 {
                    let (was, is) = (*buttons >> b & 1, rep.buttons >> b & 1);
                    if was != is {
                        if log {
                            println!("[usb] button {b} {}", if is != 0 { "down" } else { "up" });
                        }
                        sink.pointer(EV_KEY, BTN_LEFT + b as u16, is as u32);
                    }
                }
                *buttons = rep.buttons;
                if rep.wheel != 0 {
                    sink.pointer(EV_REL, REL_WHEEL, rep.wheel as u32);
                }
                if log && (rep.x.is_some() || rep.y.is_some()) {
                    println!("[usb] pointer at {} {}", self.px, self.py);
                }
                sink.pointer(EV_SYN, 0, 0);
            }
        }
    }
}
