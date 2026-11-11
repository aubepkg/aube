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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::LookupIpStrategy;

/// How long hickory gets to answer before the OS resolver takes over.
/// Shorter than hickory's own 5s-per-attempt default so a dead nameserver
/// costs one short stall instead of several.
const HICKORY_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Default)]
struct State {
    /// Built lazily: construction needs a tokio runtime in some setups.
    hickory: OnceLock<Option<TokioResolver>>,
    /// Set once hickory times out; later lookups go straight to the OS.
    use_system: AtomicBool,
}

#[derive(Clone, Default)]
pub(super) struct FallbackResolver {
    state: Arc<State>,
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
    Ok(tokio::net::lookup_host((host, 0)).await?.collect())
}

impl Resolve for FallbackResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let state = self.state.clone();
        Box::pin(async move {
            let host = name.as_str();
            if !state.use_system.load(Ordering::Relaxed)
                && let Some(resolver) = state.hickory()
            {
                match tokio::time::timeout(HICKORY_DEADLINE, resolver.lookup_ip(host)).await {
                    Ok(Ok(lookup)) => {
                        let addrs: Vec<SocketAddr> = lookup
                            .iter()
                            .map(|ip: IpAddr| SocketAddr::new(ip, 0))
                            .collect();
                        if !addrs.is_empty() {
                            return Ok(Box::new(addrs.into_iter()) as Addrs);
                        }
                    }
                    Ok(Err(err)) => {
                        tracing::debug!("hickory-dns lookup for {host} failed: {err}");
                    }
                    Err(_) => {
                        if !state.use_system.swap(true, Ordering::Relaxed) {
                            tracing::warn!(
                                "hickory-dns lookup for {host} timed out; \
                                 using the system resolver instead (is the nameserver in \
                                 /etc/resolv.conf reachable?)"
                            );
                        }
                    }
                }
            }
            let addrs = system_lookup(host).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_through_system_resolver_once_hickory_is_disabled() {
        let resolver = FallbackResolver::default();
        resolver.state.use_system.store(true, Ordering::Relaxed);
        let name: Name = "localhost".parse().unwrap();
        let addrs: Vec<_> = resolver.resolve(name).await.unwrap().collect();
        assert!(addrs.iter().any(|a| a.ip().is_loopback()));
        assert!(
            resolver.state.hickory.get().is_none(),
            "hickory must not be built once the system resolver is selected"
        );
    }
}
