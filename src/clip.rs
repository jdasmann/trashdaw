//! Audio clips on the timeline, and the per-track list that plays them.

use crate::buffer::AudioBuffer;
use crate::loader::AudioData;
use crate::nodes::db_to_linear;
use crate::processor::PrepareInfo;
use crate::transport::ProcessContext;
use std::sync::Arc;

/// De-click fade length.
const FADE_SECS: f64 = 0.003;

/// Everything about a clip that the user edits. Plain `Copy` data, shared
/// by the main thread's project model and the engine's `AudioClip`.
#[derive(Clone, Copy, Debug)]
pub struct ClipParams {
    /// Stable id, so a clip can be found again after edits.
    pub id: u32,
    /// Where the clip starts on the timeline, in beats.
    pub start_beat: f64,
    /// How long the clip is on the timeline, in beats.
    pub length_beats: f64,
    /// How far into the FILE playback begins, in seconds. File time, not
    /// beats: the audio isn't time-stretched, so this must not move when
    /// the tempo changes.
    pub offset_secs: f64,
    pub gain_db: f32,
    /// User fades, in beats. 0 means none.
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
}

impl ClipParams {
    pub fn new(id: u32, start_beat: f64, length_beats: f64) -> Self {
        Self {
            id,
            start_beat,
            length_beats,
            offset_secs: 0.0,
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
        }
    }
}

/// The de-click state. `Copy`, so it can be handed from an old clip to
/// its replacement in a new list (exercise 1).
#[derive(Clone, Copy, Default)]
struct Declick {
    /// Where we expect the next sample to be (in "samples since clip
    /// start") while the clip is sounding. `None` when it isn't.
    expected_since: Option<f64>,
    last_since: f64,
    fade_in: u32,
    tail: Option<Tail>,
}

#[derive(Clone, Copy)]
struct Tail {
    since: f64,
    remaining: u32,
}

/// One piece of audio placed on the timeline. Its position is computed
/// from the transport every sample; the only state is for de-clicking.
pub struct AudioClip {
    data: Arc<AudioData>,
    params: ClipParams,
    gain: f32,
    fade_len: u32,
    /// Set when a later clip cut this one short (exercise 3). The clip then
    /// plays one de-click length past its end, fading out under the start
    /// of the next clip instead of leaving a gap.
    overlap_out: bool,
    state: Declick,
    /// Which render pass last touched this clip (see `ClipList::render`).
    tag: u64,
}

/// Where the sound lives, computed once per block.
struct Region {
    offset_src: f64,
    ratio: f64,
    /// Length on the timeline, in output samples.
    len: f64,
    declick: f64,
    trimmed_start: bool,
    /// User fades in output samples.
    fade_in: f64,
    fade_out: f64,
}

impl Region {
    /// The full, stateless envelope. The de-click fades and the user's
    /// fades are separate gains, so they multiply (exercise 2): a user
    /// fade never removes the de-click protection, and vice versa.
    fn env(&self, since: f64) -> f32 {
        let until_end = self.len - since;
        let mut env = (until_end / self.declick).min(1.0);
        if self.trimmed_start {
            env = env.min(since / self.declick);
        }
        if self.fade_in > 0.0 {
            env *= (since / self.fade_in).min(1.0);
        }
        if self.fade_out > 0.0 {
            env *= (until_end / self.fade_out).min(1.0);
        }
        env.clamp(0.0, 1.0) as f32
    }
}

impl AudioClip {
    /// Runs on the main thread.
    pub fn new(data: Arc<AudioData>, params: ClipParams, info: &PrepareInfo) -> Self {
        let mut params = params;
        params.start_beat = params.start_beat.max(0.0);
        params.length_beats = params.length_beats.max(0.0);
        params.offset_secs = params.offset_secs.max(0.0);
        params.fade_in_beats = params.fade_in_beats.max(0.0);
        params.fade_out_beats = params.fade_out_beats.max(0.0);
        let db = params.gain_db.clamp(-60.0, 12.0);
        Self {
            data,
            params,
            gain: if db <= -60.0 { 0.0 } else { db_to_linear(db) },
            fade_len: ((FADE_SECS * info.sample_rate as f64) as u32).max(1),
            overlap_out: false,
            state: Declick::default(),
            tag: 0,
        }
    }

    /// True while the clip is sounding or ringing out. Inactive clips
    /// outside the current block don't need to be visited at all.
    pub fn is_active(&self) -> bool {
        self.state.expected_since.is_some() || self.state.tail.is_some()
    }

    fn region(&self, ctx: &ProcessContext) -> Option<Region> {
        let frames = self.data.frames();
        if frames < 2 || self.data.channels.is_empty() {
            return None;
        }
        let spb = ctx.samples_per_beat();
        let ratio = self.data.sample_rate / ctx.sample_rate;
        let offset_src = self.params.offset_secs * self.data.sample_rate;
        // Last readable position is frames - 1 (we interpolate with idx+1).
        let available = (frames - 1) as f64 - offset_src;
        if available <= 0.0 {
            return None;
        }
        let mut timeline_len = self.params.length_beats * spb;
        if self.overlap_out {
            timeline_len += self.fade_len as f64;
        }
        Some(Region {
            offset_src,
            ratio,
            len: (available / ratio).min(timeline_len),
            declick: self.fade_len as f64,
            trimmed_start: self.params.offset_secs > 0.0,
            fade_in: self.params.fade_in_beats * spb,
            fade_out: self.params.fade_out_beats * spb,
        })
    }

    fn read(&self, r: &Region, since: f64, ch: usize) -> f32 {
        if since < 0.0 || since >= r.len {
            return 0.0;
        }
        let pos = r.offset_src + since * r.ratio;
        // `since < len` should keep idx + 1 in bounds, but this is float
        // math on the audio thread, where a panic stops all sound. The
        // clamp costs nothing and makes it impossible.
        let idx = (pos as usize).min(self.data.frames() - 2);
        let frac = (pos - idx as f64) as f32;
        // Mono files feed both channels. (The mix is always stereo now, so
        // mono devices are handled once, at the output.)
        let src = &self.data.channels[ch.min(self.data.channels.len() - 1)];
        src[idx] + (src[idx + 1] - src[idx]) * frac
    }

    fn start_tail(&mut self) {
        self.state.tail = Some(Tail { since: self.state.last_since + 1.0, remaining: self.fade_len });
    }

    /// The fast path for the overwhelmingly common case: playing steadily
    /// through the middle of the clip. If the whole window is continuous
    /// with the last one, has no tail ringing out, no de-click fade-in in
    /// progress, and sits where the envelope is exactly 1, every sample is
    /// just "interpolate, scale, add". Returns false if any of that isn't
    /// true, and the general per-sample path runs instead.
    fn render_steady(&mut self, region: &Region, start: f64, buffer: &mut AudioBuffer, ctx: &ProcessContext) -> bool {
        let n = buffer.frames();
        let first = ctx.position as f64 - start;
        let continuous = self.state.expected_since.is_some_and(|e| (first - e).abs() <= 0.5);
        if !ctx.playing || !continuous || self.state.tail.is_some() || self.state.fade_in < self.fade_len {
            return false;
        }
        // Where Region::env is exactly 1: past every fade-in, before every
        // fade-out.
        let lead_in = if region.trimmed_start { region.declick } else { 0.0 }.max(region.fade_in);
        let lead_out = region.declick.max(region.fade_out);
        if first < lead_in || first + n as f64 > region.len - lead_out || buffer.channels() != 2 {
            return false;
        }

        let (out_l, out_r) = buffer.stereo_mut();
        let src_l = &self.data.channels[0][..];
        let src_r = &self.data.channels[1.min(self.data.channels.len() - 1)][..];
        let last = src_l.len() - 2;
        let gain = self.gain;
        for (i, (l, r)) in out_l.iter_mut().zip(out_r.iter_mut()).enumerate() {
            // Same position math as `read`, so the result is identical.
            let pos = region.offset_src + (first + i as f64) * region.ratio;
            let idx = (pos as usize).min(last);
            let frac = (pos - idx as f64) as f32;
            *l += (src_l[idx] + (src_l[idx + 1] - src_l[idx]) * frac) * gain;
            *r += (src_r[idx] + (src_r[idx + 1] - src_r[idx]) * frac) * gain;
        }
        self.state.expected_since = Some(first + n as f64);
        self.state.last_since = first + n as f64 - 1.0;
        true
    }

    /// Adds this clip's audio for the current window into `buffer`.
    pub fn render(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext) {
        let Some(region) = self.region(ctx) else {
            self.state = Declick::default();
            return;
        };
        let start = self.params.start_beat * ctx.samples_per_beat();
        let n = buffer.frames();

        // Fast exit: the whole block misses the clip and nothing is ringing.
        if !self.is_active() {
            let first = ctx.position as f64 - start;
            if !ctx.playing || first + n as f64 <= 0.0 || first >= region.len {
                return;
            }
        }

        if self.render_steady(&region, start, buffer, ctx) {
            return;
        }

        let fade = self.fade_len;
        for i in 0..n {
            let mut main: Option<(f64, f32)> = None;

            if ctx.playing {
                let since = (ctx.position + i as u64) as f64 - start;
                let inside = since >= 0.0 && since < region.len;
                let continuous = self.state.expected_since.is_some_and(|e| (since - e).abs() <= 0.5);

                if inside {
                    if !continuous {
                        // Jumped (or just started). If we were sounding,
                        // the old position fades out as a tail.
                        if self.state.expected_since.is_some() {
                            self.start_tail();
                        }
                        // Fade in only when landing mid-waveform; landing
                        // on the clip's first sample keeps the attack.
                        self.state.fade_in = if since >= 1.0 { 0 } else { fade };
                    }
                    self.state.expected_since = Some(since + 1.0);
                    self.state.last_since = since;

                    let mut env = region.env(since);
                    if self.state.fade_in < fade {
                        env *= self.state.fade_in as f32 / fade as f32;
                        self.state.fade_in += 1;
                    }
                    main = Some((since, env));
                } else if self.state.expected_since.is_some() {
                    // We were sounding and now we're outside. A natural end
                    // is continuous and already faded; a jump away isn't.
                    if !continuous {
                        self.start_tail();
                    }
                    self.state.expected_since = None;
                }
            } else if self.state.expected_since.take().is_some() {
                // Stopped while sounding: ring out from where we were.
                self.start_tail();
            }

            let tail = self.state.tail.as_mut().map(|t| {
                let voice = (t.since, t.remaining as f32 / fade as f32);
                t.since += 1.0;
                t.remaining -= 1;
                voice
            });
            if self.state.tail.is_some_and(|t| t.remaining == 0) {
                self.state.tail = None;
            }

            if main.is_none() && tail.is_none() {
                continue;
            }
            for ch in 0..buffer.channels() {
                let mut s = 0.0;
                if let Some((since, env)) = main {
                    s += self.read(&region, since, ch) * env;
                }
                if let Some((since, g)) = tail {
                    s += self.read(&region, since, ch) * g * region.env(since);
                }
                buffer.channel_mut(ch)[i] += s * self.gain;
            }
        }
    }
}

/// A track's clips, as an immutable-shape snapshot.
///
/// The main thread never edits this in place. On every edit it builds a
/// whole new `ClipList` and sends it over; the old one comes back as
/// garbage. The audio thread only ever sees a complete, consistent list,
/// and no locks are involved.
pub struct ClipList {
    /// Sorted by start, so we can binary-search for the current block.
    clips: Vec<AudioClip>,
    /// (id, index) pairs sorted by id, for matching clips across lists.
    by_id: Vec<(u32, usize)>,
    /// The longest clip. Any clip that starts earlier than
    /// (block start - max_len) has certainly ended before the block.
    max_len_beats: f64,
    /// Indices of clips that were sounding or ringing last block.
    /// Both vectors have capacity for every clip, so pushes never allocate.
    active: Vec<usize>,
    next_active: Vec<usize>,
    tag: u64,
}

impl ClipList {
    /// Runs on the main thread, so all the sorting and trimming here costs
    /// the audio thread nothing.
    pub fn new(mut clips: Vec<AudioClip>) -> Self {
        // Stable sort: clips with equal starts keep their order, so the one
        // added later comes second and wins below.
        clips.sort_by(|a, b| a.params.start_beat.total_cmp(&b.params.start_beat));

        // Exercise 3: a clip ends where the next one begins. Only the
        // engine's copy is trimmed; the project keeps the full length, so
        // moving the later clip away restores the earlier one (non-destructive).
        // Checking just the next clip is enough: anything after it starts
        // later still.
        for i in 1..clips.len() {
            let next_start = clips[i].params.start_beat;
            let prev = &mut clips[i - 1];
            if next_start < prev.params.start_beat + prev.params.length_beats {
                let trimmed = next_start - prev.params.start_beat;
                prev.params.length_beats = trimmed.max(0.0);
                // A clip trimmed to nothing (same start) must stay silent,
                // not play a tiny fade.
                prev.overlap_out = trimmed > 0.0;
            }
        }

        let max_len_beats = clips.iter().map(|c| c.params.length_beats).fold(0.0, f64::max);
        let mut by_id: Vec<(u32, usize)> = clips.iter().enumerate().map(|(i, c)| (c.params.id, i)).collect();
        by_id.sort_unstable();
        let n = clips.len();
        Self {
            clips,
            by_id,
            max_len_beats,
            active: Vec::with_capacity(n),
            next_active: Vec::with_capacity(n),
            tag: 0,
        }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Exercise 1: runs on the AUDIO thread when this list replaces `old`.
    /// Only clips that are currently sounding have state worth keeping,
    /// and the old list already tracks exactly those, so this costs
    /// O(active · log n): a handful of binary searches, no allocation.
    pub fn adopt_state_from(&mut self, old: &ClipList) {
        for &i in &old.active {
            let old_clip = &old.clips[i];
            if let Ok(k) = self.by_id.binary_search_by_key(&old_clip.params.id, |&(id, _)| id) {
                let j = self.by_id[k].1;
                self.clips[j].state = old_clip.state;
                // Ids are unique, so this pushes each index at most once.
                self.active.push(j);
            }
            // A deleted clip has no match; it simply stops. Keeping its
            // tail would mean keeping the old clip alive a little longer.
        }
    }

    /// Renders every clip that overlaps the current window, plus any clip
    /// still ringing out from before (e.g. after a seek away from it).
    pub fn render(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext) {
        if self.clips.is_empty() {
            return;
        }
        self.tag = self.tag.wrapping_add(1);
        self.next_active.clear();

        // 1. Clips overlapping this block, found by binary search: only
        //    clips starting in [block_start - max_len, block_end) can.
        //    Skipped while stopped: nothing new can start.
        if ctx.playing {
            let spb = ctx.samples_per_beat();
            let block_start = ctx.position as f64 / spb;
            let block_end = (ctx.position + buffer.frames() as u64) as f64 / spb;
            let hi = self.clips.partition_point(|c| c.params.start_beat < block_end);
            let lo = self
                .clips
                .partition_point(|c| c.params.start_beat + self.max_len_beats < block_start);
            for i in lo..hi {
                self.render_one(i, buffer, ctx);
            }
        }

        // 2. Clips that were active last block but aren't in the window
        //    any more. They need one more visit to notice and ring out.
        for k in 0..self.active.len() {
            let i = self.active[k];
            if self.clips[i].tag != self.tag {
                self.render_one(i, buffer, ctx);
            }
        }

        std::mem::swap(&mut self.active, &mut self.next_active);
    }

    fn render_one(&mut self, i: usize, buffer: &mut AudioBuffer, ctx: &ProcessContext) {
        let clip = &mut self.clips[i];
        clip.render(buffer, ctx);
        clip.tag = self.tag;
        if clip.is_active() {
            // Each clip is visited at most once per pass (the tag check),
            // so this can't exceed the preallocated capacity.
            self.next_active.push(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Transport;

    const SR: f32 = 48_000.0; // 24_000 samples per beat at 120 BPM
    const BEAT: usize = 24_000;
    const FADE: usize = 144; // 3 ms at 48 kHz
    const INFO: PrepareInfo = PrepareInfo { sample_rate: SR, max_frames: 4096, channels: 2 };

    fn audio(channels: Vec<Vec<f32>>) -> Arc<AudioData> {
        Arc::new(AudioData { name: "test".into(), sample_rate: SR as f64, channels })
    }

    fn dc(value: f32) -> Arc<AudioData> {
        audio(vec![vec![value; 400_000]])
    }

    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames).map(|i| i as f32 / frames as f32).collect()
    }

    fn clip_p(data: &Arc<AudioData>, p: ClipParams) -> AudioClip {
        AudioClip::new(data.clone(), p, &INFO)
    }

    fn clip(data: &Arc<AudioData>, id: u32, start: f64, len: f64, offset: f64) -> AudioClip {
        let mut p = ClipParams::new(id, start, len);
        p.offset_secs = offset;
        clip_p(data, p)
    }

    /// Renders `frames` (split at loop points, like the engine does).
    fn render(list: &mut ClipList, t: &mut Transport, frames: usize) -> Vec<f32> {
        let mut buf = AudioBuffer::new(2, frames);
        buf.clear();
        let mut offset = 0;
        while offset < frames {
            let n = t.frames_until_boundary(frames - offset);
            buf.set_window(offset, n);
            list.render(&mut buf, &t.context());
            t.advance(n);
            offset += n;
        }
        buf.set_window(0, frames);
        buf.channel(0).to_vec()
    }

    fn playing_at(beat: f64) -> Transport {
        let mut t = Transport::new(SR as f64);
        t.seek_beats(beat);
        t.playing = true;
        t
    }

    #[test]
    fn plays_from_the_right_place_after_a_seek() {
        let data = audio(vec![ramp(100_000)]);
        let mut list = ClipList::new(vec![clip(&data, 1, 4.0, 4.0, 0.0)]);
        let mut t = playing_at(5.0); // one beat in = file frame 24_000
        let out = render(&mut list, &mut t, 512);
        assert!(out[0].abs() < 1e-3, "fades in mid-waveform");
        assert!((out[300] - data.channels[0][24_300]).abs() < 1e-6);
    }

    #[test]
    fn keeps_the_attack_at_the_clip_start() {
        let data = dc(0.5);
        let mut list = ClipList::new(vec![clip(&data, 1, 1.0, 1.0, 0.0)]);
        let mut t = playing_at(0.5); // reaches the clip by playing into it
        let out = render(&mut list, &mut t, BEAT);
        assert_eq!(out[11_999], 0.0);
        assert!((out[12_000] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rings_out_after_stop_and_after_seeking_away() {
        let data = dc(0.5);
        let mut list = ClipList::new(vec![clip(&data, 1, 0.0, 4.0, 0.0)]);
        let mut t = playing_at(1.0);
        render(&mut list, &mut t, 512);

        t.playing = false;
        let out = render(&mut list, &mut t, 512);
        assert!(out[0] > 0.45 && out[FADE..].iter().all(|&s| s == 0.0));

        // Seek far past the clip: it leaves the search window, but the
        // active list still visits it so it can fade out.
        t.playing = true;
        render(&mut list, &mut t, 512);
        t.seek_beats(100.0);
        let out = render(&mut list, &mut t, 512);
        assert!(out[0] > 0.45, "tail plays after seeking away");
        assert!(out[FADE..].iter().all(|&s| s == 0.0));
        assert!(list.active.is_empty(), "then the clip goes idle");
    }

    #[test]
    fn crossfades_across_a_loop_wrap() {
        let data = dc(0.5);
        let mut list = ClipList::new(vec![clip(&data, 1, 0.0, 8.0, 0.0)]);
        let mut t = playing_at(2.0 - 100.0 / BEAT as f64);
        t.set_loop(Some((1.0, 2.0)));
        let out = render(&mut list, &mut t, 1000);
        assert!(out[FADE..].iter().all(|&s| (s - 0.5).abs() < 0.01));
    }

    #[test]
    fn offset_is_in_file_seconds() {
        let data = audio(vec![ramp(100_000)]);
        let mut list = ClipList::new(vec![clip(&data, 1, 0.0, 1.0, 0.25)]); // 12_000 frames in
        let mut t = playing_at(0.0);
        let out = render(&mut list, &mut t, 30_000);
        assert!(out[0].abs() < 1e-6, "trimmed start fades in");
        assert!((out[1000] - data.channels[0][13_000]).abs() < 1e-6);
        assert!(out[BEAT..].iter().all(|&s| s == 0.0), "stops after its length");
    }

    #[test]
    fn split_clips_sound_identical_to_the_original() {
        let data = audio(vec![ramp(100_000)]);
        let mut whole = ClipList::new(vec![clip(&data, 1, 1.0, 2.0, 0.0)]);
        // Split at beat 2: the second half starts one beat (0.5 s) into the file.
        let mut halves = ClipList::new(vec![clip(&data, 1, 1.0, 1.0, 0.0), clip(&data, 2, 2.0, 1.0, 0.5)]);
        let a = render(&mut whole, &mut playing_at(1.0), 2 * BEAT);
        let b = render(&mut halves, &mut playing_at(1.0), 2 * BEAT);
        // Identical except the tiny de-click fades around the cut.
        let differing = a.iter().zip(&b).filter(|(x, y)| (*x - *y).abs() > 1e-6).count();
        assert!(differing <= 2 * FADE, "{differing} samples differ");
    }

    #[test]
    fn only_nearby_clips_are_visited() {
        let data = dc(0.5);
        let clips = (0..1000).map(|i| clip(&data, i, i as f64 * 4.0, 0.25, 0.0)).collect();
        let mut list = ClipList::new(clips);
        let mut t = playing_at(2000.0);
        render(&mut list, &mut t, 512);
        let visited = list.clips.iter().filter(|c| c.tag == list.tag).count();
        assert!(visited <= 2, "visited {visited} of 1000 clips");
    }

    // ---- Exercise 1: keeping state across edits ----

    /// Plays a clip, then swaps in an edited list mid-playback.
    fn swap_mid_playback(adopt: bool, edit: impl Fn(&mut ClipParams)) -> Vec<f32> {
        let data = dc(0.5);
        let p = ClipParams::new(7, 0.0, 8.0);
        let mut list = ClipList::new(vec![clip_p(&data, p)]);
        let mut t = playing_at(1.0);
        render(&mut list, &mut t, 2048); // past the initial fade-in

        let mut edited = p;
        edit(&mut edited);
        let mut new_list = ClipList::new(vec![clip_p(&data, edited)]);
        if adopt {
            new_list.adopt_state_from(&list);
        }
        render(&mut new_list, &mut t, 1024)
    }

    #[test]
    fn exercise1_edit_while_playing_is_seamless() {
        let shorten = |p: &mut ClipParams| p.length_beats = 6.0;
        let without = swap_mid_playback(false, shorten);
        assert!(without[0] < 0.1, "fresh state: the clip fades in again (a dip)");

        let with = swap_mid_playback(true, shorten);
        assert!(with.iter().all(|&s| (s - 0.5).abs() < 1e-6), "adopted state: no dip at all");
    }

    #[test]
    fn exercise1_moving_a_playing_clip_crossfades() {
        // Moving the clip makes `since` jump. With adopted state, that is
        // seen as a jump: the old position fades out while the new one
        // fades in, and a constant signal stays constant.
        let out = swap_mid_playback(true, |p| p.start_beat = 0.5);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 0.01));
    }

    // ---- Exercise 2: user fades ----

    #[test]
    fn exercise2_user_fades() {
        let data = dc(0.5);
        let mut p = ClipParams::new(1, 0.0, 4.0);
        p.fade_in_beats = 1.0;
        p.fade_out_beats = 1.0;
        let mut list = ClipList::new(vec![clip_p(&data, p)]);
        let out = render(&mut list, &mut playing_at(0.0), 4 * BEAT);
        assert!((out[BEAT / 2] - 0.25).abs() < 1e-3, "halfway through the fade-in");
        assert!((out[2 * BEAT] - 0.5).abs() < 1e-6, "full level in the middle");
        assert!((out[3 * BEAT + BEAT / 2] - 0.25).abs() < 1e-3, "halfway through the fade-out");
    }

    #[test]
    fn exercise2_fades_multiply_with_declick() {
        // A long user fade-in on a trimmed-start clip: both apply.
        let data = dc(0.5);
        let mut p = ClipParams::new(1, 0.0, 4.0);
        p.offset_secs = 0.1;
        p.fade_in_beats = 1.0;
        let mut list = ClipList::new(vec![clip_p(&data, p)]);
        let out = render(&mut list, &mut playing_at(0.0), 1024);
        let expected = 0.5 * (72.0 / BEAT as f32) * 0.5; // user fade × half-way de-click
        assert!((out[72] - expected).abs() < 1e-5);
    }

    // ---- Exercise 3: later clips win ----

    #[test]
    fn exercise3_later_clip_cuts_off_the_earlier_one() {
        let (a, b) = (dc(0.5), dc(0.25));
        // A: beats 0-4. B starts at beat 2, overlapping A's second half.
        let mut list = ClipList::new(vec![clip(&a, 1, 0.0, 4.0, 0.0), clip(&b, 2, 2.0, 4.0, 0.0)]);
        let out = render(&mut list, &mut playing_at(0.0), 4 * BEAT);
        assert!((out[BEAT] - 0.5).abs() < 1e-6, "only A before B starts");
        assert!((out[2 * BEAT] - 0.75).abs() < 0.01, "B's attack at full, A fading under it");
        assert!((out[2 * BEAT + 2 * FADE] - 0.25).abs() < 1e-6, "then only B, not A + B");
    }

    #[test]
    fn exercise3_same_start_plays_only_the_later_clip() {
        let (a, b) = (dc(0.5), dc(0.25));
        let mut list = ClipList::new(vec![clip(&a, 1, 0.0, 4.0, 0.0), clip(&b, 2, 0.0, 4.0, 0.0)]);
        let out = render(&mut list, &mut playing_at(0.0), BEAT);
        assert!(out.iter().all(|&s| (s - 0.25).abs() < 1e-6), "no blip from the hidden clip");
    }

    // ---- Optimization: the fast path must match the general path ----

    #[test]
    fn steady_fast_path_matches_the_general_path_exactly() {
        let file = |sr: f64| Arc::new(AudioData { name: "r".into(), sample_rate: sr, channels: vec![ramp(200_000), ramp(200_000)] });
        let mut faded = ClipParams::new(1, 0.5, 5.0);
        faded.fade_in_beats = 0.5;
        faded.fade_out_beats = 1.0;
        faded.offset_secs = 0.3;
        let cases = [
            ("plain, 48k", file(48_000.0), ClipParams::new(1, 0.0, 6.0), None),
            ("resampled 44.1k", file(44_100.0), ClipParams::new(1, 0.25, 6.0), None),
            ("fades + trimmed start", file(44_100.0), faded, None),
            ("loop wrap mid-clip", file(48_000.0), ClipParams::new(1, 0.0, 6.0), Some((1.0, 3.0))),
        ];
        for (label, data, params, looping) in cases {
            let run = |block: usize| {
                let mut list = ClipList::new(vec![clip_p(&data, params)]);
                let mut t = playing_at(0.0);
                t.set_loop(looping);
                let mut out = Vec::new();
                for _ in 0..(7 * BEAT / block) {
                    out.extend(render(&mut list, &mut t, block));
                }
                out
            };
            // 512-frame blocks take the fast path wherever they can; a
            // first call is never "continuous", so one giant block takes
            // the general path for every sample.
            let blocks = run(512);
            let whole = {
                let mut list = ClipList::new(vec![clip_p(&data, params)]);
                let mut t = playing_at(0.0);
                t.set_loop(looping);
                render(&mut list, &mut t, blocks.len())
            };
            assert_eq!(blocks.len(), whole.len());
            let first_diff = blocks.iter().zip(&whole).position(|(a, b)| a != b);
            assert_eq!(first_diff, None, "{label}: differs at sample {first_diff:?}");
        }
    }
}
