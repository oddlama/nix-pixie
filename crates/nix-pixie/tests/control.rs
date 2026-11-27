//! Tests for `nix_pixie::control`: the relays the operator can ask for.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::control::{Setting, chosen, controller_code};
use nix_pixie::net::Relays;

#[test]
fn relays_are_asked_for_by_url_or_by_name() {
    let words = |w: &[&str]| w.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    assert_eq!(Setting::parse(&words(&["off"])).unwrap(), Setting::Off);
    assert_eq!(Setting::parse(&words(&["n0"])).unwrap(), Setting::N0);
    assert_eq!(
        Setting::parse(&words(&["default"])).unwrap(),
        Setting::Configured
    );
    assert_eq!(
        Setting::parse(&words(&["http://1.2.3.4:3340"])).unwrap(),
        Setting::Urls(vec!["http://1.2.3.4:3340".into()])
    );
    assert!(Setting::parse(&[]).is_err());
    let err = format!("{:#}", Setting::parse(&words(&["1.2.3.4"])).unwrap_err());
    assert!(err.contains("not a relay URL"), "{err}");
}

#[test]
fn chosen_relays_last_the_boot_in_a_state_file() {
    assert_eq!(chosen("off\n").unwrap(), Relays::Disabled);
    assert_eq!(chosen("n0").unwrap(), Relays::N0);
    let relays = chosen("http://1.2.3.4:3340\nhttps://relay.example.org\n").unwrap();
    assert_eq!(relays.urls().len(), 2);
    assert!(chosen("").is_err() || matches!(chosen("").unwrap(), Relays::N0));
}

#[test]
fn only_a_controllers_code_can_be_given() {
    assert_eq!(
        controller_code("acid-acorn-acre").unwrap().to_string(),
        "acid-acorn-acre"
    );
    let err = controller_code("482913-acid-acorn-acre").unwrap_err();
    assert!(err.contains("target's"), "{err}");
    assert!(controller_code("not a code").is_err());
}
