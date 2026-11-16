//! A sheet of the typeface at every size and of every icon: `cargo run -p metal-gfx --example typeicons -- OUT.ppm`.
use metal_gfx::typeface::*;
use metal_gfx::*;

fn main() {
    let out = std::env::args().nth(1).expect("usage: typeicons OUT.ppm");
    let (w, h) = (900i32, 620i32);
    let mut px = vec![0xF4F6FCu32; (w * h) as usize];
    {
        let mut c = Canvas::new(&mut px, w, h, w);
        c.gradient_v(Rect::new(0, 0, w, h), 0xFCEAF2, 0xE3E8FC);
        let mut y = 20;
        for (name, face) in [("caption", &CAPTION), ("body", &BODY), ("label", &LABEL), ("title", &TITLE), ("large", &LARGE), ("display", &DISPLAY)] {
            c.label(20, y, &CAPTION, 0x6B7399, 255, name);
            c.label(110, y, face, 0x1D2145, 255, "Quick settings — The quick brown fox jumps 0123456789");
            y += face.line_height() + 10;
        }
        c.label(110, y, &DIGITS, 0x1D2145, 255, "12:34");
        y += DIGITS.line_height() + 10;
        let icons = [Icon::Play, Icon::Pause, Icon::ChevronRight, Icon::ChevronLeft, Icon::ChevronDown, Icon::Plus, Icon::Minus, Icon::Check, Icon::Close, Icon::Search, Icon::Sparkle, Icon::Heart, Icon::Sun, Icon::Moon, Icon::Arrow, Icon::Wifi, Icon::Bluetooth, Icon::Airplane, Icon::Rotate, Icon::Bell, Icon::DoNotDisturb, Icon::Battery, Icon::Gear, Icon::Brush, Icon::Clock, Icon::Terminal, Icon::Volume, Icon::Programs];
        for (i, ic) in icons.iter().enumerate() {
            let x = 20 + (i as i32 % 14) * 62;
            let yy = y + (i as i32 / 14) * 70;
            icons::draw(&mut c, *ic, Rect::new(x, yy, 44, 44), 0x1D2145, 255);
            icons::draw(&mut c, *ic, Rect::new(x + 46, yy + 24, 16, 16), 0x4F6BFF, 255);
        }
    }
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for p in &px {
        ppm.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    std::fs::write(out, ppm).unwrap();
}
