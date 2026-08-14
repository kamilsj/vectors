use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use vectors::Database;

static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn test_path(label: &str, extension: &str) -> PathBuf {
    let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "vectors-{label}-{}-{sequence}.{extension}",
        std::process::id()
    ))
}

fn wait_until_listening(child: &mut Child, address: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(address).is_ok() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("vectors-server exited before it listened: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "vectors-server did not begin listening within ten seconds"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn cooperative_shutdown_runs_the_final_snapshot_save() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    drop(listener);

    let snapshot = test_path("shutdown-snapshot", "vdb");
    let shutdown_file = test_path("shutdown-request", "request");
    let mut child = Command::new(env!("CARGO_BIN_EXE_vectors-server"))
        .env("VECTORS_BIND", &address)
        .env("VECTORS_SNAPSHOT", &snapshot)
        .env("VECTORS_SHUTDOWN_FILE", &shutdown_file)
        .env("VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS", "2")
        .env_remove("VECTORS_DATA_DIR")
        .env_remove("VECTORS_AUTOSAVE_INTERVAL_SECS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_until_listening(&mut child, &address);
    fs::write(&shutdown_file, b"").unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "vectors-server did not stop cooperatively: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "vectors-server failed during shutdown: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!shutdown_file.exists());
    Database::open(&snapshot).expect("final shutdown did not produce a loadable snapshot");

    let _ = fs::remove_file(snapshot);
}
