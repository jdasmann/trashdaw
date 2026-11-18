use crate::buffer::AudioBuffer;
use crate::processor::{PrepareInfo, Processor};
use crate::transport::ProcessContext;
use crate::smoothing::Smoothed;
use std::f32::consts::TAU;

#[derive(Clone, Copy)]
enum Waveform {
    Sine,
    Saw,
    Square,
}

pub struct Oscillator {
    phase: f32,
    freq: f32,
    sample_rate: f32,
    waveform: Waveform,
    level: Smoothed,
}

impl Oscillator {
    pub fn new() -> Self {
        let mut level = Smoothed::new(0.0); // fade in from silence
        level.set(0.2);
        Self {
            phase: 0.0,
            freq: 440.0,
            sample_rate: 48_000.0,
            waveform: Waveform::Sine,
            level,
        }
    }
}

impl Processor for Oscillator {
    fn name(&self) -> &'static str {
        "osc"
    }

    fn params(&self) -> &'static [&'static str] {
        &["freq", "level", "wave"]
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.sample_rate = info.sample_rate;
        self.level.prepare(info.sample_rate, 0.01);
    }

    fn set_param(&mut self, id: usize, value: f32) {
        match id {
            0 => self.freq = value.clamp(1.0, self.sample_rate * 0.5),
            1 => self.level.set(value.clamp(0.0, 1.0)),
            2 => {
                self.waveform = match value as u32 {
                    0 => Waveform::Sine,
                    1 => Waveform::Saw,
                    _ => Waveform::Square,
                }
            }
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext) {
        let increment = self.freq / self.sample_rate;

        for i in 0..buffer.frames() {
            // Naive saw/square alias at high pitches; fixed in step 7.
            let raw = match self.waveform {
                Waveform::Sine => (self.phase * TAU).sin(),
                Waveform::Saw => 2.0 * self.phase - 1.0,
                Waveform::Square => if self.phase < 0.5 { 1.0 } else { -1.0 },
            };
            let s = raw * self.level.next();

            self.phase += increment;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }

            for ch in 0..buffer.channels() {
                buffer.channel_mut(ch)[i] += s;
            }
        }
    }
}
