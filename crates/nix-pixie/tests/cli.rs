//! Tests for `nix_pixie::cli`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use clap::CommandFactory;
use nix_pixie::cli::Cli;

#[test]
fn cli_definition_is_valid() {
    Cli::command().debug_assert();
}
