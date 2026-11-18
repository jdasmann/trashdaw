//! A generic parameter control, built from `ParamInfo` alone: the UI never
//! needs to know what a delay or a compressor is.

use crate::processor::{ParamInfo, ParamKind};
use eframe::egui::{self, Slider, Ui};

/// Names for whole-number parameters that are really choices.
fn choices(node: &str, param: &str) -> Option<&'static [&'static str]> {
    match (node, param) {
        ("filter", "type") => Some(&["lowpass", "highpass", "bandpass", "notch", "peak", "low shelf", "high shelf"]),
        ("synth" | "osc", "wave") => Some(&["sine", "saw", "square"]),
        _ => None,
    }
}

/// Draws one parameter. Returns the new value if the user changed it.
pub fn param(ui: &mut Ui, node: &str, p: &ParamInfo, value: f32, id: impl std::hash::Hash) -> Option<f32> {
    let unit = if p.unit.is_empty() { String::new() } else { format!(" {}", p.unit) };
    match p.kind {
        ParamKind::Toggle => {
            let mut on = value >= 0.5;
            ui.checkbox(&mut on, p.name).changed().then_some(if on { 1.0 } else { 0.0 })
        }
        ParamKind::Int => {
            let mut i = value.round() as i32;
            let changed = if let Some(names) = choices(node, p.name) {
                let mut changed = false;
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_source(id)
                        .selected_text(names.get(i as usize).copied().unwrap_or("?"))
                        .show_ui(ui, |ui| {
                            for (k, name) in names.iter().enumerate() {
                                changed |= ui.selectable_value(&mut i, k as i32, *name).changed();
                            }
                        });
                    ui.label(p.name);
                });
                changed
            } else {
                ui.add(Slider::new(&mut i, p.min as i32..=p.max as i32).text(p.name)).changed()
            };
            changed.then_some(i as f32)
        }
        ParamKind::Linear | ParamKind::Log => {
            let mut v = value;
            let slider = Slider::new(&mut v, p.min..=p.max).text(p.name).suffix(unit).logarithmic(p.kind == ParamKind::Log);
            ui.add(slider).changed().then_some(v)
        }
    }
}
