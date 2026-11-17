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

/// One raw HTTP request to `127.0.0.1:port`, returning `(status, full
/// response text)` — or `None` when nothing answered cleanly (refused,
/// timed out, or reset mid-read, which happens when a sibling test is
/// tearing its server down). Never panics.
fn probe_request(port: u16, raw_request: &str) -> Option<(u16, String)> {
    use std::io::Write;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(750)))
        .ok()?;
    stream
        .write_all(
            format!("{raw_request}\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())?;
    Some((status, response))
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
    /// Spawn mimic on a freshly allocated port — retrying when the port was
    /// raced away (see [`free_port`]) — and wait until the child serves
    /// `expect_path` with `want`, the mock this test wrote. If the child
    /// died before it could listen (bind conflict) or the port was captured
    /// by a sibling (whose mock set can't answer `expect_path`), start over
    /// on a different port.
    fn start(mut cmd: Command, expect_path: &str, want: u16) -> Self {
        for attempt in 0..5 {
            let port = free_port();
            let child = cmd
                .env("PORT", port.to_string())
                .env("MIMIC_BIND_ADDRESS", "127.0.0.1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn mimic");
            let mut server = Server { child, port };
            match server.wait_until_listening(expect_path, want) {
                Ok(()) => return server,
                Err(reason) => {
                    let _ = server.child.kill();
                    let _ = server.child.wait();
                    if attempt == 4 {
                        panic!("mimic never served {expect_path} after 5 attempts: {reason}");
                    }
                }
            }
        }
        unreachable!("loop returns or panics");
    }

    /// Wait until the child actually serves something on its port.
    ///
    /// The port comes from [`free_port`], which hands it back to the OS —
    /// under a parallel test run two spawns can race onto the same port,
    /// and a probe would then be answered by a *sibling's* server. So the
    /// probe is only trusted when the answer comes from a mock **this test
    /// wrote**; `expect_path` is that test's own route, which no sibling's
    /// mock set contains.
    fn wait_until_listening(&mut self, expect_path: &str, want: u16) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last = format!("{expect_path} not answered yet");
        while Instant::now() < deadline {
            // The child failing to bind (`fatal(...)` exits 1) is visible
            // only until a probe succeeds, so check it before probing.
            if let Some(status) = self.child.try_wait().unwrap() {
                return Err(format!("exited before listening ({status})"));
            }
            if let Some((status, _)) =
                probe_request(self.port, &format!("GET {expect_path} HTTP/1.1"))
            {
                if status == want {
                    return Ok(());
                }
                last = format!("{expect_path} answered {status}, want {want}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(last)
    }

    /// `GET path`, returning the status code.
    ///
    /// Retries across the read timeout window: a server being torn down by
    /// a sibling test can reset the connection between the successful probe
    /// above and this request, and treating that as an assertion failure
    /// made the suite flaky (a pre-existing race `wait_until_listening`'s
    /// connect-only check used to let through).
    fn status(&self, path: &str) -> u16 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((status, _)) = probe_request(self.port, &format!("GET {path} HTTP/1.1")) {
                return status;
            }
            if Instant::now() >= deadline {
                panic!("no HTTP status for {path} on {} within 10s", self.port);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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
    std::fs::write(
        dir.path().join("up.json"),
        r#"{"method":"GET","path":"/sigterm-up","status":200,"response":"up"}"#,
    )
    .unwrap();
    let mut cmd = mimic();
    cmd.env("MIMIC_MOCKS_DIR", dir.path())
        .env("MIMIC_HEALTH_PATH", "/ping");
    let mut server = Server::start(cmd, "/sigterm-up", 200);
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
    std::fs::write(
        dir.path().join("up.json"),
        r#"{"method":"GET","path":"/banner-up","status":200,"response":"up"}"#,
    )
    .unwrap();
    let mut cmd = mimic();
    cmd.env("MIMIC_MOCKS_DIR", dir.path())
        .env("MIMIC_HEALTH_PATH", "");
    let mut server = Server::start(cmd, "/banner-up", 200);
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
    let server = Server::start(cmd, "/stay", 200);
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

/// Verification API (#149) against the real binary: write a mock, force a
/// reload instead of sleeping out the poll, hit it, count it, reset it.
#[test]
fn the_verification_api_round_trip_works_over_http() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("slow.json"),
        r#"{"method":"GET","path":"/verified","status":200,"response":{"ok":true}}"#,
    )
    .unwrap();

    let mut cmd = mimic();
    cmd.env("MIMIC_MOCKS_DIR", dir.path());
    let server = Server::start(cmd, "/verified", 200);

    // Full-response HTTP helper. Retries across resets so a torn-down
    // sibling's server can't fail an assertion spuriously; the deadline
    // turns genuine absence into a failure with the last response shown.
    let request = |port: u16, raw: &str| -> (u16, String) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = (0, String::from("<no response>"));
        loop {
            if let Some(pair) = probe_request(port, raw) {
                if pair.0 != 0 {
                    return pair;
                }
                last = pair;
            }
            if Instant::now() >= deadline {
                return last;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    // 1. Force a reload after writing a NEW mock file no reload cycle has
    //    seen — the endpoint must install it without waiting the 2s tick.
    std::fs::write(
        dir.path().join("late.json"),
        r#"{"method":"GET","path":"/late","status":201,"response":{"late":true}}"#,
    )
    .unwrap();
    let (status, _) = request(server.port, "POST /admin/reload HTTP/1.1");
    assert_eq!(status, 200);
    assert_eq!(
        server.status("/late"),
        201,
        "forced reload must install the new mock immediately"
    );

    // 2. Hit the mock, then count. The startup gate in Server::start probes
    //    /verified too (that's how it knows *this* server won the port), so
    //    the log already holds one record here — /admin/reset is the
    //    documented clean-slate call and clears it before the assertion.
    server.status("/verified");
    let (reset_status, _) = request(server.port, "POST /admin/reset HTTP/1.1");
    assert_eq!(reset_status, 200);
    let (status, _) = request(
        server.port,
        "GET /admin/requests/count?path=/verified&method=GET&expect=1 HTTP/1.1",
    );
    assert_eq!(
        status, 417,
        "no record survives the reset, so expect=1 fails: expected 417 before re-hitting"
    );
    server.status("/verified");
    let (status, _) = request(
        server.port,
        "GET /admin/requests/count?path=/verified&method=GET&expect=1 HTTP/1.1",
    );
    assert_eq!(status, 200, "count?expect=1 after exactly one hit must 200");

    let (status, body) = request(
        server.port,
        "GET /admin/requests/count?path=/verified&method=GET&expect=5 HTTP/1.1",
    );
    assert_eq!(status, 417, "a wrong expectation fails with 417: {body}");
    assert!(body.contains("\"count\":1"));

    // 3. duration_ms is on every recorded request.
    let (_, requests_json) = request(server.port, "GET /admin/requests HTTP/1.1");
    assert!(
        requests_json.contains("\"duration_ms\":"),
        "request log must carry duration_ms: {requests_json}"
    );

    // 4. HAR export parses as the browser would (status + version).
    let (status, har) = request(server.port, "GET /admin/requests.har HTTP/1.1");
    assert_eq!(status, 200);
    assert!(
        har.contains("application/har+json"),
        "HAR content type: {har}"
    );
    assert!(har.contains("\"version\":\"1.2\""), "HAR body: {har}");

    // 5. Reset wipes the log again; the next unfiltered count is zero —
    //    proving step 2's reset left nothing behind either.
    let (status, _) = request(server.port, "POST /admin/reset HTTP/1.1");
    assert_eq!(status, 200);
    let (status, body) = request(server.port, "GET /admin/requests/count HTTP/1.1");
    assert_eq!(status, 200);
    assert!(body.contains("\"count\":0"), "post-reset count: {body}");
}
