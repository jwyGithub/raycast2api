//! Managed child process. Credentials travel over stdin, never command arguments.
use super::node_transport::NodeTransport;
use crate::config::UpstreamTransport;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
};

pub(super) struct Running {
    pub child: Child,
    pub endpoint: String,
}

impl NodeTransport {
    pub(crate) fn new(
        config: UpstreamTransport,
        proxies: Vec<String>,
        timeout: std::time::Duration,
    ) -> Arc<Self> {
        let mut secret = [0u8; 32];
        getrandom::getrandom(&mut secret).expect("OS CSPRNG unavailable");
        Arc::new(Self {
            config,
            proxies,
            timeout,
            token: hex::encode(secret),
            running: Mutex::new(None),
            stopping: std::sync::atomic::AtomicBool::new(false),
            http: reqwest::Client::builder()
                .no_proxy()
                .http1_only()
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .connect_timeout(timeout)
                .read_timeout(timeout)
                .build()
                .expect("IPC HTTP client"),
        })
    }

    async fn spawn_process(&self) -> Result<Running, String> {
        use std::process::Stdio;
        let mut child = Command::new(&self.config.node_path)
            .arg(std::path::Path::new(&self.config.transport_dir).join("main.mjs"))
            .env("NODE_TLS_REJECT_UNAUTHORIZED", "1")
            .env_remove("NODE_OPTIONS")
            .env_remove("OPENSSL_CONF")
            .env_remove("OPENSSL_MODULES")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("Node transport spawn failed: {error}"))?;
        let config = serde_json::json!({ "token": self.token, "profile": self.config.profile,
            "proxies": self.proxies, "timeoutMs": self.timeout.as_millis() });
        let encoded = serde_json::to_vec(&config).map_err(|error| error.to_string())?;
        if encoded.len() > 64 * 1024 {
            return Err("Node initialization exceeds 64 KiB".into());
        }
        let stdin = child.stdin.as_mut().ok_or("Node stdin unavailable")?;
        stdin
            .write_all(&encoded)
            .await
            .map_err(|error| error.to_string())?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|error| error.to_string())?;
        let stderr = child.stderr.take().ok_or("Node stderr unavailable")?;
        // Node emits only sanitized diagnostics; do not print initialization or bodies.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(message = %line, "Node transport");
            }
        });
        let stdout = child.stdout.take().ok_or("Node stdout unavailable")?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(self.config.startup_timeout_secs),
            reader.read_line(&mut line),
        )
        .await
        .map_err(|_| "Node transport startup timed out".to_string())?
        .map_err(|error| error.to_string())?;
        if line.len() > 64 * 1024 {
            return Err("Node handshake too large".into());
        }
        let handshake: serde_json::Value = serde_json::from_str(&line).map_err(|_| {
            "Node exited without a valid profile handshake; check runtime and npm ci".to_string()
        })?;
        if handshake["version"] != 1 || handshake["profile"] != self.config.profile {
            return Err("Node profile handshake mismatch".into());
        }
        let port = handshake["port"]
            .as_u64()
            .filter(|port| *port > 0 && *port <= u16::MAX as u64)
            .ok_or("invalid Node IPC port")?;
        tracing::info!(runtime = ?handshake["runtime"]["node"], binary_sha256 = ?handshake["binarySha256"], profile = %self.config.profile, "Node TLS transport ready");
        Ok(Running {
            child,
            endpoint: format!("http://127.0.0.1:{port}"),
        })
    }

    pub(crate) async fn start(self: &Arc<Self>) -> Result<(), String> {
        let mut state = self.running.lock().await;
        if self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            return Err("Node transport is stopping".into());
        }
        if state.is_some() {
            return Ok(());
        }
        let running = self.spawn_process().await?;
        if self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            return Err("Node transport is stopping".into());
        }
        *state = Some(running);
        drop(state);
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut restarts = 0u32;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let Some(node) = weak.upgrade() else {
                    break;
                };
                if node.stopping.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let exited = {
                    let mut state = node.running.lock().await;
                    if let Some(running) = state.as_mut() {
                        match running.child.try_wait() {
                            Ok(None) => false,
                            _ => {
                                *state = None;
                                true
                            }
                        }
                    } else {
                        true
                    }
                };
                if !exited {
                    continue;
                }
                if restarts >= 3 {
                    tracing::error!("Node transport restart limit reached");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1 << restarts)).await;
                restarts += 1;
                if node.stopping.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                match node.spawn_process().await {
                    Ok(running) => {
                        // Check and install under the same lock shutdown takes.
                        let mut state = node.running.lock().await;
                        if !node.stopping.load(std::sync::atomic::Ordering::Acquire) {
                            *state = Some(running);
                        }
                    }
                    Err(error) => tracing::error!(%error, "Node transport restart failed"),
                }
            }
        });
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(mut running) = self.running.lock().await.take() {
            if let Some(stdin) = running.child.stdin.as_mut() {
                let _ = stdin.write_all(b"shutdown\n").await;
            }
            if tokio::time::timeout(std::time::Duration::from_secs(5), running.child.wait())
                .await
                .is_err()
            {
                let _ = running.child.kill().await;
            }
        }
    }
}
