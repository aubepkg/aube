/**
 * Redact bearer credentials from a URL before it lands in error
 * messages, trace logs, or diagnostic output.
 *
 * Credential-bearing URL components are scrubbed:
 *   - `user:password@host` userinfo (Artifactory, Nexus, JFrog,
 *     GitHub Packages, scoped npm registries with embedded auth).
 *   - The entire query and fragment, including signed URLs and custom
 *     credential parameters. An allowlist of token names cannot cover
 *     every registry or object-storage provider.
 *   - Returns the input unchanged when no credential pattern is
 *     present.
 */
pub fn redact_url(url: &str) -> String {
    let after_userinfo = redact_userinfo(url);
    match after_userinfo.find(['?', '#']) {
        Some(start) => format!("{}***", &after_userinfo[..start + 1]),
        None => after_userinfo,
    }
}

/**
 * Redact only the `user:password@` portion of `url`, if any.
 *
 * Handles both fully-qualified (`scheme://user:pw@host`) and
 * scheme-relative (`//user:pw@host`) inputs.
 */
fn redact_userinfo(url: &str) -> String {
    let after = if let Some(scheme_end) = url.find("://") {
        scheme_end + 3
    } else if url.starts_with("//") {
        2
    } else {
        return url.to_string();
    };
    let tail = &url[after..];
    let Some(at) = tail.find('@') else {
        return url.to_string();
    };
    let authority_end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
    if at >= authority_end {
        return url.to_string();
    }
    format!("{}***@{}", &url[..after], &tail[at + 1..])
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn passthrough_when_no_userinfo() {
        assert_eq!(
            redact_url("https://registry.example.com/foo"),
            "https://registry.example.com/foo"
        );
    }

    #[test]
    fn redacts_user_and_password() {
        let input = format!("https://user:hunter2{}host.example.com/x", '\u{40}');
        let expected = format!("https://***{}host.example.com/x", '\u{40}');
        assert_eq!(redact_url(&input), expected);
    }

    #[test]
    fn does_not_redact_at_in_path() {
        let input = format!("https://host/foo{}1.0.0/bar", '\u{40}');
        assert_eq!(redact_url(&input), input);
    }

    #[test]
    fn redacts_userinfo_with_ipv6_host() {
        let input = format!("https://tok{}[::1]:8443/x", '\u{40}');
        let expected = format!("https://***{}[::1]:8443/x", '\u{40}');
        assert_eq!(redact_url(&input), expected);
    }

    #[test]
    fn redacts_scheme_relative_userinfo() {
        let input = format!("//user:pw{}host.example.com/x", '\u{40}');
        let expected = format!("//***{}host.example.com/x", '\u{40}');
        assert_eq!(redact_url(&input), expected);
    }

    #[test]
    fn query_at_sign_is_not_userinfo() {
        assert_eq!(
            redact_url("https://registry.example?token=user@secret"),
            "https://registry.example?***"
        );
    }

    #[test]
    fn fragment_at_sign_is_not_userinfo() {
        assert_eq!(
            redact_url("https://registry.example#token=user@secret"),
            "https://registry.example#***"
        );
    }

    #[test]
    fn redacts_query_token() {
        assert_eq!(
            redact_url("https://reg.example.com/x?token=abc123&v=1"),
            "https://reg.example.com/x?***"
        );
    }

    #[test]
    fn redacts_query_auth_case_insensitive() {
        assert_eq!(
            redact_url("https://reg.example.com/x?Auth=secret"),
            "https://reg.example.com/x?***"
        );
    }

    #[test]
    fn redacts_query_apikey_alias() {
        assert_eq!(
            redact_url("https://reg.example.com/x?apikey=abc&api_key=def"),
            "https://reg.example.com/x?***"
        );
    }

    #[test]
    fn redacts_fragment_along_with_query() {
        assert_eq!(
            redact_url("https://reg.example.com/x?token=abc#section"),
            "https://reg.example.com/x?***"
        );
    }

    #[test]
    fn redacts_unrecognized_query_parameters() {
        assert_eq!(
            redact_url("https://reg.example.com/x?foo=1&bar=2"),
            "https://reg.example.com/x?***"
        );
    }

    #[test]
    fn redacts_signed_urls_and_fragment_credentials() {
        for suffix in [
            "?X-Amz-Signature=secret&X-Amz-Credential=key",
            "?X-Goog-Signature=secret",
            "?sig=secret&sv=2026-01-01",
            "?custom-secret",
            "?access%5ftoken=secret#secret",
            "#access_token=secret",
        ] {
            let rendered = redact_url(&format!("https://registry.example/tarball{suffix}"));
            assert!(!rendered.contains("secret"), "{rendered}");
        }
    }
}
