//! The graphical front end, built with egui.
//!
//! egui is an "immediate mode" UI: every frame, the whole interface is
//! described again from the current state. That fits a DAW well. The
//! state already exists, in the session's project mirror, and drawing it
//! each frame means the screen can never drift out of sync with it.
//!
//! The rules from the command line still hold: the UI only talks to the
//! engine through `Session`, which sends commands through the lock-free
//! queue and reads meters and the playhead from atomics. Nothing on this
//! thread can ever block the audio thread.

mod inspector;
mod menu;
mod meter;
mod mixer;
mod pianoroll;
mod timeline;
mod widgets;

use crate::mixer::Target;
use crate::session::{self, clean_path, is_audio_file, Session};
use crate::transport::{format_bbt, BEATS_PER_BAR};
use eframe::egui::{self, Key};
use std::time::Duration;

pub fn run(session: Session) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1400.0, 860.0]).with_title("daw"),
        ..Default::default()
    };
    eframe::run_native("daw", options, Box::new(|_cc| Box::new(DawApp::new(session))))
}

pub struct DawApp {
    pub session: Session,
    selected: Target,
    selected_sample: Option<usize>,
    load_path: String,
    /// The latest error or message, shown in the status bar.
    status: String,
    log: Vec<String>,
    timeline: timeline::TimelineState,
    meter_bank: mixer::MeterBank,
    midi_ports: Vec<String>,
    loop_bars: (f64, f64),
    /// The open piano roll, if any.
    piano: Option<pianoroll::PianoRoll>,
    input_meter: meter::MeterState,
    punch_bars: (f64, f64),
    dialog: Option<menu::Dialog>,
    /// Displayed CPU share per plugin instance (by address), decayed.
    plugin_cpu: std::collections::HashMap<usize, f32>,
    /// Set once the user has chosen to quit (after any saving).
    quit_now: bool,
    title: String,
}

impl DawApp {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            selected: Target::Master,
            selected_sample: None,
            load_path: String::new(),
            status: "Drop audio files onto the window to load them. Space plays and stops.".into(),
            log: Vec::new(),
            timeline: timeline::TimelineState::default(),
            meter_bank: mixer::MeterBank::default(),
            midi_ports: Vec::new(),
            loop_bars: (1.0, 5.0),
            piano: None,
            input_meter: meter::MeterState::default(),
            punch_bars: (3.0, 5.0),
            dialog: None,
            plugin_cpu: std::collections::HashMap::new(),
            quit_now: false,
            title: String::new(),
        }
    }

    /// Shows a failed action's message instead of losing it.
    fn report<T>(&mut self, r: session::Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                self.status = e;
                None
            }
        }
    }

    /// One frame. Separate from `eframe::App::update` so tests can drive
    /// it with a bare `egui::Context`, no window needed.
    pub fn ui(&mut self, ctx: &egui::Context) {
        self.session.collect_garbage();
        for msg in self.session.take_messages().into_iter().chain(self.session.poll_recordings()) {
            self.status = msg.clone();
            self.log.push(msg);
        }
        self.handle_input(ctx);
        self.shortcuts(ctx);
        self.update_title(ctx);

        egui::TopBottomPanel::top("menu").show(ctx, |ui| self.menu_ui(ui));
        egui::TopBottomPanel::top("transport").show(ctx, |ui| self.transport_ui(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| ui.label(&self.status));
        egui::TopBottomPanel::bottom("mixer").resizable(true).default_height(270.0).show(ctx, |ui| self.mixer_ui(ui));
        egui::SidePanel::left("library").resizable(true).default_width(210.0).show(ctx, |ui| self.library_ui(ui));
        egui::SidePanel::right("inspector").resizable(true).default_width(300.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.inspector_ui(ui));
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.timeline_ui(ui));
        });
        self.piano_ui(ctx);
        self.dialogs(ctx);

        // Meters and the playhead move on their own, so keep redrawing
        // (about 60 fps). egui otherwise only redraws on input.
        ctx.request_repaint_after(Duration::from_millis(16));
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        // Space plays/stops, unless the user is typing in a text field.
        if !ctx.wants_keyboard_input() && ctx.input(|i| i.key_pressed(Key::Space)) {
            let r = self.session.toggle_play();
            self.report(r);
        }
        // Files dropped onto the window are loaded in the background.
        let dropped: Vec<_> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for path in dropped {
            if is_audio_file(&path) {
                self.status = format!("loading {}...", path.display());
                self.session.load(path, false);
            } else {
                self.status = format!("not an audio file: {}", path.display());
            }
        }
    }

    fn transport_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let playing = self.session.playing();
            if ui.button(if playing { "⏹ Stop" } else { "▶ Play" }).clicked() {
                let r = self.session.toggle_play();
                self.report(r);
            }
            if ui.button("⏮").on_hover_text("back to the start").clicked() {
                let r = self.session.seek(0.0);
                self.report(r);
            }
            ui.monospace(format!("{}   {:7.2}s", format_bbt(self.session.current_beat()), self.session.position_secs()));
            if let Some(r) = self.session.recorder() {
                if r.shared.recording.load(std::sync::atomic::Ordering::Relaxed) {
                    ui.label(egui::RichText::new("● REC").strong().color(egui::Color32::from_rgb(230, 60, 50)));
                } else if let Some(t) = self.session.rec_track {
                    ui.label(egui::RichText::new(format!("armed: track {t}")).color(egui::Color32::from_rgb(200, 120, 110)));
                }
            }
            ui.separator();

            let mut tempo = self.session.tempo();
            if ui.add(egui::DragValue::new(&mut tempo).speed(0.5).clamp_range(20.0..=999.0).suffix(" BPM")).changed() {
                let r = self.session.set_tempo(tempo);
                self.report(r);
            }
            ui.separator();

            let mut looping = self.session.project.loop_range.is_some();
            let mut changed = ui.checkbox(&mut looping, "loop bars").changed();
            changed |= ui.add(egui::DragValue::new(&mut self.loop_bars.0).speed(0.1).clamp_range(1.0..=999.0)).changed();
            ui.label("to");
            changed |= ui.add(egui::DragValue::new(&mut self.loop_bars.1).speed(0.1).clamp_range(1.0..=1000.0)).changed();
            if changed {
                let range = looping.then(|| (session::bar_to_beat(self.loop_bars.0), session::bar_to_beat(self.loop_bars.1)));
                let r = self.session.set_loop(range);
                self.report(r);
            }
            ui.checkbox(&mut self.session.return_to_start, "return on stop");
            if self.session.recorder().is_some() {
                let mut punching = self.session.punch.is_some();
                let mut changed = ui.checkbox(&mut punching, "punch").on_hover_text("record only inside this range of bars").changed();
                changed |= ui.add(egui::DragValue::new(&mut self.punch_bars.0).speed(0.1).clamp_range(1.0..=999.0)).changed();
                ui.label("to");
                changed |= ui.add(egui::DragValue::new(&mut self.punch_bars.1).speed(0.1).clamp_range(1.0..=1000.0)).changed();
                if changed {
                    let range = punching.then(|| (session::bar_to_beat(self.punch_bars.0), session::bar_to_beat(self.punch_bars.1)));
                    let r = self.session.set_punch(range);
                    self.report(r);
                }
            }
            ui.separator();

            ui.checkbox(&mut self.timeline.snapping, "snap");
            egui::ComboBox::from_id_source("grid")
                .selected_text(grid_label(self.timeline.grid))
                .width(60.0)
                .show_ui(ui, |ui| {
                    for g in [BEATS_PER_BAR, 1.0, 0.5, 0.25] {
                        ui.selectable_value(&mut self.timeline.grid, g, grid_label(g));
                    }
                });
            ui.checkbox(&mut self.timeline.follow, "follow");
            ui.separator();

            if ui.button("+ Track").clicked() {
                let r = self.session.add_track(None);
                if let Some(i) = self.report(r) {
                    self.selected = Target::Track(i);
                }
            }
        });
    }

    fn library_ui(&mut self, ui: &mut egui::Ui) {
        ui.strong("Samples");
        ui.horizontal(|ui| {
            let edit = ui.add(egui::TextEdit::singleline(&mut self.load_path).hint_text("path to a file").desired_width(140.0));
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if (ui.button("Load").clicked() || enter) && !self.load_path.trim().is_empty() {
                let path = clean_path(&self.load_path);
                self.status = format!("loading {}...", path.display());
                self.session.load(path, false);
                self.load_path.clear();
            }
        });
        let samples = self.session.samples();
        if samples.is_empty() {
            ui.weak("Drop audio files here.");
        }
        for (i, s) in samples.iter().enumerate() {
            let selected = self.selected_sample == Some(i);
            let text = format!("{} ({:.1}s)", s.name, s.duration_secs());
            if ui.selectable_label(selected, text).clicked() {
                self.selected_sample = if selected { None } else { Some(i) };
            }
        }
        if self.selected_sample.is_some() {
            ui.weak("Double-click a lane to place it.");
        }

        ui.separator();
        self.input_ui(ui);

        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Plugins");
            if ui.small_button("Scan").on_hover_text("look in the standard CLAP folders, and $CLAP_PATH").clicked() {
                let problems = self.session.scan_plugins();
                self.status = format!("found {} plugin(s)", self.session.plugins.len());
                self.log.extend(problems);
            }
        });
        if self.session.plugins.is_empty() {
            ui.weak("None found yet.");
        }
        for p in &self.session.plugins {
            let kind = if p.descriptor.is_instrument() { "instrument" } else { "effect" };
            ui.small(format!("{} ({kind})", p.descriptor.name)).on_hover_text(p.path.display().to_string());
        }

        ui.separator();
        ui.strong("MIDI input");
        ui.horizontal(|ui| {
            if ui.button("Find ports").clicked() {
                let r = self.session.midi_ports();
                if let Some(ports) = self.report(r) {
                    if ports.is_empty() {
                        self.status = "no MIDI inputs found".into();
                    }
                    self.midi_ports = ports;
                }
            }
            if self.session.midi_open() && ui.button("Close").clicked() {
                self.session.close_midi();
            }
        });
        for (i, name) in self.midi_ports.clone().iter().enumerate() {
            if ui.button(name).clicked() {
                let r = self.session.open_midi(i);
                if let Some(name) = self.report(r) {
                    self.status = format!("listening to {name}: press R on a synth track to play it");
                }
            }
        }

        if !self.log.is_empty() {
            ui.separator();
            ui.strong("Log");
            for line in self.log.iter().rev().take(8) {
                ui.small(line);
            }
        }
    }
}

impl DawApp {
    /// The audio input: level, latency, and the manual adjustment.
    fn input_ui(&mut self, ui: &mut egui::Ui) {
        use std::sync::atomic::Ordering::Relaxed;
        ui.strong("Audio input");
        let Some(r) = self.session.recorder() else {
            ui.weak("No input device: recording is unavailable.");
            return;
        };
        ui.small(&r.device);
        let (peak, out_lat, in_lat, dropped) = (
            r.shared.take_input_peak(),
            r.shared.output_latency.load(Relaxed),
            r.shared.input_latency.load(Relaxed),
            r.shared.dropped.load(Relaxed),
        );
        let dt = ui.input(|i| i.unstable_dt).clamp(0.0, 0.25);
        self.input_meter.update(peak, dt);

        // A horizontal level meter, so you can set the input gain.
        let (resp, painter) = ui.allocate_painter(egui::Vec2::new(ui.available_width().min(180.0), 8.0), egui::Sense::click());
        let rect = resp.rect;
        painter.rect_filled(rect, egui::Rounding::same(2.0), egui::Color32::from_gray(20));
        let m = self.input_meter;
        let w = meter::MeterState::fraction(m.db) * rect.width();
        let color = if m.clipped { egui::Color32::from_rgb(220, 60, 50) } else { egui::Color32::from_rgb(80, 200, 100) };
        painter.rect_filled(egui::Rect::from_min_size(rect.min, egui::Vec2::new(w, rect.height())), egui::Rounding::ZERO, color);
        if resp.clicked() {
            self.input_meter.clipped = false;
        }

        let ms = |frames: u32| frames as f64 * 1000.0 / self.session.sample_rate;
        ui.small(format!("latency: out {:.1} ms + in {:.1} ms", ms(out_lat), ms(in_lat)))
            .on_hover_text("as reported by the driver; takes are moved earlier by this much");
        let mut adjust = self.session.latency_adjust_ms;
        let drag = egui::DragValue::new(&mut adjust).speed(0.1).clamp_range(-500.0..=500.0).suffix(" ms");
        let changed = ui
            .horizontal(|ui| {
                ui.small("adjust");
                ui.add(drag).on_hover_text("extra compensation: calibrate with a loopback cable if takes land late or early").changed()
            })
            .inner;
        if changed {
            let r = self.session.set_latency_adjust(adjust);
            self.report(r);
        }
        let mut monitor = self.session.monitor;
        let tip = "hear the input through the armed track and its effects. Use headphones: with speakers and a microphone, it can feed back";
        if ui.checkbox(&mut monitor, "monitor input").on_hover_text(tip).changed() {
            let r = self.session.set_monitor(monitor);
            self.report(r);
        }
        ui.horizontal(|ui| {
            match self.session.drift_ppm() {
                Some(ppm) => ui.small(format!("clock drift: {ppm:+.1} ppm ({:.0} s)", self.session.drift.span_frames() / self.session.sample_rate)),
                None => ui.small("clock drift: measuring..."),
            }
            .on_hover_text("how much faster the input's clock runs than the output's; nonzero means takes slowly slide in time");
            if ui.small_button("reset").clicked() {
                self.session.drift.reset();
            }
        });
        if dropped > 0 {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 60), format!("{dropped} chunks dropped: the disk couldn't keep up"));
        }
    }
}

fn grid_label(grid: f64) -> &'static str {
    match grid {
        g if g == BEATS_PER_BAR => "bar",
        g if g == 1.0 => "1/4",
        g if g == 0.5 => "1/8",
        _ => "1/16",
    }
}

impl eframe::App for DawApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::timeline::ClipRef;
    use super::*;
    use crate::session::tests::TestSession;
    use eframe::egui::{Event, Modifiers, PointerButton, Pos2, Rect, Vec2};

    /// A UI with no window: frames are driven by hand with synthetic input.
    struct Harness {
        app: DawApp,
        ctx: egui::Context,
        time: f64,
        commands: rtrb::Consumer<crate::engine::Command>,
        _keep: (rtrb::Producer<crate::engine::Garbage>, rtrb::Consumer<crate::midi::LiveMidi>),
    }

    impl Harness {
        fn new(setup: impl FnOnce(&mut TestSession)) -> Self {
            let mut t = TestSession::new();
            setup(&mut t);
            let TestSession { session, commands, garbage, live } = t;
            Self { app: DawApp::new(session), ctx: egui::Context::default(), time: 0.0, commands, _keep: (garbage, live) }
        }

        fn frame(&mut self, events: Vec<Event>) {
            self.time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 860.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let _ = self.ctx.run(input, |ctx| self.app.ui(ctx));
        }

        fn button(&mut self, pos: Pos2, pressed: bool) {
            self.frame(vec![Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }]);
        }

        /// Presses at `from`, moves to `to` in steps, releases.
        fn drag(&mut self, from: Pos2, to: Pos2) {
            self.frame(vec![Event::PointerMoved(from)]);
            self.button(from, true);
            for k in 1..=10 {
                self.frame(vec![Event::PointerMoved(from + (to - from) * (k as f32 / 10.0))]);
            }
            self.button(to, false);
            self.frame(vec![]);
        }

        fn clip_rect(&self, clip: ClipRef) -> Rect {
            self.app.timeline.clip_rects.iter().find(|(c, _)| *c == clip).expect("clip not drawn").1
        }

        fn count_sent(&mut self, label: &str) -> usize {
            let mut n = 0;
            while let Ok(c) = self.commands.pop() {
                let name = match c {
                    crate::engine::Command::SetClips { .. } => "SetClips",
                    crate::engine::Command::SetSequence { .. } => "SetSequence",
                    crate::engine::Command::Seek { .. } => "Seek",
                    crate::engine::Command::Play => "Play",
                    _ => "",
                };
                n += (name == label) as usize;
            }
            n
        }
    }

    /// Two tracks: an audio clip at beat 4 on track 0, a synth with a demo
    /// MIDI clip and effects on track 1.
    fn project(t: &mut TestSession) {
        let s = &mut t.session;
        s.add_track(Some("Drums".into())).unwrap();
        s.add_track(Some("Keys".into())).unwrap();
        let sample = t.add_sample(4.0);
        let s = &mut t.session;
        s.add_clip(0, sample, 4.0).unwrap();
        s.set_synth(1, true).unwrap();
        s.add_midi_clip(1, 16.0, 16.0, session::demo_notes()).unwrap();
        for kind in ["eq", "comp", "delay", "reverb", "filter"] {
            s.add_node(Target::Track(1), kind, None).unwrap();
        }
        s.add_node(Target::Master, "comp", None).unwrap();
        s.set_loop(Some((0.0, 32.0))).unwrap();
    }

    #[test]
    fn renders_a_busy_project_without_panicking() {
        let mut h = Harness::new(project);
        for target in [Target::Track(0), Target::Track(1), Target::Aux, Target::Master] {
            h.app.selected = target; // every inspector view
            for _ in 0..3 {
                h.frame(vec![]);
            }
        }
        assert_eq!(h.app.timeline.clip_rects.len(), 2, "both clips drawn");
    }

    #[test]
    fn dragging_a_clip_moves_it_and_commits_once() {
        let mut h = Harness::new(project);
        h.frame(vec![]);
        h.count_sent("SetClips"); // ignore setup
        let clip = ClipRef::Audio { track: 0, id: 1 };
        let rect = h.clip_rect(clip);
        let beat_px = h.app.timeline.px_per_beat;
        // Grab the middle and drag 3.3 beats right: snaps to 3.
        h.drag(rect.center(), rect.center() + Vec2::new(3.3 * beat_px, 0.0));
        let p = h.app.session.clip(0, 1).unwrap().params;
        assert_eq!(p.start_beat, 7.0, "4 + 3, snapped to the beat");
        assert_eq!(h.count_sent("SetClips"), 1, "the engine heard about it once, on release");
    }

    #[test]
    fn dragging_the_right_edge_resizes() {
        let mut h = Harness::new(project);
        h.frame(vec![]);
        let clip = ClipRef::Audio { track: 0, id: 1 };
        let rect = h.clip_rect(clip);
        let beat_px = h.app.timeline.px_per_beat;
        let len_before = h.app.session.clip(0, 1).unwrap().params.length_beats;
        let edge = Pos2::new(rect.right() - 2.0, rect.center().y);
        h.drag(edge, edge - Vec2::new(2.0 * beat_px, 0.0));
        let p = h.app.session.clip(0, 1).unwrap().params;
        assert_eq!(p.start_beat, 4.0, "resizing doesn't move the start");
        assert_eq!(p.length_beats, (len_before - 2.0).round(), "the end snaps to the grid");
    }

    #[test]
    fn midi_clips_drag_too() {
        let mut h = Harness::new(project);
        h.frame(vec![]);
        let clip = ClipRef::Midi { track: 1, id: 2 };
        let rect = h.clip_rect(clip);
        let beat_px = h.app.timeline.px_per_beat;
        h.drag(rect.left_center() + Vec2::new(10.0, 0.0), rect.left_center() + Vec2::new(10.0 - 4.0 * beat_px, 0.0));
        let start = h.app.session.project.tracks[1].as_ref().unwrap().midi_clips[0].start_beat;
        assert_eq!(start, 12.0);
    }

    #[test]
    fn clicking_the_ruler_seeks() {
        let mut h = Harness::new(project);
        h.frame(vec![]);
        h.count_sent("Seek");
        // Beat 8 on the ruler: two beats right of... the clip's start at 4.
        let rect = h.clip_rect(ClipRef::Audio { track: 0, id: 1 });
        let x = rect.left() + 4.0 * h.app.timeline.px_per_beat;
        let ruler_y = rect.top() - 2.0 - timeline::RULER_H / 2.0;
        h.frame(vec![Event::PointerMoved(Pos2::new(x, ruler_y))]);
        h.button(Pos2::new(x, ruler_y), true);
        h.button(Pos2::new(x, ruler_y), false);
        assert!(h.count_sent("Seek") >= 1);
    }

    #[test]
    fn space_plays_and_stops() {
        let mut h = Harness::new(project);
        h.frame(vec![]);
        let key = |pressed| Event::Key { key: Key::Space, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE };
        h.count_sent("Play");
        h.frame(vec![key(true), key(false)]);
        assert_eq!(h.count_sent("Play"), 1, "space sent Play");
    }

    // ---- Step 9 exercises ----

    impl Harness {
        fn click_with(&mut self, pos: Pos2, button: PointerButton) {
            self.frame(vec![Event::PointerMoved(pos)]);
            for pressed in [true, false] {
                self.frame(vec![Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE }]);
            }
            self.frame(vec![]);
        }

        fn click(&mut self, pos: Pos2) {
            self.click_with(pos, PointerButton::Primary);
        }

        fn double_click(&mut self, pos: Pos2) {
            self.frame(vec![Event::PointerMoved(pos)]);
            for _ in 0..2 {
                self.button(pos, true);
                self.button(pos, false);
            }
            self.frame(vec![]);
        }
    }

    #[test]
    fn dragging_a_clip_down_moves_it_to_the_next_track() {
        let mut h = Harness::new(project);
        h.frame(vec![]);
        let rect = h.clip_rect(ClipRef::Audio { track: 0, id: 1 });
        h.drag(rect.center(), rect.center() + Vec2::new(0.0, timeline::LANE_H));
        assert!(h.app.session.clip(0, 1).is_none(), "gone from track 0");
        let moved = h.app.session.clip(1, 1).expect("on track 1");
        assert_eq!(moved.params.start_beat, 4.0, "same time");
        assert_eq!(h.app.selected, Target::Track(1), "the destination is selected");
    }

    fn automation_row(h: &Harness) -> Rect {
        let row = h.app.timeline.rows.iter().find(|r| matches!(r.kind, timeline::RowKind::Automation(_))).expect("lane shown");
        let clips = h.clip_rect(ClipRef::Audio { track: 0, id: 1 });
        // The lane spans from the clip area's left edge (beat 0 is 4 beats
        // left of the clip) to the right of the screen.
        let left = clips.left() - 4.0 * h.app.timeline.px_per_beat;
        Rect::from_min_max(Pos2::new(left, row.top), Pos2::new(clips.right() + 400.0, row.top + row.height))
    }

    #[test]
    fn automation_points_are_added_dragged_and_deleted() {
        let mut h = Harness::new(project);
        let lane = h.app.session.add_automation(0, crate::automation::AutoTarget::Volume).unwrap();
        h.app.timeline.shown_lane.insert(0, lane);
        h.frame(vec![]);
        let row = automation_row(&h);
        let ppb = h.app.timeline.px_per_beat;
        let points = |h: &Harness| h.app.session.project.tracks[0].as_ref().unwrap().automation[lane].points.clone();

        // Click at beat 8, near the top of the lane: a new point.
        let at = Pos2::new(row.left() + 8.0 * ppb, timeline::value_to_y(0.9, row));
        h.click(at);
        let p = points(&h);
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].0, 8.0);
        assert!((p[1].1 - 0.9).abs() < 0.03);

        // Drag it down and two beats right.
        h.drag(at, Pos2::new(at.x + 2.0 * ppb, timeline::value_to_y(0.2, row)));
        let p = points(&h);
        assert_eq!(p[1].0, 10.0);
        assert!((p[1].1 - 0.2).abs() < 0.03, "{}", p[1].1);

        // Right-click it: deleted.
        h.click_with(Pos2::new(row.left() + 10.0 * ppb, timeline::value_to_y(p[1].1, row)), PointerButton::Secondary);
        assert_eq!(points(&h).len(), 1);
    }

    #[test]
    fn piano_roll_opens_and_edits_notes() {
        let mut h = Harness::new(project);
        h.frame(vec![]);
        // An empty MIDI clip to edit.
        h.app.session.edit_midi_clip(1, 2, |c| c.notes.clear()).unwrap();
        h.frame(vec![]);
        let rect = h.clip_rect(ClipRef::Midi { track: 1, id: 2 });
        h.double_click(rect.center());
        assert!(h.app.piano.is_some(), "double-click opens the piano roll");
        h.frame(vec![]);

        let (grid, view) = {
            let roll = h.app.piano.as_ref().unwrap();
            (roll.grid_rect, roll.view.unwrap())
        };
        assert!(grid.width() > 100.0);
        // Click at beat 1 on pitch 80: a new note there, one grid step long.
        let at = Pos2::new(view.beat_to_x(1.0) + 3.0, view.pitch_to_y(80) + pianoroll::ROW_H / 2.0);
        h.click(at);
        let notes = || h.app.session.project.tracks[1].as_ref().unwrap().midi_clips[0].notes.clone();
        let n = notes();
        assert_eq!(n.len(), 1);
        assert_eq!((n[0].start, n[0].pitch, n[0].length), (1.0, 80, 1.0));

        // Drag it two beats right and three semitones down.
        let to = Pos2::new(at.x + 2.0 * view.px_per_beat, at.y + 3.0 * pianoroll::ROW_H);
        h.drag(at, to);
        let n = h.app.session.project.tracks[1].as_ref().unwrap().midi_clips[0].notes.clone();
        assert_eq!((n[0].start, n[0].pitch), (3.0, 77));
    }

    // ---- Step 10: recording ----

    #[test]
    fn recording_controls_render_and_finished_takes_land() {
        use crate::record::{Finished, RecordShared};
        use std::sync::atomic::Ordering::Relaxed;
        use std::sync::Arc;
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = RecordShared::new();
        let for_session = shared.clone();
        let mut h = Harness::new(move |t| {
            project(t);
            t.session.attach_recorder(session::Recorder { device: "Mic".into(), done: rx, shared: for_session });
        });
        h.app.session.set_record_track(Some(0)).unwrap();
        shared.recording.store(true, Relaxed);
        shared.record_peak(0.5);
        shared.output_latency.store(480, Relaxed);
        for _ in 0..3 {
            h.frame(vec![]);
        }
        assert!(h.app.input_meter.db > -7.0, "the input meter shows the level");

        let audio = Arc::new(crate::loader::AudioData { path: None, name: "take-001.wav".into(), sample_rate: 48_000.0, channels: vec![vec![0.1; 48_000]; 2] });
        tx.send(Finished { take: 1, track: 0, start_frame: 192_000, path: Ok("recordings/take-001.wav".into()), audio }).unwrap();
        h.frame(vec![]);
        assert!(h.app.status.starts_with("recorded take 1"), "{}", h.app.status);
        assert_eq!(h.app.session.project.tracks[0].as_ref().unwrap().clips.len(), 2);
        h.frame(vec![]); // and the new clip draws
        assert_eq!(h.app.timeline.clip_rects.len(), 3);
    }

    #[test]
    fn punch_monitor_and_drift_controls_render() {
        use crate::record::RecordShared;
        use std::sync::atomic::Ordering::Relaxed;
        let (_tx, rx) = std::sync::mpsc::channel();
        let shared = RecordShared::new();
        let for_session = shared.clone();
        let mut h = Harness::new(move |t| {
            project(t);
            t.session.attach_recorder(session::Recorder { device: "Mic".into(), done: rx, shared: for_session });
        });
        h.app.session.set_record_track(Some(0)).unwrap();
        h.app.session.set_monitor(true).unwrap();
        h.app.session.set_punch(Some((8.0, 16.0))).unwrap();
        // Two seconds' worth of counters per frame, input 100 ppm fast.
        for k in 1..=10u64 {
            let out = k * 96_000;
            shared.frames_out.store(out, Relaxed);
            shared.frames_in.store(out + out / 10_000, Relaxed);
            h.frame(vec![]);
        }
        let ppm = h.app.session.drift_ppm().expect("enough history");
        assert!((ppm - 100.0).abs() < 1.0, "{ppm}");
    }

    // ---- Step 11: undo, saving, quitting ----

    impl Harness {
        fn key(&mut self, key: Key, modifiers: Modifiers) {
            let ev = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
            self.frame(vec![ev(true), ev(false)]);
        }

        /// A frame in which the window's close button was pressed.
        fn close_requested(&mut self) -> egui::FullOutput {
            self.time += 1.0 / 60.0;
            let mut input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 860.0))),
                time: Some(self.time),
                ..Default::default()
            };
            input.viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
            self.ctx.run(input, |ctx| self.app.ui(ctx))
        }
    }

    fn sent_close_command(out: &egui::FullOutput, cmd: egui::ViewportCommand) -> bool {
        out.viewport_output.get(&egui::ViewportId::ROOT).map_or(false, |v| v.commands.contains(&cmd))
    }

    #[test]
    fn keyboard_undo_and_redo() {
        let mut h = Harness::new(project);
        h.frame(vec![]);
        h.app.session.set_volume(Target::Track(0), -12.0).unwrap();
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.app.session.project.tracks[0].as_ref().unwrap().volume_db, 0.0);
        assert_eq!(h.app.status, "undid volume");
        h.key(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(h.app.session.project.tracks[0].as_ref().unwrap().volume_db, -12.0, "redo, not a second undo");
        h.key(Key::Z, Modifiers::COMMAND);
        h.key(Key::Y, Modifiers::COMMAND);
        assert_eq!(h.app.session.project.tracks[0].as_ref().unwrap().volume_db, -12.0, "Ctrl+Y redoes too");
    }

    #[test]
    fn the_title_shows_unsaved_changes() {
        let dir = std::env::temp_dir().join("daw_ui_title");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = Harness::new(project);
        h.frame(vec![]);
        assert!(h.app.title.ends_with("untitled •"), "{}", h.app.title);
        h.app.save_as(&dir.join("song").display().to_string(), menu::Then::Nothing);
        h.frame(vec![]);
        assert_eq!(h.app.title, "daw - song.daw.json", "saved: no dot, and the extension added");
        h.app.session.set_mute(0, true).unwrap();
        h.frame(vec![]);
        assert!(h.app.title.ends_with('•'));
    }

    #[test]
    fn closing_with_unsaved_work_asks_first() {
        let mut h = Harness::new(project); // project() makes changes: unsaved
        h.frame(vec![]);
        let out = h.close_requested();
        assert!(sent_close_command(&out, egui::ViewportCommand::CancelClose), "the close was stopped");
        assert_eq!(h.app.dialog, Some(menu::Dialog::Unsaved { then: menu::Then::Quit }));
        // "Don't save": now it really closes.
        h.app.dialog = None;
        h.app.carry_on(menu::Then::Quit);
        h.time += 1.0 / 60.0;
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 860.0))), time: Some(h.time), ..Default::default() };
        let out = h.ctx.run(input, |ctx| h.app.ui(ctx));
        assert!(sent_close_command(&out, egui::ViewportCommand::Close));
    }

    #[test]
    fn closing_with_nothing_to_lose_just_closes() {
        let mut h = Harness::new(|_| {});
        h.frame(vec![]);
        let out = h.close_requested();
        assert!(!sent_close_command(&out, egui::ViewportCommand::CancelClose));
        assert_eq!(h.app.dialog, None);
    }

    #[test]
    fn new_project_with_unsaved_work_asks_then_clears() {
        let mut h = Harness::new(project);
        h.frame(vec![]);
        h.app.dialog = Some(menu::Dialog::Unsaved { then: menu::Then::New });
        h.frame(vec![]); // the dialog draws
        h.app.dialog = None;
        h.app.carry_on(menu::Then::New);
        assert!(h.app.session.project.tracks.iter().all(|t| t.is_none()));
        h.frame(vec![]);
        assert!(h.app.timeline.clip_rects.is_empty());
    }

    // ---- Step 12: plugins ----

    #[test]
    fn plugin_nodes_render_in_the_inspector() {
        use crate::clap::test_plugin::{BEEP_ID, GAIN_ID};
        let mut h = Harness::new(|t| {
            project(t);
            t.session.register_bundle(crate::clap::tests::example_bundle());
            let path = std::path::Path::new("example.clap");
            t.session.add_plugin(Target::Track(1), path, GAIN_ID, None).unwrap();
            t.session.set_plugin_instrument(1, path, BEEP_ID).unwrap();
            // And a placeholder for a plugin that couldn't be found.
            let mut missing = crate::project::NodeInfo::new(crate::clap::intern("org.gone.reverb (missing)"), &[]);
            missing.plugin = Some(crate::project::PluginInfo { path: "/nowhere/Gone.clap".into(), id: "org.gone.reverb".into(), instance: None, state: None });
            t.session.project.tracks[1].as_mut().unwrap().slots[9] = Some(missing);
        });
        h.app.selected = Target::Track(1);
        for _ in 0..3 {
            h.frame(vec![]);
        }
        assert_eq!(h.app.plugin_cpu.len(), 2, "both live plugins show their load");
    }
}
