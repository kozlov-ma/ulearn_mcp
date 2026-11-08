//! Configuration output must be usable before authentication and contain no credentials.
use serde_json::Value;
use std::{fs, path::Path, process::Command};
use tempfile::tempdir;

fn command(config_root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"));
    command
        .arg("config")
        .env_clear()
        .env("XDG_CONFIG_HOME", config_root);
    command
}

#[test]
fn default_config_is_pure_json_with_absolute_paths_without_creating_files() {
    let temporary = tempdir().unwrap();
    let config_root = temporary.path().join("not-created-yet");
    let output = command(&config_root).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: Value =
        serde_json::from_slice(&output.stdout).expect("stdout must contain only JSON");
    let server = &config["mcpServers"]["ulearn"];
    let binary = Path::new(server["command"].as_str().unwrap());
    assert!(binary.is_absolute());
    assert_eq!(
        fs::canonicalize(binary).unwrap(),
        fs::canonicalize(env!("CARGO_BIN_EXE_ulearn-mcp")).unwrap()
    );
    assert_eq!(
        server["env"]["ULEARN_TOKEN_FILE"],
        config_root
            .join("ulearn-mcp/ulearn.token")
            .to_str()
            .unwrap()
    );
    assert_eq!(server["env"]["ULEARN_API_URL"], "https://api.ulearn.me");
    assert!(
        !config_root.exists(),
        "config must not perform bootstrap or create credential directories"
    );
    assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 0);
}

#[test]
fn explicit_paths_and_api_are_preserved_without_reading_or_echoing_credentials() {
    let temporary = tempdir().unwrap();
    let config_root = temporary.path().join("config root");
    let token_file = temporary
        .path()
        .join("private tokens")
        .join("teaching account.token");
    let secret = "synthetic-token-must-never-appear";
    let api = "http://127.0.0.1:9/custom-api/";
    let output = command(&config_root)
        .env("ULEARN_TOKEN_FILE", &token_file)
        .env("ULEARN_API_URL", api)
        .env("ULEARN_TOKEN", secret)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: Value =
        serde_json::from_slice(&output.stdout).expect("stdout must contain only JSON");
    let env = &config["mcpServers"]["ulearn"]["env"];
    assert_eq!(env["ULEARN_TOKEN_FILE"], token_file.to_str().unwrap());
    assert_eq!(env["ULEARN_API_URL"], api);
    assert!(env.get("ULEARN_TOKEN").is_none());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    assert!(!token_file.exists());
    assert!(!config_root.exists());
    assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 0);
}

#[test]
fn relative_token_path_is_rejected_without_secret_leak_or_partial_json() {
    let temporary = tempdir().unwrap();
    let secret = "synthetic-token-must-never-appear";
    let output = command(temporary.path())
        .env("ULEARN_TOKEN_FILE", "relative/private.token")
        .env("ULEARN_TOKEN", secret)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("absolute"),
        "Unexpected validation error: {error}"
    );
    assert!(!error.contains(secret));
    assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 0);
}

#[test]
fn refresh_settings_are_preserved_without_reading_secrets() {
    let temporary = tempdir().unwrap();
    let output = command(temporary.path())
        .env("ULEARN_TOKEN_REFRESH_DAYS", "7")
        .env("ULEARN_AUTO_REFRESH_TOKEN", "false")
        .env("ULEARN_TOKEN", "never-serialize-this-secret")
        .output()
        .unwrap();
    assert!(output.status.success());
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    let environment = &config["mcpServers"]["ulearn"]["env"];
    assert_eq!(environment["ULEARN_TOKEN_REFRESH_DAYS"], "7");
    assert_eq!(environment["ULEARN_AUTO_REFRESH_TOKEN"], "false");
    assert!(environment.get("ULEARN_TOKEN").is_none());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("never-serialize-this-secret"));
    assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 0);
}

#[test]
fn invalid_optional_settings_fail_without_partial_configuration() {
    let temporary = tempdir().unwrap();
    for (name, value) in [
        ("ULEARN_AUTO_REFRESH_TOKEN", "maybe"),
        ("ULEARN_TOKEN_REFRESH_DAYS", "0"),
    ] {
        let output = command(temporary.path()).env(name, value).output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
    }
}

#[test]
fn session_config_before_bootstrap_is_pure_and_replaces_legacy_auth_settings() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("not-created");
    let output = command(&root)
        .arg("--session")
        .env("ULEARN_TOKEN", "never-serialize-this-token")
        .env("ULEARN_TOKEN_FILE", "/old/ulearn.token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    let environment = &config["mcpServers"]["ulearn"]["env"];
    assert_eq!(
        environment["ULEARN_SESSION_FILE"],
        root.join("ulearn-mcp/ulearn.cookie").to_str().unwrap()
    );
    assert_eq!(environment["ULEARN_API_URL"], "https://api.ulearn.me");
    assert_eq!(environment["ULEARN_WEB_URL"], "https://ulearn.me");
    assert!(environment.get("ULEARN_TOKEN").is_none());
    assert!(environment.get("ULEARN_TOKEN_FILE").is_none());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("never-serialize-this-token"));
    assert!(!root.exists());
}

#[test]
fn explicit_session_configuration_does_not_need_home_or_an_existing_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
        .arg("config")
        .env_clear()
        .env("ULEARN_SESSION_FILE", "/missing/private/ulearn.cookie")
        .env("ULEARN_API_URL", "http://127.0.0.1:9/api")
        .env("ULEARN_WEB_URL", "http://127.0.0.1:10")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    let environment = &config["mcpServers"]["ulearn"]["env"];
    assert_eq!(
        environment["ULEARN_SESSION_FILE"],
        "/missing/private/ulearn.cookie"
    );
    assert_eq!(environment["ULEARN_API_URL"], "http://127.0.0.1:9/api");
    assert_eq!(environment["ULEARN_WEB_URL"], "http://127.0.0.1:10");
}

#[test]
fn ambiguous_explicit_authentication_is_rejected_without_partial_config() {
    let dir = tempdir().unwrap();
    let output = command(dir.path())
        .env("ULEARN_SESSION_FILE", "/missing/session.cookie")
        .env("ULEARN_TOKEN", "private-token")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not both"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-token"));
}

#[test]
fn default_session_wins_default_token_but_explicit_token_wins_default_session() {
    let dir = tempdir().unwrap();
    let config_dir = dir.path().join("ulearn-mcp");
    fs::create_dir(&config_dir).unwrap();
    fs::write(
        config_dir.join("ulearn.cookie"),
        "intentionally-invalid-not-read",
    )
    .unwrap();
    fs::write(config_dir.join("ulearn.token"), "private-token").unwrap();
    let output = command(dir.path()).output().unwrap();
    assert!(output.status.success());
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        config["mcpServers"]["ulearn"]["env"]
            .get("ULEARN_SESSION_FILE")
            .is_some()
    );
    let output = command(dir.path())
        .env("ULEARN_TOKEN_FILE", "/explicit/ulearn.token")
        .output()
        .unwrap();
    assert!(output.status.success());
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    let environment = &config["mcpServers"]["ulearn"]["env"];
    assert_eq!(environment["ULEARN_TOKEN_FILE"], "/explicit/ulearn.token");
    assert!(environment.get("ULEARN_SESSION_FILE").is_none());
}

#[test]
fn custom_session_targets_require_an_explicit_destination_pair_and_file() {
    let dir = tempdir().unwrap();
    for environment in [
        vec![("ULEARN_API_URL", "https://api.example.test")],
        vec![
            ("ULEARN_API_URL", "https://api.example.test"),
            ("ULEARN_WEB_URL", "https://example.test"),
        ],
        vec![("ULEARN_WEB_URL", "https://example.test")],
        vec![
            ("ULEARN_WEB_URL", "https://example.test"),
            ("ULEARN_SESSION_FILE", "/missing/session.cookie"),
        ],
    ] {
        let output = command(dir.path())
            .arg("--session")
            .envs(environment)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("explicit"));
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}
