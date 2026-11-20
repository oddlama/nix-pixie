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
const SPAKE_IDENTITY: &[u8] = b"nix-pixie/pair/1";
const CONFIRM_KEY_LABEL: &[u8] = b"nix-pixie/pair/1 confirm key";
/// Pairing frames are tiny; anything bigger is an attack or a bug.
const MAX_FRAME: usize = 1024;

type HmacSha256 = Hmac<Sha256>;

/// Raw ed25519 public key bytes of an iroh endpoint.
pub type EndpointIdBytes = [u8; 32];

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

#[derive(Debug, Serialize, Deserialize)]
enum PairFrame {
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
            .mac(&key, params.role)
            .finalize()
            .into_bytes()
            .into(),
    };
    let check_peer = |mac: &[u8; 32]| {
        transcript
            .mac(&key, params.role.peer())
            .verify_slice(mac)
            .is_ok()
    };

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
    fn mac(&self, key: &[u8], sender: Role) -> HmacSha256 {
        let mut kdf =
            <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC takes any key size");
        kdf.update(CONFIRM_KEY_LABEL);
        let confirm_key = kdf.finalize().into_bytes();

        let mut mac =
            <HmacSha256 as KeyInit>::new_from_slice(&confirm_key).expect("HMAC takes any key size");
        mac.update(&[sender.tag()]);
        mac.update(&PROTOCOL_VERSION.to_be_bytes());
        mac.update(&self.controller_id);
        mac.update(&self.target_id);
        for message in [self.controller_spake, self.target_spake] {
            let len = u32::try_from(message.len()).expect("SPAKE2 messages are tiny");
            mac.update(&len.to_be_bytes());
            mac.update(message);
        }
        mac
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    const CONTROLLER: EndpointIdBytes = [1; 32];
    const TARGET: EndpointIdBytes = [2; 32];
    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Target-shown code: the target owns it, the controller typed it.
    fn controller(code: &Code, remote_id: EndpointIdBytes) -> PairParams<'_> {
        PairParams {
            role: Role::Controller,
            side: Side::TypedCode,
            code,
            local_id: CONTROLLER,
            remote_id,
        }
    }

    fn target(code: &Code, remote_id: EndpointIdBytes) -> PairParams<'_> {
        PairParams {
            role: Role::Target,
            side: Side::OwnsCode,
            code,
            local_id: TARGET,
            remote_id,
        }
    }

    async fn run(
        pa: PairParams<'_>,
        pb: PairParams<'_>,
    ) -> (Result<(), PairFailure>, Result<(), PairFailure>) {
        let (mut a, mut b): (DuplexStream, DuplexStream) = tokio::io::duplex(4096);
        tokio::join!(pair(&mut a, pa, TIMEOUT), pair(&mut b, pb, TIMEOUT))
    }

    #[track_caller]
    fn assert_mismatch(result: &Result<(), PairFailure>, guess_spent: bool) {
        match result {
            Err(PairFailure {
                error: PairError::Mismatch,
                guess_spent: spent,
            }) => assert_eq!(*spent, guess_spent, "guess_spent"),
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    /// Sends a Start for `guess` in `role`; returns the peer's Start.
    async fn send_guess(stream: &mut DuplexStream, guess: &Code, role: Role) -> PairFrame {
        let (_, spake) = Spake2::<Ed25519Group>::start_symmetric(
            &Password::new(guess.password()),
            &Identity::new(SPAKE_IDENTITY),
        );
        let start = PairFrame::Start {
            version: PROTOCOL_VERSION,
            role,
            spake,
        };
        write_frame(stream, &start).await.unwrap();
        read_frame(stream, MAX_FRAME).await.unwrap()
    }

    #[tokio::test]
    async fn same_code_pairs() {
        let code = Code::generate(Some(42));
        let (c, t) = run(controller(&code, TARGET), target(&code, CONTROLLER)).await;
        c.unwrap();
        t.unwrap();
    }

    /// Controller-shown code: the controller owns it, the target typed it.
    #[tokio::test]
    async fn same_code_pairs_with_sides_swapped() {
        let code = Code::generate(None);
        let c = PairParams {
            side: Side::OwnsCode,
            ..controller(&code, TARGET)
        };
        let t = PairParams {
            side: Side::TypedCode,
            ..target(&code, CONTROLLER)
        };
        let (c, t) = run(c, t).await;
        c.unwrap();
        t.unwrap();
    }

    #[tokio::test]
    async fn different_code_fails_and_spends_a_guess_on_both_sides() {
        let code = Code::parse("acid-acorn-acre").unwrap();
        let other = Code::parse("acid-acorn-aging").unwrap();
        let (c, t) = run(controller(&code, TARGET), target(&other, CONTROLLER)).await;
        assert_mismatch(&c, true);
        assert_mismatch(&t, true);
    }

    /// An attacker with its own connection to each side relays every byte.
    /// Even with the right code on both honest sides, pairing must fail,
    /// because each side saw a different peer id.
    #[tokio::test]
    async fn relay_attack_fails_on_both_sides() {
        const ATTACKER_TO_TARGET: EndpointIdBytes = [3; 32];
        const ATTACKER_TO_CONTROLLER: EndpointIdBytes = [4; 32];
        let code = Code::generate(Some(7));

        let (mut controller_side, mut attacker_a) = tokio::io::duplex(4096);
        let (mut target_side, mut attacker_b) = tokio::io::duplex(4096);
        let relay = tokio::spawn(async move {
            let _ = tokio::io::copy_bidirectional(&mut attacker_a, &mut attacker_b).await;
        });

        let (c, t) = tokio::join!(
            pair(
                &mut controller_side,
                controller(&code, ATTACKER_TO_CONTROLLER),
                TIMEOUT
            ),
            pair(&mut target_side, target(&code, ATTACKER_TO_TARGET), TIMEOUT),
        );
        relay.abort();
        assert_mismatch(&c, true);
        assert_mismatch(&t, true);
    }

    /// A typist that sends a guess must not receive the owner's MAC (which
    /// would let it check the guess) without presenting a valid MAC first. It
    /// gets nothing while it withholds its confirmation, and a `Mismatch`
    /// (counted as a guess) once it sends a wrong one.
    #[tokio::test]
    async fn owner_reveals_nothing_to_a_guessing_typist() {
        let code = Code::generate(Some(9));
        let guess = Code::generate(Some(9));
        assert_ne!(code, guess);

        let (mut honest, mut attacker) = tokio::io::duplex(4096);
        let owner =
            tokio::spawn(
                async move { pair(&mut honest, target(&code, CONTROLLER), TIMEOUT).await },
            );

        let start = send_guess(&mut attacker, &guess, Role::Controller).await;
        assert!(matches!(start, PairFrame::Start { .. }), "{start:?}");
        let next = tokio::time::timeout(
            Duration::from_millis(200),
            read_frame::<_, PairFrame>(&mut attacker, MAX_FRAME),
        )
        .await;
        assert!(
            next.is_err(),
            "owner sent {next:?} before the typist confirmed"
        );

        write_frame(&mut attacker, &PairFrame::Confirm { mac: [0; 32] })
            .await
            .unwrap();
        let reply: PairFrame = read_frame(&mut attacker, MAX_FRAME).await.unwrap();
        assert!(matches!(reply, PairFrame::Mismatch), "{reply:?}");
        assert_mismatch(&owner.await.unwrap(), true);
    }

    /// Hanging up before confirming tells the attacker nothing, so the owner
    /// must not count it (otherwise anyone could force code rotations for free).
    #[tokio::test]
    async fn owner_does_not_count_a_hangup_before_confirmation() {
        let code = Code::generate(Some(9));
        let (mut honest, mut attacker) = tokio::io::duplex(4096);
        let owner =
            tokio::spawn(
                async move { pair(&mut honest, target(&code, CONTROLLER), TIMEOUT).await },
            );
        send_guess(&mut attacker, &Code::generate(Some(9)), Role::Controller).await;
        drop(attacker);
        let failure = owner.await.unwrap().unwrap_err();
        assert!(matches!(failure.error, PairError::Frame(_)), "{failure:?}");
        assert!(!failure.guess_spent);
    }

    /// A rogue owner sees the typist's MAC first and can check one guess,
    /// then hang up. The typist must count that.
    #[tokio::test]
    async fn typist_counts_a_hangup_after_its_confirmation() {
        let code = Code::generate(Some(9));
        let (mut honest, mut rogue_owner) = tokio::io::duplex(4096);
        let typist =
            tokio::spawn(
                async move { pair(&mut honest, controller(&code, TARGET), TIMEOUT).await },
            );
        send_guess(&mut rogue_owner, &Code::generate(Some(9)), Role::Target).await;
        let confirm: PairFrame = read_frame(&mut rogue_owner, MAX_FRAME).await.unwrap();
        assert!(matches!(confirm, PairFrame::Confirm { .. }), "{confirm:?}");
        drop(rogue_owner);
        let failure = typist.await.unwrap().unwrap_err();
        assert!(failure.guess_spent, "{failure:?}");
    }

    #[tokio::test]
    async fn stalled_peer_times_out_without_spending_a_guess() {
        let code = Code::generate(Some(9));
        let (mut honest, _silent) = tokio::io::duplex(4096);
        let failure = pair(
            &mut honest,
            target(&code, CONTROLLER),
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(matches!(failure.error, PairError::Timeout), "{failure:?}");
        assert!(!failure.guess_spent);
    }

    #[tokio::test]
    async fn two_controllers_refuse_each_other() {
        let code = Code::generate(None);
        let other_controller = PairParams {
            role: Role::Controller,
            ..target(&code, CONTROLLER)
        };
        let (c, t) = run(controller(&code, TARGET), other_controller).await;
        for result in [c, t] {
            let failure = result.unwrap_err();
            assert!(
                matches!(failure.error, PairError::Role { .. }),
                "{failure:?}"
            );
            assert!(!failure.guess_spent);
        }
    }
}
