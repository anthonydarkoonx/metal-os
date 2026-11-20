extern crate std;
use super::*;
use crate::term;
use alloc::collections::BTreeMap;
use std::vec::Vec as StdVec;

#[derive(Default)]
struct Mem {
    files: BTreeMap<String, Vec<u8>>,
    log: StdVec<String>,
}

impl Storage for Mem {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, IoError> {
        self.files.get(path).cloned().ok_or(IoError::NotFound)
    }
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), IoError> {
        if path.starts_with("/system") {
            return Err(IoError::Other("read-only file system".into()));
        }
        self.log.push(format!("write {path} {} bytes", data.len()));
        self.files.insert(path.into(), data.to_vec());
        Ok(())
    }
}

fn nano() -> Editor {
    let mut e = Editor::new(Mode::Nano);
    e.cwd = String::from("/data");
    e.resize(10, 40);
    e
}

fn type_str(e: &mut Editor, io: &mut Mem, s: &str) {
    for b in s.bytes() {
        e.key(io, b);
    }
}

#[test]
fn text_round_trips_and_a_final_newline_is_not_an_extra_line() {
    let mut e = nano();
    e.set_text("one\ntwo\n");
    assert_eq!(e.line_count(), 2);
    assert_eq!(e.text(), "one\ntwo\n");
    e.set_text("no newline at the end");
    assert_eq!(e.text(), "no newline at the end\n");
    e.set_text("");
    assert!(e.is_empty() && e.text() == "");
    e.set_text("a\r\nb\r\n");
    assert_eq!(e.text(), "a\nb\n", "carriage returns go");
    e.set_text("x\n\n\ny\n");
    assert_eq!(e.line_count(), 4, "blank lines are kept");
}

#[test]
fn typing_and_deleting_edit_the_text_and_set_the_modified_flag() {
    let mut e = nano();
    let mut io = Mem::default();
    assert!(!e.modified);
    type_str(&mut e, &mut io, "hello");
    assert!(e.modified);
    assert_eq!(e.text(), "hello\n");
    e.key(&mut io, 10);
    type_str(&mut e, &mut io, "world");
    assert_eq!(e.text(), "hello\nworld\n");
    // backspace joins lines at the start of one
    for _ in 0..5 {
        e.key(&mut io, K_BACKSPACE);
    }
    e.key(&mut io, K_BACKSPACE);
    assert_eq!(e.text(), "hello\n");
    assert_eq!((e.cy, e.cx), (0, 5));
    // delete at the end of a line joins the next
    e.set_text("ab\ncd\n");
    e.cx = 2;
    e.key(&mut io, K_DELETE);
    assert_eq!(e.text(), "abcd\n");
    // Enter splits a line in the middle
    e.cx = 2;
    e.key(&mut io, 10);
    assert_eq!(e.text(), "ab\ncd\n");
    assert_eq!((e.cy, e.cx), (1, 0));
    // nothing to delete at the very start or end
    e.set_text("x\n");
    e.key(&mut io, K_BACKSPACE);
    e.goto_end();
    e.key(&mut io, K_DELETE);
    assert_eq!(e.text(), "x\n");
}

#[test]
fn the_arrows_keep_a_column_and_wrap_between_lines_and_tabs_are_four_wide() {
    let mut e = nano();
    let mut io = Mem::default();
    e.set_text("a long line here\nab\nanother long one\n");
    for _ in 0..10 {
        e.key(&mut io, K_RIGHT);
    }
    e.key(&mut io, K_DOWN);
    assert_eq!((e.cy, e.cx), (1, 2), "a short line: the end");
    e.key(&mut io, K_DOWN);
    assert_eq!((e.cy, e.cx), (2, 10), "the wanted column comes back");
    e.key(&mut io, K_UP);
    e.key(&mut io, K_UP);
    assert_eq!(e.cy, 0);
    e.key(&mut io, K_UP);
    assert_eq!(e.cy, 0, "the top");
    e.goto_start();
    e.key(&mut io, K_LEFT);
    assert_eq!((e.cy, e.cx), (0, 0));
    e.goto_end();
    assert_eq!(e.cy, 2);
    e.key(&mut io, K_RIGHT);
    assert_eq!((e.cy, e.cx), (2, 16), "no further");
    e.cy = 0;
    e.cx = 16;
    e.key(&mut io, K_RIGHT);
    assert_eq!((e.cy, e.cx), (1, 0), "wraps to the next line");
    e.key(&mut io, K_LEFT);
    assert_eq!((e.cy, e.cx), (0, 16));
    e.key(&mut io, K_HOME);
    assert_eq!(e.cx, 0);
    e.key(&mut io, K_END);
    assert_eq!(e.cx, 16);
    // tabs
    e.set_text("\tx\n");
    assert_eq!(e.display_col(0, 1), 4);
    assert_eq!(e.display_col(0, 2), 5);
    e.goto_start();
    e.key(&mut io, 9);
    assert_eq!(e.line(0).iter().collect::<String>(), "    \tx", "the Tab key inserts spaces up to the next stop");
    assert_eq!(e.row_cells(0).iter().collect::<String>(), "        x");
}

#[test]
fn pages_scroll_and_the_cursor_stays_on_screen() {
    let mut e = nano();
    let mut io = Mem::default();
    e.resize(5, 20);
    e.set_text(&(0..50).map(|i| format!("line {i}\n")).collect::<String>());
    e.key(&mut io, K_PGDN);
    assert_eq!(e.cy, 4);
    e.key(&mut io, K_PGDN);
    assert_eq!(e.cy, 8);
    assert!(e.cy >= e.top && e.cy < e.top + 5);
    e.key(&mut io, K_PGUP);
    assert_eq!(e.cy, 4);
    for _ in 0..60 {
        e.key(&mut io, K_DOWN);
    }
    assert_eq!(e.cy, 49);
    assert_eq!(e.cursor_cell().map(|c| c.1), Some(4), "the last line is at the bottom of the text area");
    // a long line scrolls sideways
    e.set_text(&"x".repeat(100));
    e.goto_end();
    assert!(e.left > 0 && e.cursor_cell().is_some());
    e.key(&mut io, K_HOME);
    assert_eq!(e.left, 0);
}

#[test]
fn ctrl_k_cuts_lines_that_ctrl_u_pastes_before_the_current_line() {
    let mut e = nano();
    let mut io = Mem::default();
    e.set_text("one\ntwo\nthree\nfour\n");
    e.cy = 1;
    e.key(&mut io, 0x0B);
    assert_eq!(e.text(), "one\nthree\nfour\n");
    e.key(&mut io, 0x0B); // a second cut in a row adds to the clipboard
    assert_eq!(e.text(), "one\nfour\n");
    assert_eq!(e.clip, "two\nthree\n");
    e.goto_start();
    e.key(&mut io, 0x15);
    assert_eq!(e.text(), "two\nthree\none\nfour\n");
    // moving between cuts starts a new clipboard
    e.goto_start();
    e.key(&mut io, 0x0B);
    e.key(&mut io, K_DOWN);
    e.key(&mut io, 0x0B);
    assert_eq!(e.clip, "one\n");
    // cutting the only line leaves an empty buffer
    e.set_text("x\n");
    e.key(&mut io, 0x0B);
    assert!(e.is_empty());
    e.key(&mut io, 0x15);
    assert_eq!(e.text(), "x\n");
}

#[test]
fn a_selection_is_cut_copied_pasted_and_replaced_by_typing() {
    let mut e = Editor::new(Mode::Gui);
    e.resize(10, 40);
    e.set_text("hello world\nsecond line\n");
    e.click(6, 0, false);
    e.click(5, 1, true);
    assert_eq!(e.selected_text().unwrap(), "world\nsecon");
    assert!(e.copy_selection());
    assert_eq!(e.clip, "world\nsecon");
    assert!(e.cut_selection());
    assert_eq!(e.text(), "hello d line\n");
    e.paste();
    assert_eq!(e.text(), "hello world\nsecond line\n");
    // typing over a selection replaces it
    e.select_all();
    assert_eq!(e.selected_text().unwrap(), "hello world\nsecond line");
    let mut io = Mem::default();
    e.key(&mut io, b'x');
    assert_eq!(e.text(), "x\n");
    // no selection: nothing to cut or copy
    assert!(!e.cut_selection() && !e.copy_selection());
    // a click without a drag selects nothing
    e.set_text("abc\n");
    e.click(1, 0, false);
    assert!(e.selection().is_none());
}

#[test]
fn search_finds_in_any_case_wraps_round_and_reports_what_it_did() {
    let mut e = nano();
    let mut io = Mem::default();
    e.set_text("alpha\nBeta gamma\nbeta again\n");
    assert!(e.search("BETA"));
    assert_eq!((e.cy, e.cx), (1, 0));
    assert!(e.search_again());
    assert_eq!((e.cy, e.cx), (2, 0));
    assert!(e.search_again());
    assert_eq!((e.cy, e.cx), (1, 0), "wrapped");
    assert_eq!(e.msg, "Search Wrapped");
    assert!(!e.search("zzz"));
    assert!(e.msg.contains("not found"));
    // the hit is highlighted until the cursor moves
    assert!(e.search("gam"));
    assert_eq!(e.highlight_cells(1), Some((5, 8)));
    e.key(&mut io, K_RIGHT);
    assert_eq!(e.highlight_cells(1), None);
    // Ctrl-W asks, Enter searches; an empty answer repeats the last search
    e.goto_start();
    e.key(&mut io, 0x17);
    assert!(e.prompt.is_some());
    type_str(&mut e, &mut io, "again");
    e.key(&mut io, 10);
    assert_eq!((e.cy, e.cx), (2, 5));
    e.goto_start();
    e.key(&mut io, 0x17);
    e.key(&mut io, 10);
    assert_eq!((e.cy, e.cx), (2, 5), "the same search again");
    // a match at the cursor itself is found after wrapping the whole way round
    e.set_text("only one\n");
    assert!(e.search("one"));
    assert!(e.search("one"));
}

#[test]
fn files_open_save_and_the_special_cases() {
    let mut io = Mem::default();
    io.files.insert("/data/a.txt".into(), b"hello\nthere\n".to_vec());
    io.files.insert("/data/bin".into(), vec![1, 0, 2]);
    io.files.insert("/data/big".into(), vec![b'x'; MAX_BYTES + 1]);
    let mut e = nano();
    assert!(e.open(&mut io, "a.txt"));
    assert_eq!(e.path.as_deref(), Some("/data/a.txt"));
    assert_eq!(e.text(), "hello\nthere\n");
    assert!(!e.modified);
    // a name that does not exist is a new file with that name
    assert!(e.open(&mut io, "new.txt"));
    assert!(e.is_empty() && e.msg == "New File" && e.path.as_deref() == Some("/data/new.txt"));
    // binary and huge files are refused and nothing changes
    e.open(&mut io, "a.txt");
    assert!(!e.open(&mut io, "bin"));
    assert!(e.msg.contains("not a text file") && e.path.as_deref() == Some("/data/a.txt"));
    assert!(!e.open(&mut io, "big"));
    assert!(e.msg.contains("too big"));
    // relative names resolve against the working directory, dots and all
    assert_eq!(e.resolve("../x/./y"), "/x/y");
    assert_eq!(e.resolve("/abs"), "/abs");
    // save writes the text and clears the flag
    type_str(&mut e, &mut io, "!");
    assert!(e.modified);
    assert!(e.save(&mut io));
    assert!(!e.modified);
    assert_eq!(io.files["/data/a.txt"], b"!hello\nthere\n");
    // errors are reported
    e.path = Some("/system/x".into());
    type_str(&mut e, &mut io, "?");
    assert!(!e.save(&mut io));
    assert!(e.msg.contains("Error writing") && e.msg.contains("read-only") && e.modified);
    // a buffer with no name asks for one
    e.new_buffer();
    assert!(!e.save(&mut io));
    assert!(matches!(&e.prompt, Some(p) if p.kind == PromptKind::WriteOut));
}

#[test]
fn nano_keys_write_out_and_exit_with_the_questions_nano_asks() {
    let mut io = Mem::default();
    let mut e = nano();
    // Ctrl-X on an untouched buffer leaves at once
    assert_eq!(e.key(&mut io, 0x18), Outcome::Exit);
    // type something; Ctrl-O asks for a name; Enter writes it
    type_str(&mut e, &mut io, "note");
    e.key(&mut io, 0x0F);
    assert!(matches!(&e.prompt, Some(p) if p.kind == PromptKind::WriteOut && p.text.is_empty()));
    type_str(&mut e, &mut io, "n.txt");
    e.key(&mut io, 10);
    assert_eq!(io.files["/data/n.txt"], b"note\n");
    assert!(!e.modified && e.prompt.is_none());
    assert!(e.msg.contains("Wrote 1 lines"));
    // Ctrl-O again offers the same name
    e.key(&mut io, 0x0F);
    assert_eq!(e.prompt.as_ref().unwrap().text, "/data/n.txt");
    e.key(&mut io, 27);
    assert!(e.prompt.is_none() && e.msg == "Cancelled");
    // modified + Ctrl-X: Y saves and exits, N throws away and exits, Esc stays
    type_str(&mut e, &mut io, "more");
    assert_eq!(e.key(&mut io, 0x18), Outcome::Continue);
    assert!(matches!(&e.prompt, Some(p) if p.kind == PromptKind::SaveFirst(Then::Exit)));
    assert_eq!(e.key(&mut io, 27), Outcome::Continue);
    assert!(e.prompt.is_none() && e.modified);
    e.key(&mut io, 0x18);
    assert_eq!(e.key(&mut io, b'y'), Outcome::Exit);
    assert_eq!(io.files["/data/n.txt"], b"notemore\n");
    let mut e2 = nano();
    type_str(&mut e2, &mut io, "junk");
    e2.key(&mut io, 0x18);
    assert_eq!(e2.key(&mut io, b'n'), Outcome::Exit);
    assert!(!io.files.contains_key("/data/junk"));
    // an unnamed buffer: Y asks for the name, then exits once it is written
    let mut e3 = nano();
    type_str(&mut e3, &mut io, "x");
    e3.key(&mut io, 0x18);
    assert_eq!(e3.key(&mut io, b'y'), Outcome::Continue);
    assert!(matches!(&e3.prompt, Some(p) if p.kind == PromptKind::WriteOut));
    type_str(&mut e3, &mut io, "u.txt");
    assert_eq!(e3.key(&mut io, 10), Outcome::Exit);
    assert_eq!(io.files["/data/u.txt"], b"x\n");
    // ... and if that name cannot be written, the editor stays
    let mut e4 = nano();
    type_str(&mut e4, &mut io, "x");
    e4.key(&mut io, 0x18);
    e4.key(&mut io, b'y');
    type_str(&mut e4, &mut io, "/system/no");
    assert_eq!(e4.key(&mut io, 10), Outcome::Continue);
    assert!(e4.msg.contains("read-only"));
}

#[test]
fn ctrl_r_inserts_a_file_ctrl_c_shows_the_position_and_ctrl_g_the_help() {
    let mut io = Mem::default();
    io.files.insert("/data/ins".into(), b"INSERTED\n".to_vec());
    let mut e = nano();
    e.set_text("ab\n");
    e.cx = 1;
    e.key(&mut io, 0x12);
    type_str(&mut e, &mut io, "ins");
    e.key(&mut io, 10);
    assert_eq!(e.text(), "aINSERTED\nb\n");
    e.key(&mut io, 0x03);
    assert!(e.msg.starts_with("line 2/2"), "{}", e.msg);
    e.key(&mut io, 0x07);
    assert!(e.help);
    e.key(&mut io, b'x');
    assert!(!e.help && e.text() == "aINSERTED\nb\n", "a key closes the help and does nothing else");
}

#[test]
fn the_window_keys_save_open_cut_copy_paste_and_quit() {
    let mut io = Mem::default();
    io.files.insert("/data/f.txt".into(), b"first\nsecond\n".to_vec());
    let mut e = Editor::new(Mode::Gui);
    e.cwd = "/data".into();
    e.resize(10, 40);
    // Ctrl-O asks for a name, then opens
    e.key(&mut io, 0x0F);
    assert!(matches!(&e.prompt, Some(p) if p.kind == PromptKind::Open));
    type_str(&mut e, &mut io, "f.txt");
    e.key(&mut io, 10);
    assert_eq!(e.text(), "first\nsecond\n");
    // select all, copy; Ctrl-X cuts; Ctrl-V pastes
    e.key(&mut io, 0x01);
    e.key(&mut io, 0x03);
    assert_eq!(e.clip, "first\nsecond");
    e.key(&mut io, 0x18);
    assert!(e.is_empty());
    e.key(&mut io, 0x16);
    assert_eq!(e.text(), "first\nsecond\n");
    // nothing selected: cut says so
    e.goto_start();
    e.key(&mut io, 0x18);
    assert!(e.msg.contains("Select"));
    // Ctrl-S saves; Ctrl-Q quits (and asks first when modified)
    type_str(&mut e, &mut io, "X");
    e.key(&mut io, 0x13);
    assert_eq!(io.files["/data/f.txt"], b"Xfirst\nsecond\n");
    assert_eq!(e.key(&mut io, 0x11), Outcome::Exit);
    type_str(&mut e, &mut io, "Y");
    assert_eq!(e.key(&mut io, 0x11), Outcome::Continue);
    assert_eq!(e.key(&mut io, b'n'), Outcome::Exit);
    // Ctrl-N with changes asks, then starts a new buffer
    let mut e = Editor::new(Mode::Gui);
    e.resize(5, 20);
    type_str(&mut e, &mut io, "abc");
    e.key(&mut io, 0x0E);
    assert!(matches!(&e.prompt, Some(p) if p.kind == PromptKind::SaveFirst(Then::New)));
    e.key(&mut io, b'n');
    assert!(e.is_empty() && !e.modified && e.path.is_none());
    // Ctrl-F searches, Ctrl-G repeats
    e.set_text("a b a b\n");
    e.key(&mut io, 0x06);
    type_str(&mut e, &mut io, "b");
    e.key(&mut io, 10);
    assert_eq!(e.cx, 2);
    e.key(&mut io, 0x07);
    assert_eq!(e.cx, 6);
}

#[test]
fn the_terminal_screen_looks_like_nano() {
    let mut e = nano();
    let mut io = Mem::default();
    let (rows, cols) = (24, 80);
    let (tr, tc) = term::text_area(rows, cols);
    assert_eq!((tr, tc), (20, 80));
    e.resize(tr, tc);
    e.set_text("first line\nsecond\n");
    e.path = Some("/data/x.txt".into());
    let text = |s: &term::Screen, r: usize| s.cells[r].iter().map(|c| c.ch).collect::<String>();
    let s = term::draw(&e, rows, cols);
    assert!(text(&s, 0).contains("Metal edit") && text(&s, 0).contains("File: /data/x.txt"));
    assert!(s.cells[0].iter().all(|c| c.inverse), "the header is reversed");
    assert!(!text(&s, 0).contains("Modified"));
    assert!(text(&s, 1).starts_with("first line") && text(&s, 2).starts_with("second"));
    assert!(text(&s, rows - 2).contains("^G") && text(&s, rows - 2).contains("Write Out") && text(&s, rows - 2).contains("^X") && text(&s, rows - 2).contains("Exit"));
    assert!(text(&s, rows - 1).contains("^U") && text(&s, rows - 1).contains("Paste"));
    assert_eq!(s.cursor, (0, 1));
    // typing marks it modified and moves the cursor
    type_str(&mut e, &mut io, "ab");
    let s = term::draw(&e, rows, cols);
    assert!(text(&s, 0).contains("Modified"));
    assert!(text(&s, 1).starts_with("abfirst line"));
    assert_eq!(s.cursor, (2, 1));
    // a message in the middle of the message line, in brackets
    e.msg = "Wrote 2 lines".into();
    let s = term::draw(&e, rows, cols);
    assert!(text(&s, rows - 3).contains("[ Wrote 2 lines ]"));
    // a prompt takes the message line and the shortcut bar, and the cursor is at the end of the text typed
    e.msg.clear();
    e.key(&mut io, 0x0F);
    type_str(&mut e, &mut io, "abc");
    let s = term::draw(&e, rows, cols);
    assert!(text(&s, rows - 3).starts_with("File Name to Write: /data/x.txtabc") || text(&s, rows - 3).starts_with("File Name to Write: "), "{}", text(&s, rows - 3));
    assert_eq!(s.cursor.1, rows - 3);
    assert!(text(&s, rows - 2).contains("^C") && text(&s, rows - 2).contains("Cancel"));
    // the help screen
    e.key(&mut io, 27);
    e.key(&mut io, 0x07);
    let s = term::draw(&e, rows, cols);
    assert!(text(&s, 0).contains("Help") && (0..rows).any(|r| text(&s, r).contains("Write Out")));
    // a selected search hit is reversed
    e.key(&mut io, b'x');
    e.set_text("find the needle here\n");
    e.search("needle");
    let s = term::draw(&e, rows, cols);
    let inv: String = s.cells[1].iter().filter(|c| c.inverse).map(|c| c.ch).collect();
    assert_eq!(inv, "needle");
    // a line that runs off the right edge is marked with a $
    e.set_text(&"y".repeat(200));
    e.resize(tr, tc);
    let s = term::draw(&e, rows, cols);
    assert_eq!(s.cells[1][cols - 1].ch, '$');
    // a screen too small to hold the editor draws nothing and does not panic
    let _ = term::draw(&e, 3, 10);
}







#[test]
fn nano_ctrl_v_pastes_like_ctrl_u_and_page_down_is_its_key() {
    let mut io = Mem::default();
    let mut e = nano();
    e.set_text("one\ntwo\nthree\n");
    e.cy = 1;
    e.key(&mut io, 0x0B); // cut "two"
    e.goto_start();
    e.key(&mut io, 0x16); // Ctrl-V
    assert_eq!(e.text(), "two\none\nthree\n");
    e.key(&mut io, 0x15); // Ctrl-U does the same
    assert_eq!(e.text(), "two\ntwo\none\nthree\n");
    // Page Down is the Page Down key (and Ctrl-V no longer pages)
    e.resize(2, 20);
    e.goto_start();
    let before = e.cy;
    e.key(&mut io, 0x16);
    assert_eq!(e.cy, before + 1, "Ctrl-V pasted a line before the cursor line, so the cursor moved down by that one line only");
    e.goto_start();
    e.key(&mut io, K_PGDN);
    assert_eq!(e.cy, 1);
    // the shortcut bar names it
    let s = term::draw(&e, 24, 80);
    let row: String = s.cells[23].iter().map(|c| c.ch).collect();
    assert!(row.contains("^U/^V") && row.contains("Paste"), "{row}");
}

#[test]
fn a_file_that_does_not_exist_opens_as_a_new_one_in_the_terminal_and_is_created_when_written() {
    let mut io = Mem::default();
    let mut e = nano();
    assert!(e.open(&mut io, "fresh.txt"));
    let s = term::draw(&e, 24, 80);
    let msg: String = s.cells[24 - 3].iter().map(|c| c.ch).collect();
    assert!(msg.contains("[ New File ]"), "{msg}");
    let head: String = s.cells[0].iter().map(|c| c.ch).collect();
    assert!(head.contains("File: /data/fresh.txt"));
    assert!(!io.files.contains_key("/data/fresh.txt"), "nothing is created until it is written");
    type_str(&mut e, &mut io, "hi");
    e.key(&mut io, 0x0F);
    assert_eq!(e.prompt.as_ref().unwrap().text, "/data/fresh.txt", "the name is already there");
    e.key(&mut io, 10);
    assert_eq!(io.files["/data/fresh.txt"], b"hi\n");
    // exiting an untouched new file leaves without asking and creates nothing
    let mut e = nano();
    e.open(&mut io, "never.txt");
    assert_eq!(e.key(&mut io, 0x18), Outcome::Exit);
    assert!(!io.files.contains_key("/data/never.txt"));
}
