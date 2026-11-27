//! Tests for `nix_pixie_proto`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie_proto::ctrl::{ControllerMsg, TargetMsg};
use nix_pixie_proto::*;

#[test]
fn alpns_carry_the_protocol_version() {
    let suffix = format!("/{PROTOCOL_VERSION}");
    for alpn in [ALPN_CTRL, ALPN_RESUME, ALPN_STORE, ALPN_LOG] {
        let alpn = std::str::from_utf8(alpn).unwrap();
        assert!(alpn.starts_with("nix-pixie/"), "{alpn}");
        assert!(alpn.ends_with(&suffix), "{alpn}");
    }
}

/// The protocol version a controller's message came with. Exhaustive on
/// purpose: a new message doesn't compile here. Then bump
/// `PROTOCOL_VERSION` (and the ALPNs), say why at it, and add the message
/// here. A changed field is a new version too, which nothing here catches.
fn controller_msg_since(message: &ControllerMsg) -> u32 {
    match message {
        ControllerMsg::Approve { .. }
        | ControllerMsg::Reject { .. }
        | ControllerMsg::RequestFacterReport
        | ControllerMsg::PrepareStore { .. }
        | ControllerMsg::PrepareDisks { .. }
        | ControllerMsg::RunDisko { .. }
        | ControllerMsg::GenerateHostKey
        | ControllerMsg::Install { .. }
        | ControllerMsg::Finish { .. }
        | ControllerMsg::PlaceFiles { .. }
        | ControllerMsg::ProvideHostKey { .. }
        | ControllerMsg::RequestAddresses
        | ControllerMsg::Exec { .. }
        | ControllerMsg::Resume { .. } => 1,
        ControllerMsg::AddBootEntry { .. }
        | ControllerMsg::SetClock { .. }
        | ControllerMsg::OpenBulk { .. } => 2,
    }
}

/// As [`controller_msg_since`], for the target's.
fn target_msg_since(message: &TargetMsg) -> u32 {
    match message {
        TargetMsg::Hello { .. }
        | TargetMsg::Inventory(_)
        | TargetMsg::FacterReport { .. }
        | TargetMsg::FacterFailed { .. }
        | TargetMsg::StoreReady { .. }
        | TargetMsg::StoreRefused { .. }
        | TargetMsg::DisksReady
        | TargetMsg::DisksRefused { .. }
        | TargetMsg::Output { .. }
        | TargetMsg::DiskoDone
        | TargetMsg::DiskoFailed { .. }
        | TargetMsg::HostKey { .. }
        | TargetMsg::HostKeyFailed { .. }
        | TargetMsg::InstallDone
        | TargetMsg::InstallFailed { .. }
        | TargetMsg::Finished { .. }
        | TargetMsg::FinishFailed { .. }
        | TargetMsg::FilesPlaced
        | TargetMsg::FilesRefused { .. }
        | TargetMsg::Addresses { .. }
        | TargetMsg::ExecOutput { .. }
        | TargetMsg::ExecExited { .. }
        | TargetMsg::ExecFailed { .. }
        | TargetMsg::Resumed { .. } => 1,
        TargetMsg::Clock { .. }
        | TargetMsg::ClockFailed { .. }
        | TargetMsg::BootEntry { .. }
        | TargetMsg::BootEntryFailed { .. }
        | TargetMsg::Bulk { .. }
        | TargetMsg::BulkFailed { .. } => 2,
    }
}

#[test]
fn the_newest_messages_are_of_the_protocol_version() {
    let newest = controller_msg_since(&ControllerMsg::SetClock { unix_ms: 0 }).max(
        target_msg_since(&TargetMsg::BootEntryFailed {
            error: String::new(),
        }),
    );
    assert_eq!(
        newest, PROTOCOL_VERSION,
        "the newest messages are of version {newest}, but PROTOCOL_VERSION is {PROTOCOL_VERSION}"
    );
    assert_eq!(controller_msg_since(&ControllerMsg::GenerateHostKey), 1);
    assert_eq!(target_msg_since(&TargetMsg::DisksReady), 1);
}
