//! The piano roll: a MIDI clip's notes on a pitch-by-time grid.
//!
//! Click empty space to add a note, drag a note to move it (in time and
//! pitch), drag its right edge to change its length, right-click to delete.
//! As on the timeline, drags are previewed locally and committed once.

use super::timeline::{drag_result, snap, DragMode};
use super::DawApp;
use crate::midi::Note;
use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Id, Pos2, Rect, Rounding, Sense, Stroke, Vec2};

pub const ROW_H: f32 = 12.0;
pub const KEYS_W: f32 = 40.0;
const EDGE_W: f32 = 5.0;

/// Maps between (beat, pitch) and screen positions. `origin` is the
/// top-left of the note grid; `top_pitch` is the pitch of the top row.
#[derive(Clone, Copy, Debug)]
pub struct PianoView {
    pub origin: Pos2,
    pub px_per_beat: f32,
    pub scroll: f64,
    pub top_pitch: i32,
}

impl PianoView {
    pub fn beat_to_x(&self, beat: f64) -> f32 {
        self.origin.x + ((beat - self.scroll) as f32) * self.px_per_beat
    }

    pub fn x_to_beat(&self, x: f32) -> f64 {
        self.scroll + ((x - self.origin.x) / self.px_per_beat) as f64
    }

    /// The top edge of a pitch's row.
    pub fn pitch_to_y(&self, pitch: i32) -> f32 {
        self.origin.y + (self.top_pitch - pitch) as f32 * ROW_H
    }

    pub fn y_to_pitch(&self, y: f32) -> i32 {
        self.top_pitch - ((y - self.origin.y) / ROW_H).floor() as i32
    }
}

/// A note after dragging it by `beats` and `pitches`: moved, or resized.
/// Notes stay inside the clip and inside MIDI's 0-127 range.
pub fn note_drag(mode: DragMode, n: Note, beats: f64, pitches: i32, grid: f64, snapping: bool, clip_len: f64) -> Note {
    match mode {
        DragMode::Move => {
            let latest = (clip_len - n.length).max(0.0);
            let start = snap(n.start + beats, grid, snapping).clamp(0.0, latest);
            let pitch = (n.pitch as i32 + pitches).clamp(0, 127) as u8;
            Note { start, pitch, ..n }
        }
        DragMode::ResizeEnd => {
            let (_, length) = drag_result(DragMode::ResizeEnd, n.start, n.length, beats, grid, snapping);
            Note { length: length.min(clip_len - n.start).max(1.0 / 64.0), ..n }
        }
    }
}

fn is_black(pitch: i32) -> bool {
    matches!(pitch.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

#[derive(Clone, Copy, Debug)]
struct NoteDrag {
    index: usize,
    mode: DragMode,
    original: Note,
    grab: (f64, i32),
    preview: Note,
}

pub struct PianoRoll {
    /// The clip being edited, by id: ids stay the same when a clip moves
    /// to another track, so the editor finds it wherever it goes.
    pub clip: u32,
    px_per_beat: f32,
    scroll: f64,
    pub top_pitch: i32,
    drag: Option<NoteDrag>,
    /// Where each note was drawn last frame (by index), for tests.
    pub note_rects: Vec<(usize, Rect)>,
    pub grid_rect: Rect,
    pub view: Option<PianoView>,
}

impl PianoRoll {
    pub fn new(_track: usize, clip: u32) -> Self {
        Self {
            clip,
            px_per_beat: 48.0,
            scroll: 0.0,
            top_pitch: 84,
            drag: None,
            note_rects: Vec::new(),
            grid_rect: Rect::NOTHING,
            view: None,
        }
    }
}

impl DawApp {
    /// Draws the piano roll window, if one is open.
    pub(super) fn piano_ui(&mut self, ctx: &egui::Context) {
        // Taken out while drawing, so it and the session can both be
        // borrowed mutably; put back unless the window was closed.
        let Some(mut roll) = self.piano.take() else { return };
        let Some(track) = self.find_midi_clip(roll.clip) else { return }; // clip deleted
        let mut open = true;
        let title = format!("Piano roll: MIDI #{} on track {track}", roll.clip);
        egui::Window::new(title)
            .id(Id::new("piano roll"))
            .open(&mut open)
            .default_size([720.0, 420.0])
            .default_pos([260.0, 120.0])
            .show(ctx, |ui| self.piano_grid(ui, &mut roll, track));
        if open {
            self.piano = Some(roll);
        }
    }

    fn find_midi_clip(&self, id: u32) -> Option<usize> {
        self.session.project.tracks.iter().position(|t| t.as_ref().map_or(false, |t| t.midi_clips.iter().any(|c| c.id == id)))
    }

    fn piano_grid(&mut self, ui: &mut egui::Ui, roll: &mut PianoRoll, track: usize) {
        let Some(clip) = self.session.project.tracks[track].as_ref().and_then(|t| t.midi_clips.iter().find(|c| c.id == roll.clip)) else { return };
        let (clip_len, mut notes) = (clip.length_beats, clip.notes.clone());
        let grid = self.timeline.grid.min(1.0);
        let snapping = self.timeline.snapping && !ui.input(|i| i.modifiers.shift);

        let size = ui.available_size().max(Vec2::new(300.0, 200.0));
        let (area, painter) = ui.allocate_painter(size, Sense::hover());
        let area = area.rect;
        let grid_rect = Rect::from_min_max(Pos2::new(area.left() + KEYS_W, area.top()), area.max);
        roll.grid_rect = grid_rect;

        // Scroll: wheel moves through pitches, shift + wheel through time.
        if ui.rect_contains_pointer(area) {
            let (scroll, shift) = ui.input(|i| (i.raw_scroll_delta, i.modifiers.shift));
            if shift || scroll.x != 0.0 {
                let dx = if scroll.x != 0.0 { scroll.x } else { scroll.y };
                roll.scroll = (roll.scroll - (dx / roll.px_per_beat) as f64).max(0.0);
            } else if scroll.y != 0.0 {
                roll.top_pitch = (roll.top_pitch + (scroll.y / ROW_H).round() as i32).clamp(12, 127);
            }
        }
        let view = PianoView { origin: grid_rect.min, px_per_beat: roll.px_per_beat, scroll: roll.scroll, top_pitch: roll.top_pitch };
        roll.view = Some(view);
        let rows = (grid_rect.height() / ROW_H).ceil() as i32;

        // Rows: black keys darker, a label on every C.
        for k in 0..rows {
            let pitch = roll.top_pitch - k;
            let y = view.pitch_to_y(pitch);
            let row = Rect::from_min_max(Pos2::new(area.left(), y), Pos2::new(area.right(), y + ROW_H));
            let shade = if is_black(pitch) { 30 } else { 40 };
            painter.rect_filled(row, Rounding::ZERO, Color32::from_gray(shade));
            if pitch.rem_euclid(12) == 0 {
                painter.text(Pos2::new(area.left() + 4.0, y + ROW_H / 2.0), Align2::LEFT_CENTER, format!("C{}", pitch / 12 - 1), FontId::proportional(10.0), Color32::from_gray(200));
                painter.line_segment([Pos2::new(grid_rect.left(), y + ROW_H), Pos2::new(area.right(), y + ROW_H)], Stroke::new(1.0_f32, Color32::from_gray(60)));
            }
        }
        // Beat lines, and shade past the clip's end (notes are cut there).
        let mut beat = (view.x_to_beat(grid_rect.left()) / grid).floor() * grid;
        while view.beat_to_x(beat) < grid_rect.right() {
            let x = view.beat_to_x(beat);
            let strong = (beat % 4.0).abs() < 1e-9;
            painter.line_segment([Pos2::new(x, area.top()), Pos2::new(x, area.bottom())], Stroke::new(1.0_f32, Color32::from_gray(if strong { 75 } else { 52 })));
            beat += grid;
        }
        let end_x = view.beat_to_x(clip_len);
        if end_x < grid_rect.right() {
            painter.rect_filled(Rect::from_min_max(Pos2::new(end_x.max(grid_rect.left()), area.top()), area.max), Rounding::ZERO, Color32::from_black_alpha(110));
        }

        // Empty grid: click to add a note one grid step long.
        let bg = ui.interact(grid_rect, Id::new(("piano bg", roll.clip)), Sense::click());
        if bg.clicked() {
            if let Some(pos) = bg.interact_pointer_pos() {
                let start = snap(view.x_to_beat(pos.x).max(0.0), grid, snapping);
                let pitch = view.y_to_pitch(pos.y).clamp(0, 127) as u8;
                if start < clip_len {
                    let note = Note { start, length: grid.min(clip_len - start), pitch, velocity: 100 };
                    let r = self.session.edit_midi_clip(track, roll.clip, |c| c.notes.push(note));
                    self.report(r);
                    notes.push(note);
                }
            }
        }

        // Notes.
        roll.note_rects.clear();
        let clip_painter = painter.with_clip_rect(grid_rect);
        for (i, note) in notes.iter().enumerate() {
            let n = match roll.drag {
                Some(d) if d.index == i => d.preview,
                _ => *note,
            };
            let y = view.pitch_to_y(n.pitch as i32);
            let rect = Rect::from_min_max(Pos2::new(view.beat_to_x(n.start), y + 1.0), Pos2::new(view.beat_to_x(n.start + n.length), y + ROW_H - 1.0));
            if !rect.intersects(grid_rect) {
                continue;
            }
            roll.note_rects.push((i, rect));
            let brightness = 120 + (n.velocity as u32 * 135 / 127) as u8;
            clip_painter.rect_filled(rect, Rounding::same(2.0), Color32::from_rgb(brightness / 2, brightness, brightness / 3));

            let body = ui.interact(rect, Id::new(("note", roll.clip, i)), Sense::click_and_drag());
            let edge_rect = Rect::from_min_max(Pos2::new(rect.right() - EDGE_W, rect.top()), rect.right_bottom());
            let edge = ui.interact(edge_rect, Id::new(("note edge", roll.clip, i)), Sense::drag()).on_hover_cursor(CursorIcon::ResizeHorizontal);
            let (resp, mode) = if edge.dragged() || edge.drag_started() || edge.drag_stopped() { (edge, DragMode::ResizeEnd) } else { (body, DragMode::Move) };

            if resp.drag_started() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    roll.drag = Some(NoteDrag { index: i, mode, original: *note, grab: (view.x_to_beat(pos.x), view.y_to_pitch(pos.y)), preview: *note });
                }
            }
            if resp.dragged() {
                if let (Some(d), Some(pos)) = (roll.drag.as_mut(), resp.interact_pointer_pos()) {
                    let (beats, pitches) = (view.x_to_beat(pos.x) - d.grab.0, view.y_to_pitch(pos.y) - d.grab.1);
                    let pitches = if d.mode == DragMode::Move { pitches } else { 0 };
                    d.preview = note_drag(d.mode, d.original, beats, pitches, grid, snapping, clip_len);
                }
            }
            if resp.drag_stopped() {
                if let Some(d) = roll.drag.take() {
                    let r = self.session.edit_midi_clip(track, roll.clip, |c| {
                        if let Some(n) = c.notes.get_mut(d.index) {
                            *n = d.preview;
                        }
                    });
                    self.report(r);
                }
            }
            if resp.secondary_clicked() {
                let r = self.session.edit_midi_clip(track, roll.clip, |c| {
                    if i < c.notes.len() {
                        c.notes.remove(i);
                    }
                });
                self.report(r);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start: f64, length: f64, pitch: u8) -> Note {
        Note { start, length, pitch, velocity: 100 }
    }

    #[test]
    fn pitches_and_rows_round_trip() {
        let v = PianoView { origin: Pos2::new(40.0, 0.0), px_per_beat: 48.0, scroll: 0.0, top_pitch: 84 };
        assert_eq!(v.pitch_to_y(84), 0.0);
        assert_eq!(v.pitch_to_y(72), 12.0 * ROW_H);
        for p in [84, 70, 60, 36] {
            assert_eq!(v.y_to_pitch(v.pitch_to_y(p) + ROW_H / 2.0), p, "the middle of a row is that row's pitch");
        }
    }

    #[test]
    fn moving_a_note_snaps_and_stays_inside_the_clip() {
        let n = note(1.0, 1.0, 60);
        let moved = note_drag(DragMode::Move, n, 1.3, 5, 0.25, true, 4.0);
        assert_eq!((moved.start, moved.pitch, moved.length), (2.25, 65, 1.0));
        let late = note_drag(DragMode::Move, n, 10.0, 0, 0.25, true, 4.0);
        assert_eq!(late.start, 3.0, "can't run off the clip's end");
        let low = note_drag(DragMode::Move, n, 0.0, -100, 0.25, true, 4.0);
        assert_eq!(low.pitch, 0, "MIDI stops at 0");
    }

    #[test]
    fn resizing_a_note_keeps_its_start_and_pitch() {
        let n = note(1.0, 1.0, 60);
        let longer = note_drag(DragMode::ResizeEnd, n, 0.6, 3, 0.25, true, 4.0);
        assert_eq!((longer.start, longer.pitch, longer.length), (1.0, 60, 1.5));
        let capped = note_drag(DragMode::ResizeEnd, n, 50.0, 0, 0.25, true, 4.0);
        assert_eq!(capped.length, 3.0, "ends at the clip's end");
    }

    #[test]
    fn black_keys() {
        let black: Vec<i32> = (60..72).filter(|&p| is_black(p)).collect();
        assert_eq!(black, vec![61, 63, 66, 68, 70]);
    }
}
