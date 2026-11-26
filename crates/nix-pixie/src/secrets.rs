//! Encryption keys for the layout: which key files disko reads on the target
//! while formatting, and where they come from.
//!
//! disko can read a key from a file (`passwordFile`, `settings.keyFile` and
//! `additionalKeyFiles` for LUKS, `keylocation = "file://..."` for ZFS), or
//! ask on its own terminal (`askPassword`, `keylocation = "prompt"`). The
//! agent runs disko without a terminal, so only files work. Their contents
//! come from hooks (`$PIXIE_OUT/disk-keys/<path>`), and whatever no hook
//! provided is asked for on the operator's terminal. The agent puts them
//! into RAM for the disko run.

use std::collections::BTreeMap;
use std::fmt;
use std::io::IsTerminal;

use anyhow::{Context, Result, bail};
use nix_pixie_proto::ctrl::KeyFile;
use tracing::warn;
use zeroize::Zeroizing;

use crate::system::Encryption;
use crate::ui;

/// ZFS refuses shorter passphrases.
const ZFS_MIN_PASSPHRASE: usize = 8;
/// ZFS refuses longer ones.
const ZFS_MAX_PASSPHRASE: usize = 512;
/// Where the agent accepts key files (in RAM on the installer).
const KEY_DIRS: [&str; 2] = ["/tmp/", "/run/"];
const ATTEMPTS: usize = 3;

/// What a key file has to contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A passphrase or key file: typed in, or any bytes from a hook.
    Passphrase { min: usize, max: Option<usize> },
    /// ZFS `keyformat = "raw"`: exactly 32 bytes. Only from a hook.
    Raw,
    /// ZFS `keyformat = "hex"`: 64 hex digits. Only from a hook.
    Hex,
}

impl Kind {
    /// Whether the operator can type it in.
    pub fn typeable(self) -> bool {
        matches!(self, Self::Passphrase { .. })
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Passphrase { .. } => "passphrase",
            Self::Raw => "raw (32 bytes)",
            Self::Hex => "hex (64 digits)",
        })
    }
}

/// A key file the layout needs, and what reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// Path on the target.
    pub path: String,
    /// E.g. `LUKS cryptroot`, `ZFS zroot/root`.
    pub users: Vec<String>,
    pub kind: Kind,
}

/// The key files the layout reads while formatting. Refuses setups that
/// would need disko's own terminal, with what to configure instead.
pub fn needs(encryption: &Encryption) -> Result<Vec<Need>> {
    let mut needs: BTreeMap<String, Need> = BTreeMap::new();
    let mut need = |path: &str, user: String, kind: Kind| -> Result<()> {
        if !KEY_DIRS.iter().any(|dir| path.starts_with(dir)) {
            bail!(
                "{user} reads its key from {path}; nix-pixie only puts keys under {} on the \
                 installer, which is in RAM",
                KEY_DIRS.join(" or ")
            );
        }
        let Some(entry) = needs.get_mut(path) else {
            needs.insert(
                path.to_string(),
                Need {
                    path: path.to_string(),
                    users: vec![user],
                    kind,
                },
            );
            return Ok(());
        };
        entry.kind = match (entry.kind, kind) {
            (Kind::Passphrase { min: a, max: x }, Kind::Passphrase { min: b, max: y }) => {
                Kind::Passphrase {
                    min: a.max(b),
                    max: match (x, y) {
                        (Some(x), Some(y)) => Some(x.min(y)),
                        (x, y) => x.or(y),
                    },
                }
            }
            (a, b) if a == b => a,
            (a, b) => bail!(
                "{path} is read as a {a} key by {} and as a {b} key by {user}",
                entry.users.join(", ")
            ),
        };
        entry.users.push(user);
        Ok(())
    };
    let any = Kind::Passphrase { min: 1, max: None };

    for luks in &encryption.luks {
        let user = format!("LUKS {}", luks.name);
        if luks.ask_password {
            bail!(
                "{user} has askPassword (disko's default when neither passwordFile nor \
                 settings.keyFile is set), so disko would ask on a terminal the installer \
                 doesn't give it. Set passwordFile = \"/tmp/secret.key\": nix-pixie puts the \
                 passphrase there for disko, from a hook or asked of you. The installed system \
                 still asks for it at boot"
            );
        }
        for path in [&luks.password_file, &luks.key_file]
            .into_iter()
            .flatten()
            .chain(&luks.additional_key_files)
        {
            need(path, user.clone(), any)?;
        }
    }
    for zfs in &encryption.zfs {
        let user = format!("ZFS {}", zfs.name);
        if zfs.keylocation == "prompt" {
            bail!(
                "{user} has keylocation = \"prompt\", so zfs would ask on a terminal the installer \
                 doesn't give it. Create it with keylocation = \"file:///tmp/secret.key\" and \
                 switch to prompting afterwards with postCreateHook = \"zfs set \
                 keylocation=prompt {}\": nix-pixie puts the passphrase there for disko, from a \
                 hook or asked of you",
                zfs.name
            );
        }
        let Some(path) = zfs.keylocation.strip_prefix("file://") else {
            // https:// and friends: zfs fetches the key itself.
            continue;
        };
        let kind = match zfs.keyformat.as_str() {
            "passphrase" => Kind::Passphrase {
                min: ZFS_MIN_PASSPHRASE,
                max: Some(ZFS_MAX_PASSPHRASE),
            },
            "raw" => Kind::Raw,
            "hex" => Kind::Hex,
            other => bail!("{user} has keyformat = \"{other}\", which nix-pixie doesn't know"),
        };
        need(path, user, kind)?;
    }
    Ok(needs.into_values().collect())
}

/// Whether the keys can come from somewhere, as far as can be told before
/// any hook ran: with hooks, they may provide them; without, only typeable
/// ones on a terminal can be had.
pub fn check_available(needs: &[Need], has_hooks: bool) -> Result<()> {
    if has_hooks {
        return Ok(());
    }
    if let Some(need) = needs.iter().find(|n| !n.kind.typeable()) {
        bail!(
            "{} needs a {} key at {}, which can't be typed in: provide it from a hook \
             ($PIXIE_OUT/disk-keys{})",
            need.users.join(", "),
            need.kind,
            need.path,
            need.path
        );
    }
    check_terminal(needs)
}

fn check_terminal(needs: &[Need]) -> Result<()> {
    if needs.is_empty() || std::io::stdin().is_terminal() {
        return Ok(());
    }
    let paths: Vec<&str> = needs.iter().map(|n| n.path.as_str()).collect();
    bail!(
        "the layout is encrypted, and no hook provided the keys for {}: run this on a \
         terminal to type them in, or provide them from a hook ($PIXIE_OUT/disk-keys/...)",
        paths.join(", ")
    )
}

/// Keys for every need: what hooks `provided` (by path on the target), and
/// the rest asked for on the terminal. Keys nothing reads are dropped.
pub async fn gather(
    needs: Vec<Need>,
    mut provided: BTreeMap<String, Zeroizing<Vec<u8>>>,
) -> Result<Vec<KeyFile>> {
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for need in needs {
        if let Some(contents) = provided.remove(&need.path) {
            // A hook may provide an empty passphrase on purpose: a bootstrap
            // one, replaced on the first boot (by a TPM, say).
            let empty_passphrase =
                contents.is_empty() && matches!(need.kind, Kind::Passphrase { .. });
            let checked = if empty_passphrase {
                Ok(())
            } else {
                check(&need, &contents)
            };
            checked.map_err(|reason| {
                anyhow::anyhow!("the key a hook provided for {}: {reason}", need.path)
            })?;
            files.push(KeyFile {
                path: need.path,
                contents,
            });
        } else if need.kind.typeable() {
            missing.push(need);
        } else {
            bail!(
                "{} needs a {} key at {}, and no hook provided it ($PIXIE_OUT/disk-keys{})",
                need.users.join(", "),
                need.kind,
                need.path,
                need.path
            );
        }
    }
    for path in provided.keys() {
        warn!(%path, "a hook provided a key the layout doesn't read; not sending it");
    }
    check_terminal(&missing)?;
    // rpassword reads the terminal itself.
    let _terminal = (!missing.is_empty()).then(ui::suspend);
    for need in missing {
        let file = tokio::task::spawn_blocking(move || ask(&need))
            .await
            .context("asking for a passphrase")??;
        files.push(file);
    }
    Ok(files)
}

fn ask(need: &Need) -> Result<KeyFile> {
    let what = need.users.join(", ");
    for _ in 0..ATTEMPTS {
        let first = Zeroizing::new(
            rpassword::prompt_password(format!("Passphrase for {what} ({}): ", need.path))
                .context("reading the passphrase from the terminal")?,
        );
        if let Err(reason) = check(need, first.as_bytes()) {
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

/// Whether `key` fits what reads it. Written as is: mind trailing newlines,
/// which are part of a key file (one is allowed after a hex key).
pub fn check(need: &Need, key: &[u8]) -> Result<(), String> {
    match need.kind {
        Kind::Passphrase { min, max } => {
            let len = key.len();
            if len < min.max(1) {
                return Err(format!("At least {} characters.", min.max(1)));
            }
            if let Some(max) = max
                && len > max
            {
                return Err(format!("At most {max} characters."));
            }
        }
        Kind::Raw if key.len() != 32 => {
            return Err(format!("A raw key has 32 bytes, not {}.", key.len()));
        }
        Kind::Hex => {
            let digits = key.strip_suffix(b"\n").unwrap_or(key);
            if digits.len() != 64 || !digits.iter().all(u8::is_ascii_hexdigit) {
                return Err("A hex key has 64 hex digits.".into());
            }
        }
        Kind::Raw => {}
    }
    Ok(())
}
