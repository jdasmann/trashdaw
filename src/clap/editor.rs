//! A plugin's own GUI ("editor"), opened in a window.
//!
//! Two modes, both CLAP's:
//!   - Embedded: the host creates a window and gives the plugin its native
//!     handle to draw into (most plugins support this; many ONLY this).
//!   - Floating: the plugin opens its own window, and the host just asks it
//!     to stay in front of the main window. No platform code needed.
//!
//! Every GUI call is [main-thread]: the editor lives in the session, and is
//! serviced once per UI frame.

use super::window::{self, Event, HostWindow, ParentWindow};
use super::Instance;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

pub struct Editor {
    inst: Arc<Instance>,
    #[cfg_attr(not(test), allow(dead_code))]
    floating: bool,
    /// The plugin already destroyed its GUI (it closed its own window).
    destroyed: bool,
    /// The size last agreed between us and the plugin: both sides' resize
    /// requests are compared to it, so they can't bounce back and forth.
    size: (u32, u32),
    // Declared last: dropped after `Drop::drop` destroys the plugin's GUI,
    // which must never outlive the window it was drawing into.
    window: Option<Box<dyn HostWindow>>,
}

impl Editor {
    /// Opens the plugin's GUI. Embedded if this build and the plugin both
    /// support it (unless `prefer_floating`), floating otherwise.
    pub fn open(inst: Arc<Instance>, title: &str, parent: Option<ParentWindow>, prefer_floating: bool) -> Result<Self, String> {
        if !inst.has_gui() {
            return Err(format!("{} has no GUI of its own", inst.name));
        }
        let api = window::api();
        let can_embed = window::embedding_available() && inst.gui_is_api_supported(api, false);
        let can_float = inst.gui_is_api_supported(api, true);
        let floating = match (can_embed, can_float) {
            (true, true) => prefer_floating,
            (true, false) => false,
            (false, true) => true,
            (false, false) if !window::embedding_available() => {
                return Err(format!("{} can only show its GUI embedded in a host window, which this build can't do on this system yet", inst.name))
            }
            (false, false) => return Err(format!("{}'s GUI doesn't support this system's windows", inst.name)),
        };
        if !inst.gui_create(api, floating) {
            return Err(format!("{} couldn't create its GUI", inst.name));
        }
        // From here on, a failure must destroy what the plugin created.
        let mut editor = Editor { inst, floating, destroyed: false, size: (0, 0), window: None };
        if floating {
            if let Some(p) = parent {
                // SAFETY: the main window outlives every editor.
                unsafe { editor.inst.gui_set_transient(&p.0) };
            }
            editor.inst.gui_suggest_title(title);
        } else {
            // macOS works in points, where the OS handles scaling; elsewhere
            // the plugin is told the scale (1.0: this host doesn't scale).
            if !cfg!(target_os = "macos") {
                editor.inst.gui_set_scale(1.0);
            }
            let (w, h) = editor.inst.gui_size().filter(|&(w, h)| w > 0 && h > 0).unwrap_or((400, 300));
            let win = window::create(title, w, h, editor.inst.gui_can_resize())?;
            // SAFETY: the window lives inside the editor, until after the
            // plugin's GUI is destroyed (see the field order).
            if !unsafe { editor.inst.gui_set_parent(&win.clap_window()) } {
                return Err(format!("{} couldn't embed its GUI", editor.inst.name));
            }
            editor.size = (w, h);
            editor.window = Some(win);
        }
        if !editor.inst.gui_show() {
            return Err(format!("{} couldn't show its GUI", editor.inst.name));
        }
        if let Some(w) = editor.window.as_mut() {
            w.show();
        }
        Ok(editor)
    }

    pub fn instance(&self) -> &Arc<Instance> {
        &self.inst
    }

    #[cfg(test)]
    pub fn is_floating(&self) -> bool {
        self.floating
    }

    /// The host window's OS id (embedded mode), for tests.
    #[cfg(test)]
    pub fn window_id(&self) -> Option<u64> {
        self.window.as_ref().map(|w| w.native_id())
    }

    /// Once per UI frame. Returns false once the editor has been closed.
    pub fn service(&mut self) -> bool {
        let gui = &self.inst.host_shared().gui;
        // The plugin closed its own (floating) window.
        if gui.closed.swap(false, Relaxed) {
            self.destroyed = gui.closed_destroyed.load(Relaxed);
            return false;
        }
        // The plugin asked to be resized: our window follows.
        let requested = gui.resize.lock().ok().and_then(|mut r| r.take());
        if let (Some((w, h)), Some(win)) = (requested, self.window.as_mut()) {
            self.size = (w, h);
            win.set_size(w, h);
        }
        if gui.show.swap(false, Relaxed) {
            self.inst.gui_show();
        }
        if gui.hide.swap(false, Relaxed) {
            self.inst.gui_hide();
        }
        let Some(win) = self.window.as_mut() else { return true };
        for event in win.poll() {
            match event {
                Event::Closed => return false,
                // The user resized our window: ask the plugin what it can
                // actually be, tell it, and snap our window to that.
                Event::Resized(w, h) if (w, h) != self.size => {
                    let (aw, ah) = if self.inst.gui_can_resize() { self.inst.gui_adjust_size(w, h) } else { self.size };
                    if (aw, ah) != self.size {
                        self.inst.gui_set_size(aw, ah);
                    }
                    self.size = (aw, ah);
                    if (aw, ah) != (w, h) {
                        win.set_size(aw, ah);
                    }
                }
                Event::Resized(..) => {}
            }
        }
        true
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        if !self.destroyed {
            self.inst.gui_hide();
            self.inst.gui_destroy();
        }
        // `window` is dropped after this: the plugin has let go of it.
    }
}
