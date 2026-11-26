//! Tests for `nix_pixie::relay_status`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::time::Duration;

use nix_pixie::relay_status::{PATIENCE, Reach, Sample};

fn sample(connected: bool, error: Option<&str>) -> Sample {
    Sample {
        url: "https://relay.example.org/".into(),
        connected,
        error: error.map(str::to_string),
    }
}

#[test]
fn a_connected_relay_is_reachable() {
    let reach = Reach::judge(&[sample(true, None)], Duration::ZERO);
    assert_eq!(reach, Reach::Reachable("https://relay.example.org/".into()));
    let line = reach.line("n0's public relays");
    assert!(
        line.contains("https://relay.example.org/ is reachable") && line.contains("other networks"),
        "{line}"
    );
}

#[test]
fn errors_count_only_after_the_patience() {
    // A network still coming up: not worth alarming anybody yet.
    let early = Reach::judge(&[sample(false, Some("dns failed"))], PATIENCE / 2);
    assert_eq!(early, Reach::Trying);
    assert_eq!(
        early.line("n0's public relays"),
        "Relay: trying to reach n0's public relays..."
    );

    let late = Reach::judge(&[sample(false, Some("dns failed"))], PATIENCE);
    let line = late.line("n0's public relays");
    assert!(
        line.starts_with("Relay: https://relay.example.org/ is not reachable (dns failed).")
            && line.contains("only controllers on this network"),
        "{line}"
    );
}

#[test]
fn no_relay_picked_yet_names_the_configured_ones() {
    let reach = Reach::judge(&[], PATIENCE * 2);
    assert_eq!(
        reach,
        Reach::Unreachable {
            relay: None,
            error: None
        }
    );
    let line = reach.line("n0's public relays");
    assert!(
        line.starts_with("Relay: can't reach n0's public relays."),
        "{line}"
    );
}
