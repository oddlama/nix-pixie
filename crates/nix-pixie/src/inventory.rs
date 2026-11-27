//! Collects the target's hardware inventory (the agent runs as root on the
//! installer). Parsing is kept apart from reading, so it can be tested.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use nix_pixie_proto::inventory::{
    Content, Cpu, DeviceTree, Disk, Dmi, Firmware, Inventory, Nic, is_valid_disk_id, nix_system,
};
use serde_json::Value;
use tracing::warn;

use crate::net::{hostname, sanitise};

/// EFI global variable GUID (`SecureBoot`, `SetupMode`).
const EFI_GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
/// Longest string we keep from sysfs or lsblk.
const MAX_FIELD: usize = 128;
/// `compatible` entries kept: real boards have a handful.
const MAX_COMPATIBLE: usize = 8;
/// Mount points listed per disk: enough to tell what holds it.
const MAX_MOUNTS: usize = 16;

/// Everything about this machine. `formatted` are the disks nix-pixie
/// formatted earlier during this boot.
pub async fn collect(formatted: &BTreeSet<String>) -> Inventory {
    let disks = match disks(formatted).await {
        Ok(disks) => disks,
        Err(e) => {
            warn!("listing disks: {e:#}");
            Vec::new()
        }
    };
    Inventory {
        system: nix_system(std::env::consts::ARCH),
        hostname: hostname(),
        dmi: dmi(Path::new("/sys/class/dmi/id")),
        device_tree: device_tree(Path::new("/proc/device-tree")),
        cpu: Cpu {
            model: std::fs::read_to_string("/proc/cpuinfo")
                .ok()
                .and_then(|text| cpu_model(&text)),
            threads: std::thread::available_parallelism()
                .map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX)),
        },
        memory_bytes: std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| mem_total(&text))
            .unwrap_or(0),
        firmware: firmware(Path::new("/sys/firmware/efi")),
        nics: nics_now(),
        disks,
    }
}

/// Runs nixos-facter and returns its JSON report.
pub async fn facter_report(scratch: &Path) -> Result<Vec<u8>> {
    let output = scratch.join("facter.json");
    let run = tokio::process::Command::new("nixos-facter")
        .arg("--output")
        .arg(&output)
        .output();
    let result = tokio::time::timeout(Duration::from_secs(120), run)
        .await
        .context("nixos-facter timed out")?
        .context("running nixos-facter (is it installed?)")?;
    anyhow::ensure!(
        result.status.success(),
        "nixos-facter failed ({}): {}",
        result.status,
        sanitise(&String::from_utf8_lossy(&result.stderr), 500)
    );
    let report = std::fs::read(&output).context("reading the facter report")?;
    let _ = std::fs::remove_file(&output);
    Ok(report)
}

fn read_field(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = sanitise(text.trim(), MAX_FIELD);
    (!text.is_empty()).then_some(text)
}

/// Firmware vendors fill unused DMI fields with placeholders. Treating them as
/// real values would make every such board match the same `dmiSerial` hint.
fn is_placeholder(value: &str) -> bool {
    const PLACEHOLDERS: &[&str] = &[
        "to be filled by o.e.m.",
        "default string",
        "not specified",
        "not applicable",
        "system serial number",
        "system product name",
        "system manufacturer",
        "0123456789",
        "none",
        "n/a",
        "unknown",
        "0",
    ];
    PLACEHOLDERS.contains(&value.to_ascii_lowercase().as_str())
}

pub fn dmi(dir: &Path) -> Dmi {
    let field = |name: &str| read_field(&dir.join(name)).filter(|v| !is_placeholder(v));
    Dmi {
        vendor: field("sys_vendor"),
        product: field("product_name"),
        serial: field("product_serial"),
        uuid: field("product_uuid"),
    }
}

/// The identification in the device tree at `dir` (`/proc/device-tree`).
/// Its properties are NUL-terminated strings, `compatible` a list of them.
pub fn device_tree(dir: &Path) -> DeviceTree {
    let strings = |name: &str| -> Vec<String> {
        let Ok(bytes) = std::fs::read(dir.join(name)) else {
            return Vec::new();
        };
        bytes
            .split(|&b| b == 0)
            .map(|s| sanitise(String::from_utf8_lossy(s).trim(), MAX_FIELD))
            .filter(|s| !s.is_empty())
            .take(MAX_COMPATIBLE)
            .collect()
    };
    DeviceTree {
        model: strings("model").into_iter().next(),
        compatible: strings("compatible"),
        serial: strings("serial-number")
            .into_iter()
            .next()
            .filter(|s| !is_placeholder(s)),
    }
}

pub fn cpu_model(cpuinfo: &str) -> Option<String> {
    cpuinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "model name").then(|| sanitise(value.trim(), MAX_FIELD))
    })
}

pub fn mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

pub fn firmware(efi: &Path) -> Firmware {
    let var = |name: &str| {
        // 4 bytes of attributes, then the value.
        let bytes = std::fs::read(efi.join("efivars").join(format!("{name}-{EFI_GLOBAL}"))).ok()?;
        bytes.get(4).map(|&b| b == 1)
    };
    Firmware {
        uefi: efi.is_dir(),
        secure_boot: var("SecureBoot"),
        setup_mode: var("SetupMode"),
    }
}

/// Physical NICs (with a `device` link), sorted by name.
pub fn nics(dir: &Path) -> Vec<Nic> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut nics: Vec<Nic> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("device").exists())
        .filter_map(|entry| {
            let path = entry.path();
            Some(Nic {
                name: sanitise(&entry.file_name().to_string_lossy(), MAX_FIELD),
                mac: read_field(&path.join("address"))?.to_ascii_lowercase(),
                link_up: read_field(&path.join("operstate")).as_deref() == Some("up"),
                addrs: Vec::new(),
            })
        })
        .collect();
    nics.sort_by(|a, b| a.name.cmp(&b.name));
    nics
}

/// The physical NICs, with their addresses now.
#[must_use]
pub fn nics_now() -> Vec<Nic> {
    let addresses: Vec<(String, IpAddr)> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|interface| (interface.name.clone(), interface.ip()))
        .collect();
    with_addresses(nics(Path::new("/sys/class/net")), &addresses)
}

/// `nics` with their addresses from `addresses` (interface, address),
/// sorted: IPv4 first, then global IPv6, then link-local.
#[must_use]
pub fn with_addresses(mut nics: Vec<Nic>, addresses: &[(String, IpAddr)]) -> Vec<Nic> {
    for nic in &mut nics {
        nic.addrs = addresses
            .iter()
            .filter(|(name, _)| *name == nic.name)
            .map(|(_, ip)| *ip)
            .collect();
        nic.addrs.sort_by_key(|ip| match ip {
            IpAddr::V4(_) => (0, *ip),
            IpAddr::V6(v6) if v6.is_unicast_link_local() => (2, *ip),
            IpAddr::V6(_) => (1, *ip),
        });
        nic.addrs.dedup();
    }
    nics
}

/// The machine's disks, from lsblk and `/dev/disk/by-id`.
pub async fn disks(formatted: &BTreeSet<String>) -> Result<Vec<Disk>> {
    let output = tokio::process::Command::new("lsblk")
        .args([
            "--json",
            "--bytes",
            "--tree",
            "--output",
            "NAME,TYPE,SIZE,MODEL,SERIAL,TRAN,ROTA,RM,RO,PTTYPE,FSTYPE,LABEL,PARTLABEL,MOUNTPOINTS",
        ])
        .output()
        .await
        .context("running lsblk (is util-linux installed?)")?;
    anyhow::ensure!(output.status.success(), "lsblk failed: {}", output.status);
    let json: Value = serde_json::from_slice(&output.stdout).context("parsing lsblk output")?;
    let mut disks = parse_lsblk(&json, &by_id_links(Path::new("/dev/disk/by-id")));
    for disk in &mut disks {
        disk.formatted_this_boot = formatted.contains(&disk.name);
    }
    Ok(disks)
}

/// Kernel device name -> the valid `/dev/disk/by-id` names pointing at it.
fn by_id_links(dir: &Path) -> HashMap<String, Vec<String>> {
    let mut links: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return links;
    };
    for entry in entries.filter_map(Result::ok) {
        let id = entry.file_name().to_string_lossy().into_owned();
        if !is_valid_disk_id(&id) {
            continue;
        }
        if let Ok(target) = std::fs::canonicalize(entry.path())
            && let Some(name) = target.file_name()
        {
            links
                .entry(name.to_string_lossy().into_owned())
                .or_default()
                .push(id);
        }
    }
    for ids in links.values_mut() {
        ids.sort();
    }
    links
}

pub fn parse_lsblk<S: std::hash::BuildHasher>(
    json: &Value,
    by_id: &HashMap<String, Vec<String>, S>,
) -> Vec<Disk> {
    let Some(devices) = json.get("blockdevices").and_then(Value::as_array) else {
        return Vec::new();
    };
    devices
        .iter()
        .filter(|dev| text(dev, "type").as_deref() == Some("disk"))
        .filter_map(|dev| {
            let name = text(dev, "name")?;
            // zram swap devices show up as disks too.
            if name.starts_with("zram") {
                return None;
            }
            let mut contents = Vec::new();
            let mut mounts = Vec::new();
            walk(dev, &mut contents, &mut mounts);
            Some(Disk {
                by_id: by_id.get(&name).cloned().unwrap_or_default(),
                size_bytes: number(dev, "size"),
                model: text(dev, "model"),
                serial: text(dev, "serial"),
                transport: text(dev, "tran"),
                rotational: flag(dev, "rota"),
                removable: flag(dev, "rm"),
                read_only: flag(dev, "ro"),
                partition_table: text(dev, "pttype"),
                contents,
                mounts,
                formatted_this_boot: false,
                name,
            })
        })
        .collect()
}

/// Collects signatures and mounts of `dev` and everything below it.
fn walk(dev: &Value, contents: &mut Vec<Content>, mounts: &mut Vec<String>) {
    let fstype = text(dev, "fstype");
    let partlabel = text(dev, "partlabel");
    if fstype.is_some() || partlabel.is_some() {
        contents.push(Content {
            device: text(dev, "name").unwrap_or_default(),
            size_bytes: number(dev, "size"),
            label: text(dev, "label"),
            fstype,
            partlabel,
        });
    }
    for mount in dev
        .get("mountpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|m| !m.is_empty())
    {
        if mounts.len() < MAX_MOUNTS {
            mounts.push(sanitise(mount, MAX_FIELD));
        }
    }
    for child in dev
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        walk(child, contents, mounts);
    }
}

fn text(dev: &Value, key: &str) -> Option<String> {
    let value = sanitise(dev.get(key)?.as_str()?.trim(), MAX_FIELD);
    (!value.is_empty()).then_some(value)
}

/// lsblk prints booleans as `true`/`false`, older versions as `"0"`/`"1"`.
fn flag(dev: &Value, key: &str) -> bool {
    match dev.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "1",
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        _ => false,
    }
}

fn number(dev: &Value, key: &str) -> u64 {
    match dev.get(key) {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}
