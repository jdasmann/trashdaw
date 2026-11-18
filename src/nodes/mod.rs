mod biquad;
mod compressor;
mod delay;
mod eq;
mod gain;
mod lowpass;
mod metronome;
mod oscillator;
mod pan;
mod reverb;

pub use biquad::Biquad;
pub use compressor::Compressor;
pub use delay::Delay;
pub use eq::Eq3;
pub use gain::Gain;
pub(crate) use gain::db_to_linear;
pub use lowpass::Lowpass;
pub use metronome::Metronome;
pub use oscillator::Oscillator;
pub use pan::Pan;
pub use reverb::Reverb;
pub(crate) use pan::pan_gains;

use crate::processor::Processor;

/// Every effect kind, as (command-line name, display name). The command
/// line and the UI's "add effect" menu both read this list.
pub const KINDS: &[(&str, &str)] = &[
    ("filter", "Filter"),
    ("eq", "3-band EQ"),
    ("comp", "Compressor"),
    ("delay", "Delay"),
    ("reverb", "Reverb"),
    ("gain", "Gain"),
    ("pan", "Pan"),
    ("lp", "One-pole lowpass"),
    ("osc", "Test oscillator"),
    ("click", "Metronome"),
];

pub fn make(kind: &str) -> Option<Box<dyn Processor>> {
    Some(match kind {
        "osc" => Box::new(Oscillator::new()),
        "gain" => Box::new(Gain::new()),
        "lp" => Box::new(Lowpass::new()),
        "pan" => Box::new(Pan::new()),
        "click" => Box::new(Metronome::new()),
        "filter" => Box::new(Biquad::new()),
        "delay" => Box::new(Delay::new()),
        "comp" => Box::new(Compressor::new()),
        "reverb" => Box::new(Reverb::new()),
        "eq" => Box::new(Eq3::new()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::AudioBuffer;
    use crate::processor::{Instrument, ParamKind, PrepareInfo};
    use crate::transport::Transport;

    const INFO: PrepareInfo = PrepareInfo { sample_rate: 48_000.0, max_frames: 512, channels: 2 };

    fn render(node: &mut dyn Processor) -> Vec<f32> {
        let mut t = Transport::new(48_000.0);
        t.playing = true;
        let mut out = Vec::new();
        let mut x: u32 = 12_345;
        for _ in 0..40 {
            let mut buf = AudioBuffer::new(2, 512);
            for ch in 0..2 {
                for s in buf.channel_mut(ch) {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    *s = (x as f32 / u32::MAX as f32) - 0.5;
                }
            }
            node.process(&mut buf, &t.context());
            t.advance(512);
            out.extend_from_slice(buf.channel(0));
            out.extend_from_slice(buf.channel(1));
        }
        out
    }

    #[test]
    fn every_kind_can_be_made() {
        for (kind, _) in KINDS {
            assert!(make(kind).is_some(), "{kind}");
        }
    }

    /// The UI shows `default` as a parameter's starting value, so it must
    /// be TRUE: a fresh node and one with every parameter explicitly set
    /// to its declared default must sound identical.
    #[test]
    fn declared_defaults_match_each_nodes_real_state() {
        for (kind, _) in KINDS {
            let mut fresh = make(kind).unwrap();
            let mut set = make(kind).unwrap();
            fresh.prepare(&INFO);
            set.prepare(&INFO);
            for (id, p) in set.param_info().iter().enumerate() {
                assert!(p.min <= p.default && p.default <= p.max, "{kind}.{}: default outside range", p.name);
                assert!(p.kind != ParamKind::Log || p.min > 0.0, "{kind}.{}: log range must be positive", p.name);
                set.set_param(id, p.default);
            }
            assert_eq!(render(fresh.as_mut()), render(set.as_mut()), "{kind}: declared defaults don't match");
        }
    }

    #[test]
    fn synth_defaults_match_too() {
        use crate::midi::{MidiEvent, MidiMessage};
        let play = |s: &mut crate::synth::Synth| {
            let ctx = Transport::new(48_000.0).context();
            let mut buf = AudioBuffer::new(2, 512);
            let mut out = Vec::new();
            for block in 0..60 {
                buf.clear();
                let ev = [MidiEvent { frame: 3, msg: MidiMessage::NoteOn { note: 50, velocity: 100 } }];
                let off = [MidiEvent { frame: 0, msg: MidiMessage::NoteOff { note: 50 } }];
                let events: &[MidiEvent] = if block == 0 { &ev } else if block == 30 { &off } else { &[] };
                s.render(&mut buf, &ctx, events);
                out.extend_from_slice(buf.channel(0));
            }
            out
        };
        let mut fresh = crate::synth::Synth::new();
        let mut set = crate::synth::Synth::new();
        fresh.prepare(&INFO);
        set.prepare(&INFO);
        for (id, p) in set.param_info().iter().enumerate() {
            set.set_param(id, p.default);
        }
        assert_eq!(play(&mut fresh), play(&mut set), "synth: declared defaults don't match");
    }
}
