use std::fs;
use tempfile::tempdir;
use ulearn_mcp::web_bootstrap::import_cookie;

#[test]
fn invalid_cookie_does_not_create_credentials() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("private/session.cookie");
    assert!(
        import_cookie(
            "https://ulearn.me",
            "session=a\r\nInjected: b",
            &path,
            false
        )
        .is_err()
    );
    assert!(!path.parent().unwrap().exists());
}

#[test]
fn import_is_private_and_replacement_requires_explicit_flag() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("private/session.cookie");
    import_cookie(
        "https://ulearn.me",
        " Cookie: session=old; other=test ",
        &path,
        false,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "session=old; other=test"
    );
    assert!(import_cookie("https://ulearn.me", "session=new", &path, false).is_err());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "session=old; other=test"
    );
    import_cookie("https://ulearn.me", "session=new", &path, true).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "session=new");
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_is_never_overwritten() {
    let dir = tempdir().unwrap();
    let target = dir.path().join("original.cookie");
    let link = dir.path().join("session.cookie");
    fs::write(&target, "session=old").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(import_cookie("https://ulearn.me", "session=new", &link, true).is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "session=old");
}

#[test]
fn custom_installations_cannot_overwrite_default_session() {
    use std::process::Command;
    let dir = tempdir().unwrap();
    let parent = dir.path().join("ulearn-mcp");
    fs::create_dir(&parent).unwrap();
    let file = parent.join("ulearn.cookie");
    fs::write(&file, "session=production").unwrap();
    for variable in ["ULEARN_API_URL", "ULEARN_WEB_URL"] {
        let result = Command::new(env!("CARGO_BIN_EXE_ulearn-mcp"))
            .args(["bootstrap-web", "--replace"])
            .env_clear()
            .env("XDG_CONFIG_HOME", dir.path())
            .env(variable, "https://custom.example")
            .output()
            .unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains("requires"), "{error}");
        assert!(!error.contains("session=production"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "session=production");
    }
}
