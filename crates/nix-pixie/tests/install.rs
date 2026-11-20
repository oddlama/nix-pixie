//! Tests for `nix_pixie::install`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::install::*;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn disk_arguments() {
    let parsed = parse_disk_args(&args(&["main=nvme0n1", "data=/dev/disk/by-id/ata-X"])).unwrap();
    assert_eq!(parsed["main"], "nvme0n1");
    assert_eq!(parsed["data"], "/dev/disk/by-id/ata-X");
    for bad in [
        &["main"][..],
        &["=vdb"],
        &["main="],
        &["main=vdb", "main=vdc"],
    ] {
        assert!(parse_disk_args(&args(bad)).is_err(), "{bad:?}");
    }
}

#[test]
fn tail_keeps_the_last_lines_and_strips_terminal_control() {
    let mut tail = Tail::default();
    for i in 0..100 {
        tail.push(format!("line {i}\n").as_bytes());
    }
    tail.push(b"\x1b[2Jcleared\n");
    let shown = tail.lines(3);
    assert_eq!(shown, "line 98\nline 99\n[2Jcleared");
}

#[test]
fn tail_is_bounded() {
    let mut tail = Tail::default();
    for _ in 0..1000 {
        tail.push(&[b'x'; 1024]);
    }
    tail.push(b"\nend");
    assert_eq!(tail.lines(1), "end");
    assert!(tail.lines(2).len() < 70 * 1024);
}
