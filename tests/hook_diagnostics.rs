//! Real hook processes keep payloads on the agent protocol and persist only metadata.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use symposium::hook_schema::{HookAgent, HookEvent};
use symposium_testlib::HookTrace;

const PROMPT: &str = "PROMPT_SECRET_CANARY";
const TOOL_ARGUMENT: &str = "TOOL_ARGUMENT_SECRET_CANARY";
const TOOL_RESULT: &str = "TOOL_RESULT_SECRET_CANARY";
const PLUGIN_OUTPUT: &str = "PLUGIN_OUTPUT_SECRET_CANARY";
const PLUGIN_STDERR: &str = "PLUGIN_STDERR_SECRET_CANARY";
const DENIAL_REASON: &str = "DENIAL_REASON_SECRET_CANARY";
const MALFORMED_INPUT: &str = "MALFORMED_INPUT_SECRET_CANARY";
const MALFORMED_OUTPUT: &str = "MALFORMED_OUTPUT_SECRET_CANARY";
const LOG_LEVEL: &str = "LOG_LEVEL_SECRET_CANARY";
const REGISTRY_URL: &str = "REGISTRY_URL_SECRET_CANARY";
const MALFORMED_CONFIG: &str = "MALFORMED_CONFIG_SECRET_CANARY";
const AGENT_CONFIG: &str = "AGENT_CONFIG_SECRET_CANARY";

const CANARIES: &[&str] = &[
    PROMPT,
    TOOL_ARGUMENT,
    TOOL_RESULT,
    PLUGIN_OUTPUT,
    PLUGIN_STDERR,
    DENIAL_REASON,
    MALFORMED_INPUT,
    MALFORMED_OUTPUT,
    LOG_LEVEL,
    REGISTRY_URL,
    MALFORMED_CONFIG,
    AGENT_CONFIG,
];

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    trace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let trace = temp.path().join("trace.jsonl");
        let fixture = Self {
            _temp: temp,
            home,
            project,
            trace,
        };
        fixture.config("trace", "");
        fixture
    }

    fn config(&self, level: &str, extra: &str) {
        std::fs::write(
            self.home.join("config.toml"),
            format!(
                "hook-scope = \"project\"\nauto-sync = false\nauto-update = \"off\"\n\
                 [defaults]\nsymposium-recommendations = false\nuser-plugins = true\n\
                 [logging]\nlevel = {level:?}\n{extra}"
            ),
        )
        .unwrap();
    }

    fn plugin(&self, event: &str, format: &str, stdout: &str, stderr: &str, exit_code: u8) {
        let plugin = self.home.join("plugins/diagnostic-probe");
        std::fs::create_dir_all(&plugin).unwrap();
        let script = plugin.join("probe.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat > received.json\nprintf '%s' '{stdout}'\n\
                 printf '%s' '{stderr}' >&2\nexit {exit_code}\n"
            ),
        )
        .unwrap();
        let script_path = script.to_string_lossy().replace('\\', "/");
        std::fs::write(
            plugin.join("Symposium.toml"),
            format!(
                "name = \"diagnostic-probe\"\ndepends-on = [\"*\"]\n\
                 [[hooks]]\nname = \"probe\"\nevent = {event:?}\n\
                 format = {format:?}\ncommand = {{ script = {script_path:?} }}\n"
            ),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str], input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cargo-agents"))
            .args(args)
            .env("SYMPOSIUM_HOME", &self.home)
            .env("SYMPOSIUM_HOOK_TRACE", &self.trace)
            .env_remove("SYMPOSIUM_ENABLE_AGENT_TESTING")
            .env_remove("SYMPOSIUM_TEST_AGENT")
            .current_dir(&self.project)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn hook(&self, event: &str, input: &str) -> Output {
        self.command(&["hook", "claude", event], input)
    }

    fn assert_received(&self, values: &[&str]) {
        let received = std::fs::read_to_string(self.project.join("received.json")).unwrap();
        for value in values {
            assert!(received.contains(value), "plugin must receive {value}");
        }
    }

    fn assert_private_diagnostics(&self) {
        let logs = std::fs::read_dir(self.home.join("logs")).unwrap();
        let mut count = 0;
        for entry in logs {
            let log = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            assert_no_canaries(&log);
            count += 1;
        }
        assert!(count > 0, "the check must inspect real log files");
        if self.trace.exists() {
            assert_no_canaries(&std::fs::read_to_string(&self.trace).unwrap());
        }
    }

    fn assert_trace(&self, event: HookEvent, input: &str, output: &Output) {
        let trace = std::fs::read_to_string(&self.trace).unwrap();
        let entry: Value = serde_json::from_str(&trace).unwrap();
        assert_eq!(
            entry,
            json!({
                "event": event,
                "agent": HookAgent::Claude,
                "input": {"byte_len": input.len()},
                "output": {"byte_len": output.stdout.len()},
                "exit_code": output.status.code().unwrap(),
                "stderr_len": output.stderr.len(),
            })
        );
        let compatible: HookTrace = serde_json::from_str(&trace).unwrap();
        assert_eq!(compatible.event, event);
        assert_eq!(compatible.input, json!({"byte_len": input.len()}));
    }
}

fn assert_no_canaries(text: &str) {
    for canary in CANARIES {
        assert!(!text.contains(canary), "diagnostics disclosed {canary}");
    }
}

fn payload(event: &str, project: &Path) -> Value {
    json!({"hook_event_name": event, "cwd": project, "session_id": "diagnostic-test"})
}

#[test]
fn prompt_and_plugin_streams_reach_the_protocol_without_persistent_copies() {
    let fixture = Fixture::new();
    let response = json!({"UserPromptSubmit": {"additionalContext": PLUGIN_OUTPUT}});
    fixture.plugin(
        "UserPromptSubmit",
        "symposium",
        &response.to_string(),
        PLUGIN_STDERR,
        0,
    );
    let mut input = payload("UserPromptSubmit", &fixture.project);
    input["prompt"] = json!(PROMPT);
    let input = input.to_string();
    let output = fixture.hook("user-prompt-submit", &input);
    assert!(output.status.success());
    let protocol: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        protocol["hookSpecificOutput"]["additionalContext"],
        PLUGIN_OUTPUT
    );
    fixture.assert_received(&[PROMPT]);
    fixture.assert_private_diagnostics();
    fixture.assert_trace(HookEvent::UserPromptSubmit, &input, &output);
}

#[test]
fn tool_arguments_and_results_stay_out_of_persistent_diagnostics() {
    let fixture = Fixture::new();
    let response = json!({"PostToolUse": {"additionalContext": PLUGIN_OUTPUT}});
    fixture.plugin(
        "PostToolUse",
        "symposium",
        &response.to_string(),
        PLUGIN_STDERR,
        0,
    );
    let mut input = payload("PostToolUse", &fixture.project);
    input["tool_name"] = json!("Bash");
    input["tool_input"] = json!({"command": TOOL_ARGUMENT});
    input["tool_response"] = json!({"stdout": TOOL_RESULT});
    let input = input.to_string();
    let output = fixture.hook("post-tool-use", &input);
    assert!(output.status.success());
    let protocol: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        protocol["hookSpecificOutput"]["additionalContext"],
        PLUGIN_OUTPUT
    );
    fixture.assert_received(&[TOOL_ARGUMENT, TOOL_RESULT]);
    fixture.assert_private_diagnostics();
    fixture.assert_trace(HookEvent::PostToolUse, &input, &output);
}

#[test]
fn denial_reason_reaches_the_agent_without_becoming_log_or_trace_content() {
    let fixture = Fixture::new();
    fixture.plugin("PreToolUse", "symposium", PLUGIN_OUTPUT, DENIAL_REASON, 2);
    let mut input = payload("PreToolUse", &fixture.project);
    input["tool_name"] = json!("Bash");
    input["tool_input"] = json!({"command": TOOL_ARGUMENT});
    let input = input.to_string();
    let output = fixture.hook("pre-tool-use", &input);
    assert!(output.status.success());
    let protocol: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(protocol["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(
        protocol["hookSpecificOutput"]["permissionDecisionReason"],
        DENIAL_REASON
    );
    fixture.assert_private_diagnostics();
    fixture.assert_trace(HookEvent::PreToolUse, &input, &output);
}

#[test]
fn blocking_stderr_is_delivered_but_only_its_length_is_traced() {
    let fixture = Fixture::new();
    fixture.plugin(
        "UserPromptSubmit",
        "symposium",
        PLUGIN_OUTPUT,
        DENIAL_REASON,
        2,
    );
    let mut input = payload("UserPromptSubmit", &fixture.project);
    input["prompt"] = json!(PROMPT);
    let input = input.to_string();
    let output = fixture.hook("user-prompt-submit", &input);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr.clone()).unwrap(),
        DENIAL_REASON
    );
    fixture.assert_private_diagnostics();
    fixture.assert_trace(HookEvent::UserPromptSubmit, &input, &output);
}

#[test]
fn malformed_typed_input_is_rejected_without_logging_parser_source_text() {
    let fixture = Fixture::new();
    let input = serde_json::to_string(MALFORMED_INPUT).unwrap();
    let output = fixture.hook("pre-tool-use", &input);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_no_canaries(&String::from_utf8(output.stderr).unwrap());
    fixture.assert_private_diagnostics();
    assert!(
        !fixture.trace.exists(),
        "failed dispatch emits no response trace"
    );
}

#[test]
fn malformed_native_plugin_output_is_not_formatted_into_diagnostics() {
    let fixture = Fixture::new();
    let response = json!({"hookSpecificOutput": MALFORMED_OUTPUT});
    fixture.plugin(
        "PreToolUse",
        "claude",
        &response.to_string(),
        PLUGIN_STDERR,
        0,
    );
    let mut input = payload("PreToolUse", &fixture.project);
    input["tool_name"] = json!("Bash");
    input["tool_input"] = json!({"command": TOOL_ARGUMENT});
    let input = input.to_string();
    let output = fixture.hook("pre-tool-use", &input);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({})
    );
    fixture.assert_private_diagnostics();
    fixture.assert_trace(HookEvent::PreToolUse, &input, &output);
}

#[test]
fn config_diagnostics_do_not_echo_invalid_values_or_source_text() {
    let fixture = Fixture::new();
    fixture.config(
        LOG_LEVEL,
        &format!("\n[[registry]]\nname = \"probe\"\ngit = {REGISTRY_URL:?}\n"),
    );
    let mut input = payload("PreToolUse", &fixture.project);
    input["tool_name"] = json!("Bash");
    let output = fixture.hook("pre-tool-use", &input.to_string());
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown log level"));
    assert_no_canaries(&stderr);
    fixture.assert_private_diagnostics();

    std::fs::write(
        fixture.home.join("config.toml"),
        format!("[broken\n{MALFORMED_CONFIG}\n"),
    )
    .unwrap();
    let output = fixture.command(&["--version"], "");
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invalid TOML; using defaults"));
    assert_no_canaries(&stderr);
}

// Windows resolves home through its known-folder API, so HOME cannot isolate this mutation.
#[cfg(unix)]
#[test]
fn cli_agent_config_failure_keeps_parser_source_private_and_original_file_intact() {
    let fixture = Fixture::new();
    let codex = fixture.home.join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    let path = codex.join("config.toml");
    let original = format!("model = '{AGENT_CONFIG}' invalid\n");
    let source = original.parse::<toml_edit::DocumentMut>().unwrap_err();
    assert!(
        source.to_string().contains(AGENT_CONFIG),
        "the parser source must exercise the leak"
    );
    std::fs::write(&path, &original).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-agents"))
        .args(["init", "--add-agent", "codex", "--hook-scope", "global"])
        .env("HOME", &fixture.home)
        .env("USERPROFILE", &fixture.home)
        .env("SYMPOSIUM_HOME", &fixture.home)
        .env_remove("SYMPOSIUM_ENABLE_AGENT_TESTING")
        .env_remove("SYMPOSIUM_TEST_AGENT")
        .current_dir(&fixture.project)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invalid TOML in agent configuration"));
    assert!(stderr.contains("config.toml"));
    assert_no_canaries(&stderr);
    assert_no_canaries(&String::from_utf8(output.stdout).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    fixture.assert_private_diagnostics();
}
