//! Tests for `nix_pixie::plan` and `nix_pixie::system`'s parsing.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::collections::BTreeMap;

use nix_pixie::plan::*;
use nix_pixie::system::{Built, Disko, DiskoDisk, Encryption, parse_build_output};
use nix_pixie_proto::inventory::{Content, Disk};

fn disk(name: &str, by_id: &[&str]) -> Disk {
    Disk {
        name: name.into(),
        by_id: by_id.iter().map(|s| (*s).to_string()).collect(),
        size_bytes: 2 << 30,
        serial: by_id
            .first()
            .and_then(|id| id.strip_prefix("virtio-"))
            .map(str::to_string),
        ..Disk::default()
    }
}

/// The installer's own disk, an empty one, and one with data.
fn target() -> Vec<Disk> {
    vec![
        Disk {
            mounts: vec!["/iso".into()],
            ..disk("vda", &["virtio-installer"])
        },
        disk("vdb", &["virtio-pixie-a"]),
        Disk {
            partition_table: Some("gpt".into()),
            contents: vec![Content {
                device: "vdc1".into(),
                fstype: Some("ext4".into()),
                ..Content::default()
            }],
            ..disk("vdc", &["virtio-pixie-b"])
        },
    ]
}

fn disko(disks: &[(&str, Option<&str>)]) -> Disko {
    Disko {
        root_mount_point: "/mnt".into(),
        zpools: Vec::new(),
        by_pixie_links: true,
        encryption: Encryption::default(),
        disks: disks
            .iter()
            .map(|(name, device)| {
                (
                    (*name).to_string(),
                    DiskoDisk {
                        device: device.map(str::to_string),
                        part_labels: vec!["disk-main-root".into()],
                    },
                )
            })
            .collect(),
    }
}

fn requests(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn plan(config: &Disko, pinned: bool, wanted: &[(&str, &str)]) -> anyhow::Result<Resolution> {
    let disks = target();
    resolve(&slots(config, pinned, &disks)?, &disks, &requests(wanted))
}

fn done(resolution: Resolution) -> Vec<Choice> {
    match resolution {
        Resolution::Done(choices) => choices,
        Resolution::Ask { role, .. } => panic!("asked for {role}"),
    }
}

fn agnostic() -> Disko {
    disko(&[("main", Some("/dev/disk/by-pixie/main"))])
}

#[test]
fn asks_when_more_than_one_disk_is_free() {
    // The installer's disk is never a candidate.
    assert_eq!(
        plan(&agnostic(), false, &[]).unwrap(),
        Resolution::Ask {
            role: "main".into(),
            candidates: vec!["vdb".into(), "vdc".into()],
        }
    );
}

#[test]
fn agnostic_disks_are_linked() {
    for spec in [
        "vdb",
        "/dev/vdb",
        "virtio-pixie-a",
        "/dev/disk/by-id/virtio-pixie-a",
        "pixie-a",
    ] {
        let choices = done(plan(&agnostic(), false, &[("main", spec)]).unwrap());
        assert_eq!(
            choices,
            [Choice {
                name: "main".into(),
                role: "main".into(),
                disk: "vdb".into(),
                pin: None,
                link: true,
            }],
            "{spec}"
        );
    }
}

#[test]
fn the_last_free_disk_is_taken_without_asking() {
    let disks = vec![target().remove(0), target().remove(1)];
    let config = agnostic();
    let choices = done(
        resolve(
            &slots(&config, false, &disks).unwrap(),
            &disks,
            &BTreeMap::new(),
        )
        .unwrap(),
    );
    assert_eq!(choices[0].disk, "vdb");
}

#[test]
fn pinned_disks_are_recorded_by_id_and_kept() {
    let unpinned = disko(&[("main", Some("/dev/disk/by-pixie/main"))]);
    let choices = done(plan(&unpinned, true, &[("main", "vdc")]).unwrap());
    assert_eq!(
        choices[0].pin.as_deref(),
        Some("/dev/disk/by-id/virtio-pixie-b")
    );
    assert!(!choices[0].link);

    // Reinstalling keeps the recorded disk, unless the operator says otherwise.
    let recorded = disko(&[("main", Some("/dev/disk/by-id/virtio-pixie-b"))]);
    assert_eq!(done(plan(&recorded, true, &[]).unwrap())[0].disk, "vdc");
    assert_eq!(
        done(plan(&recorded, true, &[("main", "vdb")]).unwrap())[0].disk,
        "vdb"
    );

    // A recorded disk that is gone is chosen anew.
    let elsewhere = disko(&[("main", Some("/dev/disk/by-id/virtio-gone"))]);
    assert!(matches!(
        plan(&elsewhere, true, &[]).unwrap(),
        Resolution::Ask { .. }
    ));

    let json = pins_json(&choices).unwrap();
    assert_eq!(
        String::from_utf8(json).unwrap(),
        "{\n  \"main\": \"/dev/disk/by-id/virtio-pixie-b\"\n}\n"
    );
}

#[test]
fn fixed_devices_must_be_on_the_target_and_are_not_choosable() {
    let fixed = disko(&[("main", Some("/dev/disk/by-id/virtio-pixie-a"))]);
    let choices = done(plan(&fixed, false, &[]).unwrap());
    assert_eq!((choices[0].disk.as_str(), choices[0].link), ("vdb", false));
    let err = plan(&fixed, false, &[("main", "vdc")]).unwrap_err();
    assert!(format!("{err:#}").contains("fixed"), "{err:#}");

    let missing = disko(&[("main", Some("/dev/disk/by-id/nvme-elsewhere"))]);
    let err = plan(&missing, false, &[]).unwrap_err();
    assert!(format!("{err:#}").contains("not on this target"), "{err:#}");

    let label = disko(&[("main", Some("/dev/disk/by-label/root"))]);
    let err = plan(&label, false, &[]).unwrap_err();
    assert!(
        format!("{err:#}").contains("can only find disks named"),
        "{err:#}"
    );
}

#[test]
fn unset_devices_explain_the_options() {
    let err = plan(&disko(&[("main", None)]), false, &[]).unwrap_err();
    let err = format!("{err:#}");
    assert!(
        err.contains("not set") && err.contains("pinnedDisk"),
        "{err}"
    );
}

#[test]
fn a_recorded_disk_in_use_says_what_holds_it() {
    let mut disks = target();
    disks[2].mounts = vec!["/mnt".into(), "/mnt/boot".into()];
    let recorded = disko(&[("main", Some("/dev/disk/by-id/virtio-pixie-b"))]);
    let err = resolve(
        &slots(&recorded, true, &disks).unwrap(),
        &disks,
        &requests(&[]),
    )
    .unwrap_err();
    let err = format!("{err:#}");
    assert!(
        err.starts_with("main can't go onto the disk recorded for it, vdc; choose another")
            && err.contains("vdc is in use (/mnt, /mnt/boot), and not by an earlier attempt")
            && err.contains("unmount it on the target, or boot the installer again"),
        "{err}"
    );
    // Its own attempt earlier in this boot: disko unmounts it again.
    disks[2].formatted_this_boot = true;
    let choices = done(
        resolve(
            &slots(&recorded, true, &disks).unwrap(),
            &disks,
            &requests(&[]),
        )
        .unwrap(),
    );
    assert_eq!(choices[0].disk, "vdc");
}

#[test]
fn refuses_disks_in_use_and_double_assignments() {
    let err = format!(
        "{:#}",
        plan(&agnostic(), false, &[("main", "vda")]).unwrap_err()
    );
    assert!(
        err.contains("vda is in use (/iso)") && err.contains("unmount it"),
        "{err}"
    );

    let two = disko(&[
        ("main", Some("/dev/disk/by-pixie/main")),
        ("data", Some("/dev/disk/by-pixie/data")),
    ]);
    let err = plan(&two, false, &[("main", "vdb"), ("data", "vdb")]).unwrap_err();
    assert!(format!("{err:#}").contains("both"), "{err:#}");

    let err = plan(&agnostic(), false, &[("nope", "vdb")]).unwrap_err();
    assert!(format!("{err:#}").contains("no such role"), "{err:#}");
}

#[test]
fn disks_formatted_during_this_boot_stay_installable() {
    let mut disks = target();
    disks[2].mounts = vec!["/mnt".into(), "/mnt/boot".into()];
    disks[2].formatted_this_boot = true;
    let config = agnostic();
    let choices = done(
        resolve(
            &slots(&config, false, &disks).unwrap(),
            &disks,
            &requests(&[("main", "vdc")]),
        )
        .unwrap(),
    );
    assert_eq!(choices[0].disk, "vdc");
}

#[test]
fn checks_requested_roles_before_pairing() {
    let mixed = disko(&[
        ("main", Some("/dev/disk/by-pixie/main")),
        ("fixed", Some("/dev/disk/by-id/virtio-x")),
    ]);
    assert!(check_requested(&mixed, false, &requests(&[("main", "vdb")])).is_ok());
    assert!(check_requested(&mixed, false, &requests(&[("fixed", "vdb")])).is_err());
    // Pinned mode: every disko disk is a role.
    assert!(check_requested(&mixed, true, &requests(&[("fixed", "vdb")])).is_ok());
}

#[test]
fn verifies_that_the_configuration_reads_the_pins() {
    let choices = vec![Choice {
        name: "main".into(),
        role: "main".into(),
        disk: "vdc".into(),
        pin: Some("/dev/disk/by-id/virtio-pixie-b".into()),
        link: false,
    }];
    let reads = disko(&[("main", Some("/dev/disk/by-id/virtio-pixie-b"))]);
    assert!(verify_pins(&reads, &choices, "disks.json").is_ok());
    let ignores = disko(&[("main", Some("/dev/disk/by-pixie/main"))]);
    let err = verify_pins(&ignores, &choices, "disks.json").unwrap_err();
    assert!(
        err.to_string().contains("nix-pixie.lib.pinnedDisk"),
        "{err}"
    );
}

#[test]
fn stable_ids_prefer_model_and_serial() {
    let nvme = disk(
        "nvme0n1",
        &[
            "nvme-eui.002538b1a1b2c3d4",
            "nvme-Samsung_SSD_980_1TB_S64ANS0T123456_1",
            "nvme-Samsung_SSD_980_1TB_S64ANS0T123456",
        ],
    );
    assert_eq!(
        stable_id(&nvme),
        Some("nvme-Samsung_SSD_980_1TB_S64ANS0T123456")
    );
    let sata = disk(
        "sda",
        &[
            "wwn-0x5000c500a1b2c3d4",
            "scsi-SATA_WDC_WD40EFRX_WD-1",
            "ata-WDC_WD40EFRX_WD-1",
        ],
    );
    assert_eq!(stable_id(&sata), Some("ata-WDC_WD40EFRX_WD-1"));
    assert_eq!(stable_id(&disk("loop0", &[])), None);
}

#[test]
fn ambiguous_specs_are_refused() {
    let mut disks = target();
    disks[2].serial = Some("vdb".into());
    let err = find(&disks, "vdb").unwrap_err();
    assert!(err.to_string().contains("several"), "{err}");
}

#[test]
fn build_results_come_in_installable_order() {
    let json = br#"[
        {"drvPath": "/nix/store/a-disko.drv", "outputs": {"out": "/nix/store/a-disko"}},
        {"drvPath": "/nix/store/b-nixos-system.drv", "outputs": {"out": "/nix/store/b-nixos-system"}}
    ]"#;
    assert_eq!(
        parse_build_output(json).unwrap(),
        Built {
            disko_script: "/nix/store/a-disko".into(),
            toplevel: "/nix/store/b-nixos-system".into(),
        }
    );
    assert!(parse_build_output(b"[]").is_err());
}

#[test]
fn refuses_other_disks_carrying_names_disko_creates() {
    let mut config = agnostic();
    config.zpools = vec!["zroot".into()];
    let names = created_names(&config);
    assert_eq!(
        names.iter().map(String::as_str).collect::<Vec<_>>(),
        ["disk-main-root", "zroot"]
    );

    let old_install = Disk {
        contents: vec![Content {
            device: "vdc2".into(),
            partlabel: Some("disk-main-root".into()),
            fstype: Some("ext4".into()),
            ..Content::default()
        }],
        ..disk("vdc", &["virtio-pixie-b"])
    };
    let old_pool = Disk {
        contents: vec![Content {
            device: "sda1".into(),
            fstype: Some("zfs_member".into()),
            label: Some("zroot".into()),
            ..Content::default()
        }],
        ..disk("sda", &[])
    };
    // An unrelated label, and a filesystem label that merely equals a name.
    let unrelated = Disk {
        contents: vec![Content {
            device: "sdb1".into(),
            partlabel: Some("data".into()),
            fstype: Some("ext4".into()),
            label: Some("zroot".into()),
            ..Content::default()
        }],
        ..disk("sdb", &[])
    };
    let disks = vec![disk("vdb", &[]), old_install, old_pool, unrelated];

    let formatting = ["vdb"].into_iter().collect();
    let err = check_name_clashes(&names, &formatting, &disks).unwrap_err();
    assert!(err.contains("vdc (disk-main-root), sda (zroot)"), "{err}");
    assert!(!err.contains("sdb"), "{err}");

    // Formatting the old disk too erases its names.
    let formatting = ["vdb", "vdc", "sda"].into_iter().collect();
    assert_eq!(check_name_clashes(&names, &formatting, &disks), Ok(()));
}

#[test]
fn agnostic_disks_need_the_module_that_keeps_their_links() {
    let mut without = agnostic();
    without.by_pixie_links = false;
    let error = check_links(&without, false, "web1")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("/dev/disk/by-pixie/main") && error.contains("nixosModules.default"),
        "{error}"
    );
    check_links(&agnostic(), false, "web1").unwrap();
    // Pinned mode falls back to by-pixie paths only until it records.
    check_links(&without, true, "web1").unwrap();
    // Fixed disks need nothing.
    let mut fixed = disko(&[("main", Some("/dev/disk/by-id/nvme-x"))]);
    fixed.by_pixie_links = false;
    check_links(&fixed, false, "web1").unwrap();
}
