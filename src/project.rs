//! The main thread's model of the project: what tracks exist, what's on
//! them, and their mixer settings. The engine holds the real-time copy;
//! this is what the UI reads, and later what gets saved to disk.

use crate::automation::AutoTarget;
use crate::clip::ClipParams;
use crate::loader::AudioData;
use crate::midi::Note;
use crate::mixer::{Target, MAX_SLOTS, MAX_TRACKS};
use crate::processor::ParamInfo;
use std::sync::Arc;

/// The main thread's copy of a clip. This is the editable one; the engine
/// gets rebuilt `AudioClip`s from it after every change.
pub struct ClipInfo {
    pub data: Arc<AudioData>,
    pub params: ClipParams,
}

/// What the main thread knows about a node or instrument: its name, its
/// parameters, and their current values. The node itself lives on the
/// audio thread; this mirror is what the UI draws.
#[derive(Clone, Debug)]
pub struct NodeInfo {
    pub name: &'static str,
    pub params: &'static [ParamInfo],
    pub values: Vec<f32>,
}

impl NodeInfo {
    /// Starts every value at its declared default, which a test guarantees
    /// matches the node's real initial state.
    pub fn new(name: &'static str, params: &'static [ParamInfo]) -> Self {
        Self { name, params, values: params.iter().map(|p| p.default).collect() }
    }

    pub fn param_index(&self, name: &str) -> Option<usize> {
        self.params.iter().position(|p| p.name == name)
    }
}

pub type SlotInfo = Option<NodeInfo>;

/// A MIDI clip: notes relative to the clip's start. Only the main thread
/// sees clips; the engine gets them flattened into a `Sequence`.
pub struct MidiClipInfo {
    pub id: u32,
    pub start_beat: f64,
    pub length_beats: f64,
    pub notes: Vec<Note>,
}

/// An automation lane in the project: what it controls, and breakpoints
/// as (beat, normalized 0..1 value).
#[derive(Clone, Debug)]
pub struct AutoLaneInfo {
    pub target: AutoTarget,
    pub points: Vec<(f64, f32)>,
}

pub struct TrackInfo {
    /// Names only live here. The engine has no use for them.
    pub name: String,
    pub slots: Vec<SlotInfo>,
    pub clips: Vec<ClipInfo>,
    pub midi_clips: Vec<MidiClipInfo>,
    /// The instrument's name and parameter names, if the track has one.
    pub instrument: SlotInfo,
    pub volume_db: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    /// Send level to the aux bus. -60 dB means off.
    pub send_db: f32,
    /// The track this one's sidechain listens to.
    pub key: Option<usize>,
    pub automation: Vec<AutoLaneInfo>,
}

impl TrackInfo {
    pub fn new(name: String) -> Self {
        Self {
            name,
            slots: vec![None; MAX_SLOTS],
            clips: Vec::new(),
            midi_clips: Vec::new(),
            instrument: None,
            volume_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            send_db: -60.0,
            key: None,
            automation: Vec::new(),
        }
    }

    pub fn first_free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.is_none())
    }
}

pub struct Project {
    pub tracks: Vec<Option<TrackInfo>>,
    pub aux: TrackInfo,
    pub master: TrackInfo,
}

impl Project {
    pub fn new() -> Self {
        Self {
            tracks: (0..MAX_TRACKS).map(|_| None).collect(),
            aux: TrackInfo::new("Aux".into()),
            master: TrackInfo::new("Master".into()),
        }
    }

    pub fn get(&self, target: Target) -> Option<&TrackInfo> {
        match target {
            Target::Master => Some(&self.master),
            Target::Aux => Some(&self.aux),
            Target::Track(i) => self.tracks.get(i)?.as_ref(),
        }
    }

    pub fn get_mut(&mut self, target: Target) -> Option<&mut TrackInfo> {
        match target {
            Target::Master => Some(&mut self.master),
            Target::Aux => Some(&mut self.aux),
            Target::Track(i) => self.tracks.get_mut(i)?.as_mut(),
        }
    }

    /// Would making `track` listen to `source` create a loop (A keyed by B
    /// keyed by A)? Then neither could render first.
    pub fn sidechain_creates_cycle(&self, track: usize, source: usize) -> bool {
        let mut current = Some(source);
        for _ in 0..=MAX_TRACKS {
            match current {
                Some(t) if t == track => return true,
                Some(t) => current = self.tracks[t].as_ref().and_then(|ti| ti.key),
                None => return false,
            }
        }
        true
    }

    pub fn first_free_track(&self) -> Option<usize> {
        self.tracks.iter().position(|t| t.is_none())
    }

    /// Parses "m" (master), "a" (aux), or a track number, if that track exists.
    pub fn parse_target(&self, s: &str) -> Option<Target> {
        match s {
            "m" => return Some(Target::Master),
            "a" => return Some(Target::Aux),
            _ => {}
        }
        let i: usize = s.parse().ok()?;
        self.tracks.get(i)?.as_ref().map(|_| Target::Track(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidechain_cycles_are_detected() {
        let mut p = Project::new();
        for i in 0..3 {
            p.tracks[i] = Some(TrackInfo::new(format!("{i}")));
        }
        p.tracks[1].as_mut().unwrap().key = Some(0); // 1 listens to 0
        p.tracks[2].as_mut().unwrap().key = Some(1); // 2 listens to 1
        assert!(p.sidechain_creates_cycle(0, 2), "0 -> 2 -> 1 -> 0");
        assert!(p.sidechain_creates_cycle(1, 1), "a track can't key itself");
        assert!(p.sidechain_creates_cycle(0, 1), "0 -> 1 -> 0");
        assert!(!p.sidechain_creates_cycle(2, 0), "2 listening to 0 is fine");
    }
}
