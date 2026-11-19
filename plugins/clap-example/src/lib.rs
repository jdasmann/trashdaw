//! The DAW's test plugins, built as a real CLAP library.
//!
//! Build:   cargo build --release
//! Install: copy the built library into a CLAP folder, renamed to
//!          "Example.clap":
//!   Linux:   target/release/libclap_example.so  -> ~/.clap/Example.clap
//!   macOS:   target/release/libclap_example.dylib -> ~/Library/Audio/Plug-Ins/CLAP/Example.clap
//!            (a plain library works for testing; a proper macOS plugin is a
//!            bundle folder with the library in Contents/MacOS)
//!   Windows: target\release\clap_example.dll -> %COMMONPROGRAMFILES%\CLAP\Example.clap

#[allow(dead_code)]
mod plugin {
    // The very same source the host's tests compile in.
    include!("../../../src/clap/test_plugin.rs");
}

/// CLAP finds a plugin library's entry point by this exact name.
#[no_mangle]
#[allow(non_upper_case_globals)]
pub static clap_entry: clap_sys::entry::clap_plugin_entry = plugin::ENTRY_DEF;
