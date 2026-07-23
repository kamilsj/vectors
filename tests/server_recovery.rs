use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn acknowledged_http_write_survives_forced_server_termination() {
    let directory = temporary_directory();
    let port = available_port();
    let mut server = Server::start(&directory, port);
    server.wait_until_ready();

    let response = request(
        port,
        "POST",
        "/v1/sql",
        r#"{"sql":"CREATE TABLE entries (id INTEGER PRIMARY KEY, value TEXT); INSERT INTO entries VALUES (1, 'persisted');"}"#,
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    server.kill();

    let mut recovered = Server::start(&directory, port);
    recovered.wait_until_ready();
    let response = request(
        port,
        "POST",
        "/v1/sql",
        r#"{"sql":"SELECT value FROM entries WHERE id = 1"}"#,
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("persisted"), "{response}");
    recovered.kill();
    fs::remove_dir_all(directory).unwrap();
}

struct Server {
    child: Option<Child>,
    port: u16,
}

impl Server {
    fn start(directory: &Path, port: u16) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_vectors-server"))
            .arg("--data-dir")
            .arg(directory)
            .arg("--port")
            .arg(port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("server should start");
        Self {
            child: Some(child),
            port,
        }
    }

    fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("server exited before becoming ready: {status}");
            }
            if request(self.port, "GET", "/healthz", "").starts_with("HTTP/1.1 200") {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("server did not become ready on port {}", self.port);
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().expect("server should be terminated");
            child.wait().expect("terminated server should be reaped");
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

fn request(port: u16, method: &str, path: &str, body: &str) -> String {
    let mut stream = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(stream) => stream,
        Err(_) => return String::new(),
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn available_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn temporary_directory() -> PathBuf {
    let sequence = DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "vectors-server-recovery-{}-{sequence}",
        std::process::id()
    ))
}
