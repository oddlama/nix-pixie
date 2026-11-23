//! Tests for `nix_pixie::hooks`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::collections::BTreeMap;

use nix_pixie::hooks::{Step, schedule};
use nix_pixie::metadata::Hook;

fn hook(after: &[&str], before: &[&str]) -> Hook {
    Hook {
        command: "x".into(),
        args: Vec::new(),
        after: after.iter().map(|s| (*s).to_string()).collect(),
        before: before.iter().map(|s| (*s).to_string()).collect(),
    }
}

fn hooks(list: &[(&str, Hook)]) -> BTreeMap<String, Hook> {
    list.iter()
        .map(|(n, h)| ((*n).to_string(), h.clone()))
        .collect()
}

fn order(list: &[(&str, Hook)]) -> Vec<(Step, String)> {
    schedule(&hooks(list))
        .unwrap()
        .all()
        .map(|(step, hook)| (step, hook.to_string()))
        .collect()
}

fn refusal(list: &[(&str, Hook)]) -> String {
    format!("{:#}", schedule(&hooks(list)).unwrap_err())
}

#[test]
fn hooks_run_right_after_their_last_dependency() {
    assert_eq!(order(&[]), []);
    assert_eq!(
        order(&[
            ("rekey", hook(&["host-key"], &["build"])),
            ("notify", hook(&["boot"], &[])),
            ("early", hook(&[], &["build"])),
        ]),
        [
            (Step::Approve, "early".to_string()),
            (Step::HostKey, "rekey".to_string()),
            (Step::Boot, "notify".to_string()),
        ]
    );
}

#[test]
fn hooks_can_order_each_other() {
    // `b` must wait for `a`, `c` must come before `a`: c, a, b. Same names
    // and constraints, alphabetical among the free.
    assert_eq!(
        order(&[
            ("a", hook(&["disks"], &["build"])),
            ("b", hook(&["a"], &[])),
            ("c", hook(&[], &["a"])),
        ]),
        [
            (Step::Approve, "c".to_string()),
            (Step::Disks, "a".to_string()),
            (Step::Disks, "b".to_string()),
        ]
    );
    let schedule = schedule(&hooks(&[
        ("z", hook(&["copy"], &[])),
        ("y", hook(&["copy"], &[])),
    ]))
    .unwrap();
    assert_eq!(schedule.after(Step::Copy), ["y", "z"]);
    assert_eq!(schedule.after(Step::Build), [] as [String; 0]);
}

#[test]
fn refuses_cycles_and_unknown_names() {
    let cycle = refusal(&[("a", hook(&["b"], &[])), ("b", hook(&["a"], &[]))]);
    assert!(
        cycle.contains("cycle") && cycle.contains("hook a"),
        "{cycle}"
    );
    // Through the steps: after the build, but before the disks are chosen.
    let backwards = refusal(&[("late", hook(&["build"], &["disks"]))]);
    assert!(backwards.contains("cycle"), "{backwards}");

    let unknown = refusal(&[("a", hook(&["biuld"], &[]))]);
    assert!(
        unknown.contains("`biuld`") && unknown.contains("host-key"),
        "{unknown}"
    );
    let clash = refusal(&[("build", hook(&[], &[]))]);
    assert!(clash.contains("named like a step"), "{clash}");
    let too_early = refusal(&[("a", hook(&[], &["approve"]))]);
    assert!(too_early.contains("before `approve`"), "{too_early}");
}

#[test]
fn commands_are_flake_outputs_unless_they_name_a_flake() {
    use nix_pixie::hooks::installable;
    assert_eq!(installable("/repo", "rekey"), "/repo#rekey");
    assert_eq!(installable("/repo", "hooks.notify"), "/repo#hooks.notify");
    assert_eq!(
        installable("/repo", "github:owner/repo#app"),
        "github:owner/repo#app"
    );
}

#[test]
fn collects_regular_files_with_their_modes() {
    use std::os::unix::fs::PermissionsExt;

    use nix_pixie::hooks::collect_files;

    let dir = std::env::temp_dir().join(format!("nix-pixie-test-hook-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let files = dir.join("files");
    let write = |path: &str, contents: &[u8], mode: u32| {
        let path = files.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    assert_eq!(collect_files(&files).unwrap(), []);
    write("etc/hook.conf", b"x", 0o644);
    // Sticky (setuid can't be created in the Nix build sandbox).
    write("var/lib/secrets/key", b"secret", 0o1600);

    let collected = collect_files(&files).unwrap();
    let summary: Vec<(&str, u32, &[u8])> = collected
        .iter()
        .map(|f| (f.path.as_str(), f.mode, f.contents.as_slice()))
        .collect();
    // Special bits are gone already here; the agent drops them as well.
    assert_eq!(
        summary,
        [
            ("/etc/hook.conf", 0o644, &b"x"[..]),
            ("/var/lib/secrets/key", 0o600, &b"secret"[..]),
        ]
    );

    std::os::unix::fs::symlink("/etc/shadow", files.join("etc/link")).unwrap();
    let err = format!("{:#}", collect_files(&files).unwrap_err());
    assert!(err.contains("only regular files"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
