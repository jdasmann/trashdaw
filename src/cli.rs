//! The command-line front end: parses text and calls `Session`. All the
//! real work lives in `session.rs`, shared with the UI.

use crate::midi::{parse_pitch, Note};
use crate::mixer::{MeterPoint, Target, MAX_TRACKS};
use crate::project::{ClipInfo, Project};
use crate::session::{bar_to_beat, clean_path, demo_notes, Result, Session};
use crate::transport::{format_bbt, BEATS_PER_BAR};
use std::io::{self, BufRead};

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

pub fn print_help() {
    println!("Tracks:    track add [name] | track rm <t> | ls");
    println!("Mixer:     vol <t|a|m> <db> | pan <t> <-1..1> | mute <t> | solo <t> | send <t> <db> | meters");
    println!("           sidechain <t> <source t> | sidechain <t> off   (a comp on <t> then listens to the source)");
    println!("Nodes:     add <t|a|m> <kind> [slot] | set <t|a|m> <slot> <param> <value> | rm <t|a|m> <slot>");
    let kinds: Vec<&str> = crate::nodes::KINDS.iter().map(|k| k.0).collect();
    println!("           kinds: {}", kinds.join(" "));
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

/// Runs the command loop until `q` or end of input.
pub fn run(session: &mut Session) -> io::Result<()> {
    print_help();
    for line in io::stdin().lock().lines() {
        session.collect_garbage();
        let line = line?;
        if line.trim() == "q" {
            break;
        }
        if let Err(e) = execute(session, &line) {
            println!("{e}");
        }
    }
    Ok(())
}

fn track_arg(session: &Session, s: &str) -> Result<usize> {
    match session.project.parse_target(s) {
        Some(Target::Track(i)) => Ok(i),
        _ => Err(format!("no track {s}")),
    }
}

fn target_arg(session: &Session, s: &str) -> Result<Target> {
    session.project.parse_target(s).ok_or_else(|| format!("no track {s} (or a/m)"))
}

fn num<T: std::str::FromStr>(s: &str, what: &str) -> Result<T> {
    s.parse().map_err(|_| format!("{what} should be a number, not {s:?}"))
}

/// Executes one command line.
pub fn execute(session: &mut Session, line: &str) -> Result {
    // `load` takes the rest of the line, so paths may contain spaces.
    if let Some(raw) = line.trim_start().strip_prefix("load ") {
        let path = clean_path(raw);
        println!("loading {}...", path.display());
        session.load(path, true);
        return Ok(());
    }
    let args: Vec<&str> = line.split_whitespace().collect();
    let s = session;
    match args.as_slice() {
        [] => {}
        ["help"] => print_help(),

        // ---- Tracks ----
        ["track", "add", name @ ..] => {
            let i = s.add_track(Some(name.join(" ")))?;
            println!("added {}", target_label(&s.project, Target::Track(i)));
        }
        ["track", "rm", t] => s.remove_track(track_arg(s, t)?)?,
        ["ls"] => list(s),

        // ---- Mixer ----
        ["vol", t, db] => s.set_volume(target_arg(s, t)?, num(db, "volume")?)?,
        ["pan", t, pos] => s.set_pan(track_arg(s, t)?, num(pos, "pan")?)?,
        ["mute", t] => {
            let i = track_arg(s, t)?;
            let on = !s.project.tracks[i].as_ref().unwrap().mute;
            s.set_mute(i, on)?;
        }
        ["solo", t] => {
            let i = track_arg(s, t)?;
            let on = !s.project.tracks[i].as_ref().unwrap().solo;
            s.set_solo(i, on)?;
        }
        ["send", t, db] => s.set_send(track_arg(s, t)?, num(db, "send")?)?,
        ["sidechain", t, "off"] => s.set_sidechain(track_arg(s, t)?, None)?,
        ["sidechain", t, src] => {
            let (track, source) = (track_arg(s, t)?, track_arg(s, src)?);
            s.set_sidechain(track, Some(source))?;
            println!("track {track} now listens to track {source} (add a comp to hear it)");
        }
        ["meters"] => meters(s),

        // ---- Effects ----
        ["add", t, kind, rest @ ..] => {
            let target = target_arg(s, t)?;
            let slot = rest.first().map(|x| num(x, "slot")).transpose()?;
            let slot = s.add_node(target, kind, slot)?;
            println!("added {kind} at slot {slot}");
        }
        ["set", t, slot, param, value] => {
            let (target, slot) = (target_arg(s, t)?, num(slot, "slot")?);
            let param = s.param_index(target, slot, param)?;
            s.set_param(target, slot, param, num(value, "value")?)?;
        }
        ["rm", t, slot] => s.remove_node(target_arg(s, t)?, num(slot, "slot")?)?,

        // ---- Samples and clips ----
        ["samples"] => {
            let lib = s.samples();
            if lib.is_empty() {
                println!("  (none loaded)");
            }
            for (i, a) in lib.iter().enumerate() {
                println!("  #{i}: {} ({:.2}s, {} Hz, {} ch)", a.name, a.duration_secs(), a.sample_rate, a.channels.len());
            }
        }
        ["clip", "add", t, sample, bar] => {
            let (track, bar): (usize, f64) = (track_arg(s, t)?, num(bar, "bar")?);
            if bar < 1.0 {
                return Err("bars start at 1".into());
            }
            let id = s.add_clip(track, num(sample, "sample")?, bar_to_beat(bar))?;
            println!("added clip #{id} to track {track} at bar {bar}");
        }
        ["clip", "rm", t, id] => s.remove_clip(track_arg(s, t)?, num(id, "clip id")?)?,
        ["clip", "split", t, id, bar] => {
            let new = s.split_clip(track_arg(s, t)?, num(id, "clip id")?, bar_to_beat(num(bar, "bar")?))?;
            println!("split; the second half is #{new}");
        }
        ["clip", "fade", t, id, fi, fo] => {
            let (fi, fo): (f64, f64) = (num(fi, "fade")?, num(fo, "fade")?);
            s.edit_clip(track_arg(s, t)?, num(id, "clip id")?, |p| {
                p.fade_in_beats = fi;
                p.fade_out_beats = fo;
            })?;
        }
        ["clip", op @ ("mv" | "len" | "off" | "gain"), t, id, value] => {
            let (track, id, v): (usize, u32, f64) = (track_arg(s, t)?, num(id, "clip id")?, num(value, "value")?);
            if *op == "mv" && v < 1.0 {
                return Err("bars start at 1".into());
            }
            s.edit_clip(track, id, |p| match *op {
                "mv" => p.start_beat = bar_to_beat(v),
                "len" => p.length_beats = v,
                "off" => p.offset_secs = v,
                _ => p.gain_db = v as f32,
            })?;
        }

        // ---- MIDI ----
        ["synth", "rm", t] => s.set_synth(track_arg(s, t)?, false)?,
        ["synth", t] => {
            let track = track_arg(s, t)?;
            s.set_synth(track, true)?;
            println!("synth on track {track}");
        }
        ["iset", t, param, value] => {
            let track = track_arg(s, t)?;
            let inst = s.project.tracks[track].as_ref().unwrap().instrument.as_ref().ok_or("that track has no instrument")?;
            let index = inst.param_index(param).ok_or_else(|| format!("the synth has no parameter {param}"))?;
            s.set_instrument_param(track, index, num(value, "value")?)?;
        }
        ["mclip", "add", t, bar, beats] => {
            let id = s.add_midi_clip(track_arg(s, t)?, bar_to_beat(num(bar, "bar")?), num(beats, "length")?, Vec::new())?;
            println!("added midi clip #{id}");
        }
        ["mclip", "demo", t, bar] => {
            let track = track_arg(s, t)?;
            let id = s.add_midi_clip(track, bar_to_beat(num(bar, "bar")?), 4.0 * BEATS_PER_BAR, demo_notes())?;
            let hint = if s.project.tracks[track].as_ref().unwrap().instrument.is_none() { "  (add an instrument: synth <t>)" } else { "" };
            println!("added midi clip #{id}{hint}");
        }
        ["mclip", "rm", t, id] => s.remove_midi_clip(track_arg(s, t)?, num(id, "clip id")?)?,
        ["note", "add", t, clip, beat, length, pitch, rest @ ..] => {
            let start: f64 = num(beat, "beat")?;
            let length: f64 = num(length, "length")?;
            let pitch = parse_pitch(pitch).ok_or_else(|| format!("not a pitch: {pitch}"))?;
            let velocity: u8 = rest.first().map(|v| num(v, "velocity")).transpose()?.unwrap_or(100);
            if start < 0.0 || length <= 0.0 || !(1..=127).contains(&velocity) {
                return Err("usage: note add <t> <clip> <beat> <length> <pitch> [velocity 1-127]".into());
            }
            s.edit_midi_clip(track_arg(s, t)?, num(clip, "clip id")?, |c| c.notes.push(Note { start, length, pitch, velocity }))?;
        }
        ["note", "clear", t, clip] => s.edit_midi_clip(track_arg(s, t)?, num(clip, "clip id")?, |c| c.notes.clear())?,
        ["midi", "ports"] => {
            let ports = s.midi_ports()?;
            if ports.is_empty() {
                println!("  (no MIDI inputs found)");
            }
            for (i, p) in ports.iter().enumerate() {
                println!("  {i}: {p}");
            }
        }
        ["midi", "open", n] => {
            let name = s.open_midi(num(n, "port")?)?;
            println!("listening to {name}  (arm a track to play it: arm <t>)");
        }
        ["midi", "close"] => s.close_midi(),
        ["arm", "off"] => s.arm(None)?,
        ["arm", t] => s.arm(Some(track_arg(s, t)?))?,

        // ---- Transport ----
        ["play"] => s.play()?,
        ["stop"] => s.stop()?,
        ["return", "on"] => s.return_to_start = true,
        ["return", "off"] => s.return_to_start = false,
        ["seek", bar] => {
            let bar: f64 = num(bar, "bar")?;
            if bar < 1.0 {
                return Err("bars start at 1".into());
            }
            s.seek(bar_to_beat(bar))?;
        }
        ["tempo", bpm] => s.set_tempo(num(bpm, "tempo")?)?,
        ["loop", "off"] => s.set_loop(None)?,
        ["loop", a, b] => {
            let (a, b): (f64, f64) = (num(a, "bar")?, num(b, "bar")?);
            if a < 1.0 {
                return Err("bars start at 1".into());
            }
            s.set_loop(Some((bar_to_beat(a), bar_to_beat(b))))?;
        }
        ["pos"] => println!(
            "  {}  {:.3}s  {} BPM  {}",
            format_bbt(s.current_beat()),
            s.position_secs(),
            s.tempo(),
            if s.playing() { "playing" } else { "stopped" },
        ),
        _ => return Err("? (type `help`)".into()),
    }
    Ok(())
}

fn list(s: &Session) {
    let targets = (0..MAX_TRACKS).map(Target::Track).chain([Target::Aux, Target::Master]);
    for target in targets {
        let Some(t) = s.project.get(target) else { continue };
        let mut flags = String::new();
        if t.mute {
            flags.push_str(" MUTE");
        }
        if t.solo {
            flags.push_str(" SOLO");
        }
        let strip = match target {
            Target::Track(_) => {
                let send = if t.send_db <= -60.0 { "off".into() } else { format!("{:+.1} dB", t.send_db) };
                let key = t.key.map(|k| format!("  sidechain <- {k}")).unwrap_or_default();
                format!("  pan {:+.2}  send {send}{key}", t.pan)
            }
            _ => String::new(),
        };
        println!("{}  {:+.1} dB{strip}{flags}", target_label(&s.project, target), t.volume_db);
        if let Some(inst) = &t.instrument {
            println!("    instrument: {}  [{}]", inst.name, inst.params.iter().map(|p| p.name).collect::<Vec<_>>().join(", "));
        }
        for c in &t.midi_clips {
            println!("    midi clip #{}: @ {}  len {:.2} beats  {} notes", c.id, format_bbt(c.start_beat), c.length_beats, c.notes.len());
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
            if let Some(node) = slot {
                let params: Vec<String> = node.params.iter().zip(&node.values).map(|(p, v)| format!("{}={v}", p.name)).collect();
                println!("    {i}: {}  [{}]", node.name, params.join(", "));
            }
        }
    }
}

fn meters(s: &Session) {
    // Peaks since the last `meters`, in dBFS. 0.0 is full scale.
    println!("     pre     post");
    let targets = (0..MAX_TRACKS).map(Target::Track).chain([Target::Aux, Target::Master]);
    for target in targets {
        if s.project.get(target).is_none() {
            continue;
        }
        let post = s.meters.take(target, MeterPoint::PostFader);
        let pre = match target {
            Target::Track(_) => peak_db(s.meters.take(target, MeterPoint::PreFader)),
            _ => "      ".into(),
        };
        let clip = if post > 1.0 { "  CLIP" } else { "" };
        println!("  {pre} {}  {}{clip}", peak_db(post), target_label(&s.project, target));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::TestSession;

    #[test]
    fn commands_drive_the_session() {
        let mut t = TestSession::new();
        let s = &mut t.session;
        for line in ["track add Keys", "synth 0", "mclip demo 0 1", "add 0 delay", "set 0 0 feedback 0.6", "vol 0 -3", "loop 1 5", "tempo 100"] {
            execute(s, line).unwrap_or_else(|e| panic!("{line}: {e}"));
        }
        let track = s.project.tracks[0].as_ref().unwrap();
        assert_eq!(track.name, "Keys");
        assert_eq!(track.midi_clips.len(), 1);
        assert_eq!(track.slots[0].as_ref().unwrap().values[2], 0.6);
        assert_eq!(track.volume_db, -3.0);
        assert_eq!(s.loop_range, Some((0.0, 16.0)));
    }

    #[test]
    fn errors_are_reported_not_panics() {
        let mut t = TestSession::new();
        for line in ["vol 5 0", "set 0 0 x 1", "clip add 0 9 1", "seek 0", "tempo fast", "blah", "add m nothing"] {
            assert!(execute(&mut t.session, line).is_err(), "{line} should fail");
        }
    }
}
