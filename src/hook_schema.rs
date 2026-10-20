//! Hook payload types for communication between editor plugins and Symposium.

use anyhow::Result;
use std::{any::Any, fmt::Debug};

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod goose;
pub mod kiro;
pub mod opencode;
pub mod symposium;

/// [`HookAgent`] and [`HookEvent`] are the SDK's: a plugin manifest names both
/// (`[[hooks]] agent = "claude"`, `event = "PreToolUse"`), and manifests cross
/// the package-manager boundary. Only the dispatch below: mapping an agent to
/// its wire-format handler: needs Symposium's agent modules, so it is a free
/// function rather than a method.
pub use symposium_sdk::hook::{HookAgent, HookEvent};

/// A hook protocol failure whose diagnostic never includes payload contents.
///
/// Original parser and adapter errors remain available through
/// [`std::error::Error::source`]. Do not log those sources or this type's debug
/// representation: parser diagnostics can contain untrusted input.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HookSchemaError {
    /// Incoming agent JSON could not be parsed.
    #[error("failed to parse hook input")]
    InputParse(#[source] serde_json::Error),
    /// A plugin's JSON output could not be parsed.
    #[error("failed to parse hook output")]
    OutputParse(#[source] serde_json::Error),
    /// Input could not be encoded for a plugin.
    #[error("failed to serialize hook input")]
    InputSerialization(#[source] serde_json::Error),
    /// An output value could not be encoded for the agent.
    #[error("failed to serialize hook output")]
    OutputSerialization(#[source] serde_json::Error),
    /// Canonical input or output does not belong to the selected event.
    #[error("hook event mismatch: expected {expected:?}, received {actual:?}")]
    EventMismatch {
        /// The event owned by the selected handler.
        expected: HookEvent,
        /// The received event, or `None` for an unsupported native/SDK variant.
        actual: Option<HookEvent>,
    },
    /// A native access-control field contains an unsupported decision literal.
    #[error("invalid hook decision at {field}")]
    InvalidDecision {
        /// A fixed schema field name; the untrusted value is deliberately omitted.
        field: &'static str,
    },
    /// A supported legacy implementation returned an error.
    #[error("legacy hook adapter failed")]
    LegacyAdapter {
        /// The original error, preserved without formatting its payload.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl HookSchemaError {
    fn legacy(source: anyhow::Error) -> Self {
        Self::LegacyAdapter {
            source: source.reallocate_into_boxed_dyn_error_without_backtrace(),
        }
    }
}

fn validate_input_event(
    input: &symposium::InputEvent,
    expected: HookEvent,
) -> Result<(), HookSchemaError> {
    let actual = input.event();
    if actual == expected {
        Ok(())
    } else {
        Err(HookSchemaError::EventMismatch {
            expected,
            actual: Some(actual),
        })
    }
}

fn validate_output_event(
    output: &symposium::OutputEvent,
    expected: HookEvent,
) -> Result<(), HookSchemaError> {
    let actual = match output {
        symposium::OutputEvent::PreToolUse(_) => Some(HookEvent::PreToolUse),
        symposium::OutputEvent::PostToolUse(_) => Some(HookEvent::PostToolUse),
        symposium::OutputEvent::UserPromptSubmit(_) => Some(HookEvent::UserPromptSubmit),
        symposium::OutputEvent::SessionStart(_) => Some(HookEvent::SessionStart),
        symposium::OutputEvent::Stop(_) => Some(HookEvent::Stop),
        _ => None,
    };
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(HookSchemaError::EventMismatch { expected, actual })
    }
}

fn validate_native_event(
    name: &str,
    expected: HookEvent,
    expected_name: &str,
) -> Result<(), HookSchemaError> {
    if name == expected_name {
        return Ok(());
    }
    let actual = match name {
        "PreToolUse" | "preToolUse" => Some(HookEvent::PreToolUse),
        "PostToolUse" | "postToolUse" => Some(HookEvent::PostToolUse),
        "UserPromptSubmit" | "userPromptSubmit" => Some(HookEvent::UserPromptSubmit),
        "SessionStart" | "agentSpawn" => Some(HookEvent::SessionStart),
        "Stop" => Some(HookEvent::Stop),
        _ => None,
    };
    Err(HookSchemaError::EventMismatch { expected, actual })
}

fn validate_decision(
    decision: Option<&str>,
    allowed: &[&str],
    field: &'static str,
) -> Result<(), HookSchemaError> {
    if decision.is_none_or(|decision| allowed.contains(&decision)) {
        Ok(())
    } else {
        Err(HookSchemaError::InvalidDecision { field })
    }
}

/// The handler that speaks `agent`'s wire format for `event`, if it has one.
pub fn agent_event(agent: HookAgent, event: HookEvent) -> Option<Box<dyn ErasedAgentHookEvent>> {
    match agent {
        HookAgent::Antigravity => antigravity::Antigravity.event(event),
        HookAgent::Claude => claude::ClaudeCode.event(event),
        HookAgent::Codex => codex::Codex.event(event),
        HookAgent::Copilot => copilot::Copilot.event(event),
        HookAgent::Goose => goose::Goose.event(event),
        HookAgent::Kiro => kiro::Kiro.event(event),
        HookAgent::OpenCode => opencode::OpenCode.event(event),
    }
}

/// Represents the data sent *from* an agent *to* a hook.
pub trait AgentHookInput: Debug {
    /// Parse input through the typed boundary.
    ///
    /// The default supports legacy implementations until root 0.6. Built-in
    /// implementations preserve concrete JSON errors without echoing payloads.
    fn try_parse_input(payload: &str) -> Result<Self, HookSchemaError>
    where
        Self: Sized,
    {
        Self::parse_input(payload).map_err(HookSchemaError::legacy)
    }
    /// Parse an incoming JSON payload string into a concrete payload struct.
    fn parse_input(payload: &str) -> Result<Self>
    where
        Self: Sized;
    /// Convert this payload into the canonical symposium input event.
    fn to_symposium(&self) -> symposium::InputEvent;
    /// Convert a canonical symposium input event into this agent's payload.
    fn from_symposium(event: &symposium::InputEvent) -> Self
    where
        Self: Sized;
    /// Convert this payload into a JSON string for forwarding to plugins.
    fn to_string(&self) -> Result<String>;

    /// Encode input with a typed error. Legacy implementations use the default
    /// adapter; built-in implementations encode directly without assertions.
    fn try_to_string(&self) -> Result<String, HookSchemaError> {
        self.to_string().map_err(HookSchemaError::legacy)
    }

    /// Whether this payload is an occurrence of the symposium event it maps
    /// to. An agent's native event can be broader than the canonical one
    /// (Antigravity's `PreInvocation` fires before every model call but stands
    /// in for `user-prompt-submit`), and a payload that is not such an
    /// occurrence is answered with the agent's neutral output and dispatched
    /// to nothing.
    fn should_dispatch(&self) -> bool {
        true
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

/// Represents the data sent *from* a hook *to* an agent.
pub trait AgentHookOutput: Debug {
    /// Parse plugin output through the typed boundary, preserving its source.
    /// The default supports legacy implementations until root 0.6.
    fn try_parse_output(output: &[u8]) -> Result<Self, HookSchemaError>
    where
        Self: Sized,
    {
        Self::parse_output(output).map_err(HookSchemaError::legacy)
    }
    /// Parse raw stdout bytes from a hook handler into a concrete output struct.
    fn parse_output(output: &[u8]) -> anyhow::Result<Self>
    where
        Self: Sized;
    /// Convert a canonical symposium output event into this agent's output.
    fn from_symposium(event: &symposium::OutputEvent) -> Self
    where
        Self: Sized;
    /// Convert this agent's output into a canonical symposium output event.
    fn to_symposium(&self) -> symposium::OutputEvent;
    /// Convert this output into a JSON value to return to the agent.
    fn to_hook_output(&self) -> serde_json::Value;

    /// Encode agent output without assuming that serialization succeeds.
    /// The default supports legacy implementations until root 0.6.
    fn try_to_hook_output(&self) -> Result<serde_json::Value, HookSchemaError> {
        Ok(self.to_hook_output())
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

/// Represents the "handler" for a specific kind of hook event
/// (e.g., a `PreToolUse` event coming from `claude`),
/// capable of parsing the native payloads and outputs
/// as well as converting from symposium types.
pub trait AgentHookEvent {
    type Input: AgentHookInput;
    type Output: AgentHookOutput;

    /// Parse input using the legacy event parser unless a typed implementation
    /// is provided. This preserves custom event-level parsing overrides.
    fn try_parse_input(&self, payload: &str) -> Result<Self::Input, HookSchemaError> {
        self.parse_input(payload).map_err(HookSchemaError::legacy)
    }

    /// Parse output using the legacy event parser unless a typed implementation
    /// is provided. This preserves custom event-level parsing overrides.
    fn try_parse_output(&self, output: &[u8]) -> Result<Self::Output, HookSchemaError> {
        self.parse_output(output).map_err(HookSchemaError::legacy)
    }

    /// Translate canonical input after checking that it belongs to this event.
    /// Built-ins validate the event; the default retains external legacy behavior.
    fn try_translate_input(
        &self,
        input: &symposium::InputEvent,
    ) -> Result<Self::Input, HookSchemaError> {
        Ok(Self::Input::from_symposium(input))
    }

    /// Translate canonical output after checking that it belongs to this event.
    /// Built-ins validate the event; the default retains external legacy behavior.
    fn try_translate_output(
        &self,
        output: &symposium::OutputEvent,
    ) -> Result<Self::Output, HookSchemaError> {
        Ok(self.translate_output(output))
    }

    /// Render a response through the typed boundary. Built-ins preserve their
    /// native protocol; the default retains custom legacy responders until root 0.6.
    fn try_respond(&self, output: &serde_json::Value) -> Result<HookResponse, HookSchemaError> {
        Ok(self.respond(output))
    }

    /// Parse an incoming JSON payload string into a concrete payload struct.
    fn parse_input(&self, payload: &str) -> anyhow::Result<Self::Input> {
        Self::Input::parse_input(payload)
    }
    /// Parse raw stdout bytes from a hook handler into a concrete output struct.
    fn parse_output(&self, output: &[u8]) -> anyhow::Result<Self::Output> {
        Self::Output::parse_output(output)
    }
    /// Convert a canonical symposium output event into this agent's output.
    fn translate_output(&self, output: &symposium::OutputEvent) -> Self::Output {
        Self::Output::from_symposium(output)
    }

    /// Serialize the final output Value to bytes for stdout.
    /// Default: JSON. Override for agents with different output formats (e.g., Kiro plain text).
    fn serialize_output(&self, output: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(output).unwrap()
    }

    /// The response the agent receives for the final output Value.
    /// Default: its serialization on stdout, exit 0. Override for agents that
    /// read part of the output from the exit code instead (e.g., Kiro, which
    /// denies a tool call only through exit 2).
    fn respond(&self, output: &serde_json::Value) -> HookResponse {
        HookResponse::success(self.serialize_output(output))
    }
}

/// What `cargo agents hook` hands back to the agent. Most agents read the
/// result from stdout, but blocking can hinge on the exit code, so the
/// response carries all three.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookResponse {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: u8,
}

impl HookResponse {
    /// Exit 0 with `stdout` for the agent to parse.
    pub fn success(stdout: Vec<u8>) -> Self {
        Self {
            stdout,
            ..Self::default()
        }
    }

    /// Exit 2 with `reason` on stderr, which every agent that honors exit
    /// codes reads as "blocked, and here is why".
    pub fn blocked(reason: &str) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: reason.as_bytes().to_vec(),
            exit_code: 2,
        }
    }
}

/// Represents an agent that can handle hook events.
pub trait Agent {
    fn event(&self, event: HookEvent) -> Option<Box<dyn ErasedAgentHookEvent>>;
}

/// Erased version of [`AgentHookEvent`]. Represents the "handler" for a
/// specific kind of hook event (e.g., a `PreToolUse` event coming from `claude`),
/// capable of parsing the native payloads and outputs as well as
/// converting from symposium types.
pub trait ErasedAgentHookEvent {
    /// Parse input with a typed failure; defaults preserve existing implementers.
    fn try_parse_input(&self, payload: &str) -> Result<Box<dyn AgentHookInput>, HookSchemaError> {
        self.parse_input(payload).map_err(HookSchemaError::legacy)
    }

    /// Parse output with a typed failure; defaults preserve existing implementers.
    fn try_parse_output(&self, output: &[u8]) -> Result<Box<dyn AgentHookOutput>, HookSchemaError> {
        self.parse_output(output).map_err(HookSchemaError::legacy)
    }

    /// Translate canonical input, rejecting mismatches in built-in handlers.
    fn try_translate_input(
        &self,
        input: &symposium::InputEvent,
    ) -> Result<Box<dyn AgentHookInput>, HookSchemaError> {
        Ok(self.translate_input(input))
    }

    /// Translate canonical output, rejecting mismatches in built-in handlers.
    fn try_translate_output(
        &self,
        output: &symposium::OutputEvent,
    ) -> Result<Box<dyn AgentHookOutput>, HookSchemaError> {
        Ok(self.translate_output(output))
    }

    /// Render a native response with a typed failure.
    fn try_respond(&self, output: &serde_json::Value) -> Result<HookResponse, HookSchemaError> {
        Ok(self.respond(output))
    }
    /// Parse an incoming JSON payload into a boxed `AgentHookPayload`.
    fn parse_input(&self, payload: &str) -> Result<Box<dyn AgentHookInput>>;

    /// Parses a stdout into a boxed `AgentHookOutput`.
    fn parse_output(&self, output: &[u8]) -> Result<Box<dyn AgentHookOutput>>;

    /// Convert a canonical symposium output event into a boxed agent output.
    fn translate_output(&self, output: &symposium::OutputEvent) -> Box<dyn AgentHookOutput>;

    /// Convert a canonical symposium input event into a boxed agent payload.
    fn translate_input(&self, input: &symposium::InputEvent) -> Box<dyn AgentHookInput>;

    /// The response for the final accumulated output (as JSON Value).
    /// Most agents take JSON on stdout; Kiro takes plain text, and exit 2 to deny.
    fn respond(&self, output: &serde_json::Value) -> HookResponse;
}

struct ErasedAgentHookEventImpl<E: AgentHookEvent + 'static>(E);

impl<E> ErasedAgentHookEvent for ErasedAgentHookEventImpl<E>
where
    E: AgentHookEvent + 'static,
    E::Input: AgentHookInput + 'static,
    E::Output: AgentHookOutput + 'static,
{
    fn try_parse_input(&self, payload: &str) -> Result<Box<dyn AgentHookInput>, HookSchemaError> {
        Ok(Box::new(self.0.try_parse_input(payload)?))
    }

    fn try_parse_output(&self, output: &[u8]) -> Result<Box<dyn AgentHookOutput>, HookSchemaError> {
        Ok(Box::new(self.0.try_parse_output(output)?))
    }

    fn try_translate_input(
        &self,
        input: &symposium::InputEvent,
    ) -> Result<Box<dyn AgentHookInput>, HookSchemaError> {
        Ok(Box::new(self.0.try_translate_input(input)?))
    }

    fn try_translate_output(
        &self,
        output: &symposium::OutputEvent,
    ) -> Result<Box<dyn AgentHookOutput>, HookSchemaError> {
        Ok(Box::new(self.0.try_translate_output(output)?))
    }

    fn try_respond(&self, output: &serde_json::Value) -> Result<HookResponse, HookSchemaError> {
        self.0.try_respond(output)
    }

    fn parse_input(&self, payload: &str) -> Result<Box<dyn AgentHookInput>> {
        let p = self.0.parse_input(payload)?;
        Ok(Box::new(p))
    }

    fn parse_output(&self, output: &[u8]) -> Result<Box<dyn AgentHookOutput>> {
        let o = self.0.parse_output(output)?;
        Ok(Box::new(o))
    }
    fn translate_output(&self, output: &symposium::OutputEvent) -> Box<dyn AgentHookOutput> {
        Box::new(self.0.translate_output(output))
    }

    fn translate_input(&self, input: &symposium::InputEvent) -> Box<dyn AgentHookInput> {
        Box::new(E::Input::from_symposium(input))
    }

    fn respond(&self, output: &serde_json::Value) -> HookResponse {
        self.0.respond(output)
    }
}

/// Helper to erase a concrete `AgentHookEvent` into a trait object.
pub fn erase_agent_hook_event<E>(e: E) -> Box<dyn ErasedAgentHookEvent>
where
    E: AgentHookEvent + 'static,
    E::Input: AgentHookInput + 'static,
    E::Output: AgentHookOutput + 'static,
{
    Box::new(ErasedAgentHookEventImpl(e))
}
