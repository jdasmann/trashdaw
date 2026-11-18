//! Mixer strips: a meter, a fader and the strip's switches per channel.

use super::meter::{MeterState, FLOOR_DB};
use super::DawApp;
use crate::mixer::{MeterPoint, Target, MAX_TRACKS};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Rounding, Sense, Slider, Stroke, Ui, Vec2};
use std::collections::HashMap;

const STRIP_W: f32 = 84.0;
const METER_H: f32 = 150.0;

#[derive(Default)]
pub struct MeterBank {
    states: HashMap<Target, MeterState>,
}

impl MeterBank {
    pub fn state(&mut self, target: Target) -> &mut MeterState {
        self.states.entry(target).or_default()
    }
}

impl DawApp {
    pub(super) fn mixer_ui(&mut self, ui: &mut Ui) {
        // Read every meter once per frame. `take` returns the highest peak
        // since the last read, so nothing between frames is missed.
        let dt = ui.input(|i| i.unstable_dt).clamp(0.0, 0.25);
        let targets: Vec<Target> = (0..MAX_TRACKS)
            .filter(|&i| self.session.project.tracks[i].is_some())
            .map(Target::Track)
            .chain([Target::Aux, Target::Master])
            .collect();
        for &t in &targets {
            let peak = self.session.meters.take(t, MeterPoint::PostFader);
            self.meter_bank.state(t).update(peak, dt);
        }

        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                for &t in &targets {
                    ui.allocate_ui(Vec2::new(STRIP_W, ui.available_height()), |ui| {
                        ui.set_width(STRIP_W);
                        ui.vertical(|ui| self.strip(ui, t));
                    });
                    ui.separator();
                }
            });
        });
    }

    fn strip(&mut self, ui: &mut Ui, target: Target) {
        let Some(info) = self.session.project.get(target) else { return };
        let (name, volume, pan, mute, solo, send) = (info.name.clone(), info.volume_db, info.pan, info.mute, info.solo, info.send_db);
        let has_instrument = info.instrument.is_some();

        let selected = self.selected == target;
        let label = if name.len() > 10 { format!("{}…", &name[..9]) } else { name };
        if ui.selectable_label(selected, label).clicked() {
            self.selected = target;
        }

        // Meter beside a vertical fader.
        ui.horizontal(|ui| {
            let meter = *self.meter_bank.state(target);
            let (resp, painter) = ui.allocate_painter(Vec2::new(12.0, METER_H), Sense::click());
            draw_meter(&painter, resp.rect, &meter);
            if resp.clicked() {
                self.meter_bank.state(target).clipped = false; // click to clear
            }
            let mut db = volume;
            let fader = ui.add_sized(
                Vec2::new(24.0, METER_H),
                Slider::new(&mut db, -60.0..=12.0).vertical().show_value(false),
            );
            if fader.changed() {
                let r = self.session.set_volume(target, db);
                self.report(r);
            }
            if fader.double_clicked() {
                let r = self.session.set_volume(target, 0.0); // double-click: back to 0 dB
                self.report(r);
            }
        });
        let meter = *self.meter_bank.state(target);
        ui.label(egui::RichText::new(format!("{:+.1} dB", volume)).small());
        let peak_text = if meter.hold_db <= FLOOR_DB { "-inf".into() } else { format!("{:.1}", meter.hold_db) };
        ui.label(egui::RichText::new(format!("pk {peak_text}")).small().color(if meter.clipped { Color32::RED } else { Color32::GRAY }));

        let Target::Track(track) = target else { return };
        let mut p = pan;
        if ui.add(Slider::new(&mut p, -1.0..=1.0).show_value(false).text("pan")).changed() {
            let r = self.session.set_pan(track, p);
            self.report(r);
        }
        ui.horizontal(|ui| {
            if ui.selectable_label(mute, "M").on_hover_text("mute").clicked() {
                let r = self.session.set_mute(track, !mute);
                self.report(r);
            }
            if ui.selectable_label(solo, "S").on_hover_text("solo").clicked() {
                let r = self.session.set_solo(track, !solo);
                self.report(r);
            }
            if has_instrument {
                let armed = self.session.armed == Some(track);
                if ui.selectable_label(armed, "R").on_hover_text("arm for live MIDI input").clicked() {
                    let r = self.session.arm(if armed { None } else { Some(track) });
                    self.report(r);
                }
            }
        });
        let mut s = send;
        if ui.add(Slider::new(&mut s, -60.0..=6.0).show_value(false).text("aux")).on_hover_text("send to the aux bus").changed() {
            let r = self.session.set_send(track, s);
            self.report(r);
        }
    }
}

fn draw_meter(painter: &egui::Painter, rect: Rect, m: &MeterState) {
    painter.rect_filled(rect, Rounding::same(2.0), Color32::from_gray(20));
    let y = |db: f32| rect.bottom() - MeterState::fraction(db) * rect.height();
    let level = Rect::from_min_max(Pos2::new(rect.left() + 1.0, y(m.db)), Pos2::new(rect.right() - 1.0, rect.bottom()));
    let color = if m.db > 0.0 {
        Color32::from_rgb(220, 60, 50)
    } else if m.db > -6.0 {
        Color32::from_rgb(230, 190, 60)
    } else {
        Color32::from_rgb(80, 200, 100)
    };
    if m.db > FLOOR_DB {
        painter.rect_filled(level, Rounding::ZERO, color);
    }
    if m.hold_db > FLOOR_DB {
        let hy = y(m.hold_db);
        painter.line_segment([Pos2::new(rect.left(), hy), Pos2::new(rect.right(), hy)], Stroke::new(1.0_f32, Color32::WHITE));
    }
    // 0 dB mark, and the clip light at the top.
    let zero = y(0.0);
    painter.line_segment([Pos2::new(rect.left(), zero), Pos2::new(rect.right(), zero)], Stroke::new(1.0_f32, Color32::from_gray(90)));
    if m.clipped {
        painter.rect_filled(Rect::from_min_size(rect.left_top(), Vec2::new(rect.width(), 4.0)), Rounding::ZERO, Color32::RED);
    }
    let _ = (Align2::CENTER_CENTER, FontId::default());
}
