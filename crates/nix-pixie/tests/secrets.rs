//! Tests for `nix_pixie::secrets` and the agent's key file policy.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::disko::check_key_path;
use std::collections::BTreeMap;

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

fn passphrase(min: usize, max: Option<usize>) -> Kind {
    Kind::Passphrase { min, max }
}

#[test]
fn files_disko_reads_are_needed_once_each() {
    let encryption = Encryption {
        luks: vec![
            Luks {
                password_file: Some("/tmp/secret.key".into()),
                additional_key_files: vec!["/run/keys/extra".into()],
                ..luks("cryptroot")
            },
            Luks {
                key_file: Some("/tmp/secret.key".into()),
                ..luks("cryptdata")
            },
        ],
        zfs: vec![
            zfs("zroot/root", "file:///tmp/secret.key", "passphrase"),
            zfs("zroot/raw", "file:///run/keys/raw", "raw"),
            // Fetched by zfs itself.
            zfs("zroot/remote", "https://keys.example/zroot", "passphrase"),
        ],
    };
    let found = needs(&encryption).unwrap();
    let summary: Vec<(&str, Vec<&str>, Kind)> = found
        .iter()
        .map(|n| {
            (
                n.path.as_str(),
                n.users.iter().map(String::as_str).collect(),
                n.kind,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                "/run/keys/extra",
                vec!["LUKS cryptroot"],
                passphrase(1, None)
            ),
            ("/run/keys/raw", vec!["ZFS zroot/raw"], Kind::Raw),
            (
                "/tmp/secret.key",
                vec!["LUKS cryptroot", "LUKS cryptdata", "ZFS zroot/root"],
                // The strictest of everything that reads it.
                passphrase(8, Some(512)),
            ),
        ]
    );
    assert_eq!(needs(&Encryption::default()).unwrap(), []);
}

#[test]
fn refuses_what_needs_disko_s_terminal_and_conflicting_uses() {
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

    let conflict = err(&Encryption {
        luks: vec![Luks {
            password_file: Some("/tmp/key".into()),
            ..luks("cryptroot")
        }],
        zfs: vec![zfs("zroot/root", "file:///tmp/key", "raw")],
    });
    assert!(conflict.contains("/tmp/key is read as"), "{conflict}");

    let unknown = err(&Encryption {
        luks: vec![],
        zfs: vec![zfs("zroot/root", "file:///tmp/key", "base64")],
    });
    assert!(unknown.contains("base64"), "{unknown}");

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
fn keys_must_fit_what_reads_them() {
    let need = |kind| Need {
        path: "/tmp/key".into(),
        users: vec![],
        kind,
    };
    let zfs = need(passphrase(8, Some(512)));
    assert!(check(&zfs, b"short").is_err());
    assert!(check(&zfs, b"long enough").is_ok());
    assert!(check(&zfs, &[b'x'; 513]).is_err());
    let luks = need(passphrase(1, None));
    assert!(check(&luks, b"").is_err());
    assert!(check(&luks, &[0, 255, 7]).is_ok(), "any bytes from a hook");

    let raw = need(Kind::Raw);
    assert!(check(&raw, &[0; 32]).is_ok());
    assert!(check(&raw, &[0; 33]).is_err());
    let hex = need(Kind::Hex);
    let digits = [b'a'; 64];
    assert!(check(&hex, &digits).is_ok());
    assert!(check(&hex, &[&digits[..], b"\n"].concat()).is_ok());
    assert!(check(&hex, &[b'g'; 64]).is_err());
    assert!(check(&hex, &digits[1..]).is_err());

    assert!(passphrase(1, None).typeable());
    assert!(!Kind::Raw.typeable() && !Kind::Hex.typeable());
}

#[tokio::test]
async fn keys_from_hooks_come_first() {
    let need = |path: &str, kind| Need {
        path: path.into(),
        users: vec!["LUKS cryptroot".into()],
        kind,
    };
    let provided = |list: &[(&str, &[u8])]| {
        list.iter()
            .map(|(p, k)| ((*p).to_string(), Zeroizing::new(k.to_vec())))
            .collect::<BTreeMap<_, _>>()
    };

    // Everything provided: nothing to ask, and unused keys are dropped.
    let files = gather(
        vec![
            need("/tmp/a", passphrase(1, None)),
            need("/run/raw", Kind::Raw),
        ],
        provided(&[
            ("/tmp/a", b"secret"),
            ("/run/raw", &[1; 32]),
            ("/tmp/unused", b"x"),
        ]),
    )
    .await
    .unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["/tmp/a", "/run/raw"]);

    let wrong = gather(
        vec![need("/run/raw", Kind::Raw)],
        provided(&[("/run/raw", b"short")]),
    )
    .await
    .unwrap_err();
    assert!(format!("{wrong:#}").contains("32 bytes"), "{wrong:#}");

    let missing = gather(vec![need("/run/raw", Kind::Raw)], BTreeMap::new())
        .await
        .unwrap_err();
    assert!(
        format!("{missing:#}").contains("$PIXIE_OUT/disk-keys/run/raw"),
        "{missing:#}"
    );

    // Without hooks, keys that can't be typed can't come from anywhere.
    let unavailable = check_available(&[need("/run/raw", Kind::Raw)], false).unwrap_err();
    assert!(
        unavailable.to_string().contains("can't be typed"),
        "{unavailable}"
    );
    assert!(check_available(&[need("/run/raw", Kind::Raw)], true).is_ok());
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

#[test]
fn pushed_host_keys_never_show_in_debug_output() {
    use nix_pixie_proto::ctrl::Secret;
    let message = ControllerMsg::ProvideHostKey {
        private: Secret(Zeroizing::new(
            b"-----BEGIN OPENSSH PRIVATE KEY-----".to_vec(),
        )),
    };
    let shown = format!("{message:?}");
    assert!(shown.contains("<35 secret bytes>"), "{shown}");
    assert!(!shown.contains("OPENSSH"), "{shown}");
}
