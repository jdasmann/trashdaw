//! The session's side of plugins: finding them, adding them to tracks, and
//! building engine nodes for them (for edits, undo, and loading projects).

use super::{Result, Session};
use crate::clap::{self, Bundle, ClapNode, Descriptor, Instance};
use crate::engine::Command;
use crate::mixer::{Target, MAX_SLOTS};
use crate::nodes;
use crate::processor::{Instrument, Processor};
use crate::project::{NodeInfo, PluginInfo};
use crate::synth::Synth;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A plugin found on disk.
#[derive(Clone, Debug)]
pub struct PluginEntry {
    pub path: PathBuf,
    pub descriptor: Descriptor,
}

impl Session {
    /// Looks through the standard CLAP folders and loads every library
    /// found, to read what's inside. Returns problems found on the way.
    ///
    /// Loading a library runs its code, and a broken one can crash the
    /// whole DAW right here. That's why large hosts scan in a separate
    /// process; it's the honest cost of keeping this one simple.
    pub fn scan_plugins(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        for path in clap::find_plugin_files(&clap::search_paths()) {
            if let Err(e) = self.bundle(&path) {
                problems.push(e);
            }
        }
        problems
    }

    /// Makes a loaded library's plugins available (scanning calls this;
    /// tests use it to add compiled-in plugins).
    pub fn register_bundle(&mut self, bundle: Arc<Bundle>) {
        for d in &bundle.descriptors {
            if !self.plugins.iter().any(|p| p.path == bundle.path && p.descriptor.id == d.id) {
                self.plugins.push(PluginEntry { path: bundle.path.clone(), descriptor: d.clone() });
            }
        }
        self.bundles.insert(bundle.path.clone(), bundle);
    }

    /// The loaded library at `path`, loading it the first time. Each
    /// library is loaded once and shared by all its instances.
    pub(super) fn bundle(&mut self, path: &Path) -> Result<Arc<Bundle>> {
        if let Some(b) = self.bundles.get(path) {
            return Ok(b.clone());
        }
        let bundle = Bundle::load(path)?;
        self.register_bundle(bundle.clone());
        Ok(bundle)
    }

    fn instance(&mut self, path: &Path, id: &str) -> Result<Arc<Instance>> {
        let bundle = self.bundle(path)?;
        Instance::create(&bundle, id, &self.info)
    }

    /// The mirror's description of a plugin node, with the instance's
    /// current parameter values.
    fn plugin_node_info(inst: &Arc<Instance>, path: &Path, id: &str) -> NodeInfo {
        let mut info = NodeInfo::new(inst.name, inst.params);
        for (i, v) in info.values.iter_mut().enumerate() {
            if let Some(x) = inst.param_value(i) {
                *v = x;
            }
        }
        info.plugin = Some(PluginInfo { path: path.to_path_buf(), id: id.to_string(), instance: Some(inst.clone()), state: None });
        info
    }

    pub(super) fn add_plugin_inner(&mut self, target: Target, path: &Path, id: &str, slot: Option<usize>) -> Result<usize> {
        let slot = match slot {
            Some(s) if s < MAX_SLOTS => s,
            Some(_) => return Err(format!("slots are 0-{}", MAX_SLOTS - 1)),
            None => self.project.get(target).ok_or("no such target")?.first_free_slot().ok_or("no free slot")?,
        };
        let inst = self.instance(path, id)?;
        let info = Self::plugin_node_info(&inst, path, id);
        let node: Box<dyn Processor> = Box::new(ClapNode::new(inst, &self.info)?);
        self.send(Command::Insert { target, slot, node })?;
        let t = self.project.get_mut(target).ok_or("no such target")?;
        let replaced = t.slots[slot].replace(info).is_some();
        if replaced {
            self.drop_lanes(target, |t| matches!(t, crate::automation::AutoTarget::Node { slot: s, .. } if s == slot))?;
        }
        Ok(slot)
    }

    pub(super) fn set_plugin_instrument_inner(&mut self, track: usize, path: &Path, id: &str) -> Result {
        self.track_info(track)?;
        let inst = self.instance(path, id)?;
        if inst.inputs > 0 && !inst.descriptor.is_instrument() {
            return Err(format!("{} is an effect, not an instrument", inst.name));
        }
        let info = Self::plugin_node_info(&inst, path, id);
        let instrument: Box<dyn Instrument> = Box::new(ClapNode::new(inst, &self.info)?);
        self.send(Command::SetInstrument { track, instrument: Some(instrument) })?;
        self.track_info_mut(track)?.instrument = Some(info);
        self.drop_lanes(Target::Track(track), |t| matches!(t, crate::automation::AutoTarget::Instrument { .. }))
    }

    /// Answers plugins' requests (on_main_thread callbacks) and passes on
    /// what they logged. Called with `collect_garbage`, regularly.
    pub(super) fn service_plugins(&mut self) {
        let mut live: Vec<Arc<Instance>> = Vec::new();
        let mut add = |n: &Option<NodeInfo>| {
            if let Some(inst) = n.as_ref().and_then(|n| n.plugin.as_ref()).and_then(|p| p.instance.clone()) {
                live.push(inst);
            }
        };
        for t in self.project.tracks.iter().flatten().chain([&self.project.aux, &self.project.master]) {
            t.slots.iter().for_each(&mut add);
            add(&t.instrument);
        }
        let messages: Vec<String> = live.iter().flat_map(|i| i.service()).collect();
        if !messages.is_empty() {
            self.messages.lock().unwrap().extend(messages);
        }
    }

    /// An instance for a mirror node that's about to go into the engine.
    /// Normally the very instance the mirror holds (so undo restores it
    /// exactly). If the engine still has it, because the old node hasn't
    /// come back as garbage yet, a fresh instance gets the old one's state.
    fn engine_instance(&mut self, p: &PluginInfo) -> Result<Option<Arc<Instance>>> {
        match &p.instance {
            None => Ok(None), // a placeholder: nothing to play
            Some(inst) if !inst.in_engine() => Ok(Some(inst.clone())),
            Some(inst) => {
                let fresh = self.instance(&p.path, &p.id)?;
                if let Some(state) = inst.save_state() {
                    fresh.load_state(&state);
                }
                Ok(Some(fresh))
            }
        }
    }

    /// A new engine node for an effect described by the mirror, with its
    /// parameter values. None for a missing plugin. Also returns the
    /// instance actually used, if it differs from the mirror's.
    pub(super) fn node_from_info(&mut self, info: &NodeInfo) -> Result<(Option<Box<dyn Processor>>, Option<Arc<Instance>>)> {
        let Some(p) = &info.plugin else {
            let mut node = nodes::make(info.name).ok_or_else(|| format!("unknown effect kind: {}", info.name))?;
            node.prepare(&self.info);
            for (param, &value) in info.values.iter().enumerate() {
                node.set_param(param, value);
            }
            return Ok((Some(node), None));
        };
        let Some(inst) = self.engine_instance(p)? else { return Ok((None, None)) };
        let mut node = ClapNode::new(inst.clone(), &self.info)?;
        for (param, &value) in info.values.iter().enumerate() {
            Processor::set_param(&mut node, param, value);
        }
        let replaced = (!p.instance.as_ref().map_or(false, |i| Arc::ptr_eq(i, &inst))).then_some(inst);
        Ok((Some(Box::new(node)), replaced))
    }

    /// The instrument version of `node_from_info`.
    pub(super) fn instrument_from_info(&mut self, info: &NodeInfo) -> Result<(Option<Box<dyn Instrument>>, Option<Arc<Instance>>)> {
        let Some(p) = &info.plugin else {
            if info.name != "synth" {
                return Err(format!("unknown instrument: {}", info.name));
            }
            let mut synth: Box<dyn Instrument> = Box::new(Synth::new());
            synth.prepare(&self.info);
            for (param, &value) in info.values.iter().enumerate() {
                synth.set_param(param, value);
            }
            return Ok((Some(synth), None));
        };
        let Some(inst) = self.engine_instance(p)? else { return Ok((None, None)) };
        let mut node = ClapNode::new(inst.clone(), &self.info)?;
        for (param, &value) in info.values.iter().enumerate() {
            Instrument::set_param(&mut node, param, value);
        }
        let replaced = (!p.instance.as_ref().map_or(false, |i| Arc::ptr_eq(i, &inst))).then_some(inst);
        Ok((Some(Box::new(node)), replaced))
    }

    /// Loading a project: a plugin node from its library path and id, with
    /// its saved state. If the library moved, it's looked for by plugin id
    /// among the installed plugins (like audio files are found by name).
    /// If it can't be found, a placeholder keeps the saved state.
    pub(super) fn load_plugin_node(&mut self, path: &Path, id: &str, state: Option<Vec<u8>>, warnings: &mut Vec<String>) -> NodeInfo {
        let mut path = path.to_path_buf();
        if !path.exists() {
            if self.plugins.is_empty() {
                warnings.extend(self.scan_plugins());
            }
            if let Some(found) = self.plugins.iter().find(|p| p.descriptor.id == id) {
                path = found.path.clone();
            }
        }
        match self.instance(&path, id) {
            Ok(inst) => {
                if let Some(state) = &state {
                    if !inst.load_state(state) {
                        warnings.push(format!("{} couldn't restore its saved state", inst.name));
                    }
                }
                Self::plugin_node_info(&inst, &path, id)
            }
            Err(e) => {
                warnings.push(format!("plugin {id} is missing ({e}); kept as a silent placeholder"));
                let mut info = NodeInfo::new(clap::intern(&format!("{id} (missing)")), &[]);
                info.plugin = Some(PluginInfo { path, id: id.to_string(), instance: None, state: state.map(Arc::new) });
                info
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::AutoTarget;
    use crate::clap::test_plugin::{BEEP_ID, GAIN_ID};
    use crate::session::tests::TestSession;

    const PATH: &str = "example.clap";

    fn with_plugins() -> TestSession {
        let mut t = TestSession::new();
        t.session.register_bundle(crate::clap::tests::example_bundle());
        t
    }

    fn plugin_of(t: &TestSession, track: usize, slot: usize) -> Arc<Instance> {
        let node = t.session.project.tracks[track].as_ref().unwrap().slots[slot].as_ref().unwrap();
        node.plugin.as_ref().unwrap().instance.clone().unwrap()
    }

    /// Gives a plugin an internal state directly, as if it had processed.
    fn set_gain(inst: &Instance, db: f32) {
        let mut state = db.to_le_bytes().to_vec();
        state.push(0);
        assert!(inst.load_state(&state));
    }

    #[test]
    fn plugins_are_listed_and_added_like_any_effect() {
        let mut t = with_plugins();
        assert_eq!(t.session.plugins.len(), 3, "gain, beep and lookahead");
        t.session.add_track(None).unwrap();
        let slot = t.session.add_plugin(Target::Track(0), Path::new(PATH), GAIN_ID, None).unwrap();
        let node = t.session.project.tracks[0].as_ref().unwrap().slots[slot].clone().unwrap();
        assert_eq!(node.name, "Example Gain");
        assert_eq!(node.values, vec![0.0, 0.0]);
        assert_eq!(t.session.history.undo_label(), Some("add Example Gain"));
        let gain = t.session.param_index(Target::Track(0), slot, "Gain").unwrap();
        t.session.set_param(Target::Track(0), slot, gain, 30.0).unwrap();
        assert_eq!(t.session.project.tracks[0].as_ref().unwrap().slots[slot].as_ref().unwrap().values[0], 24.0, "clamped to the plugin's range");
        // Plugin parameters can be automated, like built-in ones.
        t.session.add_automation(0, AutoTarget::Node { slot, param: gain }).unwrap();
    }

    #[test]
    fn undoing_a_removal_brings_back_the_same_instance() {
        let mut t = with_plugins();
        t.session.add_track(None).unwrap();
        t.session.add_plugin(Target::Track(0), Path::new(PATH), GAIN_ID, None).unwrap();
        let original = plugin_of(&t, 0, 0);
        t.session.remove_node(Target::Track(0), 0).unwrap();
        t.sent(); // the engine processes the commands; the old node is freed
        t.session.undo().unwrap();
        assert!(Arc::ptr_eq(&original, &plugin_of(&t, 0, 0)), "not a lookalike: the same instance, all its state included");
    }

    #[test]
    fn a_fast_undo_still_keeps_the_state() {
        let mut t = with_plugins();
        t.session.add_track(None).unwrap();
        t.session.add_plugin(Target::Track(0), Path::new(PATH), GAIN_ID, None).unwrap();
        let original = plugin_of(&t, 0, 0);
        set_gain(&original, 6.0);
        t.session.remove_node(Target::Track(0), 0).unwrap();
        // Undo BEFORE the engine has handed the old node back: the instance
        // is still in use, so a fresh one must take over its state.
        t.session.undo().unwrap();
        let now = plugin_of(&t, 0, 0);
        assert!(!Arc::ptr_eq(&original, &now));
        assert_eq!(now.param_value(0), Some(6.0));
    }

    #[test]
    fn plugin_instruments_play_on_tracks() {
        let mut t = with_plugins();
        t.session.add_track(None).unwrap();
        t.session.set_plugin_instrument(0, Path::new(PATH), BEEP_ID).unwrap();
        let inst = t.session.project.tracks[0].as_ref().unwrap().instrument.clone().unwrap();
        assert_eq!((inst.name, inst.values.clone()), ("Example Beep", vec![0.5]));
        assert!(t.session.set_plugin_instrument(0, Path::new(PATH), GAIN_ID).is_err(), "an effect isn't an instrument");
        t.session.undo().unwrap();
        assert!(t.session.project.tracks[0].as_ref().unwrap().instrument.is_none());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn plugin_state_is_saved_and_restored() {
        let dir = temp_dir("daw_plugin_project");
        let path = dir.join("song.daw.json");
        let mut a = with_plugins();
        a.session.add_track(None).unwrap();
        a.session.add_plugin(Target::Track(0), Path::new(PATH), GAIN_ID, None).unwrap();
        a.session.add_automation(0, AutoTarget::Node { slot: 0, param: 0 }).unwrap();
        a.session.set_plugin_instrument(0, Path::new(PATH), BEEP_ID).unwrap();
        set_gain(&plugin_of(&a, 0, 0), -9.0);
        a.session.save_project(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"kind\": \"clap\"") && text.contains("\"state\""), "{text}");

        let mut b = with_plugins();
        let warnings = b.session.open_project(&path).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(plugin_of(&b, 0, 0).param_value(0), Some(-9.0), "the state came back");
        assert_eq!(b.session.project.tracks[0].as_ref().unwrap().slots[0].as_ref().unwrap().values[0], -9.0, "and the mirror shows it");
        assert_eq!(b.session.project.tracks[0].as_ref().unwrap().automation.len(), 1, "automation on a plugin parameter, by name");
        assert!(b.session.project.tracks[0].as_ref().unwrap().instrument.is_some());
    }

    #[test]
    fn a_missing_plugin_is_kept_as_a_placeholder() {
        let dir = temp_dir("daw_plugin_missing");
        let mut a = with_plugins();
        a.session.add_track(None).unwrap();
        a.session.add_plugin(Target::Track(0), Path::new(PATH), GAIN_ID, None).unwrap();
        set_gain(&plugin_of(&a, 0, 0), 3.0);
        a.session.save_project(&dir.join("song.daw.json")).unwrap();

        // A machine without the plugin.
        let mut b = TestSession::new();
        let warnings = b.session.open_project(&dir.join("song.daw.json")).unwrap();
        assert!(warnings.iter().any(|w| w.contains("org.example.gain is missing")), "{warnings:?}");
        let node = b.session.project.tracks[0].as_ref().unwrap().slots[0].clone().unwrap();
        assert!(node.plugin.as_ref().unwrap().instance.is_none(), "a silent placeholder");
        assert!(!b.sent().iter().any(|c| c.starts_with("Insert")), "nothing goes into the engine");

        // Saving there keeps the plugin and its state...
        b.session.save_project(&dir.join("again.daw.json")).unwrap();
        // ...so back on a machine that has it, nothing was lost.
        let mut c = with_plugins();
        assert!(c.session.open_project(&dir.join("again.daw.json")).unwrap().is_empty());
        assert_eq!(plugin_of(&c, 0, 0).param_value(0), Some(3.0));
    }
}

#[cfg(test)]
mod real_library {
    use super::*;
    use crate::clap::test_plugin::GAIN_ID;
    use crate::session::tests::TestSession;

    /// With the example built and its folder in CLAP_PATH:
    ///   CLAP_PATH=<folder with Example.clap> cargo test real_library_scan -- --ignored
    #[test]
    #[ignore]
    fn real_library_scan_add_save_and_reopen() {
        let mut t = TestSession::new();
        let problems = t.session.scan_plugins();
        assert!(problems.is_empty(), "{problems:?}");
        let gain = t.session.plugins.iter().find(|p| p.descriptor.id == GAIN_ID).cloned().expect("Example Gain found by scanning");
        t.session.add_track(None).unwrap();
        t.session.add_plugin(Target::Track(0), &gain.path, GAIN_ID, None).unwrap();
        let dir = std::env::temp_dir().join("daw_real_plugin_project");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        t.session.save_project(&dir.join("song.daw.json")).unwrap();

        let mut b = TestSession::new(); // fresh: loads the library from disk
        let warnings = b.session.open_project(&dir.join("song.daw.json")).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let node = b.session.project.tracks[0].as_ref().unwrap().slots[0].clone().unwrap();
        assert_eq!(node.name, "Example Gain");
        assert!(node.plugin.unwrap().instance.is_some());
    }
}
