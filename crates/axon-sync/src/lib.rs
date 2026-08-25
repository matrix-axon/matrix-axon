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
mod client;
mod devices;
mod discovery;
mod engine;
mod error;
mod gateway;
mod lifecycle;
mod manager;
mod matrix_oauth;
mod media;
mod member_profiles;
mod meta;
mod reconcile;
mod redecrypt;
mod sync_health;
mod trust;
mod verification;

pub use backfill::BackfillHealth;
pub use devices::{DeviceInfo, DeviceList, DeviceListEngine, DeviceListError};
pub use engine::SyncEngine;
pub use error::{GatewayError, SyncError};
pub use gateway::{LeaveOutcome as GatewayLeaveOutcome, SdkGateway};
pub use lifecycle::{AccountLifecycle, LifecycleError};
pub use matrix_oauth::MatrixOAuthRegistrationManager;
pub use media::SdkMediaProxy;
pub use member_profiles::{MemberProfile, MemberProfileEngine, MemberProfileError};
pub use redecrypt::RedecryptSummary;
pub use sync_health::{AccountSyncStatus, SyncHealth, SyncState};
pub use trust::{CurrentTrust, SenderTrustEngine, TrustBundle, TrustError, TrustSnapshot};
pub use verification::{FlowStage, FlowState, VerificationEngine, VerifyError};
