//! Tests for `nix_pixie::hostkey`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::hostkey::*;

const A: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJm8/tSnY/QFVJt1/GwZ/gYCu33MyS0qLVSO3OiU+MZo";
const B: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBe4xglErMJq4pkR96Qbf1v0cUObzexN8WjG1QvJzoJH";
const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBLcf7d7IxN39ypJEOirTD2qcURSlMw2mvCU96TaWDAr1ubFljDm4Sdy12gNLs/jYvCYKx5u7Kj+Tr2Kp+OimLEw=";

#[test]
fn parses_ed25519_keys_strictly() {
    let key = PublicKey::parse(&format!("{A} root@web1\n")).unwrap();
    assert_eq!(key.to_string(), A);
    assert_eq!(PublicKey::parse(A).unwrap(), key);
    assert_ne!(PublicKey::parse(B).unwrap(), key);
    for bad in [
        "",
        "ssh-ed25519",
        ECDSA,
        // Truncated, and the right length but not an ed25519 blob.
        A.strip_suffix('o').unwrap(),
        "ssh-ed25519 AAAAB3NzaC1yc2EAAAADAQABAAABAQC1234567890abcdefghijklmnopqrstuvwxyzABCDEF",
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJm8/tSnY/QFVJt1/GwZ/gYCu33MyS0qLVSO3OiU+MZ$",
    ] {
        assert!(PublicKey::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn pub_file_lines_keep_only_plain_comments() {
    let key = PublicKey::parse(A).unwrap();
    assert_eq!(key.to_line(Some("root@web1")), format!("{A} root@web1\n"));
    assert_eq!(key.to_line(None), format!("{A}\n"));
    assert_eq!(key.to_line(Some("evil\ncomment")), format!("{A}\n"));
}

#[test]
fn reads_keyscan_output() {
    let output = format!(
        "# 192.168.1.3:22 SSH-2.0-OpenSSH_10.0\n192.168.1.3 {A}\n192.168.1.3 {ECDSA}\n\
         [192.168.1.3]:2222 {B}\ngarbage\n"
    );
    let keys: Vec<String> = parse_keyscan(&output)
        .into_iter()
        .map(|k| k.to_string())
        .collect();
    let mut expected = vec![A.to_string(), B.to_string()];
    expected.sort();
    assert_eq!(keys, expected);
}

#[test]
fn judges_answers_while_the_target_reboots() {
    let new_key = PublicKey::parse(A).unwrap();
    let live_key = PublicKey::parse(B).unwrap();
    let keys = |list: &[&PublicKey]| list.iter().map(|k| (*k).clone()).collect();

    // Pull mode: the installer still answering means waiting, not a mismatch.
    assert_eq!(
        judge(&keys(&[&live_key]), Some(&new_key), Some(&live_key)),
        Verdict::Wait
    );
    assert_eq!(
        judge(&keys(&[]), Some(&new_key), Some(&live_key)),
        Verdict::Wait
    );
    assert_eq!(
        judge(&keys(&[&new_key]), Some(&new_key), Some(&live_key)),
        Verdict::Seen(new_key.clone())
    );
    // Without knowing the installer's key, any other key is a mismatch.
    assert_eq!(
        judge(&keys(&[&live_key]), Some(&new_key), None),
        Verdict::Mismatch(live_key.clone())
    );

    // Mode "none": the first key that isn't the installer's.
    assert_eq!(
        judge(&keys(&[&live_key]), None, Some(&live_key)),
        Verdict::Wait
    );
    assert_eq!(
        judge(&keys(&[&new_key]), None, Some(&live_key)),
        Verdict::Seen(new_key)
    );
}

#[test]
fn short_keys_keep_their_end() {
    let key = PublicKey::parse(A).unwrap();
    assert_eq!(key.short(), "ssh-ed25519 …LVSO3OiU+MZo");
}
