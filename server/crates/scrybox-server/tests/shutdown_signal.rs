//! Review M1 (REVIEW-concurrency-jobs.md): SIGTERM must trigger a graceful
//! shutdown (exit 0), not the default signal death.
#![cfg(unix)]
use std::{process::Command, time::{Duration, Instant}};

#[test]
fn sigterm_exits_cleanly() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let mut child = Command::new(env!("CARGO_BIN_EXE_scrybox-server"))
        .env("SCRYBOX_BIND", format!("127.0.0.1:{port}"))
        .env_remove("HTTPS_PORT")
        .env_remove("SCRYBOX_DB_PATH")
        .env("BACKUP_INTERVAL_HOURS", "0")
        .spawn()
        .unwrap();
    // Wait for the listener.
    let t0 = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(30), "server never listened");
        if let Some(st) = child.try_wait().unwrap() {
            panic!("server exited early: {st:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = child.id().to_string();
    assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    let t1 = Instant::now();
    let st = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if t1.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            panic!("server did not exit within the drain bound");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(st.code(), Some(0), "SIGTERM must drain and exit 0, got {st:?}");
}
