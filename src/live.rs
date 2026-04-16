use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::codex::{resolve_codex_binary, CodexAppServerClient, CodexBackend};
use crate::config::CodexConfig;
use crate::state::live_backend_status_path;

#[allow(dead_code)]
const LIVE_BACKEND_HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
#[allow(dead_code)]
const LIVE_BACKEND_HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LiveBackendStatus {
    pub(crate) websocket_url: String,
    pub(crate) pid: Option<u32>,
    pub(crate) healthy: bool,
    pub(crate) last_error: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnsureLiveBackendResult {
    pub(crate) action: String,
    pub(crate) status: LiveBackendStatus,
}

#[allow(dead_code)]
pub(crate) fn live_backend_status(config: &CodexConfig) -> Result<LiveBackendStatus> {
    let mut status = read_live_backend_status()?.unwrap_or_else(|| LiveBackendStatus {
        websocket_url: config.websocket_url.clone(),
        pid: None,
        healthy: false,
        last_error: None,
    });

    if status.websocket_url != config.websocket_url {
        status = LiveBackendStatus {
            websocket_url: config.websocket_url.clone(),
            pid: None,
            healthy: false,
            last_error: None,
        };
    }

    match verify_live_backend_health(&config.websocket_url) {
        Ok(()) => {
            status.healthy = true;
            status.last_error = None;
        }
        Err(error) => {
            status.healthy = false;
            status.last_error = Some(format!("{error:#}"));
        }
    }

    write_live_backend_status(&status)?;
    Ok(status)
}

#[allow(dead_code)]
pub(crate) fn ensure_live_backend(config: &CodexConfig) -> Result<EnsureLiveBackendResult> {
    let status = live_backend_status(config)?;
    if status.healthy {
        return Ok(EnsureLiveBackendResult {
            action: "reused".to_string(),
            status,
        });
    }

    if let Some(pid) = status.pid {
        terminate_backend_pid(pid);
    }

    start_live_backend(config, "started")
}

#[allow(dead_code)]
pub(crate) fn reset_live_backend(config: &CodexConfig) -> Result<EnsureLiveBackendResult> {
    if let Some(status) = read_live_backend_status()? {
        if let Some(pid) = status.pid {
            terminate_backend_pid(pid);
        }
    }

    start_live_backend(config, "restarted")
}

#[allow(dead_code)]
fn start_live_backend(config: &CodexConfig, action: &str) -> Result<EnsureLiveBackendResult> {
    let pid = spawn_live_backend_process(config)?;

    let status = wait_for_live_backend(config, pid).with_context(|| {
        format!(
            "managed live backend failed to become healthy at {}",
            config.websocket_url
        )
    })?;

    Ok(EnsureLiveBackendResult {
        action: action.to_string(),
        status,
    })
}

#[allow(dead_code)]
fn wait_for_live_backend(config: &CodexConfig, pid: u32) -> Result<LiveBackendStatus> {
    let started = Instant::now();
    let mut last_error = None;

    while started.elapsed() < LIVE_BACKEND_HEALTH_TIMEOUT {
        match verify_live_backend_health(&config.websocket_url) {
            Ok(()) => {
                let status = LiveBackendStatus {
                    websocket_url: config.websocket_url.clone(),
                    pid: Some(pid),
                    healthy: true,
                    last_error: None,
                };
                write_live_backend_status(&status)?;
                return Ok(status);
            }
            Err(error) => {
                last_error = Some(format!("{error:#}"));
                thread::sleep(LIVE_BACKEND_HEALTH_POLL_INTERVAL);
            }
        }
    }

    terminate_backend_pid(pid);
    let status = LiveBackendStatus {
        websocket_url: config.websocket_url.clone(),
        pid: Some(pid),
        healthy: false,
        last_error,
    };
    write_live_backend_status(&status)?;

    let message = status
        .last_error
        .clone()
        .unwrap_or_else(|| "unknown live backend startup failure".to_string());
    bail!("{message}");
}

#[allow(dead_code)]
fn verify_live_backend_health(websocket_url: &str) -> Result<()> {
    let _client = CodexAppServerClient::connect_with_backend(CodexBackend::SharedWebsocket {
        url: websocket_url.to_string(),
    })?;
    Ok(())
}

#[allow(dead_code)]
fn read_live_backend_status() -> Result<Option<LiveBackendStatus>> {
    let path = live_backend_status_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read live backend status at {}", path.display()))?;
    let status = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse live backend status at {}", path.display()))?;
    Ok(Some(status))
}

#[allow(dead_code)]
fn write_live_backend_status(status: &LiveBackendStatus) -> Result<()> {
    let path = live_backend_status_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_vec_pretty(status)?)?;
    Ok(())
}

#[allow(dead_code)]
fn spawn_live_backend_process(config: &CodexConfig) -> Result<u32> {
    #[cfg(test)]
    if let Some(pid) = spawn_test_live_backend(&config.websocket_url)? {
        return Ok(pid);
    }

    let resolved = resolve_codex_binary()?;
    let child = Command::new(&resolved.path)
        .arg("app-server")
        .arg("--listen")
        .arg(&config.websocket_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {} app-server", resolved.path.display()))?;

    Ok(child.id())
}

#[allow(dead_code)]
fn terminate_backend_pid(pid: u32) {
    #[cfg(test)]
    if terminate_test_live_backend(pid) {
        return;
    }

    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::net::{TcpListener, TcpStream};
#[cfg(test)]
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(test)]
use std::sync::mpsc::{self, Sender, TryRecvError};
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
#[cfg(test)]
use std::thread::JoinHandle;
#[cfg(test)]
use tungstenite::Message;
#[cfg(test)]
use url::Url;

#[cfg(test)]
struct TestBackendHandle {
    address: String,
    stop_tx: Sender<()>,
    join: JoinHandle<()>,
}

#[cfg(test)]
fn test_backend_registry() -> &'static Mutex<BTreeMap<u32, TestBackendHandle>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<u32, TestBackendHandle>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(test)]
fn next_test_backend_pid() -> u32 {
    static NEXT_PID: AtomicU32 = AtomicU32::new(50_000);
    NEXT_PID.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
fn spawn_test_live_backend(websocket_url: &str) -> Result<Option<u32>> {
    if std::env::var("CODEX_LIVE_TEST_FAKE_SPAWN").ok().as_deref() != Some("1") {
        return Ok(None);
    }

    let parsed = Url::parse(websocket_url)
        .with_context(|| format!("invalid websocket url for test backend: {websocket_url}"))?;
    let host = parsed
        .host_str()
        .context("test websocket url missing host")?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .context("test websocket url missing port")?;
    let address = format!("{host}:{port}");
    let listener = TcpListener::bind(&address)
        .with_context(|| format!("bind fake live backend at {address}"))?;

    let (stop_tx, stop_rx) = mpsc::channel();
    let join = std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((stream, _)) => {
                match stop_rx.try_recv() {
                    Ok(()) | Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => {}
                }

                let mut socket = match tungstenite::accept(stream) {
                    Ok(socket) => socket,
                    Err(_) => continue,
                };

                let initialize = match socket.read() {
                    Ok(message) => message,
                    Err(_) => continue,
                };
                let initialize = match initialize.into_text() {
                    Ok(text) => text,
                    Err(_) => continue,
                };
                let initialize: serde_json::Value = match serde_json::from_str(&initialize) {
                    Ok(value) => value,
                    Err(_) => continue,
                };

                let _ = socket.send(Message::Text(
                    serde_json::to_string(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": initialize["id"],
                        "result": {
                            "protocolVersion": 1,
                            "serverInfo": { "name": "fake-codex", "version": "test" }
                        }
                    }))
                    .expect("serialize initialize response")
                    .into(),
                ));

                let _ = socket
                    .get_mut()
                    .set_read_timeout(Some(Duration::from_millis(200)));
                let _ = socket.read();
            }
            Err(_) => break,
        }
    });

    let pid = next_test_backend_pid();
    test_backend_registry()
        .lock()
        .expect("test backend registry lock")
        .insert(
            pid,
            TestBackendHandle {
                address: address.clone(),
                stop_tx,
                join,
            },
        );
    Ok(Some(pid))
}

#[cfg(test)]
fn terminate_test_live_backend(pid: u32) -> bool {
    let handle = test_backend_registry()
        .lock()
        .expect("test backend registry lock")
        .remove(&pid);
    if let Some(handle) = handle {
        let _ = handle.stop_tx.send(());
        let _ = TcpStream::connect(&handle.address);
        let _ = handle.join.join();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempHome {
        previous_home: Option<String>,
        root: PathBuf,
    }

    impl TempHome {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "codex-live-test-{name}-{}-{}",
                std::process::id(),
                next_test_backend_pid()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("create temp home");
            let previous_home = std::env::var("HOME").ok();
            std::env::set_var("HOME", &root);
            Self {
                previous_home,
                root,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            if let Some(previous_home) = &self.previous_home {
                std::env::set_var("HOME", previous_home);
            } else {
                std::env::remove_var("HOME");
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct LiveTestEnv {
        previous_spawn: Option<String>,
    }

    impl LiveTestEnv {
        fn fake_spawn() -> Self {
            let previous_spawn = std::env::var("CODEX_LIVE_TEST_FAKE_SPAWN").ok();
            std::env::set_var("CODEX_LIVE_TEST_FAKE_SPAWN", "1");
            Self { previous_spawn }
        }
    }

    impl Drop for LiveTestEnv {
        fn drop(&mut self) {
            if let Some(previous_spawn) = &self.previous_spawn {
                std::env::set_var("CODEX_LIVE_TEST_FAKE_SPAWN", previous_spawn);
            } else {
                std::env::remove_var("CODEX_LIVE_TEST_FAKE_SPAWN");
            }
        }
    }

    fn live_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn shared_codex_config(websocket_url: &str) -> CodexConfig {
        CodexConfig {
            live_mode: crate::CodexLiveMode::Shared,
            websocket_url: websocket_url.to_string(),
        }
    }

    fn random_websocket_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind random port");
        let address = listener.local_addr().expect("local addr");
        drop(listener);
        format!("ws://{address}")
    }

    fn wait_until_healthy(websocket_url: &str) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if verify_live_backend_health(websocket_url).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("fake live backend did not become healthy at {websocket_url}");
    }

    #[test]
    fn ensure_live_backend_reuses_healthy_backend() {
        let _guard = live_test_lock().lock().expect("live test lock");
        let _home = TempHome::new("reuse");
        let websocket_url = random_websocket_url();
        let _env = LiveTestEnv::fake_spawn();

        let pid = spawn_test_live_backend(&websocket_url)
            .expect("spawn test backend")
            .expect("test backend pid");
        wait_until_healthy(&websocket_url);
        let initial = LiveBackendStatus {
            websocket_url: websocket_url.clone(),
            pid: Some(pid),
            healthy: true,
            last_error: None,
        };
        write_live_backend_status(&initial).expect("write initial status");

        let result = ensure_live_backend(&shared_codex_config(&websocket_url)).expect("ensure");

        assert_eq!(result.action, "reused");
        assert_eq!(result.status.pid, Some(pid));
        assert!(result.status.healthy);

        terminate_backend_pid(pid);
    }

    #[test]
    fn reset_live_backend_restarts_unhealthy_backend() {
        let _guard = live_test_lock().lock().expect("live test lock");
        let _home = TempHome::new("reset");
        let websocket_url = random_websocket_url();
        let _env = LiveTestEnv::fake_spawn();

        write_live_backend_status(&LiveBackendStatus {
            websocket_url: websocket_url.clone(),
            pid: Some(41_242),
            healthy: false,
            last_error: Some("socket closed".to_string()),
        })
        .expect("write unhealthy status");

        let result = reset_live_backend(&shared_codex_config(&websocket_url)).expect("reset");

        assert_eq!(result.action, "restarted");
        assert!(result.status.healthy);
        assert!(result.status.pid.is_some());
        assert_eq!(result.status.websocket_url, websocket_url);
        assert_eq!(result.status.last_error, None);

        let persisted = read_live_backend_status()
            .expect("read status")
            .expect("persisted status");
        assert_eq!(persisted, result.status);

        terminate_backend_pid(result.status.pid.expect("pid"));
    }
}
