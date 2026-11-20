//! Utilisation tracking: the fraction of time something (a task, or a core) was busy, as a
//! decaying average over fixed windows. Rises quickly, falls slowly.

pub const SCALE: u32 = 1024;
/// Windows of silence after which the average is simply zero.
const FORGET: u64 = 16;

#[derive(Clone, Copy, Debug)]
pub struct Util {
    window: u64,
    start: u64,
    acc: u64,
    avg: u32,
}

impl Util {
    pub const fn new(window: u64) -> Util {
        Util { window, start: 0, acc: 0, avg: 0 }
    }

    /// Close every window that ended by `now`.
    fn roll(&mut self, now: u64) {
        if now < self.start + self.window {
            return;
        }
        let whole = (now - self.start) / self.window;
        if whole > FORGET {
            self.avg = 0;
            self.acc = 0;
            self.start += whole * self.window;
            return;
        }
        for _ in 0..whole {
            let sample = (self.acc.min(self.window) * SCALE as u64 / self.window) as u32;
            self.avg = if sample > self.avg { (self.avg + sample * 3) / 4 } else { (self.avg * 3 + sample) / 4 };
            self.acc = 0;
            self.start += self.window;
        }
    }

    /// Record that the entity was busy from `from` to `to` (`from` not before the previous `to`).
    pub fn run(&mut self, mut from: u64, to: u64) {
        if to <= from {
            return;
        }
        self.roll(from);
        loop {
            let end = self.start + self.window;
            if to <= end {
                self.acc += to - from;
                return;
            }
            self.acc += end - from;
            from = end;
            self.roll(end);
        }
    }

    /// The estimate at `now`, 0..=SCALE.
    pub fn value(&mut self, now: u64) -> u32 {
        self.roll(now);
        let elapsed = (now - self.start).max(self.window / 4).max(1);
        let partial = (self.acc.min(elapsed) * SCALE as u64 / elapsed) as u32;
        self.avg.max(partial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_entity_converges_to_full() {
        let mut u = Util::new(100);
        u.run(0, 1000);
        assert!(u.value(1000) > 1000, "{}", u.value(1000));
    }

    #[test]
    fn half_busy_is_about_half() {
        let mut u = Util::new(100);
        for w in 0..30 {
            u.run(w * 100, w * 100 + 50);
        }
        let v = u.value(3000);
        assert!((480..=544).contains(&v), "{v}");
    }

    #[test]
    fn idleness_decays_and_is_forgotten() {
        let mut u = Util::new(100);
        u.run(0, 1000);
        let a = u.value(1200);
        let b = u.value(1500);
        assert!(b < a);
        assert_eq!(u.value(100_000), 0);
    }

    #[test]
    fn a_run_is_split_across_windows() {
        let mut u = Util::new(100);
        u.run(50, 350);
        assert!(u.value(350) >= 700);
    }

    #[test]
    fn rises_within_a_window() {
        let mut u = Util::new(1000);
        u.run(0, 300);
        assert!(u.value(300) > 900); // 300 busy of 300 elapsed
    }
}
