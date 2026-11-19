//! Project files: saving and opening.
//!
//! The file format is its own set of types, converted to and from the
//! in-memory ones. Saving `Project` directly would freeze its internal
//! layout as a file format forever; this way the program can change while
//! old files keep opening. Parameters are stored BY NAME, not by position,
//! so adding a parameter to an effect later doesn't scramble old projects,
//! and names a newer version doesn't know are skipped with a warning.
//!
//! Audio isn't stored in the project file, only referenced: by a path
//! relative to the project file when it's inside the project's folder (so
//! the folder can be moved as a whole), absolute otherwise. A missing file
//! is searched for by name next to the project; if it's still not found,
//! its clips stay as silent placeholders that remember the path, so saving
//! again never loses them.

use super::{Result, Session};
use crate::automation::AutoTarget;
use crate::loader::{self, AudioData};
use crate::mixer::{MAX_SLOTS, MAX_TRACKS};
use crate::nodes;
use crate::processor::Instrument;
use crate::project::{AutoLaneInfo, ClipInfo, MidiClipInfo, NodeInfo, Project, TrackInfo};
use crate::record::WavWriter;
use crate::synth::Synth;
use crate::clip::ClipParams;
use crate::midi::Note;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FORMAT: &str = "daw-project";
const VERSION: u32 = 1;
pub const EXTENSION: &str = "daw.json";

// ---------------------------------------------------------------------
// The format
// ---------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ProjectFile {
    pub format: String,
    pub version: u32,
    pub tempo: f64,
    #[serde(default)]
    pub loop_range: Option<(f64, f64)>,
    /// Every audio file the project uses; clips refer to these by index.
    pub files: Vec<FileRef>,
    pub tracks: Vec<TrackFile>,
    pub aux: StripFile,
    pub master: StripFile,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct FileRef {
    pub path: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct StripFile {
    pub volume_db: f32,
    #[serde(default)]
    pub effects: Vec<EffectFile>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct TrackFile {
    pub index: usize,
    pub name: String,
    #[serde(flatten)]
    pub strip: StripFile,
    #[serde(default)]
    pub pan: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub solo: bool,
    #[serde(default = "off_db")]
    pub send_db: f32,
    #[serde(default)]
    pub sidechain: Option<usize>,
    #[serde(default)]
    pub instrument: Option<NodeFile>,
    #[serde(default)]
    pub clips: Vec<ClipFile>,
    #[serde(default)]
    pub midi_clips: Vec<MidiClipFile>,
    #[serde(default)]
    pub automation: Vec<LaneFile>,
}

fn off_db() -> f32 {
    -60.0
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct NodeFile {
    pub kind: String,
    /// By name. A BTreeMap so the file's order is stable (diff-friendly).
    pub params: BTreeMap<String, f32>,
    /// For a CLAP plugin (kind "clap"): which one, and its saved state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginFile>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct PluginFile {
    pub path: String,
    pub id: String,
    /// The plugin's own opaque state, base64. It holds what parameters
    /// can't (a loaded preset, internal settings), so when it's present
    /// it's what gets restored; the parameters are a readable record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct EffectFile {
    pub slot: usize,
    #[serde(flatten)]
    pub node: NodeFile,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ClipFile {
    pub id: u32,
    pub file: usize,
    pub start_beat: f64,
    pub length_beats: f64,
    #[serde(default)]
    pub offset_secs: f64,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub fade_in_beats: f64,
    #[serde(default)]
    pub fade_out_beats: f64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct MidiClipFile {
    pub id: u32,
    pub start_beat: f64,
    pub length_beats: f64,
    pub notes: Vec<NoteFile>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct NoteFile {
    pub start: f64,
    pub length: f64,
    pub pitch: u8,
    pub velocity: u8,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct LaneFile {
    pub target: TargetFile,
    pub points: Vec<(f64, f32)>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetFile {
    Volume,
    Pan,
    Effect { slot: usize, param: String },
    Instrument { param: String },
}

// ---------------------------------------------------------------------
// Project -> file
// ---------------------------------------------------------------------

fn node_file(n: &NodeInfo) -> NodeFile {
    let params = n.params.iter().zip(&n.values).map(|(p, &v)| (p.name.to_string(), v)).collect();
    let Some(p) = &n.plugin else { return NodeFile { kind: n.name.into(), params, plugin: None } };
    // Ask the live plugin for its state [main thread]; a placeholder keeps
    // the state it was loaded with.
    let state = p.instance.as_ref().and_then(|i| i.save_state()).or_else(|| p.state.as_ref().map(|s| s.to_vec()));
    NodeFile {
        kind: "clap".into(),
        params,
        plugin: Some(PluginFile { path: p.path.to_string_lossy().into_owned(), id: p.id.clone(), state: state.map(|s| crate::clap::base64_encode(&s)) }),
    }
}

fn strip_file(t: &TrackInfo) -> StripFile {
    let effects = t.slots.iter().enumerate().filter_map(|(slot, n)| Some(EffectFile { slot, node: node_file(n.as_ref()?) })).collect();
    StripFile { volume_db: t.volume_db, effects }
}

/// A path as it should be written: relative (with '/' separators, so the
/// file works on any OS) if it's inside the project's folder.
fn portable(path: &Path, project_dir: &Path) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    match abs.strip_prefix(&dir) {
        Ok(rel) => rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"),
        Err(_) => abs.to_string_lossy().into_owned(),
    }
}

pub fn to_file(project: &Project, files: &[Arc<AudioData>], project_dir: &Path) -> ProjectFile {
    let index = |d: &Arc<AudioData>| files.iter().position(|f| Arc::ptr_eq(f, d)).expect("every clip's audio is in the list");
    let tracks = project
        .tracks
        .iter()
        .enumerate()
        .filter_map(|(i, t)| Some((i, t.as_ref()?)))
        .map(|(index_, t)| TrackFile {
            index: index_,
            name: t.name.clone(),
            strip: strip_file(t),
            pan: t.pan,
            mute: t.mute,
            solo: t.solo,
            send_db: t.send_db,
            sidechain: t.key,
            instrument: t.instrument.as_ref().map(node_file),
            clips: t
                .clips
                .iter()
                .map(|c| {
                    let p = c.params;
                    ClipFile {
                        id: p.id,
                        file: index(&c.data),
                        start_beat: p.start_beat,
                        length_beats: p.length_beats,
                        offset_secs: p.offset_secs,
                        gain_db: p.gain_db,
                        fade_in_beats: p.fade_in_beats,
                        fade_out_beats: p.fade_out_beats,
                    }
                })
                .collect(),
            midi_clips: t
                .midi_clips
                .iter()
                .map(|c| MidiClipFile {
                    id: c.id,
                    start_beat: c.start_beat,
                    length_beats: c.length_beats,
                    notes: c.notes.iter().map(|n| NoteFile { start: n.start, length: n.length, pitch: n.pitch, velocity: n.velocity }).collect(),
                })
                .collect(),
            automation: t
                .automation
                .iter()
                .filter_map(|l| {
                    let target = match l.target {
                        AutoTarget::Volume => TargetFile::Volume,
                        AutoTarget::Pan => TargetFile::Pan,
                        AutoTarget::Node { slot, param } => {
                            TargetFile::Effect { slot, param: t.slots.get(slot)?.as_ref()?.params.get(param)?.name.into() }
                        }
                        AutoTarget::Instrument { param } => TargetFile::Instrument { param: t.instrument.as_ref()?.params.get(param)?.name.into() },
                    };
                    Some(LaneFile { target, points: l.points.clone() })
                })
                .collect(),
        })
        .collect();
    ProjectFile {
        format: FORMAT.into(),
        version: VERSION,
        tempo: project.tempo,
        loop_range: project.loop_range,
        files: files
            .iter()
            .map(|f| FileRef { path: f.path.as_deref().map_or_else(String::new, |p| portable(p, project_dir)), name: f.name.clone() })
            .collect(),
        tracks,
        aux: strip_file(&project.aux),
        master: strip_file(&project.master),
    }
}

// ---------------------------------------------------------------------
// File -> project
// ---------------------------------------------------------------------

/// Loads a plugin node: path, id, decoded state.
pub type PluginLoader<'a> = dyn FnMut(&Path, &str, Option<Vec<u8>>, &mut Vec<String>) -> NodeInfo + 'a;

/// A node of `kind` with values set by name. Unknown names are reported;
/// parameters the file doesn't mention keep their defaults.
fn node_info(file: &NodeFile, make: impl Fn(&str) -> Option<NodeInfo>, plugins: &mut PluginLoader, warnings: &mut Vec<String>) -> Option<NodeInfo> {
    if let Some(p) = &file.plugin {
        let state = p.state.as_deref().and_then(|s| {
            let bytes = crate::clap::base64_decode(s);
            if bytes.is_none() {
                warnings.push(format!("plugin {}'s saved state is damaged; using its defaults", p.id));
            }
            bytes
        });
        let has_state = state.is_some();
        let mut info = plugins(Path::new(&p.path), &p.id, state, warnings);
        if !has_state {
            set_by_name(&mut info, &file.params, warnings);
        }
        return Some(info);
    }
    let Some(mut info) = make(&file.kind) else {
        warnings.push(format!("unknown effect or instrument \"{}\" skipped", file.kind));
        return None;
    };
    set_by_name(&mut info, &file.params, warnings);
    Some(info)
}

fn set_by_name(info: &mut NodeInfo, params: &BTreeMap<String, f32>, warnings: &mut Vec<String>) {
    if info.plugin.as_ref().map_or(false, |p| p.instance.is_none()) {
        return; // a missing plugin: nothing to set
    }
    for (name, &value) in params {
        match info.param_index(name) {
            Some(i) => info.values[i] = value.clamp(info.params[i].min, info.params[i].max),
            None => warnings.push(format!("{} has no parameter \"{name}\" (ignored)", info.name)),
        }
    }
}

fn make_effect(kind: &str) -> Option<NodeInfo> {
    let node = nodes::make(kind)?;
    Some(NodeInfo::new(node.name(), node.param_info()))
}

fn make_instrument(kind: &str) -> Option<NodeInfo> {
    (kind == "synth").then(|| {
        let s = Synth::new();
        NodeInfo::new(s.name(), s.param_info())
    })
}

fn strip_from(file: &StripFile, info: &mut TrackInfo, plugins: &mut PluginLoader, warnings: &mut Vec<String>) {
    info.volume_db = file.volume_db.clamp(-60.0, 12.0);
    for e in &file.effects {
        if e.slot >= MAX_SLOTS {
            warnings.push(format!("effect in slot {} skipped (slots go up to {})", e.slot, MAX_SLOTS - 1));
            continue;
        }
        info.slots[e.slot] = node_info(&e.node, make_effect, plugins, warnings);
    }
}

/// Builds the project from a parsed file and its (already found or
/// placeholder) audio. Returns the project, the highest clip id, and
/// warnings about anything that couldn't be used.
pub fn from_file(file: &ProjectFile, audio: &[Arc<AudioData>], plugins: &mut PluginLoader) -> std::result::Result<(Project, u32, Vec<String>), String> {
    if file.format != FORMAT {
        return Err("this isn't a daw project file".into());
    }
    if file.version > VERSION {
        return Err(format!("this project was saved by a newer version (format {}; this version reads up to {VERSION})", file.version));
    }
    let mut warnings = Vec::new();
    let mut project = Project::new();
    project.tempo = file.tempo.clamp(20.0, 999.0);
    project.loop_range = file.loop_range.filter(|(a, b)| *a >= 0.0 && b > a);
    let mut max_id = 0;
    for t in &file.tracks {
        if t.index >= MAX_TRACKS || project.tracks[t.index].is_some() {
            warnings.push(format!("track {} skipped (bad or repeated index)", t.index));
            continue;
        }
        let mut info = TrackInfo::new(t.name.clone());
        strip_from(&t.strip, &mut info, plugins, &mut warnings);
        info.pan = t.pan.clamp(-1.0, 1.0);
        info.mute = t.mute;
        info.solo = t.solo;
        info.send_db = t.send_db.clamp(-60.0, 12.0);
        info.key = t.sidechain.filter(|&k| k < MAX_TRACKS && k != t.index);
        info.instrument = t.instrument.as_ref().and_then(|n| node_info(n, make_instrument, plugins, &mut warnings));
        for c in &t.clips {
            let Some(data) = audio.get(c.file) else {
                warnings.push(format!("clip #{} refers to a file that isn't listed", c.id));
                continue;
            };
            let mut p = ClipParams::new(c.id, c.start_beat.max(0.0), c.length_beats.max(0.0));
            p.offset_secs = c.offset_secs.max(0.0);
            p.gain_db = c.gain_db;
            p.fade_in_beats = c.fade_in_beats.max(0.0);
            p.fade_out_beats = c.fade_out_beats.max(0.0);
            info.clips.push(ClipInfo { data: data.clone(), params: p });
            max_id = max_id.max(c.id);
        }
        for c in &t.midi_clips {
            let notes = c.notes.iter().map(|n| Note { start: n.start, length: n.length, pitch: n.pitch.min(127), velocity: n.velocity.min(127) }).collect();
            info.midi_clips.push(MidiClipInfo { id: c.id, start_beat: c.start_beat.max(0.0), length_beats: c.length_beats, notes });
            max_id = max_id.max(c.id);
        }
        for l in &t.automation {
            let target = match &l.target {
                TargetFile::Volume => Some(AutoTarget::Volume),
                TargetFile::Pan => Some(AutoTarget::Pan),
                TargetFile::Effect { slot, param } => {
                    info.slots.get(*slot).and_then(|s| s.as_ref()).and_then(|n| n.param_index(param)).map(|param| AutoTarget::Node { slot: *slot, param })
                }
                TargetFile::Instrument { param } => info.instrument.as_ref().and_then(|i| i.param_index(param)).map(|param| AutoTarget::Instrument { param }),
            };
            match target {
                Some(target) => info.automation.push(AutoLaneInfo { target, points: l.points.clone() }),
                None => warnings.push(format!("automation lane on track {} skipped: its parameter no longer exists", t.index)),
            }
        }
        project.tracks[t.index] = Some(info);
    }
    // A sidechain to a track that didn't load would point at nothing.
    for i in 0..MAX_TRACKS {
        let missing = project.tracks[i].as_ref().and_then(|t| t.key).filter(|&k| project.tracks[k].is_none());
        if let Some(k) = missing {
            project.tracks[i].as_mut().unwrap().key = None;
            warnings.push(format!("track {i}'s sidechain source (track {k}) is missing"));
        }
    }
    strip_from(&file.aux, &mut project.aux, plugins, &mut warnings);
    strip_from(&file.master, &mut project.master, plugins, &mut warnings);
    Ok((project, max_id, warnings))
}

/// Finds a referenced file: where it was, else by name in the places a
/// moved project's files are likely to be.
pub fn locate(reference: &FileRef, project_dir: &Path, audio_dir: &Path) -> Option<PathBuf> {
    let given = PathBuf::from(&reference.path);
    let given = if given.is_absolute() { given } else { project_dir.join(given) };
    let file_name = given.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| reference.name.clone().into());
    let candidates = [given, project_dir.join(&file_name), audio_dir.join(&file_name), project_dir.join("recordings").join(&file_name)];
    candidates.into_iter().find(|p| p.is_file())
}

/// The folder next to `project.daw.json` for audio the project owns:
/// recordings that only existed in memory get written here.
pub fn audio_dir(project_path: &Path) -> PathBuf {
    let stem = project_path.file_name().map(|n| n.to_string_lossy().trim_end_matches(&format!(".{EXTENSION}")).to_string()).unwrap_or_default();
    project_path.with_file_name(format!("{stem} audio"))
}

// ---------------------------------------------------------------------
// The session's side
// ---------------------------------------------------------------------

impl Session {
    /// Saves to `path`. Audio that exists only in memory (a take whose file
    /// couldn't be written) is written into the project's audio folder.
    pub fn save_project(&mut self, path: &Path) -> Result {
        let project_dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf();
        self.write_memory_only_audio(path)?;

        // Every file in the library, so loaded-but-unused samples come back
        // too, plus any clip audio that isn't in it.
        let mut files: Vec<Arc<AudioData>> = self.samples().into_iter().filter(|f| f.path.is_some()).collect();
        for t in self.project.tracks.iter().flatten() {
            for c in &t.clips {
                if !files.iter().any(|f| Arc::ptr_eq(f, &c.data)) {
                    files.push(c.data.clone());
                }
            }
        }
        let file = to_file(&self.project, &files, &project_dir);
        let json = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;

        // Write a temporary file, then rename it over the real one. A rename
        // is atomic, so a crash mid-save can never leave a half-written
        // project in place of the old one.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("couldn't write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("couldn't save {}: {e}", path.display()))?;
        self.project_path = Some(path.to_path_buf());
        self.history.mark_saved();
        Ok(())
    }

    /// Clips whose audio has no file get one, so the project can find it.
    fn write_memory_only_audio(&mut self, path: &Path) -> Result {
        let dir = audio_dir(path);
        let mut replaced: Vec<(Arc<AudioData>, Arc<AudioData>)> = Vec::new();
        for t in self.project.tracks.iter().flatten() {
            for c in t.clips.iter().filter(|c| c.data.path.is_none() && c.data.frames() > 0) {
                if replaced.iter().any(|(old, _)| Arc::ptr_eq(old, &c.data)) {
                    continue;
                }
                std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
                let name = if c.data.name.to_ascii_lowercase().ends_with(".wav") { c.data.name.clone() } else { format!("{}.wav", c.data.name) };
                let file = dir.join(name);
                let mut w = WavWriter::create(&file, c.data.sample_rate as u32).map_err(|e| e.to_string())?;
                let (l, r) = (&c.data.channels[0], &c.data.channels[1.min(c.data.channels.len() - 1)]);
                w.write(l, r).map_err(|e| e.to_string())?;
                w.finish().map_err(|e| e.to_string())?;
                // AudioData is immutable and shared, so the version with a
                // path is a new one. The engine keeps playing the old one,
                // which sounds the same.
                let with_path = AudioData { name: c.data.name.clone(), path: Some(file), sample_rate: c.data.sample_rate, channels: c.data.channels.clone() };
                replaced.push((c.data.clone(), Arc::new(with_path)));
            }
        }
        for (old, new) in &replaced {
            for t in self.project.tracks.iter_mut().flatten() {
                for c in t.clips.iter_mut().filter(|c| Arc::ptr_eq(&c.data, old)) {
                    c.data = new.clone();
                }
            }
            let mut lib = self.library.lock().unwrap();
            for entry in lib.iter_mut().filter(|e| Arc::ptr_eq(e, old)) {
                *entry = new.clone();
            }
        }
        Ok(())
    }

    /// Opens a project, replacing the current one. Returns warnings about
    /// anything missing or unrecognized (an empty list is a clean load).
    pub fn open_project(&mut self, path: &Path) -> Result<Vec<String>> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("couldn't read {}: {e}", path.display()))?;
        let file: ProjectFile = serde_json::from_str(&text).map_err(|e| format!("{} isn't a valid project file: {e}", path.display()))?;
        let project_dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf();
        let adir = audio_dir(path);

        let mut warnings = Vec::new();
        let audio: Vec<Arc<AudioData>> = file
            .files
            .iter()
            .map(|r| {
                let found = locate(r, &project_dir, &adir).and_then(|p| match loader::load_audio(&p) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        warnings.push(format!("couldn't read {}: {e}", p.display()));
                        None
                    }
                });
                Arc::new(found.unwrap_or_else(|| {
                    warnings.push(format!("missing audio file: {} (its clips stay as silent placeholders)", r.path));
                    // No samples, but the path is kept, so saving writes it
                    // back and nothing is lost once the file is found.
                    AudioData { name: r.name.clone(), path: Some(PathBuf::from(&r.path)), sample_rate: self.sample_rate, channels: vec![Vec::new()] }
                }))
            })
            .collect();
        let mut load = |path: &Path, id: &str, state: Option<Vec<u8>>, w: &mut Vec<String>| self.load_plugin_node(path, id, state, w);
        let (project, max_id, more) = from_file(&file, &audio, &mut load)?;
        warnings.extend(more);

        self.stop()?;
        self.seek(0.0)?;
        self.replace_project(project)?;
        *self.library.lock().unwrap() = audio.into_iter().filter(|a| a.frames() > 0).collect();
        self.next_clip_id = self.next_clip_id.max(max_id + 1);
        self.project_path = Some(path.to_path_buf());
        Ok(warnings)
    }

    /// Starts over with an empty project.
    pub fn new_project(&mut self) -> Result {
        self.stop()?;
        self.seek(0.0)?;
        self.replace_project(Project::new())?;
        self.library.lock().unwrap().clear();
        self.project_path = None;
        Ok(())
    }

    fn replace_project(&mut self, project: Project) -> Result {
        let current = std::mem::replace(&mut self.project, project.clone());
        self.reconcile(&current, &project)?;
        self.history.reset();
        self.forget_missing_tracks();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::Target;
    use crate::session::tests::TestSession;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_wav(path: &Path, frames: usize, value: f32) {
        let mut w = WavWriter::create(path, 48_000).unwrap();
        let samples = vec![value; frames];
        w.write(&samples, &samples).unwrap();
        w.finish().unwrap();
    }

    /// A project with a bit of everything, using `drums.wav` in `dir`.
    fn busy(t: &mut TestSession, dir: &Path) {
        let wav = dir.join("drums.wav");
        write_wav(&wav, 48_000, 0.25);
        t.session.library.lock().unwrap().push(Arc::new(loader::load_audio(&wav).unwrap()));
        let s = &mut t.session;
        s.set_tempo(96.0).unwrap();
        s.set_loop(Some((0.0, 16.0))).unwrap();
        s.add_track(Some("Drums".into())).unwrap();
        s.add_track(Some("Keys".into())).unwrap();
        let clip = s.add_clip(0, 0, 4.0).unwrap();
        s.edit_clip(0, clip, |p| {
            p.gain_db = -3.0;
            p.fade_out_beats = 0.5;
        })
        .unwrap();
        s.set_synth(1, true).unwrap();
        s.set_instrument_param(1, 6, 800.0).unwrap();
        s.add_midi_clip(1, 8.0, 16.0, super::super::demo_notes()).unwrap();
        let slot = s.add_node(Target::Track(1), "comp", None).unwrap();
        s.set_param(Target::Track(1), slot, 0, -24.0).unwrap();
        s.set_sidechain(1, Some(0)).unwrap();
        s.set_pan(1, -0.4).unwrap();
        s.set_send(1, -9.0).unwrap();
        s.add_automation(1, AutoTarget::Node { slot, param: 0 }).unwrap();
        s.set_automation_points(1, 0, vec![(0.0, 0.2), (8.0, 0.9)]).unwrap();
        s.add_node(Target::Master, "eq", None).unwrap();
        s.set_volume(Target::Master, -2.0).unwrap();
    }

    /// The project as a file, for comparing two projects completely.
    fn as_file(s: &Session, dir: &Path) -> ProjectFile {
        let mut files: Vec<Arc<AudioData>> = Vec::new();
        for t in s.project.tracks.iter().flatten() {
            for c in &t.clips {
                if !files.iter().any(|f| Arc::ptr_eq(f, &c.data)) {
                    files.push(c.data.clone());
                }
            }
        }
        to_file(&s.project, &files, dir)
    }

    #[test]
    fn every_effect_kind_round_trips_by_name() {
        // Saving writes node.name(); loading looks it up with nodes::make().
        for (kind, _) in nodes::KINDS {
            assert_eq!(nodes::make(kind).unwrap().name(), *kind);
        }
    }

    #[test]
    fn save_and_open_round_trip() {
        let dir = temp_dir("daw_project_roundtrip");
        let path = dir.join("song.daw.json");
        let mut a = TestSession::new();
        busy(&mut a, &dir);
        a.session.save_project(&path).unwrap();
        assert!(!a.session.history.unsaved());

        let mut b = TestSession::new();
        let warnings = b.session.open_project(&path).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(as_file(&b.session, &dir), as_file(&a.session, &dir), "everything came back");
        assert_eq!(b.session.tempo(), 96.0);
        assert_eq!(b.session.samples().len(), 1, "the library too");
        assert!(b.session.history.undo_label().is_none(), "a fresh history");
        // And the engine heard about all of it.
        let sent = b.sent();
        for expected in ["AddTrack 0", "AddTrack 1", "SetClips 0", "SetSequence 1", "Insert 0", "SetSidechain 1 Some(0)"] {
            assert!(sent.iter().any(|s| s == expected), "{expected} missing");
        }
    }

    #[test]
    fn a_moved_project_folder_still_finds_its_audio() {
        let dir = temp_dir("daw_project_move_a");
        let mut a = TestSession::new();
        busy(&mut a, &dir);
        a.session.save_project(&dir.join("song.daw.json")).unwrap();
        let text = std::fs::read_to_string(dir.join("song.daw.json")).unwrap();
        assert!(text.contains("\"path\": \"drums.wav\""), "stored relative to the project");

        let moved = std::env::temp_dir().join("daw_project_move_b");
        let _ = std::fs::remove_dir_all(&moved);
        std::fs::rename(&dir, &moved).unwrap();
        let mut b = TestSession::new();
        let warnings = b.session.open_project(&moved.join("song.daw.json")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(b.session.project.tracks[0].as_ref().unwrap().clips[0].data.frames(), 48_000);
    }

    #[test]
    fn missing_audio_becomes_a_placeholder_that_saving_keeps() {
        let dir = temp_dir("daw_project_missing");
        let mut a = TestSession::new();
        busy(&mut a, &dir);
        a.session.save_project(&dir.join("song.daw.json")).unwrap();
        std::fs::remove_file(dir.join("drums.wav")).unwrap();

        let mut b = TestSession::new();
        let warnings = b.session.open_project(&dir.join("song.daw.json")).unwrap();
        assert!(warnings.iter().any(|w| w.contains("missing audio file: drums.wav")), "{warnings:?}");
        let clip = &b.session.project.tracks[0].as_ref().unwrap().clips[0];
        assert_eq!(clip.data.frames(), 0, "silent");
        assert_eq!(clip.params.start_beat, 4.0, "but still there");

        // Saving again keeps the reference; once the file is back, it loads.
        b.session.save_project(&dir.join("again.daw.json")).unwrap();
        write_wav(&dir.join("drums.wav"), 1_000, 0.5);
        let mut c = TestSession::new();
        assert!(c.session.open_project(&dir.join("again.daw.json")).unwrap().is_empty());
        assert_eq!(c.session.project.tracks[0].as_ref().unwrap().clips[0].data.frames(), 1_000);
    }

    #[test]
    fn a_relocated_file_is_found_by_name() {
        let dir = temp_dir("daw_project_relink");
        let mut a = TestSession::new();
        busy(&mut a, &dir);
        a.session.save_project(&dir.join("song.daw.json")).unwrap();
        // The user tidies up: audio into "song audio/".
        let adir = audio_dir(&dir.join("song.daw.json"));
        std::fs::create_dir_all(&adir).unwrap();
        std::fs::rename(dir.join("drums.wav"), adir.join("drums.wav")).unwrap();
        let mut b = TestSession::new();
        assert!(b.session.open_project(&dir.join("song.daw.json")).unwrap().is_empty());
        assert_eq!(b.session.project.tracks[0].as_ref().unwrap().clips[0].data.frames(), 48_000);
    }

    #[test]
    fn takes_only_in_memory_are_written_on_save() {
        let dir = temp_dir("daw_project_memory_take");
        let mut a = TestSession::new();
        a.session.add_track(None).unwrap();
        let take = Arc::new(AudioData { name: "take-007.wav".into(), path: None, sample_rate: 48_000.0, channels: vec![vec![0.5; 4_800]; 2] });
        a.session.library.lock().unwrap().push(take);
        a.session.add_clip(0, 0, 0.0).unwrap();
        a.session.save_project(&dir.join("song.daw.json")).unwrap();
        let written = dir.join("song audio").join("take-007.wav");
        assert!(written.is_file(), "written next to the project");

        let mut b = TestSession::new();
        assert!(b.session.open_project(&dir.join("song.daw.json")).unwrap().is_empty());
        let clip = &b.session.project.tracks[0].as_ref().unwrap().clips[0];
        assert_eq!(clip.data.channels[0], vec![0.5; 4_800]);
    }

    #[test]
    fn unknown_names_are_skipped_with_warnings_and_newer_files_refused() {
        let dir = temp_dir("daw_project_compat");
        let path = dir.join("song.daw.json");
        let mut a = TestSession::new();
        busy(&mut a, &dir);
        a.session.save_project(&path).unwrap();
        // A "future" file: a parameter and an effect this version lacks.
        let text = std::fs::read_to_string(&path).unwrap();
        let future = text.replace("\"threshold\"", "\"tube_drive\"").replacen("\"kind\": \"eq\"", "\"kind\": \"flanger\"", 1);
        std::fs::write(&path, &future).unwrap();
        let mut b = TestSession::new();
        let warnings = b.session.open_project(&path).unwrap();
        assert!(warnings.iter().any(|w| w.contains("tube_drive")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("flanger")), "{warnings:?}");
        assert!(b.session.project.tracks[1].is_some(), "the rest loads");

        std::fs::write(&path, text.replace("\"version\": 1", "\"version\": 99")).unwrap();
        assert!(TestSession::new().session.open_project(&path).unwrap_err().contains("newer version"));
        std::fs::write(&path, "{\"hello\": 1}").unwrap();
        assert!(TestSession::new().session.open_project(&path).is_err());
    }

    #[test]
    fn opening_replaces_the_old_project_and_new_clears_it() {
        let dir = temp_dir("daw_project_replace");
        let mut a = TestSession::new();
        busy(&mut a, &dir);
        a.session.save_project(&dir.join("song.daw.json")).unwrap();

        let mut b = TestSession::new();
        for _ in 0..4 {
            b.session.add_track(None).unwrap();
        }
        b.session.open_project(&dir.join("song.daw.json")).unwrap();
        assert!(b.session.project.tracks[2].is_none() && b.session.project.tracks[3].is_none(), "old tracks removed");
        let sent = b.sent();
        assert!(sent.iter().any(|s| s == "RemoveTrack 3"));

        b.session.new_project().unwrap();
        assert!(b.session.project.tracks.iter().all(|t| t.is_none()));
        assert!(b.session.samples().is_empty());
        assert_eq!(b.session.tempo(), 120.0);
    }
}
