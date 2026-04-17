use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
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
const LIVE_BACKEND_TERMINATE_TIMEOUT: Duration = Duration::from_secs(3);
#[allow(dead_code)]
const LIVE_BACKEND_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
#[allow(dead_code)]
const LIVE_BACKEND_LOCK_STALE_AFTER: Duration = Duration::from_secs(120);

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LiveBackendStatus {
    pub(crate) websocket_url: String,
    pub(crate) pid: Option<u32>,
    #[serde(default)]
    pub(crate) process_start_key: Option<String>,
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
struct LiveBackendLock {
    path: PathBuf,
}

impl Drop for LiveBackendLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[allow(dead_code)]
pub(crate) fn live_backend_status(config: &CodexConfig) -> Result<LiveBackendStatus> {
    let _lock = acquire_live_backend_lock()?;
    live_backend_status_unlocked(config)
}

#[allow(dead_code)]
pub(crate) fn ensure_live_backend(config: &CodexConfig) -> Result<EnsureLiveBackendResult> {
    let _lock = acquire_live_backend_lock()?;
    let status = live_backend_status_unlocked(config)?;
    if status.healthy {
        return Ok(EnsureLiveBackendResult {
            action: "reused".to_string(),
            status,
        });
    }

    if let Some(pid) = status.pid {
        terminate_managed_backend_pid(
            pid,
            &config.websocket_url,
            status.process_start_key.as_deref(),
        );
    }

    start_live_backend(config, "started")
}

#[allow(dead_code)]
pub(crate) fn reset_live_backend(config: &CodexConfig) -> Result<EnsureLiveBackendResult> {
    let _lock = acquire_live_backend_lock()?;
    if let Some(status) = read_live_backend_status()? {
        if let Some(pid) = status.pid {
            terminate_managed_backend_pid(
                pid,
                &status.websocket_url,
                status.process_start_key.as_deref(),
            );
        }
    }

    start_live_backend(config, "restarted")
}

#[allow(dead_code)]
fn live_backend_status_unlocked(config: &CodexConfig) -> Result<LiveBackendStatus> {
    let mut status = read_live_backend_status()?.unwrap_or_else(|| LiveBackendStatus {
        websocket_url: config.websocket_url.clone(),
        pid: None,
        process_start_key: None,
        healthy: false,
        last_error: None,
    });

    if status.websocket_url != config.websocket_url {
        status = LiveBackendStatus {
            websocket_url: config.websocket_url.clone(),
            pid: None,
            process_start_key: None,
            healthy: false,
            last_error: None,
        };
    }

    if status.pid.is_none() {
        if let Some(pid) = discover_backend_pid_for_websocket_url(&config.websocket_url) {
            status.pid = Some(pid);
            status.process_start_key = backend_process_start_key(pid);
        }
    }

    let managed_pid_matches = match status.pid {
        Some(pid) => backend_pid_matches(
            pid,
            &config.websocket_url,
            status.process_start_key.as_deref(),
        ),
        None => false,
    };
    match verify_live_backend_health(&config.websocket_url) {
        Ok(()) if managed_pid_matches => {
            status.healthy = true;
            status.last_error = None;
        }
        Ok(()) => {
            status.healthy = false;
            status.last_error = Some(match status.pid {
                Some(pid) => format!(
                    "managed live backend process {pid} is not running with the configured websocket URL"
                ),
                None => {
                    "websocket URL answered but no managed codex app-server process was found"
                        .to_string()
                }
            });
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
        if !backend_pid_matches(pid, &config.websocket_url, None) {
            last_error = Some(format!(
                "managed live backend process {pid} exited before becoming healthy with the configured websocket URL"
            ));
            break;
        }

        match verify_live_backend_health(&config.websocket_url) {
            Ok(()) => {
                let status = LiveBackendStatus {
                    websocket_url: config.websocket_url.clone(),
                    pid: Some(pid),
                    process_start_key: backend_process_start_key(pid),
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

    let process_start_key = backend_process_start_key(pid);
    terminate_managed_backend_pid(pid, &config.websocket_url, process_start_key.as_deref());
    let status = LiveBackendStatus {
        websocket_url: config.websocket_url.clone(),
        pid: Some(pid),
        process_start_key: backend_process_start_key(pid),
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
fn live_backend_lock_path() -> Result<PathBuf> {
    Ok(live_backend_status_path()?.with_file_name("live-backend.lock"))
}

#[allow(dead_code)]
fn acquire_live_backend_lock() -> Result<LiveBackendLock> {
    let path = live_backend_lock_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let started = Instant::now();
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "pid={}", std::process::id())?;
                return Ok(LiveBackendLock { path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if live_backend_lock_is_stale(&path) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if started.elapsed() >= LIVE_BACKEND_LOCK_TIMEOUT {
                    bail!(
                        "live backend state is locked at {}; try again shortly or remove stale lock if no bridge process is active",
                        path.display()
                    );
                }
                thread::sleep(LIVE_BACKEND_HEALTH_POLL_INTERVAL);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create live backend lock at {}", path.display())
                });
            }
        }
    }
}

#[allow(dead_code)]
fn live_backend_lock_is_stale(path: &PathBuf) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .map(|elapsed| elapsed > LIVE_BACKEND_LOCK_STALE_AFTER)
        .unwrap_or(false)
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
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("live backend status path missing file name")?;
    let tmp_path = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    fs::write(&tmp_path, serde_json::to_vec_pretty(status)?)?;
    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "failed to replace live backend status file {}",
            path.display()
        )
    })?;
    Ok(())
}

#[allow(dead_code)]
fn spawn_live_backend_process(config: &CodexConfig) -> Result<u32> {
    #[cfg(test)]
    if let Some(pid) = spawn_test_live_backend(&config.websocket_url)? {
        return Ok(pid);
    }

    let resolved = resolve_codex_binary()?;

    #[cfg(unix)]
    {
        let output = Command::new("sh")
            .arg("-c")
            .arg(
                r#"
binary=$1
url=$2
nohup "$binary" app-server --listen "$url" </dev/null >/dev/null 2>&1 &
printf '%s\n' "$!"
"#,
            )
            .arg("codex-live-backend")
            .arg(&resolved.path)
            .arg(&config.websocket_url)
            .output()
            .with_context(|| format!("failed to spawn {} app-server", resolved.path.display()))?;

        if !output.status.success() {
            bail!(
                "failed to spawn {} app-server: {}",
                resolved.path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let pid = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .with_context(|| {
                format!(
                    "failed to parse spawned app-server pid from {}",
                    String::from_utf8_lossy(&output.stdout).trim()
                )
            })?;
        return Ok(pid);
    }

    #[cfg(not(unix))]
    {
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
}

#[allow(dead_code)]
fn terminate_managed_backend_pid(pid: u32, websocket_url: &str, process_start_key: Option<&str>) {
    if backend_pid_matches(pid, websocket_url, process_start_key) {
        terminate_backend_pid(pid);
    }
}

#[allow(dead_code)]
fn terminate_backend_pid(pid: u32) {
    #[cfg(test)]
    if terminate_test_live_backend(pid) {
        return;
    }

    #[cfg(test)]
    if test_fake_spawn_enabled() {
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
        wait_for_backend_pid_exit(pid, LIVE_BACKEND_TERMINATE_TIMEOUT);
        if backend_pid_is_alive(pid) {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            wait_for_backend_pid_exit(pid, LIVE_BACKEND_TERMINATE_TIMEOUT);
        }
    }

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        wait_for_backend_pid_exit(pid, LIVE_BACKEND_TERMINATE_TIMEOUT);
    }
}

#[allow(dead_code)]
fn discover_backend_pid_for_websocket_url(websocket_url: &str) -> Option<u32> {
    #[cfg(test)]
    if test_fake_spawn_enabled() {
        return test_backend_registry()
            .lock()
            .expect("test backend registry lock")
            .iter()
            .find_map(|(pid, handle)| (handle.websocket_url == websocket_url).then_some(*pid));
    }

    #[cfg(unix)]
    {
        let output = Command::new("ps")
            .args(["ax", "-o", "pid=,command="])
            .output();
        return output
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .find_map(|line| {
                        let trimmed = line.trim_start();
                        let (pid, command) = trimmed.split_once(char::is_whitespace)?;
                        if command.contains("app-server")
                            && command.contains("--listen")
                            && command.contains(websocket_url)
                        {
                            pid.parse::<u32>().ok()
                        } else {
                            None
                        }
                    })
            });
    }

    #[cfg(windows)]
    {
        return None;
    }

    #[allow(unreachable_code)]
    None
}

#[allow(dead_code)]
fn backend_pid_matches(pid: u32, websocket_url: &str, process_start_key: Option<&str>) -> bool {
    if !backend_pid_is_alive(pid) {
        return false;
    }

    let command_matches = backend_pid_command_matches(pid, websocket_url);
    if !command_matches {
        return false;
    }

    match process_start_key {
        Some(expected) => backend_process_start_key(pid).as_deref() == Some(expected),
        None => true,
    }
}

#[allow(dead_code)]
fn backend_pid_command_matches(pid: u32, websocket_url: &str) -> bool {
    #[cfg(test)]
    if test_fake_spawn_enabled() {
        return test_backend_registry()
            .lock()
            .expect("test backend registry lock")
            .get(&pid)
            .map(|handle| handle.websocket_url == websocket_url)
            .unwrap_or(false);
    }

    #[cfg(unix)]
    {
        let pid_arg = pid.to_string();
        let output = Command::new("ps")
            .args(["-p", &pid_arg, "-o", "command="])
            .output();
        return output
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                let command = String::from_utf8_lossy(&output.stdout);
                command.contains("app-server")
                    && command.contains("--listen")
                    && command.contains(websocket_url)
            })
            .unwrap_or(false);
    }

    #[cfg(windows)]
    {
        return true;
    }

    #[allow(unreachable_code)]
    false
}

#[allow(dead_code)]
fn backend_process_start_key(pid: u32) -> Option<String> {
    #[cfg(test)]
    if test_fake_spawn_enabled() {
        return test_backend_registry()
            .lock()
            .expect("test backend registry lock")
            .get(&pid)
            .map(|handle| handle.process_start_key.clone());
    }

    #[cfg(unix)]
    {
        let pid_arg = pid.to_string();
        let output = Command::new("ps")
            .args(["-p", &pid_arg, "-o", "lstart="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let started = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return (!started.is_empty()).then_some(started);
    }

    #[cfg(windows)]
    {
        return None;
    }

    #[allow(unreachable_code)]
    None
}

#[allow(dead_code)]
fn wait_for_backend_pid_exit(pid: u32, timeout: Duration) {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if !backend_pid_is_alive(pid) {
            return;
        }
        thread::sleep(LIVE_BACKEND_HEALTH_POLL_INTERVAL);
    }
}

#[allow(dead_code)]
fn backend_pid_is_alive(pid: u32) -> bool {
    #[cfg(test)]
    if test_fake_spawn_enabled() {
        return test_backend_registry()
            .lock()
            .expect("test backend registry lock")
            .contains_key(&pid);
    }

    #[cfg(unix)]
    {
        return Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
    }

    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        return Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
            .map(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            })
            .unwrap_or(false);
    }

    #[allow(unreachable_code)]
    false
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
    websocket_url: String,
    process_start_key: String,
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
fn test_fake_spawn_enabled() -> bool {
    std::env::var("CODEX_LIVE_TEST_FAKE_SPAWN").ok().as_deref() == Some("1")
}

#[cfg(test)]
fn spawn_test_live_backend(websocket_url: &str) -> Result<Option<u32>> {
    if !test_fake_spawn_enabled() {
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
                websocket_url: websocket_url.to_string(),
                process_start_key: format!("test-backend-{pid}"),
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
            process_start_key: backend_process_start_key(pid),
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
            process_start_key: Some("stale-test-process".to_string()),
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

    #[test]
    fn wait_for_live_backend_rejects_dead_managed_pid_even_if_socket_is_healthy() {
        let _guard = live_test_lock().lock().expect("live test lock");
        let _home = TempHome::new("dead-pid");
        let websocket_url = random_websocket_url();
        let _env = LiveTestEnv::fake_spawn();

        let healthy_pid = spawn_test_live_backend(&websocket_url)
            .expect("spawn test backend")
            .expect("test backend pid");
        wait_until_healthy(&websocket_url);
        let dead_pid = healthy_pid + 1;

        let error = wait_for_live_backend(&shared_codex_config(&websocket_url), dead_pid)
            .expect_err("dead managed pid should fail");

        assert!(format!("{error:#}").contains("exited before becoming healthy"));
        terminate_backend_pid(healthy_pid);
    }

    #[test]
    fn live_backend_status_marks_dead_managed_pid_unhealthy_even_if_socket_is_healthy() {
        let _guard = live_test_lock().lock().expect("live test lock");
        let _home = TempHome::new("status-dead-pid");
        let websocket_url = random_websocket_url();
        let _env = LiveTestEnv::fake_spawn();

        let healthy_pid = spawn_test_live_backend(&websocket_url)
            .expect("spawn test backend")
            .expect("test backend pid");
        wait_until_healthy(&websocket_url);
        let dead_pid = healthy_pid + 1;
        write_live_backend_status(&LiveBackendStatus {
            websocket_url: websocket_url.clone(),
            pid: Some(dead_pid),
            process_start_key: Some("dead-test-process".to_string()),
            healthy: true,
            last_error: None,
        })
        .expect("write stale status");

        let status = live_backend_status(&shared_codex_config(&websocket_url)).expect("status");

        assert!(!status.healthy);
        assert_eq!(status.pid, Some(dead_pid));
        let expected_error = format!(
            "managed live backend process {dead_pid} is not running with the configured websocket URL"
        );
        assert_eq!(status.last_error.as_deref(), Some(expected_error.as_str()));
        terminate_backend_pid(healthy_pid);
    }

    #[test]
    fn live_backend_status_adopts_managed_process_when_status_file_is_missing() {
        let _guard = live_test_lock().lock().expect("live test lock");
        let _home = TempHome::new("status-adopt");
        let websocket_url = random_websocket_url();
        let _env = LiveTestEnv::fake_spawn();

        let pid = spawn_test_live_backend(&websocket_url)
            .expect("spawn test backend")
            .expect("test backend pid");
        wait_until_healthy(&websocket_url);

        let status = live_backend_status(&shared_codex_config(&websocket_url)).expect("status");

        assert!(status.healthy);
        assert_eq!(status.pid, Some(pid));
        assert_eq!(status.process_start_key, backend_process_start_key(pid));
        terminate_backend_pid(pid);
    }

    #[test]
    fn terminate_managed_backend_pid_requires_matching_process_start_key() {
        let _guard = live_test_lock().lock().expect("live test lock");
        let _home = TempHome::new("terminate-start-key");
        let websocket_url = random_websocket_url();
        let _env = LiveTestEnv::fake_spawn();

        let pid = spawn_test_live_backend(&websocket_url)
            .expect("spawn test backend")
            .expect("test backend pid");
        wait_until_healthy(&websocket_url);

        terminate_managed_backend_pid(pid, &websocket_url, Some("wrong-start-key"));

        assert!(backend_pid_matches(pid, &websocket_url, None));
        terminate_backend_pid(pid);
    }

    #[test]
    fn reset_live_backend_does_not_terminate_pid_that_does_not_match_recorded_url() {
        let _guard = live_test_lock().lock().expect("live test lock");
        let _home = TempHome::new("reset-stale-pid");
        let current_url = random_websocket_url();
        let stale_recorded_url = random_websocket_url();
        let reset_url = random_websocket_url();
        let _env = LiveTestEnv::fake_spawn();

        let current_pid = spawn_test_live_backend(&current_url)
            .expect("spawn test backend")
            .expect("test backend pid");
        wait_until_healthy(&current_url);
        write_live_backend_status(&LiveBackendStatus {
            websocket_url: stale_recorded_url,
            pid: Some(current_pid),
            process_start_key: backend_process_start_key(current_pid),
            healthy: false,
            last_error: Some("stale pid".to_string()),
        })
        .expect("write stale status");

        let result = reset_live_backend(&shared_codex_config(&reset_url)).expect("reset");

        assert!(backend_pid_matches(current_pid, &current_url, None));
        assert!(result.status.healthy);
        assert_ne!(result.status.pid, Some(current_pid));
        terminate_backend_pid(current_pid);
        terminate_backend_pid(result.status.pid.expect("reset pid"));
    }

    #[test]
    fn live_backend_lock_is_released_on_drop() {
        let _guard = live_test_lock().lock().expect("live test lock");
        let _home = TempHome::new("lock-release");

        let lock = acquire_live_backend_lock().expect("acquire lock");
        let path = live_backend_lock_path().expect("lock path");
        assert!(path.exists());

        drop(lock);

        assert!(!path.exists());
    }

    #[test]
    fn write_live_backend_status_replaces_existing_status_without_tmp_leftover() {
        let _guard = live_test_lock().lock().expect("live test lock");
        let _home = TempHome::new("atomic-status");
        let websocket_url = random_websocket_url();
        let initial = LiveBackendStatus {
            websocket_url: websocket_url.clone(),
            pid: Some(1),
            process_start_key: None,
            healthy: false,
            last_error: Some("starting".to_string()),
        };
        let updated = LiveBackendStatus {
            websocket_url,
            pid: Some(2),
            process_start_key: None,
            healthy: true,
            last_error: None,
        };

        write_live_backend_status(&initial).expect("write initial");
        write_live_backend_status(&updated).expect("write updated");

        let persisted = read_live_backend_status()
            .expect("read status")
            .expect("persisted status");
        let status_path = live_backend_status_path().expect("status path");
        let tmp_leftovers = fs::read_dir(status_path.parent().expect("status parent"))
            .expect("read status dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("live-backend.json.tmp")
            })
            .count();

        assert_eq!(persisted, updated);
        assert_eq!(tmp_leftovers, 0);
    }
}
