//! Tracks, the aux and master buses, and metering.

use crate::automation::{AutoTarget, AutomationSet};
use crate::buffer::AudioBuffer;
use crate::clip::ClipList;
use crate::midi::{EventBuffer, MidiMessage, Sequence, Sequencer};
use crate::nodes::{db_to_linear, pan_gains};
use crate::processor::{Instrument, PrepareInfo, Processor};
use crate::smoothing::Smoothed;
use crate::transport::ProcessContext;
use std::f32::consts::SQRT_2;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

pub const MAX_TRACKS: usize = 16;
pub const MAX_SLOTS: usize = 16;
/// The whole mix is stereo internally, whatever the device has.
pub const MIX_CHANNELS: usize = 2;

/// Where a command is aimed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Target {
    Master,
    Aux,
    Track(usize),
}

/// Peaks from one track render.
pub struct TrackPeaks {
    /// After the effect chain, before volume/balance/mute.
    pub pre: f32,
    /// What the track actually contributes to the mix.
    pub post: f32,
}

/// Running peak. `f32::max` silently ignores NaN, which would make a
/// broken track look silent on its meter; count NaN as infinitely loud.
#[inline]
fn peak_with(peak: f32, s: f32) -> f32 {
    if s.is_nan() { f32::INFINITY } else { peak.max(s.abs()) }
}

fn db_to_gain(db: f32) -> f32 {
    let db = db.clamp(-60.0, 12.0);
    if db <= -60.0 { 0.0 } else { db_to_linear(db) }
}

/// A channel strip: clips and an instrument, then an effect chain, then volume, balance, mute, solo,
/// and a send to the aux bus. The aux and master buses are `Track`s too;
/// they just ignore pan/mute/solo/send.
pub struct Track {
    /// The track's audio source: clips on the timeline.
    clips: Box<ClipList>,
    /// The track's MIDI source: an instrument played by the sequence
    /// (MIDI clips) and, if the track is armed, live input.
    instrument: Option<Box<dyn Instrument>>,
    sequencer: Sequencer,
    /// Reused every block for this track's MIDI events.
    events: EventBuffer,
    automation: Box<AutomationSet>,
    slots: Vec<Option<Box<dyn Processor>>>,
    /// The track's own working buffer, so its chain can't affect others.
    buffer: AudioBuffer,
    volume: Smoothed,
    pan: Smoothed,
    /// Ramps between 0 and 1 for mute/solo, so they don't click.
    audible: Smoothed,
    /// Post-fader send level to the aux bus. Starts at 0 (off).
    send: Smoothed,
    pub mute: bool,
    pub solo: bool,
}

impl Track {
    /// Runs on the main thread: allocates the buffer and slots.
    pub fn new(info: &PrepareInfo) -> Self {
        let mut slots = Vec::with_capacity(MAX_SLOTS);
        slots.resize_with(MAX_SLOTS, || None);
        let mut volume = Smoothed::new(1.0);
        let mut pan = Smoothed::new(0.0);
        let mut audible = Smoothed::new(1.0);
        let mut send = Smoothed::new(0.0);
        volume.prepare(info.sample_rate, 0.01);
        pan.prepare(info.sample_rate, 0.01);
        audible.prepare(info.sample_rate, 0.005);
        send.prepare(info.sample_rate, 0.01);
        Self {
            clips: Box::new(ClipList::empty()),
            instrument: None,
            sequencer: Sequencer::new(),
            events: EventBuffer::new(512),
            automation: Box::new(AutomationSet::default()),
            slots,
            buffer: AudioBuffer::new(MIX_CHANNELS, info.max_frames),
            volume,
            pan,
            audible,
            send,
            mute: false,
            solo: false,
        }
    }

    pub fn slot_mut(&mut self, slot: usize) -> &mut Option<Box<dyn Processor>> {
        &mut self.slots[slot]
    }

    /// Swaps in a new clip list and returns the old one, which the caller
    /// must send back to the main thread to be freed. Clips that are
    /// playing carry their de-click state over, so edits don't click.
    pub fn replace_clips(&mut self, mut clips: Box<ClipList>) -> Box<ClipList> {
        clips.adopt_state_from(&self.clips);
        std::mem::replace(&mut self.clips, clips)
    }

    /// Returns the old instrument, to be freed on the main thread.
    pub fn replace_instrument(&mut self, instrument: Option<Box<dyn Instrument>>) -> Option<Box<dyn Instrument>> {
        std::mem::replace(&mut self.instrument, instrument)
    }

    pub fn instrument_mut(&mut self) -> Option<&mut Box<dyn Instrument>> {
        self.instrument.as_mut()
    }

    /// Returns the old automation, to be freed on the main thread.
    pub fn replace_automation(&mut self, automation: Box<AutomationSet>) -> Box<AutomationSet> {
        std::mem::replace(&mut self.automation, automation)
    }

    /// Applies every automation lane's value at the window's start. Values
    /// go through the same setters as manual changes, so the smoothers
    /// turn these block-rate steps into continuous ramps.
    fn apply_automation(&mut self, ctx: &ProcessContext) {
        if self.automation.lanes.is_empty() {
            return;
        }
        let beat = ctx.position as f64 / ctx.samples_per_beat();
        // Borrow the fields separately: the lanes are iterated mutably
        // while the things they control are changed.
        let Track { automation, slots, instrument, volume, pan, .. } = self;
        for lane in &mut automation.lanes {
            let Some(v) = lane.changed_value_at(beat) else { continue };
            match lane.target {
                AutoTarget::Volume => volume.set(db_to_gain(v)),
                AutoTarget::Pan => pan.set(v.clamp(-1.0, 1.0)),
                AutoTarget::Node { slot, param } => {
                    if let Some(node) = slots.get_mut(slot).and_then(|s| s.as_mut()) {
                        node.set_param(param, v);
                    }
                }
                AutoTarget::Instrument { param } => {
                    if let Some(inst) = instrument.as_mut() {
                        inst.set_param(param, v);
                    }
                }
            }
        }
    }

    /// Returns the old sequence, to be freed on the main thread.
    pub fn replace_sequence(&mut self, sequence: Box<Sequence>) -> Box<Sequence> {
        self.sequencer.replace(sequence)
    }

    pub fn set_volume_db(&mut self, db: f32) {
        self.volume.set(db_to_gain(db));
    }

    pub fn set_pan(&mut self, pos: f32) {
        self.pan.set(pos.clamp(-1.0, 1.0));
    }

    pub fn set_send_db(&mut self, db: f32) {
        self.send.set(db_to_gain(db));
    }

    /// The bus that other tracks sum into (used for aux and master).
    pub fn bus(&mut self) -> &mut AudioBuffer {
        &mut self.buffer
    }

    fn run_chain(&mut self, ctx: &ProcessContext, key: Option<&AudioBuffer>) {
        for node in self.slots.iter_mut().flatten() {
            node.process_keyed(&mut self.buffer, ctx, key);
        }
    }

    /// The track's own buffer: after its chain, before its fader. This is
    /// where a sidechain is tapped from.
    pub fn output(&self) -> &AudioBuffer {
        &self.buffer
    }

    /// Peak of each channel of the track's buffer.
    fn peaks(&self) -> (f32, f32) {
        let (l, r) = self.buffer.stereo();
        let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, &s| peak_with(m, s));
        (peak(l), peak(r))
    }

    /// Renders this track for frames `offset..offset+n` of the block and
    /// adds the result into `master` and, scaled by the send, into `aux`
    /// (whose windows the caller has set). `live` holds live MIDI for this
    /// window, with frames counted from `live_base`. `key` is the sidechain
    /// signal for this window, if the track has one.
    #[allow(clippy::too_many_arguments)]
    pub fn render_into(
        &mut self,
        offset: usize,
        n: usize,
        ctx: &ProcessContext,
        any_solo: bool,
        master: &mut AudioBuffer,
        aux: &mut AudioBuffer,
        live: &[(usize, MidiMessage)],
        live_base: usize,
        key: Option<&AudioBuffer>,
    ) -> TrackPeaks {
        self.buffer.set_window(offset, n);
        self.buffer.clear();
        self.apply_automation(ctx);
        // Clips and the instrument write the track's raw audio; the chain
        // then processes it.
        self.clips.render(&mut self.buffer, ctx);
        if let Some(instrument) = self.instrument.as_mut() {
            self.events.clear();
            self.sequencer.events(ctx, n, &mut self.events);
            for &(frame, msg) in live {
                self.events.push(frame - live_base, msg);
            }
            self.events.sort();
            instrument.render(&mut self.buffer, ctx, self.events.as_slice());
        }

        // The chain ALWAYS runs, even when muted or silent. Its nodes have
        // state (filter memory, oscillator phase) that
        // must keep moving with time, and we can't know whether a chain is
        // silent without running it: a clip might start this very block.
        self.run_chain(ctx, key);
        let (pre_l, pre_r) = self.peaks();
        let pre = pre_l.max(pre_r);

        let audible = !self.mute && (!any_solo || self.solo);
        self.audible.set(if audible { 1.0 } else { 0.0 });

        // Exercise 3: if nothing can come out, skip the per-sample fader
        // loop. The smoothers still have to move as if time passed, or a
        // volume ramp would freeze while the track is silent.
        if pre == 0.0 || self.audible.is_at(0.0) {
            self.volume.skip(n);
            self.pan.skip(n);
            self.audible.skip(n);
            self.send.skip(n);
            return TrackPeaks { pre, post: 0.0 };
        }

        let (src_l, src_r) = self.buffer.stereo();
        let (master_l, master_r) = master.stereo_mut();
        let (aux_l, aux_r) = aux.stereo_mut();

        let settled =
            self.volume.is_settled() && self.pan.is_settled() && self.audible.is_settled() && self.send.is_settled();
        if settled {
            // The common case: nothing is moving. Every gain is a constant
            // for the whole block, so compute them once (no per-sample sin
            // and cos for the balance) and run a plain multiply-add loop
            // over slices, which the compiler vectorizes.
            let gain = self.volume.value() * self.audible.value();
            let (bal_l, bal_r) = balance_gains(self.pan.value());
            let (gl, gr) = (gain * bal_l, gain * bal_r);
            let send = self.send.value();
            for i in 0..n {
                let left = src_l[i] * gl;
                let right = src_r[i] * gr;
                master_l[i] += left;
                master_r[i] += right;
                // Post-fader send: the copy follows volume, balance and mute.
                aux_l[i] += left * send;
                aux_r[i] += right * send;
            }
            // Scaling by a positive constant scales the peak by the same
            // constant, so the post-fader peak needs no second pass.
            let post = if gl == 0.0 && gr == 0.0 { 0.0 } else { (pre_l * gl).max(pre_r * gr) };
            return TrackPeaks { pre, post };
        }

        // Something is ramping: advance the smoothers sample by sample.
        // The balance math only runs per sample if the pan itself moves.
        let pan_moving = !self.pan.is_settled();
        let mut balance = balance_gains(self.pan.value());
        let mut post = 0.0f32;
        for i in 0..n {
            let gain = self.volume.next() * self.audible.next();
            if pan_moving {
                balance = balance_gains(self.pan.next());
            }
            let send = self.send.next();
            let left = src_l[i] * gain * balance.0;
            let right = src_r[i] * gain * balance.1;
            master_l[i] += left;
            master_r[i] += right;
            aux_l[i] += left * send;
            aux_r[i] += right * send;
            post = peak_with(peak_with(post, left), right);
        }
        TrackPeaks { pre, post }
    }

    /// For buses: run the chain and volume on the bus in place.
    /// The caller has already set the window. Returns the peak.
    pub fn process_in_place(&mut self, ctx: &ProcessContext) -> f32 {
        self.run_chain(ctx, None);
        let (l, r) = self.buffer.stereo_mut();
        if self.volume.is_settled() {
            let gain = self.volume.value();
            if gain != 1.0 {
                l.iter_mut().chain(r.iter_mut()).for_each(|s| *s *= gain);
            }
        } else {
            for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                let gain = self.volume.next();
                *a *= gain;
                *b *= gain;
            }
        }
        let (l, r) = self.buffer.stereo();
        l.iter().chain(r).fold(0.0f32, |m, &s| peak_with(m, s))
    }

    /// Adds this bus's current window into `bus` (aux -> master).
    pub fn add_into(&self, bus: &mut AudioBuffer) {
        for ch in 0..MIX_CHANNELS {
            for (dst, src) in bus.channel_mut(ch).iter_mut().zip(self.buffer.channel(ch)) {
                *dst += src;
            }
        }
    }
}

/// Stereo balance for a stereo track. Same equal-power curve as the Pan
/// node, but scaled so center is unity: the side you pan toward stays at
/// full level and the other side fades out. (A true pan law, like the Pan
/// node's -3 dB center, is for placing a *mono* source.)
pub(crate) fn balance_gains(pos: f32) -> (f32, f32) {
    let (l, r) = pan_gains(pos);
    ((l * SQRT_2).min(1.0), (r * SQRT_2).min(1.0))
}

/// Which meter on a track.
#[derive(Clone, Copy)]
pub enum MeterPoint {
    PreFader,
    PostFader,
}

/// Peak levels, written by the audio thread and read by the UI.
///
/// Each slot holds the highest peak since the UI last read it, so short
/// peaks aren't missed when the UI polls slowly. The trick: for
/// non-negative floats, the bit patterns sort the same way as the values,
/// so `AtomicU32::fetch_max` on the raw bits is a lock-free "max".
pub struct Meters {
    pre: Vec<AtomicU32>,
    post: Vec<AtomicU32>,
    aux: AtomicU32,
    master: AtomicU32,
}

impl Meters {
    pub fn new() -> Arc<Self> {
        let bank = || (0..MAX_TRACKS).map(|_| AtomicU32::new(0)).collect();
        Arc::new(Self {
            pre: bank(),
            post: bank(),
            aux: AtomicU32::new(0),
            master: AtomicU32::new(0),
        })
    }

    /// Buses only have one meter, so `point` is ignored for them.
    fn slot(&self, target: Target, point: MeterPoint) -> &AtomicU32 {
        match (target, point) {
            (Target::Master, _) => &self.master,
            (Target::Aux, _) => &self.aux,
            (Target::Track(i), MeterPoint::PreFader) => &self.pre[i],
            (Target::Track(i), MeterPoint::PostFader) => &self.post[i],
        }
    }

    /// Audio thread.
    pub fn record(&self, target: Target, point: MeterPoint, peak: f32) {
        // NaN has bits above every number, so it would pin the meter there
        // forever. Show it as infinite instead, which reads as CLIP.
        let peak = if peak.is_nan() { f32::INFINITY } else { peak.abs() };
        self.slot(target, point).fetch_max(peak.to_bits(), Ordering::Relaxed);
    }

    /// UI thread: returns the peak since the last call and resets it.
    pub fn take(&self, target: Target, point: MeterPoint) -> f32 {
        f32::from_bits(self.slot(target, point).swap(0, Ordering::Relaxed))
    }
}
