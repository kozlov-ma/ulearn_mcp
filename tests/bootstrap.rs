//! Token bootstrap must not damage a working credential or persist an invalid one.
use serde_json::json;
use std::{fs, path::Path};
use tempfile::tempdir;
use ulearn_mcp::bootstrap::import_token;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

async fn account(mock: &MockServer, token: &str, status: u16) {
    Mock::given(method("GET"))
        .and(path("/account"))
        .and(header("authorization", format!("Bearer {token}")))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_json(json!({"id":"instructor-test","login":"teacher"})),
        )
        .expect(1)
        .mount(mock)
        .await;
}

#[cfg(unix)]
fn assert_mode(path: &Path, expected: u32) {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        expected
    );
}

#[tokio::test]
async fn invalid_token_creates_neither_file_nor_parent_directory() {
    let dir = tempdir().unwrap();
    let parent = dir.path().join("not-yet-created");
    let file = parent.join("ulearn.token");
    let mock = MockServer::start().await;
    account(&mock, "invalid-token", 401).await;
    assert!(
        import_token(&mock.uri(), "invalid-token", &file, false)
            .await
            .is_err()
    );
    assert!(!file.exists());
    assert!(!parent.exists());
}

#[tokio::test]
async fn successful_import_trims_raw_token_and_secures_new_files() {
    let dir = tempdir().unwrap();
    let parent = dir.path().join("private");
    let file = parent.join("ulearn.token");
    let mock = MockServer::start().await;
    account(&mock, "valid-token", 200).await;
    import_token(&mock.uri(), "  valid-token\r\n", &file, false)
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap().trim(), "valid-token");
    #[cfg(unix)]
    {
        assert_mode(&parent, 0o700);
        assert_mode(&file, 0o600);
    }
}

#[tokio::test]
async fn successful_import_accepts_bearer_header_value() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.token");
    let mock = MockServer::start().await;
    account(&mock, "valid-token", 200).await;
    import_token(&mock.uri(), "  Bearer valid-token\n", &file, false)
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(file).unwrap().trim(), "valid-token");
}

#[tokio::test]
async fn existing_file_refusal_performs_no_network_request() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.token");
    fs::write(&file, "existing-token").unwrap();
    let mock = MockServer::start().await;
    assert!(
        import_token(&mock.uri(), "new-token", &file, false)
            .await
            .is_err()
    );
    assert_eq!(fs::read_to_string(file).unwrap(), "existing-token");
    assert!(mock.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn invalid_replacement_preserves_existing_token() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.token");
    fs::write(&file, "existing-token").unwrap();
    let mock = MockServer::start().await;
    account(&mock, "invalid-token", 401).await;
    assert!(
        import_token(&mock.uri(), "invalid-token", &file, true)
            .await
            .is_err()
    );
    assert_eq!(fs::read_to_string(file).unwrap(), "existing-token");
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "No temporary credential files should remain"
    );
}

#[tokio::test]
async fn valid_replacement_saves_new_token_with_private_permissions() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("ulearn.token");
    fs::write(&file, "existing-token").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    }
    let mock = MockServer::start().await;
    account(&mock, "replacement-token", 200).await;
    import_token(&mock.uri(), "replacement-token", &file, true)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(&file).unwrap().trim(),
        "replacement-token"
    );
    #[cfg(unix)]
    assert_mode(&file, 0o600);
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "Atomic replacement should clean temporary files"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_target_is_rejected_without_modifying_destination_or_contacting_api() {
    use std::os::unix::fs::symlink;
    let dir = tempdir().unwrap();
    let actual = dir.path().join("actual.token");
    let link = dir.path().join("ulearn.token");
    fs::write(&actual, "existing-token").unwrap();
    symlink(&actual, &link).unwrap();
    let mock = MockServer::start().await;
    for replace in [false, true] {
        assert!(
            import_token(&mock.uri(), "new-token", &link, replace)
                .await
                .is_err()
        );
    }
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(actual).unwrap(), "existing-token");
    assert!(mock.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn directory_target_is_rejected_before_network_access() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("ulearn.token");
    fs::create_dir(&target).unwrap();
    let mock = MockServer::start().await;
    assert!(
        import_token(&mock.uri(), "new-token", &target, true)
            .await
            .is_err()
    );
    assert!(target.is_dir());
    assert!(mock.received_requests().await.unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn check_loads_default_token_file_and_rejects_public_permissions_before_network() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    let dir = tempdir().unwrap();
    let private = dir.path().join("ulearn-mcp");
    fs::create_dir(&private).unwrap();
    let file = private.join("ulearn.token");
    fs::write(&file, "synthetic-default-token").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
        .arg("--check")
        .env_clear()
        .env("XDG_CONFIG_HOME", dir.path())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("must not be readable by other users"),
        "Unexpected error: {error}"
    );
    assert!(!error.contains("synthetic-default-token"));
}

#[tokio::test]
async fn custom_api_does_not_receive_implicit_default_production_token() {
    use std::process::Command;
    let dir = tempdir().unwrap();
    let private = dir.path().join("ulearn-mcp");
    fs::create_dir(&private).unwrap();
    let file = private.join("ulearn.token");
    fs::write(&file, "synthetic-production-token").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"custom-account"})))
        .expect(1)
        .mount(&mock)
        .await;
    let api = mock.uri();
    let config = dir.path().to_owned();
    let result = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
            .arg("--check")
            .env_clear()
            .env("XDG_CONFIG_HOME", config)
            .env("ULEARN_API_URL", api)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].headers.get("authorization").is_none(),
        "Default production credential must never reach a custom API"
    );
    assert!(!String::from_utf8_lossy(&result.stdout).contains("synthetic-production-token"));
}
