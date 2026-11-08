//! Import the explicitly supplied website session without exposing it to MCP.
use crate::web_client::UlearnWebClient;
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

pub fn default_cookie_path() -> Result<PathBuf> {
    Ok(crate::bootstrap::default_token_path()?.with_file_name("ulearn.cookie"))
}

fn check_destination(path: &Path, replace: bool) -> Result<()> {
    ensure!(path.is_absolute(), "Cookie file path must be absolute");
    ensure!(
        path.file_name().is_some(),
        "Cookie file path must include a filename"
    );
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "Cookie destination must be a regular file, not a directory or symlink"
            );
            ensure!(
                replace,
                "Cookie file already exists; use bootstrap-web --replace to update it"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => anyhow::bail!("Could not inspect cookie destination"),
    }
    Ok(())
}

/// Validate syntax and save atomically. This does not verify account or session validity.
pub fn import_cookie(base_url: &str, input: &str, path: &Path, replace: bool) -> Result<()> {
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
    UlearnWebClient::new(base_url, cookie.to_owned())?;
    let parent = path
        .parent()
        .context("Cookie path has no parent directory")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(parent)
        .context("Could not create cookie directory")?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).context("Could not create private cookie file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(cookie.as_bytes())
        .context("Could not write cookie file")?;
    temp.as_file().sync_all()?;
    check_destination(path, replace)?;
    if replace {
        temp.persist(path)
            .map_err(|e| e.error)
            .context("Could not replace cookie file")?;
    } else {
        temp.persist_noclobber(path)
            .map_err(|e| e.error)
            .context("Could not save cookie file")?;
    }
    Ok(())
}

pub async fn run(args: &[String]) -> Result<()> {
    if args == ["--help"] || args == ["-h"] {
        println!(
            "ulearn-mcp bootstrap-web [COOKIE_FILE] [--replace]\n\nДополнительные баллы редактируются через сессию сайта ulearn.me.\nВ своём браузере войдите в тот же аккаунт преподавателя, что используется для API.\nОткройте ведомость, затем DevTools → Network и скопируйте значение заголовка Cookie её запроса.\nВставьте его в скрытый ввод этой команды; не передавайте cookie в чат.\nФормат: name=value; another=value. Импорт проверяет формат, но не действительность сессии.\nПо умолчанию: ~/.config/ulearn-mcp/ulearn.cookie (учитывается XDG_CONFIG_HOME).\nМожно задать ULEARN_WEB_COOKIE_FILE и ULEARN_WEB_URL для своей установки.\nИстёкшую сессию замените командой bootstrap-web --replace."
        );
        return Ok(());
    }
    let mut replace = false;
    let mut explicit_path = None;
    for arg in args {
        if arg == "--replace" {
            ensure!(!replace, "Duplicate --replace flag");
            replace = true;
        } else {
            ensure!(
                !arg.starts_with('-') && explicit_path.is_none(),
                "Usage: ulearn-mcp bootstrap-web [COOKIE_FILE] [--replace]"
            );
            explicit_path = Some(PathBuf::from(arg));
        }
    }
    let api = crate::web_client::optional_env("ULEARN_API_URL")?;
    let web = crate::web_client::optional_env("ULEARN_WEB_URL")?;
    let configured_file = crate::web_client::optional_env("ULEARN_WEB_COOKIE_FILE")?;
    let explicit_path = explicit_path.or_else(|| configured_file.map(PathBuf::from));
    crate::web_client::validate_installation_config(
        api.as_deref(),
        web.as_deref(),
        explicit_path.as_deref().and_then(Path::to_str),
    )?;
    let path = explicit_path.map(Ok).unwrap_or_else(default_cookie_path)?;
    let base = web.unwrap_or_else(|| crate::web_client::DEFAULT_WEB_URL.to_owned());
    // Validate the configured destination before prompting for a secret.
    UlearnWebClient::new(&base, "validation=placeholder".to_owned())?;
    check_destination(&path, replace)?;
    ensure!(
        std::io::stdin().is_terminal(),
        "Bootstrap needs an interactive terminal. Run `ulearn-mcp bootstrap-web` yourself; never send cookies through chat or MCP arguments"
    );
    eprintln!(
        "Сайт: {base}\nФайл: {}\nСкопируйте Cookie запроса ведомости из своего браузера (bootstrap-web --help). Используйте тот же аккаунт, что и для API.",
        path.display()
    );
    let cookie = rpassword::prompt_password("Cookie (ввод скрыт): ")?;
    import_cookie(&base, &cookie, &path, replace)?;
    eprintln!(
        "Cookie сохранён. Действительность сессии и аккаунт не проверялись. Баллы не изменялись. Для нестандартного пути задайте ULEARN_WEB_COOKIE_FILE; перезапуск после замены файла не требуется."
    );
    Ok(())
}
