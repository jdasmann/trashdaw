//! A feedback delay (echo), optionally synced to the tempo.

use crate::buffer::AudioBuffer;
use crate::processor::{ParamInfo, PrepareInfo, Processor};
use crate::smoothing::Smoothed;
use crate::transport::ProcessContext;
use std::f32::consts::TAU;

const MAX_SECONDS: f32 = 4.0;

/// A delay line is a ring buffer: write the newest sample at `write`, read
/// an older one `delay` samples behind it. Its length is a power of two, so
/// wrapping around is a bit-mask instead of a division.
struct Line {
    buf: Vec<f32>,
    mask: usize,
}

impl Line {
    fn new(min_len: usize) -> Self {
        let len = min_len.next_power_of_two();
        Self { buf: vec![0.0; len], mask: len - 1 }
    }

    /// The sample `delay` samples before `write`. Delay times fall between
    /// samples, so interpolate linearly between the two neighbors.
    #[inline(always)]
    fn read(&self, write: usize, delay: f32) -> f32 {
        let whole = delay as usize;
        let frac = delay - whole as f32;
        let newer = self.buf[write.wrapping_sub(whole) & self.mask];
        let older = self.buf[write.wrapping_sub(whole + 1) & self.mask];
        newer + (older - newer) * frac
    }
}

pub struct Delay {
    lines: Vec<Line>,
    write: usize,
    sample_rate: f32,
    ms: f32,
    /// If above zero, the delay time is this many beats (tempo sync).
    beats: f32,
    /// In samples. Smoothed, so changing the time glides (a tape-style
    /// pitch bend) instead of jumping (a click).
    time: Smoothed,
    feedback: Smoothed,
    mix: Smoothed,
    /// One-pole lowpass in the feedback path: each repeat gets darker,
    /// like analog and tape echoes.
    damp_coeff: f32,
    damp_state: [f32; 2],
    /// Echoes alternate left and right.
    ping_pong: bool,
}

impl Delay {
    pub fn new() -> Self {
        let mut time = Smoothed::new(0.0);
        time.set(0.0);
        Self {
            lines: Vec::new(),
            write: 0,
            sample_rate: 48_000.0,
            ms: 250.0,
            beats: 0.0,
            time,
            feedback: Smoothed::new(0.4),
            mix: Smoothed::new(0.3),
            damp_coeff: 1.0,
            damp_state: [0.0; 2],
            ping_pong: false,
        }
    }

    fn max_delay(&self) -> f32 {
        // Leave room for the interpolation's extra sample.
        self.lines.first().map_or(0.0, |l| (l.buf.len() - 2) as f32)
    }

    fn set_damp(&mut self, damp: f32) {
        let damp = damp.clamp(0.0, 1.0);
        if damp == 0.0 {
            // Truly off. Even a "20 kHz" one-pole noticeably softens each
            // repeat at 48 kHz, so 0 must bypass the filter entirely.
            self.damp_coeff = 1.0;
            return;
        }
        // From 16 kHz down to 500 Hz, on a log scale (how pitch is heard).
        let cutoff = 16_000.0 * (500.0f32 / 16_000.0).powf(damp);
        self.damp_coeff = 1.0 - (-TAU * cutoff / self.sample_rate).exp();
    }
}

impl Processor for Delay {
    fn name(&self) -> &'static str {
        "delay"
    }

    /// ms: delay time; beats: tempo-synced time (0 = use ms);
    /// feedback 0..0.95; damp 0..1; mix 0..1 (dry/wet); pingpong 0/1.
    fn param_info(&self) -> &'static [ParamInfo] {
        // A `const`: a reference to an array built by (const) function
        // calls isn't promoted to 'static automatically.
        const PARAMS: &[ParamInfo] = &[
                ParamInfo::log("ms", 1.0, 4_000.0, 250.0, "ms"),
                ParamInfo::linear("beats", 0.0, 16.0, 0.0, "beats"),
                ParamInfo::linear("feedback", 0.0, 0.95, 0.4, ""),
                ParamInfo::linear("damp", 0.0, 1.0, 0.3, ""),
                ParamInfo::linear("mix", 0.0, 1.0, 0.3, ""),
                ParamInfo::toggle("pingpong", false),
            ];
        PARAMS
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.sample_rate = info.sample_rate;
        let len = (MAX_SECONDS * info.sample_rate) as usize + 2;
        self.lines = (0..info.channels).map(|_| Line::new(len)).collect();
        self.time.prepare(info.sample_rate, 0.05);
        self.feedback.prepare(info.sample_rate, 0.01);
        self.mix.prepare(info.sample_rate, 0.01);
        self.set_damp(0.3);
        // Start at the configured time rather than gliding up from zero.
        self.time = Smoothed::new(self.ms * info.sample_rate / 1000.0);
        self.time.prepare(info.sample_rate, 0.05);
    }

    fn set_param(&mut self, id: usize, value: f32) {
        match id {
            0 => self.ms = value.clamp(1.0, MAX_SECONDS * 1000.0),
            1 => self.beats = value.clamp(0.0, 16.0),
            // Feedback at or above 1 would grow forever.
            2 => self.feedback.set(value.clamp(0.0, 0.95)),
            3 => self.set_damp(value),
            4 => self.mix.set(value.clamp(0.0, 1.0)),
            5 => self.ping_pong = value >= 0.5,
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext) {
        if self.lines.is_empty() {
            return;
        }
        // The target time is decided once per block. With tempo sync it
        // follows tempo changes automatically.
        let target = if self.beats > 0.0 {
            self.beats * ctx.samples_per_beat() as f32
        } else {
            self.ms * self.sample_rate / 1000.0
        };
        self.time.set(target.clamp(1.0, self.max_delay()));

        if buffer.channels() < 2 || self.lines.len() < 2 {
            return;
        }
        let (line_l, line_r) = self.lines.split_at_mut(1);
        let (line_l, line_r) = (&mut line_l[0], &mut line_r[0]);
        let mask = line_l.mask;
        let (out_l, out_r) = buffer.stereo_mut();
        for (l, r) in out_l.iter_mut().zip(out_r.iter_mut()) {
            let delay = self.time.next();
            let feedback = self.feedback.next();
            let mix = self.mix.next();
            let (x_l, x_r) = (*l, *r);

            let wet_l = line_l.read(self.write, delay);
            let wet_r = line_r.read(self.write, delay);
            let [d_l, d_r] = &mut self.damp_state;
            *d_l += self.damp_coeff * (wet_l - *d_l);
            *d_r += self.damp_coeff * (wet_r - *d_r);

            let (into_l, into_r) = if self.ping_pong {
                // The input goes in on the LEFT only, and each side's
                // feedback goes into the OTHER side: left, right, left...
                // It's summed to mono first: a centered sound fed into both
                // lines would echo on both sides at once, and swapping two
                // identical signals changes nothing, so nothing would bounce.
                ((x_l + x_r) * 0.5 + *d_r * feedback, *d_l * feedback)
            } else {
                (x_l + *d_l * feedback, x_r + *d_r * feedback)
            };
            line_l.buf[self.write] = into_l;
            line_r.buf[self.write] = into_r;

            *l = x_l + (wet_l - x_l) * mix;
            *r = x_r + (wet_r - x_r) * mix;
            self.write = (self.write + 1) & mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    const SR: f32 = 48_000.0;

    fn delay() -> Delay {
        let mut d = Delay::new();
        d.prepare(&PrepareInfo { sample_rate: SR, max_frames: 48_000, channels: 2 });
        d
    }

    /// Renders an impulse at frame 0 and returns channel 0.
    fn impulse_response(d: &mut Delay, ctx: &ProcessContext, frames: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(2, frames);
        buf.clear();
        buf.channel_mut(0)[0] = 1.0;
        d.process(&mut buf, ctx);
        buf.channel(0).to_vec()
    }

    #[test]
    fn echoes_arrive_on_time_and_decay_by_the_feedback() {
        let mut d = delay();
        d.set_param(0, 10.0); // 10 ms = 480 samples
        d.prepare(&PrepareInfo { sample_rate: SR, max_frames: 48_000, channels: 2 });
        d.set_param(2, 0.5);
        d.set_param(3, 0.0); // no damping
        d.mix = Smoothed::new(1.0); // wet only
        d.feedback = Smoothed::new(0.5);
        let out = impulse_response(&mut d, &Transport::new(SR as f64).context(), 2_000);
        let peaks: Vec<usize> = (0..out.len()).filter(|&i| out[i].abs() > 0.05).collect();
        assert_eq!(peaks, vec![480, 960, 1440, 1920]);
        // Each repeat at half the level of the one before.
        for (k, &at) in peaks.iter().enumerate() {
            assert!((out[at] - 0.5f32.powi(k as i32)).abs() < 1e-6, "echo {k}: {}", out[at]);
        }
        assert!(out.iter().enumerate().all(|(i, x)| peaks.contains(&i) || x.abs() < 1e-6), "nothing between echoes");
    }

    #[test]
    fn tempo_sync_follows_the_beat() {
        let mut d = delay();
        d.set_param(1, 0.25); // a sixteenth note
        d.mix = Smoothed::new(1.0);
        d.feedback = Smoothed::new(0.0);
        let mut t = Transport::new(SR as f64); // 120 BPM: 24_000 samples/beat
        // Let the time glide to the synced value first.
        let mut warm = AudioBuffer::new(2, 48_000);
        warm.clear();
        d.process(&mut warm, &t.context());
        t.advance(48_000);
        let out = impulse_response(&mut d, &t.context(), 7_000);
        let echo = out.iter().position(|x| x.abs() > 0.5);
        assert_eq!(echo, Some(6_000));
    }

    #[test]
    fn damping_darkens_each_repeat() {
        let mut d = delay();
        d.set_param(0, 10.0);
        d.prepare(&PrepareInfo { sample_rate: SR, max_frames: 48_000, channels: 2 });
        d.set_param(3, 0.8);
        d.mix = Smoothed::new(1.0);
        d.feedback = Smoothed::new(0.9);
        let out = impulse_response(&mut d, &Transport::new(SR as f64).context(), 3_000);
        // A lowpassed impulse spreads out: the repeat's peak is lower and
        // its energy is smeared over neighboring samples.
        let peak = |c: usize| out[c - 20..c + 60].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak(960) < 0.9 * peak(480));
    }

    #[test]
    fn ping_pong_alternates_sides() {
        let mut d = delay();
        d.set_param(0, 10.0); // 480 samples
        d.prepare(&PrepareInfo { sample_rate: SR, max_frames: 48_000, channels: 2 });
        d.set_param(3, 0.0);
        d.set_param(5, 1.0);
        d.mix = Smoothed::new(1.0);
        d.feedback = Smoothed::new(0.5);
        // A CENTERED impulse: both channels at once.
        let mut buf = AudioBuffer::new(2, 2_000);
        buf.clear();
        buf.channel_mut(0)[0] = 1.0;
        buf.channel_mut(1)[0] = 1.0;
        d.process(&mut buf, &Transport::new(SR as f64).context());
        let (l, r) = buf.stereo();
        let hits = |x: &[f32]| (0..x.len()).filter(|&i| x[i].abs() > 0.05).collect::<Vec<_>>();
        assert_eq!(hits(l), vec![480, 1440], "left: 1st and 3rd echoes");
        assert_eq!(hits(r), vec![960, 1920], "right: 2nd and 4th");
        assert!((l[480] - 1.0).abs() < 1e-6 && (r[960] - 0.5).abs() < 1e-6);
        assert!((l[1440] - 0.25).abs() < 1e-6 && (r[1920] - 0.125).abs() < 1e-6);
    }
}
