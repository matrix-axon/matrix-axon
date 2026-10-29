// A workspace `cargo build` turns on reqwest's `cookies` feature through the
// Matrix OAuth smoke crate. The unread watcher's spawned future then nests
// that HTTP stack deeply enough that proving `Send` overflows rustc's default
// recursion limit.
#![recursion_limit = "256"]
//! matrix-rust-sdk sync engine wrapper (Simplified Sliding Sync only).
//!
//! `axon-sync` owns the live connection to each account's homeserver. It builds
//! one matrix-rust-sdk [`Client`](matrix_sdk::Client) per account, authenticates
//! (login on first boot, session restore thereafter), and runs a
//! [`SyncService`](matrix_sdk_ui::sync_service::SyncService) — Simplified
//! Sliding Sync (MSC4186), no legacy `/sync`. Every synced timeline event is
//! persisted into the Postgres archive; events the SDK can't decrypt (UTDs) are
//! stored as ciphertext and back-filled by the re-decryption queue once their
//! megolm keys arrive (see [`redecrypt`]).
//!
//! The public surface is [`SyncEngine`]: start it with a `Store`, a
//! `SyncConfig`, and a cancellation token; it supervises a task per account and
//! restarts failed syncs with exponential backoff.

mod backfill;
mod backup;
mod client;
mod devices;
mod discovery;
mod display_names;
mod engine;
mod error;
mod gateway;
mod homeserver_gate;
mod lifecycle;
mod manager;
mod matrix_oauth;
mod matrix_oauth_acquire;
#[cfg(test)]
mod matrix_oauth_browser_tests;
mod matrix_oauth_device_grant;
mod matrix_oauth_flow;
mod matrix_oauth_grant;
mod media;
mod preview_generation;
pub use preview_generation::{AcquiredPreview, HomeserverPreviewGenerator, PreviewFailure};
mod member_counts;
mod member_profiles;
mod meta;
mod push;
mod reconcile;
mod redecrypt;
mod room_details;
mod room_sweep;
mod state_redaction;
mod supplementary_state;
mod sync_health;
mod trust;
mod verification;

pub use backfill::BackfillHealth;
pub use backup::{BackupAction, BackupHealth, BackupSnapshot, BackupStateView, RecoveryStateView};
pub use devices::{DeviceInfo, DeviceList, DeviceListEngine, DeviceListError};
pub use engine::SyncEngine;
pub use error::{GatewayError, SyncError};
pub use gateway::{LeaveOutcome as GatewayLeaveOutcome, SdkGateway};
pub use homeserver_gate::{BreakerSnapshot, GateSnapshot, HomeserverGates, InteractiveCall};
pub use lifecycle::{AccountLifecycle, LifecycleError};
pub use matrix_oauth::MatrixOAuthRegistrationManager;
pub use matrix_oauth_acquire::{
    MatrixOAuthAcquireEngine, MatrixOAuthAcquireError, MatrixOAuthAcquirePresentation,
    MatrixOAuthAcquireStage, MatrixOAuthAcquireState, MatrixOAuthBrowserStage,
    MatrixOAuthBrowserState,
};
pub use matrix_oauth_grant::{
    MatrixOAuthGrantEngine, MatrixOAuthGrantError, MatrixOAuthGrantPresentation,
    MatrixOAuthGrantStage, MatrixOAuthGrantState,
};
pub use media::SdkMediaProxy;
pub use member_profiles::{MemberProfile, MemberProfileEngine, MemberProfileError};
pub use redecrypt::RedecryptSummary;
pub use sync_health::{AccountSyncStatus, SyncHealth, SyncState};
pub use trust::{CurrentTrust, SenderTrustEngine, TrustBundle, TrustError, TrustSnapshot};
pub use verification::{FlowStage, FlowState, VerificationEngine, VerifyError};
