//! Undo and redo, by snapshots and reconciliation.
//!
//! Before every edit, the session keeps a copy of the project. It's cheap:
//! audio is shared through `Arc`s, so a copy duplicates the structure (a
//! few kilobytes), never the samples. Undo swaps the current project for
//! the copy, then RECONCILES the engine: it compares the two projects and
//! sends only the commands that turn one into the other. Undoing a volume
//! change sends one `SetVolume`; nothing else is touched, so reverb tails
//! and playing notes carry on. Loading a project file is the same
//! operation, from an empty project to the loaded one.
//!
//! The alternative, giving every action its own "inverse", puts undo code
//! into every action, and each one is a chance to get it subtly wrong.
//! Here, an action only has to say what it is called.

use super::{build_automation, build_sequence, ClipParams, MidiClipInfo, Note, Result, Session};
use crate::automation::AutoTarget;
use crate::engine::Command;
use crate::mixer::{Target, Track, MAX_SLOTS, MAX_TRACKS};
use crate::project::{NodeInfo, Project, TrackInfo};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many steps of undo to keep.
const MAX_STEPS: usize = 200;
/// Repeated changes to the same thing within this long become one step,
/// so dragging a slider is one undo, not hundreds.
const MERGE_WINDOW: Duration = Duration::from_secs(1);

/// What a continuous gesture changes. Consecutive edits with the same key
/// (within `MERGE_WINDOW`) merge into one undo step.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Merge {
    Volume(Target),
    Pan(usize),
    Send(usize),
    Param(Target, usize, usize),
    InstrumentParam(usize, usize),
    Rename(usize),
    Tempo,
    Loop,
}

#[derive(Debug)]
struct Step {
    label: String,
    project: Project,
    /// Which state of the project this is, for the unsaved-changes check.
    state: u64,
}

#[derive(Debug, Default)]
pub struct History {
    undo: Vec<Step>,
    redo: Vec<Step>,
    last: Option<(Merge, Instant)>,
    /// Every distinct state of the project gets a number. The project is
    /// unsaved exactly when the current state isn't the one last saved.
    current: u64,
    saved: u64,
    next_state: u64,
}

impl History {
    /// Records that the project just changed from `before`.
    pub(crate) fn record(&mut self, label: &str, before: Project, merge: Option<Merge>) {
        self.record_at(label, before, merge, Instant::now());
    }

    fn record_at(&mut self, label: &str, before: Project, merge: Option<Merge>, now: Instant) {
        let merging = matches!((merge, self.last), (Some(k), Some((last, at))) if k == last && now.duration_since(at) < MERGE_WINDOW)
            && !self.undo.is_empty();
        if !merging {
            self.undo.push(Step { label: label.into(), project: before, state: self.current });
            if self.undo.len() > MAX_STEPS {
                self.undo.remove(0);
            }
        }
        // A new edit makes the redo steps unreachable.
        self.redo.clear();
        self.last = merge.map(|k| (k, now));
        self.next_state += 1;
        self.current = self.next_state;
    }

    pub fn undo_label(&self) -> Option<&str> {
        self.undo.last().map(|s| s.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|s| s.label.as_str())
    }

    /// Labels, oldest first.
    pub fn labels(&self) -> (Vec<&str>, Vec<&str>) {
        (self.undo.iter().map(|s| s.label.as_str()).collect(), self.redo.iter().rev().map(|s| s.label.as_str()).collect())
    }

    pub fn unsaved(&self) -> bool {
        self.current != self.saved
    }

    pub(crate) fn mark_saved(&mut self) {
        self.saved = self.current;
    }

    /// For a recovered project: its changes aren't in the real file yet.
    pub(crate) fn mark_unsaved(&mut self) {
        self.saved = u64::MAX;
    }

    /// Forgets everything: for a new or freshly loaded project.
    pub(crate) fn reset(&mut self) {
        *self = History::default();
    }

    #[cfg(test)]
    pub(crate) fn expire_merge(&mut self) {
        self.last = None;
    }
}

impl Session {
    /// Runs an edit, and records an undo step if (and only if) it worked.
    fn undoable<T>(&mut self, label: &str, merge: Option<Merge>, edit: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        // Every edit passes through here, so this one check covers them all:
        // a project being loaded is about to replace this one, and an edit
        // made now would be silently lost.
        if self.loading.is_some() {
            return Err("a project is loading: edits will be possible once it's open".into());
        }
        let before = self.project.clone();
        let result = edit(self);
        if result.is_ok() {
            self.history.record(label, before, merge);
        }
        result
    }

    pub fn undo(&mut self) -> Result<String> {
        let step = self.history.undo.pop().ok_or("nothing to undo")?;
        let current = std::mem::replace(&mut self.project, step.project.clone());
        self.reconcile(&current, &step.project)?;
        let state = self.history.current;
        self.history.redo.push(Step { label: step.label.clone(), project: current, state });
        self.history.current = step.state;
        self.history.last = None;
        self.forget_missing_tracks();
        Ok(step.label)
    }

    pub fn redo(&mut self) -> Result<String> {
        let step = self.history.redo.pop().ok_or("nothing to redo")?;
        let current = std::mem::replace(&mut self.project, step.project.clone());
        self.reconcile(&current, &step.project)?;
        let state = self.history.current;
        self.history.undo.push(Step { label: step.label.clone(), project: current, state });
        self.history.current = step.state;
        self.history.last = None;
        self.forget_missing_tracks();
        Ok(step.label)
    }

    /// After undo/redo/load, arming can't point at a track that's gone (the
    /// engine forgets it the same way when the track is removed).
    pub(super) fn forget_missing_tracks(&mut self) {
        let exists = |t: Option<usize>, p: &Project| t.filter(|&t| p.tracks.get(t).map_or(false, |x| x.is_some()));
        self.armed = exists(self.armed, &self.project);
        self.rec_track = exists(self.rec_track, &self.project);
    }

    // -----------------------------------------------------------------
    // Reconciliation: make the engine go from `from` to `to`.
    // -----------------------------------------------------------------

    pub(super) fn reconcile(&mut self, from: &Project, to: &Project) -> Result {
        if from.tempo != to.tempo {
            self.send(Command::SetTempo { bpm: to.tempo })?;
        }
        if from.loop_range != to.loop_range {
            self.send(Command::SetLoop { range: to.loop_range })?;
        }
        // Tracks appearing and disappearing first, so sidechains below can
        // point at any track that exists in `to`.
        let fresh = TrackInfo::new(String::new()); // what a new engine track starts as
        for i in 0..MAX_TRACKS {
            match (&from.tracks[i], &to.tracks[i]) {
                (Some(_), None) => self.send(Command::RemoveTrack { index: i })?,
                (None, Some(_)) => self.send(Command::AddTrack { index: i, track: Box::new(Track::new(&self.info)) })?,
                _ => {}
            }
        }
        for i in 0..MAX_TRACKS {
            if let Some(b) = &to.tracks[i] {
                let a = from.tracks[i].as_ref().unwrap_or(&fresh);
                self.reconcile_strip(Target::Track(i), a, b)?;
            }
        }
        self.reconcile_strip(Target::Aux, &from.aux, &to.aux)?;
        self.reconcile_strip(Target::Master, &from.master, &to.master)
    }

    fn reconcile_strip(&mut self, target: Target, a: &TrackInfo, b: &TrackInfo) -> Result {
        if a.volume_db != b.volume_db {
            self.send(Command::SetVolume { target, db: b.volume_db })?;
        }
        // Effects before automation: lanes point at effect slots.
        for slot in 0..MAX_SLOTS {
            match (&a.slots[slot], &b.slots[slot]) {
                (Some(_), None) => self.send(Command::Remove { target, slot })?,
                (None, None) => {}
                (old, Some(new)) => match old.as_ref().filter(|o| o.same_node(new)) {
                    // The same effect (for a plugin: the same instance):
                    // just the parameters that changed.
                    Some(old) => {
                        for (param, (&x, &y)) in old.values.iter().zip(&new.values).enumerate() {
                            if x != y {
                                self.send(Command::SetParam { target, slot, param, value: y })?;
                            }
                        }
                    }
                    // A new (or different) effect, built with its values.
                    None => {
                        let (node, replaced) = self.node_from_info(new)?;
                        match node {
                            Some(node) => self.send(Command::Insert { target, slot, node })?,
                            // A missing plugin plays nothing.
                            None if old.is_some() => self.send(Command::Remove { target, slot })?,
                            None => {}
                        }
                        if let Some(inst) = replaced {
                            self.set_instance(target, Some(slot), inst);
                        }
                    }
                },
            }
        }
        let Target::Track(track) = target else { return Ok(()) };
        if a.pan != b.pan {
            self.send(Command::SetPan { track, pos: b.pan })?;
        }
        if a.mute != b.mute {
            self.send(Command::SetMute { track, on: b.mute })?;
        }
        if a.solo != b.solo {
            self.send(Command::SetSolo { track, on: b.solo })?;
        }
        if a.send_db != b.send_db {
            self.send(Command::SetSend { track, db: b.send_db })?;
        }
        if a.key != b.key {
            self.send(Command::SetSidechain { track, source: b.key })?;
        }
        self.reconcile_instrument(track, a.instrument.as_ref(), b.instrument.as_ref())?;
        // Lists are compared whole: if anything differs, the track gets a
        // fresh snapshot, exactly like after an ordinary edit.
        let same_clips = a.clips.len() == b.clips.len()
            && a.clips.iter().zip(&b.clips).all(|(x, y)| Arc::ptr_eq(&x.data, &y.data) && x.params == y.params);
        if !same_clips {
            let clips = self.build_clips(b);
            self.send(Command::SetClips { track, clips })?;
        }
        if a.midi_clips != b.midi_clips {
            self.send(Command::SetSequence { track, sequence: build_sequence(b) })?;
        }
        if a.automation != b.automation {
            self.send(Command::SetAutomation { track, automation: build_automation(b)? })?;
        }
        Ok(())
    }

    fn reconcile_instrument(&mut self, track: usize, a: Option<&NodeInfo>, b: Option<&NodeInfo>) -> Result {
        match (a, b) {
            (Some(_), None) => self.send(Command::SetInstrument { track, instrument: None }),
            (None, None) => Ok(()),
            (old, Some(new)) => match old.filter(|o| o.same_node(new)) {
                Some(old) => {
                    for (param, (&x, &y)) in old.values.iter().zip(&new.values).enumerate() {
                        if x != y {
                            self.send(Command::SetInstrumentParam { track, param, value: y })?;
                        }
                    }
                    Ok(())
                }
                None => {
                    let (instrument, replaced) = self.instrument_from_info(new)?;
                    if let Some(inst) = replaced {
                        self.set_instance(Target::Track(track), None, inst);
                    }
                    self.send(Command::SetInstrument { track, instrument })
                }
            },
        }
    }

    /// Records that the mirror's plugin at `slot` (or the instrument, if
    /// None) is now a different instance.
    fn set_instance(&mut self, target: Target, slot: Option<usize>, inst: Arc<crate::clap::Instance>) {
        let Some(t) = self.project.get_mut(target) else { return };
        let node = match slot {
            Some(s) => t.slots.get_mut(s).and_then(|n| n.as_mut()),
            None => t.instrument.as_mut(),
        };
        if let Some(p) = node.and_then(|n| n.plugin.as_mut()) {
            p.instance = Some(inst);
        }
    }

    pub fn add_plugin(&mut self, target: Target, path: &std::path::Path, id: &str, slot: Option<usize>) -> Result<usize> {
        let name = self.plugins.iter().find(|p| p.descriptor.id == id).map_or(id.to_string(), |p| p.descriptor.name.clone());
        self.undoable(&format!("add {name}"), None, |s| s.add_plugin_inner(target, path, id, slot))
    }

    pub fn set_plugin_instrument(&mut self, track: usize, path: &std::path::Path, id: &str) -> Result {
        let name = self.plugins.iter().find(|p| p.descriptor.id == id).map_or(id.to_string(), |p| p.descriptor.name.clone());
        self.undoable(&format!("set instrument {name}"), None, |s| s.set_plugin_instrument_inner(track, path, id))
    }

    // -----------------------------------------------------------------
    // Every editing action, made undoable. The real work is in the
    // `_inner` versions in session.rs; these name the step and say which
    // gestures merge.
    // -----------------------------------------------------------------

    pub fn set_tempo(&mut self, bpm: f64) -> Result {
        self.undoable("tempo", Some(Merge::Tempo), |s| s.set_tempo_inner(bpm))
    }
    pub fn set_loop(&mut self, range: Option<(f64, f64)>) -> Result {
        self.undoable("loop", Some(Merge::Loop), |s| s.set_loop_inner(range))
    }
    pub fn add_track(&mut self, name: Option<String>) -> Result<usize> {
        self.undoable("add track", None, |s| s.add_track_inner(name))
    }
    pub fn remove_track(&mut self, index: usize) -> Result {
        self.undoable("remove track", None, |s| s.remove_track_inner(index))
    }
    pub fn rename_track(&mut self, track: usize, name: String) -> Result {
        self.undoable("rename track", Some(Merge::Rename(track)), |s| s.rename_track_inner(track, name))
    }
    pub fn set_volume(&mut self, target: Target, db: f32) -> Result {
        self.undoable("volume", Some(Merge::Volume(target)), |s| s.set_volume_inner(target, db))
    }
    pub fn set_pan(&mut self, track: usize, pos: f32) -> Result {
        self.undoable("pan", Some(Merge::Pan(track)), |s| s.set_pan_inner(track, pos))
    }
    pub fn set_mute(&mut self, track: usize, on: bool) -> Result {
        self.undoable("mute", None, |s| s.set_mute_inner(track, on))
    }
    pub fn set_solo(&mut self, track: usize, on: bool) -> Result {
        self.undoable("solo", None, |s| s.set_solo_inner(track, on))
    }
    pub fn set_send(&mut self, track: usize, db: f32) -> Result {
        self.undoable("send", Some(Merge::Send(track)), |s| s.set_send_inner(track, db))
    }
    pub fn set_sidechain(&mut self, track: usize, source: Option<usize>) -> Result {
        self.undoable("sidechain", None, |s| s.set_sidechain_inner(track, source))
    }
    pub fn add_node(&mut self, target: Target, kind: &str, slot: Option<usize>) -> Result<usize> {
        self.undoable(&format!("add {kind}"), None, |s| s.add_node_inner(target, kind, slot))
    }
    pub fn remove_node(&mut self, target: Target, slot: usize) -> Result {
        self.undoable("remove effect", None, |s| s.remove_node_inner(target, slot))
    }
    pub fn set_param(&mut self, target: Target, slot: usize, param: usize, value: f32) -> Result {
        let label = self.param_label(target, slot, param);
        self.undoable(&label, Some(Merge::Param(target, slot, param)), |s| s.set_param_inner(target, slot, param, value))
    }
    pub fn set_synth(&mut self, track: usize, on: bool) -> Result {
        self.undoable(if on { "add synth" } else { "remove synth" }, None, |s| s.set_synth_inner(track, on))
    }
    pub fn set_instrument_param(&mut self, track: usize, param: usize, value: f32) -> Result {
        let name = self.project.tracks.get(track).and_then(|t| t.as_ref()).and_then(|t| t.instrument.as_ref()).and_then(|i| i.params.get(param)).map_or("?", |p| p.name);
        let label = format!("synth {name}");
        self.undoable(&label, Some(Merge::InstrumentParam(track, param)), |s| s.set_instrument_param_inner(track, param, value))
    }
    pub fn add_clip(&mut self, track: usize, sample: usize, start_beat: f64) -> Result<u32> {
        self.undoable("add clip", None, |s| s.add_clip_inner(track, sample, start_beat))
    }
    pub fn remove_clip(&mut self, track: usize, id: u32) -> Result {
        self.undoable("delete clip", None, |s| s.remove_clip_inner(track, id))
    }
    pub fn edit_clip(&mut self, track: usize, id: u32, edit: impl FnOnce(&mut ClipParams)) -> Result {
        self.undoable("edit clip", None, move |s| s.edit_clip_inner(track, id, edit))
    }
    pub fn split_clip(&mut self, track: usize, id: u32, at_beat: f64) -> Result<u32> {
        self.undoable("split clip", None, |s| s.split_clip_inner(track, id, at_beat))
    }
    pub fn add_midi_clip(&mut self, track: usize, start_beat: f64, length_beats: f64, notes: Vec<Note>) -> Result<u32> {
        self.undoable("add MIDI clip", None, move |s| s.add_midi_clip_inner(track, start_beat, length_beats, notes))
    }
    pub fn remove_midi_clip(&mut self, track: usize, id: u32) -> Result {
        self.undoable("delete MIDI clip", None, |s| s.remove_midi_clip_inner(track, id))
    }
    pub fn edit_midi_clip(&mut self, track: usize, id: u32, edit: impl FnOnce(&mut MidiClipInfo)) -> Result {
        self.undoable("edit notes", None, move |s| s.edit_midi_clip_inner(track, id, edit))
    }
    pub fn move_clip(&mut self, from: usize, id: u32, to: usize, start_beat: f64) -> Result {
        self.undoable("move clip", None, |s| s.move_clip_inner(from, id, to, start_beat))
    }
    pub fn move_midi_clip(&mut self, from: usize, id: u32, to: usize, start_beat: f64) -> Result {
        self.undoable("move MIDI clip", None, |s| s.move_midi_clip_inner(from, id, to, start_beat))
    }
    pub fn add_automation(&mut self, track: usize, target: AutoTarget) -> Result<usize> {
        self.undoable("add automation", None, |s| s.add_automation_inner(track, target))
    }
    pub fn set_automation_points(&mut self, track: usize, lane: usize, points: Vec<(f64, f32)>) -> Result {
        self.undoable("edit automation", None, move |s| s.set_automation_points_inner(track, lane, points))
    }
    pub fn remove_automation(&mut self, track: usize, lane: usize) -> Result {
        self.undoable("remove automation", None, |s| s.remove_automation_inner(track, lane))
    }

    fn param_label(&self, target: Target, slot: usize, param: usize) -> String {
        let node = self.project.get(target).and_then(|t| t.slots.get(slot)).and_then(|s| s.as_ref());
        match node {
            Some(n) => format!("{} {}", n.name, n.params.get(param).map_or("?", |p| p.name)),
            None => "effect parameter".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::TestSession;

    #[test]
    fn undo_and_redo_send_only_what_changed() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        t.session.add_node(Target::Track(0), "reverb", None).unwrap();
        t.session.set_volume(Target::Track(0), -6.0).unwrap();
        t.sent();

        assert_eq!(t.session.undo().unwrap(), "volume");
        assert_eq!(t.session.project.tracks[0].as_ref().unwrap().volume_db, 0.0);
        assert_eq!(t.sent(), vec!["SetVolume 0"], "one command: the reverb keeps ringing");

        assert_eq!(t.session.redo().unwrap(), "volume");
        assert_eq!(t.session.project.tracks[0].as_ref().unwrap().volume_db, -6.0);
        assert_eq!(t.sent(), vec!["SetVolume -6"]);
    }

    #[test]
    fn undoing_a_track_removal_rebuilds_everything_on_it() {
        let mut t = TestSession::new();
        t.session.add_track(Some("Keys".into())).unwrap();
        t.session.set_synth(0, true).unwrap();
        t.session.add_midi_clip(0, 0.0, 16.0, super::super::demo_notes()).unwrap();
        let slot = t.session.add_node(Target::Track(0), "delay", None).unwrap();
        t.session.set_param(Target::Track(0), slot, 2, 0.7).unwrap();
        t.session.add_automation(0, AutoTarget::Volume).unwrap();
        t.session.remove_track(0).unwrap();
        t.sent();

        assert_eq!(t.session.undo().unwrap(), "remove track");
        let track = t.session.project.tracks[0].as_ref().unwrap();
        assert_eq!(track.name, "Keys");
        assert_eq!(track.slots[slot].as_ref().unwrap().values[2], 0.7);
        let sent = t.sent();
        for expected in ["AddTrack 0", "Insert 0", "SetSequence 0", "other"] {
            assert!(sent.iter().any(|s| s == expected), "{expected} missing from {sent:?}");
        }
    }

    #[test]
    fn a_slider_drag_is_one_undo_step() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        let slot = t.session.add_node(Target::Track(0), "filter", None).unwrap();
        for f in (100..2_000).step_by(50) {
            t.session.set_param(Target::Track(0), slot, 1, f as f32).unwrap();
        }
        assert_eq!(t.session.history.undo_label(), Some("filter freq"));
        t.session.undo().unwrap();
        assert_eq!(t.session.project.tracks[0].as_ref().unwrap().slots[slot].as_ref().unwrap().values[1], 1_000.0, "back to before the drag");
        assert_eq!(t.session.history.undo_label(), Some("add filter"));

        // A different parameter, or a pause, starts a new step.
        t.session.set_param(Target::Track(0), slot, 1, 500.0).unwrap();
        t.session.history.expire_merge();
        t.session.set_param(Target::Track(0), slot, 1, 600.0).unwrap();
        t.session.undo().unwrap();
        assert_eq!(t.session.project.tracks[0].as_ref().unwrap().slots[slot].as_ref().unwrap().values[1], 500.0);
    }

    #[test]
    fn a_new_edit_clears_redo_and_failures_leave_no_step() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        t.session.set_mute(0, true).unwrap();
        t.session.undo().unwrap();
        assert_eq!(t.session.history.redo_label(), Some("mute"));
        t.session.set_solo(0, true).unwrap();
        assert_eq!(t.session.history.redo_label(), None);
        assert!(t.session.set_volume(Target::Track(9), 0.0).is_err());
        assert_eq!(t.session.history.undo_label(), Some("solo"), "the failed edit recorded nothing");
        assert!(t.session.redo().is_err());
    }

    #[test]
    fn unsaved_changes_follow_undo_and_redo() {
        let mut t = TestSession::new();
        assert!(!t.session.history.unsaved());
        t.session.add_track(None).unwrap();
        assert!(t.session.history.unsaved());
        t.session.history.mark_saved();
        assert!(!t.session.history.unsaved());
        t.session.set_mute(0, true).unwrap();
        assert!(t.session.history.unsaved());
        t.session.undo().unwrap();
        assert!(!t.session.history.unsaved(), "undone back to exactly the saved state");
        t.session.redo().unwrap();
        assert!(t.session.history.unsaved());
    }

    #[test]
    fn undo_of_tempo_and_arming_stay_consistent() {
        let mut t = TestSession::new();
        t.session.add_track(None).unwrap();
        t.session.set_tempo(90.0).unwrap();
        t.session.undo().unwrap();
        assert_eq!(t.session.tempo(), 120.0);
        // Arming isn't part of the song, but it can't outlive its track.
        t.session.arm(Some(0)).unwrap();
        t.session.undo().unwrap(); // undoes "add track"
        assert_eq!(t.session.armed, None);
    }
}
