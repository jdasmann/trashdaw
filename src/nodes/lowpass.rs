use crate::buffer::AudioBuffer;
use crate::processor::{ParamInfo, PrepareInfo, Processor};
use crate::transport::ProcessContext;
use crate::smoothing::Smoothed;
use std::f32::consts::TAU;

/// A one-pole lowpass filter: gentle 6 dB/octave slope.
/// Its interesting property for us: it keeps state per channel.
pub struct Lowpass {
    cutoff: Smoothed,
    sample_rate: f32,
    state: Vec<f32>, // allocated in prepare, never on the audio thread
}

impl Lowpass {
    pub fn new() -> Self {
        Self {
            cutoff: Smoothed::new(20_000.0),
            sample_rate: 48_000.0,
            state: Vec::new(),
        }
    }
}

impl Processor for Lowpass {
    fn name(&self) -> &'static str {
        "lp"
    }

    fn param_info(&self) -> &'static [ParamInfo] {
        // A `const`: a reference to an array built by (const) function
        // calls isn't promoted to 'static automatically.
        const PARAMS: &[ParamInfo] = &[ParamInfo::log("cutoff", 20.0, 20_000.0, 20_000.0, "Hz")];
        PARAMS
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.sample_rate = info.sample_rate;
        self.cutoff.prepare(info.sample_rate, 0.02);
        self.state = vec![0.0; info.channels];
    }

    fn set_param(&mut self, id: usize, value: f32) {
        if id == 0 {
            self.cutoff.set(value.clamp(20.0, self.sample_rate * 0.45));
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext) {
        if self.cutoff.is_settled() {
            // The coefficient depends only on the cutoff: when it isn't
            // moving, compute it (and its `exp`) once per block, not once
            // per sample. Each channel is then a tight loop over a slice.
            let a = 1.0 - (-TAU * self.cutoff.value() / self.sample_rate).exp();
            for ch in 0..buffer.channels() {
                let mut y = self.state[ch];
                for x in buffer.channel_mut(ch) {
                    y += a * (*x - y);
                    *x = y;
                }
                self.state[ch] = y;
            }
            return;
        }

        for i in 0..buffer.frames() {
            let fc = self.cutoff.next();
            let a = 1.0 - (-TAU * fc / self.sample_rate).exp();

            for ch in 0..buffer.channels() {
                let x = buffer.channel(ch)[i];
                let y = &mut self.state[ch];
                *y += a * (x - *y);
                buffer.channel_mut(ch)[i] = *y;
            }
        }
    }
}
