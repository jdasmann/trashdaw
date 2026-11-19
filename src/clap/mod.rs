//! Hosting CLAP plugins: loading other people's effects and instruments.
//!
//! A plugin is a shared library with a C interface: structs of function
//! pointers. Everything that crosses that boundary is `unsafe`: the
//! compiler can't check C code, so this module carries the burden of using
//! it exactly as the CLAP specification says, and presents safe types to
//! the rest of the DAW.
//!
//! CLAP's central rule is about threads. Every function is marked
//! [main-thread], [audio-thread] or [thread-safe], and calling one on the
//! wrong thread is a host bug even when it seems to work. Here:
//!   - Loading, creating, activating, destroying, reading parameter info
//!     and saving state happen on the main thread (in the session), like
//!     every other "prepare it, then send it" step since step 2.
//!   - start_processing, process and stop_processing happen on the audio
//!     thread, in `ClapNode` (instance.rs).
//!
//! What a host can NOT do is protect itself from a plugin. Plugin code runs
//! inside our process, with full access to our memory: if it crashes, the
//! DAW crashes; if it takes too long, the audio drops out. Hosts that need
//! protection (Bitwig, for example) run plugins in separate processes.

mod instance;
#[cfg(test)]
pub mod test_plugin;

pub use instance::{ClapNode, Instance};

use clap_sys::entry::clap_plugin_entry;
use clap_sys::ext::log::{clap_host_log, clap_log_severity, CLAP_EXT_LOG, CLAP_LOG_ERROR, CLAP_LOG_WARNING};
use clap_sys::ext::params::{clap_host_params, clap_param_clear_flags, clap_param_rescan_flags, CLAP_EXT_PARAMS};
use clap_sys::ext::thread_check::{clap_host_thread_check, CLAP_EXT_THREAD_CHECK};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin_features::CLAP_PLUGIN_FEATURE_INSTRUMENT;
use clap_sys::version::{clap_version_is_compatible, CLAP_VERSION};
use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::ThreadId;

// ---------------------------------------------------------------------
// Thread identity, for the plugin's "which thread am I on?" questions.
// ---------------------------------------------------------------------

thread_local! {
    static AUDIO_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as an audio thread. The engine calls this at
/// the start of every callback: a thread-local write, real-time safe.
pub fn enter_audio_thread() {
    AUDIO_THREAD.with(|a| a.set(true));
}

pub fn on_audio_thread() -> bool {
    AUDIO_THREAD.with(|a| a.get())
}

// ---------------------------------------------------------------------
// The host: what the plugin sees of us.
// ---------------------------------------------------------------------

/// A `&'static CStr` from a string literal. (Rust 1.77+ has `c"..."`
/// literals for this; this works on older compilers too.)
macro_rules! cstr {
    ($s:literal) => {
        // SAFETY: the literal gets exactly one NUL, at the end.
        unsafe { CStr::from_bytes_with_nul_unchecked(concat!($s, "\0").as_bytes()) }
    };
}

const HOST_NAME: &CStr = cstr!("daw");
const HOST_VENDOR: &CStr = cstr!("step-by-step");
const HOST_URL: &CStr = cstr!("");
const HOST_VERSION: &CStr = cstr!("0.12");

/// Our side of one plugin instance. Boxed, so its address never changes:
/// the plugin keeps a pointer to `clap_host`, whose `host_data` points back
/// here. It must outlive the plugin.
pub struct HostShared {
    clap_host: clap_host,
    /// The thread that created the plugin: by our design, the main thread.
    main_thread: ThreadId,
    /// The plugin asked for `on_main_thread` to be called.
    pub callback_requested: AtomicBool,
    /// The plugin asked to be deactivated and reactivated.
    pub restart_requested: AtomicBool,
    /// Messages the plugin logged. `log` may be called from the audio
    /// thread, so it only ever *tries* the lock and drops the message if
    /// it's contended: a plugin's log line isn't worth a dropout.
    pub log: Mutex<Vec<String>>,
}

impl HostShared {
    pub fn new() -> Box<Self> {
        let mut host = Box::new(Self {
            clap_host: clap_host {
                clap_version: CLAP_VERSION,
                host_data: std::ptr::null_mut(),
                name: HOST_NAME.as_ptr(),
                vendor: HOST_VENDOR.as_ptr(),
                url: HOST_URL.as_ptr(),
                version: HOST_VERSION.as_ptr(),
                get_extension: Some(host_get_extension),
                request_restart: Some(host_request_restart),
                request_process: Some(host_request_process),
                request_callback: Some(host_request_callback),
            },
            main_thread: std::thread::current().id(),
            callback_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            log: Mutex::new(Vec::new()),
        });
        let me: *mut HostShared = &mut *host;
        host.clap_host.host_data = me.cast();
        host
    }

    pub fn as_clap(&self) -> *const clap_host {
        &self.clap_host
    }

    pub fn take_log(&self) -> Vec<String> {
        self.log.lock().map(|mut l| std::mem::take(&mut *l)).unwrap_or_default()
    }
}

/// Gets our `HostShared` back from the pointer the plugin hands us.
///
/// SAFETY: `host` must be a `clap_host` created by `HostShared::new`, still
/// alive. That's guaranteed while the plugin is alive, and plugins may only
/// call us while they are.
unsafe fn shared<'a>(host: *const clap_host) -> Option<&'a HostShared> {
    if host.is_null() {
        return None;
    }
    ((*host).host_data as *const HostShared).as_ref()
}

// Every callback below is `extern "C"`: called by plugin code. A Rust panic
// must never unwind out of one (that's undefined behavior across a C
// boundary), so none of them can panic: no unwraps, no indexing.

unsafe extern "C" fn host_get_extension(_host: *const clap_host, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    let id = CStr::from_ptr(id);
    if id == CLAP_EXT_THREAD_CHECK {
        &THREAD_CHECK as *const clap_host_thread_check as *const c_void
    } else if id == CLAP_EXT_LOG {
        &LOG as *const clap_host_log as *const c_void
    } else if id == CLAP_EXT_PARAMS {
        &PARAMS as *const clap_host_params as *const c_void
    } else {
        std::ptr::null() // anything else: "not supported", which plugins must handle
    }
}

unsafe extern "C" fn host_request_restart(host: *const clap_host) {
    if let Some(h) = shared(host) {
        h.restart_requested.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn host_request_process(_host: *const clap_host) {
    // We always process every active plugin, so there's nothing to do.
}

unsafe extern "C" fn host_request_callback(host: *const clap_host) {
    if let Some(h) = shared(host) {
        h.callback_requested.store(true, Ordering::Relaxed);
    }
}

static THREAD_CHECK: clap_host_thread_check = clap_host_thread_check { is_main_thread: Some(is_main_thread), is_audio_thread: Some(is_audio_thread) };

unsafe extern "C" fn is_main_thread(host: *const clap_host) -> bool {
    shared(host).map_or(false, |h| std::thread::current().id() == h.main_thread)
}

unsafe extern "C" fn is_audio_thread(_host: *const clap_host) -> bool {
    on_audio_thread()
}

static LOG: clap_host_log = clap_host_log { log: Some(host_log) };

unsafe extern "C" fn host_log(host: *const clap_host, severity: clap_log_severity, msg: *const c_char) {
    let (Some(h), false) = (shared(host), msg.is_null()) else { return };
    let level = match severity {
        CLAP_LOG_ERROR => "error",
        CLAP_LOG_WARNING => "warning",
        _ => "info",
    };
    let text = format!("plugin {level}: {}", CStr::from_ptr(msg).to_string_lossy());
    if let Ok(mut log) = h.log.try_lock() {
        log.push(text);
    }
}

static PARAMS: clap_host_params = clap_host_params { rescan: Some(params_rescan), clear: Some(params_clear), request_flush: Some(params_request_flush) };

// Parameter lists are read once, at creation; a plugin that changes them
// later would need reloading. (Real hosts handle rescans; it's an exercise.)
unsafe extern "C" fn params_rescan(_host: *const clap_host, _flags: clap_param_rescan_flags) {}
unsafe extern "C" fn params_clear(_host: *const clap_host, _id: clap_id, _flags: clap_param_clear_flags) {}
unsafe extern "C" fn params_request_flush(_host: *const clap_host) {}

// ---------------------------------------------------------------------
// Bundles: a loaded plugin library.
// ---------------------------------------------------------------------

/// What a library says about one of its plugins.
#[derive(Clone, Debug, PartialEq)]
pub struct Descriptor {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub features: Vec<String>,
}

impl Descriptor {
    pub fn is_instrument(&self) -> bool {
        self.features.iter().any(|f| f.as_bytes() == CLAP_PLUGIN_FEATURE_INSTRUMENT.to_bytes())
    }
}

/// A loaded plugin library. Every instance created from it holds an `Arc`
/// to it, because unloading the library while an instance exists would
/// unmap the instance's code out from under it.
pub struct Bundle {
    pub path: PathBuf,
    entry: *const clap_plugin_entry,
    factory: *const clap_plugin_factory,
    pub descriptors: Vec<Descriptor>,
    // Declared last, so it's dropped (unloaded) last: after `Drop::drop`
    // below has called the library's `deinit`.
    _library: Option<libloading::Library>,
}

// SAFETY: the entry and factory are immutable tables of functions that
// CLAP specifies as callable from any thread.
unsafe impl Send for Bundle {}
unsafe impl Sync for Bundle {}

fn to_string(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: CLAP strings are null-terminated, and live as long as
        // the descriptor they belong to.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

impl Bundle {
    /// Loads a `.clap` file. On macOS a `.clap` is a bundle folder with the
    /// library inside; elsewhere it's the library itself.
    pub fn load(path: &Path) -> Result<Arc<Bundle>, String> {
        let binary = if path.is_dir() {
            let macos = path.join("Contents").join("MacOS");
            std::fs::read_dir(&macos)
                .ok()
                .and_then(|mut d| d.find_map(|e| e.ok().map(|e| e.path())))
                .ok_or_else(|| format!("{} has no library inside Contents/MacOS", path.display()))?
        } else {
            path.to_path_buf()
        };
        // SAFETY: loading a library runs its initializers: we're trusting
        // the file to be a real plugin. There's no way to load native code
        // "safely"; this is the first of many trusts.
        let library = unsafe { libloading::Library::new(&binary) }.map_err(|e| format!("couldn't load {}: {e}", path.display()))?;
        // SAFETY: CLAP requires a symbol named `clap_entry` that IS a
        // `clap_plugin_entry` struct, so the symbol's address is a pointer
        // to one.
        let entry: *const clap_plugin_entry = unsafe {
            let symbol = library.get::<*const clap_plugin_entry>(b"clap_entry\0").map_err(|_| format!("{} isn't a CLAP plugin (no clap_entry)", path.display()))?;
            *symbol
        };
        // SAFETY: the library stays loaded (it moves into the Bundle).
        unsafe { Self::from_entry(entry, path, Some(library)) }
    }

    /// Initializes a plugin entry: from a library, or (in tests) compiled in.
    ///
    /// SAFETY: `entry` must point to a valid `clap_plugin_entry` that stays
    /// valid as long as `library` is loaded (or forever, if it's None).
    pub unsafe fn from_entry(entry: *const clap_plugin_entry, path: &Path, library: Option<libloading::Library>) -> Result<Arc<Bundle>, String> {
        let e = entry.as_ref().ok_or("null clap_entry")?;
        if !clap_version_is_compatible(e.clap_version) {
            return Err(format!("{} uses an incompatible CLAP version", path.display()));
        }
        let c_path = CString::new(path.to_string_lossy().into_owned()).map_err(|_| "bad path")?;
        let init = e.init.ok_or("plugin entry has no init")?;
        if !init(c_path.as_ptr()) {
            return Err(format!("{} failed to initialize", path.display()));
        }
        let factory = e.get_factory.map_or(std::ptr::null(), |f| f(CLAP_PLUGIN_FACTORY_ID.as_ptr())) as *const clap_plugin_factory;
        let mut bundle = Bundle { path: path.to_path_buf(), entry, factory, descriptors: Vec::new(), _library: library };
        let f = factory.as_ref().ok_or("the library has no plugin factory")?;
        let count = f.get_plugin_count.map_or(0, |c| c(factory));
        for i in 0..count {
            let Some(d) = f.get_plugin_descriptor.and_then(|g| g(factory, i).as_ref()) else { continue };
            let mut features = Vec::new();
            let mut p = d.features;
            while !p.is_null() && !(*p).is_null() {
                features.push(to_string(*p));
                p = p.add(1);
            }
            bundle.descriptors.push(Descriptor { id: to_string(d.id), name: to_string(d.name), vendor: to_string(d.vendor), version: to_string(d.version), features });
        }
        Ok(Arc::new(bundle))
    }

    pub(crate) fn factory(&self) -> *const clap_plugin_factory {
        self.factory
    }
}

impl Drop for Bundle {
    fn drop(&mut self) {
        // SAFETY: every instance holds an Arc<Bundle>, so none are left.
        unsafe {
            if let Some(deinit) = self.entry.as_ref().and_then(|e| e.deinit) {
                deinit();
            }
        }
    }
}

// ---------------------------------------------------------------------
// Finding plugins on disk
// ---------------------------------------------------------------------

/// The standard CLAP folders for this OS, plus anything in $CLAP_PATH.
pub fn search_paths() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("CLAP_PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        dirs.extend(home.map(|h| h.join("Library/Audio/Plug-Ins/CLAP")));
        dirs.push("/Library/Audio/Plug-Ins/CLAP".into());
    } else if cfg!(windows) {
        dirs.extend(std::env::var_os("COMMONPROGRAMFILES").map(|p| PathBuf::from(p).join("CLAP")));
        dirs.extend(std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Programs/Common/CLAP")));
    } else {
        dirs.extend(home.map(|h| h.join(".clap")));
        dirs.push("/usr/lib/clap".into());
    }
    dirs
}

/// Every `.clap` file or bundle under `dirs`, recursively.
pub fn find_plugin_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map_or(false, |x| x.eq_ignore_ascii_case("clap")) {
                out.push(p); // a file, or a macOS bundle folder: don't descend
            } else if p.is_dir() && depth < 6 {
                walk(&p, out, depth + 1);
            }
        }
    }
    let mut out = Vec::new();
    for d in dirs {
        walk(d, &mut out, 0);
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------

/// Turns a runtime string into a `&'static str`, keeping one copy of each.
///
/// `ParamInfo` and `Processor::name` use `&'static str`, which fits the
/// built-in effects, whose names are compiled in. A plugin's names only
/// exist at runtime, so they're leaked on purpose: interned, so loading
/// the same plugin a hundred times leaks its names only once.
pub fn intern(s: &str) -> &'static str {
    static STRINGS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut set = STRINGS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&existing) = set.get(s) {
        return existing;
    }
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}

/// Plugin state is opaque bytes; JSON needs text. Standard base64.
pub fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() { T[(n >> shift & 63) as usize] as char } else { '=' });
        }
    }
    out
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let bytes = text.trim().as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = n << 6 | value(c)? as u32;
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8][..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
pub mod tests;
