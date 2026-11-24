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
            println!("Connexion réussie. Les jetons sont enregistrés dans le cache privé.");
        }
        [arg] if arg == "serve" => serve().await?,
        [arg] if arg == "logout" => {
            auth::Cache::open()?.remove()?;
            println!("Cache de connexion supprimé.");
        }
        _ => bail!("usage : codex_ai <login|serve|logout>"),
    }
    Ok(())
}

fn help() {
    println!(
        "codex_ai login   Authentifier un compte ChatGPT et mettre les jetons en cache\n\
              codex_ai serve   Démarrer l'API locale (CODEX_API_KEY requis, CODEX_DEBUG=1 pour les logs)\n\
              codex_ai logout  Effacer les jetons du cache local"
    );
}

fn http_client() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(120))
        .user_agent(concat!("codex_ai/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("construction du client HTTP")
}

async fn serve() -> Result<()> {
    let key = api::local_key(&env::var("CODEX_API_KEY").context("CODEX_API_KEY est requis")?)?;
    let address: SocketAddr = match env::var("CODEX_LISTEN") {
        Ok(address) => address
            .parse()
            .context("CODEX_LISTEN doit être une adresse IP et un port")?,
        Err(env::VarError::NotPresent) => SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)),
        Err(error) => return Err(error).context("CODEX_LISTEN invalide"),
    };
    ensure!(
        address.ip().is_loopback(),
        "CODEX_LISTEN doit être une adresse loopback"
    );
    let client = http_client()?;
    let session = auth::Session::load(auth::Cache::open()?, client.clone())?;
    let debug = env::var_os("CODEX_DEBUG").is_some_and(|value| value == "1");
    let router = api::router(api::Codex::new(client, session), key, debug);
    let listener = TcpListener::bind(address)
        .await
        .context("écoute de l'API locale")?;
    println!("API OpenAI : http://{}/v1", listener.local_addr()?);
    if debug {
        eprintln!("Journalisation de diagnostic activée (CODEX_DEBUG=1)");
    }
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("Impossible d'attendre Ctrl-C : {error}");
            }
        })
        .await
        .context("serveur API")
}
