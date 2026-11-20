//! Pairing: SPAKE2 over the pairing code, bound to the iroh connection.
//!
//! Both sides already know the peer's endpoint id: iroh authenticates it
//! during the QUIC handshake. What they don't know is whether that endpoint is
//! the machine the operator meant. The PAKE answers that: only someone who knows
//! the code derives the same key, and an attacker gets one online guess per
//! attempt.
//!
//! The PAKE runs *inside* a connection whose encryption does not depend on
//! the PAKE key. Without binding the two, an attacker holding one connection to
//! each side could relay the SPAKE2 messages unchanged. Both sides would agree on
//! a key while each is actually talking to the attacker. So the key confirmation
//! MACs cover both endpoint ids *as each side observed them*. In a relay, the
//! controller sees the attacker's id where the target sees its own, the MACs
//! differ, and both sides fail.
//!
//! Message flow. The *owner* is the side that generated the code and shows it
//! (the target for target-shown codes, the controller for controller-shown
//! codes). The *typist* is the side where the operator typed it in. Who opened
//! the QUIC connection doesn't matter. The controller always dials, so it
//! never has to accept connections through the operator's firewall.
//!
//! ```text
//! typist                                owner
//! Start { version, role, spake }  ->  <- Start { version, role, spake }
//! Confirm { mac }                 ->
//!                                     <- Confirm { mac }  | Mismatch
//! Paired | Mismatch               ->
//! ```
//!
//! The typist confirms first. The owner reveals its MAC only after verifying
//! the typist's. Otherwise an attacker could send a guess, receive the owner's
//! MAC, check its guess against it, and hang up. That attempt would look like a
//! dropped connection, not a wrong code. With this order, an attacker who hangs
//! up early learns nothing, and every real guess ends in
//! [`PairError::Mismatch`] on the owner. That is exactly what the owner counts
//! to decide when to rotate the code.
//!
//! Both sides report success only when both MACs verified.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::PROTOCOL_VERSION;
use crate::code::Code;
use crate::frame::{FrameError, read_frame, write_frame};

/// SPAKE2 identity string; separates our use of the code from any other.
pub const SPAKE_IDENTITY: &[u8] = b"nix-pixie/pair/1";
const CONFIRM_KEY_LABEL: &[u8] = b"nix-pixie/pair/1 confirm key";
/// Pairing frames are tiny; anything bigger is an attack or a bug.
pub const MAX_FRAME: usize = 1024;

type HmacSha256 = Hmac<Sha256>;

/// Raw ed25519 public key bytes of an iroh endpoint.
pub type EndpointIdBytes = [u8; 32];

/// Which end of the session we are. Independent of who owns the code, and
/// bound into the key confirmation so neither side's MAC can be reflected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Controller,
    Target,
}

impl Role {
    fn peer(self) -> Self {
        match self {
            Self::Controller => Self::Target,
            Self::Target => Self::Controller,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Controller => 1,
            Self::Target => 2,
        }
    }
}

/// Our relation to the code. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The operator typed the code here. Confirms first.
    TypedCode,
    /// We generated and showed the code. Confirms last, and counts failures.
    OwnsCode,
}

/// Frames of the pairing exchange, in wire order: both sides send `Start`,
/// then `Confirm` (the typist first), and the owner ends with `Paired` or
/// `Mismatch`.
#[derive(Debug, Serialize, Deserialize)]
pub enum PairFrame {
    Start {
        version: u32,
        role: Role,
        spake: Vec<u8>,
    },
    Confirm {
        mac: [u8; 32],
    },
    Paired,
    Mismatch,
}

/// Why pairing failed.
#[derive(Debug, thiserror::Error)]
pub enum PairError {
    /// The codes differ, or someone is relaying the exchange. The two cases
    /// can't be told apart, by design. On the owner, this is one spent guess
    /// at the code.
    #[error("pairing failed: wrong code, or the connection is being tampered with")]
    Mismatch,
    #[error("peer speaks protocol version {peer}, we speak {ours}")]
    Version { ours: u32, peer: u32 },
    #[error("peer claims role {peer:?}, but we are {ours:?}")]
    Role { ours: Role, peer: Role },
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("pairing timed out")]
    Timeout,
    #[error(transparent)]
    Frame(#[from] FrameError),
}

/// A failed pairing attempt.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct PairFailure {
    pub error: PairError,
    /// Whether the peer could have learned if its guess at the code was
    /// right. Both sides count these and stop using the code after a few:
    ///
    /// * The owner spends a guess when it receives a wrong confirmation.
    /// * The typist spends a guess as soon as it has sent its confirmation,
    ///   whatever happens next. A rogue owner can check one guess against it
    ///   and then just hang up.
    pub guess_spent: bool,
}

/// Inputs to [`pair`]. The endpoint ids must come from the transport's
/// authentication (iroh), never from anything the peer sent.
#[derive(Clone, Copy, Debug)]
pub struct PairParams<'a> {
    pub role: Role,
    pub side: Side,
    pub code: &'a Code,
    pub local_id: EndpointIdBytes,
    pub remote_id: EndpointIdBytes,
}

/// Runs the pairing exchange, giving up after `timeout`. On `Ok`, both sides
/// verified each other's key confirmation.
///
/// # Errors
///
/// A [`PairFailure`] when the exchange fails for any reason; it says whether
/// the attempt counts as a spent guess at the code.
pub async fn pair<S>(
    stream: &mut S,
    params: PairParams<'_>,
    timeout: Duration,
) -> Result<(), PairFailure>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Tracked outside the exchange future so it survives the timeout.
    let spent = AtomicBool::new(false);
    let error = match tokio::time::timeout(timeout, exchange(stream, params, &spent)).await {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(error)) => error,
        Err(_) => PairError::Timeout,
    };
    Err(PairFailure {
        error,
        guess_spent: spent.load(Ordering::Relaxed),
    })
}

async fn exchange<S>(
    stream: &mut S,
    params: PairParams<'_>,
    spent: &AtomicBool,
) -> Result<(), PairError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (spake, ours) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(params.code.password()),
        &Identity::new(SPAKE_IDENTITY),
    );
    write_frame(
        stream,
        &PairFrame::Start {
            version: PROTOCOL_VERSION,
            role: params.role,
            spake: ours.clone(),
        },
    )
    .await?;

    let theirs = match read_frame(stream, MAX_FRAME).await? {
        PairFrame::Start {
            version,
            role,
            spake,
        } => {
            if version != PROTOCOL_VERSION {
                return Err(PairError::Version {
                    ours: PROTOCOL_VERSION,
                    peer: version,
                });
            }
            if role != params.role.peer() {
                return Err(PairError::Role {
                    ours: params.role,
                    peer: role,
                });
            }
            spake
        }
        _ => return Err(PairError::Protocol("expected Start")),
    };

    let key = spake
        .finish(&theirs)
        .map_err(|_| PairError::Protocol("malformed SPAKE2 message"))?;

    let transcript = Transcript::new(&params, &ours, &theirs);
    let confirm = PairFrame::Confirm {
        mac: transcript
            .mac(&key, params.role)?
            .finalize()
            .into_bytes()
            .into(),
    };
    let peer_mac = transcript.mac(&key, params.role.peer())?;
    // Constant-time comparison.
    let check_peer = |mac: &[u8; 32]| peer_mac.clone().verify_slice(mac).is_ok();

    match params.side {
        Side::TypedCode => {
            // From here on the peer can check a guess against our MAC.
            spent.store(true, Ordering::Relaxed);
            write_frame(stream, &confirm).await?;
            match read_frame(stream, MAX_FRAME).await? {
                PairFrame::Confirm { mac } if check_peer(&mac) => {
                    write_frame(stream, &PairFrame::Paired).await?;
                    Ok(())
                }
                PairFrame::Confirm { .. } => reject(stream).await,
                PairFrame::Mismatch => Err(PairError::Mismatch),
                _ => Err(PairError::Protocol("expected Confirm or Mismatch")),
            }
        }
        Side::OwnsCode => {
            match read_frame(stream, MAX_FRAME).await? {
                PairFrame::Confirm { mac } if check_peer(&mac) => {}
                PairFrame::Confirm { .. } => {
                    spent.store(true, Ordering::Relaxed);
                    return reject(stream).await;
                }
                _ => return Err(PairError::Protocol("expected Confirm")),
            }
            write_frame(stream, &confirm).await?;
            match read_frame(stream, MAX_FRAME).await? {
                PairFrame::Paired => Ok(()),
                PairFrame::Mismatch => Err(PairError::Mismatch),
                _ => Err(PairError::Protocol("expected Paired or Mismatch")),
            }
        }
    }
}

/// Tells the peer its confirmation was wrong (best effort, so it fails fast
/// instead of timing out) and fails.
async fn reject<S>(stream: &mut S) -> Result<(), PairError>
where
    S: AsyncWrite + Unpin,
{
    let _ = write_frame(stream, &PairFrame::Mismatch).await;
    Err(PairError::Mismatch)
}

/// Everything the confirmation MACs cover, in a role-independent order.
struct Transcript<'a> {
    controller_id: EndpointIdBytes,
    target_id: EndpointIdBytes,
    controller_spake: &'a [u8],
    target_spake: &'a [u8],
}

impl<'a> Transcript<'a> {
    fn new(params: &PairParams<'_>, ours: &'a [u8], theirs: &'a [u8]) -> Self {
        match params.role {
            Role::Controller => Self {
                controller_id: params.local_id,
                target_id: params.remote_id,
                controller_spake: ours,
                target_spake: theirs,
            },
            Role::Target => Self {
                controller_id: params.remote_id,
                target_id: params.local_id,
                controller_spake: theirs,
                target_spake: ours,
            },
        }
    }

    /// The confirmation MAC that `sender` sends. Including the sender's role
    /// stops an attacker from reflecting a side's own MAC back to it.
    fn mac(&self, key: &[u8], sender: Role) -> Result<HmacSha256, PairError> {
        // HMAC takes keys of any length; the error can't happen.
        let new = |key: &[u8]| {
            <HmacSha256 as KeyInit>::new_from_slice(key)
                .map_err(|_| PairError::Protocol("HMAC key rejected"))
        };
        let mut kdf = new(key)?;
        kdf.update(CONFIRM_KEY_LABEL);
        let confirm_key = kdf.finalize().into_bytes();

        let mut mac = new(&confirm_key)?;
        mac.update(&[sender.tag()]);
        mac.update(&PROTOCOL_VERSION.to_be_bytes());
        mac.update(&self.controller_id);
        mac.update(&self.target_id);
        for message in [self.controller_spake, self.target_spake] {
            let len = u32::try_from(message.len())
                .map_err(|_| PairError::Protocol("oversized SPAKE2 message"))?;
            mac.update(&len.to_be_bytes());
            mac.update(message);
        }
        Ok(mac)
    }
}
