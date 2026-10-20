//! Startup diagnostics must stay off protocol stdout and preserve user files.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn startup_command(root: &Path, home: &Path, args: &[&str]) -> Command {
    let project = root.join("project");
    let user_home = root.join("user-home");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&user_home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-agents"));
    command
        .args(args)
        .current_dir(project)
        .env("SYMPOSIUM_HOME", home)
        .env("HOME", &user_home)
        .env("USERPROFILE", &user_home)
        .env("CARGO_NET_OFFLINE", "true")
        .env_remove("SYMPOSIUM_CARGO")
        .env_remove("SYMPOSIUM_ENABLE_AGENT_TESTING")
        .env_remove("SYMPOSIUM_TEST_AGENT")
        .stdin(Stdio::null());
    command
}

fn assert_startup_failure(output: Output, operation: &str, path: &Path) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(operation), "stderr: {stderr}");
    assert!(
        stderr.contains(&path.display().to_string()),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "stderr: {stderr}");
}

#[test]
fn configuration_directory_failure_preserves_file_and_hook_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("symposium-home");
    let original = b"existing file must survive startup";
    std::fs::write(&home, original).unwrap();

    let output = startup_command(tmp.path(), &home, &["hook", "claude", "pre-tool-use"])
        .output()
        .unwrap();

    assert_startup_failure(output, "failed to create configuration directory", &home);
    assert_eq!(std::fs::read(&home).unwrap(), original);
}

#[test]
fn unreadable_configuration_does_not_silently_use_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("symposium-home");
    let config = home.join("config.toml");
    std::fs::create_dir_all(&config).unwrap();
    let marker = config.join("preserve-me");
    std::fs::write(&marker, b"original").unwrap();

    let output = startup_command(tmp.path(), &home, &["hook", "claude", "pre-tool-use"])
        .output()
        .unwrap();

    assert_startup_failure(output, "failed to read configuration", &config);
    assert_eq!(std::fs::read(marker).unwrap(), b"original");
}

#[test]
fn log_directory_failure_preserves_file_and_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("symposium-home");
    std::fs::create_dir(&home).unwrap();
    let logs = home.join("logs");
    let original = b"existing log-path file";
    std::fs::write(&logs, original).unwrap();

    for args in [&["status"][..], &["hook", "claude", "pre-tool-use"][..]] {
        let output = startup_command(tmp.path(), &home, args).output().unwrap();
        assert_startup_failure(output, "failed to create log directory", &logs);
        assert_eq!(std::fs::read(&logs).unwrap(), original);
        assert!(!home.join("state.toml").exists());
    }
}

#[test]
fn missing_configuration_allows_help_without_initializing_user_state() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("symposium-home");

    let output = startup_command(tmp.path(), &home, &["--help"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Commands for humans"), "stdout: {stdout}");
    assert!(stdout.contains("Commands for agents"), "stdout: {stdout}");
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
    assert!(!home.join("config.toml").exists());
    assert!(!home.join("state.toml").exists());
    assert!(!home.join("logs").exists());
    assert!(!tmp.path().join("user-home/.claude").exists());
    assert!(!tmp.path().join("user-home/.codex").exists());
}
