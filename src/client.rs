//! The small, deliberately non-retrying HTTP boundary to ulearn.
//!
//! Neither credentials nor server response bodies are included in errors. In
//! particular, failed writes are never retried: a lost response does not imply
//! that a comment or a grade was not saved.

use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, StatusCode, Url, header};
use serde_json::Value;
use std::{env, fs, io::Read, path::Path, time::Duration};

const DEFAULT_API_URL: &str = "https://api.ulearn.me";
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOKEN_BYTES: u64 = 64 * 1024;

#[derive(Clone)]
pub struct UlearnClient {
    client: Client,
    base_url: Url,
    authenticated: bool,
}

impl UlearnClient {
    pub fn from_env() -> Result<Self> {
        let base_url =
            optional_env("ULEARN_API_URL")?.unwrap_or_else(|| DEFAULT_API_URL.to_owned());
        let direct_token = optional_env("ULEARN_TOKEN")?;
        let token_file = optional_env("ULEARN_TOKEN_FILE")?;
        let token = match (direct_token, token_file) {
            (Some(_), Some(_)) => bail!("Set only one of ULEARN_TOKEN or ULEARN_TOKEN_FILE"),
            (Some(token), None) => Some(token),
            (None, Some(path)) => Some(read_token_file(Path::new(&path))?),
            (None, None) => {
                // Never implicitly forward a production credential to a custom API.
                if base_url.trim_end_matches('/') == DEFAULT_API_URL {
                    match crate::bootstrap::default_token_path() {
                        Ok(path) => match fs::symlink_metadata(&path) {
                            Ok(_) => Some(read_token_file(&path)?),
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                            Err(_) => bail!("Could not inspect default token file"),
                        },
                        Err(_) => None,
                    }
                } else {
                    None
                }
            }
        };
        Self::new(&base_url, token)
    }

    pub fn new(base_url: &str, token: Option<String>) -> Result<Self> {
        let mut base_url =
            Url::parse(base_url).map_err(|_| anyhow::anyhow!("Invalid ULEARN_API_URL"))?;
        validate_base_url(&base_url)?;
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }

        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );
        let authenticated = token.is_some();
        if let Some(token) = token {
            let token = token.trim();
            if token.is_empty() || token.len() > MAX_TOKEN_BYTES as usize {
                bail!("Ulearn token is empty or too large");
            }
            let mut authorization = header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| anyhow::anyhow!("Ulearn token contains invalid header characters"))?;
            authorization.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, authorization);
        }
        let client = Client::builder()
            .default_headers(headers)
            .user_agent(concat!("ulearn-mcp/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|_| anyhow::anyhow!("Could not initialize the HTTP client"))?;
        Ok(Self {
            client,
            base_url,
            authenticated,
        })
    }

    pub async fn get(&self, segments: &[&str], query: &[(String, String)]) -> Result<Value> {
        self.request(Method::GET, segments, query, None).await
    }

    pub async fn post(
        &self,
        segments: &[&str],
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        self.request(Method::POST, segments, query, body).await
    }

    pub async fn put(&self, segments: &[&str], query: &[(String, String)]) -> Result<Value> {
        self.request(Method::PUT, segments, query, None).await
    }

    /// Exchange credentials with ulearn directly. The caller must keep the
    /// returned bearer token out of logs and MCP responses.
    pub async fn login(&self, login: &str, password: &str) -> Result<String> {
        if login.trim().is_empty() || password.is_empty() {
            bail!("Ulearn login and password must be nonempty");
        }
        let response = self
            .request(
                Method::POST,
                &["account", "login"],
                &[],
                Some(serde_json::json!({"login": login, "password": password})),
            )
            .await?;
        let token = response
            .get("token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .context("Ulearn login did not return a token; check your login credentials")?;
        if token.len() > MAX_TOKEN_BYTES as usize
            || header::HeaderValue::from_str(&format!("Bearer {token}")).is_err()
        {
            bail!("Ulearn login returned an invalid token");
        }
        Ok(token.to_owned())
    }

    fn url(&self, segments: &[&str], query: &[(String, String)]) -> Result<Url> {
        let mut url = self.base_url.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("API URL cannot contain path segments"))?;
            path.pop_if_empty();
            for segment in segments {
                if segment.is_empty()
                    || *segment == "."
                    || *segment == ".."
                    || segment.chars().any(char::is_control)
                {
                    bail!("Invalid API path segment");
                }
                path.push(segment);
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        Ok(url)
    }

    async fn request(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let write = method != Method::GET;
        let login_request = method == Method::POST && segments == ["account", "login"];
        if write && !self.authenticated && !login_request {
            bail!("Ulearn writes require ULEARN_TOKEN or ULEARN_TOKEN_FILE");
        }
        let url = self.url(segments, query)?;
        let mut request = self.client.request(method, url);
        if let Some(body) = body {
            request = request.json(&body);
        }
        // No retry middleware: POST creates comments and may finalize grades.
        let mut response = request.send().await.map_err(|error| {
            let reason = if error.is_timeout() {
                "Ulearn request timed out"
            } else if error.is_connect() {
                "Could not connect to ulearn"
            } else {
                "Ulearn request failed"
            };
            outcome_error(reason, write)
        })?;
        let status = response.status();
        if !status.is_success() {
            let reason = status_message(status);
            if write && status.is_server_error() {
                return Err(outcome_error(&reason, true));
            }
            bail!("{reason}");
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(outcome_error(
                "Ulearn response exceeds the 16 MiB limit; narrow the query",
                write,
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| outcome_error("Could not read the ulearn response", write))?
        {
            if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                return Err(outcome_error(
                    "Ulearn response exceeds the 16 MiB limit; narrow the query",
                    write,
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            outcome_error(
                "Ulearn returned a successful response that was not JSON",
                write,
            )
        })?;
        if value
            .get("error")
            .is_some_and(|error| !error.is_null() && error != false)
            || value
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status.eq_ignore_ascii_case("error"))
        {
            return Err(outcome_error(
                "Ulearn reported an application error despite HTTP success",
                write,
            ));
        }
        Ok(value)
    }
}

fn optional_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => bail!("{name} is not valid UTF-8"),
    }
}

fn validate_base_url(url: &Url) -> Result<()> {
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("ULEARN_API_URL must not contain credentials, a query, or a fragment");
    }
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.host_str().is_none() || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
    {
        bail!("ULEARN_API_URL must use HTTPS (HTTP is allowed only on loopback for local tests)");
    }
    Ok(())
}

fn read_token_file(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path).context("Could not inspect ULEARN_TOKEN_FILE")?;
    if !metadata.is_file() || metadata.len() > MAX_TOKEN_BYTES {
        bail!("ULEARN_TOKEN_FILE must be a regular file of at most 64 KiB, not a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("ULEARN_TOKEN_FILE must not be readable by other users; run chmod 600 on it");
        }
    }
    let file = fs::File::open(path).context("Could not open ULEARN_TOKEN_FILE")?;
    let mut token = String::new();
    file.take(MAX_TOKEN_BYTES + 1)
        .read_to_string(&mut token)
        .context("Could not read ULEARN_TOKEN_FILE as UTF-8")?;
    if token.len() > MAX_TOKEN_BYTES as usize {
        bail!("ULEARN_TOKEN_FILE exceeds 64 KiB");
    }
    Ok(token)
}

fn outcome_error(message: &str, write: bool) -> anyhow::Error {
    if write {
        anyhow::anyhow!(
            "{message}. The write outcome is unknown: inspect the submission/reviews before retrying; do not blindly repeat the write"
        )
    } else {
        anyhow::anyhow!("{message}")
    }
}

fn status_message(status: StatusCode) -> String {
    let hint = match status {
        StatusCode::UNAUTHORIZED => {
            "authentication required or token expired; configure ULEARN_TOKEN or ULEARN_TOKEN_FILE"
        }
        StatusCode::FORBIDDEN => {
            "the account does not have permission for this course/student/action"
        }
        StatusCode::NOT_FOUND => "the course, slide, submission, or review was not found",
        StatusCode::CONFLICT => {
            "the remote state conflicts with this request; refresh before trying again"
        }
        StatusCode::TOO_MANY_REQUESTS => "rate limited; wait before another request",
        StatusCode::BAD_REQUEST => "ulearn rejected the parameters",
        StatusCode::PAYLOAD_TOO_LARGE => "ulearn rejected the request size",
        status if status.is_redirection() => {
            "redirect refused; check ULEARN_API_URL (credentials were not forwarded)"
        }
        _ => "ulearn returned an unsuccessful response",
    };
    format!("Ulearn HTTP {}: {hint}", status.as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };

    #[test]
    fn rejects_unsafe_base_urls_and_invalid_tokens() {
        for url in [
            "http://api.ulearn.me",
            "https://user:password@api.ulearn.me",
            "https://api.ulearn.me?q=1",
            "https://api.ulearn.me/#x",
            "file:///tmp/api",
        ] {
            assert!(UlearnClient::new(url, None).is_err(), "accepted {url}");
        }
        assert!(UlearnClient::new("https://api.ulearn.me", Some("\n".into())).is_err());
        assert!(
            UlearnClient::new("https://api.ulearn.me", Some("token\r\nInjected: x".into()))
                .is_err()
        );
        assert!(UlearnClient::new("http://[::1]:8080", None).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn token_files_require_private_permissions_and_reject_symlinks() {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ulearn-mcp-token-test-{}-{suffix}",
            std::process::id()
        ));
        let link = path.with_extension("link");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"synthetic-test-token\n").unwrap();
        drop(file);
        assert_eq!(read_token_file(&path).unwrap(), "synthetic-test-token\n");
        symlink(&path, &link).unwrap();
        assert!(read_token_file(&link).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_token_file(&path)
                .unwrap_err()
                .to_string()
                .contains("chmod 600")
        );
        fs::remove_file(link).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn encodes_parameters_and_preserves_duplicate_query_keys() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"source":"one\ntwo\n"})))
            .expect(1)
            .mount(&server)
            .await;
        let client = UlearnClient::new(&server.uri(), Some("test-token".into())).unwrap();
        let body = client
            .get(
                &["courses", "a/b ?#"],
                &[
                    ("groupIds".into(), "1".into()),
                    ("groupIds".into(), "2".into()),
                ],
            )
            .await
            .unwrap();
        assert_eq!(body["source"], "one\ntwo\n");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests[0].url.path(), "/courses/a%2Fb%20%3F%23");
        assert_eq!(requests[0].url.query(), Some("groupIds=1&groupIds=2"));
        assert!(client.url(&[".."], &[]).is_err());
    }

    #[tokio::test]
    async fn post_score_supports_empty_response_and_exact_query() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/submissions/42/score"))
            .and(query_param("percent", "75"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let client = UlearnClient::new(&server.uri(), Some("token".into())).unwrap();
        assert_eq!(
            client
                .post(
                    &["submissions", "42", "score"],
                    &[("percent".into(), "75".into())],
                    None
                )
                .await
                .unwrap(),
            Value::Null
        );
    }

    #[tokio::test]
    async fn does_not_retry_failed_writes_or_leak_response_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("private-secret-response"))
            .expect(1)
            .mount(&server)
            .await;
        let client = UlearnClient::new(&server.uri(), Some("private-secret-token".into())).unwrap();
        let error = client
            .post(&["reviews"], &[], Some(json!({"text":"private-comment"})))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("outcome is unknown"));
        assert!(!error.contains("private-"));
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let source = MockServer::start().await;
        let destination = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&destination)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", destination.uri()))
            .expect(1)
            .mount(&source)
            .await;
        let client = UlearnClient::new(&source.uri(), Some("secret".into())).unwrap();
        assert!(
            client
                .get(&["account"], &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("redirect refused")
        );
    }

    #[tokio::test]
    async fn refuses_unauthenticated_writes_before_network() {
        let server = MockServer::start().await;
        let client = UlearnClient::new(&server.uri(), None).unwrap();
        assert!(client.post(&["reviews"], &[], None).await.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn login_is_the_only_unauthenticated_write_exception() {
        use wiremock::matchers::body_json;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/account/login"))
            .and(body_json(
                json!({"login":"teacher", "password":"synthetic-password"}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"status":"ok", "token":"synthetic-token"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = UlearnClient::new(&server.uri(), None).unwrap();
        assert_eq!(
            client.login("teacher", "synthetic-password").await.unwrap(),
            "synthetic-token"
        );
        assert!(client.put(&["account", "login"], &[]).await.is_err());
        assert!(
            client
                .post(&["account", "api-token"], &[], None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn application_errors_do_not_leak_details_or_fake_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"error":"private-detail"})),
            )
            .mount(&server)
            .await;
        let client = UlearnClient::new(&server.uri(), None).unwrap();
        let error = client
            .login("teacher", "secret")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("application error"));
        assert!(!error.contains("private-detail"));
    }

    #[tokio::test]
    async fn rejects_oversized_or_non_json_success_without_echoing_body() {
        let server = MockServer::start().await;
        Mock::given(path("/large"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_RESPONSE_BYTES + 1]),
            )
            .mount(&server)
            .await;
        Mock::given(path("/html"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>private data</html>"))
            .mount(&server)
            .await;
        let client = UlearnClient::new(&server.uri(), None).unwrap();
        assert!(
            client
                .get(&["large"], &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("16 MiB")
        );
        let error = client.get(&["html"], &[]).await.unwrap_err().to_string();
        assert!(error.contains("not JSON"));
        assert!(!error.contains("private data"));
    }
}
