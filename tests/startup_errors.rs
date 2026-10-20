//! Startup failures must be reported without contaminating hook protocol stdout.

use std::path::Path;
use std::process::Command;

use symposium::config::{LoggingError, Symposium};

#[cfg(unix)]
fn assert_startup_failure(output: std::process::Output, expected: &str) {
    assert!(
        output.status.success(),
        "Claude reads its native JSON denial"
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "deny");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}

#[test]
fn subscriber_conflict_is_reported() {
    const CHILD: &str = "SYMPOSIUM_SUBSCRIBER_CONFLICT_TEST";
    if let Some(home) = std::env::var_os(CHILD) {
        let sym = Symposium::try_from_dir(Path::new(&home)).unwrap();
        tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
        let error = sym.try_init_logging(None).unwrap_err();
        assert!(matches!(error, LoggingError::Subscriber { .. }));
        assert!(std::error::Error::source(&error).is_some());
        return;
    }

    // A separate process owns the global subscriber; parallel tests stay isolated.
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "subscriber_conflict_is_reported", "--nocapture"])
        .env(CHILD, tmp.path())
        .env("SYMPOSIUM_HOME", tmp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child stdout: {}\nchild stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn legacy_entry_point_signatures_remain_compatible() {
    let _: fn() -> Symposium = Symposium::from_environment;
    let _: fn(&Path) -> Symposium = Symposium::from_dir;
    let _: fn(&Symposium, Option<symposium::report::ReportLayer>) = Symposium::init_logging;
    let _: fn() -> symposium::dirs::SymposiumDirs =
        symposium::dirs::SymposiumDirs::from_environment;
}

#[cfg(unix)]
#[test]
fn deleted_working_directory_is_a_reported_startup_error() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("deleted-workspace");
    std::fs::create_dir(&cwd).unwrap();
    let output = Command::new("sh")
        .args([
            "-c",
            "cd \"$1\" && rmdir \"$1\" && exec \"$2\" hook claude pre-tool-use",
            "startup-test",
        ])
        .arg(&cwd)
        .arg(env!("CARGO_BIN_EXE_cargo-agents"))
        .env("SYMPOSIUM_HOME", tmp.path().join("home"))
        .output()
        .unwrap();
    assert_startup_failure(output, "failed to get current directory");
}
