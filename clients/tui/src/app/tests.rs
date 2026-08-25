//! Unit tests for [`App`].
//!
//! These cover behaviour that lives in `app.rs` itself, so they sit under
//! `app/tests/` rather than beside the `app/*.rs` submodule they exercise;
//! each module here is named for the behaviour it covers. Fixtures used by
//! more than one module live in [`support`].

mod commands;
mod completion;
mod input;
mod lifecycle;
mod live_events;
mod navigation;
mod popups;
mod reactions;
mod render;
mod room_completion;
mod rooms;
mod status;
mod support;
mod threads;
mod verification;
