use super::*;

/// QEMU's `usb-tablet` HID report descriptor (hw/usb/dev-hid.c).
const TABLET: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75,
    0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x05, 0x01, 0x09, 0x38,
    0x15, 0x81, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0, 0xc0,
];

/// The classic boot mouse descriptor (three buttons, X, Y relative).
const MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75,
    0x05, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0xc0, 0xc0,
];

/// A keyboard's descriptor has no X and Y.
const KEYBOARD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95,
    0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
];

#[test]
fn tablet_fields() {
    let p = parse_pointer(TABLET).unwrap();
    assert_eq!(p.report_id, 0);
    assert_eq!(p.buttons, Some((0, 3)));
    let x = p.x.unwrap();
    assert_eq!((x.offset, x.size, x.min, x.max, x.relative, x.signed), (8, 16, 0, 0x7fff, false, false));
    assert_eq!(p.y.unwrap().offset, 24);
    let w = p.wheel.unwrap();
    assert_eq!((w.offset, w.size, w.relative, w.signed), (40, 8, true, true));
}

#[test]
fn tablet_report() {
    let p = parse_pointer(TABLET).unwrap();
    // buttons 0b101, x 0x4000, y 0x1000, wheel -1
    let r = p.report(&[0b101, 0x00, 0x40, 0x00, 0x10, 0xff]).unwrap();
    assert_eq!(r, PointerReport { buttons: 5, x: Some(0x4000), y: Some(0x1000), wheel: -1 });
    assert_eq!(Pointer::scale_abs(&p.x.unwrap(), 0x4000), 16384);
    assert_eq!(Pointer::scale_abs(&p.x.unwrap(), 0x7fff), 32767);
    assert!(p.report(&[0, 0, 0]).is_none()); // too short for the fields
}

#[test]
fn mouse_fields_and_boot_layout_agree() {
    let p = parse_pointer(MOUSE).unwrap();
    assert_eq!(p.buttons, Some((0, 3)));
    assert!(p.x.unwrap().relative && p.y.unwrap().relative);
    assert_eq!((p.x.unwrap().offset, p.y.unwrap().offset), (8, 16));
    let r = p.report(&[1, 5, 0xfb]).unwrap();
    assert_eq!((r.buttons, r.x, r.y), (1, Some(5), Some(-5)));
    let b = boot_mouse().report(&[2, 3, 4, 0xff]).unwrap();
    assert_eq!((b.buttons, b.x, b.y, b.wheel), (2, Some(3), Some(4), -1));
}

#[test]
fn keyboard_descriptor_is_not_a_pointer() {
    assert!(parse_pointer(KEYBOARD).is_none());
}

#[test]
fn report_ids() {
    // report id 1: a keyboard-ish byte, report id 2: the pointer (2 buttons, 8-bit X and Y)
    let d = [
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x02, 0x05, 0x09, 0x19, 0x01, 0x29, 0x02, 0x15, 0x00, 0x25, 0x01, 0x95, 0x02, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x06, 0x81, 0x03,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0xc0,
    ];
    let p = parse_pointer(&d).unwrap();
    assert_eq!(p.report_id, 2);
    assert!(p.report(&[1, 0, 0, 0]).is_none()); // another report id
    let r = p.report(&[2, 3, 1, 0xff]).unwrap();
    assert_eq!((r.buttons, r.x, r.y), (3, Some(1), Some(-1)));
}

#[test]
fn garbage_descriptors_do_not_panic() {
    let mut s = 12345u32;
    let mut rnd = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    for _ in 0..20000 {
        let n = rnd() as usize % 80;
        let d: std::vec::Vec<u8> = (0..n).map(|_| rnd() as u8).collect();
        if let Some(p) = parse_pointer(&d) {
            let r: std::vec::Vec<u8> = (0..(rnd() % 12)).map(|_| rnd() as u8).collect();
            let _ = p.report(&r);
        }
    }
    for _ in 0..5000 {
        let mut d = TABLET.to_vec();
        for _ in 0..3 {
            let i = rnd() as usize % d.len();
            d[i] = rnd() as u8;
        }
        let _ = parse_pointer(&d);
    }
}

fn collect(k: &mut Keyboard, r: &[u8]) -> std::vec::Vec<(u16, u32)> {
    let mut v = std::vec::Vec::new();
    k.report(r, |c, x| v.push((c, x)));
    v
}

#[test]
fn keyboard_press_release_and_modifiers() {
    let mut k = Keyboard::new();
    assert_eq!(collect(&mut k, &[0, 0, 4, 0, 0, 0, 0, 0]), [(30, 1)]); // 'a'
    assert_eq!(collect(&mut k, &[0, 0, 4, 5, 0, 0, 0, 0]), [(48, 1)]); // 'b' joins
    assert_eq!(collect(&mut k, &[2, 0, 4, 5, 0, 0, 0, 0]), [(42, 1)]); // left shift
    assert_eq!(collect(&mut k, &[0, 0, 5, 0, 0, 0, 0, 0]), [(42, 0), (30, 0)]);
    assert_eq!(collect(&mut k, &[0; 8]), [(48, 0)]);
    assert!(collect(&mut k, &[0, 0, 1, 1, 1, 1, 1, 1]).is_empty()); // rollover error
    assert!(collect(&mut k, &[0, 0, 4]).is_empty()); // short
    assert_eq!(collect(&mut k, &[0, 0, 0x28, 0x29, 0, 0, 0, 0]), [(28, 1), (1, 1)]);
}

#[test]
fn every_letter_and_digit_has_a_code() {
    assert!((4u8..=0x27).all(|u| usage_to_linux(u) != 0));
    assert_eq!(usage_to_linux(0x1e), 2);
    assert_eq!(usage_to_linux(0x27), 11);
    assert_eq!(usage_to_linux(0xff), 0);
}

#[test]
fn config_descriptor_walk() {
    // configuration (9) + interface 0 HID boot keyboard + HID descriptor + endpoint 0x81 + interface 1 HID mouse + HID + endpoint 0x82
    let mut c: std::vec::Vec<u8> = std::vec![9, 2, 0, 0, 2, 1, 0, 0xa0, 50];
    for (num, proto, ep, rlen) in [(0u8, 1u8, 0x81u8, 63u16), (1, 2, 0x82, 52)] {
        c.extend_from_slice(&[9, 4, num, 0, 1, 3, 1, proto, 0]);
        c.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, rlen as u8, (rlen >> 8) as u8]);
        c.extend_from_slice(&[7, 5, ep, 3, 8, 0, 10]);
    }
    let total = c.len() as u16;
    c[2..4].copy_from_slice(&total.to_le_bytes());
    let mut out = [HidIface::default(); 4];
    let n = hid_interfaces(&c, &mut out);
    assert_eq!(n, 2);
    assert_eq!(out[0], HidIface { config_value: 1, number: 0, subclass: 1, protocol: 1, report_len: 63, ep_addr: 0x81, ep_max_packet: 8, ep_interval: 10 });
    assert_eq!((out[1].number, out[1].protocol, out[1].ep_addr, out[1].report_len), (1, 2, 0x82, 52));
    // cut anywhere: no panic, and never more than the interfaces that were complete
    for cut in 0..c.len() {
        let _ = hid_interfaces(&c[..cut], &mut out);
    }
}

#[test]
fn intervals() {
    assert_eq!(xhci_interval(1, 10), 6); // 10 ms full speed: 80 frames -> 2^6
    assert_eq!(xhci_interval(2, 1), 3);
    assert_eq!(xhci_interval(3, 4), 3); // high speed: bInterval is exponent + 1
    assert_eq!(xhci_interval(4, 0), 0);
}
