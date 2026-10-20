//! Typed hook boundaries preserve native protocols and existing implementers.

use std::any::Any;
use std::error::Error;

use serde_json::{Value, json};
use symposium::hook_schema::{
    AgentHookEvent, AgentHookInput, AgentHookOutput, ErasedAgentHookEvent, HookAgent, HookEvent,
    HookResponse, HookSchemaError, agent_event, erase_agent_hook_event,
    symposium::{InputEvent, OutputEvent},
};

fn input_for(event: HookEvent) -> InputEvent {
    use symposium_sdk::hook::*;
    match event {
        HookEvent::PreToolUse => Input::PreToolUse(PreToolUseInput::new(
            "Bash".into(),
            json!({"command": "example"}),
            None,
            None,
        )),
        HookEvent::PostToolUse => Input::PostToolUse(PostToolUseInput::new(
            "Bash".into(),
            json!({}),
            json!({}),
            None,
            None,
        )),
        HookEvent::UserPromptSubmit => {
            Input::UserPromptSubmit(UserPromptSubmitInput::new("example".into(), None, None))
        }
        HookEvent::SessionStart => Input::SessionStart(SessionStartInput::new(None, None)),
        HookEvent::Stop => Input::Stop(StopInput::new(None, None)),
        _ => panic!("unsupported fixture event"),
    }
}

#[test]
fn typed_native_boundaries_match_legacy_responses_and_reject_wrong_events() {
    let agents = [
        HookAgent::Antigravity,
        HookAgent::Claude,
        HookAgent::Codex,
        HookAgent::Copilot,
        HookAgent::Kiro,
        HookAgent::Goose,
        HookAgent::OpenCode,
    ];
    let events = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::UserPromptSubmit,
        HookEvent::SessionStart,
        HookEvent::Stop,
    ];
    let mut supported = 0;
    for agent in agents {
        for event in events {
            let Some(handler) = agent_event(agent, event) else {
                continue;
            };
            supported += 1;
            let input = input_for(event);
            let typed = handler.try_translate_input(&input).unwrap();
            assert_eq!(
                typed.try_to_string().unwrap(),
                handler.translate_input(&input).to_string().unwrap()
            );
            let parsed = handler
                .try_parse_input(&typed.try_to_string().unwrap())
                .unwrap();
            assert_eq!(parsed.to_symposium().event(), event);

            for output in [
                OutputEvent::empty_for(event),
                OutputEvent::with_context(event, "context".into()),
            ] {
                let typed_output = handler
                    .try_translate_output(&output)
                    .unwrap()
                    .try_to_hook_output()
                    .unwrap();
                let old_output = handler.translate_output(&output).to_hook_output();
                assert_eq!(typed_output, old_output, "{agent:?} {event:?}");
                let response = handler.try_respond(&typed_output).unwrap();
                assert_eq!(
                    response,
                    handler.respond(&old_output),
                    "{agent:?} {event:?}"
                );
                let parsed = handler.try_parse_output(&response.stdout).unwrap();
                assert_eq!(
                    parsed.try_to_hook_output().unwrap(),
                    handler
                        .parse_output(&response.stdout)
                        .unwrap()
                        .to_hook_output()
                );
            }

            let other = if event == HookEvent::Stop {
                HookEvent::SessionStart
            } else {
                HookEvent::Stop
            };
            assert!(
                matches!(handler.try_translate_input(&input_for(other)), Err(HookSchemaError::EventMismatch { expected, actual }) if expected == event && actual == Some(other))
            );
            assert!(
                matches!(handler.try_translate_output(&OutputEvent::empty_for(other)), Err(HookSchemaError::EventMismatch { expected, actual }) if expected == event && actual == Some(other))
            );
        }
    }
    assert_eq!(supported, 22);
}

#[test]
fn parse_errors_preserve_the_source_without_echoing_payloads() {
    const CANARY: &str = "UNTRUSTED_SCHEMA_CANARY";
    let input = format!("{{\"{CANARY}\":{{}}}}");
    let error = InputEvent::try_parse_input(&input).unwrap_err();
    assert!(matches!(error, HookSchemaError::InputParse(_)));
    assert!(!error.to_string().contains(CANARY));
    assert!(error.source().unwrap().to_string().contains(CANARY));

    for agent in [
        HookAgent::Antigravity,
        HookAgent::Claude,
        HookAgent::Codex,
        HookAgent::Copilot,
    ] {
        let handler = agent_event(agent, HookEvent::PreToolUse).unwrap();
        let error = handler.try_parse_output(b"{bad JSON").unwrap_err();
        assert!(matches!(error, HookSchemaError::OutputParse(_)));
        assert!(error.source().is_some());
        assert_eq!(error.to_string(), "failed to parse hook output");
    }
}

#[test]
fn native_discriminators_cannot_change_the_selected_event() {
    for agent in [HookAgent::Claude, HookAgent::Codex, HookAgent::Kiro] {
        let handler = agent_event(agent, HookEvent::PreToolUse).unwrap();
        let input = handler
            .try_translate_input(&input_for(HookEvent::PreToolUse))
            .unwrap();
        let mut payload: Value = serde_json::from_str(&input.try_to_string().unwrap()).unwrap();
        payload["hook_event_name"] = Value::String(
            if agent == HookAgent::Kiro {
                "postToolUse"
            } else {
                "PostToolUse"
            }
            .into(),
        );
        assert!(matches!(
            handler.try_parse_input(&payload.to_string()),
            Err(HookSchemaError::EventMismatch {
                expected: HookEvent::PreToolUse,
                actual: Some(HookEvent::PostToolUse)
            })
        ));
    }
    for agent in [HookAgent::Claude, HookAgent::Codex] {
        let handler = agent_event(agent, HookEvent::PreToolUse).unwrap();
        let mut output = handler
            .try_translate_output(&OutputEvent::with_context(
                HookEvent::PreToolUse,
                "context".into(),
            ))
            .unwrap()
            .try_to_hook_output()
            .unwrap();
        output["hookSpecificOutput"]["hookEventName"] = Value::String("Stop".into());
        assert!(matches!(
            handler.try_parse_output(&serde_json::to_vec(&output).unwrap()),
            Err(HookSchemaError::EventMismatch {
                expected: HookEvent::PreToolUse,
                actual: Some(HookEvent::Stop)
            })
        ));
    }
}

#[test]
fn native_decisions_validate_literals_without_dropping_supported_forms() {
    let cases: [(HookAgent, Value, &str, &[&str]); 5] = [
        (
            HookAgent::Claude,
            json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": null}, "vendorExtension": 7}),
            "/hookSpecificOutput/permissionDecision",
            &["allow", "deny", "ask", "defer"],
        ),
        (
            HookAgent::Codex,
            json!({"hookSpecificOutput": {"permissionDecision": null}, "vendorExtension": 7}),
            "/hookSpecificOutput/permissionDecision",
            &["allow", "deny"],
        ),
        (
            HookAgent::Codex,
            json!({"decision": null, "vendorExtension": 7}),
            "/decision",
            &["allow", "block", "deny"],
        ),
        (
            HookAgent::Copilot,
            json!({"permissionDecision": null, "vendorExtension": 7}),
            "/permissionDecision",
            &["allow", "deny", "ask"],
        ),
        (
            HookAgent::Antigravity,
            json!({"decision": null}),
            "/decision",
            &[
                "allow",
                "deny",
                "ask",
                "force_ask",
                "deny_unless_prior_grant",
            ],
        ),
    ];
    for (agent, template, field, allowed) in cases {
        let handler = agent_event(agent, HookEvent::PreToolUse).unwrap();
        for literal in allowed {
            let mut payload = template.clone();
            *payload.pointer_mut(field).unwrap() = Value::String((*literal).into());
            let parsed = handler
                .try_parse_output(&serde_json::to_vec(&payload).unwrap())
                .unwrap();
            let encoded = parsed.try_to_hook_output().unwrap();
            assert_eq!(encoded.pointer(field), payload.pointer(field), "{agent:?}");
            if agent != HookAgent::Antigravity {
                assert_eq!(encoded["vendorExtension"], 7);
            }
        }
        for invalid in ["", "PRIVATE_INVALID_DECISION_CANARY"] {
            let mut payload = template.clone();
            *payload.pointer_mut(field).unwrap() = Value::String(invalid.into());
            let error = handler
                .try_parse_output(&serde_json::to_vec(&payload).unwrap())
                .unwrap_err();
            assert!(
                matches!(error, HookSchemaError::InvalidDecision { .. }),
                "{agent:?}"
            );
            assert!(
                !error
                    .to_string()
                    .contains("PRIVATE_INVALID_DECISION_CANARY")
            );
        }
    }
    for agent in [HookAgent::Claude, HookAgent::Codex, HookAgent::Copilot] {
        let handler = agent_event(agent, HookEvent::PreToolUse).unwrap();
        let value = json!({"vendorExtension": {"new": true}});
        assert_eq!(
            handler
                .try_parse_output(&serde_json::to_vec(&value).unwrap())
                .unwrap()
                .try_to_hook_output()
                .unwrap(),
            value
        );
    }
}

// These types intentionally implement no serialization traits or added methods.
// Existing external implementers must keep compiling without new bounds.
#[derive(Debug)]
struct LegacyInput;
impl AgentHookInput for LegacyInput {
    fn parse_input(_: &str) -> anyhow::Result<Self> {
        Ok(Self)
    }
    fn to_symposium(&self) -> InputEvent {
        input_for(HookEvent::Stop)
    }
    fn from_symposium(_: &InputEvent) -> Self {
        Self
    }
    fn to_string(&self) -> anyhow::Result<String> {
        Ok("legacy-input".into())
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[derive(Debug)]
struct LegacyOutput;
impl AgentHookOutput for LegacyOutput {
    fn parse_output(_: &[u8]) -> anyhow::Result<Self> {
        Ok(Self)
    }
    fn from_symposium(_: &OutputEvent) -> Self {
        Self
    }
    fn to_symposium(&self) -> OutputEvent {
        OutputEvent::empty_for(HookEvent::Stop)
    }
    fn to_hook_output(&self) -> Value {
        json!({"legacy": true})
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

struct LegacyEvent;
impl AgentHookEvent for LegacyEvent {
    type Input = LegacyInput;
    type Output = LegacyOutput;
    fn parse_input(&self, _: &str) -> anyhow::Result<Self::Input> {
        Err(std::io::Error::other("legacy parser marker").into())
    }
    fn parse_output(&self, _: &[u8]) -> anyhow::Result<Self::Output> {
        Err(std::io::Error::other("legacy output marker").into())
    }
    fn respond(&self, _: &Value) -> HookResponse {
        HookResponse::blocked("legacy responder")
    }
}

struct LegacyErased;
impl ErasedAgentHookEvent for LegacyErased {
    fn parse_input(&self, _: &str) -> anyhow::Result<Box<dyn AgentHookInput>> {
        Ok(Box::new(LegacyInput))
    }
    fn parse_output(&self, _: &[u8]) -> anyhow::Result<Box<dyn AgentHookOutput>> {
        Ok(Box::new(LegacyOutput))
    }
    fn translate_input(&self, _: &InputEvent) -> Box<dyn AgentHookInput> {
        Box::new(LegacyInput)
    }
    fn translate_output(&self, _: &OutputEvent) -> Box<dyn AgentHookOutput> {
        Box::new(LegacyOutput)
    }
    fn respond(&self, _: &Value) -> HookResponse {
        HookResponse::blocked("erased responder")
    }
}

#[test]
fn legacy_trait_implementations_keep_their_custom_behavior() {
    assert_eq!(LegacyInput.try_to_string().unwrap(), "legacy-input");
    assert_eq!(
        LegacyOutput.try_to_hook_output().unwrap(),
        json!({"legacy": true})
    );
    let erased = erase_agent_hook_event(LegacyEvent);
    let error = erased.try_parse_input("ignored").unwrap_err();
    assert!(matches!(error, HookSchemaError::LegacyAdapter { .. }));
    assert_eq!(error.to_string(), "legacy hook adapter failed");
    assert_eq!(error.source().unwrap().to_string(), "legacy parser marker");
    assert_eq!(
        erased
            .try_parse_output(b"ignored")
            .unwrap_err()
            .source()
            .unwrap()
            .to_string(),
        "legacy output marker"
    );
    assert_eq!(
        erased.try_respond(&json!({})).unwrap(),
        HookResponse::blocked("legacy responder")
    );

    let direct: &dyn ErasedAgentHookEvent = &LegacyErased;
    assert_eq!(
        direct
            .try_parse_input("ignored")
            .unwrap()
            .try_to_string()
            .unwrap(),
        "legacy-input"
    );
    assert_eq!(
        direct
            .try_parse_output(b"ignored")
            .unwrap()
            .try_to_hook_output()
            .unwrap(),
        json!({"legacy": true})
    );
    assert!(
        direct
            .try_translate_input(&input_for(HookEvent::Stop))
            .is_ok()
    );
    assert!(
        direct
            .try_translate_output(&OutputEvent::empty_for(HookEvent::Stop))
            .is_ok()
    );
    assert_eq!(
        direct.try_respond(&json!({})).unwrap(),
        HookResponse::blocked("erased responder")
    );
}

#[test]
fn typed_responses_keep_native_denial_and_plain_text_rules() {
    let deny = OutputEvent::PreToolUse(symposium_sdk::hook::PreToolUseOutput::deny("reason"));
    for agent in [
        HookAgent::Antigravity,
        HookAgent::Claude,
        HookAgent::Codex,
        HookAgent::Copilot,
        HookAgent::Kiro,
    ] {
        let handler = agent_event(agent, HookEvent::PreToolUse).unwrap();
        let value = handler
            .try_translate_output(&deny)
            .unwrap()
            .try_to_hook_output()
            .unwrap();
        let response = handler.try_respond(&value).unwrap();
        assert_eq!(response, handler.respond(&value));
        if agent == HookAgent::Kiro {
            assert_eq!(response, HookResponse::blocked("reason"));
        } else {
            assert_eq!(response.exit_code, 0);
            assert!(!response.stdout.is_empty());
        }
    }
    let kiro = agent_event(HookAgent::Kiro, HookEvent::SessionStart).unwrap();
    let parsed = kiro.try_parse_output(b"plain\xfftext").unwrap();
    assert_eq!(
        parsed.try_to_hook_output().unwrap()["additionalContext"],
        "plain\u{fffd}text"
    );
    let antigravity = agent_event(HookAgent::Antigravity, HookEvent::PreToolUse).unwrap();
    let neutral = antigravity
        .try_translate_output(&OutputEvent::empty_for(HookEvent::PreToolUse))
        .unwrap()
        .try_to_hook_output()
        .unwrap();
    assert_eq!(neutral, json!({"decision": "allow"}));
}
