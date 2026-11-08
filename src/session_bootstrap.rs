//! One user-supplied website session supplies both API and gradebook authentication.
use crate::{bootstrap, client::UlearnClient, session::UlearnSession};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

pub const DEFAULT_WEB_URL: &str = "https://ulearn.me";

pub fn default_session_path() -> Result<PathBuf> {
    Ok(bootstrap::default_token_path()?.with_file_name("ulearn.cookie"))
}

pub(crate) fn optional_env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => anyhow::bail!("{name} is not valid UTF-8"),
    }
}

/// Resolve configuration without reading any credential contents.
pub fn settings(explicit: Option<PathBuf>) -> Result<(String, String, PathBuf)> {
    let explicit_api = optional_env("ULEARN_API_URL")?;
    let api = explicit_api
        .clone()
        .unwrap_or_else(|| bootstrap::DEFAULT_API_URL.into());
    let web = optional_env("ULEARN_WEB_URL")?;
    let path = explicit.or_else(|| std::env::var_os("ULEARN_SESSION_FILE").map(PathBuf::from));
    if api.trim_end_matches('/') != bootstrap::DEFAULT_API_URL {
        ensure!(
            web.is_some() && path.is_some(),
            "Custom ULEARN_API_URL requires explicit ULEARN_WEB_URL and session file"
        );
    }
    let web = web.unwrap_or_else(|| DEFAULT_WEB_URL.into());
    if web.trim_end_matches('/') != DEFAULT_WEB_URL {
        ensure!(
            explicit_api.is_some() && path.is_some(),
            "Custom ULEARN_WEB_URL requires explicit ULEARN_API_URL and session file"
        );
    }
    let path = path.map(Ok).unwrap_or_else(default_session_path)?;
    ensure!(path.is_absolute(), "Session file path must be absolute");
    UlearnSession::from_cookie(&api, &web, "validation=placeholder".into())?;
    Ok((api, web, path))
}

pub fn configuration(api: &str, web: &str, path: &Path) -> Result<Value> {
    ensure!(path.is_absolute(), "Session file path must be absolute");
    UlearnSession::from_cookie(api, web, "validation=placeholder".into())?;
    let binary = std::env::current_exe()?;
    Ok(json!({"mcpServers":{"ulearn":{"command":binary,"env":{
        "ULEARN_API_URL":api,"ULEARN_WEB_URL":web,"ULEARN_SESSION_FILE":path
    }}}}))
}

pub fn configuration_from_env() -> Result<Value> {
    let (api, web, path) = settings(None)?;
    configuration(&api, &web, &path)
}

fn check_destination(path: &Path, replace: bool) -> Result<()> {
    ensure!(path.is_absolute(), "Session file path must be absolute");
    ensure!(
        path.file_name().is_some(),
        "Session file path must include a filename"
    );
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "Session destination must be a regular file, not a directory or symlink"
            );
            ensure!(
                replace,
                "Session file already exists; use bootstrap-session --replace"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => anyhow::bail!("Could not inspect session destination"),
    }
    Ok(())
}

/// Validate the session by exchanging it for an API token and checking its account.
/// Neither invalid credentials nor derived JWTs are persisted.
pub async fn import_session(
    api: &str,
    web: &str,
    input: &str,
    path: &Path,
    replace: bool,
) -> Result<Value> {
    check_destination(path, replace)?;
    let input = input.trim();
    let cookie = if input
        .get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("cookie:"))
    {
        input[7..].trim()
    } else {
        input
    };
    let session = UlearnSession::from_cookie(api, web, cookie.to_owned())?;
    let client = UlearnClient::from_session(session)?;
    let account = client
        .get(&["account"], &[])
        .await
        .context("Session verification failed; credential file was not changed")?;
    let user = &account["user"];
    let id = user["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Session account has no user ID")?;

    let parent = path
        .parent()
        .context("Session path has no parent directory")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(parent)
        .context("Could not create session directory")?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).context("Could not create private session file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(cookie.as_bytes())
        .context("Could not write session file")?;
    temp.as_file().sync_all()?;
    check_destination(path, replace)?;
    if replace {
        temp.persist(path)
            .map_err(|e| e.error)
            .context("Could not replace session file")?;
    } else {
        temp.persist_noclobber(path)
            .map_err(|e| e.error)
            .context("Could not save session file")?;
    }
    Ok(json!({"account_id":id,"name":user["visibleName"],"login":user["login"]}))
}

pub async fn run(args: &[String]) -> Result<()> {
    if args == ["--help"] || args == ["-h"] {
        println!(
            "ulearn-mcp bootstrap-session [SESSION_FILE] [--replace]\n\n1. Войдите в ulearn в своём браузере и откройте ведомость.\n2. В DevTools → Network скопируйте значение заголовка Cookie запроса к ulearn.\n3. Вставьте его в скрытый ввод этой команды. Не передавайте cookie в чат.\n\nКоманда проверит сессию через POST /account/token и GET /account.\nСессия сохраняется с правами 0600, API-токен получается автоматически и хранится только в памяти.\nПо умолчанию: ~/.config/ulearn-mcp/ulearn.cookie (учитывается XDG_CONFIG_HOME).\nДля другой установки задайте ULEARN_API_URL, ULEARN_WEB_URL и ULEARN_SESSION_FILE.\nПри истечении сессии войдите снова и выполните bootstrap-session --replace.\nАвтоматическое продление самой браузерной cookie не выполняется."
        );
        return Ok(());
    }
    let mut replace = false;
    let mut explicit = None;
    for arg in args {
        if arg == "--replace" {
            ensure!(!replace, "Duplicate --replace flag");
            replace = true;
        } else {
            ensure!(
                !arg.starts_with('-') && explicit.is_none(),
                "Usage: ulearn-mcp bootstrap-session [SESSION_FILE] [--replace]"
            );
            explicit = Some(PathBuf::from(arg));
        }
    }
    let (api, web, path) = settings(explicit)?;
    check_destination(&path, replace)?;
    ensure!(
        std::io::stdin().is_terminal(),
        "Bootstrap needs an interactive terminal. Run `ulearn-mcp bootstrap-session` yourself; never send cookies through chat or MCP arguments"
    );
    eprintln!(
        "API: {api}\nСайт: {web}\nФайл: {}\nCookie будет использоваться для этой пары API и сайта. Инструкция: bootstrap-session --help.",
        path.display()
    );
    let cookie = rpassword::prompt_password("Cookie (ввод скрыт): ")?;
    let account = import_session(&api, &web, &cookie, &path, replace).await?;
    eprintln!(
        "Сессия проверена и сохранена. Аккаунт: {} ({}). Оценки не изменялись.",
        account["name"].as_str().unwrap_or(""),
        account["login"].as_str().unwrap_or("")
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&configuration(&api, &web, &path)?)?
    );
    eprintln!(
        "Замените запись ulearn в настройках MCP этим JSON и перезапустите подключение. Повторный вывод: ulearn-mcp config --session. При замене сессии того же аккаунта файл подхватится автоматически; смена аккаунта требует перезапуска MCP."
    );
    Ok(())
}
