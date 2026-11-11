//! DNS resolver that prefers in-process hickory lookups but falls back to
//! the OS resolver (`getaddrinfo`).
//!
//! Hickory reads the nameserver list from `/etc/resolv.conf`. On macOS that
//! file is informational only: the real resolver configuration lives in
//! SystemConfiguration, and VPN / split-DNS setups routinely leave a
//! nameserver there that only the OS's scoped routing can reach. Hickory
//! then times out on every lookup while every other tool resolves fine.
//! Retrying the same dead nameserver cannot help, so after a timeout this
//! resolver stops consulting hickory for the rest of the process.

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;
use tokio::sync::Semaphore;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::LookupIpStrategy;

/// Upper bound on how long hickory gets to answer before the OS resolver
/// takes over. Shorter than hickory's own 5s-per-attempt default so a dead
/// nameserver costs one short stall instead of several.
const MAX_HICKORY_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Default)]
struct State {
    /// Built lazily, per client, so each new client picks up the current
    /// nameserver list (VPNs come and go during a long-lived process).
    hickory: OnceLock<Option<TokioResolver>>,
    /// Set once hickory times out; later lookups go straight to the OS.
    /// Shared by every client in the process, so a dead nameserver is
    /// discovered once rather than once per metadata / tarball /
    /// per-registry client.
    use_system: Arc<AtomicBool>,
}

static USE_SYSTEM: LazyLock<Arc<AtomicBool>> = LazyLock::new(Arc::default);

/// `getaddrinfo` cannot be cancelled, so a stalled lookup keeps its
/// blocking thread. Cap how many may be in flight so they can never
/// exhaust the blocking pool that install work shares.
const MAX_SYSTEM_LOOKUPS: usize = 8;
static SYSTEM_LOOKUPS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_SYSTEM_LOOKUPS)));

#[derive(Clone)]
pub(super) struct FallbackResolver {
    state: Arc<State>,
    deadline: Duration,
}

impl FallbackResolver {
    /// `fetch_timeout` is the client's whole-request timeout. Hickory gets
    /// at most half of it so the system fallback still has time to answer
    /// before the request itself is cut off.
    pub(super) fn shared(fetch_timeout: Duration) -> Self {
        Self {
            state: Arc::new(State {
                use_system: USE_SYSTEM.clone(),
                ..State::default()
            }),
            deadline: MAX_HICKORY_DEADLINE.min(fetch_timeout / 2),
        }
    }
}

impl State {
    fn hickory(&self) -> Option<&TokioResolver> {
        self.hickory
            .get_or_init(|| {
                let mut builder = match TokioResolver::builder_tokio() {
                    Ok(builder) => builder,
                    Err(err) => {
                        tracing::debug!("hickory-dns: system DNS config unavailable: {err}");
                        return None;
                    }
                };
                // Look up both families so connect can happy-eyeball.
                builder.options_mut().ip_strategy = LookupIpStrategy::Ipv6AndIpv4;
                builder
                    .build()
                    .inspect_err(|err| tracing::debug!("hickory-dns: build failed: {err}"))
                    .ok()
            })
            .as_ref()
    }
}

async fn system_lookup(host: &str) -> Result<Vec<SocketAddr>, std::io::Error> {
    let permit = SYSTEM_LOOKUPS
        .clone()
        .acquire_owned()
        .await
        .map_err(std::io::Error::other)?;
    let host = host.to_owned();
    // The permit moves into the blocking closure, so the slot stays taken
    // until `getaddrinfo` actually returns, even if this future is
    // dropped by a request timeout.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), 0)).map(Iterator::collect)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Try `primary` within `deadline`, otherwise use `fallback`. A timeout
/// (not an ordinary error) flips `use_system` for the rest of the process.
async fn resolve_with<E: std::fmt::Display>(
    state: &State,
    host: &str,
    deadline: Duration,
    primary: Option<impl Future<Output = Result<Vec<IpAddr>, E>>>,
    fallback: impl Future<Output = Result<Vec<SocketAddr>, std::io::Error>>,
) -> Result<Vec<SocketAddr>, std::io::Error> {
    if !state.use_system.load(Ordering::Relaxed)
        && let Some(primary) = primary
    {
        match tokio::time::timeout(deadline, primary).await {
            Ok(Ok(ips)) if !ips.is_empty() => {
                return Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect());
            }
            Ok(Ok(_)) => {}
            Ok(Err(err)) => tracing::debug!("hickory-dns lookup for {host} failed: {err}"),
            Err(_) => {
                if !state.use_system.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_DNS_HICKORY_TIMEOUT,
                        "hickory-dns lookup for {host} timed out; using the system resolver \
                         instead (is the nameserver in /etc/resolv.conf reachable?)"
                    );
                }
            }
        }
    }
    fallback.await
}

impl Resolve for FallbackResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let (state, deadline) = (self.state.clone(), self.deadline);
        Box::pin(async move {
            let host = name.as_str();
            let primary = state.hickory().map(|resolver| async move {
                resolver
                    .lookup_ip(host)
                    .await
                    .map(|lookup| lookup.iter().collect::<Vec<_>>())
            });
            let addrs = resolve_with(&state, host, deadline, primary, system_lookup(host)).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const SYSTEM: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 0);

    async fn system_stub() -> Result<Vec<SocketAddr>, std::io::Error> {
        Ok(vec![SYSTEM])
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_switches_future_lookups_to_the_system_resolver() {
        let state = State::default();
        let hang = std::future::pending::<Result<Vec<IpAddr>, String>>();
        let got = resolve_with(
            &state,
            "h",
            Duration::from_secs(3),
            Some(hang),
            system_stub(),
        )
        .await
        .unwrap();
        assert_eq!(got, vec![SYSTEM]);
        assert!(state.use_system.load(Ordering::Relaxed));

        // Hickory is no longer consulted, even though it would answer.
        let ok = async { Ok::<_, String>(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]) };
        let got = resolve_with(&state, "h", Duration::from_secs(3), Some(ok), system_stub())
            .await
            .unwrap();
        assert_eq!(got, vec![SYSTEM]);
    }

    #[tokio::test]
    async fn ordinary_error_falls_back_without_disabling_hickory() {
        let state = State::default();
        let fail = async { Err::<Vec<IpAddr>, _>("nxdomain".to_string()) };
        let got = resolve_with(
            &state,
            "h",
            Duration::from_secs(3),
            Some(fail),
            system_stub(),
        )
        .await
        .unwrap();
        assert_eq!(got, vec![SYSTEM]);
        assert!(!state.use_system.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn hickory_answer_is_used_when_it_succeeds() {
        let state = State::default();
        let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        let ok = async move { Ok::<_, String>(vec![ip]) };
        let got = resolve_with(&state, "h", Duration::from_secs(3), Some(ok), system_stub())
            .await
            .unwrap();
        assert_eq!(got, vec![SocketAddr::new(ip, 0)]);
    }

    #[tokio::test]
    async fn system_lookup_resolves_localhost() {
        let addrs = system_lookup("localhost").await.unwrap();
        assert!(addrs.iter().any(|a| a.ip().is_loopback()));
    }

    #[test]
    fn deadline_leaves_room_inside_a_short_fetch_timeout() {
        let r = FallbackResolver::shared(Duration::from_secs(2));
        assert_eq!(r.deadline, Duration::from_secs(1));
        let r = FallbackResolver::shared(Duration::from_secs(300));
        assert_eq!(r.deadline, MAX_HICKORY_DEADLINE);
    }
}
