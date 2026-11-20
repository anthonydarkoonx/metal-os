//! The protocols between drivers and their clients: request numbers, reply layouts and the constants a client needs to talk to a driver. A driver and its
//! clients (the console, the shell, `netd`) both depend on this, so neither has to include the other. (Test-only requests are marked as such.)

/// The block driver (role 4).
pub mod blk {
    pub const BLK_READ: u64 = 20;
    pub const BLK_WRITE: u64 = 21;
    pub const BLK_CRASH: u64 = 22;
    /// Ask the block driver for the disk size in sectors.
    pub const BLK_INFO: u64 = 24;
    /// Test only: submit a request without notifying the device, so it never answers.
    pub const BLK_STALL: u64 = 25;
    /// The block driver gives a command this long before declaring the device dead (5 s).
    pub const BLK_WATCHDOG_TICKS: u64 = 500;
    /// Test only: a driver that points its device's DMA at memory it was never granted.
    pub const BLK_EVIL: u64 = 23;
}

/// The display driver (role 12).
pub mod gpu {

    /// Present a rectangle: w1 = x | y << 16 | w << 32 | h << 48.
    pub const GPU_FLUSH: u64 = 30;
    /// Test only: a driver whose device is made to read memory outside its grant.
    pub const GPU_EVIL: u64 = 31;
    /// Ask for (scanout width | height << 32, whether the host reports the display enabled).
    pub const GPU_INFO: u64 = 32;
    /// Test only: send a command without notifying the device, so it never answers.
    pub const GPU_STALL: u64 = 33;
    /// Present up to two rectangles in one call: w1 and w2 are each `x | y << 16 | w << 32 | h << 48`; a zero rectangle is skipped.
    /// Both are copied to the host, then one flush covers the pair. Replies 0, or 1 if a rectangle is outside the screen.
    pub const GPU_FLUSH2: u64 = 34;
}

/// The input driver (role 14): keyboard and pointer.
pub mod input {
    /// Ask for pending characters: reply = [count, bytes 0..8 packed little-endian, bytes 8..16].
    pub const KBD_POLL: u64 = 70;
    /// Ask for pending input events (keys as the device numbers them, pointer position, buttons, wheel):
    /// reply = [count | 1 << 8 if the machine has a pointer, event, event], at most two events of eight bytes (see `Event`).
    pub const INPUT_POLL: u64 = 71;
    /// Like `INPUT_POLL`, but if there is no event yet the call waits for one: `w[1]` = the longest wait in milliseconds (at most 1000; 0 = do not wait).
    /// The reply is the same; no events means the time ran out. The driver does nothing else while it waits (so one client should use it, and a
    /// `KBD_POLL` from another waits its turn).
    pub const INPUT_WAIT: u64 = 72;

    /// An input event, packed in a word: `kind` (low byte), `code` (next 16 bits), `value` (high 32 bits).
    pub mod ev {
        /// `code` = key number (the device's numbering: the PC/AT set-1 numbers of the main block, 0x110.. for mouse buttons),
        /// `value` = 1 pressed, 0 released, 2 auto-repeat.
        pub const KEY: u64 = 1;
        /// The pointer's absolute position, `value` = x | y << 16, each 0..=32767 of the device's range.
        pub const MOVE: u64 = 2;
        /// A pointer button, `code` = 1 left, 2 right, 3 middle, 4.. others; `value` = 1 pressed, 0 released.
        pub const BUTTON: u64 = 3;
        /// The wheel, `value` = signed clicks (positive = away from the user).
        pub const WHEEL: u64 = 4;
        pub fn pack(kind: u64, code: u16, value: i32) -> u64 {
            kind | (code as u64) << 8 | (value as u32 as u64) << 32
        }
    }

    // special characters the driver reports
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
    /// Ctrl+Alt+Delete or Ctrl+Shift+Escape: the "attention" key; the console shows the task manager instead of passing it to a window.
    pub const K_ATTENTION: u8 = 0x90;
}

/// The network driver (role 18); the rings it shares with `netd` are in `netring`.
pub mod net {
    pub const NET_EVENT: u64 = 0x100;
}
