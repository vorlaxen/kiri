//! `kiri engine`: start, stop, restart, and inspect the Kiri engine lifecycle
//! from the CLI (#983).
//!
//! Uses the same daemon endpoints as the GUI window (`POST /kiri/wake`,
//! `POST /kiri/stop`, `POST /kiri/sleep`, and background `GET /info`), keeping
//! GUI and CLI states synchronized without terminating the daemon.

use std::io;

use serde_json::{Value, json};

use crate::client;

pub const USAGE: &str = "\
usage: kiri engine [status] [--json]   show engine state, version, and resource usage
       kiri engine start               start the Kiri engine (and wake the VM)
       kiri engine stop                stop containers and power off the VM (keeps daemon alive)
       kiri engine restart             restart the Kiri engine";

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Help,
    Start,
    Stop,
    Restart,
    Status { json: bool },
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    if args.iter().any(|a| matches!(a.as_str(), "-h" | "--help")) {
        return match args.first().map(String::as_str) {
            Some("-h" | "--help" | "help" | "start" | "stop" | "restart" | "status" | "--json") => {
                Ok(Cmd::Help)
            }
            _ => Err(USAGE.into()),
        };
    }
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["status"] => Ok(Cmd::Status { json: false }),
        ["--json"] | ["status", "--json"] => Ok(Cmd::Status { json: true }),
        ["help"] => Ok(Cmd::Help),
        ["start"] => Ok(Cmd::Start),
        ["stop"] => Ok(Cmd::Stop),
        ["restart"] => Ok(Cmd::Restart),
        _ => Err(USAGE.into()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EngineState {
    /// Daemon is running and the VM / runtime is awake.
    Running,
    /// Daemon is running, VM is idle-asleep (wakes on next command).
    Asleep,
    /// Daemon is running, engine is explicitly stopped until Start.
    Stopped,
    /// Daemon is not reachable.
    NotRunning,
}

impl EngineState {
    fn json_name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Asleep => "asleep",
            Self::Stopped => "stopped",
            Self::NotRunning => "not_running",
        }
    }

    /// Exit code for `kiri engine status`: 0 when available (`running` or
    /// `asleep`), 1 when `stopped` or `not_running`.
    fn exit_code(self) -> u8 {
        match self {
            Self::Running | Self::Asleep => 0,
            Self::Stopped | Self::NotRunning => 1,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StatusReport {
    state: EngineState,
    daemon_running: bool,
    vm_awake: bool,
    stopped: bool,
    version: Option<String>,
    containers_running: Option<u64>,
    containers_total: Option<u64>,
    cpus: Option<u64>,
    memory_used_bytes: Option<u64>,
    memory_ceiling_bytes: Option<u64>,
}

impl StatusReport {
    fn from_info(info: &Value) -> Self {
        let vm_awake = info["KiriVmAwake"].as_bool().unwrap_or(false);
        let stopped = !vm_awake && info["KiriStopped"] == true;
        let state = if vm_awake {
            EngineState::Running
        } else if stopped {
            EngineState::Stopped
        } else {
            EngineState::Asleep
        };
        let version = info["KiriVersion"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| info["ServerVersion"].as_str().filter(|s| !s.is_empty()))
            .map(str::to_owned);
        Self {
            state,
            daemon_running: true,
            vm_awake,
            stopped,
            version,
            containers_running: info["ContainersRunning"].as_u64(),
            containers_total: info["Containers"].as_u64(),
            cpus: info["NCPU"].as_u64(),
            memory_used_bytes: info["KiriMemoryUsed"].as_u64(),
            memory_ceiling_bytes: info["KiriMemoryCeiling"].as_u64(),
        }
    }

    fn stopped_without_info() -> Self {
        Self {
            state: EngineState::Stopped,
            daemon_running: true,
            vm_awake: false,
            stopped: true,
            version: None,
            containers_running: None,
            containers_total: None,
            cpus: None,
            memory_used_bytes: None,
            memory_ceiling_bytes: None,
        }
    }

    fn not_running() -> Self {
        Self {
            state: EngineState::NotRunning,
            daemon_running: false,
            vm_awake: false,
            stopped: false,
            version: None,
            containers_running: None,
            containers_total: None,
            cpus: None,
            memory_used_bytes: None,
            memory_ceiling_bytes: None,
        }
    }

    fn to_json(&self) -> Value {
        let mut obj = json!({
            "state": self.state.json_name(),
            "daemon_running": self.daemon_running,
            "vm_awake": self.vm_awake,
            "stopped": self.stopped,
        });
        if let Some(map) = obj.as_object_mut() {
            if let Some(v) = &self.version {
                map.insert("version".into(), json!(v));
            }
            if let Some(n) = self.containers_running {
                map.insert("containers_running".into(), json!(n));
            }
            if let Some(n) = self.containers_total {
                map.insert("containers_total".into(), json!(n));
            }
            if let Some(n) = self.cpus {
                map.insert("cpus".into(), json!(n));
            }
            if let Some(n) = self.memory_used_bytes {
                map.insert("memory_used_bytes".into(), json!(n));
            }
            if let Some(n) = self.memory_ceiling_bytes {
                map.insert("memory_ceiling_bytes".into(), json!(n));
            }
        }
        obj
    }

    fn format_human(&self) -> String {
        let mut out = String::new();
        let state_line = match self.state {
            EngineState::Running => "running",
            EngineState::Asleep => "asleep (wakes on next command)",
            EngineState::Stopped => "stopped (run `kiri engine start` to start)",
            EngineState::NotRunning => "not running (run `kiri engine start` or `kiri daemon`)",
        };
        out.push_str(&format!("State            {state_line}\n"));
        if let Some(v) = &self.version {
            out.push_str(&format!("Version          {v}\n"));
        }
        if let (Some(running), Some(total)) = (self.containers_running, self.containers_total) {
            out.push_str(&format!(
                "Containers       {running} running ({total} total)\n"
            ));
        }
        if self.state == EngineState::Running {
            if let Some(cpus) = self.cpus.filter(|&c| c > 0) {
                out.push_str(&format!("CPUs             {cpus}\n"));
            }
            if let Some(ceiling) = self.memory_ceiling_bytes.filter(|&b| b > 0) {
                let used = self.memory_used_bytes.unwrap_or(0);
                out.push_str(&format!(
                    "Memory           {} / {}\n",
                    format_bytes(used),
                    format_bytes(ceiling)
                ));
            }
        }
        out
    }
}

fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    const KIB: f64 = (1u64 << 10) as f64;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Whether an HTTP 503 error response came from Kiri's stopped gate (e.g.
/// `kiri-win.exe` refusing requests while stopped without waking WSL).
fn is_stopped_refusal(r: &client::Response) -> bool {
    r.status == 503 && r.error().contains("Kiri is stopped")
}

/// Inspects the engine state using a background `GET /info` request so an
/// asleep VM is never woken and the idle timer is never reset.
fn inspect_with(fetch: impl FnOnce() -> io::Result<client::Response>) -> io::Result<StatusReport> {
    match fetch() {
        Ok(r) if r.ok() => Ok(StatusReport::from_info(&r.json())),
        Ok(r) if is_stopped_refusal(&r) => Ok(StatusReport::stopped_without_info()),
        Ok(r) => Err(io::Error::other(r.error())),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::BrokenPipe
            ) =>
        {
            Ok(StatusReport::not_running())
        }
        Err(e) => Err(e),
    }
}

/// Starts the Kiri engine (bringing up the daemon on macOS if needed, then
/// calling `POST /kiri/wake`). Shared by `kiri start` and `kiri engine start`.
#[cfg(unix)]
pub(crate) fn wake_engine() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if !client::alive() {
        if let Ok(d) = crate::daemon::state_dir() {
            let _ = std::fs::remove_file(d.join("stopped"));
        }
        crate::update::start_engine()?;
    }
    client::check_alive_with(not_running_message())?;
    let r = client::request("POST", "/kiri/wake", None)?;
    // 404: a daemon older than `/kiri/wake`.
    if !r.ok() && r.status != 404 {
        return Err(io::Error::other(r.error()));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn wake_engine() -> io::Result<()> {
    let r = client::request("POST", "/kiri/wake", None);
    match r {
        Ok(resp) if resp.ok() || resp.status == 404 => Ok(()),
        Ok(resp) => Err(io::Error::other(resp.error())),
        Err(_) => crate::wsl::run(&["start".to_owned()]),
    }
}

#[cfg(unix)]
fn not_running_message() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Kiri's engine isn't running. Open Kiri, or start it with `kiri daemon`"
    }
    #[cfg(not(target_os = "macos"))]
    {
        client::not_running_message()
    }
}

fn stop_with(mut req: impl FnMut(&str, &str) -> io::Result<client::Response>) -> io::Result<bool> {
    let r = req("POST", "/kiri/stop")?;
    if r.ok() {
        return Ok(true);
    }
    if is_stopped_refusal(&r) {
        return Ok(false);
    }
    if r.status == 404 {
        // Older daemon without `/kiri/stop`: fall back to `/kiri/sleep`, matching the GUI.
        let s = req("POST", "/kiri/sleep")?;
        if s.ok() {
            return Ok(true);
        }
        return Err(io::Error::other(s.error()));
    }
    Err(io::Error::other(r.error()))
}

fn stop_engine() -> io::Result<()> {
    client::check_alive()?;
    let stopped = stop_with(|m, p| client::request(m, p, None))?;
    if stopped {
        println!("Kiri engine stopped");
    } else {
        println!("Kiri engine is already stopped");
    }
    Ok(())
}

fn restart_with(
    mut req: impl FnMut(&str, &str) -> io::Result<client::Response>,
    wake: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    // Sleep (not stop) so an interruption never leaves a persistent stopped
    // marker, while still stopping running containers (holding restart-policy
    // containers for wake to bring back) and powering off the VM.
    match req("POST", "/kiri/sleep") {
        Ok(r) if r.ok() || r.status == 404 || is_stopped_refusal(&r) => {}
        Ok(r) => return Err(io::Error::other(r.error())),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(e),
    }
    wake()
}

fn restart_engine() -> io::Result<()> {
    restart_with(|m, p| client::request(m, p, None), wake_engine)?;
    println!("Kiri engine restarted");
    Ok(())
}

/// Runs `kiri engine ...` and returns the process exit code.
pub fn run(args: &[String]) -> io::Result<u8> {
    let cmd = parse(args).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    match cmd {
        Cmd::Help => {
            println!("{USAGE}");
            Ok(0)
        }
        Cmd::Start => {
            wake_engine()?;
            println!("Kiri engine started");
            Ok(0)
        }
        Cmd::Stop => {
            stop_engine()?;
            Ok(0)
        }
        Cmd::Restart => {
            restart_engine()?;
            Ok(0)
        }
        Cmd::Status { json } => {
            let report = inspect_with(|| client::request_background("GET", "/info"))?;
            if json {
                println!("{}", report.to_json());
            } else {
                print!("{}", report.format_human());
            }
            Ok(report.state.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn parse_handles_all_subcommands_flags_and_help() {
        assert_eq!(parse(&s(&[])), Ok(Cmd::Status { json: false }));
        assert_eq!(parse(&s(&["status"])), Ok(Cmd::Status { json: false }));
        assert_eq!(parse(&s(&["--json"])), Ok(Cmd::Status { json: true }));
        assert_eq!(
            parse(&s(&["status", "--json"])),
            Ok(Cmd::Status { json: true })
        );
        assert_eq!(parse(&s(&["start"])), Ok(Cmd::Start));
        assert_eq!(parse(&s(&["stop"])), Ok(Cmd::Stop));
        assert_eq!(parse(&s(&["restart"])), Ok(Cmd::Restart));
        for h in [
            vec!["-h"],
            vec!["--help"],
            vec!["help"],
            vec!["start", "--help"],
            vec!["stop", "-h"],
            vec!["restart", "--help"],
            vec!["status", "--help"],
            vec!["status", "--json", "-h"],
        ] {
            assert_eq!(parse(&s(&h)), Ok(Cmd::Help), "failed for {h:?}");
        }
        for bad in [
            vec!["bogus"],
            vec!["start", "extra"],
            vec!["stop", "--json"],
            vec!["restart", "--force"],
            vec!["status", "--bogus"],
            vec!["bogus", "--help"],
        ] {
            assert!(parse(&s(&bad)).is_err(), "expected error for {bad:?}");
        }
    }

    #[test]
    fn status_report_classifies_running_asleep_stopped_and_not_running() {
        let running = StatusReport::from_info(&json!({
            "KiriVmAwake": true,
            "KiriStopped": false,
            "KiriVersion": "0.2.2",
            "Containers": 5,
            "ContainersRunning": 2,
            "NCPU": 4,
            "KiriMemoryUsed": 512u64 << 20,
            "KiriMemoryCeiling": 4u64 << 30,
        }));
        assert_eq!(running.state, EngineState::Running);
        assert_eq!(running.state.exit_code(), 0);
        let j = running.to_json();
        assert_eq!(j["state"], "running");
        assert_eq!(j["daemon_running"], true);
        assert_eq!(j["vm_awake"], true);
        assert_eq!(j["stopped"], false);
        assert_eq!(j["version"], "0.2.2");
        assert_eq!(j["containers_running"], 2);
        assert_eq!(j["containers_total"], 5);
        assert_eq!(j["cpus"], 4);
        assert_eq!(j["memory_used_bytes"], 512u64 << 20);
        assert_eq!(j["memory_ceiling_bytes"], 4u64 << 30);
        let text = running.format_human();
        assert!(text.contains("State            running\n"));
        assert!(text.contains("Version          0.2.2\n"));
        assert!(text.contains("Containers       2 running (5 total)\n"));
        assert!(text.contains("CPUs             4\n"));
        assert!(text.contains("Memory           512.0 MiB / 4.0 GiB\n"));

        let asleep = StatusReport::from_info(&json!({
            "KiriVmAwake": false,
            "KiriStopped": false,
            "KiriVersion": "0.2.2",
            "Containers": 3,
            "ContainersRunning": 0,
        }));
        assert_eq!(asleep.state, EngineState::Asleep);
        assert_eq!(asleep.state.exit_code(), 0);
        assert_eq!(asleep.to_json()["state"], "asleep");
        assert!(
            asleep
                .format_human()
                .contains("asleep (wakes on next command)")
        );

        let stopped = StatusReport::from_info(&json!({
            "KiriVmAwake": false,
            "KiriStopped": true,
            "KiriVersion": "0.2.2",
            "Containers": 3,
            "ContainersRunning": 0,
        }));
        assert_eq!(stopped.state, EngineState::Stopped);
        assert_eq!(stopped.state.exit_code(), 1);
        assert_eq!(stopped.to_json()["state"], "stopped");
        assert_eq!(stopped.to_json()["stopped"], true);
        assert!(
            stopped
                .format_human()
                .contains("stopped (run `kiri engine start` to start)")
        );

        let down = StatusReport::not_running();
        assert_eq!(down.state, EngineState::NotRunning);
        assert_eq!(down.state.exit_code(), 1);
        assert_eq!(down.to_json()["state"], "not_running");
        assert_eq!(down.to_json()["daemon_running"], false);
        assert!(down.format_human().contains("not running"));
    }

    #[test]
    fn inspect_handles_stopped_gate_and_unreachable_daemon() {
        let refused = client::Response {
            status: 503,
            body: br#"{"message":"Kiri is stopped: run `kiri start`, or press Start in the Kiri window, to run containers again"}"#.to_vec(),
        };
        let report = inspect_with(|| Ok(refused)).unwrap();
        assert_eq!(report.state, EngineState::Stopped);
        assert!(report.daemon_running);
        assert!(report.stopped);
        assert_eq!(report.state.exit_code(), 1);

        let down =
            inspect_with(|| Err(io::Error::new(io::ErrorKind::ConnectionRefused, "refused")))
                .unwrap();
        assert_eq!(down.state, EngineState::NotRunning);
        assert!(!down.daemon_running);
        assert_eq!(down.state.exit_code(), 1);
    }

    #[test]
    fn stop_falls_back_to_sleep_on_404_and_is_idempotent_when_stopped() {
        let mut calls = Vec::new();
        let stopped = stop_with(|m, p| {
            calls.push(format!("{m} {p}"));
            let status = if p == "/kiri/stop" { 404 } else { 204 };
            Ok(client::Response {
                status,
                body: Vec::new(),
            })
        })
        .unwrap();
        assert!(stopped);
        assert_eq!(calls, vec!["POST /kiri/stop", "POST /kiri/sleep"]);

        let already = stop_with(|_, _| {
            Ok(client::Response {
                status: 503,
                body: br#"{"message":"Kiri is stopped: run `kiri start`"}"#.to_vec(),
            })
        })
        .unwrap();
        assert!(!already);
    }

    #[test]
    fn restart_sleeps_then_wakes() {
        let calls = std::cell::RefCell::new(Vec::new());
        restart_with(
            |m, p| {
                calls.borrow_mut().push(format!("{m} {p}"));
                Ok(client::Response {
                    status: 204,
                    body: Vec::new(),
                })
            },
            || {
                calls.borrow_mut().push("wake".into());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), vec!["POST /kiri/sleep", "wake"]);
    }

    #[cfg(unix)]
    #[test]
    fn cli_end_to_end_over_unix_socket() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::{Arc, Mutex};

        let Some(bin) = option_env!("CARGO_BIN_EXE_kiri")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                let test_exe = std::env::current_exe().ok()?;
                let dir = test_exe.parent()?.parent()?;
                Some(dir.join("kiri")).filter(|p| p.is_file())
            })
        else {
            return;
        };

        let dir = std::env::temp_dir().join(format!("kiri-engine-cli-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("docker.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        #[derive(Default)]
        struct MockState {
            awake: bool,
            stopped: bool,
            requests: Vec<(String, bool)>,
        }

        let state = Arc::new(Mutex::new(MockState {
            awake: true,
            stopped: false,
            requests: Vec::new(),
        }));
        let srv_state = Arc::clone(&state);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let raw = String::from_utf8_lossy(&buf[..n]);
                let first_line = raw.lines().next().unwrap_or("");
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");
                let bg = raw
                    .lines()
                    .any(|l| l.eq_ignore_ascii_case("x-kiri-background: 1"));

                let mut st = srv_state.lock().unwrap();
                if path != "/_ping" {
                    st.requests.push((format!("{method} {path}"), bg));
                }
                let (status_line, body) = match (method, path) {
                    ("GET", "/_ping") => ("200 OK", "OK".to_owned()),
                    ("GET", "/info") => {
                        let v = json!({
                            "KiriVmAwake": st.awake,
                            "KiriStopped": st.stopped,
                            "KiriVersion": "0.2.2",
                            "Containers": 2,
                            "ContainersRunning": if st.awake { 1 } else { 0 },
                            "NCPU": 4,
                            "KiriMemoryUsed": 256u64 << 20,
                            "KiriMemoryCeiling": 2u64 << 30,
                        });
                        ("200 OK", v.to_string())
                    }
                    ("POST", "/kiri/stop") => {
                        st.awake = false;
                        st.stopped = true;
                        ("204 No Content", String::new())
                    }
                    ("POST", "/kiri/sleep") => {
                        st.awake = false;
                        ("204 No Content", String::new())
                    }
                    ("POST", "/kiri/wake") => {
                        st.stopped = false;
                        st.awake = true;
                        ("204 No Content", String::new())
                    }
                    ("POST", "/__quit") => {
                        let _ = write!(
                            stream,
                            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        break;
                    }
                    _ => ("404 Not Found", json!({"message": "not found"}).to_string()),
                };
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let run_kiri = |args: &[&str], socket: &std::path::Path| {
            std::process::Command::new(&bin)
                .args(args)
                .env("KIRI_SOCKET", socket)
                .output()
                .expect("spawn kiri")
        };

        // 1. status --json while running (must use X-Kiri-Background: 1 and exit 0)
        let out = run_kiri(&["engine", "status", "--json"], &sock);
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["state"], "running");
        assert_eq!(v["vm_awake"], true);
        assert_eq!(v["stopped"], false);

        // 2. stop
        let out = run_kiri(&["engine", "stop"], &sock);
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "Kiri engine stopped"
        );

        // 3. status while stopped -> exit 1
        let out = run_kiri(&["engine", "status", "--json"], &sock);
        assert_eq!(out.status.code(), Some(1));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["state"], "stopped");
        assert_eq!(v["stopped"], true);

        // 4. start -> exit 0, running again
        let out = run_kiri(&["engine", "start"], &sock);
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "Kiri engine started"
        );

        // 5. restart -> sleep then wake, exit 0
        let out = run_kiri(&["engine", "restart"], &sock);
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "Kiri engine restarted"
        );

        // 6. status when daemon is not running -> exit 1, not_running
        let missing = dir.join("missing.sock");
        let out = run_kiri(&["engine", "status", "--json"], &missing);
        assert_eq!(out.status.code(), Some(1));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["state"], "not_running");
        assert_eq!(v["daemon_running"], false);

        // Shut down mock server and verify recorded requests.
        if let Ok(mut s) = std::os::unix::net::UnixStream::connect(&sock) {
            let _ =
                s.write_all(b"POST /__quit HTTP/1.1\r\nHost: kiri\r\nContent-Length: 0\r\n\r\n");
        }
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);

        let reqs = &state.lock().unwrap().requests;
        assert_eq!(
            reqs.as_slice(),
            &[
                ("GET /info".to_owned(), true),
                ("POST /kiri/stop".to_owned(), false),
                ("GET /info".to_owned(), true),
                ("POST /kiri/wake".to_owned(), false),
                ("POST /kiri/sleep".to_owned(), false),
                ("POST /kiri/wake".to_owned(), false),
            ]
        );
    }
}
