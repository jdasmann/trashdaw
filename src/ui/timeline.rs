//! The arrangement view: a ruler, one lane per track, clips you can drag
//! and resize, and the playhead.

use super::DawApp;
use crate::automation::AutoTarget;
use crate::loader::AudioData;
use crate::mixer::{Target, MAX_TRACKS};
use crate::transport::BEATS_PER_BAR;
use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Id, Pos2, Rect, Rounding, Sense, Stroke, Ui, Vec2};
use std::collections::HashMap;
use std::sync::Arc;

pub const RULER_H: f32 = 22.0;
pub const LANE_H: f32 = 64.0;
pub const HEADER_W: f32 = 120.0;
/// Width of the grab zone on a clip's right edge, for resizing.
const EDGE_W: f32 = 6.0;

// ---------------------------------------------------------------------
// Pure math: tested without a window.
// ---------------------------------------------------------------------

/// Maps between beats and screen x. `origin` is where beat `scroll`
/// appears on screen.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub origin: f32,
    pub scroll: f64,
    pub px_per_beat: f32,
}

impl View {
    pub fn beat_to_x(&self, beat: f64) -> f32 {
        self.origin + ((beat - self.scroll) as f32) * self.px_per_beat
    }

    pub fn x_to_beat(&self, x: f32) -> f64 {
        self.scroll + ((x - self.origin) / self.px_per_beat) as f64
    }
}

pub fn snap(beat: f64, grid: f64, enabled: bool) -> f64 {
    if enabled && grid > 0.0 {
        (beat / grid).round() * grid
    } else {
        beat
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DragMode {
    Move,
    ResizeEnd,
}

/// Where a clip ends up after dragging it by `delta` beats.
/// Returns (start, length).
pub fn drag_result(mode: DragMode, start: f64, len: f64, delta: f64, grid: f64, snapping: bool) -> (f64, f64) {
    match mode {
        DragMode::Move => (snap(start + delta, grid, snapping).max(0.0), len),
        DragMode::ResizeEnd => {
            let min_len = if snapping { grid } else { 1.0 / 64.0 };
            let end = snap(start + len + delta, grid, snapping);
            (start, (end - start).max(min_len))
        }
    }
}

/// A waveform overview: (min, max) of each block of `FRAMES_PER_BUCKET`
/// frames of the first channel. Computed once per file, so drawing never
/// has to scan millions of samples.
pub const FRAMES_PER_BUCKET: usize = 256;

pub fn overview(data: &AudioData) -> Vec<(f32, f32)> {
    data.channels
        .first()
        .map(|ch| {
            ch.chunks(FRAMES_PER_BUCKET)
                .map(|b| b.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &s| (lo.min(s), hi.max(s))))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------
// State and drawing
// ---------------------------------------------------------------------

/// Height of an automation lane under its track.
pub const AUTO_H: f32 = 52.0;
/// Hit radius for automation points, in pixels.
const POINT_R: f32 = 5.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RowKind {
    Clips,
    /// The track's automation lane with this index.
    Automation(usize),
}

#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub track: usize,
    pub kind: RowKind,
    pub top: f32,
    pub height: f32,
}

/// Stacks the rows: each track's clip lane, then its automation lane if
/// one is shown. `tracks` is (track, shown lane). The same layout decides
/// where things are drawn AND which track the pointer is over.
pub fn layout(tracks: &[(usize, Option<usize>)], top: f32) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut y = top;
    for &(track, lane) in tracks {
        rows.push(Row { track, kind: RowKind::Clips, top: y, height: LANE_H });
        y += LANE_H;
        if let Some(lane) = lane {
            rows.push(Row { track, kind: RowKind::Automation(lane), top: y, height: AUTO_H });
            y += AUTO_H;
        }
    }
    rows
}

/// The track whose rows contain `y`.
pub fn track_at(rows: &[Row], y: f32) -> Option<usize> {
    rows.iter().find(|r| y >= r.top && y < r.top + r.height).map(|r| r.track)
}

/// Normalized automation value (0..1) to y within a lane, and back. A
/// few pixels of padding keep the extremes grabbable.
pub fn value_to_y(n: f32, rect: Rect) -> f32 {
    rect.bottom() - 4.0 - n * (rect.height() - 8.0)
}

pub fn y_to_value(y: f32, rect: Rect) -> f32 {
    ((rect.bottom() - 4.0 - y) / (rect.height() - 8.0)).clamp(0.0, 1.0)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ClipRef {
    Audio { track: usize, id: u32 },
    Midi { track: usize, id: u32 },
}

impl ClipRef {
    fn track(&self) -> usize {
        match *self {
            ClipRef::Audio { track, .. } | ClipRef::Midi { track, .. } => track,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    clip: ClipRef,
    mode: DragMode,
    start: f64,
    len: f64,
    grab_beat: f64,
    preview: (f64, f64),
    /// Where the clip would land if released now (vertical moves).
    preview_track: usize,
}

/// Dragging one automation point. The whole lane is copied, edited
/// locally while dragging, and committed once on release.
#[derive(Clone, Debug)]
struct PointDrag {
    track: usize,
    lane: usize,
    index: usize,
    points: Vec<(f64, f32)>,
}

pub struct TimelineState {
    pub px_per_beat: f32,
    pub scroll: f64,
    /// Snap grid in beats.
    pub grid: f64,
    pub snapping: bool,
    pub follow: bool,
    drag: Option<Drag>,
    point_drag: Option<PointDrag>,
    /// Which automation lane each track shows, if any.
    pub shown_lane: HashMap<usize, usize>,
    /// Where things were drawn last frame: for tests, and for anything that
    /// needs to know what's under the pointer.
    pub clip_rects: Vec<(ClipRef, Rect)>,
    pub rows: Vec<Row>,
    waveforms: HashMap<usize, Arc<Vec<(f32, f32)>>>,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            px_per_beat: 24.0,
            scroll: 0.0,
            grid: 1.0,
            snapping: true,
            follow: true,
            drag: None,
            point_drag: None,
            shown_lane: HashMap::new(),
            clip_rects: Vec::new(),
            rows: Vec::new(),
            waveforms: HashMap::new(),
        }
    }
}

impl TimelineState {
    /// The cached overview for a file, computed the first time it's needed.
    /// Keyed by the Arc's address: the same file shares one overview.
    fn waveform(&mut self, data: &Arc<AudioData>) -> Arc<Vec<(f32, f32)>> {
        let key = Arc::as_ptr(data) as usize;
        self.waveforms.entry(key).or_insert_with(|| Arc::new(overview(data))).clone()
    }
}

const AUDIO_CLIP: Color32 = Color32::from_rgb(52, 101, 164);
const MIDI_CLIP: Color32 = Color32::from_rgb(96, 140, 72);
const PLAYHEAD: Color32 = Color32::from_rgb(230, 80, 60);
const AUTO_LINE: Color32 = Color32::from_rgb(230, 170, 60);

/// One clip to draw: where it is now, or where a drag would put it.
struct Item {
    clip: ClipRef,
    track: usize,
    start: f64,
    len: f64,
    label: String,
}

impl DawApp {
    pub(super) fn timeline_ui(&mut self, ui: &mut Ui) {
        // Which lane each track shows; forget lanes that no longer exist
        // (an effect was removed, taking its lanes with it).
        let tracks: Vec<(usize, Option<usize>)> = (0..MAX_TRACKS)
            .filter_map(|i| {
                let t = self.session.project.tracks[i].as_ref()?;
                let lane = self.timeline.shown_lane.get(&i).copied().filter(|&l| l < t.automation.len());
                Some((i, lane))
            })
            .collect();
        let rows_height: f32 = tracks.iter().map(|(_, l)| LANE_H + if l.is_some() { AUTO_H } else { 0.0 }).sum();
        let height = (RULER_H + rows_height).max(ui.available_height());
        let (area, painter) = ui.allocate_painter(Vec2::new(ui.available_width(), height), Sense::hover());
        let area = area.rect;
        painter.rect_filled(area, Rounding::ZERO, Color32::from_gray(28));

        self.zoom_and_scroll(ui, area);
        let view = View { origin: area.left() + HEADER_W, scroll: self.timeline.scroll, px_per_beat: self.timeline.px_per_beat };
        let tempo = self.session.tempo();
        let playhead = self.session.current_beat();
        let snapping = self.timeline.snapping && !ui.input(|i| i.modifiers.shift);

        // Keep the playhead in view while playing.
        if self.timeline.follow && self.session.playing() {
            let right = view.x_to_beat(area.right() - 40.0);
            if playhead > right || playhead < self.timeline.scroll {
                self.timeline.scroll = (playhead - 2.0).max(0.0);
            }
        }

        self.ruler_ui(ui, &painter, &view, area, snapping);

        let rows = layout(&tracks, area.top() + RULER_H);
        self.timeline.rows = rows.clone();
        self.timeline.clip_rects.clear();

        for row in &rows {
            let rect = Rect::from_min_max(Pos2::new(view.origin, row.top), Pos2::new(area.right(), row.top + row.height));
            let header = Rect::from_min_max(Pos2::new(area.left(), row.top), Pos2::new(view.origin, row.top + row.height));
            match row.kind {
                RowKind::Clips => self.clip_row(ui, &painter, &view, rect, header, row.track, snapping),
                RowKind::Automation(lane) => self.automation_row(ui, &painter, &view, rect, header, row.track, lane, snapping),
            }
        }
        self.draw_clips(ui, &painter, &view, &rows, area, tempo, snapping);

        // --- Playhead ---
        let x = view.beat_to_x(playhead);
        if x >= view.origin {
            painter.line_segment([Pos2::new(x, area.top()), Pos2::new(x, area.bottom())], Stroke::new(1.5_f32, PLAYHEAD));
        }
        if tracks.is_empty() {
            painter.text(area.center(), Align2::CENTER_CENTER, "Add a track with \"+ Track\" above", FontId::proportional(15.0), Color32::from_gray(140));
        }
    }

    /// The ruler: bar numbers and the loop range; click or drag to seek.
    fn ruler_ui(&mut self, ui: &mut Ui, painter: &egui::Painter, view: &View, area: Rect, snapping: bool) {
        let ruler = Rect::from_min_max(Pos2::new(view.origin, area.top()), Pos2::new(area.right(), area.top() + RULER_H));
        painter.rect_filled(ruler, Rounding::ZERO, Color32::from_gray(40));
        if let Some((a, b)) = self.session.loop_range {
            let r = Rect::from_x_y_ranges(view.beat_to_x(a).max(view.origin)..=view.beat_to_x(b), ruler.y_range());
            painter.rect_filled(r, Rounding::ZERO, Color32::from_rgba_unmultiplied(220, 170, 60, 70));
        }
        let first_bar = (view.x_to_beat(view.origin) / BEATS_PER_BAR).floor().max(0.0) as i64;
        let last_bar = (view.x_to_beat(area.right()) / BEATS_PER_BAR).ceil() as i64;
        // Thin out bar labels when zoomed out.
        let label_every = (60.0 / (self.timeline.px_per_beat * BEATS_PER_BAR as f32)).ceil().max(1.0) as i64;
        for bar in first_bar..=last_bar {
            let x = view.beat_to_x(bar as f64 * BEATS_PER_BAR);
            painter.line_segment([Pos2::new(x, ruler.top()), Pos2::new(x, area.bottom())], Stroke::new(1.0_f32, Color32::from_gray(48)));
            if bar % label_every == 0 {
                painter.text(Pos2::new(x + 3.0, ruler.center().y), Align2::LEFT_CENTER, format!("{}", bar + 1), FontId::proportional(11.0), Color32::from_gray(170));
            }
        }
        let resp = ui.interact(ruler, Id::new("ruler"), Sense::click_and_drag());
        if resp.clicked() || resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let beat = snap(view.x_to_beat(pos.x).max(0.0), self.timeline.grid, snapping);
                let r = self.session.seek(beat);
                self.report(r);
            }
        }
    }

    /// A track's clip lane: the header, and the empty background.
    #[allow(clippy::too_many_arguments)]
    fn clip_row(&mut self, ui: &mut Ui, painter: &egui::Painter, view: &View, lane: Rect, header: Rect, track: usize, snapping: bool) {
        let selected = self.selected == Target::Track(track);
        painter.rect_filled(header, Rounding::ZERO, if selected { Color32::from_gray(60) } else { Color32::from_gray(38) });
        painter.line_segment([lane.left_bottom(), lane.right_bottom()], Stroke::new(1.0_f32, Color32::from_gray(50)));
        let name = self.session.project.tracks[track].as_ref().map(|t| t.name.clone()).unwrap_or_default();
        painter.text(header.left_center() + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, &name, FontId::proportional(13.0), Color32::from_gray(220));
        if ui.interact(header, Id::new(("header", track)), Sense::click()).clicked() {
            self.selected = Target::Track(track);
        }
        // Empty lane: double-click to add a clip there.
        let resp = ui.interact(lane, Id::new(("lane", track)), Sense::click());
        if resp.clicked() {
            self.selected = Target::Track(track);
        }
        if resp.double_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let beat = snap(view.x_to_beat(pos.x).max(0.0), BEATS_PER_BAR, snapping).max(0.0);
                self.add_clip_at(track, beat);
            }
        }
    }

    /// Double-clicking an empty lane: place the selected sample there, or
    /// an empty one-bar MIDI clip if the track has an instrument.
    fn add_clip_at(&mut self, track: usize, beat: f64) {
        let has_instrument = self.session.project.tracks[track].as_ref().map_or(false, |t| t.instrument.is_some());
        let r = match self.selected_sample {
            Some(sample) => self.session.add_clip(track, sample, beat).map(|_| ()),
            None if has_instrument => self.session.add_midi_clip(track, beat, BEATS_PER_BAR, Vec::new()).map(|_| ()),
            None => Err("select a sample in the library first (or add a synth to this track for MIDI)".into()),
        };
        self.report(r);
    }

    fn zoom_and_scroll(&mut self, ui: &Ui, area: Rect) {
        if !ui.rect_contains_pointer(area) {
            return;
        }
        let (zoom, scroll, pointer) = ui.input(|i| (i.zoom_delta(), i.raw_scroll_delta, i.pointer.hover_pos()));
        let t = &mut self.timeline;
        if zoom != 1.0 {
            // Zoom around the pointer, so what's under it stays put.
            let origin = area.left() + HEADER_W;
            let anchor_x = pointer.map_or(origin, |p| p.x.max(origin));
            let before = View { origin, scroll: t.scroll, px_per_beat: t.px_per_beat }.x_to_beat(anchor_x);
            t.px_per_beat = (t.px_per_beat * zoom).clamp(2.0, 400.0);
            t.scroll = (before - ((anchor_x - origin) / t.px_per_beat) as f64).max(0.0);
        }
        // Horizontal wheel (or shift + wheel) scrolls the timeline.
        let dx = if scroll.x != 0.0 { scroll.x } else if ui.input(|i| i.modifiers.shift) { scroll.y } else { 0.0 };
        if dx != 0.0 {
            t.scroll = (t.scroll - (dx / t.px_per_beat) as f64).max(0.0);
        }
    }

    /// Every clip on every track, drawn in its lane. A clip being dragged
    /// is drawn where it would land, which may be another track's lane.
    #[allow(clippy::too_many_arguments)]
    fn draw_clips(&mut self, ui: &mut Ui, painter: &egui::Painter, view: &View, rows: &[Row], area: Rect, tempo: f64, snapping: bool) {
        let mut items = Vec::new();
        for row in rows.iter().filter(|r| r.kind == RowKind::Clips) {
            let Some(info) = self.session.project.tracks[row.track].as_ref() else { continue };
            let track = row.track;
            for c in &info.clips {
                let clip = ClipRef::Audio { track, id: c.params.id };
                items.push(Item { clip, track, start: c.params.start_beat, len: c.params.length_beats, label: c.data.name.clone() });
            }
            for c in &info.midi_clips {
                let clip = ClipRef::Midi { track, id: c.id };
                items.push(Item { clip, track, start: c.start_beat, len: c.length_beats, label: format!("MIDI #{}", c.id) });
            }
        }

        for mut item in items {
            if let Some(d) = self.timeline.drag.filter(|d| d.clip == item.clip) {
                (item.start, item.len) = d.preview;
                item.track = d.preview_track;
            }
            let Some(row) = rows.iter().find(|r| r.track == item.track && r.kind == RowKind::Clips) else { continue };
            let lane = Rect::from_min_max(Pos2::new(view.origin, row.top), Pos2::new(area.right(), row.top + row.height));
            let x0 = view.beat_to_x(item.start);
            let x1 = view.beat_to_x(item.start + item.len);
            if x1 < lane.left() || x0 > lane.right() {
                continue;
            }
            let rect = Rect::from_min_max(Pos2::new(x0.max(lane.left()), lane.top() + 2.0), Pos2::new(x1.min(lane.right()), lane.bottom() - 2.0));
            self.timeline.clip_rects.push((item.clip, rect));
            let color = match item.clip {
                ClipRef::Audio { .. } => AUDIO_CLIP,
                ClipRef::Midi { .. } => MIDI_CLIP,
            };
            painter.rect_filled(rect, Rounding::same(3.0), color);
            self.draw_clip_contents(painter, item.clip, rect, x0, view.px_per_beat, tempo);
            painter.text(rect.left_top() + Vec2::new(4.0, 2.0), Align2::LEFT_TOP, &item.label, FontId::proportional(11.0), Color32::WHITE);
            self.clip_interaction(ui, view, rows, item.clip, rect, item.start, item.len, snapping);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn clip_interaction(&mut self, ui: &mut Ui, view: &View, rows: &[Row], clip: ClipRef, rect: Rect, start: f64, len: f64, snapping: bool) {
        // The Id is the ORIGINAL clip, so the drag stays the same widget
        // even while the clip is drawn in another track's lane.
        let body = ui.interact(rect, Id::new(("clip", clip)), Sense::click_and_drag());
        let edge_rect = Rect::from_min_max(Pos2::new(rect.right() - EDGE_W, rect.top()), rect.right_bottom());
        let edge = ui.interact(edge_rect, Id::new(("edge", clip)), Sense::drag()).on_hover_cursor(CursorIcon::ResizeHorizontal);
        let (resp, mode) = if edge.dragged() || edge.drag_started() || edge.drag_stopped() {
            (edge, DragMode::ResizeEnd)
        } else {
            (body, DragMode::Move)
        };

        if resp.drag_started() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let track = clip.track();
                self.timeline.drag = Some(Drag { clip, mode, start, len, grab_beat: view.x_to_beat(pos.x), preview: (start, len), preview_track: track });
            }
        }
        if resp.dragged() {
            if let (Some(d), Some(pos)) = (self.timeline.drag.as_mut(), resp.interact_pointer_pos()) {
                if d.clip == clip {
                    let delta = view.x_to_beat(pos.x) - d.grab_beat;
                    d.preview = drag_result(d.mode, d.start, d.len, delta, self.timeline.grid, snapping);
                    // Moving (not resizing) can also change tracks.
                    if d.mode == DragMode::Move {
                        if let Some(track) = track_at(rows, pos.y) {
                            d.preview_track = track;
                        }
                    }
                }
            }
        }
        if resp.drag_stopped() {
            if let Some(d) = self.timeline.drag.take().filter(|d| d.clip == clip) {
                self.commit_clip(clip, d.preview, d.preview_track);
            }
        }
        if resp.clicked() {
            self.selected = Target::Track(clip.track());
        }
        if resp.double_clicked() {
            if let ClipRef::Midi { track, id } = clip {
                self.piano = Some(super::pianoroll::PianoRoll::new(track, id));
            }
        }
        let playhead = self.session.current_beat();
        resp.context_menu(|ui| {
            if let ClipRef::Midi { track, id } = clip {
                if ui.button("Edit notes").clicked() {
                    self.piano = Some(super::pianoroll::PianoRoll::new(track, id));
                    ui.close_menu();
                }
            }
            if ui.button("Split at playhead").clicked() {
                if let ClipRef::Audio { track, id } = clip {
                    let r = self.session.split_clip(track, id, playhead).map(|_| ());
                    self.report(r);
                }
                ui.close_menu();
            }
            if ui.button("Delete").clicked() {
                let r = match clip {
                    ClipRef::Audio { track, id } => self.session.remove_clip(track, id),
                    ClipRef::Midi { track, id } => self.session.remove_midi_clip(track, id),
                };
                self.report(r);
                ui.close_menu();
            }
        });
    }

    /// Sends the finished drag to the engine, once. A move to another
    /// track is a single session action.
    fn commit_clip(&mut self, clip: ClipRef, (start, len): (f64, f64), to: usize) {
        let r = match clip {
            ClipRef::Audio { track, id } if to != track => self.session.move_clip(track, id, to, start),
            ClipRef::Midi { track, id } if to != track => self.session.move_midi_clip(track, id, to, start),
            ClipRef::Audio { track, id } => self.session.edit_clip(track, id, |p| {
                p.start_beat = start;
                p.length_beats = len;
            }),
            ClipRef::Midi { track, id } => self.session.edit_midi_clip(track, id, |c| {
                c.start_beat = start;
                c.length_beats = len;
            }),
        };
        if self.report(r).is_some() && to != clip.track() {
            self.selected = Target::Track(to);
        }
    }

    /// An automation lane: the line, its points, and editing.
    /// Click empty space to add a point, drag a point to move it,
    /// right-click a point to delete it.
    #[allow(clippy::too_many_arguments)]
    fn automation_row(&mut self, ui: &mut Ui, painter: &egui::Painter, view: &View, rect: Rect, header: Rect, track: usize, lane: usize, snapping: bool) {
        let Some(info) = self.session.project.tracks[track].as_ref() else { return };
        let Some(lane_info) = info.automation.get(lane) else { return };
        let target = lane_info.target;
        let label = self.lane_label(track, target);
        // While a point is being dragged, draw the locally edited copy.
        let points = match &self.timeline.point_drag {
            Some(d) if d.track == track && d.lane == lane => d.points.clone(),
            _ => lane_info.points.clone(),
        };

        painter.rect_filled(header, Rounding::ZERO, Color32::from_gray(33));
        painter.text(header.left_center() + Vec2::new(14.0, 0.0), Align2::LEFT_CENTER, format!("↳ {label}"), FontId::proportional(11.0), AUTO_LINE);
        painter.rect_filled(rect, Rounding::ZERO, Color32::from_gray(24));
        painter.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0_f32, Color32::from_gray(50)));

        // The line: flat before the first point and after the last.
        let to_screen = |&(b, v): &(f64, f32)| Pos2::new(view.beat_to_x(b), value_to_y(v, rect));
        let mut line: Vec<Pos2> = points.iter().map(to_screen).collect();
        if let (Some(first), Some(last)) = (line.first().copied(), line.last().copied()) {
            line.insert(0, Pos2::new(rect.left(), first.y));
            line.push(Pos2::new(rect.right(), last.y));
        }
        let clip_painter = painter.with_clip_rect(rect);
        for w in line.windows(2) {
            clip_painter.line_segment([w[0], w[1]], Stroke::new(1.5_f32, AUTO_LINE));
        }

        // Background first, points after, so a point wins under the pointer.
        let bg = ui.interact(rect, Id::new(("auto bg", track, lane)), Sense::click());
        if bg.clicked() {
            if let Some(pos) = bg.interact_pointer_pos() {
                let mut new = points.clone();
                new.push((snap(view.x_to_beat(pos.x).max(0.0), self.timeline.grid, snapping), y_to_value(pos.y, rect)));
                let r = self.session.set_automation_points(track, lane, new);
                self.report(r);
            }
        }
        for (i, p) in points.iter().enumerate() {
            let center = to_screen(p);
            if !rect.contains(center) {
                continue;
            }
            clip_painter.circle_filled(center, POINT_R - 1.0, AUTO_LINE);
            let hit = Rect::from_center_size(center, Vec2::splat(POINT_R * 2.0));
            let resp = ui.interact(hit, Id::new(("point", track, lane, i)), Sense::click_and_drag());
            if resp.drag_started() {
                self.timeline.point_drag = Some(PointDrag { track, lane, index: i, points: points.clone() });
            }
            if resp.dragged() {
                if let (Some(d), Some(pos)) = (self.timeline.point_drag.as_mut(), resp.interact_pointer_pos()) {
                    d.points[d.index] = (snap(view.x_to_beat(pos.x).max(0.0), self.timeline.grid, snapping), y_to_value(pos.y, rect));
                }
            }
            if resp.drag_stopped() {
                if let Some(d) = self.timeline.point_drag.take() {
                    let r = self.session.set_automation_points(d.track, d.lane, d.points);
                    self.report(r);
                }
            }
            if resp.secondary_clicked() {
                let mut new = points.clone();
                new.remove(i);
                let r = self.session.set_automation_points(track, lane, new);
                self.report(r);
            }
        }
    }

    /// "volume", "synth: cutoff", "1 delay: feedback"...
    pub(super) fn lane_label(&self, track: usize, target: AutoTarget) -> String {
        let t = self.session.project.tracks[track].as_ref();
        match target {
            AutoTarget::Volume => "volume".into(),
            AutoTarget::Pan => "pan".into(),
            AutoTarget::Instrument { param } => {
                let name = t.and_then(|t| t.instrument.as_ref()).and_then(|i| i.params.get(param)).map_or("?", |p| p.name);
                format!("synth: {name}")
            }
            AutoTarget::Node { slot, param } => {
                let node = t.and_then(|t| t.slots.get(slot)).and_then(|s| s.as_ref());
                format!("{slot} {}: {}", node.map_or("?", |n| n.name), node.and_then(|n| n.params.get(param)).map_or("?", |p| p.name))
            }
        }
    }

    /// A waveform for audio clips, little note bars for MIDI clips.
    fn draw_clip_contents(&mut self, painter: &egui::Painter, clip: ClipRef, rect: Rect, clip_x0: f32, ppb: f32, tempo: f64) {
        let inner = rect.shrink2(Vec2::new(1.0, 14.0)).translate(Vec2::new(0.0, 6.0));
        match clip {
            ClipRef::Audio { track, id } => {
                let Some(c) = self.session.clip(track, id) else { return };
                let (data, offset) = (c.data.clone(), c.params.offset_secs);
                let wave = self.timeline.waveform(&data);
                let mid = inner.center().y;
                let half = inner.height() / 2.0;
                let buckets_per_sec = data.sample_rate / FRAMES_PER_BUCKET as f64;
                let mut x = inner.left();
                while x < inner.right() {
                    // File time at this pixel and the next.
                    let secs = |px: f32| offset + ((px - clip_x0) / ppb) as f64 * 60.0 / tempo;
                    let (b0, b1) = ((secs(x) * buckets_per_sec) as usize, (secs(x + 1.0) * buckets_per_sec).ceil() as usize);
                    let (lo, hi) = wave
                        .get(b0..b1.max(b0 + 1).min(wave.len()))
                        .unwrap_or(&[])
                        .iter()
                        .fold((0.0f32, 0.0f32), |(lo, hi), &(a, b)| (lo.min(a), hi.max(b)));
                    if hi > lo {
                        painter.line_segment(
                            [Pos2::new(x, mid - hi.min(1.0) * half), Pos2::new(x, mid - lo.max(-1.0) * half)],
                            Stroke::new(1.0_f32, Color32::from_rgb(170, 200, 240)),
                        );
                    }
                    x += 1.0;
                }
            }
            ClipRef::Midi { track, id } => {
                let Some(c) = self.session.project.tracks[track].as_ref().and_then(|t| t.midi_clips.iter().find(|c| c.id == id)) else { return };
                if c.notes.is_empty() {
                    return;
                }
                let lo = c.notes.iter().map(|n| n.pitch).min().unwrap_or(60) as f32;
                let hi = c.notes.iter().map(|n| n.pitch).max().unwrap_or(60) as f32;
                let span = (hi - lo).max(12.0);
                for n in c.notes.iter().filter(|n| n.start < c.length_beats) {
                    let x0 = clip_x0 + n.start as f32 * ppb;
                    let x1 = clip_x0 + (n.start + n.length).min(c.length_beats) as f32 * ppb;
                    let y = inner.bottom() - (n.pitch as f32 - lo) / span * inner.height();
                    let r = Rect::from_min_max(Pos2::new(x0, y - 1.5), Pos2::new(x1.max(x0 + 1.0), y + 1.5)).intersect(inner);
                    if r.is_positive() {
                        painter.rect_filled(r, Rounding::ZERO, Color32::from_rgb(200, 235, 170));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beats_and_pixels_round_trip() {
        let v = View { origin: 120.0, scroll: 8.0, px_per_beat: 24.0 };
        assert_eq!(v.beat_to_x(8.0), 120.0);
        assert_eq!(v.beat_to_x(10.0), 168.0);
        for beat in [0.0, 3.25, 8.0, 100.5] {
            assert!((v.x_to_beat(v.beat_to_x(beat)) - beat).abs() < 1e-4);
        }
    }

    #[test]
    fn dragging_snaps_and_never_goes_negative() {
        assert_eq!(drag_result(DragMode::Move, 4.0, 2.0, 1.3, 1.0, true), (5.0, 2.0));
        assert_eq!(drag_result(DragMode::Move, 4.0, 2.0, 1.3, 1.0, false), (5.3, 2.0));
        assert_eq!(drag_result(DragMode::Move, 1.0, 2.0, -5.0, 1.0, true), (0.0, 2.0));
    }

    #[test]
    fn resizing_snaps_the_end_and_keeps_a_minimum() {
        assert_eq!(drag_result(DragMode::ResizeEnd, 4.0, 2.0, 0.6, 1.0, true), (4.0, 3.0));
        assert_eq!(drag_result(DragMode::ResizeEnd, 4.0, 2.0, -10.0, 1.0, true), (4.0, 1.0), "at least one grid step");
        let (_, len) = drag_result(DragMode::ResizeEnd, 4.0, 2.0, -10.0, 1.0, false);
        assert!(len > 0.0 && len < 0.1, "free resize: tiny but positive");
    }

    #[test]
    fn overview_keeps_the_extremes() {
        let mut ch = vec![0.0f32; 1000];
        ch[10] = 0.9;
        ch[300] = -0.7;
        let data = AudioData { name: "x".into(), sample_rate: 48_000.0, channels: vec![ch] };
        let o = overview(&data);
        assert_eq!(o.len(), 4); // 1000 / 256, rounded up
        assert_eq!(o[0], (0.0, 0.9));
        assert_eq!(o[1], (-0.7, 0.0));
    }

    #[test]
    fn layout_stacks_lanes_and_finds_tracks() {
        let rows = layout(&[(0, None), (3, Some(1)), (5, None)], 100.0);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].kind, RowKind::Clips);
        assert_eq!(rows[2].kind, RowKind::Automation(1));
        assert_eq!(rows[3].top, 100.0 + 2.0 * LANE_H + AUTO_H, "below two clip lanes and one automation lane");
        assert_eq!(track_at(&rows, 100.0 + LANE_H + 1.0), Some(3));
        assert_eq!(track_at(&rows, 100.0 + 2.0 * LANE_H + 1.0), Some(3), "the automation lane belongs to its track");
        assert_eq!(track_at(&rows, 99.0), None);
    }

    #[test]
    fn automation_values_and_y_round_trip() {
        let r = Rect::from_min_size(Pos2::new(0.0, 100.0), Vec2::new(500.0, 52.0));
        for v in [0.0, 0.25, 1.0] {
            assert!((y_to_value(value_to_y(v, r), r) - v).abs() < 1e-5);
        }
        assert_eq!(y_to_value(0.0, r), 1.0, "clamped above the lane");
    }
}
