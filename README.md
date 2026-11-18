# daw

A digital audio workstation in Rust, built step by step: a real-time audio
engine, transport, tracks and mixer, audio and MIDI clips, a polyphonic
synth, and effects, driven from a command line.

## Requirements

- Rust 1.77 or newer (`midir` 0.11 needs it; on an older toolchain, change
  `midir` to `"0.10"` in `Cargo.toml`).
- Linux: ALSA development headers, e.g. `sudo apt install libasound2-dev pkg-config`.
- macOS and Windows: nothing extra.

## Running

    cargo run --release

Type `help` for all commands. A quick session:

    track add Keys
    synth 0
    mclip demo 0 1
    add 0 delay
    set 0 0 beats 0.75
    add a reverb
    send 0 -6
    loop 1 5
    play

Load audio with `load ~/Music/drums.wav`, then `track add`, `clip add 1 0 1`.

## Testing and benchmarks

    cargo test
    cargo test --release bench -- --ignored --nocapture --test-threads=1

The heavy-project benchmark saves its output to `/tmp/daw_golden.bin` on
the first run and fails later runs if the sound changes. Delete that file
to accept a new reference.

## Layout

    src/
      main.rs        command line, device setup, project editing
      engine.rs      the audio thread: commands, render loop, live MIDI timing
      transport.rs   playhead, tempo, loop, sample/beat conversions
      mixer.rs       tracks, aux and master buses, metering
      clip.rs        audio clips and the per-track clip list
      midi.rs        MIDI messages, events, sequence, sequencer
      synth.rs       polyphonic synth (PolyBLEP oscillators, ADSR)
      processor.rs   the Processor (effect) and Instrument traits
      project.rs     the main thread's model of the project
      buffer.rs      planar audio buffer
      smoothing.rs   parameter smoothing
      loader.rs      audio file decoding (symphonia)
      denormal.rs    flush-to-zero guard for the audio thread
      bench.rs       offline benchmarks (tests only)
      nodes/         effects: filter, delay, comp, reverb, gain, lp, pan,
                     osc, click
