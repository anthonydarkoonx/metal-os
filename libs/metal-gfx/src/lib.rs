//! 2D drawing for the compositor and the UI toolkit. `no_std`, no `unsafe`, and every primitive is clipped: nothing
//! writes outside the canvas or its clip rectangle, whatever coordinates it is given (the compositor draws what untrusted programs
//! describe, so a bad rectangle must not be able to reach other memory).
//!
//! Pixels are `0x00RRGGBB` words. Shapes are anti-aliased (coverage computed per edge pixel in 1/16 pixel units), shadows are
//! stacks of translucent rounded rectangles, text uses the 5x9 bitmap font at an integer scale.
#![no_std]
extern crate alloc;

pub mod font;
mod font_data;
pub mod typeface;
pub use typeface::Face;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.w)
    }
    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h)
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.right() && py < self.bottom()
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let (x, y) = (self.x.max(o.x), self.y.max(o.y));
        let (r, b) = (self.right().min(o.right()), self.bottom().min(o.bottom()));
        if r <= x || b <= y {
            Rect::default()
        } else {
            Rect { x, y, w: r - x, h: b - y }
        }
    }
    pub fn overlaps(&self, o: &Rect) -> bool {
        !self.intersect(o).is_empty()
    }
    /// Whether the two overlap or touch along an edge (their union is then no bigger than needed to hold both, in area terms).
    fn touches(&self, o: &Rect) -> bool {
        self.x <= o.right() && o.x <= self.right() && self.y <= o.bottom() && o.y <= self.bottom()
    }
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        Rect { x, y, w: self.right().max(o.right()) - x, h: self.bottom().max(o.bottom()) - y }
    }
    /// Does `self` cover all of `o`?
    pub fn contains_rect(&self, o: &Rect) -> bool {
        !o.is_empty() && o.x >= self.x && o.y >= self.y && o.right() <= self.right() && o.bottom() <= self.bottom()
    }
    /// The parts of `self` that are not in `o`, as up to four rectangles that do not overlap each other.
    pub fn subtract(&self, o: &Rect) -> ([Rect; 4], usize) {
        let mut out = [Rect::default(); 4];
        let i = self.intersect(o);
        if i.is_empty() {
            out[0] = *self;
            return (out, 1);
        }
        let mut n = 0;
        let mut push = |r: Rect| {
            if !r.is_empty() {
                out[n] = r;
                n += 1;
            }
        };
        push(Rect::new(self.x, self.y, self.w, i.y - self.y)); // above
        push(Rect::new(self.x, i.bottom(), self.w, self.bottom() - i.bottom())); // below
        push(Rect::new(self.x, i.y, i.x - self.x, i.h)); // left of the overlap
        push(Rect::new(i.right(), i.y, self.right() - i.right(), i.h)); // right of the overlap
        (out, n)
    }
    pub fn translate(&self, dx: i32, dy: i32) -> Rect {
        Rect { x: self.x.saturating_add(dx), y: self.y.saturating_add(dy), w: self.w, h: self.h }
    }
    pub fn inflate(&self, d: i32) -> Rect {
        Rect { x: self.x - d, y: self.y - d, w: self.w + 2 * d, h: self.h + 2 * d }
    }
}

/// A set of rectangles to repaint (the damage). At most `CAP` of them: when it is full the rectangles are merged into their
/// bounding box, so it can only ever grow too large, never miss a pixel.
#[derive(Clone, Copy)]
pub struct Region {
    r: [Rect; Region::CAP],
    n: usize,
}

impl Region {
    pub const CAP: usize = 12;

    pub const fn new() -> Region {
        Region { r: [Rect::new(0, 0, 0, 0); Region::CAP], n: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn rects(&self) -> &[Rect] {
        &self.r[..self.n]
    }

    pub fn clear(&mut self) {
        self.n = 0;
    }

    pub fn bounds(&self) -> Rect {
        self.rects().iter().fold(Rect::default(), |a, b| a.union(b))
    }

    /// Add a rectangle. It is merged with a rectangle it overlaps or touches only when the union is not much bigger than the two
    /// together (a window that moved leaves two frames, not one huge box with a hole in the middle); otherwise both are kept and
    /// the overlap is cut out of the newcomer so that it is painted once. When the set is full everything collapses into the bounding box, which may be too
    /// large but never misses a pixel.
    pub fn add(&mut self, add: Rect) {
        self.add_with(add, true);
    }

    /// `merge` false: the rectangle is only cut against what is there (used for the pieces of a cut one, which must not merge
    /// again: a merge could re-create the overlap that was just cut out, for ever).
    fn add_with(&mut self, mut add: Rect, merge: bool) {
        if add.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.n {
            let other = self.r[i];
            if merge && other.touches(&add) {
                let u = add.union(&other);
                let (ua, a, b, o) = (u.w as i64 * u.h as i64, add.w as i64 * add.h as i64, other.w as i64 * other.h as i64, {
                    let x = add.intersect(&other);
                    x.w as i64 * x.h as i64
                });
                if ua * 4 <= (a + b - o) * 5 {
                    add = u;
                    self.n -= 1;
                    self.r[i] = self.r[self.n];
                    i = 0; // (the union may now be worth merging with one that was passed over)
                    continue;
                }
            }
            if other.overlaps(&add) {
                // kept apart (merging would make a much bigger box), but the shared part must not be painted twice
                if other.contains_rect(&add) {
                    return;
                }
                if add.contains_rect(&other) {
                    self.n -= 1;
                    self.r[i] = self.r[self.n];
                    continue;
                }
                let (pieces, n) = add.subtract(&other);
                for p in &pieces[..n] {
                    self.add_with(*p, false);
                }
                return;
            }
            i += 1;
        }
        if self.n == Self::CAP {
            let all = self.bounds();
            self.r[0] = all.union(&add);
            self.n = 1;
        } else {
            self.r[self.n] = add;
            self.n += 1;
        }
    }

    pub fn add_region(&mut self, o: &Region) {
        for r in o.rects() {
            self.add(*r);
        }
    }

    pub fn intersects(&self, r: &Rect) -> bool {
        self.rects().iter().any(|d| d.overlaps(r))
    }
}

impl Default for Region {
    fn default() -> Region {
        Region::new()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// colour
// ---------------------------------------------------------------------------------------------------------------
fn div255(v: u32) -> u32 {
    (v + 128 + ((v + 128) >> 8)) >> 8
}

/// `src` over `dst` with `alpha` (0 = dst, 255 = src), per channel.
pub fn blend(dst: u32, src: u32, alpha: u32) -> u32 {
    let a = alpha.min(255);
    if a == 255 {
        return src & 0xFF_FFFF;
    }
    if a == 0 {
        return dst & 0xFF_FFFF;
    }
    let ch = |s: u32| div255(((src >> s) & 0xFF) * a + ((dst >> s) & 0xFF) * (255 - a)) << s;
    ch(16) | ch(8) | ch(0)
}

/// A colour between `a` and `b`, `t` of 256 of the way.
pub fn mix(a: u32, b: u32, t: u32) -> u32 {
    let t = t.min(256);
    let ch = |s: u32| (((a >> s) & 0xFF) * (256 - t) + ((b >> s) & 0xFF) * t) >> 8 << s;
    ch(16) | ch(8) | ch(0)
}

fn isqrt(v: u32) -> u32 {
    if v < 2 {
        return v;
    }
    let mut x = v;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + v / x) / 2;
    }
    x
}

/// Coverage (0..=255) of pixel `(x, y)` inside the corner of a circle of radius `r` whose centre is `(r, r)` from the corner
/// pixel's own origin: 0 outside, 255 inside, in between along the edge.
fn corner_cov(r: i32, x: i32, y: i32) -> u32 {
    // work in sixteenths of a pixel, at the pixel centre
    let dx = r * 16 - (x * 16 + 8);
    let dy = r * 16 - (y * 16 + 8);
    if dx <= 0 || dy <= 0 {
        return 255;
    }
    let d = isqrt((dx * dx + dy * dy) as u32) as i32;
    let c = r * 16 - d + 8; // 16 = fully inside, 0 = fully outside
    (c.clamp(0, 16) * 255 / 16) as u32
}

// ---------------------------------------------------------------------------------------------------------------
// the canvas
// ---------------------------------------------------------------------------------------------------------------
pub struct Canvas<'a> {
    px: &'a mut [u32],
    w: i32,
    h: i32,
    stride: i32,
    clip: Rect,
}

impl<'a> Canvas<'a> {
    /// A canvas over a rectangle of this one, with its own origin at the rectangle's top left (a widget drawn in a part of a window).
    pub fn sub(&mut self, r: Rect) -> Canvas<'_> {
        let r = r.intersect(&Rect::new(0, 0, self.w, self.h));
        if r.is_empty() {
            return Canvas::new(&mut [], 0, 0, 1);
        }
        let off = (r.y * self.stride + r.x) as usize;
        Canvas::new(&mut self.px[off..], r.w, r.h, self.stride)
    }

    /// A canvas over `px`, `w` x `h` pixels, `stride` words per row. A slice that is too short makes the canvas smaller (so
    /// that every pixel it names is really there).
    pub fn new(px: &'a mut [u32], w: i32, h: i32, stride: i32) -> Canvas<'a> {
        let (w, stride) = (w.max(0), stride.max(1));
        let rows = (px.len() as i64 / stride as i64) as i32; // whole rows that exist
        let h = h.clamp(0, rows);
        let w = w.min(stride);
        Canvas { px, w, h, stride, clip: Rect::new(0, 0, w, h) }
    }

    pub fn width(&self) -> i32 {
        self.w
    }
    pub fn height(&self) -> i32 {
        self.h
    }
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.w, self.h)
    }
    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Restrict drawing to `r` (and the canvas); returns the previous clip.
    pub fn set_clip(&mut self, r: Rect) -> Rect {
        let old = self.clip;
        self.clip = r.intersect(&self.bounds());
        old
    }

    pub fn reset_clip(&mut self) {
        self.clip = self.bounds();
    }

    pub fn get(&self, x: i32, y: i32) -> Option<u32> {
        self.bounds().contains(x, y).then(|| self.px[(y * self.stride + x) as usize])
    }

    pub(crate) fn at(&mut self, x: i32, y: i32) -> &mut u32 {
        &mut self.px[(y * self.stride + x) as usize]
    }

    /// Plot one pixel with coverage `cov` (0..=255) of `color`, if inside the clip.
    pub fn plot(&mut self, x: i32, y: i32, color: u32, cov: u32) {
        if cov == 0 || !self.clip.contains(x, y) {
            return;
        }
        let p = self.at(x, y);
        *p = blend(*p, color, cov);
    }

    /// An opaque rectangle.
    pub fn fill(&mut self, r: Rect, color: u32) {
        let r = r.intersect(&self.clip);
        for y in r.y..r.bottom() {
            let o = (y * self.stride + r.x) as usize;
            self.px[o..o + r.w as usize].fill(color & 0xFF_FFFF);
        }
    }

    /// A translucent rectangle.
    pub fn fill_blend(&mut self, r: Rect, color: u32, alpha: u32) {
        if alpha >= 255 {
            return self.fill(r, color);
        }
        let r = r.intersect(&self.clip);
        for y in r.y..r.bottom() {
            let o = (y * self.stride + r.x) as usize;
            for p in &mut self.px[o..o + r.w as usize] {
                *p = blend(*p, color, alpha);
            }
        }
    }

    /// A vertical gradient from `top` to `bottom` over the rectangle.
    pub fn gradient_v(&mut self, r: Rect, top: u32, bottom: u32) {
        let full = r;
        let r = r.intersect(&self.clip);
        for y in r.y..r.bottom() {
            let t = ((y - full.y) * 256 / full.h.max(1)) as u32;
            let c = mix(top, bottom, t);
            let o = (y * self.stride + r.x) as usize;
            self.px[o..o + r.w as usize].fill(c);
        }
    }

    /// A rectangle with rounded corners of radius `radius`, anti-aliased, with `alpha` opacity. (A radius of half the smaller
    /// side makes a pill or a circle.)
    pub fn fill_round(&mut self, r: Rect, radius: i32, color: u32, alpha: u32) {
        if r.is_empty() {
            return;
        }
        let radius = radius.clamp(0, r.w.min(r.h) / 2);
        let vis = r.intersect(&self.clip);
        if vis.is_empty() {
            return;
        }
        for y in vis.y..vis.bottom() {
            // distance of this row from the top or bottom edge, inside the corner band?
            let from_top = y - r.y;
            let from_bottom = r.bottom() - 1 - y;
            let band = from_top.min(from_bottom);
            if band >= radius {
                self.fill_blend(Rect::new(vis.x, y, vis.w, 1), color, alpha);
                continue;
            }
            // the row inside the band: `band` counts rows from the nearest edge, and the corner circle is mirrored
            let cy = band;
            for x in vis.x..vis.right() {
                let from_left = x - r.x;
                let from_right = r.right() - 1 - x;
                let edge = from_left.min(from_right);
                let cov = if edge >= radius { 255 } else { corner_cov(radius, edge, cy) };
                self.plot(x, y, color, cov * alpha.min(255) / 255);
            }
        }
    }

    /// A soft shadow around a rounded rectangle: `layers` translucent outlines growing outwards, drawn *under* what is drawn
    /// next. `offset_y` moves it down (a window looks lifted).
    pub fn shadow(&mut self, r: Rect, radius: i32, layers: i32, strength: u32, offset_y: i32) {
        let r = r.translate(0, offset_y);
        for i in (1..=layers).rev() {
            // the outermost layers are the faintest: the shadow fades out
            let a = strength * (layers - i + 1) as u32 / (layers as u32 * 3).max(1);
            self.fill_round(r.inflate(i), radius + i, 0x000000, a);
        }
    }

    /// A soft shadow of a given colour (cool shadows suit light backdrops): `blur` pixels of falloff, `strength` 0..=255.
    pub fn shadow_color(&mut self, r: Rect, radius: i32, blur: i32, strength: u32, offset_y: i32, color: u32) {
        let r = r.translate(0, offset_y);
        for i in (1..=blur).rev() {
            // a smooth falloff: the layers' opacity follows the square of the distance to the edge
            let t = (blur - i + 1) as u32;
            let a = strength * t * t / (blur as u32 * blur as u32 * 4).max(1);
            self.fill_round(r.inflate(i), radius + i, color, a);
        }
    }

    /// Copy a block of opaque pixels (`src`, `sw` x `sh`, `ss` words per row) to `(dx, dy)`.
    pub fn blit(&mut self, src: &[u32], sw: i32, sh: i32, ss: i32, dx: i32, dy: i32) {
        self.blit_round(src, sw, sh, ss, dx, dy, 0, 0)
    }

    /// As `blit`, with the top corners rounded by `top_radius` and the bottom ones by `bottom_radius` (anti-aliased: the edge
    /// pixels are blended with what is already there).
    pub fn blit_round(&mut self, src: &[u32], sw: i32, sh: i32, ss: i32, dx: i32, dy: i32, top_radius: i32, bottom_radius: i32) {
        let ss = ss.max(1);
        let sh = sh.clamp(0, (src.len() as i64 / ss as i64) as i32);
        let sw = sw.clamp(0, ss);
        let dst = Rect::new(dx, dy, sw, sh);
        let vis = dst.intersect(&self.clip);
        let (tr, br) = (top_radius.clamp(0, sw.min(sh) / 2), bottom_radius.clamp(0, sw.min(sh) / 2));
        for y in vis.y..vis.bottom() {
            let sy = y - dy;
            let (band, radius) = if sy < tr { (sy, tr) } else if sh - 1 - sy < br { (sh - 1 - sy, br) } else { (0, 0) };
            let so = (sy * ss) as usize;
            let (x0, x1) = (vis.x - dx, vis.right() - dx);
            if radius == 0 {
                let d = (y * self.stride + vis.x) as usize;
                self.px[d..d + vis.w as usize].copy_from_slice(&src[so + x0 as usize..so + x1 as usize]);
                continue;
            }
            for sx in x0..x1 {
                let edge = sx.min(sw - 1 - sx);
                let cov = if edge >= radius { 255 } else { corner_cov(radius, edge, band) };
                self.plot(dx + sx, y, src[so + sx as usize], cov);
            }
        }
    }

    /// Copy opaque pixels (`src`, `sw` x `sh`, `ss` words per row) scaled (nearest) into `dst`, with rounded corners of `radius` and
    /// `alpha` opacity: what a window looks like while it grows or shrinks.
    pub fn blit_scaled(&mut self, src: &[u32], sw: i32, sh: i32, ss: i32, dst: Rect, radius: i32, alpha: u32) {
        let ss = ss.max(1);
        let sh = sh.clamp(0, (src.len() as i64 / ss as i64) as i32);
        let sw = sw.clamp(0, ss);
        if sw == 0 || sh == 0 || dst.is_empty() {
            return;
        }
        let vis = dst.intersect(&self.clip);
        let radius = radius.clamp(0, dst.w.min(dst.h) / 2);
        for y in vis.y..vis.bottom() {
            let dy = y - dst.y;
            let sy = ((dy as i64 * sh as i64) / dst.h as i64).clamp(0, sh as i64 - 1) as i32;
            let band = dy.min(dst.h - 1 - dy);
            let so = (sy * ss) as usize;
            for x in vis.x..vis.right() {
                let dx = x - dst.x;
                let sx = ((dx as i64 * sw as i64) / dst.w as i64).clamp(0, sw as i64 - 1) as i32;
                let edge = dx.min(dst.w - 1 - dx);
                let cov = if edge >= radius || band >= radius { 255 } else { corner_cov(radius, edge, band) };
                if cov > 0 {
                    let p = self.at(x, y);
                    *p = blend(*p, src[so + sx as usize], cov * alpha.min(255) / 255);
                }
            }
        }
    }

    /// Draw `text` with its top-left at `(x, y)` of a 9-pixel line, as the whole system has always placed small text. At `scale` 1 it is the interface's
    /// proportional typeface (IBM Plex Sans, 13 px) with its capitals on the same rows the old 5 x 9 bitmap font used, so layouts that centre text on a
    /// 9-pixel box still do; at larger scales it is the bitmap font, each font pixel `scale` x `scale` pixels (big numbers, the terminal's cells). Returns the
    /// width drawn.
    pub fn text(&mut self, x: i32, y: i32, scale: i32, color: u32, alpha: u32, s: &str) -> i32 {
        let scale = scale.clamp(1, 16);
        if scale == 1 {
            let face = &typeface::CAPTION;
            // (the baseline is the 8th row of the 9-pixel box)
            return self.label(x, y + 8 - face.ascent, face, color, alpha, s);
        }
        let mut cx = x;
        for ch in s.bytes() {
            if let Some(rows) = font::glyph(ch) {
                for (r, bits) in rows.iter().enumerate() {
                    for col in 0..font::W {
                        if bits >> (font::W - 1 - col) & 1 == 1 {
                            self.fill_blend(Rect::new(cx + col * scale, y + r as i32 * scale, scale, scale), color, alpha);
                        }
                    }
                }
            }
            cx = cx.saturating_add(font::ADVANCE * scale);
        }
        cx - x
    }
}

/// The width `text` takes at `scale` (the same face `Canvas::text` draws with).
pub fn text_width(s: &str, scale: i32) -> i32 {
    if scale <= 1 {
        return typeface::CAPTION.width(s);
    }
    s.len() as i32 * font::ADVANCE * scale.clamp(1, 16)
}

/// An arrow cursor, 12 x 19 pixels: `0` transparent, `1` outline, `2` fill. The hot spot is the top-left pixel.
pub const CURSOR_W: i32 = 12;
pub const CURSOR_H: i32 = 19;
pub const CURSOR: [&str; 19] = [
    "1...........",
    "11..........",
    "121.........",
    "1221........",
    "12221.......",
    "122221......",
    "1222221.....",
    "12222221....",
    "122222221...",
    "1222222221..",
    "12222222221.",
    "122222222221",
    "1222222111..",
    "122122221...",
    "12.1222221..",
    "1..1222221..",
    "...12222221.",
    "....1222221.",
    ".....1111...",
];

/// What the pointer looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Arrow,
    /// A double arrow left and right (over a window's side edges).
    ResizeH,
    /// Up and down (top and bottom edges).
    ResizeV,
    /// Along the diagonal from top left to bottom right (the top left and bottom right corners).
    ResizeNwse,
    /// From top right to bottom left.
    ResizeNesw,
}

/// Every shape fits in this box around its hot spot: `CURSOR_LEFT` pixels left of it, `CURSOR_TOP` above, `CURSOR_BOX_W` x `CURSOR_BOX_H` in
/// all (what the compositor saves and restores under the pointer).
pub const CURSOR_LEFT: i32 = 10;
pub const CURSOR_TOP: i32 = 10;
pub const CURSOR_BOX_W: i32 = 22;
pub const CURSOR_BOX_H: i32 = 29;
/// The resize shapes are drawn in a square grid of this size, with the hot spot in the middle.
const GRID: i32 = 19;

impl Shape {
    /// Is the cell `(gx, gy)` of the 19 x 19 grid part of the white body of a resize shape?
    fn body(self, gx: i32, gy: i32) -> bool {
        if !(0..GRID).contains(&gx) || !(0..GRID).contains(&gy) {
            return false;
        }
        let (c, last) = (GRID / 2, GRID - 1);
        match self {
            Shape::Arrow => false,
            // two arrowheads (triangles five cells deep, one cell in from the edge) joined by a shaft three cells thick
            Shape::ResizeH => {
                let (l, r) = (gx - 1, last - 1 - gx);
                ((0..=4).contains(&l) && (gy - c).abs() <= l) || ((0..=4).contains(&r) && (gy - c).abs() <= r) || ((5..=GRID - 6).contains(&gx) && (gy - c).abs() <= 1)
            }
            Shape::ResizeV => Shape::ResizeH.body(gy, gx),
            // diagonal: a right-angled head in each corner and a shaft along the diagonal
            Shape::ResizeNwse => {
                let head_a = gx >= 1 && gy >= 1 && (gx - 1) + (gy - 1) <= 6;
                let head_b = gx <= last - 1 && gy <= last - 1 && (last - 1 - gx) + (last - 1 - gy) <= 6;
                head_a || head_b || ((gx - gy).abs() <= 1 && (8..=2 * last - 8).contains(&(gx + gy)))
            }
            Shape::ResizeNesw => Shape::ResizeNwse.body(last - gx, gy),
        }
    }

    /// The pixel at `(dx, dy)` from the hot spot: 0 transparent, 1 black outline, 2 white body.
    pub fn pixel(self, dx: i32, dy: i32) -> u8 {
        if self == Shape::Arrow {
            return if (0..CURSOR_W).contains(&dx) && (0..CURSOR_H).contains(&dy) {
                match CURSOR[dy as usize].as_bytes()[dx as usize] {
                    b'1' => 1,
                    b'2' => 2,
                    _ => 0,
                }
            } else {
                0
            };
        }
        let (gx, gy) = (dx + GRID / 2, dy + GRID / 2);
        if self.body(gx, gy) {
            return 2;
        }
        let near = (-1..=1).any(|j| (-1..=1).any(|i| self.body(gx + i, gy + j)));
        near as u8
    }
}

impl Canvas<'_> {
    /// The cursor arrow with its hot spot at `(x, y)`.
    pub fn cursor(&mut self, x: i32, y: i32) {
        self.cursor_shape(x, y, Shape::Arrow);
    }

    /// The pointer in the given shape with its hot spot at `(x, y)`.
    pub fn cursor_shape(&mut self, x: i32, y: i32, shape: Shape) {
        for dy in -CURSOR_TOP..CURSOR_BOX_H - CURSOR_TOP {
            for dx in -CURSOR_LEFT..CURSOR_BOX_W - CURSOR_LEFT {
                match shape.pixel(dx, dy) {
                    1 => self.plot(x + dx, y + dy, 0x000000, 255),
                    2 => self.plot(x + dx, y + dy, 0xFFFFFF, 255),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------------------------------------------
// anti-aliased polygons and lines
// ---------------------------------------------------------------------------------------------------------------
fn isqrt64(v: i64) -> i64 {
    if v < 2 {
        return v.max(0);
    }
    let mut x = v;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + v / x) / 2;
    }
    x
}

/// A point in sixteenths of a pixel.
pub type Pt = (i32, i32);

fn inside_poly(poly: &[Pt], x: i32, y: i32) -> bool {
    // even-odd rule: count the edges a ray to the right crosses
    let mut c = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let ((xi, yi), (xj, yj)) = (poly[i], poly[j]);
        if (yi > y) != (yj > y) {
            let t = (y - yi) as i64 * (xj - xi) as i64 / (yj - yi) as i64 + xi as i64;
            if (x as i64) < t {
                c = !c;
            }
        }
        j = i;
    }
    c
}

impl Canvas<'_> {
    /// Fill a polygon given in sixteenths of a pixel, anti-aliased (16 samples per pixel).
    pub fn fill_poly(&mut self, poly: &[Pt], color: u32, alpha: u32) {
        if poly.len() < 3 {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for &(x, y) in poly {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        let bb = Rect::new(x0.div_euclid(16), y0.div_euclid(16), (x1 - x0) / 16 + 2, (y1 - y0) / 16 + 2).intersect(&self.clip);
        for py in bb.y..bb.bottom() {
            for px in bb.x..bb.right() {
                let mut n = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        if inside_poly(poly, px * 16 + sx * 4 + 2, py * 16 + sy * 4 + 2) {
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    self.plot(px, py, color, n * 255 / 16 * alpha.min(255) / 255);
                }
            }
        }
    }

    /// A line from `a` to `b` (sixteenths of a pixel), `width` sixteenths thick, with round ends.
    pub fn line(&mut self, a: Pt, b: Pt, width: i32, color: u32, alpha: u32) {
        let (dx, dy) = ((b.0 - a.0) as i64, (b.1 - a.1) as i64);
        let len = isqrt64(dx * dx + dy * dy).max(1);
        let (nx, ny) = ((-dy * (width as i64) / 2 / len) as i32, (dx * (width as i64) / 2 / len) as i32);
        self.fill_poly(&[(a.0 + nx, a.1 + ny), (b.0 + nx, b.1 + ny), (b.0 - nx, b.1 - ny), (a.0 - nx, a.1 - ny)], color, alpha);
        let r = width / 2;
        for p in [a, b] {
            self.fill_disc(p, r, color, alpha);
        }
    }

    /// A disc centred at `c` (sixteenths of a pixel) of radius `r` (sixteenths), anti-aliased.
    pub fn fill_disc(&mut self, c: Pt, r: i32, color: u32, alpha: u32) {
        let bb = Rect::new((c.0 - r).div_euclid(16) - 1, (c.1 - r).div_euclid(16) - 1, 2 * r / 16 + 4, 2 * r / 16 + 4).intersect(&self.clip);
        for py in bb.y..bb.bottom() {
            for px in bb.x..bb.right() {
                let (dx, dy) = ((px * 16 + 8 - c.0) as i64, (py * 16 + 8 - c.1) as i64);
                let cov = (r as i64 - isqrt64(dx * dx + dy * dy) + 8).clamp(0, 16);
                if cov > 0 {
                    self.plot(px, py, color, (cov * 255 / 16) as u32 * alpha.min(255) / 255);
                }
            }
        }
    }
}
