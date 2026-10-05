#![cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DAEMON: &str = env!("CARGO_BIN_EXE_bondsymphonic-daemon");

#[test]
fn the_bridge_carries_a_request_and_its_reply() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("mcp.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "{\"ping\":1}\n");
        let mut w = conn;
        w.write_all(b"{\"pong\":1}\n").unwrap();
        // Closing our end is what ends the bridge.
    });
    let mut child = Command::new(DAEMON)
        .args(["mcp-bridge", "--socket"])
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"ping\":1}\n")
        .unwrap(); // stdin dropped -> EOF
    let mut out = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut out)
        .unwrap();
    assert_eq!(out, "{\"pong\":1}\n");
    server.join().unwrap();
    assert!(child.wait().unwrap().success());
}

/// An agent may keep its stdin open while the daemon goes away; the bridge
/// must still exit rather than wait on a read that will never finish.
#[test]
fn the_bridge_exits_when_the_socket_closes_even_with_stdin_open() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("mcp.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        conn.write_all(b"{\"bye\":1}\n").unwrap();
    });
    let mut child = Command::new(DAEMON)
        .args(["mcp-bridge", "--socket"])
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take().unwrap(); // held open
    let mut out = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut out)
        .unwrap();
    assert_eq!(out, "{\"bye\":1}\n");
    server.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("bridge did not exit after the socket closed");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_missing_socket_fails_with_its_path() {
    let out = Command::new(DAEMON)
        .args(["mcp-bridge", "--socket", "/nonexistent/mcp.sock"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("/nonexistent/mcp.sock"));
}
