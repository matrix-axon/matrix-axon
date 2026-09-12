//! HTTP route handlers for the `/v1/` read API.

use std::io::{self, Write};

/// Size cap for opaque JSON values on device-state entries and instance
/// preferences (ADR 0048 / ADR 0103). Matrix caps whole events at 64 KiB.
pub(crate) const MAX_OPAQUE_JSON_BYTES: usize = 64 * 1024;

/// Whether serializing `value` would exceed `cap` bytes. Counts as it writes
/// and stops at the cap, so an oversized payload is not fully re-serialized
/// into a `String` just to reject it.
pub(crate) fn json_exceeds_byte_cap(value: &serde_json::Value, cap: usize) -> bool {
    struct Counter {
        n: usize,
        cap: usize,
    }
    impl Write for Counter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.n = self.n.saturating_add(buf.len());
            if self.n > self.cap {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(&mut Counter { n: 0, cap }, value).is_err()
}

pub mod account_actions;
pub mod accounts;
pub mod bootstrap;
pub mod device_state;
pub mod devices;
pub mod ephemeral;
pub mod events;
pub mod invites;
pub mod matrix_oauth_acquire;
pub mod matrix_oauth_grant;
pub mod media;
pub mod membership;
pub mod messages;
pub mod oauth;
pub mod power_levels;
pub mod preferences;
pub mod room_entry;
pub mod room_settings;
pub mod rooms;
pub mod search;
pub mod status;
pub mod uploads;
pub mod verify;
