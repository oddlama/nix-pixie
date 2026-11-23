//! Finding a target through a relay, from its code alone.
//!
//! On a local network the target announces itself and the controller hears
//! it. Across the internet nobody hears anything, so the two need a meeting
//! point both can compute from what they share: the nameplate.
//!
//! An iroh relay forwards packets to whoever is connected to it under an
//! endpoint id, which means whoever holds that ed25519 key. So the target
//! connects a second endpoint, the *rendezvous endpoint*, with a key derived
//! from its nameplate ([`secret`]). The controller derives the same key,
//! dials that endpoint id through the relay and is sent the target's
//! [`Announcement`], the same one a probe reply carries. It then dials the
//! endpoint named there, as it would after finding it on its own network.
//!
//! Nothing here is secret, and nothing is authenticated. The nameplate is
//! public, so anyone can compute the key, answer in the target's place and
//! announce any endpoint. That is the same as forging an announcement on a
//! local network: it sends the controller to the wrong machine, where
//! pairing fails. The words of the code take no part in this, so neither the
//! relay nor an impostor learns anything that helps guessing them.

use sha2::{Digest, Sha256};

use crate::announce::{AnnounceError, Announcement};

/// Domain separation for [`secret`]. Changing it moves every rendezvous.
const CONTEXT: &[u8] = b"nix-pixie rendezvous 1\0";

/// An encoded announcement is never longer than this (see
/// [`crate::announce::PROBE_LEN`]). Readers stop here.
pub const MAX_REPLY: usize = 512;

/// The ed25519 secret key of the rendezvous endpoint for `nameplate`.
/// Despite the name, everybody who knows the nameplate can compute it.
#[must_use]
pub fn secret(nameplate: u32) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(CONTEXT);
    hash.update(nameplate.to_be_bytes());
    hash.finalize().into()
}

/// What a rendezvous endpoint sends whoever connects: the whole content of
/// one stream.
///
/// # Errors
///
/// If the announcement doesn't serialise, which only a bug can cause.
pub fn encode(announcement: &Announcement) -> postcard::Result<Vec<u8>> {
    let reply = postcard::to_stdvec(announcement)?;
    debug_assert!(reply.len() <= MAX_REPLY, "reply of {} bytes", reply.len());
    Ok(reply)
}

/// The announcement a rendezvous endpoint sent.
///
/// # Errors
///
/// If `reply` is too long, malformed or of another protocol version.
pub fn decode(reply: &[u8]) -> Result<Announcement, AnnounceError> {
    if reply.len() > MAX_REPLY {
        return Err(AnnounceError::Invalid("reply"));
    }
    postcard::from_bytes::<Announcement>(reply)
        .map_err(|_| AnnounceError::Invalid("reply"))?
        .validated()
}
