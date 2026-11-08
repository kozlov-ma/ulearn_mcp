//! The legacy gradebook uses a website session, not an API bearer token.
//! Never reads browser storage, follows redirects, or retries a grade write.
use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, StatusCode, Url, header};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

pub const DEFAULT_WEB_URL: &str = "https://ulearn.me";
const MAX_COOKIE_BYTES: u64 = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
enum CookieSource {
    Inline(String),
    File(PathBuf),
}

#[derive(Clone)]
pub struct UlearnWebClient {
    client: Client,
    base_url: Url,
    cookie: CookieSource,
}

impl UlearnWebClient {
    pub fn from_env() -> Result<Self> {
        let api = optional_env("ULEARN_API_URL")?;
        let web = optional_env("ULEARN_WEB_URL")?;
        let cookie_file = optional_env("ULEARN_WEB_COOKIE_FILE")?;
        validate_installation_config(api.as_deref(), web.as_deref(), cookie_file.as_deref())?;
        let base = web.unwrap_or_else(|| DEFAULT_WEB_URL.into());
        let path = match cookie_file {
            Some(path) => PathBuf::from(path),
            None if base.trim_end_matches('/') == DEFAULT_WEB_URL => {
                crate::bootstrap::default_token_path()?.with_file_name("ulearn.cookie")
            }
            None => bail!("Custom ULEARN_WEB_URL requires an explicit ULEARN_WEB_COOKIE_FILE"),
        };
        Self::from_cookie_file(&base, path)
    }

    /// Intended for an explicitly supplied session or a local test server.
    /// Do not expose cookie values as MCP arguments or tool results.
    pub fn new(base_url: &str, cookie: String) -> Result<Self> {
        Self::build(base_url, CookieSource::Inline(cookie))
    }

    /// Re-read this private file before every write so session replacement takes effect.
    pub fn from_cookie_file(base_url: &str, path: PathBuf) -> Result<Self> {
        ensure!(
            path.is_absolute(),
            "Website cookie file path must be absolute"
        );
        read_cookie_file(&path)?;
        Self::build(base_url, CookieSource::File(path))
    }

    fn build(base_url: &str, cookie: CookieSource) -> Result<Self> {
        let base_url =
            Url::parse(base_url).map_err(|_| anyhow::anyhow!("Invalid ULEARN_WEB_URL"))?;
        validate_base_url(&base_url)?;
        if let CookieSource::Inline(value) = &cookie {
            cookie_header(value)?;
        }
        let client = Client::builder()
            .user_agent(concat!("ulearn-mcp/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|_| anyhow::anyhow!("Could not initialize the website client"))?;
        Ok(Self {
            client,
            base_url,
            cookie,
        })
    }

    pub async fn set_additional_score(
        &self,
        course_id: &str,
        unit_id: &str,
        user_id: &str,
        scoring_group_id: &str,
        score: Option<i32>,
    ) -> Result<Value> {
        for id in [course_id, unit_id, user_id, scoring_group_id] {
            ensure!(
                !id.trim().is_empty() && !id.chars().any(char::is_control),
                "Grade target IDs must be nonempty and contain no control characters"
            );
        }
        ensure!(
            score.is_none_or(|value| value >= 0),
            "Additional score must be nonnegative or null"
        );
        let cookie = match &self.cookie {
            CookieSource::Inline(value) => cookie_header(value)?,
            CookieSource::File(path) => cookie_header(&read_cookie_file(path)?)?,
        };
        let mut url = self.base_url.clone();
        url.set_path("/Admin/SetAdditionalScore");
        url.query_pairs_mut().extend_pairs([
            ("courseId", course_id),
            ("unitId", unit_id),
            ("userId", user_id),
            ("scoringGroupId", scoring_group_id),
        ]);
        let score_text = score.map(|value| value.to_string()).unwrap_or_default();
        let mut response = self
            .client
            .post(url)
            .header(header::COOKIE, cookie)
            .header(header::ACCEPT, "application/json")
            .form(&[("score", score_text)])
            .send()
            .await
            .map_err(|_| unknown_outcome("Website score request failed"))?;
        let status = response.status();
        if status.is_redirection() {
            bail!(
                "Website redirected the score request; refresh the website session with bootstrap-web. Redirect was not followed; inspect module scores before retrying"
            );
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            bail!(
                "Website session is expired or lacks permission; use bootstrap-web to replace the session"
            );
        }
        if !status.is_success() {
            if status.is_server_error() {
                return Err(unknown_outcome("Website returned a server error"));
            }
            bail!(
                "Website rejected the score request (HTTP {})",
                status.as_u16()
            );
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
        {
            return Err(unknown_outcome(
                "Website score response exceeds the size limit",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| unknown_outcome("Could not read website score response"))?
        {
            if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                return Err(unknown_outcome(
                    "Website score response exceeds the size limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| unknown_outcome("Website returned an invalid score response"))?;
        if value.get("status").and_then(Value::as_str) == Some("error") {
            bail!(
                "Website rejected the score change; check the course limits and instructor permissions"
            );
        }
        let expected = score.map_or_else(|| json!(""), |value| json!(value));
        if value.get("status").and_then(Value::as_str) != Some("ok")
            || value.get("score") != Some(&expected)
        {
            return Err(unknown_outcome(
                "Website did not confirm the requested score",
            ));
        }
        // Only return the documented receipt; arbitrary response fields may be private.
        Ok(json!({"status":"ok", "score": expected}))
    }
}

pub(crate) fn validate_installation_config(
    api_url: Option<&str>,
    web_url: Option<&str>,
    cookie_file: Option<&str>,
) -> Result<()> {
    if api_url.is_some_and(|url| url.trim_end_matches('/') != crate::bootstrap::DEFAULT_API_URL) {
        ensure!(
            web_url.is_some() && cookie_file.is_some(),
            "Custom ULEARN_API_URL requires explicit ULEARN_WEB_URL and ULEARN_WEB_COOKIE_FILE; refusing to mix API and website installations"
        );
    }
    if web_url.is_some_and(|url| url.trim_end_matches('/') != DEFAULT_WEB_URL) {
        ensure!(
            cookie_file.is_some(),
            "Custom ULEARN_WEB_URL requires an explicit cookie file path"
        );
    }
    Ok(())
}

pub(crate) fn optional_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => bail!("{name} is not valid UTF-8"),
    }
}

fn validate_base_url(url: &Url) -> Result<()> {
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "ULEARN_WEB_URL must be an origin without credentials, path, query, or fragment"
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
        "ULEARN_WEB_URL must use HTTPS (HTTP is allowed only on loopback for local tests)"
    );
    Ok(())
}

fn cookie_header(input: &str) -> Result<header::HeaderValue> {
    let cookie = input.trim();
    ensure!(
        !cookie.is_empty() && cookie.len() <= MAX_COOKIE_BYTES as usize,
        "Website cookie is empty or too large"
    );
    ensure!(
        !cookie.chars().any(char::is_control),
        "Website cookie contains invalid characters"
    );
    // Accept a Cookie header value, not a Set-Cookie line or browser export JSON.
    for pair in cookie.split(';') {
        let (name, value) = pair
            .trim()
            .split_once('=')
            .context("Use a Cookie header value containing name=value pairs")?;
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                && !value.contains(';'),
            "Invalid website cookie name/value pair"
        );
    }
    let mut value = header::HeaderValue::from_str(cookie)
        .map_err(|_| anyhow::anyhow!("Website cookie contains invalid header characters"))?;
    value.set_sensitive(true);
    Ok(value)
}

fn read_cookie_file(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .context("Could not read website cookie file; run ulearn-mcp bootstrap-web")?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_COOKIE_BYTES,
        "Website cookie file must be a regular file of at most 64 KiB, not a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "Website cookie file must be private; run chmod 600 on it"
        );
    }
    let mut value = String::new();
    fs::File::open(path)
        .context("Could not open website cookie file")?
        .take(MAX_COOKIE_BYTES + 1)
        .read_to_string(&mut value)
        .context("Could not read website cookie file as UTF-8")?;
    ensure!(
        value.len() <= MAX_COOKIE_BYTES as usize,
        "Website cookie file exceeds 64 KiB"
    );
    cookie_header(&value)?;
    Ok(value)
}

fn unknown_outcome(message: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{message}. The write outcome is unknown: read module scores before retrying; do not blindly repeat the write"
    )
}

#[cfg(test)]
mod configuration_tests {
    use super::validate_installation_config;

    #[test]
    fn custom_api_requires_both_explicit_website_settings() {
        let custom_api = Some("https://private.example/api");
        for (web, cookie) in [
            (None, None),
            (Some("https://ulearn.me"), None),
            (None, Some("/private/session.cookie")),
        ] {
            assert!(validate_installation_config(custom_api, web, cookie).is_err());
        }
        // An intentional API/website pairing is permitted, including production web.
        assert!(
            validate_installation_config(
                custom_api,
                Some("https://ulearn.me"),
                Some("/private/session.cookie")
            )
            .is_ok()
        );
    }

    #[test]
    fn production_api_allows_default_website_configuration() {
        for api in [
            None,
            Some("https://api.ulearn.me"),
            Some("https://api.ulearn.me/"),
        ] {
            assert!(validate_installation_config(api, None, None).is_ok());
        }
    }
}
