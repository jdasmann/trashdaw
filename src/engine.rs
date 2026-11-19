use crate::buffer::AudioBuffer;
use crate::automation::AutomationSet;
use crate::clip::ClipList;
use crate::record::{take_start, RecEvent, RecordShared, CHUNK};
use crate::midi::{LiveMidi, MidiMessage, Sequence, SUSTAIN_PEDAL};
use crate::mixer::{MeterPoint, Meters, Target, Track, MAX_TRACKS, MIX_CHANNELS};
use crate::processor::{Instrument, PrepareInfo, Processor};
use crate::transport::{Transport, TransportShared};
use cpal::{FromSample, Sample};
use rtrb::{Consumer, Producer};
use std::sync::Arc;
use std::time::Instant;

/// Messages from the control thread to the audio thread.
pub enum Command {
    /// Tracks, like nodes, are built and allocated on the main thread.
    AddTrack { index: usize, track: Box<Track> },
    RemoveTrack { index: usize },
    /// A complete new clip list for a track, replacing the old one.
    SetClips { track: usize, clips: Box<ClipList> },
    /// A track's automation lanes, replacing the old ones.
    SetAutomation { track: usize, automation: Box<AutomationSet> },
    /// A track's MIDI clips, compiled into one note timeline.
    SetSequence { track: usize, sequence: Box<Sequence> },
    SetInstrument { track: usize, instrument: Option<Box<dyn Instrument>> },
    SetInstrumentParam { track: usize, param: usize, value: f32 },
    /// Which track receives live MIDI input.
    Arm { track: Option<usize> },
    /// The track that records from the input (None = not armed).
    SetRecordTrack { track: Option<usize> },
    /// Manual latency compensation, in frames, added to what the driver reports.
    SetLatencyAdjust { frames: i64 },
    /// Record only inside this range of beats (None = everywhere).
    SetPunch { range: Option<(f64, f64)> },
    /// Hear the input through the armed track.
    SetMonitor { on: bool },
    /// Which track's signal `track`'s nodes may listen to (sidechain).
    SetSidechain { track: usize, source: Option<usize> },
    /// The node arrives already constructed and prepared. Moving a Box
    /// through the queue is just moving a pointer: no allocation.
    Insert { target: Target, slot: usize, node: Box<dyn Processor> },
    Remove { target: Target, slot: usize },
    SetParam { target: Target, slot: usize, param: usize, value: f32 },
    // Mixer
    SetVolume { target: Target, db: f32 },
    SetPan { track: usize, pos: f32 },
    SetMute { track: usize, on: bool },
    SetSolo { track: usize, on: bool },
    SetSend { track: usize, db: f32 },
    // Transport
    Play,
    Stop,
    Seek { beat: f64 },
    SetTempo { bpm: f64 },
    SetLoop { range: Option<(f64, f64)> },
}

/// Things the audio thread is done with, sent back to be freed.
///
/// Nothing ever reads these payloads: they exist only so they can be
/// DROPPED on the main thread, because freeing memory isn't real-time safe
/// on the audio thread. The compiler rightly notices the fields are never
/// read, so the warning is silenced on purpose. (Its suggestion, replacing
/// the fields with `()`, would free them on the audio thread instead:
/// exactly what this type exists to prevent.)
#[allow(dead_code)]
pub enum Garbage {
    Node(Box<dyn Processor>),
    Track(Box<Track>),
    Clips(Box<ClipList>),
    Sequence(Box<Sequence>),
    Automation(Box<AutomationSet>),
    Instrument(Box<dyn Instrument>),
}

pub struct Engine {
    tracks: Vec<Option<Box<Track>>>,
    aux: Track,
    master: Track,
    max_frames: usize,
    device_channels: usize,
    transport: Transport,
    shared: Arc<TransportShared>,
    meters: Arc<Meters>,
    commands: Consumer<Command>,
    garbage: Producer<Garbage>,
    // --- Live MIDI ---
    live_rx: Consumer<LiveMidi>,
    /// This callback's live events as (frame within the callback, message).
    live: Vec<(usize, MidiMessage)>,
    armed: Option<usize>,
    /// Live notes currently held down on the armed track.
    live_held: [bool; 128],
    /// Whether the armed track's sustain pedal is down.
    live_pedal: bool,
    /// When the armed track changes while keys are held, the old track
    /// gets these note-offs, or its notes would never be released.
    release_on: Option<usize>,
    release_events: Vec<(usize, MidiMessage)>,
    last_callback: Option<Instant>,
    sample_rate: f64,
    // --- Sidechains ---
    /// keys[t] = the track whose signal track t listens to.
    keys: [Option<usize>; MAX_TRACKS],
    /// Tracks in the order they must render: every key source before the
    /// tracks that listen to it. Rebuilt only when a sidechain changes.
    order: Vec<usize>,
    /// A copy of each key source's output for the current window. Copying
    /// sidesteps borrowing two tracks at once, and costs very little.
    key_buffers: Vec<AudioBuffer>,
    is_key_source: [bool; MAX_TRACKS],
    // --- Recording ---
    input: Option<Input>,
    rec_track: Option<usize>,
    take: Option<TakeState>,
    next_take: u32,
    latency_adjust: i64,
    /// Punch range in beats: record only inside it.
    punch: Option<(f64, f64)>,
    monitor: bool,
    monitor_fifo: MonitorFifo,
    monitor_l: Vec<f32>,
    monitor_r: Vec<f32>,
}

/// The input side, wired up only when there's an input device.
struct Input {
    /// Interleaved samples from the input callback.
    samples: Consumer<f32>,
    channels: usize,
    recorder: Producer<RecEvent>,
    shared: Arc<RecordShared>,
    /// The chunk being filled for the disk writer.
    left: [f32; CHUNK],
    right: [f32; CHUNK],
    len: usize,
}

impl Input {
    /// Never blocks: if the disk writer has fallen behind, drop and count.
    fn send(&mut self, ev: RecEvent) {
        if self.recorder.push(ev).is_err() {
            self.shared.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn flush(&mut self) {
        if self.len > 0 {
            let ev = RecEvent::Samples { len: self.len, left: self.left, right: self.right };
            self.send(ev);
            self.len = 0;
        }
    }
}

#[derive(Clone, Copy)]
struct TakeState {
    id: u32,
    track: usize,
    /// Where the transport should be at the next callback if time is
    /// continuous. A loop wrap or seek ends the take and starts a new one.
    expected: u64,
    /// The timeline position of the take's next frame.
    next_pos: i64,
}

/// Sends the partly filled chunk to the disk writer, if there is one.
fn flush_chunk(recorder: &mut Producer<RecEvent>, shared: &RecordShared, left: &[f32; CHUNK], right: &[f32; CHUNK], len: &mut usize) {
    if *len > 0 {
        if recorder.push(RecEvent::Samples { len: *len, left: *left, right: *right }).is_err() {
            shared.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        *len = 0;
    }
}

/// Live input on its way to the armed track, for monitoring. Input arrives
/// in bursts that don't line up with output blocks, so it waits here: a
/// fixed-size ring (no allocation) of stereo frames.
struct MonitorFifo {
    left: Vec<f32>,
    right: Vec<f32>,
    read: usize,
    len: usize,
}

impl MonitorFifo {
    fn new(capacity: usize) -> Self {
        Self { left: vec![0.0; capacity], right: vec![0.0; capacity], read: 0, len: 0 }
    }

    fn push(&mut self, l: f32, r: f32) {
        let cap = self.left.len();
        if self.len == cap {
            // Full: drop the oldest frame rather than grow the delay.
            self.read = (self.read + 1) % cap;
            self.len -= 1;
        }
        let write = (self.read + self.len) % cap;
        self.left[write] = l;
        self.right[write] = r;
        self.len += 1;
    }

    /// Fills `l` and `r` with the oldest frames; silence if it runs dry.
    fn pop_into(&mut self, l: &mut [f32], r: &mut [f32]) {
        let cap = self.left.len();
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            if self.len == 0 {
                (*a, *b) = (0.0, 0.0);
                continue;
            }
            (*a, *b) = (self.left[self.read], self.right[self.read]);
            self.read = (self.read + 1) % cap;
            self.len -= 1;
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    /// Keeps the delay to at most `max` frames: monitoring must stay tight.
    fn trim_to(&mut self, max: usize) {
        while self.len > max {
            self.read = (self.read + 1) % self.left.len();
            self.len -= 1;
        }
    }
}

impl Engine {
    pub fn new(
        info: &PrepareInfo,
        device_channels: usize,
        shared: Arc<TransportShared>,
        meters: Arc<Meters>,
        commands: Consumer<Command>,
        garbage: Producer<Garbage>,
        live_rx: Consumer<LiveMidi>,
    ) -> Self {
        let mut tracks = Vec::with_capacity(MAX_TRACKS);
        tracks.resize_with(MAX_TRACKS, || None);
        Self {
            tracks,
            aux: Track::new(info),
            master: Track::new(info),
            max_frames: info.max_frames,
            device_channels,
            transport: Transport::new(info.sample_rate as f64),
            shared,
            meters,
            commands,
            garbage,
            live_rx,
            live: Vec::with_capacity(256),
            armed: None,
            live_held: [false; 128],
            live_pedal: false,
            release_on: None,
            release_events: Vec::with_capacity(128),
            last_callback: None,
            sample_rate: info.sample_rate as f64,
            keys: [None; MAX_TRACKS],
            order: (0..MAX_TRACKS).collect(),
            key_buffers: (0..MAX_TRACKS).map(|_| AudioBuffer::new(MIX_CHANNELS, info.max_frames)).collect(),
            is_key_source: [false; MAX_TRACKS],
            input: None,
            rec_track: None,
            take: None,
            next_take: 1,
            latency_adjust: 0,
            punch: None,
            monitor: false,
            monitor_fifo: MonitorFifo::new(8192),
            monitor_l: vec![0.0; info.max_frames],
            monitor_r: vec![0.0; info.max_frames],
        }
    }

    /// Connects an input device. `samples` must hold whole frames (its
    /// capacity a multiple of `channels`), so a frame never straddles the
    /// ring's wrap-around point.
    pub fn set_input(&mut self, samples: Consumer<f32>, channels: usize, recorder: Producer<RecEvent>, shared: Arc<RecordShared>) {
        self.input = Some(Input { samples, channels: channels.max(1), recorder, shared, left: [0.0; CHUNK], right: [0.0; CHUNK], len: 0 });
    }

    fn end_take(&mut self) {
        if let (Some(take), Some(input)) = (self.take.take(), self.input.as_mut()) {
            input.flush();
            input.send(RecEvent::End { take: take.id });
            input.shared.recording.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Reads everything the input callback has delivered. Runs at the start
    /// of every callback, recording or not: draining the ring is what keeps
    /// input latency from building up.
    ///
    /// Each input frame gets a timeline position: the latency-compensated
    /// one, meaning what the player was hearing when they played it. A take
    /// includes exactly the frames whose position is inside the punch range
    /// (or all of them, without one), so it starts and ends on the exact
    /// frame, even when a callback straddles the punch point.
    fn capture_input(&mut self, frames: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(input) = self.input.as_ref() else { return };
        let channels = input.channels;
        let waiting = input.samples.slots() / channels;
        let (in_lat, out_lat) = (input.shared.input_latency.load(Relaxed), input.shared.output_latency.load(Relaxed));
        let position = self.transport.position;
        let spb = self.transport.context().samples_per_beat();
        let punch = self.punch.map(|(a, b)| ((a * spb).round() as i64, (b * spb).round() as i64));
        let active = self.transport.playing && self.rec_track.is_some();

        // A jump (loop wrap, seek), a stop, or a change of armed track ends
        // the take in progress.
        let continuous = self.take.is_some_and(|t| t.expected == position && Some(t.track) == self.rec_track);
        if self.take.is_some() && (!active || !continuous) {
            self.end_take();
        }
        // Inside a take, positions continue by counting frames, so the take
        // has no gaps or overlaps. A new one starts from the latency math.
        let mut next_pos = match self.take {
            Some(t) => t.next_pos,
            None => take_start(position, waiting, in_lat, out_lat, self.latency_adjust),
        };
        let monitoring = self.monitor && self.rec_track.is_some();

        // Borrow the fields separately: the chunk being read borrows the
        // input ring while samples go elsewhere.
        let Input { samples, recorder, shared, left, right, len, .. } = self.input.as_mut().unwrap();
        let Ok(chunk) = samples.read_chunk(waiting * channels) else { return };
        let (a, b) = chunk.as_slices();
        let mut peak = 0.0f32;
        for frame in a.chunks_exact(channels).chain(b.chunks_exact(channels)) {
            // Mono inputs feed both sides; extra channels are ignored.
            let (l, r) = (frame[0], frame[1.min(channels - 1)]);
            peak = peak.max(l.abs()).max(r.abs());
            if monitoring {
                self.monitor_fifo.push(l, r);
            }
            let pos = next_pos;
            next_pos += 1;
            let wanted = active && punch.map_or(true, |(start, end)| pos >= start && pos < end);
            match (wanted, self.take.is_some()) {
                (true, false) => {
                    // Punch in (or simply start), on this exact frame.
                    let (id, track) = (self.next_take, self.rec_track.unwrap());
                    self.next_take += 1;
                    if recorder.push(RecEvent::Begin { take: id, track, start_frame: pos }).is_err() {
                        shared.dropped.fetch_add(1, Relaxed);
                    }
                    shared.recording.store(true, Relaxed);
                    self.take = Some(TakeState { id, track, expected: 0, next_pos: pos });
                }
                (false, true) => {
                    // Punch out, on this exact frame.
                    let id = self.take.take().unwrap().id;
                    flush_chunk(recorder, shared, left, right, len);
                    if recorder.push(RecEvent::End { take: id }).is_err() {
                        shared.dropped.fetch_add(1, Relaxed);
                    }
                    shared.recording.store(false, Relaxed);
                }
                _ => {}
            }
            if wanted {
                left[*len] = l;
                right[*len] = r;
                *len += 1;
                if *len == CHUNK {
                    flush_chunk(recorder, shared, left, right, len);
                }
            }
        }
        chunk.commit_all();
        shared.record_peak(peak);
        // For clock-drift measurement: frames received vs frames rendered.
        shared.frames_in.fetch_add(waiting as u64, Relaxed);
        shared.frames_out.fetch_add(frames as u64, Relaxed);
        if let Some(t) = self.take.as_mut() {
            t.expected = position + frames as u64;
            t.next_pos = next_pos;
        }
    }

    /// Orders tracks so each key source renders before its listeners. Each
    /// track has at most one source, so this just walks each chain of
    /// sources to its top and adds it top-down. Fixed-size arrays only:
    /// it runs on the audio thread, so it must not allocate. (`order` has
    /// capacity for every track, so its pushes don't either.)
    fn rebuild_order(&mut self) {
        self.order.clear();
        let mut done = [false; MAX_TRACKS];
        for start in 0..MAX_TRACKS {
            let mut chain = [0usize; MAX_TRACKS];
            let mut len = 0;
            let mut current = Some(start);
            while let Some(t) = current {
                // Stop at anything already placed, or at a cycle (which the
                // main thread refuses to create, but never trust that here).
                if done[t] || chain[..len].contains(&t) {
                    break;
                }
                chain[len] = t;
                len += 1;
                current = self.keys[t];
            }
            for &t in chain[..len].iter().rev() {
                done[t] = true;
                self.order.push(t);
            }
        }
        self.is_key_source = [false; MAX_TRACKS];
        for source in self.keys.iter().flatten() {
            self.is_key_source[*source] = true;
        }
    }

    fn target_mut(&mut self, target: Target) -> Option<&mut Track> {
        match target {
            Target::Master => Some(&mut self.master),
            Target::Aux => Some(&mut self.aux),
            Target::Track(i) => self.tracks.get_mut(i)?.as_deref_mut(),
        }
    }

    fn track_mut(&mut self, index: usize) -> Option<&mut Track> {
        self.target_mut(Target::Track(index))
    }

    fn handle_commands(&mut self) {
        while let Ok(cmd) = self.commands.pop() {
            match cmd {
                Command::AddTrack { index, track } => {
                    if let Some(old) = self.tracks[index].replace(track) {
                        self.retire(Garbage::Track(old));
                    }
                }
                Command::RemoveTrack { index } => {
                    if self.rec_track == Some(index) {
                        self.end_take();
                        self.rec_track = None;
                    }
                    // Forget any sidechain to or from the removed track.
                    if self.keys[index].is_some() || self.is_key_source[index] {
                        self.keys[index] = None;
                        for key in &mut self.keys {
                            if *key == Some(index) {
                                *key = None;
                            }
                        }
                        self.rebuild_order();
                    }
                    if self.armed == Some(index) {
                        // Otherwise a new track created in this slot would
                        // silently be armed. Its notes go with the track.
                        self.armed = None;
                        self.live_held = [false; 128];
                        self.live_pedal = false;
                    }
                    // The track's nodes go with it, freed on the main thread.
                    if let Some(old) = self.tracks[index].take() {
                        self.retire(Garbage::Track(old));
                    }
                }
                Command::SetClips { track, clips } => {
                    // Swapping a Box is a pointer swap. The old list (and
                    // the Arcs inside it) is freed on the main thread.
                    let old = match self.track_mut(track) {
                        Some(t) => t.replace_clips(clips),
                        None => clips, // track gone: send it straight back
                    };
                    self.retire(Garbage::Clips(old));
                }
                Command::SetAutomation { track, automation } => {
                    let old = match self.track_mut(track) {
                        Some(t) => t.replace_automation(automation),
                        None => automation,
                    };
                    self.retire(Garbage::Automation(old));
                }
                Command::SetSequence { track, sequence } => {
                    let old = match self.track_mut(track) {
                        Some(t) => t.replace_sequence(sequence),
                        None => sequence,
                    };
                    self.retire(Garbage::Sequence(old));
                }
                Command::SetInstrument { track, instrument } => {
                    let old = match self.track_mut(track) {
                        Some(t) => t.replace_instrument(instrument),
                        None => instrument,
                    };
                    if let Some(old) = old {
                        self.retire(Garbage::Instrument(old));
                    }
                }
                Command::SetInstrumentParam { track, param, value } => {
                    if let Some(inst) = self.track_mut(track).and_then(|t| t.instrument_mut()) {
                        inst.set_param(param, value);
                    }
                }
                Command::SetRecordTrack { track } => self.rec_track = track,
                Command::SetLatencyAdjust { frames } => self.latency_adjust = frames,
                Command::SetPunch { range } => self.punch = range,
                Command::SetMonitor { on } => {
                    self.monitor = on;
                    self.monitor_fifo.clear();
                }
                Command::SetSidechain { track, source } => {
                    if track < MAX_TRACKS && source.map_or(true, |s| s < MAX_TRACKS && s != track) {
                        self.keys[track] = source;
                        self.rebuild_order();
                    }
                }
                Command::Arm { track } => {
                    // (If a release is already waiting to be delivered, keep
                    // it: nothing new can be held until the next callback.)
                    if track != self.armed && self.release_on.is_none() {
                        self.release_events.clear();
                        if let Some(old) = self.armed {
                            // Lift the old track's pedal first (releasing
                            // notes it was sustaining), then stop held keys.
                            // ForceOff, because these are clean-up.
                            if self.live_pedal {
                                self.release_events
                                    .push((0, MidiMessage::ControlChange { controller: SUSTAIN_PEDAL, value: 0 }));
                                self.live_pedal = false;
                            }
                            for (note, held) in self.live_held.iter_mut().enumerate() {
                                if *held {
                                    self.release_events.push((0, MidiMessage::ForceOff { note: note as u8 }));
                                    *held = false;
                                }
                            }
                            self.release_on = (!self.release_events.is_empty()).then_some(old);
                        }
                    }
                    self.armed = track;
                }
                Command::Insert { target, slot, node } => {
                    let old = match self.target_mut(target) {
                        Some(track) => track.slot_mut(slot).replace(node),
                        None => Some(node), // track gone: send the node back
                    };
                    if let Some(old) = old {
                        self.retire(Garbage::Node(old));
                    }
                }
                Command::Remove { target, slot } => {
                    if let Some(old) = self.target_mut(target).and_then(|t| t.slot_mut(slot).take()) {
                        self.retire(Garbage::Node(old));
                    }
                }
                Command::SetParam { target, slot, param, value } => {
                    if let Some(node) = self.target_mut(target).and_then(|t| t.slot_mut(slot).as_mut()) {
                        node.set_param(param, value);
                    }
                }
                Command::SetVolume { target, db } => {
                    if let Some(t) = self.target_mut(target) {
                        t.set_volume_db(db);
                    }
                }
                Command::SetPan { track, pos } => {
                    if let Some(t) = self.track_mut(track) {
                        t.set_pan(pos);
                    }
                }
                Command::SetMute { track, on } => {
                    if let Some(t) = self.track_mut(track) {
                        t.mute = on;
                    }
                }
                Command::SetSolo { track, on } => {
                    if let Some(t) = self.track_mut(track) {
                        t.solo = on;
                    }
                }
                Command::SetSend { track, db } => {
                    if let Some(t) = self.track_mut(track) {
                        t.set_send_db(db);
                    }
                }
                Command::Play => self.transport.playing = true,
                Command::Stop => self.transport.playing = false,
                Command::Seek { beat } => self.transport.seek_beats(beat),
                Command::SetTempo { bpm } => self.transport.set_tempo(bpm),
                Command::SetLoop { range } => self.transport.set_loop(range),
            }
        }
    }

    /// Dropping a Box frees memory, which is not real-time safe, so we
    /// hand old nodes and tracks back to the main thread instead.
    fn retire(&mut self, item: Garbage) {
        // If the queue is somehow full the item is dropped here. That is
        // rare and only a glitch risk; size the queue generously.
        let _ = self.garbage.push(item);
    }

    /// Turns live MIDI arrival times into sample positions in this callback.
    ///
    /// We can't play an event at the instant it arrived: this callback is
    /// computing audio that will be heard a little later. Placing every
    /// event at the start of the callback would make timing jitter by up
    /// to a whole buffer. Instead, each event is played exactly as long
    /// after the start of this callback as it arrived after the start of
    /// the previous one: a fixed one-buffer delay, with no jitter.
    /// (`Instant::now` is a cheap clock read, fine on the audio thread.)
    fn collect_live_midi(&mut self, frames: usize) {
        let now = Instant::now();
        self.live.clear();
        while let Ok(ev) = self.live_rx.pop() {
            let frame = match self.last_callback {
                Some(prev) => (ev.at.saturating_duration_since(prev).as_secs_f64() * self.sample_rate) as usize,
                None => 0,
            };
            if self.live.len() < self.live.capacity() {
                self.live.push((frame.min(frames.saturating_sub(1)), ev.msg));
                match ev.msg {
                    MidiMessage::NoteOn { note, .. } => self.live_held[note as usize] = true,
                    MidiMessage::NoteOff { note } | MidiMessage::ForceOff { note } => {
                        self.live_held[note as usize] = false
                    }
                    MidiMessage::ControlChange { controller: SUSTAIN_PEDAL, value } => self.live_pedal = value >= 64,
                    MidiMessage::ControlChange { .. } => {}
                }
            }
        }
        self.last_callback = Some(now);
    }

    /// Called from the cpal callback with an interleaved device buffer.
    pub fn render<T>(&mut self, out: &mut [T])
    where
        T: Sample + FromSample<f32>,
    {
        // Flush denormals to zero for this whole callback (see denormal.rs).
        let _no_denormals = crate::denormal::NoDenormals::new();

        // Commands take effect at the start of a callback, so they are
        // quantized to the device buffer size. Fine for UI actions.
        self.handle_commands();

        let dev_ch = self.device_channels;
        self.collect_live_midi(out.len() / dev_ch);
        self.capture_input(out.len() / dev_ch);
        // At most two callbacks' worth waiting: bursts are absorbed, but
        // the monitoring delay can't creep up.
        self.monitor_fifo.trim_to(2 * (out.len() / dev_ch));
        let mut block_start = 0; // frame of this block within the callback
        // Solo state can only change between callbacks, so check it once.
        let any_solo = self.tracks.iter().flatten().any(|t| t.solo);

        for block in out.chunks_mut(self.max_frames * dev_ch) {
            let frames = block.len() / dev_ch;
            for bus in [self.aux.bus(), self.master.bus()] {
                bus.set_window(0, frames);
                bus.clear();
            }

            // Split the block wherever time jumps (the loop end), so each
            // processor call sees contiguous time.
            let mut offset = 0;
            while offset < frames {
                let n = self.transport.frames_until_boundary(frames - offset);
                let ctx = self.transport.context();

                // Render order matters:
                //   1. every track -> master bus (direct) and aux bus (send)
                //   2. aux chain runs on the complete send mix -> master
                //   3. master chain runs on everything
                // (`self.master`, `self.aux` and `self.tracks` are separate
                // fields, so borrowing them mutably at once is fine.)
                let master_bus = self.master.bus();
                let aux_bus = self.aux.bus();
                master_bus.set_window(offset, n);
                aux_bus.set_window(offset, n);
                // Live events that fall in this window go to the armed track.
                let window = block_start + offset;
                let lo = self.live.partition_point(|e| e.0 < window);
                let hi = self.live.partition_point(|e| e.0 < window + n);
                for k in 0..self.order.len() {
                    let i = self.order[k];
                    // Only listen to a source that exists (and so rendered).
                    let key_source = self.keys[i].filter(|&s| self.tracks[s].is_some());
                    if let Some(track) = self.tracks[i].as_mut() {
                        let key = key_source.map(|s| &self.key_buffers[s]);
                        let live = if self.armed == Some(i) {
                            &self.live[lo..hi]
                        } else if self.release_on == Some(i) && window == 0 {
                            &self.release_events[..]
                        } else {
                            &[][..]
                        };
                        // The armed track hears the live input, if monitoring.
                        let monitor = if self.monitor && self.rec_track == Some(i) {
                            self.monitor_fifo.pop_into(&mut self.monitor_l[..n], &mut self.monitor_r[..n]);
                            Some((&self.monitor_l[..n], &self.monitor_r[..n]))
                        } else {
                            None
                        };
                        let peaks = track.render_into(offset, n, &ctx, any_solo, master_bus, aux_bus, live, window, key, monitor);
                        self.meters.record(Target::Track(i), MeterPoint::PreFader, peaks.pre);
                        self.meters.record(Target::Track(i), MeterPoint::PostFader, peaks.post);
                        // If anyone listens to this track, keep a copy of
                        // this window's output for them.
                        if self.is_key_source[i] {
                            let (src_l, src_r) = track.output().stereo();
                            let kb = &mut self.key_buffers[i];
                            kb.set_window(offset, n);
                            let (key_l, key_r) = kb.stereo_mut();
                            key_l.copy_from_slice(src_l);
                            key_r.copy_from_slice(src_r);
                        }
                    }
                }

                if window == 0 {
                    self.release_on = None; // delivered in the first window
                }

                let peak = self.aux.process_in_place(&ctx);
                self.meters.record(Target::Aux, MeterPoint::PostFader, peak);
                self.aux.add_into(self.master.bus());

                let peak = self.master.process_in_place(&ctx);
                self.meters.record(Target::Master, MeterPoint::PostFader, peak);

                self.transport.advance(n);
                offset += n;
            }

            // Stereo mix -> device channels, with a hard clip as a last
            // line of defense. Mono devices get L+R averaged; devices with
            // more than two channels get silence on the extras.
            let bus = self.master.bus();
            bus.set_window(0, frames);
            for (i, frame) in block.chunks_mut(dev_ch).enumerate() {
                let l = bus.channel(0)[i];
                let r = bus.channel(1)[i];
                for (ch, sample) in frame.iter_mut().enumerate() {
                    let s = match (dev_ch, ch) {
                        (1, _) => (l + r) * 0.5,
                        (_, c) if c < MIX_CHANNELS => bus.channel(c)[i],
                        _ => 0.0,
                    };
                    // A NaN (e.g. from an unstable filter) must never reach
                    // the device: `clamp` passes NaN straight through.
                    let s = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
                    *sample = T::from_sample(s);
                }
            }
            block_start += frames;
        }

        self.shared.publish(&self.transport);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::AudioBuffer;
    use crate::transport::ProcessContext;
    use rtrb::RingBuffer;
    use std::sync::Mutex;

    const INFO: PrepareInfo = PrepareInfo { sample_rate: 48_000.0, max_frames: 512, channels: MIX_CHANNELS };

    struct Harness {
        engine: Engine,
        tx: Producer<Command>,
        meters: Arc<Meters>,
        _garbage: Consumer<Garbage>,
        live_tx: Producer<LiveMidi>,
    }

    impl Harness {
        fn new(device_channels: usize) -> Self {
            let (tx, rx) = RingBuffer::<Command>::new(64);
            let (gtx, grx) = RingBuffer::<Garbage>::new(64);
            let (live_tx, live_rx) = RingBuffer::<LiveMidi>::new(64);
            let meters = Meters::new();
            let engine = Engine::new(&INFO, device_channels, TransportShared::new(), meters.clone(), rx, gtx, live_rx);
            Self { engine, tx, meters, _garbage: grx, live_tx }
        }
        fn send(&mut self, c: Command) {
            assert!(self.tx.push(c).is_ok());
        }
        fn add_track(&mut self, index: usize) {
            self.send(Command::AddTrack { index, track: Box::new(Track::new(&INFO)) });
        }
        fn insert(&mut self, track: usize, node: Box<dyn Processor>) {
            self.send(Command::Insert { target: Target::Track(track), slot: 0, node });
        }
        /// Renders enough blocks for smoothers to settle; returns the last.
        fn settle(&mut self, device_channels: usize) -> Vec<f32> {
            let mut out = vec![0f32; 512 * device_channels];
            for _ in 0..20 {
                self.engine.render(&mut out);
            }
            out
        }
    }

    /// Outputs a constant value on both channels.
    struct Dc(f32);
    impl Processor for Dc {
        fn name(&self) -> &'static str { "dc" }
        fn param_info(&self) -> &'static [crate::processor::ParamInfo] { &[] }
        fn prepare(&mut self, _: &PrepareInfo) {}
        fn set_param(&mut self, _: usize, _: f32) {}
        fn process(&mut self, b: &mut AudioBuffer, _: &ProcessContext) {
            for ch in 0..b.channels() {
                b.channel_mut(ch).iter_mut().for_each(|s| *s += self.0);
            }
        }
    }

    /// Records (position, frames) for every process call. A Mutex on the
    /// audio thread is exactly what we must never do in real code, but in
    /// a test nothing has a deadline.
    struct Probe(Arc<Mutex<Vec<(u64, usize)>>>);
    impl Processor for Probe {
        fn name(&self) -> &'static str { "probe" }
        fn param_info(&self) -> &'static [crate::processor::ParamInfo] { &[] }
        fn prepare(&mut self, _: &PrepareInfo) {}
        fn set_param(&mut self, _: usize, _: f32) {}
        fn process(&mut self, b: &mut AudioBuffer, ctx: &ProcessContext) {
            self.0.lock().unwrap().push((ctx.position, b.frames()));
        }
    }

    #[test]
    fn split_at_loop_end() {
        let mut h = Harness::new(2);
        h.add_track(0);
        let calls = Arc::new(Mutex::new(Vec::new()));
        h.insert(0, Box::new(Probe(calls.clone())));
        h.send(Command::SetLoop { range: Some((0.0, 1.0)) });
        h.send(Command::Seek { beat: 23_900.0 / 24_000.0 });
        h.send(Command::Play);
        let mut out = vec![0f32; 512 * 2];
        h.engine.render(&mut out);
        assert_eq!(*calls.lock().unwrap(), vec![(23_900, 100), (0, 412)]);
    }

    #[test]
    fn tracks_sum_into_master() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.add_track(3);
        h.insert(0, Box::new(Dc(0.25)));
        h.insert(3, Box::new(Dc(0.125)));
        let out = h.settle(2);
        assert!((out[0] - 0.375).abs() < 1e-4);
    }

    #[test]
    fn volume_and_balance() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.insert(0, Box::new(Dc(0.5)));
        h.send(Command::SetVolume { target: Target::Track(0), db: -6.0206 }); // x0.5
        h.send(Command::SetPan { track: 0, pos: -1.0 });
        let out = h.settle(2);
        assert!((out[0] - 0.25).abs() < 1e-4, "left at half volume");
        assert!(out[1].abs() < 1e-4, "right silent when hard left");

        h.send(Command::SetPan { track: 0, pos: 0.0 });
        let out = h.settle(2);
        assert!((out[0] - 0.25).abs() < 1e-4 && (out[1] - 0.25).abs() < 1e-4, "center is unity");
    }

    #[test]
    fn mute_and_solo() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.add_track(1);
        h.insert(0, Box::new(Dc(0.25)));
        h.insert(1, Box::new(Dc(0.5)));

        h.send(Command::SetSolo { track: 1, on: true });
        assert!((h.settle(2)[0] - 0.5).abs() < 1e-4, "solo silences other tracks");

        h.send(Command::SetMute { track: 1, on: true });
        assert!(h.settle(2)[0].abs() < 1e-4, "a muted solo track is silent too");

        h.send(Command::SetSolo { track: 1, on: false });
        h.send(Command::SetMute { track: 1, on: false });
        assert!((h.settle(2)[0] - 0.75).abs() < 1e-4);
    }

    #[test]
    fn master_volume_and_meters() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.insert(0, Box::new(Dc(0.5)));
        h.send(Command::SetVolume { target: Target::Master, db: -6.0206 });
        h.settle(2);
        h.meters.take(Target::Track(0), MeterPoint::PostFader);
        h.meters.take(Target::Master, MeterPoint::PostFader);
        h.settle(2);
        assert!((h.meters.take(Target::Track(0), MeterPoint::PostFader) - 0.5).abs() < 1e-4);
        assert!((h.meters.take(Target::Master, MeterPoint::PostFader) - 0.25).abs() < 1e-4);
        assert_eq!(h.meters.take(Target::Master, MeterPoint::PostFader), 0.0, "take resets");
    }

    #[test]
    fn mono_device_gets_average() {
        let mut h = Harness::new(1);
        h.add_track(0);
        h.insert(0, Box::new(Dc(0.5)));
        h.send(Command::SetPan { track: 0, pos: -1.0 });
        let out = h.settle(1);
        assert!((out[0] - 0.25).abs() < 1e-4);
    }

    // ---- Exercise tests ----

    #[test]
    fn exercise1_pre_and_post_fader_meters() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.insert(0, Box::new(Dc(0.5)));
        h.send(Command::SetVolume { target: Target::Track(0), db: -6.0206 });
        h.send(Command::SetMute { track: 0, on: true });
        h.settle(2);
        let pre = h.meters.take(Target::Track(0), MeterPoint::PreFader);
        let post = h.meters.take(Target::Track(0), MeterPoint::PostFader);
        h.settle(2);
        let pre = pre.max(h.meters.take(Target::Track(0), MeterPoint::PreFader));
        let post_settled = h.meters.take(Target::Track(0), MeterPoint::PostFader);
        assert!((pre - 0.5).abs() < 1e-4, "pre-fader ignores volume and mute");
        assert!(post > 0.0 && post <= 0.5, "post-fader saw the fade-out");
        assert_eq!(post_settled, 0.0, "post-fader is silent once muted");
    }

    #[test]
    fn exercise2_send_to_aux() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.insert(0, Box::new(Dc(0.25)));
        h.send(Command::SetSend { track: 0, db: 0.0 });
        assert!((h.settle(2)[0] - 0.5).abs() < 1e-4, "direct 0.25 + send 0.25");

        h.send(Command::SetVolume { target: Target::Aux, db: -6.0206 });
        assert!((h.settle(2)[0] - 0.375).abs() < 1e-4, "aux volume scales only the send");

        h.send(Command::SetVolume { target: Target::Track(0), db: -6.0206 });
        assert!((h.settle(2)[0] - 0.1875).abs() < 1e-4, "post-fader: track volume scales both");

        h.send(Command::SetMute { track: 0, on: true });
        assert!(h.settle(2)[0].abs() < 1e-4, "muting the track mutes its send");
    }

    #[test]
    fn exercise2_aux_chain_processes_only_sends() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.insert(0, Box::new(Dc(0.25)));
        h.send(Command::SetSend { track: 0, db: 0.0 });
        // A node on the aux that adds 0.1 proves the aux chain runs, once.
        h.send(Command::Insert { target: Target::Aux, slot: 0, node: Box::new(Dc(0.1)) });
        assert!((h.settle(2)[0] - 0.6).abs() < 1e-4);
    }

    #[test]
    fn exercise3_silent_track_keeps_smoothers_moving() {
        let mut h = Harness::new(2);
        h.add_track(0);
        // Silent chain, so the fader loop is skipped while volume changes.
        h.send(Command::SetVolume { target: Target::Track(0), db: -6.0206 });
        h.settle(2);
        // Now make it audible: it must already be at -6 dB, not ramping.
        h.insert(0, Box::new(Dc(0.5)));
        let mut out = vec![0f32; 512 * 2];
        h.engine.render(&mut out);
        assert!((out[0] - 0.25).abs() < 1e-4);
    }

    #[test]
    fn clips_play_through_the_track() {
        use crate::clip::{AudioClip, ClipParams};
        use crate::loader::AudioData;
        let mut h = Harness::new(2);
        h.add_track(0);
        let data = Arc::new(AudioData { name: "dc".into(), sample_rate: 48_000.0, channels: vec![vec![0.5; 100_000]] });
        let clip = AudioClip::new(data, ClipParams::new(1, 0.0, 4.0), &INFO);
        h.send(Command::SetClips { track: 0, clips: Box::new(ClipList::new(vec![clip])) });
        h.send(Command::SetVolume { target: Target::Track(0), db: -6.0206 });
        h.send(Command::Play);
        let out = h.settle(2);
        assert!((out[0] - 0.25).abs() < 1e-4, "clip -> track fader -> master");
        // The old (empty) clip list came back to be freed.
        assert!(matches!(h._garbage.pop(), Ok(Garbage::Clips(_))));
    }

    /// Records the frame of every event it receives.
    struct EventProbe(Arc<Mutex<Vec<(usize, MidiMessage)>>>);
    impl Instrument for EventProbe {
        fn name(&self) -> &'static str { "probe" }
        fn param_info(&self) -> &'static [crate::processor::ParamInfo] { &[] }
        fn prepare(&mut self, _: &PrepareInfo) {}
        fn set_param(&mut self, _: usize, _: f32) {}
        fn render(&mut self, _: &mut AudioBuffer, _: &ProcessContext, events: &[crate::midi::MidiEvent]) {
            self.0.lock().unwrap().extend(events.iter().map(|e| (e.frame, e.msg)));
        }
    }

    #[test]
    fn live_midi_keeps_its_timing_and_goes_to_the_armed_track() {
        use std::time::Duration;
        let mut h = Harness::new(2);
        h.add_track(0);
        h.add_track(1);
        let armed = Arc::new(Mutex::new(Vec::new()));
        let other = Arc::new(Mutex::new(Vec::new()));
        h.send(Command::SetInstrument { track: 0, instrument: Some(Box::new(EventProbe(other.clone()))) });
        h.send(Command::SetInstrument { track: 1, instrument: Some(Box::new(EventProbe(armed.clone()))) });
        h.send(Command::Arm { track: Some(1) });

        // A 1024-frame callback is two 512-frame blocks.
        let mut out = vec![0f32; 1024 * 2];
        h.engine.render(&mut out);
        let prev = h.engine.last_callback.unwrap();

        // Notes that "arrived" 5 ms and 15 ms after that callback began.
        let note = MidiMessage::NoteOn { note: 60, velocity: 100 };
        for ms in [5, 15] {
            let at = prev + Duration::from_millis(ms);
            assert!(h.live_tx.push(LiveMidi { at, msg: note }).is_ok());
        }
        h.engine.render(&mut out);

        // 5 ms = 240 frames into block 0; 15 ms = frame 720 = 208 into block 1.
        assert_eq!(*armed.lock().unwrap(), vec![(240, note), (208, note)]);
        assert!(other.lock().unwrap().is_empty(), "unarmed tracks get nothing");
    }

    #[test]
    fn sequence_plays_the_synth_through_the_mixer() {
        use crate::midi::{Note, Sequence};
        let mut h = Harness::new(2);
        h.add_track(0);
        let mut synth: Box<dyn Instrument> = Box::new(crate::synth::Synth::new());
        synth.prepare(&INFO);
        h.send(Command::SetInstrument { track: 0, instrument: Some(synth) });
        let notes = [Note { start: 0.5, length: 1.0, pitch: 57, velocity: 100 }];
        let seq = Sequence::build([(0.0, 4.0, &notes[..])]);
        h.send(Command::SetSequence { track: 0, sequence: Box::new(seq) });
        h.send(Command::Play);

        let mut out = vec![0f32; 512 * 2];
        let mut first_sound = None;
        for block in 0..100 {
            h.engine.render(&mut out);
            if first_sound.is_none() {
                if let Some(i) = out.chunks(2).position(|f| f[0] != 0.0) {
                    first_sound = Some(block * 512 + i);
                }
            }
        }
        // Beat 0.5 at 120 BPM / 48 kHz = sample 12_000. The first sample is
        // the oscillator's zero crossing, so sound starts one sample later.
        assert_eq!(first_sound, Some(12_001));
    }

    #[test]
    fn regression_rearming_releases_held_live_notes_on_the_old_track() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.add_track(1);
        let first = Arc::new(Mutex::new(Vec::new()));
        h.send(Command::SetInstrument { track: 0, instrument: Some(Box::new(EventProbe(first.clone()))) });
        h.send(Command::SetInstrument { track: 1, instrument: Some(Box::new(EventProbe(Arc::new(Mutex::new(Vec::new()))))) });
        h.send(Command::Arm { track: Some(0) });
        let mut out = vec![0f32; 512 * 2];
        h.engine.render(&mut out);
        let at = h.engine.last_callback.unwrap();
        assert!(h.live_tx.push(LiveMidi { at, msg: MidiMessage::NoteOn { note: 62, velocity: 90 } }).is_ok());
        h.engine.render(&mut out);
        // Key still held; the player switches to another track.
        h.send(Command::Arm { track: Some(1) });
        h.engine.render(&mut out);
        let got = first.lock().unwrap().clone();
        assert_eq!(got.last().map(|e| e.1), Some(MidiMessage::ForceOff { note: 62 }), "stuck note: {got:?}");
    }

    #[test]
    fn regression_removing_the_armed_track_disarms() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.send(Command::Arm { track: Some(0) });
        h.send(Command::RemoveTrack { index: 0 });
        let mut out = vec![0f32; 512 * 2];
        h.engine.render(&mut out);
        assert_eq!(h.engine.armed, None, "a new track 0 would be armed by surprise");
    }

    #[test]
    fn regression_nan_never_reaches_the_device() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.insert(0, Box::new(Dc(f32::NAN)));
        let out = h.settle(2);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(h.meters.take(Target::Master, MeterPoint::PostFader), f32::INFINITY, "shows as CLIP");
    }

    #[test]
    fn regression_two_rearms_in_one_callback_still_release() {
        let mut h = Harness::new(2);
        for i in 0..3 {
            h.add_track(i);
        }
        let first = Arc::new(Mutex::new(Vec::new()));
        h.send(Command::SetInstrument { track: 0, instrument: Some(Box::new(EventProbe(first.clone()))) });
        h.send(Command::Arm { track: Some(0) });
        let mut out = vec![0f32; 512 * 2];
        h.engine.render(&mut out);
        let at = h.engine.last_callback.unwrap();
        assert!(h.live_tx.push(LiveMidi { at, msg: MidiMessage::NoteOn { note: 62, velocity: 90 } }).is_ok());
        h.engine.render(&mut out);
        h.send(Command::Arm { track: Some(1) });
        h.send(Command::Arm { track: Some(2) });
        h.engine.render(&mut out);
        assert_eq!(first.lock().unwrap().last().map(|e| e.1), Some(MidiMessage::ForceOff { note: 62 }));
    }

    // ---- Step 8 exercise 1: sidechain ----

    /// Track 0: steady "bass" at 0.5 with a sidechained compressor.
    /// Track 3: the "kick" key, a higher number, so it must render first.
    fn sidechain_setup(key_level: f32) -> Harness {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.add_track(3);
        h.insert(0, Box::new(Dc(0.5)));
        let mut comp: Box<dyn Processor> = Box::new(crate::nodes::Compressor::new());
        comp.prepare(&INFO);
        comp.set_param(0, -30.0); // threshold
        comp.set_param(1, 10.0); // ratio
        comp.set_param(4, 0.0); // hard knee
        h.send(Command::Insert { target: Target::Track(0), slot: 1, node: comp });
        h.insert(3, Box::new(Dc(key_level)));
        h.send(Command::SetVolume { target: Target::Track(3), db: -60.0 }); // key not heard
        h.send(Command::SetSidechain { track: 0, source: Some(3) });
        h
    }

    #[test]
    fn sidechain_ducks_by_the_key_not_the_signal() {
        // The bass (0.5 = -6 dB) is above the -30 dB threshold, but with a
        // silent key nothing is compressed: the detector listens to the key.
        let mut h = sidechain_setup(0.0);
        assert!((h.settle(2)[0] - 0.5).abs() < 1e-3, "silent key: bass untouched");

        // A loud key (-6 dB, 24 dB over at 10:1) ducks the bass by 21.6 dB,
        // even though the key track itself is turned all the way down.
        let mut h = sidechain_setup(0.5);
        let out = h.settle(2)[0];
        let expected = 0.5 * 10f32.powf(-21.6 / 20.0);
        assert!((out - expected).abs() < 1e-3, "got {out}, expected {expected}");
    }

    #[test]
    fn render_order_puts_key_sources_first() {
        let mut h = Harness::new(2);
        h.send(Command::SetSidechain { track: 0, source: Some(5) });
        h.send(Command::SetSidechain { track: 5, source: Some(9) });
        let mut out = vec![0f32; 512 * 2];
        h.engine.render(&mut out);
        let pos = |t: usize| h.engine.order.iter().position(|&x| x == t).unwrap();
        assert!(pos(9) < pos(5) && pos(5) < pos(0), "{:?}", h.engine.order);
        assert_eq!(h.engine.order.len(), MAX_TRACKS, "every track exactly once");
    }

    #[test]
    fn removing_the_key_track_returns_to_normal_compression() {
        // With a silent key the bass passes untouched...
        let mut h = sidechain_setup(0.0);
        assert!((h.settle(2)[0] - 0.5).abs() < 1e-3);
        // ...and with the key track gone, the compressor listens to its own
        // track again, like any compressor: the -6 dB bass is now 24 dB
        // over the threshold, and gets turned down by itself.
        h.send(Command::RemoveTrack { index: 3 });
        let out = h.settle(2)[0];
        assert!((out - 0.5 * 10f32.powf(-21.6 / 20.0)).abs() < 1e-3, "got {out}");
        assert_eq!(h.engine.keys[0], None);
    }

    #[test]
    fn rearming_with_the_pedal_down_lifts_it_on_the_old_track() {
        let mut h = Harness::new(2);
        h.add_track(0);
        h.add_track(1);
        let first = Arc::new(Mutex::new(Vec::new()));
        h.send(Command::SetInstrument { track: 0, instrument: Some(Box::new(EventProbe(first.clone()))) });
        h.send(Command::Arm { track: Some(0) });
        let mut out = vec![0f32; 512 * 2];
        h.engine.render(&mut out);
        let at = h.engine.last_callback.unwrap();
        // Pedal down, play a note, let go of the key: the note is sustained.
        for msg in [
            MidiMessage::ControlChange { controller: 64, value: 127 },
            MidiMessage::NoteOn { note: 62, velocity: 90 },
            MidiMessage::NoteOff { note: 62 },
        ] {
            assert!(h.live_tx.push(LiveMidi { at, msg }).is_ok());
        }
        h.engine.render(&mut out);
        h.send(Command::Arm { track: Some(1) });
        h.engine.render(&mut out);
        // The pedal-up would otherwise go to the new track, and track 0's
        // sustained note would ring forever.
        let got: Vec<MidiMessage> = first.lock().unwrap().iter().map(|e| e.1).collect();
        assert_eq!(got.last(), Some(&MidiMessage::ControlChange { controller: 64, value: 0 }), "{got:?}");
    }

    // ---- Step 9 exercise 3: automation ----

    fn automated_gain(points: Vec<(f64, f32)>) -> Harness {
        use crate::automation::{AutoTarget, Envelope, Lane};
        let mut h = Harness::new(2);
        h.add_track(0);
        h.insert(0, Box::new(Dc(0.5)));
        let mut gain: Box<dyn Processor> = Box::new(crate::nodes::Gain::new());
        gain.prepare(&INFO);
        let info = gain.param_info()[0];
        h.send(Command::Insert { target: Target::Track(0), slot: 1, node: gain });
        let lane = Lane::new(AutoTarget::Node { slot: 1, param: 0 }, info, Envelope::new(points));
        h.send(Command::SetAutomation { track: 0, automation: Box::new(AutomationSet { lanes: vec![lane] }) });
        h
    }

    fn db(x: f32) -> f32 {
        20.0 * x.abs().log10()
    }

    #[test]
    fn automation_ramps_a_parameter_over_time() {
        use crate::automation::{normalize, VOLUME_INFO};
        // The gain node's range is the same as the fader's: -60..+12 dB.
        let n = |db: f32| normalize(&VOLUME_INFO, db);
        let mut h = automated_gain(vec![(0.0, n(0.0)), (4.0, n(-24.0))]);
        h.send(Command::Play);
        let mut out = vec![0f32; 512 * 2];
        // Beat 2 at 120 BPM / 48 kHz = sample 48_000: halfway down the ramp.
        for _ in 0..(48_000 / 512) {
            h.engine.render(&mut out);
        }
        let level = db(out[0]) - db(0.5);
        assert!((level - -12.0).abs() < 0.5, "halfway: {level:.2} dB");
    }

    #[test]
    fn automation_applies_on_seek_even_while_stopped() {
        use crate::automation::{normalize, VOLUME_INFO};
        let n = |db: f32| normalize(&VOLUME_INFO, db);
        let mut h = automated_gain(vec![(0.0, n(0.0)), (4.0, n(0.0)), (4.0, n(-20.0))]);
        h.send(Command::Seek { beat: 8.0 });
        let out = h.settle(2); // stopped the whole time
        assert!((db(out[0]) - db(0.5) - -20.0).abs() < 0.1, "the value at the playhead, without playing");
    }

    // ---- Step 10: recording ----

    struct Rec {
        input: Producer<f32>,
        events: Consumer<RecEvent>,
        shared: Arc<RecordShared>,
        channels: usize,
        next_sample: u32,
    }

    impl Rec {
        fn wire(h: &mut Harness, channels: usize) -> Self {
            let (input, samples) = RingBuffer::<f32>::new(8192 * channels);
            let (recorder, events) = RingBuffer::<RecEvent>::new(1024);
            let shared = RecordShared::new();
            h.engine.set_input(samples, channels, recorder, shared.clone());
            Self { input, events, shared, channels, next_sample: 1 }
        }

        /// What an input callback does: push `frames` frames. Each frame's
        /// value is its own sequence number, so gaps and repeats show up.
        fn feed(&mut self, frames: usize) {
            for _ in 0..frames {
                let v = self.next_sample as f32;
                for ch in 0..self.channels {
                    let _ = self.input.push(if ch == 0 { v } else { -v });
                }
                self.next_sample += 1;
            }
        }

        fn drain(&mut self) -> (Vec<RecEvent>, Vec<f32>, Vec<f32>) {
            let (mut ctrl, mut l, mut r) = (Vec::new(), Vec::new(), Vec::new());
            while let Ok(ev) = self.events.pop() {
                match ev {
                    RecEvent::Samples { len, left, right } => {
                        l.extend_from_slice(&left[..len]);
                        r.extend_from_slice(&right[..len]);
                    }
                    other => ctrl.push(other),
                }
            }
            (ctrl, l, r)
        }
    }

    fn begin_frame(ev: &RecEvent) -> Option<(u32, i64)> {
        match *ev {
            RecEvent::Begin { take, start_frame, .. } => Some((take, start_frame)),
            _ => None,
        }
    }

    #[test]
    fn a_take_starts_compensated_and_records_every_frame() {
        let mut h = Harness::new(2);
        h.add_track(0);
        let mut rec = Rec::wire(&mut h, 2);
        rec.shared.input_latency.store(100, std::sync::atomic::Ordering::Relaxed);
        rec.shared.output_latency.store(400, std::sync::atomic::Ordering::Relaxed);
        h.send(Command::SetLatencyAdjust { frames: 20 });
        h.send(Command::SetRecordTrack { track: Some(0) });
        h.send(Command::Seek { beat: 1.0 }); // 24_000
        h.send(Command::Play);

        let mut out = vec![0f32; 512 * 2];
        rec.feed(300); // already waiting when the first callback runs
        for _ in 0..20 {
            h.engine.render(&mut out);
            rec.feed(512);
        }
        h.send(Command::Stop);
        h.engine.render(&mut out);

        let (ctrl, left, right) = rec.drain();
        assert_eq!(begin_frame(&ctrl[0]), Some((1, 24_000 - 300 - 100 - 400 - 20)), "every delay subtracted");
        assert!(matches!(ctrl.last(), Some(RecEvent::End { take: 1 })));
        // Every frame, in order, no gaps: 1, 2, 3...
        let expected: Vec<f32> = (1..=left.len() as u32).map(|i| i as f32).collect();
        assert_eq!(left, expected);
        assert!(right.iter().zip(&left).all(|(r, l)| *r == -*l), "stereo kept apart");
        assert_eq!(left.len(), 300 + 19 * 512, "everything delivered before the stop");
    }

    #[test]
    fn loop_recording_makes_one_take_per_lap() {
        let mut h = Harness::new(2);
        h.add_track(0);
        let mut rec = Rec::wire(&mut h, 2);
        h.send(Command::SetRecordTrack { track: Some(0) });
        h.send(Command::SetLoop { range: Some((0.0, 1.0)) }); // 24_000 frames
        h.send(Command::Play);
        let mut out = vec![0f32; 512 * 2];
        for _ in 0..120 {
            rec.feed(512);
            h.engine.render(&mut out); // 120 x 512 = 2.56 laps
        }
        let (ctrl, ..) = rec.drain();
        let begins: Vec<(u32, i64)> = ctrl.iter().filter_map(begin_frame).collect();
        assert_eq!(begins.len(), 3, "{begins:?}");
        // Laps 2 and 3 begin near the loop start (minus the frames waiting).
        assert!(begins[1].1 <= 0 && begins[1].1 > -1024 && begins[2].1 <= 0 && begins[2].1 > -1024, "{begins:?}");
        let ends = ctrl.iter().filter(|e| matches!(e, RecEvent::End { .. })).count();
        assert_eq!(ends, 2, "the first two laps are finished");
    }

    #[test]
    fn mono_input_records_to_both_sides() {
        let mut h = Harness::new(2);
        h.add_track(0);
        let mut rec = Rec::wire(&mut h, 1);
        h.send(Command::SetRecordTrack { track: Some(0) });
        h.send(Command::Play);
        let mut out = vec![0f32; 512 * 2];
        for _ in 0..4 {
            rec.feed(512);
            h.engine.render(&mut out);
        }
        let (_, left, right) = rec.drain();
        assert!(!left.is_empty());
        assert_eq!(left, right);
    }

    #[test]
    fn input_is_drained_even_when_not_recording() {
        let mut h = Harness::new(2);
        h.add_track(0);
        let mut rec = Rec::wire(&mut h, 2);
        let mut out = vec![0f32; 512 * 2];
        for _ in 0..50 {
            rec.feed(512);
            h.engine.render(&mut out);
        }
        assert!(rec.input.slots() > 8192 * 2 - 2 * 512, "the ring never fills up");
        assert!(rec.drain().0.is_empty(), "nothing recorded");
        assert!(rec.shared.take_input_peak() > 0.0, "but the input meter still works");
    }

    // ---- Step 10 exercises ----

    #[test]
    fn punch_records_exactly_the_punch_range() {
        let mut h = Harness::new(2);
        h.add_track(0);
        let mut rec = Rec::wire(&mut h, 2);
        h.send(Command::SetRecordTrack { track: Some(0) });
        h.send(Command::SetPunch { range: Some((1.0, 2.0)) }); // frames 24_000..48_000
        h.send(Command::Play);
        let mut out = vec![0f32; 512 * 2];
        for _ in 0..120 {
            rec.feed(512);
            h.engine.render(&mut out);
        }
        let (ctrl, left, _) = rec.drain();
        let begins: Vec<(u32, i64)> = ctrl.iter().filter_map(begin_frame).collect();
        assert_eq!(begins, vec![(1, 24_000)], "punched in on the exact frame");
        assert!(matches!(ctrl.last(), Some(RecEvent::End { take: 1 })), "and out again");
        assert_eq!(left.len(), 24_000, "exactly one beat of audio");
        // Each frame is fed 512 frames before the playhead reaches it, so
        // the frame at timeline position p carries sequence number p + 513.
        assert_eq!(left[0], 24_000.0 + 513.0, "the first frame is the one AT the punch point");
        assert_eq!(*left.last().unwrap(), 47_999.0 + 513.0);
    }

    #[test]
    fn monitoring_plays_the_input_through_the_tracks_effects() {
        let mut h = Harness::new(2);
        h.add_track(0);
        let mut rec = Rec::wire(&mut h, 2);
        let mut gain: Box<dyn Processor> = Box::new(crate::nodes::Gain::new());
        gain.prepare(&INFO);
        gain.set_param(0, -6.0206); // halve it
        h.send(Command::Insert { target: Target::Track(0), slot: 0, node: gain });
        h.send(Command::SetRecordTrack { track: Some(0) });

        let mut out = vec![0f32; 512 * 2];
        let mut run = |h: &mut Harness| {
            for _ in 0..30 {
                for _ in 0..512 {
                    let _ = rec.input.push(0.5);
                    let _ = rec.input.push(0.5);
                }
                h.engine.render(&mut out);
            }
            out[100]
        };
        assert_eq!(run(&mut h), 0.0, "off by default (it can feed back through speakers)");
        h.send(Command::SetMonitor { on: true });
        assert!((run(&mut h) - 0.25).abs() < 1e-4, "on: heard through the track's gain effect, while stopped");
        h.send(Command::SetRecordTrack { track: None });
        assert_eq!(run(&mut h), 0.0, "only the armed track monitors");
    }

    #[test]
    fn a_fast_input_clock_shows_up_as_drift() {
        use crate::record::DriftEstimator;
        use std::sync::atomic::Ordering::Relaxed;
        let mut h = Harness::new(2);
        h.add_track(0);
        let mut rec = Rec::wire(&mut h, 2);
        let mut out = vec![0f32; 512 * 2];
        let mut est = DriftEstimator::default();
        // The input delivers 513 frames for every 512 rendered: 1953 ppm.
        for _ in 0..2_000 {
            rec.feed(513);
            h.engine.render(&mut out);
            est.add(rec.shared.frames_in.load(Relaxed), rec.shared.frames_out.load(Relaxed));
        }
        let ppm = est.ppm(48_000.0).unwrap();
        assert!((ppm - 1_000_000.0 / 512.0).abs() < 1.0, "{ppm:.1} ppm");
    }
}
