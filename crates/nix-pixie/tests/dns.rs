//! Tests for `nix_pixie::dns`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::dns::DnsResolver;
use nix_pixie::dns::{KEEP, Lookup, SystemResolver};

const TIMEOUT: Duration = Duration::from_secs(5);
const V4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 7);
const V6: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 7);

/// A lookup that counts how often it is asked and answers only when told to.
struct Gate {
    asked: Arc<AtomicUsize>,
    release: Sender<io::Result<Vec<IpAddr>>>,
}

impl Gate {
    fn new() -> (Self, Lookup) {
        let asked = Arc::new(AtomicUsize::new(0));
        let (release, answers): (_, Receiver<io::Result<Vec<IpAddr>>>) = channel();
        let answers = Mutex::new(answers);
        let lookup: Lookup = {
            let asked = Arc::clone(&asked);
            Arc::new(move |_host: &str| {
                asked.fetch_add(1, Ordering::SeqCst);
                let answer = answers.lock().unwrap().recv();
                answer.unwrap_or_else(|_| Err(io::Error::other("the test is over")))
            })
        };
        (Self { asked, release }, lookup)
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }

    /// Waits until `count` lookups have been started.
    async fn wait_for(&self, count: usize) {
        tokio::time::timeout(TIMEOUT, async {
            while self.asked() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the lookups never started");
    }

    fn answer(&self, answer: io::Result<Vec<IpAddr>>) {
        self.release.send(answer).unwrap();
    }
}

#[tokio::test]
async fn addresses_need_no_name_server() {
    let resolver = DnsResolver::custom(SystemResolver::new());
    let v4: Vec<_> = resolver
        .lookup_ipv4(V4.to_string(), TIMEOUT)
        .await
        .unwrap()
        .collect();
    assert_eq!(v4, [IpAddr::V4(V4)]);
    let v6: Vec<_> = resolver
        .lookup_ipv6(V6.to_string(), TIMEOUT)
        .await
        .unwrap()
        .collect();
    assert_eq!(v6, [IpAddr::V6(V6)]);
}

/// A resolver that keeps nothing, so every answer is a lookup's.
fn forgetful(lookup: Lookup) -> DnsResolver {
    DnsResolver::custom(SystemResolver::with_lookup(lookup, Duration::ZERO))
}

#[tokio::test]
async fn each_family_gets_its_own_addresses() {
    let (gate, lookup) = Gate::new();
    let resolver = forgetful(lookup);
    gate.answer(Ok(vec![V6.into(), V4.into()]));
    gate.answer(Ok(vec![V6.into(), V4.into()]));
    gate.answer(Ok(vec![V4.into()]));

    let v4: Vec<_> = resolver
        .lookup_ipv4("relay.example", TIMEOUT)
        .await
        .unwrap()
        .collect();
    assert_eq!(v4, [IpAddr::V4(V4)]);
    let v6: Vec<_> = resolver
        .lookup_ipv6("relay.example", TIMEOUT)
        .await
        .unwrap()
        .collect();
    assert_eq!(v6, [IpAddr::V6(V6)]);
    // A name without addresses of a family is not an error.
    let none = resolver
        .lookup_ipv6("relay.example", TIMEOUT)
        .await
        .unwrap()
        .count();
    assert_eq!(none, 0);
}

#[tokio::test]
async fn one_lookup_per_name_however_many_ask() {
    let (gate, lookup) = Gate::new();
    let resolver = forgetful(lookup);

    let mut asking = tokio::task::JoinSet::new();
    for n in 0..50 {
        let resolver = resolver.clone();
        asking.spawn(async move {
            if n % 2 == 0 {
                resolver
                    .lookup_ipv4("relay.example", TIMEOUT)
                    .await
                    .map(Iterator::count)
            } else {
                resolver
                    .lookup_ipv6("relay.example", TIMEOUT)
                    .await
                    .map(Iterator::count)
            }
        });
    }
    // Every task has asked by the time the first lookup is seen: they all run
    // on this thread, before this one continues.
    gate.wait_for(1).await;
    tokio::task::yield_now().await;
    assert_eq!(gate.asked(), 1);

    gate.answer(Ok(vec![V4.into(), V6.into()]));
    while let Some(found) = asking.join_next().await {
        assert_eq!(found.unwrap().unwrap(), 1);
    }
    assert_eq!(gate.asked(), 1);

    // Nothing is kept here: asking again looks the name up again.
    gate.answer(Ok(vec![V4.into()]));
    let again = resolver
        .lookup_ipv4("relay.example", TIMEOUT)
        .await
        .unwrap()
        .count();
    assert_eq!(again, 1);
    assert_eq!(gate.asked(), 2);
}

#[tokio::test]
async fn giving_up_on_a_lookup_does_not_start_another() {
    let (gate, lookup) = Gate::new();
    let resolver = forgetful(lookup);

    // iroh gives up after a few seconds and asks again; the name server may
    // take far longer to not answer.
    for _ in 0..5 {
        let gave_up = resolver
            .lookup_ipv4("relay.example", Duration::from_millis(20))
            .await;
        assert!(gave_up.is_err());
    }
    assert_eq!(gate.asked(), 1);

    gate.answer(Ok(vec![V4.into()]));
    gate.answer(Ok(vec![V4.into()]));
    let found = resolver
        .lookup_ipv4("relay.example", TIMEOUT)
        .await
        .unwrap()
        .count();
    assert_eq!(found, 1);
}

#[tokio::test]
async fn an_answer_nobody_waited_for_reaches_whoever_asks_next() {
    let (gate, lookup) = Gate::new();
    let resolver = DnsResolver::custom(SystemResolver::with_lookup(lookup, KEEP));

    // The system's resolver takes longer than iroh waits.
    let gave_up = resolver
        .lookup_ipv4("relay.example", Duration::from_millis(20))
        .await;
    assert!(gave_up.is_err());
    gate.answer(Ok(vec![V4.into(), V6.into()]));

    // Nothing answers a second lookup, so these are the first one's.
    let v4: Vec<_> = resolver
        .lookup_ipv4("relay.example", TIMEOUT)
        .await
        .unwrap()
        .collect();
    assert_eq!(v4, [IpAddr::V4(V4)]);
    let v6: Vec<_> = resolver
        .lookup_ipv6("relay.example", TIMEOUT)
        .await
        .unwrap()
        .collect();
    assert_eq!(v6, [IpAddr::V6(V6)]);
    assert_eq!(gate.asked(), 1);

    // Until the cache is cleared.
    resolver.clear_cache();
    gate.answer(Ok(vec![V4.into()]));
    let again = resolver
        .lookup_ipv6("relay.example", TIMEOUT)
        .await
        .unwrap()
        .count();
    assert_eq!(again, 0);
    assert_eq!(gate.asked(), 2);
}

#[tokio::test]
async fn failures_are_not_kept() {
    let (gate, lookup) = Gate::new();
    let resolver = DnsResolver::custom(SystemResolver::with_lookup(lookup, KEEP));

    gate.answer(Err(io::Error::other("no name server answered")));
    assert!(
        resolver
            .lookup_ipv4("relay.example", TIMEOUT)
            .await
            .is_err()
    );

    // The name server is back.
    gate.answer(Ok(vec![V4.into()]));
    let found = resolver
        .lookup_ipv4("relay.example", TIMEOUT)
        .await
        .unwrap()
        .count();
    assert_eq!(found, 1);
    assert_eq!(gate.asked(), 2);
}

#[tokio::test]
async fn names_are_looked_up_side_by_side() {
    let (gate, lookup) = Gate::new();
    let resolver = forgetful(lookup);

    let mut asking = tokio::task::JoinSet::new();
    for host in ["a.example", "b.example", "c.example"] {
        let resolver = resolver.clone();
        asking.spawn(async move {
            resolver
                .lookup_ipv4(host, TIMEOUT)
                .await
                .map(Iterator::count)
        });
    }
    gate.wait_for(3).await;
    for _ in 0..3 {
        gate.answer(Ok(vec![V4.into()]));
    }
    while let Some(found) = asking.join_next().await {
        assert_eq!(found.unwrap().unwrap(), 1);
    }
}

#[tokio::test]
async fn a_failed_lookup_fails_for_everyone_and_says_why() {
    let (gate, lookup) = Gate::new();
    let resolver = forgetful(lookup);

    let other = {
        let resolver = resolver.clone();
        tokio::spawn(async move {
            resolver
                .lookup_ipv6("relay.example", TIMEOUT)
                .await
                .map(Iterator::count)
        })
    };
    gate.wait_for(1).await;
    let asking = {
        let resolver = resolver.clone();
        tokio::spawn(async move {
            resolver
                .lookup_ipv4("relay.example", TIMEOUT)
                .await
                .map(Iterator::count)
        })
    };
    // Lets the second one ask before there is an answer.
    tokio::task::yield_now().await;
    gate.answer(Err(io::Error::other("no name server answered")));

    for result in [other.await.unwrap(), asking.await.unwrap()] {
        let err = format!("{:#}", anyhow::Error::from(result.unwrap_err()));
        assert!(err.contains("relay.example"), "{err}");
        assert!(err.contains("no name server answered"), "{err}");
    }
    assert_eq!(gate.asked(), 1);
}

#[tokio::test]
async fn after_a_network_change_nobody_waits_for_an_old_lookup() {
    // The first lookup waits for a name server that never answers, the ones
    // after it are answered at once.
    let asked = Arc::new(AtomicUsize::new(0));
    let (_never, old_network) = channel::<()>();
    let old_network = Mutex::new(old_network);
    let lookup: Lookup = {
        let asked = Arc::clone(&asked);
        Arc::new(move |_host: &str| {
            if asked.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = old_network.lock().unwrap().recv();
                return Err(io::Error::other("the test is over"));
            }
            Ok(vec![V4.into()])
        })
    };
    let resolver = forgetful(lookup);

    for _ in 0..2 {
        let stuck = resolver
            .lookup_ipv4("relay.example", Duration::from_millis(20))
            .await;
        assert!(stuck.is_err());
    }
    // The lookup runs on a thread of its own, which takes a moment to start
    // on a busy machine. Both attempts share it.
    tokio::time::timeout(TIMEOUT, async {
        while asked.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the lookup never started");
    assert_eq!(asked.load(Ordering::SeqCst), 1);

    resolver.reset();
    let found = resolver
        .lookup_ipv4("relay.example", TIMEOUT)
        .await
        .unwrap()
        .count();
    assert_eq!(found, 1);
    assert_eq!(asked.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn txt_records_are_not_looked_up() {
    let resolver = DnsResolver::custom(SystemResolver::new());
    assert!(resolver.lookup_txt("relay.example", TIMEOUT).await.is_err());
}
