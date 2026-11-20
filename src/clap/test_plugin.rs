// Two tiny CLAP plugins, written against the raw C interface, the way a
// plugin author would: "Example Gain" (an effect) and "Example Beep" (an
// instrument).
//
// They exist to test the host. Compiled into the tests, they run
// in-process; compiled by `plugins/clap-example` (which `include!`s this
// file), they become a real `.clap` library for testing real loading.
//
// Each plugin also AUDITS the host: every lifecycle call asks the host's
// thread-check extension which thread it's on, and records a violation if
// CLAP's rules are broken. The host's tests then assert there were none.

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{clap_event_header, clap_event_note, clap_event_param_value, clap_input_events, clap_output_events, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE};
use clap_sys::ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::log::{clap_host_log, CLAP_EXT_LOG, CLAP_LOG_INFO};
use clap_sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_HIDDEN, CLAP_PARAM_IS_STEPPED};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::id::{clap_id, CLAP_INVALID_ID};
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::plugin_features::{CLAP_PLUGIN_FEATURE_AUDIO_EFFECT, CLAP_PLUGIN_FEATURE_INSTRUMENT};
use clap_sys::process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;
use std::ffi::{c_char, c_void, CStr};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

macro_rules! lit {
    ($s:literal) => {
        concat!($s, "\0").as_ptr() as *const c_char
    };
}

pub const GAIN_ID: &str = "org.example.gain";
pub const BEEP_ID: &str = "org.example.beep";
pub const LOOKAHEAD_ID: &str = "org.example.lookahead";
/// Example Lookahead delays its audio by this much, and says so.
pub const LOOKAHEAD_FRAMES: usize = 100;
/// Parameter ids, deliberately not 0, 1, 2: hosts must not assume ids are
/// indices.
pub const GAIN_PARAM: clap_id = 7;
pub const INVERT_PARAM: clap_id = 3;
pub const HIDDEN_PARAM: clap_id = 99;
pub const LEVEL_PARAM: clap_id = 1;

struct Features([*const c_char; 2]);
// SAFETY: pointers to string literals, never written.
unsafe impl Sync for Features {}

static GAIN_FEATURES: Features = Features([lit!("audio-effect"), std::ptr::null()]);
static BEEP_FEATURES: Features = Features([lit!("instrument"), std::ptr::null()]);

static GAIN_DESC: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: lit!("org.example.gain"),
    name: lit!("Example Gain"),
    vendor: lit!("daw tutorial"),
    url: lit!(""),
    manual_url: lit!(""),
    support_url: lit!(""),
    version: lit!("1.0"),
    description: lit!("A gain effect for testing hosts"),
    features: &GAIN_FEATURES.0 as *const [*const c_char; 2] as *const *const c_char,
};

static LOOKAHEAD_DESC: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: lit!("org.example.lookahead"),
    name: lit!("Example Lookahead"),
    vendor: lit!("daw tutorial"),
    url: lit!(""),
    manual_url: lit!(""),
    support_url: lit!(""),
    version: lit!("1.0"),
    description: lit!("Delays its input by 100 frames and reports that latency, like a look-ahead limiter"),
    features: &GAIN_FEATURES.0 as *const [*const c_char; 2] as *const *const c_char,
};

static BEEP_DESC: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: lit!("org.example.beep"),
    name: lit!("Example Beep"),
    vendor: lit!("daw tutorial"),
    url: lit!(""),
    manual_url: lit!(""),
    support_url: lit!(""),
    version: lit!("1.0"),
    description: lit!("A one-voice sine instrument for testing hosts"),
    features: &BEEP_FEATURES.0 as *const [*const c_char; 2] as *const *const c_char,
};

// Keep the feature constants referenced, so they're checked to match.
const _: () = assert!(CLAP_PLUGIN_FEATURE_AUDIO_EFFECT.to_bytes().len() == 12 && CLAP_PLUGIN_FEATURE_INSTRUMENT.to_bytes().len() == 10);

/// What a test can inspect after the fact. Shared (`Arc`) so it survives
/// the plugin being destroyed.
pub struct Records {
    pub calls: Mutex<Vec<&'static str>>,
    pub violations: Mutex<Vec<String>>,
    pub processed_blocks: AtomicU32,
    /// The last transport the host sent: tempo (f64 bits), position
    /// (CLAP fixed-point beats), flags, bar number, loop end (fixed-point).
    pub tempo: AtomicU64,
    pub song_pos_beats: AtomicI64,
    pub transport_flags: AtomicU32,
    pub bar_number: AtomicI32,
    pub loop_end_beats: AtomicI64,
    /// A test-only knob: burn this many microseconds in every `process`,
    /// to play a plugin that's too slow.
    pub spin_micros: AtomicU32,
}

impl Default for Records {
    fn default() -> Self {
        Self {
            // Room reserved up front: start/stop_processing record calls on
            // the AUDIO thread, where a growing Vec would allocate. (The
            // host's allocation check caught exactly that.)
            calls: Mutex::new(Vec::with_capacity(64)),
            violations: Mutex::new(Vec::new()),
            processed_blocks: AtomicU32::new(0),
            tempo: AtomicU64::new(0),
            song_pos_beats: AtomicI64::new(0),
            transport_flags: AtomicU32::new(0),
            bar_number: AtomicI32::new(0),
            loop_end_beats: AtomicI64::new(0),
            spin_micros: AtomicU32::new(0),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Gain,
    Beep,
    Lookahead,
}

/// Example Lookahead's delay line. Only `process` touches it, and CLAP
/// never calls `process` twice at once, so it needs no lock.
struct DelayLine(UnsafeCell<([Vec<f32>; 2], usize)>);
// SAFETY: see above: one caller at a time, by the CLAP threading rules.
unsafe impl Sync for DelayLine {}

#[derive(Clone, Copy, PartialEq)]
enum Expect {
    Main,
    Audio,
}

struct Example {
    clap: clap_plugin,
    host: *const clap_host,
    kind: Kind,
    records: Arc<Records>,
    gain_db: AtomicU32,
    invert: AtomicBool,
    level: AtomicU32,
    note: AtomicI32,
    phase: AtomicU32,
    sample_rate: AtomicU32,
    active: AtomicBool,
    processing: AtomicBool,
    delay: DelayLine,
}

impl Example {
    /// SAFETY: `p` must be a plugin created by this factory, not destroyed.
    unsafe fn from<'a>(p: *const clap_plugin) -> &'a Example {
        &*((*p).plugin_data as *const Example)
    }

    fn f32(a: &AtomicU32) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }

    /// Records the call and checks it happened on the thread CLAP requires.
    fn check(&self, call: &'static str, expect: Expect) {
        if let Ok(mut c) = self.records.calls.lock() {
            if call != "process" {
                c.push(call);
            }
        }
        // SAFETY: the host outlives the plugin; extensions are static.
        let ok = unsafe {
            let tc = (*self.host).get_extension.map_or(std::ptr::null(), |g| g(self.host, CLAP_EXT_THREAD_CHECK.as_ptr())) as *const clap_host_thread_check;
            match tc.as_ref() {
                None => true, // host offers no thread check: can't audit
                Some(tc) => match expect {
                    Expect::Main => tc.is_main_thread.map_or(true, |f| f(self.host)),
                    Expect::Audio => tc.is_audio_thread.map_or(true, |f| f(self.host)),
                },
            }
        };
        if !ok {
            if let Ok(mut v) = self.records.violations.lock() {
                v.push(format!("{call} called off the {} thread", if expect == Expect::Main { "main" } else { "audio" }));
            }
        }
    }

    fn apply(&self, header: *const clap_event_header) {
        // SAFETY: the host hands us valid events of the type in the header.
        unsafe {
            match (*header).type_ {
                CLAP_EVENT_PARAM_VALUE => {
                    let e = &*(header as *const clap_event_param_value);
                    match e.param_id {
                        GAIN_PARAM => self.gain_db.store((e.value as f32).to_bits(), Ordering::Relaxed),
                        INVERT_PARAM => self.invert.store(e.value >= 0.5, Ordering::Relaxed),
                        LEVEL_PARAM => self.level.store((e.value as f32).to_bits(), Ordering::Relaxed),
                        _ => {}
                    }
                }
                CLAP_EVENT_NOTE_ON => self.note.store((*(header as *const clap_event_note)).key as i32, Ordering::Relaxed),
                CLAP_EVENT_NOTE_OFF => {
                    let key = (*(header as *const clap_event_note)).key as i32;
                    let _ = self.note.compare_exchange(key, -1, Ordering::Relaxed, Ordering::Relaxed);
                }
                _ => {}
            }
        }
    }

    fn read_events(&self, events: *const clap_input_events) {
        // SAFETY: the host's event list is valid for the call.
        unsafe {
            let Some(list) = events.as_ref() else { return };
            let n = list.size.map_or(0, |s| s(events));
            for i in 0..n {
                if let Some(h) = list.get.map(|g| g(events, i)).filter(|h| !h.is_null()) {
                    self.apply(h);
                }
            }
        }
    }
}

/// For tests: the records of a plugin the host created.
///
/// SAFETY: `p` must be a live plugin from this file's factory.
pub unsafe fn records(p: *const clap_plugin) -> Arc<Records> {
    Example::from(p).records.clone()
}

unsafe extern "C" fn init(p: *const clap_plugin) -> bool {
    let me = Example::from(p);
    me.check("init", Expect::Main);
    // Say hello through the host's log, if it has one.
    let log = (*me.host).get_extension.map_or(std::ptr::null(), |g| g(me.host, CLAP_EXT_LOG.as_ptr())) as *const clap_host_log;
    if let Some(log) = log.as_ref().and_then(|l| l.log) {
        log(me.host, CLAP_LOG_INFO, lit!("hello from the example plugin"));
    }
    true
}

unsafe extern "C" fn destroy(p: *const clap_plugin) {
    let me = Example::from(p);
    me.check("destroy", Expect::Main);
    if me.active.load(Ordering::Relaxed) {
        me.records.violations.lock().unwrap().push("destroyed while still active".into());
    }
    drop(Box::from_raw((*p).plugin_data as *mut Example));
}

unsafe extern "C" fn activate(p: *const clap_plugin, sample_rate: f64, _min: u32, _max: u32) -> bool {
    let me = Example::from(p);
    me.check("activate", Expect::Main);
    me.sample_rate.store((sample_rate as f32).to_bits(), Ordering::Relaxed);
    me.active.store(true, Ordering::Relaxed);
    true
}

unsafe extern "C" fn deactivate(p: *const clap_plugin) {
    let me = Example::from(p);
    me.check("deactivate", Expect::Main);
    if me.processing.load(Ordering::Relaxed) {
        me.records.violations.lock().unwrap().push("deactivated while still processing".into());
    }
    me.active.store(false, Ordering::Relaxed);
}

unsafe extern "C" fn start_processing(p: *const clap_plugin) -> bool {
    let me = Example::from(p);
    me.check("start_processing", Expect::Audio);
    me.processing.store(true, Ordering::Relaxed);
    true
}

unsafe extern "C" fn stop_processing(p: *const clap_plugin) {
    let me = Example::from(p);
    me.check("stop_processing", Expect::Audio);
    me.processing.store(false, Ordering::Relaxed);
}

unsafe extern "C" fn reset(_p: *const clap_plugin) {}

unsafe extern "C" fn process(p: *const clap_plugin, process: *const clap_process) -> clap_process_status {
    let me = Example::from(p);
    me.check("process", Expect::Audio);
    if !me.processing.load(Ordering::Relaxed) {
        me.records.violations.lock().unwrap().push("process before start_processing".into());
    }
    let Some(pr) = process.as_ref() else { return CLAP_PROCESS_ERROR };
    me.read_events(pr.in_events);
    if let Some(t) = pr.transport.as_ref() {
        let r = &me.records;
        r.tempo.store(t.tempo.to_bits(), Ordering::Relaxed);
        r.song_pos_beats.store(t.song_pos_beats, Ordering::Relaxed);
        r.transport_flags.store(t.flags, Ordering::Relaxed);
        r.bar_number.store(t.bar_number, Ordering::Relaxed);
        r.loop_end_beats.store(t.loop_end_beats, Ordering::Relaxed);
    }
    let spin = me.records.spin_micros.load(Ordering::Relaxed);
    if spin > 0 {
        let until = std::time::Instant::now() + std::time::Duration::from_micros(spin as u64);
        while std::time::Instant::now() < until {
            std::hint::spin_loop();
        }
    }
    let n = pr.frames_count as usize;
    let Some(out) = pr.audio_outputs.as_ref() else { return CLAP_PROCESS_ERROR };
    let out_ch = out.channel_count as usize;
    match me.kind {
        Kind::Gain => {
            let Some(input) = pr.audio_inputs.as_ref() else { return CLAP_PROCESS_ERROR };
            let g = 10f32.powf(Example::f32(&me.gain_db) / 20.0) * if me.invert.load(Ordering::Relaxed) { -1.0 } else { 1.0 };
            for ch in 0..out_ch.min(input.channel_count as usize) {
                let src = std::slice::from_raw_parts(*input.data32.add(ch), n);
                let dst = std::slice::from_raw_parts_mut(*out.data32.add(ch), n);
                for (d, s) in dst.iter_mut().zip(src) {
                    *d = s * g;
                }
            }
        }
        Kind::Lookahead => {
            let Some(input) = pr.audio_inputs.as_ref() else { return CLAP_PROCESS_ERROR };
            let (lines, pos) = &mut *me.delay.0.get();
            let len = lines[0].len();
            let start = *pos;
            for ch in 0..out_ch.min(input.channel_count as usize).min(2) {
                let src = std::slice::from_raw_parts(*input.data32.add(ch), n);
                let dst = std::slice::from_raw_parts_mut(*out.data32.add(ch), n);
                let line = &mut lines[ch];
                for i in 0..n {
                    let at = (start + i) % len;
                    dst[i] = line[at]; // what went in LOOKAHEAD_FRAMES ago
                    line[at] = src[i];
                }
            }
            *pos = (start + n) % len;
        }
        Kind::Beep => {
            let note = me.note.load(Ordering::Relaxed);
            let level = Example::f32(&me.level);
            let freq = 440.0 * 2f32.powf((note as f32 - 69.0) / 12.0);
            let step = freq / Example::f32(&me.sample_rate);
            let mut phase = Example::f32(&me.phase);
            for i in 0..n {
                let s = if note >= 0 { (phase * std::f32::consts::TAU).sin() * level } else { 0.0 };
                for ch in 0..out_ch {
                    *(*out.data32.add(ch)).add(i) = s;
                }
                phase = (phase + step) % 1.0;
            }
            me.phase.store(phase.to_bits(), Ordering::Relaxed);
        }
    }
    me.records.processed_blocks.fetch_add(1, Ordering::Relaxed);
    let _: (*const clap_output_events, *const clap_audio_buffer) = (pr.out_events, pr.audio_inputs);
    CLAP_PROCESS_CONTINUE
}

unsafe extern "C" fn get_extension(_p: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = CStr::from_ptr(id);
    if id == CLAP_EXT_PARAMS {
        &PARAMS as *const clap_plugin_params as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &PORTS as *const clap_plugin_audio_ports as *const c_void
    } else if id == CLAP_EXT_STATE {
        &STATE as *const clap_plugin_state as *const c_void
    } else if id == CLAP_EXT_LATENCY {
        &LATENCY as *const clap_plugin_latency as *const c_void
    } else {
        std::ptr::null()
    }
}

unsafe extern "C" fn on_main_thread(p: *const clap_plugin) {
    Example::from(p).check("on_main_thread", Expect::Main);
}

// ---- latency ----

static LATENCY: clap_plugin_latency = clap_plugin_latency { get: Some(latency_get) };

unsafe extern "C" fn latency_get(p: *const clap_plugin) -> u32 {
    let me = Example::from(p);
    me.check("latency.get", Expect::Main);
    if me.kind == Kind::Lookahead { LOOKAHEAD_FRAMES as u32 } else { 0 }
}

// ---- params ----

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: Some(params_text_to_value),
    flush: Some(params_flush),
};

unsafe extern "C" fn params_count(p: *const clap_plugin) -> u32 {
    match Example::from(p).kind {
        Kind::Gain => 3,
        Kind::Beep => 1,
        Kind::Lookahead => 0,
    }
}

fn copy_name(dst: &mut [c_char], name: &str) {
    for (d, b) in dst.iter_mut().zip(name.bytes().chain(std::iter::once(0))) {
        *d = b as c_char;
    }
}

unsafe extern "C" fn params_get_info(p: *const clap_plugin, index: u32, info: *mut clap_param_info) -> bool {
    let me = Example::from(p);
    me.check("params.get_info", Expect::Main);
    let Some(info) = info.as_mut() else { return false };
    let (id, name, flags, min, max, default) = match (me.kind, index) {
        (Kind::Gain, 0) => (GAIN_PARAM, "Gain", 0, -24.0, 24.0, 0.0),
        (Kind::Gain, 1) => (INVERT_PARAM, "Invert", CLAP_PARAM_IS_STEPPED, 0.0, 1.0, 0.0),
        (Kind::Gain, 2) => (HIDDEN_PARAM, "Secret", CLAP_PARAM_IS_HIDDEN, 0.0, 1.0, 0.0),
        (Kind::Beep, 0) => (LEVEL_PARAM, "Level", 0, 0.0, 1.0, 0.5),
        _ => return false,
    };
    info.id = id;
    info.flags = flags;
    info.cookie = std::ptr::null_mut();
    copy_name(&mut info.name, name);
    copy_name(&mut info.module, "");
    info.min_value = min;
    info.max_value = max;
    info.default_value = default;
    true
}

unsafe extern "C" fn params_get_value(p: *const clap_plugin, id: clap_id, out: *mut f64) -> bool {
    let me = Example::from(p);
    let v = match id {
        GAIN_PARAM => Example::f32(&me.gain_db) as f64,
        INVERT_PARAM => me.invert.load(Ordering::Relaxed) as u8 as f64,
        LEVEL_PARAM => Example::f32(&me.level) as f64,
        HIDDEN_PARAM => 0.0,
        _ => return false,
    };
    *out = v;
    true
}

unsafe extern "C" fn params_value_to_text(_p: *const clap_plugin, _id: clap_id, _v: f64, _buf: *mut c_char, _cap: u32) -> bool {
    false
}

unsafe extern "C" fn params_text_to_value(_p: *const clap_plugin, _id: clap_id, _text: *const c_char, _out: *mut f64) -> bool {
    false
}

unsafe extern "C" fn params_flush(p: *const clap_plugin, events: *const clap_input_events, _out: *const clap_output_events) {
    Example::from(p).read_events(events);
}

// ---- audio ports ----

static PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(ports_count), get: Some(ports_get) };

unsafe extern "C" fn ports_count(p: *const clap_plugin, is_input: bool) -> u32 {
    match (Example::from(p).kind, is_input) {
        (Kind::Beep, true) => 0,
        _ => 1,
    }
}

unsafe extern "C" fn ports_get(_p: *const clap_plugin, index: u32, _is_input: bool, info: *mut clap_audio_port_info) -> bool {
    let Some(info) = info.as_mut().filter(|_| index == 0) else { return false };
    info.id = 0;
    copy_name(&mut info.name, "main");
    info.flags = CLAP_AUDIO_PORT_IS_MAIN;
    info.channel_count = 2;
    info.port_type = std::ptr::null();
    info.in_place_pair = CLAP_INVALID_ID;
    true
}

// ---- state: [gain f32][invert u8] or [level f32] ----

static STATE: clap_plugin_state = clap_plugin_state { save: Some(state_save), load: Some(state_load) };

unsafe extern "C" fn state_save(p: *const clap_plugin, stream: *const clap_ostream) -> bool {
    let me = Example::from(p);
    me.check("state.save", Expect::Main);
    let mut bytes = Vec::new();
    match me.kind {
        Kind::Gain => {
            bytes.extend_from_slice(&Example::f32(&me.gain_db).to_le_bytes());
            bytes.push(me.invert.load(Ordering::Relaxed) as u8);
        }
        Kind::Beep => bytes.extend_from_slice(&Example::f32(&me.level).to_le_bytes()),
        Kind::Lookahead => {}
    }
    let Some(write) = (*stream).write else { return false };
    write(stream, bytes.as_ptr() as *const c_void, bytes.len() as u64) == bytes.len() as i64
}

unsafe extern "C" fn state_load(p: *const clap_plugin, stream: *const clap_istream) -> bool {
    let me = Example::from(p);
    me.check("state.load", Expect::Main);
    let mut buf = [0u8; 5];
    let Some(read) = (*stream).read else { return false };
    let n = read(stream, buf.as_mut_ptr() as *mut c_void, 5);
    match (me.kind, n) {
        (Kind::Gain, 5) => {
            me.gain_db.store(f32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]).to_bits(), Ordering::Relaxed);
            me.invert.store(buf[4] != 0, Ordering::Relaxed);
            true
        }
        (Kind::Beep, 4) => {
            me.level.store(f32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]).to_bits(), Ordering::Relaxed);
            true
        }
        (Kind::Lookahead, 0) => true,
        _ => false,
    }
}

// ---- factory and entry ----

static FACTORY: clap_plugin_factory = clap_plugin_factory { get_plugin_count: Some(factory_count), get_plugin_descriptor: Some(factory_descriptor), create_plugin: Some(factory_create) };

unsafe extern "C" fn factory_count(_f: *const clap_plugin_factory) -> u32 {
    3
}

unsafe extern "C" fn factory_descriptor(_f: *const clap_plugin_factory, index: u32) -> *const clap_plugin_descriptor {
    match index {
        0 => &GAIN_DESC,
        1 => &BEEP_DESC,
        2 => &LOOKAHEAD_DESC,
        _ => std::ptr::null(),
    }
}

unsafe extern "C" fn factory_create(_f: *const clap_plugin_factory, host: *const clap_host, id: *const c_char) -> *const clap_plugin {
    let id = CStr::from_ptr(id).to_bytes();
    let (kind, desc): (Kind, *const clap_plugin_descriptor) = match id {
        b"org.example.gain" => (Kind::Gain, &GAIN_DESC),
        b"org.example.beep" => (Kind::Beep, &BEEP_DESC),
        b"org.example.lookahead" => (Kind::Lookahead, &LOOKAHEAD_DESC),
        _ => return std::ptr::null(),
    };
    let me = Box::new(Example {
        clap: clap_plugin {
            desc,
            plugin_data: std::ptr::null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(deactivate),
            start_processing: Some(start_processing),
            stop_processing: Some(stop_processing),
            reset: Some(reset),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(on_main_thread),
        },
        host,
        kind,
        records: Arc::new(Records::default()),
        gain_db: AtomicU32::new(0f32.to_bits()),
        invert: AtomicBool::new(false),
        level: AtomicU32::new(0.5f32.to_bits()),
        note: AtomicI32::new(-1),
        phase: AtomicU32::new(0),
        sample_rate: AtomicU32::new(48_000f32.to_bits()),
        active: AtomicBool::new(false),
        processing: AtomicBool::new(false),
        delay: DelayLine(UnsafeCell::new(([vec![0.0; LOOKAHEAD_FRAMES], vec![0.0; LOOKAHEAD_FRAMES]], 0))),
    });
    let raw = Box::into_raw(me);
    (*raw).clap.plugin_data = raw as *mut c_void;
    &(*raw).clap
}

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
    if CStr::from_ptr(id) == CLAP_PLUGIN_FACTORY_ID {
        &FACTORY as *const clap_plugin_factory as *const c_void
    } else {
        std::ptr::null()
    }
}

/// The entry point's contents. A `const`, so a real plugin library can
/// export it under the required name (a static can't be initialized from
/// another static).
pub const ENTRY_DEF: clap_plugin_entry = clap_plugin_entry { clap_version: CLAP_VERSION, init: Some(entry_init), deinit: Some(entry_deinit), get_factory: Some(entry_get_factory) };

/// The entry point, for compiling the plugins into the host's tests.
pub static ENTRY: clap_plugin_entry = ENTRY_DEF;
