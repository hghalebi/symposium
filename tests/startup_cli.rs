//! Startup diagnostics must stay off protocol stdout and preserve user files.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use symposium::hook::{HookAgent, HookEvent, failure_response};

const HOOK_AGENTS: [HookAgent; 5] = [
    HookAgent::Claude,
    HookAgent::Codex,
    HookAgent::Copilot,
    HookAgent::Kiro,
    HookAgent::Antigravity,
];

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

fn assert_startup_denial(output: Output, agent: HookAgent, operation: &str, path: &Path) {
    let expected = failure_response(agent, HookEvent::PreToolUse).unwrap();
    assert_eq!(output.status.code(), Some(expected.exit_code.into()));
    assert_eq!(output.stdout, expected.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(operation), "stderr: {stderr}");
    assert!(
        stderr.contains(&path.display().to_string()),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("panicked"));
    if agent == HookAgent::Kiro {
        assert!(output.stderr.starts_with(&expected.stderr));
    }
}

#[test]
fn configuration_directory_failure_preserves_file_and_hook_stdout() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("symposium-home");
    let original = b"existing file must survive startup";
    std::fs::write(&home, original).unwrap();

    for agent in HOOK_AGENTS {
        let output = startup_command(tmp.path(), &home, &["hook", agent.as_str(), "pre-tool-use"])
            .output()
            .unwrap();
        assert_startup_denial(
            output,
            agent,
            "failed to create configuration directory",
            &home,
        );
    }
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

    for agent in HOOK_AGENTS {
        let output = startup_command(tmp.path(), &home, &["hook", agent.as_str(), "pre-tool-use"])
            .output()
            .unwrap();
        assert_startup_denial(output, agent, "failed to read configuration", &config);
    }
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

    let output = startup_command(tmp.path(), &home, &["status"])
        .output()
        .unwrap();
    assert_startup_failure(output, "failed to create log directory", &logs);
    for agent in HOOK_AGENTS {
        let output = startup_command(tmp.path(), &home, &["hook", agent.as_str(), "pre-tool-use"])
            .output()
            .unwrap();
        assert_startup_denial(output, agent, "failed to create log directory", &logs);
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

#[cfg(unix)]
#[test]
fn startup_native_denial_survives_a_closed_diagnostic_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home-file");
    std::fs::write(&home, b"preserve").unwrap();
    let output = antigravity_with_closed_stderr(tmp.path(), &home);
    assert!(output.status.success());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["decision"], "deny");
    assert_eq!(std::fs::read(&home).unwrap(), b"preserve");
}

#[cfg(unix)]
fn antigravity_with_closed_stderr(root: &Path, home: &Path) -> Output {
    Command::new("sh")
        .args([
            "-c",
            "exec 2>&-; exec \"$1\" hook antigravity pre-tool-use",
            "closed-stderr-test",
        ])
        .arg(env!("CARGO_BIN_EXE_cargo-agents"))
        .env("SYMPOSIUM_HOME", home)
        .env("CARGO_NET_OFFLINE", "true")
        .env_remove("SYMPOSIUM_ENABLE_AGENT_TESTING")
        .env_remove("SYMPOSIUM_TEST_AGENT")
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn startup_warning_failure_with_broken_stderr_still_delivers_native_denial() {
    use std::io::Write as _;
    let inputs = [
        "private = 'STARTUP_WARNING_PRIVATE_CANARY'\n[broken",
        "hook-scope = 'project'\nauto-sync = false\nauto-update = 'off'\n\
         [defaults]\nsymposium-recommendations = false\nuser-plugins = false\n\
         [logging]\nlevel = 'STARTUP_WARNING_PRIVATE_CANARY'\n",
    ];
    for original in inputs {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let config = home.join("config.toml");
        std::fs::write(&config, original).unwrap();
        // std ignores a closed stderr fd; a pipe without a reader produces EPIPE.
        let (reader, mut writer) = std::io::pipe().unwrap();
        drop(reader);
        assert_eq!(
            writer.write_all(b"probe").unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-agents"))
            .args(["hook", "antigravity", "pre-tool-use"])
            .env("SYMPOSIUM_HOME", &home)
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("SYMPOSIUM_ENABLE_AGENT_TESTING")
            .env_remove("SYMPOSIUM_TEST_AGENT")
            .current_dir(tmp.path())
            .stdin(Stdio::null())
            .stderr(Stdio::from(writer))
            .output()
            .unwrap();
        let expected = failure_response(HookAgent::Antigravity, HookEvent::PreToolUse).unwrap();
        assert_eq!(output.status.code(), Some(expected.exit_code.into()));
        assert_eq!(output.stdout, expected.stdout);
        assert!(output.stderr.is_empty());
        assert_eq!(std::fs::read_to_string(config).unwrap(), original);
        assert!(!home.join("state.toml").exists());
    }
}
