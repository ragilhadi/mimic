//! Process-level behaviour that in-process router tests can't see: the command
//! line, startup configuration errors, the startup banner, graceful shutdown,
//! and hot reload as the running binary performs it (#137).

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_mimic");

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A command that can't pick up the developer's own configuration.
fn mimic() -> Command {
    let mut cmd = Command::new(BIN);
    for (key, _) in std::env::vars() {
        if key.starts_with("MIMIC_") || key == "PORT" {
            cmd.env_remove(key);
        }
    }
    cmd.env("NO_COLOR", "1");
    cmd
}

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(mut cmd: Command, port: u16) -> Self {
        let child = cmd
            .env("PORT", port.to_string())
            .env("MIMIC_BIND_ADDRESS", "127.0.0.1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mimic");
        let server = Server { child, port };
        server.wait_until_listening();
        server
    }

    fn wait_until_listening(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("mimic never listened on {}", self.port);
    }

    /// `GET path`, returning the status code.
    fn status(&self, path: &str) -> u16 {
        use std::io::Write;
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {response:?}"))
    }

    fn wait_for_exit(&mut self, within: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// Everything the process wrote, after it has exited.
    fn output(mut self) -> String {
        let mut out = String::new();
        if let Some(mut s) = self.child.stdout.take() {
            let _ = s.read_to_string(&mut out);
        }
        if let Some(mut s) = self.child.stderr.take() {
            let _ = s.read_to_string(&mut out);
        }
        out
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn empty_mocks_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn version_flags_print_the_version_and_do_not_start_a_server() {
    for flag in ["--version", "-V"] {
        let out = mimic().arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(env!("CARGO_PKG_VERSION")),
            "{flag}: {stdout}"
        );
    }
}

#[test]
fn help_flags_print_usage_and_exit_zero() {
    for flag in ["--help", "-h"] {
        let out = mimic().arg(flag).output().unwrap();
        assert!(out.status.success(), "{flag}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("Usage: mimic"));
    }
}

#[test]
fn an_unknown_argument_exits_2_instead_of_starting_a_server() {
    let out = mimic().arg("--hlep").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unrecognized argument '--hlep'"),
        "{stderr}"
    );
    assert!(stderr.contains("Usage: mimic"), "{stderr}");
}

#[test]
fn an_unusable_port_fails_startup_cleanly() {
    let dir = empty_mocks_dir();
    let out = mimic()
        .env("MIMIC_MOCKS_DIR", dir.path())
        .env("PORT", "not-a-port")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("PORT is set to 'not-a-port'"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn an_unusable_bind_address_fails_startup_cleanly() {
    let dir = empty_mocks_dir();
    let out = mimic()
        .env("MIMIC_MOCKS_DIR", dir.path())
        .env("PORT", free_port().to_string())
        .env("MIMIC_BIND_ADDRESS", "nope")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("MIMIC_BIND_ADDRESS"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn sigterm_stops_the_server_gracefully_and_the_banner_names_the_real_health_path() {
    let dir = empty_mocks_dir();
    let mut cmd = mimic();
    cmd.env("MIMIC_MOCKS_DIR", dir.path())
        .env("MIMIC_HEALTH_PATH", "/ping");
    let mut server = Server::start(cmd, free_port());
    assert_eq!(server.status("/ping"), 200);

    let pid = server.child.id().to_string();
    let sent = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(sent.success());

    let status = server
        .wait_for_exit(Duration::from_secs(8))
        .expect("SIGTERM should stop the server well inside the grace period");
    assert!(status.success(), "a graceful stop exits 0, got {status:?}");

    let log = server.output();
    assert!(log.contains("Shutdown signal received"), "{log}");
    assert!(
        log.contains("Health check available at http://127.0.0.1:"),
        "{log}"
    );
    assert!(log.contains("/ping"), "{log}");
    assert!(
        !log.contains("/health\n") && !log.contains("/health "),
        "the banner must not name the default health path: {log}"
    );
}

#[test]
fn the_banner_omits_the_health_line_when_the_health_check_is_disabled() {
    let dir = empty_mocks_dir();
    let mut cmd = mimic();
    cmd.env("MIMIC_MOCKS_DIR", dir.path())
        .env("MIMIC_HEALTH_PATH", "");
    let mut server = Server::start(cmd, free_port());
    let _ = server.child.kill();
    server.wait_for_exit(Duration::from_secs(5));
    let log = server.output();
    assert!(!log.contains("Health check available"), "{log}");
}

/// The report: delete a mock while a sibling is broken, then delete the broken
/// file — the deleted route has to stop being served. Needs two reload cycles
/// per step, hence the waits.
#[test]
fn a_deleted_mock_stops_being_served_even_after_a_broken_sibling_goes_away() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
    write(
        "a.json",
        r#"{"method":"GET","path":"/gone","status":200,"response":"x"}"#,
    );
    write(
        "b.json",
        r#"{"method":"GET","path":"/stay","status":200,"response":"y"}"#,
    );

    let mut cmd = mimic();
    cmd.env("MIMIC_MOCKS_DIR", dir.path());
    let server = Server::start(cmd, free_port());
    assert_eq!(server.status("/gone"), 200);

    let until = |path: &str, want: u16| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if server.status(path) == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!("{path} never became {want}");
    };

    write("broken.json", "{bad");
    std::fs::remove_file(dir.path().join("a.json")).unwrap();
    until("/gone", 404);

    std::fs::remove_file(dir.path().join("broken.json")).unwrap();
    // Nothing else changed; the route must still be gone, and stay gone.
    std::thread::sleep(Duration::from_millis(4500));
    assert_eq!(server.status("/gone"), 404);
    assert_eq!(server.status("/stay"), 200);
}
