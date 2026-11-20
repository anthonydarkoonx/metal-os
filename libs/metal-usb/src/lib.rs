//! The parts of USB input that are plain data processing, kept apart from the controller driver (`user/src/xhci.rs`) so that they can be tested on the host:
//! walking a configuration descriptor for HID interfaces, reading a HID report descriptor for a pointer's fields, turning keyboard and pointer reports into
//! changes, and the key table. Everything that comes from a device is untrusted: every length is checked, nothing indexes past what it was given.
#![no_std]

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;

// ---- configuration descriptors ----------------------------------------------------------------------------------------------------------

/// A HID interface with an interrupt IN endpoint: what the driver needs to start it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HidIface {
    pub config_value: u8,
    pub number: u8,
    /// 1 = boot interface (the report layout is fixed), 0 = none.
    pub subclass: u8,
    /// With subclass 1: 1 keyboard, 2 mouse.
    pub protocol: u8,
    /// Length of the report descriptor (from the HID descriptor).
    pub report_len: u16,
    /// Endpoint address (bit 7 set: IN), its maximum packet size and `bInterval`.
    pub ep_addr: u8,
    pub ep_max_packet: u16,
    pub ep_interval: u8,
}

const DT_CONFIG: u8 = 2;
const DT_INTERFACE: u8 = 4;
const DT_ENDPOINT: u8 = 5;
const DT_HID: u8 = 0x21;

/// The HID interfaces (class 3) of a configuration descriptor that have an interrupt IN endpoint, up to `out.len()` of them. Returns how many.
pub fn hid_interfaces(config: &[u8], out: &mut [HidIface]) -> usize {
    let mut n = 0;
    if config.len() < 9 || config[1] != DT_CONFIG {
        return 0;
    }
    let config_value = config[5];
    let total = (u16::from_le_bytes([config[2], config[3]]) as usize).min(config.len());
    let mut at = config[0] as usize;
    let mut cur: Option<HidIface> = None;
    let mut done = |c: &mut Option<HidIface>, n: &mut usize| {
        if let Some(i) = c.take() {
            if i.ep_addr != 0 && *n < out.len() {
                out[*n] = i;
                *n += 1;
            }
        }
    };
    while at + 2 <= total {
        let (len, ty) = (config[at] as usize, config[at + 1]);
        if len < 2 || at + len > total {
            break;
        }
        let d = &config[at..at + len];
        match ty {
            DT_INTERFACE if len >= 9 => {
                done(&mut cur, &mut n);
                if d[5] == 3 && d[3] == 0 {
                    // (alternate setting 0 only)
                    cur = Some(HidIface { config_value, number: d[2], subclass: d[6], protocol: d[7], ..HidIface::default() });
                }
            }
            DT_HID if len >= 9 => {
                // bNumDescriptors at 5, then (type, length) pairs: the report descriptor is type 0x22
                if let Some(i) = cur.as_mut() {
                    let mut p = 6;
                    for _ in 0..d[5] {
                        if p + 3 > len {
                            break;
                        }
                        if d[p] == 0x22 {
                            i.report_len = u16::from_le_bytes([d[p + 1], d[p + 2]]);
                        }
                        p += 3;
                    }
                }
            }
            DT_ENDPOINT if len >= 7 => {
                if let Some(i) = cur.as_mut() {
                    // interrupt (bmAttributes 3) IN (bit 7), the first one only
                    if d[3] & 3 == 3 && d[2] & 0x80 != 0 && i.ep_addr == 0 {
                        (i.ep_addr, i.ep_max_packet, i.ep_interval) = (d[2], u16::from_le_bytes([d[4], d[5]]) & 0x7FF, d[6]);
                    }
                }
            }
            _ => {}
        }
        at += len;
    }
    done(&mut cur, &mut n);
    n
}

/// A hub interface with its status-change endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HubIface {
    pub config_value: u8,
    pub number: u8,
    /// bInterfaceProtocol: 2 = a hub with one transaction translator per port.
    pub protocol: u8,
    pub ep_addr: u8,
    pub ep_max_packet: u16,
    pub ep_interval: u8,
}

/// The first hub interface (class 9) with an interrupt IN endpoint in a configuration descriptor.
pub fn hub_interface(config: &[u8]) -> Option<HubIface> {
    if config.len() < 9 || config[1] != DT_CONFIG {
        return None;
    }
    let total = (u16::from_le_bytes([config[2], config[3]]) as usize).min(config.len());
    let mut at = config[0] as usize;
    let mut cur: Option<HubIface> = None;
    while at + 2 <= total {
        let (len, ty) = (config[at] as usize, config[at + 1]);
        if len < 2 || at + len > total {
            break;
        }
        let d = &config[at..at + len];
        match ty {
            DT_INTERFACE if len >= 9 => {
                if cur.is_some_and(|c| c.ep_addr != 0) {
                    return cur;
                }
                cur = (d[5] == 9 && d[3] == 0).then_some(HubIface { config_value: config[5], number: d[2], protocol: d[7], ..HubIface::default() });
            }
            DT_ENDPOINT if len >= 7 => {
                if let Some(h) = cur.as_mut() {
                    if d[3] & 3 == 3 && d[2] & 0x80 != 0 && h.ep_addr == 0 {
                        (h.ep_addr, h.ep_max_packet, h.ep_interval) = (d[2], u16::from_le_bytes([d[4], d[5]]) & 0x7FF, d[6]);
                    }
                }
            }
            _ => {}
        }
        at += len;
    }
    cur.filter(|c| c.ep_addr != 0)
}

/// What a hub descriptor says: the number of downstream ports, how long power takes to settle (milliseconds), and the transaction translator think time (0..=3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubInfo {
    pub ports: u8,
    pub power_good_ms: u16,
    pub think_time: u8,
}

/// Read a hub descriptor (USB 2 type 0x29). A hub with no ports, or more than 15 (the route string has four bits per tier), is refused.
pub fn hub_descriptor(d: &[u8]) -> Option<HubInfo> {
    if d.len() < 7 || d[1] != 0x29 {
        return None;
    }
    let ports = d[2];
    (1..=15).contains(&ports).then(|| HubInfo { ports, power_good_ms: (d[5] as u16 * 2).min(500), think_time: (d[3] >> 5) & 3 })
}

/// What a hub port's status words say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortStatus {
    pub connected: bool,
    pub enabled: bool,
    pub reset: bool,
    pub connect_changed: bool,
    pub reset_changed: bool,
    /// The speed of what is connected, once the port was reset: the xHCI speed id (1 full, 2 low, 3 high).
    pub speed: u8,
}

/// `GET_STATUS` for a hub port answers 4 bytes: `wPortStatus`, `wPortChange`.
pub fn port_status(d: &[u8]) -> Option<PortStatus> {
    if d.len() < 4 {
        return None;
    }
    let (st, ch) = (u16::from_le_bytes([d[0], d[1]]), u16::from_le_bytes([d[2], d[3]]));
    Some(PortStatus {
        connected: st & 1 != 0,
        enabled: st & 2 != 0,
        reset: st & 1 << 4 != 0,
        connect_changed: ch & 1 != 0,
        reset_changed: ch & 1 << 4 != 0,
        speed: if st & 1 << 10 != 0 { 3 } else if st & 1 << 9 != 0 { 2 } else { 1 },
    })
}

/// The xHCI endpoint context `Interval` for an interrupt endpoint: an exponent of 125 microsecond frames. Full and low speed devices give `bInterval` in
/// milliseconds (1..=255), high and super speed give an exponent plus one (1..=16).
pub fn xhci_interval(speed: u8, b_interval: u8) -> u8 {
    match speed {
        // 1 full, 2 low: round the milliseconds down to a power of two of 125 us frames
        1 | 2 => {
            let frames = (b_interval.max(1) as u32) * 8;
            (31 - frames.leading_zeros()).clamp(3, 10) as u8
        }
        _ => b_interval.clamp(1, 16) - 1,
    }
}

// ---- keyboards ---------------------------------------------------------------------------------------------------------------------------

/// Linux input key code for a USB HID keyboard usage (page 7), 0 if it has none here.
pub fn usage_to_linux(u: u8) -> u16 {
    const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    match u {
        0x04..=0x1d => LETTERS[(u - 4) as usize],
        0x1e..=0x27 => (u - 0x1e) as u16 + 2, // 1..9, 0
        0x28 => 28,                           // enter
        0x29 => 1,                            // escape
        0x2a => 14,                           // backspace
        0x2b => 15,                           // tab
        0x2c => 57,                           // space
        0x2d => 12,
        0x2e => 13,
        0x2f => 26,
        0x30 => 27,
        0x31 => 43,
        0x33 => 39,
        0x34 => 40,
        0x35 => 41,
        0x36 => 51,
        0x37 => 52,
        0x38 => 53,
        0x39 => 58,                           // caps lock
        0x3a..=0x43 => (u - 0x3a) as u16 + 59, // F1..F10
        0x44 => 87,
        0x45 => 88,
        0x49 => 110, // insert
        0x4a => 102,
        0x4b => 104,
        0x4c => 111,
        0x4d => 107,
        0x4e => 109,
        0x4f => 106,
        0x50 => 105,
        0x51 => 108,
        0x52 => 103,
        _ => 0,
    }
}

/// Linux key codes of the modifier bits of a boot keyboard report's first byte (left ctrl, shift, alt, gui, then right).
const MODIFIERS: [u16; 8] = [29, 42, 56, 125, 97, 54, 100, 126];

/// What a boot keyboard last reported.
#[derive(Clone, Copy, Default)]
pub struct Keyboard {
    mods: u8,
    keys: [u8; 6],
}

impl Keyboard {
    pub const fn new() -> Keyboard {
        Keyboard { mods: 0, keys: [0; 6] }
    }

    /// A boot-protocol report (8 bytes: modifiers, reserved, six key usages): `f(code, value)` for every key that went down (1) or up (0) since the last one.
    /// A report that says "too many keys" (usage 1 in every slot) changes nothing. Shorter reports are ignored.
    pub fn report(&mut self, r: &[u8], mut f: impl FnMut(u16, u32)) {
        if r.len() < 8 {
            return;
        }
        let keys: [u8; 6] = [r[2], r[3], r[4], r[5], r[6], r[7]];
        if keys.iter().all(|&k| k == 1) {
            return; // rollover error
        }
        for (bit, &code) in MODIFIERS.iter().enumerate() {
            let (was, is) = (self.mods >> bit & 1, r[0] >> bit & 1);
            if was != is {
                f(code, is as u32);
            }
        }
        for &k in self.keys.iter().filter(|&&k| k > 3 && !keys.contains(&k)) {
            if usage_to_linux(k) != 0 {
                f(usage_to_linux(k), 0);
            }
        }
        for &k in keys.iter().filter(|&&k| k > 3 && !self.keys.contains(&k)) {
            if usage_to_linux(k) != 0 {
                f(usage_to_linux(k), 1);
            }
        }
        self.mods = r[0];
        self.keys = keys;
    }
}

// ---- pointers: HID report descriptors ----------------------------------------------------------------------------------------------------

/// One value in a report: where it is (in bits from the start of the report data), how wide, its range, and whether it counts movement (relative) or
/// says where something is (absolute).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub offset: u16,
    pub size: u8,
    pub signed: bool,
    pub min: i32,
    pub max: i32,
    pub relative: bool,
}

/// Where a pointing device's buttons, axes and wheel are in its reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    /// Report id the fields are in (0: the device uses none: reports are just the data).
    pub report_id: u8,
    /// Button bits: (offset in bits, how many, at most 8 are used).
    pub buttons: Option<(u16, u8)>,
    pub x: Option<Field>,
    pub y: Option<Field>,
    pub wheel: Option<Field>,
}

/// What one pointer report says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerReport {
    pub buttons: u8,
    /// Raw axis values (a position for an absolute axis, a step for a relative one), where the report has the axis.
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub wheel: i32,
}

#[derive(Clone, Copy, Default)]
struct Globals {
    page: u32,
    min: i32,
    max: i32,
    size: u32,
    count: u32,
    id: u8,
}

/// Read the first pointer-like application collection of a HID report descriptor: Generic Desktop X, Y and wheel, and the button page. `None` if there are no
/// X and Y axes (a keyboard, say).
pub fn parse_pointer(desc: &[u8]) -> Option<Pointer> {
    let mut g = Globals::default();
    let mut stack: [Globals; 4] = [Globals::default(); 4];
    let mut depth = 0;
    let mut usages = [0u32; 16];
    let mut nusage = 0;
    let (mut umin, mut umax) = (0u32, 0u32);
    let mut bits: u16 = 0; // running offset in bits (of the current report id)
    let mut p = Pointer::default();
    let mut seen_id = false;
    let mut locked = false;
    let mut at = 0;
    while at < desc.len() {
        let b = desc[at];
        let (size, ty, tag) = match b {
            0xFE => {
                // long item: skip it
                let l = *desc.get(at + 1)? as usize;
                at += 3 + l;
                continue;
            }
            _ => (match b & 3 { 3 => 4, n => n as usize }, (b >> 2) & 3, b >> 4),
        };
        let data = desc.get(at + 1..at + 1 + size)?;
        let u = data.iter().enumerate().fold(0u32, |v, (i, &x)| v | (x as u32) << (8 * i));
        let s = match size {
            1 => u as u8 as i8 as i32,
            2 => u as u16 as i16 as i32,
            _ => u as i32,
        };
        match (ty, tag) {
            (1, 0) => g.page = u,
            (1, 1) => g.min = s,
            (1, 2) => g.max = if size == 4 || s >= 0 { s } else { u as i32 }, // (a one-byte 0xFF maximum is 255, not -1, when the minimum is 0)
            (1, 7) => g.size = u,
            (1, 8) => {
                if !seen_id || u as u8 != g.id {
                    bits = 0; // each report id has its own bits
                }
                g.id = u as u8;
                seen_id = true;
            }
            (1, 9) => g.count = u,
            (1, 10) => {
                if depth < stack.len() {
                    stack[depth] = g;
                    depth += 1;
                }
            }
            (1, 11) => {
                if depth > 0 {
                    depth -= 1;
                    g = stack[depth];
                }
            }
            (2, 0) => {
                if nusage < usages.len() {
                    usages[nusage] = u;
                    nusage += 1;
                }
            }
            (2, 1) => umin = u,
            (2, 2) => umax = u,
            (0, 8) => {
                // Input: walk the fields it defines (only those of the first report id that has a pointer's fields count)
                let constant = u & 1 != 0;
                let relative = u & 4 != 0;
                let id = if seen_id { g.id } else { 0 };
                let mine = !locked || id == p.report_id;
                let usage_of = |i: u32| -> u32 {
                    if nusage > 0 {
                        usages[(i as usize).min(nusage - 1)]
                    } else if umax >= umin {
                        (umin + i).min(umax)
                    } else {
                        0
                    }
                };
                if !constant && mine {
                    for i in 0..g.count.min(64) {
                        let off = bits.saturating_add(i.saturating_mul(g.size).min(0xFFFF) as u16);
                        let field = Field { offset: off, size: g.size.min(32) as u8, signed: g.min < 0, min: g.min, max: g.max, relative };
                        let before = (p.buttons, p.x, p.y, p.wheel);
                        match (g.page, usage_of(i) & 0xFFFF) {
                            (9, 1) if p.buttons.is_none() => p.buttons = Some((off, g.count.min(8) as u8)),
                            (1, 0x30) if p.x.is_none() => p.x = Some(field),
                            (1, 0x31) if p.y.is_none() => p.y = Some(field),
                            (1, 0x38) if p.wheel.is_none() => p.wheel = Some(field),
                            _ => {}
                        }
                        if before != (p.buttons, p.x, p.y, p.wheel) && !locked {
                            (locked, p.report_id) = (true, id);
                        }
                    }
                }
                bits = bits.saturating_add(g.count.saturating_mul(g.size).min(0xFFFF) as u16);
                nusage = 0;
                (umin, umax) = (0, 0);
            }
            // (every main item clears the local items: Output, Feature, Collection, End Collection)
            (0, 9) | (0, 10) | (0, 11) | (0, 12) => {
                nusage = 0;
                (umin, umax) = (0, 0);
            }
            _ => {}
        }
        at += 1 + size;
    }
    (p.x.is_some() && p.y.is_some()).then_some(p)
}

/// `size` bits at bit `offset` of `data` (little endian bit order), sign extended if `signed`; `None` if they do not lie inside it.
fn bits_at(data: &[u8], offset: u16, size: u8, signed: bool) -> Option<i32> {
    if size == 0 || size > 32 {
        return None;
    }
    let (off, size) = (offset as usize, size as usize);
    if off + size > data.len() * 8 {
        return None;
    }
    let mut v = 0u64;
    for i in 0..size {
        let bit = off + i;
        v |= ((data[bit / 8] >> (bit % 8) & 1) as u64) << i;
    }
    if signed && v >> (size - 1) & 1 != 0 {
        v |= !0u64 << size;
    }
    Some(v as i64 as i32)
}

impl Pointer {
    /// Decode one report: with a report id (`report_id != 0`) its first byte must match, and the fields lie after it.
    pub fn report(&self, r: &[u8]) -> Option<PointerReport> {
        let data = if self.report_id != 0 {
            if *r.first()? != self.report_id {
                return None;
            }
            &r[1..]
        } else {
            r
        };
        // (a field the descriptor defines but the report is too short to hold makes the whole report invalid)
        let field = |f: &Option<Field>| -> Option<Option<i32>> {
            match f {
                Some(f) => bits_at(data, f.offset, f.size, f.signed).map(Some),
                None => Some(None),
            }
        };
        let buttons = match self.buttons {
            Some((off, n)) => bits_at(data, off, n, false)? as u8,
            None => 0,
        };
        Some(PointerReport { buttons, x: field(&self.x)?, y: field(&self.y)?, wheel: field(&self.wheel)?.unwrap_or(0) })
    }

    /// An absolute axis value mapped onto 0 ..= 32767.
    pub fn scale_abs(f: &Field, v: i32) -> i32 {
        let span = (f.max as i64 - f.min as i64).max(1);
        (((v as i64 - f.min as i64) * 32767) / span).clamp(0, 32767) as i32
    }
}

/// A boot-protocol mouse report (buttons, dx, dy, optional wheel) as the fixed layout it is.
pub fn boot_mouse() -> Pointer {
    let axis = |offset| Some(Field { offset, size: 8, signed: true, min: -127, max: 127, relative: true });
    Pointer { report_id: 0, buttons: Some((0, 3)), x: axis(8), y: axis(16), wheel: axis(24) }
}
