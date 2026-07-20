use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use vectors::{api, Database};

#[actix_web::main]
async fn main() -> io::Result<()> {
    let bind_address = env::var("VECTORS_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let snapshot = env::var_os("VECTORS_SNAPSHOT").map(PathBuf::from);
    let autosave_interval = autosave_interval(snapshot.as_deref())?;
    let api_token = env::var("VECTORS_API_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    let database = match snapshot.as_deref() {
        Some(path) if path.exists() => Database::open(path).map_err(database_error)?,
        _ => Database::new(),
    };
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

    eprintln!("vectors HTTP API listening on http://{bind_address}");
    let result = match api_token {
        Some(token) => api::serve_authenticated(database.clone(), &bind_address, token).await,
        None => api::serve(database.clone(), &bind_address).await,
    };
    if let Some(autosave) = autosave {
        autosave.stop()?;
    }
    result?;
    if let Some(path) = snapshot {
        database.save(path).map_err(database_error)?;
    }
    Ok(())
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
