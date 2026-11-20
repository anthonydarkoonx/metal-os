//! The editor in the terminal: `edit FILE` (or `nano FILE`) takes over the console with a nano-style full-screen editor (`metallium-edit`), working on
//! the shell's own VFS session. This file only draws the editor's character grid with the console and feeds it keys; everything else is the library.
use crate::console::{Console, COLS, ROWS};
use crate::vfs::{self, Vfs};
use alloc::string::String;
use alloc::vec::Vec;
use metallium_edit::term::{self, Cell};
use metallium_edit::{Editor, IoError, Mode, Outcome, Storage};

struct VfsIo<'a>(&'a Vfs);

fn io_error(e: i64) -> IoError {
    match e as u64 {
        vfs::ENOENT => IoError::NotFound,
        vfs::EROFS => IoError::Other(String::from("read-only file system")),
        vfs::ENOSPC => IoError::Other(String::from("no space left")),
        vfs::EISDIR => IoError::Other(String::from("it is a folder")),
        other => IoError::Other(alloc::format!("error {other}")),
    }
}

impl Storage for VfsIo<'_> {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, IoError> {
        self.0.read_file(path).map_err(io_error)
    }
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), IoError> {
        self.0.write_file(path, data).map_err(io_error)
    }
}

pub struct TermEdit {
    ed: Editor,
    /// What is on the screen now, so that only the cells that change are drawn.
    shown: Vec<Vec<Cell>>,
}

impl TermEdit {
    /// Start editing `file` (relative to `cwd`), or a new buffer when there is none.
    pub fn new(vfs: &Vfs, cwd: &str, file: Option<&str>) -> TermEdit {
        let mut ed = Editor::new(Mode::Nano);
        ed.cwd = String::from(cwd);
        let (rows, cols) = term::text_area(ROWS, COLS);
        ed.resize(rows, cols);
        if let Some(f) = file {
            ed.open(&mut VfsIo(vfs), f);
        }
        TermEdit { ed, shown: Vec::new() }
    }

    /// One key. `Exit` when the editor is done.
    pub fn key(&mut self, vfs: &Vfs, k: u8) -> Outcome {
        if k == 0x0c {
            self.shown.clear(); // Ctrl-L: draw everything again
            return Outcome::Continue;
        }
        self.ed.key(&mut VfsIo(vfs), k)
    }

    /// Draw what changed since the last time and put the cursor where the editor's is.
    pub fn draw(&mut self, con: &mut Console) {
        let screen = term::draw(&self.ed, ROWS, COLS);
        for (r, row) in screen.cells.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                if self.shown.get(r).and_then(|x| x.get(c)) != Some(cell) {
                    con.put_cell(c, r, if cell.ch.is_ascii() { cell.ch as u8 } else { b'?' }, cell.inverse);
                }
            }
        }
        self.shown = screen.cells;
        con.goto(screen.cursor.0, screen.cursor.1);
    }
}
