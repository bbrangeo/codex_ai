mod api;
mod auth;
mod telemetry;

use std::env;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use reqwest::Client;
use serde_json::json;
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Semaphore;

use crate::telemetry::LogLevel;

const DEFAULT_PORT: u16 = 8787;
const DEFAULT_MAX_CONCURRENCY: usize = 4;
const SERVE_USAGE: &str = "serve [--logs <info|debug>]";
const LOGIN_USAGE: &str = "login [--no-browser]";

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args_os();
    let executable = args.next();
    let program = executable
        .as_deref()
        .and_then(|path| Path::new(path).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or(env!("CARGO_BIN_NAME"));
    let args: Vec<_> = args.collect();
    match args.as_slice() {
        [] => help(program),
        [arg] if arg == "--help" || arg == "-h" => help(program),
        [arg, options @ ..] if arg == "login" => {
            let no_browser = match options {
                [] => false,
                [flag] if flag == "--no-browser" => true,
                [flag] if flag == "--help" || flag == "-h" => {
                    println!("Usage: {program} {LOGIN_USAGE}");
                    return Ok(());
                }
                _ => bail!("usage: {program} {LOGIN_USAGE}"),
            };
            auth::login(auth::Cache::open()?, &http_client()?, no_browser).await?;
            println!("Signed in successfully. Credentials saved.");
        }
        [arg, flag] if arg == "serve" && (flag == "--help" || flag == "-h") => {
            println!("Usage: {program} {SERVE_USAGE}");
        }
        [arg, options @ ..] if arg == "serve" => {
            let log_level = match options {
                [] if env::var_os("CODEX_DEBUG").is_some_and(|value| value == "1") => {
                    LogLevel::Debug
                }
                [] => LogLevel::Off,
                [flag, level] if flag == "--logs" => match level.to_str() {
                    Some("info") => LogLevel::Info,
                    Some("debug") => LogLevel::Debug,
                    _ => bail!("--logs must be info or debug"),
                },
                _ => bail!("usage: {program} {SERVE_USAGE}"),
            };
            serve(log_level).await?;
        }
        [arg] if arg == "status" || arg == "whoami" => status()?,
        [arg] if arg == "logout" => {
            auth::Cache::open()?.logout()?;
            println!("Login cache cleared.");
        }
        _ => bail!("usage: {program} <login|serve|status|whoami|logout>"),
    }
    Ok(())
}

fn help(program: &str) {
    println!(
        "{program} {LOGIN_USAGE}  Sign in to a ChatGPT account and save credentials\n\
         {program} {SERVE_USAGE}  Start the local API (CODEX_API_KEY required)\n\
         {program} status (alias: whoami)  Show cached account and listener information\n\
         {program} logout  Clear saved credentials"
    );
}

fn http_client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .user_agent(concat!("codex_ai/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the HTTP client")
}

fn listen_address() -> Result<SocketAddr> {
    let address: SocketAddr = match env::var("CODEX_LISTEN") {
        Ok(address) => address
            .parse()
            .context("CODEX_LISTEN must specify an IP address and port")?,
        Err(env::VarError::NotPresent) => SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)),
        Err(error) => return Err(error).context("invalid CODEX_LISTEN"),
    };
    ensure!(
        address.ip().is_loopback(),
        "CODEX_LISTEN must use a loopback address"
    );
    Ok(address)
}

fn status() -> Result<()> {
    let cache = auth::Cache::open()?;
    let credentials = cache.load()?;
    let port = match cache.listener()? {
        Some(address) => Some(address.port()),
        None => Some(listen_address()?.port()).filter(|port| *port != 0),
    };
    let info = json!({
        "email": credentials.as_ref().and_then(|credentials| credentials.email.as_deref()),
        "account_id": credentials.as_ref().and_then(|credentials| credentials.account_id.as_deref()),
        "expires_at": credentials.as_ref().map(|credentials| credentials.expires_at),
        "port": port,
    });
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(())
}

async fn serve(log_level: LogLevel) -> Result<()> {
    let key = api::local_key(&env::var("CODEX_API_KEY").context("CODEX_API_KEY is required")?)?;
    let address = listen_address()?;
    let max_concurrency = match env::var("CODEX_MAX_CONCURRENCY") {
        Ok(value) => value
            .parse::<usize>()
            .context("CODEX_MAX_CONCURRENCY must be a positive integer")?,
        Err(env::VarError::NotPresent) => DEFAULT_MAX_CONCURRENCY,
        Err(error) => return Err(error).context("invalid CODEX_MAX_CONCURRENCY"),
    };
    ensure!(
        (1..=Semaphore::MAX_PERMITS).contains(&max_concurrency),
        "CODEX_MAX_CONCURRENCY must be between 1 and {}",
        Semaphore::MAX_PERMITS
    );
    let client = http_client()?;
    let cache = auth::Cache::open()?;
    let _instance = cache.server_lock()?;
    cache.clear_listener()?;
    let session = Arc::new(auth::Session::load(cache.clone(), client.clone())?);
    let chat_tools =
        if env::var_os("CODEX_CHAT_FUNCTION_TOOLS_ONLY").is_some_and(|value| value == "1") {
            api::ChatTools::FunctionsOnly
        } else {
            api::ChatTools::Strict
        };
    let router = api::router(
        api::Codex::new(client, Arc::clone(&session), max_concurrency),
        key,
        log_level,
        chat_tools,
    );
    let listener = TcpListener::bind(address)
        .await
        .context("binding the local API listener")?;
    #[cfg(unix)]
    let mut terminate = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    #[cfg(unix)]
    let mut hangup = signal(SignalKind::hangup()).context("installing SIGHUP handler")?;
    let address = listener.local_addr()?;
    cache.record_listener(address)?;
    println!("OpenAI API: http://{address}/v1");
    if log_level == LogLevel::Debug {
        eprintln!("Diagnostic logging enabled (debug)");
    }
    #[cfg(unix)]
    let reload = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            while hangup.recv().await.is_some() {
                if let Err(error) = session.reload().await {
                    eprintln!("Could not reload OAuth credentials: {error:#}");
                }
            }
        })
    };
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            #[cfg(unix)]
            tokio::select! {
                () = ctrl_c() => {},
                _ = terminate.recv() => {},
            }
            #[cfg(not(unix))]
            ctrl_c().await;
        })
        .await;
    #[cfg(unix)]
    reload.abort();
    session.finish().await;
    let cleanup = cache.clear_listener();
    result.context("API server")?;
    cleanup
}

async fn ctrl_c() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("Could not wait for Ctrl-C: {error}");
    }
}
