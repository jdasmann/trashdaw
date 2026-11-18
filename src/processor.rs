use crate::buffer::AudioBuffer;
use crate::transport::ProcessContext;

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

    /// Parameter names. A parameter's id is its index in this list.
    fn params(&self) -> &'static [&'static str];

    fn prepare(&mut self, info: &PrepareInfo);

    fn set_param(&mut self, id: usize, value: f32);

    /// Process in place: generators add into the buffer, effects
    /// transform what's already there. `ctx` says where this block sits
    /// in time; within one call, time is contiguous.
    fn process(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext);
}

/// A sound source driven by MIDI. Separate from `Processor` because it
/// has a different job: effects transform audio, instruments create it
/// from events. A track has at most one, before its effect chain.
pub trait Instrument: Send {
    fn name(&self) -> &'static str;
    fn params(&self) -> &'static [&'static str];
    /// Main thread. Allocate here.
    fn prepare(&mut self, info: &PrepareInfo);
    /// Audio thread.
    fn set_param(&mut self, id: usize, value: f32);
    /// Audio thread. Adds into `buffer`. `events` are sorted by frame,
    /// relative to the start of the buffer's current window.
    fn render(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext, events: &[crate::midi::MidiEvent]);
}
