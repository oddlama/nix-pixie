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

#[test]
fn a_rebuild_compares_against_the_disks_as_pinned() {
    use nix_pixie::install::planned_facts;
    use nix_pixie::plan::Choice;
    use nix_pixie::system::{Boot, Disko, DiskoDisk, Encryption, Facts, Ssh};
    let disk = |device: &str| DiskoDisk {
        device: Some(device.into()),
        part_labels: vec![],
    };
    let facts = Facts {
        system: "x86_64-linux".into(),
        disko: Some(Disko {
            root_mount_point: "/mnt".into(),
            disks: [
                ("main".to_string(), disk("/dev/disk/by-pixie/main")),
                ("data".to_string(), disk("/dev/disk/by-id/nvme-data")),
            ]
            .into(),
            zpools: vec![],
            by_pixie_links: false,
            encryption: Encryption::default(),
        }),
        boot: Boot::default(),
        ssh: Ssh {
            enable: true,
            ports: vec![22],
            host_key_path: None,
        },
    };
    let choices = [Choice {
        name: "main".into(),
        role: "main".into(),
        disk: "nvme0n1".into(),
        pin: Some("/dev/disk/by-id/nvme-main".into()),
        link: false,
    }];
    let planned = planned_facts(&facts, &choices);
    let disks = &planned.disko.as_ref().unwrap().disks;
    // Recorded before the first build: what a rebuild evaluates to.
    assert_eq!(
        disks["main"].device.as_deref(),
        Some("/dev/disk/by-id/nvme-main")
    );
    assert_eq!(
        disks["data"].device.as_deref(),
        Some("/dev/disk/by-id/nvme-data")
    );
    assert_eq!(planned.ssh, facts.ssh);
}
