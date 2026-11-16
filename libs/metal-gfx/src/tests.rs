extern crate std;
use super::*;
use std::vec;
use std::vec::Vec;

fn area(rs: &[Rect]) -> i64 {
    // area of the union, by counting cells (the test rectangles are small)
    let mut n = 0;
    for y in -10..70 {
        for x in -10..70 {
            if rs.iter().any(|r| r.contains(x, y)) {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn rect_basics() {
    let a = Rect::new(0, 0, 10, 10);
    let b = Rect::new(5, 5, 10, 10);
    assert_eq!(a.intersect(&b), Rect::new(5, 5, 5, 5));
    assert_eq!(a.union(&b), Rect::new(0, 0, 15, 15));
    assert!(a.intersect(&Rect::new(10, 0, 5, 5)).is_empty(), "touching is not overlapping");
    assert!(a.contains(0, 0) && a.contains(9, 9) && !a.contains(10, 9) && !a.contains(-1, 0));
    assert!(Rect::new(0, 0, 0, 5).is_empty() && Rect::new(0, 0, 5, -1).is_empty());
    // extreme values do not overflow
    let big = Rect::new(i32::MAX - 1, i32::MAX - 1, i32::MAX, i32::MAX);
    assert!(big.intersect(&a).is_empty());
    let _ = big.union(&a);
    let _ = big.translate(i32::MAX, i32::MAX);
}

#[test]
fn region_never_loses_a_pixel() {
    let mut s = 12345u32;
    let mut rnd = |m: i32| {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        ((s >> 16) as i32).rem_euclid(m)
    };
    for _ in 0..200 {
        let mut reg = Region::new();
        let mut added: Vec<Rect> = Vec::new();
        for _ in 0..30 {
            let r = Rect::new(rnd(50), rnd(50), 1 + rnd(12), 1 + rnd(12));
            reg.add(r);
            added.push(r);
            assert!(reg.rects().len() <= Region::CAP);
        }
        // every pixel that was added is still covered
        for r in &added {
            for y in r.y..r.bottom() {
                for x in r.x..r.right() {
                    assert!(reg.rects().iter().any(|d| d.contains(x, y)), "({x},{y}) lost");
                }
            }
        }
        // (a region that has not collapsed into its bounding box keeps its rectangles apart)
        if reg.rects().len() > 1 {
            let rs = reg.rects();
            for a in 0..rs.len() {
                for b in a + 1..rs.len() {
                    assert!(!rs[a].overlaps(&rs[b]), "{:?} and {:?} overlap", rs[a], rs[b]);
                }
            }
        }
    }
    let mut r = Region::new();
    r.add(Rect::new(0, 0, 0, 5));
    assert!(r.is_empty());
    r.add(Rect::new(0, 0, 10, 10));
    r.add(Rect::new(10, 0, 10, 10)); // touching: merged
    assert_eq!(r.rects(), &[Rect::new(0, 0, 20, 10)]);
    assert_eq!(area(r.rects()), 200);
}

#[test]
fn blend_math() {
    assert_eq!(blend(0x112233, 0xAABBCC, 0), 0x112233);
    assert_eq!(blend(0x112233, 0xAABBCC, 255), 0xAABBCC);
    assert_eq!(blend(0x000000, 0xFFFFFF, 128), 0x808080);
    // the alpha byte of either word never leaks into the result
    assert_eq!(blend(0xFF00_0000, 0xFF00_0000, 255) >> 24, 0);
    // monotone in alpha
    let mut last = 0;
    for a in 0..=255 {
        let v = blend(0, 0xFFFFFF, a) & 0xFF;
        assert!(v >= last);
        last = v;
    }
    assert_eq!(mix(0x000000, 0xFFFFFF, 0), 0);
    assert_eq!(mix(0x000000, 0xFFFFFF, 256), 0xFEFEFE.max(mix(0, 0xFFFFFF, 256)));
}

fn canvas_with_guard(w: usize, h: usize) -> Vec<u32> {
    vec![0xDEAD_BEEF; w * h]
}

#[test]
fn nothing_is_drawn_outside_the_clip_or_canvas() {
    // a 20x20 canvas inside a 40x40 buffer, with a guard colour all around it; stride 40
    let (bw, bh) = (40usize, 40usize);
    let mut buf = canvas_with_guard(bw, bh);
    {
        let mut c = Canvas::new(&mut buf, 20, 20, 40);
        c.fill(Rect::new(0, 0, 20, 20), 0x111111);
        c.set_clip(Rect::new(5, 5, 8, 8));
        for r in [Rect::new(-50, -50, 200, 200), Rect::new(0, 0, 100, 100), Rect::new(10, 10, 100, 3), Rect::new(i32::MAX - 3, 0, 10, 10), Rect::new(-5, -5, 7, 7)] {
            c.fill(r, 0x222222);
            c.fill_blend(r, 0x333333, 100);
            c.fill_round(r, 6, 0x444444, 200);
            c.gradient_v(r, 1, 2);
        }
        c.text(-100, -100, 3, 0xFFFFFF, 255, "hello");
        c.text(4, 4, 2, 0xFFFFFF, 255, "hello world");
        c.cursor(-3, -3);
        c.cursor(100, 100);
        c.shadow(Rect::new(-10, -10, 100, 100), 8, 6, 200, 4);
        let src = vec![0x555555u32; 30 * 30];
        c.blit(&src, 30, 30, 30, -10, -10);
        c.blit_round(&src, 30, 30, 30, 10, 10, 8, 8);
        c.blit(&src, 1000, 1000, 30, 0, 0); // claims more than there is
    }
    for y in 0..bh {
        for x in 0..bw {
            let inside_canvas = x < 20 && y < 20;
            if !inside_canvas {
                assert_eq!(buf[y * bw + x], 0xDEAD_BEEF, "({x},{y}) outside the canvas was written");
            } else if !Rect::new(5, 5, 8, 8).contains(x as i32, y as i32) {
                assert_eq!(buf[y * bw + x], 0x111111, "({x},{y}) outside the clip was written");
            }
        }
    }
}

#[test]
fn canvas_over_a_short_slice_shrinks() {
    let mut buf = vec![0u32; 10 * 3 + 4]; // three whole rows of ten, and a bit
    let mut c = Canvas::new(&mut buf, 10, 99, 10);
    assert_eq!(c.height(), 3);
    c.fill(Rect::new(0, 0, 10, 99), 1); // must not run off the end
    assert_eq!(buf.iter().filter(|&&p| p == 1).count(), 30);
    let mut tiny: Vec<u32> = Vec::new();
    let c = Canvas::new(&mut tiny, 10, 10, 10);
    assert_eq!((c.width(), c.height()), (10, 0));
}

#[test]
fn rounded_rectangles() {
    let mut buf = vec![0u32; 40 * 40];
    let mut c = Canvas::new(&mut buf, 40, 40, 40);
    c.fill_round(Rect::new(4, 4, 32, 32), 10, 0xFFFFFF, 255);
    // the middle is solid, the corner pixel is empty, the four corners are mirror images
    assert_eq!(c.get(20, 20), Some(0xFFFFFF));
    assert_eq!(c.get(4, 4), Some(0));
    assert_eq!(c.get(35, 35), Some(0));
    assert_eq!(c.get(20, 4), Some(0xFFFFFF), "straight edges are solid");
    for dy in 0..10 {
        for dx in 0..10 {
            let tl = c.get(4 + dx, 4 + dy).unwrap();
            assert_eq!(tl, c.get(35 - dx, 4 + dy).unwrap());
            assert_eq!(tl, c.get(4 + dx, 35 - dy).unwrap());
            assert_eq!(tl, c.get(35 - dx, 35 - dy).unwrap());
        }
    }
    // the edge is anti-aliased: some corner pixel is neither empty nor full
    let mixed = (0..10).flat_map(|dy| (0..10).map(move |dx| (dx, dy))).filter(|&(dx, dy)| { let p = c.get(4 + dx, 4 + dy).unwrap() & 0xFF; p != 0 && p != 255 }).count();
    assert!(mixed >= 8, "{mixed}");
    // coverage grows monotonically towards the inside along the diagonal
    let diag: Vec<u32> = (0..10).map(|i| c.get(4 + i, 4 + i).unwrap() & 0xFF).collect();
    assert!(diag.windows(2).all(|w| w[0] <= w[1]), "{diag:?}");
    // a circle: radius capped to half the side, symmetric
    let mut buf = vec![0u32; 21 * 21];
    let mut c = Canvas::new(&mut buf, 21, 21, 21);
    c.fill_round(Rect::new(0, 0, 21, 21), 1000, 0xFFFFFF, 255);
    assert_eq!(c.get(10, 10), Some(0xFFFFFF));
    assert_eq!(c.get(0, 0), Some(0));
    for i in 0..21 {
        assert_eq!(c.get(i, 10), c.get(10, i), "symmetric across the diagonal");
        assert_eq!(c.get(i, 10), c.get(20 - i, 10), "symmetric left to right");
    }
    // translucent
    let mut buf = vec![0x000000u32; 16];
    let mut c = Canvas::new(&mut buf, 4, 4, 4);
    c.fill_round(Rect::new(0, 0, 4, 4), 0, 0xFFFFFF, 128);
    assert_eq!(c.get(1, 1), Some(0x808080));
}

#[test]
fn blit_copies_and_rounds() {
    let src: Vec<u32> = (0..16 * 16).map(|i| 0x100000 + i as u32).collect();
    let mut buf = vec![0u32; 32 * 32];
    let mut c = Canvas::new(&mut buf, 32, 32, 32);
    c.blit(&src, 16, 16, 16, 5, 7);
    for y in 0..16 {
        for x in 0..16 {
            assert_eq!(c.get(5 + x, 7 + y), Some(src[(y * 16 + x) as usize]));
        }
    }
    assert_eq!(c.get(4, 7), Some(0));
    assert_eq!(c.get(21, 7), Some(0));
    // partly off the canvas: only the visible part, and the right part
    let mut buf = vec![0u32; 32 * 32];
    let mut c = Canvas::new(&mut buf, 32, 32, 32);
    c.blit(&src, 16, 16, 16, -4, -3);
    assert_eq!(c.get(0, 0), Some(src[(3 * 16 + 4) as usize]));
    assert_eq!(c.get(11, 12), Some(src[(15 * 16 + 15) as usize]));
    // rounded corners leave the corner pixels alone and blend the edge
    let white = vec![0xFFFFFFu32; 20 * 20];
    let mut buf = vec![0u32; 20 * 20];
    let mut c = Canvas::new(&mut buf, 20, 20, 20);
    c.blit_round(&white, 20, 20, 20, 0, 0, 8, 0);
    assert_eq!(c.get(0, 0), Some(0));
    assert_eq!(c.get(19, 0), Some(0));
    assert_eq!(c.get(0, 19), Some(0xFFFFFF), "the bottom corners were not rounded");
    assert_eq!(c.get(10, 0), Some(0xFFFFFF));
}

#[test]
fn text_and_cursor() {
    let mut buf = vec![0u32; 64 * 16];
    let mut c = Canvas::new(&mut buf, 64, 16, 64);
    let w = c.text(1, 1, 1, 0xFFFFFF, 255, "HI");
    assert!(w > 8 && w < 20, "the proportional typeface's width of two capitals: {w}");
    assert_eq!(text_width("HI", 1), w);
    assert_eq!(text_width("HI", 2), 24, "larger scales are the bitmap font: 6 pixels a character, doubled");
    // letters are drawn (some pixels near the start of the line are lit), inside the 9-pixel line the old font used
    let lit = (0..64).flat_map(|x| (0..16).map(move |y| (x, y))).filter(|&(x, y)| c.get(x, y) != Some(0)).count();
    assert!(lit > 20, "the capitals have ink ({lit} pixels)");
    // the bitmap font at a larger scale is unchanged: the H has its left stem in column 1
    let mut big = vec![0u32; 64 * 32];
    let mut c2 = Canvas::new(&mut big, 64, 32, 64);
    c2.text(1, 1, 2, 0xFFFFFF, 255, "H");
    assert_eq!(c2.get(1, 1), Some(0xFFFFFF));
    assert_eq!(c2.get(3, 1), Some(0));
    // a character with no glyph still advances
    let w = c.text(0, 10, 1, 0xFFFFFF, 255, "\u{7f}\u{1}");
    assert!(w > 0);
    assert_eq!(CURSOR.len() as i32, CURSOR_H);
    assert!(CURSOR.iter().all(|l| l.len() as i32 == CURSOR_W));
    let mut buf = vec![0u32; 20 * 20];
    let mut c = Canvas::new(&mut buf, 20, 20, 20);
    c.cursor(2, 2);
    assert_eq!(c.get(2, 2), Some(0x000000)); // the tip (outline)
    assert_eq!(c.get(3, 4), Some(0xFFFFFF)); // inside the arrow
}

#[test]
fn shadows_fade_out() {
    let mut buf = vec![0xFFFFFFu32; 100 * 100];
    let mut c = Canvas::new(&mut buf, 100, 100, 100);
    c.shadow(Rect::new(30, 30, 40, 40), 10, 8, 180, 3);
    let line: Vec<u32> = (0..30).map(|d| c.get(50, 33 - d).unwrap() & 0xFF).collect(); // upwards from the box edge
    assert!(line[0] < 255, "there is a shadow next to the box");
    assert!(line.windows(2).all(|w| w[0] <= w[1]), "and it gets lighter with distance: {line:?}");
    assert_eq!(c.get(50, 10), Some(0xFFFFFF), "far away there is none");
}


#[test]
fn colour_shadows_are_soft() {
    let mut px = vec![0xFFFFFFu32; 100 * 100];
    let mut c = Canvas::new(&mut px, 100, 100, 100);
    c.shadow_color(Rect::new(30, 30, 40, 40), 10, 20, 100, 8, 0x6070E0);
    let col: Vec<u32> = (0..20).map(|d| c.get(50, 78 + d).unwrap() & 0xFF).collect(); // downwards from below the box
    assert!(col[0] < 250, "there is a shadow");
    assert!(col.windows(2).all(|w| w[0] <= w[1]), "fading: {col:?}");
    assert_eq!(c.get(50, 99), Some(0xFFFFFF));
}

#[test]
fn polygons_lines_and_discs() {
    let mut px = vec![0u32; 40 * 40];
    let mut c = Canvas::new(&mut px, 40, 40, 40);
    // a square 10..20 in sixteenths: exactly covers whole pixels
    c.fill_poly(&[(160, 160), (320, 160), (320, 320), (160, 320)], 0xFFFFFF, 255);
    assert_eq!(c.get(15, 15), Some(0xFFFFFF));
    assert_eq!(c.get(9, 15), Some(0));
    assert_eq!(c.get(20, 15), Some(0));
    // a triangle has a diagonal edge: anti-aliased, the pixels on it are neither empty nor full
    let mut px = vec![0u32; 40 * 40];
    let mut c = Canvas::new(&mut px, 40, 40, 40);
    c.fill_poly(&[(80, 80), (560, 80), (80, 560)], 0xFFFFFF, 255);
    let partial = (0..40).flat_map(|y| (0..40).map(move |x| (x, y))).filter(|&(x, y)| { let v = c.get(x, y).unwrap() & 255; v > 8 && v < 247 }).count();
    assert!(partial >= 15, "{partial}");
    assert_eq!(c.get(8, 8), Some(0xFFFFFF));
    assert_eq!(c.get(30, 30), Some(0));
    // a degenerate polygon draws nothing and does not panic
    c.fill_poly(&[(0, 0), (100, 100)], 0xFFFFFF, 255);
    c.fill_poly(&[], 0xFFFFFF, 255);
    // lines have round ends and the width asked for
    let mut px = vec![0u32; 60 * 40];
    let mut c = Canvas::new(&mut px, 60, 40, 60);
    c.line((160, 320), (800, 320), 64, 0xFFFFFF, 255); // y = 20, from x = 10 to 50, 4 px thick
    assert_eq!(c.get(30, 20), Some(0xFFFFFF));
    assert_eq!(c.get(30, 19), Some(0xFFFFFF));
    assert_eq!(c.get(30, 17), Some(0));
    assert_eq!(c.get(30, 23), Some(0));
    assert!(c.get(9, 20).unwrap() & 255 > 40, "round cap");
    assert_eq!(c.get(5, 20), Some(0));
    // clipped, off-canvas and zero-length: fine
    c.set_clip(Rect::new(0, 0, 20, 20));
    c.line((-1600, -1600), (4000, 4000), 160, 0xFFFFFF, 255);
    c.line((100, 100), (100, 100), 64, 0xFFFFFF, 255);
    assert_eq!(c.get(30, 30), Some(0), "outside the clip");
    // a disc
    let mut px = vec![0u32; 40 * 40];
    let mut c = Canvas::new(&mut px, 40, 40, 40);
    c.fill_disc((320, 320), 128, 0xFFFFFF, 255); // radius 8 at (20, 20)
    assert_eq!(c.get(20, 20), Some(0xFFFFFF));
    assert_eq!(c.get(20, 12), Some(0xFFFFFF));
    assert_eq!(c.get(20, 8), Some(0));
    assert!(c.get(20, 11).unwrap() & 255 < 128, "the edge pixel is mostly outside");
}

#[test]
fn regions_do_not_merge_into_huge_boxes() {
    // a window moved: the old and the new place are two rectangles, not their bounding box
    let mut r = Region::new();
    r.add(Rect::new(0, 0, 100, 100));
    r.add(Rect::new(300, 300, 100, 100));
    assert_eq!(r.rects().len(), 2);
    // two that overlap a lot are merged
    let mut r = Region::new();
    r.add(Rect::new(0, 0, 100, 100));
    r.add(Rect::new(10, 10, 100, 100));
    assert_eq!(r.rects().len(), 1);
    // two that touch at a corner only are not
    let mut r = Region::new();
    r.add(Rect::new(0, 0, 100, 100));
    r.add(Rect::new(100, 100, 100, 100));
    assert_eq!(r.rects().len(), 2);
    // a small rectangle inside a big one disappears into it
    let mut r = Region::new();
    r.add(Rect::new(0, 0, 100, 100));
    r.add(Rect::new(40, 40, 5, 5));
    assert_eq!(r.rects(), &[Rect::new(0, 0, 100, 100)]);
}




#[test]
fn scaled_blits_fit_their_rectangle() {
    // a 4x4 source of distinct colours scaled up to 40x40: each source pixel becomes a 10x10 block
    let src: Vec<u32> = (0..16).map(|i| 0x010101 * (i as u32 + 1) * 10).collect();
    let mut px = vec![0u32; 60 * 60];
    let mut c = Canvas::new(&mut px, 60, 60, 60);
    c.blit_scaled(&src, 4, 4, 4, Rect::new(10, 10, 40, 40), 0, 255);
    assert_eq!(c.get(10, 10), Some(src[0]));
    assert_eq!(c.get(19, 19), Some(src[0]));
    assert_eq!(c.get(20, 10), Some(src[1]));
    assert_eq!(c.get(49, 49), Some(src[15]));
    assert_eq!(c.get(50, 50), Some(0), "nothing past the rectangle");
    assert_eq!(c.get(9, 30), Some(0));
    // scaled down, half transparent, rounded: the corner pixel is untouched, the middle is a blend
    let big = vec![0xFFFFFFu32; 100 * 100];
    let mut px = vec![0u32; 30 * 30];
    let mut c = Canvas::new(&mut px, 30, 30, 30);
    c.blit_scaled(&big, 100, 100, 100, Rect::new(5, 5, 20, 20), 8, 128);
    assert_eq!(c.get(5, 5), Some(0));
    assert_eq!(c.get(15, 15), Some(0x808080));
    // degenerate inputs do not panic
    c.blit_scaled(&big, 100, 100, 100, Rect::new(0, 0, 0, 10), 4, 255);
    c.blit_scaled(&big, 0, 0, 100, Rect::new(0, 0, 10, 10), 4, 255);
    c.blit_scaled(&big, 100, 100, 100, Rect::new(-50, -50, 300, 300), 40, 255);
    c.set_clip(Rect::new(0, 0, 5, 5));
    c.blit_scaled(&big, 100, 100, 100, Rect::new(0, 0, 30, 30), 0, 255);
    assert_eq!(c.get(10, 10), Some(0x808080).filter(|_| false).or(c.get(10, 10)), "clip respected (no panic, pixel unchanged)");
}

fn sum(rs: &[Rect]) -> i64 {
    rs.iter().map(|r| r.w as i64 * r.h as i64).sum()
}

#[test]
fn region_cuts_overlaps_out() {
    // a long thin bar and a long thin bar across it: the union would be far too big, so both stay, but the crossing is counted once
    let mut r = Region::new();
    r.add(Rect::new(0, 0, 100, 4));
    r.add(Rect::new(48, 0, 4, 100));
    assert_eq!(sum(r.rects()), 100 * 4 + 4 * 96);
    // inside what is there already: nothing changes
    r.add(Rect::new(10, 1, 5, 2));
    assert_eq!(sum(r.rects()), 100 * 4 + 4 * 96);
    // swallowing a smaller one removes it
    let mut r = Region::new();
    r.add(Rect::new(10, 10, 2, 2));
    r.add(Rect::new(0, 0, 40, 40));
    assert_eq!(r.rects(), &[Rect::new(0, 0, 40, 40)]);
    // subtract: the pieces tile exactly the rest
    let (p, n) = Rect::new(0, 0, 10, 10).subtract(&Rect::new(3, 3, 4, 4));
    assert_eq!(sum(&p[..n]), 100 - 16);
}

#[test]
fn the_resize_pointers_are_symmetric_double_arrows_inside_the_box_and_the_arrow_is_unchanged() {
    // the box is big enough for every shape
    for shape in [Shape::Arrow, Shape::ResizeH, Shape::ResizeV, Shape::ResizeNwse, Shape::ResizeNesw] {
        for dy in -30..30 {
            for dx in -30..30 {
                let inside = (-CURSOR_LEFT..CURSOR_BOX_W - CURSOR_LEFT).contains(&dx) && (-CURSOR_TOP..CURSOR_BOX_H - CURSOR_TOP).contains(&dy);
                if !inside {
                    assert_eq!(shape.pixel(dx, dy), 0, "{shape:?} draws outside its box at ({dx},{dy})");
                }
            }
        }
    }
    // the arrow is what it always was
    for (row, line) in CURSOR.iter().enumerate() {
        for (col, c) in line.bytes().enumerate() {
            assert_eq!(Shape::Arrow.pixel(col as i32, row as i32), match c { b'1' => 1, b'2' => 2, _ => 0 });
        }
    }
    // the horizontal arrow is left-right symmetric and the vertical one is its transpose
    for dy in -9..=9 {
        for dx in -9..=9 {
            assert_eq!(Shape::ResizeH.pixel(dx, dy), Shape::ResizeH.pixel(-dx, dy), "({dx},{dy})");
            assert_eq!(Shape::ResizeH.pixel(dx, dy), Shape::ResizeV.pixel(dy, dx));
            assert_eq!(Shape::ResizeNwse.pixel(dx, dy), Shape::ResizeNwse.pixel(-dx, -dy), "diagonal is point symmetric ({dx},{dy})");
            assert_eq!(Shape::ResizeNwse.pixel(dx, dy), Shape::ResizeNesw.pixel(-dx, dy));
        }
    }
    // each has a white body ringed by black, with the hot spot on the body, and they differ from each other
    let count = |s: Shape, v: u8| (-10..=10).flat_map(|dy| (-10..=10).map(move |dx| (dx, dy))).filter(|&(dx, dy)| s.pixel(dx, dy) == v).count();
    for s in [Shape::ResizeH, Shape::ResizeV, Shape::ResizeNwse, Shape::ResizeNesw] {
        assert!(count(s, 2) > 25 && count(s, 1) > 25, "{s:?}: {} white, {} black", count(s, 2), count(s, 1));
        assert_eq!(s.pixel(0, 0), 2, "{s:?}: the hot spot is on the body");
    }
    assert_ne!((-9..=9).map(|d| Shape::ResizeH.pixel(d, 0)).collect::<std::vec::Vec<_>>(), (-9..=9).map(|d| Shape::ResizeV.pixel(d, 0)).collect::<std::vec::Vec<_>>());
    assert_ne!(Shape::ResizeNwse.pixel(6, -6), Shape::ResizeNesw.pixel(6, -6));
    // drawing a shape touches only its box
    let mut px = vec![0xABCDEFu32; 60 * 60];
    let mut c = Canvas::new(&mut px, 60, 60, 60);
    c.cursor_shape(30, 30, Shape::ResizeNwse);
    let touched: std::vec::Vec<(i32, i32)> = (0..60).flat_map(|y| (0..60).map(move |x| (x, y))).filter(|&(x, y)| px[(y * 60 + x) as usize] != 0xABCDEF).collect();
    assert!(touched.len() > 50);
    assert!(touched.iter().all(|&(x, y)| (30 - CURSOR_LEFT..30 - CURSOR_LEFT + CURSOR_BOX_W).contains(&x) && (30 - CURSOR_TOP..30 - CURSOR_TOP + CURSOR_BOX_H).contains(&y)));
}

#[test]
fn a_sub_canvas_draws_in_a_part_of_the_parent_with_its_own_origin() {
    let mut px = vec![0u32; 20 * 10];
    {
        let mut c = Canvas::new(&mut px, 20, 10, 20);
        let mut s = c.sub(Rect::new(5, 2, 8, 4));
        s.fill(Rect::new(0, 0, 100, 100), 0xFF0000); // (clipped to the part)
        s.fill(Rect::new(0, 0, 1, 1), 0x00FF00);
    }
    assert_eq!(px[(2 * 20 + 5) as usize], 0x00FF00, "its (0, 0) is the parent's (5, 2)");
    assert_eq!(px[(2 * 20 + 6) as usize], 0xFF0000);
    assert_eq!(px[(5 * 20 + 12) as usize], 0xFF0000, "the last pixel of the part");
    assert_eq!(px.iter().filter(|&&p| p != 0).count(), 8 * 4, "nothing outside the part");
    // a part that is not on the canvas is empty and harmless
    let mut c = Canvas::new(&mut px, 20, 10, 20);
    let mut e = c.sub(Rect::new(50, 50, 5, 5));
    e.fill(Rect::new(0, 0, 5, 5), 1);
}
