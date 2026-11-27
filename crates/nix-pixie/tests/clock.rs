//! Tests for `nix_pixie::clock`: how far off a target's clock was.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::clock::describe;

#[test]
fn offsets_read_in_the_largest_fitting_unit() {
    assert_eq!(describe(3_400), "3 s ahead");
    assert_eq!(describe(-90_000), "90 s behind");
    assert_eq!(describe(-1_500_000), "25 min behind");
    assert_eq!(describe(36_000_000), "10 h ahead");
    assert_eq!(describe(-143 * 86_400_000), "143 days behind");
    assert_eq!(describe(i64::MIN), "106751991167 days behind");
}
