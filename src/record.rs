//! Recording: getting audio from an input device into a clip.
//!
//! Three real-time-safe hops:
//!   input callback --(ring of samples)--> engine (audio thread)
//!   engine --(ring of RecEvents)--> disk writer thread
//!   disk writer --(channel)--> main thread, which places the clip
//!
//! The engine is in the middle because only it knows where the transport
//! is. The disk writer exists because writing files can block for many
//! milliseconds, which no audio callback may ever do.

use crate::loader::AudioData;
use rtrb::Consumer;
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Frames per message from the engine to the disk writer.
pub const CHUNK: usize = 256;

/// A message from the engine to the disk writer. Fixed size (no heap), so
/// the engine can send it through a ring buffer without allocating.
#[derive(Clone, Copy)]
pub enum RecEvent {
    /// A take begins. `start_frame` is its timeline position in samples,
    /// already latency-compensated (and possibly negative, if the take
    /// began right at the start of the song).
    Begin { take: u32, track: usize, start_frame: i64 },
    Samples { len: usize, left: [f32; CHUNK], right: [f32; CHUNK] },
    End { take: u32 },
}

/// Recording state shared between threads: written by the audio threads,
/// read by the UI. Atomics only, like the meters.
pub struct RecordShared {
    /// True while a take is being recorded.
    pub recording: AtomicBool,
    /// Chunks the disk writer couldn't keep up with (dropped, not waited for).
    pub dropped: AtomicU32,
    /// Peak input level since the UI last read it (f32 bits; see Meters).
    pub input_peak: AtomicU32,
    /// Latencies reported by the driver, in frames.
    pub output_latency: AtomicU32,
    pub input_latency: AtomicU32,
    /// Running totals of frames received from the input and rendered for
    /// the output. Their difference grows steadily if the clocks differ.
    pub frames_in: AtomicU64,
    pub frames_out: AtomicU64,
    /// The drift correction being applied, in thousandths of a ppm.
    pub correction_ppm: std::sync::atomic::AtomicI64,
}

impl RecordShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            recording: AtomicBool::new(false),
            dropped: AtomicU32::new(0),
            input_peak: AtomicU32::new(0),
            output_latency: AtomicU32::new(0),
            input_latency: AtomicU32::new(0),
            frames_in: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            correction_ppm: std::sync::atomic::AtomicI64::new(0),
        })
    }

    pub fn record_peak(&self, peak: f32) {
        self.input_peak.fetch_max(peak.abs().to_bits(), Ordering::Relaxed);
    }

    pub fn take_input_peak(&self) -> f32 {
        f32::from_bits(self.input_peak.swap(0, Ordering::Relaxed))
    }
}

/// Where a take starts on the timeline.
///
/// The first input frame the engine reads in this callback was captured a
/// while ago: it has been waiting in the ring (`waiting` frames, at most)
/// and took `input_latency` to arrive from the converter. And the player
/// was hearing audio `output_latency` behind what the engine is rendering.
/// So what they played then belongs `waiting + input + output` frames
/// before the playhead, plus any manual adjustment (from calibrating with
/// a loopback cable, for drivers that report latency poorly).
pub fn take_start(position: u64, waiting: usize, input_latency: u32, output_latency: u32, adjust: i64) -> i64 {
    position as i64 - waiting as i64 - input_latency as i64 - output_latency as i64 - adjust
}

/// Estimates clock drift between the input and output devices.
///
/// Two devices running at "48 kHz" never run at exactly the same rate:
/// each has its own crystal, typically off by tens of parts per million.
/// The difference (frames received - frames rendered) therefore grows
/// steadily. But it also wobbles by up to a whole buffer at any instant,
/// because input arrives in bursts. Comparing two readings would mostly
/// measure the wobble; instead this fits a straight line through every
/// reading, and the line's slope is the drift.
#[derive(Default, Clone, Debug)]
pub struct DriftEstimator {
    origin: Option<(u64, u64)>,
    n: f64,
    mean_x: f64,
    mean_y: f64,
    /// Sums of squared/cross deviations, updated incrementally (Welford's
    /// method): stable even when the frame counts reach billions, where the
    /// textbook sum-of-squares formula loses its precision.
    m2_x: f64,
    c_xy: f64,
    span: f64,
}

impl DriftEstimator {
    /// Adds a reading of the two running totals.
    pub fn add(&mut self, frames_in: u64, frames_out: u64) {
        let (in0, out0) = *self.origin.get_or_insert((frames_in, frames_out));
        let x = frames_out.saturating_sub(out0) as f64;
        let y = (frames_in as f64 - in0 as f64) - x;
        self.n += 1.0;
        let dx = x - self.mean_x;
        self.mean_x += dx / self.n;
        self.mean_y += (y - self.mean_y) / self.n;
        self.m2_x += dx * (x - self.mean_x);
        self.c_xy += dx * (y - self.mean_y);
        self.span = x;
    }

    /// The drift in parts per million (positive: the input runs fast), once
    /// there are `min_frames` of history to fit.
    pub fn ppm(&self, min_frames: f64) -> Option<f64> {
        (self.n >= 3.0 && self.span >= min_frames && self.m2_x > 0.0).then(|| self.c_xy / self.m2_x * 1e6)
    }

    /// How many output frames the estimate covers.
    pub fn span_frames(&self) -> f64 {
        self.span
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Step 10 exercise: drift CORRECTION, by adaptive resampling.
///
/// Input frames go into a small buffer; out come exactly as many frames as
/// the output renders, read at a fractional position that advances by
/// `ratio` (close to 1) per frame, with 4-point cubic interpolation. A PI
/// controller steers the ratio to keep the buffer at a target fill: if it
/// keeps filling up, the input clock is fast, so read a little faster. The
/// integral term is what finds the true clock ratio; on its own, the
/// proportional term would always settle with some error left.
///
/// The gains are deliberately gentle. Input arrives in whole device
/// buffers, so at a realistic 20 ppm an extra buffer's worth turns up only
/// every few minutes: the fill level is a coarse sawtooth, and a fast
/// controller would chase it, wobbling the pitch. Slow and steady settles
/// in minutes and then holds (clock drift barely changes). Simulated, it
/// keeps 5-100 ppm of drift with under 50 ppm of wobble, well under a
/// tenth of a cent.
pub struct DriftCorrector {
    l: Vec<f32>,
    r: Vec<f32>,
    mask: usize,
    read: usize,
    len: usize,
    /// Fractional read position, in frames past `read`.
    pos: f64,
    ratio: f64,
    integral: f64,
    smoothed_error: f64,
    primed: bool,
    pub underruns: u32,
}

const KP: f64 = 1e-7;
const KI: f64 = 1e-11;
const SMOOTHING: f64 = 0.001;
/// No real pair of crystal clocks differs by more than this.
pub const MAX_CORRECTION: f64 = 1e-3;

impl DriftCorrector {
    /// Main thread: all the memory it will ever use.
    pub fn new(capacity: usize) -> Self {
        let n = capacity.next_power_of_two();
        Self { l: vec![0.0; n], r: vec![0.0; n], mask: n - 1, read: 0, len: 0, pos: 1.0, ratio: 1.0, integral: 0.0, smoothed_error: 0.0, primed: false, underruns: 0 }
    }

    /// The current correction, in parts per million.
    pub fn ppm(&self) -> f64 {
        (self.ratio - 1.0) * 1e6
    }

    /// How long ago (in frames) the next output frame was captured: the
    /// latency this buffer adds.
    pub fn latency_frames(&self) -> usize {
        (self.len as f64 - self.pos).max(0.0) as usize
    }

    pub fn push(&mut self, l: &[f32], r: &[f32]) {
        let cap = self.l.len();
        for (&a, &b) in l.iter().zip(r) {
            if self.len == cap {
                self.read = (self.read + 1) & self.mask; // full: drop the oldest
                self.len -= 1;
            }
            let at = (self.read + self.len) & self.mask;
            self.l[at] = a;
            self.r[at] = b;
            self.len += 1;
        }
    }

    /// Produces exactly `out_l.len()` frames, resampled to the output clock.
    pub fn pull(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
        let n = out_l.len();
        // Two callbacks of headroom (plus room for one input burst).
        let target = (2 * n + 512) as f64;
        if !self.primed {
            if (self.len as f64) < target {
                out_l.fill(0.0);
                out_r.fill(0.0);
                return;
            }
            self.primed = true;
            self.pos = 1.0;
        }
        let error = self.len as f64 - target;
        self.smoothed_error += SMOOTHING * (error - self.smoothed_error);
        self.integral = (self.integral + self.smoothed_error).clamp(-1e9, 1e9);
        self.ratio = (1.0 + KP * self.smoothed_error + KI * self.integral).clamp(1.0 - MAX_CORRECTION, 1.0 + MAX_CORRECTION);

        let at = |k: usize| (self.read + k) & self.mask;
        for i in 0..n {
            let k = self.pos as usize;
            if k + 2 >= self.len {
                // Ran dry: silence, and prime again.
                out_l[i..].fill(0.0);
                out_r[i..].fill(0.0);
                self.primed = false;
                self.underruns += 1;
                break;
            }
            let t = (self.pos - k as f64) as f32;
            let (a, b, c, d) = (at(k - 1), at(k), at(k + 1), at(k + 2));
            out_l[i] = cubic(self.l[a], self.l[b], self.l[c], self.l[d], t);
            out_r[i] = cubic(self.r[a], self.r[b], self.r[c], self.r[d], t);
            self.pos += self.ratio;
        }
        // Drop what's been read, keeping one frame before the position for
        // the interpolation.
        let drop = (self.pos as usize).saturating_sub(1).min(self.len);
        self.read = (self.read + drop) & self.mask;
        self.len -= drop;
        self.pos -= drop as f64;
    }
}

/// Catmull-Rom interpolation between p1 and p2 (t in 0..1). Unlike linear
/// interpolation, it keeps high frequencies nearly intact.
fn cubic(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    0.5 * (2.0 * p1 + (p2 - p0) * t + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t + (3.0 * (p1 - p2) + p3 - p0) * t * t * t)
}

/// What the disk writer hands to the main thread when a take ends.
pub struct Finished {
    pub take: u32,
    pub track: usize,
    pub start_frame: i64,
    pub path: Result<PathBuf, String>,
    pub audio: Arc<AudioData>,
}

/// Writes a stereo 32-bit float WAV as samples arrive, so even a crash
/// mid-take leaves the audio recorded so far on disk. The sizes in the
/// header are filled in by `finish`.
pub struct WavWriter {
    file: BufWriter<File>,
    frames: u32,
}

impl WavWriter {
    pub fn create(path: &Path, sample_rate: u32) -> io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let channels: u16 = 2;
        let block_align = channels * 4;
        file.write_all(b"RIFF\0\0\0\0WAVE")?; // RIFF size patched later
        // "fmt " for IEEE float: 18 bytes, with an empty extension (cbSize).
        file.write_all(b"fmt ")?;
        file.write_all(&18u32.to_le_bytes())?;
        file.write_all(&3u16.to_le_bytes())?; // WAVE_FORMAT_IEEE_FLOAT
        file.write_all(&channels.to_le_bytes())?;
        file.write_all(&sample_rate.to_le_bytes())?;
        file.write_all(&(sample_rate * block_align as u32).to_le_bytes())?;
        file.write_all(&block_align.to_le_bytes())?;
        file.write_all(&32u16.to_le_bytes())?;
        file.write_all(&0u16.to_le_bytes())?;
        // Non-PCM formats should carry a "fact" chunk with the frame count.
        file.write_all(b"fact")?;
        file.write_all(&4u32.to_le_bytes())?;
        file.write_all(&0u32.to_le_bytes())?; // patched later
        file.write_all(b"data\0\0\0\0")?; // patched later
        Ok(Self { file, frames: 0 })
    }

    pub fn write(&mut self, left: &[f32], right: &[f32]) -> io::Result<()> {
        for (l, r) in left.iter().zip(right) {
            self.file.write_all(&l.to_le_bytes())?;
            self.file.write_all(&r.to_le_bytes())?;
        }
        self.frames += left.len() as u32;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        let data_bytes = self.frames * 8;
        self.file.flush()?;
        let mut f = self.file.into_inner().map_err(|e| e.into_error())?;
        // Header layout: RIFF(12) + fmt(8+18) + fact(8+4) + data(8).
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(4 + 26 + 12 + 8 + data_bytes).to_le_bytes())?;
        f.seek(SeekFrom::Start(12 + 26 + 8))?;
        f.write_all(&self.frames.to_le_bytes())?;
        f.seek(SeekFrom::Start(12 + 26 + 12 + 4))?;
        f.write_all(&data_bytes.to_le_bytes())?;
        f.sync_all()
    }
}

struct Take {
    take: u32,
    track: usize,
    start_frame: i64,
    path: PathBuf,
    wav: Option<WavWriter>,
    error: Option<String>,
    left: Vec<f32>,
    right: Vec<f32>,
}

/// The disk writer: an ordinary thread that polls the ring. It can't be
/// woken by the engine, since signalling a thread isn't real-time safe, so
/// it sleeps briefly between polls. The ring holds seconds of audio, far
/// more than a few milliseconds of sleep.
pub fn spawn_writer(mut events: Consumer<RecEvent>, dir: PathBuf, sample_rate: u32, done: mpsc::Sender<Finished>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut current: Option<Take> = None;
        loop {
            while let Ok(ev) = events.pop() {
                match ev {
                    RecEvent::Begin { take, track, start_frame } => {
                        let path = dir.join(format!("take-{take:03}.wav"));
                        let (wav, error) = match std::fs::create_dir_all(&dir).and_then(|_| WavWriter::create(&path, sample_rate)) {
                            Ok(w) => (Some(w), None),
                            Err(e) => (None, Some(format!("couldn't write {}: {e}", path.display()))),
                        };
                        current = Some(Take { take, track, start_frame, path, wav, error, left: Vec::new(), right: Vec::new() });
                    }
                    RecEvent::Samples { len, left, right } => {
                        if let Some(t) = current.as_mut() {
                            t.left.extend_from_slice(&left[..len]);
                            t.right.extend_from_slice(&right[..len]);
                            if let Some(wav) = t.wav.as_mut() {
                                if let Err(e) = wav.write(&left[..len], &right[..len]) {
                                    t.error = Some(format!("writing {} failed: {e}", t.path.display()));
                                    t.wav = None;
                                }
                            }
                        }
                    }
                    RecEvent::End { take } => {
                        if let Some(t) = current.take().filter(|t| t.take == take) {
                            let mut error = t.error;
                            if let Some(wav) = t.wav {
                                if let Err(e) = wav.finish() {
                                    error = Some(format!("finishing {} failed: {e}", t.path.display()));
                                }
                            }
                            let name = t.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            let path = match error {
                                None => Ok(t.path),
                                Some(e) => Err(e),
                            };
                            // Remember the file, unless writing it failed.
                            let file = path.as_ref().ok().cloned();
                            let audio = Arc::new(AudioData { name, path: file, sample_rate: sample_rate as f64, channels: vec![t.left, t.right] });
                            let _ = done.send(Finished { take: t.take, track: t.track, start_frame: t.start_frame, path, audio });
                        }
                    }
                }
            }
            if events.is_abandoned() {
                // The engine is gone (the app is quitting). If a take was
                // still running, finish its file so it isn't left corrupt.
                if let Some(wav) = current.and_then(|t| t.wav) {
                    let _ = wav.finish();
                }
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    #[test]
    fn take_start_subtracts_every_delay() {
        // Playhead at 48_000; 300 frames waiting; 128 in, 512 out.
        assert_eq!(take_start(48_000, 300, 128, 512, 0), 48_000 - 940);
        assert_eq!(take_start(48_000, 300, 128, 512, 60), 48_000 - 1_000, "manual adjustment adds to it");
        assert!(take_start(100, 300, 0, 0, 0) < 0, "can start before the song does");
    }

    #[test]
    fn wav_round_trips_through_the_loader() {
        let path = std::env::temp_dir().join("daw_record_test.wav");
        let left: Vec<f32> = (0..1000).map(|i| (i as f32 / 1000.0) * 1.5 - 0.75).collect();
        let right: Vec<f32> = left.iter().map(|x| -x).collect();
        let mut w = WavWriter::create(&path, 48_000).unwrap();
        w.write(&left[..600], &right[..600]).unwrap();
        w.write(&left[600..], &right[600..]).unwrap();
        w.finish().unwrap();

        let audio = crate::loader::load_audio(&path).unwrap();
        assert_eq!(audio.sample_rate, 48_000.0);
        assert_eq!(audio.frames(), 1000);
        assert_eq!(audio.channels[0], left, "float WAV: bit-exact");
        assert_eq!(audio.channels[1], right);
    }

    #[test]
    fn writer_thread_turns_events_into_a_finished_take() {
        let dir = std::env::temp_dir().join("daw_record_writer_test");
        let (mut tx, rx) = RingBuffer::<RecEvent>::new(64);
        let (done_tx, done_rx) = mpsc::channel();
        let writer = spawn_writer(rx, dir.clone(), 48_000, done_tx);

        let mut left = [0.0; CHUNK];
        left[0] = 0.5;
        let right = [0.25; CHUNK];
        assert!(tx.push(RecEvent::Begin { take: 7, track: 2, start_frame: -40 }).is_ok());
        for _ in 0..3 {
            assert!(tx.push(RecEvent::Samples { len: CHUNK, left, right }).is_ok());
        }
        assert!(tx.push(RecEvent::Samples { len: 10, left, right }).is_ok());
        assert!(tx.push(RecEvent::End { take: 7 }).is_ok());

        let f = done_rx.recv_timeout(Duration::from_secs(5)).expect("take finished");
        assert_eq!((f.take, f.track, f.start_frame), (7, 2, -40));
        assert_eq!(f.audio.frames(), 3 * CHUNK + 10);
        assert_eq!(f.audio.channels[0][CHUNK], 0.5);
        let on_disk = crate::loader::load_audio(f.path.as_ref().unwrap()).unwrap();
        assert_eq!(on_disk.channels, f.audio.channels, "the file matches what was handed over");

        drop(tx); // the engine goes away...
        writer.join().unwrap(); // ...and the writer finishes cleanly
    }

    #[test]
    fn drift_estimate_sees_through_the_wobble() {
        // A true drift of +40 ppm, buried in +/-512 frames of burst jitter,
        // read 60 times a second for two minutes.
        let mut est = DriftEstimator::default();
        let mut seed: u32 = 99;
        for k in 0..60 * 120 {
            let out = k as u64 * 800;
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let jitter = (seed >> 22) as i64 - 512;
            let inp = (out as f64 * (1.0 + 40e-6)) as i64 + jitter + 1_000;
            est.add(inp.max(0) as u64, out);
        }
        let ppm = est.ppm(48_000.0 * 10.0).unwrap();
        assert!((ppm - 40.0).abs() < 6.0, "estimated {ppm:.1} ppm");
        // The naive two-point estimate, for comparison, can be far off:
        // 512 frames of jitter over 5.76 M frames is +/-89 ppm.
    }

    #[test]
    fn drift_needs_enough_history() {
        let mut est = DriftEstimator::default();
        est.add(0, 0);
        est.add(512, 512);
        est.add(1024, 1024);
        assert_eq!(est.ppm(48_000.0), None, "too short to say");
    }

    /// The input device: `drift_ppm` faster than the output, delivering in
    /// 256-frame bursts. Returns (correction in ppm over the last half,
    /// fill min, fill max, underruns after priming).
    fn run_corrector(drift_ppm: f64, callbacks: usize) -> (f64, usize, usize, u32) {
        let mut c = DriftCorrector::new(65_536);
        let (mut produced, mut delivered) = (0.0f64, 0usize);
        let (mut out_l, mut out_r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
        let (mut ppm_sum, mut ppm_n, mut lo, mut hi) = (0.0, 0, usize::MAX, 0);
        let mut primed_underruns = 0;
        for k in 0..callbacks {
            produced += 512.0 * (1.0 + drift_ppm * 1e-6);
            let burst = (produced as usize - delivered) / 256 * 256;
            let frames: Vec<f32> = (delivered..delivered + burst).map(|i| (i as f32 * 0.001).sin()).collect();
            c.push(&frames, &frames);
            delivered += burst;
            let before = c.underruns;
            c.pull(&mut out_l, &mut out_r);
            if k > 100 {
                primed_underruns += c.underruns - before;
            }
            if k >= callbacks / 2 {
                ppm_sum += c.ppm();
                ppm_n += 1;
                lo = lo.min(c.len);
                hi = hi.max(c.len);
            }
        }
        (ppm_sum / ppm_n as f64, lo, hi, primed_underruns)
    }

    #[test]
    fn drift_correction_finds_the_clock_ratio_and_holds_the_buffer_steady() {
        // ~45 simulated minutes each: the controller is gentle on purpose.
        for drift in [100.0, 20.0, -50.0] {
            let (ppm, lo, hi, underruns) = run_corrector(drift, 250_000);
            assert!((ppm - drift).abs() < 0.15 * drift.abs() + 2.0, "drift {drift}: corrected {ppm:.1} ppm");
            assert_eq!(underruns, 0, "drift {drift}: ran dry");
            assert!(hi < 4_000 && lo > 0, "drift {drift}: buffer {lo}..{hi} wandered");
        }
    }

    #[test]
    fn cubic_interpolation_is_exact_at_the_samples_and_smooth_between() {
        assert_eq!(cubic(1.0, 2.0, 3.0, 4.0, 0.0), 2.0);
        assert_eq!(cubic(1.0, 2.0, 3.0, 4.0, 1.0), 3.0);
        assert!((cubic(1.0, 2.0, 3.0, 4.0, 0.5) - 2.5).abs() < 1e-6, "a straight line stays straight");
    }
}
