//! One plugin instance, and the node that plays it in the engine.

use super::{intern, Bundle, Descriptor, HostShared};
use crate::buffer::AudioBuffer;
use crate::midi::{MidiEvent, MidiMessage};
use crate::processor::{Instrument, ParamInfo, ParamKind, PrepareInfo, Processor};
use crate::transport::ProcessContext;
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::{
    clap_event_header, clap_event_note, clap_event_param_value, clap_event_transport, clap_input_events, clap_output_events,
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_TRANSPORT,
    CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_HAS_TIME_SIGNATURE,
    CLAP_TRANSPORT_IS_LOOP_ACTIVE, CLAP_TRANSPORT_IS_PLAYING,
};
use clap_sys::ext::gui::{clap_plugin_gui, clap_window, CLAP_EXT_GUI};
use clap_sys::ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::posix_fd_support::{clap_plugin_posix_fd_support, CLAP_EXT_POSIX_FD_SUPPORT};
use clap_sys::ext::timer_support::{clap_plugin_timer_support, CLAP_EXT_TIMER_SUPPORT};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::ext::audio_ports::{clap_audio_port_info, clap_plugin_audio_ports, CLAP_EXT_AUDIO_PORTS};
use clap_sys::ext::params::{clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_HIDDEN, CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{clap_process, CLAP_PROCESS_ERROR};
use clap_sys::stream::{clap_istream, clap_ostream};
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// A plugin instance: created, initialized and activated, ready to be
/// processed. This is the main-thread side; `ClapNode` is the audio side.
/// Both hold an `Arc` to it, because CLAP lets the main thread call some
/// functions (saving state, reading values) while the audio thread is
/// processing.
pub struct Instance {
    plugin: *const clap_plugin,
    pub descriptor: Descriptor,
    pub name: &'static str,
    /// The plugin's visible parameters, as the DAW describes parameters.
    pub params: &'static [ParamInfo],
    /// The plugin's own id for each of those (CLAP ids aren't indices).
    pub param_ids: Vec<clap_id>,
    params_ext: *const clap_plugin_params,
    state_ext: *const clap_plugin_state,
    gui_ext: *const clap_plugin_gui,
    timer_ext: *const clap_plugin_timer_support,
    fd_ext: *const clap_plugin_posix_fd_support,
    /// Channels of the main input and output ports (0 input: an instrument).
    pub inputs: u32,
    pub outputs: u32,
    /// The highest share of a block's time spent in `process` since the UI
    /// last asked (f32 bits; 1.0 = the whole time budget).
    cpu: AtomicU32,
    /// The plugin returned an error from `process`: it's bypassed.
    pub failed: AtomicBool,
    /// The watchdog bypassed it for being too slow (see `ClapNode::run`).
    overloaded: AtomicBool,
    overload_reported: AtomicBool,
    /// How many frames late the plugin's output is (a look-ahead limiter,
    /// a linear-phase EQ...). The mixer compensates for it.
    pub latency: u32,
    /// A plugin instance may be in only one place in the engine at once.
    in_engine: AtomicBool,
    // The plugin holds pointers to `host` and runs code from `bundle`, so
    // both are declared after everything else: Rust drops fields in order,
    // after `Drop::drop` below has destroyed the plugin.
    host: Box<HostShared>,
    _bundle: Arc<Bundle>,
}

// SAFETY: CLAP's threading model allows a plugin instance to be used from
// the main thread and the audio thread concurrently, each calling only the
// functions marked for it. Every use below follows those markings.
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

/// Reads a CLAP fixed-size string field.
fn fixed_str(chars: &[std::ffi::c_char]) -> String {
    // Up to the first NUL, or the whole array if a plugin forgot one.
    let bytes: Vec<u8> = chars.iter().map(|&c| c as u8).take_while(|&b| b != 0).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One leaked parameter table per distinct plugin parameter list: an
/// instance's names and ranges are needed as `&'static` (see `intern`).
fn intern_params(key: String, params: Vec<ParamInfo>) -> &'static [ParamInfo] {
    static TABLES: OnceLock<Mutex<HashMap<String, &'static [ParamInfo]>>> = OnceLock::new();
    let mut tables = TABLES.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    tables.entry(key).or_insert_with(|| Box::leak(params.into_boxed_slice()))
}

impl Instance {
    /// Creates, initializes and activates plugin `id` from `bundle`.
    /// [main-thread]
    pub fn create(bundle: &Arc<Bundle>, id: &str, info: &PrepareInfo) -> Result<Arc<Instance>, String> {
        let descriptor = bundle.descriptors.iter().find(|d| d.id == id).cloned().ok_or_else(|| format!("{} has no plugin {id}", bundle.path.display()))?;
        let host = HostShared::new();
        let c_id = CString::new(id).map_err(|_| "bad plugin id")?;
        // SAFETY: the factory is valid while the bundle is loaded, and
        // `host` outlives the plugin (it's dropped after it, below).
        let plugin = unsafe {
            let factory = bundle.factory();
            let create = (*factory).create_plugin.ok_or("factory can't create plugins")?;
            create(factory, host.as_clap(), c_id.as_ptr())
        };
        // SAFETY: from here on, `plugin` is a live instance until destroyed.
        let p = unsafe { plugin.as_ref() }.ok_or_else(|| format!("couldn't create {id}"))?;
        let destroy = || unsafe {
            if let Some(d) = p.destroy {
                d(plugin)
            }
        };
        if !unsafe { p.init.map_or(false, |init| init(plugin)) } {
            destroy();
            return Err(format!("{} failed to initialize", descriptor.name));
        }
        let ext = |id: &CStr| -> *const c_void { unsafe { p.get_extension.map_or(std::ptr::null(), |g| g(plugin, id.as_ptr())) } };
        let params_ext = ext(CLAP_EXT_PARAMS) as *const clap_plugin_params;
        let state_ext = ext(CLAP_EXT_STATE) as *const clap_plugin_state;
        let ports_ext = ext(CLAP_EXT_AUDIO_PORTS) as *const clap_plugin_audio_ports;

        // Parameters: the visible, writable ones become DAW parameters.
        let mut params = Vec::new();
        let mut param_ids = Vec::new();
        if let Some(ext) = unsafe { params_ext.as_ref() } {
            let count = unsafe { ext.count.map_or(0, |c| c(plugin)) };
            for i in 0..count {
                // SAFETY: a zeroed clap_param_info is a valid "empty" one
                // (all integers, floats, null pointers and NUL chars).
                let mut info: clap_param_info = unsafe { std::mem::zeroed() };
                if !unsafe { ext.get_info.map_or(false, |g| g(plugin, i, &mut info)) } {
                    continue;
                }
                if info.flags & (CLAP_PARAM_IS_HIDDEN | CLAP_PARAM_IS_READONLY) != 0 || info.max_value < info.min_value {
                    continue;
                }
                let (min, max) = (info.min_value as f32, info.max_value as f32);
                let kind = match info.flags & CLAP_PARAM_IS_STEPPED != 0 {
                    true if min == 0.0 && max == 1.0 => ParamKind::Toggle,
                    true => ParamKind::Int,
                    false => ParamKind::Linear,
                };
                let name = intern(&fixed_str(&info.name));
                params.push(ParamInfo { name, min, max, default: (info.default_value as f32).clamp(min, max), unit: "", kind });
                param_ids.push(info.id);
            }
        }
        let key = format!("{id}|{params:?}");

        // Audio ports: the first input and output port's channel counts.
        let port_channels = |is_input: bool| -> u32 {
            let Some(ext) = (unsafe { ports_ext.as_ref() }) else { return if is_input && descriptor.is_instrument() { 0 } else { 2 } };
            if unsafe { ext.count.map_or(0, |c| c(plugin, is_input)) } == 0 {
                return 0;
            }
            let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
            if unsafe { ext.get.map_or(false, |g| g(plugin, 0, is_input, &mut info)) } {
                info.channel_count.min(2)
            } else {
                2
            }
        };
        let (inputs, outputs) = (port_channels(true), port_channels(false));
        if outputs == 0 {
            destroy();
            return Err(format!("{} has no audio output", descriptor.name));
        }

        // Activation allocates the plugin's buffers: main thread, before
        // the engine ever sees it.
        let max = info.max_frames as u32;
        if !unsafe { p.activate.map_or(false, |a| a(plugin, info.sample_rate as f64, 1, max)) } {
            destroy();
            return Err(format!("{} failed to activate", descriptor.name));
        }
        // Latency may only be asked once active [main-thread & active].
        let latency_ext = ext(CLAP_EXT_LATENCY) as *const clap_plugin_latency;
        let latency = unsafe { latency_ext.as_ref().and_then(|l| l.get).map_or(0, |g| g(plugin)) };
        let gui_ext = ext(CLAP_EXT_GUI) as *const clap_plugin_gui;
        let timer_ext = ext(CLAP_EXT_TIMER_SUPPORT) as *const clap_plugin_timer_support;
        let fd_ext = ext(CLAP_EXT_POSIX_FD_SUPPORT) as *const clap_plugin_posix_fd_support;
        Ok(Arc::new(Instance {
            plugin,
            name: intern(&descriptor.name),
            descriptor,
            params: intern_params(key, params),
            param_ids,
            params_ext,
            state_ext,
            gui_ext,
            timer_ext,
            fd_ext,
            inputs,
            outputs,
            cpu: AtomicU32::new(0),
            failed: AtomicBool::new(false),
            overloaded: AtomicBool::new(false),
            overload_reported: AtomicBool::new(false),
            latency,
            in_engine: AtomicBool::new(false),
            host,
            _bundle: bundle.clone(),
        }))
    }

    /// The plugin's opaque state: everything it needs to be restored,
    /// including what isn't a parameter. [main-thread]
    pub fn save_state(&self) -> Option<Vec<u8>> {
        let ext = unsafe { self.state_ext.as_ref() }?;
        let mut bytes: Vec<u8> = Vec::new();
        let stream = clap_ostream { ctx: (&mut bytes as *mut Vec<u8>).cast(), write: Some(stream_write) };
        // SAFETY: `stream` and `bytes` outlive the call.
        let ok = unsafe { ext.save.map_or(false, |s| s(self.plugin, &stream)) };
        ok.then_some(bytes)
    }

    /// [main-thread]
    pub fn load_state(&self, bytes: &[u8]) -> bool {
        let Some(ext) = (unsafe { self.state_ext.as_ref() }) else { return false };
        let mut reader = Reader { bytes, pos: 0 };
        let stream = clap_istream { ctx: (&mut reader as *mut Reader).cast(), read: Some(stream_read) };
        // SAFETY: `stream` and `reader` outlive the call.
        unsafe { ext.load.map_or(false, |l| l(self.plugin, &stream)) }
    }

    /// The current value of parameter `index` (for after loading state).
    /// [main-thread]
    pub fn param_value(&self, index: usize) -> Option<f32> {
        let ext = unsafe { self.params_ext.as_ref() }?;
        let id = *self.param_ids.get(index)?;
        let mut v = 0.0f64;
        unsafe { ext.get_value.map_or(false, |g| g(self.plugin, id, &mut v)) }.then_some(v as f32)
    }

    /// Answers the plugin's requests and collects what it logged. Call
    /// regularly. [main-thread]
    pub fn service(&self) -> Vec<String> {
        if self.host.callback_requested.swap(false, Ordering::Relaxed) {
            // SAFETY: the plugin asked for exactly this, on this thread.
            unsafe {
                if let Some(f) = (*self.plugin).on_main_thread {
                    f(self.plugin)
                }
            }
        }
        self.run_timers();
        self.poll_fds();
        let mut log = self.host.take_log();
        // The watchdog DECIDED on the audio thread; it's REPORTED here,
        // because building a message allocates.
        if self.is_overloaded() && !self.overload_reported.swap(true, Ordering::Relaxed) {
            log.push(format!(
                "{} used more than {:.0}% of the time budget for {WATCHDOG_BLOCKS} blocks in a row, so it was bypassed to stop the dropouts. Re-enable it in the inspector.",
                self.name,
                WATCHDOG_LIMIT * 100.0
            ));
        }
        if self.host.restart_requested.swap(false, Ordering::Relaxed) {
            log.push(format!("{} asked to be restarted, which isn't supported yet: remove and re-add it if it misbehaves", self.name));
        }
        log
    }

    /// The highest CPU share since the last call: 0.5 = half the block's
    /// time budget went to this plugin.
    pub fn take_cpu(&self) -> f32 {
        f32::from_bits(self.cpu.swap(0, Ordering::Relaxed))
    }

    /// Calls the plugin back for every timer that's due. [main-thread]
    fn run_timers(&self) {
        let Some(on_timer) = (unsafe { self.timer_ext.as_ref() }).and_then(|t| t.on_timer) else { return };
        let now = Instant::now();
        // Collect first: the plugin may (un)register timers from inside its
        // callback, which takes the same lock.
        let due: Vec<clap_id> = match self.host.timers.lock() {
            Ok(mut timers) => timers
                .iter_mut()
                .filter(|t| t.2 <= now)
                .map(|t| {
                    t.2 = now + t.1;
                    t.0
                })
                .collect(),
            Err(_) => return,
        };
        for id in due {
            // SAFETY: a timer the plugin registered, called on the main thread.
            unsafe { on_timer(self.plugin, id) };
        }
    }

    /// Tells the plugin which of its file descriptors are ready (without
    /// waiting: this runs once per UI frame). [main-thread]
    #[cfg(unix)]
    fn poll_fds(&self) {
        use clap_sys::ext::posix_fd_support::{CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE};
        let Some(on_fd) = (unsafe { self.fd_ext.as_ref() }).and_then(|f| f.on_fd) else { return };
        let watched: Vec<(i32, u32)> = match self.host.fds.lock() {
            Ok(fds) => fds.clone(),
            Err(_) => return,
        };
        if watched.is_empty() {
            return;
        }
        let mut polls: Vec<libc::pollfd> = watched
            .iter()
            .map(|&(fd, flags)| {
                let mut events = 0;
                if flags & CLAP_POSIX_FD_READ != 0 {
                    events |= libc::POLLIN;
                }
                if flags & CLAP_POSIX_FD_WRITE != 0 {
                    events |= libc::POLLOUT;
                }
                libc::pollfd { fd, events, revents: 0 }
            })
            .collect();
        // SAFETY: a valid array of pollfd, and a zero timeout.
        if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, 0) } <= 0 {
            return;
        }
        for p in polls.iter().filter(|p| p.revents != 0) {
            let mut flags = 0;
            if p.revents & libc::POLLIN != 0 {
                flags |= CLAP_POSIX_FD_READ;
            }
            if p.revents & libc::POLLOUT != 0 {
                flags |= CLAP_POSIX_FD_WRITE;
            }
            if p.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                flags |= CLAP_POSIX_FD_ERROR;
            }
            // SAFETY: an fd the plugin registered, on the main thread.
            unsafe { on_fd(self.plugin, p.fd, flags) };
        }
    }

    #[cfg(not(unix))]
    fn poll_fds(&self) {}

    // ---- GUI [main-thread]: thin wrappers, used by `Editor` ----

    pub fn has_gui(&self) -> bool {
        !self.gui_ext.is_null()
    }

    pub(super) fn host_shared(&self) -> &HostShared {
        &self.host
    }

    fn gui(&self) -> Option<&clap_plugin_gui> {
        // SAFETY: the plugin's extension table lives as long as the plugin.
        unsafe { self.gui_ext.as_ref() }
    }

    pub(super) fn gui_is_api_supported(&self, api: &CStr, floating: bool) -> bool {
        self.gui().and_then(|g| g.is_api_supported).map_or(false, |f| unsafe { f(self.plugin, api.as_ptr(), floating) })
    }

    pub(super) fn gui_create(&self, api: &CStr, floating: bool) -> bool {
        self.gui().and_then(|g| g.create).map_or(false, |f| unsafe { f(self.plugin, api.as_ptr(), floating) })
    }

    pub(super) fn gui_destroy(&self) {
        if let Some(f) = self.gui().and_then(|g| g.destroy) {
            unsafe { f(self.plugin) }
        }
    }

    pub(super) fn gui_set_scale(&self, scale: f64) -> bool {
        self.gui().and_then(|g| g.set_scale).map_or(false, |f| unsafe { f(self.plugin, scale) })
    }

    pub(super) fn gui_size(&self) -> Option<(u32, u32)> {
        let f = self.gui()?.get_size?;
        let (mut w, mut h) = (0u32, 0u32);
        unsafe { f(self.plugin, &mut w, &mut h) }.then_some((w, h))
    }

    pub(super) fn gui_can_resize(&self) -> bool {
        self.gui().and_then(|g| g.can_resize).map_or(false, |f| unsafe { f(self.plugin) })
    }

    /// The nearest size the plugin can actually be.
    pub(super) fn gui_adjust_size(&self, w: u32, h: u32) -> (u32, u32) {
        let Some(f) = self.gui().and_then(|g| g.adjust_size) else { return (w, h) };
        let (mut w2, mut h2) = (w, h);
        if unsafe { f(self.plugin, &mut w2, &mut h2) } {
            (w2, h2)
        } else {
            (w, h)
        }
    }

    pub(super) fn gui_set_size(&self, w: u32, h: u32) -> bool {
        self.gui().and_then(|g| g.set_size).map_or(false, |f| unsafe { f(self.plugin, w, h) })
    }

    /// SAFETY: `window` must describe a live native window of the API the
    /// GUI was created with.
    pub(super) unsafe fn gui_set_parent(&self, window: &clap_window) -> bool {
        self.gui().and_then(|g| g.set_parent).map_or(false, |f| f(self.plugin, window))
    }

    /// SAFETY: as for `gui_set_parent`.
    pub(super) unsafe fn gui_set_transient(&self, window: &clap_window) -> bool {
        self.gui().and_then(|g| g.set_transient).map_or(false, |f| f(self.plugin, window))
    }

    pub(super) fn gui_suggest_title(&self, title: &str) {
        let (Some(f), Ok(t)) = (self.gui().and_then(|g| g.suggest_title), CString::new(title)) else { return };
        unsafe { f(self.plugin, t.as_ptr()) }
    }

    pub(super) fn gui_show(&self) -> bool {
        self.gui().and_then(|g| g.show).map_or(false, |f| unsafe { f(self.plugin) })
    }

    pub(super) fn gui_hide(&self) -> bool {
        self.gui().and_then(|g| g.hide).map_or(false, |f| unsafe { f(self.plugin) })
    }

    pub fn is_overloaded(&self) -> bool {
        self.overloaded.load(Ordering::Relaxed)
    }

    /// Lets a plugin the watchdog bypassed play again.
    pub fn reenable(&self) {
        self.overload_reported.store(false, Ordering::Relaxed);
        self.overloaded.store(false, Ordering::Relaxed);
    }

    pub fn in_engine(&self) -> bool {
        self.in_engine.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn raw(&self) -> *const clap_plugin {
        self.plugin
    }
}

impl Drop for Instance {
    /// [main-thread]: the last `Arc` goes when the session (or the
    /// garbage the engine sends back) lets go, both on the main thread.
    /// The engine stopped processing first, on the audio thread.
    fn drop(&mut self) {
        // SAFETY: the plugin is alive, active, and not processing.
        unsafe {
            let p = &*self.plugin;
            if let Some(d) = p.deactivate {
                d(self.plugin);
            }
            if let Some(d) = p.destroy {
                d(self.plugin);
            }
        }
    }
}

unsafe extern "C" fn stream_write(stream: *const clap_ostream, buffer: *const c_void, size: u64) -> i64 {
    let Some(bytes) = ((*stream).ctx as *mut Vec<u8>).as_mut() else { return -1 };
    if buffer.is_null() {
        return -1;
    }
    bytes.extend_from_slice(std::slice::from_raw_parts(buffer as *const u8, size as usize));
    size as i64
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

unsafe extern "C" fn stream_read(stream: *const clap_istream, buffer: *mut c_void, size: u64) -> i64 {
    let Some(r) = ((*stream).ctx as *mut Reader).as_mut() else { return -1 };
    if buffer.is_null() {
        return -1;
    }
    let n = (size as usize).min(r.bytes.len() - r.pos);
    std::ptr::copy_nonoverlapping(r.bytes[r.pos..].as_ptr(), buffer as *mut u8, n);
    r.pos += n;
    n as i64 // 0 = end of stream
}

// ---------------------------------------------------------------------
// The audio side
// ---------------------------------------------------------------------

/// A CLAP event as the plugin reads it: `get()` hands out a pointer to the
/// header, which is the first field of every event struct (`#[repr(C)]`).
#[derive(Clone, Copy)]
enum Event {
    Param(clap_event_param_value),
    Note(clap_event_note),
}

const MAX_EVENTS: usize = 1024;

/// The watchdog: a plugin using more than this share of the block's time,
/// for this many blocks in a row, is bypassed.
pub const WATCHDOG_LIMIT: f32 = 0.8;
pub const WATCHDOG_BLOCKS: u32 = 8;

/// Beats or seconds as CLAP's 32.32 fixed-point numbers: exact for musical
/// subdivisions, and free of the drift that repeatedly added floats have.
fn fixed(x: f64, factor: i64) -> i64 {
    (x * factor as f64).round() as i64
}

/// Where we are, for tempo-synced plugins (delays, arpeggiators, LFOs).
fn transport_event(ctx: &ProcessContext) -> clap_event_transport {
    use crate::transport::BEATS_PER_BAR;
    let spb = ctx.samples_per_beat();
    let beats = ctx.position as f64 / spb;
    let secs_per_beat = 60.0 / ctx.tempo;
    let mut flags = CLAP_TRANSPORT_HAS_TEMPO | CLAP_TRANSPORT_HAS_BEATS_TIMELINE | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE | CLAP_TRANSPORT_HAS_TIME_SIGNATURE;
    if ctx.playing {
        flags |= CLAP_TRANSPORT_IS_PLAYING;
    }
    let (loop_start, loop_end) = ctx.loop_beats.unwrap_or((0.0, 0.0));
    if ctx.loop_beats.is_some() {
        flags |= CLAP_TRANSPORT_IS_LOOP_ACTIVE;
    }
    let bar = (beats / BEATS_PER_BAR).floor();
    clap_event_transport {
        header: header(std::mem::size_of::<clap_event_transport>(), 0, CLAP_EVENT_TRANSPORT),
        flags,
        song_pos_beats: fixed(beats, CLAP_BEATTIME_FACTOR),
        song_pos_seconds: fixed(ctx.position as f64 / ctx.sample_rate, CLAP_SECTIME_FACTOR),
        tempo: ctx.tempo,
        tempo_inc: 0.0,
        loop_start_beats: fixed(loop_start, CLAP_BEATTIME_FACTOR),
        loop_end_beats: fixed(loop_end, CLAP_BEATTIME_FACTOR),
        loop_start_seconds: fixed(loop_start * secs_per_beat, CLAP_SECTIME_FACTOR),
        loop_end_seconds: fixed(loop_end * secs_per_beat, CLAP_SECTIME_FACTOR),
        bar_start: fixed(bar * BEATS_PER_BAR, CLAP_BEATTIME_FACTOR),
        bar_number: bar as i32,
        tsig_num: BEATS_PER_BAR as u16,
        tsig_denom: 4,
    }
}

/// A plugin in the engine, as an effect (`Processor`) or as an instrument.
pub struct ClapNode {
    inst: Arc<Instance>,
    started: bool,
    /// Parameter changes since the last block, sent as events at its start.
    pending: Vec<(clap_id, f64)>,
    events: Vec<Event>,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    steady_time: i64,
    sample_rate: f64,
    /// Blocks in a row over the watchdog's limit.
    overruns: u32,
}

// SAFETY: the events hold raw pointers (null "cookies") that are never
// dereferenced by us; the node is only ever used by one thread at a time.
unsafe impl Send for ClapNode {}

impl ClapNode {
    /// [main-thread]: all allocation happens here.
    pub fn new(inst: Arc<Instance>, info: &PrepareInfo) -> Result<Self, String> {
        if inst.in_engine.swap(true, Ordering::Relaxed) {
            return Err(format!("this {} is already in use", inst.name));
        }
        let n = info.max_frames;
        Ok(Self {
            inst,
            started: false,
            pending: Vec::with_capacity(256),
            events: Vec::with_capacity(MAX_EVENTS),
            in_l: vec![0.0; n],
            in_r: vec![0.0; n],
            out_l: vec![0.0; n],
            out_r: vec![0.0; n],
            steady_time: 0,
            sample_rate: info.sample_rate as f64,
            overruns: 0,
        })
    }

    fn queue_param(&mut self, index: usize, value: f32) {
        if let Some(&id) = self.inst.param_ids.get(index) {
            if self.pending.len() < self.pending.capacity() {
                self.pending.push((id, value as f64));
            }
        }
    }

    /// [audio-thread] Runs the plugin over the buffer's window. As an
    /// effect, its output replaces the buffer; as an instrument, it's
    /// added (like every other source on a track).
    fn run(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext, midi: &[MidiEvent], instrument: bool) {
        let n = buffer.frames().min(self.out_l.len());
        // Bypassed: an effect passes the audio through untouched; an
        // instrument adds nothing (silence: there's nothing to pass on).
        if n == 0 || self.inst.failed.load(Ordering::Relaxed) || self.inst.is_overloaded() {
            self.overruns = 0;
            return;
        }
        let plugin = self.inst.plugin;
        if !self.started {
            // SAFETY: activated on the main thread; this is the audio thread.
            self.started = unsafe { (*plugin).start_processing.map_or(true, |s| s(plugin)) };
            if !self.started {
                self.inst.failed.store(true, Ordering::Relaxed);
                return;
            }
        }

        // Events: parameter changes at the start, then notes at their frames.
        self.events.clear();
        for (param_id, value) in self.pending.drain(..) {
            self.events.push(Event::Param(clap_event_param_value {
                header: header(std::mem::size_of::<clap_event_param_value>(), 0, CLAP_EVENT_PARAM_VALUE),
                param_id,
                cookie: std::ptr::null_mut(),
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value,
            }));
        }
        for ev in midi.iter().take(MAX_EVENTS - self.events.len()) {
            let (kind, key, velocity) = match ev.msg {
                MidiMessage::NoteOn { note, velocity } => (CLAP_EVENT_NOTE_ON, note, velocity as f64 / 127.0),
                MidiMessage::NoteOff { note } | MidiMessage::ForceOff { note } => (CLAP_EVENT_NOTE_OFF, note, 0.0),
                MidiMessage::ControlChange { .. } => continue, // not passed on yet
            };
            let time = ev.frame.min(n - 1) as u32;
            self.events.push(Event::Note(clap_event_note {
                header: header(std::mem::size_of::<clap_event_note>(), time, kind),
                note_id: -1,
                port_index: 0,
                channel: 0,
                key: key as i16,
                velocity,
            }));
        }

        // Audio. The plugin reads from and writes to our scratch buffers,
        // through arrays of channel pointers, as C expects.
        if self.inst.inputs > 0 {
            if instrument {
                self.in_l[..n].fill(0.0);
                self.in_r[..n].fill(0.0);
            } else {
                let (l, r) = buffer.stereo();
                self.in_l[..n].copy_from_slice(&l[..n]);
                self.in_r[..n].copy_from_slice(&r[..n]);
            }
        }
        let mut in_ptrs = [self.in_l.as_mut_ptr(), self.in_r.as_mut_ptr()];
        let mut out_ptrs = [self.out_l.as_mut_ptr(), self.out_r.as_mut_ptr()];
        let input = clap_audio_buffer { data32: in_ptrs.as_mut_ptr(), data64: std::ptr::null_mut(), channel_count: self.inst.inputs, latency: 0, constant_mask: 0 };
        let mut output = clap_audio_buffer { data32: out_ptrs.as_mut_ptr(), data64: std::ptr::null_mut(), channel_count: self.inst.outputs, latency: 0, constant_mask: 0 };
        let in_events = clap_input_events { ctx: (&self.events as *const Vec<Event>) as *mut c_void, size: Some(events_size), get: Some(events_get) };
        let out_events = clap_output_events { ctx: std::ptr::null_mut(), try_push: Some(events_push) };
        let transport = transport_event(ctx);
        let process = clap_process {
            steady_time: self.steady_time,
            frames_count: n as u32,
            transport: &transport,
            audio_inputs: if self.inst.inputs > 0 { &input } else { std::ptr::null() },
            audio_outputs: &mut output,
            audio_inputs_count: (self.inst.inputs > 0) as u32,
            audio_outputs_count: 1,
            in_events: &in_events,
            out_events: &out_events,
        };

        // The one thing we can measure about someone else's code: time.
        let start = Instant::now();
        // SAFETY: every pointer in `process` refers to locals or fields
        // that live until after this call, and n <= the activated maximum.
        let status = unsafe { (*plugin).process.map_or(CLAP_PROCESS_ERROR, |p| p(plugin, &process)) };
        let load = (start.elapsed().as_secs_f64() * self.sample_rate / n as f64) as f32;
        self.inst.cpu.fetch_max(load.to_bits(), Ordering::Relaxed);
        self.steady_time += n as i64;

        // The watchdog DECIDES here, on the audio thread: only this thread
        // knows how long each call took, and only it can act in time, on
        // the very next block. A single slow block is forgiven (a plugin
        // may allocate once, say); a run of them means dropouts.
        if load > WATCHDOG_LIMIT {
            self.overruns += 1;
            if self.overruns >= WATCHDOG_BLOCKS {
                self.inst.overloaded.store(true, Ordering::Relaxed);
            }
        } else {
            self.overruns = 0;
        }

        if status == CLAP_PROCESS_ERROR {
            self.inst.failed.store(true, Ordering::Relaxed);
            return; // the buffer is untouched: bypassed
        }
        let out_r: &[f32] = if self.inst.outputs >= 2 { &self.out_r[..n] } else { &self.out_l[..n] };
        let (l, r) = buffer.stereo_mut();
        if instrument {
            l[..n].iter_mut().zip(&self.out_l[..n]).for_each(|(d, s)| *d += s);
            r[..n].iter_mut().zip(out_r).for_each(|(d, s)| *d += s);
        } else {
            l[..n].copy_from_slice(&self.out_l[..n]);
            r[..n].copy_from_slice(out_r);
        }
    }

    /// [audio-thread] Must happen before the plugin leaves the engine.
    fn stop_processing(&mut self) {
        if self.started {
            // SAFETY: this is the audio thread, and the plugin is processing.
            unsafe {
                if let Some(s) = (*self.inst.plugin).stop_processing {
                    s(self.inst.plugin);
                }
            }
            self.started = false;
        }
    }
}

impl Drop for ClapNode {
    fn drop(&mut self) {
        // The engine stops plugins on the audio thread before retiring
        // them (see `Processor::stop`). Only if the whole engine is torn
        // down at exit could one still be running here; stop it anyway.
        self.stop_processing();
        self.inst.in_engine.store(false, Ordering::Relaxed);
    }
}

fn header(size: usize, time: u32, type_: u16) -> clap_event_header {
    clap_event_header { size: size as u32, time, space_id: CLAP_CORE_EVENT_SPACE_ID, type_, flags: 0 }
}

unsafe extern "C" fn events_size(list: *const clap_input_events) -> u32 {
    ((*list).ctx as *const Vec<Event>).as_ref().map_or(0, |e| e.len() as u32)
}

unsafe extern "C" fn events_get(list: *const clap_input_events, index: u32) -> *const clap_event_header {
    match ((*list).ctx as *const Vec<Event>).as_ref().and_then(|e| e.get(index as usize)) {
        Some(Event::Param(p)) => &p.header,
        Some(Event::Note(n)) => &n.header,
        None => std::ptr::null(),
    }
}

unsafe extern "C" fn events_push(_list: *const clap_output_events, _event: *const clap_event_header) -> bool {
    true // accepted and ignored: we don't follow plugin-side changes yet
}

impl Processor for ClapNode {
    fn name(&self) -> &'static str {
        self.inst.name
    }
    fn param_info(&self) -> &'static [ParamInfo] {
        self.inst.params
    }
    fn prepare(&mut self, _info: &PrepareInfo) {
        // Done in `Instance::create` (activation) and `ClapNode::new`.
    }
    fn set_param(&mut self, id: usize, value: f32) {
        self.queue_param(id, value);
    }
    fn process(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext) {
        self.run(buffer, ctx, &[], false);
    }
    fn stop(&mut self) {
        self.stop_processing();
    }
    fn latency(&self) -> u32 {
        self.inst.latency
    }
}

impl Instrument for ClapNode {
    fn name(&self) -> &'static str {
        self.inst.name
    }
    fn param_info(&self) -> &'static [ParamInfo] {
        self.inst.params
    }
    fn prepare(&mut self, _info: &PrepareInfo) {}
    fn set_param(&mut self, id: usize, value: f32) {
        self.queue_param(id, value);
    }
    fn render(&mut self, buffer: &mut AudioBuffer, ctx: &ProcessContext, events: &[MidiEvent]) {
        self.run(buffer, ctx, events, true);
    }
    fn stop(&mut self) {
        self.stop_processing();
    }
    fn latency(&self) -> u32 {
        self.inst.latency
    }
}
