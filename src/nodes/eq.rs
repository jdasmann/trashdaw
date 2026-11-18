//! A three-band EQ: low shelf, mid peak, high shelf, in one node.
//!
//! Why one struct with three coefficient sets instead of three `Biquad`
//! nodes in a row?
//! - One pass over the buffer: each sample goes through all three bands
//!   while it's in a register, and the state stays in f64 the whole way
//!   (three nodes would round to f32 between stages).
//! - Bands at exactly 0 dB are skipped. A flat EQ costs almost nothing.
//! The trade-off: "settled" is all-or-nothing. Moving one band's frequency
//! puts all three on the slower per-sample path for that block, where three
//! separate nodes would only slow down the one that moves.

use super::biquad::{Coeffs, FilterKind, State, CONTROL_RATE};
use crate::buffer::AudioBuffer;
use crate::processor::{ParamInfo, PrepareInfo, Processor};
use crate::smoothing::Smoothed;
use crate::transport::ProcessContext;

struct Band {
    kind: FilterKind,
    freq: Smoothed,
    gain_db: Smoothed,
    q: Smoothed,
    coeffs: Coeffs,
}

impl Band {
    fn new(kind: FilterKind, freq: f32, q: f32) -> Self {
        Self {
            kind,
            freq: Smoothed::new(freq),
            gain_db: Smoothed::new(0.0),
            q: Smoothed::new(q),
            coeffs: Coeffs::new(kind, freq as f64, q as f64, 0.0, 48_000.0),
        }
    }

    fn update(&mut self, sample_rate: f32) {
        self.coeffs = Coeffs::new(
            self.kind,
            self.freq.value() as f64,
            self.q.value() as f64,
            self.gain_db.value() as f64,
            sample_rate as f64,
        );
    }

    fn settled(&self) -> bool {
        self.freq.is_settled() && self.gain_db.is_settled() && self.q.is_settled()
    }

    /// At exactly 0 dB a shelf or peak is a perfect identity: skip it.
    fn flat(&self) -> bool {
        self.gain_db.is_settled() && self.gain_db.value() == 0.0
    }

    fn advance(&mut self) {
        self.freq.next();
        self.gain_db.next();
        self.q.next();
    }
}

pub struct Eq3 {
    bands: [Band; 3],
    /// [channel][band]
    state: [[State; 3]; 2],
    sample_rate: f32,
    countdown: usize,
}

impl Eq3 {
    pub fn new() -> Self {
        let q = std::f32::consts::FRAC_1_SQRT_2;
        Self {
            bands: [
                Band::new(FilterKind::LowShelf, 120.0, q),
                Band::new(FilterKind::Peak, 1_000.0, 1.0),
                Band::new(FilterKind::HighShelf, 8_000.0, q),
            ],
            state: [[State::default(); 3]; 2],
            sample_rate: 48_000.0,
            countdown: 0,
        }
    }
}

impl Processor for Eq3 {
    fn name(&self) -> &'static str {
        "eq"
    }

    fn param_info(&self) -> &'static [ParamInfo] {
        // A `const`: a reference to an array built by (const) function
        // calls isn't promoted to 'static automatically.
        const PARAMS: &[ParamInfo] = &[
                ParamInfo::log("low_freq", 10.0, 20_000.0, 120.0, "Hz"),
                ParamInfo::linear("low_gain", -24.0, 24.0, 0.0, "dB"),
                ParamInfo::log("mid_freq", 10.0, 20_000.0, 1_000.0, "Hz"),
                ParamInfo::linear("mid_gain", -24.0, 24.0, 0.0, "dB"),
                ParamInfo::log("mid_q", 0.1, 10.0, 1.0, ""),
                ParamInfo::log("high_freq", 10.0, 20_000.0, 8_000.0, "Hz"),
                ParamInfo::linear("high_gain", -24.0, 24.0, 0.0, "dB"),
            ];
        PARAMS
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.sample_rate = info.sample_rate;
        for b in &mut self.bands {
            b.freq.prepare(info.sample_rate, 0.02);
            b.gain_db.prepare(info.sample_rate, 0.02);
            b.q.prepare(info.sample_rate, 0.02);
            b.update(info.sample_rate);
        }
    }

    fn set_param(&mut self, id: usize, value: f32) {
        let max_freq = self.sample_rate * 0.49;
        let freq = value.clamp(10.0, max_freq);
        let gain = value.clamp(-24.0, 24.0);
        match id {
            0 => self.bands[0].freq.set(freq),
            1 => self.bands[0].gain_db.set(gain),
            2 => self.bands[1].freq.set(freq),
            3 => self.bands[1].gain_db.set(gain),
            4 => self.bands[1].q.set(value.clamp(0.1, 10.0)),
            5 => self.bands[2].freq.set(freq),
            6 => self.bands[2].gain_db.set(gain),
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext) {
        let channels = buffer.channels().min(2);

        if self.bands.iter().all(Band::settled) {
            // Which bands do anything? Reset the state of flat ones, so a
            // band that comes back later starts clean instead of releasing
            // a stale value from long ago.
            let active = [!self.bands[0].flat(), !self.bands[1].flat(), !self.bands[2].flat()];
            for ch in 0..2 {
                for b in 0..3 {
                    if !active[b] {
                        self.state[ch][b] = State::default();
                    }
                }
            }
            if !active.iter().any(|&a| a) {
                return; // a flat EQ: nothing to do at all
            }
            let coeffs = [self.bands[0].coeffs, self.bands[1].coeffs, self.bands[2].coeffs];
            for ch in 0..channels {
                let mut st = self.state[ch];
                for x in buffer.channel_mut(ch) {
                    let mut v = *x as f64;
                    for b in 0..3 {
                        if active[b] {
                            v = st[b].tick(&coeffs[b], v);
                        }
                    }
                    *x = v as f32;
                }
                self.state[ch] = st;
            }
            return;
        }

        // Something moves: advance every band's smoothers per sample and
        // recompute coefficients at control rate.
        for i in 0..buffer.frames() {
            for b in &mut self.bands {
                b.advance();
            }
            if self.countdown == 0 {
                for b in &mut self.bands {
                    b.update(self.sample_rate);
                }
                self.countdown = CONTROL_RATE;
            }
            self.countdown -= 1;
            for ch in 0..channels {
                let mut v = buffer.channel(ch)[i] as f64;
                for b in 0..3 {
                    v = self.state[ch][b].tick(&self.bands[b].coeffs, v);
                }
                buffer.channel_mut(ch)[i] = v as f32;
            }
        }
        if self.bands.iter().all(Band::settled) {
            for b in &mut self.bands {
                b.update(self.sample_rate);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Biquad;
    use super::*;
    use crate::transport::Transport;
    use std::f64::consts::TAU;

    const SR: f32 = 48_000.0;
    const INFO: PrepareInfo = PrepareInfo { sample_rate: SR, max_frames: 48_000, channels: 2 };

    fn settle(node: &mut dyn Processor) {
        let mut buf = AudioBuffer::new(2, 48_000);
        buf.clear();
        node.process(&mut buf, &Transport::new(SR as f64).context());
    }

    fn sine(freq: f64) -> AudioBuffer {
        let mut buf = AudioBuffer::new(2, 48_000);
        for ch in 0..2 {
            for (i, s) in buf.channel_mut(ch).iter_mut().enumerate() {
                *s = (TAU * freq * i as f64 / SR as f64).sin() as f32;
            }
        }
        buf
    }

    fn gain_db(node: &mut dyn Processor, freq: f64) -> f64 {
        let mut buf = sine(freq);
        node.process(&mut buf, &Transport::new(SR as f64).context());
        let tail = &buf.channel(0)[24_000..];
        let rms = (tail.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        20.0 * (rms * 2f64.sqrt()).log10()
    }

    fn eq(low: f32, mid: f32, high: f32) -> Eq3 {
        let mut e = Eq3::new();
        e.prepare(&INFO);
        e.set_param(1, low);
        e.set_param(3, mid);
        e.set_param(6, high);
        settle(&mut e);
        e
    }

    #[test]
    fn flat_eq_is_a_perfect_bypass() {
        let mut e = eq(0.0, 0.0, 0.0);
        let mut buf = sine(440.0);
        let before = buf.channel(0).to_vec();
        e.process(&mut buf, &Transport::new(SR as f64).context());
        assert_eq!(buf.channel(0), &before[..], "bit-identical");
    }

    #[test]
    fn bands_shape_their_regions() {
        let mut e = eq(6.0, -9.0, 4.0);
        assert!((gain_db(&mut e, 30.0) - 6.0).abs() < 0.3, "low shelf");
        assert!((gain_db(&mut e, 1_000.0) - -9.0).abs() < 0.3, "mid peak");
        assert!((gain_db(&mut e, 18_000.0) - 4.0).abs() < 0.5, "high shelf");
    }

    #[test]
    fn matches_three_separate_biquads() {
        let mut e = eq(5.0, -7.0, 3.0);
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let mut chain: Vec<Biquad> = [(5.0, 120.0, q, 5.0), (4.0, 1_000.0, 1.0, -7.0), (6.0, 8_000.0, q, 3.0)]
            .iter()
            .map(|&(kind, freq, q, gain)| {
                let mut b = Biquad::new();
                b.prepare(&INFO);
                b.set_param(0, kind);
                b.set_param(1, freq);
                b.set_param(2, q);
                b.set_param(3, gain);
                settle(&mut b);
                b
            })
            .collect();
        let ctx = Transport::new(SR as f64).context();
        let mut a = sine(300.0);
        let mut b = sine(300.0);
        e.process(&mut a, &ctx);
        for f in &mut chain {
            f.process(&mut b, &ctx);
        }
        // Not bit-identical: the separate nodes round to f32 between stages.
        let worst = a.channel(0).iter().zip(b.channel(0)).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "max difference {worst}");
    }
}
