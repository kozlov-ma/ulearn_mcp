//! Legacy grade writes use the same pinned session that authenticated API preflight.
//! No independent cookie source, redirects, or automatic mutation retries.
use crate::session::SessionSnapshot;
use anyhow::{Result, bail, ensure};
use reqwest::{Client, StatusCode, Url, header};
use serde_json::{Value, json};
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct UlearnWebClient {
    client: Client,
    base_url: Url,
    session: SessionSnapshot,
}

impl UlearnWebClient {
    /// Only an authenticated, immutable snapshot from UlearnClient::pin_session
    /// may supply the website session; secrets never enter MCP arguments.
    pub fn from_session(session: SessionSnapshot) -> Result<Self> {
        let base_url =
            Url::parse(session.web_url()).map_err(|_| anyhow::anyhow!("Invalid website origin"))?;
        validate_base_url(&base_url)?;
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
            session,
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
        let cookie = self.session.cookie_header()?;
        let mut url = self.base_url.clone();
        url.set_path("/Admin/SetAdditionalScore");
        url.query_pairs_mut().extend_pairs([
            ("courseId", course_id),
            ("unitId", unit_id),
            ("userId", user_id),
            ("scoringGroupId", scoring_group_id),
        ]);
        let score_text = score.map(|value| value.to_string()).unwrap_or_default();
        // Keep preflight and mutation on the same validated account and session.
        // Never reload a replacement cookie during an in-flight grade operation.
        self.session.ensure_current()?;
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
                "Website redirected the score request; refresh the website session with bootstrap-session --replace. Redirect was not followed; inspect module scores before retrying"
            );
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            bail!(
                "Website session is expired or lacks permission; use bootstrap-session --replace to replace the session"
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

fn unknown_outcome(message: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{message}. The write outcome is unknown: read module scores before retrying; do not blindly repeat the write"
    )
}
