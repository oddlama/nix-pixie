//! Tests for `nix_pixie::agent`.

use std::time::Duration;

use nix_pixie::agent::rotation_delay;

#[test]
fn rotation_delay_doubles_up_to_a_minute() {
    let secs: Vec<u64> = (1..=8).map(|n| rotation_delay(n).as_secs()).collect();
    assert_eq!(secs, [2, 4, 8, 16, 32, 60, 60, 60]);
}

#[test]
fn rotation_delay_saturates() {
    assert_eq!(rotation_delay(0), Duration::from_secs(2));
    assert_eq!(rotation_delay(u32::MAX), Duration::from_secs(60));
}
