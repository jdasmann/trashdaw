//! MIDI messages, sample-accurate events, and the compiled note sequence.

use crate::transport::ProcessContext;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MidiMessage {
    NoteOn { note: u8, velocity: u8 },
    NoteOff { note: u8 },
}

impl MidiMessage {
    /// Parses raw bytes from a MIDI device. Everything except notes
    /// (controllers, pitch bend, clock...) is ignored for now.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let (&status, rest) = bytes.split_first()?;
        match (status & 0xF0, rest) {
            // A note-on with velocity 0 means note-off (running status trick).
            (0x90, [note, 0, ..]) | (0x80, [note, _, ..]) => Some(Self::NoteOff { note: note & 0x7F }),
            (0x90, [note, velocity, ..]) => Some(Self::NoteOn { note: note & 0x7F, velocity: velocity & 0x7F }),
            _ => None,
        }
    }
}

/// A message at an exact sample, relative to the start of the current
/// processing window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MidiEvent {
    pub frame: usize,
    pub msg: MidiMessage,
}

impl MidiEvent {
    /// Sort key: by time, and note-offs before note-ons at the same frame,
    /// so a repeated note releases before it retriggers.
    fn order(&self) -> (usize, u8) {
        (self.frame, matches!(self.msg, MidiMessage::NoteOn { .. }) as u8)
    }
}

/// A fixed-capacity list of events, reused every block.
pub struct EventBuffer {
    events: Vec<MidiEvent>,
}

impl EventBuffer {
    pub fn new(capacity: usize) -> Self {
        Self { events: Vec::with_capacity(capacity) }
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Never allocates: if it's full, the event is dropped. (Hundreds of
    /// notes in one block is not music anyway.)
    pub fn push(&mut self, frame: usize, msg: MidiMessage) {
        if self.events.len() < self.events.capacity() {
            self.events.push(MidiEvent { frame, msg });
        }
    }

    pub fn sort(&mut self) {
        // `sort_unstable` sorts in place. The stable `sort` may allocate a
        // scratch buffer, which is not allowed on the audio thread.
        self.events.sort_unstable_by_key(|e| e.order());
    }

    pub fn as_slice(&self) -> &[MidiEvent] {
        &self.events
    }
}

/// A note from live input, stamped when it arrived.
#[derive(Clone, Copy, Debug)]
pub struct LiveMidi {
    pub at: Instant,
    pub msg: MidiMessage,
}

/// A note inside a MIDI clip, relative to the clip's start (in beats).
#[derive(Clone, Copy, Debug)]
pub struct Note {
    pub start: f64,
    pub length: f64,
    pub pitch: u8,
    pub velocity: u8,
}

/// A track's MIDI clips flattened into one timeline: every note-on and
/// note-off at its absolute beat, sorted. Built on the main thread, so the
/// audio thread never thinks about clips, only about "what happens in this
/// block" (two binary searches).
pub struct Sequence {
    ons: Vec<(f64, u8, u8)>, // (beat, pitch, velocity)
    offs: Vec<(f64, u8)>,    // (beat, pitch)
}

impl Sequence {
    pub fn empty() -> Self {
        Self { ons: Vec::new(), offs: Vec::new() }
    }

    /// `clips` are (clip start, clip length, notes). Notes are cut off at
    /// the clip's end, so shortening a clip shortens its notes too.
    pub fn build<'a>(clips: impl IntoIterator<Item = (f64, f64, &'a [Note])>) -> Self {
        let mut seq = Self::empty();
        for (clip_start, clip_len, notes) in clips {
            for n in notes {
                if n.start < 0.0 || n.start >= clip_len || n.length <= 0.0 {
                    continue;
                }
                let end = (n.start + n.length).min(clip_len);
                seq.ons.push((clip_start + n.start, n.pitch, n.velocity));
                seq.offs.push((clip_start + end, n.pitch));
            }
        }
        seq.ons.sort_by(|a, b| a.0.total_cmp(&b.0));
        seq.offs.sort_by(|a, b| a.0.total_cmp(&b.0));
        seq
    }
}

/// Plays a `Sequence` and keeps track of which notes it has turned on, so
/// it can turn them off again when time jumps.
pub struct Sequencer {
    sequence: Box<Sequence>,
    /// The beat where the next window should start if time is continuous.
    /// Measured in beats, not samples: a tempo change rescales the sample
    /// position but is not a jump in the music, so held notes must survive.
    expected_beat: Option<f64>,
    held: [bool; 128],
    /// Set when the sequence is replaced: a held note may have been deleted
    /// by the edit, and then its note-off would never arrive.
    release_pending: bool,
}

impl Sequencer {
    pub fn new() -> Self {
        Self {
            sequence: Box::new(Sequence::empty()),
            expected_beat: None,
            held: [false; 128],
            release_pending: false,
        }
    }

    /// Swaps in a new sequence; returns the old one to be freed elsewhere.
    pub fn replace(&mut self, sequence: Box<Sequence>) -> Box<Sequence> {
        // Releasing every held note on an edit is the safe choice: it may
        // cut a note that still exists, but it can never leave one droning.
        // (Note chasing would restart the ones that still exist.)
        self.release_pending = true;
        std::mem::replace(&mut self.sequence, sequence)
    }

    fn release_held(&mut self, frame: usize, out: &mut EventBuffer) {
        for (note, held) in self.held.iter_mut().enumerate() {
            if *held {
                out.push(frame, MidiMessage::NoteOff { note: note as u8 });
                *held = false;
            }
        }
    }

    /// Adds this window's events to `out`.
    pub fn events(&mut self, ctx: &ProcessContext, frames: usize, out: &mut EventBuffer) {
        if !ctx.playing {
            // Stopped: release whatever we were holding, exactly once.
            if self.expected_beat.take().is_some() || self.release_pending {
                self.release_held(0, out);
            }
            self.release_pending = false;
            return;
        }

        let spb = ctx.samples_per_beat();
        let beat = ctx.position as f64 / spb;
        // A seek or loop wrap: notes we started are now meaningless, and
        // their note-offs will never be reached. Release them first. (Up to
        // a sample of difference is rounding from a tempo change.)
        let jumped = self.expected_beat.is_some_and(|e| ((beat - e) * spb).abs() > 1.0);
        if jumped || self.release_pending {
            self.release_held(0, out);
            self.release_pending = false;
        }
        self.expected_beat = Some((ctx.position + frames as u64) as f64 / spb);

        let start = ctx.position as f64;
        let end = start + frames as f64;
        // An event at beat b sounds on the first sample at or after it.
        let at = |beat: f64| (beat * spb).ceil();
        let frame = |beat: f64| ((at(beat) - start).max(0.0) as usize).min(frames - 1);

        let seq = &self.sequence;
        let (mut i, off_end) = (
            seq.offs.partition_point(|e| at(e.0) < start),
            seq.offs.partition_point(|e| at(e.0) < end),
        );
        let (mut j, on_end) = (
            seq.ons.partition_point(|e| at(e.0) < start),
            seq.ons.partition_point(|e| at(e.0) < end),
        );

        // Walk both lists in time order, like merging two sorted lists. A
        // note can start AND end inside one window, so handling all offs
        // before all ons would skip its off (it isn't held yet) and leave it
        // hanging. At the same sample, offs go first so repeats retrigger.
        while i < off_end || j < on_end {
            let take_off = j == on_end || (i < off_end && at(seq.offs[i].0) <= at(seq.ons[j].0));
            if take_off {
                let (b, note) = seq.offs[i];
                i += 1;
                if self.held[note as usize] {
                    out.push(frame(b), MidiMessage::NoteOff { note });
                    self.held[note as usize] = false;
                }
            } else {
                let (b, note, velocity) = seq.ons[j];
                j += 1;
                out.push(frame(b), MidiMessage::NoteOn { note, velocity });
                self.held[note as usize] = true;
            }
        }
    }
}

/// Note names like "C4", "F#3", "Bb2" (C4 = 60, middle C), or a number.
pub fn parse_pitch(s: &str) -> Option<u8> {
    if let Ok(n) = s.parse::<u8>() {
        return (n < 128).then_some(n);
    }
    let mut chars = s.chars();
    let base = match chars.next()?.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let rest: String = chars.collect();
    let (shift, octave) = match rest.chars().next()? {
        '#' => (1, &rest[1..]),
        'b' => (-1, &rest[1..]),
        _ => (0, rest.as_str()),
    };
    let octave: i32 = octave.parse().ok()?;
    let n = (octave + 1) * 12 + base + shift;
    (0..128).contains(&n).then_some(n as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    #[test]
    fn parses_messages() {
        assert_eq!(MidiMessage::parse(&[0x90, 60, 100]), Some(MidiMessage::NoteOn { note: 60, velocity: 100 }));
        assert_eq!(MidiMessage::parse(&[0x93, 60, 0]), Some(MidiMessage::NoteOff { note: 60 }));
        assert_eq!(MidiMessage::parse(&[0x80, 60, 64]), Some(MidiMessage::NoteOff { note: 60 }));
        assert_eq!(MidiMessage::parse(&[0xB0, 7, 100]), None);
        assert_eq!(MidiMessage::parse(&[0xF8]), None);
    }

    #[test]
    fn parses_pitches() {
        assert_eq!(parse_pitch("C4"), Some(60));
        assert_eq!(parse_pitch("A4"), Some(69));
        assert_eq!(parse_pitch("F#3"), Some(54));
        assert_eq!(parse_pitch("Bb2"), Some(46));
        assert_eq!(parse_pitch("C-1"), Some(0));
        assert_eq!(parse_pitch("64"), Some(64));
        assert_eq!(parse_pitch("H4"), None);
    }

    fn collect(seqr: &mut Sequencer, t: &mut Transport, blocks: usize, n: usize) -> Vec<(u64, MidiMessage)> {
        let mut all = Vec::new();
        let mut buf = EventBuffer::new(64);
        for _ in 0..blocks {
            buf.clear();
            let ctx = t.context();
            seqr.events(&ctx, n, &mut buf);
            buf.sort();
            all.extend(buf.as_slice().iter().map(|e| (ctx.position + e.frame as u64, e.msg)));
            t.advance(n);
        }
        all
    }

    #[test]
    fn events_land_on_exact_samples_across_blocks() {
        // 130 BPM: a beat is 22153.846 samples, so beats fall between samples.
        let notes = [Note { start: 1.0, length: 0.5, pitch: 60, velocity: 100 }];
        let mut seqr = Sequencer::new();
        seqr.replace(Box::new(Sequence::build([(0.0, 4.0, &notes[..])])));
        let mut t = Transport::new(48_000.0);
        t.set_tempo(130.0);
        t.playing = true;
        let events = collect(&mut seqr, &mut t, 200, 333);
        let spb = 48_000.0 * 60.0 / 130.0;
        assert_eq!(
            events,
            vec![
                ((1.0f64 * spb).ceil() as u64, MidiMessage::NoteOn { note: 60, velocity: 100 }),
                ((1.5f64 * spb).ceil() as u64, MidiMessage::NoteOff { note: 60 }),
            ]
        );
    }

    #[test]
    fn notes_are_cut_at_the_clip_end() {
        let notes = [Note { start: 0.0, length: 10.0, pitch: 60, velocity: 100 }];
        let seq = Sequence::build([(4.0, 2.0, &notes[..])]);
        assert_eq!(seq.offs, vec![(6.0, 60)]);
    }

    #[test]
    fn seeking_releases_held_notes() {
        let notes = [Note { start: 0.0, length: 4.0, pitch: 64, velocity: 90 }];
        let mut seqr = Sequencer::new();
        seqr.replace(Box::new(Sequence::build([(0.0, 4.0, &notes[..])])));
        let mut t = Transport::new(48_000.0);
        t.playing = true;
        collect(&mut seqr, &mut t, 1, 512); // note-on
        t.seek_beats(10.0);
        let events = collect(&mut seqr, &mut t, 1, 512);
        assert_eq!(events, vec![(240_000, MidiMessage::NoteOff { note: 64 })]);
    }

    #[test]
    fn loops_retrigger_and_stop_releases() {
        let notes = [Note { start: 0.0, length: 2.0, pitch: 67, velocity: 80 }];
        let mut seqr = Sequencer::new();
        seqr.replace(Box::new(Sequence::build([(0.0, 4.0, &notes[..])])));
        let mut t = Transport::new(48_000.0);
        t.set_loop(Some((0.0, 1.0))); // the note is longer than the loop
        t.playing = true;
        let mut events = Vec::new();
        let mut buf = EventBuffer::new(64);
        for _ in 0..100 {
            let n = t.frames_until_boundary(512);
            buf.clear();
            seqr.events(&t.context(), n, &mut buf);
            buf.sort();
            events.extend(buf.as_slice().iter().map(|e| e.msg));
            t.advance(n);
        }
        let ons = events.iter().filter(|m| matches!(m, MidiMessage::NoteOn { .. })).count();
        let offs = events.iter().filter(|m| matches!(m, MidiMessage::NoteOff { .. })).count();
        assert_eq!(ons, 3, "100 blocks of 512 at 24000/beat = 2.13 loops -> 3 starts");
        assert_eq!(offs, 2, "each wrap releases the held note");

        t.playing = false;
        buf.clear();
        seqr.events(&t.context(), 512, &mut buf);
        assert_eq!(buf.as_slice(), &[MidiEvent { frame: 0, msg: MidiMessage::NoteOff { note: 67 } }]);
    }

    // ---- Regression tests (from the code review) ----

    fn one_note(start: f64, length: f64) -> Sequencer {
        let notes = [Note { start, length, pitch: 60, velocity: 100 }];
        let mut seqr = Sequencer::new();
        seqr.replace(Box::new(Sequence::build([(0.0, 8.0, &notes[..])])));
        seqr
    }

    fn held_after(seqr: &Sequencer) -> bool {
        seqr.held[60]
    }

    #[test]
    fn regression_note_shorter_than_a_block_is_released() {
        // On at sample 1000, off at sample 1012: both in the same 512-block.
        let mut seqr = one_note(1000.0 / 24_000.0, 12.0 / 24_000.0);
        let mut t = Transport::new(48_000.0);
        t.playing = true;
        let events = collect(&mut seqr, &mut t, 4, 512);
        assert_eq!(events.len(), 2, "got {events:?}");
        assert!(!held_after(&seqr), "note left hanging");
    }

    #[test]
    fn regression_tempo_change_keeps_held_notes() {
        let mut seqr = one_note(0.0, 4.0);
        let mut t = Transport::new(48_000.0);
        t.playing = true;
        collect(&mut seqr, &mut t, 10, 512);
        t.set_tempo(90.0);
        let events = collect(&mut seqr, &mut t, 1, 512);
        assert!(events.is_empty(), "a tempo change released the note: {events:?}");
    }

    #[test]
    fn regression_editing_releases_notes_that_may_no_longer_exist() {
        let mut seqr = one_note(0.0, 4.0);
        let mut t = Transport::new(48_000.0);
        t.playing = true;
        collect(&mut seqr, &mut t, 10, 512);
        // The user deletes the clip while its note is sounding.
        seqr.replace(Box::new(Sequence::empty()));
        let events = collect(&mut seqr, &mut t, 1, 512);
        assert_eq!(events.first().map(|e| e.1), Some(MidiMessage::NoteOff { note: 60 }), "note would drone forever");
    }
}
