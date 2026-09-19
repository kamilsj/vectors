//! Thin CLI bridge to the native updater. The SQL engine never runs an updater.

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const HELP: &str = "\
Update vectors from the latest stable GitHub release.

Usage: vectors update [options]

  --check             Check availability without installing
  --watch             Automatically check and apply newer releases
  --interval SECONDS  Watch interval: 60–604800 (default: 21600)
  --install-dir PATH  Override the current executable's installation directory
  --no-start          Replace binaries without restarting a managed server
  -h, --help          Show this help

Updates verify release checksums and preserve durable data. A running managed
server restarts gracefully; a stopped server stays stopped. Watch mode must
remain running (use your service manager for unattended operation). On Windows,
installation opens a separate updater window so vectors.exe can be replaced.
Provide your server's authentication, provider keys, and runtime settings in
the updater environment. Use --check to inspect availability first.";

#[derive(Debug, Default, PartialEq)]
struct Options {
    check: bool,
    watch: bool,
    no_start: bool,
    interval: Option<u32>,
    install_dir: Option<PathBuf>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn parse(arguments: &[String]) -> io::Result<Option<Options>> {
    let mut options = Options::default();
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => return Ok(None),
            "--check" => options.check = true,
            "--watch" => options.watch = true,
            "--no-start" => options.no_start = true,
            "--interval" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| invalid("--interval needs seconds"))?;
                let seconds = value
                    .parse::<u32>()
                    .map_err(|_| invalid("--interval must be an integer from 60 to 604800"))?;
                if !(60..=604800).contains(&seconds) {
                    return Err(invalid("--interval must be an integer from 60 to 604800"));
                }
                options.interval = Some(seconds);
            }
            "--install-dir" => {
                let path = arguments
                    .next()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| invalid("--install-dir needs a path"))?;
                options.install_dir = Some(PathBuf::from(path));
            }
            _ => {
                return Err(invalid(
                    "unknown update option; run 'vectors update --help'",
                ))
            }
        }
    }
    if options.check && options.watch {
        return Err(invalid("--check and --watch cannot be combined"));
    }
    Ok(Some(options))
}

pub(super) fn run(arguments: &[String]) -> io::Result<i32> {
    let Some(mut options) = parse(arguments)? else {
        println!("{HELP}");
        return Ok(0);
    };
    if options.install_dir.is_none() {
        options.install_dir =
            match env::var_os("VECTORS_INSTALL_DIR").filter(|value| !value.is_empty()) {
                Some(path) => Some(path.into()),
                None => env::current_exe()?.parent().map(PathBuf::from),
            };
    }
    invoke(&options)
}

#[cfg(unix)]
fn invoke(options: &Options) -> io::Result<i32> {
    let mut command = Command::new("sh");
    command.args(["-s", "--"]);
    if options.check {
        command.arg("--check");
    }
    if options.watch {
        command.arg("--watch");
    }
    if options.no_start {
        command.arg("--no-start");
    }
    if let Some(interval) = options.interval {
        command.arg("--interval").arg(interval.to_string());
    }
    if let Some(path) = &options.install_dir {
        command.arg("--install-dir").arg(path);
    }
    let mut child = command.stdin(Stdio::piped()).spawn()?;
    let write = child
        .stdin
        .take()
        .expect("piped updater input")
        .write_all(include_bytes!("../update.sh"));
    let status = child.wait()?;
    // Keep the shell's useful error status when it rejects the invocation
    // before consuming the whole embedded script.
    if status.success() {
        write?;
    }
    Ok(status.code().unwrap_or(1))
}

#[cfg(windows)]
fn invoke(options: &Options) -> io::Result<i32> {
    use std::fs::{self, OpenOptions};
    use std::os::windows::process::CommandExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let path = env::temp_dir().join(format!("vectors-update-{}-{nonce}.ps1", std::process::id()));
    let mut script = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    script.write_all(include_bytes!("../update.ps1"))?;
    script.sync_all()?;
    drop(script);
    let mut parameters = serde_json::json!({
        "Check": options.check,
        "Watch": options.watch,
        "NoStart": options.no_start,
        "IntervalSeconds": options.interval.unwrap_or(21600),
        "InstallDir": options.install_dir,
    });
    if !options.check {
        parameters["WaitForProcessId"] = serde_json::json!(std::process::id());
        parameters["CleanupScriptPath"] = serde_json::json!(path);
    }
    // Parameter values travel through JSON in the child environment, never
    // through PowerShell source interpolation. This does not change execution
    // policy or write credentials to disk.
    let mut command = Command::new("powershell.exe");
    command.arg("-NoProfile");
    if !options.check {
        command.arg("-NoExit");
    }
    command.args(["-Command", "$ErrorActionPreference = 'Stop'; $values = ConvertFrom-Json $env:VECTORS_UPDATER_OPTIONS; $options = @{}; $values.psobject.Properties | ForEach-Object { $options[$_.Name] = $_.Value }; try { & ([scriptblock]::Create([IO.File]::ReadAllText($env:VECTORS_UPDATER_SCRIPT))) @options } catch { [Console]::Error.WriteLine($_.Exception.Message); if ($options.Check) { exit 1 } }"]);
    command
        .env("VECTORS_UPDATER_SCRIPT", &path)
        .env("VECTORS_UPDATER_OPTIONS", parameters.to_string());
    if options.check {
        let result = command.stdin(Stdio::null()).status();
        let _ = fs::remove_file(path);
        return Ok(result?.code().unwrap_or(1));
    }
    const CREATE_NEW_CONSOLE: u32 = 0x00000010;
    let result = command.creation_flags(CREATE_NEW_CONSOLE).spawn();
    match result {
        Ok(child) => {
            println!("Updater started in a separate window (PID {}). Follow that window for the result; this process will exit so vectors.exe can be replaced.", child.id());
            Ok(0)
        }
        Err(error) => {
            let _ = fs::remove_file(path);
            Err(error)
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn invoke(_options: &Options) -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "automatic updates support Linux, macOS, and Windows",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn update_options_preserve_paths_without_shell_interpolation() {
        let options = parse(&args(&[
            "--watch",
            "--interval",
            "3600",
            "--install-dir",
            "a path/$(literal)",
            "--no-start",
        ]))
        .unwrap()
        .unwrap();
        assert!(options.watch && options.no_start);
        assert_eq!(options.interval, Some(3600));
        assert_eq!(
            options.install_dir,
            Some(PathBuf::from("a path/$(literal)"))
        );
    }

    #[test]
    fn invalid_update_options_are_rejected_before_starting_a_process() {
        for values in [
            vec!["--check", "--watch"],
            vec!["--interval"],
            vec!["--interval", "59"],
            vec!["--interval", "604801"],
            vec!["--interval", "NaN"],
            vec!["--install-dir"],
            vec!["--unknown"],
        ] {
            assert!(parse(&args(&values)).is_err(), "{values:?}");
        }
        assert_eq!(parse(&args(&["--help"])).unwrap(), None);
    }
}
