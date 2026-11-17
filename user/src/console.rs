//! A text console in a window: a scrolling character grid drawn into a pixel buffer the console shows as the terminal window.
//!
//! Characters are drawn in IBM Plex Mono (14 px) on a black ground, light grey text (the classic terminal look) (8 x 19 pixel cells, 80 columns by 28 rows).
//! Drawing only touches the buffer; `take_dirty` says which rows changed since it was last asked, so the console repaints just those.
use crate::{font, *};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

pub const BG: u32 = 0x000000;
pub const FG: u32 = 0xC0C0C0;
pub const ACCENT: u32 = 0xFFFFFF;
pub const DIM: u32 = 0x808080;
pub const ERR: u32 = 0xFF5555;

/// The colours above are *slots*: what the shell asks for. What is drawn is what the slot is mapped to now (the system colour scheme: see
/// `retheme`), so the terminal follows the theme. In the order BG, FG, ACCENT, DIM, ERR.
static THEMED: [AtomicU32; 5] = [AtomicU32::new(BG), AtomicU32::new(FG), AtomicU32::new(ACCENT), AtomicU32::new(DIM), AtomicU32::new(ERR)];
const SLOTS: [u32; 5] = [BG, FG, ACCENT, DIM, ERR];

/// The colour drawn for a slot colour (anything that is not a slot is drawn as it is).
pub fn themed(slot: u32) -> u32 {
    match SLOTS.iter().position(|&s| s == slot) {
        Some(i) => THEMED[i].load(Relaxed),
        None => slot,
    }
}

/// Re-colour the terminal: every pixel of `buf` (`WIDTH` x `HEIGHT`) that has a slot's current colour gets the new colour, and what is drawn from
/// now on uses the new ones. The whole buffer counts as changed.
pub fn retheme(buf: *mut u32, new: [u32; 5]) {
    let old: [u32; 5] = core::array::from_fn(|i| THEMED[i].load(Relaxed));
    if old == new {
        return;
    }
    // SAFETY: `buf` is the terminal's pixel buffer of `WIDTH * HEIGHT` words, alive for the life of the console.
    let px = unsafe { core::slice::from_raw_parts_mut(buf, WIDTH * HEIGHT) };
    for p in px.iter_mut() {
        if let Some(i) = old.iter().position(|&o| o == *p) {
            *p = new[i];
        }
    }
    for (t, n) in THEMED.iter().zip(new) {
        t.store(n, Relaxed);
    }
    DIRTY_Y0.fetch_min(0, Relaxed);
    DIRTY_Y1.fetch_max(HEIGHT, Relaxed);
}

pub const CELL_W: usize = 8;
pub const CELL_H: usize = 19;
const MARGIN: usize = 4;
pub const COLS: usize = 80;
pub const ROWS: usize = 28;
/// The size of the buffer a console needs, in pixels.
pub const WIDTH: usize = 2 * MARGIN + COLS * CELL_W;
pub const HEIGHT: usize = 2 * MARGIN + ROWS * CELL_H;

/// A saved screen: see `Console::snapshot`.
pub struct Snapshot {
    runs: Vec<(u32, u32)>,
    cx: usize,
    cy: usize,
}

pub struct Console {
    fb: *mut u32,
    cx: usize,
    cy: usize,
    fg: u32,
    cursor_shown: bool,
}

/// The rows of the buffer that changed since `take_dirty` was last called (`NONE` = none). Statics, because the console reads them
/// while the shell, which owns the console, is busy waiting for a program.
static DIRTY_Y0: AtomicUsize = AtomicUsize::new(NONE);
static DIRTY_Y1: AtomicUsize = AtomicUsize::new(0);
const NONE: usize = usize::MAX;

impl Console {
    /// A console drawing into `fb`, a buffer of `WIDTH` x `HEIGHT` pixels.
    ///
    /// # Safety
    /// `fb` must point to `WIDTH * HEIGHT` mapped, writable words that stay mapped (and are used by nothing else in this task) for
    /// as long as the console lives.
    pub unsafe fn new(fb: *mut u32) -> Console {
        Console { fb, cx: 0, cy: 0, fg: FG, cursor_shown: false }
    }

    /// The pixel buffer, for the font routines (which bounds-check every write).
    fn px(&mut self) -> &'static mut [u32] {
        // SAFETY: `Console::new`'s contract: `fb` is `WIDTH * HEIGHT` mapped words for the console's life; the slice is used within
        // one drawing call and no other reference to the buffer is live then.
        unsafe { core::slice::from_raw_parts_mut(self.fb, WIDTH * HEIGHT) }
    }

    fn mark(&mut self, y0: usize, y1: usize) {
        DIRTY_Y0.fetch_min(y0, Relaxed);
        DIRTY_Y1.fetch_max(y1, Relaxed);
    }

    fn cell(col: usize, row: usize) -> (usize, usize) {
        (MARGIN + col * CELL_W, MARGIN + row * CELL_H)
    }

    /// Clear everything and put the cursor at the top left.
    pub fn reset(&mut self) {
        font::fill_rect(self.px(), WIDTH, 0, 0, WIDTH, HEIGHT, themed(BG));
        self.cx = 0;
        self.cy = 0;
        self.cursor_shown = false;
        self.mark(0, HEIGHT);
    }

    /// Clear the text area and put the cursor at the top left.
    pub fn clear(&mut self) {
        self.reset();
    }

    pub fn set_color(&mut self, color: u32) {
        self.fg = color;
    }

    fn cursor(&mut self, on: bool) {
        if on == self.cursor_shown {
            return;
        }
        let (x, y) = Self::cell(self.cx.min(COLS - 1), self.cy);
        font::fill_rect(self.px(), WIDTH, x, y + 16, CELL_W, 2, if on { themed(ACCENT) } else { themed(BG) });
        self.cursor_shown = on;
        self.mark(y, y + CELL_H);
    }

    fn newline(&mut self) {
        self.cx = 0;
        if self.cy + 1 < ROWS {
            self.cy += 1;
            return;
        }
        // scroll the text area up by one line
        // SAFETY: `fb` is the WIDTH x HEIGHT buffer; source rows MARGIN+CELL_H.. and destination rows MARGIN.. both end inside it ((ROWS-1)*CELL_H rows from row MARGIN+CELL_H <= HEIGHT-MARGIN); `copy` handles the overlap.
        unsafe { core::ptr::copy(self.fb.add((MARGIN + CELL_H) * WIDTH), self.fb.add(MARGIN * WIDTH), (ROWS - 1) * CELL_H * WIDTH) };
        font::fill_rect(self.px(), WIDTH, 0, MARGIN + (ROWS - 1) * CELL_H, WIDTH, CELL_H, themed(BG));
        self.mark(MARGIN, MARGIN + ROWS * CELL_H);
    }

    /// Write one character at the cursor. `\n` ends the line; other control characters are ignored.
    pub fn put(&mut self, ch: u8) {
        self.cursor(false);
        match ch {
            b'\n' => self.newline(),
            0x20..=0x7e => {
                if self.cx == COLS {
                    self.newline();
                }
                let (x, y) = Self::cell(self.cx, self.cy);
                font::draw_mono(self.px(), WIDTH, x, y, themed(self.fg), ch);
                self.cx += 1;
                self.mark(y, y + CELL_H);
            }
            _ => {}
        }
    }

    pub fn print(&mut self, s: &str) {
        for b in s.bytes() {
            self.put(b);
        }
    }

    /// Move the cursor back one cell and erase what was there.
    pub fn backspace(&mut self) {
        self.cursor(false);
        if self.cx == 0 {
            if self.cy == 0 {
                return;
            }
            self.cy -= 1;
            self.cx = COLS;
        }
        self.cx -= 1;
        let (x, y) = Self::cell(self.cx, self.cy);
        font::fill_rect(self.px(), WIDTH, x, y, CELL_W, CELL_H, themed(BG));
        self.mark(y, y + CELL_H);
    }

    /// Draw one character cell at (`col`, `row`) wherever it is: the colours swapped if `inverse` (a full-screen program's header and key hints).
    pub fn put_cell(&mut self, col: usize, row: usize, ch: u8, inverse: bool) {
        if col >= COLS || row >= ROWS {
            return;
        }
        self.cursor(false);
        let (x, y) = Self::cell(col, row);
        let (bg, fg) = if inverse { (themed(FG), themed(BG)) } else { (themed(BG), themed(FG)) };
        font::fill_rect(self.px(), WIDTH, x, y, CELL_W, CELL_H, bg);
        if (0x20..=0x7e).contains(&ch) && ch != b' ' {
            font::draw_mono(self.px(), WIDTH, x, y, fg, ch);
        }
        self.mark(y, y + CELL_H);
    }

    /// The screen as it is now, run-length coded (a console is mostly background, so it is small), and where the cursor is: what a full-screen
    /// program puts back when it ends (`restore`).
    pub fn snapshot(&mut self) -> Snapshot {
        self.cursor(false);
        // SAFETY: `fb` is the console's buffer of `WIDTH * HEIGHT` words.
        let px = unsafe { core::slice::from_raw_parts(self.fb, WIDTH * HEIGHT) };
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &p in px {
            match runs.last_mut() {
                Some((c, n)) if *c == p => *n += 1,
                _ => runs.push((p, 1)),
            }
        }
        Snapshot { runs, cx: self.cx, cy: self.cy }
    }

    /// Put a screen back (the whole buffer counts as changed).
    pub fn restore(&mut self, s: &Snapshot) {
        // SAFETY: as in `snapshot`.
        let px = unsafe { core::slice::from_raw_parts_mut(self.fb, WIDTH * HEIGHT) };
        let mut at = 0;
        for &(c, n) in &s.runs {
            let end = (at + n as usize).min(px.len());
            px[at..end].fill(c);
            at = end;
        }
        self.cx = s.cx;
        self.cy = s.cy;
        self.cursor_shown = false;
        self.mark(0, HEIGHT);
    }

    /// Put the (text) cursor at a cell.
    pub fn goto(&mut self, col: usize, row: usize) {
        self.cursor(false);
        self.cx = col.min(COLS - 1);
        self.cy = row.min(ROWS - 1);
    }

    /// Show the cursor. (Nothing is presented here: the console asks `take_dirty` and repaints.)
    pub fn flush(&mut self) {
        self.cursor(true);
    }

}

/// The rows (y0, y1) of the console's buffer that changed since the last call.
pub fn take_dirty() -> Option<(usize, usize)> {
    let (y0, y1) = (DIRTY_Y0.swap(NONE, Relaxed), DIRTY_Y1.swap(0, Relaxed));
    (y0 != NONE && y1 > y0).then_some((y0, y1.min(HEIGHT)))
}
