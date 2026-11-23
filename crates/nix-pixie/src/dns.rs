//! Looking up the names of relays with the system's resolver.
//!
//! iroh brings a resolver of its own, and it doesn't suit a machine that may
//! sit on a network without internet:
//!
//! - It sends every query to as many as three name servers at once, each in
//!   up to six datagrams from six sockets, and iroh asks it for every relay's
//!   name again and again while it probes the network. Where no name server
//!   answers, that adds up to several hundred open sockets every half minute.
//!   That is enough to reach the usual limit of 1024 open files, at which
//!   point whatever else the process opens just then fails.
//! - When the system's name servers don't answer it asks Cloudflare, Google
//!   and Quad9 instead, which nobody running an install has agreed to.
//! - It reads `/etc/resolv.conf` and `/etc/hosts` itself, so whatever else
//!   the system is set up to find names with (`/etc/nsswitch.conf`) doesn't
//!   count.
//!
//! So names are looked up as everything else on the machine looks them up,
//! with `getaddrinfo`, and each name by one lookup at a time however many ask
//! for it.
//!
//! iroh waits three seconds for an answer and then asks again. The system's
//! resolver can take longer than that to succeed, for example when the first
//! name server it tries is dead. So an answer is kept for a moment, for
//! whoever asks next.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use iroh::dns::{BoxIter, DnsError, DnsResolver, Resolver, TxtRecordData};
use n0_error::AnyError;
use tokio::sync::watch;

/// A blocking lookup of one name's addresses.
pub type Lookup = Arc<dyn Fn(&str) -> io::Result<Vec<IpAddr>> + Send + Sync>;

/// How long the addresses of a name are handed out without another lookup.
/// Long enough for iroh's next attempt after one it gave up on, and short
/// enough not to matter next to any record's lifetime.
pub const KEEP: Duration = Duration::from_secs(30);

/// What a lookup found, for everyone who waited for it.
type Answer = Result<Arc<[IpAddr]>, Arc<str>>;
/// Where the answer of a lookup arrives, and stays.
type Waiting = watch::Receiver<Option<Answered>>;
/// The future type iroh's [`Resolver`] returns.
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The answer of a finished lookup.
#[derive(Debug, Clone)]
struct Answered {
    /// What the lookup found.
    answer: Answer,
    /// When it finished.
    at: Instant,
}

/// The resolver every endpoint of this process shares.
static SHARED: LazyLock<DnsResolver> = LazyLock::new(|| DnsResolver::custom(SystemResolver::new()));

/// The resolver for iroh endpoints: the system's, see [`SystemResolver`].
pub fn resolver() -> DnsResolver {
    SHARED.clone()
}

/// A [`Resolver`] that asks the system's, with at most one lookup per name
/// under way.
///
/// Whoever asks for a name that is already being looked up waits for that
/// answer instead of starting another lookup, so the lookups under way are
/// bounded by the number of names (the relays'), not by how often iroh asks.
/// Addresses that were found are handed out again for a short while (see
/// [`KEEP`]); failures never are, so a name server that comes back is asked
/// at the next occasion.
///
/// Each lookup blocks a thread of its own until the system gives up, which
/// can take half a minute with a name server that doesn't answer. These are
/// plain threads rather than tokio's blocking ones, because the runtime would
/// wait for those before the process can exit.
#[derive(Clone)]
pub struct SystemResolver {
    /// How one name is looked up.
    lookup: Lookup,
    /// How long found addresses are handed out again.
    keep: Duration,
    /// The latest lookup of each name, under way or finished.
    lookups: Arc<Mutex<HashMap<String, Waiting>>>,
}

impl SystemResolver {
    /// A resolver that looks names up with `getaddrinfo`.
    pub fn new() -> Self {
        Self::with_lookup(Arc::new(getaddrinfo), KEEP)
    }

    /// A resolver that looks names up with `lookup`, which may block, and
    /// hands out what it found for `keep`.
    pub fn with_lookup(lookup: Lookup, keep: Duration) -> Self {
        Self {
            lookup,
            keep,
            lookups: Arc::default(),
        }
    }

    /// The addresses of `host`, of both families.
    async fn addresses(&self, host: &str) -> Result<Arc<[IpAddr]>, DnsError> {
        let mut waiting = self.join(host);
        let answer = waiting
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|answered| answered.as_ref().map(|answered| answered.answer.clone()))
            .unwrap_or_else(|| Err("the lookup was abandoned".into()));
        answer.map_err(|reason| failed(host, &reason))
    }

    /// Joins the lookup of `host` that is under way or has just found
    /// something, or starts one.
    fn join(&self, host: &str) -> Waiting {
        let mut lookups = lock(&self.lookups);
        if let Some(waiting) = lookups.get(host)
            && self.is_usable(waiting)
        {
            return waiting.clone();
        }

        let (answer, waiting) = watch::channel(None);
        let thread = {
            let answer = answer.clone();
            let lookup = Arc::clone(&self.lookup);
            let host = host.to_owned();
            move || {
                let found = lookup(&host)
                    .map(Arc::from)
                    .map_err(|err| Arc::from(err.to_string()));
                answer.send_replace(Some(Answered {
                    answer: found,
                    at: Instant::now(),
                }));
            }
        };
        match std::thread::Builder::new()
            .name("name-lookup".to_owned())
            .spawn(thread)
        {
            Ok(_) => {
                lookups.insert(host.to_owned(), waiting.clone());
            }
            Err(err) => {
                answer.send_replace(Some(Answered {
                    answer: Err(format!("starting a thread: {err}").into()),
                    at: Instant::now(),
                }));
            }
        }
        waiting
    }

    /// Whether to wait for this lookup, or take what it found, instead of
    /// starting another.
    fn is_usable(&self, lookup: &Waiting) -> bool {
        let answered = lookup.borrow().clone();
        match answered {
            // Under way, unless it died without an answer.
            None => lookup.has_changed().is_ok(),
            Some(Answered { answer, at }) => answer.is_ok() && at.elapsed() < self.keep,
        }
    }
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SystemResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemResolver")
            .field("keep", &self.keep)
            .field("lookups", &lock(&self.lookups).len())
            .finish_non_exhaustive()
    }
}

impl Resolver for SystemResolver {
    fn lookup_ipv4(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv4Addr>, DnsError>> {
        let this = self.clone();
        Box::pin(async move {
            let found: Vec<Ipv4Addr> = this
                .addresses(&host)
                .await?
                .iter()
                .filter_map(|addr| match addr {
                    IpAddr::V4(addr) => Some(*addr),
                    IpAddr::V6(_) => None,
                })
                .collect();
            let found: BoxIter<Ipv4Addr> = Box::new(found.into_iter());
            Ok(found)
        })
    }

    fn lookup_ipv6(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv6Addr>, DnsError>> {
        let this = self.clone();
        Box::pin(async move {
            let found: Vec<Ipv6Addr> = this
                .addresses(&host)
                .await?
                .iter()
                .filter_map(|addr| match addr {
                    IpAddr::V4(_) => None,
                    IpAddr::V6(addr) => Some(*addr),
                })
                .collect();
            let found: BoxIter<Ipv6Addr> = Box::new(found.into_iter());
            Ok(found)
        })
    }

    /// Fails: only iroh's address lookup services ask for these, and none of
    /// them is used.
    fn lookup_txt(&self, host: String) -> BoxFuture<Result<BoxIter<TxtRecordData>, DnsError>> {
        Box::pin(async move { Err(failed(&host, "TXT records are not looked up")) })
    }

    fn clear_cache(&self) {
        lock(&self.lookups).retain(|_, lookup| lookup.borrow().is_none());
    }

    /// After a network change: what was found may not hold on the new
    /// network, and the lookups under way may be waiting for name servers of
    /// the old one, so nobody joins them.
    fn reset(&self) -> Box<dyn Resolver> {
        Box::new(Self::with_lookup(Arc::clone(&self.lookup), self.keep))
    }
}

/// Looks `host` up as the C library does: `/etc/nsswitch.conf`, `/etc/hosts`
/// and the name servers of `/etc/resolv.conf`.
fn getaddrinfo(host: &str) -> io::Result<Vec<IpAddr>> {
    Ok((host, 0).to_socket_addrs()?.map(|addr| addr.ip()).collect())
}

/// The error for a lookup of `host` that failed because of `reason`.
fn failed(host: &str, reason: &str) -> DnsError {
    DnsError::from(AnyError::from_string(format!(
        "looking up {host}: {reason}"
    )))
}

/// Locks `mutex`. A panic elsewhere doesn't leave the map in a state worth
/// refusing: every entry is whole.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
