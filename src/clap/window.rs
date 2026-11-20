//! Native windows for plugin GUIs.
//!
//! An embedded plugin GUI draws into a window the HOST creates, handed to
//! it as a raw OS handle: an X11 window id on Linux, an `NSView*` on macOS,
//! an `HWND` on Windows. That's unavoidably platform-specific, so it lives
//! behind one small trait.
//!
//! Linux (X11) is implemented and tested (against a real X server, Xvfb).
//! macOS (Cocoa) and Windows (Win32) are still to come, behind the opt-in
//! `embedded-gui` Cargo feature: they can't be compiled where this was
//! written, and code that can't be compiled must not break the default
//! build.

use clap_sys::ext::gui::{clap_window, clap_window_handle, CLAP_WINDOW_API_COCOA, CLAP_WINDOW_API_WIN32, CLAP_WINDOW_API_X11};
use std::ffi::CStr;

pub enum Event {
    /// The user closed the window.
    Closed,
    /// The user resized it, to this inner size.
    Resized(u32, u32),
}

/// A top-level window a plugin's GUI is embedded in. [main-thread]
pub trait HostWindow {
    /// The handle the plugin draws into, as CLAP describes it.
    fn clap_window(&self) -> clap_window;
    fn show(&mut self);
    fn set_size(&mut self, width: u32, height: u32);
    /// What happened since the last call. Called once per UI frame.
    fn poll(&mut self) -> Vec<Event>;
    /// The OS's id for the window, for tests.
    fn native_id(&self) -> u64;
}

/// The window API plugins draw into on this OS.
pub fn api() -> &'static CStr {
    if cfg!(target_os = "macos") {
        CLAP_WINDOW_API_COCOA
    } else if cfg!(windows) {
        CLAP_WINDOW_API_WIN32
    } else {
        CLAP_WINDOW_API_X11
    }
}

/// Can this build create windows for embedded GUIs? Only where there's a
/// `HostWindow` implementation: X11 for now. (macOS and Windows will join
/// behind the `embedded-gui` feature.) Elsewhere, only plugins that can
/// open their own floating window show a GUI.
pub fn embedding_available() -> bool {
    cfg!(target_os = "linux")
}

/// A window for a plugin GUI of `width` x `height` (the plugin's size).
#[allow(unused_variables)]
pub fn create(title: &str, width: u32, height: u32, resizable: bool) -> Result<Box<dyn HostWindow>, String> {
    #[cfg(target_os = "linux")]
    return Ok(Box::new(x11::X11Window::new(title, width, height, resizable)?));
    #[allow(unreachable_code)]
    Err("embedded plugin GUIs aren't available in this build (see the `embedded-gui` feature)".into())
}

/// The DAW's main window, as CLAP describes windows: what a floating plugin
/// window is told to stay in front of.
#[derive(Clone, Copy)]
pub struct ParentWindow(pub clap_window);

impl ParentWindow {
    pub fn from_raw(raw: raw_window_handle::RawWindowHandle) -> Option<Self> {
        use raw_window_handle::RawWindowHandle as R;
        let (api, specific) = match raw {
            R::Xlib(h) => (CLAP_WINDOW_API_X11, clap_window_handle { x11: h.window as _ }),
            R::Xcb(h) => (CLAP_WINDOW_API_X11, clap_window_handle { x11: h.window.get() as _ }),
            R::AppKit(h) => (CLAP_WINDOW_API_COCOA, clap_window_handle { cocoa: h.ns_view.as_ptr() }),
            R::Win32(h) => (CLAP_WINDOW_API_WIN32, clap_window_handle { win32: h.hwnd.get() as *mut _ }),
            _ => return None, // Wayland has no "stay in front of" for other clients
        };
        Some(Self(clap_window { api: api.as_ptr(), specific }))
    }
}

#[cfg(target_os = "linux")]
pub mod x11 {
    use super::{Event, HostWindow};
    use clap_sys::ext::gui::{clap_window, clap_window_handle, CLAP_WINDOW_API_X11};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, WindowClass};
    use x11rb::protocol::Event as X;
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _;

    /// A plain X11 top-level window, on its own connection to the X server
    /// (separate from the UI toolkit's), with events read once per frame.
    pub struct X11Window {
        conn: RustConnection,
        window: u32,
        wm_delete: u32,
        size: (u32, u32),
    }

    fn err(e: impl std::fmt::Display) -> String {
        format!("X11: {e}")
    }

    impl X11Window {
        pub fn new(title: &str, width: u32, height: u32, resizable: bool) -> Result<Self, String> {
            let (conn, screen) = x11rb::connect(None).map_err(err)?;
            let root = conn.setup().roots[screen].root;
            let black = conn.setup().roots[screen].black_pixel;
            let window = conn.generate_id().map_err(err)?;
            let aux = CreateWindowAux::new().background_pixel(black).event_mask(EventMask::STRUCTURE_NOTIFY);
            let (w, h) = (width.clamp(1, 16_384) as u16, height.clamp(1, 16_384) as u16);
            conn.create_window(x11rb::COPY_DEPTH_FROM_PARENT, window, root, 0, 0, w, h, 0, WindowClass::INPUT_OUTPUT, 0, &aux).map_err(err)?;
            conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes()).map_err(err)?;
            // Ask the window manager to TELL us about the close button,
            // instead of killing the connection.
            let atom = |name: &[u8]| -> Result<u32, String> { Ok(conn.intern_atom(false, name).map_err(err)?.reply().map_err(err)?.atom) };
            let (protocols, wm_delete) = (atom(b"WM_PROTOCOLS")?, atom(b"WM_DELETE_WINDOW")?);
            conn.change_property32(PropMode::REPLACE, window, protocols, AtomEnum::ATOM, &[wm_delete]).map_err(err)?;
            if !resizable {
                // WM_NORMAL_HINTS with min = max size: flags PMinSize | PMaxSize.
                let mut hints = [0u32; 18];
                hints[0] = (1 << 4) | (1 << 5);
                hints[5] = width;
                hints[6] = height;
                hints[7] = width;
                hints[8] = height;
                conn.change_property32(PropMode::REPLACE, window, AtomEnum::WM_NORMAL_HINTS, AtomEnum::WM_SIZE_HINTS, &hints).map_err(err)?;
            }
            conn.flush().map_err(err)?;
            Ok(Self { conn, window, wm_delete, size: (width, height) })
        }
    }

    impl HostWindow for X11Window {
        fn clap_window(&self) -> clap_window {
            clap_window { api: CLAP_WINDOW_API_X11.as_ptr(), specific: clap_window_handle { x11: self.window as _ } }
        }

        fn show(&mut self) {
            let _ = self.conn.map_window(self.window);
            let _ = self.conn.flush();
        }

        fn set_size(&mut self, width: u32, height: u32) {
            self.size = (width, height);
            let _ = self.conn.configure_window(self.window, &ConfigureWindowAux::new().width(width).height(height));
            let _ = self.conn.flush();
        }

        fn poll(&mut self) -> Vec<Event> {
            let mut events = Vec::new();
            while let Ok(Some(ev)) = self.conn.poll_for_event() {
                match ev {
                    X::ClientMessage(m) if m.window == self.window && m.data.as_data32()[0] == self.wm_delete => events.push(Event::Closed),
                    X::ConfigureNotify(c) if c.window == self.window => {
                        let size = (c.width as u32, c.height as u32);
                        if size != self.size {
                            self.size = size;
                            events.push(Event::Resized(size.0, size.1));
                        }
                    }
                    _ => {}
                }
            }
            events
        }

        fn native_id(&self) -> u64 {
            self.window as u64
        }
    }

    impl Drop for X11Window {
        fn drop(&mut self) {
            let _ = self.conn.destroy_window(self.window);
            let _ = self.conn.flush();
        }
    }
}
