//! Offline benchmarks: render a heavy project without an audio device.
//!
//! Run with:  cargo test --release bench -- --ignored --nocapture
//!
//! `bench_heavy_project` saves its output to /tmp/daw_golden.bin on the first
//! run and compares against it afterwards, so an optimization that changes
//! the sound fails loudly. Delete the file to accept a new reference.
//! For an instruction-level profile, run `profile_full` under Valgrind:
//!   valgrind --tool=callgrind <test binary> profile_full --ignored

use crate::clip::{AudioClip, ClipList, ClipParams};
use crate::engine::{Command, Engine, Garbage};
use crate::loader::AudioData;
use crate::midi::{LiveMidi, Note, Sequence};
use crate::mixer::{Meters, Target, Track, MIX_CHANNELS};
use crate::nodes;
use crate::processor::{Instrument, PrepareInfo, Processor};
use crate::synth::Synth;
use crate::transport::TransportShared;
use rtrb::RingBuffer;
use std::sync::Arc;
use std::time::Instant;

pub const SR: f32 = 48_000.0;
pub const BLOCK: usize = 512;
pub const INFO: PrepareInfo = PrepareInfo { sample_rate: SR, max_frames: BLOCK, channels: MIX_CHANNELS };

/// Deterministic noise, so every run renders identical audio.
fn noise(frames: usize, seed: u32) -> Vec<f32> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..frames)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32) * 0.4 - 0.2
        })
        .collect()
}

/// A dense pattern that keeps all 16 voices busy: 8-note chords held for a
/// bar, plus 16th notes on top.
fn dense_notes(bars: usize) -> Vec<Note> {
    let mut notes = Vec::new();
    for bar in 0..bars {
        let b = bar as f64 * 4.0;
        for k in 0..8u8 {
            notes.push(Note { start: b, length: 3.9, pitch: 36 + k * 5, velocity: 80 });
        }
        for step in 0..16 {
            notes.push(Note { start: b + step as f64 * 0.25, length: 0.2, pitch: 72 + (step % 7) as u8, velocity: 90 });
        }
    }
    notes
}

pub struct Project {
    pub engine: Engine,
    _keep: (rtrb::Consumer<Garbage>, rtrb::Producer<LiveMidi>),
}

/// 8 audio tracks (long stereo clip, lowpass, gain, panned, send) and
/// 4 synth tracks (dense chords), an aux with a filter, playing.
pub fn heavy_project() -> Project {
    project(Parts { audio: true, synths: true, effects: true })
}

#[derive(Clone, Copy)]
pub struct Parts {
    pub audio: bool,
    pub synths: bool,
    pub effects: bool,
}

pub fn project(parts: Parts) -> Project {
    let (mut tx, rx) = RingBuffer::<Command>::new(1024);
    let (gtx, grx) = RingBuffer::<Garbage>::new(1024);
    let (ltx, lrx) = RingBuffer::<LiveMidi>::new(16);
    let engine = Engine::new(&INFO, 2, TransportShared::new(), Meters::new(), rx, gtx, lrx);
    let mut send = |c: Command| assert!(tx.push(c).is_ok());

    for t in 0..12 {
        send(Command::AddTrack { index: t, track: Box::new(Track::new(&INFO)) });
        send(Command::SetPan { track: t, pos: (t as f32 / 11.0) * 1.6 - 0.8 });
        send(Command::SetSend { track: t, db: -12.0 });
        send(Command::SetVolume { target: Target::Track(t), db: -9.0 });

        if parts.effects {
            let mut lp: Box<dyn Processor> = Box::new(nodes::Lowpass::new());
            lp.prepare(&INFO);
            lp.set_param(0, 2_000.0 + t as f32 * 500.0);
            send(Command::Insert { target: Target::Track(t), slot: 0, node: lp });
            let mut gain: Box<dyn Processor> = Box::new(nodes::Gain::new());
            gain.prepare(&INFO);
            send(Command::Insert { target: Target::Track(t), slot: 1, node: gain });
        }

        if t < 8 && parts.audio {
            let frames = 48_000 * 70;
            let data = Arc::new(AudioData {
                path: None,
                name: "noise".into(),
                sample_rate: 44_100.0, // forces resampling, the realistic case
                channels: vec![noise(frames, t as u32 + 1), noise(frames, t as u32 + 100)],
            });
            let clip = AudioClip::new(data, ClipParams::new(1, 0.0, 200.0), &INFO);
            send(Command::SetClips { track: t, clips: Box::new(ClipList::new(vec![clip])) });
        } else if t >= 8 && parts.synths {
            let mut synth: Box<dyn Instrument> = Box::new(Synth::new());
            synth.prepare(&INFO);
            synth.set_param(0, (t % 3) as f32); // a mix of sine, saw, square
            send(Command::SetInstrument { track: t, instrument: Some(synth) });
            let notes = dense_notes(40);
            let seq = Sequence::build([(0.0, 160.0, &notes[..])]);
            send(Command::SetSequence { track: t, sequence: Box::new(seq) });
        }
    }
    let mut lp: Box<dyn Processor> = Box::new(nodes::Lowpass::new());
    lp.prepare(&INFO);
    lp.set_param(0, 800.0);
    send(Command::Insert { target: Target::Aux, slot: 0, node: lp });
    send(Command::Play);
    Project { engine, _keep: (grx, ltx) }
}

/// Renders `seconds` of audio; returns (output, wall-clock seconds).
pub fn render(project: &mut Project, seconds: usize) -> (Vec<f32>, f64) {
    let blocks = seconds * SR as usize / BLOCK;
    let mut out = vec![0f32; BLOCK * 2];
    let mut all = Vec::with_capacity(blocks * BLOCK * 2);
    let start = Instant::now();
    for _ in 0..blocks {
        project.engine.render(&mut out);
        all.extend_from_slice(&out);
    }
    (all, start.elapsed().as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = "/tmp/daw_golden.bin";

    fn ms_per_block(parts: Parts) -> f64 {
        let mut p = project(parts);
        render(&mut p, 1);
        let (_, wall) = render(&mut p, 10);
        wall * 1000.0 / (10.0 * SR as f64 / BLOCK as f64)
    }

    #[test]
    #[ignore]
    fn bench_clip_reader_alone() {
        use crate::buffer::AudioBuffer;
        use crate::transport::Transport;
        let frames = 48_000 * 30;
        let data = Arc::new(AudioData { path: None, name: "n".into(), sample_rate: 44_100.0, channels: vec![noise(frames, 1), noise(frames, 2)] });
        let mut list = ClipList::new(vec![AudioClip::new(data, ClipParams::new(1, 0.0, 100.0), &INFO)]);
        let mut t = Transport::new(SR as f64);
        t.playing = true;
        let mut buf = AudioBuffer::new(2, BLOCK);
        let blocks = 2000;
        let start = Instant::now();
        let mut sink = 0.0;
        for _ in 0..blocks {
            buf.clear();
            list.render(&mut buf, &t.context());
            sink += buf.channel(0)[7];
            t.advance(BLOCK);
        }
        println!("\n  clip reader: {:.4} ms per block per clip  ({sink:.1})", start.elapsed().as_secs_f64() * 1000.0 / blocks as f64);
    }




    /// Reads (and clears) the CPU's sticky floating-point flags: bit 1 means
    /// some operation had a denormal input, bit 4 that one underflowed to a
    /// denormal. Lets us check for denormals without changing the code
    /// being measured.
    #[cfg(target_arch = "x86_64")]
    fn take_fp_flags() -> (bool, bool) {
        let mut csr: u32 = 0;
        unsafe {
            std::arch::asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack, preserves_flags));
            let cleared = csr & !0x3F;
            std::arch::asm!("ldmxcsr [{}]", in(reg) &cleared, options(nostack, preserves_flags, readonly));
        }
        (csr & 0x02 != 0, csr & 0x10 != 0)
    }

    #[test]
    #[ignore]
    #[cfg(target_arch = "x86_64")]
    fn bench_no_denormals_on_the_audio_path() {
        let parts = Parts { audio: true, synths: false, effects: false };
        let mut p = project(parts);
        render(&mut p, 1);
        take_fp_flags();
        let mut out = vec![0f32; BLOCK * 2];
        let mut first = None;
        for block in 0..2000 {
            p.engine.render(&mut out);
            let (denormal_in, underflow) = take_fp_flags();
            if (denormal_in || underflow) && first.is_none() {
                first = Some((block, denormal_in, underflow));
            }
        }
        assert_eq!(first, None, "denormal operations on the audio path (block, input, underflow)");
    }




    #[test]
    #[ignore]
    fn profile_full() {
        let mut p = project(Parts { audio: true, synths: true, effects: true });
        render(&mut p, 3);
    }

    #[test]
    #[ignore]
    fn bench_synth_waves() {
        use crate::buffer::AudioBuffer;
        use crate::midi::{MidiEvent, MidiMessage};
        use crate::transport::Transport;
        println!();
        for (wave, name) in [(0.0, "sine"), (1.0, "saw"), (2.0, "square")] {
            let mut synth = Synth::new();
            synth.prepare(&INFO);
            synth.set_param(0, wave);
            let ctx = Transport::new(SR as f64).context();
            let mut buf = AudioBuffer::new(2, BLOCK);
            let notes: Vec<MidiEvent> = (0..16).map(|k| MidiEvent { frame: 0, msg: MidiMessage::NoteOn { note: 40 + k * 3, velocity: 100 } }).collect();
            synth.render(&mut buf, &ctx, &notes);
            let blocks = 3000;
            let start = Instant::now();
            for _ in 0..blocks {
                buf.clear();
                synth.render(&mut buf, &ctx, &[]);
            }
            let ns = start.elapsed().as_secs_f64() * 1e9 / (blocks * BLOCK * 16) as f64;
            println!("  {name:<7} {ns:.2} ns per voice-sample");
        }
    }

    #[test]
    #[ignore]
    fn bench_breakdown() {
        let none = Parts { audio: false, synths: false, effects: false };
        let rows = [
            ("12 empty tracks (mixer only)", none),
            ("+ effects on every track", Parts { effects: true, ..none }),
            ("8 audio clip tracks", Parts { audio: true, ..none }),
            ("4 synth tracks", Parts { synths: true, ..none }),
        ];
        println!();
        for (name, parts) in rows {
            println!("  {:>8.4} ms  {name}", ms_per_block(parts));
        }
    }

    #[test]
    #[ignore]
    fn bench_heavy_project() {
        let mut p = heavy_project();
        render(&mut p, 1); // warm up
        let seconds = 20;
        let (out, wall) = render(&mut p, seconds);
        let budget_ms = BLOCK as f64 / SR as f64 * 1000.0;
        let per_block_ms = wall * 1000.0 / (seconds as f64 * SR as f64 / BLOCK as f64);
        println!(
            "\n{seconds}s of audio in {wall:.3}s = {:.1}x real time | {per_block_ms:.4} ms per block ({:.1}% of the {budget_ms:.2} ms budget)",
            seconds as f64 / wall,
            per_block_ms / budget_ms * 100.0
        );

        // Golden comparison: the first run saves, later runs compare.
        let bytes: Vec<u8> = out.iter().flat_map(|s| s.to_le_bytes()).collect();
        match std::fs::read(GOLDEN) {
            Err(_) => {
                std::fs::write(GOLDEN, &bytes).unwrap();
                println!("saved golden output");
            }
            Ok(golden) => {
                let g: Vec<f32> = golden.chunks(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
                let max_diff = g.iter().zip(&out).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
                println!("max difference from golden: {max_diff:.2e}");
                assert!(max_diff < 1e-4, "output changed audibly");
            }
        }
    }

    /// The cost of each effect, per stereo sample.
    #[test]
    #[ignore]
    fn bench_effects() {
        use crate::buffer::AudioBuffer;
        use crate::transport::Transport;
        let _guard = crate::denormal::NoDenormals::new();
        let ctx = Transport::new(SR as f64).context();
        let input = noise(BLOCK, 7);
        let cases: Vec<(&str, Box<dyn Fn() -> Box<dyn Processor>>)> = vec![
            ("gain (settled)", Box::new(|| Box::new(nodes::Gain::new()))),
            ("lp one-pole", Box::new(|| Box::new(nodes::Lowpass::new()))),
            ("filter (settled)", Box::new(|| Box::new(nodes::Biquad::new()))),
            ("delay", Box::new(|| Box::new(nodes::Delay::new()))),
            ("comp", Box::new(|| Box::new(nodes::Compressor::new()))),
            ("reverb", Box::new(|| Box::new(nodes::Reverb::new()))),
        ];
        println!();
        for (name, make) in cases {
            let mut node = make();
            node.prepare(&INFO);
            if name == "gain (settled)" {
                node.set_param(0, -6.0);
            }
            let mut buf = AudioBuffer::new(2, BLOCK);
            let blocks = 4000;
            let run = |node: &mut Box<dyn Processor>, buf: &mut AudioBuffer, n: usize| {
                for _ in 0..n {
                    buf.channel_mut(0).copy_from_slice(&input);
                    buf.channel_mut(1).copy_from_slice(&input);
                    node.process(buf, &ctx);
                }
            };
            run(&mut node, &mut buf, 200); // let smoothers settle
            let start = Instant::now();
            run(&mut node, &mut buf, blocks);
            let ns = start.elapsed().as_secs_f64() * 1e9 / (blocks * BLOCK) as f64;
            println!("  {name:<18} {ns:6.2} ns per stereo sample  ({:.4} ms per block)", ns * BLOCK as f64 / 1e6);
        }
    }

    /// Why the denormal guard exists: a reverb tail decaying in silence
    /// passes through denormals, as feedback loops all over audio code do.
    /// Returns (saw a denormal input, saw an underflow, seconds taken).
    #[cfg(target_arch = "x86_64")]
    fn reverb_tail(guarded: bool) -> (bool, bool, f64) {
        use crate::buffer::AudioBuffer;
        use crate::transport::Transport;
        let ctx = Transport::new(SR as f64).context();
        let _guard = guarded.then(crate::denormal::NoDenormals::new);
        let mut r = nodes::Reverb::new();
        r.prepare(&INFO);
        r.set_param(0, 0.2);
        r.set_param(3, 1.0);
        let mut buf = AudioBuffer::new(2, BLOCK);
        let (mut denormal_in, mut underflow) = (false, false);
        let start = Instant::now();
        // An impulse, then 60 s of silence while the tail decays away.
        for block in 0..(60 * SR as usize / BLOCK) {
            buf.clear();
            if block == 0 {
                buf.channel_mut(0)[0] = 1.0;
            }
            take_fp_flags();
            r.process(&mut buf, &ctx);
            let flags = take_fp_flags();
            denormal_in |= flags.0;
            underflow |= flags.1;
        }
        (denormal_in, underflow, start.elapsed().as_secs_f64())
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn reverb_tail_needs_the_denormal_guard() {
        let (denormal_in, _, _) = reverb_tail(false);
        assert!(denormal_in, "expected the decaying tail to feed denormals into the math");
        // With the guard, results that would be denormal are flushed to
        // zero. The CPU still raises its underflow flag when it does that,
        // but no operation ever receives a denormal input: the slow path.
        let (denormal_in, underflow, _) = reverb_tail(true);
        assert!(!denormal_in, "the guard should keep denormals out of every operation");
        assert!(underflow, "(flushing is how underflow is handled, so this flag is expected)");
    }

    #[test]
    #[ignore]
    #[cfg(target_arch = "x86_64")]
    fn bench_reverb_tail_with_and_without_guard() {
        let (_, _, off) = reverb_tail(false);
        let (_, _, on) = reverb_tail(true);
        println!("\n  60 s of reverb tail: {off:.3} s without the guard, {on:.3} s with it ({:.1}x)", off / on);
    }

    /// Step 8, exercise 3: one Eq3 node vs three Biquad nodes.
    #[test]
    #[ignore]
    fn bench_eq_designs() {
        use crate::buffer::AudioBuffer;
        use crate::transport::Transport;
        let _guard = crate::denormal::NoDenormals::new();
        let ctx = Transport::new(SR as f64).context();
        let input = noise(BLOCK, 3);
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let bands = [(5.0, 120.0, q), (4.0, 1_000.0, 1.0), (6.0, 8_000.0, q)];

        // `sweep`: keep moving the mid frequency, so it never settles.
        let time = |nodes: &mut Vec<Box<dyn Processor>>, sweep: Option<(usize, usize)>| {
            let mut buf = AudioBuffer::new(2, BLOCK);
            let blocks = 4000;
            let mut elapsed = 0.0;
            for block in 0..blocks + 200 {
                buf.channel_mut(0).copy_from_slice(&input);
                buf.channel_mut(1).copy_from_slice(&input);
                if let Some((node, param)) = sweep {
                    nodes[node].set_param(param, if block % 2 == 0 { 800.0 } else { 1_200.0 });
                }
                let start = Instant::now();
                for n in nodes.iter_mut() {
                    n.process(&mut buf, &ctx);
                }
                if block >= 200 {
                    elapsed += start.elapsed().as_secs_f64();
                }
            }
            elapsed * 1e9 / (blocks * BLOCK) as f64
        };
        let one = |gains: [f32; 3]| -> Vec<Box<dyn Processor>> {
            let mut e: Box<dyn Processor> = Box::new(nodes::Eq3::new());
            e.prepare(&INFO);
            for (param, g) in [1, 3, 6].into_iter().zip(gains) {
                e.set_param(param, g);
            }
            vec![e]
        };
        let three = |gains: [f32; 3]| -> Vec<Box<dyn Processor>> {
            bands
                .iter()
                .zip(gains)
                .map(|(&(kind, freq, q), g)| {
                    let mut b: Box<dyn Processor> = Box::new(nodes::Biquad::new());
                    b.prepare(&INFO);
                    b.set_param(0, kind);
                    b.set_param(1, freq);
                    b.set_param(2, q);
                    b.set_param(3, g);
                    b
                })
                .collect()
        };
        let boost = [4.0, -6.0, 3.0];
        println!();
        println!("  {:<34} {:>10} {:>12}", "ns per stereo sample", "one Eq3", "3 Biquads");
        let rows: [(&str, [f32; 3], Option<(usize, usize)>, Option<(usize, usize)>); 3] = [
            ("all three bands active, settled", boost, None, None),
            ("flat (all bands at 0 dB)", [0.0; 3], None, None),
            ("mid frequency moving", boost, Some((0, 2)), Some((1, 1))),
        ];
        for (name, gains, sweep_one, sweep_three) in rows {
            let a = time(&mut one(gains), sweep_one);
            let b = time(&mut three(gains), sweep_three);
            println!("  {name:<34} {a:>10.2} {b:>12.2}");
        }
    }
}
