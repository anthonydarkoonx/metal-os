//! Icons drawn as vector shapes (anti-aliased), on a 240-unit grid scaled to whatever size is asked for. Ring-shaped icons (gear, moon,
//! battery outline) are one polygon whose inner contour runs the other way: the even-odd fill leaves the hole open, so they work over
//! any backdrop.
use crate::{Canvas, Pt, Rect};
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Play,
    Pause,
    ChevronRight,
    ChevronLeft,
    ChevronDown,
    Plus,
    Minus,
    Check,
    Close,
    Search,
    Sparkle,
    Heart,
    Sun,
    Moon,
    Arrow,
    Wifi,
    Bluetooth,
    Airplane,
    Rotate,
    Bell,
    DoNotDisturb,
    Battery,
    Gear,
    Brush,
    Clock,
    Terminal,
    Volume,
    Programs,
}

const COS: [i32; 32] = [1024, 1004, 946, 851, 724, 569, 392, 200, 0, -200, -392, -569, -724, -851, -946, -1004, -1024, -1004, -946, -851, -724, -569, -392, -200, 0, 200, 392, 569, 724, 851, 946, 1004];
const SIN: [i32; 32] = [0, 200, 392, 569, 724, 851, 946, 1004, 1024, 1004, 946, 851, 724, 569, 392, 200, 0, -200, -392, -569, -724, -851, -946, -1004, -1024, -1004, -946, -851, -724, -569, -392, -200];

/// Points on a circle of radius `r` around `(cx, cy)` (grid units), from step `a` to step `b` of 32 (inclusive; `b` may exceed 32).
fn arc(cx: i32, cy: i32, r: i32, a: i32, b: i32) -> Vec<(i32, i32)> {
    (a..=b).map(|i| (cx + r * COS[i.rem_euclid(32) as usize] / 1024, cy + r * SIN[i.rem_euclid(32) as usize] / 1024)).collect()
}

/// An outer contour and an inner one, joined into one polygon that the even-odd rule fills as a ring.
fn ring(outer: Vec<(i32, i32)>, inner: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    let mut v = outer;
    let first = v[0];
    v.extend(inner.into_iter().rev());
    v.push(first);
    v
}

/// Draw `icon` filling `r` (a square), in `color`.
pub fn draw(c: &mut Canvas, icon: Icon, r: Rect, color: u32, alpha: u32) {
    let s = r.w.min(r.h).max(1);
    let (ox, oy) = (r.x + (r.w - s) / 2, r.y + (r.h - s) / 2);
    let p = |x: i32, y: i32| -> Pt { ((ox * 16) + x * s * 16 / 240, (oy * 16) + y * s * 16 / 240) };
    let pts = |v: &[(i32, i32)]| -> Vec<Pt> { v.iter().map(|&(x, y)| p(x, y)).collect() };
    let w = (s * 16 * 22 / 240).max(24); // stroke 2.2 units, at least 1.5 px
    let poly = |pts: &[(i32, i32)], c: &mut Canvas| {
        for pair in pts.windows(2) {
            c.line(p(pair[0].0, pair[0].1), p(pair[1].0, pair[1].1), w, color, alpha);
        }
    };
    let disc = |cx: i32, cy: i32, rad: i32, c: &mut Canvas| c.fill_disc(p(cx, cy), s * 16 * rad / 240, color, alpha);
    match icon {
        Icon::Play => c.fill_poly(&pts(&[(80, 45), (80, 195), (195, 120)]), color, alpha),
        Icon::Pause => {
            c.fill_round(Rect::new(ox + s * 65 / 240, oy + s * 45 / 240, (s * 40 / 240).max(2), s * 150 / 240), (s * 20 / 240).max(1), color, alpha);
            c.fill_round(Rect::new(ox + s * 135 / 240, oy + s * 45 / 240, (s * 40 / 240).max(2), s * 150 / 240), (s * 20 / 240).max(1), color, alpha);
        }
        Icon::ChevronRight => poly(&[(90, 50), (160, 120), (90, 190)], c),
        Icon::ChevronLeft => poly(&[(150, 50), (80, 120), (150, 190)], c),
        Icon::ChevronDown => poly(&[(50, 90), (120, 160), (190, 90)], c),
        Icon::Plus => {
            poly(&[(120, 50), (120, 190)], c);
            poly(&[(50, 120), (190, 120)], c);
        }
        Icon::Minus => poly(&[(50, 120), (190, 120)], c),
        Icon::Check => poly(&[(50, 125), (100, 175), (190, 70)], c),
        Icon::Close => {
            poly(&[(60, 60), (180, 180)], c);
            poly(&[(180, 60), (60, 180)], c);
        }
        Icon::Search => {
            poly(&arc(108, 108, 58, 0, 32), c);
            poly(&[(150, 150), (200, 200)], c);
        }
        Icon::Sparkle => c.fill_poly(&pts(&[(120, 25), (143, 97), (215, 120), (143, 143), (120, 215), (97, 143), (25, 120), (97, 97)]), color, alpha),
        Icon::Heart => {
            disc(85, 90, 50, c);
            disc(155, 90, 50, c);
            c.fill_poly(&pts(&[(38, 105), (202, 105), (120, 205)]), color, alpha);
        }
        Icon::Sun => {
            disc(120, 120, 46, c);
            for i in 0..8 {
                let (cs, sn) = (COS[i * 4], SIN[i * 4]);
                c.line(p(120 + 70 * cs / 1024, 120 + 70 * sn / 1024), p(120 + 95 * cs / 1024, 120 + 95 * sn / 1024), w, color, alpha);
            }
        }
        Icon::Moon => {
            const MOON: [(i32, i32); 54] = [(199, 156), (194, 166), (188, 176), (180, 184), (172, 192), (162, 198), (152, 203), (141, 207), (129, 209), (118, 210), (107, 209), (95, 207), (84, 203), (74, 198), (64, 192), (56, 184), (48, 176), (42, 166), (37, 156), (33, 145), (31, 133), (30, 122), (31, 111), (33, 99), (37, 88), (42, 78), (48, 68), (56, 60), (64, 52), (74, 46), (84, 41), (95, 37), (98, 40), (92, 48), (88, 57), (85, 66), (83, 76), (82, 86), (83, 96), (85, 106), (88, 115), (92, 124), (98, 132), (104, 140), (112, 146), (120, 152), (129, 156), (138, 159), (148, 161), (158, 162), (168, 161), (178, 159), (187, 156), (196, 152)];
            c.fill_poly(&pts(&MOON), color, alpha);
        }
        Icon::Arrow => {
            poly(&[(45, 120), (195, 120)], c);
            poly(&[(135, 60), (195, 120), (135, 180)], c);
        }
        Icon::Wifi => {
            for rad in [50, 95, 140] {
                poly(&arc(120, 195, rad, 21, 27), c);
            }
            disc(120, 192, 16, c);
        }
        Icon::Bluetooth => {
            poly(&[(60, 75), (175, 170), (120, 215), (120, 25), (175, 70), (60, 165)], c);
        }
        Icon::Airplane => c.fill_poly(&pts(&[(120, 20), (138, 40), (138, 95), (225, 140), (225, 160), (138, 135), (138, 185), (165, 205), (165, 220), (120, 208), (75, 220), (75, 205), (102, 185), (102, 135), (15, 160), (15, 140), (102, 95), (102, 40)]), color, alpha),
        Icon::Rotate => {
            poly(&arc(120, 120, 78, 2, 28), c);
            c.fill_poly(&pts(&[(205, 60), (205, 125), (150, 95)]), color, alpha);
        }
        Icon::Bell => {
            let mut v = arc(120, 105, 62, 16, 32);
            v.extend([(182, 160), (205, 185), (35, 185), (58, 160)]);
            c.fill_poly(&pts(&v), color, alpha);
            disc(120, 40, 14, c);
            poly(&arc(120, 190, 24, 2, 14), c);
        }
        Icon::DoNotDisturb => {
            c.fill_poly(&pts(&ring(arc(120, 120, 95, 0, 32), arc(120, 120, 72, 0, 32))), color, alpha);
            c.fill_round(Rect::new(ox + s * 70 / 240, oy + s * 108 / 240, s * 100 / 240, (s * 24 / 240).max(2)), (s * 12 / 240).max(1), color, alpha);
        }
        Icon::Battery => {
            // the case (a ring), the terminal, the level
            let outer = [(30, 70), (190, 70), (190, 170), (30, 170)];
            let inner = [(48, 88), (172, 88), (172, 152), (48, 152)];
            c.fill_poly(&pts(&ring(outer.to_vec(), inner.to_vec())), color, alpha);
            c.fill_poly(&pts(&[(196, 95), (214, 95), (214, 145), (196, 145)]), color, alpha);
            c.fill_poly(&pts(&[(58, 98), (140, 98), (140, 142), (58, 142)]), color, alpha);
        }
        Icon::Gear => {
            let mut outer = Vec::new();
            for i in 0..32 {
                let rad = if (i / 2) % 2 == 0 { 100 } else { 76 };
                outer.push((120 + rad * COS[i] / 1024, 120 + rad * SIN[i] / 1024));
            }
            c.fill_poly(&pts(&ring(outer, arc(120, 120, 38, 0, 32))), color, alpha);
        }
        Icon::Brush => {
            c.line(p(200, 40), p(105, 135), (s * 16 * 30 / 240).max(24), color, alpha);
            c.fill_poly(&pts(&[(95, 125), (125, 155), (95, 195), (55, 205), (30, 185), (45, 150)]), color, alpha);
        }
        Icon::Clock => {
            c.fill_poly(&pts(&ring(arc(120, 120, 95, 0, 32), arc(120, 120, 76, 0, 32))), color, alpha);
            poly(&[(120, 65), (120, 125), (160, 150)], c);
        }
        Icon::Terminal => {
            poly(&[(55, 65), (115, 120), (55, 175)], c);
            poly(&[(125, 175), (195, 175)], c);
        }
        Icon::Volume => {
            c.fill_poly(&pts(&[(35, 90), (80, 90), (125, 50), (125, 190), (80, 150), (35, 150)]), color, alpha);
            poly(&arc(125, 120, 45, 26, 38), c);
            poly(&arc(125, 120, 80, 27, 37), c);
        }
        Icon::Programs => {
            for (gx, gy) in [(55, 55), (120, 55), (185, 55), (55, 120), (120, 120), (185, 120), (55, 185), (120, 185), (185, 185)] {
                disc(gx, gy, 18, c);
            }
        }
    }
}
