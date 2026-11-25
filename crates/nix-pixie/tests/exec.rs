//! Tests for `nix_pixie::exec`: which programs hooks may run on the target.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::exec::*;

const SBCTL: &str = "/nix/store/0fpk8cxsmqddvb0nvljdnwki62fp95ra-sbctl-0.17";

#[test]
fn programs_are_files_in_the_store() {
    assert_eq!(store_path(&format!("{SBCTL}/bin/sbctl")), Ok(SBCTL));
    assert_eq!(
        store_path("/nix/store/0fpk8cxsmqddvb0nvljdnwki62fp95ra-script"),
        Err(
            "/nix/store/0fpk8cxsmqddvb0nvljdnwki62fp95ra-script: only programs in the Nix store \
             run on the target, e.g. ${pkgs.sbctl}/bin/sbctl, so they bring everything they need"
                .to_string()
        )
    );
    for refused in [
        "sbctl",
        "/usr/bin/sbctl",
        "/run/current-system/sw/bin/sbctl",
        "/nix/store/",
        "/nix/store//bin/x",
        &format!("{SBCTL}/bin/"),
        &format!("{SBCTL}/bin/../../../../bin/sh"),
        &format!("{SBCTL}/./bin/sbctl"),
        "/nix/store/0fpk8cxsmqddvb0nvljdnwki62fp95ra-a=b/bin/x",
        "/nix/storeX/0fpk8cxsmqddvb0nvljdnwki62fp95ra-x/bin/x",
    ] {
        assert!(store_path(refused).is_err(), "{refused}");
    }
}

#[test]
fn the_target_only_runs_what_it_was_given() {
    let missing = format!("{SBCTL}/bin/sbctl-that-is-not-there");
    let error = command(&missing, &[], None).unwrap_err();
    assert!(error.contains("isn't in the target's store"), "{error}");
    let error = command(&missing, &[], Some("/nonexistent")).unwrap_err();
    assert!(error.contains("no installed system"), "{error}");
    assert!(command("/bin/sh", &[], None).is_err());
}

#[tokio::test]
async fn programs_get_a_clean_environment() {
    // A shell from the store, to look at its environment.
    let bash = std::process::Command::new("sh")
        .args(["-c", "readlink -f \"$(command -v bash)\""])
        .output()
        .unwrap();
    let bash = String::from_utf8(bash.stdout).unwrap();
    let bash = bash.trim();
    assert!(bash.starts_with("/nix/store/"), "{bash}");
    let script = r#"echo "$HOME $LANG $PATH ${USER:-no-user}""#;
    let out = command(bash, &["-c".into(), script.into()], None)
        .unwrap()
        .output()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "/root C.UTF-8 /var/empty no-user\n"
    );
}
