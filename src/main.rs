#[cfg(test)]
mod bench;
mod automation;
mod buffer;
mod clap;
mod cli;
mod clip;
mod denormal;
mod engine;
mod loader;
mod midi;
mod mixer;
mod nodes;
mod processor;
mod record;
mod rtcheck;
mod project;
mod session;
mod smoothing;
mod synth;
mod transport;
mod ui;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use record::{RecEvent, RecordShared};
use rtrb::{Consumer, Producer};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use engine::{Command, Engine, Garbage};
use midi::LiveMidi;
use mixer::{Meters, MAX_SLOTS, MAX_TRACKS, MIX_CHANNELS};
use processor::PrepareInfo;
use rtrb::RingBuffer;
use session::Session;
use transport::TransportShared;

const MAX_BLOCK: usize = 512;

/// In tests and debug builds, every allocation is checked against the
/// audio thread's no-allocation rule (see rtcheck.rs).
#[cfg(any(test, debug_assertions))]
#[global_allocator]
static ALLOCATOR: rtcheck::CheckingAllocator = rtcheck::CheckingAllocator;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `cargo run --release` opens the window; `cargo run --release -- --cli`
    // gives the command line from the earlier steps.
    let use_cli = std::env::args().any(|a| a == "--cli");

    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device available")?;
    let supported = device.default_output_config()?;
    println!("Device: {}", device.name()?);

    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    match format {
        SampleFormat::F32 => run::<f32>(&host, &device, &config, use_cli),
        SampleFormat::I16 => run::<i16>(&host, &device, &config, use_cli),
        SampleFormat::U16 => run::<u16>(&host, &device, &config, use_cli),
        other => Err(format!("unsupported sample format: {other:?}").into()),
    }
}

/// An open input device: its stream (which must stay alive) and the ring
/// its callback writes into.
struct OpenInput {
    stream: cpal::Stream,
    samples: Consumer<f32>,
    channels: usize,
    name: String,
}

/// Opens the default input device at the output's sample rate.
fn open_input(host: &cpal::Host, sample_rate: u32, shared: Arc<RecordShared>) -> Result<OpenInput, String> {
    let device = host.default_input_device().ok_or("no input device")?;
    let name = device.name().unwrap_or_else(|_| "input".into());
    // Recording needs the input at the SAME rate as the output: resampling
    // the input isn't supported yet. Prefer stereo (or mono) float formats.
    let chosen = device
        .supported_input_configs()
        .map_err(|e| e.to_string())?
        .filter(|c| c.min_sample_rate().0 <= sample_rate && sample_rate <= c.max_sample_rate().0)
        .max_by_key(|c| (c.channels() <= 2, c.sample_format() == SampleFormat::F32, c.channels() == 2))
        .ok_or_else(|| format!("{name} can't run at {sample_rate} Hz, so recording is unavailable"))?
        .with_sample_rate(cpal::SampleRate(sample_rate));
    let channels = chosen.channels() as usize;
    let format = chosen.sample_format();
    let config: cpal::StreamConfig = chosen.into();
    // About a second of audio, in whole frames (a multiple of `channels`).
    let (producer, samples) = RingBuffer::<f32>::new(sample_rate as usize * channels);
    let stream = match format {
        SampleFormat::F32 => build_input::<f32>(&device, &config, producer, shared),
        SampleFormat::I16 => build_input::<i16>(&device, &config, producer, shared),
        SampleFormat::U16 => build_input::<u16>(&device, &config, producer, shared),
        other => return Err(format!("unsupported input sample format: {other:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(OpenInput { stream, samples, channels, name })
}

/// The input callback: convert to f32 and push whole frames into the ring.
/// Like the output callback, it runs on a real-time thread: no locks, no
/// allocation, no waiting.
fn build_input<T>(device: &cpal::Device, config: &cpal::StreamConfig, mut producer: Producer<f32>, shared: Arc<RecordShared>) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let sample_rate = config.sample_rate.0 as f64;
    device
        .build_input_stream(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                // How long ago this audio was captured, if the driver says.
                let ts = info.timestamp();
                if let Some(d) = ts.callback.duration_since(&ts.capture) {
                    shared.input_latency.store((d.as_secs_f64() * sample_rate) as u32, Relaxed);
                }
                // If the engine has fallen behind, the rest is dropped.
                let n = producer.slots().min(data.len()) / channels * channels;
                if let Ok(chunk) = producer.write_chunk_uninit(n) {
                    chunk.fill_from_iter(data.iter().map(|&s| <f32 as FromSample<T>>::from_sample_(s)));
                }
            },
            |err| eprintln!("input stream error: {err}"),
            None,
        )
        .map_err(|e| e.to_string())
}

fn run<T>(host: &cpal::Host, device: &cpal::Device, config: &cpal::StreamConfig, use_cli: bool) -> Result<(), Box<dyn std::error::Error>>
where
    T: SizedSample + FromSample<f32>,
{
    // Nodes always see a stereo mix; the engine maps it to the device.
    let info = PrepareInfo { sample_rate: config.sample_rate.0 as f32, max_frames: MAX_BLOCK, channels: MIX_CHANNELS };

    let (cmd_tx, cmd_rx) = RingBuffer::<Command>::new(1024);
    // Big enough to hold every node of every track at once.
    let (garbage_tx, garbage_rx) = RingBuffer::<Garbage>::new(MAX_TRACKS * (MAX_SLOTS + 1) * 4);
    let (live_tx, live_rx) = RingBuffer::<LiveMidi>::new(1024);
    let shared = TransportShared::new();
    let meters = Meters::new();

    let mut engine = Engine::new(&info, config.channels as usize, shared.clone(), meters.clone(), cmd_rx, garbage_tx, live_rx);

    // Recording: open the input, connect it to the engine, and start the
    // disk writer. Without an input device, everything else still works.
    let rec_shared = RecordShared::new();
    let mut recorder = None;
    let mut writer = None;
    let input = match open_input(host, config.sample_rate.0, rec_shared.clone()) {
        Ok(input) => Some(input),
        Err(e) => {
            println!("Recording unavailable: {e}");
            None
        }
    };
    let input_stream = input.map(|input| {
        println!("Input: {} ({} ch)", input.name, input.channels);
        // Room for ~5 s of audio while the disk is busy.
        let (rec_tx, rec_rx) = RingBuffer::<RecEvent>::new(1024);
        engine.set_input(input.samples, input.channels, rec_tx, rec_shared.clone());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        writer = Some(record::spawn_writer(rec_rx, "recordings".into(), config.sample_rate.0, done_tx));
        recorder = Some(session::Recorder { device: input.name, done: done_rx, shared: rec_shared.clone() });
        input.stream
    });

    let sample_rate = config.sample_rate.0 as f64;
    let out_shared = rec_shared.clone();
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            // How long until this audio is heard, if the driver says.
            let ts = info.timestamp();
            if let Some(d) = ts.playback.duration_since(&ts.callback) {
                out_shared.output_latency.store((d.as_secs_f64() * sample_rate) as u32, Relaxed);
            }
            engine.render(data)
        },
        |err| eprintln!("stream error: {err}"),
        None,
    )?;
    stream.play()?;

    let mut session = Session::new(info, cmd_tx, garbage_rx, shared, meters, live_tx);
    if let Some(r) = recorder {
        session.attach_recorder(r);
    }
    if use_cli {
        cli::run(&mut session)?;
    } else {
        ui::run(session)?;
    }
    // The streams live until here, so audio keeps running for as long as
    // either front end does. Dropping the output stream drops the engine,
    // which tells the disk writer to finish up; then wait for it.
    drop(stream);
    drop(input_stream);
    if let Some(w) = writer {
        let _ = w.join();
    }
    Ok(())
}
