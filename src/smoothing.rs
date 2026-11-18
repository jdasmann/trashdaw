/// One-pole parameter smoother. Prevents clicks when values jump.
pub struct Smoothed {
    current: f32,
    target: f32,
    coeff: f32,
}

/// Close enough to call it arrived, relative to the target's size (or
/// absolute, for targets below 1). Snapping also avoids creeping toward
/// the target forever, and into slow denormal floats.
const SNAP: f32 = 1e-7;

#[inline(always)]
fn close_enough(remaining: f32, target: f32) -> bool {
    remaining.abs() < SNAP * target.abs().max(1.0)
}

impl Smoothed {
    pub fn new(value: f32) -> Self {
        Self { current: value, target: value, coeff: 1.0 }
    }

    /// `time_s` is roughly how long it takes to reach the target (~63%).
    pub fn prepare(&mut self, sample_rate: f32, time_s: f32) {
        self.coeff = 1.0 - (-1.0 / (time_s * sample_rate)).exp();
    }

    pub fn set(&mut self, target: f32) {
        self.target = target;
    }

    #[inline]
    pub fn next(&mut self) -> f32 {
        let next = self.current + (self.target - self.current) * self.coeff;
        // `next == current` catches the f32 stall: near large values a step
        // smaller than half the spacing between floats rounds to nothing,
        // and without this the value would stop short of the target forever.
        self.current = if next == self.current || close_enough(next - self.target, self.target) {
            self.target
        } else {
            next
        };
        self.current
    }

    /// Jumps ahead exactly as if `next` had been called `n` times, without
    /// the loop. Each step keeps (1 - coeff) of the remaining distance, so
    /// after n steps (1 - coeff)^n of it is left.
    pub fn skip(&mut self, n: usize) {
        let remaining = (self.current - self.target) * (1.0 - self.coeff).powi(n as i32);
        let next = self.target + remaining;
        self.current = if next == self.target || close_enough(remaining, self.target) { self.target } else { next };
    }

    /// True when the value has arrived and is not moving. Lets callers
    /// compute things once per block instead of once per sample.
    #[inline]
    pub fn is_settled(&self) -> bool {
        self.current == self.target
    }

    /// The current value, without advancing.
    #[inline]
    pub fn value(&self) -> f32 {
        self.current
    }

    /// True once the value has fully arrived at `value` and is staying.
    pub fn is_at(&self, value: f32) -> bool {
        self.current == value && self.target == value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_matches_stepping() {
        let mut a = Smoothed::new(1.0);
        let mut b = Smoothed::new(1.0);
        a.prepare(48_000.0, 0.01);
        b.prepare(48_000.0, 0.01);
        a.set(0.25);
        b.set(0.25);
        for _ in 0..300 {
            a.next();
        }
        b.skip(300);
        assert!((a.next() - b.next()).abs() < 1e-5);
    }

    #[test]
    fn settles_exactly() {
        let mut s = Smoothed::new(1.0);
        s.prepare(48_000.0, 0.005);
        s.set(0.0);
        s.skip(48_000);
        assert!(s.is_at(0.0));
    }

    #[test]
    fn large_values_settle() {
        // Regression: in f32, a step smaller than half the spacing between
        // representable values rounds to nothing, and the old smoother
        // stalled short of large targets forever (6000.586 instead of 6000).
        for (from, to, time) in [(12_000.0, 6_000.0, 0.05), (20_000.0, 2_000.0, 0.02), (440.0, 880.0, 0.01)] {
            let mut s = Smoothed::new(from);
            s.prepare(48_000.0, time);
            s.set(to);
            for _ in 0..48_000 * 2 {
                s.next();
            }
            assert!(s.is_settled(), "{from} -> {to} stalled at {}", s.value());
        }
    }
}
