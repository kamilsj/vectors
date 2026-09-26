//! Small, literal API-key files loaded before the server starts any threads.

use std::env;
use std::fs::{self, File, Metadata};
use std::io::{self, Read};
use std::path::Path;

const MAX_FILE_BYTES: usize = 64 * 1024;
const KEY_NAMES: [&str; 2] = ["OPENAI_API_KEY", "VOYAGE_API_KEY"];
type Assignments = Vec<(&'static str, String)>;

/// Call only during single-threaded startup, before creating the async runtime.
pub(super) fn load(explicit_path: Option<&Path>) -> io::Result<()> {
    let path = explicit_path.unwrap_or_else(|| Path::new(".env.local"));
    let Some(contents) = read_file(path, explicit_path.is_none())? else {
        return Ok(());
    };
    let assignments = resolve(&contents, |name| env::var_os(name).is_some())?;
    // Parsing and validation finish before any process state changes. No values
    // are logged, and an existing value (even an empty one) is never replaced.
    for (name, value) in assignments {
        env::set_var(name, value);
    }
    Ok(())
}

fn read_file(path: &Path, optional: bool) -> io::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if optional && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_metadata(&metadata)?;
    let file = File::open(path)?;
    validate_metadata(&file.metadata()?)?;
    let mut contents = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut contents)?;
    if contents.len() > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "API-key file exceeds 64 KiB",
        ));
    }
    Ok(Some(contents))
}

fn validate_metadata(metadata: &Metadata) -> io::Result<()> {
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "API-key file must be a regular file, not a link or directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "API-key file must be private; set permissions to 0600",
            ));
        }
    }
    Ok(())
}

fn resolve(contents: &[u8], mut exists: impl FnMut(&str) -> bool) -> io::Result<Assignments> {
    let assignments = parse(contents)?;
    Ok(assignments
        .into_iter()
        .filter(|(name, value)| !value.trim().is_empty() && !exists(name))
        .collect())
}

fn parse(contents: &[u8]) -> io::Result<Assignments> {
    if contents.len() > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "API-key file exceeds 64 KiB",
        ));
    }
    let contents = std::str::from_utf8(contents).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "API-key file must contain UTF-8 text",
        )
    })?;
    let mut assignments = Vec::new();
    let mut seen = [false; KEY_NAMES.len()];
    for (index, line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| line_error(line_number, "expected NAME=value"))?;
        let key_index = KEY_NAMES
            .iter()
            .position(|key| *key == name.trim())
            .ok_or_else(|| line_error(line_number, "unsupported variable"))?;
        if seen[key_index] {
            return Err(line_error(line_number, "duplicate variable"));
        }
        seen[key_index] = true;
        let value = value.trim();
        let value = if let Some(quote @ ('\'' | '"')) = value.chars().next() {
            if value.len() < 2 || !value.ends_with(quote) {
                return Err(line_error(line_number, "unmatched quotes"));
            }
            &value[1..value.len() - 1]
        } else {
            if value.contains(['\'', '"']) {
                return Err(line_error(
                    line_number,
                    "quotes must enclose the whole value",
                ));
            }
            value
        };
        if value.contains('\0') {
            return Err(line_error(line_number, "invalid credential character"));
        }
        assignments.push((KEY_NAMES[key_index], value.into()));
    }
    Ok(assignments)
}

fn line_error(line: usize, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid API-key file at line {line}: {reason}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestFile(std::path::PathBuf);

    impl TestFile {
        fn new(contents: &[u8]) -> Self {
            let path = env::temp_dir().join(format!(
                "vectors-env-file-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            use std::io::Write;
            options.open(&path).unwrap().write_all(contents).unwrap();
            Self(path)
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn parses_comments_quotes_crlf_and_literal_shell_characters() {
        let assignments = parse(b" # comment\r\n\r\nOPENAI_API_KEY = 'synthetic=$HOME#part'\r\nVOYAGE_API_KEY=\"$(no-command)\\literal\"\r\n").unwrap();
        assert_eq!(
            assignments,
            vec![
                ("OPENAI_API_KEY", "synthetic=$HOME#part".into()),
                ("VOYAGE_API_KEY", "$(no-command)\\literal".into())
            ]
        );
    }

    #[test]
    fn preserves_existing_values_including_empty_and_skips_blank_file_values() {
        let contents = b"OPENAI_API_KEY=file-openai\nVOYAGE_API_KEY=file-voyage\n";
        for existing_value in ["existing", ""] {
            let environment = [("OPENAI_API_KEY", existing_value)];
            let assignments = resolve(contents, |name| {
                environment.iter().any(|(key, _)| *key == name)
            })
            .unwrap();
            assert_eq!(assignments, vec![("VOYAGE_API_KEY", "file-voyage".into())]);
        }
        for contents in [
            b"OPENAI_API_KEY=\nVOYAGE_API_KEY=''".as_slice(),
            b"OPENAI_API_KEY=\"\"\nVOYAGE_API_KEY=' '",
        ] {
            assert!(resolve(contents, |_| false).unwrap().is_empty());
        }
    }

    #[test]
    fn rejects_entire_file_before_resolving_any_environment_changes() {
        for invalid in [
            "VOYAGE_API_KEY='private-unclosed",
            "UNKNOWN=private-unknown",
            "VECTORS_API_TOKEN=private-token",
            "OPENAI_API_KEY=private-duplicate",
            "private-malformed",
            "VOYAGE_API_KEY=private\0null",
        ] {
            let contents = format!("OPENAI_API_KEY=private-first\n{invalid}");
            let mut checked_environment = false;
            let error = resolve(contents.as_bytes(), |_| {
                checked_environment = true;
                false
            })
            .unwrap_err();
            assert!(
                !checked_environment,
                "a malformed file must fail before applying any entries"
            );
            let message = error.to_string();
            assert!(message.contains("line 2"));
            assert!(!message.contains("private"));
        }
    }

    #[test]
    fn rejects_invalid_utf8_oversized_files_and_unmatched_quotes() {
        assert!(parse(&[0xff]).is_err());
        assert!(parse(&vec![b' '; MAX_FILE_BYTES + 1]).is_err());
        for contents in [
            b"OPENAI_API_KEY='".as_slice(),
            b"OPENAI_API_KEY=\"mismatch'",
            b"OPENAI_API_KEY=unquoted'",
            b"OPENAI_API_KEY=export \"key\"",
        ] {
            assert!(parse(contents).is_err());
        }
    }

    #[test]
    fn reads_private_regular_files_and_distinguishes_optional_missing_files() {
        let file = TestFile::new(b"OPENAI_API_KEY=synthetic-only\n");
        assert_eq!(
            read_file(&file.0, false).unwrap().unwrap(),
            b"OPENAI_API_KEY=synthetic-only\n"
        );
        let absent = file.0.with_extension("absent");
        assert!(read_file(&absent, true).unwrap().is_none());
        assert_eq!(
            read_file(&absent, false).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(read_file(&env::temp_dir(), false).is_err());
        let oversized = TestFile::new(&vec![b' '; MAX_FILE_BYTES + 1]);
        assert!(read_file(&oversized.0, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_public_permissions_and_symbolic_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let file = TestFile::new(b"OPENAI_API_KEY=synthetic-only\n");
        fs::set_permissions(&file.0, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            read_file(&file.0, false).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&file.0, fs::Permissions::from_mode(0o600)).unwrap();
        let link = TestFile(file.0.with_extension("link"));
        symlink(&file.0, &link.0).unwrap();
        assert_eq!(
            read_file(&link.0, false).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
