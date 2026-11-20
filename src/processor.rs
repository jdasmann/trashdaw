use crate::buffer::AudioBuffer;
use crate::transport::ProcessContext;

/// How a parameter's value should be shown and edited.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamKind {
    /// Evenly spaced, like a pan position or a level in dB.
    Linear,
    /// Spaced by ratio, like frequency or time: 100 -> 200 Hz should feel
    /// as big as 1000 -> 2000 Hz. Requires min > 0.
    Log,
    /// Whole numbers, like a filter type or waveform.
    Int,
    /// Off (0) or on (1).
    Toggle,
}

/// Everything a UI needs to show a parameter without knowing the node:
/// the same idea as parameter info in plugin standards such as CLAP.
#[derive(Clone, Copy, Debug)]
pub struct ParamInfo {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    /// Must match the node's actual initial state (a test checks this).
    pub default: f32,
    pub unit: &'static str,
    pub kind: ParamKind,
}

impl ParamInfo {
    pub const fn linear(name: &'static str, min: f32, max: f32, default: f32, unit: &'static str) -> Self {
        Self { name, min, max, default, unit, kind: ParamKind::Linear }
    }
    pub const fn log(name: &'static str, min: f32, max: f32, default: f32, unit: &'static str) -> Self {
        Self { name, min, max, default, unit, kind: ParamKind::Log }
    }
    pub const fn int(name: &'static str, min: f32, max: f32, default: f32) -> Self {
        Self { name, min, max, default, unit: "", kind: ParamKind::Int }
    }
    pub const fn toggle(name: &'static str, default: bool) -> Self {
        Self { name, min: 0.0, max: 1.0, default: if default { 1.0 } else { 0.0 }, unit: "", kind: ParamKind::Toggle }
    }
}

/// Everything a processor needs to know before it starts running.
pub struct PrepareInfo {
    pub sample_rate: f32,
    pub max_frames: usize,
    pub channels: usize,
}

/// The building block of the engine: oscillators, effects, and later
/// sample players, synths, and plugins all implement this.
///
/// Contract:
/// - `prepare` runs on the MAIN thread. Allocate here.
/// - `set_param` and `process` run on the AUDIO thread. Never allocate,
///   lock, or do I/O in them.
pub trait Processor: Send {
    fn name(&self) -> &'static str;

    /// The parameters. A parameter's id is its index in this list.
    fn param_info(&self) -> &'static [ParamInfo];

    fn prepare(&mut self, info: &PrepareInfo);

    fn set_param(&mut self, id: usize, value: f32);

    /// Process in place: generators add into the buffer, effects
    /// transform what's already there. `ctx` says where this block sits
    /// in time; within one call, time is contiguous.
    fn process(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext);

    /// Like `process`, but with the track's sidechain ("key") signal, if
    /// it has one: another track's audio for the node to listen to, such
    /// as a kick drum driving a compressor on the bass. Most nodes don't
    /// care, so by default the key is ignored.
    fn process_keyed(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext, key: Option<&AudioBuffer>) {
        let _ = key;
        self.process(buffer, ctx);
    }

    /// Audio thread, just before the node leaves the engine (it's then
    /// freed on the main thread). Plugins must stop processing on the
    /// audio thread; built-in nodes have nothing to do.
    fn stop(&mut self) {}

    /// How many frames late this node's output is. Built-in effects have
    /// none; some plugins do (look-ahead). The mixer compensates for it.
    fn latency(&self) -> u32 {
        0
    }
}

/// A sound source driven by MIDI. Separate from `Processor` because it
/// has a different job: effects transform audio, instruments create it
/// from events. A track has at most one, before its effect chain.
pub trait Instrument: Send {
    fn name(&self) -> &'static str;
    fn param_info(&self) -> &'static [ParamInfo];
    /// Main thread. Allocate here.
    fn prepare(&mut self, info: &PrepareInfo);
    /// Audio thread.
    fn set_param(&mut self, id: usize, value: f32);
    /// Audio thread. Adds into `buffer`. `events` are sorted by frame,
    /// relative to the start of the buffer's current window.
    fn render(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext, events: &[crate::midi::MidiEvent]);

    /// Audio thread, just before the instrument leaves the engine.
    fn stop(&mut self) {}

    /// How many frames late the instrument's output is.
    fn latency(&self) -> u32 {
        0
    }
}
