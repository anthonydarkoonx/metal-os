//! The block device driver of QEMU's virtio-blk (role 4): the device window, its queue and three buffers (the file servers' and the client's) in shared memory,
//! requests answered one at a time. A machine with a different disk supplies its own `blk.rs`.
use metallium_driver_rt::virtio::*;
use metallium_driver_rt::*;

const MMIO_VA: usize = 0x8020_0000; // the device's register BAR
const RING_VA: usize = 0x8021_0000;
const BUF_VA: usize = 0x8022_0000; // 3 consecutive pages: rofs, lfs, client
const CFG_VA: usize = 0x8024_0000; // read-only copy of the device's PCI config space
const QN: u16 = 8;

// request header + status byte live in the queue page, after the rings
const OFF_HDR: usize = 0x400;
const OFF_STATUS: usize = 0x410;

struct Blk {
    pci: virtio::Pci,
    q: virtio::Queue,
    buf_pa: [u64; 3],
    sectors: u64,
}

impl Blk {
    /// One synchronous request through one of the client data buffers. Returns the device status byte.
    fn io(&mut self, write: bool, buf: usize, sector: u64, nsec: u64) -> u8 {
        self.io_at(write, self.buf_pa[buf], va(BUF_VA + buf * 0x1000), sector, nsec)
    }

    /// Like `io`, with the data buffer at an arbitrary physical address (`data_va` = where this task maps it, 0 if it
    /// does not: the cache maintenance is then skipped, which only the deliberately misbehaving test does).
    fn io_at(&mut self, write: bool, data_pa: u64, data_va: usize, sector: u64, nsec: u64) -> u8 {
        // SAFETY: the request header and status live at OFF_HDR/OFF_STATUS inside the queue page at RING_VA (mapped read-write in blk_init), above the rings (avail 0x100, used 0x200) so they overlap nothing; the device reads/writes them only after `submit`.
        unsafe {
            let hdr = (va(RING_VA) + OFF_HDR) as *mut u8;
            write_volatile(hdr as *mut u32, write as u32); // VIRTIO_BLK_T_IN=0 / OUT=1
            write_volatile(hdr.add(4) as *mut u32, 0);
            write_volatile(hdr.add(8) as *mut u64, sector);
            write_volatile((va(RING_VA) + OFF_STATUS) as *mut u8, 0xFF);
        }
        let ring_pa = self.q.pa();
        self.q.submit(&[
            virtio::Buf { pa: ring_pa + OFF_HDR as u64, va: va(RING_VA) + OFF_HDR, len: 16, dev_writes: false },
            virtio::Buf { pa: data_pa, va: data_va, len: (nsec * 512) as u32, dev_writes: !write },
            virtio::Buf { pa: ring_pa + OFF_STATUS as u64, va: va(RING_VA) + OFF_STATUS, len: 1, dev_writes: true },
        ]);
        self.wait_done(BLK_WATCHDOG_TICKS);
        // SAFETY: OFF_STATUS is inside the mapped queue page; volatile because the device wrote the byte (the used ring flush in `done` covered the cache).
        unsafe { read_volatile((va(RING_VA) + OFF_STATUS) as *const u8) }
    }

    /// Wait for the device to finish the request in flight. If it does not within `limit` ticks it is
    /// treated as dead: the driver exits, the kernel restarts it, and starting up resets the device.
    fn wait_done(&mut self, limit: u64) {
        // (the interrupt line may be shared with other devices, so a wake-up does not mean this device answered:
        // the deadline is absolute, not restarted by every wake-up)
        let deadline = ticks() + limit;
        loop {
            let left = deadline.saturating_sub(ticks());
            if left == 0 || irq_wait_timeout(6, left) == E_TIMEDOUT {
                println!("[blk] watchdog: the device did not answer within {} ms; restarting the driver", limit * 10);
                exit(1);
            }
            self.pci.ack_isr();
            if self.q.done() {
                return;
            }
        }
    }

    /// Test only: a request the device is never told about.
    fn stall(&mut self) {
        let ring_pa = self.q.pa();
        self.q.submit_quiet(&[
            virtio::Buf { pa: ring_pa + OFF_HDR as u64, va: va(RING_VA) + OFF_HDR, len: 16, dev_writes: false },
            virtio::Buf { pa: self.buf_pa[2], va: va(BUF_VA + 2 * 0x1000), len: 512, dev_writes: true },
            virtio::Buf { pa: ring_pa + OFF_STATUS as u64, va: va(RING_VA) + OFF_STATUS, len: 1, dev_writes: true },
        ]);
        self.wait_done(100);
    }
}

fn blk_init() -> Result<Blk, &'static str> {
    if mem_map(1, MMIO_VA, true) != 0 || mem_map(2, RING_VA, true) != 0 || mem_map(7, CFG_VA, false) != 0 {
        return Err("mapping failed");
    }
    // A word wait or wake on a device mapping must be refused (-5), not take the kernel down (SECURITY.md, M3).
    let (w, k) = (svc(32, va(MMIO_VA) as u64, 0, 1, 0, 0)[0] as i64, svc(33, va(MMIO_VA) as u64, 1, 0, 0, 0)[0] as i64);
    println!("[blk] TEST WAIT_WORD and WAKE_WORD on a device mapping are refused (wait {w}, wake {k}): {}", crate::verdict(w == -5 && k == -5));
    let (s1, _, bar_pages, _) = cap_info(1);
    let (s2, ring_pa, _, _) = cap_info(2);
    if s1 != 0 || s2 != 0 {
        return Err("cap_info failed");
    }
    let mut buf_pa = [0u64; 3];
    for i in 0..3 {
        // slots 3..5: buffers for rofs, lfs and the test client
        if mem_map(3 + i as u64, BUF_VA + i * 0x1000, true) != 0 {
            return Err("buffer mapping failed");
        }
        let (s, pa, _, _) = cap_info(3 + i as u64);
        if s != 0 {
            return Err("cap_info failed");
        }
        buf_pa[i] = pa;
    }
    // SAFETY: the BAR and config-space mappings the kernel gave this driver (`bar_pages` pages at MMIO_VA / CFG_VA, per its facts).
    let pci = unsafe { virtio::Pci::open(va(MMIO_VA), bar_pages as usize * 4096, va(CFG_VA), 0x1042) }?;
    pci.start()?; // resets the device: also discards any state a crashed predecessor left behind
    // The DMA page outlives a crashed driver: wipe stale avail/used indices so the
    // device cannot replay the previous instance's requests.
    // SAFETY: RING_VA is the 4096-byte queue page mapped read-write just above; nothing (device included) uses it yet because `pci.queue` has not run.
    unsafe { core::ptr::write_bytes(va(RING_VA) as *mut u8, 0, 4096) };
    let q = pci.queue(0, QN, va(RING_VA), ring_pa)?;
    pci.driver_ok();
    let sectors = pci.config32(0) as u64 | (pci.config32(4) as u64) << 32;
    Ok(Blk { pci, q, buf_pa, sectors })
}

fn buf_for(sender: u64) -> Option<usize> {
    match sender {
        5 => Some(0), // rofs-server
        6 => Some(1), // lfs-server
        0 => Some(2), // test client
        _ => None,
    }
}

pub fn blk_driver() -> ! {
    println!("[blk] driver starting");
    let mut blk = match blk_init() {
        Ok(b) => b,
        Err(e) => {
            println!("[blk] init failed: {e}");
            exit(1)
        }
    };
    println!("[blk] virtio-blk ready: {} sectors", blk.sectors);
    loop {
        let m = recv(0);
        match m.w[0] {
            BLK_READ | BLK_WRITE => {
                let (sector, nsec) = (m.w[1], m.w[2]);
                if nsec == 0 || nsec > 8 || sector.saturating_add(nsec) > blk.sectors {
                    reply([1, 0, 0]);
                    continue;
                }
                // Each client may only use its own DMA buffer; the sender id is set by the kernel.
                let Some(buf) = buf_for(m.sender) else {
                    reply([1, 0, 0]);
                    continue;
                };
                let st = blk.io(m.w[0] == BLK_WRITE, buf, sector, nsec);
                reply([st as u64, nsec * 512, 0]);
            }
            BLK_EVIL | BLK_CRASH if m.sender != 0 => {
                reply([1, 0, 0]); // fault injection is the test conductor's tool: found by the syscall fuzzer
            }
            BLK_EVIL => {
                // Ask the device to read kernel memory and write it to the disk. The kernel's
                // IOMMU confines the device to the pages this driver holds; the access faults
                // and the kernel restarts us, so this never replies.
                // The target is the page just past our last buffer: not ours, and (given how the
                // kernel allocates) the shared page the test conductor filled with a marker.
                let target = blk.buf_pa[2] + 0x1000;
                println!("[blk] misbehaving: pointing the device at memory outside my grant ({target:#x})");
                let st = blk.io_at(true, target, 0, m.w[1], 1);
                reply([st as u64, 0, 0]); // only reached if the DMA was NOT blocked
            }
            BLK_INFO => {
                reply([blk.sectors, 0, 0]);
            }
            BLK_STALL => {
                if m.sender != 0 {
                    reply([1, 0, 0]); // the conductor's test tool
                    continue;
                }
                println!("[blk] test: submitting a request without telling the device");
                blk.stall(); // exits through the watchdog; never replies
            }
            BLK_CRASH => {
                println!("[blk] simulated fault while a request is in flight");
                // SAFETY: deliberate fault injection: 0x2000 is below every mapping (images load at 0x8000_0000 or higher), so the store traps and the kernel restarts the driver; that is the test.
                unsafe { write_volatile(0x2000 as *mut u64, 0xBAD) };
            }
            _ => {
                reply([1, 0, 0]);
            }
        }
    }
}

