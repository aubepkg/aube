//! HTTP client helpers reused across aube crates.
//!
//! The npm registry path is dominated by cold TCP+TLS handshakes,
//! per-origin DNS lookups, and per-request priority noise. Each helper
//! here addresses one of those costs without owning a `reqwest::Client`
//! itself — call sites keep their builders and pass them in.
//!
//! Killswitch convention follows aube-util: every optimization that
//! defaults ON ships an `AUBE_DISABLE_*` env var. Each killswitch is
//! named in the doc comment of the function reading it so cargo doc
//! enumerates them.

pub mod prewarm;
pub mod priority;
pub mod race;
pub mod resolve;
pub mod ticket_cache;

/// Whether the platform verifier could load no roots, so reqwest refuses to
/// build a rustls client (minimal containers without `ca-certificates`, or
/// `SSL_CERT_FILE` naming a missing file). Probed once per process, warning
/// once when it is.
///
/// Only rustls on Unix (except Android and Apple) has this failure; the
/// Windows and Apple verifiers call the OS directly.
#[cfg(all(
    feature = "rustls",
    unix,
    not(target_os = "android"),
    not(target_vendor = "apple")
))]
fn system_roots_missing() -> bool {
    static MISSING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MISSING.get_or_init(|| {
        let missing = reqwest::Client::builder().use_rustls_tls().build().is_err();
        if missing {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_BUNDLED_ROOTS_FALLBACK,
                "no CA certificates could be loaded from the system, so aube is trusting its bundled Mozilla roots. Install ca-certificates, or check SSL_CERT_FILE and SSL_CERT_DIR"
            );
        }
        missing
    })
}

/// Trust Mozilla's bundled root certificates only when the system store
/// yields no roots at all.
///
/// Roots that do load from the OS are never supplemented, so a CA removed
/// from the OS store (or an enterprise trust policy) stays untrusted, and
/// `SSL_CERT_FILE` / `SSL_CERT_DIR` keep replacing the system store. When the
/// store is empty, aube warns once and uses the bundle instead of failing
/// every request.
///
/// A no-op on Windows and Apple targets, and compiled to a no-op when the
/// `rustls` feature is not enabled — this crate leaves the TLS backend to the
/// final binary (the aube binary selects rustls via aube-registry's defaults).
pub fn with_webpki_root_fallback(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    #[cfg(all(
        feature = "rustls",
        unix,
        not(target_os = "android"),
        not(target_vendor = "apple")
    ))]
    {
        if !system_roots_missing() {
            return builder;
        }
        let certs = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| {
                reqwest::Certificate::from_der(cert.as_ref())
                    // webpki-root-certs is generated as valid DER; failure means the dependency is corrupt.
                    .expect("webpki root certificate must be valid DER")
            })
            .collect::<Vec<_>>();
        builder.tls_certs_merge(certs)
    }

    #[cfg(not(all(
        feature = "rustls",
        unix,
        not(target_os = "android"),
        not(target_vendor = "apple")
    )))]
    {
        builder
    }
}
