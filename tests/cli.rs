//! CLI tests: the binary's argument handling, run as a real process.

use serde_json::json;
use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn aiproxy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aiproxy"))
}

/// A config file that keeps the router's persisted state inside a temp dir.
fn config(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        json!({ "stateDir": state_dir.display().to_string(), "backends": [] }).to_string(),
    )
    .expect("write config");
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

/// Run the router with `args` and collect the lines it prints on stdout.
fn run_router(config: &std::path::Path, args: &[&str]) -> (Child, mpsc::Receiver<String>) {
    let mut command = aiproxy();
    command
        .arg("--config")
        .arg(config)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
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
