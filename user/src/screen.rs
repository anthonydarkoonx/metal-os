//! The screen task's session: the terminal, centred on the screen, with the keyboard. There is no windowing system (the old GUI was removed on
//! 2026-11-16; a new one is designed in `docs/`). The command shell draws into a buffer this task owns; this file copies the changed rows to the
//! scanout buffer and asks the display driver to flush them.
use crate::gpu::{H, W};
use crate::proto::gpu::GPU_FLUSH2;
use crate::proto::input::KBD_POLL;
use crate::shell::{Host, Shell as Terminal};
use crate::*;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

const FB_VA: usize = 0xBC00_0000;
/// The backdrop behind the terminal.
const BACKDROP: u32 = 0x101820;

static mut KEYS: Vec<u8> = Vec::new();

fn take_keys() -> Vec<u8> {
    poll_keys();
    // SAFETY: the screen task is single-threaded; the shell and this file never run at the same time.
    unsafe { core::mem::take(&mut *core::ptr::addr_of_mut!(KEYS)) }
}

fn poll_keys() {
    let (st, r) = call(4, [KBD_POLL, 0, 0]);
    if st == 0 {
        for i in 0..r[0].min(16) {
            let k = (r[1 + i as usize / 8] >> (8 * (i % 8))) as u8;
            if k != crate::proto::input::K_ATTENTION {
                // SAFETY: as above.
                unsafe { (*core::ptr::addr_of_mut!(KEYS)).push(k) };
            }
        }
    }
}

/// Copy the terminal's changed rows to the screen and flush them.
fn present(fb: *mut u32, term: &[u32], ox: usize, oy: usize) -> bool {
    let Some((y0, y1)) = crate::console::take_dirty() else { return false };
    let (tw, th) = (crate::console::WIDTH, crate::console::HEIGHT);
    let y1 = y1.min(th);
    for y in y0..y1 {
        // SAFETY: the row lies inside the `W` x `H` scanout buffer (the terminal is centred and smaller than the screen).
        unsafe { core::ptr::copy_nonoverlapping(term.as_ptr().add(y * tw), fb.add((oy + y) * W + ox), tw) };
    }
    let r = (ox as u64) | ((oy + y0) as u64) << 16 | (tw as u64) << 32 | ((y1 - y0) as u64) << 48;
    let _ = call(1, [GPU_FLUSH2, r, 0]);
    true
}

/// The program manager's control protocol (docs/PROGRAMS.md): INSTALL and START with the id in two words.
const OP_INSTALL: u64 = 1;
const OP_START: u64 = 3;
/// The programs of the desktop, installed from the system image at every start (the first time they are not there yet).
const DESKTOP_PROGRAMS: [&str; 5] = ["desktop", "settings", "files", "taskmgr", "capmgr"];

fn pack_id(s: &str) -> (u64, u64) {
    let mut b = [0u8; 16];
    let n = s.len().min(16);
    b[..n].copy_from_slice(&s.as_bytes()[..n]);
    (u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap()))
}

// ---- the screen task as the compositor (../../docs/DESKTOP.md §14 P7) ----
//
// The shell keeps drawing into its own buffer as before; the "Terminal" (the desktop's SHOW_CONSOLE) is a window that shows that
// buffer and gives it the keys. The window is a client of the compositor inside this task (`metal_compositor_os::Local`: no IPC).
// While a command runs, the shell calls its `idle`, which turns the compositor once, so the desktop stays alive meanwhile.

struct Desk {
    server: metal_compositor_os::Server,
    /// The console window's connection and window, while it is shown.
    console: Option<(metal_desktop_proto::client::Connection, metal_ui::app::AppWindow)>,
    /// The shell's pixel buffer (`console::WIDTH` x `console::HEIGHT`).
    term: *mut u32,
    /// A turn is running (the shell's `idle` must not start another inside it).
    turning: bool,
}

static mut DESK: Option<Desk> = None;

fn desk() -> &'static mut Desk {
    // SAFETY: the screen task is single-threaded, and every user of `DESK` (the loop, `turn`, the shell's `idle` and key hooks)
    // finishes with it before the next one starts: `turn` is not re-entered (`turning`), and the shell runs outside of `turn`.
    unsafe { (*core::ptr::addr_of_mut!(DESK)).as_mut().expect("the desktop is set up") }
}

/// The console window's view: the shell's buffer, and the keys typed in it (as the keyboard driver's console codes).
struct ConsoleView {
    id: metal_ui::Id,
    rect: metal_ui::Rect,
    term: *mut u32,
}

impl metal_ui::Widget for ConsoleView {
    metal_ui::widget_basics!();
    fn measure(&self, _th: &metal_ui::Theme) -> metal_ui::Size {
        metal_ui::Size::new(crate::console::WIDTH as i32, crate::console::HEIGHT as i32)
    }
    fn layout(&mut self, r: metal_ui::Rect, _th: &metal_ui::Theme) {
        self.rect = r;
    }
    fn draw(&self, p: &mut metal_ui::Painter) {
        let (w, h) = (crate::console::WIDTH, crate::console::HEIGHT);
        // SAFETY: `term` is the shell's buffer of `w * h` words, leaked (alive for ever); nothing writes it while this draws (one thread).
        let px = unsafe { core::slice::from_raw_parts(self.term, w * h) };
        p.fill(self.rect, crate::console::themed(crate::console::BG));
        p.image(px, w as i32, h as i32, metal_ui::Rect::new(self.rect.x, self.rect.y, w as i32, h as i32));
    }
    fn event(&mut self, ev: &metal_ui::Event, _cx: &mut metal_ui::Ctx) -> bool {
        use crate::proto::input::*;
        use metal_ui::key;
        let metal_ui::Event::KeyDown { code, ch, mods, .. } = *ev else { return false };
        let b = match code {
            key::ENTER | key::KP_ENTER => b'\n',
            key::BACKSPACE => K_BACKSPACE,
            key::TAB => b'\t',
            key::UP => K_UP,
            key::DOWN => K_DOWN,
            key::LEFT => K_LEFT,
            key::RIGHT => K_RIGHT,
            key::HOME => K_HOME,
            key::END => K_END,
            key::PAGE_UP => K_PGUP,
            key::PAGE_DOWN => K_PGDN,
            key::DELETE => K_DELETE,
            key::ESC => 0x1B,
            _ => match ch {
                Some(c) if c.is_ascii_alphabetic() && mods.ctrl() => c.to_ascii_lowercase() as u8 & 0x1F,
                Some(c) if c.is_ascii() && !c.is_ascii_control() => c as u8,
                _ => return false,
            },
        };
        // SAFETY: as for `KEYS` above (one thread; the shell reads them later).
        unsafe { (*core::ptr::addr_of_mut!(KEYS)).push(b) };
        true
    }
    fn focusable(&self) -> bool {
        true
    }
}

fn console_keys() -> Vec<u8> {
    // (the keyboard driver's character queue is not read now: the compositor reads the keys as events, and asking for both
    // would double the round trips to the driver; a full character queue only drops characters nobody reads)
    // SAFETY: as above.
    unsafe { core::mem::take(&mut *core::ptr::addr_of_mut!(KEYS)) }
}

/// While a shell command runs: keep the desktop going.
fn console_idle() {
    let busy = turn();
    desk().server.pause(busy);
}

fn open_console(d: &mut Desk) {
    use metal_ui::app::{AppWindow, Options};
    let mut t = d.server.local();
    let Ok(mut conn) = metal_desktop_proto::client::Connection::hello(&mut t) else { return };
    let mut buf = [0u8; 256];
    let th = conn.theme(&mut t, &mut buf).ok().and_then(|n| metal_ui::Theme::from_bytes(&buf[..n])).unwrap_or(metal_ui::theme::LIGHT);
    crate::console::retheme(d.term, th.terminal_slots());
    let view = ConsoleView { id: metal_ui::Id::new(1), rect: metal_ui::Rect::default(), term: d.term };
    let mut ui = metal_ui::Ui::new(view, &th, crate::console::WIDTH as i32, crate::console::HEIGHT as i32);
    let mut p = metal_ui::MemPlatform::default();
    ui.set_focus(metal_ui::Id::new(1), &mut p);
    match AppWindow::open(&mut conn, &mut t, ui, "Terminal", Options { resizable: false, centered: true, skip_taskbar: false }) {
        Ok(w) => {
            let _ = conn.set_icon(&mut t, w.num, &metal_ui::icons::to_pixels(metal_ui::icons::Icon::Terminal, &th));
            d.console = Some((conn, w));
        }
        Err(e) => println!("[console] no window: {e}"),
    }
}

/// One turn of the compositor and of the console window.
fn turn() -> bool {
    let d = desk();
    if d.turning {
        return false;
    }
    d.turning = true;
    let mut busy = d.server.step();
    // the desktop asked for the terminal: shown (again, in front)
    if d.server.comp.console_requested {
        d.server.comp.console_requested = false;
        if let Some((mut conn, w)) = d.console.take() {
            let mut t = d.server.local();
            let _ = conn.destroy(&mut t, w.num);
            conn.bye(&mut t);
        }
        open_console(d);
        busy = true;
    }
    // the console window's events and drawing
    let changed = crate::console::take_dirty().is_some();
    let mut close = false;
    if let Some((conn, w)) = d.console.as_mut() {
        let mut t = d.server.local();
        let mut p = metal_ui::MemPlatform { clip: None, now: ticks() * 10 };
        while let Some(e) = conn.next_event(&mut t) {
            if let metal_desktop_proto::Event::ThemeChanged { .. } = e {
                let mut buf = [0u8; 256];
                if let Some(th) = conn.theme(&mut t, &mut buf).ok().and_then(|n| metal_ui::Theme::from_bytes(&buf[..n])) {
                    crate::console::retheme(d.term, th.terminal_slots());
                }
            }
            w.handle(conn, &mut t, &e, &mut p);
        }
        let _ = w.take_actions();
        if changed {
            w.invalidate();
        }
        close = w.close_requested;
        let _ = w.update(conn, &mut t);
    }
    if close {
        // (the shell goes on: the window shows it again when the terminal is asked for)
        if let Some((mut conn, w)) = d.console.take() {
            let mut t = d.server.local();
            let _ = conn.destroy(&mut t, w.num);
            conn.bye(&mut t);
        }
    }
    busy |= d.server.render();
    d.turning = false;
    busy
}

fn compositor(facts: crate::info::Facts, ctl: u64, display: u64, manager: u64, power: u64) -> ! {
    for id in DESKTOP_PROGRAMS {
        let (a, b) = pack_id(id);
        let (st, r) = call(ctl, [OP_INSTALL, a, b]);
        println!("[desktop] install {id}: status {st} {}", r[0]);
    }
    let cfg = metal_compositor_os::Config {
        display,
        gpu: 1,
        input: 4,
        framebuffer: 2,
        fb_handle: FB_VA,
        manager: (manager != 0).then_some(manager as u32),
        power: (power != 0).then_some(power),
    };
    let mut server = metal_compositor_os::Server::new(cfg, &metal_ui::theme::LIGHT);
    server.comp.console_available = true;
    // the shell, drawing into its own buffer as it always has
    let (tw, th) = (crate::console::WIDTH, crate::console::HEIGHT);
    let term_buf: &'static mut [u32] = Box::leak(vec![crate::console::themed(crate::console::BG); tw * th].into_boxed_slice());
    let term = term_buf.as_mut_ptr();
    // SAFETY: as in the console session (`run` below): the buffer is leaked and holds WIDTH * HEIGHT words.
    let mut shell = unsafe { Terminal::new(term, facts, Host { keys: console_keys, idle: console_idle }) };
    shell.progmgr = Some(ctl);
    // SAFETY: set once, before anything reads it (one thread).
    unsafe { *core::ptr::addr_of_mut!(DESK) = Some(Desk { server, console: None, term, turning: false }) };
    shell.start();
    // (the manager tells the compositor that this program has the desktop role with an asynchronous message, so it does not
    // wait for the compositor, which is this task, still here)
    let (a, b) = pack_id("desktop");
    let (st, r) = call(ctl, [OP_START, a, b]);
    println!("[desktop] start desktop: status {st} {}", r[0]);
    loop {
        let busy = turn();
        let keys = console_keys();
        let typed = !keys.is_empty();
        for k in keys {
            shell.typed(k);
        }
        desk().server.pause(busy || typed);
    }
}

pub fn run(facts: crate::info::Facts, ctl: u64, w2: u64) -> ! {
    // w2 (the console session's): the display endpoint's slot, the manager's task, the power capability's slot
    let (display, manager, power) = (w2 & 0xFF, (w2 >> 8) & 0xFF, (w2 >> 16) & 0xFF);
    if ctl != 0 && display != 0 {
        compositor(facts, ctl, display, manager, power);
    }
    if mem_map(2, FB_VA, true) != 0 {
        println!("[console] cannot map the framebuffer");
        exit(1)
    }
    let fb = va(FB_VA) as *mut u32;
    let (tw, th) = (crate::console::WIDTH, crate::console::HEIGHT);
    let term_buf: &'static mut [u32] = Box::leak(vec![crate::console::themed(crate::console::BG); tw * th].into_boxed_slice());
    let (ox, oy) = (W.saturating_sub(tw) / 2, H.saturating_sub(th) / 2);
    // SAFETY: the scanout buffer is `W * H` words.
    unsafe { core::slice::from_raw_parts_mut(fb, W * H) }.fill(BACKDROP);
    let _ = call(1, [GPU_FLUSH2, (W as u64) << 32 | (H as u64) << 48, 0]);
    // SAFETY: `term_buf` is leaked, so it lives for ever; it holds `tw * th` (WIDTH * HEIGHT) words and nothing else uses it.
    let mut shell = unsafe { Terminal::new(term_buf.as_mut_ptr(), facts, Host { keys: take_keys, idle: || sleep(1) }) };
    shell.progmgr = (ctl != 0).then_some(ctl);
    shell.start();
    println!("[console] up: terminal only, no windowing system");
    let mut last_active = 0u64;
    loop {
        let shown = present(fb, term_buf, ox, oy);
        let keys = take_keys();
        let typed = !keys.is_empty();
        for k in keys {
            shell.typed(k);
        }
        if shown || typed {
            last_active = ticks();
        }
        if !shown && !typed {
            if ticks().wrapping_sub(last_active) < 25 {
                sleep_us(500);
            } else {
                sleep(1);
            }
        }
    }
}
