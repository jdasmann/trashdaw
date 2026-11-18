use crate::buffer::AudioBuffer;
use crate::processor::{ParamInfo, PrepareInfo, Processor};
use crate::transport::ProcessContext;
use std::f32::consts::TAU;

/// Clicks on every beat, higher on the first beat of each bar.
/// `beats` sets how many beats make a bar (3 for waltz time, etc.).
/// The point of this node: it reads the transport and places each click
/// on the exact sample where the beat falls.
pub struct Metronome {
    sample_rate: f32,
    level: f32,
    beats_per_bar: u64,
    // Click voice state
    phase: f32,
    freq: f32,
    env: f32,
    decay: f32,
}

impl Metronome {
    pub fn new() -> Self {
        Self {
            sample_rate: 48_000.0,
            level: 0.3,
            beats_per_bar: 4,
            phase: 0.0,
            freq: 880.0,
            env: 0.0,
            decay: 0.0,
        }
    }
}

impl Processor for Metronome {
    fn name(&self) -> &'static str {
        "click"
    }

    fn param_info(&self) -> &'static [ParamInfo] {
        // A `const`: a reference to an array built by (const) function
        // calls isn't promoted to 'static automatically.
        const PARAMS: &[ParamInfo] = &[ParamInfo::linear("level", 0.0, 1.0, 0.3, ""), ParamInfo::int("beats", 1.0, 16.0, 4.0)];
        PARAMS
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.sample_rate = info.sample_rate;
        // Envelope falls to 0.001 (-60 dB) over 30 ms.
        self.decay = (0.001f32.ln() / (0.030 * info.sample_rate)).exp();
    }

    fn set_param(&mut self, id: usize, value: f32) {
        match id {
            // Not smoothed: it only scales short clicks, so jumps are inaudible.
            0 => self.level = value.clamp(0.0, 1.0),
            1 => self.beats_per_bar = value.round().clamp(1.0, 16.0) as u64,
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext) {
        let beats_per_sample = 1.0 / ctx.samples_per_beat();

        for i in 0..buffer.frames() {
            if ctx.playing {
                let beat = ctx.beat_at(i);
                // A beat boundary falls somewhere between the previous
                // sample and this one exactly when the fractional part is
                // smaller than one sample's worth of beats. Stateless, so
                // it also behaves correctly after seeks and loop wraps.
                if beat.fract() < beats_per_sample {
                    let beat_index = beat.floor() as u64;
                    let downbeat = beat_index % self.beats_per_bar == 0;
                    self.freq = if downbeat { 1760.0 } else { 880.0 };
                    self.phase = 0.0;
                    self.env = 1.0;
                }
            }

            // Keep ringing out even after stop, so clicks never cut off.
            if self.env > 1e-4 {
                let s = (self.phase * TAU).sin() * self.env * self.level;
                self.phase += self.freq / self.sample_rate;
                if self.phase >= 1.0 {
                    self.phase -= 1.0;
                }
                self.env *= self.decay;
                for ch in 0..buffer.channels() {
                    buffer.channel_mut(ch)[i] += s;
                }
            }
        }
    }
}
