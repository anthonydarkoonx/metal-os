//! A text editor as pure state. One `Editor` serves two front ends: the terminal (`term`: a nano-style screen laid out in a character grid, with
//! Ctrl-O to write out, Ctrl-X to exit, Ctrl-K / Ctrl-U to cut and paste lines, Ctrl-W to search ...) and the window (`gui`: menu bar, mouse,
//! Ctrl-S / Ctrl-C / Ctrl-V ...). Files come through the [`Storage`] trait, so the same code runs on the real file system and in host tests.
//!
//! Text is a list of lines of characters. The font is ASCII, so anything else shows as `?` (it is kept as it is in the buffer and written back as
//! UTF-8). A tab is four columns wide on screen. There is no undo yet.
#![no_std]
extern crate alloc;

pub mod term;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// How the file manager asks the editor to open a file: it writes the path into this file (it sees the whole file system); the editor reads and
/// removes it. (The editor is started or brought forward through the taskbar, see `LAUNCH_REQUEST`.)
pub const OPEN_REQUEST: &str = "/data/programdata/open-in-edit";
/// A program that cannot start programs asks the taskbar to: it writes a program id here (the taskbar's own directory, `/data/launch` for the
/// taskbar, which is `/data/programdata/taskbar/launch` for a program that sees the whole file system); the taskbar reads, removes and obeys.
pub const LAUNCH_REQUEST: &str = "/data/programdata/taskbar/launch";

/// Columns a tab character takes (it moves to the next multiple of this).
pub const TAB: usize = 4;
/// The largest file the editor opens, and the most lines it holds.
pub const MAX_BYTES: usize = 1024 * 1024;
pub const MAX_LINES: usize = 60_000;

/// Keys as the keyboard driver sends them.
pub const K_BACKSPACE: u8 = 0x08;
pub const K_UP: u8 = 0x80;
pub const K_DOWN: u8 = 0x81;
pub const K_LEFT: u8 = 0x82;
pub const K_RIGHT: u8 = 0x83;
pub const K_HOME: u8 = 0x84;
pub const K_END: u8 = 0x85;
pub const K_PGUP: u8 = 0x86;
pub const K_PGDN: u8 = 0x87;
pub const K_DELETE: u8 = 0x88;

/// Why a file could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IoError {
    NotFound,
    Other(String),
}

impl IoError {
    fn words(&self) -> String {
        match self {
            IoError::NotFound => String::from("no such file"),
            IoError::Other(s) => s.clone(),
        }
    }
}

/// Where files come from and go to.
pub trait Storage {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, IoError>;
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), IoError>;
}

/// Which keys do what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// nano: Ctrl-O write out, Ctrl-X exit, Ctrl-K cut a line, Ctrl-U or Ctrl-V paste, Ctrl-W search, Ctrl-G help ... (Page Down is its key)
    Nano,
    /// A window: Ctrl-S save, Ctrl-O open, Ctrl-X cut, Ctrl-C copy, Ctrl-V paste, Ctrl-A select all, Ctrl-F find, Ctrl-Q quit.
    Gui,
}

/// What to do once a modified buffer has been dealt with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    Exit,
    New,
    Open,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// Ctrl-O / Save As: the file name to write.
    WriteOut,
    /// File > Open: the file name to read.
    Open,
    /// Ctrl-R: a file to insert at the cursor.
    ReadFile,
    Search,
    /// "Save modified buffer?": Y, N or cancel.
    SaveFirst(Then),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub text: String,
}

impl Prompt {
    /// The question as it is shown in front of the text.
    pub fn label(&self) -> &'static str {
        match self.kind {
            PromptKind::WriteOut => "File Name to Write: ",
            PromptKind::Open => "File to Open: ",
            PromptKind::ReadFile => "File to Insert: ",
            PromptKind::Search => "Search: ",
            PromptKind::SaveFirst(_) => "Save modified buffer?  (Y = yes, N = no, Esc = cancel)",
        }
    }

    pub fn takes_text(&self) -> bool {
        !matches!(self.kind, PromptKind::SaveFirst(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Exit,
}

pub struct Editor {
    lines: Vec<Vec<char>>,
    pub cy: usize,
    pub cx: usize,
    /// The display column up and down arrows try to keep.
    want: usize,
    /// First visible line and first visible display column.
    pub top: usize,
    pub left: usize,
    /// Size of the text area in characters.
    pub rows: usize,
    pub cols: usize,
    /// The other end of the selection (line, character), the cursor being the first.
    anchor: Option<(usize, usize)>,
    /// A search hit to highlight: line and the character range.
    hit: Option<(usize, usize, usize)>,
    pub modified: bool,
    pub path: Option<String>,
    /// Relative file names are taken from here.
    pub cwd: String,
    pub clip: String,
    cut_chain: bool,
    pub msg: String,
    pub prompt: Option<Prompt>,
    pub help: bool,
    pub mode: Mode,
    last_search: String,
    /// What to do after a "save first" prompt was answered.
    then: Option<Then>,
}

fn clean(c: char) -> char {
    if (' '..='~').contains(&c) {
        c
    } else {
        '?'
    }
}

impl Editor {
    pub fn new(mode: Mode) -> Editor {
        Editor {
            lines: vec![Vec::new()],
            cy: 0,
            cx: 0,
            want: 0,
            top: 0,
            left: 0,
            rows: 20,
            cols: 80,
            anchor: None,
            hit: None,
            modified: false,
            path: None,
            cwd: String::from("/"),
            clip: String::new(),
            cut_chain: false,
            msg: String::new(),
            prompt: None,
            help: false,
            mode,
            last_search: String::new(),
            then: None,
        }
    }

    // ---- the text ----------------------------------------------------------------------------------------------------------------
    /// The text as a file holds it: lines joined with newlines and a newline at the end (unless it is empty).
    pub fn text(&self) -> String {
        let mut s = String::new();
        for (i, l) in self.lines.iter().enumerate() {
            if i + 1 == self.lines.len() && l.is_empty() {
                break;
            }
            s.extend(l.iter());
            s.push('\n');
        }
        s
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(|l| l.trim_end_matches('\r').chars().collect()).collect();
        if self.lines.len() > 1 && self.lines.last().is_some_and(|l| l.is_empty()) {
            self.lines.pop(); // (the newline at the end of the file is not an empty last line)
        }
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        self.cy = 0;
        self.cx = 0;
        self.want = 0;
        self.top = 0;
        self.left = 0;
        self.anchor = None;
        self.hit = None;
        self.modified = false;
        self.cut_chain = false;
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, i: usize) -> &[char] {
        &self.lines[i]
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        self.scroll();
    }

    // ---- columns on screen -------------------------------------------------------------------------------------------------------
    /// The display column of character `idx` of line `y` (tabs advance to the next multiple of `TAB`).
    pub fn display_col(&self, y: usize, idx: usize) -> usize {
        let mut col = 0;
        for &c in self.lines[y].iter().take(idx) {
            col = if c == '\t' { (col / TAB + 1) * TAB } else { col + 1 };
        }
        col
    }

    /// The character of line `y` that display column `col` is on or nearest (the end of the line if it is shorter; the right half of a tab
    /// counts as after it).
    fn idx_at_col(&self, y: usize, col: usize) -> usize {
        let mut c = 0;
        for (i, &ch) in self.lines[y].iter().enumerate() {
            let next = if ch == '\t' { (c / TAB + 1) * TAB } else { c + 1 };
            if col < next {
                return if next - c > 1 && col >= c + (next - c) / 2 { i + 1 } else { i };
            }
            c = next;
        }
        self.lines[y].len()
    }

    /// Line `y` as cells for the columns `left..left + cols`: printable characters, tabs as spaces. (The selected cell range, if any, is
    /// `selection_cells`.)
    pub fn row_cells(&self, y: usize) -> Vec<char> {
        let mut out = Vec::new();
        let mut col = 0;
        for &c in &self.lines[y] {
            if c == '\t' {
                let next = (col / TAB + 1) * TAB;
                while col < next {
                    if col >= self.left && col < self.left + self.cols {
                        out.push(' ');
                    }
                    col += 1;
                }
            } else {
                if col >= self.left && col < self.left + self.cols {
                    out.push(clean(c));
                }
                col += 1;
            }
        }
        out
    }

    /// The cells of line `y`, as screen columns, that are selected or are a search hit: `Some((from, to))` relative to the first visible column.
    pub fn highlight_cells(&self, y: usize) -> Option<(usize, usize)> {
        let (from, to) = if let Some(((y0, x0), (y1, x1))) = self.selection() {
            if y < y0 || y > y1 {
                return None;
            }
            let a = if y == y0 { x0 } else { 0 };
            let b = if y == y1 { x1 } else { self.lines[y].len() + 1 };
            (a, b)
        } else if let Some((hy, a, b)) = self.hit {
            if hy != y {
                return None;
            }
            (a, b)
        } else {
            return None;
        };
        let start = self.display_col(y, from.min(self.lines[y].len()));
        let end = if to > self.lines[y].len() { self.display_col(y, self.lines[y].len()) + 1 } else { self.display_col(y, to) };
        let (s, e) = (start.max(self.left), end.min(self.left + self.cols));
        (e > s).then(|| (s - self.left, e - self.left))
    }

    /// Keep the cursor on screen.
    pub fn scroll(&mut self) {
        if self.cy < self.top {
            self.top = self.cy;
        } else if self.cy >= self.top + self.rows {
            self.top = self.cy + 1 - self.rows;
        }
        let col = self.display_col(self.cy, self.cx);
        if col < self.left {
            self.left = col;
        } else if col >= self.left + self.cols {
            self.left = col + 1 - self.cols;
        }
    }

    /// Where the cursor is on the text area: (column, row), if it is inside it.
    pub fn cursor_cell(&self) -> Option<(usize, usize)> {
        let col = self.display_col(self.cy, self.cx);
        (self.cy >= self.top && self.cy < self.top + self.rows && col >= self.left && col < self.left + self.cols).then(|| (col - self.left, self.cy - self.top))
    }

    // ---- selection ---------------------------------------------------------------------------------------------------------------
    /// The selection as ((line, char), (line, char)), start before end.
    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let a = self.anchor?;
        let b = (self.cy, self.cx);
        if a == b {
            return None;
        }
        Some(if a < b { (a, b) } else { (b, a) })
    }

    pub fn selected_text(&self) -> Option<String> {
        let ((y0, x0), (y1, x1)) = self.selection()?;
        let mut s = String::new();
        for y in y0..=y1 {
            let a = if y == y0 { x0 } else { 0 };
            let b = if y == y1 { x1 } else { self.lines[y].len() };
            s.extend(self.lines[y][a..b].iter());
            if y != y1 {
                s.push('\n');
            }
        }
        Some(s)
    }

    pub fn select_all(&mut self) {
        self.anchor = Some((0, 0));
        self.cy = self.lines.len() - 1;
        self.cx = self.lines[self.cy].len();
        self.want = self.display_col(self.cy, self.cx);
        self.hit = None;
        self.scroll();
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Delete the selection (if there is one): whether anything was deleted.
    fn delete_selection(&mut self) -> bool {
        let Some(((y0, x0), (y1, x1))) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let tail: Vec<char> = self.lines[y1][x1..].to_vec();
        self.lines[y0].truncate(x0);
        self.lines[y0].extend(tail);
        self.lines.drain(y0 + 1..=y1);
        self.cy = y0;
        self.cx = x0;
        self.anchor = None;
        self.modified = true;
        self.want = self.display_col(self.cy, self.cx);
        true
    }

    // ---- editing -----------------------------------------------------------------------------------------------------------------
    fn touch(&mut self) {
        self.modified = true;
        self.cut_chain = false;
        self.hit = None;
        self.msg.clear();
    }

    pub fn insert_char(&mut self, c: char) {
        if c == '\n' {
            return self.newline();
        }
        self.delete_selection();
        if self.lines[self.cy].len() >= 4000 {
            self.msg = String::from("Line too long");
            return;
        }
        self.lines[self.cy].insert(self.cx, c);
        self.cx += 1;
        self.touch();
        self.want = self.display_col(self.cy, self.cx);
        self.scroll();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        for c in s.chars() {
            if c == '\n' {
                self.newline();
            } else if c != '\r' {
                self.insert_char(c);
            }
        }
    }

    pub fn tab(&mut self) {
        let col = self.display_col(self.cy, self.cx);
        for _ in 0..TAB - col % TAB {
            self.insert_char(' ');
        }
    }

    pub fn newline(&mut self) {
        self.delete_selection();
        if self.lines.len() >= MAX_LINES {
            self.msg = String::from("Too many lines");
            return;
        }
        let rest: Vec<char> = self.lines[self.cy].split_off(self.cx);
        self.lines.insert(self.cy + 1, rest);
        self.cy += 1;
        self.cx = 0;
        self.touch();
        self.want = 0;
        self.scroll();
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            self.touch();
            return self.scroll();
        }
        if self.cx > 0 {
            self.cx -= 1;
            self.lines[self.cy].remove(self.cx);
        } else if self.cy > 0 {
            let line = self.lines.remove(self.cy);
            self.cy -= 1;
            self.cx = self.lines[self.cy].len();
            self.lines[self.cy].extend(line);
        } else {
            return;
        }
        self.touch();
        self.want = self.display_col(self.cy, self.cx);
        self.scroll();
    }

    pub fn delete(&mut self) {
        if self.delete_selection() {
            self.touch();
            return self.scroll();
        }
        if self.cx < self.lines[self.cy].len() {
            self.lines[self.cy].remove(self.cx);
        } else if self.cy + 1 < self.lines.len() {
            let next = self.lines.remove(self.cy + 1);
            self.lines[self.cy].extend(next);
        } else {
            return;
        }
        self.touch();
        self.scroll();
    }

    // ---- moving ------------------------------------------------------------------------------------------------------------------
    fn moved(&mut self, keep_want: bool) {
        self.cut_chain = false;
        self.hit = None;
        if !keep_want {
            self.want = self.display_col(self.cy, self.cx);
        }
        self.scroll();
    }

    pub fn left(&mut self) {
        self.anchor = None;
        if self.cx > 0 {
            self.cx -= 1;
        } else if self.cy > 0 {
            self.cy -= 1;
            self.cx = self.lines[self.cy].len();
        }
        self.moved(false);
    }

    pub fn right(&mut self) {
        self.anchor = None;
        if self.cx < self.lines[self.cy].len() {
            self.cx += 1;
        } else if self.cy + 1 < self.lines.len() {
            self.cy += 1;
            self.cx = 0;
        }
        self.moved(false);
    }

    pub fn up(&mut self) {
        self.anchor = None;
        if self.cy > 0 {
            self.cy -= 1;
            self.cx = self.idx_at_col(self.cy, self.want);
        }
        self.moved(true);
    }

    pub fn down(&mut self) {
        self.anchor = None;
        if self.cy + 1 < self.lines.len() {
            self.cy += 1;
            self.cx = self.idx_at_col(self.cy, self.want);
        }
        self.moved(true);
    }

    pub fn home(&mut self) {
        self.anchor = None;
        self.cx = 0;
        self.moved(false);
    }

    pub fn end(&mut self) {
        self.anchor = None;
        self.cx = self.lines[self.cy].len();
        self.moved(false);
    }

    pub fn page_up(&mut self) {
        self.anchor = None;
        self.cy = self.cy.saturating_sub(self.rows.saturating_sub(1).max(1));
        self.top = self.top.saturating_sub(self.rows.saturating_sub(1).max(1));
        self.cx = self.idx_at_col(self.cy, self.want);
        self.moved(true);
    }

    pub fn page_down(&mut self) {
        self.anchor = None;
        let step = self.rows.saturating_sub(1).max(1);
        self.cy = (self.cy + step).min(self.lines.len() - 1);
        self.cx = self.idx_at_col(self.cy, self.want);
        self.moved(true);
    }

    pub fn goto_start(&mut self) {
        self.anchor = None;
        self.cy = 0;
        self.cx = 0;
        self.moved(false);
    }

    pub fn goto_end(&mut self) {
        self.anchor = None;
        self.cy = self.lines.len() - 1;
        self.cx = self.lines[self.cy].len();
        self.moved(false);
    }

    /// The pointer pressed at screen cell (col, row) of the text area: put the cursor there. `extend` keeps the anchor (a drag).
    pub fn click(&mut self, col: usize, row: usize, extend: bool) {
        let y = (self.top + row).min(self.lines.len() - 1);
        let c = self.left + col;
        if !extend {
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Some((self.cy, self.cx));
        }
        self.cy = y;
        self.cx = self.idx_at_col(y, c);
        if !extend {
            self.anchor = Some((self.cy, self.cx)); // (a drag starts here; with nothing between anchor and cursor there is no selection)
        }
        self.moved(false);
    }

    /// Scroll the view by lines without moving the cursor (the wheel).
    pub fn scroll_by(&mut self, lines: i32) {
        let max_top = self.lines.len().saturating_sub(1) as i32;
        self.top = (self.top as i32 + lines).clamp(0, max_top) as usize;
    }

    // ---- cut, copy, paste --------------------------------------------------------------------------------------------------------
    /// nano's Ctrl-K: cut the line the cursor is on (several in a row go into the clipboard together).
    pub fn cut_line(&mut self) {
        let chain = self.cut_chain;
        let mut text: String = self.lines[self.cy].iter().collect();
        text.push('\n');
        if self.lines.len() == 1 {
            self.lines[0].clear();
            self.cx = 0;
        } else {
            self.lines.remove(self.cy);
            if self.cy >= self.lines.len() {
                self.cy = self.lines.len() - 1;
            }
            self.cx = self.cx.min(self.lines[self.cy].len());
        }
        if chain {
            self.clip.push_str(&text);
        } else {
            self.clip = text;
        }
        self.touch();
        self.cut_chain = true;
        self.scroll();
    }

    pub fn cut_selection(&mut self) -> bool {
        let Some(t) = self.selected_text() else { return false };
        self.clip = t;
        self.delete_selection();
        self.touch();
        self.scroll();
        true
    }

    pub fn copy_selection(&mut self) -> bool {
        let Some(t) = self.selected_text() else { return false };
        self.clip = t;
        true
    }

    /// Put the clipboard at the cursor (a pasted line from `cut_line` goes in front of the current line, like nano).
    pub fn paste(&mut self) {
        if self.clip.is_empty() {
            self.msg = String::from("Nothing to paste");
            return;
        }
        let clip = self.clip.clone();
        if self.mode == Mode::Nano && clip.ends_with('\n') && self.selection().is_none() {
            // whole lines: they go before the cursor's line
            let at = self.cy;
            let new: Vec<Vec<char>> = clip.trim_end_matches('\n').split('\n').map(|l| l.chars().collect()).collect();
            if self.lines.len() + new.len() > MAX_LINES {
                self.msg = String::from("Too many lines");
                return;
            }
            let n = new.len();
            for (i, l) in new.into_iter().enumerate() {
                self.lines.insert(at + i, l);
            }
            self.cy = at + n;
            self.cx = self.cx.min(self.lines[self.cy].len());
            self.touch();
            self.moved(false);
            return;
        }
        self.insert_str(&clip);
        self.scroll();
    }

    // ---- searching ---------------------------------------------------------------------------------------------------------------
    /// Look for `needle` (any case) after the cursor, wrapping round the end. Returns whether it was found; the cursor goes to it.
    pub fn search(&mut self, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        self.last_search = String::from(needle);
        let pat: Vec<char> = needle.chars().flat_map(|c| c.to_lowercase()).collect();
        let n = self.lines.len();
        for k in 0..=n {
            let y = (self.cy + k) % n;
            let low: Vec<char> = self.lines[y].iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
            let from = if k == 0 { self.cx + 1 } else { 0 };
            let upto = if k == n { self.cx + pat.len() } else { usize::MAX };
            if low.len() < pat.len() {
                continue;
            }
            for x in from..=low.len() - pat.len() {
                if x >= upto {
                    break;
                }
                if low[x..x + pat.len()] == pat[..] {
                    if y < self.cy || (y == self.cy && x <= self.cx) {
                        self.msg = String::from("Search Wrapped");
                    } else {
                        self.msg.clear();
                    }
                    self.anchor = None;
                    self.cy = y;
                    self.cx = x;
                    self.hit = Some((y, x, x + pat.len()));
                    self.want = self.display_col(y, x);
                    self.scroll();
                    return true;
                }
            }
        }
        self.msg = format!("\"{needle}\" not found");
        false
    }

    /// Look for the last search again.
    pub fn search_again(&mut self) -> bool {
        let s = self.last_search.clone();
        if s.is_empty() {
            self.msg = String::from("Nothing to search for");
            return false;
        }
        self.search(&s)
    }

    pub fn last_search(&self) -> &str {
        &self.last_search
    }

    // ---- files -------------------------------------------------------------------------------------------------------------------
    /// An absolute path for what was typed.
    pub fn resolve(&self, name: &str) -> String {
        let full = if name.starts_with('/') { String::from(name) } else { format!("{}/{}", self.cwd.trim_end_matches('/'), name) };
        let mut parts: Vec<&str> = Vec::new();
        for c in full.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                x => parts.push(x),
            }
        }
        format!("/{}", parts.join("/"))
    }

    /// Open a file into the buffer (replacing it). A file that does not exist is a new, empty one with that name (as in nano).
    pub fn open(&mut self, io: &mut dyn Storage, name: &str) -> bool {
        let path = self.resolve(name);
        match io.read(&path) {
            Ok(data) => {
                if data.len() > MAX_BYTES {
                    self.msg = format!("\"{path}\" is too big to edit ({} KiB; the limit is {} KiB)", data.len() / 1024, MAX_BYTES / 1024);
                    return false;
                }
                if data.contains(&0) {
                    self.msg = format!("\"{path}\" is not a text file");
                    return false;
                }
                let text = String::from_utf8_lossy(&data).into_owned();
                if text.split('\n').count() > MAX_LINES {
                    self.msg = format!("\"{path}\" has too many lines");
                    return false;
                }
                self.set_text(&text);
                self.msg = format!("Read {} lines", self.lines.len() - (self.is_empty() as usize));
            }
            Err(IoError::NotFound) => {
                self.set_text("");
                self.msg = String::from("New File");
            }
            Err(e) => {
                self.msg = format!("Error reading {path}: {}", e.words());
                return false;
            }
        }
        self.path = Some(path);
        true
    }

    /// Write the buffer to `name` (or to its own path).
    pub fn save_to(&mut self, io: &mut dyn Storage, name: &str) -> bool {
        let path = self.resolve(name);
        let text = self.text();
        match io.write(&path, text.as_bytes()) {
            Ok(()) => {
                self.modified = false;
                self.msg = format!("Wrote {} lines to {path}", self.lines.len() - self.is_empty() as usize);
                self.path = Some(path);
                true
            }
            Err(e) => {
                self.msg = format!("Error writing {path}: {}", e.words());
                false
            }
        }
    }

    /// Save to the buffer's own file, or ask for a name.
    pub fn save(&mut self, io: &mut dyn Storage) -> bool {
        match self.path.clone() {
            Some(p) => self.save_to(io, &p),
            None => {
                self.ask(PromptKind::WriteOut, "");
                false
            }
        }
    }

    pub fn new_buffer(&mut self) {
        self.set_text("");
        self.path = None;
        self.msg.clear();
    }

    /// Insert the contents of a file at the cursor.
    pub fn insert_file(&mut self, io: &mut dyn Storage, name: &str) {
        let path = self.resolve(name);
        match io.read(&path) {
            Ok(d) if d.len() <= MAX_BYTES && !d.contains(&0) => {
                let t = String::from_utf8_lossy(&d).into_owned();
                self.insert_str(&t);
                self.msg = format!("Inserted {path}");
            }
            Ok(d) if d.contains(&0) => self.msg = format!("\"{path}\" is not a text file"),
            Ok(_) => self.msg = format!("\"{path}\" is too big"),
            Err(e) => self.msg = format!("Error reading {path}: {}", e.words()),
        }
    }

    // ---- prompts -----------------------------------------------------------------------------------------------------------------
    pub fn ask(&mut self, kind: PromptKind, text: &str) {
        self.prompt = Some(Prompt { kind, text: String::from(text) });
        self.help = false;
    }

    /// The user wants to leave (or start something new that replaces the buffer): ask first if there are unsaved changes. Whether it can go on.
    pub fn leave(&mut self, then: Then) -> bool {
        if self.modified {
            self.ask(PromptKind::SaveFirst(then), "");
            false
        } else {
            true
        }
    }

    fn proceed(&mut self, then: Then) -> Outcome {
        match then {
            Then::Exit => return Outcome::Exit,
            Then::New => self.new_buffer(),
            Then::Open => self.ask(PromptKind::Open, ""),
        }
        Outcome::Continue
    }

    fn finish_prompt(&mut self, io: &mut dyn Storage) -> Outcome {
        let Some(p) = self.prompt.take() else { return Outcome::Continue };
        self.apply_prompt(io, p.kind, &p.text)
    }

    /// Carry out an answer to a question: `text` is what was typed or chosen (a file name, a search). The window's file chooser, which asks
    /// instead of the text prompt, calls this with the path it was given.
    pub fn apply_prompt(&mut self, io: &mut dyn Storage, kind: PromptKind, text: &str) -> Outcome {
        let name = text.trim();
        match kind {
            PromptKind::WriteOut => {
                if name.is_empty() {
                    self.msg = String::from("Cancelled");
                } else if self.save_to(io, name) {
                    if let Some(t) = self.then.take() {
                        return self.proceed(t);
                    }
                }
                self.then = None;
            }
            PromptKind::Open => {
                if !name.is_empty() {
                    self.open(io, name);
                }
            }
            PromptKind::ReadFile => {
                if !name.is_empty() {
                    self.insert_file(io, name);
                }
            }
            PromptKind::Search => {
                let needle = if name.is_empty() { self.last_search.clone() } else { String::from(text) };
                if !needle.is_empty() {
                    self.search(&needle);
                }
            }
            PromptKind::SaveFirst(_) => {}
        }
        Outcome::Continue
    }

    /// The question was given up (Escape, Cancel).
    pub fn cancel_prompt(&mut self) {
        self.prompt = None;
        self.then = None;
        self.msg = String::from("Cancelled");
    }

    fn prompt_key(&mut self, io: &mut dyn Storage, k: u8) -> Outcome {
        let Some(p) = self.prompt.as_mut() else { return Outcome::Continue };
        if let PromptKind::SaveFirst(then) = p.kind {
            match k {
                b'y' | b'Y' => {
                    self.prompt = None;
                    if self.path.is_some() {
                        let path = self.path.clone().unwrap_or_default();
                        if self.save_to(io, &path) {
                            return self.proceed(then);
                        }
                    } else {
                        self.then = Some(then);
                        self.ask(PromptKind::WriteOut, "");
                    }
                }
                b'n' | b'N' => {
                    self.prompt = None;
                    self.modified = false; // (what the user chose to throw away)
                    return self.proceed(then);
                }
                27 | 0x03 => {
                    self.prompt = None;
                    self.msg = String::from("Cancelled");
                }
                _ => {}
            }
            return Outcome::Continue;
        }
        match k {
            10 | 13 => return self.finish_prompt(io),
            27 | 0x03 => {
                self.prompt = None;
                self.then = None;
                self.msg = String::from("Cancelled");
            }
            K_BACKSPACE | 127 => {
                p.text.pop();
            }
            0x20..=0x7e if p.text.len() < 200 => p.text.push(k as char),
            _ => {}
        }
        Outcome::Continue
    }

    // ---- keys --------------------------------------------------------------------------------------------------------------------
    /// A typed character or special key. Returns `Exit` when the editor should end.
    pub fn key(&mut self, io: &mut dyn Storage, k: u8) -> Outcome {
        if self.prompt.is_some() {
            return self.prompt_key(io, k);
        }
        if self.help {
            self.help = false;
            return Outcome::Continue;
        }
        match k {
            0x20..=0x7e => self.insert_char(k as char),
            10 | 13 => self.newline(),
            9 => self.tab(),
            K_BACKSPACE => self.backspace(),
            K_DELETE | 127 => self.delete(),
            K_UP => self.up(),
            K_DOWN => self.down(),
            K_LEFT => self.left(),
            K_RIGHT => self.right(),
            K_HOME => self.home(),
            K_END => self.end(),
            K_PGUP => self.page_up(),
            K_PGDN => self.page_down(),
            _ => return self.control(io, k),
        }
        Outcome::Continue
    }

    fn control(&mut self, io: &mut dyn Storage, k: u8) -> Outcome {
        match self.mode {
            Mode::Nano => match k {
                0x0F => {
                    let name = self.path.clone().unwrap_or_default();
                    self.ask(PromptKind::WriteOut, &name);
                }
                0x13 => {
                    self.save(io);
                }
                0x18 => {
                    if self.leave(Then::Exit) {
                        return Outcome::Exit;
                    }
                }
                0x0B => self.cut_line(),
                0x15 | 0x16 => self.paste(),
                0x17 => {
                    let last = self.last_search.clone();
                    self.ask(PromptKind::Search, &last);
                    if let Some(p) = self.prompt.as_mut() {
                        p.text.clear();
                    }
                }
                0x14 => {
                    self.search_again();
                }
                0x12 => self.ask(PromptKind::ReadFile, ""),
                0x07 => self.help = true,
                0x01 => self.home(),
                0x05 => self.end(),
                0x19 => self.page_up(),
                0x03 => {
                    self.msg = format!("line {}/{}, col {}", self.cy + 1, self.lines.len(), self.display_col(self.cy, self.cx) + 1);
                }
                _ => {}
            },
            Mode::Gui => match k {
                0x13 => {
                    self.save(io);
                }
                0x0F => {
                    if self.leave(Then::Open) {
                        self.ask(PromptKind::Open, "");
                    }
                }
                0x0E => {
                    if self.leave(Then::New) {
                        self.new_buffer();
                    }
                }
                0x11 => {
                    if self.leave(Then::Exit) {
                        return Outcome::Exit;
                    }
                }
                0x18 => {
                    if !self.cut_selection() {
                        self.msg = String::from("Select something to cut");
                    }
                }
                0x03 => {
                    if !self.copy_selection() {
                        self.msg = String::from("Select something to copy");
                    }
                }
                0x16 => self.paste(),
                0x01 => self.select_all(),
                0x06 => {
                    let last = self.last_search.clone();
                    self.ask(PromptKind::Search, &last);
                }
                0x07 => {
                    self.search_again();
                }
                _ => {}
            },
        }
        Outcome::Continue
    }
}

#[cfg(test)]
mod tests;
