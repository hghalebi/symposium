//! Hook failures preserve typed sources, safe diagnostics, and native denials.

use std::error::Error;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use symposium::config::Symposium;
use symposium::hook::{HookError, execute_hook, try_execute_hook};
use symposium::hook_schema::{HookAgent, HookEvent, agent_event, symposium as canonical};
use tokio::io::AsyncWriteExt;

const CANARY: &str = "HOOK_RUNTIME_PRIVATE_CANARY";
const DENIAL_REASON: &str = "native denial must remain final";
const AGENTS: [HookAgent; 5] = [
    HookAgent::Claude,
    HookAgent::Codex,
    HookAgent::Copilot,
    HookAgent::Kiro,
    HookAgent::Antigravity,
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
        std::fs::write(
            home.join("config.toml"),
            "hook-scope = \"project\"\nauto-sync = false\nauto-update = \"off\"\n\
             [defaults]\nsymposium-recommendations = false\nuser-plugins = true\n\
             [logging]\nlevel = \"trace\"\n",
        )
        .unwrap();
        Self {
            trace: temp.path().join("trace.jsonl"),
            _temp: temp,
            home,
            project,
        }
    }

    fn symposium(&self) -> Symposium {
        Symposium::try_from_dir(&self.home).unwrap()
    }

    fn input(&self, agent: HookAgent) -> String {
        let input = canonical::InputEvent::PreToolUse(canonical::PreToolUseInput::new(
            "RuntimeProbe".into(),
            json!({"private": CANARY}),
            Some("runtime-test".into()),
            Some(self.project.display().to_string()),
        ));
        agent_event(agent, HookEvent::PreToolUse)
            .unwrap()
            .try_translate_input(&input)
            .unwrap()
            .try_to_string()
            .unwrap()
    }

    fn hook(&self, agent: HookAgent, input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cargo-agents"))
            .args(["hook", agent.as_str(), "pre-tool-use"])
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

    fn plugin(&self, name: &str, output: &Value) {
        self.plugin_script(
            name,
            &format!("#!/bin/sh\ncat > /dev/null\n: > {name}-ran\nprintf '%s' '{output}'\n"),
        );
    }

    fn plugin_script(&self, name: &str, contents: &str) {
        let plugin = self.home.join("plugins").join(name);
        std::fs::create_dir_all(&plugin).unwrap();
        let script = plugin.join("hook.sh");
        std::fs::write(&script, contents).unwrap();
        let script = script.to_string_lossy().replace('\\', "/");
        std::fs::write(
            plugin.join("Symposium.toml"),
            format!(
                "name = {name:?}\ndepends-on = [\"*\"]\n\
                 [[hooks]]\nname = \"probe\"\nevent = \"PreToolUse\"\n\
                 format = \"symposium\"\ncommand = {{ script = {script:?} }}\n"
            ),
        )
        .unwrap();
    }

    fn assert_private_diagnostics(&self, output: &Output) {
        assert_private(&String::from_utf8_lossy(&output.stderr));
        let mut logs = 0;
        for entry in std::fs::read_dir(self.home.join("logs")).unwrap() {
            let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            assert_private(&text);
            logs += 1;
        }
        assert!(logs > 0, "the test must inspect actual log files");
        if self.trace.exists() {
            let trace = std::fs::read_to_string(&self.trace).unwrap();
            assert_private(&trace);
            for line in trace.lines() {
                let trace: symposium_testlib::HookTrace = serde_json::from_str(line).unwrap();
                assert_eq!(trace.event, HookEvent::PreToolUse);
                assert!(trace.input.get("byte_len").is_some());
                assert!(trace.output.get("byte_len").is_some());
            }
        }
    }
}

fn assert_private(text: &str) {
    assert!(
        !text.contains(CANARY),
        "diagnostics disclosed private input"
    );
}

fn denial_json(agent: HookAgent, reason: &str) -> Value {
    match agent {
        HookAgent::Claude | HookAgent::Codex => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }),
        HookAgent::Copilot => json!({
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }),
        HookAgent::Antigravity => json!({"decision": "deny", "reason": reason}),
        HookAgent::Kiro | HookAgent::Goose | HookAgent::OpenCode => {
            panic!("this agent has no native JSON tool denial")
        }
    }
}

fn assert_native_denial(agent: HookAgent, output: &Output) {
    assert_private(&String::from_utf8_lossy(&output.stdout));
    assert_private(&String::from_utf8_lossy(&output.stderr));
    if agent == HookAgent::Kiro {
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty(), "Kiro needs a denial reason");
        return;
    }
    assert!(output.status.success(), "{agent:?} reads denial JSON");
    assert!(output.stderr.is_empty(), "{agent:?} protocol stderr");
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    let reason = match agent {
        HookAgent::Claude | HookAgent::Codex => {
            &response["hookSpecificOutput"]["permissionDecisionReason"]
        }
        HookAgent::Copilot => &response["permissionDecisionReason"],
        HookAgent::Antigravity => &response["reason"],
        HookAgent::Kiro | HookAgent::Goose | HookAgent::OpenCode => unreachable!(),
    };
    let reason = reason.as_str().filter(|reason| !reason.is_empty()).unwrap();
    assert_eq!(response, denial_json(agent, reason), "{agent:?} denial");
}

#[test]
fn malformed_permission_inputs_produce_native_denials_without_diagnostic_copies() {
    let inputs = [
        serde_json::to_string(CANARY).unwrap(),
        format!("{{\"private\":\"{CANARY}\""),
        format!("{{\"tool_name\":null,\"toolName\":null,\"toolCall\":\"{CANARY}\"}}"),
    ];
    for agent in AGENTS {
        for input in &inputs {
            let fixture = Fixture::new();
            let output = fixture.hook(agent, input);
            assert_native_denial(agent, &output);
            fixture.assert_private_diagnostics(&output);
        }
    }
}

#[test]
fn mismatched_native_input_events_cannot_dispatch_a_permission_hook() {
    for agent in [HookAgent::Claude, HookAgent::Codex, HookAgent::Kiro] {
        let post_tool_use = if agent == HookAgent::Kiro {
            "postToolUse"
        } else {
            "PostToolUse"
        };
        for label in [post_tool_use, CANARY] {
            let fixture = Fixture::new();
            fixture.plugin("event-must-not-dispatch", &json!({"PreToolUse": {}}));
            let mut input: Value = serde_json::from_str(&fixture.input(agent)).unwrap();
            input["hook_event_name"] = json!(label);
            let output = fixture.hook(agent, &input.to_string());
            assert_native_denial(agent, &output);
            fixture.assert_private_diagnostics(&output);
            assert!(!fixture.project.join("event-must-not-dispatch-ran").exists());
        }
    }
}

#[tokio::test]
async fn canonical_and_legacy_execution_keep_safe_errors_with_original_parser_sources() {
    let fixture = Fixture::new();
    let sym = fixture.symposium();
    let input = serde_json::to_string(CANARY).unwrap();
    for agent in AGENTS {
        let error = try_execute_hook(&sym, agent, HookEvent::PreToolUse, &input)
            .await
            .unwrap_err();
        assert_private(&error.to_string());
        let mut source = error.source();
        let mut parser = None;
        while let Some(error) = source {
            if let Some(error) = error.downcast_ref::<serde_json::Error>() {
                parser = Some(error);
                break;
            }
            source = error.source();
        }
        assert!(
            parser.unwrap().to_string().contains(CANARY),
            "the retained parser diagnostic must exercise the disclosure hazard"
        );

        let legacy = execute_hook(&sym, agent, HookEvent::PreToolUse, &input)
            .await
            .unwrap_err();
        assert_private(&legacy.to_string());
        assert!(
            legacy.downcast_ref::<HookError>().is_some(),
            "the compatibility adapter must retain the canonical typed error"
        );
    }
}

#[test]
fn untagged_native_denials_in_symposium_format_stop_later_plugins() {
    for agent in [
        HookAgent::Claude,
        HookAgent::Codex,
        HookAgent::Copilot,
        HookAgent::Antigravity,
    ] {
        let fixture = Fixture::new();
        fixture.plugin("a-deny", &denial_json(agent, DENIAL_REASON));
        fixture.plugin(
            "zz-late-rewrite",
            &json!({"PreToolUse": {"updatedInput": {"command": "echo rewritten"}}}),
        );
        let output = fixture.hook(agent, &fixture.input(agent));
        assert_native_denial(agent, &output);
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            denial_json(agent, DENIAL_REASON),
            "{agent:?} must retain the first native denial"
        );
        assert!(fixture.project.join("a-deny-ran").exists());
        assert!(
            !fixture.project.join("zz-late-rewrite-ran").exists(),
            "{agent:?} must not run later hooks after an untagged native denial"
        );
        fixture.assert_private_diagnostics(&output);
    }
}

#[test]
fn invalid_canonical_outputs_deny_instead_of_falling_back_to_native_parsers() {
    let outputs = [
        json!({"PreToolUse": {"decision": CANARY}}),
        json!({"PostToolUse": {}}),
    ];
    for agent in AGENTS {
        for response in &outputs {
            let fixture = Fixture::new();
            fixture.plugin("a-invalid", response);
            fixture.plugin("zz-late", &json!({"PreToolUse": {}}));
            let output = fixture.hook(agent, &fixture.input(agent));
            assert_native_denial(agent, &output);
            assert!(fixture.project.join("a-invalid-ran").exists());
            assert!(
                !fixture.project.join("zz-late-ran").exists(),
                "{agent:?} must not run later hooks after invalid canonical output"
            );
            fixture.assert_private_diagnostics(&output);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_output_is_drained_while_large_input_is_still_being_written() {
    const STREAM_BYTES: usize = 2_097_152;

    let fixture = Fixture::new();
    fixture.plugin_script(
        "pipe-pressure",
        "#!/bin/sh\n\
         printf '%s' '{\"PreToolUse\":{\"additionalContext\":\"'\n\
         printf '%2097152s' '' | tr ' ' x\n\
         printf '%s' '\"}}'\n\
         printf '%2097152s' '' >&2\n\
         cat > received.json\n",
    );
    let mut input: Value = serde_json::from_str(&fixture.input(HookAgent::Claude)).unwrap();
    input["tool_input"] = json!({"private": CANARY, "padding": "i".repeat(STREAM_BYTES)});
    let input = input.to_string();
    let expected = agent_event(HookAgent::Claude, HookEvent::PreToolUse)
        .unwrap()
        .try_parse_input(&input)
        .unwrap()
        .to_symposium();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_cargo-agents"))
        .args(["hook", "claude", "pre-tool-use"])
        .env("SYMPOSIUM_HOME", &fixture.home)
        .env("SYMPOSIUM_HOOK_TRACE", &fixture.trace)
        .env_remove("SYMPOSIUM_ENABLE_AGENT_TESTING")
        .env_remove("SYMPOSIUM_TEST_AGENT")
        .current_dir(&fixture.project)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let io = async move {
        let (written, output) = tokio::join!(
            async move {
                let written = stdin.write_all(input.as_bytes()).await;
                drop(stdin);
                written
            },
            child.wait_with_output(),
        );
        written.unwrap();
        output.unwrap()
    };
    let output = tokio::time::timeout(Duration::from_secs(20), io)
        .await
        .expect("hook must not deadlock when plugin output fills before it reads input");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    let context = response["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(context.len(), STREAM_BYTES);
    assert!(context.bytes().all(|byte| byte == b'x'));
    let received: Value =
        serde_json::from_slice(&std::fs::read(fixture.project.join("received.json")).unwrap())
            .unwrap();
    assert!(
        received == serde_json::to_value(expected).unwrap(),
        "plugin must receive the complete canonical input"
    );
    fixture.assert_private_diagnostics(&output);
}
