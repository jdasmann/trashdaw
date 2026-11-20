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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Instant, SystemTime};

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
    /// Set in an autosaved recovery copy: the real project it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_from: Option<String>,
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

pub fn to_file(project: &Project, files: &[Arc<AudioData>], paths: &[Option<PathBuf>], project_dir: &Path) -> ProjectFile {
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
            .zip(paths)
            .map(|(f, p)| FileRef { path: p.as_deref().map_or_else(String::new, |p| portable(p, project_dir)), name: f.name.clone() })
            .collect(),
        tracks,
        aux: strip_file(&project.aux),
        master: strip_file(&project.master),
        recovered_from: None,
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
fn check_format(file: &ProjectFile) -> std::result::Result<(), String> {
    if file.format != FORMAT {
        return Err("this isn't a daw project file".into());
    }
    if file.version > VERSION {
        return Err(format!("this project was saved by a newer version (format {}; this version reads up to {VERSION})", file.version));
    }
    Ok(())
}

pub fn from_file(file: &ProjectFile, audio: &[Arc<AudioData>], plugins: &mut PluginLoader) -> std::result::Result<(Project, u32, Vec<String>), String> {
    check_format(file)?;
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

/// A project's recovery copy: next to it, or for an untitled project, in
/// the recovery folder.
pub fn recovery_path_for(project: Option<&Path>, recovery_dir: &Path) -> PathBuf {
    match project {
        Some(p) => {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let stem = name.strip_suffix(&format!(".{EXTENSION}")).unwrap_or(&name);
            p.with_file_name(format!("{stem}.recovery.{EXTENSION}"))
        }
        None => recovery_dir.join(format!("untitled.recovery.{EXTENSION}")),
    }
}

/// The OS's place for an application's own data, for untitled projects'
/// recovery copies.
pub fn default_recovery_dir() -> PathBuf {
    let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Application Support"))
    } else if cfg!(windows) {
        var("APPDATA")
    } else {
        var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("daw").join("recovery")
}

/// What a worker thread found for one file.
enum Decoded {
    Loaded(AudioData),
    Failed(String),
    Missing,
}

/// A project being opened in the background.
pub struct LoadJob {
    path: PathBuf,
    file: ProjectFile,
    audio: Vec<Option<Decoded>>,
    received: usize,
    rx: mpsc::Receiver<(usize, Decoded)>,
    cancel: Arc<AtomicBool>,
}

// ---------------------------------------------------------------------
// The session's side
// ---------------------------------------------------------------------

impl Session {
    /// Saves to `path`. Audio that exists only in memory (a take whose file
    /// couldn't be written) is written into the project's audio folder.
    pub fn save_project(&mut self, path: &Path) -> Result {
        self.save_project_with(path, false).map(|_| ())
    }

    /// Saves; with `collect`, first copies every audio file the project uses
    /// that lives outside its folder into its audio folder, so the project
    /// is self-contained (to send to a collaborator, say). Returns warnings.
    pub fn save_project_with(&mut self, path: &Path, collect: bool) -> Result<Vec<String>> {
        if self.loading.is_some() {
            return Err("a project is still loading".into());
        }
        self.write_memory_only_audio(path)?;
        let warnings = if collect { self.collect_audio(path)? } else { Vec::new() };
        self.write_project_file(path, None)?;
        self.project_path = Some(path.to_path_buf());
        self.history.mark_saved();
        // The real file now has everything: an older recovery copy is moot.
        let _ = std::fs::remove_file(recovery_path_for(Some(path), &self.recovery_dir));
        Ok(warnings)
    }

    /// Where a file's audio is, as of now: relocated by "collect", or where
    /// it was loaded from.
    fn effective_path(&self, data: &Arc<AudioData>) -> Option<PathBuf> {
        self.relocated.iter().find(|(d, _)| Arc::ptr_eq(d, data)).map(|(_, p)| p.clone()).or_else(|| data.path.clone())
    }

    /// Every audio file the project uses: the library (so loaded-but-unused
    /// samples come back too) plus any clip audio that isn't in it.
    fn used_audio(&self) -> Vec<Arc<AudioData>> {
        let mut files: Vec<Arc<AudioData>> = self.samples().into_iter().filter(|f| self.effective_path(f).is_some()).collect();
        for t in self.project.tracks.iter().flatten() {
            for c in &t.clips {
                if !files.iter().any(|f| Arc::ptr_eq(f, &c.data)) {
                    files.push(c.data.clone());
                }
            }
        }
        files
    }

    /// Writes the project file itself (no audio), atomically.
    fn write_project_file(&self, path: &Path, recovered_from: Option<&Path>) -> Result {
        let project_dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf();
        let files = self.used_audio();
        let paths: Vec<Option<PathBuf>> = files.iter().map(|f| self.effective_path(f)).collect();
        let mut file = to_file(&self.project, &files, &paths, &project_dir);
        file.recovered_from = recovered_from.map(|p| p.to_string_lossy().into_owned());
        let json = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&project_dir).map_err(|e| format!("couldn't create {}: {e}", project_dir.display()))?;
        // Write a temporary file, then rename it over the real one. A rename
        // is atomic, so a crash mid-save can never leave a half-written
        // project in place of the old one.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("couldn't write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("couldn't save {}: {e}", path.display()))
    }

    /// Step 11 exercise 2: copies audio from outside the project's folder
    /// into its audio folder. Two different files with the same name get
    /// distinct names; one file used many times is copied once.
    fn collect_audio(&mut self, path: &Path) -> Result<Vec<String>> {
        let canon = |p: &Path| std::fs::canonicalize(p).ok();
        let project_dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let inside = canon(project_dir).unwrap_or_else(|| project_dir.to_path_buf());
        let dir = audio_dir(path);
        let mut warnings = Vec::new();
        let mut copied: Vec<(PathBuf, PathBuf)> = Vec::new(); // (source, destination)
        for data in self.used_audio() {
            let Some(source) = self.effective_path(&data) else { continue };
            let Some(source) = canon(&source) else {
                warnings.push(format!("can't collect {}: the file is missing", source.display()));
                continue;
            };
            if source.starts_with(&inside) {
                continue; // already part of the project
            }
            let dest = match copied.iter().find(|(s, _)| *s == source) {
                Some((_, d)) => d.clone(),
                None => {
                    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
                    let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "audio".into());
                    let ext = source.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
                    let mut dest = dir.join(format!("{stem}{ext}"));
                    let mut k = 2;
                    // Taken by another file (this time, or by an earlier
                    // collect)? Then "drums (2).wav", "drums (3).wav"...
                    while copied.iter().any(|(_, d)| *d == dest) || (dest.exists() && canon(&dest) != Some(source.clone())) {
                        dest = dir.join(format!("{stem} ({k}){ext}"));
                        k += 1;
                    }
                    std::fs::copy(&source, &dest).map_err(|e| format!("couldn't copy {}: {e}", source.display()))?;
                    copied.push((source, dest.clone()));
                    dest
                }
            };
            // Record the new home. (Making a new AudioData just to change its
            // path would mean copying every sample in memory.)
            self.relocated.retain(|(d, _)| !Arc::ptr_eq(d, &data));
            self.relocated.push((data, dest));
        }
        Ok(warnings)
    }

    // -----------------------------------------------------------------
    // Step 11 exercise 3: autosave and crash recovery
    // -----------------------------------------------------------------

    /// Where this project's recovery copy goes: next to the project, or for
    /// an untitled one, in the recovery folder.
    pub fn recovery_path(&self) -> PathBuf {
        recovery_path_for(self.project_path.as_deref(), &self.recovery_dir)
    }

    /// If there are unsaved changes and it's been long enough, writes a
    /// recovery copy. Call regularly. NEVER writes the real project file:
    /// that holds the version the user chose to keep, and autosave captures
    /// whatever state they're in, mistakes included.
    pub fn autosave_tick(&mut self, now: Instant) -> Option<Result<PathBuf>> {
        if self.loading.is_some() || !self.history.unsaved() || now.duration_since(self.last_autosave) < self.autosave_every {
            return None;
        }
        self.last_autosave = now;
        let path = self.recovery_path();
        let real = self.project_path.clone();
        Some(self.write_project_file(&path, real.as_deref()).map(|_| path))
    }

    /// A recovery copy newer than the project itself, if there is one:
    /// work that was never saved (most likely because of a crash).
    pub fn pending_recovery(&self) -> Option<(PathBuf, SystemTime)> {
        let path = self.recovery_path();
        let saved_at = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        if let Some(project) = &self.project_path {
            if let Ok(project_time) = std::fs::metadata(project).and_then(|m| m.modified()) {
                if project_time > saved_at {
                    return None; // saved properly since: stale
                }
            }
        }
        Some((path, saved_at))
    }

    /// Opens the recovery copy. It belongs to the original project (if it
    /// had one) and counts as unsaved, so saving writes the real file.
    pub fn restore_recovery(&mut self) -> Result<Vec<String>> {
        let (path, _) = self.pending_recovery().ok_or("there's nothing to recover")?;
        self.open_project(&path)
    }

    /// Forgets the recovery copy (the user chose not to keep the changes).
    pub fn discard_recovery(&self) {
        let _ = std::fs::remove_file(self.recovery_path());
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

    /// Opens a project, replacing the current one, and waits until it's
    /// loaded. Returns warnings about anything missing or unrecognized (an
    /// empty list is a clean load). The UI uses `begin_open` instead.
    pub fn open_project(&mut self, path: &Path) -> Result<Vec<String>> {
        self.begin_open(path)?;
        loop {
            if let Some(result) = self.poll_open() {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Step 11 exercise 1: starts opening a project in the background. The
    /// file is read and checked here (that's quick); the audio is decoded
    /// on worker threads. Until `poll_open` finishes it, edits are refused:
    /// the loaded project is about to replace the current one, and anything
    /// changed in between would be silently lost.
    pub fn begin_open(&mut self, path: &Path) -> Result {
        if self.loading.is_some() {
            return Err("a project is already loading".into());
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("couldn't read {}: {e}", path.display()))?;
        let file: ProjectFile = serde_json::from_str(&text).map_err(|e| format!("{} isn't a valid project file: {e}", path.display()))?;
        check_format(&file)?;
        let project_dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf();
        let adir = audio_dir(path);

        // A shared queue of files; one worker per core, at most one per file.
        let queue: Arc<Mutex<Vec<(usize, FileRef)>>> = Arc::new(Mutex::new(
            file.files.iter().enumerate().map(|(i, r)| (i, FileRef { path: r.path.clone(), name: r.name.clone() })).rev().collect(),
        ));
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).min(file.files.len().max(1));
        for _ in 0..workers {
            let (queue, tx, cancel, project_dir, adir) = (queue.clone(), tx.clone(), cancel.clone(), project_dir.clone(), adir.clone());
            std::thread::spawn(move || loop {
                let Some((index, reference)) = queue.lock().unwrap().pop() else { return };
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let outcome = match locate(&reference, &project_dir, &adir) {
                    None => Decoded::Missing,
                    Some(p) => match loader::load_audio(&p) {
                        Ok(a) => Decoded::Loaded(a),
                        Err(e) => Decoded::Failed(format!("couldn't read {}: {e}", p.display())),
                    },
                };
                if tx.send((index, outcome)).is_err() {
                    return; // cancelled: nobody is listening
                }
            });
        }
        let total = file.files.len();
        self.loading = Some(LoadJob { path: path.to_path_buf(), file, audio: (0..total).map(|_| None).collect(), received: 0, rx, cancel });
        Ok(())
    }

    /// (files decoded, total), while a project is loading.
    pub fn load_progress(&self) -> Option<(usize, usize)> {
        self.loading.as_ref().map(|j| (j.received, j.audio.len()))
    }

    pub fn loading_path(&self) -> Option<&Path> {
        self.loading.as_ref().map(|j| j.path.as_path())
    }

    /// Stops a load in progress; the current project stays as it was.
    pub fn cancel_open(&mut self) {
        if let Some(job) = self.loading.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Collects decoded files. When the last one arrives, builds the project
    /// (plugins are created here: on the main thread, as CLAP requires) and
    /// swaps it in. Returns the outcome then, None while still loading.
    pub fn poll_open(&mut self) -> Option<Result<Vec<String>>> {
        let job = self.loading.as_mut()?;
        while let Ok((index, outcome)) = job.rx.try_recv() {
            if job.audio[index].is_none() {
                job.received += 1;
            }
            job.audio[index] = Some(outcome);
        }
        if job.received < job.audio.len() {
            return None;
        }
        let job = self.loading.take().unwrap();
        Some(self.finish_open(job))
    }

    fn finish_open(&mut self, job: LoadJob) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        let audio: Vec<Arc<AudioData>> = job
            .file
            .files
            .iter()
            .zip(job.audio)
            .map(|(r, outcome)| {
                let loaded = match outcome {
                    Some(Decoded::Loaded(a)) => Some(a),
                    Some(Decoded::Failed(e)) => {
                        warnings.push(e);
                        None
                    }
                    _ => None,
                };
                Arc::new(loaded.unwrap_or_else(|| {
                    warnings.push(format!("missing audio file: {} (its clips stay as silent placeholders)", r.path));
                    // No samples, but the path is kept, so saving writes it
                    // back and nothing is lost once the file is found.
                    AudioData { name: r.name.clone(), path: Some(PathBuf::from(&r.path)), sample_rate: self.sample_rate, channels: vec![Vec::new()] }
                }))
            })
            .collect();
        let mut load = |path: &Path, id: &str, state: Option<Vec<u8>>, w: &mut Vec<String>| self.load_plugin_node(path, id, state, w);
        let (project, max_id, more) = from_file(&job.file, &audio, &mut load)?;
        warnings.extend(more);

        self.stop()?;
        self.seek(0.0)?;
        self.replace_project(project)?;
        *self.library.lock().unwrap() = audio.into_iter().filter(|a| a.frames() > 0).collect();
        self.relocated.clear();
        self.next_clip_id = self.next_clip_id.max(max_id + 1);
        match &job.file.recovered_from {
            // A recovery copy: it belongs to the real project, and its
            // changes aren't in that file yet.
            Some(real) => {
                self.project_path = Some(PathBuf::from(real));
                self.history.mark_unsaved();
            }
            None if job.path.to_string_lossy().ends_with(&format!(".recovery.{EXTENSION}")) => {
                self.project_path = None; // an untitled project's recovery copy
                self.history.mark_unsaved();
            }
            None => self.project_path = Some(job.path),
        }
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
        let paths: Vec<Option<PathBuf>> = files.iter().map(|f| f.path.clone()).collect();
        to_file(&s.project, &files, &paths, dir)
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

    // ---- Step 11 exercise 1: background loading ----

    #[test]
    fn loading_happens_in_the_background_and_edits_wait() {
        let dir = temp_dir("daw_bg_load");
        let mut a = TestSession::new();
        a.session.add_track(None).unwrap();
        for i in 0..6 {
            let wav = dir.join(format!("part{i}.wav"));
            write_wav(&wav, 96_000, 0.1 * (i + 1) as f32);
            a.session.library.lock().unwrap().push(Arc::new(loader::load_audio(&wav).unwrap()));
            a.session.add_clip(0, i, i as f64 * 4.0).unwrap();
        }
        a.session.save_project(&dir.join("song.daw.json")).unwrap();

        let mut b = TestSession::new();
        b.session.add_track(Some("before".into())).unwrap();
        b.session.begin_open(&dir.join("song.daw.json")).unwrap();
        assert_eq!(b.session.load_progress().map(|(_, total)| total), Some(6));
        // While it loads, every edit is refused (one check, in `undoable`)...
        let err = b.session.add_track(None).unwrap_err();
        assert!(err.contains("loading"), "{err}");
        assert!(b.session.set_volume(Target::Track(0), -3.0).is_err());
        assert!(b.session.save_project(&dir.join("x.daw.json")).is_err());
        // ...and the current project is untouched until the load finishes.
        assert_eq!(b.session.project.tracks[0].as_ref().unwrap().name, "before");

        let result = loop {
            if let Some(r) = b.session.poll_open() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert!(result.unwrap().is_empty());
        let clips = &b.session.project.tracks[0].as_ref().unwrap().clips;
        assert_eq!(clips.len(), 6);
        assert!(clips.iter().all(|c| c.data.frames() == 96_000), "every file decoded");
        assert!(b.session.add_track(None).is_ok(), "editable again");
    }

    #[test]
    fn a_load_can_be_cancelled() {
        let dir = temp_dir("daw_bg_cancel");
        let mut a = TestSession::new();
        busy(&mut a, &dir);
        a.session.save_project(&dir.join("song.daw.json")).unwrap();
        let mut b = TestSession::new();
        b.session.add_track(Some("mine".into())).unwrap();
        b.session.begin_open(&dir.join("song.daw.json")).unwrap();
        b.session.cancel_open();
        assert!(b.session.poll_open().is_none() && b.session.load_progress().is_none());
        assert_eq!(b.session.project.tracks[0].as_ref().unwrap().name, "mine", "nothing replaced");
        assert!(b.session.add_track(None).is_ok());
        assert!(b.session.open_project(&dir.join("song.daw.json")).is_ok(), "and can load again");
    }

    // ---- Step 11 exercise 2: collect and save ----

    #[test]
    fn collect_makes_the_project_self_contained() {
        let outside = temp_dir("daw_collect_outside");
        std::fs::create_dir_all(outside.join("kit1")).unwrap();
        std::fs::create_dir_all(outside.join("kit2")).unwrap();
        let project = temp_dir("daw_collect_project");
        // Two DIFFERENT files with the same name, and one already inside.
        write_wav(&outside.join("kit1/drums.wav"), 1_000, 0.1);
        write_wav(&outside.join("kit2/drums.wav"), 2_000, 0.2);
        write_wav(&project.join("local.wav"), 3_000, 0.3);
        let mut a = TestSession::new();
        a.session.add_track(None).unwrap();
        for (i, f) in [outside.join("kit1/drums.wav"), outside.join("kit2/drums.wav"), project.join("local.wav")].iter().enumerate() {
            a.session.library.lock().unwrap().push(Arc::new(loader::load_audio(f).unwrap()));
            a.session.add_clip(0, i, i as f64 * 4.0).unwrap();
        }
        a.session.add_clip(0, 0, 12.0).unwrap(); // the same file used twice
        let warnings = a.session.save_project_with(&project.join("song.daw.json"), true).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let adir = project.join("song audio");
        let mut copied: Vec<String> = std::fs::read_dir(&adir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        copied.sort();
        assert_eq!(copied, vec!["drums (2).wav", "drums.wav"], "both kept apart; the local file isn't copied; nothing copied twice");

        // The originals can now disappear: the project has its own copies.
        std::fs::remove_dir_all(&outside).unwrap();
        let mut b = TestSession::new();
        assert!(b.session.open_project(&project.join("song.daw.json")).unwrap().is_empty());
        let frames: Vec<usize> = b.session.project.tracks[0].as_ref().unwrap().clips.iter().map(|c| c.data.frames()).collect();
        assert_eq!(frames, vec![1_000, 2_000, 3_000, 1_000], "each clip still has its own audio");
        // Collecting again copies nothing new.
        b.session.save_project_with(&project.join("song.daw.json"), true).unwrap();
        assert_eq!(std::fs::read_dir(&adir).unwrap().count(), 2);
    }

    // ---- Step 11 exercise 3: autosave and recovery ----

    #[test]
    fn autosave_writes_a_recovery_copy_never_the_project() {
        let dir = temp_dir("daw_autosave");
        let path = dir.join("song.daw.json");
        let mut s = TestSession::new();
        busy(&mut s, &dir);
        s.session.save_project(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        s.session.autosave_every = std::time::Duration::ZERO;
        assert!(s.session.autosave_tick(Instant::now()).is_none(), "nothing unsaved: nothing to do");

        s.session.add_track(Some("experiment".into())).unwrap();
        let written = s.session.autosave_tick(Instant::now()).unwrap().unwrap();
        assert_eq!(written, dir.join("song.recovery.daw.json"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved, "the real project is untouched");

        // "Crash", and start again: the recovery copy is offered and restored.
        let mut after = TestSession::new();
        after.session.open_project(&path).unwrap();
        assert!(after.session.pending_recovery().is_some());
        after.session.restore_recovery().unwrap();
        assert!(after.session.project.tracks.iter().flatten().any(|t| t.name == "experiment"));
        assert_eq!(after.session.project_path.as_deref(), Some(path.as_path()), "it belongs to the real project");
        assert!(after.session.history.unsaved(), "and isn't in that file yet");

        // Saving for real makes the recovery copy obsolete.
        after.session.save_project(&path).unwrap();
        assert!(after.session.pending_recovery().is_none());
        assert!(!dir.join("song.recovery.daw.json").exists());
    }

    #[test]
    fn untitled_work_is_recovered_too() {
        let mut s = TestSession::new();
        s.session.autosave_every = std::time::Duration::ZERO;
        s.session.add_track(Some("never saved".into())).unwrap();
        let written = s.session.autosave_tick(Instant::now()).unwrap().unwrap();
        assert!(written.starts_with(&s.session.recovery_dir), "an untitled project's copy goes in the recovery folder");

        let mut after = TestSession::new();
        after.session.recovery_dir = s.session.recovery_dir.clone();
        assert!(after.session.pending_recovery().is_some());
        after.session.restore_recovery().unwrap();
        assert_eq!(after.session.project.tracks[0].as_ref().unwrap().name, "never saved");
        assert_eq!(after.session.project_path, None, "still untitled");
        assert!(after.session.history.unsaved());
        // Choosing not to keep it removes it for good.
        after.session.discard_recovery();
        assert!(after.session.pending_recovery().is_none());
    }
}
