//! Meter ballistics: how a level meter moves on screen.
//!
//! The engine reports the highest peak since the UI last asked (step 5's
//! lock-free `fetch_max`). Drawing those raw numbers would flicker, so
//! meters rise instantly, fall at a steady rate, and keep a "peak hold"
//! tick at the recent maximum for a moment. Clipping latches until clicked.

/// The bottom of the scale; quieter than this draws as empty.
pub const FLOOR_DB: f32 = -60.0;
/// How fast the bar falls, in dB per second.
const FALL_DB_PER_S: f32 = 24.0;
/// How long the peak-hold tick stays before it starts falling.
const HOLD_SECONDS: f32 = 1.5;

#[derive(Clone, Copy, Debug)]
pub struct MeterState {
    pub db: f32,
    pub hold_db: f32,
    hold_left: f32,
    pub clipped: bool,
}

impl Default for MeterState {
    fn default() -> Self {
        Self { db: FLOOR_DB, hold_db: FLOOR_DB, hold_left: 0.0, clipped: false }
    }
}

pub fn to_db(linear: f32) -> f32 {
    if linear <= 0.0 || linear.is_nan() {
        FLOOR_DB
    } else {
        (20.0 * linear.log10()).max(FLOOR_DB)
    }
}

impl MeterState {
    /// `peak` is the linear peak since the last frame, `dt` the seconds
    /// since the last frame.
    pub fn update(&mut self, peak: f32, dt: f32) {
        let new = to_db(peak);
        // Rise instantly; fall at a steady rate, never below the new value.
        self.db = if new >= self.db { new } else { (self.db - FALL_DB_PER_S * dt).max(new) };
        if new >= self.hold_db {
            self.hold_db = new;
            self.hold_left = HOLD_SECONDS;
        } else if self.hold_left > 0.0 {
            self.hold_left -= dt;
        } else {
            self.hold_db = (self.hold_db - FALL_DB_PER_S * dt).max(self.db);
        }
        // Over full scale (or NaN, which the engine reports as infinite).
        if peak > 1.0 {
            self.clipped = true;
        }
    }

    /// 0..1 position on the meter for a level in dB (top of scale +6 dB).
    pub fn fraction(db: f32) -> f32 {
        ((db - FLOOR_DB) / (6.0 - FLOOR_DB)).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rises_instantly_and_falls_steadily() {
        let mut m = MeterState::default();
        m.update(0.5, 0.016); // -6 dB
        assert!((m.db - -6.02).abs() < 0.01);
        m.update(0.0, 0.5); // silence for half a second: 12 dB lower
        assert!((m.db - -18.02).abs() < 0.01);
        m.update(0.0, 10.0);
        assert_eq!(m.db, FLOOR_DB, "never below the floor");
    }

    #[test]
    fn peak_hold_waits_then_falls() {
        let mut m = MeterState::default();
        m.update(1.0, 0.016); // 0 dB
        for _ in 0..90 {
            m.update(0.01, 0.016); // ~1.44 s of -40 dB
        }
        assert_eq!(m.hold_db, 0.0, "still holding");
        for _ in 0..20 {
            m.update(0.01, 0.016);
        }
        assert!(m.hold_db < 0.0, "now falling");
        assert!(m.hold_db >= m.db, "but never below the bar");
    }

    #[test]
    fn clipping_latches() {
        let mut m = MeterState::default();
        m.update(1.2, 0.016);
        m.update(0.1, 0.016);
        assert!(m.clipped);
        m.update(f32::INFINITY, 0.016);
        assert!(m.clipped, "infinite (NaN from the engine) counts as clipping");
    }
}
