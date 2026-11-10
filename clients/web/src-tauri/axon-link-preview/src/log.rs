//! The Swift reader's `device-preview` log events, under the same names.
//!
//! Events carry kinds, counts, families and statuses, never a host, URL or
//! address, so a log can be shared without revealing what the user typed.
//! Written to stderr in a debug build, or in any build when
//! `AXON_LINK_PREVIEW_LOG=1` is set, which is how to trace a lookup in a
//! release build on the desktop.

use std::sync::OnceLock;

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        cfg!(debug_assertions)
            || std::env::var_os("AXON_LINK_PREVIEW_LOG").is_some_and(|v| v == "1")
    })
}

pub(crate) fn event(message: &str) {
    if enabled() && !cfg!(test) {
        eprintln!("axon-link-preview: {message}");
    }
}
