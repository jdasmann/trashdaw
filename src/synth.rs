//! A polyphonic subtractive-style synth: band-limited oscillators,
//! ADSR envelopes, and voice allocation.

use crate::buffer::AudioBuffer;
use crate::midi::{MidiEvent, MidiMessage, SUSTAIN_PEDAL};
use crate::processor::{Instrument, ParamInfo, PrepareInfo};
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
    /// The voice's own filter: coefficients and the two state values.
    filter: Svf,
}

/// A state-variable lowpass in "topology-preserving transform" form (after
/// Vadim Zavalishin and Andrew Simper). Unlike a biquad, its cutoff can be
/// modulated constantly, on every voice, without instability or zipper
/// noise, which is exactly what a filter envelope does.
#[derive(Clone, Copy)]
struct Svf {
    a1: f32,
    a2: f32,
    a3: f32,
    ic1: f32,
    ic2: f32,
}

impl Svf {
    const IDLE: Svf = Svf { a1: 1.0, a2: 0.0, a3: 0.0, ic1: 0.0, ic2: 0.0 };

    /// `g` is the pre-warped cutoff (tan(π·fc/sr)), `k` the damping
    /// (2 = no resonance, near 0 = ringing).
    #[inline(always)]
    fn set(&mut self, g: f32, k: f32) {
        self.a1 = 1.0 / (1.0 + g * (g + k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    #[inline(always)]
    fn lowpass(&mut self, x: f32) -> f32 {
        let v3 = x - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        v2
    }
}

/// The cutoff parameter's top value, which means "filter off".
const CUTOFF_OFF: f32 = 20_000.0;

/// Recompute each voice's filter coefficients this often while its
/// envelope moves: tan() per sample per voice would be wasteful.
const FILTER_CONTROL_RATE: usize = 16;

/// Per-block filter settings shared by every voice.
struct FilterSettings {
    cutoff: f32,
    env_octaves: f32,
    k: f32,
    max_cutoff: f32,
    pi_over_sr: f32,
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
        filter: Svf::IDLE,
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
/// `FILTERED` is a const generic, so the compiler builds two versions of
/// this loop: one with the filter and one without. With a runtime check in
/// here instead, unfiltered sine and square voices measured 12-18% slower,
/// even though the branch always went the same way: it kept the loop from
/// being optimized as tightly.
#[inline(always)]
fn run_voice<const FILTERED: bool>(
    v: &mut Voice,
    env: &Envelope,
    filter: &FilterSettings,
    mix: &mut [f32],
    osc: impl Fn(f32, f32) -> f32,
) {
    for (i, out) in mix.iter_mut().enumerate() {
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
        let mut s = osc(v.phase, v.inc);
        if FILTERED {
            let f = filter;
            if i % FILTER_CONTROL_RATE == 0 {
                // The voice's own envelope opens the filter: at full level
                // the cutoff is `env_octaves` above the base cutoff.
                let fc = (f.cutoff * (f.env_octaves * v.level).exp2()).min(f.max_cutoff);
                v.filter.set((fc * f.pi_over_sr).tan(), f.k);
            }
            s = v.filter.lowpass(s);
        }
        *out += s * v.level * v.velocity;
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
    cutoff: f32,
    env_octaves: f32,
    resonance: f32,
    /// Sustain pedal down?
    pedal: bool,
    /// Notes whose key was released while the pedal was down: they keep
    /// ringing until the pedal comes up.
    sustained: [bool; 128],
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
            cutoff: 20_000.0,
            env_octaves: 0.0,
            resonance: 0.0,
            pedal: false,
            sustained: [false; 128],
        }
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        // Played again: the key holds it now, not the pedal.
        self.sustained[note as usize] = false;
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
            v.filter = Svf::IDLE;
        }
        // A retriggered or stolen voice keeps its phase and attacks from
        // its current level, so the waveform doesn't jump.
        v.note = note;
        v.inc = freq / self.sample_rate;
        v.velocity = velocity as f32 / 127.0;
        v.stage = Stage::Attack;
        v.started = self.counter;
    }

    /// A key was released. With the pedal down, the note keeps ringing.
    fn note_off(&mut self, note: u8) {
        if self.pedal {
            if self.voices.iter().any(|v| v.note == note && !matches!(v.stage, Stage::Idle | Stage::Release)) {
                self.sustained[note as usize] = true;
            }
            return;
        }
        self.release(note);
    }

    fn pedal_changed(&mut self, value: u8) {
        let down = value >= 64; // the MIDI convention for on/off controllers
        if self.pedal && !down {
            for note in 0..128u8 {
                if self.sustained[note as usize] {
                    self.sustained[note as usize] = false;
                    self.release(note);
                }
            }
        }
        self.pedal = down;
    }

    /// Starts the release of every voice playing `note`.
    fn release(&mut self, note: u8) {
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
        // Decided before borrowing `self.mix` below.
        let filter_on = self.filter_active();
        let Some(mix) = self.mix.get_mut(..n) else { return };
        mix.fill(0.0);

        let env = Envelope {
            attack_step: 1.0 / (self.attack.max(0.001) * self.sample_rate),
            decay_step: (1.0 - self.sustain) / (self.decay.max(0.001) * self.sample_rate),
            sustain: self.sustain,
        };
        let max_cutoff = self.sample_rate * 0.45;
        let settings = FilterSettings {
            cutoff: self.cutoff.min(max_cutoff),
            env_octaves: self.env_octaves,
            // Resonance 0..1 -> damping 2..0.04 (Q from 0.5 to 25).
            k: 2.0 * (1.0 - 0.98 * self.resonance),
            max_cutoff,
            pi_over_sr: std::f32::consts::PI / self.sample_rate,
        };
        let f = &settings;
        for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Idle) {
            // Monomorphized per waveform AND per filter on/off: the choice
            // is made here, once per voice, never inside the sample loop.
            match (self.wave, filter_on) {
                (Wave::Sine, false) => run_voice::<false>(v, &env, f, mix, |p, _| fast_sin_turns(p)),
                (Wave::Sine, true) => run_voice::<true>(v, &env, f, mix, |p, _| fast_sin_turns(p)),
                (Wave::Saw, false) => run_voice::<false>(v, &env, f, mix, |p, dt| oscillator(Wave::Saw, p, dt)),
                (Wave::Saw, true) => run_voice::<true>(v, &env, f, mix, |p, dt| oscillator(Wave::Saw, p, dt)),
                (Wave::Square, false) => run_voice::<false>(v, &env, f, mix, |p, dt| oscillator(Wave::Square, p, dt)),
                (Wave::Square, true) => run_voice::<true>(v, &env, f, mix, |p, dt| oscillator(Wave::Square, p, dt)),
            }
        }

        let (l, r) = buffer.stereo_mut();
        for ((l, r), &s) in l[from..to].iter_mut().zip(&mut r[from..to]).zip(mix.iter()) {
            let s = s * self.level.next();
            *l += s;
            *r += s;
        }
    }

    /// The filter is off at its top cutoff with no envelope: then every
    /// voice skips it, and the synth sounds exactly as it did before it had
    /// a filter. (Not "at or above 0.45 x sample rate": at 48 kHz that's
    /// 21.6 kHz, above the 20 kHz default, so the filter would always run.)
    fn filter_active(&self) -> bool {
        self.cutoff < CUTOFF_OFF || self.env_octaves > 0.0
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

    fn param_info(&self) -> &'static [ParamInfo] {
        // A `const`: a reference to an array built by (const) function
        // calls isn't promoted to 'static automatically.
        const PARAMS: &[ParamInfo] = &[
                ParamInfo::int("wave", 0.0, 2.0, 1.0), // sine, saw, square
                ParamInfo::log("attack", 0.001, 10.0, 0.005, "s"),
                ParamInfo::log("decay", 0.001, 10.0, 0.2, "s"),
                ParamInfo::linear("sustain", 0.0, 1.0, 0.6, ""),
                ParamInfo::log("release", 0.001, 10.0, 0.3, "s"),
                ParamInfo::linear("level", 0.0, 1.0, 0.2, ""),
                ParamInfo::log("cutoff", 20.0, 20_000.0, 20_000.0, "Hz"),
                ParamInfo::linear("env_amount", 0.0, 10.0, 0.0, "oct"),
                ParamInfo::linear("resonance", 0.0, 1.0, 0.0, ""),
            ];
        PARAMS
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
            // Filter: base cutoff in Hz (20000 = off), envelope amount in
            // octaves, resonance 0..1.
            6 => self.cutoff = value.clamp(20.0, CUTOFF_OFF),
            7 => self.env_octaves = value.clamp(0.0, 10.0),
            8 => self.resonance = value.clamp(0.0, 1.0),
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
                MidiMessage::ForceOff { note } => {
                    self.sustained[note as usize] = false;
                    self.release(note);
                }
                MidiMessage::ControlChange { controller: SUSTAIN_PEDAL, value } => self.pedal_changed(value),
                MidiMessage::ControlChange { .. } => {}
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

    // ---- Step 7 exercise 3: sustain pedal ----

    fn pedal(frame: usize, down: bool) -> MidiEvent {
        MidiEvent { frame, msg: MidiMessage::ControlChange { controller: 64, value: if down { 127 } else { 0 } } }
    }

    fn releasing(s: &Synth, note: u8) -> bool {
        s.voices.iter().any(|v| v.note == note && v.stage == Stage::Release)
    }

    #[test]
    fn pedal_holds_released_keys_until_it_lifts() {
        let mut s = synth();
        run(&mut s, 512, &[pedal(0, true), on(10, 60), off(100, 60)]);
        assert_eq!(s.sounding(), 1);
        assert!(!releasing(&s, 60), "key up, pedal down: still sustaining");
        run(&mut s, 512, &[pedal(0, false)]);
        assert!(releasing(&s, 60), "pedal up: now it releases");
    }

    #[test]
    fn replaying_a_sustained_note_then_releasing_it() {
        let mut s = synth();
        run(&mut s, 512, &[pedal(0, true), on(0, 60), off(50, 60), on(100, 60), off(200, 60)]);
        assert!(!releasing(&s, 60));
        run(&mut s, 512, &[pedal(0, false)]);
        assert!(releasing(&s, 60));
    }

    #[test]
    fn force_off_ignores_the_pedal() {
        // What the sequencer sends when you seek while the pedal is held:
        // the old notes must stop, not ring on under the new position.
        let mut s = synth();
        run(&mut s, 512, &[pedal(0, true), on(0, 60), on(0, 64)]);
        run(&mut s, 512, &[MidiEvent { frame: 0, msg: MidiMessage::ForceOff { note: 60 } }]);
        assert!(releasing(&s, 60), "forced off despite the pedal");
        assert!(!releasing(&s, 64), "other notes unaffected");
        // And it doesn't come back to life when the pedal lifts later.
        run(&mut s, 48_000, &[]);
        run(&mut s, 512, &[pedal(0, false)]);
        assert!(s.voices.iter().all(|v| v.note != 60 || v.stage == Stage::Idle));
    }

    #[test]
    fn other_controllers_are_ignored() {
        let mut s = synth();
        let cc = MidiEvent { frame: 0, msg: MidiMessage::ControlChange { controller: 1, value: 127 } };
        run(&mut s, 512, &[cc, on(0, 60), off(100, 60)]);
        assert!(releasing(&s, 60), "mod wheel isn't a sustain pedal");
    }

    // ---- Step 7 exercise 2: a filter per voice ----

    fn held_note(s: &mut Synth, note: u8, frames: usize) -> Vec<f32> {
        run(s, frames, &[on(0, note)])
    }

    #[test]
    fn filter_is_off_by_default() {
        // Regression: an earlier version compared two synths that were
        // BOTH filtered, so it passed while the default sound had changed.
        // The heavy benchmark's reference render caught it.
        let mut s = synth();
        assert!(!s.filter_active());
        s.set_param(6, 19_000.0);
        assert!(s.filter_active());
        s.set_param(6, 20_000.0);
        s.set_param(7, 1.0);
        assert!(s.filter_active(), "the envelope alone turns it on");
        s.set_param(7, 0.0);
        assert!(!s.filter_active());
    }

    #[test]
    fn lowpass_darkens_the_saw() {
        // A 220 Hz saw: its 20th harmonic is 4400 Hz.
        let mut open = synth();
        let mut closed = synth();
        closed.set_param(6, 500.0);
        let a = held_note(&mut open, 57, 48_000);
        let b = held_note(&mut closed, 57, 48_000);
        let (ha, hb) = (goertzel(&a[24_000..], 4_400.0), goertzel(&b[24_000..], 4_400.0));
        // A two-pole lowpass falls 12 dB per octave; 4400 Hz is 3.14
        // octaves above 500 Hz, so expect about -37.7 dB.
        let db = 20.0 * (hb / ha).log10();
        let expected = -12.0 * (4_400.0f32 / 500.0).log2();
        assert!((db - expected).abs() < 2.0, "20th harmonic: {db:.1} dB, expected about {expected:.1}");
        // The fundamental passes nearly untouched.
        let (fa, fb) = (goertzel(&a[24_000..], 220.0), goertzel(&b[24_000..], 220.0));
        assert!(fb > fa * 0.8, "fundamental: open {fa:.4}, filtered {fb:.4}");
    }

    #[test]
    fn filter_envelope_opens_then_closes() {
        // Low base cutoff, 5 octaves of envelope, a short decay to a low
        // sustain: bright at the start of the note, dark later.
        let mut s = synth();
        s.set_param(6, 200.0);
        s.set_param(7, 5.0);
        s.set_param(2, 0.2); // decay 200 ms
        s.set_param(3, 0.1); // sustain
        let out = held_note(&mut s, 45, 48_000);
        let harmonic = 110.0 * 20.0; // 2.2 kHz
        let early = goertzel(&out[240..2_640], harmonic); // 5-55 ms
        let late = goertzel(&out[40_000..42_400], harmonic);
        assert!(early > 20.0 * late, "early {early:.5}, late {late:.6}");
    }

    #[test]
    fn resonance_boosts_the_cutoff_region_and_stays_stable() {
        let mut flat = synth();
        let mut res = synth();
        for s in [&mut flat, &mut res] {
            s.set_param(6, 880.0); // the 8th harmonic of a 110 Hz saw
        }
        res.set_param(8, 0.95);
        let a = held_note(&mut flat, 45, 48_000);
        let b = held_note(&mut res, 45, 48_000);
        assert!(goertzel(&b[24_000..], 880.0) > 3.0 * goertzel(&a[24_000..], 880.0));
        assert!(b.iter().all(|x| x.is_finite() && x.abs() < 10.0));

        // Full resonance with the envelope sweeping the cutoff hard.
        let mut wild = synth();
        wild.set_param(6, 50.0);
        wild.set_param(7, 9.0);
        wild.set_param(8, 1.0);
        wild.set_param(2, 0.05);
        let events: Vec<_> = (0..16).map(|k| on(k * 300, 30 + k as u8 * 5)).collect();
        let out = run(&mut wild, 48_000, &events);
        assert!(out.iter().all(|x| x.is_finite() && x.abs() < 10.0), "unstable");
    }
}
