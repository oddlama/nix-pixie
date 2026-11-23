//! Tests for `nix_pixie::cli`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use clap::{CommandFactory, Parser};
use nix_pixie::cli::Cli;

#[test]
fn cli_definition_is_valid() {
    Cli::command().debug_assert();
}

#[test]
fn relay_flags_exclude_each_other() {
    for command in [
        &["agent"][..],
        &["controller", "pair", "--code", "1-a-b-c"],
        &["controller", "install", "--code", "1-a-b-c"],
    ] {
        let with =
            |flags: &[&str]| Cli::try_parse_from(["nix-pixie"].iter().chain(command).chain(flags));
        assert!(with(&[]).is_ok());
        assert!(with(&["--no-relay"]).is_ok());
        assert!(
            with(&[
                "--relay",
                "https://a.example",
                "--relay",
                "https://b.example"
            ])
            .is_ok()
        );
        assert!(with(&["--relay", "https://a.example", "--no-relay"]).is_err());
    }
}
