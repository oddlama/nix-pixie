//! Passphrases for encrypted layouts: which key files the disko
//! configuration reads on the target while formatting, and asking the
//! operator for them on the terminal.
//!
//! disko can read a passphrase from a file (`passwordFile` and
//! `settings.keyFile` for LUKS, `keylocation = "file://..."` for ZFS), or ask
//! on its own terminal (`askPassword`, `keylocation = "prompt"`). The agent
//! runs disko without a terminal, so only files work: the controller asks
//! for each, and the agent puts them into RAM for the disko run. Taking them
//! from scripts instead of a terminal is for later.

use std::collections::BTreeMap;
use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use nix_pixie_proto::ctrl::KeyFile;
use zeroize::Zeroizing;

use crate::system::Encryption;

/// ZFS refuses shorter passphrases.
const ZFS_MIN_PASSPHRASE: usize = 8;
/// ZFS refuses longer ones.
const ZFS_MAX_PASSPHRASE: usize = 512;
/// Where the agent accepts key files (in RAM on the installer).
const KEY_DIRS: [&str; 2] = ["/tmp/", "/run/"];
const ATTEMPTS: usize = 3;

/// A key file the layout needs, and what reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// Path on the target.
    pub path: String,
    /// E.g. `LUKS cryptroot`, `ZFS zroot/root`.
    pub users: Vec<String>,
    /// Shortest acceptable passphrase.
    pub min_len: usize,
    /// Longest acceptable passphrase, if there is a limit.
    pub max_len: Option<usize>,
}

/// The key files the layout reads while formatting. Refuses setups that
/// would need disko's own terminal, with what to configure instead.
pub fn needs(encryption: &Encryption) -> Result<Vec<Need>> {
    let mut needs: BTreeMap<String, Need> = BTreeMap::new();
    let mut need =
        |path: &str, user: String, min_len: usize, max_len: Option<usize>| -> Result<()> {
            if !KEY_DIRS.iter().any(|dir| path.starts_with(dir)) {
                bail!(
                    "{user} reads its key from {path}; nix-pixie only puts keys under {} on the \
                 installer, which is in RAM",
                    KEY_DIRS.join(" or ")
                );
            }
            let entry = needs.entry(path.to_string()).or_insert_with(|| Need {
                path: path.to_string(),
                users: Vec::new(),
                min_len: 1,
                max_len: None,
            });
            entry.users.push(user);
            entry.min_len = entry.min_len.max(min_len);
            entry.max_len = match (entry.max_len, max_len) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            Ok(())
        };

    for luks in &encryption.luks {
        let user = format!("LUKS {}", luks.name);
        if luks.ask_password {
            bail!(
                "{user} has askPassword (disko's default when neither passwordFile nor \
                 settings.keyFile is set), so disko would ask on a terminal the installer \
                 doesn't give it. Set passwordFile = \"/tmp/secret.key\": nix-pixie asks you for \
                 the passphrase and puts it there for disko. The installed system still asks for \
                 it at boot"
            );
        }
        if !luks.additional_key_files.is_empty() {
            bail!(
                "{user} has additionalKeyFiles; key files can't be typed in, and nix-pixie can't \
                 take them from elsewhere yet"
            );
        }
        for path in [&luks.password_file, &luks.key_file].into_iter().flatten() {
            need(path, user.clone(), 1, None)?;
        }
    }
    for zfs in &encryption.zfs {
        let user = format!("ZFS {}", zfs.name);
        if zfs.keylocation == "prompt" {
            bail!(
                "{user} has keylocation = \"prompt\", so zfs would ask on a terminal the installer \
                 doesn't give it. Create it with keylocation = \"file:///tmp/secret.key\" and \
                 switch to prompting afterwards with postCreateHook = \"zfs set \
                 keylocation=prompt {}\": nix-pixie asks you for the passphrase and puts it there \
                 for disko",
                zfs.name
            );
        }
        let Some(path) = zfs.keylocation.strip_prefix("file://") else {
            // https:// and friends: zfs fetches the key itself.
            continue;
        };
        if zfs.keyformat != "passphrase" {
            bail!(
                "{user} has keyformat = \"{}\"; only passphrases can be typed in, and nix-pixie \
                 can't take key files from elsewhere yet",
                zfs.keyformat
            );
        }
        need(path, user, ZFS_MIN_PASSPHRASE, Some(ZFS_MAX_PASSPHRASE))?;
    }
    Ok(needs.into_values().collect())
}

/// Whether the passphrases can be asked for: only on a terminal for now.
pub fn check_interactive(needs: &[Need]) -> Result<()> {
    if needs.is_empty() || std::io::stdin().is_terminal() {
        return Ok(());
    }
    let paths: Vec<&str> = needs.iter().map(|n| n.path.as_str()).collect();
    bail!(
        "the layout is encrypted, and its passphrases ({}) are typed in on a terminal; run \
         this on one",
        paths.join(", ")
    )
}

/// Asks the operator for every key file's passphrase, twice, without echo.
pub async fn ask(needs: Vec<Need>) -> Result<Vec<KeyFile>> {
    let mut files = Vec::new();
    for need in needs {
        let file = tokio::task::spawn_blocking(move || ask_one(&need))
            .await
            .context("asking for a passphrase")??;
        files.push(file);
    }
    Ok(files)
}

fn ask_one(need: &Need) -> Result<KeyFile> {
    let what = need.users.join(", ");
    for _ in 0..ATTEMPTS {
        let first = Zeroizing::new(
            rpassword::prompt_password(format!("Passphrase for {what} ({}): ", need.path))
                .context("reading the passphrase from the terminal")?,
        );
        if let Err(reason) = acceptable(need, &first) {
            eprintln!("{reason}");
            continue;
        }
        let again = Zeroizing::new(
            rpassword::prompt_password("Again: ").context("reading the passphrase")?,
        );
        if *first != *again {
            eprintln!("They differ.");
            continue;
        }
        return Ok(KeyFile {
            path: need.path.clone(),
            contents: Zeroizing::new(first.as_bytes().to_vec()),
        });
    }
    bail!("no passphrase for {}", need.path)
}

/// Whether `passphrase` fits what reads it.
pub fn acceptable(need: &Need, passphrase: &str) -> Result<(), String> {
    let len = passphrase.len();
    if len < need.min_len {
        return Err(format!("At least {} characters.", need.min_len));
    }
    if let Some(max) = need.max_len
        && len > max
    {
        return Err(format!("At most {max} characters."));
    }
    Ok(())
}
