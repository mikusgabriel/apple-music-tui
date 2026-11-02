//! Apple Music TUI library crate.
//!
//! This crate owns all durable state for the TUI process (Queue, Config,
//! Session, tokens, API access, MPRIS) in the two-process architecture.
//!
//! Module stubs are declared here; their contents are implemented in
//! subsequent tasks.

pub mod api;
pub mod auth;
pub mod config;
pub mod domain;
pub mod ipc;
pub mod keybindings;
pub mod mpris;
pub mod now_playing;
pub mod preview;
pub mod queue;
pub mod session;
pub mod token_store;
pub mod ui;
