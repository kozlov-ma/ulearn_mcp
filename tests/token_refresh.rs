use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::tempdir;
use ulearn_mcp::client::UlearnClient;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn token(expires: u64) -> String {
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(json!({"exp":expires,"sub":"teacher"}).to_string())
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
async fn renewal(mock: &MockServer, previous: &str, next: &str) {
    Mock::given(method("POST"))
        .and(path("/account/api-token"))
        .and(query_param("days", "30"))
        .and(header("authorization", format!("Bearer {previous}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":next})))
        .expect(1)
        .mount(mock)
        .await;
}

#[tokio::test]
async fn concurrent_requests_refresh_once_and_use_new_token_without_leaking_it() {
    let mock = MockServer::start().await;
    let old = token(now() + 100);
    let new = token(now() + 86400);
    renewal(&mock, &old, &new).await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .and(header("authorization", format!("Bearer {new}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"teacher"})))
        .expect(2)
        .mount(&mock)
        .await;
    let client = UlearnClient::new(&mock.uri(), Some(old)).unwrap();
    let clone = client.clone();
    let (a, b) = tokio::join!(client.get(&["account"], &[]), clone.get(&["account"], &[]));
    assert_eq!(a.unwrap()["id"], "teacher");
    assert_eq!(b.unwrap()["id"], "teacher");
    assert_eq!(mock.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn refresh_persists_private_token_and_hot_reloads_external_replacement() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.token");
    let mock = MockServer::start().await;
    let old = token(now() + 100);
    let new = token(now() + 86400);
    save(&file, &old);
    renewal(&mock, &old, &new).await;
    let client = UlearnClient::from_token_file(&mock.uri(), file.clone()).unwrap();
    let result = client.refresh_token(None).await.unwrap();
    assert_eq!(result["persisted"], true);
    assert!(!result.to_string().contains(&new));
    assert_eq!(fs::read_to_string(&file).unwrap(), new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let replacement = token(now() + 172800);
    save(&file, &replacement);
    Mock::given(method("GET"))
        .and(path("/account"))
        .and(header("authorization", format!("Bearer {replacement}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"teacher"})))
        .expect(1)
        .mount(&mock)
        .await;
    client.get(&["account"], &[]).await.unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn expired_token_requires_login_without_network_or_student_write() {
    let mock = MockServer::start().await;
    let client = UlearnClient::new(&mock.uri(), Some(token(now() - 1))).unwrap();
    let error = client
        .post(&["reviews"], &[], Some(json!({"text":"comment"})))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("bootstrap --replace"));
    assert!(mock.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn invalid_refresh_response_preserves_file_and_does_not_send_original_write() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.token");
    let mock = MockServer::start().await;
    let old = token(now() + 100);
    save(&file, &old);
    renewal(&mock, &old, "private-invalid-secret").await;
    let client = UlearnClient::from_token_file(&mock.uri(), file.clone()).unwrap();
    let error = client
        .post(&["reviews"], &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("private-invalid-secret"));
    assert_eq!(fs::read_to_string(file).unwrap(), old);
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn explicit_refresh_days_are_validated_and_401_never_replays_original_write() {
    let mock = MockServer::start().await;
    let client = UlearnClient::new(&mock.uri(), Some(token(now() + 86400))).unwrap();
    for days in [0, 366] {
        assert!(client.refresh_token(Some(days)).await.is_err());
    }
    assert!(mock.received_requests().await.unwrap().is_empty());
    Mock::given(method("POST"))
        .and(path("/reviews"))
        .respond_with(ResponseTemplate::new(401).set_body_string("secret-server-response"))
        .expect(1)
        .mount(&mock)
        .await;
    let error = client
        .post(&["reviews"], &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("401"));
    assert!(!error.contains("secret-server-response"));
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn failed_renewal_is_not_retried_or_allowed_to_send_original_operation() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/account/api-token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("private-server-body"))
        .expect(1)
        .mount(&mock)
        .await;
    let client = UlearnClient::new(&mock.uri(), Some(token(now() + 60))).unwrap();
    let error = client.post(&["reviews"], &[], None).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("original operation was not sent")
    );
    assert!(!format!("{error:#}").contains("private-server-body"));
    assert_eq!(mock.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn external_replacement_during_refresh_is_not_overwritten() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.token");
    let mock = MockServer::start().await;
    let old = token(now() + 100);
    let new = token(now() + 86400);
    let external = token(now() + 172800);
    save(&file, &old);
    let replaced_file = file.clone();
    let replaced_token = external.clone();
    Mock::given(method("POST"))
        .and(path("/account/api-token"))
        .respond_with(move |_: &wiremock::Request| {
            save(&replaced_file, &replaced_token);
            ResponseTemplate::new(200).set_body_json(json!({"token":new}))
        })
        .expect(1)
        .mount(&mock)
        .await;
    let client = UlearnClient::from_token_file(&mock.uri(), file.clone()).unwrap();
    assert!(
        client
            .refresh_token(None)
            .await
            .unwrap_err()
            .to_string()
            .contains("changed during renewal")
    );
    assert_eq!(fs::read_to_string(file).unwrap(), external);
}

#[tokio::test]
async fn cli_refresh_saves_token_and_prints_only_metadata() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.token");
    let mock = MockServer::start().await;
    let old = token(now() + 86400);
    let new = token(now() + 172800);
    save(&file, &old);
    Mock::given(method("POST"))
        .and(path("/account/api-token"))
        .and(query_param("days", "7"))
        .and(header("authorization", format!("Bearer {old}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":new})))
        .expect(1)
        .mount(&mock)
        .await;
    let api = mock.uri();
    let token_path = file.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
            .args(["refresh-token", "--days", "7"])
            .env_clear()
            .env("ULEARN_TOKEN_FILE", token_path)
            .env("ULEARN_API_URL", api)
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
    assert_eq!(fs::read_to_string(file).unwrap(), new);
    for value in [&old, &new] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(value));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(value));
    }
}

#[tokio::test]
async fn forbidden_auto_renewal_keeps_valid_reads_and_is_suppressed_for_unchanged_token() {
    let mock = MockServer::start().await;
    let old = token(now() + 100);
    Mock::given(method("POST"))
        .and(path("/account/api-token"))
        .respond_with(ResponseTemplate::new(403).set_body_string("private-forbidden-body"))
        .expect(2)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/courses"))
        .and(header("authorization", format!("Bearer {old}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"courses":[]})))
        .expect(2)
        .mount(&mock)
        .await;
    let client = UlearnClient::new(&mock.uri(), Some(old)).unwrap();
    client.get(&["courses"], &[]).await.unwrap();
    client.clone().get(&["courses"], &[]).await.unwrap();
    assert_eq!(mock.received_requests().await.unwrap().len(), 3);
    // Explicit renewal must still report the permission error instead of faking success.
    let error = client.refresh_token(None).await.unwrap_err();
    assert!(format!("{error:#}").contains("403"));
    assert!(!format!("{error:#}").contains("private-forbidden-body"));
    assert_eq!(mock.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn forbidden_renewal_is_reconsidered_after_token_file_changes() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.token");
    let mock = MockServer::start().await;
    let old = token(now() + 100);
    save(&file, &old);
    Mock::given(method("POST"))
        .and(path("/account/api-token"))
        .and(header("authorization", format!("Bearer {old}")))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/courses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"courses":[]})))
        .expect(2)
        .mount(&mock)
        .await;
    let client = UlearnClient::from_token_file(&mock.uri(), file.clone()).unwrap();
    client.get(&["courses"], &[]).await.unwrap();
    let replacement = token(now() + 150);
    let refreshed = token(now() + 86400);
    save(&file, &replacement);
    renewal(&mock, &replacement, &refreshed).await;
    client.get(&["courses"], &[]).await.unwrap();
    assert_eq!(fs::read_to_string(file).unwrap(), refreshed);
}
