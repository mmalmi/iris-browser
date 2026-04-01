#[cfg(unix)]
use anyhow::Context;
use anyhow::Result;
use std::path::PathBuf;

use super::util::format_bytes;

pub(crate) fn format_daemon_status(status: &serde_json::Value, include_header: bool) -> String {
    let mut lines = Vec::new();
    if include_header {
        lines.push("Daemon Status:".to_string());
    }
    let status_text = status["status"].as_str().unwrap_or("unknown");
    lines.push(format!("  Status: {}", status_text));

    if let Some(storage) = status.get("storage") {
        lines.push(String::new());
        lines.push("Storage:".to_string());
        if let Some(total) = storage.get("total_dags") {
            lines.push(format!("  Total DAGs: {}", total));
        }
        if let Some(pinned) = storage.get("pinned_dags") {
            lines.push(format!("  Pinned DAGs: {}", pinned));
        }
        if let Some(bytes) = storage.get("total_bytes").and_then(|b| b.as_u64()) {
            lines.push(format!("  Total size: {}", format_bytes(bytes)));
        }
    }

    if let Some(webrtc) = status.get("webrtc") {
        lines.push(String::new());
        lines.push("WebRTC:".to_string());
        if webrtc
            .get("enabled")
            .and_then(|e| e.as_bool())
            .unwrap_or(false)
        {
            lines.push("  Enabled: yes".to_string());
            if let Some(total) = webrtc.get("total_peers") {
                lines.push(format!("  Total peers: {}", total));
            }
            if let Some(connected) = webrtc.get("connected") {
                lines.push(format!("  Connected: {}", connected));
            }
            if let Some(dc) = webrtc.get("with_data_channel") {
                lines.push(format!("  With data channel: {}", dc));
            }
            if let Some(sent) = webrtc.get("bytes_sent").and_then(|b| b.as_u64()) {
                lines.push(format!("  Bytes sent: {}", format_bytes(sent)));
            }
            if let Some(received) = webrtc.get("bytes_received").and_then(|b| b.as_u64()) {
                lines.push(format!("  Bytes received: {}", format_bytes(received)));
            }
        } else {
            lines.push("  Enabled: no".to_string());
        }
    }

    if let Some(upstream) = status.get("upstream") {
        if let Some(count) = upstream.get("blossom_servers").and_then(|c| c.as_u64()) {
            if count > 0 {
                lines.push(String::new());
                lines.push("Upstream:".to_string());
                lines.push(format!("  Blossom servers: {}", count));
            }
        }
    }

    lines.join("\n")
}

#[cfg(unix)]
fn default_daemon_log_file() -> PathBuf {
    hashtree_cli::config::get_hashtree_dir()
        .join("logs")
        .join("htree.log")
}

fn default_daemon_pid_file() -> PathBuf {
    hashtree_cli::config::get_hashtree_dir().join("htree.pid")
}

#[cfg(unix)]
pub(crate) fn build_daemon_args(
    addr: &str,
    relays: Option<&str>,
    data_dir: Option<&PathBuf>,
) -> Vec<std::ffi::OsString> {
    let mut args = Vec::new();
    args.push(std::ffi::OsString::from("--addr"));
    args.push(std::ffi::OsString::from(addr));
    if let Some(relays) = relays {
        args.push(std::ffi::OsString::from("--relays"));
        args.push(std::ffi::OsString::from(relays));
    }
    if let Some(data_dir) = data_dir {
        args.push(std::ffi::OsString::from("--data-dir"));
        args.push(data_dir.as_os_str().to_owned());
    }
    args
}

pub(crate) fn spawn_daemon(
    addr: &str,
    relays: Option<&str>,
    data_dir: Option<PathBuf>,
    log_file: Option<&PathBuf>,
    pid_file: Option<&PathBuf>,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::{self, OpenOptions};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let log_path = log_file.cloned().unwrap_or_else(default_daemon_log_file);
        let pid_path = pid_file.cloned().unwrap_or_else(default_daemon_pid_file);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create log dir {}", parent.display()))?;
        }
        if let Some(parent) = pid_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create pid dir {}", parent.display()))?;
        }

        if pid_path.exists() {
            let pid = read_pid_file(&pid_path)
                .with_context(|| format!("Failed to read pid file {}", pid_path.display()))?;
            if is_process_running(pid) {
                anyhow::bail!("Daemon already running (pid {})", pid);
            }
            fs::remove_file(&pid_path).with_context(|| {
                format!("Failed to remove stale pid file {}", pid_path.display())
            })?;
        }

        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("Failed to open log file {}", log_path.display()))?;
        let log_err = log.try_clone().context("Failed to clone log file handle")?;

        let exe = std::env::current_exe().context("Failed to locate htree binary")?;
        let mut cmd = Command::new(exe);
        cmd.arg("start")
            .args(build_daemon_args(addr, relays, data_dir.as_ref()))
            .env("HTREE_DAEMONIZED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));

        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn().context("Failed to spawn daemon")?;
        write_pid_file(&pid_path, child.id())
            .with_context(|| format!("Failed to write pid file {}", pid_path.display()))?;
        println!("Started hashtree daemon (pid {})", child.id());
        println!("Log file: {}", log_path.display());
        println!("PID file: {}", pid_path.display());
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = addr;
        let _ = relays;
        let _ = data_dir;
        let _ = log_file;
        let _ = pid_file;
        anyhow::bail!("Daemon mode is only supported on Unix systems");
    }
}

#[cfg(unix)]
pub(crate) fn parse_pid(contents: &str) -> Result<i32> {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        anyhow::bail!("PID file is empty");
    }
    let pid: i32 = trimmed.parse().context("Invalid PID value")?;
    if pid <= 0 {
        anyhow::bail!("PID must be a positive integer");
    }
    Ok(pid)
}

#[cfg(unix)]
pub(crate) fn read_pid_file(path: &std::path::Path) -> Result<i32> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read pid file {}", path.display()))?;
    parse_pid(&contents)
}

#[cfg(unix)]
pub(crate) fn write_pid_file(path: &std::path::Path, pid: u32) -> Result<()> {
    std::fs::write(path, format!("{}\n", pid))
        .with_context(|| format!("Failed to write pid file {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn is_process_running(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == libc::ESRCH => false,
        Some(code) if code == libc::EPERM => true,
        _ => false,
    }
}

#[cfg(unix)]
fn signal_process(pid: i32, signal: i32) -> Result<()> {
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    anyhow::bail!("Failed to signal pid {}: {}", pid, err);
}

pub(crate) fn stop_daemon(pid_file: Option<&PathBuf>) -> Result<()> {
    let pid_path = pid_file.cloned().unwrap_or_else(default_daemon_pid_file);

    #[cfg(unix)]
    {
        let pid = read_pid_file(&pid_path)?;
        if !is_process_running(pid) {
            let _ = std::fs::remove_file(&pid_path);
            anyhow::bail!("Daemon not running (pid {})", pid);
        }

        signal_process(pid, libc::SIGTERM)?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !is_process_running(pid) {
                std::fs::remove_file(&pid_path)
                    .with_context(|| format!("Failed to remove pid file {}", pid_path.display()))?;
                println!("Stopped hashtree daemon (pid {})", pid);
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        anyhow::bail!("Timed out waiting for daemon to stop (pid {})", pid);
    }

    #[cfg(not(unix))]
    {
        let _ = pid_path;
        anyhow::bail!("Daemon stop is only supported on Unix systems");
    }
}
