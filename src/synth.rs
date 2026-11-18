//! A polyphonic subtractive-style synth: band-limited oscillators,
//! ADSR envelopes, and voice allocation.

use crate::buffer::AudioBuffer;
use crate::midi::{MidiEvent, MidiMessage};
use crate::processor::{Instrument, PrepareInfo};
use crate::smoothing::Smoothed;
use crate::transport::ProcessContext;
use std::f32::consts::TAU;

const VOICES: usize = 16;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Wave {
    Sine,
    Saw,
    Square,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy)]
struct Voice {
    note: u8,
    phase: f32,
    /// Phase increment per sample (frequency / sample rate).
    inc: f32,
    velocity: f32,
    stage: Stage,
    /// Envelope level, 0..1.
    level: f32,
    release_step: f32,
    /// When it was started, for "steal the oldest".
    started: u64,
}

impl Voice {
    const IDLE: Voice = Voice {
        note: 0,
        phase: 0.0,
        inc: 0.0,
        velocity: 0.0,
        stage: Stage::Idle,
        level: 0.0,
        release_step: 0.0,
        started: 0,
    };
}

/// PolyBLEP: a small polynomial correction applied around each jump in a
/// saw or square wave. A naive saw jumps from +1 to -1 instantly, which
/// contains infinitely high harmonics; the ones above Nyquist fold back
/// down as inharmonic "aliasing". Rounding off the jump over one sample on
/// each side removes most of it, for the cost of a couple of multiplies.
/// `t` is the phase (0..1), `dt` the phase increment.
fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

/// sin(2π · phase) for phase in [0, 1), about 4x faster than `f32::sin`.
///
/// `f32::sin` is accurate to the last bit for ANY angle, which costs a
/// careful range reduction. An oscillator's phase is always in [0, 1), so
/// we can fold it onto a quarter wave, where a short Taylor polynomial is
/// accurate to ~4e-6 (-108 dB): far below anything audible.
#[inline(always)]
pub(crate) fn fast_sin_turns(phase: f32) -> f32 {
    use std::f32::consts::PI;
    // sin(2π·phase) = -sin(2π·(phase - 0.5)), and that angle is in [-π, π).
    let a = (phase - 0.5) * TAU;
    // Fold onto [0, π/2] using sin(π - x) = sin(x), keeping the sign.
    let x = a.abs().min(PI - a.abs());
    let x2 = x * x;
    // Taylor series up to x^9, evaluated in Horner form.
    let s = x * (1.0 + x2 * (-1.0 / 6.0 + x2 * (1.0 / 120.0 + x2 * (-1.0 / 5040.0 + x2 * (1.0 / 362_880.0)))));
    -s.copysign(a)
}

fn oscillator(wave: Wave, phase: f32, dt: f32) -> f32 {
    match wave {
        Wave::Sine => fast_sin_turns(phase),
        Wave::Saw => 2.0 * phase - 1.0 - poly_blep(phase, dt),
        Wave::Square => {
            let naive = if phase < 0.5 { 1.0 } else { -1.0 };
            // A square has two jumps per cycle: up at 0 and down at 0.5.
            // (A subtraction, not `% 1.0`: float `%` is a slow library
            // call, and for values in [0.5, 1.5) this is exactly the same.)
            let half = phase + 0.5;
            let half = if half >= 1.0 { half - 1.0 } else { half };
            naive + poly_blep(phase, dt) - poly_blep(half, dt)
        }
    }
}

/// Per-block envelope constants.
struct Envelope {
    attack_step: f32,
    decay_step: f32,
    sustain: f32,
}

/// Runs one voice over `mix`, adding into it. `osc(phase, dt)` is the
/// waveform; being a generic closure, it is inlined into this loop.
#[inline(always)]
fn run_voice(v: &mut Voice, env: &Envelope, mix: &mut [f32], osc: impl Fn(f32, f32) -> f32) {
    for out in mix.iter_mut() {
        match v.stage {
            Stage::Idle => return,
            Stage::Attack => {
                v.level += env.attack_step;
                if v.level >= 1.0 {
                    v.level = 1.0;
                    v.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                v.level -= env.decay_step;
                if v.level <= env.sustain {
                    v.level = env.sustain;
                    v.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => v.level = env.sustain,
            Stage::Release => {
                v.level -= v.release_step;
                if v.level <= 0.0 {
                    *v = Voice::IDLE;
                    return;
                }
            }
        }
        *out += osc(v.phase, v.inc) * v.level * v.velocity;
        v.phase += v.inc;
        if v.phase >= 1.0 {
            v.phase -= 1.0;
        }
    }
}

pub struct Synth {
    voices: [Voice; VOICES],
    /// Scratch buffer the voices are summed into, sized in `prepare`.
    mix: Vec<f32>,
    sample_rate: f32,
    wave: Wave,
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
    level: Smoothed,
    counter: u64,
}

impl Synth {
    pub fn new() -> Self {
        let mut level = Smoothed::new(0.2);
        level.set(0.2);
        Self {
            voices: [Voice::IDLE; VOICES],
            mix: Vec::new(),
            sample_rate: 48_000.0,
            wave: Wave::Saw,
            attack: 0.005,
            decay: 0.2,
            sustain: 0.6,
            release: 0.3,
            level,
            counter: 0,
        }
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        self.counter += 1;
        let freq = 440.0 * 2f32.powf((note as f32 - 69.0) / 12.0);

        // Voice allocation, in order of preference:
        //  1. the same note already sounding (retrigger it),
        //  2. an idle voice,
        //  3. the quietest voice that is releasing,
        //  4. the oldest voice.
        let index = self
            .voices
            .iter()
            .position(|v| v.stage != Stage::Idle && v.note == note)
            .or_else(|| self.voices.iter().position(|v| v.stage == Stage::Idle))
            .or_else(|| {
                (0..VOICES)
                    .filter(|&i| self.voices[i].stage == Stage::Release)
                    .min_by(|&a, &b| self.voices[a].level.total_cmp(&self.voices[b].level))
            })
            .unwrap_or_else(|| (0..VOICES).min_by_key(|&i| self.voices[i].started).unwrap());

        let v = &mut self.voices[index];
        if v.stage == Stage::Idle {
            v.phase = 0.0;
            v.level = 0.0;
        }
        // A retriggered or stolen voice keeps its phase and attacks from
        // its current level, so the waveform doesn't jump.
        v.note = note;
        v.inc = freq / self.sample_rate;
        v.velocity = velocity as f32 / 127.0;
        v.stage = Stage::Attack;
        v.started = self.counter;
    }

    fn note_off(&mut self, note: u8) {
        let release_samples = self.release.max(0.001) * self.sample_rate;
        for v in &mut self.voices {
            if v.note == note && !matches!(v.stage, Stage::Idle | Stage::Release) {
                v.stage = Stage::Release;
                // Linear release from wherever the envelope is now.
                v.release_step = v.level / release_samples;
            }
        }
    }

    /// Renders frames `from..to`, voice by voice.
    ///
    /// The obvious loop is "for each sample, for each voice": every sample
    /// re-checks every voice's stage and the waveform type. Turning it
    /// inside out ("for each voice, for each sample") gives each voice a
    /// tight loop, chooses the waveform once per voice instead of once per
    /// sample, and skips idle voices entirely. Voices are still summed in
    /// the same order, so the output is bit-identical.
    fn render_span(&mut self, buffer: &mut AudioBuffer, from: usize, to: usize) {
        // The scratch buffer was sized in `prepare`; work in chunks of it so
        // even an oversized request can't make us allocate.
        let mut start = from;
        while start < to {
            let end = to.min(start + self.mix.len().max(1));
            self.render_chunk(buffer, start, end);
            if self.mix.is_empty() {
                break; // not prepared: nothing we can safely do
            }
            start = end;
        }
    }

    fn render_chunk(&mut self, buffer: &mut AudioBuffer, from: usize, to: usize) {
        let n = to - from;
        let Some(mix) = self.mix.get_mut(..n) else { return };
        mix.fill(0.0);

        let env = Envelope {
            attack_step: 1.0 / (self.attack.max(0.001) * self.sample_rate),
            decay_step: (1.0 - self.sustain) / (self.decay.max(0.001) * self.sample_rate),
            sustain: self.sustain,
        };
        for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Idle) {
            // Monomorphized per waveform: the match happens here, once.
            match self.wave {
                Wave::Sine => run_voice(v, &env, mix, |p, _| fast_sin_turns(p)),
                Wave::Saw => run_voice(v, &env, mix, |p, dt| oscillator(Wave::Saw, p, dt)),
                Wave::Square => run_voice(v, &env, mix, |p, dt| oscillator(Wave::Square, p, dt)),
            }
        }

        let (l, r) = buffer.stereo_mut();
        for ((l, r), &s) in l[from..to].iter_mut().zip(&mut r[from..to]).zip(mix.iter()) {
            let s = s * self.level.next();
            *l += s;
            *r += s;
        }
    }

    #[cfg(test)]
    fn sounding(&self) -> usize {
        self.voices.iter().filter(|v| v.stage != Stage::Idle).count()
    }
}

impl Instrument for Synth {
    fn name(&self) -> &'static str {
        "synth"
    }

    fn params(&self) -> &'static [&'static str] {
        &["wave", "attack", "decay", "sustain", "release", "level"]
    }

    fn prepare(&mut self, info: &PrepareInfo) {
        self.sample_rate = info.sample_rate;
        self.mix = vec![0.0; info.max_frames];
        self.level.prepare(info.sample_rate, 0.01);
    }

    fn set_param(&mut self, id: usize, value: f32) {
        match id {
            0 => {
                self.wave = match value as u32 {
                    0 => Wave::Sine,
                    1 => Wave::Saw,
                    _ => Wave::Square,
                }
            }
            // Envelope times in seconds.
            1 => self.attack = value.clamp(0.0, 10.0),
            2 => self.decay = value.clamp(0.0, 10.0),
            3 => self.sustain = value.clamp(0.0, 1.0),
            4 => self.release = value.clamp(0.0, 10.0),
            5 => self.level.set(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    /// Sample-accurate: render up to each event, apply it, continue.
    fn render(&mut self, buffer: &mut AudioBuffer, _ctx: &ProcessContext, events: &[MidiEvent]) {
        let n = buffer.frames();
        let mut cursor = 0;
        for ev in events {
            let frame = ev.frame.min(n);
            self.render_span(buffer, cursor, frame);
            cursor = frame;
            match ev.msg {
                MidiMessage::NoteOn { note, velocity } => self.note_on(note, velocity),
                MidiMessage::NoteOff { note } => self.note_off(note),
            }
        }
        self.render_span(buffer, cursor, n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    const SR: f32 = 48_000.0;

    fn synth() -> Synth {
        let mut s = Synth::new();
        s.prepare(&PrepareInfo { sample_rate: SR, max_frames: 48_000, channels: 2 });
        s
    }

    fn run(s: &mut Synth, frames: usize, events: &[MidiEvent]) -> Vec<f32> {
        let mut buf = AudioBuffer::new(2, frames);
        buf.clear();
        s.render(&mut buf, &Transport::new(SR as f64).context(), events);
        buf.channel(0).to_vec()
    }

    fn on(frame: usize, note: u8) -> MidiEvent {
        MidiEvent { frame, msg: MidiMessage::NoteOn { note, velocity: 127 } }
    }

    fn off(frame: usize, note: u8) -> MidiEvent {
        MidiEvent { frame, msg: MidiMessage::NoteOff { note } }
    }

    #[test]
    fn note_on_is_sample_accurate() {
        let mut s = synth();
        s.set_param(0, 0.0); // sine
        let out = run(&mut s, 512, &[on(300, 69)]);
        assert!(out[..300].iter().all(|&x| x == 0.0));
        assert!(out[301] != 0.0);
    }

    #[test]
    fn envelope_releases_to_silence_and_frees_the_voice() {
        let mut s = synth();
        s.set_param(4, 0.1); // 100 ms release = 4800 frames
        run(&mut s, 4800, &[on(0, 60), off(2400, 60)]);
        assert_eq!(s.sounding(), 1, "still releasing, 2400 frames in");
        let out = run(&mut s, 4800, &[]);
        assert!(out[2300] != 0.0, "audible until the release ends");
        assert!(out[2500..].iter().all(|&x| x == 0.0));
        assert_eq!(s.sounding(), 0);
    }

    #[test]
    fn more_notes_than_voices_steals() {
        let mut s = synth();
        let events: Vec<_> = (0..24).map(|i| on(i * 10, 40 + i as u8)).collect();
        run(&mut s, 512, &events);
        assert_eq!(s.sounding(), VOICES);
        // The 8 oldest notes were stolen; the newest are all present.
        assert!((48..64).all(|n| s.voices.iter().any(|v| v.note == n)));
    }

    /// Magnitude of one frequency in a signal (a single DFT bin).
    fn goertzel(x: &[f32], freq: f32) -> f32 {
        let w = TAU * freq / SR;
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        let c = 2.0 * (w as f64).cos();
        for &v in x {
            let s = v as f64 + c * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        ((s1 * s1 + s2 * s2 - c * s1 * s2).sqrt() / x.len() as f64) as f32
    }

    #[test]
    fn polyblep_saw_has_far_less_aliasing() {
        // A 5 kHz saw at 48 kHz: its 9th harmonic (45 kHz) folds back to
        // 3 kHz, where a real saw has nothing.
        let (f, n) = (5_000.0, 48_000);
        let dt = f / SR;
        let naive: Vec<f32> = (0..n).map(|i| 2.0 * ((i as f32 * dt) % 1.0) - 1.0).collect();
        let blep: Vec<f32> = (0..n).map(|i| oscillator(Wave::Saw, (i as f32 * dt) % 1.0, dt)).collect();
        let alias_ratio = |x: &[f32]| goertzel(x, 3_000.0) / goertzel(x, f);
        let (a, b) = (alias_ratio(&naive), alias_ratio(&blep));
        assert!(b < a / 10.0, "naive {a:.4}, polyblep {b:.4}");
    }

    #[test]
    fn fast_sine_is_accurate() {
        let worst = (0..1_000_000)
            .map(|i| i as f32 / 1_000_000.0)
            .map(|p| (fast_sin_turns(p) - (p * TAU).sin()).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "max error {worst:e}");
    }

    #[test]
    fn square_phase_wrap_matches_modulo() {
        for i in 0..100_000 {
            let p = i as f32 / 100_000.0;
            let half = p + 0.5;
            let half = if half >= 1.0 { half - 1.0 } else { half };
            assert_eq!(half, (p + 0.5) % 1.0);
        }
    }
}
