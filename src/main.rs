use rmcp::ServiceExt;
use std::io::{self, Write};
use std::path::Path;
use ulearn_mcp::{client::UlearnClient, server::UlearnServer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "bootstrap") {
        return ulearn_mcp::bootstrap::run(&args[1..]).await;
    }
    match args.first().map(String::as_str) {
        Some("--help" | "-h") => {
            eprintln!(
                "ulearn-mcp [bootstrap [TOKEN_FILE] [--replace] | --check | login TOKEN_FILE]\n\nDefault: MCP over stdio.\nBootstrap: interactive token setup; use bootstrap --help for instructions.\n--check: verify account/token via GET /account.\n\nEnvironment: ULEARN_TOKEN or ULEARN_TOKEN_FILE, ULEARN_API_URL (default https://api.ulearn.me).\nLogin: ulearn-mcp login /absolute/path/ulearn.token (hidden password prompt).\nThe default ~/.config/ulearn-mcp/ulearn.token is loaded automatically for api.ulearn.me. Without a token, only public course content is available."
            );
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("ulearn-mcp {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("login") => {
            let path = std::env::args().nth(2).ok_or_else(|| {
                anyhow::anyhow!("Usage: ulearn-mcp login /absolute/path/ulearn.token")
            })?;
            return login(Path::new(&path)).await;
        }
        Some("--check") => {
            let account = UlearnClient::from_env()?.get(&["account"], &[]).await?;
            println!("{}", serde_json::to_string_pretty(&account)?);
            return Ok(());
        }
        Some(_) => anyhow::bail!("Unknown argument; use --help"),
        None => {}
    }
    let server = UlearnServer::new(UlearnClient::from_env()?);
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn login(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(path.is_absolute(), "Token file path must be absolute");
    anyhow::ensure!(
        !path.exists(),
        "Token file already exists; choose a new path to avoid overwriting credentials"
    );
    let base = std::env::var("ULEARN_API_URL").unwrap_or_else(|_| "https://api.ulearn.me".into());
    eprintln!(
        "Sign in to {base}. Token will be saved to {}",
        path.display()
    );
    eprint!("Ulearn login: ");
    io::stderr().flush()?;
    let mut username = String::new();
    io::stdin().read_line(&mut username)?;
    let password = rpassword::prompt_password("Password: ")?;
    let client = UlearnClient::new(&base, None)?;
    let token = client.login(username.trim(), &password).await?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    eprintln!(
        "Saved token. Set ULEARN_TOKEN_FILE to this absolute path, then run ulearn-mcp --check."
    );
    Ok(())
}
