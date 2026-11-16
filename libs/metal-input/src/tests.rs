use super::driver::*;
use super::event::*;
use super::gesture::*;
use std::vec::Vec;

fn word(kind: u64, code: u16, value: i32) -> u64 {
    kind | (code as u64) << 8 | (value as u32 as u64) << 32
}

fn feed_all(t: &mut Translator, words: &[u64], now: u32) -> Vec<Event> {
    let mut v = Vec::new();
    for &w in words {
        t.feed(w, now, &mut |e| v.push(e));
    }
    v
}

// ---- fixed point ------------------------------------------------------------------------------------------------------

#[test]
fn fixed_point() {
    assert_eq!(Fx::from_int(3).0, 768);
    assert_eq!(Fx(700).floor(), 2);
    assert_eq!(Fx(-1).floor(), -1);
    assert_eq!(Fx(127).round(), 0);
    assert_eq!(Fx(128).round(), 1);
    assert_eq!(Fx::from_int(5) - Fx::from_int(7), Fx::from_int(-2));
    assert_eq!((-Fx::from_int(4)).abs(), Fx::from_int(4));
    // saturates instead of overflowing
    assert_eq!(Fx(i32::MAX) + Fx(1), Fx(i32::MAX));
    assert_eq!(Fx::from_int(i32::MAX).0, i32::MAX);
}

#[test]
fn absolute_coordinates() {
    assert_eq!(abs_to_fx(0, 1024), Fx(0));
    assert_eq!(abs_to_fx(16384, 1024), Fx::from_int(512));
    // the last device position stays just inside the screen
    assert!(abs_to_fx(32767, 1024).floor() <= 1023);
    assert_eq!(abs_to_fx(-5, 1024), Fx(0));
    assert_eq!(abs_to_fx(99999, 1024), abs_to_fx(32767, 1024));
    assert_eq!(abs_to_fx(100, -5), Fx(0));
}

// ---- driver words -----------------------------------------------------------------------------------------------------

#[test]
fn move_gives_enter_then_move() {
    let mut t = Translator::new(1000, 500);
    let ev = feed_all(&mut t, &[word(MOVE, 0, 16384 | 8192 << 16)], 10);
    assert_eq!(ev.len(), 2);
    let (Event::Pointer(a), Event::Pointer(b)) = (ev[0], ev[1]) else { panic!() };
    assert_eq!(a.phase, Phase::Enter);
    assert_eq!(b.phase, Phase::Move);
    assert_eq!((b.x, b.y), (Fx::from_int(500), Fx::from_int(125)));
    assert_eq!(b.kind, PointerKind::Mouse);
    assert_eq!(b.id, 0);
    // the second move is only a move
    assert_eq!(feed_all(&mut t, &[word(MOVE, 0, 0)], 20).len(), 1);
    assert_eq!(t.pointer_at(), (Fx(0), Fx(0)));
}

#[test]
fn buttons_track_the_mask_and_drop_duplicates() {
    let mut t = Translator::new(100, 100);
    let ev = feed_all(&mut t, &[word(BUTTON, 1, 1), word(BUTTON, 1, 1), word(BUTTON, 2, 1), word(BUTTON, 1, 0), word(BUTTON, 2, 0), word(BUTTON, 2, 0)], 5);
    let ps: Vec<Pointer> = ev.iter().map(|e| match e { Event::Pointer(p) => *p, _ => panic!() }).collect();
    assert_eq!(ps.len(), 4, "the duplicate press and the duplicate release are dropped");
    assert_eq!((ps[0].phase, ps[0].buttons, ps[0].button, ps[0].pressure), (Phase::Down, BTN_LEFT, BTN_LEFT, 255));
    assert_eq!((ps[1].phase, ps[1].buttons, ps[1].button), (Phase::Down, BTN_LEFT | BTN_RIGHT, BTN_RIGHT));
    assert_eq!((ps[2].phase, ps[2].buttons), (Phase::Up, BTN_RIGHT));
    assert_eq!((ps[3].phase, ps[3].buttons, ps[3].pressure), (Phase::Up, 0, 0));
    // buttons the system has no meaning for are ignored
    assert!(feed_all(&mut t, &[word(BUTTON, 9, 1), word(BUTTON, 0, 1)], 6).is_empty());
}

#[test]
fn wheel_becomes_a_scroll() {
    let mut t = Translator::new(100, 100);
    let ev = feed_all(&mut t, &[word(WHEEL, 0, 2), word(WHEEL, 0, -1), word(WHEEL, 0, 0), word(WHEEL, 0, 5000)], 0);
    assert_eq!(ev.len(), 3);
    let Event::Scroll(s) = ev[0] else { panic!() };
    assert_eq!((s.dy, s.lines, s.source), (Fx::from_int(80), 2, ScrollSource::Wheel));
    let Event::Scroll(s) = ev[1] else { panic!() };
    assert_eq!((s.dy, s.lines), (Fx::from_int(-40), -1));
    let Event::Scroll(s) = ev[2] else { panic!() };
    assert_eq!(s.lines, 100, "a huge value is clamped");
}

#[test]
fn keys_carry_modifiers() {
    let mut t = Translator::new(100, 100);
    let ev = feed_all(
        &mut t,
        &[word(KEY, KEY_LCTRL, 1), word(KEY, 0x1E, 1), word(KEY, 0x1E, 2), word(KEY, 0x1E, 0), word(KEY, KEY_LCTRL, 0), word(KEY, 0x1E, 1), word(KEY, 0x1E, 7)],
        0,
    );
    let ks: Vec<Key> = ev.iter().map(|e| match e { Event::Key(k) => *k, _ => panic!() }).collect();
    assert_eq!(ks.len(), 6, "an unknown value is dropped");
    assert!(ks[0].mods.has(Mods::CTRL), "the modifier's own press already counts");
    assert!(ks[1].mods.has(Mods::CTRL) && ks[1].phase == KeyPhase::Down);
    assert_eq!(ks[2].phase, KeyPhase::Repeat);
    assert_eq!(ks[3].phase, KeyPhase::Up);
    assert!(!ks[4].mods.has(Mods::CTRL), "released");
    assert_eq!(ks[5].mods, Mods(0));
    assert_eq!(ks[1].code, 0x1E);
    assert_eq!(ks[1].ch, None);
}

#[test]
fn several_modifiers_and_unknown_kinds() {
    let mut t = Translator::new(100, 100);
    feed_all(&mut t, &[word(KEY, KEY_LCTRL, 1), word(KEY, KEY_LALT, 1), word(KEY, KEY_LSHIFT, 1), word(KEY, KEY_LSUPER, 1)], 0);
    assert!(t.mods().has(Mods::CTRL | Mods::ALT | Mods::SHIFT | Mods::SUPER));
    feed_all(&mut t, &[word(KEY, KEY_RSHIFT, 0)], 0);
    assert!(!t.mods().has(Mods::SHIFT));
    // the right-hand Ctrl and Alt count too, and the numbers are the driver's (kbd.rs: 97 and 100)
    let mut r = Translator::new(100, 100);
    feed_all(&mut r, &[word(KEY, 97, 1), word(KEY, 100, 1)], 0);
    assert!(r.mods().has(Mods::CTRL | Mods::ALT));
    assert_eq!((KEY_LCTRL, KEY_LSHIFT, KEY_LALT, KEY_TAB, KEY_LSUPER), (29, 42, 56, 15, 125));
    assert!(feed_all(&mut t, &[word(99, 1, 1), word(TOUCH, 1, 1), 0, u64::MAX], 0).len() <= 1);
}

#[test]
fn translator_never_panics_on_any_word() {
    let mut t = Translator::new(1920, 1080);
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..20000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        t.feed(x, x as u32, &mut |_| {});
        t.feed(x & 0xFFFF_FFFF_FF00_FF03 | 2, 0, &mut |_| {});
    }
}

// ---- gestures ---------------------------------------------------------------------------------------------------------

fn ptr(id: u8, kind: PointerKind, phase: Phase, x: i32, y: i32, time: u32) -> Pointer {
    let held = matches!(phase, Phase::Down | Phase::Move);
    Pointer {
        id,
        kind,
        phase,
        x: Fx::from_int(x),
        y: Fx::from_int(y),
        buttons: if held { BTN_LEFT } else { 0 },
        button: if matches!(phase, Phase::Down | Phase::Up) { BTN_LEFT } else { 0 },
        pressure: 255,
        time,
    }
}
fn touch(id: u8, phase: Phase, x: i32, y: i32, t: u32) -> Pointer {
    ptr(id, PointerKind::Touch, phase, x, y, t)
}
fn mouse(phase: Phase, x: i32, y: i32, t: u32) -> Pointer {
    ptr(0, PointerKind::Mouse, phase, x, y, t)
}

#[test]
fn tap_and_click() {
    let mut r = Recognizer::new();
    assert_eq!(r.event(&touch(1, Phase::Down, 50, 60, 0)), None);
    assert_eq!(r.event(&touch(1, Phase::Up, 50, 60, 120)), Some(Gesture::Tap { x: Fx::from_int(50), y: Fx::from_int(60) }));
    // a mouse click is a tap
    assert_eq!(r.event(&mouse(Phase::Down, 5, 5, 1000)), None);
    assert_eq!(r.event(&mouse(Phase::Up, 5, 5, 1050)), Some(Gesture::Tap { x: Fx::from_int(5), y: Fx::from_int(5) }));
}

#[test]
fn small_wobble_is_still_a_tap() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 100, 100, 0));
    assert_eq!(r.event(&touch(1, Phase::Move, 104, 103, 50)), None);
    assert!(matches!(r.event(&touch(1, Phase::Up, 104, 103, 90)), Some(Gesture::Tap { .. })));
}

#[test]
fn a_slow_press_is_not_a_tap() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 10, 10, 0));
    assert_eq!(r.event(&touch(1, Phase::Up, 10, 10, TAP_MS + 1)), None);
    // exactly on the limit still counts
    r.event(&touch(1, Phase::Down, 10, 10, 1000));
    assert!(matches!(r.event(&touch(1, Phase::Up, 10, 10, 1000 + TAP_MS)), Some(Gesture::Tap { .. })));
}

#[test]
fn long_press_for_touch_but_not_for_mouse() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 30, 40, 0));
    assert_eq!(r.tick(LONG_PRESS_MS - 1), None);
    assert_eq!(r.tick(LONG_PRESS_MS), Some(Gesture::ContextMenu { x: Fx::from_int(30), y: Fx::from_int(40) }));
    assert_eq!(r.tick(LONG_PRESS_MS + 100), None, "only once");
    assert_eq!(r.event(&touch(1, Phase::Up, 30, 40, 900)), None, "lifting after a long press is not a tap");

    let mut m = Recognizer::new();
    m.event(&mouse(Phase::Down, 30, 40, 0));
    assert_eq!(m.tick(5000), None, "holding the mouse button is not a long press");
}

#[test]
fn moving_cancels_the_long_press() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 0, 0, 0));
    r.event(&touch(1, Phase::Move, 50, 0, 100));
    assert_eq!(r.tick(2000), None);
}

#[test]
fn right_click_is_a_context_menu() {
    let mut r = Recognizer::new();
    let mut p = mouse(Phase::Down, 77, 88, 0);
    p.button = BTN_RIGHT;
    p.buttons = BTN_RIGHT;
    assert_eq!(r.event(&p), Some(Gesture::ContextMenu { x: Fx::from_int(77), y: Fx::from_int(88) }));
    p.phase = Phase::Up;
    p.buttons = 0;
    assert_eq!(r.event(&p), None);
    // the middle button is not a gesture
    let mut p = mouse(Phase::Down, 1, 1, 0);
    p.button = BTN_MIDDLE;
    assert_eq!(r.event(&p), None);
}

#[test]
fn pan_sums_to_the_distance_moved() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 100, 100, 0));
    let mut total = (Fx::ZERO, Fx::ZERO);
    let mut n = 0;
    for (i, x) in (110..=200).step_by(10).enumerate() {
        if let Some(Gesture::Pan { dx, dy, .. }) = r.event(&touch(1, Phase::Move, x, 100, 16 * (i as u32 + 1))) {
            total = (total.0 + dx, total.1 + dy);
            n += 1;
        }
    }
    assert!(n >= 8);
    assert_eq!(total, (Fx::from_int(100), Fx::ZERO), "the deltas add up to the whole movement");
}

#[test]
fn the_first_pan_includes_the_slop_distance() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 0, 0, 0));
    assert_eq!(r.event(&touch(1, Phase::Move, 6, 0, 10)), None);
    let g = r.event(&touch(1, Phase::Move, 12, 0, 20));
    assert_eq!(g, Some(Gesture::Pan { dx: Fx::from_int(12), dy: Fx::ZERO, x: Fx::from_int(12), y: Fx::ZERO }));
}

#[test]
fn fling_velocity() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 0, 0, 0));
    r.event(&touch(1, Phase::Move, 20, 0, 10));
    r.event(&touch(1, Phase::Move, 40, 0, 20)); // 20 lp in 10 ms = 2000 lp/s
    assert_eq!(r.event(&touch(1, Phase::Up, 40, 0, 30)), Some(Gesture::PanEnd { vx: 2000, vy: 0 }));
    // a pointer that stopped before lifting does not fling
    r.event(&touch(1, Phase::Down, 0, 0, 1000));
    r.event(&touch(1, Phase::Move, 20, 0, 1010));
    r.event(&touch(1, Phase::Move, 40, 0, 1020));
    assert_eq!(r.event(&touch(1, Phase::Up, 40, 0, 1500)), Some(Gesture::PanEnd { vx: 0, vy: 0 }));
}

#[test]
fn mouse_drag_pans() {
    let mut r = Recognizer::new();
    r.event(&mouse(Phase::Down, 10, 10, 0));
    // moving without a button held (a hover) does nothing
    let mut hover = mouse(Phase::Move, 300, 300, 5);
    hover.buttons = 0;
    assert!(matches!(r.event(&mouse(Phase::Move, 40, 10, 10)), Some(Gesture::Pan { .. })));
    let mut r2 = Recognizer::new();
    assert_eq!(r2.event(&hover), None);
}

#[test]
fn pinch_factor() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 100, 100, 0));
    assert_eq!(r.event(&touch(2, Phase::Down, 200, 100, 5)), None);
    // spread to twice the distance
    let g = r.event(&touch(2, Phase::Move, 300, 100, 20));
    assert_eq!(g, Some(Gesture::Pinch { factor_q8: 512, cx: Fx::from_int(200), cy: Fx::from_int(100) }));
    // pinch in by half of that
    let g = r.event(&touch(2, Phase::Move, 200, 100, 40));
    assert_eq!(g, Some(Gesture::Pinch { factor_q8: 128, cx: Fx::from_int(150), cy: Fx::from_int(100) }));
    // a move that changes nothing gives nothing
    assert_eq!(r.event(&touch(2, Phase::Move, 200, 100, 50)), None);
}

#[test]
fn two_pointers_never_tap_or_pan() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 100, 100, 0));
    r.event(&touch(2, Phase::Down, 150, 100, 10));
    assert_eq!(r.event(&touch(1, Phase::Up, 100, 100, 50)), None);
    assert_eq!(r.event(&touch(2, Phase::Up, 150, 100, 60)), None);
    // and the recogniser is clean again afterwards
    r.event(&touch(1, Phase::Down, 5, 5, 1000));
    assert!(matches!(r.event(&touch(1, Phase::Up, 5, 5, 1010)), Some(Gesture::Tap { .. })));
}

#[test]
fn cancel_gives_nothing_and_clears() {
    let mut r = Recognizer::new();
    r.event(&touch(1, Phase::Down, 0, 0, 0));
    assert_eq!(r.event(&touch(1, Phase::Cancel, 0, 0, 10)), None);
    assert_eq!(r.tick(5000), None);
    // a third pointer while two are active is ignored, and nothing breaks
    r.event(&touch(1, Phase::Down, 0, 0, 100));
    r.event(&touch(2, Phase::Down, 10, 0, 100));
    assert_eq!(r.event(&touch(3, Phase::Down, 20, 0, 100)), None);
    assert_eq!(r.event(&touch(3, Phase::Up, 20, 0, 110)), None);
}

#[test]
fn pen_behaves_like_touch() {
    let mut r = Recognizer::new();
    r.event(&ptr(4, PointerKind::Pen, Phase::Down, 9, 9, 0));
    assert!(matches!(r.tick(LONG_PRESS_MS), Some(Gesture::ContextMenu { .. })));
}

#[test]
fn recogniser_never_panics_on_hostile_traces() {
    let mut r = Recognizer::new();
    let mut x = 0x1234_5678_9ABC_DEF1u64;
    for _ in 0..50000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let phase = [Phase::Enter, Phase::Down, Phase::Move, Phase::Up, Phase::Cancel, Phase::Leave][(x % 6) as usize];
        let kind = [PointerKind::Mouse, PointerKind::Touch, PointerKind::Pen][(x >> 8) as usize % 3];
        let p = Pointer {
            id: (x >> 16) as u8 % 4,
            kind,
            phase,
            x: Fx((x >> 24) as i32),
            y: Fx((x >> 40) as i32),
            buttons: (x >> 12) as u8 & 7,
            button: [0, 1, 2, 4][(x >> 20) as usize % 4],
            pressure: 0,
            time: (x >> 32) as u32,
        };
        r.event(&p);
        r.tick((x >> 11) as u32);
    }
}

#[test]
fn left_handed_swaps_the_buttons_even_while_one_is_held() {
    let mut t = Translator::new(100, 100);
    let buttons = |ev: Vec<Event>| ev.iter().map(|e| match e { Event::Pointer(p) => (p.phase, p.button, p.buttons), _ => panic!() }).collect::<Vec<_>>();
    t.set_left_handed(true);
    assert!(t.left_handed());
    assert_eq!(buttons(feed_all(&mut t, &[word(BUTTON, 1, 1), word(BUTTON, 1, 0)], 1)), std::vec![(Phase::Down, BTN_RIGHT, BTN_RIGHT), (Phase::Up, BTN_RIGHT, 0)]);
    assert_eq!(buttons(feed_all(&mut t, &[word(BUTTON, 2, 1)], 2)), std::vec![(Phase::Down, BTN_LEFT, BTN_LEFT)]);
    // switched back while the physical right button is still down: its release is still seen, nothing stays stuck
    t.set_left_handed(false);
    assert_eq!(buttons(feed_all(&mut t, &[word(BUTTON, 2, 0)], 3)), std::vec![(Phase::Up, BTN_RIGHT, 0)]);
    assert_eq!(buttons(feed_all(&mut t, &[word(BUTTON, 3, 1)], 4)), std::vec![(Phase::Down, BTN_MIDDLE, BTN_MIDDLE)]);
}
