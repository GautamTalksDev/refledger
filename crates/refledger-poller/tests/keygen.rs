//! CLI coverage for `refledger-poller keygen`.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use tempfile::TempDir;

#[test]
fn keygen_cli_stdout_never_contains_seed_and_refuses_overwrite() {
    let bin = env!("CARGO_BIN_EXE_refledger-poller");
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("signing.key");

    let out = Command::new(bin)
        .args(["keygen", "--out", path.to_str().unwrap()])
        .output()
        .expect("run keygen");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let seed = std::fs::read_to_string(&path).expect("read seed");
    let seed = seed.trim();
    assert_eq!(seed.len(), 64);

    assert!(
        !stdout.contains(seed),
        "stdout must never contain the seed; got:\n{stdout}"
    );
    assert!(
        !stderr.contains(seed),
        "stderr must never contain the seed; got:\n{stderr}"
    );
    assert!(stdout.contains("public_key="));
    assert!(stdout.contains("key_id=sha256:"));

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let again = Command::new(bin)
        .args(["keygen", "--out", path.to_str().unwrap()])
        .output()
        .expect("run keygen overwrite");
    assert!(!again.status.success(), "overwrite must fail");
    let err = String::from_utf8_lossy(&again.stderr);
    assert!(
        err.to_lowercase().contains("overwrite")
            || err.contains("AlreadyExists")
            || err.contains("exists"),
        "expected overwrite refusal, got: {err}"
    );
}
