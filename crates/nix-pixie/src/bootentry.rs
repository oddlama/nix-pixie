//! A firmware boot entry for the installed system (`AddBootEntry`), for boot
//! loaders that make none themselves: lanzaboote installs systemd-boot to the
//! EFI system partition and leaves the firmware's variables alone. Then the
//! firmware boots whatever comes first in its boot order, which may be the
//! installer again, and again after the reboot that enrolls Secure Boot
//! keys. This does what `bootctl install` would: an entry "Linux Boot
//! Manager" for the partition's systemd-boot, first in the boot order.
//! Through efibootmgr.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::info;

/// The entry's name, as systemd-boot's own.
pub const LABEL: &str = "Linux Boot Manager";

/// What the firmware needs for booting the installed system next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing: no UEFI, or the boot loader makes its own entry.
    Nothing,
    /// An entry for the systemd-boot on the EFI system partition mounted
    /// here, on the target.
    Add { esp: String },
    /// One, but the configuration doesn't allow writing EFI variables
    /// (`boot.loader.efi.canTouchEfiVariables = false`).
    NotAllowed,
}

impl Plan {
    /// For a configuration's boot loader, installed under `root` on a target
    /// that boots with UEFI or not.
    #[must_use]
    pub fn of(boot: &crate::system::Boot, uefi: bool, root: &str) -> Self {
        if !uefi || !boot.lanzaboote {
            return Self::Nothing;
        }
        if !boot.efi_variables {
            return Self::NotAllowed;
        }
        let esp = Path::new(root).join(boot.esp_mount_point.trim_start_matches('/'));
        Self::Add {
            esp: esp.to_string_lossy().into_owned(),
        }
    }
}

/// The firmware's boot entries and order, as `efibootmgr --verbose` lists
/// them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Listing {
    /// `BootOrder`, as hex numbers.
    pub order: Vec<String>,
    pub entries: Vec<Entry>,
}

/// One `Boot####` variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its hex number, as in `BootOrder`.
    pub number: String,
    /// The rest of its line: the label and the device path.
    pub text: String,
}

impl Entry {
    /// Whether it boots `loader` (`\EFI\...`) from the partition with GPT
    /// GUID `partuuid`.
    #[must_use]
    pub fn boots(&self, partuuid: &str, loader: &str) -> bool {
        let text = self.text.to_ascii_lowercase().replace('/', "\\");
        text.contains(&partuuid.to_ascii_lowercase())
            && text.contains(&loader.to_ascii_lowercase().replace('/', "\\"))
    }
}

/// Reads `efibootmgr --verbose`'s output.
#[must_use]
pub fn parse(output: &str) -> Listing {
    let mut listing = Listing::default();
    for line in output.lines() {
        if let Some(order) = line.strip_prefix("BootOrder:") {
            listing.order = order
                .split(',')
                .map(|n| n.trim().to_ascii_uppercase())
                .filter(|n| !n.is_empty())
                .collect();
        } else if let Some(rest) = line.strip_prefix("Boot")
            && let Some(number) = rest.get(..4)
            && number.chars().all(|c| c.is_ascii_hexdigit())
        {
            let text = rest.get(4..).unwrap_or_default();
            listing.entries.push(Entry {
                number: number.to_ascii_uppercase(),
                text: text.trim_start_matches('*').trim().to_string(),
            });
        }
    }
    listing
}

/// `order` with `first` in front.
#[must_use]
pub fn order_with_first(order: &[String], first: &str) -> Vec<String> {
    std::iter::once(first.to_string())
        .chain(order.iter().filter(|n| *n != first).cloned())
        .collect()
}

/// The partition an EFI system partition is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// Its disk, e.g. `/dev/nvme0n1`.
    pub disk: PathBuf,
    /// Its number on the disk, from 1.
    pub number: u32,
    /// Its GPT GUID.
    pub partuuid: String,
}

/// Makes the firmware boot the systemd-boot on the EFI system partition
/// mounted at `esp`: its entry, made unless there, first in the boot order.
/// Returns the entry's number, and whether it was made.
pub async fn add(esp: &Path) -> Result<(String, bool)> {
    let loader = loader(esp)?;
    let partition = partition(esp).await?;
    let listing = list().await?;
    let found = |listing: &Listing| {
        listing
            .entries
            .iter()
            .find(|e| e.boots(&partition.partuuid, &loader))
            .map(|e| e.number.clone())
    };
    let (number, created) = if let Some(number) = found(&listing) {
        (number, false)
    } else {
        efibootmgr(&[
            "--create".as_ref(),
            "--disk".as_ref(),
            partition.disk.as_os_str(),
            "--part".as_ref(),
            partition.number.to_string().as_ref(),
            "--label".as_ref(),
            LABEL.as_ref(),
            "--loader".as_ref(),
            loader.as_ref(),
        ])
        .await
        .context("making the boot entry")?;
        let number = found(&list().await?).with_context(|| {
            format!(
                "efibootmgr made no entry for {loader} on {}",
                partition.partuuid
            )
        })?;
        (number, true)
    };
    let listing = list().await?;
    if listing.order.first() != Some(&number) {
        let order = order_with_first(&listing.order, &number).join(",");
        efibootmgr(&["--bootorder".as_ref(), order.as_ref()])
            .await
            .context("putting the boot entry first")?;
        // Some firmware takes a write and keeps its own.
        let kept = list().await?.order;
        if kept.first() != Some(&number) {
            bail!(
                "the firmware keeps its own boot order ({}) rather than Boot{number} first",
                kept.join(",")
            );
        }
    }
    info!(
        entry = %format!("Boot{number}"),
        %loader,
        disk = %partition.disk.display(),
        created,
        "the firmware boots the installed system next"
    );
    Ok((number, created))
}

/// systemd-boot on the EFI system partition, as the firmware names it:
/// `\EFI\systemd\systemd-boot<arch>.efi`.
fn loader(esp: &Path) -> Result<String> {
    let dir = esp.join("EFI/systemd");
    let names = std::fs::read_dir(&dir)
        .with_context(|| format!("looking for systemd-boot in {}", dir.display()))?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            let path = Path::new(name);
            name.to_ascii_lowercase().starts_with("systemd-boot")
                && path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("efi"))
        })
        .collect::<Vec<_>>();
    match names.as_slice() {
        [name] => Ok(format!("\\EFI\\systemd\\{name}")),
        [] => bail!("there is no systemd-boot in {}", dir.display()),
        _ => bail!("more than one systemd-boot in {}: {names:?}", dir.display()),
    }
}

/// The partition mounted at `esp`: by the mount's device number, which is a
/// partition of a disk, or else no place for a boot entry (RAID, say).
async fn partition(esp: &Path) -> Result<Partition> {
    let mountinfo =
        std::fs::read_to_string("/proc/self/mountinfo").context("reading the mount table")?;
    let device = mounted_device(&mountinfo, esp)
        .with_context(|| format!("nothing is mounted at {}", esp.display()))?;
    let sys = std::fs::canonicalize(format!("/sys/dev/block/{device}"))
        .with_context(|| format!("looking up block device {device}"))?;
    let number = std::fs::read_to_string(sys.join("partition"))
        .ok()
        .and_then(|n| n.trim().parse().ok())
        .with_context(|| {
            format!(
                "{} holds the EFI system partition, but isn't a partition of a disk: make the \
                 boot entries yourself",
                sys.display()
            )
        })?;
    let name = |path: &Path| {
        path.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    };
    let disk = PathBuf::from("/dev").join(name(sys.parent().unwrap_or(&sys)));
    let node = PathBuf::from("/dev").join(name(&sys));
    let out = tokio::process::Command::new("lsblk")
        .args(["--nodeps", "--noheadings", "--output", "PARTUUID"])
        .arg(&node)
        .output()
        .await
        .context("running lsblk")?;
    let partuuid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || partuuid.is_empty() {
        bail!("{} has no GPT partition GUID", node.display());
    }
    Ok(Partition {
        disk,
        number,
        partuuid,
    })
}

/// The device number (`major:minor`) mounted at `mount_point`, from
/// `/proc/self/mountinfo`; the last such mount, which is the one in use.
#[must_use]
pub fn mounted_device(mountinfo: &str, mount_point: &Path) -> Option<String> {
    let wanted = mount_point.to_string_lossy();
    mountinfo.lines().rev().find_map(|line| {
        let mut fields = line.split(' ');
        let device = fields.nth(2)?;
        let point = fields.nth(1)?;
        (unescape(point) == wanted).then(|| device.to_string())
    })
}

/// A mountinfo path: spaces and the like as octal escapes.
fn unescape(field: &str) -> String {
    let mut out = String::new();
    let mut chars = field.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let digits: String = (0..3).filter_map(|_| chars.next()).collect();
            if let Ok(byte) = u8::from_str_radix(&digits, 8) {
                out.push(char::from(byte));
            } else {
                out.push('\\');
                out.push_str(&digits);
            }
        } else {
            out.push(c);
        }
    }
    out
}

async fn list() -> Result<Listing> {
    efibootmgr(&["--verbose".as_ref()])
        .await
        .map(|out| parse(&out))
        .context("listing the boot entries")
}

async fn efibootmgr(args: &[&std::ffi::OsStr]) -> Result<String> {
    let out = tokio::process::Command::new("efibootmgr")
        .args(args)
        .output()
        .await
        .context("running efibootmgr")?;
    if !out.status.success() {
        bail!(
            "efibootmgr failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
