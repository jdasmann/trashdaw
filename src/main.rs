#[cfg(test)]
mod bench;
mod buffer;
mod clip;
mod denormal;
mod engine;
mod loader;
mod midi;
mod mixer;
mod nodes;
mod processor;
mod project;
mod smoothing;
mod synth;
mod transport;

use clip::{AudioClip, ClipList, ClipParams};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use engine::{Command, Engine, Garbage};
use loader::AudioData;
use midi::{parse_pitch, LiveMidi, MidiMessage, Note, Sequence};
use midir::{MidiInput, MidiInputConnection};
use mixer::{MeterPoint, Meters, Target, Track, MAX_SLOTS, MAX_TRACKS, MIX_CHANNELS};
use processor::{PrepareInfo, Processor};
use project::{ClipInfo, MidiClipInfo, Project, TrackInfo};
use rtrb::{Producer, RingBuffer};
use std::io::{self, BufRead};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use transport::{format_bbt, samples_per_beat, TransportShared, BEATS_PER_BAR};

const MAX_BLOCK: usize = 512;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no output device available")?;
    let supported = device.default_output_config()?;

    println!("Device: {}", device.name()?);

    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    match format {
        SampleFormat::F32 => run::<f32>(&device, &config),
        SampleFormat::I16 => run::<i16>(&device, &config),
        SampleFormat::U16 => run::<u16>(&device, &config),
        other => Err(format!("unsupported sample format: {other:?}").into()),
    }
}

fn make_node(kind: &str) -> Option<Box<dyn Processor>> {
    match kind {
        "osc" => Some(Box::new(nodes::Oscillator::new())),
        "gain" => Some(Box::new(nodes::Gain::new())),
        "lp" => Some(Box::new(nodes::Lowpass::new())),
        "pan" => Some(Box::new(nodes::Pan::new())),
        "click" => Some(Box::new(nodes::Metronome::new())),
        "filter" => Some(Box::new(nodes::Biquad::new())),
        "delay" => Some(Box::new(nodes::Delay::new())),
        "comp" => Some(Box::new(nodes::Compressor::new())),
        "reverb" => Some(Box::new(nodes::Reverb::new())),
        _ => None,
    }
}

fn send(tx: &mut Producer<Command>, cmd: Command) {
    if tx.push(cmd).is_err() {
        eprintln!("command queue full");
    }
}

/// Bars are 1-based for humans; beats are 0-based for the engine.
fn bar_to_beat(bar: f64) -> f64 {
    (bar - 1.0) * BEATS_PER_BAR
}

/// Current playhead in beats, as last published by the audio thread.
fn current_beat(shared: &TransportShared, sample_rate: f64) -> f64 {
    shared.position() as f64 / samples_per_beat(sample_rate, shared.tempo())
}

fn target_label(project: &Project, target: Target) -> String {
    match target {
        Target::Master => "master".into(),
        Target::Aux => "aux".into(),
        Target::Track(i) => format!("track {i} ({})", project.get(target).map_or("?", |t| &t.name)),
    }
}

/// Peak level as dBFS text for the meters.
fn peak_db(peak: f32) -> String {
    if peak <= 1e-5 {
        "  -inf".into()
    } else {
        format!("{:6.1}", 20.0 * peak.log10())
    }
}

/// Prepares a node (allocating on this thread) and ships it to the engine.
fn insert(
    tx: &mut Producer<Command>,
    project: &mut Project,
    info: &PrepareInfo,
    target: Target,
    slot_arg: Option<&&str>,
    mut node: Box<dyn Processor>,
) {
    let Some(track) = project.get_mut(target) else { return };
    let slot = match slot_arg {
        Some(s) => s.parse::<usize>().ok().filter(|&s| s < MAX_SLOTS),
        None => track.first_free_slot(),
    };
    let Some(slot) = slot else {
        println!("no free slot (or bad slot number)");
        return;
    };
    node.prepare(info);
    track.slots[slot] = Some((node.name(), node.params()));
    println!("added {} at slot {slot}", node.name());
    send(tx, Command::Insert { target, slot, node });
}

/// Rebuilds a track's whole clip list from the project and sends it to the
/// engine. Called after every clip edit: building a fresh snapshot on this
/// thread is simpler and safer than editing the engine's list in place.
fn sync_clips(tx: &mut Producer<Command>, project: &Project, info: &PrepareInfo, track: usize) {
    let Some(t) = project.tracks[track].as_ref() else { return };
    let clips = t
        .clips
        .iter()
        .map(|c| AudioClip::new(c.data.clone(), c.params, info))
        .collect();
    send(tx, Command::SetClips { track, clips: Box::new(ClipList::new(clips)) });
}

/// Flattens a track's MIDI clips into one note timeline for the engine.
fn sync_sequence(tx: &mut Producer<Command>, project: &Project, track: usize) {
    let Some(t) = project.tracks[track].as_ref() else { return };
    let sequence = Sequence::build(t.midi_clips.iter().map(|c| (c.start_beat, c.length_beats, &c.notes[..])));
    send(tx, Command::SetSequence { track, sequence: Box::new(sequence) });
}

/// Four bars of an arpeggio (Am, F, C, G) with a bass note, so there's
/// something to hear without typing every note.
fn demo_notes() -> Vec<Note> {
    let chords = [[57u8, 60, 64], [53, 57, 60], [48, 52, 55], [55, 59, 62]];
    let pattern = [0usize, 1, 2, 3, 2, 1, 0, 1]; // 3 = root an octave up
    let mut notes = Vec::new();
    for (bar, chord) in chords.iter().enumerate() {
        let bar_start = bar as f64 * BEATS_PER_BAR;
        notes.push(Note { start: bar_start, length: 3.5, pitch: chord[0] - 12, velocity: 90 });
        for (step, &p) in pattern.iter().enumerate() {
            let pitch = if p == 3 { chord[0] + 12 } else { chord[p] };
            let velocity = if step % 2 == 0 { 100 } else { 75 };
            notes.push(Note { start: bar_start + step as f64 * 0.5, length: 0.45, pitch, velocity });
        }
    }
    notes
}

/// Removes quotes a terminal may add when you drag a file in, and expands
/// a leading `~` (a shell feature: Rust's file APIs treat it literally).
fn clean_path(raw: &str) -> PathBuf {
    let t = raw.trim();
    let t = t
        .strip_prefix('"').and_then(|s| s.strip_suffix('"'))
        .or_else(|| t.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(t);
    if let Some(rest) = t.strip_prefix("~/").or_else(|| t.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(t)
}

fn print_help() {
    println!("Tracks:    track add [name] | track rm <t> | ls");
    println!("Mixer:     vol <t|a|m> <db> | pan <t> <-1..1> | mute <t> | solo <t> | send <t> <db> | meters");
    println!("Nodes:     add <t|a|m> <kind> [slot] | set <t|a|m> <slot> <param> <value> | rm <t|a|m> <slot>");
    println!("           kinds: osc gain lp pan click | filter delay comp reverb");
    println!("           filter types: 0 lowpass 1 highpass 2 bandpass 3 notch 4 peak 5 low-shelf 6 high-shelf");
    println!("Samples:   load <path> | samples");
    println!("Clips:     clip add <t> <sample#> <bar> | clip rm <t> <id>");
    println!("           clip mv <t> <id> <bar> | clip len <t> <id> <beats> | clip off <t> <id> <secs>");
    println!("           clip gain <t> <id> <db> | clip split <t> <id> <bar> | clip fade <t> <id> <in> <out>");
    println!("           (a later clip cuts off any earlier clip it overlaps)");
    println!("MIDI:      synth <t> | synth rm <t> | iset <t> <param> <value>");
    println!("           mclip add <t> <bar> <beats> | mclip demo <t> <bar> | mclip rm <t> <id>");
    println!("           note add <t> <clip> <beat> <length> <pitch> [velocity] | note clear <t> <clip>");
    println!("           midi ports | midi open <port#> | midi close | arm <t> | arm off");
    println!("           (pitches: 60 or C4, F#3, Bb2; note beats are relative to the clip)");
    println!("Transport: play | stop | seek <bar> | tempo <bpm> | loop <start_bar> <end_bar> | loop off | pos");
    println!("           return <on|off> | help | q");
    println!("(<t> is a track number, a is the aux bus, m is the master bus)");
}

fn run<T>(device: &cpal::Device, config: &cpal::StreamConfig) -> Result<(), Box<dyn std::error::Error>>
where
    T: SizedSample + FromSample<f32>,
{
    // Nodes always see a stereo mix; the engine maps it to the device.
    let info = PrepareInfo {
        sample_rate: config.sample_rate.0 as f32,
        max_frames: MAX_BLOCK,
        channels: MIX_CHANNELS,
    };
    let sample_rate = info.sample_rate as f64;

    let (mut cmd_tx, cmd_rx) = RingBuffer::<Command>::new(256);
    // Big enough to hold every node of every track at once.
    let (garbage_tx, mut garbage_rx) = RingBuffer::<Garbage>::new(MAX_TRACKS * (MAX_SLOTS + 1) * 4);
    // Live MIDI: the MIDI driver's thread pushes, the audio thread pops.
    // The producer sits behind a Mutex so it can be handed to whichever
    // connection is open. That lock is only ever taken on the MIDI thread
    // and here, never on the audio thread.
    let (live_tx, live_rx) = RingBuffer::<LiveMidi>::new(1024);
    let live_tx = Arc::new(Mutex::new(live_tx));
    let mut midi_conn: Option<MidiInputConnection<()>> = None;
    let shared = TransportShared::new();
    let meters = Meters::new();

    let mut engine = Engine::new(
        &info,
        config.channels as usize,
        shared.clone(),
        meters.clone(),
        cmd_rx,
        garbage_tx,
        live_rx,
    );

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| engine.render(data),
        |err| eprintln!("stream error: {err}"),
        None,
    )?;
    stream.play()?;

    let mut project = Project::new();
    let mut next_clip_id: u32 = 1;

    // Loaded audio files. A Mutex is fine here: only the main thread and
    // loader threads touch it, never the audio thread.
    let library: Arc<Mutex<Vec<Arc<AudioData>>>> = Arc::new(Mutex::new(Vec::new()));

    // Return-to-start lives entirely on the main thread: the engine only
    // ever sees ordinary Stop and Seek commands.
    let mut return_to_start = true;
    let mut play_start: Option<f64> = None; // beat where playback began

    print_help();

    for line in io::stdin().lock().lines() {
        // Free anything the audio thread has finished with.
        while let Ok(item) = garbage_rx.pop() {
            match item {
                Garbage::Node(node) => drop(node),
                Garbage::Track(track) => drop(track), // drops its nodes and clips too
                Garbage::Clips(clips) => drop(clips),
                Garbage::Sequence(seq) => drop(seq),
                Garbage::Instrument(inst) => drop(inst),
            }
        }

        let line = line?;

        // `load` takes the rest of the line, so paths may contain spaces.
        if let Some(raw) = line.trim_start().strip_prefix("load ") {
            let path = clean_path(raw);
            let library = library.clone();
            println!("loading {}...", path.display());
            // Decoding can take seconds; do it on its own thread so the
            // command line (and later the UI) stays responsive.
            thread::spawn(move || match loader::load_audio(&path) {
                Ok(audio) => {
                    let mut lib = library.lock().unwrap();
                    lib.push(Arc::new(audio));
                    let a = lib.last().unwrap();
                    println!(
                        "loaded #{}: {} ({:.2}s, {} Hz, {} ch)",
                        lib.len() - 1,
                        a.name,
                        a.duration_secs(),
                        a.sample_rate,
                        a.channels.len()
                    );
                }
                Err(e) => println!("failed to load {}: {e}", path.display()),
            });
            continue;
        }

        let args: Vec<&str> = line.split_whitespace().collect();

        match args.as_slice() {
            // ---- Tracks ----
            ["track", "add", name @ ..] => {
                let Some(index) = project.first_free_track() else {
                    println!("all {MAX_TRACKS} tracks in use");
                    continue;
                };
                let name = if name.is_empty() { format!("Track {}", index + 1) } else { name.join(" ") };
                // Allocating the track's buffers happens here, not in the engine.
                let track = Box::new(Track::new(&info));
                project.tracks[index] = Some(TrackInfo::new(name));
                send(&mut cmd_tx, Command::AddTrack { index, track });
                println!("added {}", target_label(&project, Target::Track(index)));
            }
            ["track", "rm", t] => match project.parse_target(t) {
                Some(Target::Track(index)) => {
                    project.tracks[index] = None;
                    send(&mut cmd_tx, Command::RemoveTrack { index });
                }
                _ => println!("no such track"),
            },
            ["ls"] => {
                let targets = (0..MAX_TRACKS).map(Target::Track).chain([Target::Aux, Target::Master]);
                for target in targets {
                    let Some(t) = project.get(target) else { continue };
                    let mut flags = String::new();
                    if t.mute { flags.push_str(" MUTE"); }
                    if t.solo { flags.push_str(" SOLO"); }
                    let strip = match target {
                        Target::Track(_) => {
                            let send = if t.send_db <= -60.0 { "off".into() } else { format!("{:+.1} dB", t.send_db) };
                            format!("  pan {:+.2}  send {send}", t.pan)
                        }
                        _ => String::new(),
                    };
                    println!("{}  {:+.1} dB{strip}{flags}", target_label(&project, target), t.volume_db);
                    if let Some((name, params)) = t.instrument {
                        println!("    instrument: {name}  [{}]", params.join(", "));
                    }
                    for c in &t.midi_clips {
                        println!(
                            "    midi clip #{}: @ {}  len {:.2} beats  {} notes",
                            c.id, format_bbt(c.start_beat), c.length_beats, c.notes.len()
                        );
                    }
                    let mut clips: Vec<&ClipInfo> = t.clips.iter().collect();
                    clips.sort_by(|a, b| a.params.start_beat.total_cmp(&b.params.start_beat));
                    for c in clips {
                        let p = &c.params;
                        let fades = if p.fade_in_beats > 0.0 || p.fade_out_beats > 0.0 {
                            format!("  fades {:.2}/{:.2}", p.fade_in_beats, p.fade_out_beats)
                        } else {
                            String::new()
                        };
                        println!(
                            "    clip #{}: {} @ {}  len {:.2} beats  offset {:.3}s  {:+.1} dB{fades}",
                            p.id, c.data.name, format_bbt(p.start_beat), p.length_beats, p.offset_secs, p.gain_db
                        );
                    }
                    for (i, slot) in t.slots.iter().enumerate() {
                        if let Some((name, params)) = slot {
                            println!("    {i}: {name}  [{}]", params.join(", "));
                        }
                    }
                }
            }

            // ---- Mixer ----
            ["vol", t, db] => match (project.parse_target(t), db.parse::<f32>()) {
                (Some(target), Ok(db)) => {
                    let db = db.clamp(-60.0, 12.0);
                    project.get_mut(target).unwrap().volume_db = db;
                    send(&mut cmd_tx, Command::SetVolume { target, db });
                }
                _ => println!("usage: vol <t|a|m> <db>   (-60 is silence, max +12)"),
            },
            ["pan", t, pos] => match (project.parse_target(t), pos.parse::<f32>()) {
                (Some(Target::Track(track)), Ok(pos)) => {
                    let pos = pos.clamp(-1.0, 1.0);
                    project.tracks[track].as_mut().unwrap().pan = pos;
                    send(&mut cmd_tx, Command::SetPan { track, pos });
                }
                _ => println!("usage: pan <t> <-1..1>"),
            },
            ["mute", t] | ["solo", t] => match project.parse_target(t) {
                Some(Target::Track(track)) => {
                    let info = project.tracks[track].as_mut().unwrap();
                    // Toggle, using the mirror to know the current state.
                    if args[0] == "mute" {
                        info.mute = !info.mute;
                        send(&mut cmd_tx, Command::SetMute { track, on: info.mute });
                    } else {
                        info.solo = !info.solo;
                        send(&mut cmd_tx, Command::SetSolo { track, on: info.solo });
                    }
                }
                _ => println!("usage: {} <t>", args[0]),
            },
            ["send", t, db] => match (project.parse_target(t), db.parse::<f32>()) {
                (Some(Target::Track(track)), Ok(db)) => {
                    let db = db.clamp(-60.0, 12.0);
                    project.tracks[track].as_mut().unwrap().send_db = db;
                    send(&mut cmd_tx, Command::SetSend { track, db });
                }
                _ => println!("usage: send <t> <db>   (-60 is off)"),
            },
            ["meters"] => {
                // Peaks since the last `meters`, in dBFS. 0.0 is full scale.
                // Tracks show pre-fader (after the chain) and post-fader.
                println!("     pre     post");
                let targets = (0..MAX_TRACKS).map(Target::Track).chain([Target::Aux, Target::Master]);
                for target in targets {
                    if project.get(target).is_none() {
                        continue;
                    }
                    let post = meters.take(target, MeterPoint::PostFader);
                    let pre = match target {
                        Target::Track(_) => peak_db(meters.take(target, MeterPoint::PreFader)),
                        _ => "      ".into(),
                    };
                    let clip = if post > 1.0 { "  CLIP" } else { "" };
                    println!("  {pre} {}  {}{clip}", peak_db(post), target_label(&project, target));
                }
            }

            // ---- Nodes ----
            ["add", t, kind, rest @ ..] => match (project.parse_target(t), make_node(kind)) {
                (Some(target), Some(node)) => {
                    insert(&mut cmd_tx, &mut project, &info, target, rest.first(), node)
                }
                _ => println!("usage: add <t|a|m> <kind> [slot]   (kinds: osc gain lp pan click filter delay comp reverb)"),
            },
            ["set", t, slot, param, value] => {
                let found = project.parse_target(t).and_then(|target| {
                    let slot = slot.parse::<usize>().ok()?;
                    let (_, params) = project.get(target)?.slots.get(slot)?.as_ref()?;
                    let param = params.iter().position(|name| name == param)?;
                    Some((target, slot, param, value.parse::<f32>().ok()?))
                });
                match found {
                    Some((target, slot, param, value)) => {
                        send(&mut cmd_tx, Command::SetParam { target, slot, param, value })
                    }
                    None => println!("no such track, slot, or parameter (try `ls`)"),
                }
            }
            ["rm", t, slot] => {
                let found = project.parse_target(t).and_then(|target| {
                    let slot = slot.parse::<usize>().ok()?;
                    project.get_mut(target)?.slots.get_mut(slot)?.take()?;
                    Some((target, slot))
                });
                match found {
                    Some((target, slot)) => send(&mut cmd_tx, Command::Remove { target, slot }),
                    None => println!("slot is empty"),
                }
            }

            // ---- Samples ----
            ["samples"] => {
                let lib = library.lock().unwrap();
                if lib.is_empty() {
                    println!("  (none loaded)");
                }
                for (i, a) in lib.iter().enumerate() {
                    println!("  #{i}: {} ({:.2}s, {} Hz, {} ch)", a.name, a.duration_secs(), a.sample_rate, a.channels.len());
                }
            }
            // ---- Clips ----
            ["clip", "add", t, sample, bar] => {
                let track = match project.parse_target(t) {
                    Some(Target::Track(i)) => Some(i),
                    _ => None,
                };
                // Cloning the Arc: the clip and the library share one copy.
                let data = sample
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| library.lock().unwrap().get(i).cloned());
                let bar = bar.parse::<f64>().ok().filter(|b| *b >= 1.0);
                let (Some(track), Some(data), Some(bar)) = (track, data, bar) else {
                    println!("usage: clip add <t> <sample#> <bar>  (see `samples`)");
                    continue;
                };
                // Clips always have an explicit length: the whole file, in
                // beats at the current tempo. It stays fixed in beats after
                // that, like the clip's position.
                let length_beats = data.duration_secs() * shared.tempo() / 60.0;
                let id = next_clip_id;
                next_clip_id += 1;
                println!("added clip #{id} ({}) to track {track} at bar {bar}", data.name);
                let params = ClipParams::new(id, bar_to_beat(bar), length_beats);
                project.tracks[track].as_mut().unwrap().clips.push(ClipInfo { data, params });
                sync_clips(&mut cmd_tx, &project, &info, track);
            }
            ["clip", "rm", t, id] => {
                let removed = match (project.parse_target(t), id.parse::<u32>()) {
                    (Some(Target::Track(track)), Ok(id)) => {
                        let clips = &mut project.tracks[track].as_mut().unwrap().clips;
                        let before = clips.len();
                        clips.retain(|c| c.params.id != id);
                        (clips.len() < before).then_some(track)
                    }
                    _ => None,
                };
                match removed {
                    Some(track) => sync_clips(&mut cmd_tx, &project, &info, track),
                    None => println!("no such clip (try `ls`)"),
                }
            }
            ["clip", "fade", t, id, fade_in, fade_out] => {
                let parsed = (fade_in.parse::<f64>(), fade_out.parse::<f64>());
                let found = match (project.parse_target(t), id.parse::<u32>(), parsed) {
                    (Some(Target::Track(track)), Ok(id), (Ok(fi), Ok(fo))) if fi >= 0.0 && fo >= 0.0 => project
                        .tracks[track]
                        .as_mut()
                        .and_then(|ti| ti.clips.iter_mut().find(|c| c.params.id == id))
                        .map(|c| {
                            c.params.fade_in_beats = fi;
                            c.params.fade_out_beats = fo;
                            track
                        }),
                    _ => None,
                };
                match found {
                    Some(track) => sync_clips(&mut cmd_tx, &project, &info, track),
                    None => println!("usage: clip fade <t> <id> <in_beats> <out_beats>  (0 = none)"),
                }
            }
            ["clip", op @ ("mv" | "len" | "off" | "gain" | "split"), t, id, value] => {
                let found = match (project.parse_target(t), id.parse::<u32>(), value.parse::<f64>()) {
                    (Some(Target::Track(track)), Ok(id), Ok(value)) => project.tracks[track]
                        .as_ref()
                        .and_then(|ti| ti.clips.iter().position(|c| c.params.id == id))
                        .map(|index| (track, index, value)),
                    _ => None,
                };
                let Some((track, index, value)) = found else {
                    println!("usage: clip {op} <t> <id> <value>  (try `ls`)");
                    continue;
                };
                let clips = &mut project.tracks[track].as_mut().unwrap().clips;
                let ok = match *op {
                    "mv" if value >= 1.0 => {
                        clips[index].params.start_beat = bar_to_beat(value);
                        true
                    }
                    "len" if value > 0.0 => {
                        clips[index].params.length_beats = value;
                        true
                    }
                    "off" if value >= 0.0 => {
                        clips[index].params.offset_secs = value;
                        true
                    }
                    "gain" => {
                        clips[index].params.gain_db = (value as f32).clamp(-60.0, 12.0);
                        true
                    }
                    "split" => {
                        let c = &clips[index];
                        let p = c.params;
                        let at = bar_to_beat(value);
                        if at <= p.start_beat || at >= p.start_beat + p.length_beats {
                            println!("the split point must be inside the clip");
                            false
                        } else {
                            // The second half starts later in the FILE by
                            // however long the first half lasts in seconds.
                            // The first half keeps the fade-in, the second
                            // keeps the fade-out.
                            let first_len = at - p.start_beat;
                            let first_secs = first_len * 60.0 / shared.tempo();
                            let mut second = p;
                            second.id = next_clip_id;
                            second.start_beat = at;
                            second.length_beats = p.length_beats - first_len;
                            second.offset_secs = p.offset_secs + first_secs;
                            second.fade_in_beats = 0.0;
                            next_clip_id += 1;
                            let data = c.data.clone();
                            clips[index].params.length_beats = first_len;
                            clips[index].params.fade_out_beats = 0.0;
                            println!("split into #{} and #{}", p.id, second.id);
                            clips.push(ClipInfo { data, params: second });
                            true
                        }
                    }
                    _ => {
                        println!("invalid value for clip {op}");
                        false
                    }
                };
                if ok {
                    sync_clips(&mut cmd_tx, &project, &info, track);
                }
            }

            // ---- MIDI: instruments ----
            ["synth", t] => match project.parse_target(t) {
                Some(Target::Track(track)) => {
                    let mut synth: Box<dyn processor::Instrument> = Box::new(synth::Synth::new());
                    synth.prepare(&info);
                    project.tracks[track].as_mut().unwrap().instrument = Some((synth.name(), synth.params()));
                    send(&mut cmd_tx, Command::SetInstrument { track, instrument: Some(synth) });
                    println!("synth on track {track}");
                }
                _ => println!("usage: synth <t>"),
            },
            ["synth", "rm", t] => match project.parse_target(t) {
                Some(Target::Track(track)) => {
                    project.tracks[track].as_mut().unwrap().instrument = None;
                    send(&mut cmd_tx, Command::SetInstrument { track, instrument: None });
                }
                _ => println!("usage: synth rm <t>"),
            },
            ["iset", t, param, value] => {
                let found = match project.parse_target(t) {
                    Some(Target::Track(track)) => project.tracks[track].as_ref().and_then(|ti| {
                        let (_, params) = ti.instrument?;
                        let param = params.iter().position(|p| p == param)?;
                        Some((track, param, value.parse::<f32>().ok()?))
                    }),
                    _ => None,
                };
                match found {
                    Some((track, param, value)) => send(&mut cmd_tx, Command::SetInstrumentParam { track, param, value }),
                    None => println!("no such track, instrument, or parameter (try `ls`)"),
                }
            }

            // ---- MIDI: clips and notes ----
            ["mclip", kind @ ("add" | "demo"), t, bar, rest @ ..] => {
                let track = match project.parse_target(t) {
                    Some(Target::Track(i)) => Some(i),
                    _ => None,
                };
                let bar = bar.parse::<f64>().ok().filter(|b| *b >= 1.0);
                let (notes, length) = if *kind == "demo" {
                    (demo_notes(), Some(4.0 * BEATS_PER_BAR))
                } else {
                    (Vec::new(), rest.first().and_then(|b| b.parse::<f64>().ok()).filter(|b| *b > 0.0))
                };
                let (Some(track), Some(bar), Some(length_beats)) = (track, bar, length) else {
                    println!("usage: mclip add <t> <bar> <beats> | mclip demo <t> <bar>");
                    continue;
                };
                let id = next_clip_id;
                next_clip_id += 1;
                let ti = project.tracks[track].as_mut().unwrap();
                ti.midi_clips.push(MidiClipInfo { id, start_beat: bar_to_beat(bar), length_beats, notes });
                let hint = if ti.instrument.is_none() { "  (add an instrument: synth <t>)" } else { "" };
                println!("added midi clip #{id} to track {track}{hint}");
                sync_sequence(&mut cmd_tx, &project, track);
            }
            ["mclip", "rm", t, id] => {
                let removed = match (project.parse_target(t), id.parse::<u32>()) {
                    (Some(Target::Track(track)), Ok(id)) => {
                        let clips = &mut project.tracks[track].as_mut().unwrap().midi_clips;
                        let before = clips.len();
                        clips.retain(|c| c.id != id);
                        (clips.len() < before).then_some(track)
                    }
                    _ => None,
                };
                match removed {
                    Some(track) => sync_sequence(&mut cmd_tx, &project, track),
                    None => println!("no such midi clip (try `ls`)"),
                }
            }
            ["note", "add", t, clip, beat, length, pitch, rest @ ..] => {
                let note = (beat.parse::<f64>(), length.parse::<f64>(), parse_pitch(pitch));
                let velocity = match rest.first() {
                    Some(v) => v.parse::<u8>().ok().filter(|v| (1..=127).contains(v)),
                    None => Some(100),
                };
                let found = match (project.parse_target(t), clip.parse::<u32>(), note, velocity) {
                    (Some(Target::Track(track)), Ok(id), (Ok(start), Ok(length), Some(pitch)), Some(velocity))
                        if start >= 0.0 && length > 0.0 =>
                    {
                        project.tracks[track]
                            .as_mut()
                            .and_then(|ti| ti.midi_clips.iter_mut().find(|c| c.id == id))
                            .map(|c| {
                                c.notes.push(Note { start, length, pitch, velocity });
                                track
                            })
                    }
                    _ => None,
                };
                match found {
                    Some(track) => sync_sequence(&mut cmd_tx, &project, track),
                    None => println!("usage: note add <t> <clip> <beat> <length> <pitch> [velocity]"),
                }
            }
            ["note", "clear", t, clip] => {
                let found = match (project.parse_target(t), clip.parse::<u32>()) {
                    (Some(Target::Track(track)), Ok(id)) => project.tracks[track]
                        .as_mut()
                        .and_then(|ti| ti.midi_clips.iter_mut().find(|c| c.id == id))
                        .map(|c| {
                            c.notes.clear();
                            track
                        }),
                    _ => None,
                };
                match found {
                    Some(track) => sync_sequence(&mut cmd_tx, &project, track),
                    None => println!("no such midi clip"),
                }
            }

            // ---- MIDI: live input ----
            ["midi", "ports"] => match MidiInput::new("daw") {
                Ok(input) => {
                    let ports = input.ports();
                    if ports.is_empty() {
                        println!("  (no MIDI inputs found)");
                    }
                    for (i, p) in ports.iter().enumerate() {
                        println!("  {i}: {}", input.port_name(p).unwrap_or_else(|_| "?".into()));
                    }
                }
                Err(e) => println!("MIDI unavailable: {e}"),
            },
            ["midi", "open", n] => {
                // Only one input at a time: close the old one first.
                drop(midi_conn.take());
                let opened = MidiInput::new("daw").map_err(|e| e.to_string()).and_then(|input| {
                    let port = n
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| input.ports().get(i).cloned())
                        .ok_or_else(|| "no such port (see `midi ports`)".to_string())?;
                    let name = input.port_name(&port).unwrap_or_default();
                    let tx = live_tx.clone();
                    // This closure runs on the MIDI driver's thread. Stamp
                    // the arrival time immediately; the audio thread turns
                    // it into an exact sample position.
                    let conn = input
                        .connect(&port, "daw-in", move |_, bytes, _| {
                            if let Some(msg) = MidiMessage::parse(bytes) {
                                let _ = tx.lock().unwrap().push(LiveMidi { at: Instant::now(), msg });
                            }
                        }, ())
                        .map_err(|e| e.to_string())?;
                    Ok((conn, name))
                });
                match opened {
                    Ok((conn, name)) => {
                        midi_conn = Some(conn);
                        println!("listening to {name}  (arm a track to play it: arm <t>)");
                    }
                    Err(e) => println!("couldn't open MIDI input: {e}"),
                }
            }
            ["midi", "close"] => drop(midi_conn.take()),
            ["arm", "off"] => send(&mut cmd_tx, Command::Arm { track: None }),
            ["arm", t] => match project.parse_target(t) {
                Some(Target::Track(track)) => send(&mut cmd_tx, Command::Arm { track: Some(track) }),
                _ => println!("usage: arm <t> | arm off"),
            },

            // ---- Transport ----
            ["play"] => {
                // Only remember the start if we weren't already playing.
                // Stored in beats, so a tempo change mid-playback is fine.
                // (The published position can lag by one callback; a human
                // can't type `seek` then `play` faster than that.)
                if play_start.is_none() {
                    play_start = Some(current_beat(&shared, sample_rate));
                }
                send(&mut cmd_tx, Command::Play);
            }
            ["stop"] => {
                send(&mut cmd_tx, Command::Stop);
                if let Some(beat) = play_start.take() {
                    if return_to_start {
                        // Queued right behind Stop, so the engine applies
                        // both in the same callback.
                        send(&mut cmd_tx, Command::Seek { beat });
                    }
                }
            }
            ["return", "on"] => return_to_start = true,
            ["return", "off"] => return_to_start = false,
            ["seek", bar] => match bar.parse::<f64>() {
                Ok(bar) if bar >= 1.0 => send(&mut cmd_tx, Command::Seek { beat: bar_to_beat(bar) }),
                _ => println!("usage: seek <bar>  (bars start at 1)"),
            },
            ["tempo", bpm] => match bpm.parse::<f64>() {
                Ok(bpm) if (20.0..=999.0).contains(&bpm) => send(&mut cmd_tx, Command::SetTempo { bpm }),
                _ => println!("usage: tempo <20-999>"),
            },
            ["loop", "off"] => send(&mut cmd_tx, Command::SetLoop { range: None }),
            ["loop", start, end] => match (start.parse::<f64>(), end.parse::<f64>()) {
                (Ok(s), Ok(e)) if s >= 1.0 && e > s => send(
                    &mut cmd_tx,
                    Command::SetLoop { range: Some((bar_to_beat(s), bar_to_beat(e))) },
                ),
                _ => println!("usage: loop <start_bar> <end_bar>  (end after start)"),
            },
            ["pos"] => {
                let position = shared.position();
                println!(
                    "  {}  {:.3}s  {} BPM  {}",
                    format_bbt(current_beat(&shared, sample_rate)),
                    position as f64 / sample_rate,
                    shared.tempo(),
                    if shared.playing() { "playing" } else { "stopped" },
                );
            }

            ["help"] => print_help(),
            ["q"] => break,
            [] => {}
            _ => println!("? (type `help`)"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod regression_tests {
    #[test]
    fn regression_tilde_paths_expand() {
        let p = super::clean_path("~/Music/drums.wav");
        assert!(!p.starts_with("~"), "{} would fail to open", p.display());
    }
}
