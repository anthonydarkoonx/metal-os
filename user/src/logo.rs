//! The `fetch` logo: a planet like Earth (Gliese-style) drawn as a wireframe sphere with pixel lines, in integer arithmetic.
//!
//! Latitude rings and meridians are polylines projected orthographically; only the half facing the viewer is drawn. A few coarse
//! continent outlines sit on the front. Nothing here allocates, and every pixel write is bounds-checked.

/// The logo is a `SIZE` x `SIZE` pixel square.
pub const SIZE: usize = 152;
const R: i32 = 70;
const C: i32 = (SIZE / 2) as i32;
/// Fixed point: 1000 = 1.0.
const ONE: i32 = 1000;
/// Axial tilt (rotation in the screen plane) and how far the north pole leans toward the viewer, in degrees.
const TILT: i32 = 23;
const LEAN: i32 = 20;
/// Longitude shown at the centre: the Atlantic, so both Americas and Africa are in view.
const CENTRE_LON: i32 = -20;

/// sin of an angle in 0..=90 degrees, in thousandths (a Taylor series, evaluated when the table is built).
const fn sin90(deg: usize) -> i32 {
    let x = deg as f64 * 3.141592653589793 / 180.0;
    let x2 = x * x;
    let s = x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0 * (1.0 - x2 / 110.0)))));
    (s * 1000.0 + 0.5) as i32
}

const SIN: [i32; 91] = {
    let mut t = [0; 91];
    let mut i = 0;
    while i <= 90 {
        t[i] = sin90(i);
        i += 1;
    }
    t
};

fn sin(deg: i32) -> i32 {
    let d = deg.rem_euclid(360) as usize;
    match d {
        0..=90 => SIN[d],
        91..=180 => SIN[180 - d],
        181..=270 => -SIN[d - 180],
        _ => -SIN[360 - d],
    }
}

fn cos(deg: i32) -> i32 {
    sin(deg + 90)
}

/// A pixel on the sphere's surface, or None when it is on the far side.
fn project(lat: i32, lon: i32) -> Option<(i32, i32)> {
    let lon = 90 - (lon - CENTRE_LON);
    let (x, y, z) = (cos(lat) * cos(lon) / ONE, sin(lat), cos(lat) * sin(lon) / ONE);
    // lean the north pole toward the viewer
    let (y, z) = ((y * cos(LEAN) - z * sin(LEAN)) / ONE, (y * sin(LEAN) + z * cos(LEAN)) / ONE);
    if z < 0 {
        return None;
    }
    // tilt the axis in the screen plane
    let (x, y) = ((x * cos(TILT) - y * sin(TILT)) / ONE, (x * sin(TILT) + y * cos(TILT)) / ONE);
    Some((C + x * R / ONE, C - y * R / ONE))
}

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

/// Draw the surface path `pts` (lat, lon in degrees), a segment at a time in steps of at most 6 degrees. Back-facing parts are skipped.
fn path(fb: &mut [u32], width: usize, ox: usize, oy: usize, pts: &[(i32, i32)], color: u32) {
    for w in pts.windows(2) {
        let ((la0, lo0), (la1, lo1)) = (w[0], w[1]);
        let n = ((la1 - la0).abs().max((lo1 - lo0).abs()) + 5) / 6;
        let n = n.max(1);
        let mut prev = project(la0, lo0);
        for i in 1..=n {
            let cur = project(la0 + (la1 - la0) * i / n, lo0 + (lo1 - lo0) * i / n);
            if let (Some(p), Some(q)) = (prev, cur) {
                line(fb, width, ox, oy, p, q, color);
            }
            prev = cur;
        }
    }
}

const NORTH_AMERICA: [(i32, i32); 12] = [(70, -165), (60, -140), (50, -125), (30, -115), (20, -105), (15, -90), (8, -80), (25, -80), (45, -65), (60, -65), (70, -90), (70, -165)];
const SOUTH_AMERICA: [(i32, i32); 9] = [(8, -77), (10, -62), (-5, -35), (-23, -42), (-40, -62), (-55, -70), (-40, -74), (-18, -70), (8, -77)];
const AFRICA: [(i32, i32); 11] = [(35, -6), (32, 30), (12, 43), (-5, 40), (-25, 35), (-35, 20), (-18, 12), (5, 8), (5, -10), (20, -17), (35, -6)];
const EURASIA: [(i32, i32); 15] = [(36, -9), (48, -5), (58, 5), (70, 25), (72, 100), (65, 170), (50, 140), (30, 122), (20, 108), (8, 100), (22, 70), (25, 57), (35, 35), (40, 25), (36, -9)];

/// Draw the planet with its top-left corner at pixel (`ox`, `oy`) of the `width`-pixel-wide buffer `fb`.
/// `grid`: the latitude and longitude lines, `rim`: the outline and equator, `land`: the continents.
pub fn draw(fb: &mut [u32], width: usize, ox: usize, oy: usize, grid: u32, rim: u32, land: u32) {
    // meridians every 30 degrees, pole to pole
    for m in 0..12 {
        let lon = m * 30 - 180;
        let mut pts = [(0, 0); 19];
        for (i, p) in pts.iter_mut().enumerate() {
            *p = (i as i32 * 10 - 90, lon);
        }
        path(fb, width, ox, oy, &pts, grid);
    }
    // parallels every 30 degrees; the equator is brighter
    for lat in [-60, -30, 30, 60, 0] {
        let mut pts = [(0, 0); 37];
        for (i, p) in pts.iter_mut().enumerate() {
            *p = (lat, i as i32 * 10 - 180);
        }
        path(fb, width, ox, oy, &pts, if lat == 0 { rim } else { grid });
    }
    for shape in [&NORTH_AMERICA[..], &SOUTH_AMERICA[..], &AFRICA[..], &EURASIA[..]] {
        path(fb, width, ox, oy, shape, land);
    }
    // the limb
    let mut prev = (C + R, C);
    for a in (10..=360).step_by(5) {
        let cur = (C + R * cos(a) / ONE, C - R * sin(a) / ONE);
        line(fb, width, ox, oy, prev, cur, rim);
        prev = cur;
    }
}
