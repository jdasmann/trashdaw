//! The menu bar, keyboard shortcuts, the window title, and the dialogs for
//! opening, saving, and not losing unsaved work.

use super::DawApp;
use crate::session::PROJECT_EXTENSION;
use eframe::egui::{self, Key, Modifiers, ViewportCommand};
use std::path::PathBuf;

/// What to do once unsaved changes have been dealt with.
#[derive(Clone, Debug, PartialEq)]
pub enum Then {
    Nothing,
    New,
    Open(PathBuf),
    Quit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Dialog {
    /// Ask for a path to open.
    Open { path: String },
    /// Ask for a path to save to, then carry on with `then`. `collect`
    /// copies all the project's audio into its folder first.
    SaveAs { path: String, then: Then, collect: bool },
    /// "Save changes before...?"
    Unsaved { then: Then },
    /// "Recover unsaved work from <time>?"
    Recover { saved_at: std::time::SystemTime },
}

impl DawApp {
    pub(super) fn menu_ui(&mut self, ui: &mut egui::Ui) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New").clicked() {
                    self.after_unsaved_check(Then::New);
                    ui.close_menu();
                }
                if ui.button("Open...").clicked() {
                    self.dialog = Some(Dialog::Open { path: String::new() });
                    ui.close_menu();
                }
                if ui.add(egui::Button::new("Save").shortcut_text(ui.ctx().format_shortcut(&SAVE))).clicked() {
                    self.save(Then::Nothing);
                    ui.close_menu();
                }
                if ui.button("Save As...").clicked() {
                    self.dialog = Some(Dialog::SaveAs { path: self.suggested_path(), then: Then::Nothing, collect: false });
                    ui.close_menu();
                }
            });
            ui.menu_button("Edit", |ui| {
                let undo = self.session.history.undo_label().map(|l| format!("Undo {l}"));
                let redo = self.session.history.redo_label().map(|l| format!("Redo {l}"));
                let undo_button = egui::Button::new(undo.as_deref().unwrap_or("Undo")).shortcut_text(ui.ctx().format_shortcut(&UNDO));
                if ui.add_enabled(undo.is_some(), undo_button).clicked() {
                    self.undo();
                    ui.close_menu();
                }
                let redo_button = egui::Button::new(redo.as_deref().unwrap_or("Redo")).shortcut_text(ui.ctx().format_shortcut(&REDO));
                if ui.add_enabled(redo.is_some(), redo_button).clicked() {
                    self.redo();
                    ui.close_menu();
                }
                ui.separator();
                ui.menu_button("History", |ui| {
                    let (undo, redo) = self.session.history.labels();
                    if undo.is_empty() && redo.is_empty() {
                        ui.weak("Nothing yet.");
                    }
                    for label in undo.iter().rev().take(20) {
                        ui.label(*label);
                    }
                    for label in redo.iter().take(20) {
                        ui.weak(format!("{label} (undone)"));
                    }
                });
            });
        });
    }

    /// Undo, redo and save from the keyboard. Skipped while typing in a
    /// text field, where ⌘Z should undo the typing instead.
    pub(super) fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        // Redo first: ⌘⇧Z must not also count as ⌘Z.
        let (redo, undo, save) = ctx.input_mut(|i| {
            let redo = i.consume_shortcut(&REDO) || i.consume_shortcut(&REDO_ALT);
            (redo, i.consume_shortcut(&UNDO), i.consume_shortcut(&SAVE))
        });
        if redo {
            self.redo();
        } else if undo {
            self.undo();
        }
        if save {
            self.save(Then::Nothing);
        }
    }

    fn undo(&mut self) {
        let r = self.session.undo();
        if let Some(label) = self.report(r) {
            self.status = format!("undid {label}");
        }
    }

    fn redo(&mut self) {
        let r = self.session.redo();
        if let Some(label) = self.report(r) {
            self.status = format!("redid {label}");
        }
    }

    /// Saves to the current file, or asks where if there isn't one.
    fn save(&mut self, then: Then) {
        match self.session.project_path.clone() {
            Some(path) => {
                let r = self.session.save_project(&path);
                if self.report(r).is_some() {
                    self.status = format!("saved {}", path.display());
                    self.carry_on(then);
                }
            }
            None => self.dialog = Some(Dialog::SaveAs { path: self.suggested_path(), then, collect: false }),
        }
    }

    pub(super) fn save_as(&mut self, raw: &str, then: Then, collect: bool) {
        let mut path = crate::session::clean_path(raw);
        if !raw.trim().ends_with(".json") {
            path = PathBuf::from(format!("{}.{PROJECT_EXTENSION}", path.display()));
        }
        let r = self.session.save_project_with(&path, collect);
        if let Some(warnings) = self.report(r) {
            self.status = match (collect, warnings.len()) {
                (true, 0) => format!("saved {}, with all its audio", path.display()),
                (true, n) => format!("saved {}; {n} file(s) couldn't be collected: see the log", path.display()),
                _ => format!("saved {}", path.display()),
            };
            self.log.extend(warnings);
            self.carry_on(then);
        }
    }

    /// Called every frame: finishes a background load, and autosaves.
    pub(super) fn tick_project(&mut self) {
        if let Some(r) = self.session.poll_open() {
            if let Some(warnings) = self.report(r) {
                let name = self.session.project_path.as_ref().map_or("project".into(), |p| p.display().to_string());
                self.status = match warnings.len() {
                    0 => format!("opened {name}"),
                    n => format!("opened {name} with {n} warning(s): see the log"),
                };
                self.log.extend(warnings);
                self.forget_view_state();
                self.offer_recovery();
            }
        }
        if let Some(Err(e)) = self.session.autosave_tick(std::time::Instant::now()) {
            self.status = format!("autosave failed: {e}");
        }
    }

    /// If there's unsaved work from last time (a crash, probably), ask.
    pub(super) fn offer_recovery(&mut self) {
        if self.dialog.is_none() {
            if let Some((_, saved_at)) = self.session.pending_recovery() {
                self.dialog = Some(Dialog::Recover { saved_at });
            }
        }
    }

    fn suggested_path(&self) -> String {
        self.session.project_path.as_ref().map_or_else(|| format!("untitled.{PROJECT_EXTENSION}"), |p| p.display().to_string())
    }

    /// Before replacing the project or quitting: ask first if there are
    /// unsaved changes.
    fn after_unsaved_check(&mut self, then: Then) {
        if self.session.history.unsaved() {
            self.dialog = Some(Dialog::Unsaved { then });
        } else {
            self.carry_on(then);
        }
    }

    pub(super) fn carry_on(&mut self, then: Then) {
        match then {
            Then::Nothing => {}
            Then::New => {
                let r = self.session.new_project();
                if self.report(r).is_some() {
                    self.status = "new project".into();
                    self.forget_view_state();
                }
            }
            Then::Open(path) => {
                // In the background: `tick_project` finishes it.
                let r = self.session.begin_open(&path);
                if self.report(r).is_some() {
                    self.status = format!("opening {}...", path.display());
                }
            }
            Then::Quit => self.quit_now = true,
        }
    }

    /// Selections and open editors refer to the old project.
    fn forget_view_state(&mut self) {
        self.selected = crate::mixer::Target::Master;
        self.selected_sample = None;
        self.piano = None;
        self.timeline.shown_lane.clear();
    }

    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        // While a project loads: a progress bar, and a way out.
        if let Some((done, total)) = self.session.load_progress() {
            let name = self.session.loading_path().and_then(|p| p.file_name()).map_or("project".into(), |n| n.to_string_lossy().into_owned());
            egui::Window::new("Opening").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(format!("Loading {name}: {done} of {total} audio files"));
                ui.add(egui::ProgressBar::new(if total == 0 { 1.0 } else { done as f32 / total as f32 }).show_percentage());
                if ui.button("Cancel").clicked() {
                    self.session.cancel_open();
                    self.status = "opening cancelled".into();
                }
            });
            ctx.request_repaint();
        }

        // The window's close button: catch it if there's work to lose.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_now && self.session.history.unsaved() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::Unsaved { then: Then::Quit });
        }
        if self.quit_now {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }

        let Some(mut dialog) = self.dialog.take() else { return };
        let mut keep = true;
        let title = match &dialog {
            Dialog::Open { .. } => "Open project",
            Dialog::SaveAs { .. } => "Save project as",
            Dialog::Unsaved { .. } => "Unsaved changes",
            Dialog::Recover { .. } => "Recover unsaved work",
        };
        egui::Window::new(title).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            match &mut dialog {
                Dialog::Open { path } => {
                    ui.label("Path to a project file:");
                    let edit = ui.add(egui::TextEdit::singleline(path).desired_width(360.0).hint_text(format!("song.{PROJECT_EXTENSION}")));
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.horizontal(|ui| {
                        if (ui.button("Open").clicked() || enter) && !path.trim().is_empty() {
                            let target = crate::session::clean_path(path);
                            keep = false;
                            self.after_unsaved_check(Then::Open(target));
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
                Dialog::SaveAs { path, then, collect } => {
                    ui.label("Save to:");
                    let edit = ui.add(egui::TextEdit::singleline(path).desired_width(360.0));
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.checkbox(collect, "Copy all audio into the project's folder")
                        .on_hover_text("makes the project self-contained, to move or send to someone");
                    ui.weak("Audio inside the project's folder is stored relative to it, so the folder can be moved.");
                    ui.horizontal(|ui| {
                        if (ui.button("Save").clicked() || enter) && !path.trim().is_empty() {
                            let (p, t, c) = (path.clone(), then.clone(), *collect);
                            keep = false;
                            self.save_as(&p, t, c);
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
                Dialog::Recover { saved_at } => {
                    let ago = saved_at.elapsed().map_or("a while".into(), |d| match d.as_secs() {
                        s if s < 120 => format!("{s} seconds"),
                        s if s < 7_200 => format!("{} minutes", s / 60),
                        s => format!("{} hours", s / 3_600),
                    });
                    ui.label(format!("There's unsaved work from {ago} ago, most likely from a crash."));
                    ui.horizontal(|ui| {
                        if ui.button("Recover it").clicked() {
                            keep = false;
                            let r = self.session.restore_recovery();
                            if let Some(warnings) = self.report(r) {
                                self.status = "recovered: save to keep it".into();
                                self.log.extend(warnings);
                                self.forget_view_state();
                            }
                        }
                        if ui.button("Discard it").clicked() {
                            keep = false;
                            self.session.discard_recovery();
                        }
                    });
                }
                Dialog::Unsaved { then } => {
                    let what = match then {
                        Then::Quit => "quitting",
                        Then::New => "starting a new project",
                        _ => "opening another project",
                    };
                    ui.label(format!("Save your changes before {what}?"));
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            let t = then.clone();
                            keep = false;
                            self.save(t);
                        }
                        if ui.button("Don't save").clicked() {
                            let t = then.clone();
                            keep = false;
                            // A deliberate "no": the recovery copy goes too.
                            self.session.discard_recovery();
                            self.carry_on(t);
                        }
                        if ui.button("Cancel").clicked() {
                            keep = false;
                        }
                    });
                }
            }
        });
        // An action above may have opened a different dialog (Save -> Save As).
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }

    /// "daw - song.daw.json •": the • means unsaved changes.
    pub(super) fn update_title(&mut self, ctx: &egui::Context) {
        let name = self.session.project_path.as_ref().and_then(|p| p.file_name()).map_or("untitled".into(), |n| n.to_string_lossy().into_owned());
        let title = format!("daw - {name}{}", if self.session.history.unsaved() { " •" } else { "" });
        if self.title != title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }
}

const UNDO: egui::KeyboardShortcut = egui::KeyboardShortcut::new(Modifiers::COMMAND, Key::Z);
const REDO: egui::KeyboardShortcut = egui::KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Z);
const REDO_ALT: egui::KeyboardShortcut = egui::KeyboardShortcut::new(Modifiers::COMMAND, Key::Y);
const SAVE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
