//! Automation: parameter values that change over time, drawn as lines of
//! breakpoints on the timeline.
//!
//! It's evaluated on the AUDIO thread, from the transport position, every
//! processing window. The main thread could send `SetParam` from the UI
//! instead, but only as often as it draws a frame (~60 times a second),
//! with timing that jitters whenever the UI is busy, and not at all while
//! the window is minimized. Evaluated here, like clips and the sequencer,
//! it works out "what's the value at this beat?" each time rather than
//! remembering where it was, so seeks, loops and tempo changes just work.

use crate::processor::{ParamInfo, ParamKind};

/// What a lane controls on its track.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AutoTarget {
    Volume,
    Pan,
    Node { slot: usize, param: usize },
    Instrument { param: usize },
}

/// The track fader and pan, described like any other parameter.
pub const VOLUME_INFO: ParamInfo = ParamInfo::linear("volume", -60.0, 12.0, 0.0, "dB");
pub const PAN_INFO: ParamInfo = ParamInfo::linear("pan", -1.0, 1.0, 0.0, "");

/// Maps a parameter value onto 0..1, on the parameter's own scale: a log
/// parameter (frequency, time) is evenly spaced by ratio, so a line from
/// 100 Hz to 10 kHz passes 1 kHz halfway, as the ear expects.
pub fn normalize(p: &ParamInfo, value: f32) -> f32 {
    let v = value.clamp(p.min, p.max);
    let n = match p.kind {
        ParamKind::Log => (v / p.min).ln() / (p.max / p.min).ln(),
        _ => (v - p.min) / (p.max - p.min),
    };
    if n.is_finite() { n.clamp(0.0, 1.0) } else { 0.0 }
}

/// The inverse of `normalize`. Whole-number and on/off parameters step.
pub fn denormalize(p: &ParamInfo, n: f32) -> f32 {
    let n = n.clamp(0.0, 1.0);
    match p.kind {
        ParamKind::Log => p.min * (p.max / p.min).powf(n),
        ParamKind::Int => (p.min + (p.max - p.min) * n).round(),
        ParamKind::Toggle => if n >= 0.5 { 1.0 } else { 0.0 },
        ParamKind::Linear => p.min + (p.max - p.min) * n,
    }
}

/// Breakpoints (beat, normalized value), sorted by beat. Linear between
/// points; flat before the first and after the last. Two points at the
/// same beat make an instant step.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Envelope {
    pub points: Vec<(f64, f32)>,
}

impl Envelope {
    pub fn new(mut points: Vec<(f64, f32)>) -> Self {
        // A stable sort keeps the order of points at the same beat, so a
        // step goes the way it was drawn.
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        Self { points }
    }

    pub fn value_at(&self, beat: f64) -> Option<f32> {
        let p = &self.points;
        // The first point AFTER `beat`. Binary search, like everything else
        // the audio thread looks up on the timeline.
        let i = p.partition_point(|&(b, _)| b <= beat);
        match i {
            _ if p.is_empty() => None,
            0 => Some(p[0].1),
            i if i == p.len() => Some(p[i - 1].1),
            i => {
                let ((b0, v0), (b1, v1)) = (p[i - 1], p[i]);
                Some(v0 + (v1 - v0) * ((beat - b0) / (b1 - b0)) as f32)
            }
        }
    }
}

pub struct Lane {
    pub target: AutoTarget,
    pub info: ParamInfo,
    pub envelope: Envelope,
    /// The last value applied, so a flat stretch doesn't resend it.
    last: Option<f32>,
}

impl Lane {
    pub fn new(target: AutoTarget, info: ParamInfo, envelope: Envelope) -> Self {
        Self { target, info, envelope, last: None }
    }

    /// The parameter value at `beat`, if it differs from the last one
    /// applied. Real-time safe: a binary search and some arithmetic.
    pub fn changed_value_at(&mut self, beat: f64) -> Option<f32> {
        let v = denormalize(&self.info, self.envelope.value_at(beat)?);
        if self.last == Some(v) {
            return None;
        }
        self.last = Some(v);
        Some(v)
    }
}

/// All of a track's lanes: a snapshot, rebuilt and swapped on every edit,
/// like clip lists and sequences.
#[derive(Default)]
pub struct AutomationSet {
    pub lanes: Vec<Lane>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_interpolates_and_holds() {
        let e = Envelope::new(vec![(4.0, 1.0), (0.0, 0.0), (8.0, 0.5)]);
        assert_eq!(e.value_at(-1.0), Some(0.0), "flat before the first point");
        assert_eq!(e.value_at(2.0), Some(0.5));
        assert_eq!(e.value_at(6.0), Some(0.75));
        assert_eq!(e.value_at(100.0), Some(0.5), "flat after the last");
        assert_eq!(Envelope::default().value_at(1.0), None);
    }

    #[test]
    fn two_points_at_one_beat_make_a_step() {
        let e = Envelope::new(vec![(0.0, 0.0), (4.0, 0.0), (4.0, 1.0), (8.0, 1.0)]);
        assert!(e.value_at(3.999).unwrap() < 0.01);
        assert_eq!(e.value_at(4.0), Some(1.0));
    }

    #[test]
    fn normalization_follows_the_parameter_scale() {
        let freq = ParamInfo::log("freq", 100.0, 10_000.0, 1_000.0, "Hz");
        assert!((normalize(&freq, 1_000.0) - 0.5).abs() < 1e-6, "1 kHz is halfway on a log scale");
        assert!((denormalize(&freq, 0.5) - 1_000.0).abs() < 0.01);
        let db = VOLUME_INFO;
        assert!((denormalize(&db, normalize(&db, -6.0)) - -6.0).abs() < 1e-4);
        let kind = ParamInfo::int("type", 0.0, 6.0, 0.0);
        assert_eq!(denormalize(&kind, 0.55), 3.0, "whole numbers step");
        assert_eq!(denormalize(&ParamInfo::toggle("on", false), 0.7), 1.0);
    }

    #[test]
    fn unchanged_values_are_not_resent() {
        let mut lane = Lane::new(AutoTarget::Volume, VOLUME_INFO, Envelope::new(vec![(0.0, 0.5), (4.0, 0.5)]));
        assert!(lane.changed_value_at(0.0).is_some());
        assert!(lane.changed_value_at(1.0).is_none(), "flat: nothing to do");
    }
}
