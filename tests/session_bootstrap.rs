use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};
use tempfile::tempdir;
use ulearn_mcp::session_bootstrap::import_session;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

async fn valid_session(api: &MockServer, cookie: &str) {
    let expires = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let token = format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(json!({"exp":expires}).to_string())
    );
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .and(header("cookie", cookie))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":token})))
        .expect(1)
        .mount(api)
        .await;
    Mock::given(method("GET")).and(path("/account"))
        .and(header("authorization", format!("Bearer {token}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "isAuthenticated":true,"user":{"id":"teacher-id","visibleName":"Teacher","login":"teacher"}
        })))
        .mount(api).await;
}

#[tokio::test]
async fn verified_import_saves_only_cookie_and_returns_account_metadata() {
    let dir = tempdir().unwrap();
    let parent = dir.path().join("private");
    let file = parent.join("ulearn.cookie");
    let api = MockServer::start().await;
    valid_session(&api, "session=private-value").await;
    let result = import_session(
        &api.uri(),
        &api.uri(),
        "  Cookie: session=private-value\r\n",
        &file,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        json!({"account_id":"teacher-id","name":"Teacher","login":"teacher"})
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "session=private-value");
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for request in api.received_requests().await.unwrap() {
        if request.url.path() == "/account/token" {
            assert!(request.headers.get("authorization").is_none());
        } else {
            assert!(request.headers.get("cookie").is_none());
        }
    }
}

#[tokio::test]
async fn rejected_session_does_not_create_files_or_leak_response() {
    let dir = tempdir().unwrap();
    let parent = dir.path().join("not-created");
    let file = parent.join("ulearn.cookie");
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .respond_with(ResponseTemplate::new(401).set_body_string("private-response-value"))
        .expect(1)
        .mount(&api)
        .await;
    let error = import_session(
        &api.uri(),
        &api.uri(),
        "session=private-cookie",
        &file,
        false,
    )
    .await
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(!error.contains("private-cookie"));
    assert!(!error.contains("private-response-value"));
    assert!(error.contains("bootstrap-session --replace"));
    assert!(!parent.exists());
}

#[tokio::test]
async fn invalid_replacement_preserves_existing_session() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.cookie");
    fs::write(&file, "session=old").unwrap();
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&api)
        .await;
    assert!(
        import_session(&api.uri(), &api.uri(), "session=rejected", &file, true)
            .await
            .is_err()
    );
    assert_eq!(fs::read_to_string(file).unwrap(), "session=old");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn existing_destination_requires_replace_before_network_access() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.cookie");
    fs::write(&file, "session=old").unwrap();
    let api = MockServer::start().await;
    assert!(
        import_session(&api.uri(), &api.uri(), "session=new", &file, false)
            .await
            .is_err()
    );
    assert_eq!(fs::read_to_string(file).unwrap(), "session=old");
    assert!(api.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn verified_replacement_is_private_and_leaves_no_temporary_files() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.cookie");
    fs::write(&file, "session=old").unwrap();
    let api = MockServer::start().await;
    valid_session(&api, "session=new").await;
    import_session(&api.uri(), &api.uri(), "session=new", &file, true)
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "session=new");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_destination_is_rejected_before_network_access() {
    use std::os::unix::fs::symlink;
    let dir = tempdir().unwrap();
    let actual = dir.path().join("actual.cookie");
    let link = dir.path().join("ulearn.cookie");
    fs::write(&actual, "session=old").unwrap();
    symlink(&actual, &link).unwrap();
    let api = MockServer::start().await;
    assert!(
        import_session(&api.uri(), &api.uri(), "session=new", &link, true)
            .await
            .is_err()
    );
    assert_eq!(fs::read_to_string(actual).unwrap(), "session=old");
    assert!(api.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn valid_exchange_without_account_identity_does_not_save_cookie() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.cookie");
    let api = MockServer::start().await;
    let expires = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let token = format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(json!({"exp":expires}).to_string())
    );
    Mock::given(method("POST"))
        .and(path("/account/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token":token})))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"isAuthenticated":false})))
        .mount(&api)
        .await;
    assert!(
        import_session(&api.uri(), &api.uri(), "session=anonymous", &file, false)
            .await
            .is_err()
    );
    assert!(!file.exists());
}
