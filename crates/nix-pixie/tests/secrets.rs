//! Tests for `nix_pixie::secrets` and the agent's key file policy.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::disko::check_key_path;
use nix_pixie::secrets::*;
use nix_pixie::system::{Encryption, Luks, ZfsKey};
use nix_pixie_proto::ctrl::{ControllerMsg, KeyFile};
use zeroize::Zeroizing;

fn luks(name: &str) -> Luks {
    Luks {
        name: name.into(),
        ask_password: false,
        password_file: None,
        key_file: None,
        additional_key_files: Vec::new(),
    }
}

fn zfs(name: &str, keylocation: &str, keyformat: &str) -> ZfsKey {
    ZfsKey {
        name: name.into(),
        keylocation: keylocation.into(),
        keyformat: keyformat.into(),
    }
}

fn err(encryption: &Encryption) -> String {
    format!("{:#}", needs(encryption).unwrap_err())
}

#[test]
fn files_disko_reads_are_needed_once_each() {
    let encryption = Encryption {
        luks: vec![
            Luks {
                password_file: Some("/tmp/secret.key".into()),
                ..luks("cryptroot")
            },
            Luks {
                key_file: Some("/tmp/secret.key".into()),
                ..luks("cryptdata")
            },
        ],
        zfs: vec![
            zfs("zroot/root", "file:///run/keys/zfs", "passphrase"),
            // Fetched by zfs itself.
            zfs("zroot/remote", "https://keys.example/zroot", "passphrase"),
        ],
    };
    let found = needs(&encryption).unwrap();
    assert_eq!(
        found,
        [
            Need {
                path: "/run/keys/zfs".into(),
                users: vec!["ZFS zroot/root".into()],
                min_len: 8,
                max_len: Some(512),
            },
            Need {
                path: "/tmp/secret.key".into(),
                users: vec!["LUKS cryptroot".into(), "LUKS cryptdata".into()],
                min_len: 1,
                max_len: None,
            },
        ]
    );
    assert_eq!(needs(&Encryption::default()).unwrap(), []);
}

#[test]
fn refuses_what_needs_disko_s_terminal_or_real_key_files() {
    let ask = err(&Encryption {
        luks: vec![Luks {
            ask_password: true,
            ..luks("cryptroot")
        }],
        zfs: vec![],
    });
    assert!(
        ask.contains("askPassword") && ask.contains("passwordFile"),
        "{ask}"
    );

    let prompt = err(&Encryption {
        luks: vec![],
        zfs: vec![zfs("zroot/root", "prompt", "passphrase")],
    });
    assert!(prompt.contains("postCreateHook"), "{prompt}");

    let raw = err(&Encryption {
        luks: vec![],
        zfs: vec![zfs("zroot/root", "file:///tmp/key", "raw")],
    });
    assert!(raw.contains("keyformat"), "{raw}");

    let additional = err(&Encryption {
        luks: vec![Luks {
            password_file: Some("/tmp/secret.key".into()),
            additional_key_files: vec!["/tmp/more.key".into()],
            ..luks("cryptroot")
        }],
        zfs: vec![],
    });
    assert!(additional.contains("additionalKeyFiles"), "{additional}");

    let on_disk = err(&Encryption {
        luks: vec![Luks {
            password_file: Some("/root/secret.key".into()),
            ..luks("cryptroot")
        }],
        zfs: vec![],
    });
    assert!(on_disk.contains("/tmp/ or /run/"), "{on_disk}");
}

#[test]
fn passphrase_limits() {
    let zfs = Need {
        path: "/tmp/zfs".into(),
        users: vec![],
        min_len: 8,
        max_len: Some(512),
    };
    assert!(acceptable(&zfs, "short").is_err());
    assert!(acceptable(&zfs, "long enough").is_ok());
    assert!(acceptable(&zfs, &"x".repeat(513)).is_err());
    let luks = Need {
        min_len: 1,
        max_len: None,
        ..zfs
    };
    assert!(acceptable(&luks, "").is_err());
    assert!(acceptable(&luks, "x").is_ok());
}

const MOUNTINFO: &str = "\
22 1 0:21 / / rw - ext4 /dev/vda rw
23 22 0:22 / /tmp rw - tmpfs tmpfs rw
24 22 0:23 / /run rw - tmpfs tmpfs rw
25 24 0:24 / /run/media rw - ext4 /dev/sdb1 rw
";

#[test]
fn key_files_only_go_into_ram() {
    assert_eq!(check_key_path("/tmp/secret.key", MOUNTINFO), Ok(()));
    assert_eq!(check_key_path("/run/keys/zfs", MOUNTINFO), Ok(()));
    for (path, why) in [
        ("/run/media/usb/key", "would be on ext4"),
        ("/root/key", "plain paths under"),
        ("/tmp", "plain paths under"),
        ("/tmp/../etc/key", "plain paths under"),
        ("tmp/key", "plain paths under"),
    ] {
        let err = check_key_path(path, MOUNTINFO).unwrap_err();
        assert!(err.contains(why), "{path}: {err}");
    }
    // A test VM's /tmp may be on its disk: refused, not silently written.
    let disk_tmp = "22 1 0:21 / / rw - ext4 /dev/vda rw\n";
    assert!(check_key_path("/tmp/secret.key", disk_tmp).is_err());
}

#[test]
fn keys_never_show_in_debug_output() {
    let message = ControllerMsg::RunDisko {
        script: "/nix/store/x-disko".into(),
        key_files: vec![KeyFile {
            path: "/tmp/secret.key".into(),
            contents: Zeroizing::new(b"hunter22".to_vec()),
        }],
    };
    let shown = format!("{message:?}");
    assert!(
        shown.contains("/tmp/secret.key") && shown.contains("<8 bytes>"),
        "{shown}"
    );
    assert!(
        !shown.contains("hunter22") && !shown.contains("104"),
        "{shown}"
    );
}
