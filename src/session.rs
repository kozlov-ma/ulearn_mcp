//! One explicitly supplied website session authenticates both API and gradebook.
//! Cookie and derived JWT values never appear in Debug, errors, or MCP results.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, Method, StatusCode, Url, header};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

pub const DEFAULT_API_URL: &str = "https://api.ulearn.me";
pub const DEFAULT_WEB_URL: &str = "https://ulearn.me";
const MAX_SECRET_BYTES: u64 = 64 * 1024;
const MAX_AUTH_RESPONSE_BYTES: usize = 1024 * 1024;
const RENEW_BEFORE_SECONDS: u64 = 300;

#[derive(Clone)]
enum CookieSource {
    Inline(String),
    File(PathBuf),
}

impl CookieSource {
    fn read(&self) -> Result<String> {
        let value = match self {
            Self::Inline(value) => value.clone(),
            Self::File(path) => read_session_file(path)?,
        };
        validate_cookie(&value)?;
        Ok(value.trim().to_owned())
    }
}

#[derive(Clone)]
pub struct SessionSnapshot {
    source: CookieSource,
    cookie: String,
    token: String,
    account_id: String,
    expires_at: u64,
    renew_at: u64,
    web_url: Url,
}

impl SessionSnapshot {
    pub fn web_url(&self) -> &str {
        self.web_url.as_str()
    }
    pub fn account_id(&self) -> &str {
        &self.account_id
    }
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }
    pub fn ensure_current(&self) -> Result<()> {
        ensure!(
            self.source.read()? == self.cookie,
            "Website session changed during the operation; read the target again before writing"
        );
        ensure!(
            self.expires_at > unix_now(),
            "Session-derived token expired during the operation; read the target again before writing"
        );
        Ok(())
    }
    pub(crate) fn bearer_token(&self) -> &str {
        &self.token
    }
    pub(crate) fn cookie_header(&self) -> Result<header::HeaderValue> {
        self.ensure_current()?;
        validate_cookie(&self.cookie)
    }
}

#[derive(Default)]
struct SessionState {
    cookie: Option<String>,
    token: Option<SessionSnapshot>,
    account_id: Option<String>,
}

#[derive(Clone)]
pub struct UlearnSession {
    client: Client,
    api_url: Url,
    web_url: Url,
    source: CookieSource,
    state: Arc<Mutex<SessionState>>,
}

impl UlearnSession {
    pub fn from_file(api_url: &str, web_url: &str, path: PathBuf) -> Result<Self> {
        ensure!(
            path.is_absolute(),
            "ULEARN_SESSION_FILE must be an absolute path"
        );
        Self::build(api_url, web_url, CookieSource::File(path))
    }

    /// Validates syntax and destinations locally; authentication happens on first use.
    pub fn from_cookie(api_url: &str, web_url: &str, cookie: String) -> Result<Self> {
        Self::build(api_url, web_url, CookieSource::Inline(cookie))
    }

    fn build(api_url: &str, web_url: &str, source: CookieSource) -> Result<Self> {
        let mut api_url = parse_url(api_url, false)?;
        if !api_url.path().ends_with('/') {
            api_url.set_path(&format!("{}/", api_url.path()));
        }
        let web_url = parse_url(web_url, true)?;
        source.read()?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .user_agent(concat!("ulearn-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Could not initialize the session client")?;
        Ok(Self {
            client,
            api_url,
            web_url,
            source,
            state: Arc::new(Mutex::new(SessionState::default())),
        })
    }

    pub fn api_url(&self) -> &str {
        self.api_url.as_str()
    }

    /// A single lock prevents concurrent exchanges and pins the first verified account.
    pub async fn authenticate(&self, force: bool) -> Result<SessionSnapshot> {
        let mut state = self.state.lock().await;
        let cookie = self.source.read()?;
        if state.cookie.as_deref() != Some(cookie.as_str()) {
            state.token = None;
            state.cookie = Some(cookie.clone());
        }
        if !force
            && let Some(cached) = &state.token
            && cached.renew_at > unix_now()
        {
            cached.ensure_current()?;
            return Ok(cached.clone());
        }
        let token_response = self
            .auth_json(
                Method::POST,
                "account/token",
                Some(validate_cookie(&cookie)?),
                None,
            )
            .await?;
        let token = token_response
            .get("token")
            .and_then(Value::as_str)
            .context(
                "Website session did not return an API token; run bootstrap-session --replace",
            )?
            .trim()
            .to_owned();
        ensure!(
            !token.is_empty() && token.len() <= MAX_SECRET_BYTES as usize,
            "Session returned an invalid API token"
        );
        bearer_header(&token)?;
        // Unverified claims only schedule renewal. GET /account verifies the token remotely.
        let expires_at =
            token_expiry(&token).context("Session returned an API token without a valid expiry")?;
        let received_at = unix_now();
        ensure!(
            expires_at > received_at,
            "Session returned an expired API token"
        );
        // Most tokens renew five minutes early; short-lived installations use
        // half their remaining lifetime instead of exchanging on every request.
        let renew_at = expires_at - RENEW_BEFORE_SECONDS.min((expires_at - received_at) / 2);
        let account = self
            .auth_json(Method::GET, "account", None, Some(&token))
            .await?;
        ensure!(
            account.get("isAuthenticated") != Some(&Value::Bool(false)),
            "Session account is not authenticated"
        );
        let account_id = account["user"]["id"]
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .context("Could not verify the website session account identity")?
            .to_owned();
        ensure!(
            state
                .account_id
                .as_ref()
                .is_none_or(|previous| previous == &account_id),
            "Website session belongs to a different account; restart MCP and read the target again before writing"
        );
        let snapshot = SessionSnapshot {
            source: self.source.clone(),
            cookie,
            token,
            account_id: account_id.clone(),
            expires_at,
            renew_at,
            web_url: self.web_url.clone(),
        };
        snapshot.ensure_current()?;
        state.account_id = Some(account_id);
        state.token = Some(snapshot.clone());
        Ok(snapshot)
    }

    /// A rejected bearer token must not poison the cache until its scheduled renewal.
    /// Matching preserves a newer token installed by a concurrent exchange.
    pub(crate) async fn invalidate_token(&self, rejected: &str) {
        let mut state = self.state.lock().await;
        if state
            .token
            .as_ref()
            .is_some_and(|snapshot| snapshot.token == rejected)
        {
            state.token = None;
        }
    }

    pub async fn refresh(&self) -> Result<Value> {
        let snapshot = self.authenticate(true).await?;
        Ok(
            json!({"refreshed":true,"expires_at":snapshot.expires_at,"persisted":false,"authentication":"session","account_id":snapshot.account_id}),
        )
    }

    async fn auth_json(
        &self,
        method: Method,
        path: &str,
        cookie: Option<header::HeaderValue>,
        token: Option<&str>,
    ) -> Result<Value> {
        let url = self
            .api_url
            .join(path)
            .context("Invalid session API path")?;
        let mut request = self
            .client
            .request(method, url)
            .header(header::ACCEPT, "application/json");
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, bearer_header(token)?);
        }
        let mut response = request.send().await.map_err(|_| {
            anyhow::anyhow!(
                "Session authentication request failed; original operation was not sent"
            )
        })?;
        let status = response.status();
        if status.is_redirection() {
            bail!("Session authentication redirect refused; run bootstrap-session --replace");
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            bail!("Website session expired or was rejected; run bootstrap-session --replace");
        }
        ensure!(
            status.is_success(),
            "Session authentication failed (HTTP {}); original operation was not sent",
            status.as_u16()
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= MAX_AUTH_RESPONSE_BYTES as u64),
            "Session authentication response exceeds the size limit"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("Could not read session authentication response"))?
        {
            ensure!(
                chunk.len() <= MAX_AUTH_RESPONSE_BYTES.saturating_sub(bytes.len()),
                "Session authentication response exceeds the size limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Session authentication returned invalid JSON"))?;
        ensure!(
            !value
                .get("error")
                .is_some_and(|error| !error.is_null() && error != false)
                && !value
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status.eq_ignore_ascii_case("error")),
            "Session authentication was rejected"
        );
        Ok(value)
    }
}

pub fn default_session_path() -> Result<PathBuf> {
    Ok(crate::bootstrap::default_token_path()?.with_file_name("ulearn.cookie"))
}

/// Explicit tokens win over default sessions; callers reject explicit ambiguity.
pub(crate) fn session_from_env(
    api_url: &str,
    has_explicit_token: bool,
) -> Result<Option<UlearnSession>> {
    let explicit = optional_env("ULEARN_SESSION_FILE")?;
    ensure!(
        !(explicit.is_some() && has_explicit_token),
        "Set either ULEARN_SESSION_FILE or ULEARN_TOKEN/ULEARN_TOKEN_FILE, not both"
    );
    if has_explicit_token {
        return Ok(None);
    }
    let configured_web = optional_env("ULEARN_WEB_URL")?;
    let web_url = configured_web.as_deref().unwrap_or(DEFAULT_WEB_URL);
    if let Some(path) = explicit {
        ensure!(
            api_url.trim_end_matches('/') == DEFAULT_API_URL || configured_web.is_some(),
            "Custom ULEARN_API_URL with a session requires explicit ULEARN_WEB_URL"
        );
        ensure!(
            web_url.trim_end_matches('/') == DEFAULT_WEB_URL
                || optional_env("ULEARN_API_URL")?.is_some(),
            "Custom ULEARN_WEB_URL with a session requires explicit ULEARN_API_URL"
        );
        return Ok(Some(UlearnSession::from_file(
            api_url,
            web_url,
            PathBuf::from(path),
        )?));
    }
    if api_url.trim_end_matches('/') != DEFAULT_API_URL
        || web_url.trim_end_matches('/') != DEFAULT_WEB_URL
    {
        return Ok(None);
    }
    let path = match default_session_path() {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(Some(UlearnSession::from_file(api_url, web_url, path)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => bail!("Could not inspect default website session file"),
    }
}

fn optional_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(_) => bail!("{name} is not valid UTF-8"),
    }
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn token_expiry(token: &str) -> Option<u64> {
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let value: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).ok()?).ok()?;
    value["exp"].as_u64()
}
fn bearer_header(token: &str) -> Result<header::HeaderValue> {
    let mut value = header::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| anyhow::anyhow!("Session returned an invalid token header"))?;
    value.set_sensitive(true);
    Ok(value)
}
fn validate_cookie(input: &str) -> Result<header::HeaderValue> {
    let cookie = input.trim();
    ensure!(
        !cookie.is_empty() && cookie.len() <= MAX_SECRET_BYTES as usize,
        "Website session is empty or too large"
    );
    ensure!(
        !cookie.chars().any(char::is_control),
        "Website session contains invalid characters"
    );
    for pair in cookie.split(';') {
        let (name, _) = pair
            .trim()
            .split_once('=')
            .context("Use a Cookie header value containing name=value pairs")?;
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)),
            "Invalid website cookie name/value pair"
        );
    }
    let mut value = header::HeaderValue::from_str(cookie)
        .map_err(|_| anyhow::anyhow!("Website session contains invalid header characters"))?;
    value.set_sensitive(true);
    Ok(value)
}
fn read_session_file(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .context("Could not read website session file; run bootstrap-session")?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_SECRET_BYTES,
        "Website session file must be a regular file of at most 64 KiB, not a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "Website session file must be private; run chmod 600 on it"
        );
    }
    let mut value = String::new();
    fs::File::open(path)
        .context("Could not open website session file")?
        .take(MAX_SECRET_BYTES + 1)
        .read_to_string(&mut value)
        .context("Could not read website session file as UTF-8")?;
    ensure!(
        value.len() <= MAX_SECRET_BYTES as usize,
        "Website session file exceeds 64 KiB"
    );
    Ok(value)
}
fn parse_url(input: &str, website: bool) -> Result<Url> {
    let url = Url::parse(input).map_err(|_| anyhow::anyhow!("Invalid session destination URL"))?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Session URLs must not contain credentials, query, or fragment"
    );
    ensure!(
        !website || url.path() == "/",
        "ULEARN_WEB_URL must be an origin without a path"
    );
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        url.host_str().is_some() && (url.scheme() == "https" || url.scheme() == "http" && loopback),
        "Session URLs must use HTTPS (HTTP is allowed only on loopback for local tests)"
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[tokio::test]
    async fn expired_cached_token_is_reacquired_from_cookie_without_api_token_renewal() {
        let api = MockServer::start().await;
        let counter = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/account/token"))
            .respond_with(move |_: &wiremock::Request| {
                let generation = counter.fetch_add(1, Ordering::SeqCst);
                let token = format!(
                    "e30.{}.signature{generation}",
                    URL_SAFE_NO_PAD.encode(json!({"exp":unix_now()+3600}).to_string())
                );
                ResponseTemplate::new(200).set_body_json(json!({"token":token}))
            })
            .expect(2)
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/account"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"isAuthenticated":true,"user":{"id":"teacher"}})),
            )
            .expect(2)
            .mount(&api)
            .await;
        let session =
            UlearnSession::from_cookie(&api.uri(), &api.uri(), "session=test".into()).unwrap();
        let first = session.authenticate(false).await.unwrap();
        {
            let mut state = session.state.lock().await;
            let cached = state.token.as_mut().unwrap();
            cached.expires_at = unix_now() - 1;
            cached.renew_at = unix_now() - 2;
        }
        let second = session.authenticate(false).await.unwrap();
        // A delayed 401 for the old token must preserve the newer cache entry.
        session.invalidate_token(first.bearer_token()).await;
        let cached = session.authenticate(false).await.unwrap();
        assert_eq!(second.bearer_token(), cached.bearer_token());
        assert_ne!(first.bearer_token(), second.bearer_token());
        assert_eq!(first.account_id(), second.account_id());
        assert_eq!(api.received_requests().await.unwrap().len(), 4);
    }
}
