//! Everything the main thread does, in one place.
//!
//! The command line and the UI are both thin front ends over this: they
//! call the same methods, which update the project mirror and send
//! commands to the engine through the same lock-free queue. Neither front
//! end ever touches the audio thread directly, and nothing here blocks.

use crate::automation::{self, AutoTarget, AutomationSet, Envelope, Lane, PAN_INFO, VOLUME_INFO};
use crate::clip::{AudioClip, ClipList, ClipParams};
use crate::engine::{Command, Garbage};
use crate::loader::{self, AudioData};
use crate::midi::{LiveMidi, MidiMessage, Note, Sequence};
use crate::mixer::{Meters, Target, Track, MAX_SLOTS, MAX_TRACKS};
use crate::nodes;
use crate::processor::{Instrument, ParamInfo, PrepareInfo};
use crate::record::{DriftEstimator, Finished, RecordShared};
use crate::project::{AutoLaneInfo, ClipInfo, MidiClipInfo, NodeInfo, Project, TrackInfo};
use crate::synth::Synth;
use crate::transport::{samples_per_beat, TransportShared, BEATS_PER_BAR};
use midir::{MidiInput, MidiInputConnection};
use rtrb::{Consumer, Producer};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

pub type Result<T = ()> = std::result::Result<T, String>;

/// Bars are 1-based for humans; beats are 0-based for the engine.
pub fn bar_to_beat(bar: f64) -> f64 {
    (bar - 1.0) * BEATS_PER_BAR
}

pub struct Session {
    pub info: PrepareInfo,
    pub sample_rate: f64,
    tx: Producer<Command>,
    garbage: Consumer<Garbage>,
    pub shared: Arc<TransportShared>,
    pub meters: Arc<Meters>,
    pub project: Project,
    /// Loaded audio files. A Mutex is fine here: only the main thread and
    /// loader threads touch it, never the audio thread.
    pub library: Arc<Mutex<Vec<Arc<AudioData>>>>,
    /// Messages from background work (file loading) for the front end.
    messages: Arc<Mutex<Vec<String>>>,
    next_clip_id: u32,
    /// Jump back to where playback began when stopping.
    pub return_to_start: bool,
    play_start: Option<f64>,
    /// Mirrors of transport state the engine doesn't publish.
    pub loop_range: Option<(f64, f64)>,
    pub armed: Option<usize>,
    // Live MIDI: the driver's thread pushes into this, the audio thread
    // pops. Behind a Mutex so it can be handed to whichever connection is
    // open; the lock is never taken on the audio thread.
    live_tx: Arc<Mutex<Producer<LiveMidi>>>,
    midi_conn: Option<MidiInputConnection<()>>,
    // --- Recording ---
    recorder: Option<Recorder>,
    /// The track armed for recording.
    pub rec_track: Option<usize>,
    /// Manual latency adjustment in milliseconds (see `record::take_start`).
    pub latency_adjust_ms: f64,
    /// Record only inside this range (beats).
    pub punch: Option<(f64, f64)>,
    pub monitor: bool,
    pub drift: DriftEstimator,
}

/// The main thread's end of recording, present when there's an input.
pub struct Recorder {
    pub device: String,
    pub done: mpsc::Receiver<Finished>,
    pub shared: Arc<RecordShared>,
}

impl Session {
    pub fn new(
        info: PrepareInfo,
        tx: Producer<Command>,
        garbage: Consumer<Garbage>,
        shared: Arc<TransportShared>,
        meters: Arc<Meters>,
        live_tx: Producer<LiveMidi>,
    ) -> Self {
        Self {
            sample_rate: info.sample_rate as f64,
            info,
            tx,
            garbage,
            shared,
            meters,
            project: Project::new(),
            library: Arc::new(Mutex::new(Vec::new())),
            messages: Arc::new(Mutex::new(Vec::new())),
            next_clip_id: 1,
            return_to_start: true,
            play_start: None,
            loop_range: None,
            armed: None,
            live_tx: Arc::new(Mutex::new(live_tx)),
            midi_conn: None,
            recorder: None,
            rec_track: None,
            latency_adjust_ms: 0.0,
            punch: None,
            monitor: false,
            drift: DriftEstimator::default(),
        }
    }

    pub fn attach_recorder(&mut self, recorder: Recorder) {
        self.recorder = Some(recorder);
    }

    pub fn recorder(&self) -> Option<&Recorder> {
        self.recorder.as_ref()
    }

    /// Arms a track for recording (or disarms with `None`). Recording then
    /// happens whenever the transport plays.
    pub fn set_record_track(&mut self, track: Option<usize>) -> Result {
        if self.recorder.is_none() && track.is_some() {
            return Err("no input device: recording is unavailable".into());
        }
        if let Some(t) = track {
            self.track_info(t)?;
        }
        self.send(Command::SetRecordTrack { track })?;
        self.rec_track = track;
        Ok(())
    }

    pub fn set_latency_adjust(&mut self, ms: f64) -> Result {
        let ms = ms.clamp(-500.0, 500.0);
        self.send(Command::SetLatencyAdjust { frames: (ms * self.sample_rate / 1000.0).round() as i64 })?;
        self.latency_adjust_ms = ms;
        Ok(())
    }

    /// Records only between `range.0` and `range.1` (beats) while armed
    /// and playing, or everywhere with `None`.
    pub fn set_punch(&mut self, range: Option<(f64, f64)>) -> Result {
        if let Some((a, b)) = range {
            if a < 0.0 || b <= a {
                return Err("the punch range must end after it starts".into());
            }
        }
        self.send(Command::SetPunch { range })?;
        self.punch = range;
        Ok(())
    }

    /// Hear the input through the armed track (and its effects). Off by
    /// default: with a microphone and speakers, it can feed back.
    pub fn set_monitor(&mut self, on: bool) -> Result {
        if on && self.recorder.is_none() {
            return Err("no input device to monitor".into());
        }
        self.send(Command::SetMonitor { on })?;
        self.monitor = on;
        Ok(())
    }

    /// Clock drift between input and output, in ppm, once there are at
    /// least 10 seconds of history.
    pub fn drift_ppm(&self) -> Option<f64> {
        self.drift.ppm(self.sample_rate * 10.0)
    }

    /// Turns finished takes into clips, and samples the drift counters.
    /// Call regularly, like `collect_garbage`. Returns a message per take.
    pub fn poll_recordings(&mut self) -> Vec<String> {
        let finished: Vec<Finished> = match &self.recorder {
            Some(r) => {
                use std::sync::atomic::Ordering::Relaxed;
                self.drift.add(r.shared.frames_in.load(Relaxed), r.shared.frames_out.load(Relaxed));
                r.done.try_iter().collect()
            }
            None => return Vec::new(),
        };
        finished.into_iter().map(|f| self.place_take(f)).collect()
    }

    fn place_take(&mut self, f: Finished) -> String {
        let secs = f.audio.duration_secs();
        let saved = match &f.path {
            Ok(p) => format!("saved as {}", p.display()),
            Err(e) => format!("{e}; the take is kept in memory only"),
        };
        let index = {
            let mut lib = self.library.lock().unwrap();
            lib.push(f.audio.clone());
            lib.len() - 1
        };
        if secs <= 0.0 || self.track_info(f.track).is_err() {
            return format!("take {} ({secs:.1}s) is in the library as #{index} ({saved})", f.take);
        }
        // Latency compensation can put a take's start before the song's: it
        // then starts at zero, with that much trimmed off the front.
        let (start_frame, offset_secs) = if f.start_frame < 0 {
            (0, -f.start_frame as f64 / f.audio.sample_rate)
        } else {
            (f.start_frame, 0.0)
        };
        let spb = samples_per_beat(self.sample_rate, self.tempo());
        let id = self.new_clip_id();
        let mut params = ClipParams::new(id, start_frame as f64 / spb, (secs - offset_secs).max(0.0) * self.tempo() / 60.0);
        params.offset_secs = offset_secs;
        self.project.tracks[f.track].as_mut().unwrap().clips.push(ClipInfo { data: f.audio, params });
        if let Err(e) = self.sync_clips(f.track) {
            return e;
        }
        format!("recorded take {} ({secs:.1}s) on track {} ({saved})", f.take, f.track)
    }

    fn send(&mut self, cmd: Command) -> Result {
        // Never blocks: if the engine has fallen behind, the edit is dropped
        // and reported, rather than freezing the UI.
        self.tx.push(cmd).map_err(|_| "command queue full".to_string())
    }

    /// Frees whatever the audio thread has finished with. Call regularly:
    /// the CLI does it per command, the UI once per frame.
    pub fn collect_garbage(&mut self) {
        while let Ok(item) = self.garbage.pop() {
            drop(item); // nodes, tracks, clip lists... freed here, safely
        }
    }

    /// Messages from background work since the last call.
    pub fn take_messages(&self) -> Vec<String> {
        std::mem::take(&mut *self.messages.lock().unwrap())
    }

    // ---------------------------------------------------------------
    // Transport
    // ---------------------------------------------------------------

    /// Current playhead in beats, as last published by the audio thread.
    pub fn current_beat(&self) -> f64 {
        self.shared.position() as f64 / samples_per_beat(self.sample_rate, self.shared.tempo())
    }

    pub fn position_secs(&self) -> f64 {
        self.shared.position() as f64 / self.sample_rate
    }

    pub fn playing(&self) -> bool {
        self.shared.playing()
    }

    pub fn tempo(&self) -> f64 {
        self.shared.tempo()
    }

    pub fn play(&mut self) -> Result {
        // Only remember the start if we weren't already playing. Stored in
        // beats, so a tempo change mid-playback is fine.
        if self.play_start.is_none() {
            self.play_start = Some(self.current_beat());
        }
        self.send(Command::Play)
    }

    pub fn stop(&mut self) -> Result {
        self.send(Command::Stop)?;
        if let Some(beat) = self.play_start.take() {
            if self.return_to_start {
                // Queued right behind Stop: applied in the same callback.
                self.send(Command::Seek { beat })?;
            }
        }
        Ok(())
    }

    pub fn toggle_play(&mut self) -> Result {
        if self.playing() || self.play_start.is_some() {
            self.stop()
        } else {
            self.play()
        }
    }

    pub fn seek(&mut self, beat: f64) -> Result {
        if beat < 0.0 {
            return Err("can't seek before the start".into());
        }
        self.send(Command::Seek { beat })
    }

    pub fn set_tempo(&mut self, bpm: f64) -> Result {
        if !(20.0..=999.0).contains(&bpm) {
            return Err("tempo must be 20-999 BPM".into());
        }
        self.send(Command::SetTempo { bpm })
    }

    /// A loop in beats, or `None` for no loop.
    pub fn set_loop(&mut self, range: Option<(f64, f64)>) -> Result {
        if let Some((start, end)) = range {
            if start < 0.0 || end <= start {
                return Err("the loop must end after it starts".into());
            }
        }
        self.loop_range = range;
        self.send(Command::SetLoop { range })
    }

    // ---------------------------------------------------------------
    // Tracks and mixer
    // ---------------------------------------------------------------

    fn track_info(&self, track: usize) -> Result<&TrackInfo> {
        self.project.tracks.get(track).and_then(|t| t.as_ref()).ok_or_else(|| format!("no track {track}"))
    }

    fn track_info_mut(&mut self, track: usize) -> Result<&mut TrackInfo> {
        self.project.tracks.get_mut(track).and_then(|t| t.as_mut()).ok_or_else(|| format!("no track {track}"))
    }

    fn target_info_mut(&mut self, target: Target) -> Result<&mut TrackInfo> {
        self.project.get_mut(target).ok_or_else(|| format!("no such target: {target:?}"))
    }

    pub fn add_track(&mut self, name: Option<String>) -> Result<usize> {
        let index = self.project.first_free_track().ok_or_else(|| format!("all {MAX_TRACKS} tracks in use"))?;
        let name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("Track {}", index + 1));
        // The track's buffers are allocated here, not in the engine.
        let track = Box::new(Track::new(&self.info));
        self.send(Command::AddTrack { index, track })?;
        self.project.tracks[index] = Some(TrackInfo::new(name));
        Ok(index)
    }

    pub fn remove_track(&mut self, index: usize) -> Result {
        self.track_info(index)?;
        self.send(Command::RemoveTrack { index })?;
        self.project.tracks[index] = None;
        // Tracks that listened to it lose their sidechain; the engine does
        // the same, and disarms it, on its side.
        for t in self.project.tracks.iter_mut().flatten() {
            if t.key == Some(index) {
                t.key = None;
            }
        }
        if self.armed == Some(index) {
            self.armed = None;
        }
        if self.rec_track == Some(index) {
            self.rec_track = None;
        }
        Ok(())
    }

    pub fn rename_track(&mut self, track: usize, name: String) -> Result {
        self.track_info_mut(track)?.name = name;
        Ok(())
    }

    pub fn set_volume(&mut self, target: Target, db: f32) -> Result {
        let db = db.clamp(-60.0, 12.0);
        self.target_info_mut(target)?;
        self.send(Command::SetVolume { target, db })?;
        self.target_info_mut(target)?.volume_db = db;
        Ok(())
    }

    pub fn set_pan(&mut self, track: usize, pos: f32) -> Result {
        let pos = pos.clamp(-1.0, 1.0);
        self.track_info(track)?;
        self.send(Command::SetPan { track, pos })?;
        self.track_info_mut(track)?.pan = pos;
        Ok(())
    }

    pub fn set_mute(&mut self, track: usize, on: bool) -> Result {
        self.track_info(track)?;
        self.send(Command::SetMute { track, on })?;
        self.track_info_mut(track)?.mute = on;
        Ok(())
    }

    pub fn set_solo(&mut self, track: usize, on: bool) -> Result {
        self.track_info(track)?;
        self.send(Command::SetSolo { track, on })?;
        self.track_info_mut(track)?.solo = on;
        Ok(())
    }

    pub fn set_send(&mut self, track: usize, db: f32) -> Result {
        let db = db.clamp(-60.0, 12.0);
        self.track_info(track)?;
        self.send(Command::SetSend { track, db })?;
        self.track_info_mut(track)?.send_db = db;
        Ok(())
    }

    pub fn set_sidechain(&mut self, track: usize, source: Option<usize>) -> Result {
        self.track_info(track)?;
        if let Some(s) = source {
            self.track_info(s)?;
            if self.project.sidechain_creates_cycle(track, s) {
                return Err(format!("that would make a loop: track {s} already listens (directly or not) to track {track}"));
            }
        }
        self.send(Command::SetSidechain { track, source })?;
        self.track_info_mut(track)?.key = source;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Effect chains
    // ---------------------------------------------------------------

    /// Adds an effect; returns the slot it went into.
    pub fn add_node(&mut self, target: Target, kind: &str, slot: Option<usize>) -> Result<usize> {
        let mut node = nodes::make(kind).ok_or_else(|| format!("unknown effect kind: {kind}"))?;
        let slot = match slot {
            Some(s) if s < MAX_SLOTS => s,
            Some(_) => return Err(format!("slots are 0-{}", MAX_SLOTS - 1)),
            None => self.target_info_mut(target)?.first_free_slot().ok_or("no free slot")?,
        };
        self.target_info_mut(target)?;
        node.prepare(&self.info); // allocation happens here, on this thread
        let info = NodeInfo::new(node.name(), node.param_info());
        self.send(Command::Insert { target, slot, node })?;
        let replaced = self.target_info_mut(target)?.slots[slot].replace(info).is_some();
        if replaced {
            self.drop_lanes(target, |t| matches!(t, AutoTarget::Node { slot: s, .. } if s == slot))?;
        }
        Ok(slot)
    }

    pub fn remove_node(&mut self, target: Target, slot: usize) -> Result {
        let t = self.target_info_mut(target)?;
        if t.slots.get(slot).map_or(true, |s| s.is_none()) {
            return Err("that slot is empty".into());
        }
        self.send(Command::Remove { target, slot })?;
        self.target_info_mut(target)?.slots[slot] = None;
        // A lane aimed at this slot would otherwise drive whatever is put
        // there next, through the wrong parameter.
        self.drop_lanes(target, |t| matches!(t, AutoTarget::Node { slot: s, .. } if s == slot))
    }

    pub fn set_param(&mut self, target: Target, slot: usize, param: usize, value: f32) -> Result {
        let node = self
            .target_info_mut(target)?
            .slots
            .get(slot)
            .and_then(|s| s.as_ref())
            .ok_or("that slot is empty")?;
        let p = node.params.get(param).ok_or("no such parameter")?;
        let value = value.clamp(p.min, p.max);
        self.send(Command::SetParam { target, slot, param, value })?;
        if let Some(node) = self.target_info_mut(target)?.slots[slot].as_mut() {
            node.values[param] = value;
        }
        Ok(())
    }

    pub fn param_index(&self, target: Target, slot: usize, name: &str) -> Result<usize> {
        let node = self
            .project
            .get(target)
            .and_then(|t| t.slots.get(slot))
            .and_then(|s| s.as_ref())
            .ok_or("that slot is empty")?;
        node.param_index(name).ok_or_else(|| {
            let names: Vec<_> = node.params.iter().map(|p| p.name).collect();
            format!("{} has no parameter {name} (it has: {})", node.name, names.join(", "))
        })
    }

    // ---------------------------------------------------------------
    // Instruments
    // ---------------------------------------------------------------

    pub fn set_synth(&mut self, track: usize, on: bool) -> Result {
        self.track_info(track)?;
        let instrument: Option<Box<dyn Instrument>> = if on {
            let mut synth: Box<dyn Instrument> = Box::new(Synth::new());
            synth.prepare(&self.info);
            Some(synth)
        } else {
            None
        };
        let info = instrument.as_ref().map(|i| NodeInfo::new(i.name(), i.param_info()));
        self.send(Command::SetInstrument { track, instrument })?;
        self.track_info_mut(track)?.instrument = info;
        // A new (or no) instrument: lanes for the old one no longer apply.
        self.drop_lanes(Target::Track(track), |t| matches!(t, AutoTarget::Instrument { .. }))
    }

    pub fn set_instrument_param(&mut self, track: usize, param: usize, value: f32) -> Result {
        let inst = self.track_info(track)?.instrument.as_ref().ok_or("that track has no instrument")?;
        let p = inst.params.get(param).ok_or("no such parameter")?;
        let value = value.clamp(p.min, p.max);
        self.send(Command::SetInstrumentParam { track, param, value })?;
        if let Some(inst) = self.track_info_mut(track)?.instrument.as_mut() {
            inst.values[param] = value;
        }
        Ok(())
    }

    // ---------------------------------------------------------------
    // Samples and audio clips
    // ---------------------------------------------------------------

    /// Decodes a file on a background thread (it can take seconds), then
    /// adds it to the library. If `echo` is set, the result is printed as
    /// soon as it's ready; otherwise it's queued for `take_messages`.
    pub fn load(&self, path: PathBuf, echo: bool) {
        let library = self.library.clone();
        let messages = self.messages.clone();
        std::thread::spawn(move || {
            let msg = match loader::load_audio(&path) {
                Ok(audio) => {
                    let mut lib = library.lock().unwrap();
                    lib.push(Arc::new(audio));
                    let a = lib.last().unwrap();
                    format!(
                        "loaded #{}: {} ({:.2}s, {} Hz, {} ch)",
                        lib.len() - 1,
                        a.name,
                        a.duration_secs(),
                        a.sample_rate,
                        a.channels.len()
                    )
                }
                Err(e) => format!("failed to load {}: {e}", path.display()),
            };
            if echo {
                println!("{msg}");
            } else {
                messages.lock().unwrap().push(msg);
            }
        });
    }

    pub fn samples(&self) -> Vec<Arc<AudioData>> {
        self.library.lock().unwrap().clone()
    }

    /// Rebuilds a track's whole clip list and sends it to the engine. Called
    /// after every clip edit: building a fresh snapshot on this thread is
    /// simpler and safer than editing the engine's list in place.
    fn sync_clips(&mut self, track: usize) -> Result {
        let t = self.track_info(track)?;
        let clips = t.clips.iter().map(|c| AudioClip::new(c.data.clone(), c.params, &self.info)).collect();
        self.send(Command::SetClips { track, clips: Box::new(ClipList::new(clips)) })
    }

    fn new_clip_id(&mut self) -> u32 {
        let id = self.next_clip_id;
        self.next_clip_id += 1;
        id
    }

    /// Places sample `sample` (an index into the library) on a track at
    /// `start_beat`. Returns the new clip's id.
    pub fn add_clip(&mut self, track: usize, sample: usize, start_beat: f64) -> Result<u32> {
        self.track_info(track)?;
        let data = self.library.lock().unwrap().get(sample).cloned().ok_or_else(|| format!("no sample #{sample}"))?;
        // Clips always have an explicit length: the whole file, in beats at
        // the current tempo. It stays fixed in beats after that.
        let length_beats = data.duration_secs() * self.tempo() / 60.0;
        let id = self.new_clip_id();
        let params = ClipParams::new(id, start_beat.max(0.0), length_beats);
        self.track_info_mut(track)?.clips.push(ClipInfo { data, params });
        self.sync_clips(track)?;
        Ok(id)
    }

    pub fn remove_clip(&mut self, track: usize, id: u32) -> Result {
        let clips = &mut self.track_info_mut(track)?.clips;
        let before = clips.len();
        clips.retain(|c| c.params.id != id);
        if clips.len() == before {
            return Err(format!("no clip #{id} on track {track}"));
        }
        self.sync_clips(track)
    }

    pub fn clip(&self, track: usize, id: u32) -> Option<&ClipInfo> {
        self.track_info(track).ok()?.clips.iter().find(|c| c.params.id == id)
    }

    /// Edits a clip's parameters; `edit` gets a copy, which is validated
    /// before it's kept.
    pub fn edit_clip(&mut self, track: usize, id: u32, edit: impl FnOnce(&mut ClipParams)) -> Result {
        let clip = self
            .track_info_mut(track)?
            .clips
            .iter_mut()
            .find(|c| c.params.id == id)
            .ok_or_else(|| format!("no clip #{id} on track {track}"))?;
        let mut p = clip.params;
        edit(&mut p);
        if p.start_beat < 0.0 || p.length_beats <= 0.0 || p.offset_secs < 0.0 || p.fade_in_beats < 0.0 || p.fade_out_beats < 0.0 {
            return Err("invalid clip value".into());
        }
        p.id = id; // the id is not editable
        p.gain_db = p.gain_db.clamp(-60.0, 12.0);
        clip.params = p;
        self.sync_clips(track)
    }

    /// Splits a clip at `at_beat`; returns the id of the new second half.
    pub fn split_clip(&mut self, track: usize, id: u32, at_beat: f64) -> Result<u32> {
        let tempo = self.tempo();
        let new_id = self.next_clip_id;
        let clips = &mut self.track_info_mut(track)?.clips;
        let index = clips.iter().position(|c| c.params.id == id).ok_or_else(|| format!("no clip #{id}"))?;
        let p = clips[index].params;
        if at_beat <= p.start_beat || at_beat >= p.start_beat + p.length_beats {
            return Err("the split point must be inside the clip".into());
        }
        // The second half starts later in the FILE by however long the first
        // half lasts in seconds. The first half keeps the fade-in, the
        // second keeps the fade-out.
        let first_len = at_beat - p.start_beat;
        let mut second = p;
        second.id = new_id;
        second.start_beat = at_beat;
        second.length_beats = p.length_beats - first_len;
        second.offset_secs = p.offset_secs + first_len * 60.0 / tempo;
        second.fade_in_beats = 0.0;
        let data = clips[index].data.clone();
        clips[index].params.length_beats = first_len;
        clips[index].params.fade_out_beats = 0.0;
        clips.push(ClipInfo { data, params: second });
        self.next_clip_id += 1;
        self.sync_clips(track)?;
        Ok(new_id)
    }

    // ---------------------------------------------------------------
    // MIDI clips
    // ---------------------------------------------------------------

    /// Flattens a track's MIDI clips into one note timeline for the engine.
    fn sync_sequence(&mut self, track: usize) -> Result {
        let t = self.track_info(track)?;
        let sequence = Sequence::build(t.midi_clips.iter().map(|c| (c.start_beat, c.length_beats, &c.notes[..])));
        self.send(Command::SetSequence { track, sequence: Box::new(sequence) })
    }

    pub fn add_midi_clip(&mut self, track: usize, start_beat: f64, length_beats: f64, notes: Vec<Note>) -> Result<u32> {
        self.track_info(track)?;
        if start_beat < 0.0 || length_beats <= 0.0 {
            return Err("invalid clip position or length".into());
        }
        let id = self.new_clip_id();
        self.track_info_mut(track)?.midi_clips.push(MidiClipInfo { id, start_beat, length_beats, notes });
        self.sync_sequence(track)?;
        Ok(id)
    }

    pub fn remove_midi_clip(&mut self, track: usize, id: u32) -> Result {
        let clips = &mut self.track_info_mut(track)?.midi_clips;
        let before = clips.len();
        clips.retain(|c| c.id != id);
        if clips.len() == before {
            return Err(format!("no midi clip #{id} on track {track}"));
        }
        self.sync_sequence(track)
    }

    /// Edits a MIDI clip (move it, resize it, change its notes).
    pub fn edit_midi_clip(&mut self, track: usize, id: u32, edit: impl FnOnce(&mut MidiClipInfo)) -> Result {
        let clip = self
            .track_info_mut(track)?
            .midi_clips
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or_else(|| format!("no midi clip #{id} on track {track}"))?;
        let mut edited = MidiClipInfo { id, start_beat: clip.start_beat, length_beats: clip.length_beats, notes: clip.notes.clone() };
        edit(&mut edited);
        if edited.start_beat < 0.0 || edited.length_beats <= 0.0 {
            return Err("invalid clip position or length".into());
        }
        edited.id = id;
        *clip = edited;
        self.sync_sequence(track)
    }

    // ---------------------------------------------------------------
    // Moving clips between tracks
    // ---------------------------------------------------------------

    /// Moves an audio clip to another track (and/or time) as ONE action:
    /// everything is checked before anything changes, so a failure can't
    /// leave the clip deleted from one track and missing from the other.
    pub fn move_clip(&mut self, from: usize, id: u32, to: usize, start_beat: f64) -> Result {
        self.track_info(to)?;
        if start_beat < 0.0 {
            return Err("can't move before the start".into());
        }
        let clips = &mut self.track_info_mut(from)?.clips;
        let index = clips.iter().position(|c| c.params.id == id).ok_or_else(|| format!("no clip #{id} on track {from}"))?;
        let mut clip = clips.remove(index);
        clip.params.start_beat = start_beat;
        self.track_info_mut(to)?.clips.push(clip);
        self.sync_clips(from)?;
        if to != from {
            self.sync_clips(to)?;
        }
        Ok(())
    }

    /// The MIDI-clip version of `move_clip`.
    pub fn move_midi_clip(&mut self, from: usize, id: u32, to: usize, start_beat: f64) -> Result {
        self.track_info(to)?;
        if start_beat < 0.0 {
            return Err("can't move before the start".into());
        }
        let clips = &mut self.track_info_mut(from)?.midi_clips;
        let index = clips.iter().position(|c| c.id == id).ok_or_else(|| format!("no midi clip #{id} on track {from}"))?;
        let mut clip = clips.remove(index);
        clip.start_beat = start_beat;
        self.track_info_mut(to)?.midi_clips.push(clip);
        self.sync_sequence(from)?;
        if to != from {
            self.sync_sequence(to)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------
    // Automation
    // ---------------------------------------------------------------

    /// What a lane on `track` would control: its range and scale.
    pub fn automation_info(&self, track: usize, target: AutoTarget) -> Result<ParamInfo> {
        let t = self.track_info(track)?;
        match target {
            AutoTarget::Volume => Ok(VOLUME_INFO),
            AutoTarget::Pan => Ok(PAN_INFO),
            AutoTarget::Node { slot, param } => t
                .slots
                .get(slot)
                .and_then(|s| s.as_ref())
                .and_then(|n| n.params.get(param))
                .copied()
                .ok_or_else(|| "no such effect parameter".into()),
            AutoTarget::Instrument { param } => t
                .instrument
                .as_ref()
                .and_then(|i| i.params.get(param))
                .copied()
                .ok_or_else(|| "no such instrument parameter".into()),
        }
    }

    /// The parameter's current (manual) value, to start a new lane from.
    fn current_value(&self, track: usize, target: AutoTarget) -> Result<f32> {
        let t = self.track_info(track)?;
        Ok(match target {
            AutoTarget::Volume => t.volume_db,
            AutoTarget::Pan => t.pan,
            AutoTarget::Node { slot, param } => t.slots[slot].as_ref().map_or(0.0, |n| n.values[param]),
            AutoTarget::Instrument { param } => t.instrument.as_ref().map_or(0.0, |i| i.values[param]),
        })
    }

    /// Adds a lane for `target` (or finds the existing one); returns its
    /// index. A new lane starts flat at the parameter's current value.
    pub fn add_automation(&mut self, track: usize, target: AutoTarget) -> Result<usize> {
        let info = self.automation_info(track, target)?;
        if let Some(i) = self.track_info(track)?.automation.iter().position(|l| l.target == target) {
            return Ok(i);
        }
        let start = automation::normalize(&info, self.current_value(track, target)?);
        let lanes = &mut self.track_info_mut(track)?.automation;
        lanes.push(AutoLaneInfo { target, points: vec![(0.0, start)] });
        let index = lanes.len() - 1;
        self.sync_automation(track)?;
        Ok(index)
    }

    pub fn set_automation_points(&mut self, track: usize, lane: usize, points: Vec<(f64, f32)>) -> Result {
        let mut points: Vec<(f64, f32)> = points.into_iter().map(|(b, v)| (b.max(0.0), v.clamp(0.0, 1.0))).collect();
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        let lanes = &mut self.track_info_mut(track)?.automation;
        lanes.get_mut(lane).ok_or("no such automation lane")?.points = points;
        self.sync_automation(track)
    }

    pub fn remove_automation(&mut self, track: usize, lane: usize) -> Result {
        let lanes = &mut self.track_info_mut(track)?.automation;
        if lane >= lanes.len() {
            return Err("no such automation lane".into());
        }
        lanes.remove(lane);
        self.sync_automation(track)
    }

    fn drop_lanes(&mut self, target: Target, matches: impl Fn(AutoTarget) -> bool) -> Result {
        let Target::Track(track) = target else { return Ok(()) };
        let lanes = &mut self.track_info_mut(track)?.automation;
        let before = lanes.len();
        lanes.retain(|l| !matches(l.target));
        if lanes.len() != before {
            self.sync_automation(track)?;
        }
        Ok(())
    }

    /// Builds the engine's snapshot of a track's lanes and sends it.
    fn sync_automation(&mut self, track: usize) -> Result {
        let lanes = self.track_info(track)?.automation.clone();
        let mut set = AutomationSet::default();
        for lane in lanes {
            let info = self.automation_info(track, lane.target)?;
            set.lanes.push(Lane::new(lane.target, info, Envelope::new(lane.points)));
        }
        self.send(Command::SetAutomation { track, automation: Box::new(set) })
    }

    // ---------------------------------------------------------------
    // Live MIDI input
    // ---------------------------------------------------------------

    pub fn midi_ports(&self) -> Result<Vec<String>> {
        let input = MidiInput::new("daw").map_err(|e| format!("MIDI unavailable: {e}"))?;
        Ok(input.ports().iter().map(|p| input.port_name(p).unwrap_or_else(|_| "?".into())).collect())
    }

    /// Opens MIDI input port `index`; returns its name.
    pub fn open_midi(&mut self, index: usize) -> Result<String> {
        // Only one input at a time: close the old one first.
        drop(self.midi_conn.take());
        let input = MidiInput::new("daw").map_err(|e| e.to_string())?;
        let port = input.ports().get(index).cloned().ok_or("no such port")?;
        let name = input.port_name(&port).unwrap_or_default();
        let tx = self.live_tx.clone();
        // This closure runs on the MIDI driver's thread. Stamp the arrival
        // time immediately; the audio thread turns it into a sample.
        let conn = input
            .connect(
                &port,
                "daw-in",
                move |_, bytes, _| {
                    if let Some(msg) = MidiMessage::parse(bytes) {
                        let _ = tx.lock().unwrap().push(LiveMidi { at: Instant::now(), msg });
                    }
                },
                (),
            )
            .map_err(|e| e.to_string())?;
        self.midi_conn = Some(conn);
        Ok(name)
    }

    pub fn close_midi(&mut self) {
        drop(self.midi_conn.take());
    }

    pub fn midi_open(&self) -> bool {
        self.midi_conn.is_some()
    }

    pub fn arm(&mut self, track: Option<usize>) -> Result {
        if let Some(t) = track {
            self.track_info(t)?;
        }
        self.send(Command::Arm { track })?;
        self.armed = track;
        Ok(())
    }
}

/// Four bars of an arpeggio (Am, F, C, G) with a bass note, so there's
/// something to hear without entering every note.
pub fn demo_notes() -> Vec<Note> {
    let chords = [[57u8, 60, 64], [53, 57, 60], [48, 52, 55], [55, 59, 62]];
    let pattern = [0usize, 1, 2, 3, 2, 1, 0, 1]; // 3 = root an octave up
    let mut notes = Vec::new();
    for (bar, chord) in chords.iter().enumerate() {
        let bar_start = bar as f64 * BEATS_PER_BAR;
        notes.push(Note { start: bar_start, length: 3.5, pitch: chord[0] - 12, velocity: 90 });
        for (step, &p) in pattern.iter().enumerate() {
            let pitch = if p == 3 { chord[0] + 12 } else { chord[p] };
            let velocity = if step % 2 == 0 { 100 } else { 75 };
            notes.push(Note { start: bar_start + step as f64 * 0.5, length: 0.45, pitch, velocity });
        }
    }
    notes
}

/// Removes quotes a terminal may add when you drag a file in, and expands
/// a leading `~` (a shell feature: Rust's file APIs treat it literally).
pub fn clean_path(raw: &str) -> PathBuf {
    let t = raw.trim();
    let t = t
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| t.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(t);
    if let Some(rest) = t.strip_prefix("~/").or_else(|| t.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(t)
}

/// Audio file types the loader understands, for filtering dropped files.
pub fn is_audio_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("wav" | "flac" | "mp3" | "ogg" | "aif" | "aiff")
    )
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::mixer::MIX_CHANNELS;
    use crate::transport::Transport;
    use rtrb::RingBuffer;

    pub const INFO: PrepareInfo = PrepareInfo { sample_rate: 48_000.0, max_frames: 512, channels: MIX_CHANNELS };

    /// A session wired to queues we can inspect, with no audio device.
    pub struct TestSession {
        pub session: Session,
        pub commands: Consumer<Command>,
        pub garbage: Producer<Garbage>,
        pub live: Consumer<LiveMidi>,
    }

    impl TestSession {
        pub fn new() -> Self {
            let (tx, commands) = RingBuffer::<Command>::new(1024);
            let (gtx, grx) = RingBuffer::<Garbage>::new(16);
            let (ltx, lrx) = RingBuffer::<LiveMidi>::new(16);
            let session = Session::new(INFO, tx, grx, TransportShared::new(), Meters::new(), ltx);
            Self { session, commands, garbage: gtx, live: lrx }
        }

        /// Every command sent since the last call, as short labels.
        pub fn sent(&mut self) -> Vec<String> {
            let mut out = Vec::new();
            while let Ok(c) = self.commands.pop() {
                out.push(match c {
                    Command::AddTrack { index, .. } => format!("AddTrack {index}"),
                    Command::RemoveTrack { index } => format!("RemoveTrack {index}"),
                    Command::SetClips { track, .. } => format!("SetClips {track}"),
                    Command::SetSequence { track, .. } => format!("SetSequence {track}"),
                    Command::Insert { slot, .. } => format!("Insert {slot}"),
                    Command::SetParam { param, value, .. } => format!("SetParam {param}={value}"),
                    Command::SetVolume { db, .. } => format!("SetVolume {db}"),
                    Command::Play => "Play".into(),
                    Command::Stop => "Stop".into(),
                    Command::Seek { beat } => format!("Seek {beat}"),
                    Command::SetSidechain { track, source } => format!("SetSidechain {track} {source:?}"),
                    _ => "other".into(),
                });
            }
            out
        }

        /// Makes the published transport say the playhead is at `beat`.
        pub fn publish_position(&self, beat: f64, playing: bool) {
            let mut t = Transport::new(48_000.0);
            t.seek_beats(beat);
            t.playing = playing;
            self.session.shared.publish(&t);
        }

        pub fn add_sample(&self, seconds: f64) -> usize {
            let frames = (seconds * 48_000.0) as usize;
            let mut lib = self.session.library.lock().unwrap();
            lib.push(Arc::new(AudioData { name: "s".into(), sample_rate: 48_000.0, channels: vec![vec![0.1; frames]] }));
            lib.len() - 1
        }
    }

    #[test]
    fn tracks_update_the_mirror_and_the_engine() {
        let mut t = TestSession::new();
        let a = t.session.add_track(None).unwrap();
        let b = t.session.add_track(Some("Bass".into())).unwrap();
        assert_eq!((a, b), (0, 1));
        assert_eq!(t.session.project.tracks[1].as_ref().unwrap().name, "Bass");
        t.session.set_volume(Target::Track(1), -6.0).unwrap();
        assert_eq!(t.session.project.tracks[1].as_ref().unwrap().volume_db, -6.0);
        assert_eq!(t.sent(), vec!["AddTrack 0", "AddTrack 1", "SetVolume -6"]);
        assert!(t.session.set_volume(Target::Track(7), 0.0).is_err(), "no track 7");
    }

    #[test]
    fn effects_record_their_parameter_values() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        let slot = t.session.add_node(Target::Track(0), "delay", None).unwrap();
        let node = t.session.project.tracks[0].as_ref().unwrap().slots[slot].clone().unwrap();
        assert_eq!(node.name, "delay");
        assert_eq!(node.values[0], 250.0, "starts at the declared default");
        let fb = t.session.param_index(Target::Track(0), slot, "feedback").unwrap();
        t.session.set_param(Target::Track(0), slot, fb, 2.0).unwrap(); // above max: clamped
        let node = t.session.project.tracks[0].as_ref().unwrap().slots[slot].clone().unwrap();
        assert_eq!(node.values[fb], 0.95);
        assert!(t.session.param_index(Target::Track(0), slot, "nope").unwrap_err().contains("feedback"));
    }

    #[test]
    fn return_to_start() {
        let mut t = TestSession::new();
        t.publish_position(8.0, false);
        t.session.play().unwrap();
        t.publish_position(20.0, true);
        t.session.stop().unwrap();
        assert_eq!(t.sent(), vec!["Play", "Stop", "Seek 8"]);
        t.session.return_to_start = false;
        t.session.play().unwrap();
        t.session.stop().unwrap();
        assert_eq!(t.sent(), vec!["Play", "Stop"]);
    }

    #[test]
    fn split_keeps_the_sound_continuous() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        let s = t.add_sample(10.0);
        let id = t.session.add_clip(0, s, 4.0).unwrap();
        t.session.edit_clip(0, id, |p| {
            p.fade_in_beats = 0.5;
            p.fade_out_beats = 1.0;
        })
        .unwrap();
        let second = t.session.split_clip(0, id, 6.0).unwrap();
        let (a, b) = (t.session.clip(0, id).unwrap().params, t.session.clip(0, second).unwrap().params);
        assert_eq!((a.start_beat, a.length_beats), (4.0, 2.0));
        assert_eq!(b.start_beat, 6.0);
        assert!((b.offset_secs - 1.0).abs() < 1e-9, "2 beats at 120 BPM = 1 s into the file");
        assert_eq!((a.fade_in_beats, a.fade_out_beats, b.fade_in_beats, b.fade_out_beats), (0.5, 0.0, 0.0, 1.0));
        assert!(t.session.split_clip(0, id, 9.0).is_err(), "outside the clip");
    }

    #[test]
    fn invalid_edits_are_rejected_and_change_nothing() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        let s = t.add_sample(2.0);
        let id = t.session.add_clip(0, s, 0.0).unwrap();
        t.sent();
        assert!(t.session.edit_clip(0, id, |p| p.length_beats = -1.0).is_err());
        assert_eq!(t.session.clip(0, id).unwrap().params.length_beats, 4.0, "2 s at 120 BPM");
        assert!(t.sent().is_empty(), "nothing sent to the engine");
    }

    #[test]
    fn sidechain_loops_are_refused() {
        let mut t = TestSession::new();
        for _ in 0..2 {
            t.session.add_track(None).unwrap();
        }
        t.session.set_sidechain(1, Some(0)).unwrap();
        assert!(t.session.set_sidechain(0, Some(1)).is_err());
        t.session.remove_track(0).unwrap();
        assert_eq!(t.session.project.tracks[1].as_ref().unwrap().key, None, "removed key source");
    }

    #[test]
    fn clean_paths() {
        assert!(!clean_path("~/a.wav").starts_with("~"));
        assert_eq!(clean_path("'/x y/a.wav'"), PathBuf::from("/x y/a.wav"));
        assert!(is_audio_file(Path::new("/a/B.WAV")) && !is_audio_file(Path::new("/a/b.txt")));
    }

    // ---- Step 9 exercises ----

    #[test]
    fn moving_a_clip_to_another_track_is_one_action() {
        let mut t = TestSession::new();
        for _ in 0..2 {
            t.session.add_track(None).unwrap();
        }
        let s = t.add_sample(2.0);
        let id = t.session.add_clip(0, s, 4.0).unwrap();
        t.sent();
        t.session.move_clip(0, id, 1, 8.0).unwrap();
        assert!(t.session.clip(0, id).is_none());
        assert_eq!(t.session.clip(1, id).unwrap().params.start_beat, 8.0, "same id, new track and time");
        assert_eq!(t.sent(), vec!["SetClips 0", "SetClips 1"]);
        // A bad destination changes nothing at all.
        assert!(t.session.move_clip(1, id, 9, 0.0).is_err());
        assert!(t.session.clip(1, id).is_some(), "still where it was");
        assert!(t.sent().is_empty());
    }

    #[test]
    fn automation_lanes_follow_their_targets() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        t.session.set_volume(Target::Track(0), -6.0).unwrap();
        let lane = t.session.add_automation(0, AutoTarget::Volume).unwrap();
        let start = t.session.project.tracks[0].as_ref().unwrap().automation[lane].points[0].1;
        assert!((automation::denormalize(&VOLUME_INFO, start) - -6.0).abs() < 1e-4, "starts at the current value");
        assert_eq!(t.session.add_automation(0, AutoTarget::Volume).unwrap(), lane, "one lane per target");

        // Lanes aimed at an effect go away with it.
        let slot = t.session.add_node(Target::Track(0), "filter", None).unwrap();
        t.session.add_automation(0, AutoTarget::Node { slot, param: 1 }).unwrap();
        assert_eq!(t.session.project.tracks[0].as_ref().unwrap().automation.len(), 2);
        t.session.remove_node(Target::Track(0), slot).unwrap();
        let lanes = &t.session.project.tracks[0].as_ref().unwrap().automation;
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].target, AutoTarget::Volume);
        assert!(t.session.add_automation(0, AutoTarget::Node { slot, param: 1 }).is_err(), "nothing there now");
    }

    #[test]
    fn replacing_the_synth_drops_its_lanes() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        t.session.set_synth(0, true).unwrap();
        t.session.add_automation(0, AutoTarget::Instrument { param: 6 }).unwrap();
        t.session.set_synth(0, false).unwrap();
        assert!(t.session.project.tracks[0].as_ref().unwrap().automation.is_empty());
    }

    // ---- Step 10: recording ----

    fn with_recorder(t: &mut TestSession) -> mpsc::Sender<Finished> {
        let (tx, rx) = mpsc::channel();
        t.session.attach_recorder(Recorder { device: "test".into(), done: rx, shared: RecordShared::new() });
        tx
    }

    fn finished(track: usize, start_frame: i64, seconds: f64) -> Finished {
        let frames = (seconds * 48_000.0) as usize;
        let audio = Arc::new(AudioData { name: "take-001.wav".into(), sample_rate: 48_000.0, channels: vec![vec![0.1; frames]; 2] });
        Finished { take: 1, track, start_frame, path: Ok(PathBuf::from("recordings/take-001.wav")), audio }
    }

    #[test]
    fn finished_takes_become_clips() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        let tx = with_recorder(&mut t);
        tx.send(finished(0, 96_000, 2.0)).unwrap(); // 2 s from beat 4
        let msgs = t.session.poll_recordings();
        assert!(msgs[0].starts_with("recorded take 1"), "{msgs:?}");
        let clip = &t.session.project.tracks[0].as_ref().unwrap().clips[0];
        assert_eq!((clip.params.start_beat, clip.params.length_beats, clip.params.offset_secs), (4.0, 4.0, 0.0));
        assert_eq!(t.session.samples().len(), 1, "and it's in the library");
    }

    #[test]
    fn a_take_starting_before_zero_is_trimmed() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        let tx = with_recorder(&mut t);
        tx.send(finished(0, -2_400, 2.0)).unwrap(); // 50 ms before the start
        t.session.poll_recordings();
        let p = t.session.project.tracks[0].as_ref().unwrap().clips[0].params;
        assert_eq!(p.start_beat, 0.0);
        assert!((p.offset_secs - 0.05).abs() < 1e-9, "the first 50 ms are skipped");
        assert!((p.length_beats - 1.95 * 2.0).abs() < 1e-9, "and the clip is that much shorter");
    }

    #[test]
    fn recording_needs_an_input() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        assert!(t.session.set_record_track(Some(0)).is_err());
        with_recorder(&mut t);
        t.session.set_record_track(Some(0)).unwrap();
        t.session.remove_track(0).unwrap();
        assert_eq!(t.session.rec_track, None, "removing the track disarms it");
    }

    #[test]
    fn recording_end_to_end() {
        use crate::engine::Engine;
        use crate::record::{spawn_writer, RecEvent};
        use std::sync::atomic::Ordering::Relaxed;

        // Session -> engine -> disk writer -> session, all real.
        let TestSession { session: mut s, commands, garbage: _g, live: _l } = TestSession::new();
        let (gtx, grx) = RingBuffer::<Garbage>::new(256);
        let (_ltx, lrx) = RingBuffer::<LiveMidi>::new(16);
        let mut engine = Engine::new(&INFO, 2, s.shared.clone(), s.meters.clone(), commands, gtx, lrx);
        drop(grx);
        let (mut input, samples) = RingBuffer::<f32>::new(48_000 * 2);
        let (rec_tx, rec_rx) = RingBuffer::<RecEvent>::new(1024);
        let shared = RecordShared::new();
        shared.output_latency.store(480, Relaxed); // 10 ms
        engine.set_input(samples, 2, rec_tx, shared.clone());
        let dir = std::env::temp_dir().join("daw_e2e_recording");
        let _ = std::fs::remove_dir_all(&dir);
        let (done_tx, done_rx) = mpsc::channel();
        let writer = spawn_writer(rec_rx, dir.clone(), 48_000, done_tx);
        s.attach_recorder(Recorder { device: "test".into(), done: done_rx, shared });

        s.add_track(Some("Vocals".into())).unwrap();
        s.set_record_track(Some(0)).unwrap();
        s.seek(4.0).unwrap(); // bar 2
        s.play().unwrap();

        // One second: a 220 Hz tone arriving 512 frames per callback.
        let mut out = vec![0f32; 512 * 2];
        let mut n = 0u32;
        for _ in 0..94 {
            for _ in 0..512 {
                let v = (n as f32 * 220.0 / 48_000.0 * std::f32::consts::TAU).sin() * 0.5;
                let _ = input.push(v);
                let _ = input.push(v);
                n += 1;
            }
            engine.render(&mut out);
        }
        s.stop().unwrap();
        engine.render(&mut out);

        // Wait for the disk writer, then let the session place the take.
        let mut msgs = Vec::new();
        for _ in 0..500 {
            msgs = s.poll_recordings();
            if !msgs.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(msgs[0].starts_with("recorded take 1"), "{msgs:?}");
        let clip = &s.project.tracks[0].as_ref().unwrap().clips[0];
        // Bar 2 = sample 96_000; the first input frames were already 512
        // frames old, and output latency is 480: 992 frames = 0.0413 beats.
        assert!((clip.params.start_beat - (96_000.0 - 992.0) / 24_000.0).abs() < 1e-9, "{}", clip.params.start_beat);
        assert_eq!(clip.data.frames(), 94 * 512, "every frame recorded");
        assert!((clip.data.channels[0][100] - (100.0f32 * 220.0 / 48_000.0 * std::f32::consts::TAU).sin() * 0.5).abs() < 1e-6);
        let on_disk = crate::loader::load_audio(&dir.join("take-001.wav")).unwrap();
        assert_eq!(on_disk.channels, clip.data.channels, "the WAV on disk is the take");

        drop(engine);
        writer.join().unwrap();
    }

    #[test]
    fn punch_and_monitor_settings() {
        let mut t = TestSession::new();
        assert!(t.session.set_monitor(true).is_err(), "nothing to monitor without an input");
        with_recorder(&mut t);
        t.session.set_monitor(true).unwrap();
        assert!(t.session.set_punch(Some((8.0, 4.0))).is_err());
        t.session.set_punch(Some((4.0, 8.0))).unwrap();
        assert_eq!(t.session.punch, Some((4.0, 8.0)));
    }
}
