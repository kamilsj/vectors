use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn embedded_updater_rejects_a_missing_installation_without_networking() {
    // Exercise the real shell/PowerShell bridge, including a path whose spaces
    // and punctuation must arrive as data rather than command source.
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let missing = std::env::temp_dir().join(format!(
        "vectors-missing-{}-{unique} space ' $(literal)",
        std::process::id()
    ));
    assert!(!missing.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_vectors"))
        .args(["update", "--check", "--install-dir"])
        .arg(&missing)
        .env_remove("VECTORS_VERSION")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("installation directory does not exist")
            || stderr.contains("No installation exists"),
        "unexpected updater error: {stderr}"
    );
    assert!(!missing.exists());
    assert!(output.stdout.is_empty());
}
