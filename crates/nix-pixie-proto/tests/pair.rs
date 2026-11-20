//! Tests for `nix_pixie_proto::pair`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::time::Duration;

use nix_pixie_proto::PROTOCOL_VERSION;
use nix_pixie_proto::code::Code;
use nix_pixie_proto::frame::{read_frame, write_frame};
use nix_pixie_proto::pair::*;
use spake2::{Ed25519Group, Identity, Password, Spake2};
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
    let code = Code::generate(Some(42)).unwrap();
    let (c, t) = run(controller(&code, TARGET), target(&code, CONTROLLER)).await;
    c.unwrap();
    t.unwrap();
}

/// Controller-shown code: the controller owns it, the target typed it.
#[tokio::test]
async fn same_code_pairs_with_sides_swapped() {
    let code = Code::generate(None).unwrap();
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
    let code = Code::generate(Some(7)).unwrap();

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
    let code = Code::generate(Some(9)).unwrap();
    let guess = Code::generate(Some(9)).unwrap();
    assert_ne!(code, guess);

    let (mut honest, mut attacker) = tokio::io::duplex(4096);
    let owner =
        tokio::spawn(async move { pair(&mut honest, target(&code, CONTROLLER), TIMEOUT).await });

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
    let code = Code::generate(Some(9)).unwrap();
    let (mut honest, mut attacker) = tokio::io::duplex(4096);
    let owner =
        tokio::spawn(async move { pair(&mut honest, target(&code, CONTROLLER), TIMEOUT).await });
    send_guess(
        &mut attacker,
        &Code::generate(Some(9)).unwrap(),
        Role::Controller,
    )
    .await;
    drop(attacker);
    let failure = owner.await.unwrap().unwrap_err();
    assert!(matches!(failure.error, PairError::Frame(_)), "{failure:?}");
    assert!(!failure.guess_spent);
}

/// A rogue owner sees the typist's MAC first and can check one guess,
/// then hang up. The typist must count that.
#[tokio::test]
async fn typist_counts_a_hangup_after_its_confirmation() {
    let code = Code::generate(Some(9)).unwrap();
    let (mut honest, mut rogue_owner) = tokio::io::duplex(4096);
    let typist =
        tokio::spawn(async move { pair(&mut honest, controller(&code, TARGET), TIMEOUT).await });
    send_guess(
        &mut rogue_owner,
        &Code::generate(Some(9)).unwrap(),
        Role::Target,
    )
    .await;
    let confirm: PairFrame = read_frame(&mut rogue_owner, MAX_FRAME).await.unwrap();
    assert!(matches!(confirm, PairFrame::Confirm { .. }), "{confirm:?}");
    drop(rogue_owner);
    let failure = typist.await.unwrap().unwrap_err();
    assert!(failure.guess_spent, "{failure:?}");
}

#[tokio::test]
async fn stalled_peer_times_out_without_spending_a_guess() {
    let code = Code::generate(Some(9)).unwrap();
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
    let code = Code::generate(None).unwrap();
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
