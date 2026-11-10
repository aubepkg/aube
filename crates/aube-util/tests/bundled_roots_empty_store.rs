#![cfg(all(
    feature = "rustls",
    unix,
    not(target_os = "android"),
    not(target_vendor = "apple")
))]

//! Own test binary: the system-roots probe is cached per process, and this
//! test must set the environment before the first client is built.

#[test]
fn falls_back_to_bundled_roots_when_system_store_is_empty() {
    // SAFETY: the only test in this binary, so no other thread touches the environment.
    unsafe {
        std::env::remove_var("SSL_CERT_FILE");
        std::env::set_var("SSL_CERT_DIR", "/nonexistent/aube-util-test");
    }
    assert!(
        reqwest::Client::builder().use_rustls_tls().build().is_err(),
        "reqwest should refuse to build with no system roots"
    );
    let built = aube_util::http::with_webpki_root_fallback(reqwest::Client::builder())
        .use_rustls_tls()
        .build();
    assert!(built.is_ok(), "bundled roots should rescue the build");
}
