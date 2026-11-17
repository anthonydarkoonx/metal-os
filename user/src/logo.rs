//! The `fetch` logo: a chip on a PCB, drawn as pixel lines in integer arithmetic.
//!
//! An IC package with a die inside it, pins on all four sides, and traces that fan out from every pin with a 45 degree bend to a
//! square pad. One side is described once and rotated by 90 degrees three times. Every pixel write is bounds-checked.

/// The logo is a `SIZE` x `SIZE` pixel square.
pub const SIZE: usize = 152;
const C: i32 = 76;
/// The package spans C-HALF..C+HALF; the die is inset from it.
const HALF: i32 = 36;
const DIE: i32 = 22;
const CORE: i32 = 12;
const PINS: i32 = 8;
const PITCH: i32 = 8;
const STUB: i32 = 6;
/// Pads sit at this distance from the centre, and the traces fan out to PAD_PITCH between pads.
const PAD_X: i32 = C - 66;
const PAD_PITCH: i32 = 14;

fn plot(fb: &mut [u32], width: usize, ox: usize, oy: usize, x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= SIZE as i32 || y >= SIZE as i32 {
        return;
    }
    if let Some(p) = fb.get_mut((oy + y as usize) * width + ox + x as usize) {
        *p = color;
    }
}

/// Bresenham.
fn line(fb: &mut [u32], width: usize, ox: usize, oy: usize, a: (i32, i32), b: (i32, i32), color: u32) {
    let (mut x, mut y) = a;
    let (dx, dy) = ((b.0 - x).abs(), -(b.1 - y).abs());
    let (sx, sy) = (if x < b.0 { 1 } else { -1 }, if y < b.1 { 1 } else { -1 });
    let mut err = dx + dy;
    loop {
        plot(fb, width, ox, oy, x, y, color);
        if (x, y) == b {
            return;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// A square outline centred on the middle of the logo, `half` pixels from the centre to each edge.
fn square(fb: &mut [u32], width: usize, ox: usize, oy: usize, cx: i32, cy: i32, half: i32, color: u32) {
    let (l, r, t, b) = (cx - half, cx + half, cy - half, cy + half);
    for (p, q) in [((l, t), (r, t)), ((r, t), (r, b)), ((r, b), (l, b)), ((l, b), (l, t))] {
        line(fb, width, ox, oy, p, q, color);
    }
}

/// Turn a point 90 degrees about the centre, `n` times.
fn turn(p: (i32, i32), n: u32) -> (i32, i32) {
    let mut p = p;
    for _ in 0..n {
        p = (2 * C - p.1, p.0);
    }
    p
}

fn seg(fb: &mut [u32], width: usize, ox: usize, oy: usize, a: (i32, i32), b: (i32, i32), n: u32, color: u32) {
    line(fb, width, ox, oy, turn(a, n), turn(b, n), color);
}

/// Draw the chip with its top-left corner at pixel (`ox`, `oy`) of the `width`-pixel-wide buffer `fb`.
/// `grid`: the traces, `rim`: the package and pins, `land`: the die and the pads.
pub fn draw(fb: &mut [u32], width: usize, ox: usize, oy: usize, grid: u32, rim: u32, land: u32) {
    // the package, the die, the core, and the corners joining package to die (a little depth)
    square(fb, width, ox, oy, C, C, HALF, rim);
    square(fb, width, ox, oy, C, C, DIE, land);
    square(fb, width, ox, oy, C, C, CORE, land);
    for (sx, sy) in [(-1, -1), (1, -1), (1, 1), (-1, 1)] {
        line(fb, width, ox, oy, (C + sx * HALF, C + sy * HALF), (C + sx * DIE, C + sy * DIE), grid);
        line(fb, width, ox, oy, (C + sx * DIE, C + sy * DIE), (C + sx * CORE, C + sy * CORE), grid);
    }
    // pin 1: a small diamond in the top-left corner of the package
    let (px, py) = (C - HALF + 5, C - HALF + 5);
    for (a, b) in [((px - 2, py), (px, py - 2)), ((px, py - 2), (px + 2, py)), ((px + 2, py), (px, py + 2)), ((px, py + 2), (px - 2, py))] {
        line(fb, width, ox, oy, a, b, land);
    }
    for side in 0..4 {
        for i in 0..PINS {
            let y = C + (2 * i - PINS + 1) * PITCH / 2; // the pin's row
            let t = C + (2 * i - PINS + 1) * PAD_PITCH / 2; // the row of its pad
            let (edge, tip) = (C - HALF, C - HALF - STUB);
            seg(fb, width, ox, oy, (edge, y), (tip, y), side, rim);
            // trace: a short run out, a 45 degree fan toward the pad row, then straight out to the pad
            let dy = t - y;
            let bend = tip - 3;
            let end = bend - dy.abs();
            seg(fb, width, ox, oy, (tip, y), (bend, y), side, grid);
            seg(fb, width, ox, oy, (bend, y), (end, t), side, grid);
            seg(fb, width, ox, oy, (end, t), (PAD_X + 2, t), side, grid);
            // the pad: a small square
            let (a, b) = (turn((PAD_X - 2, t - 2), side), turn((PAD_X + 2, t + 2), side));
            square(fb, width, ox, oy, (a.0 + b.0) / 2, (a.1 + b.1) / 2, 2, land);
        }
    }
}
