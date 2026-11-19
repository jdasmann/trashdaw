# daw

A digital audio workstation in Rust, built step by step: a real-time audio
engine, transport, tracks and mixer, audio and MIDI clips, a polyphonic
synth, effects, a graphical interface, recording, projects with undo, and
CLAP plugin hosting.

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
    cargo run --release -- --cli   # the command line

In the window:

- **File** menu: New, Open, Save, Save As. Projects are `.daw.json` files;
  audio stays in its own files, referenced relative to the project when
  it's inside the project's folder, so the folder can be moved or shared
  as a whole. The title bar shows **•** when there are unsaved changes.
- **Edit** menu: **Undo/Redo** (⌘Z / ⇧⌘Z, or Ctrl+Z / Ctrl+Shift+Z /
  Ctrl+Y), and a History list. A slider drag is one undo step.

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
  the clip light), pan, **M**ute, **S**olo, **●** to arm for recording,
  **R** to arm for a MIDI keyboard, and the aux send.
- **Recording**: arm a track with **●**, then play. The take lands as a
  clip when you stop, and is saved in `recordings/`. With a loop, each lap
  becomes its own take, newest on top. The "Audio input" panel (left)
  shows the input level, the latency the driver reports, and a manual
  adjustment for calibrating timing.
- **Punch in/out**: tick **punch** in the transport bar and set its bars
  (shown in red on the ruler). Play from earlier to hear the lead-in; only
  the punch range is recorded, to the exact sample.
- **Monitor input** (Audio input panel) plays your input through the armed
  track and its effects. Use headphones: with speakers it can feed back.
  The panel also measures **clock drift** between input and output.

## Plugins (CLAP)

The DAW hosts CLAP plugins, both effects and instruments. **Scan** in the
left panel (or `plugins scan`) looks in the standard folders:

- Linux: `~/.clap`, `/usr/lib/clap`
- macOS: `~/Library/Audio/Plug-Ins/CLAP`, `/Library/Audio/Plug-Ins/CLAP`
- Windows: `%COMMONPROGRAMFILES%\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP`

plus any folders in `$CLAP_PATH`. Plugin effects then appear in the
inspector's "+ add effect" menu, and instruments in its Instrument menu.
Their parameters work like built-in ones: sliders, automation, undo, and
saving (with each plugin's own state).

`plugins/clap-example` contains two tiny example plugins (a gain and a
beep synth) as their own crate, a known-good plugin to try:

    cd plugins/clap-example && cargo build --release

then copy the built library into a CLAP folder, renamed `Example.clap`
(`target/release/libclap_example.so` on Linux, `.dylib` on macOS,
`clap_example.dll` on Windows).

A plugin runs inside the DAW's process: a crashing plugin takes the DAW
with it, and a slow one causes dropouts. The inspector shows each plugin's
share of the time budget, and bypasses a plugin that reports an error.

## Testing and benchmarks

    cargo test
    cargo test --release bench -- --ignored --nocapture --test-threads=1

The UI tests drive real egui frames with synthetic mouse and keyboard
input, without opening a window.

## Layout

    src/
      main.rs        audio device setup; starts the UI or the command line
      session.rs     every main-thread action, shared by the UI and the CLI
      session/       undo/redo (snapshots + reconciliation), project files
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
      record.rs      recording: latency compensation, WAV writer, disk thread
      clap/          CLAP plugin hosting: loading, instances, the plugin node
      denormal.rs    flush-to-zero guard for the audio thread
      bench.rs       offline benchmarks (tests only)
      nodes/         effects: filter, eq, delay, comp, reverb, gain, lp,
                     pan, osc, click
    plugins/
      clap-example/  two example CLAP plugins, built as a real .clap library
