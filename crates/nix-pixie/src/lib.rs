//! nix-pixie: provision NixOS machines from an empty disk to a running system.
//!
//! One binary with three roles (see `docs/design.md`). This library holds
//! all of it so the tests can reach the internals; `main.rs` only installs
//! the tracing subscriber and runs [`cli::run`]. It is not a published API.
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    reason = "internal library of the binary, not a published API"
)]

pub mod agent;
pub mod bootentry;
pub mod bulk;
pub mod child;
pub mod cli;
pub mod clock;
pub mod console;
pub mod control;
pub mod controller;
pub mod discovery;
pub mod disko;
pub mod dns;
pub mod encrypt;
pub mod events;
pub mod exec;
pub mod exit;
pub mod facter;
pub mod hooks;
pub mod hostkey;
pub mod identity;
pub mod install;
pub mod inventory;
pub mod link;
pub mod mdns;
pub mod metadata;
pub mod net;
pub mod nix;
pub mod nixlog;
pub mod nixos;
pub mod plan;
pub mod preflight;
pub mod probe;
pub mod push;
pub mod relay_status;
pub mod rendezvous;
pub mod screen;
pub mod secrets;
pub mod serial;
pub mod state;
pub mod store;
pub mod system;
pub mod target;
pub mod ui;

use tokio::signal::unix::{SignalKind, signal};

/// Resolves on Ctrl-C or SIGTERM (systemd stop).
pub async fn shutdown_signal() {
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        Err(e) => {
            tracing::warn!("can't handle SIGTERM, only Ctrl-C: {e}");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    exit::interrupted();
}
