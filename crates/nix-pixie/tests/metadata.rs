//! Tests for `nix_pixie::metadata`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::path::Path;

use nix_pixie::metadata::*;
use nix_pixie_proto::inventory::{Dmi, Inventory, Nic};

const METADATA: &str = r#"{
    "version": 1,
    "machines": {
        "web1": {
            "configuration": "web1",
            "match": { "mac": ["52:54:00:12:01:02"], "dmiSerial": [] },
            "disks": { "file": "machines/web1/disks.json" },
            "hostKey": { "mode": "none", "publicKeyPath": null },
            "hooks": {},
            "facter": { "reportPath": "machines/web1/facter.json" }
        },
        "db1": {
            "configuration": "db",
            "match": { "mac": [], "dmiSerial": ["SN-42"] },
            "disks": { "file": null },
            "hostKey": { "mode": "pull", "publicKeyPath": "machines/db1/host.pub" },
            "hooks": {
                "rekey": { "command": "agenix-rekey", "args": ["rekey"], "after": ["host-key"], "before": ["build"] }
            },
            "facter": { "reportPath": null }
        }
    }
}"#;

fn inventory(mac: &str, serial: Option<&str>) -> Inventory {
    Inventory {
        nics: vec![Nic {
            name: "eth1".into(),
            mac: mac.into(),
            link_up: true,
            addrs: vec![],
        }],
        dmi: Dmi {
            serial: serial.map(Into::into),
            ..Dmi::default()
        },
        ..Inventory::default()
    }
}

#[test]
fn parses_and_suggests() {
    let metadata = parse(METADATA.as_bytes()).unwrap();
    assert_eq!(metadata.machines["db1"].configuration, "db");
    assert_eq!(
        metadata.machines["web1"].facter.report_path.as_deref(),
        Some("machines/web1/facter.json")
    );
    let db1 = &metadata.machines["db1"];
    assert_eq!(db1.host_key.mode, HostKeyMode::Pull);
    assert_eq!(
        db1.host_key.public_key_path.as_deref(),
        Some("machines/db1/host.pub")
    );
    assert_eq!(metadata.machines["web1"].host_key.mode, HostKeyMode::None);

    let by_mac = metadata.suggest(&inventory("52:54:00:12:01:02", None));
    assert_eq!(
        by_mac,
        [Suggestion {
            machine: "web1".into(),
            reasons: vec!["MAC 52:54:00:12:01:02 (eth1)".into()]
        }]
    );
    let by_serial = metadata.suggest(&inventory("aa:aa:aa:aa:aa:aa", Some("SN-42")));
    assert_eq!(
        by_serial
            .iter()
            .map(|s| s.machine.as_str())
            .collect::<Vec<_>>(),
        ["db1"]
    );
    assert_eq!(
        metadata.suggest(&inventory("aa:aa:aa:aa:aa:aa", None)),
        Vec::new()
    );
}

#[test]
fn rejects_other_versions() {
    let other = METADATA.replace("\"version\": 1", "\"version\": 2");
    let err = parse(other.as_bytes()).unwrap_err().to_string();
    assert!(err.contains("version 2"), "{err}");
}

#[test]
fn repo_paths_stay_inside() {
    let root = Path::new("/repo");
    assert_eq!(
        repo_path(root, "machines/a/facter.json").unwrap(),
        Path::new("/repo/machines/a/facter.json")
    );
    for bad in ["", "/etc/passwd", "../x", "a/../../x", "./a"] {
        assert!(repo_path(root, bad).is_err(), "{bad}");
    }
}

#[test]
fn recognises_local_flakes() {
    let dir = std::env::temp_dir().join(format!("nix-pixie-flake-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("flake.nix"), "{}").unwrap();
    let canonical = std::fs::canonicalize(&dir).unwrap();
    let path = dir.to_str().unwrap();
    assert_eq!(local_flake_dir(path), Some(canonical.clone()));
    assert_eq!(
        local_flake_dir(&format!("path:{path}")),
        Some(canonical.clone())
    );
    assert_eq!(
        local_flake_dir(&format!("git+file://{path}?ref=main")),
        Some(canonical)
    );
    assert_eq!(local_flake_dir("github:oddlama/infra"), None);
    assert_eq!(local_flake_dir("/nonexistent/flake"), None);
    std::fs::remove_dir_all(dir).unwrap();
}
