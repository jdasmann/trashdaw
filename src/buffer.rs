/// A planar (non-interleaved) audio buffer with fixed capacity.
///
/// All memory is allocated once in `new`. On the audio thread we only
/// move the *window* (which part of the capacity processors see),
/// never reallocate.
pub struct AudioBuffer {
    data: Vec<f32>,
    channels: usize,
    capacity: usize,
    start: usize,
    frames: usize,
}

impl AudioBuffer {
    pub fn new(channels: usize, capacity: usize) -> Self {
        Self {
            data: vec![0.0; channels * capacity],
            channels,
            capacity,
            start: 0,
            frames: capacity,
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Number of frames in the current window.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Restrict what `channel`/`channel_mut` expose to `frames` frames
    /// starting at `start`. The engine uses this to split a block at a
    /// loop point without processors needing to know.
    pub fn set_window(&mut self, start: usize, frames: usize) {
        assert!(start + frames <= self.capacity, "window exceeds buffer capacity");
        self.start = start;
        self.frames = frames;
    }

    /// Zeroes the current window.
    pub fn clear(&mut self) {
        for ch in 0..self.channels {
            self.channel_mut(ch).fill(0.0);
        }
    }

    /// Both channels of a stereo buffer at once. With two separate slices,
    /// a loop can zip over them: no per-sample bounds checks, and the
    /// compiler can vectorize it.
    pub fn stereo(&self) -> (&[f32], &[f32]) {
        assert!(self.channels >= 2);
        let (l, r) = self.data.split_at(self.capacity);
        let w = self.start..self.start + self.frames;
        (&l[w.clone()], &r[w])
    }

    /// Mutable version of `stereo`. `split_at_mut` is how you borrow two
    /// non-overlapping parts of one Vec mutably at the same time.
    pub fn stereo_mut(&mut self) -> (&mut [f32], &mut [f32]) {
        assert!(self.channels >= 2);
        let (l, r) = self.data.split_at_mut(self.capacity);
        let w = self.start..self.start + self.frames;
        (&mut l[w.clone()], &mut r[w])
    }

    pub fn channel(&self, ch: usize) -> &[f32] {
        let base = ch * self.capacity + self.start;
        &self.data[base..base + self.frames]
    }

    pub fn channel_mut(&mut self, ch: usize) -> &mut [f32] {
        let base = ch * self.capacity + self.start;
        &mut self.data[base..base + self.frames]
    }
}
