//! The terminal screen of the editor, laid out as a grid of characters (what nano looks like): a header line, the text, a message line and two
//! lines of shortcuts. The shell draws the grid with its console; here it is only data, so it can be tested.
use crate::{Editor, Mode, PromptKind};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// One character cell: its character and whether it is drawn reversed (light ground, dark ink) as nano does for the header, the message line's
/// shortcut keys and the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub inverse: bool,
}

pub struct Screen {
    pub cells: Vec<Vec<Cell>>,
    /// The cursor's place (column, row).
    pub cursor: (usize, usize),
}

/// Rows the editor uses for itself: the header above the text, and the message and two shortcut lines below.
pub const HEADER: usize = 1;
pub const FOOTER: usize = 3;

/// The size of the text area for a screen of `rows` x `cols`.
pub fn text_area(rows: usize, cols: usize) -> (usize, usize) {
    (rows.saturating_sub(HEADER + FOOTER).max(1), cols)
}

const SHORTCUTS: [[(&str, &str); 6]; 2] = [
    [("^G", "Help"), ("^O", "Write Out"), ("^W", "Where Is"), ("^K", "Cut"), ("^C", "Cur Pos"), ("^X", "Exit")],
    [("^R", "Read File"), ("^U/^V", "Paste"), ("^T", "Next Match"), ("^A", "Home"), ("^E", "End"), ("^Y", "Page Up")],
];

fn put(row: &mut [Cell], col: usize, text: &str, inverse: bool) -> usize {
    let mut c = col;
    for ch in text.chars() {
        if c < row.len() {
            row[c] = Cell { ch, inverse };
        }
        c += 1;
    }
    c
}

fn blank(cols: usize) -> Vec<Cell> {
    vec![Cell { ch: ' ', inverse: false }; cols]
}

/// Draw the whole screen. The editor's text area must have been sized with `text_area` (`Editor::resize`).
pub fn draw(ed: &Editor, rows: usize, cols: usize) -> Screen {
    let mut cells: Vec<Vec<Cell>> = (0..rows).map(|_| blank(cols)).collect();
    if rows < HEADER + FOOTER + 1 || cols < 20 {
        return Screen { cells, cursor: (0, 0) };
    }
    if ed.help {
        return help(rows, cols);
    }
    // header: the program, the file, whether it is modified
    let name = ed.path.clone().unwrap_or_else(|| String::from("New Buffer"));
    let mut head = blank(cols);
    for c in head.iter_mut() {
        c.inverse = true;
    }
    put(&mut head, 1, "Metal edit 1.0", true);
    let mid = format!("File: {name}");
    put(&mut head, (cols.saturating_sub(mid.len())) / 2, &mid, true);
    if ed.modified {
        put(&mut head, cols.saturating_sub(10), "Modified", true);
    }
    cells[0] = head;
    // the text
    let (trows, _) = text_area(rows, cols);
    for r in 0..trows {
        let y = ed.top + r;
        if y >= ed.line_count() {
            break;
        }
        let text = ed.row_cells(y);
        let row = &mut cells[HEADER + r];
        for (i, ch) in text.iter().enumerate() {
            row[i] = Cell { ch: *ch, inverse: false };
        }
        if let Some((a, b)) = ed.highlight_cells(y) {
            for c in row.iter_mut().take(b.min(cols)).skip(a) {
                c.inverse = true;
            }
        }
        // a line that goes on to the right of the screen is marked, as nano does
        if ed.left + cols < ed.display_col(y, ed.line(y).len()) {
            row[cols - 1] = Cell { ch: '$', inverse: true };
        }
    }
    // the message line, or the prompt
    let msg_row = rows - FOOTER;
    let mut cursor = ed.cursor_cell().map_or((0, HEADER), |(c, r)| (c, r + HEADER));
    if let Some(p) = &ed.prompt {
        let label = p.label();
        let end = put(&mut cells[msg_row], 0, label, true);
        if p.takes_text() {
            let shown: String = p.text.chars().rev().take(cols.saturating_sub(end + 2)).collect::<Vec<_>>().into_iter().rev().collect();
            let after = put(&mut cells[msg_row], end, &shown, false);
            cursor = (after.min(cols - 1), msg_row);
        } else {
            cursor = (cols - 1, msg_row);
        }
    } else if !ed.msg.is_empty() {
        let text = format!("[ {} ]", ed.msg);
        let at = cols.saturating_sub(text.len()) / 2;
        put(&mut cells[msg_row], at, &text, true);
    }
    // the shortcuts
    for (i, line) in SHORTCUTS.iter().enumerate() {
        let row = &mut cells[rows - 2 + i];
        let width = cols / 6;
        let items: Vec<(&str, &str)> = if ed.prompt.is_some() { if i == 0 { vec![("^C", "Cancel"), ("Enter", "Accept")] } else { vec![] } } else { line.to_vec() };
        for (k, (key, what)) in items.iter().enumerate() {
            let c = put(row, k * width, key, true);
            put(row, c + 1, what, false);
        }
    }
    Screen { cells, cursor }
}

fn help(rows: usize, cols: usize) -> Screen {
    let mut cells: Vec<Vec<Cell>> = (0..rows).map(|_| blank(cols)).collect();
    let lines = [
        "Metal edit: a small text editor (keys as in nano)",
        "",
        "^O  Write Out: save the file (asks for the name)     ^X  Exit (asks to save changes)",
        "^W  Where Is: search; Enter alone repeats it         ^T  go to the next match",
        "^K  Cut the current line (several in a row stay together)",
        "^U or ^V  Paste what was cut, in front of the current line",
        "^R  Read File: insert another file at the cursor",
        "^A  start of line    ^E  end of line    ^Y or Page Up  page up    Page Down  page down",
        "^C  show the line and column       ^L  redraw the screen",
        "Arrow keys, Home, End, Page Up, Page Down, Delete and Backspace work as usual.",
        "",
        "Press any key to go back.",
    ];
    for (i, l) in lines.iter().enumerate() {
        if i + 1 < rows {
            put(&mut cells[i + 1], 2, l, false);
        }
    }
    put(&mut cells[0], 0, &format!("{:<width$}", " Help", width = cols), true);
    Screen { cells, cursor: (0, 0) }
}

/// The keys the terminal editor understands, for a test or a help line: always `Mode::Nano`.
pub fn mode() -> Mode {
    Mode::Nano
}

/// Is this prompt asking for text (so the cursor goes to it)?
pub fn asks_text(ed: &Editor) -> bool {
    matches!(&ed.prompt, Some(p) if p.kind != PromptKind::SaveFirst(crate::Then::Exit) && p.takes_text())
}
