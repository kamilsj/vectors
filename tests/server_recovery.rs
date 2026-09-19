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
fn stale_admin_mutations_remain_rejected_after_server_restarts() {
    let directory = temporary_directory();
    let port = available_port();
    let mut server = Server::start(&directory, port);
    server.wait_until_ready();
    let response = request(
        port,
        "POST",
        "/v1/sql",
        r#"{"sql":"CREATE TABLE entries (id INTEGER PRIMARY KEY, value TEXT); INSERT INTO entries VALUES (1, 'original');"}"#,
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    server.kill();

    let mut server = Server::start(&directory, port);
    server.wait_until_ready();
    let page = request(port, "GET", "/v1/admin/tables/entries/rows", "");
    let page: serde_json::Value =
        serde_json::from_str(page.split_once("\r\n\r\n").unwrap().1).unwrap();
    let old_revision = page["revision"].as_u64().unwrap();
    let response = request(
        port,
        "POST",
        "/v1/sql",
        r#"{"sql":"UPDATE entries SET value = 'another client committed this' WHERE id = 1"}"#,
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    server.kill();

    let mut server = Server::start(&directory, port);
    server.wait_until_ready();
    for (method, path, body) in [
        (
            "PATCH",
            "/v1/admin/tables/entries/rows",
            serde_json::json!({
                "expected_revision": old_revision,
                "key": {"column": "id", "value": 1},
                "values": {"value": "stale browser overwrite"}
            }),
        ),
        (
            "DELETE",
            "/v1/admin/tables/entries/rows",
            serde_json::json!({
                "expected_revision": old_revision,
                "key": {"column": "id", "value": 1}
            }),
        ),
        (
            "DELETE",
            "/v1/admin/tables/entries",
            serde_json::json!({
                "expected_revision": old_revision,
                "confirm_table": "entries"
            }),
        ),
    ] {
        let response = request(port, method, path, &body.to_string());
        assert!(
            response.starts_with("HTTP/1.1 409"),
            "{method} {path}: {response}"
        );
        assert!(response.contains("stale_revision"), "{response}");
    }
    let page = request(port, "GET", "/v1/admin/tables/entries/rows", "");
    let page: serde_json::Value =
        serde_json::from_str(page.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert!(page["revision"].as_u64().unwrap() > old_revision);
    assert_eq!(page["rows"][0][1], "another client committed this");
    server.kill();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn admin_edits_and_nonsecret_embedding_settings_survive_restart() {
    let directory = temporary_directory();
    let port = available_port();
    let mut server = Server::start(&directory, port);
    server.wait_until_ready();
    let response = request(
        port,
        "POST",
        "/v1/sql",
        r#"{"sql":"CREATE TABLE entries (id INTEGER PRIMARY KEY, value TEXT); INSERT INTO entries VALUES (1, 'original');"}"#,
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let page = request(port, "GET", "/v1/admin/tables/entries/rows", "");
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    let page: serde_json::Value =
        serde_json::from_str(page.split_once("\r\n\r\n").unwrap().1).unwrap();
    let body = serde_json::json!({"expected_revision":page["revision"],"key":{"column":"id","value":1},"values":{"value":"admin persisted"}}).to_string();
    let response = request(port, "PATCH", "/v1/admin/tables/entries/rows", &body);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let response = request(
        port,
        "PUT",
        "/v1/settings/embeddings",
        r#"{"provider":"voyage","model":"voyage-4-lite","dimensions":256,"batch_size":16,"api_key":"synthetic-provider-key"}"#,
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(!response.contains("synthetic-provider-key"));
    server.kill();

    let config = fs::read_to_string(directory.join("embedding-settings.json")).unwrap();
    assert!(!config.contains("synthetic-provider-key"));
    assert!(!config.contains("api_key"));
    let mut recovered = Server::start(&directory, port);
    recovered.wait_until_ready();
    let response = request(
        port,
        "POST",
        "/v1/sql",
        r#"{"sql":"SELECT value FROM entries WHERE id=1"}"#,
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("admin persisted"), "{response}");
    let response = request(port, "GET", "/v1/settings/embeddings", "");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let settings: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(settings["model"], "voyage-4-lite");
    assert_eq!(settings["dimensions"], 256);
    assert_eq!(settings["batch_size"], 16);
    assert_eq!(settings["configured"], false);
    recovered.kill();
    fs::remove_dir_all(directory).unwrap();
}

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
            .env_remove("OPENAI_API_KEY")
            .env_remove("VOYAGE_API_KEY")
            .env_remove("VECTORS_API_TOKEN")
            .env("VECTORS_HTTP_WORKERS", "2")
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
