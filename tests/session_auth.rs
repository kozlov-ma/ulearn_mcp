use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::tempdir;
use ulearn_mcp::{client::UlearnClient, session::UlearnSession};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn token(expiry: u64, suffix: &str) -> String {
    format!(
        "e30.{}.{}",
        URL_SAFE_NO_PAD.encode(json!({"exp":expiry}).to_string()),
        suffix
    )
}
fn save(path: &Path, value: &str) {
    fs::write(path, value).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}
async fn auth(api: &MockServer, cookie: &str, bearer: &str, account_id: &str) {
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .and(header("cookie", cookie))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":bearer})))
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .and(header("authorization", format!("Bearer {bearer}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"isAuthenticated":true,"user":{"id":account_id}})),
        )
        .expect(1)
        .mount(api)
        .await;
}
async fn courses(api: &MockServer, bearer: &str, count: u64) {
    Mock::given(method("GET"))
        .and(path("/courses"))
        .and(header("authorization", format!("Bearer {bearer}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"courses":[]})))
        .expect(count)
        .mount(api)
        .await;
}

#[tokio::test]
async fn exchanges_cookie_once_and_uses_only_bearer_for_concurrent_api_calls() {
    let api = MockServer::start().await;
    let bearer = token(now() + 3600, "test-signature");
    auth(&api, "session=private", &bearer, "teacher").await;
    courses(&api, &bearer, 2).await;
    let session =
        UlearnSession::from_cookie(&api.uri(), &api.uri(), "session=private".into()).unwrap();
    let client = UlearnClient::from_session(session).unwrap();
    let clone = client.clone();
    let (first, second) = tokio::join!(client.get(&["courses"], &[]), clone.get(&["courses"], &[]));
    first.unwrap();
    second.unwrap();
    let requests = api.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].headers.get("authorization").is_none());
    for request in &requests[1..] {
        assert!(request.headers.get("cookie").is_none());
    }
}

#[tokio::test]
async fn refresh_from_session_returns_metadata_without_persisting_or_disclosing_token() {
    let api = MockServer::start().await;
    let bearer = token(now() + 3600, "private-signature");
    auth(&api, "session=private-cookie", &bearer, "teacher").await;
    let client = UlearnClient::from_session(
        UlearnSession::from_cookie(&api.uri(), &api.uri(), "session=private-cookie".into())
            .unwrap(),
    )
    .unwrap();
    assert!(client.refresh_token(Some(30)).await.is_err());
    let result = client.refresh_token(None).await.unwrap();
    assert_eq!(result["authentication"], "session");
    assert_eq!(result["persisted"], false);
    assert_eq!(result["account_id"], "teacher");
    assert!(!result.to_string().contains("private"));
}

#[tokio::test]
async fn same_account_session_replacement_reauthenticates_and_invalidates_pinned_operation() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("session.cookie");
    let api = MockServer::start().await;
    let first = token(now() + 3600, "first");
    let second = token(now() + 3600, "second");
    save(&file, "session=first");
    auth(&api, "session=first", &first, "teacher").await;
    auth(&api, "session=second", &second, "teacher").await;
    courses(&api, &second, 1).await;
    let client = UlearnClient::from_session_file(&api.uri(), &api.uri(), file.clone()).unwrap();
    let pinned = client.pin_session().await.unwrap();
    let snapshot = pinned.session_snapshot().unwrap();
    save(&file, "session=second");
    assert!(snapshot.ensure_current().is_err());
    assert!(
        pinned
            .post(&["reviews"], &[], None)
            .await
            .unwrap_err()
            .to_string()
            .contains("session changed")
    );
    client.get(&["courses"], &[]).await.unwrap();
    assert!(
        api.received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() != "/reviews")
    );
}

#[tokio::test]
async fn replacing_session_with_different_account_cannot_silently_send_original_write() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("session.cookie");
    let api = MockServer::start().await;
    let first = token(now() + 3600, "first");
    let second = token(now() + 3600, "second");
    save(&file, "session=first");
    auth(&api, "session=first", &first, "teacher-a").await;
    auth(&api, "session=second", &second, "teacher-b").await;
    let client = UlearnClient::from_session_file(&api.uri(), &api.uri(), file.clone()).unwrap();
    client.pin_session().await.unwrap();
    save(&file, "session=second");
    let error = client
        .post(&["reviews"], &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("different account"));
    assert!(
        api.received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() != "/reviews")
    );
}

#[tokio::test]
async fn expired_or_redirected_session_never_forwards_cookie_or_sends_original_operation() {
    for status in [401, 302, 500] {
        let api = MockServer::start().await;
        let destination = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/account/token"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Location", destination.uri())
                    .set_body_string("private-server-response"),
            )
            .expect(1)
            .mount(&api)
            .await;
        let client = UlearnClient::from_session(
            UlearnSession::from_cookie(&api.uri(), &api.uri(), "session=private".into()).unwrap(),
        )
        .unwrap();
        let error = client.post(&["reviews"], &[], None).await.unwrap_err();
        assert!(!format!("{error:#}").contains("private"));
        assert_eq!(api.received_requests().await.unwrap().len(), 1);
        assert!(destination.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn token_is_verified_before_use_and_original_401_is_never_replayed() {
    let api = MockServer::start().await;
    let bearer = token(now() + 3600, "private");
    auth(&api, "session=valid", &bearer, "teacher").await;
    Mock::given(method("POST"))
        .and(path("/reviews"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&api)
        .await;
    let client = UlearnClient::from_session(
        UlearnSession::from_cookie(&api.uri(), &api.uri(), "session=valid".into()).unwrap(),
    )
    .unwrap();
    assert!(client.post(&["reviews"], &[], None).await.is_err());
    assert_eq!(api.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn invalid_derived_token_or_unverified_identity_does_not_authorize_operation() {
    for invalid_token in [true, false] {
        let api = MockServer::start().await;
        let bearer = if invalid_token {
            token(now() - 1, "expired")
        } else {
            token(now() + 3600, "unverified")
        };
        Mock::given(method("POST"))
            .and(path("/account/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":bearer})))
            .expect(1)
            .mount(&api)
            .await;
        if !invalid_token {
            Mock::given(method("GET"))
                .and(path("/account"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"isAuthenticated":false})),
                )
                .expect(1)
                .mount(&api)
                .await;
        }
        let client = UlearnClient::from_session(
            UlearnSession::from_cookie(&api.uri(), &api.uri(), "session=invalid".into()).unwrap(),
        )
        .unwrap();
        assert!(client.post(&["reviews"], &[], None).await.is_err());
        assert!(
            api.received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| r.url.path() != "/reviews")
        );
    }
}

#[tokio::test]
async fn session_file_replacement_during_exchange_is_rejected() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("session.cookie");
    let api = MockServer::start().await;
    let bearer = token(now() + 3600, "token");
    save(&file, "session=first");
    let changed = file.clone();
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .respond_with(move |_: &wiremock::Request| {
            save(&changed, "session=second");
            ResponseTemplate::new(200).set_body_json(json!({"token":bearer}))
        })
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"isAuthenticated":true,"user":{"id":"teacher"}})),
        )
        .expect(1)
        .mount(&api)
        .await;
    let client = UlearnClient::from_session_file(&api.uri(), &api.uri(), file).unwrap();
    assert!(
        client
            .post(&["reviews"], &[], None)
            .await
            .unwrap_err()
            .to_string()
            .contains("session changed")
    );
    assert_eq!(api.received_requests().await.unwrap().len(), 2);
}

#[cfg(unix)]
#[test]
fn session_file_requires_private_permissions_and_rejects_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempdir().unwrap();
    let file = dir.path().join("session.cookie");
    let link = dir.path().join("link");
    save(&file, "session=private");
    symlink(&file, &link).unwrap();
    assert!(UlearnSession::from_file("https://api.ulearn.me", "https://ulearn.me", link).is_err());
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(UlearnSession::from_file("https://api.ulearn.me", "https://ulearn.me", file).is_err());
}

#[test]
fn explicit_session_and_token_are_rejected_before_network_or_secret_output() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
        .arg("--check")
        .env_clear()
        .env("ULEARN_SESSION_FILE", "/missing/session.cookie")
        .env("ULEARN_TOKEN", "private-token")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not both"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-token"));
}

#[test]
fn default_session_takes_precedence_over_default_token_without_leaking_either() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("ulearn-mcp");
    fs::create_dir(&config).unwrap();
    save(
        &config.join("ulearn.cookie"),
        "invalid-cookie-without-equals",
    );
    save(&config.join("ulearn.token"), "private-token");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
        .arg("--check")
        .env_clear()
        .env("XDG_CONFIG_HOME", dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Cookie header"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-token"));
}

#[tokio::test]
async fn custom_api_does_not_receive_implicit_default_session_or_token() {
    let dir = tempdir().unwrap();
    let config = dir.path().join("ulearn-mcp");
    fs::create_dir(&config).unwrap();
    save(&config.join("ulearn.cookie"), "session=private");
    save(&config.join("ulearn.token"), "private-token");
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"public":true})))
        .expect(1)
        .mount(&api)
        .await;
    let url = api.uri();
    let root = dir.path().to_owned();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
            .arg("--check")
            .env_clear()
            .env("XDG_CONFIG_HOME", root)
            .env("ULEARN_API_URL", url)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success());
    let requests = api.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].headers.get("cookie").is_none());
    assert!(requests[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn short_lived_session_token_is_cached_until_its_adaptive_renewal_deadline() {
    let api = MockServer::start().await;
    let bearer = token(now() + 60, "short-lived");
    auth(&api, "session=short", &bearer, "teacher").await;
    courses(&api, &bearer, 2).await;
    let client = UlearnClient::from_session(
        UlearnSession::from_cookie(&api.uri(), &api.uri(), "session=short".into()).unwrap(),
    )
    .unwrap();
    client.get(&["courses"], &[]).await.unwrap();
    client.get(&["courses"], &[]).await.unwrap();
    assert_eq!(api.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn explicit_session_file_authenticates_without_home_or_xdg_configuration() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("session.cookie");
    save(&file, "session=explicit");
    let api = MockServer::start().await;
    let bearer = token(now() + 3600, "explicit");
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .and(header("cookie", "session=explicit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":bearer})))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .and(header("authorization", format!("Bearer {bearer}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"isAuthenticated":true,"user":{"id":"teacher"}})),
        )
        .expect(2)
        .mount(&api)
        .await;
    let url = api.uri();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
            .arg("--check")
            .env_clear()
            .env("ULEARN_SESSION_FILE", file)
            .env("ULEARN_API_URL", &url)
            .env("ULEARN_WEB_URL", &url)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&bearer));
}

#[tokio::test]
async fn original_401_evicts_derived_token_without_replaying_write_and_next_call_reauthenticates() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let api = MockServer::start().await;
    let first = token(now() + 3600, "rejected");
    let second = token(now() + 3600, "fresh");
    let count = Arc::new(AtomicUsize::new(0));
    let first_response = first.clone();
    let second_response = second.clone();
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .respond_with(move |_: &wiremock::Request| {
            let token = if count.fetch_add(1, Ordering::SeqCst) == 0 {
                &first_response
            } else {
                &second_response
            };
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
    Mock::given(method("POST"))
        .and(path("/reviews"))
        .and(header("authorization", format!("Bearer {first}")))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&api)
        .await;
    courses(&api, &second, 1).await;
    let client = UlearnClient::from_session(
        UlearnSession::from_cookie(&api.uri(), &api.uri(), "session=valid".into()).unwrap(),
    )
    .unwrap();
    assert!(client.post(&["reviews"], &[], None).await.is_err());
    assert_eq!(api.received_requests().await.unwrap().len(), 3);
    client.get(&["courses"], &[]).await.unwrap();
    let requests = api.received_requests().await.unwrap();
    assert_eq!(requests.len(), 6);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/reviews")
            .count(),
        1
    );
}

#[test]
fn custom_website_requires_explicit_api_before_reading_or_sending_session() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
        .arg("--check")
        .env_clear()
        .env("ULEARN_SESSION_FILE", "/missing/private.cookie")
        .env("ULEARN_WEB_URL", "https://custom.example")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires explicit ULEARN_API_URL"));
    assert!(output.stdout.is_empty());
}
