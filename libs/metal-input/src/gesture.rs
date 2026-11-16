//! The gesture recogniser: pointers in, tap / context menu / pan / pinch out.
//!
//! One pointer: a tap (down and up inside the slop within 300 ms), a long press (touch and pen only: held 500 ms inside the slop; the caller
//! calls [`Recognizer::tick`] with the time), a pan (moved past the slop; deltas, then the release velocity). Two pointers: a pinch (the factor
//! by which the distance between them changed). The mouse gets a context menu from the right button, and a pan from the left button held
//! while moving; it never gets a long press (holding and then dragging is normal with a mouse).

use crate::event::*;

pub const TAP_SLOP: Fx = Fx::from_int(10);
pub const TAP_MS: u32 = 300;
pub const LONG_PRESS_MS: u32 = 500;
/// A pan release counts as a fling only if the last move was this recent.
pub const FLING_WINDOW_MS: u32 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Gesture {
    Tap { x: Fx, y: Fx },
    /// A long press (touch, pen) or a right click (mouse): the context menu.
    ContextMenu { x: Fx, y: Fx },
    /// The pointer moved by (`dx`, `dy`) since the last `Pan`; `x`, `y` is where it is now. The first one includes the slop distance.
    Pan { dx: Fx, dy: Fx, x: Fx, y: Fx },
    /// The pan ended; velocity in logical pixels per second (0 if the pointer had stopped before it lifted).
    PanEnd { vx: i32, vy: i32 },
    /// The distance between two pointers changed by `factor_q8 / 256` since the last `Pinch` (256 = unchanged); `cx`, `cy` the midpoint.
    Pinch { factor_q8: i32, cx: Fx, cy: Fx },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Pending,
    Panning,
    /// Taken by a long press or a pinch: lifting it gives nothing.
    Consumed,
}

#[derive(Clone, Copy, Debug)]
struct Track {
    id: u8,
    kind: PointerKind,
    state: State,
    x0: Fx,
    y0: Fx,
    x: Fx,
    y: Fx,
    t0: u32,
    last_t: u32,
    vx: i32,
    vy: i32,
}

#[derive(Clone, Debug, Default)]
pub struct Recognizer {
    tracks: [Option<Track>; 2],
    pinch_dist: u64,
}

fn isqrt(n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

fn dist(ax: Fx, ay: Fx, bx: Fx, by: Fx) -> u64 {
    let (dx, dy) = ((ax.0 as i64 - bx.0 as i64).unsigned_abs(), (ay.0 as i64 - by.0 as i64).unsigned_abs());
    isqrt(dx * dx + dy * dy)
}

impl Recognizer {
    pub fn new() -> Recognizer {
        Recognizer::default()
    }

    fn slot(&self, id: u8) -> Option<usize> {
        self.tracks.iter().position(|t| t.map_or(false, |t| t.id == id))
    }

    fn active(&self) -> usize {
        self.tracks.iter().flatten().count()
    }

    /// Feed one pointer event; at most one gesture results.
    pub fn event(&mut self, p: &Pointer) -> Option<Gesture> {
        match p.phase {
            Phase::Down => {
                // the mouse's right button is a context menu at once; its other buttons are not gestures
                if p.kind == PointerKind::Mouse {
                    if p.button == BTN_RIGHT {
                        return Some(Gesture::ContextMenu { x: p.x, y: p.y });
                    }
                    if p.button != BTN_LEFT {
                        return None;
                    }
                }
                if self.slot(p.id).is_some() {
                    return None;
                }
                let free = self.tracks.iter().position(|t| t.is_none())?;
                self.tracks[free] = Some(Track {
                    id: p.id,
                    kind: p.kind,
                    state: State::Pending,
                    x0: p.x,
                    y0: p.y,
                    x: p.x,
                    y: p.y,
                    t0: p.time,
                    last_t: p.time,
                    vx: 0,
                    vy: 0,
                });
                if self.active() == 2 {
                    // a second pointer: both are a pinch now, not a tap or a pan
                    for t in self.tracks.iter_mut().flatten() {
                        t.state = State::Consumed;
                    }
                    if let (Some(a), Some(b)) = (self.tracks[0], self.tracks[1]) {
                        self.pinch_dist = dist(a.x, a.y, b.x, b.y);
                    }
                }
                None
            }
            Phase::Move => {
                let i = self.slot(p.id)?;
                if self.active() == 2 {
                    self.tracks[i].as_mut()?.x = p.x;
                    self.tracks[i].as_mut()?.y = p.y;
                    let (a, b) = (self.tracks[0]?, self.tracks[1]?);
                    let d = dist(a.x, a.y, b.x, b.y);
                    let prev = core::mem::replace(&mut self.pinch_dist, d);
                    if prev == 0 || d == prev {
                        return None;
                    }
                    let factor = ((d as u128 * 256 + prev as u128 / 2) / prev as u128).min(i32::MAX as u128) as i32;
                    return Some(Gesture::Pinch {
                        factor_q8: factor,
                        cx: Fx(((a.x.0 as i64 + b.x.0 as i64) / 2) as i32),
                        cy: Fx(((a.y.0 as i64 + b.y.0 as i64) / 2) as i32),
                    });
                }
                let t = self.tracks[i].as_mut()?;
                let (px, py, pt) = (t.x, t.y, t.last_t);
                t.x = p.x;
                t.y = p.y;
                t.last_t = p.time;
                match t.state {
                    State::Pending if dist(t.x, t.y, t.x0, t.y0) > TAP_SLOP.0 as u64 => {
                        t.state = State::Panning;
                        Some(Gesture::Pan { dx: t.x - t.x0, dy: t.y - t.y0, x: t.x, y: t.y })
                    }
                    State::Panning => {
                        let (dx, dy) = (t.x - px, t.y - py);
                        let dt = p.time.wrapping_sub(pt).max(1) as i64;
                        t.vx = (dx.0 as i64 * 1000 / dt / 256) as i32;
                        t.vy = (dy.0 as i64 * 1000 / dt / 256) as i32;
                        Some(Gesture::Pan { dx, dy, x: t.x, y: t.y })
                    }
                    _ => None,
                }
            }
            Phase::Up | Phase::Cancel => {
                if p.kind == PointerKind::Mouse && p.phase == Phase::Up && p.button != BTN_LEFT {
                    return None;
                }
                let i = self.slot(p.id)?;
                let t = self.tracks[i].take()?;
                if self.active() < 2 {
                    self.pinch_dist = 0;
                }
                if p.phase == Phase::Cancel {
                    return None;
                }
                match t.state {
                    State::Pending if p.time.wrapping_sub(t.t0) <= TAP_MS => Some(Gesture::Tap { x: p.x, y: p.y }),
                    State::Panning => {
                        let fresh = p.time.wrapping_sub(t.last_t) <= FLING_WINDOW_MS;
                        Some(Gesture::PanEnd { vx: if fresh { t.vx } else { 0 }, vy: if fresh { t.vy } else { 0 } })
                    }
                    _ => None,
                }
            }
            Phase::Enter | Phase::Leave => None,
        }
    }

    /// The time has moved on to `now`: a touch or pen held still long enough becomes a context menu.
    pub fn tick(&mut self, now: u32) -> Option<Gesture> {
        if self.active() != 1 {
            return None;
        }
        let t = self.tracks.iter_mut().flatten().next()?;
        if t.kind != PointerKind::Mouse && t.state == State::Pending && now.wrapping_sub(t.t0) >= LONG_PRESS_MS {
            t.state = State::Consumed;
            return Some(Gesture::ContextMenu { x: t.x, y: t.y });
        }
        None
    }
}
