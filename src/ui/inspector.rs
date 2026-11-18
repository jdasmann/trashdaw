//! The inspector: the selected track's instrument and effect chain.

use super::{widgets, DawApp};
use crate::automation::AutoTarget;
use crate::mixer::Target;
use crate::nodes;
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
            let mut on = inst.is_some();
            if ui.checkbox(&mut on, "synth").changed() {
                let r = self.session.set_synth(track, on);
                self.report(r);
            }
        });
        let Some(inst) = inst else { return };
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
            egui::CollapsingHeader::new(format!("{slot}: {}", node.name)).id_source(("slot", target, slot)).default_open(true).show(ui, |ui| {
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
        egui::ComboBox::from_id_source(("add effect", target)).selected_text("+ add effect").show_ui(ui, |ui| {
            for (kind, label) in nodes::KINDS {
                if ui.selectable_label(false, *label).clicked() {
                    let r = self.session.add_node(target, kind, None).map(|_| ());
                    self.report(r);
                }
            }
        });
    }
}
