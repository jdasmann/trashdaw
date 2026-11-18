#[cfg(test)]
mod bench;
mod automation;
mod buffer;
mod cli;
mod clip;
mod denormal;
mod engine;
mod loader;
mod midi;
mod mixer;
mod nodes;
mod processor;
mod project;
mod session;
mod smoothing;
mod synth;
mod transport;
mod ui;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use engine::{Command, Engine, Garbage};
use midi::LiveMidi;
use mixer::{Meters, MAX_SLOTS, MAX_TRACKS, MIX_CHANNELS};
use processor::PrepareInfo;
use rtrb::RingBuffer;
use session::Session;
use transport::TransportShared;

const MAX_BLOCK: usize = 512;

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
        SampleFormat::F32 => run::<f32>(&device, &config, use_cli),
        SampleFormat::I16 => run::<i16>(&device, &config, use_cli),
        SampleFormat::U16 => run::<u16>(&device, &config, use_cli),
        other => Err(format!("unsupported sample format: {other:?}").into()),
    }
}

fn run<T>(device: &cpal::Device, config: &cpal::StreamConfig, use_cli: bool) -> Result<(), Box<dyn std::error::Error>>
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
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| engine.render(data),
        |err| eprintln!("stream error: {err}"),
        None,
    )?;
    stream.play()?;

    let mut session = Session::new(info, cmd_tx, garbage_rx, shared, meters, live_tx);
    if use_cli {
        cli::run(&mut session)?;
    } else {
        ui::run(session)?;
    }
    // `stream` lives until here, so audio keeps running for as long as
    // either front end does.
    drop(stream);
    Ok(())
}
