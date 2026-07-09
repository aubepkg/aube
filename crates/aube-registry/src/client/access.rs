use super::body::{check_body_cap, read_body_capped};
use super::{RegistryClient, encoded_name};
use crate::Error;

impl RegistryClient {
    /// List packages visible to the current user or a requested user,
    /// organization, or `scope:team` entity.
    pub async fn access_list_packages(
        &self,
        entity: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let registry_url = &self.config.registry;
        let url = match entity {
            None => format!(
                "{}/-/package?format=cli",
                registry_url.trim_end_matches('/')
            ),
            Some(entity) if entity.contains(':') => {
                let (scope, team) = entity.split_once(':').expect("contains colon");
                format!(
                    "{}/-/team/{}/{}/package?format=cli",
                    registry_url.trim_end_matches('/'),
                    encode_access_component(scope.trim_start_matches('@')),
                    encode_access_component(team),
                )
            }
            Some(entity) if entity.starts_with('@') => format!(
                "{}/-/org/{}/package?format=cli",
                registry_url.trim_end_matches('/'),
                encode_access_component(entity.trim_start_matches('@')),
            ),
            Some(entity) => format!(
                "{}/-/user/{}/package?format=cli",
                registry_url.trim_end_matches('/'),
                encode_access_component(entity),
            ),
        };
        self.access_request(reqwest::Method::GET, &url, registry_url, None, None, None)
            .await
    }

    /// List package collaborators, optionally restricted to one user.
    pub async fn access_list_collaborators(
        &self,
        name: &str,
        user: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let registry_url = self.registry_url_for(name);
        let mut url = format!(
            "{}/-/package/{}/collaborators?format=cli",
            registry_url.trim_end_matches('/'),
            encoded_name(name),
        );
        if let Some(user) = user {
            url.push_str("&user=");
            url.push_str(&encode_access_component(user));
        }
        self.access_request(
            reqwest::Method::GET,
            &url,
            registry_url,
            Some(name),
            None,
            None,
        )
        .await
    }

    /// Get a package's registry access status and MFA policy.
    pub async fn access_get_status(&self, name: &str) -> Result<serde_json::Value, Error> {
        let registry_url = self.registry_url_for(name);
        let url = access_package_url(registry_url, name, "access");
        self.access_request(
            reqwest::Method::GET,
            &url,
            registry_url,
            Some(name),
            None,
            None,
        )
        .await
    }

    /// Change a scoped package's visibility (`public` or `restricted`).
    pub async fn access_set_status(
        &self,
        name: &str,
        access: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = access_package_url(registry_url, name, "access");
        self.access_request(
            reqwest::Method::POST,
            &url,
            registry_url,
            Some(name),
            Some(serde_json::json!({ "access": access })),
            otp,
        )
        .await
        .map(|_| ())
    }

    /// Change a package's publish MFA requirement.
    pub async fn access_set_mfa(
        &self,
        name: &str,
        publish_requires_tfa: bool,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = access_package_url(registry_url, name, "access");
        self.access_request(
            reqwest::Method::POST,
            &url,
            registry_url,
            Some(name),
            Some(serde_json::json!({ "publish_requires_tfa": publish_requires_tfa })),
            otp,
        )
        .await
        .map(|_| ())
    }

    /// Grant a team read-only or read-write access to a package.
    pub async fn access_grant(
        &self,
        name: &str,
        scope: &str,
        team: &str,
        permissions: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        self.access_team_request(
            reqwest::Method::PUT,
            name,
            scope,
            team,
            Some(serde_json::json!({ "package": name, "permissions": permissions })),
            otp,
        )
        .await
    }

    /// Revoke a team's access to a package.
    pub async fn access_revoke(
        &self,
        name: &str,
        scope: &str,
        team: &str,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        self.access_team_request(
            reqwest::Method::DELETE,
            name,
            scope,
            team,
            Some(serde_json::json!({ "package": name })),
            otp,
        )
        .await
    }

    async fn access_team_request(
        &self,
        method: reqwest::Method,
        name: &str,
        scope: &str,
        team: &str,
        body: Option<serde_json::Value>,
        otp: Option<&str>,
    ) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = format!(
            "{}/-/team/{}/{}/package",
            registry_url.trim_end_matches('/'),
            encode_access_component(scope.trim_start_matches('@')),
            encode_access_component(team),
        );
        self.access_request(method, &url, registry_url, Some(name), body, otp)
            .await
            .map(|_| ())
    }

    async fn access_request(
        &self,
        method: reqwest::Method,
        url: &str,
        registry_url: &str,
        package_name: Option<&str>,
        body: Option<serde_json::Value>,
        otp: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let mut request = match package_name {
            Some(name) => self.authed_request_for_package(method, url, registry_url, name),
            None => self.authed_request(method, url, registry_url),
        };
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .json(&body);
        }
        if let Some(otp) = otp {
            request = request.header("npm-otp", otp);
        }

        let resp = request.send().await?;
        match resp.status() {
            reqwest::StatusCode::NOT_FOUND => {
                return Err(Error::NotFound(
                    package_name.unwrap_or("access").to_string(),
                ));
            }
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                return Err(Error::Unauthorized);
            }
            status if !status.is_success() => {
                let status = status.as_u16();
                let body =
                    read_body_capped(resp, self.fetch_policy.packument_max_bytes, "access").await?;
                return Err(Error::RegistryWrite {
                    status,
                    body: String::from_utf8_lossy(&body).into_owned(),
                });
            }
            _ => {}
        }
        check_body_cap(&resp, self.fetch_policy.packument_max_bytes, "access")?;
        let body = read_body_capped(resp, self.fetch_policy.packument_max_bytes, "access").await?;
        if body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&body)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
    }
}

fn access_package_url(registry_url: &str, name: &str, suffix: &str) -> String {
    format!(
        "{}/-/package/{}/{}",
        registry_url.trim_end_matches('/'),
        encoded_name(name),
        suffix,
    )
}

fn encode_access_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{RegistryClient, access_package_url, encode_access_component};
    use crate::config::NpmConfig;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn package_access_url_encodes_scoped_name() {
        assert_eq!(
            access_package_url("https://registry.example/", "@scope/pkg", "access"),
            "https://registry.example/-/package/@scope%2Fpkg/access"
        );
    }

    #[test]
    fn access_component_escapes_reserved_characters() {
        assert_eq!(encode_access_component("dev team/@a"), "dev%20team%2F%40a");
    }

    #[tokio::test]
    async fn package_access_routes_through_scoped_registry_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/-/package/@scope%2Fpkg/access"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "access": "restricted" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/-/package/@scope%2Fpkg/access"))
            .and(header("npm-otp", "123456"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = RegistryClient::from_config(NpmConfig {
            registry: format!("{}/", server.uri()),
            ..Default::default()
        });
        assert_eq!(
            client
                .access_get_status("@scope/pkg")
                .await
                .expect("get status"),
            serde_json::json!({ "access": "restricted" })
        );
        client
            .access_set_status("@scope/pkg", "restricted", Some("123456"))
            .await
            .expect("set status");
    }
}
