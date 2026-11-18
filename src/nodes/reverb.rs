//! An algorithmic reverb in the Schroeder-Moorer style, laid out like the
//! classic public-domain "Freeverb": 8 parallel comb filters per channel
//! build up a dense, decaying echo pattern, then 4 allpass filters in
//! series smear it so individual echoes blur into a smooth tail.

use crate::buffer::AudioBuffer;
use crate::processor::{PrepareInfo, Processor};
use crate::smoothing::Smoothed;
use crate::transport::ProcessContext;

/// Delay lengths in samples at 44.1 kHz (scaled to the real rate). They
/// are chosen to share no common factors, so the echoes of different combs
/// rarely line up, which would sound metallic.
const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];
/// The right channel's delays are slightly longer, which decorrelates
/// left and right: that difference is what makes a reverb sound wide.
const STEREO_SPREAD: usize = 23;
/// Keeps the summed combs from clipping.
const INPUT_GAIN: f32 = 0.015;

/// A feedback comb with a lowpass in its loop. The loop gain sets how long
/// the tail lasts; the lowpass makes high frequencies die faster, the way
/// air and soft surfaces absorb them in a real room.
struct Comb {
    buf: Vec<f32>,
    pos: usize,
    filtered: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)], pos: 0, filtered: 0.0 }
    }

    #[inline(always)]
    fn tick(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.pos];
        self.filtered = out + (self.filtered - out) * damp;
        self.buf[self.pos] = input + self.filtered * feedback;
        self.pos += 1;
        if self.pos == self.buf.len() {
            self.pos = 0;
        }
        out
    }
}

/// An allpass passes every frequency at equal level but scrambles phase,
/// which diffuses the comb echoes without coloring the tone.
struct Allpass {
    buf: Vec<f32>,
    pos: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)], pos: 0 }
    }

    #[inline(always)]
    fn tick(&mut self, input: f32) -> f32 {
        let delayed = self.buf[self.pos];
        self.buf[self.pos] = input + delayed * 0.5;
        self.pos += 1;
        if self.pos == self.buf.len() {
            self.pos = 0;
        }
        delayed - input
    }
}

struct Channel {
    combs: Vec<Comb>,
    allpasses: Vec<Allpass>,
}

impl Channel {
    fn new(scale: f32, extra: usize) -> Self {
        let len = |n: usize| ((n + extra) as f32 * scale).round() as usize;
        Self {
            combs: COMB_TUNING.iter().map(|&n| Comb::new(len(n))).collect(),
            allpasses: ALLPASS_TUNING.iter().map(|&n| Allpass::new(len(n))).collect(),
        }
    }

    #[inline(always)]
    fn tick(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let mut out = 0.0;
        for c in &mut self.combs {
            out += c.tick(input, feedback, damp);
        }
        for a in &mut self.allpasses {
            out = a.tick(out);
        }
        out
    }
}

pub struct Reverb {
    channels: Vec<Channel>,
    size: f32,
    damp: f32,
    width: Smoothed,
    mix: Smoothed,
}

impl Reverb {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
            size: 0.5,
            damp: 0.5,
            width: Smoothed::new(1.0),
            mix: Smoothed::new(0.25),
        }
    }

    /// Room size 0..1 maps to comb feedback 0.7..0.98: how much of each
    /// echo survives a trip around the loop.
    fn feedback(&self) -> f32 {
        0.7 + self.size * 0.28
    }
}

impl Processor for Reverb {
    fn name(&self) -> &'static str {
        "reverb"
    }

    /// size, damp, width, mix: all 0..1.
    fn params(&self) -> &'static [&'static str] {
        &["size", "damp", "width", "mix"]
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        let scale = info.sample_rate / 44_100.0;
        // All twelve delay lines per channel are allocated here, once.
        self.channels = vec![Channel::new(scale, 0), Channel::new(scale, STEREO_SPREAD)];
        self.width.prepare(info.sample_rate, 0.01);
        self.mix.prepare(info.sample_rate, 0.01);
    }

    fn set_param(&mut self, id: usize, value: f32) {
        let v = value.clamp(0.0, 1.0);
        match id {
            // Size and damp change loop gains, which already change slowly
            // inside the filters, so they don't click.
            0 => self.size = v,
            1 => self.damp = v,
            2 => self.width.set(v),
            3 => self.mix.set(v),
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext) {
        if self.channels.len() < 2 || buffer.channels() < 2 {
            return;
        }
        let feedback = self.feedback();
        let damp = self.damp * 0.4;
        let (left, right) = self.channels.split_at_mut(1);
        let (l_ch, r_ch) = (&mut left[0], &mut right[0]);
        let (l, r) = buffer.stereo_mut();
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            // Both sides are fed the same mono sum; their different delay
            // lengths make them diverge.
            let input = (*a + *b) * INPUT_GAIN;
            let wet_l = l_ch.tick(input, feedback, damp);
            let wet_r = r_ch.tick(input, feedback, damp);

            // Width blends the two tails: 1 = fully separate, 0 = mono.
            let width = self.width.next();
            let same = 0.5 + width / 2.0;
            let cross = (1.0 - width) / 2.0;
            let mix = self.mix.next();
            // A gain of 3 on the wet signal brings the tail to roughly
            // the dry level at mix = 0.5.
            let out_l = (wet_l * same + wet_r * cross) * 3.0;
            let out_r = (wet_r * same + wet_l * cross) * 3.0;
            *a += (out_l - *a) * mix;
            *b += (out_r - *b) * mix;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    const SR: f32 = 48_000.0;

    fn reverb(size: f32) -> Reverb {
        let mut r = Reverb::new();
        r.prepare(&PrepareInfo { sample_rate: SR, max_frames: 48_000, channels: 2 });
        r.set_param(0, size);
        r.mix = Smoothed::new(1.0);
        r
    }

    /// Energy in each 100 ms window after an impulse.
    fn decay(r: &mut Reverb, seconds: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let ctx = Transport::new(SR as f64).context();
        let mut energy = Vec::new();
        let (mut all_l, mut all_r) = (Vec::new(), Vec::new());
        for block in 0..seconds * 10 {
            let mut buf = AudioBuffer::new(2, 4_800);
            buf.clear();
            if block == 0 {
                buf.channel_mut(0)[0] = 1.0;
                buf.channel_mut(1)[0] = 1.0;
            }
            r.process(&mut buf, &ctx);
            let (l, rr) = buf.stereo();
            assert!(l.iter().chain(rr).all(|x| x.is_finite()));
            energy.push(l.iter().chain(rr).map(|x| x * x).sum::<f32>());
            all_l.extend_from_slice(l);
            all_r.extend_from_slice(rr);
        }
        (energy, all_l, all_r)
    }

    #[test]
    fn tail_decays_away() {
        let (energy, ..) = decay(&mut reverb(0.5), 6);
        let peak = energy.iter().cloned().fold(0.0f32, f32::max);
        assert!(peak > 0.0);
        assert!(*energy.last().unwrap() < peak * 1e-6, "still ringing after 6 s");
    }

    /// Seconds for the energy to fall 60 dB below its peak (RT60).
    fn rt60(size: f32) -> f32 {
        let (energy, ..) = decay(&mut reverb(size), 12);
        let peak = energy.iter().cloned().fold(0.0f32, f32::max);
        let i = energy.iter().position(|&e| e < peak * 1e-6).unwrap_or(energy.len());
        i as f32 * 0.1
    }

    #[test]
    fn bigger_rooms_ring_longer() {
        let (small, large) = (rt60(0.2), rt60(0.9));
        assert!(large > 2.0 * small, "small {small} s, large {large} s");
    }

    #[test]
    fn the_tail_is_stereo() {
        let (_, l, r) = decay(&mut reverb(0.5), 1);
        // Correlation of left and right: 1 would be mono.
        let dot: f32 = l.iter().zip(&r).map(|(a, b)| a * b).sum();
        let norm = (l.iter().map(|a| a * a).sum::<f32>() * r.iter().map(|b| b * b).sum::<f32>()).sqrt();
        assert!(dot / norm < 0.5, "correlation {}", dot / norm);
    }

    #[test]
    fn mix_zero_is_dry() {
        let mut r = reverb(0.5);
        r.mix = Smoothed::new(0.0);
        let mut buf = AudioBuffer::new(2, 1000);
        for (i, s) in buf.channel_mut(0).iter_mut().enumerate() {
            *s = (i as f32 * 0.01).sin();
        }
        let before = buf.channel(0).to_vec();
        r.process(&mut buf, &Transport::new(SR as f64).context());
        assert_eq!(buf.channel(0), &before[..]);
    }
}
