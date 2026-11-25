#![cfg(unix)]

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn reports_actual_port_and_handles_hup_and_term() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let cache = temp.path().join("private");
    DirBuilder::new().mode(0o700).create(&cache)?;
    let credentials = json!({
        "email": "person@example.test",
        "account_id": "test-account",
        "expires_at": 4_102_444_800_u64,
        "access_token": "private-access-token",
        "refresh_token": "private-refresh-token",
    });
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(cache.join("tokens.json"))?
        .write_all(&serde_json::to_vec(&credentials)?)?;

    let command = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_codex_ai"));
        cmd.env("CODEX_CACHE_DIR", &cache)
            .env("CODEX_LISTEN", "127.0.0.1:0")
            .env("CODEX_CREDENTIAL_STORE", "file")
            .env_remove("CODEX_MAX_CONCURRENCY");
        cmd
    };
    let mut server = Server(
        command()
            .arg("serve")
            .env("CODEX_API_KEY", "service-integration-test-local-key")
            .stdout(Stdio::null())
            .spawn()?,
    );
    let address: SocketAddr = timeout(Duration::from_secs(5), async {
        loop {
            match fs::read_to_string(cache.join("listener.addr")) {
                Ok(address) => break Ok(address.parse()?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
            ensure!(
                server.0.try_wait()?.is_none(),
                "server exited during startup"
            );
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;

    let status = |name: &str| -> Result<Value> {
        let output = command().arg(name).env_remove("CODEX_API_KEY").output()?;
        ensure!(output.status.success(), "{name} command failed");
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let info = status("status")?;
    assert_eq!(info, status("whoami")?);
    assert_eq!(
        info,
        json!({
            "email": credentials["email"],
            "account_id": credentials["account_id"],
            "expires_at": credentials["expires_at"],
            "port": address.port(),
        })
    );

    let pid = server.0.id().to_string();
    let send_signal = |signal: &str| -> Result<()> {
        ensure!(
            Command::new("kill")
                .args([signal, &pid])
                .status()?
                .success(),
            "could not send {signal}"
        );
        Ok(())
    };
    send_signal("-HUP")?;
    let response = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?
        .get(format!("http://{address}/healthz"))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    send_signal("-TERM")?;
    let exit = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = server.0.try_wait()? {
                break Ok::<_, std::io::Error>(status);
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert!(exit.success(), "SIGTERM did not exit cleanly: {exit}");
    assert!(!cache.join("listener.addr").exists());
    assert_eq!(status("status")?["port"], Value::Null);
    Ok(())
}
