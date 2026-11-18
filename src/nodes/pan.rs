use crate::buffer::AudioBuffer;
use crate::processor::{ParamInfo, PrepareInfo, Processor};
use crate::transport::ProcessContext;
use crate::smoothing::Smoothed;
use std::f32::consts::FRAC_PI_4;

/// Equal-power stereo panner.
/// `pos` runs from -1.0 (hard left) through 0.0 (center) to 1.0 (hard right).
pub struct Pan {
    pos: Smoothed,
}

impl Pan {
    pub fn new() -> Self {
        Self { pos: Smoothed::new(0.0) }
    }
}

/// Returns (left_gain, right_gain). cos² + sin² = 1, so total power
/// stays constant across the sweep; each side is ~0.707 (-3 dB) at center.
#[inline]
pub(crate) fn pan_gains(pos: f32) -> (f32, f32) {
    let theta = (pos.clamp(-1.0, 1.0) + 1.0) * FRAC_PI_4;
    (theta.cos(), theta.sin())
}

impl Processor for Pan {
    fn name(&self) -> &'static str {
        "pan"
    }

    fn param_info(&self) -> &'static [ParamInfo] {
        // A `const`: a reference to an array built by (const) function
        // calls isn't promoted to 'static automatically.
        const PARAMS: &[ParamInfo] = &[ParamInfo::linear("pos", -1.0, 1.0, 0.0, "")];
        PARAMS
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.pos.prepare(info.sample_rate, 0.01);
    }

    fn set_param(&mut self, id: usize, value: f32) {
        if id == 0 {
            self.pos.set(value.clamp(-1.0, 1.0));
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext) {
        // Nothing to pan in mono. Extra channels beyond stereo are left alone.
        if buffer.channels() < 2 {
            return;
        }

        for i in 0..buffer.frames() {
            let (left, right) = pan_gains(self.pos.next());
            buffer.channel_mut(0)[i] *= left;
            buffer.channel_mut(1)[i] *= right;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pan_gains;

    #[test]
    fn equal_power() {
        for pos in [-1.0, -0.5, 0.0, 0.3, 1.0] {
            let (l, r) = pan_gains(pos);
            assert!((l * l + r * r - 1.0).abs() < 1e-6);
        }
        let (l, r) = pan_gains(-1.0);
        assert!((l - 1.0).abs() < 1e-6 && r.abs() < 1e-6);
        let (l, r) = pan_gains(0.0);
        assert!((l - r).abs() < 1e-6);
    }
}
