//! Tests for `nix_pixie::nixos`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::path::PathBuf;

use nix_pixie::nixos::below_root;

#[test]
fn host_key_paths_stay_below_the_root() {
    assert_eq!(
        below_root("/mnt", "/etc/ssh/ssh_host_ed25519_key").unwrap(),
        PathBuf::from("/mnt/etc/ssh/ssh_host_ed25519_key")
    );
    // Impermanence setups keep keys elsewhere.
    assert_eq!(
        below_root("/mnt", "/persist/etc/ssh/ssh_host_ed25519_key").unwrap(),
        PathBuf::from("/mnt/persist/etc/ssh/ssh_host_ed25519_key")
    );
    // `.` is harmless: it names the same file.
    assert_eq!(
        below_root("/mnt", "/etc/./key").unwrap(),
        PathBuf::from("/mnt/etc/key")
    );
    for bad in ["etc/ssh/key", "/", "/etc/../../key", "/..", ""] {
        assert!(below_root("/mnt", bad).is_err(), "{bad:?}");
    }
}
