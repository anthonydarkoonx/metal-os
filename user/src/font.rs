//! Drawing text straight into a framebuffer (the console). The 5x9 glyph table itself is in `metallium_gfx::font`.

pub use metallium_gfx::font::GLYPHS;

fn glyph(c: u8) -> Option<&'static [u8; 9]> {
    GLYPHS.iter().find(|(g, _)| *g == c).map(|(_, rows)| rows)
}

/// Fill a rectangle of a `width`-pixel-wide framebuffer (`fb` is the whole buffer; whatever falls outside it is not drawn).
pub fn fill_rect(fb: &mut [u32], width: usize, x: usize, y: usize, w: usize, h: usize, color: u32) {
    for row in y..y + h {
        for col in x..x + w {
            if col < width {
                if let Some(p) = fb.get_mut(row * width + col) {
                    *p = color;
                }
            }
        }
    }
}

/// Draw one character with its top-left corner at (x, y); each font pixel becomes a
/// `scale` x `scale` block. Characters without a glyph are drawn as blanks.
pub fn draw_char(fb: &mut [u32], width: usize, x: usize, y: usize, scale: usize, color: u32, ch: u8) {
    let Some(rows) = glyph(ch) else { return };
    for (r, bits) in rows.iter().enumerate() {
        for col in 0..5 {
            if bits >> (4 - col) & 1 == 1 {
                fill_rect(fb, width, x + col * scale, y + r * scale, scale, scale, color);
            }
        }
    }
}

/// Draw character `ch` of the monospace face (IBM Plex Mono, 14 px) with the top left of its cell at (x, y): each pixel is the cell's own colour blended
/// with `color` by the glyph's coverage, so it is smooth on any ground.
pub fn draw_mono(fb: &mut [u32], width: usize, x: usize, y: usize, color: u32, ch: u8) {
    let face = &metallium_gfx::typeface::MONO;
    let Some(g) = face.glyph(ch as char) else { return };
    for row in 0..g.h as usize {
        for col in 0..g.w as usize {
            let i = row * g.w as usize + col;
            let byte = face.data[g.off as usize + i / 2];
            let cov = (if i % 2 == 0 { byte & 15 } else { byte >> 4 }) as u32 * 17;
            if cov == 0 {
                continue;
            }
            let (px, py) = ((x as i32 + g.bx as i32 + col as i32), (y as i32 + g.by as i32 + row as i32));
            if px < 0 || py < 0 || px as usize >= width {
                continue;
            }
            if let Some(p) = fb.get_mut(py as usize * width + px as usize) {
                *p = metallium_gfx::blend(*p, color, cov);
            }
        }
    }
}

/// Draw `text` starting at (x, y); each character advances 6 font pixels (5 + 1 gap).
pub fn draw_text(fb: &mut [u32], width: usize, x: usize, y: usize, scale: usize, color: u32, text: &str) {
    for (i, ch) in text.bytes().enumerate() {
        draw_char(fb, width, x + i * 6 * scale, y, scale, color, ch);
    }
}
