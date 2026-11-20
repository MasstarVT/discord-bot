//! dashboard.js as a process: the parked start without configuration (log lines, /healthz 200, every other path
//! 503), a configured start that listens and logs the public URL, and the DASHBOARD_URL warning.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Kills the child when the test ends, also on a failed assertion.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Starts the binary in an empty directory (no `.env`) with only `vars` set, and collects its output lines.
fn start(vars: &[(&str, String)]) -> (Running, mpsc::Receiver<String>, std::path::PathBuf) {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dashboard-process-{}-{}", std::process::id(), free_port()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dashboard"));
    cmd.current_dir(&dir).env_clear().stdout(Stdio::piped()).stderr(Stdio::piped());
    for keep in ["PATH", "SystemRoot", "SYSTEMROOT", "TEMP", "TMP", "HOME", "USERPROFILE"] {
        if let Ok(v) = std::env::var(keep) {
            cmd.env(keep, v);
        }
    }
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd.env("FORCE_COLOR", "0");
    let mut child = cmd.spawn().expect("the dashboard binary starts");
    let (tx, rx) = mpsc::channel();
    for pipe in [Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>, Box::new(child.stderr.take().unwrap())] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
    }
    (Running(child), rx, dir)
}

fn wait_for(rx: &mpsc::Receiver<String>, seen: &mut Vec<String>, needle: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if seen.iter().any(|l| l.contains(needle)) {
            return true;
        }
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(100)) {
            seen.push(l);
        }
    }
    seen.iter().any(|l| l.contains(needle))
}

fn get(port: u16, path: &str) -> (u16, String) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let r = reqwest::get(format!("http://127.0.0.1:{port}{path}")).await.expect("answer");
        (r.status().as_u16(), r.text().await.unwrap())
    })
}

#[test]
fn without_configuration_it_parks() {
    let port = free_port();
    let (child, rx, dir) = start(&[("DASHBOARD_PORT", port.to_string()), ("DASHBOARD_HOST", "127.0.0.1".into())]);
    let mut seen = Vec::new();
    assert!(wait_for(&rx, &mut seen, &format!("Dashboard parked on 127.0.0.1:{port}.")), "{seen:#?}");
    let joined = seen.join("\n");
    assert!(
        joined.contains("Dashboard not started: missing CLIENT_ID, DISCORD_TOKEN, DISCORD_CLIENT_SECRET, DASHBOARD_SESSION_SECRET, DATABASE_URL, REDIS_URL."),
        "{joined}"
    );
    assert!(joined.contains("Set these keys (see the \"Web dashboard\" section of .env.example) and restart. Parked: /healthz answers 200, every other path 503."));
    assert_eq!(get(port, "/healthz").0, 200);
    let (s, body) = get(port, "/");
    assert_eq!(s, 503, "{body}");
    drop(child);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn configured_it_listens_and_warns_about_a_default_url() {
    let (Some(db), Some(redis)) = (std::env::var("TEST_DATABASE_URL").ok(), std::env::var("TEST_REDIS_URL").ok()) else {
        eprintln!("skipped: TEST_DATABASE_URL or TEST_REDIS_URL not set");
        return;
    };
    let port = free_port();
    let (child, rx, dir) = start(&[
        ("CLIENT_ID", "123456789012345678".into()),
        ("DISCORD_TOKEN", "bot-token".into()),
        ("DISCORD_CLIENT_SECRET", "client-secret".into()),
        ("DASHBOARD_SESSION_SECRET", "a-session-secret-that-is-long-enough-0123456789".into()),
        ("DATABASE_URL", db),
        ("REDIS_URL", redis),
        ("DASHBOARD_PORT", port.to_string()),
        ("DASHBOARD_HOST", "0.0.0.0".into()),
    ]);
    let mut seen = Vec::new();
    assert!(wait_for(&rx, &mut seen, &format!("Dashboard listening on 0.0.0.0:{port} (public URL http://localhost:8080).")), "{seen:#?}");
    // The warning is logged before the listening line, but on stderr: the two pipes are read by separate threads, so
    // it can reach `rx` after the stdout line. Wait for it rather than expecting it in `seen` already.
    assert!(
        wait_for(
            &rx,
            &mut seen,
            "Dashboard: DASHBOARD_URL is not set, so saving only works at http://localhost:8080. Browsers that open the dashboard by another address will be able to read but not save changes until DASHBOARD_URL is set to that address."
        ),
        "{seen:#?}"
    );
    assert_eq!(get(port, "/healthz").0, 200);
    // A configured server answers the API (a log-in is required), not the parked 503.
    let (s, body) = get(port, "/api/catalog");
    assert_eq!(s, 401, "{body}");
    assert_eq!(get(port, "/").0, 200);
    drop(child);
    let _ = std::fs::remove_dir_all(dir);
}
