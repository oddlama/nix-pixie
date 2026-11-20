//! Tests for `nix_pixie::identity`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use nix_pixie::identity::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nix-pixie-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn creates_once_then_loads_the_same_key() {
    let dir = temp_dir("identity");
    let path = dir.join("nested/identity");
    let first = load_or_create(&path).unwrap();
    let second = load_or_create(&path).unwrap();
    assert_eq!(first.public(), second.public());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn refuses_a_world_readable_key() {
    let dir = temp_dir("identity-perms");
    let path = dir.join("identity");
    load_or_create(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let err = load_or_create(&path).unwrap_err().to_string();
    assert!(err.contains("accessible by other users"), "{err}");
    fs::remove_dir_all(dir).unwrap();
}
