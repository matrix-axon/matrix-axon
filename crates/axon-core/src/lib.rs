//! Shared types, errors, and configuration for Axon.
//!
//! `axon-core` is the lowest crate in the workspace: it depends on no other
//! `axon-*` crate, so every other crate may depend on it. It owns the typed
//! [`Config`] loader and the top-level [`Error`] enum that downstream crate
//! errors convert into.

pub mod account_actions;
pub mod config;
pub mod error;
pub mod live;
pub mod media;
pub mod message;
pub mod power_levels;
pub mod room_entry;
pub mod secret;

pub use account_actions::{MatrixProfile, PublicRoomSummary, PublicRoomsPage, PublicRoomsQuery};
pub use config::{
    AppleOauthConfig, Config, GenericOauthProviderConfig, MediaConfig, OauthClientConfig,
    OauthConfig, OauthProvidersConfig, SearchConfig, SyncConfig,
};
pub use error::{ConfigError, Error, Result};
pub use live::{
    DeviceStateFrame, EphemeralFrame, InviteAddedFrame, InviteRemovedFrame, LiveEvent, LiveFrame,
    SenderTrustFrame, SyncStateFrame, UnreadCountsFrame, VerificationFrame, VerificationFrameKind,
};
pub use media::{ThumbnailMethod, ThumbnailSpec};
pub use message::{Formatted, MediaAttachment, MediaSendKind, Relation};
pub use power_levels::{PowerLevelChanges, ResolvedPowerLevels};
pub use room_entry::{CreateRoomRequest, RoomPreset};
pub use secret::{generate_opaque_secret, hash_secret};
