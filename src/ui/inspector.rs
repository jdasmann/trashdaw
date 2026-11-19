//! The inspector: the selected track's instrument and effect chain.

use super::{widgets, DawApp};
use crate::automation::AutoTarget;
use crate::mixer::Target;
use crate::nodes;
use std::sync::Arc;
use crate::session::demo_notes;
use crate::transport::BEATS_PER_BAR;
use eframe::egui::{self, Ui};

impl DawApp {
    pub(super) fn inspector_ui(&mut self, ui: &mut Ui) {
        let target = self.selected;
        let Some(info) = self.session.project.get(target) else {
            ui.label("Select a track.");
            return;
        };
        let title = match target {
            Target::Track(i) => format!("Track {i}"),
            Target::Aux => "Aux bus".into(),
            Target::Master => "Master".into(),
        };
        ui.heading(title);

        if let Target::Track(track) = target {
            let mut name = info.name.clone();
            if ui.text_edit_singleline(&mut name).changed() {
                let r = self.session.rename_track(track, name);
                self.report(r);
            }
            ui.separator();
            self.instrument_ui(ui, track);
        }

        ui.separator();
        ui.strong("Effects");
        self.chain_ui(ui, target);

        if let Target::Track(track) = target {
            ui.separator();
            self.automation_ui(ui, track);
            ui.separator();
            if ui.button("Remove track").clicked() {
                let r = self.session.remove_track(track);
                if self.report(r).is_some() {
                    self.selected = Target::Master;
                }
            }
        }
    }

    fn instrument_ui(&mut self, ui: &mut Ui, track: usize) {
        let inst = self.session.project.tracks[track].as_ref().and_then(|t| t.instrument.clone());
        ui.horizontal(|ui| {
            ui.strong("Instrument");
            let current = inst.as_ref().map_or("none", |i| i.name);
            let instruments: Vec<_> = self.session.plugins.iter().filter(|p| p.descriptor.is_instrument()).cloned().collect();
            egui::ComboBox::from_id_source(("instrument", track)).selected_text(current).show_ui(ui, |ui| {
                if ui.selectable_label(inst.is_none(), "none").clicked() {
                    let r = self.session.set_synth(track, false);
                    self.report(r);
                }
                if ui.selectable_label(current == "synth", "Synth (built in)").clicked() {
                    let r = self.session.set_synth(track, true);
                    self.report(r);
                }
                if !instruments.is_empty() {
                    ui.separator();
                }
                for p in instruments {
                    if ui.selectable_label(false, format!("{} (CLAP)", p.descriptor.name)).clicked() {
                        let r = self.session.set_plugin_instrument(track, &p.path, &p.descriptor.id);
                        self.report(r);
                    }
                }
            });
        });
        let Some(inst) = inst else { return };
        if let Some(p) = &inst.plugin {
            self.plugin_status(ui, p);
        }
        if ui.button("+ demo MIDI clip at playhead").clicked() {
            let bar = (self.session.current_beat() / BEATS_PER_BAR).floor() * BEATS_PER_BAR;
            let r = self.session.add_midi_clip(track, bar, 4.0 * BEATS_PER_BAR, demo_notes()).map(|_| ());
            self.report(r);
        }
        for (i, p) in inst.params.iter().enumerate() {
            if let Some(v) = widgets::param(ui, inst.name, p, inst.values[i], ("inst", track, i)) {
                let r = self.session.set_instrument_param(track, i, v);
                self.report(r);
            }
        }
    }

    /// Lists the track's automation lanes (one can be shown under the
    /// track on the timeline) and offers every parameter to automate.
    fn automation_ui(&mut self, ui: &mut Ui, track: usize) {
        ui.strong("Automation");
        let Some(t) = self.session.project.tracks[track].as_ref() else { return };
        let lanes: Vec<AutoTarget> = t.automation.iter().map(|l| l.target).collect();
        // Everything on this track that can be automated. Collected now, so
        // the borrow of the project ends before the widgets below need
        // `self` mutably.
        let mut options = vec![AutoTarget::Volume, AutoTarget::Pan];
        if let Some(inst) = &t.instrument {
            options.extend((0..inst.params.len()).map(|param| AutoTarget::Instrument { param }));
        }
        for (slot, node) in t.slots.iter().enumerate() {
            if let Some(node) = node {
                options.extend((0..node.params.len()).map(|param| AutoTarget::Node { slot, param }));
            }
        }
        let shown = self.timeline.shown_lane.get(&track).copied();
        for (i, target) in lanes.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.radio(shown == Some(i), self.lane_label(track, *target)).clicked() {
                    if shown == Some(i) {
                        self.timeline.shown_lane.remove(&track);
                    } else {
                        self.timeline.shown_lane.insert(track, i);
                    }
                }
                if ui.small_button("remove").clicked() {
                    let r = self.session.remove_automation(track, i);
                    if self.report(r).is_some() {
                        // Lanes after this one move up an index.
                        match shown {
                            Some(s) if s == i => {
                                self.timeline.shown_lane.remove(&track);
                            }
                            Some(s) if s > i => {
                                self.timeline.shown_lane.insert(track, s - 1);
                            }
                            _ => {}
                        }
                    }
                }
            });
        }

        egui::ComboBox::from_id_source(("automate", track)).selected_text("+ automate...").show_ui(ui, |ui| {
            for target in options.into_iter().filter(|o| !lanes.contains(o)) {
                if ui.selectable_label(false, self.lane_label(track, target)).clicked() {
                    let r = self.session.add_automation(track, target);
                    if let Some(i) = self.report(r) {
                        self.timeline.shown_lane.insert(track, i);
                    }
                }
            }
        });
    }

    fn chain_ui(&mut self, ui: &mut Ui, target: Target) {
        let slots = self.session.project.get(target).map(|t| t.slots.clone()).unwrap_or_default();
        for (slot, node) in slots.iter().enumerate() {
            let Some(node) = node else { continue };
            let title = if node.plugin.is_some() { format!("{slot}: {} (CLAP)", node.name) } else { format!("{slot}: {}", node.name) };
            egui::CollapsingHeader::new(title).id_source(("slot", target, slot)).default_open(true).show(ui, |ui| {
                if let Some(p) = &node.plugin {
                    self.plugin_status(ui, p);
                }
                for (i, p) in node.params.iter().enumerate() {
                    if let Some(v) = widgets::param(ui, node.name, p, node.values[i], ("param", target, slot, i)) {
                        let r = self.session.set_param(target, slot, i, v);
                        self.report(r);
                    }
                }
                if ui.small_button("remove").clicked() {
                    let r = self.session.remove_node(target, slot);
                    self.report(r);
                }
            });
        }
        let plugin_effects: Vec<_> = self.session.plugins.iter().filter(|p| !p.descriptor.is_instrument()).cloned().collect();
        egui::ComboBox::from_id_source(("add effect", target)).selected_text("+ add effect").show_ui(ui, |ui| {
            for (kind, label) in nodes::KINDS {
                if ui.selectable_label(false, *label).clicked() {
                    let r = self.session.add_node(target, kind, None).map(|_| ());
                    self.report(r);
                }
            }
            if !plugin_effects.is_empty() {
                ui.separator();
            }
            for p in plugin_effects {
                if ui.selectable_label(false, format!("{} (CLAP)", p.descriptor.name)).clicked() {
                    let r = self.session.add_plugin(target, &p.path, &p.descriptor.id, None).map(|_| ());
                    self.report(r);
                }
            }
        });
    }

    /// A plugin's health: missing, failed, or how much of the time budget
    /// it uses. That's all a host can know about someone else's code.
    fn plugin_status(&mut self, ui: &mut Ui, p: &crate::project::PluginInfo) {
        let Some(inst) = &p.instance else {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 60), format!("missing: {} (saving keeps it)", p.path.display()));
            return;
        };
        if inst.failed.load(std::sync::atomic::Ordering::Relaxed) {
            ui.colored_label(egui::Color32::from_rgb(230, 60, 50), "the plugin reported an error: bypassed");
            return;
        }
        // Peak since last frame, held and decayed so it's readable.
        let key = Arc::as_ptr(inst) as usize;
        let shown = self.plugin_cpu.entry(key).or_insert(0.0);
        *shown = inst.take_cpu().max(*shown * 0.97);
        let pct = *shown * 100.0;
        let color = if pct > 50.0 { egui::Color32::from_rgb(230, 120, 60) } else { egui::Color32::GRAY };
        ui.small(egui::RichText::new(format!("CPU {pct:.1}% of the block budget")).color(color));
    }
}
