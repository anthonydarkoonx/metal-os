//! The interface typeface: IBM Plex Sans, rasterised at the sizes the interface uses (`tools/mkfont.py`, data in `font_data.rs`),
//! anti-aliased, proportional. A `Face` is one size and weight. Coordinates: `label` takes the top-left of the *line box*; the text
//! sits on its baseline `ascent` pixels below it, and the box is `line_height()` tall.
use crate::{blend, Canvas};

pub struct Glyph {
    /// The advance in sixteenths of a pixel.
    pub adv: u16,
    /// The bitmap's offset from the pen position (x) and from the top of the line box (y).
    pub bx: i8,
    pub by: i8,
    pub w: u8,
    pub h: u8,
    /// Where the bitmap starts in `Face::data` (4-bit coverage, two pixels per byte, low nibble first).
    pub off: u32,
}

pub struct Face {
    pub ascent: i32,
    pub descent: i32,
    pub first: u8,
    pub glyphs: &'static [Glyph],
    pub data: &'static [u8],
}

pub use super::font_data::{BODY, CAPTION, DIGITS, DISPLAY, LABEL, LARGE, MONO, TITLE};

impl Face {
    pub fn glyph(&self, ch: char) -> Option<&Glyph> {
        let c = ch as u32;
        let i = c.checked_sub(self.first as u32)? as usize;
        self.glyphs.get(i)
    }

    /// A character without a glyph is drawn as the replacement box's advance.
    fn advance16(&self, ch: char) -> i32 {
        self.glyph(ch).map_or(self.glyph('?').map_or(0, |g| g.adv as i32), |g| g.adv as i32)
    }

    pub fn line_height(&self) -> i32 {
        self.ascent + self.descent
    }

    /// The width of `s` in pixels.
    pub fn width(&self, s: &str) -> i32 {
        (s.chars().map(|c| self.advance16(c)).sum::<i32>() + 8) >> 4
    }

    /// How many characters of `s` fit in `max_w` pixels.
    pub fn fit(&self, s: &str, max_w: i32) -> usize {
        let (mut acc, mut n) = (0, 0);
        for c in s.chars() {
            acc += self.advance16(c);
            if (acc + 8) >> 4 > max_w {
                break;
            }
            n += 1;
        }
        n
    }
}

impl Canvas<'_> {
    /// Draw `s` with its line box's top-left at `(x, y)`; returns the width drawn.
    pub fn label(&mut self, x: i32, y: i32, face: &Face, color: u32, alpha: u32, s: &str) -> i32 {
        let mut pen = x * 16;
        let clip = self.clip();
        for ch in s.chars() {
            let Some(g) = face.glyph(ch).or_else(|| face.glyph('?')) else { continue };
            if g.w > 0 && g.h > 0 {
                let (gx, gy) = (((pen + 8) >> 4) + g.bx as i32, y + g.by as i32);
                let (w, h) = (g.w as i32, g.h as i32);
                if crate::Rect::new(gx, gy, w, h).overlaps(&clip) {
                    for row in 0..h {
                        let py = gy + row;
                        if py < clip.y || py >= clip.bottom() {
                            continue;
                        }
                        for col in 0..w {
                            let px = gx + col;
                            if px < clip.x || px >= clip.right() {
                                continue;
                            }
                            let i = (row * w + col) as usize;
                            let byte = face.data[g.off as usize + i / 2];
                            let cov = (if i % 2 == 0 { byte & 15 } else { byte >> 4 }) as u32 * 17;
                            if cov != 0 {
                                let p = self.at(px, py);
                                *p = blend(*p, color, cov * alpha.min(255) / 255);
                            }
                        }
                    }
                }
            }
            pen += g.adv as i32;
        }
        ((pen + 8) >> 4) - x
    }

    /// `s` centred in `r` (horizontally and vertically, by its line box).
    pub fn label_centered(&mut self, r: crate::Rect, face: &Face, color: u32, alpha: u32, s: &str) {
        let w = face.width(s);
        self.label(r.x + (r.w - w) / 2, r.y + (r.h - face.line_height()) / 2, face, color, alpha, s);
    }

    /// `s` right-aligned to `right` at line top `y`.
    pub fn label_right(&mut self, right: i32, y: i32, face: &Face, color: u32, alpha: u32, s: &str) {
        let w = face.width(s);
        self.label(right - w, y, face, color, alpha, s);
    }
}
