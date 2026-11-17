//! The conductor's display tests and the colour-bar test card the screen task draws for them (the driver itself is `gpu.rs`).
use crate::gpu::{H, W};
use crate::proto::gpu::*;
use crate::*;

// ---------------------------------------------------------------------------
// the conductor's display tests; the pixels are drawn by the screen task (info.rs)
// ---------------------------------------------------------------------------
/// Colour bars (white, yellow, cyan, green, magenta, red, blue, black), a grey ramp, and a title.
/// # Safety
/// `fb` must be the mapped `W * H` word framebuffer.
pub unsafe fn draw_test_card(fb: *mut u32) {
    const BARS: [u32; 8] = [0xFFFFFF, 0xFFFF00, 0x00FFFF, 0x00FF00, 0xFF00FF, 0xFF0000, 0x0000FF, 0x000000];
    for y in 0..H {
        for x in 0..W {
            let c = if y < H * 2 / 3 {
                BARS[x / (W / 8)]
            } else if y < H * 5 / 6 {
                let v = (x * 255 / (W - 1)) as u32;
                v << 16 | v << 8 | v
            } else {
                0x000040
            };
            // SAFETY: `fb` is the W x H framebuffer mapped by `info_task` (FB_BYTES = W*H*4, checked by the gpu driver's grant); x < W, y < H.
            unsafe { write_volatile(fb.add(y * W + x), c) };
        }
    }
    // SAFETY: the contract: `fb` is `W * H` mapped words; the reference lives for this call only.
    font::draw_text(unsafe { core::slice::from_raw_parts_mut(fb, W * H) }, W, (W - 8 * 6 * 8) / 2, H * 5 / 6 + 28, 8, 0xFFFFFF, "METAL OS");
}

fn flush_all() -> i64 {
    let (st, r) = call(21, [GPU_FLUSH, (W as u64) << 32 | (H as u64) << 48, 0]);
    if st != 0 {
        st
    } else if r[0] != 0 {
        -1
    } else {
        0
    }
}

pub fn gpu_tests() {
    // the screen task draws a test card into the framebuffer and has the driver present it
    let (st, r) = call(23, [crate::info::INFO_CARD, 0, 0]);
    let st = if st != 0 { st } else if r[0] != 0 { -1 } else { 0 };
    println!("[client] TEST display: a test card is drawn and the driver presents it (status={st}): {}", verdict(st == 0));
    // The harness takes a screenshot as soon as it sees this line; hold the picture for it.
    println!("[gpu] frame presented");
    sleep(40);

    // a driver whose device is made to read outside its grant: the IOMMU stops it, it is restarted
    let (st, _) = call(21, [GPU_EVIL, 0, 0]);
    println!("[client] TEST IOMMU stops the display driver's device from reading outside its grant, driver restarted (status={st}): {}", verdict(st == E_DEAD));
    // the restarted driver resets the device and sets the scanout up again; the picture is still in the framebuffer
    let mut st = -1;
    for _ in 0..50 {
        st = flush_all();
        if st == 0 {
            break;
        }
        sleep(2);
    }
    println!("[client] TEST restarted display driver presents the frame again (status={st}): {}", verdict(st == 0));
    println!("[gpu] frame re-presented");

    sleep(40);

    // a display device that never answers: the driver's watchdog restarts it
    let (st, _) = call(21, [GPU_STALL, 0, 0]);
    println!("[client] TEST a display device that never answers is caught by the driver's watchdog, driver restarted (status={st}): {}", verdict(st == E_DEAD));
    let mut st = -1;
    for _ in 0..50 {
        st = flush_all();
        if st == 0 {
            break;
        }
        sleep(2);
    }
    println!("[client] TEST the display driver presents again after the watchdog restart (status={st}): {}", verdict(st == 0));
}
