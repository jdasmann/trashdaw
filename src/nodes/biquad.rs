//! A biquad filter: the building block of EQs. One design covers lowpass,
//! highpass, bandpass, notch, peaking bells and shelves; only the five
//! coefficients change.
//!
//! Coefficients follow Robert Bristow-Johnson's "Audio EQ Cookbook", the
//! standard reference for these formulas.

use crate::buffer::AudioBuffer;
use crate::processor::{PrepareInfo, Processor};
use crate::smoothing::Smoothed;
use crate::transport::ProcessContext;
use std::f64::consts::TAU;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FilterKind {
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
    Peak,
    LowShelf,
    HighShelf,
}

impl FilterKind {
    fn from_index(i: u32) -> Self {
        match i {
            0 => Self::Lowpass,
            1 => Self::Highpass,
            2 => Self::Bandpass,
            3 => Self::Notch,
            4 => Self::Peak,
            5 => Self::LowShelf,
            _ => Self::HighShelf,
        }
    }
}

/// Normalized coefficients (a0 divided out).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coeffs {
    pub fn new(kind: FilterKind, freq: f64, q: f64, gain_db: f64, sample_rate: f64) -> Self {
        let w0 = TAU * freq / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a = 10f64.powf(gain_db / 40.0); // shelf/peak amplitude
        let sq = 2.0 * a.sqrt() * alpha;

        let (b0, b1, b2, a0, a1, a2) = match kind {
            FilterKind::Lowpass => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::Highpass => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::Bandpass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
            FilterKind::Peak => (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a),
            FilterKind::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cos + sq),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - sq),
                (a + 1.0) + (a - 1.0) * cos + sq,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - sq,
            ),
            FilterKind::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cos + sq),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - sq),
                (a + 1.0) - (a - 1.0) * cos + sq,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - sq,
            ),
        };
        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// The exact gain at `freq`, from the transfer function H(z) on the
    /// unit circle. Used by the tests to check the measured response.
    #[cfg(test)]
    pub fn magnitude_db(&self, freq: f64, sample_rate: f64) -> f64 {
        let w = TAU * freq / sample_rate;
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num = ((self.b0 + self.b1 * c1 + self.b2 * c2).powi(2) + (self.b1 * s1 + self.b2 * s2).powi(2)).sqrt();
        let den = ((1.0 + self.a1 * c1 + self.a2 * c2).powi(2) + (self.a1 * s1 + self.a2 * s2).powi(2)).sqrt();
        20.0 * (num / den).log10()
    }
}

/// Transposed Direct Form II: the usual choice for floating point. It
/// needs only two state values per channel and behaves well numerically
/// when coefficients change.
#[derive(Clone, Copy, Default)]
struct State {
    z1: f64,
    z2: f64,
}

impl State {
    #[inline(always)]
    fn tick(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }
}

/// While a parameter moves, recompute coefficients every this many
/// samples. The math (sin, cos, pow) is too expensive per sample, and
/// changes this fast are smooth to the ear.
const CONTROL_RATE: usize = 16;

pub struct Biquad {
    kind: FilterKind,
    freq: Smoothed,
    q: Smoothed,
    gain_db: Smoothed,
    sample_rate: f32,
    coeffs: Coeffs,
    /// f64 state on purpose: at low cutoffs the poles sit very close to
    /// the unit circle, and f32 rounding audibly shifts the response (or
    /// adds noise). On modern CPUs f64 scalar math costs about the same.
    state: [State; 2],
    countdown: usize,
}

impl Biquad {
    pub fn new() -> Self {
        let mut s = Self {
            kind: FilterKind::Lowpass,
            freq: Smoothed::new(1_000.0),
            q: Smoothed::new(std::f32::consts::FRAC_1_SQRT_2),
            gain_db: Smoothed::new(0.0),
            sample_rate: 48_000.0,
            coeffs: Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 },
            state: [State::default(); 2],
            countdown: 0,
        };
        s.update_coeffs();
        s
    }

    fn update_coeffs(&mut self) {
        self.coeffs = Coeffs::new(
            self.kind,
            self.freq.value() as f64,
            self.q.value() as f64,
            self.gain_db.value() as f64,
            self.sample_rate as f64,
        );
    }

    fn settled(&self) -> bool {
        self.freq.is_settled() && self.q.is_settled() && self.gain_db.is_settled()
    }
}

impl Processor for Biquad {
    fn name(&self) -> &'static str {
        "filter"
    }

    /// type: 0 lowpass, 1 highpass, 2 bandpass, 3 notch, 4 peak,
    /// 5 low shelf, 6 high shelf. freq in Hz, gain in dB (peak/shelves).
    fn params(&self) -> &'static [&'static str] {
        &["type", "freq", "q", "gain"]
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.sample_rate = info.sample_rate;
        // A slower ramp for frequency: sweeps sound smooth, not stepped.
        self.freq.prepare(info.sample_rate, 0.02);
        self.q.prepare(info.sample_rate, 0.02);
        self.gain_db.prepare(info.sample_rate, 0.02);
        self.update_coeffs();
    }

    fn set_param(&mut self, id: usize, value: f32) {
        match id {
            0 => {
                self.kind = FilterKind::from_index(value.max(0.0) as u32);
                self.update_coeffs();
            }
            1 => self.freq.set(value.clamp(10.0, self.sample_rate * 0.49)),
            2 => self.q.set(value.clamp(0.1, 30.0)),
            3 => self.gain_db.set(value.clamp(-30.0, 30.0)),
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext) {
        let channels = buffer.channels().min(2);
        if self.settled() {
            // Constant coefficients: a tight loop per channel.
            let c = self.coeffs;
            for ch in 0..channels {
                let mut st = self.state[ch];
                for x in buffer.channel_mut(ch) {
                    *x = st.tick(&c, *x as f64) as f32;
                }
                self.state[ch] = st;
            }
            return;
        }

        for i in 0..buffer.frames() {
            self.freq.next();
            self.q.next();
            self.gain_db.next();
            if self.countdown == 0 {
                self.update_coeffs();
                self.countdown = CONTROL_RATE;
            }
            self.countdown -= 1;
            for ch in 0..channels {
                let x = buffer.channel(ch)[i] as f64;
                buffer.channel_mut(ch)[i] = self.state[ch].tick(&self.coeffs, x) as f32;
            }
        }
        // Make sure the final, settled values are what we run with next.
        if self.settled() {
            self.update_coeffs();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    const SR: f64 = 48_000.0;

    /// Passes a sine through the filter and measures its steady-state gain.
    fn measured_db(f: &mut Biquad, freq: f64) -> f64 {
        f.state = [State::default(); 2];
        let n = 48_000;
        let mut buf = AudioBuffer::new(2, n);
        for (i, s) in buf.channel_mut(0).iter_mut().enumerate() {
            *s = (TAU * freq * i as f64 / SR).sin() as f32;
        }
        f.process(&mut buf, &Transport::new(SR).context());
        // Skip the start, where the filter is still settling.
        let tail = &buf.channel(0)[n / 2..];
        let rms = (tail.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        20.0 * (rms * 2f64.sqrt()).log10()
    }

    fn filter(kind: FilterKind, freq: f32, q: f32, gain: f32) -> Biquad {
        let mut f = Biquad::new();
        f.prepare(&PrepareInfo { sample_rate: SR as f32, max_frames: 48_000, channels: 2 });
        f.kind = kind;
        f.freq = Smoothed::new(freq);
        f.q = Smoothed::new(q);
        f.gain_db = Smoothed::new(gain);
        f.update_coeffs();
        f
    }

    #[test]
    fn responses_match_the_textbook_shapes() {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let near = |a: f64, b: f64, tol: f64| assert!((a - b).abs() < tol, "{a:.2} dB, expected {b:.2}");

        let mut lp = filter(FilterKind::Lowpass, 1_000.0, q, 0.0);
        near(measured_db(&mut lp, 100.0), 0.0, 0.1);
        near(measured_db(&mut lp, 1_000.0), -3.01, 0.1); // the -3 dB point
        // At least the analog 12 dB/octave (-40 dB a decade up). Digital is
        // steeper near the top, because the bilinear transform puts a zero
        // exactly at Nyquist, so compare with the exact transfer function.
        let at_10k = measured_db(&mut lp, 10_000.0);
        assert!(at_10k < -40.0);
        near(at_10k, lp.coeffs.magnitude_db(10_000.0, SR), 0.1);

        let mut hp = filter(FilterKind::Highpass, 1_000.0, q, 0.0);
        near(measured_db(&mut hp, 10_000.0), 0.0, 0.1);
        near(measured_db(&mut hp, 1_000.0), -3.01, 0.1);

        let mut peak = filter(FilterKind::Peak, 2_000.0, 1.0, 6.0);
        near(measured_db(&mut peak, 2_000.0), 6.0, 0.1);
        near(measured_db(&mut peak, 100.0), 0.0, 0.2);

        let mut notch = filter(FilterKind::Notch, 1_000.0, 2.0, 0.0);
        assert!(measured_db(&mut notch, 1_000.0) < -40.0);

        let mut low_shelf = filter(FilterKind::LowShelf, 200.0, q, -9.0);
        near(measured_db(&mut low_shelf, 30.0), -9.0, 0.3);
        near(measured_db(&mut low_shelf, 5_000.0), 0.0, 0.1);

        let mut high_shelf = filter(FilterKind::HighShelf, 4_000.0, q, 6.0);
        near(measured_db(&mut high_shelf, 15_000.0), 6.0, 0.3);
        near(measured_db(&mut high_shelf, 100.0), 0.0, 0.1);
    }

    #[test]
    fn measured_response_matches_the_transfer_function() {
        let mut f = filter(FilterKind::Peak, 800.0, 3.0, -12.0);
        for freq in [50.0, 400.0, 800.0, 1_600.0, 8_000.0] {
            let expected = f.coeffs.magnitude_db(freq, SR);
            assert!((measured_db(&mut f, freq) - expected).abs() < 0.1);
        }
    }

    #[test]
    fn low_cutoffs_stay_accurate() {
        // Poles very close to the unit circle: the case f64 state is for.
        let mut lp = filter(FilterKind::Lowpass, 20.0, std::f32::consts::FRAC_1_SQRT_2, 0.0);
        assert!((measured_db(&mut lp, 5.0) - 0.0).abs() < 0.1);
        assert!((measured_db(&mut lp, 20.0) + 3.01).abs() < 0.1);
    }

    #[test]
    fn fast_sweeps_stay_stable() {
        let mut f = Biquad::new();
        f.prepare(&PrepareInfo { sample_rate: SR as f32, max_frames: 512, channels: 2 });
        f.set_param(2, 20.0); // very resonant
        let ctx = Transport::new(SR).context();
        let mut buf = AudioBuffer::new(2, 512);
        for block in 0..400 {
            // Jump the cutoff between extremes every block.
            f.set_param(1, if block % 2 == 0 { 30.0 } else { 20_000.0 });
            for (i, s) in buf.channel_mut(0).iter_mut().enumerate() {
                *s = ((i * 7919 % 1000) as f32 / 500.0) - 1.0;
            }
            f.process(&mut buf, &ctx);
            assert!(buf.channel(0).iter().all(|s| s.is_finite() && s.abs() < 100.0), "blew up at block {block}");
        }
    }
}
