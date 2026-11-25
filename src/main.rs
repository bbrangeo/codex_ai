mod api;
mod auth;
mod telemetry;

use std::env;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use reqwest::Client;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::telemetry::LogLevel;

const DEFAULT_PORT: u16 = 8787;
const DEFAULT_MAX_CONCURRENCY: usize = 4;
const SERVE_USAGE: &str = "serve [--logs <info|debug>]";

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
        [arg] if arg == "login" => {
            let cache = auth::Cache::open()?;
            auth::login(&cache, &http_client()?).await?;
            println!("Signed in successfully. Tokens are saved in the private cache.");
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
        [arg] if arg == "logout" => {
            auth::Cache::open()?.remove()?;
            println!("Login cache cleared.");
        }
        _ => bail!("usage: {program} <login|serve|logout>"),
    }
    Ok(())
}

fn help(program: &str) {
    println!(
        "{program} login   Sign in to a ChatGPT account and cache tokens\n\
         {program} {SERVE_USAGE}  Start the local API (CODEX_API_KEY required)\n\
         {program} logout  Clear tokens from the local cache"
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

async fn serve(log_level: LogLevel) -> Result<()> {
    let key = api::local_key(&env::var("CODEX_API_KEY").context("CODEX_API_KEY is required")?)?;
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
    let session = auth::Session::load(auth::Cache::open()?, client.clone())?;
    let chat_tools =
        if env::var_os("CODEX_CHAT_FUNCTION_TOOLS_ONLY").is_some_and(|value| value == "1") {
            api::ChatTools::FunctionsOnly
        } else {
            api::ChatTools::Strict
        };
    let router = api::router(
        api::Codex::new(client, session, max_concurrency),
        key,
        log_level,
        chat_tools,
    );
    let listener = TcpListener::bind(address)
        .await
        .context("binding the local API listener")?;
    println!("OpenAI API: http://{}/v1", listener.local_addr()?);
    if log_level == LogLevel::Debug {
        eprintln!("Diagnostic logging enabled (debug)");
    }
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("Could not wait for Ctrl-C: {error}");
            }
        })
        .await
        .context("API server")
}
