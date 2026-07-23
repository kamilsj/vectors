use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use vectors::{api, Database};

#[derive(Debug, PartialEq, Eq)]
enum StartupAction {
    Run(StartupOptions),
    Help,
    Version,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct StartupOptions {
    bind_override: Option<String>,
    data_dir_override: Option<PathBuf>,
}

#[actix_web::main]
async fn main() -> io::Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let options = match parse_arguments(&arguments)? {
        StartupAction::Version => {
            println!("vectors-server {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        StartupAction::Help => {
            println!(
                "vectors-server {}\n\nUsage: vectors-server [options]\n\nStarts the HTTP API and web console. Command-line options override their VECTORS_* environment equivalents.\n\nOptions:\n  -p, --port PORT       Listen on 127.0.0.1:PORT\n      --bind ADDRESS    Listen on ADDRESS, for example 0.0.0.0:9000\n      --data-dir PATH   Persist writes in PATH with a WAL and checkpoints\n  -h, --help            Show this help\n  -V, --version         Show version",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(());
        }
        StartupAction::Run(options) => options,
    };
    let bind_address = options
        .bind_override
        .or_else(|| env::var("VECTORS_BIND").ok())
        .unwrap_or_else(|| "127.0.0.1:8080".into());
    let data_dir = options
        .data_dir_override
        .or_else(|| non_empty_path("VECTORS_DATA_DIR"));
    let snapshot = non_empty_path("VECTORS_SNAPSHOT");
    if data_dir.is_some() && snapshot.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "VECTORS_DATA_DIR and VECTORS_SNAPSHOT are mutually exclusive",
        ));
    }
    let autosave_interval = autosave_interval(snapshot.as_deref())?;
    let api_token = env::var("VECTORS_API_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    let database = match (data_dir.as_deref(), snapshot.as_deref()) {
        (Some(directory), _) => Database::open_persistent(directory).map_err(database_error)?,
        (None, Some(path)) if path.exists() => Database::open(path).map_err(database_error)?,
        (None, _) => Database::new(),
    };
    if let Some(directory) = &data_dir {
        eprintln!(
            "durable storage enabled in {} (synchronized WAL and checkpoints)",
            directory.display()
        );
    }
    let autosave = match (snapshot.as_ref(), autosave_interval) {
        (Some(path), Some(interval)) => {
            eprintln!(
                "saving snapshots to {} every {} second(s)",
                path.display(),
                interval.as_secs()
            );
            Some(AutosaveWorker::start(
                database.clone(),
                path.clone(),
                interval,
            )?)
        }
        _ => None,
    };

    eprintln!("vectors HTTP API starting on http://{bind_address}");
    let result = match api_token {
        Some(token) => api::serve_authenticated(database.clone(), &bind_address, token).await,
        None => api::serve(database.clone(), &bind_address).await,
    };
    if let Some(autosave) = autosave {
        autosave.stop()?;
    }
    if let Err(error) = result {
        if error.kind() == io::ErrorKind::AddrInUse {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "cannot listen on {bind_address}: address already in use; try 'vectors-server --port {}'",
                    suggested_port(&bind_address)
                ),
            ));
        }
        return Err(error);
    }
    if data_dir.is_some() {
        database.checkpoint().map_err(database_error)?;
    } else if let Some(path) = snapshot {
        database.save(path).map_err(database_error)?;
    }
    Ok(())
}

fn non_empty_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn parse_arguments(arguments: &[String]) -> io::Result<StartupAction> {
    match arguments {
        [argument] if matches!(argument.as_str(), "--version" | "-V") => {
            return Ok(StartupAction::Version);
        }
        [argument] if matches!(argument.as_str(), "--help" | "-h") => {
            return Ok(StartupAction::Help);
        }
        _ => {}
    }

    let mut options = StartupOptions::default();
    let mut index = 0;
    while index < arguments.len() {
        let option = &arguments[index];
        let value = arguments.get(index + 1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{option} requires a value"),
            )
        })?;
        match option.as_str() {
            "--port" | "-p" => {
                if options.bind_override.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--port and --bind may only be supplied once",
                    ));
                }
                let port = value.parse::<u16>().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--port must be an integer from 1 through 65535",
                    )
                })?;
                if port == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--port must be an integer from 1 through 65535",
                    ));
                }
                options.bind_override = Some(format!("127.0.0.1:{port}"));
            }
            "--bind" => {
                if options.bind_override.is_some() || value.trim().is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--port and --bind may only be supplied once",
                    ));
                }
                options.bind_override = Some(value.clone());
            }
            "--data-dir" => {
                if options.data_dir_override.is_some() || value.trim().is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--data-dir may only be supplied once with a non-empty path",
                    ));
                }
                options.data_dir_override = Some(PathBuf::from(value));
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid arguments; run 'vectors-server --help'",
                ));
            }
        }
        index += 2;
    }
    Ok(StartupAction::Run(options))
}

fn suggested_port(bind_address: &str) -> u16 {
    bind_address
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .and_then(|port| port.checked_add(1))
        .filter(|port| *port != 0)
        .unwrap_or(8081)
}

fn database_error(error: vectors::Error) -> io::Error {
    io::Error::other(error)
}

fn autosave_interval(snapshot: Option<&Path>) -> io::Result<Option<Duration>> {
    let value = match env::var("VECTORS_AUTOSAVE_INTERVAL_SECS") {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "VECTORS_AUTOSAVE_INTERVAL_SECS must be valid UTF-8",
            ))
        }
    };
    if snapshot.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "VECTORS_AUTOSAVE_INTERVAL_SECS requires VECTORS_SNAPSHOT",
        ));
    }
    parse_autosave_interval(&value).map(Some)
}

fn parse_autosave_interval(value: &str) -> io::Result<Duration> {
    let seconds = value.parse::<u64>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "VECTORS_AUTOSAVE_INTERVAL_SECS must be a positive integer",
        )
    })?;
    if seconds == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "VECTORS_AUTOSAVE_INTERVAL_SECS must be greater than zero",
        ));
    }
    Ok(Duration::from_secs(seconds))
}

struct AutosaveWorker {
    control: Arc<(Mutex<bool>, Condvar)>,
    handle: JoinHandle<()>,
}

impl AutosaveWorker {
    fn start(database: Database, path: PathBuf, interval: Duration) -> io::Result<Self> {
        let last_saved_revision = database.revision().map_err(database_error)?;
        let control = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_control = control.clone();
        let handle = thread::Builder::new()
            .name("vectors-autosave".into())
            .spawn(move || {
                autosave_loop(
                    database,
                    path,
                    interval,
                    last_saved_revision,
                    thread_control,
                )
            })?;
        Ok(Self { control, handle })
    }

    fn stop(self) -> io::Result<()> {
        let (stopped, wake) = &*self.control;
        match stopped.lock() {
            Ok(mut stopped) => *stopped = true,
            Err(poisoned) => *poisoned.into_inner() = true,
        }
        wake.notify_all();
        self.handle
            .join()
            .map_err(|_| io::Error::other("autosave worker panicked"))
    }
}

fn autosave_loop(
    database: Database,
    path: PathBuf,
    interval: Duration,
    mut last_saved_revision: u64,
    control: Arc<(Mutex<bool>, Condvar)>,
) {
    let (stopped, wake) = &*control;
    let mut stopped = match stopped.lock() {
        Ok(stopped) => stopped,
        Err(_) => return,
    };
    loop {
        let result = wake.wait_timeout(stopped, interval);
        let (next_stopped, wait) = match result {
            Ok(result) => result,
            Err(_) => return,
        };
        stopped = next_stopped;
        if *stopped {
            return;
        }
        if wait.timed_out() {
            drop(stopped);
            match database.save_if_changed(&path, last_saved_revision) {
                Ok(Some(revision)) => last_saved_revision = revision,
                Ok(None) => {}
                Err(error) => {
                    eprintln!("failed to save snapshot to {}: {error}", path.display())
                }
            }
            stopped = match control.0.lock() {
                Ok(stopped) => stopped,
                Err(_) => return,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use vectors::{ExecutionResult, Value};

    use super::*;

    static PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn validates_autosave_intervals() {
        assert_eq!(
            parse_autosave_interval("15").unwrap(),
            Duration::from_secs(15)
        );
        assert_eq!(
            parse_autosave_interval("0").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            parse_autosave_interval("soon").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn parses_bind_options_and_suggests_another_port() {
        assert_eq!(
            parse_arguments(&[
                "--port".into(),
                "8081".into(),
                "--data-dir".into(),
                "./data".into(),
            ])
            .unwrap(),
            StartupAction::Run(StartupOptions {
                bind_override: Some("127.0.0.1:8081".into()),
                data_dir_override: Some(PathBuf::from("./data")),
            })
        );
        assert_eq!(
            parse_arguments(&["--bind".into(), "0.0.0.0:9000".into()]).unwrap(),
            StartupAction::Run(StartupOptions {
                bind_override: Some("0.0.0.0:9000".into()),
                data_dir_override: None,
            })
        );
        assert!(parse_arguments(&["--port".into(), "0".into()]).is_err());
        assert!(parse_arguments(&["--port".into(), "busy".into()]).is_err());
        assert!(parse_arguments(&["--data-dir".into()]).is_err());
        assert_eq!(suggested_port("127.0.0.1:8080"), 8081);
        assert_eq!(suggested_port("invalid"), 8081);
    }

    #[test]
    fn periodically_saves_a_loadable_snapshot() {
        let sequence = PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "vectors-autosave-test-{}-{sequence}.vdb",
            std::process::id()
        ));
        let database = Database::new();
        database
            .execute("CREATE TABLE entries (id INTEGER PRIMARY KEY)")
            .unwrap();
        let worker =
            AutosaveWorker::start(database.clone(), path.clone(), Duration::from_millis(10))
                .unwrap();
        database.execute("INSERT INTO entries VALUES (1)").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut loaded = false;
        while Instant::now() < deadline {
            if let Ok(restored) = Database::open(&path) {
                if let Ok(results) = restored.execute("SELECT COUNT(*) FROM entries") {
                    loaded = matches!(
                        &results[0],
                        ExecutionResult::Query(result)
                            if result.rows[0][0] == Value::Integer(1)
                    );
                    if loaded {
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(5));
        }

        worker.stop().unwrap();
        let _ = fs::remove_file(path);
        assert!(loaded, "autosave did not produce a loadable snapshot");
    }
}
