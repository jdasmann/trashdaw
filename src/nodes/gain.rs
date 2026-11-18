use crate::buffer::AudioBuffer;
use crate::processor::{ParamInfo, PrepareInfo, Processor};
use crate::transport::ProcessContext;
use crate::smoothing::Smoothed;

pub struct Gain {
    gain: Smoothed,
}

impl Gain {
    pub fn new() -> Self {
        Self { gain: Smoothed::new(1.0) }
    }
}

pub(crate) fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

impl Processor for Gain {
    fn name(&self) -> &'static str {
        "gain"
    }

    fn param_info(&self) -> &'static [ParamInfo] {
        // A `const`: a reference to an array built by (const) function
        // calls isn't promoted to 'static automatically.
        const PARAMS: &[ParamInfo] = &[ParamInfo::linear("db", -60.0, 12.0, 0.0, "dB")];
        PARAMS
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.gain.prepare(info.sample_rate, 0.01);
    }

    fn set_param(&mut self, id: usize, value: f32) {
        if id == 0 {
            // -60 dB is treated as silence.
            let db = value.clamp(-60.0, 12.0);
            self.gain.set(if db <= -60.0 { 0.0 } else { db_to_linear(db) });
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext) {
        if self.gain.is_settled() {
            let g = self.gain.value();
            if g != 1.0 {
                for ch in 0..buffer.channels() {
                    buffer.channel_mut(ch).iter_mut().for_each(|s| *s *= g);
                }
            }
            return;
        }

        for i in 0..buffer.frames() {
            let g = self.gain.next();
            for ch in 0..buffer.channels() {
                buffer.channel_mut(ch)[i] *= g;
            }
        }
    }
}
