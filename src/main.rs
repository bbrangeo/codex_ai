mod api;
mod auth;
mod chat;
mod telemetry;

use std::env;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use reqwest::Client;
use tokio::net::TcpListener;

const DEFAULT_PORT: u16 = 8787;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => help(),
        [arg] if arg == "--help" || arg == "-h" => help(),
        [arg] if arg == "login" => {
            let cache = auth::Cache::open()?;
            auth::login(&cache, &http_client()?).await?;
            println!("Signed in successfully. Tokens are saved in the private cache.");
        }
        [arg] if arg == "serve" => serve().await?,
        [arg] if arg == "logout" => {
            auth::Cache::open()?.remove()?;
            println!("Login cache cleared.");
        }
        _ => bail!("usage: codex_ai <login|serve|logout>"),
    }
    Ok(())
}

fn help() {
    println!(
        "codex_ai login   Sign in to a ChatGPT account and cache tokens\n\
              codex_ai serve   Start the local API (CODEX_API_KEY required, CODEX_DEBUG=1 for diagnostic logs)\n\
              codex_ai logout  Clear tokens from the local cache"
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

async fn serve() -> Result<()> {
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
    let client = http_client()?;
    let session = auth::Session::load(auth::Cache::open()?, client.clone())?;
    let debug = env::var_os("CODEX_DEBUG").is_some_and(|value| value == "1");
    let router = api::router(api::Codex::new(client, session), key, debug);
    let listener = TcpListener::bind(address)
        .await
        .context("binding the local API listener")?;
    println!("OpenAI API: http://{}/v1", listener.local_addr()?);
    if debug {
        eprintln!("Diagnostic logging enabled (CODEX_DEBUG=1)");
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
