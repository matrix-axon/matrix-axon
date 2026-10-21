//! Remote push for suspended mobile clients (ADR 0113).
//!
//! The sync loop only enqueues an observation. One router task decides whether
//! that observation is a wake, and spawned tasks talk to APNs and FCM.

mod decision;
mod deliver;
mod message;
mod observe;
mod payload;
mod router;

pub(crate) use observe::{count_observation, watch_room_updates, RoomWatch};
pub(crate) use router::{spawn, PushIngress};
