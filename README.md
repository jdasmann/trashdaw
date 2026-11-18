# daw

A digital audio workstation in Rust, built step by step: a real-time audio
engine, transport, tracks and mixer, audio and MIDI clips, a polyphonic
synth with a per-voice filter, effects, and a graphical interface.

## Requirements

- Rust 1.77 or newer (`midir` 0.11 needs it; on an older toolchain, change
  `midir` to `"0.10"` in `Cargo.toml`).
- Linux: ALSA development headers, e.g. `sudo apt install libasound2-dev pkg-config`,
  plus a normal desktop (X11 or Wayland, with OpenGL).
- macOS and Windows: nothing extra.

`eframe` (the UI framework) is pinned to 0.27, the version this code was
built and tested against. Newer versions rename a few methods; see the
step 9 notes if you want to upgrade.

## Running

    cargo run --release            # the window
    cargo run --release -- --cli   # the command line from steps 1-8

In the window:

- **+ Track** adds a track. Select it to edit it in the inspector (right).
- **Drop audio files** onto the window to load them, select one in the
  library (left), then **double-click a lane** to place it.
- Tick **synth** in the inspector to give a track an instrument, then
  **+ demo MIDI clip**, or double-click its lane for an empty clip.
- **Drag clips** to move them, including **up or down to another track**;
  drag their **right edge** to resize, and **right-click** to split or
  delete. Hold **Shift** to bypass snapping.
- **Double-click a MIDI clip** (or right-click, "Edit notes") to open the
  **piano roll**: click to add a note, drag to move it in time and pitch,
  drag its right edge to resize it, right-click to delete it.
- **Automation**: in the inspector, pick a parameter under "+ automate...".
  Its lane appears under the track: click to add a point, drag points,
  right-click a point to delete it.
- Click or drag the **ruler** to move the playhead. **Space** plays and
  stops. **Ctrl + scroll** zooms, **Shift + scroll** scrolls sideways.
- The mixer (bottom): fader (double-click for 0 dB), meter (click to clear
  the clip light), pan, **M**ute, **S**olo, **R** to arm for a MIDI
  keyboard, and the aux send.

## Testing and benchmarks

    cargo test
    cargo test --release bench -- --ignored --nocapture --test-threads=1

The UI tests drive real egui frames with synthetic mouse and keyboard
input, without opening a window.

## Layout

    src/
      main.rs        audio device setup; starts the UI or the command line
      session.rs     every main-thread action, shared by the UI and the CLI
      cli.rs         the command-line front end
      ui/            the egui front end: timeline, piano roll, mixer,
                     inspector, meters
      engine.rs      the audio thread: commands, render loop, live MIDI timing
      transport.rs   playhead, tempo, loop, sample/beat conversions
      mixer.rs       tracks, aux and master buses, metering
      clip.rs        audio clips and the per-track clip list
      automation.rs  automation lanes, evaluated on the audio thread
      midi.rs        MIDI messages, sequence, sequencer, note chasing
      synth.rs       polyphonic synth (PolyBLEP oscillators, ADSR, SVF filter)
      processor.rs   the Processor and Instrument traits, parameter info
      project.rs     the main thread's model of the project
      buffer.rs      planar audio buffer
      smoothing.rs   parameter smoothing
      loader.rs      audio file decoding (symphonia)
      denormal.rs    flush-to-zero guard for the audio thread
      bench.rs       offline benchmarks (tests only)
      nodes/         effects: filter, eq, delay, comp, reverb, gain, lp,
                     pan, osc, click
