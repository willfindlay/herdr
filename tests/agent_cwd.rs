//! Integration coverage for the server-side agent cwd poll.
//!
//! The poll reads the foreground process group cwd from the OS, so it only
//! works on Unix and needs a live server with a real PTY behind it.
#![cfg(unix)]

pub mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::Value;
use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid, wait_for_socket,
};

/// Matches `AGENT_CWD_POLL_INTERVAL` plus the session save debounce that
/// follows it, with room for a loaded machine.
const SAVED_CWD_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl SpawnedHerdr {
    /// Waits for the server to exit on its own after `server.stop`, so the
    /// session it writes on the way out is on disk before the test reads it.
    fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            thread::sleep(POLL_INTERVAL);
        }
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        unregister_spawned_herdr_pid(pid);
    }
}

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-agent-cwd-test-{}-{nanos}",
        std::process::id()
    ))
}

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

fn spawn_server(config_home: &Path, runtime_dir: &Path, api_socket: &Path) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env("HERDR_SOCKET_PATH", api_socket);
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env_remove("HERDR_SESSION");
    cmd.env_remove("HERDR_ENV");
    cmd.env("SHELL", "/bin/sh");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    drop(pair.slave);

    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn send_json_request(socket_path: &Path, request: &Value) -> Value {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");
    writeln!(stream, "{request}").unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(&response).expect("response should be valid JSON")
}

fn pane_get(socket_path: &Path, pane_id: &str) -> Value {
    send_json_request(
        socket_path,
        &serde_json::json!({
            "id": "pane_get",
            "method": "pane.get",
            "params": {"pane_id": pane_id},
        }),
    )
}

/// Temp directories on some machines sit under a symlink, and the server
/// reports process cwds the way the OS resolves them.
fn canonical(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn pane_string_field(pane: &Value, field: &str) -> Option<PathBuf> {
    pane["result"]["pane"][field].as_str().map(canonical)
}

/// Reads the cwd the saved session records for the workspace labelled
/// `label`. The workspace under test holds a single pane, so its tab has one
/// entry.
fn saved_pane_cwd(session_path: &Path, label: &str) -> Option<PathBuf> {
    let session: Value = serde_json::from_str(&fs::read_to_string(session_path).ok()?).ok()?;
    let workspace = session["workspaces"]
        .as_array()?
        .iter()
        .find(|workspace| workspace["custom_name"] == label)?;
    let panes = workspace["tabs"][0]["panes"].as_object()?;
    assert_eq!(
        panes.len(),
        1,
        "workspace {label} should have saved exactly one pane: {panes:?}"
    );
    panes.values().next()?["cwd"].as_str().map(canonical)
}

#[test]
fn saved_pane_cwd_follows_detected_agent_cwd() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let session_path = config_home.join(app_dir_name()).join("session.json");
    let label = "agent-cwd";

    // The pane shell stays in shell_cwd while its foreground process runs in
    // agent_cwd. Before the agent cwd poll the saved session recorded
    // shell_cwd (issue #3256).
    let shell_cwd = base.join("shell-cwd");
    let agent_cwd = base.join("agent-cwd");
    fs::create_dir_all(&shell_cwd).unwrap();
    fs::create_dir_all(&agent_cwd).unwrap();

    // A `cd` sent to the pane would move the shell itself, so the directory
    // change happens inside a script the shell runs as a foreground child.
    let script = base.join("run-in-agent-cwd.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\ncd {} && exec sleep 60\n", agent_cwd.display()),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let mut spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));

    let created = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "workspace_create",
            "method": "workspace.create",
            "params": {"cwd": shell_cwd.to_str().unwrap(), "label": label, "focus": true},
        }),
    );
    assert!(created.get("error").is_none(), "{created}");
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("workspace.create should return its root pane")
        .to_string();

    // Report an agent from a source other than the official hook, so the
    // terminal counts as an agent terminal while carrying no session ref.
    let reported = send_json_request(
        &api_socket,
        &serde_json::json!({
            "id": "pane_report_agent",
            "method": "pane.report_agent",
            "params": {
                "pane_id": pane_id,
                "source": "repro",
                "agent": "claude",
                "state": "working",
            },
        }),
    );
    assert!(reported.get("error").is_none(), "{reported}");
    assert_eq!(
        pane_get(&api_socket, &pane_id)["result"]["pane"]["agent"],
        "claude",
        "the pane should report the agent it was told about"
    );

    // Drive the pane into the diverged state, resending while the shell is
    // still starting up and has not read its PTY yet.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut next_send = Instant::now();
    let mut pane = pane_get(&api_socket, &pane_id);
    while Instant::now() < deadline {
        if pane_string_field(&pane, "foreground_cwd") == Some(canonical(&agent_cwd)) {
            break;
        }
        if Instant::now() >= next_send {
            let sent = send_json_request(
                &api_socket,
                &serde_json::json!({
                    "id": "pane_send_text",
                    "method": "pane.send_text",
                    "params": {
                        "pane_id": pane_id,
                        "text": format!("{}\n", script.display()),
                    },
                }),
            );
            assert!(sent.get("error").is_none(), "{sent}");
            next_send = Instant::now() + Duration::from_secs(2);
        }
        thread::sleep(POLL_INTERVAL);
        pane = pane_get(&api_socket, &pane_id);
    }

    assert_eq!(
        pane_string_field(&pane, "foreground_cwd"),
        Some(canonical(&agent_cwd)),
        "the pane's foreground process should be running in the agent cwd: {pane}"
    );
    assert_eq!(
        pane_string_field(&pane, "cwd"),
        Some(canonical(&shell_cwd)),
        "the pane shell should have stayed in its own cwd: {pane}"
    );

    // The poll runs on a coarse deadline and the save that follows it is
    // debounced, so wait for the recorded cwd instead of sleeping a fixed
    // span.
    let deadline = Instant::now() + SAVED_CWD_TIMEOUT;
    while Instant::now() < deadline {
        if saved_pane_cwd(&session_path, label) == Some(canonical(&agent_cwd)) {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    assert_eq!(
        saved_pane_cwd(&session_path, label),
        Some(canonical(&agent_cwd)),
        "the agent cwd poll should have recorded the agent cwd in {}",
        session_path.display()
    );

    let stopped = send_json_request(
        &api_socket,
        &serde_json::json!({"id": "server_stop", "method": "server.stop", "params": {}}),
    );
    assert!(stopped.get("error").is_none(), "{stopped}");
    assert!(
        spawned.wait_for_exit(Duration::from_secs(15)),
        "server should exit after server.stop"
    );

    assert_eq!(
        saved_pane_cwd(&session_path, label),
        Some(canonical(&agent_cwd)),
        "the session saved at shutdown should record the agent cwd, not the shell cwd"
    );

    drop(spawned);
    cleanup_test_base(&base);
}
