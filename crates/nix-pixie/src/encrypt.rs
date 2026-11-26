//! Secrets hooks hand to the controller for the repository, which only ever
//! get there encrypted with age, to the machine's `secrets.recipients`: a
//! LUKS recovery key the target generated, say. A hook writes the plaintext
//! to `$PIXIE_OUT/encrypted/<path>` (in RAM), and `<path>` in the
//! repository gets the age file.

use std::io::Write;

use anyhow::{Context, Result, bail};

/// One recipient: an age X25519 key (`age1...`) or an SSH public key
/// (`ssh-ed25519 ...`, `ssh-rsa ...`).
fn recipient(text: &str) -> Result<Box<dyn age::Recipient + Send>> {
    let text = text.trim();
    if text.starts_with("age1") {
        let recipient: age::x25519::Recipient = text
            .parse()
            .map_err(|e: &str| anyhow::anyhow!("{text}: {e}"))?;
        return Ok(Box::new(recipient));
    }
    if text.starts_with("ssh-") {
        let recipient: age::ssh::Recipient =
            text.parse().map_err(|e| anyhow::anyhow!("{text}: {e:?}"))?;
        return Ok(Box::new(recipient));
    }
    bail!("{text}: not an age recipient (age1...) or SSH public key (ssh-...)")
}

/// Checks the machine's recipients, before anything is encrypted to them.
pub fn check_recipients(recipients: &[String]) -> Result<()> {
    for text in recipients {
        recipient(text).context("secrets.recipients")?;
    }
    Ok(())
}

/// `plaintext`, as an age file for `recipients` (binary, like agenix's).
pub fn encrypt(recipients: &[String], plaintext: &[u8]) -> Result<Vec<u8>> {
    if recipients.is_empty() {
        bail!(
            "the machine has no secrets.recipients to encrypt to; secrets only go into the \
             repository encrypted"
        );
    }
    let parsed: Vec<Box<dyn age::Recipient + Send>> = recipients
        .iter()
        .map(|r| recipient(r))
        .collect::<Result<_>>()?;
    let encryptor = age::Encryptor::with_recipients(parsed.iter().map(|r| r.as_ref() as _))
        .context("encrypting to the recipients")?;
    let mut out = Vec::new();
    let mut writer = encryptor.wrap_output(&mut out).context("encrypting")?;
    writer.write_all(plaintext).context("encrypting")?;
    writer.finish().context("encrypting")?;
    Ok(out)
}
