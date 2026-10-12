//! CLI tests: the binary's argument handling, run as a real process.

use serde_json::json;
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn aiproxy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aiproxy"))
}

/// Write a config file at `path`, keeping the router's persisted state in
/// `state_dir` (created here) so tests never touch the real state directory.
fn write_config(
    path: &std::path::Path,
    state_dir: &std::path::Path,
    mut contents: serde_json::Value,
) {
    std::fs::create_dir_all(state_dir).expect("state dir");
    contents["stateDir"] = json!(state_dir.display().to_string());
    if contents.get("backends").is_none() {
        contents["backends"] = json!([]);
    }
    std::fs::write(path, contents.to_string()).expect("write config");
}

/// A config file that keeps the router's persisted state inside a temp dir.
fn config(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("config.json");
    write_config(&path, &dir.path().join("state"), json!({}));
    path
}

/// Write the user-level config (`~/.config/aiproxy/aiproxy.json`) under `home`.
fn user_config(home: &std::path::Path, contents: serde_json::Value) -> std::path::PathBuf {
    let path = home.join(".config/aiproxy/aiproxy.json");
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("config dir");
    write_config(&path, &home.join("state"), contents);
    path
}

/// A TCP port that is free at the time it is picked.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Spawn the router and collect the lines it prints on stdout.
fn spawn_router(command: &mut Command) -> (Child, mpsc::Receiver<String>) {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().expect("spawn aiproxy");

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    (child, rx)
}

/// Run the router with `args` and collect the lines it prints on stdout.
fn run_router(config: &std::path::Path, args: &[&str]) -> (Child, mpsc::Receiver<String>) {
    let mut command = aiproxy();
    command.arg("--config").arg(config).args(args);
    spawn_router(&mut command)
}

/// Wait for a line containing `expected`, killing the router either way.
fn wait_for_line(child: &mut Child, rx: &mpsc::Receiver<String>, expected: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if line.contains(expected) => {
                seen = true;
                break;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    seen
}

#[test]
fn help_lists_the_bind_option() {
    let output = aiproxy().arg("--help").output().expect("run --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("-b, --bind <BIND>"), "{stdout}");
}

#[test]
fn a_port_that_disagrees_with_the_bind_fails() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = config(&dir);
    let output = aiproxy()
        .arg("--config")
        .arg(&config)
        .args(["--bind", "*:9090", "--port", "8080"])
        .output()
        .expect("run aiproxy");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Both ports are named so the conflict is obvious, and the router never
    // starts.
    assert!(stderr.contains("9090"), "{stderr}");
    assert!(stderr.contains("8080"), "{stderr}");
    assert!(!stderr.contains("listening on"), "{stderr}");
}

#[test]
fn a_port_that_agrees_with_the_bind_is_accepted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = config(&dir);
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    let (mut child, rx) = run_router(&config, &["--bind", &bind, "--port", &port.to_string()]);
    assert!(
        wait_for_line(
            &mut child,
            &rx,
            &format!("listening on http://127.0.0.1:{port}")
        ),
        "the router did not report listening on 127.0.0.1:{port}"
    );
}

#[test]
fn a_bind_address_selects_the_listen_address() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = config(&dir);
    let port = free_port();
    let (mut child, rx) = run_router(&config, &["--bind", &format!("127.0.0.1:{port}")]);
    assert!(
        wait_for_line(
            &mut child,
            &rx,
            &format!("listening on http://127.0.0.1:{port}")
        ),
        "the router did not report listening on 127.0.0.1:{port}"
    );
}

#[test]
fn a_wildcard_bind_listens_on_every_interface() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = config(&dir);
    let port = free_port();
    let (mut child, rx) = run_router(&config, &["--bind", &format!("*:{port}")]);
    assert!(
        wait_for_line(
            &mut child,
            &rx,
            &format!("listening on http://0.0.0.0:{port}")
        ),
        "the router did not report listening on 0.0.0.0:{port}"
    );
}

#[test]
fn a_bind_without_a_port_uses_the_configured_port() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config = config(&dir);
    let port = free_port();
    let (mut child, rx) = run_router(&config, &["-p", &port.to_string(), "-b", "localhost"]);
    assert!(
        wait_for_line(
            &mut child,
            &rx,
            &format!("listening on http://localhost:{port}")
        ),
        "the router did not report listening on localhost:{port}"
    );
}

#[test]
fn the_user_config_is_found_without_a_config_option() {
    let dir = tempfile::tempdir().expect("temp dir");
    let port = free_port();
    let path = user_config(dir.path(), json!({ "port": port }));
    let mut command = aiproxy();
    command.env("HOME", dir.path());
    let (mut child, rx) = spawn_router(&mut command);
    assert!(
        wait_for_line(
            &mut child,
            &rx,
            &format!(
                "config: {} — listening on http://127.0.0.1:{port}",
                path.display()
            )
        ),
        "the router did not load {} and listen on port {port}",
        path.display()
    );
}

#[test]
fn an_explicit_config_wins_over_the_user_config() {
    let dir = tempfile::tempdir().expect("temp dir");
    user_config(dir.path(), json!({ "port": free_port() }));
    let port = free_port();
    let explicit = dir.path().join("explicit.json");
    write_config(
        &explicit,
        &dir.path().join("explicit-state"),
        json!({ "port": port }),
    );
    let mut command = aiproxy();
    command
        .env("HOME", dir.path())
        .arg("--config")
        .arg(&explicit);
    let (mut child, rx) = spawn_router(&mut command);
    assert!(
        wait_for_line(
            &mut child,
            &rx,
            &format!("listening on http://127.0.0.1:{port}")
        ),
        "the router did not use the explicit config on port {port}"
    );
}

#[test]
fn a_config_bind_selects_the_listen_address() {
    let dir = tempfile::tempdir().expect("temp dir");
    let port = free_port();
    let path = dir.path().join("config.json");
    write_config(
        &path,
        &dir.path().join("state"),
        json!({ "bind": format!("127.0.0.1:{port}") }),
    );
    let (mut child, rx) = run_router(&path, &[]);
    assert!(
        wait_for_line(
            &mut child,
            &rx,
            &format!("listening on http://127.0.0.1:{port}")
        ),
        "the router did not listen on the config bind port {port}"
    );
}

#[test]
fn the_command_line_bind_overrides_the_config_bind() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config_port = free_port();
    let cli_port = free_port();
    let path = dir.path().join("config.json");
    write_config(
        &path,
        &dir.path().join("state"),
        json!({ "bind": format!("127.0.0.1:{config_port}") }),
    );
    let (mut child, rx) = run_router(&path, &["--bind", &format!("localhost:{cli_port}")]);
    assert!(
        wait_for_line(
            &mut child,
            &rx,
            &format!("listening on http://localhost:{cli_port}")
        ),
        "the command-line bind did not override the config bind"
    );
}

#[test]
fn the_command_line_port_overrides_a_config_bind_port() {
    let dir = tempfile::tempdir().expect("temp dir");
    let config_port = free_port();
    let cli_port = free_port();
    let path = dir.path().join("config.json");
    write_config(
        &path,
        &dir.path().join("state"),
        json!({ "bind": format!("127.0.0.1:{config_port}") }),
    );
    let (mut child, rx) = run_router(&path, &["--port", &cli_port.to_string()]);
    assert!(
        wait_for_line(
            &mut child,
            &rx,
            &format!("listening on http://127.0.0.1:{cli_port}")
        ),
        "--port did not override the config bind port"
    );
}

#[test]
fn an_invalid_config_bind_refuses_to_start() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.json");
    write_config(
        &path,
        &dir.path().join("state"),
        json!({ "bind": "localhost:abc" }),
    );
    let output = aiproxy()
        .arg("--config")
        .arg(&path)
        .output()
        .expect("run aiproxy");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("localhost:abc"), "{stderr}");
    assert!(stderr.contains("config"), "{stderr}");
    assert!(!stderr.contains("listening on"), "{stderr}");
}
