//! Interactive credential import stays outside MCP: secrets never pass through tool calls.
use crate::client::UlearnClient;
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

pub const DEFAULT_API_URL: &str = "https://api.ulearn.me";
pub const INSTRUCTIONS: &str = "Как получить токен ulearn:\n\
1. Откройте https://ulearn.me/token в своём браузере и войдите в аккаунт преподавателя.\n\
2. На странице «Токен доступа» нажмите «Скопировать токен». Показывать его на экране не нужно.\n\
3. Вставьте токен в запрос ниже и нажмите Enter. Ввод скрыт.\n\
Токен даёт доступ к аккаунту: вставляйте его только в этот локальный терминал, не в чат.\n\
Если токен истечёт, снова скопируйте его со страницы и запустите bootstrap --replace.";

pub fn default_token_path() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        let root = PathBuf::from(root);
        ensure!(
            root.is_absolute(),
            "XDG_CONFIG_HOME must be an absolute path"
        );
        return Ok(root.join("ulearn-mcp/ulearn.token"));
    }
    let home = std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .context("Set HOME or XDG_CONFIG_HOME, or specify an absolute token file path")?;
    let home = PathBuf::from(home);
    ensure!(home.is_absolute(), "HOME must be an absolute path");
    Ok(home.join(".config/ulearn-mcp/ulearn.token"))
}

fn check_destination(path: &Path, replace: bool) -> Result<()> {
    ensure!(path.is_absolute(), "Token file path must be absolute");
    ensure!(
        path.file_name().is_some(),
        "Token file path must include a filename"
    );
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "Token destination must be a regular file, not a directory or symlink"
            );
            ensure!(
                replace,
                "Token file already exists; use bootstrap --replace to update it"
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("Could not inspect token destination"),
    }
    Ok(())
}

fn normalize_token(input: &str) -> Result<&str> {
    let trimmed = input.trim();
    let token = if trimmed
        .get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("bearer "))
    {
        trimmed[7..].trim()
    } else {
        trimmed
    };
    ensure!(!token.is_empty(), "Token must not be empty");
    ensure!(token.len() <= 64 * 1024, "Token is too large");
    ensure!(
        !token.chars().any(char::is_whitespace),
        "Paste only the token value, not JSON or a full HTTP header"
    );
    Ok(token)
}

/// Validate the token against the chosen API before writing anything to disk.
/// Failed validation leaves an existing credential untouched.
pub async fn import_token(api_url: &str, input: &str, path: &Path, replace: bool) -> Result<()> {
    check_destination(path, replace)?;
    let token = normalize_token(input)?;
    let client = UlearnClient::new(api_url, Some(token.to_owned()))?.without_auto_refresh();
    client
        .get(&["account"], &[])
        .await
        .context("Token verification failed; credential file was not changed")?;
    let parent = path
        .parent()
        .context("Token path has no parent directory")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(parent)
        .context("Could not create credential directory")?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).context("Could not create private token file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(token.as_bytes())
        .context("Could not write token file")?;
    temp.as_file().sync_all()?;
    // Recheck to handle another setup process creating a file during the GET.
    check_destination(path, replace)?;
    if replace {
        temp.persist(path)
            .map_err(|e| e.error)
            .context("Could not replace token file")?;
    } else {
        temp.persist_noclobber(path)
            .map_err(|e| e.error)
            .context("Could not save token file (it may have been created by another process)")?;
    }
    Ok(())
}

fn configured_token_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    explicit
        .or_else(|| std::env::var_os("ULEARN_TOKEN_FILE").map(PathBuf::from))
        .map(Ok)
        .unwrap_or_else(default_token_path)
}

fn client_config(binary: &Path, token_path: &Path, api: &str) -> Result<serde_json::Value> {
    ensure!(binary.is_absolute(), "Executable path must be absolute");
    ensure!(token_path.is_absolute(), "Token file path must be absolute");
    UlearnClient::new(api, None)?;
    let mut environment = serde_json::json!({"ULEARN_TOKEN_FILE":token_path,"ULEARN_API_URL":api});
    for name in [
        "ULEARN_WEB_URL",
        "ULEARN_WEB_COOKIE_FILE",
        "ULEARN_AUTO_REFRESH_TOKEN",
        "ULEARN_TOKEN_REFRESH_DAYS",
    ] {
        let value = match std::env::var(name) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => continue,
            Err(_) => anyhow::bail!("{name} is not valid UTF-8"),
        };
        match name {
            "ULEARN_WEB_URL" => {
                crate::web_client::UlearnWebClient::new(&value, "validation=placeholder".into())?;
            }
            "ULEARN_WEB_COOKIE_FILE" => ensure!(
                Path::new(&value).is_absolute(),
                "ULEARN_WEB_COOKIE_FILE must be an absolute path"
            ),
            "ULEARN_AUTO_REFRESH_TOKEN" => ensure!(
                matches!(value.as_str(), "true" | "false" | "1" | "0"),
                "ULEARN_AUTO_REFRESH_TOKEN must be true or false"
            ),
            "ULEARN_TOKEN_REFRESH_DAYS" => ensure!(
                value
                    .parse::<u32>()
                    .is_ok_and(|days| (1..=365).contains(&days)),
                "ULEARN_TOKEN_REFRESH_DAYS must be an integer from 1 to 365"
            ),
            _ => unreachable!(),
        }
        environment[name] = value.into();
    }
    Ok(serde_json::json!({"mcpServers":{"ulearn":{"command":binary,"env":environment}}}))
}

/// Print-ready configuration without reading tokens or accessing the network.
/// File authentication is intentional: never serialize ULEARN_TOKEN into config.
pub fn configuration_from_env() -> Result<serde_json::Value> {
    let path = configured_token_path(None)?;
    let api = std::env::var("ULEARN_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.into());
    client_config(&std::env::current_exe()?, &path, &api)
}

pub async fn run(args: &[String]) -> Result<()> {
    if args == ["--help"] || args == ["-h"] {
        println!(
            "ulearn-mcp bootstrap [TOKEN_FILE] [--replace]\n\n{INSTRUCTIONS}\n\nПо умолчанию: $XDG_CONFIG_HOME/ulearn-mcp/ulearn.token\nили ~/.config/ulearn-mcp/ulearn.token. Можно задать ULEARN_TOKEN_FILE.\nПроверка токена: GET /account. Файл сохраняется с правами 0600 на Unix.\nБез --replace существующий файл не перезаписывается.\nКоманда предназначена для вашего терминала, а не для MCP tool call."
        );
        return Ok(());
    }
    let mut replace = false;
    let mut path = None;
    for arg in args {
        if arg == "--replace" {
            ensure!(!replace, "Duplicate --replace flag");
            replace = true;
        } else {
            ensure!(
                !arg.starts_with('-') && path.is_none(),
                "Usage: ulearn-mcp bootstrap [TOKEN_FILE] [--replace]"
            );
            path = Some(PathBuf::from(arg));
        }
    }
    let path = configured_token_path(path)?;
    let api = std::env::var("ULEARN_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.into());
    // Validate origin before asking for the secret; a custom server must be visible to the user.
    UlearnClient::new(&api, None)?;
    check_destination(&path, replace)?;
    if api.trim_end_matches('/') == DEFAULT_API_URL {
        eprintln!("{INSTRUCTIONS}");
    } else {
        eprintln!(
            "Настроен другой API: {api}. Получите токен именно этой установки ulearn; инструкция https://ulearn.me/token относится только к api.ulearn.me."
        );
    }
    eprintln!("\nПроверка: {api}/account\nФайл: {}", path.display());
    ensure!(
        std::io::stdin().is_terminal(),
        "Bootstrap needs an interactive terminal. Run `ulearn-mcp bootstrap` yourself; never send the token through chat or an MCP tool argument"
    );
    let token = rpassword::prompt_password("Токен (ввод скрыт): ")?;
    import_token(&api, &token, &path, replace).await?;
    eprintln!("Токен проверен и сохранён. Комментарии и оценки не изменялись.");
    let binary = std::env::current_exe()?;
    let config = client_config(&binary, &path, &api)?;
    println!(
        "Конфигурация для MCP-клиентов с mcpServers (без секретов):\n{}",
        serde_json::to_string_pretty(&config)?
    );
    eprintln!(
        "Повторно вывести JSON настройки: ulearn-mcp config. При стандартном пути и API токен подхватывается автоматически. Запущенный MCP подхватит замену файла токена при следующем запросе. ULEARN_TOKEN/ULEARN_TOKEN_FILE имеют приоритет."
    );
    Ok(())
}
