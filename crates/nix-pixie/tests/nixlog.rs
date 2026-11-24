//! Tests for `nix_pixie::nixlog`: reading Nix's `internal-json` log, with
//! lines as Nix 2.34 writes them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::nixlog::*;

fn feed(progress: &mut Progress, lines: &str) {
    for line in lines.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match parse(line) {
            Line::Event(event) => progress.feed(&event),
            Line::Text(text) => panic!("not an event: {text}"),
        }
    }
}

#[test]
fn parses_events_and_passes_other_lines_on() {
    assert_eq!(
        parse(r#"@nix {"action":"msg","level":1,"msg":"warning: Git tree '/src' is dirty"}"#),
        Line::Event(Event::Msg {
            level: 1,
            msg: "warning: Git tree '/src' is dirty".into()
        })
    );
    assert_eq!(
        parse(r#"@nix {"action":"stop","id":7}"#),
        Line::Event(Event::Stop { id: 7 })
    );
    assert_eq!(
        parse(r#"@nix {"action":"setPhase","id":7}"#),
        Line::Event(Event::Other)
    );
    for text in ["building on remote host", "@nix not json", ""] {
        assert_eq!(parse(text), Line::Text(text.into()));
    }
}

#[test]
fn counts_builds_like_nix_does() {
    let mut progress = Progress::default();
    feed(
        &mut progress,
        r#"
        @nix {"action":"start","id":1,"level":4,"parent":0,"text":"evaluating derivation 'path:/src#b'","type":0}
        "#,
    );
    assert_eq!(progress.summary(), "evaluating");
    feed(
        &mut progress,
        r#"
        @nix {"action":"stop","id":1}
        @nix {"action":"start","id":10,"level":0,"parent":0,"text":"","type":104}
        @nix {"action":"start","id":11,"level":0,"parent":0,"text":"","type":103}
        @nix {"action":"result","fields":[0,2,0,0],"id":10,"type":105}
        @nix {"action":"start","fields":["/nix/store/cq10yr2nwjqlzzx9ysqhdy244qjq7fsh-a.drv","",1,1],"id":12,"level":3,"parent":0,"text":"building '/nix/store/cq10yr2nwjqlzzx9ysqhdy244qjq7fsh-a.drv'","type":105}
        @nix {"action":"result","fields":[0,2,1,0],"id":10,"type":105}
        @nix {"action":"result","fields":["hello from a"],"id":12,"type":101}
        "#,
    );
    assert_eq!(progress.summary(), "0/2 built · building a");
    feed(
        &mut progress,
        r#"
        @nix {"action":"result","fields":[1,2,0,0],"id":10,"type":105}
        @nix {"action":"stop","id":12}
        "#,
    );
    assert_eq!(progress.summary(), "1/2 built");
    feed(
        &mut progress,
        r#"
        @nix {"action":"result","fields":[2,2,0,0],"id":10,"type":105}
        @nix {"action":"stop","id":11}
        @nix {"action":"stop","id":10}
        "#,
    );
    assert_eq!(progress.summary(), "2/2 built");
    assert_eq!(progress.outcome(), "2 built");
}

#[test]
fn counts_copied_paths_and_bytes() {
    let mut progress = Progress::default();
    feed(
        &mut progress,
        r#"
        @nix {"action":"start","id":5,"level":3,"parent":0,"text":"copying 2 paths","type":103}
        @nix {"action":"result","fields":[100,3145728],"id":5,"type":106}
        @nix {"action":"result","fields":[0,2,1,0],"id":5,"type":105}
        @nix {"action":"start","fields":["/nix/store/bx7jdvkba26sy20ryr4l617qqmzj9hff-libunistring-1.4.2","daemon","unix:///tmp/s"],"id":6,"level":3,"parent":0,"text":"copying path '/nix/store/bx7jdvkba26sy20ryr4l617qqmzj9hff-libunistring-1.4.2' to 'unix:///tmp/s'","type":100}
        @nix {"action":"result","fields":[1048576,2097152,0,0],"id":6,"type":105}
        "#,
    );
    assert_eq!(
        progress.summary(),
        "0/2 copied · 1.0 MiB/3.0 MiB · copying libunistring-1.4.2"
    );
    feed(
        &mut progress,
        r#"
        @nix {"action":"result","fields":[2097152,2097152,0,0],"id":6,"type":105}
        @nix {"action":"stop","id":6}
        @nix {"action":"result","fields":[1,2,1,0],"id":5,"type":105}
        @nix {"action":"start","fields":["/nix/store/pqyiidz5mi73wv5f91d48s9plcqyj5bz-glibc-2.44-57","https://cache.nixos.org"],"id":7,"level":3,"parent":0,"text":"copying path '/nix/store/pqyiidz5mi73wv5f91d48s9plcqyj5bz-glibc-2.44-57' from 'https://cache.nixos.org'","type":100}
        "#,
    );
    assert_eq!(
        progress.summary(),
        "1/2 copied · 2.0 MiB/3.0 MiB · fetching glibc-2.44-57"
    );
    feed(
        &mut progress,
        r#"
        @nix {"action":"result","fields":[1048576,1048576,0,0],"id":7,"type":105}
        @nix {"action":"stop","id":7}
        @nix {"action":"result","fields":[2,2,0,0],"id":5,"type":105}
        @nix {"action":"stop","id":5}
        "#,
    );
    assert_eq!(progress.summary(), "2/2 copied · 3.0 MiB/3.0 MiB");
    assert_eq!(progress.outcome(), "2 copied · 3.0 MiB");
    assert_eq!(
        progress.copies(),
        Count {
            done: 2,
            expected: 2,
            running: 0,
            failed: 0
        }
    );
}

#[test]
fn nothing_to_do_says_nothing() {
    let mut progress = Progress::default();
    feed(
        &mut progress,
        r#"
        @nix {"action":"start","id":1,"level":0,"parent":0,"text":"","type":104}
        @nix {"action":"start","id":2,"level":6,"parent":0,"text":"querying info about missing paths","type":0}
        @nix {"action":"stop","id":2}
        @nix {"action":"stop","id":1}
        @nix {"action":"stop","id":99}
        @nix {"action":"result","fields":[1,1,0,0],"id":99,"type":105}
        "#,
    );
    assert_eq!(progress.summary(), "");
    assert_eq!(progress.outcome(), "");
}

#[test]
fn shortens_store_paths() {
    assert_eq!(
        store_name("/nix/store/cq10yr2nwjqlzzx9ysqhdy244qjq7fsh-linux-6.12.drv"),
        "linux-6.12"
    );
    assert_eq!(store_name("plain"), "plain");
    assert_eq!(
        shorten_store_paths(
            "building '/nix/store/cq10yr2nwjqlzzx9ysqhdy244qjq7fsh-a.drv' and /nix/store/aaaabbbbccccddddeeeeffffgggghhhh-b"
        ),
        "building 'a' and b"
    );
}

#[test]
fn strips_escape_sequences() {
    assert_eq!(
        strip_ansi("\x1b[31;1merror:\x1b[0m builder for '\x1b[35;1ma\x1b[0m' failed"),
        "error: builder for 'a' failed"
    );
}
