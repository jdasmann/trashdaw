//! A feed-forward compressor: when the signal gets louder than the
//! threshold, turn it down by an amount set by the ratio.
//!
//! The design follows Giannoulis, Massberg & Reiss, "Digital Dynamic Range
//! Compressor Design: A Tutorial and Analysis" (JAES, 2012): measure the
//! level, compute the desired gain from a static curve, THEN smooth that
//! gain with separate attack and release times, all in decibels.

use super::db_to_linear;
use crate::buffer::AudioBuffer;
use crate::processor::{ParamInfo, PrepareInfo, Processor};
use crate::smoothing::Smoothed;
use crate::transport::ProcessContext;

pub struct Compressor {
    sample_rate: f32,
    threshold: f32,
    ratio: f32,
    knee: f32,
    attack_ms: f32,
    release_ms: f32,
    attack_coeff: f32,
    release_coeff: f32,
    makeup: Smoothed,
    /// The smoothed gain change in dB (0 or negative). This IS the
    /// compressor's state: how hard it is currently turning down.
    gain_db: f32,
}

impl Compressor {
    pub fn new() -> Self {
        let mut c = Self {
            sample_rate: 48_000.0,
            threshold: -18.0,
            ratio: 4.0,
            knee: 6.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            attack_coeff: 0.0,
            release_coeff: 0.0,
            makeup: Smoothed::new(0.0),
            gain_db: 0.0,
        };
        c.update_times();
        c
    }

    /// One-pole coefficient for a time constant: after `ms`, the gain has
    /// moved 63% of the way to its target.
    fn coeff(&self, ms: f32) -> f32 {
        (-1.0 / (ms.max(0.01) * 0.001 * self.sample_rate)).exp()
    }

    fn update_times(&mut self) {
        self.attack_coeff = self.coeff(self.attack_ms);
        self.release_coeff = self.coeff(self.release_ms);
    }

    /// The static curve: input level in dB -> desired output level in dB.
    /// Below the threshold, unchanged; above it, every `ratio` dB in gives
    /// 1 dB out. The soft knee blends the two over `knee` dB, so the
    /// compressor eases in instead of switching on at an exact level.
    fn curve(&self, x: f32) -> f32 {
        let over = x - self.threshold;
        if 2.0 * over < -self.knee {
            x
        } else if 2.0 * over.abs() <= self.knee {
            let k = over + self.knee / 2.0;
            x + (1.0 / self.ratio - 1.0) * k * k / (2.0 * self.knee)
        } else {
            self.threshold + over / self.ratio
        }
    }
}

impl Processor for Compressor {
    fn name(&self) -> &'static str {
        "comp"
    }

    /// threshold dB; ratio (4 = 4:1); attack/release in ms; knee dB;
    /// makeup gain dB.
    fn param_info(&self) -> &'static [ParamInfo] {
        // A `const`: a reference to an array built by (const) function
        // calls isn't promoted to 'static automatically.
        const PARAMS: &[ParamInfo] = &[
                ParamInfo::linear("threshold", -60.0, 0.0, -18.0, "dB"),
                ParamInfo::log("ratio", 1.0, 100.0, 4.0, ":1"),
                ParamInfo::log("attack", 0.01, 500.0, 10.0, "ms"),
                ParamInfo::log("release", 1.0, 5_000.0, 100.0, "ms"),
                ParamInfo::linear("knee", 0.0, 24.0, 6.0, "dB"),
                ParamInfo::linear("makeup", -24.0, 24.0, 0.0, "dB"),
            ];
        PARAMS
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.sample_rate = info.sample_rate;
        self.makeup.prepare(info.sample_rate, 0.01);
        self.update_times();
    }

    fn set_param(&mut self, id: usize, value: f32) {
        match id {
            // Threshold/ratio/knee can jump freely: the gain they produce
            // still goes through the attack/release smoothing below.
            0 => self.threshold = value.clamp(-60.0, 0.0),
            1 => self.ratio = value.clamp(1.0, 100.0),
            2 => {
                self.attack_ms = value.clamp(0.01, 500.0);
                self.update_times();
            }
            3 => {
                self.release_ms = value.clamp(1.0, 5_000.0);
                self.update_times();
            }
            4 => self.knee = value.clamp(0.0, 24.0),
            // Makeup is applied directly, so it needs its own smoothing.
            5 => self.makeup.set(value.clamp(-24.0, 24.0)),
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext) {
        self.run(buffer, None);
    }

    /// With a sidechain, the DETECTOR listens to the key while the GAIN
    /// still applies to this track: the kick hits, the bass ducks.
    fn process_keyed(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext, key: Option<&AudioBuffer>) {
        let key = key.filter(|k| k.channels() >= 2 && k.frames() == buffer.frames());
        self.run(buffer, key.map(|k| k.stereo()));
    }
}

impl Compressor {
    fn run(&mut self, buffer: &mut AudioBuffer, key: Option<(&[f32], &[f32])>) {
        if buffer.channels() < 2 {
            return;
        }
        let (l, r) = buffer.stereo_mut();
        for (i, (a, b)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
            // Stereo-linked detection: both channels get the same gain, so
            // compressing a hit on one side doesn't shift the stereo image.
            let level = match key {
                Some((kl, kr)) => kl[i].abs().max(kr[i].abs()),
                None => a.abs().max(b.abs()),
            }
            .max(1e-6); // floor at -120 dB
            let x_db = 20.0 * level.log10();
            let target = self.curve(x_db) - x_db; // 0 or negative

            // The compressor's own smoothing, and its whole character.
            // Attack when the gain must go DOWN (getting louder), release
            // when it may come back up. Without it the gain would follow
            // every waveform cycle, which is distortion, not compression.
            let coeff = if target < self.gain_db { self.attack_coeff } else { self.release_coeff };
            self.gain_db = coeff * self.gain_db + (1.0 - coeff) * target;

            let g = db_to_linear(self.gain_db + self.makeup.next());
            *a *= g;
            *b *= g;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    const SR: f32 = 48_000.0;

    fn comp() -> Compressor {
        let mut c = Compressor::new();
        c.prepare(&PrepareInfo { sample_rate: SR, max_frames: 48_000, channels: 2 });
        c
    }

    /// Feeds a constant level (DC, so the detector sees it exactly).
    fn run_dc(c: &mut Compressor, db: f32, frames: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(2, frames);
        let v = 10f32.powf(db / 20.0);
        buf.channel_mut(0).fill(v);
        buf.channel_mut(1).fill(v);
        c.process(&mut buf, &Transport::new(SR as f64).context());
        buf.channel(0).iter().map(|x| 20.0 * x.abs().log10()).collect()
    }

    #[test]
    fn static_curve() {
        let mut c = comp();
        c.knee = 0.0; // hard knee: exact textbook numbers
        // 12 dB over a -18 dB threshold at 4:1 comes out 3 dB over.
        let out = run_dc(&mut c, -6.0, 48_000);
        assert!((out.last().unwrap() - -15.0).abs() < 0.01, "{}", out.last().unwrap());

        let mut c = comp();
        let out = run_dc(&mut c, -30.0, 48_000);
        assert!((out.last().unwrap() - -30.0).abs() < 1e-4, "below threshold: untouched");
    }

    #[test]
    fn soft_knee_is_continuous() {
        let c = comp();
        let mut last = c.curve(-40.0);
        for i in 1..=400 {
            let x = -40.0 + i as f32 * 0.1;
            let y = c.curve(x);
            assert!((y - last).abs() < 0.11, "jump at {x} dB");
            assert!(y <= x + 1e-5, "never boosts");
            last = y;
        }
    }

    #[test]
    fn attack_time_is_the_time_constant() {
        let mut c = comp();
        c.knee = 0.0;
        c.set_param(2, 10.0); // 10 ms attack = 480 samples
        let out = run_dc(&mut c, -6.0, 4_800);
        // The full reduction is 9 dB; after one time constant, 63% of it.
        let reduction_at = |i: usize| -6.0 - out[i];
        assert!((reduction_at(480) - 9.0 * 0.632).abs() < 0.1, "{}", reduction_at(480));
        assert!((reduction_at(4_799) - 9.0).abs() < 0.05);
    }

    #[test]
    fn release_lets_go_slowly() {
        let mut c = comp();
        c.knee = 0.0;
        c.set_param(3, 100.0); // 100 ms release = 4800 samples
        run_dc(&mut c, -6.0, 48_000); // fully compressed: -9 dB
        let out = run_dc(&mut c, -30.0, 4_800);
        // Signal dropped below threshold; the gain recovers over ~100 ms,
        // so right after the drop the quiet signal is still turned down.
        assert!(out[10] < -38.0, "{}", out[10]);
        assert!((out[4_799] - (-30.0 - 9.0 * (-1.0f32).exp())).abs() < 0.1, "{}", out[4_799]);
    }

    #[test]
    fn makeup_gain_applies() {
        let mut c = comp();
        c.knee = 0.0;
        c.makeup = Smoothed::new(9.0);
        let out = run_dc(&mut c, -6.0, 48_000);
        assert!((out.last().unwrap() - -6.0).abs() < 0.01, "9 dB makeup undoes 9 dB of reduction");
    }
}
