use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc};
use tokio::time;

use crate::telemetry::Trace;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CALLBACK_PATH: &str = "/auth/callback";
const REFRESH_MARGIN: Duration = Duration::from_secs(300);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
pub(super) const ORIGINATOR: &str = "codex_ai";

#[derive(Deserialize, Serialize)]
pub(super) struct Credentials {
    pub(super) access_token: String,
    refresh_token: String,
    expires_at: u64,
    pub(super) account_id: Option<String>,
}

pub(super) struct Cache {
    path: PathBuf,
    _lock: File,
}

impl Cache {
    pub(super) fn open() -> Result<Self> {
        let directory = if let Some(path) = env::var_os("CODEX_CACHE_DIR") {
            PathBuf::from(path)
        } else if let Some(path) = env::var_os("XDG_CACHE_HOME").filter(|s| !s.is_empty()) {
            PathBuf::from(path).join("codex_ai")
        } else {
            PathBuf::from(env::var_os("HOME").context("HOME est absent")?).join(".cache/codex_ai")
        };
        Self::open_at(directory)
    }

    fn open_at(directory: PathBuf) -> Result<Self> {
        ensure!(
            directory.is_absolute(),
            "le répertoire de cache doit être absolu"
        );
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .with_context(|| format!("création du cache {}", directory.display()))?;
        let metadata = fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
            "le répertoire de cache doit être réel et privé (chmod 700) : {}",
            directory.display()
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join("lock"))?;
        lock.try_lock().context(
            "cache verrouillé ou inaccessible ; arrêtez l'autre instance avant de continuer",
        )?;
        Ok(Self {
            path: directory.join("tokens.json"),
            _lock: lock,
        })
    }

    fn load(&self) -> Result<Option<Credentials>> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("lecture du cache OAuth"),
        };
        ensure!(
            metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
            "le fichier de jetons doit être régulier et privé (chmod 600)"
        );
        let credentials: Credentials = serde_json::from_reader(File::open(&self.path)?)
            .map_err(|_| anyhow!("cache OAuth illisible ; relancez `codex_ai login`"))?;
        ensure!(
            !credentials.access_token.is_empty() && !credentials.refresh_token.is_empty(),
            "cache OAuth incomplet ; relancez `codex_ai login`"
        );
        Ok(Some(credentials))
    }

    fn save(&self, credentials: &Credentials) -> Result<()> {
        let directory = self.path.parent().context("répertoire de cache absent")?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(file.as_file_mut(), credentials)?;
        file.as_file().sync_all()?;
        file.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }

    pub(super) fn remove(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("suppression du cache OAuth"),
        }
    }
}

pub(super) struct Session {
    cache: Cache,
    client: Client,
    token_url: Url,
    state: Arc<Mutex<SessionState>>,
}

struct SessionState {
    credentials: Option<Arc<Credentials>>,
    dirty: bool,
}

pub(super) enum SessionError {
    LoginRequired,
    Temporary(anyhow::Error),
}

impl Session {
    pub(super) fn load(cache: Cache, client: Client) -> Result<Self> {
        let credentials = cache
            .load()?
            .context("aucune session en cache ; lancez `codex_ai login`")?;
        Ok(Self {
            cache,
            client,
            token_url: Url::parse(TOKEN_URL)?,
            state: Arc::new(Mutex::new(SessionState {
                credentials: Some(Arc::new(credentials)),
                dirty: false,
            })),
        })
    }

    pub(super) async fn credentials(
        self: &Arc<Self>,
        rejected: Option<&Arc<Credentials>>,
        trace: Trace,
    ) -> Result<Arc<Credentials>, SessionError> {
        let mut state = Arc::clone(&self.state).lock_owned().await;
        let current = state
            .credentials
            .clone()
            .ok_or(SessionError::LoginRequired)?;
        if state.dirty {
            self.cache.save(&current).map_err(SessionError::Temporary)?;
            state.dirty = false;
        }
        let rejected = rejected.is_some_and(|rejected| Arc::ptr_eq(&current, rejected));
        if !rejected && current.expires_at > now().saturating_add(REFRESH_MARGIN.as_secs()) {
            trace.event(format_args!("oauth.cache_hit"));
            return Ok(current);
        }

        trace.event(format_args!(
            "oauth.refresh.start reason={}",
            if rejected { "rejected" } else { "expiry" }
        ));
        // A client disconnect must not interrupt the persistence of a rotated refresh token.
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let result = session.refresh(state, trace).await;
            trace.event(format_args!("oauth.refresh.end success={}", result.is_ok()));
            result
        })
        .await
        .map_err(|error| SessionError::Temporary(error.into()))?
    }

    async fn refresh(
        &self,
        mut state: OwnedMutexGuard<SessionState>,
        trace: Trace,
    ) -> Result<Arc<Credentials>, SessionError> {
        let current = state
            .credentials
            .as_ref()
            .ok_or(SessionError::LoginRequired)?;
        let response = self
            .client
            .post(self.token_url.clone())
            .timeout(TOKEN_TIMEOUT)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", current.refresh_token.as_str()),
            ])
            .send()
            .await
            .context("rafraîchissement OAuth inaccessible")
            .map_err(SessionError::Temporary)?;
        trace.event(format_args!(
            "oauth.refresh.http status={}",
            response.status().as_u16()
        ));

        if matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            state.credentials = None;
            state.dirty = false;
            if let Err(error) = self.cache.remove() {
                eprintln!("Impossible de supprimer le cache OAuth : {error:#}");
            }
            return Err(SessionError::LoginRequired);
        }
        let fresh = Arc::new(
            read_tokens(response, Some(current))
                .await
                .map_err(SessionError::Temporary)?,
        );
        state.credentials = Some(Arc::clone(&fresh));
        state.dirty = true;
        self.cache.save(&fresh).map_err(SessionError::Temporary)?;
        state.dirty = false;
        Ok(fresh)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

async fn read_tokens(
    response: reqwest::Response,
    previous: Option<&Credentials>,
) -> Result<Credentials> {
    ensure!(
        response.status().is_success(),
        "OAuth a répondu HTTP {}",
        response.status()
    );
    let tokens: TokenResponse = response
        .json()
        .await
        .map_err(|_| anyhow!("réponse OAuth invalide"))?;
    ensure!(
        !tokens.access_token.is_empty(),
        "jeton d'accès OAuth absent"
    );
    // These claims are routing/expiry hints from the HTTPS token endpoint, not local authorization.
    let access_claims = claims(&tokens.access_token);
    let id_claims = tokens.id_token.as_deref().and_then(claims);
    let now = now();
    let expires_at = tokens
        .expires_in
        .and_then(|seconds| now.checked_add(seconds))
        .or_else(|| access_claims.as_ref()?.get("exp")?.as_u64())
        .filter(|expires_at| *expires_at > now)
        .context("expiration du jeton OAuth absente ou passée")?;
    let refresh_token = tokens
        .refresh_token
        .filter(|token| !token.is_empty())
        .or_else(|| previous.map(|credentials| credentials.refresh_token.clone()))
        .context("jeton de rafraîchissement OAuth absent")?;
    let account_id = id_claims
        .as_ref()
        .and_then(account_id)
        .or_else(|| access_claims.as_ref().and_then(account_id))
        .map(str::to_owned)
        .or_else(|| previous.and_then(|credentials| credentials.account_id.clone()));
    Ok(Credentials {
        access_token: tokens.access_token,
        refresh_token,
        expires_at,
        account_id,
    })
}

fn claims(jwt: &str) -> Option<Value> {
    let bytes = URL_SAFE_NO_PAD.decode(jwt.split('.').nth(1)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn account_id(claims: &Value) -> Option<&str> {
    [
        &claims["chatgpt_account_id"],
        &claims["https://api.openai.com/auth"]["chatgpt_account_id"],
        &claims["organizations"][0]["id"],
    ]
    .into_iter()
    .filter_map(Value::as_str)
    .find(|id| !id.is_empty())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone)]
struct PendingLogin {
    state: Arc<str>,
    code: mpsc::Sender<Result<String, &'static str>>,
}

#[derive(Deserialize)]
struct Callback {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(login): State<PendingLogin>,
    Query(query): Query<Callback>,
) -> impl IntoResponse {
    let result = if !query
        .state
        .as_deref()
        .is_some_and(|state| bool::from(state.as_bytes().ct_eq(login.state.as_bytes())))
    {
        (StatusCode::BAD_REQUEST, "État OAuth invalide.")
    } else {
        let code = if query.error.is_some() {
            Err("connexion refusée ou annulée par OpenAI")
        } else {
            query
                .code
                .filter(|code| !code.is_empty())
                .ok_or("code OAuth absent")
        };
        let (status, message) = if code.is_ok() {
            (
                StatusCode::OK,
                "Code reçu. Le résultat de la connexion s'affiche dans le terminal.",
            )
        } else {
            (
                StatusCode::BAD_REQUEST,
                "Connexion non terminée. Consultez le terminal.",
            )
        };
        if login.code.try_send(code).is_ok() {
            (status, message)
        } else {
            (
                StatusCode::CONFLICT,
                "Cette connexion a déjà reçu une réponse.",
            )
        }
    };
    (result.0, [(header::CACHE_CONTROL, "no-store")], result.1)
}

pub(super) async fn login(cache: &Cache, client: &Client) -> Result<()> {
    // The registered Codex OAuth client only accepts specific loopback redirects.
    let listener = match TcpListener::bind(("127.0.0.1", 1455)).await {
        Err(error) if error.kind() == ErrorKind::AddrInUse => {
            TcpListener::bind(("127.0.0.1", 1457)).await
        }
        result => result,
    }
    .context("ports OAuth 1455/1457 indisponibles sur localhost")?;
    let redirect = format!(
        "http://localhost:{}{CALLBACK_PATH}",
        listener.local_addr()?.port()
    );
    let verifier = random_urlsafe()?;
    let state = random_urlsafe()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
    let mut url = Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect.as_str()),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", ORIGINATOR),
    ]);
    let (code, mut received) = mpsc::channel(1);
    let app = Router::new()
        .route(CALLBACK_PATH, get(callback))
        .with_state(PendingLogin {
            state: state.into(),
            code,
        });
    println!("Ouvrez cette URL sur cette machine pour vous connecter :\n{url}");
    let code = tokio::select! {
        result = time::timeout(LOGIN_TIMEOUT, received.recv()) => {
            result.context("délai de connexion OAuth dépassé")?
                .context("callback OAuth interrompu")?
                .map_err(anyhow::Error::msg)?
        }
        result = async { axum::serve(listener, app).await } => {
            result.context("serveur de callback OAuth")?;
            bail!("serveur de callback OAuth interrompu");
        }
    };
    let response = client
        .post(TOKEN_URL)
        .timeout(TOKEN_TIMEOUT)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .context("échange du code OAuth")?;
    let credentials = read_tokens(response, None).await?;
    cache.save(&credentials).context("écriture du cache OAuth")
}

fn random_urlsafe() -> Result<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("aléa OAuth indisponible : {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
pub(super) fn test_session(directory: PathBuf, client: Client) -> Result<Session> {
    let cache = Cache::open_at(directory)?;
    cache.save(&Credentials {
        access_token: "test-upstream-token".into(),
        refresh_token: "test-refresh-token".into(),
        expires_at: now() + 3600,
        account_id: Some("test-account".into()),
    })?;
    Session::load(cache, client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::routing::post;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn callback_requires_matching_state() -> Result<()> {
        let (code, mut received) = mpsc::channel(1);
        let login = PendingLogin {
            state: "expected".into(),
            code,
        };
        for (state, status) in [
            ("wrong", StatusCode::BAD_REQUEST),
            ("expected", StatusCode::OK),
        ] {
            let response = callback(
                State(login.clone()),
                Query(Callback {
                    state: Some(state.into()),
                    code: Some("test-code".into()),
                    error: None,
                }),
            )
            .await
            .into_response();
            assert_eq!(response.status(), status);
            if state == "wrong" {
                assert!(matches!(
                    received.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
            }
        }
        assert!(matches!(received.try_recv(), Ok(Ok(code)) if code == "test-code"));
        Ok(())
    }

    #[tokio::test]
    async fn refresh_is_shared_and_persists_rotated_credentials() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("private");
        let cache = Cache::open_at(path.clone())?;
        assert!(
            Cache::open_at(path).is_err(),
            "second session must not acquire the cache"
        );
        cache.save(&Credentials {
            access_token: "expired".into(),
            refresh_token: "original".into(),
            expires_at: 0,
            account_id: Some("account".into()),
        })?;
        let mut session = Session::load(cache, Client::new())?;
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/token", post(|State(calls): State<Arc<AtomicUsize>>| async move {
                calls.fetch_add(1, Ordering::Relaxed);
                Json(json!({"access_token": "fresh", "refresh_token": "rotated", "expires_in": 3600}))
            }))
            .with_state(Arc::clone(&calls));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        session.token_url = Url::parse(&format!("http://{}/token", listener.local_addr()?))?;
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let session = Arc::new(session);
        let (a, b) = tokio::join!(
            session.credentials(None, Trace::default()),
            session.credentials(None, Trace::default())
        );
        server.abort();
        assert_eq!(a.ok().context("first refresh")?.access_token, "fresh");
        assert_eq!(b.ok().context("second refresh")?.access_token, "fresh");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "refresh only once");
        let cached = session.cache.load()?.context("fresh credentials on disk")?;
        assert_eq!(cached.refresh_token, "rotated");
        assert_eq!(cached.account_id.as_deref(), Some("account"));
        assert_eq!(
            fs::metadata(&session.cache.path)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }
}
