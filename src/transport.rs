//! Time: where the playhead is, how fast it moves, and how to convert
//! between samples (what the hardware counts), seconds, and beats.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Fixed 4/4 for now; time signature changes come with the tempo map later.
pub const BEATS_PER_BAR: f64 = 4.0;
/// Ticks per quarter note, used only for display (and later, MIDI).
pub const PPQ: f64 = 960.0;

/// A snapshot of the transport for one (sub-)block, handed to every
/// processor. Within a single call, time is guaranteed to be contiguous:
/// the engine splits blocks at loop points, so frame `i` is always at
/// `position + i`.
#[derive(Clone, Copy)]
pub struct ProcessContext {
    pub sample_rate: f64,
    pub playing: bool,
    /// Sample position of the first frame in this block.
    pub position: u64,
    pub tempo: f64,
    /// The loop range in beats, if looping (for plugins that want it).
    pub loop_beats: Option<(f64, f64)>,
}

impl ProcessContext {
    pub fn samples_per_beat(&self) -> f64 {
        samples_per_beat(self.sample_rate, self.tempo)
    }

    /// Musical position (in beats) of frame `frame` within this block.
    pub fn beat_at(&self, frame: usize) -> f64 {
        (self.position + frame as u64) as f64 / self.samples_per_beat()
    }
}

pub fn samples_per_beat(sample_rate: f64, tempo: f64) -> f64 {
    sample_rate * 60.0 / tempo
}

/// Audio-thread transport state. Only the engine owns and mutates this.
pub struct Transport {
    pub playing: bool,
    /// The master clock, in samples. Integer, so it never drifts.
    pub position: u64,
    tempo: f64,
    sample_rate: f64,
    /// Loop region in beats. Stored musically so it stays on the same
    /// bars when the tempo changes.
    loop_beats: Option<(f64, f64)>,
}

impl Transport {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            playing: false,
            position: 0,
            tempo: 120.0,
            sample_rate,
            loop_beats: None,
        }
    }

    fn beats_to_samples(&self, beats: f64) -> u64 {
        (beats * samples_per_beat(self.sample_rate, self.tempo)).round().max(0.0) as u64
    }

    fn current_beat(&self) -> f64 {
        self.position as f64 / samples_per_beat(self.sample_rate, self.tempo)
    }

    pub fn tempo(&self) -> f64 {
        self.tempo
    }

    /// Changing tempo must not move the playhead musically. If we kept the
    /// sample position as-is, beat 9.0 at 120 BPM would suddenly become
    /// beat 6.75 at 90 BPM. So we rescale the sample position.
    pub fn set_tempo(&mut self, bpm: f64) {
        let beat = self.current_beat();
        self.tempo = bpm;
        self.position = self.beats_to_samples(beat);
    }

    pub fn seek_beats(&mut self, beat: f64) {
        self.position = self.beats_to_samples(beat);
    }

    pub fn set_loop(&mut self, range: Option<(f64, f64)>) {
        self.loop_beats = range;
    }

    fn loop_samples(&self) -> Option<(u64, u64)> {
        self.loop_beats
            .map(|(s, e)| (self.beats_to_samples(s), self.beats_to_samples(e)))
    }

    /// How many frames (at most `max`) can be processed before time
    /// becomes discontinuous (i.e. the loop end).
    pub fn frames_until_boundary(&self, max: usize) -> usize {
        if self.playing {
            if let Some((_, end)) = self.loop_samples() {
                if self.position < end {
                    return max.min((end - self.position) as usize);
                }
            }
        }
        max
    }

    pub fn advance(&mut self, frames: usize) {
        if !self.playing {
            return;
        }
        let before = self.position;
        self.position += frames as u64;
        if let Some((start, end)) = self.loop_samples() {
            // Thanks to frames_until_boundary, we land exactly on `end`.
            if before < end && self.position >= end {
                self.position = start + (self.position - end);
            }
        }
    }

    pub fn context(&self) -> ProcessContext {
        ProcessContext {
            sample_rate: self.sample_rate,
            playing: self.playing,
            position: self.position,
            tempo: self.tempo,
            loop_beats: self.loop_beats,
        }
    }
}

/// Transport state published by the audio thread for the UI to read.
/// Atomics are real-time safe: no locks, no allocation.
pub struct TransportShared {
    position: AtomicU64,
    playing: AtomicBool,
    tempo_bits: AtomicU64, // f64 stored as raw bits
    /// Total plugin delay compensation, in frames: how much later than
    /// the playhead everything is heard.
    plugin_latency: AtomicU64,
    /// Allocations caught on the audio thread (debug builds only).
    rt_violations: AtomicU64,
    /// Frames in the most recent device callback.
    callback_frames: AtomicU64,
}

impl TransportShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            position: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            tempo_bits: AtomicU64::new(120f64.to_bits()),
            plugin_latency: AtomicU64::new(0),
            rt_violations: AtomicU64::new(0),
            callback_frames: AtomicU64::new(0),
        })
    }

    /// Audio thread: called once per callback.
    pub fn publish(&self, t: &Transport) {
        // Relaxed is enough: these are display values, read independently.
        self.position.store(t.position, Ordering::Relaxed);
        self.playing.store(t.playing, Ordering::Relaxed);
        self.tempo_bits.store(t.tempo().to_bits(), Ordering::Relaxed);
    }

    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn tempo(&self) -> f64 {
        f64::from_bits(self.tempo_bits.load(Ordering::Relaxed))
    }

    pub fn set_plugin_latency(&self, frames: u64) {
        self.plugin_latency.store(frames, Ordering::Relaxed);
    }

    pub fn plugin_latency(&self) -> u64 {
        self.plugin_latency.load(Ordering::Relaxed)
    }

    pub fn add_rt_violations(&self, n: u64) {
        self.rt_violations.fetch_add(n, Ordering::Relaxed);
    }

    pub fn rt_violations(&self) -> u64 {
        self.rt_violations.load(Ordering::Relaxed)
    }

    pub fn set_callback_frames(&self, frames: u64) {
        self.callback_frames.store(frames, Ordering::Relaxed);
    }

    /// How many frames the device asks for per callback: the buffer size,
    /// and with it the latency of the output (step 1's exercise).
    pub fn callback_frames(&self) -> u64 {
        self.callback_frames.load(Ordering::Relaxed)
    }
}

/// Formats a beat position as bars.beats.ticks, 1-based like every DAW.
pub fn format_bbt(beat: f64) -> String {
    let bar = (beat / BEATS_PER_BAR).floor() as u64 + 1;
    let beat_in_bar = (beat % BEATS_PER_BAR).floor() as u64 + 1;
    let ticks = (beat.fract() * PPQ).floor() as u64;
    format!("{bar}.{beat_in_bar}.{ticks:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_change_keeps_musical_position() {
        let mut t = Transport::new(48_000.0);
        t.seek_beats(9.0);
        t.set_tempo(90.0);
        assert!((t.current_beat() - 9.0).abs() < 1e-6);
    }

    #[test]
    fn loop_splits_and_wraps_exactly() {
        let mut t = Transport::new(48_000.0); // 120 BPM: 24_000 samples/beat
        t.set_loop(Some((0.0, 1.0)));
        t.playing = true;
        t.position = 23_900;
        let n = t.frames_until_boundary(512);
        assert_eq!(n, 100);
        t.advance(n);
        assert_eq!(t.position, 0);
    }

    #[test]
    fn bbt_formatting() {
        assert_eq!(format_bbt(0.0), "1.1.000");
        assert_eq!(format_bbt(5.5), "2.2.480");
    }
}
