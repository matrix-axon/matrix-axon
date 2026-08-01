//! Shared machinery for the two binaries in this package.
//!
//! Both drive the real `axon-tui` under a pseudo-terminal against a real Axon:
//! `axon-smoke-tui` asserts on the rendered screen for CI, and `axon-demo-tui`
//! (ADR 0086) pumps that same PTY to the developer's terminal so a screen
//! recorder sees genuine Sixel/Kitty graphics. They differ in what they do with
//! the output, not in how they get it, so the driver, the child environment,
//! and the local-stack manifest live here rather than in either binary.
//!
//! The black-box rule still holds for everything below: no `axon-*` product
//! crate is ever imported, only spawned (`scripts/check-smoke-isolation.sh`).

pub mod env;
pub mod local_stack;
pub mod pty;
