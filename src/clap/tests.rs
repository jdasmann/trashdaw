use super::test_plugin::{self, BEEP_ID, ENTRY, GAIN_ID};
use super::*;
use crate::buffer::AudioBuffer;
use crate::midi::{MidiEvent, MidiMessage};
use crate::processor::{Instrument, ParamKind, PrepareInfo, Processor};
use crate::transport::Transport;

pub const INFO: PrepareInfo = PrepareInfo { sample_rate: 48_000.0, max_frames: 512, channels: 2 };

/// The example plugins, compiled in rather than loaded from disk.
pub fn example_bundle() -> Arc<Bundle> {
    // SAFETY: ENTRY is a static, valid forever.
    unsafe { Bundle::from_entry(&ENTRY, Path::new("example.clap"), None) }.unwrap()
}

fn dc(value: f32) -> AudioBuffer {
    let mut b = AudioBuffer::new(2, 512);
    b.channel_mut(0).fill(value);
    b.channel_mut(1).fill(value);
    b
}

#[test]
fn a_bundle_lists_its_plugins() {
    let b = example_bundle();
    let names: Vec<&str> = b.descriptors.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Example Gain", "Example Beep", "Example Lookahead"]);
    assert!(!b.descriptors[0].is_instrument());
    assert!(b.descriptors[1].is_instrument());
}

#[test]
fn parameters_come_from_the_plugin() {
    let inst = Instance::create(&example_bundle(), GAIN_ID, &INFO).unwrap();
    let names: Vec<&str> = inst.params.iter().map(|p| p.name).collect();
    assert_eq!(names, vec!["Gain", "Invert"], "the hidden parameter is left out");
    assert_eq!(inst.param_ids, vec![test_plugin::GAIN_PARAM, test_plugin::INVERT_PARAM], "ids are the plugin's, not indices");
    assert_eq!((inst.params[0].min, inst.params[0].max, inst.params[0].kind), (-24.0, 24.0, ParamKind::Linear));
    assert_eq!(inst.params[1].kind, ParamKind::Toggle, "stepped 0..1 is an on/off switch");
    assert_eq!((inst.inputs, inst.outputs), (2, 2));
    assert!(inst.service().iter().any(|l| l.contains("hello from the example plugin")), "the host log works");
}

#[test]
fn a_plugin_effect_processes_audio_and_follows_parameters() {
    enter_audio_thread(); // this test thread plays both roles
    let inst = Instance::create(&example_bundle(), GAIN_ID, &INFO).unwrap();
    let mut node = ClapNode::new(inst.clone(), &INFO).unwrap();
    let ctx = Transport::new(48_000.0).context();
    let mut buf = dc(0.5);
    Processor::process(&mut node, &mut buf, &ctx);
    assert!((buf.channel(0)[10] - 0.5).abs() < 1e-6, "unity gain by default");

    Processor::set_param(&mut node, 0, -6.0206); // the "Gain" parameter (id 7)
    let mut buf = dc(0.5);
    Processor::process(&mut node, &mut buf, &ctx);
    assert!((buf.channel(1)[10] - 0.25).abs() < 1e-4, "{}", buf.channel(1)[10]);

    Processor::set_param(&mut node, 1, 1.0); // "Invert"
    let mut buf = dc(0.5);
    Processor::process(&mut node, &mut buf, &ctx);
    assert!(buf.channel(0)[10] < 0.0);
    assert!(inst.take_cpu() > 0.0, "time spent in the plugin is measured");
    Processor::stop(&mut node);
}

#[test]
fn a_failed_plugin_is_bypassed() {
    enter_audio_thread();
    let inst = Instance::create(&example_bundle(), GAIN_ID, &INFO).unwrap();
    let mut node = ClapNode::new(inst.clone(), &INFO).unwrap();
    Processor::set_param(&mut node, 0, -24.0);
    inst.failed.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut buf = dc(0.5);
    Processor::process(&mut node, &mut buf, &Transport::new(48_000.0).context());
    assert_eq!(buf.channel(0)[10], 0.5, "the audio passes through untouched");
}

#[test]
fn a_plugin_instrument_plays_notes_at_their_frames() {
    enter_audio_thread();
    let inst = Instance::create(&example_bundle(), BEEP_ID, &INFO).unwrap();
    assert_eq!(inst.inputs, 0, "an instrument: no audio input");
    let mut node = ClapNode::new(inst, &INFO).unwrap();
    let ctx = Transport::new(48_000.0).context();
    let mut buf = AudioBuffer::new(2, 512);
    buf.clear();
    let on = [MidiEvent { frame: 100, msg: MidiMessage::NoteOn { note: 69, velocity: 100 } }];
    Instrument::render(&mut node, &mut buf, &ctx, &on);
    // The example plugin applies events at the block start (a simple
    // plugin's choice); what the host must get right is passing them on.
    assert!(buf.channel(0).iter().any(|&s| s.abs() > 0.1), "it plays");
    let mut buf = AudioBuffer::new(2, 512);
    buf.clear();
    let off = [MidiEvent { frame: 0, msg: MidiMessage::ForceOff { note: 69 } }];
    Instrument::render(&mut node, &mut buf, &ctx, &off);
    assert!(buf.channel(0).iter().all(|&s| s == 0.0), "and stops");
    Instrument::stop(&mut node);
}

#[test]
fn state_saves_and_restores_what_parameters_cant_see() {
    enter_audio_thread();
    let bundle = example_bundle();
    let a = Instance::create(&bundle, GAIN_ID, &INFO).unwrap();
    let mut node = ClapNode::new(a.clone(), &INFO).unwrap();
    Processor::set_param(&mut node, 0, 9.5);
    Processor::process(&mut node, &mut dc(0.1), &Transport::new(48_000.0).context());
    Processor::stop(&mut node);
    let state = a.save_state().unwrap();

    let b = Instance::create(&bundle, GAIN_ID, &INFO).unwrap();
    assert_eq!(b.param_value(0), Some(0.0));
    assert!(b.load_state(&state));
    assert_eq!(b.param_value(0), Some(9.5));
}

#[test]
fn an_instance_can_be_in_the_engine_only_once() {
    let inst = Instance::create(&example_bundle(), GAIN_ID, &INFO).unwrap();
    let first = ClapNode::new(inst.clone(), &INFO).unwrap();
    assert!(ClapNode::new(inst.clone(), &INFO).is_err());
    drop(first);
    assert!(ClapNode::new(inst, &INFO).is_ok(), "free again once it leaves");
}

/// The real thing: the plugin created on the main thread, processed by the
/// engine on its own audio thread, removed, and destroyed back on the main
/// thread. The plugin audits every call; there must be no violations.
#[test]
fn thread_rules_hold_through_the_real_engine() {
    use crate::engine::{Command, Engine, Garbage};
    use crate::midi::LiveMidi;
    use crate::mixer::{Meters, Target, Track};
    use crate::transport::TransportShared;
    use rtrb::RingBuffer;

    let (mut tx, rx) = RingBuffer::<Command>::new(64);
    let (gtx, mut grx) = RingBuffer::<Garbage>::new(64);
    let (_ltx, lrx) = RingBuffer::<LiveMidi>::new(4);
    let mut engine = Engine::new(&INFO, 2, TransportShared::new(), Meters::new(), rx, gtx, lrx);

    let inst = Instance::create(&example_bundle(), GAIN_ID, &INFO).unwrap(); // main thread
    // SAFETY: the plugin is alive (we hold an Arc).
    let records = unsafe { test_plugin::records(inst.raw()) };
    let node: Box<dyn Processor> = Box::new(ClapNode::new(inst.clone(), &INFO).unwrap());
    assert!(tx.push(Command::AddTrack { index: 0, track: Box::new(Track::new(&INFO)) }).is_ok());
    assert!(tx.push(Command::Insert { target: Target::Track(0), slot: 0, node }).is_ok());

    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let audio = std::thread::spawn(move || {
        let mut out = vec![0f32; 512 * 2];
        for _ in 0..10 {
            engine.render(&mut out);
        }
        done_tx.send(()).unwrap();
        go_rx.recv().unwrap(); // the main thread queues the removal
        engine.render(&mut out); // ...which this callback carries out
        engine
    });
    done_rx.recv().unwrap();
    assert!(tx.push(Command::Remove { target: Target::Track(0), slot: 0 }).is_ok());
    go_tx.send(()).unwrap();
    let _engine = audio.join().unwrap();

    // Back on the main thread: free what the engine retired, and our Arc.
    while let Ok(g) = grx.pop() {
        drop(g);
    }
    drop(inst);

    let violations = records.violations.lock().unwrap().clone();
    assert!(violations.is_empty(), "{violations:?}");
    let calls = records.calls.lock().unwrap().clone();
    // latency.get comes right after activate: CLAP only allows it while active.
    assert_eq!(calls, vec!["init", "params.get_info", "params.get_info", "params.get_info", "activate", "latency.get", "start_processing", "stop_processing", "deactivate", "destroy"]);
    assert_eq!(records.processed_blocks.load(std::sync::atomic::Ordering::Relaxed), 10);
}

#[test]
fn base64_round_trips() {
    assert_eq!(base64_encode(b"Man"), "TWFu");
    assert_eq!(base64_encode(b"M"), "TQ==");
    assert_eq!(base64_encode(b"Ma"), "TWE=");
    for len in 0..20 {
        let bytes: Vec<u8> = (0..len as u8).map(|i| i.wrapping_mul(37).wrapping_add(250)).collect();
        assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes);
    }
    assert_eq!(base64_decode("not base64!"), None);
}

#[test]
fn interning_keeps_one_copy() {
    let a = intern(&String::from("some plugin parameter"));
    let b = intern(&format!("some plugin {}", "parameter"));
    assert!(std::ptr::eq(a, b));
}

/// Loads a REAL plugin library from disk. Build the example first:
///   cargo build --manifest-path plugins/clap-example/Cargo.toml
/// then: DAW_TEST_CLAP=<path to the built library> cargo test real_library -- --ignored
#[test]
#[ignore]
fn real_library_loads_from_disk() {
    let path = std::env::var("DAW_TEST_CLAP").expect("set DAW_TEST_CLAP to a .clap file");
    let bundle = Bundle::load(Path::new(&path)).unwrap();
    assert_eq!(bundle.descriptors.len(), 3);
    enter_audio_thread();
    let inst = Instance::create(&bundle, GAIN_ID, &INFO).unwrap();
    let mut node = ClapNode::new(inst, &INFO).unwrap();
    Processor::set_param(&mut node, 0, -6.0206);
    let mut buf = dc(0.5);
    Processor::process(&mut node, &mut buf, &Transport::new(48_000.0).context());
    assert!((buf.channel(0)[0] - 0.25).abs() < 1e-4);
    Processor::stop(&mut node);
}

#[test]
fn scanning_finds_plugin_files_and_bundles() {
    let root = std::env::temp_dir().join("daw_clap_scan");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Vendor/Sub")).unwrap();
    std::fs::write(root.join("A.clap"), b"").unwrap();
    std::fs::write(root.join("Vendor/Sub/B.CLAP"), b"").unwrap();
    std::fs::write(root.join("readme.txt"), b"").unwrap();
    // A macOS-style bundle: a folder named *.clap, found but not entered.
    std::fs::create_dir_all(root.join("C.clap/Contents/MacOS")).unwrap();
    std::fs::write(root.join("C.clap/Contents/MacOS/C"), b"").unwrap();
    let found: Vec<String> = find_plugin_files(&[root.clone()]).iter().map(|p| p.strip_prefix(&root).unwrap().display().to_string()).collect();
    assert_eq!(found, vec!["A.clap", "C.clap", "Vendor/Sub/B.CLAP"]);
}

#[test]
fn a_file_that_isnt_a_plugin_is_refused_cleanly() {
    let path = std::env::temp_dir().join("daw_not_a_plugin.clap");
    std::fs::write(&path, b"definitely not a shared library").unwrap();
    let err = Bundle::load(&path).err().expect("refused");
    assert!(err.contains("couldn't load"), "{err}");
}

// ---- Step 12 exercises ----

#[test]
fn plugins_are_told_where_the_song_is() {
    use clap_sys::events::{CLAP_TRANSPORT_IS_LOOP_ACTIVE, CLAP_TRANSPORT_IS_PLAYING};
    use clap_sys::fixedpoint::CLAP_BEATTIME_FACTOR;
    use std::sync::atomic::Ordering::Relaxed;
    enter_audio_thread();
    let inst = Instance::create(&example_bundle(), GAIN_ID, &INFO).unwrap();
    // SAFETY: the plugin is alive.
    let rec = unsafe { test_plugin::records(inst.raw()) };
    let mut node = ClapNode::new(inst, &INFO).unwrap();
    let mut t = Transport::new(48_000.0);
    t.set_tempo(90.0);
    t.seek_beats(10.5); // bar 3 (0-based: 2), beat 2.5 of it
    t.set_loop(Some((8.0, 16.0)));
    t.playing = true;
    Processor::process(&mut node, &mut dc(0.1), &t.context());
    assert_eq!(f64::from_bits(rec.tempo.load(Relaxed)), 90.0);
    let beats = rec.song_pos_beats.load(Relaxed) as f64 / CLAP_BEATTIME_FACTOR as f64;
    assert!((beats - 10.5).abs() < 1e-4, "{beats}");
    assert_eq!(rec.bar_number.load(Relaxed), 2);
    assert_eq!(rec.loop_end_beats.load(Relaxed), 16 * CLAP_BEATTIME_FACTOR, "fixed point: exact");
    let flags = rec.transport_flags.load(Relaxed);
    assert!(flags & CLAP_TRANSPORT_IS_PLAYING != 0 && flags & CLAP_TRANSPORT_IS_LOOP_ACTIVE != 0);
    Processor::stop(&mut node);
}

/// Tiny blocks (64 frames = 1.33 ms of budget), and a plugin that burns
/// 2 ms per call: about 150% of the budget.
fn slow_plugin(id: &str) -> (Arc<Instance>, Arc<test_plugin::Records>, ClapNode) {
    use std::sync::atomic::Ordering::Relaxed;
    let small = PrepareInfo { sample_rate: 48_000.0, max_frames: 64, channels: 2 };
    let inst = Instance::create(&example_bundle(), id, &small).unwrap();
    // SAFETY: the plugin is alive.
    let rec = unsafe { test_plugin::records(inst.raw()) };
    rec.spin_micros.store(2_000, Relaxed);
    let node = ClapNode::new(inst.clone(), &small).unwrap();
    (inst, rec, node)
}

fn small_dc(value: f32) -> AudioBuffer {
    let mut b = AudioBuffer::new(2, 64);
    b.channel_mut(0).fill(value);
    b.channel_mut(1).fill(value);
    b
}

#[test]
fn the_watchdog_bypasses_a_plugin_that_is_too_slow() {
    use super::instance::WATCHDOG_BLOCKS;
    use std::sync::atomic::Ordering::Relaxed;
    enter_audio_thread();
    let (inst, rec, mut node) = slow_plugin(GAIN_ID);
    let ctx = Transport::new(48_000.0).context();
    Processor::set_param(&mut node, 0, -6.0206); // so the plugin's effect is visible
    for _ in 0..WATCHDOG_BLOCKS {
        Processor::process(&mut node, &mut small_dc(0.5), &ctx);
    }
    assert!(inst.is_overloaded(), "bypassed after {WATCHDOG_BLOCKS} slow blocks in a row");
    let mut buf = small_dc(0.5);
    Processor::process(&mut node, &mut buf, &ctx);
    assert_eq!(buf.channel(0)[0], 0.5, "an effect is bypassed by passing the audio through");
    assert_eq!(rec.processed_blocks.load(Relaxed), WATCHDOG_BLOCKS, "the plugin isn't called any more");

    let messages = inst.service();
    assert!(messages.iter().any(|m| m.contains("bypassed to stop the dropouts")), "{messages:?}");
    assert!(inst.service().iter().all(|m| !m.contains("bypassed")), "reported once, not every frame");

    rec.spin_micros.store(0, Relaxed); // the user fixes things, and re-enables it
    inst.reenable();
    let mut buf = small_dc(0.5);
    Processor::process(&mut node, &mut buf, &ctx);
    assert!((buf.channel(0)[0] - 0.25).abs() < 1e-4, "playing again");
    Processor::stop(&mut node);
}

#[test]
fn one_slow_block_is_forgiven() {
    use std::sync::atomic::Ordering::Relaxed;
    enter_audio_thread();
    let (inst, rec, mut node) = slow_plugin(GAIN_ID);
    let ctx = Transport::new(48_000.0).context();
    for k in 0..40 {
        // Slow only every fourth block: never 8 in a row.
        rec.spin_micros.store(if k % 4 == 0 { 2_000 } else { 0 }, Relaxed);
        Processor::process(&mut node, &mut small_dc(0.5), &ctx);
    }
    assert!(!inst.is_overloaded());
    Processor::stop(&mut node);
}

#[test]
fn a_bypassed_instrument_is_silent() {
    use super::instance::WATCHDOG_BLOCKS;
    enter_audio_thread();
    let (inst, _rec, mut node) = slow_plugin(BEEP_ID);
    let ctx = Transport::new(48_000.0).context();
    let on = [MidiEvent { frame: 0, msg: MidiMessage::NoteOn { note: 69, velocity: 100 } }];
    for k in 0..WATCHDOG_BLOCKS {
        let mut buf = AudioBuffer::new(2, 64);
        buf.clear();
        Instrument::render(&mut node, &mut buf, &ctx, if k == 0 { &on } else { &[] });
    }
    assert!(inst.is_overloaded());
    let mut buf = AudioBuffer::new(2, 64);
    buf.clear();
    Instrument::render(&mut node, &mut buf, &ctx, &[]);
    assert!(buf.channel(0).iter().all(|&s| s == 0.0), "silence: there's nothing to pass through");
    Instrument::stop(&mut node);
}

#[test]
fn plugins_report_their_latency() {
    let bundle = example_bundle();
    assert_eq!(Instance::create(&bundle, test_plugin::LOOKAHEAD_ID, &INFO).unwrap().latency, test_plugin::LOOKAHEAD_FRAMES as u32);
    assert_eq!(Instance::create(&bundle, GAIN_ID, &INFO).unwrap().latency, 0);
}
