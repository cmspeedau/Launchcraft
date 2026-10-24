//! Launchcraft Desktop Application Core & IPC Handlers.
//!
//! Provides the bridge between the desktop user interface (Tauri v2)
//! and the Launchcraft engine crates (`launchcraft-core`, `launchcraft-sync`,
//! `launchcraft-runtime`, and `launchcraft-auth`).

pub mod commands;
pub mod state;

pub use state::AppState;
