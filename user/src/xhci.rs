//! USB host controller (xHCI) driver for the input driver task (role 14): finds keyboards and pointing devices on the controller's ports and hands their
//! reports to `kbd.rs` as the same key and pointer events the virtio input devices produce.
//!
//! Everything the device and driver share (rings, contexts, report buffers) is one DMA object the kernel gave this task (`OBJ_USB_DMA`); the IOMMU lets the
//! controller reach nothing else. Nothing waits for an interrupt: like the virtio input devices, the controller is polled when someone asks for input.
//!
//! What it does not do: hubs (devices must sit on a root port), hot plug (ports are looked at once, at start), and key repeat (a held key reports once; the
//! virtio keyboards repeat because the host does).
use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};
use metal_driver_rt::dma;
use metal_driver_rt::*;
use metal_usb::{hid_interfaces, xhci_interval, HidIface, Keyboard, Pointer};

/// Where this task maps the controller's registers and shared memory (a handle in the big window for the memory: it is more than 64 KiB's worth of handle).
const MMIO_H: usize = 0x8027_0000;
const DMA_H: usize = 0xA010_0000;
/// Capability slots (see `devices.rs`).
const SLOT_MMIO: u64 = 7;
const SLOT_DMA: u64 = 8;

// layout of the shared memory (the kernel makes it USB_DMA_PAGES = 32 pages)
const O_DCBAA: usize = 0x0000; // device context base address array: 256 entries
const O_SCRATCH_ARRAY: usize = 0x0800;
const O_CMD: usize = 0x1000; // command ring (64 TRBs used)
const O_EVT: usize = 0x2000; // event ring (256 TRBs)
const O_ERST: usize = 0x3000;
const O_INPUT: usize = 0x4000; // input context (also the one for every command: commands are one at a time)
const O_DEV0: usize = 0x8000; // per device: 4 pages
const DEV_STRIDE: usize = 0x4000;
const MAX_DEVS: usize = 4;
const O_SCRATCH: usize = 0x18000; // scratchpad buffers: up to 8 pages
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
}

/// One interrupt IN endpoint being read.
struct Ep {
    dci: u8,
    ring: Ring,
    buf_va: usize,
    buf_pa: u64,
    len: usize,
    iface: HidIface,
    kind: Kind,
}

struct Dev {
    slot: u8,
    va: usize,
    pa: u64,
    ctrl: Ring,
    eps: Vec<Ep>,
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
    /// Pointer position (0..32767) for relative devices.
    px: i32,
    py: i32,
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
        w32(op + 0x38, max_slots.min(8)); // CONFIG
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
            px: 16384,
            py: 16384,
            log,
            keyboards: 0,
            pointers: 0,
        };
        for port in 1..=max_ports.min(16) {
            if u.devs.len() < MAX_DEVS {
                if let Err(e) = u.port(port) {
                    println!("[usb] port {port}: {e}");
                }
            }
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

    /// A control transfer on endpoint 0 of `d`; `data` is (physical address, length, device-to-host) of the data stage, if any.
    fn control(&mut self, di: usize, setup: [u8; 8], data: Option<(u64, usize, bool)>) -> Result<(), &'static str> {
        let slot = self.devs[di].slot;
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

    /// GET_DESCRIPTOR-like read into the device's control buffer; the bytes.
    fn read_ctrl(&mut self, di: usize, request_type: u8, request: u8, value: u16, index: u16, len: usize) -> Result<Vec<u8>, &'static str> {
        let (va, pa) = (self.devs[di].va + 0x3000, self.devs[di].pa + 0x3000);
        let len = len.min(512);
        dma::flush(va, len);
        let setup = [request_type, request, value as u8, (value >> 8) as u8, index as u8, (index >> 8) as u8, len as u8, (len >> 8) as u8];
        self.control(di, setup, Some((pa, len, true)))?;
        dma::flush(va, len);
        let mut out = alloc::vec![0u8; len];
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: `va..va+len` is the device's control buffer inside the shared memory (len <= 512).
            *b = unsafe { read_volatile((va + i) as *const u8) };
        }
        Ok(out)
    }

    fn set_request(&mut self, di: usize, request_type: u8, request: u8, value: u16, index: u16) -> Result<(), &'static str> {
        let setup = [request_type, request, value as u8, (value >> 8) as u8, index as u8, (index >> 8) as u8, 0, 0];
        self.control(di, setup, None)
    }

    /// Reset port `port` and, if something is connected, address it and start its HID interfaces.
    fn port(&mut self, port: u32) -> Result<(), &'static str> {
        const CHANGES: u32 = 0x00FE_0000; // the write-1-to-clear change bits
        const PED: u32 = 1 << 1;
        let reg = self.port_reg(port);
        let ps = r32(reg);
        if ps & 1 == 0 {
            return Ok(()); // nothing connected
        }
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
        let speed = ps >> 10 & 0xF;
        // slot
        let (cc, slot) = self.command(0, T_ENABLE_SLOT << 10)?;
        if cc != CC_SUCCESS || slot == 0 || slot > 8 {
            return Err("no device slot");
        }
        let di = self.devs.len();
        let base = O_DEV0 + di * DEV_STRIDE;
        let (va, pa) = (self.dma_va + base, self.dma_pa + base as u64);
        // address device: slot context, endpoint 0's context
        let (ic, csz) = (self.input_ctx(), self.csz);
        // SAFETY: the input context page is inside the shared memory; commands are one at a time, so nothing else uses it now.
        unsafe { core::ptr::write_bytes(ic as *mut u8, 0, 4096) };
        let mps: u32 = match speed {
            4 | 5 => 512,
            3 => 64,
            _ => 8,
        };
        w32(ic + 4, 0b11); // add: slot, endpoint 0
        w32(ic + csz, 1 << 27 | speed << 20);
        w32(ic + csz + 4, port << 16);
        w32(ic + 2 * csz + 4, 3 << 1 | 4 << 3 | mps << 16); // CErr 3, control endpoint
        w64(ic + 2 * csz + 8, pa + 0x1000 | 1);
        w32(ic + 2 * csz + 16, 8);
        dma::clean(ic, 4096);
        w64(self.dma_va + O_DCBAA + 8 * slot as usize, pa);
        dma::clean(self.dma_va + O_DCBAA, 8 * 16);
        dma::flush(va, 4096);
        self.devs.push(Dev { slot: slot as u8, va, pa, ctrl: Ring::new(va + 0x1000, pa + 0x1000), eps: Vec::new() });
        let (cc, _) = self.command(self.dma_pa + O_INPUT as u64, T_ADDRESS << 10 | slot << 24)?;
        if cc != CC_SUCCESS {
            self.devs.pop();
            return Err("address device failed");
        }
        // the configuration descriptor, and which HID interfaces it has
        let head = self.read_ctrl(di, 0x80, 6, 2 << 8, 0, 9)?;
        let total = u16::from_le_bytes([head[2], head[3]]) as usize;
        let config = self.read_ctrl(di, 0x80, 6, 2 << 8, 0, total.clamp(9, 512))?;
        let mut ifs = [HidIface::default(); 2];
        let n = hid_interfaces(&config, &mut ifs);
        if n == 0 {
            self.devs.pop();
            return Err("not an input device");
        }
        self.start_interfaces(di, port, speed, &ifs[..n])
    }

    fn start_interfaces(&mut self, di: usize, _port: u32, speed: u32, ifs: &[HidIface]) -> Result<(), &'static str> {
        let (slot, base_va, base_pa) = (self.devs[di].slot, self.devs[di].va, self.devs[di].pa);
        // what each interface is
        let mut eps: Vec<Ep> = Vec::new();
        for (k, i) in ifs.iter().enumerate() {
            let kind = if i.subclass == 1 && i.protocol == 1 {
                Kind::Keyboard(Keyboard::new())
            } else if i.subclass == 1 && i.protocol == 2 {
                Kind::Pointer { layout: metal_usb::boot_mouse(), buttons: 0 }
            } else {
                // a descriptor tells where a pointer's fields are
                let desc = self.read_ctrl(di, 0x81, 6, 0x22 << 8, i.number as u16, i.report_len as usize)?;
                match metal_usb::parse_pointer(&desc) {
                    Some(layout) => Kind::Pointer { layout, buttons: 0 },
                    None => continue,
                }
            };
            let dci = (i.ep_addr & 0xF) * 2 + 1; // IN endpoint
            let len = (i.ep_max_packet as usize).clamp(4, 64);
            let ring_va = base_va + 0x2000 + k * 0x400;
            eps.push(Ep { dci, ring: Ring::new(ring_va, base_pa + 0x2000 + k as u64 * 0x400), buf_va: base_va + 0x3400 + k * 0x40, buf_pa: base_pa + 0x3400 + k as u64 * 0x40, len, iface: *i, kind });
        }
        if eps.is_empty() {
            return Err("no keyboard or pointer interface");
        }
        // Configure Endpoint: the slot context as the controller has it, with the new endpoints added
        let (ic, csz) = (self.input_ctx(), self.csz);
        // SAFETY: the input context page is inside the shared memory, not in use (commands are one at a time).
        unsafe { core::ptr::write_bytes(ic as *mut u8, 0, 4096) };
        dma::flush(base_va, csz);
        let max_dci = eps.iter().map(|e| e.dci).max().unwrap_or(1) as u32;
        let mut add = 1u32;
        for off in (0..csz).step_by(4) {
            w32(ic + csz + off, r32(base_va + off)); // the slot context out of the output context
        }
        w32(ic + csz, r32(ic + csz) & !(0x1F << 27) | max_dci << 27);
        for e in eps.iter() {
            let at = ic + (e.dci as usize + 1) * csz;
            let mp = e.iface.ep_max_packet as u32 & 0x7FF;
            add |= 1 << e.dci;
            w32(at, (xhci_interval(speed as u8, e.iface.ep_interval) as u32) << 16);
            w32(at + 4, 3 << 1 | 7 << 3 | mp << 16); // CErr 3, interrupt IN
            w64(at + 8, e.ring.pa | 1);
            w32(at + 16, mp | mp << 16);
        }
        w32(ic + 4, add);
        dma::clean(ic, 4096);
        let (cc, _) = self.command(self.dma_pa + O_INPUT as u64, T_CONFIGURE << 10 | (slot as u32) << 24)?;
        if cc != CC_SUCCESS {
            return Err("configure endpoint failed");
        }
        // the device: configuration, boot protocol where there is one, then the first read of each endpoint
        self.set_request(di, 0x00, 9, ifs[0].config_value as u16, 0)?;
        for i in eps.iter().map(|e| e.iface).filter(|i| i.subclass == 1) {
            let _ = self.set_request(di, 0x21, 0x0B, 0, i.number as u16); // SET_PROTOCOL: boot
        }
        for e in eps.iter_mut() {
            match e.kind {
                Kind::Keyboard(_) => self.keyboards += 1,
                Kind::Pointer { .. } => self.pointers += 1,
            }
            self.queue_read(slot, e);
        }
        self.devs[di].eps = eps;
        Ok(())
    }

    fn queue_read(&self, slot: u8, e: &mut Ep) {
        dma::flush(e.buf_va, 64);
        e.ring.push(e.buf_pa, e.len as u32, T_NORMAL << 10 | 1 << 5 | 1 << 2); // IOC, interrupt on short packet
        w32(self.db + 4 * slot as usize, e.dci as u32);
    }

    /// Take what the controller has reported: decode every finished read, give the reports to `sink`, and start the next read.
    pub fn poll(&mut self, sink: &mut dyn Sink) {
        let mut events = core::mem::take(&mut self.stash);
        while let Some(e) = self.next_event() {
            events.push(e);
        }
        for e in events {
            if e[3] >> 10 & 0x3F != T_TRANSFER_EVENT {
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
                self.deliver(&mut ep, &r[..got.min(64)], sink);
            }
            if cc != 6 && cc != 3 {
                // (not a stall, not a babble: read again)
                self.queue_read(slot, &mut ep);
            }
            self.devs[di].eps.push(ep);
        }
    }

    fn deliver(&mut self, ep: &mut Ep, r: &[u8], sink: &mut dyn Sink) {
        let log = self.log;
        match &mut ep.kind {
            Kind::Keyboard(k) => k.report(r, |code, value| {
                if log {
                    println!("[usb] key {code} {}", if value != 0 { "down" } else { "up" });
                }
                sink.key(code, value);
            }),
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
