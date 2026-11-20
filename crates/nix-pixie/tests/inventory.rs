//! Tests for `nix_pixie::inventory`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::collections::HashMap;

use nix_pixie::inventory::*;
use nix_pixie_proto::inventory::{Firmware, Nic};
use serde_json::Value;

#[test]
fn parses_lsblk() {
    let json: Value = serde_json::from_str(
        r#"{"blockdevices": [
          {"name":"sr0","type":"rom","size":1048576,"model":"QEMU DVD-ROM","serial":"QM00003","tran":"ata","rota":true,"rm":true,"ro":false,"pttype":null,"fstype":"iso9660","label":"nixos","partlabel":null,"mountpoints":["/iso"]},
          {"name":"vda","type":"disk","size":1073741824,"model":null,"serial":"pixie-data","tran":"virtio","rota":true,"rm":false,"ro":false,"pttype":"gpt","fstype":null,"label":null,"partlabel":null,"mountpoints":[null],
           "children":[
             {"name":"vda1","type":"part","size":536870912,"model":null,"serial":null,"tran":null,"rota":true,"rm":false,"ro":false,"pttype":"gpt","fstype":"ext4","label":"old-root","partlabel":"disk-main-root","mountpoints":[null]},
             {"name":"vda2","type":"part","size":536870912,"model":null,"serial":null,"tran":null,"rota":true,"rm":false,"ro":false,"pttype":"gpt","fstype":"crypto_LUKS","label":null,"partlabel":null,"mountpoints":[null],
              "children":[{"name":"luks","type":"crypt","size":520093696,"fstype":"swap","label":null,"partlabel":null,"mountpoints":["[SWAP]"]}]}
           ]},
          {"name":"nvme0n1","type":"disk","size":"500107862016","model":"Samsung SSD 980 \u001b[31m","serial":"S64A","tran":"nvme","rota":"0","rm":"0","ro":"1","pttype":null,"fstype":null,"label":null,"partlabel":null,"mountpoints":[null]},
          {"name":"zram0","type":"disk","size":1024,"mountpoints":["[SWAP]"]}
        ]}"#,
    )
    .unwrap();
    let by_id = HashMap::from([("vda".to_string(), vec!["virtio-pixie-data".to_string()])]);
    let disks = parse_lsblk(&json, &by_id);
    assert_eq!(
        disks.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        ["vda", "nvme0n1"]
    );

    let vda = &disks[0];
    assert_eq!(vda.by_id, ["virtio-pixie-data"]);
    assert_eq!(vda.size_bytes, 1 << 30);
    assert_eq!(vda.partition_table.as_deref(), Some("gpt"));
    assert!(vda.rotational && !vda.removable && !vda.read_only);
    assert!(vda.in_use, "swap inside LUKS counts as in use");
    let fstypes: Vec<_> = vda.contents.iter().map(|c| c.fstype.as_deref()).collect();
    assert_eq!(fstypes, [Some("ext4"), Some("crypto_LUKS"), Some("swap")]);
    assert_eq!(vda.contents[0].partlabel.as_deref(), Some("disk-main-root"));

    let nvme = &disks[1];
    assert_eq!(nvme.size_bytes, 500_107_862_016);
    assert_eq!(
        nvme.model.as_deref(),
        Some("Samsung SSD 980 [31m"),
        "control characters stripped"
    );
    assert!(!nvme.rotational && nvme.read_only && !nvme.in_use && !nvme.has_data());
}

#[test]
fn parses_proc_files() {
    assert_eq!(
        mem_total("MemTotal:        2034232 kB\nMemFree: 1 kB\n"),
        Some(2_034_232 * 1024)
    );
    assert_eq!(mem_total("garbage"), None);
    let cpuinfo = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Xeon(R) CPU\n";
    assert_eq!(cpu_model(cpuinfo).as_deref(), Some("Intel(R) Xeon(R) CPU"));
    assert_eq!(cpu_model("processor : 0\n"), None);
}

#[test]
fn reads_sysfs_like_trees() {
    let root = std::env::temp_dir().join(format!("nix-pixie-inventory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let write = |path: &str, contents: &[u8]| {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    };
    write("dmi/sys_vendor", b"QEMU\n");
    write("dmi/product_name", b"Standard PC\n");
    write("dmi/product_serial", b"To Be Filled By O.E.M.\n");
    write(
        "efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c",
        &[6, 0, 0, 0, 1],
    );
    write(
        "efi/efivars/SetupMode-8be4df61-93ca-11d2-aa0d-00e098032b8c",
        &[6, 0, 0, 0, 0],
    );
    write("net/eth0/address", b"52:54:00:12:01:02\n");
    write("net/eth0/operstate", b"up\n");
    write("net/eth0/device/vendor", b"x");
    write("net/br0/address", b"aa:bb:cc:dd:ee:ff\n");

    let dmi = dmi(&root.join("dmi"));
    assert_eq!(dmi.vendor.as_deref(), Some("QEMU"));
    assert_eq!(dmi.serial, None, "placeholder serials are dropped");
    let efi = firmware(&root.join("efi"));
    assert_eq!(
        efi,
        Firmware {
            uefi: true,
            secure_boot: Some(true),
            setup_mode: Some(false)
        }
    );
    assert_eq!(
        firmware(&root.join("missing")),
        Firmware {
            uefi: false,
            secure_boot: None,
            setup_mode: None
        }
    );
    let nics = nics(&root.join("net"));
    assert_eq!(
        nics,
        [Nic {
            name: "eth0".into(),
            mac: "52:54:00:12:01:02".into(),
            link_up: true
        }]
    );
    std::fs::remove_dir_all(root).unwrap();
}
