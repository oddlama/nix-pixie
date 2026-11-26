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

#[test]
fn placed_files_get_exact_modes_and_stay_inside() {
    use std::os::unix::fs::PermissionsExt;

    use nix_pixie::nixos::place_file;
    use nix_pixie_proto::ctrl::PlacedFile;
    use zeroize::Zeroizing;

    let base = std::env::temp_dir().join(format!("nix-pixie-test-place-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("mnt");
    let outside = base.join("installer-etc");
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    // Like NixOS's /etc/static, a symlink out of the installed system.
    std::os::unix::fs::symlink(&outside, root.join("etc/static")).unwrap();
    let real_root = std::fs::canonicalize(&root).unwrap();
    let root_str = root.to_str().unwrap();
    let file = |path: &str, mode: u32, contents: &[u8]| {
        PlacedFile::file(path.into(), mode, Zeroizing::new(contents.to_vec()))
    };

    place_file(
        &real_root,
        root_str,
        &file("/var/lib/secret", 0o4640, b"one"),
    )
    .unwrap();
    let placed = root.join("var/lib/secret");
    assert_eq!(std::fs::read(&placed).unwrap(), b"one");
    let mode = std::fs::metadata(&placed).unwrap().permissions().mode() & 0o7777;
    assert_eq!(
        mode, 0o640,
        "setuid dropped, exactly 0640 whatever the umask"
    );
    // Overwrites.
    place_file(
        &real_root,
        root_str,
        &file("/var/lib/secret", 0o600, b"two"),
    )
    .unwrap();
    assert_eq!(std::fs::read(&placed).unwrap(), b"two");

    let err = place_file(
        &real_root,
        root_str,
        &file("/etc/static/passwd", 0o644, b"x"),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("symlink"), "{err}");
    assert!(!outside.join("passwd").exists());
    let err = place_file(&real_root, root_str, &file("/etc", 0o644, b"x")).unwrap_err();
    assert!(err.to_string().contains("directory"), "{err}");
    std::fs::remove_dir_all(&base).unwrap();
}
