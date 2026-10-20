//! SDK for writing symposium hook handlers in Rust.
//!
//! A symposium hook is a program that reads a JSON event on stdin and writes a
//! JSON response to stdout. This crate provides the types and a [`try_run`] harness
//! so you can focus on the logic.
//!
//! # Example
//!
//! ```no_run
//! use std::convert::Infallible;
//! use std::io::Write as _;
//! use std::process::ExitCode;
//! use symposium_sdk::hook::{HookHandler, PreToolUseInput, PreToolUseOutput, try_run};
//!
//! struct MyHook;
//!
//! impl HookHandler<Infallible> for MyHook {
//!     async fn pre_tool_use(&self, event: &PreToolUseInput) -> Result<PreToolUseOutput, Infallible> {
//!         if event.tool_name == "Bash" {
//!             Ok(PreToolUseOutput::context("Remember: prefer non-destructive commands"))
//!         } else {
//!             Ok(PreToolUseOutput::default())
//!         }
//!     }
//! }
//!
//! fn main() -> ExitCode {
//!     match try_run(MyHook) {
//!         Ok(exit) => exit,
//!         Err(error) => {
//!             if writeln!(std::io::stderr().lock(), "symposium-hook: {error}").is_err() {
//!                 return ExitCode::FAILURE;
//!             }
//!             ExitCode::FAILURE
//!         }
//!     }
//! }
//! ```

pub use anyhow;

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::process::ExitCode;

// ── Event types ─────────────────────────────────────────────────────────

/// Agents supported by Symposium hooks.
///
/// Lives here rather than in Symposium because a plugin manifest names an
/// agent (`[[hooks]] agent = "claude"`), and manifests cross the package-manager
/// boundary. Dispatching an event to an agent's wire format stays in Symposium.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum HookAgent {
    #[cfg_attr(feature = "clap", value(name = "antigravity"))]
    #[serde(rename = "antigravity")]
    Antigravity,
    #[cfg_attr(feature = "clap", value(name = "claude"))]
    #[serde(rename = "claude")]
    Claude,
    #[cfg_attr(feature = "clap", value(name = "codex"))]
    #[serde(rename = "codex")]
    Codex,
    #[cfg_attr(feature = "clap", value(name = "copilot"))]
    #[serde(rename = "copilot")]
    Copilot,
    #[cfg_attr(feature = "clap", value(name = "goose"))]
    #[serde(rename = "goose")]
    Goose,
    #[cfg_attr(feature = "clap", value(name = "kiro"))]
    #[serde(rename = "kiro")]
    Kiro,
    #[cfg_attr(feature = "clap", value(name = "opencode"))]
    #[serde(rename = "opencode")]
    OpenCode,
}

impl HookAgent {
    /// Canonical lowercase agent name (matches the config `[[agent]]` names).
    pub fn as_str(&self) -> &'static str {
        match self {
            HookAgent::Antigravity => "antigravity",
            HookAgent::Claude => "claude",
            HookAgent::Codex => "codex",
            HookAgent::Copilot => "copilot",
            HookAgent::Goose => "goose",
            HookAgent::Kiro => "kiro",
            HookAgent::OpenCode => "opencode",
        }
    }
}

/// Hook event types supported by Symposium.
#[non_exhaustive]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum HookEvent {
    #[cfg_attr(feature = "clap", value(name = "pre-tool-use"))]
    #[serde(rename = "PreToolUse")]
    PreToolUse,

    #[cfg_attr(feature = "clap", value(name = "post-tool-use"))]
    #[serde(rename = "PostToolUse")]
    PostToolUse,

    #[cfg_attr(feature = "clap", value(name = "user-prompt-submit"))]
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit,

    #[cfg_attr(feature = "clap", value(name = "session-start"))]
    #[serde(rename = "SessionStart")]
    SessionStart,

    #[cfg_attr(feature = "clap", value(name = "stop"))]
    #[serde(rename = "Stop")]
    Stop,
}

// ── Input types ─────────────────────────────────────────────────────────

/// Input event received on stdin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Input {
    PreToolUse(PreToolUseInput),
    PostToolUse(PostToolUseInput),
    UserPromptSubmit(UserPromptSubmitInput),
    SessionStart(SessionStartInput),
    Stop(StopInput),
}

impl Input {
    /// Returns the event type for this input.
    pub fn event(&self) -> HookEvent {
        match self {
            Input::PreToolUse(_) => HookEvent::PreToolUse,
            Input::PostToolUse(_) => HookEvent::PostToolUse,
            Input::UserPromptSubmit(_) => HookEvent::UserPromptSubmit,
            Input::SessionStart(_) => HookEvent::SessionStart,
            Input::Stop(_) => HookEvent::Stop,
        }
    }

    /// Extract the working directory from any event variant.
    pub fn cwd(&self) -> Option<&str> {
        match self {
            Input::PreToolUse(p) => p.cwd.as_deref(),
            Input::PostToolUse(p) => p.cwd.as_deref(),
            Input::UserPromptSubmit(p) => p.cwd.as_deref(),
            Input::SessionStart(p) => p.cwd.as_deref(),
            Input::Stop(p) => p.cwd.as_deref(),
        }
    }

    /// Extract the session id from any event variant.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Input::PreToolUse(p) => p.session_id.as_deref(),
            Input::PostToolUse(p) => p.session_id.as_deref(),
            Input::UserPromptSubmit(p) => p.session_id.as_deref(),
            Input::SessionStart(p) => p.session_id.as_deref(),
            Input::Stop(p) => p.session_id.as_deref(),
        }
    }

    /// Check whether this event matches the given matcher string.
    ///
    /// For tool events (`PreToolUse`, `PostToolUse`), the matcher is a regex
    /// tested against the tool name. For other events, all matchers match.
    /// The wildcard `"*"` matches everything.
    pub fn matches_matcher(&self, matcher: &str) -> bool {
        if matcher == "*" {
            return true;
        }

        let tool_name = match self {
            Input::PreToolUse(p) => &p.tool_name,
            Input::PostToolUse(p) => &p.tool_name,
            Input::UserPromptSubmit(_) | Input::SessionStart(_) | Input::Stop(_) => return true,
        };

        regex::Regex::new(matcher).is_ok_and(|re| re.is_match(tool_name))
    }
}

/// Input for a `PreToolUse` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PreToolUseInput {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl PreToolUseInput {
    pub fn new(
        tool_name: String,
        tool_input: serde_json::Value,
        session_id: Option<String>,
        cwd: Option<String>,
    ) -> Self {
        Self {
            tool_name,
            tool_input,
            session_id,
            cwd,
        }
    }
}

/// Input for a `PostToolUse` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PostToolUseInput {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: serde_json::Value,
    #[serde(default)]
    pub tool_response: serde_json::Value,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl PostToolUseInput {
    pub fn new(
        tool_name: String,
        tool_input: serde_json::Value,
        tool_response: serde_json::Value,
        session_id: Option<String>,
        cwd: Option<String>,
    ) -> Self {
        Self {
            tool_name,
            tool_input,
            tool_response,
            session_id,
            cwd,
        }
    }
}

/// Input for a `UserPromptSubmit` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UserPromptSubmitInput {
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl UserPromptSubmitInput {
    pub fn new(prompt: String, session_id: Option<String>, cwd: Option<String>) -> Self {
        Self {
            prompt,
            session_id,
            cwd,
        }
    }
}

/// Input for a `SessionStart` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionStartInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl SessionStartInput {
    pub fn new(session_id: Option<String>, cwd: Option<String>) -> Self {
        Self { session_id, cwd }
    }
}

/// Input for a `Stop` event — emitted when an agent session/turn ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StopInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl StopInput {
    pub fn new(session_id: Option<String>, cwd: Option<String>) -> Self {
        Self { session_id, cwd }
    }
}

// ── Output types ────────────────────────────────────────────────────────

/// Output event written to stdout.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Output {
    PreToolUse(PreToolUseOutput),
    PostToolUse(PostToolUseOutput),
    UserPromptSubmit(UserPromptSubmitOutput),
    SessionStart(SessionStartOutput),
    Stop(StopOutput),
}

impl Output {
    /// Create an empty output for the given event type.
    pub fn empty_for(event: HookEvent) -> Self {
        match event {
            HookEvent::PreToolUse => Output::PreToolUse(PreToolUseOutput::default()),
            HookEvent::PostToolUse => Output::PostToolUse(PostToolUseOutput::default()),
            HookEvent::UserPromptSubmit => {
                Output::UserPromptSubmit(UserPromptSubmitOutput::default())
            }
            HookEvent::SessionStart => Output::SessionStart(SessionStartOutput::default()),
            HookEvent::Stop => Output::Stop(StopOutput::default()),
        }
    }

    /// Create an output with additional context for the given event type.
    pub fn with_context(event: HookEvent, context: String) -> Self {
        match event {
            HookEvent::PreToolUse => Output::PreToolUse(PreToolUseOutput::context(context)),
            HookEvent::PostToolUse => Output::PostToolUse(PostToolUseOutput::context(context)),
            HookEvent::UserPromptSubmit => {
                Output::UserPromptSubmit(UserPromptSubmitOutput::context(context))
            }
            HookEvent::SessionStart => Output::SessionStart(SessionStartOutput::context(context)),
            HookEvent::Stop => Output::Stop(StopOutput::context(context)),
        }
    }

    /// Extract additional context from any variant.
    pub fn additional_context(&self) -> Option<&str> {
        match self {
            Output::PreToolUse(o) => o.additional_context.as_deref(),
            Output::PostToolUse(o) => o.additional_context.as_deref(),
            Output::UserPromptSubmit(o) => o.additional_context.as_deref(),
            Output::SessionStart(o) => o.additional_context.as_deref(),
            Output::Stop(o) => o.additional_context.as_deref(),
        }
    }
}

/// Output for a `PreToolUse` event.
/// Decision for a `PreToolUse` hook.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Allow the tool call to proceed (default).
    #[default]
    Allow,
    /// Block the tool call.
    Deny,
}

#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreToolUseOutput {
    #[serde(default, skip_serializing_if = "Decision::is_allow")]
    pub decision: Decision,
    #[serde(
        rename = "additionalContext",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
    #[serde(
        rename = "updatedInput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_input: Option<serde_json::Value>,
}

impl Decision {
    fn is_allow(&self) -> bool {
        *self == Decision::Allow
    }
}

impl PreToolUseOutput {
    /// Create an output with all fields specified.
    pub fn new(
        decision: Decision,
        additional_context: Option<String>,
        updated_input: Option<serde_json::Value>,
    ) -> Self {
        Self {
            decision,
            additional_context,
            updated_input,
        }
    }

    /// Create an output that injects additional context.
    pub fn context(text: impl Into<String>) -> Self {
        Self {
            additional_context: Some(text.into()),
            ..Default::default()
        }
    }

    /// Create an output that replaces the tool input.
    pub fn with_updated_input(input: serde_json::Value) -> Self {
        Self {
            updated_input: Some(input),
            ..Default::default()
        }
    }

    /// Deny the tool call with a reason.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            decision: Decision::Deny,
            additional_context: Some(reason.into()),
            ..Default::default()
        }
    }
}

/// Output for a `PostToolUse` event.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostToolUseOutput {
    #[serde(
        rename = "additionalContext",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

impl PostToolUseOutput {
    /// Create an output with all fields specified.
    pub fn new(additional_context: Option<String>) -> Self {
        Self { additional_context }
    }

    /// Create an output that injects additional context.
    pub fn context(text: impl Into<String>) -> Self {
        Self::new(Some(text.into()))
    }
}

/// Output for a `UserPromptSubmit` event.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserPromptSubmitOutput {
    #[serde(
        rename = "additionalContext",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

impl UserPromptSubmitOutput {
    /// Create an output with all fields specified.
    pub fn new(additional_context: Option<String>) -> Self {
        Self { additional_context }
    }

    /// Create an output that injects additional context.
    pub fn context(text: impl Into<String>) -> Self {
        Self::new(Some(text.into()))
    }
}

/// Output for a `SessionStart` event.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStartOutput {
    #[serde(
        rename = "additionalContext",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

impl SessionStartOutput {
    /// Create an output with all fields specified.
    pub fn new(additional_context: Option<String>) -> Self {
        Self { additional_context }
    }

    /// Create an output that injects additional context.
    pub fn context(text: impl Into<String>) -> Self {
        Self::new(Some(text.into()))
    }
}

/// Output for a `Stop` event.
#[non_exhaustive]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StopOutput {
    #[serde(
        rename = "additionalContext",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

impl StopOutput {
    /// Create an output with all fields specified.
    pub fn new(additional_context: Option<String>) -> Self {
        Self { additional_context }
    }

    /// Create an output that injects additional context.
    pub fn context(text: impl Into<String>) -> Self {
        Self::new(Some(text.into()))
    }
}

// ── Handler trait ───────────────────────────────────────────────────────

/// Default dispatch for handlers using the SDK 0.1 `anyhow` contract.
///
/// This supported compatibility entry point retains its single handler type
/// parameter until SDK 0.2. New typed handlers use [`try_default_handle_event`].
///
/// # Errors
///
/// Returns the error from the event-specific handler unchanged.
pub async fn default_handle_event<H>(handler: &H, input: &Input) -> anyhow::Result<Output>
where
    H: HookHandler + ?Sized,
{
    try_default_handle_event(handler, input).await
}

/// Dispatch an event while preserving the handler's concrete error type.
///
/// No thread-safety or lifetime bounds are needed: handlers execute on the
/// calling thread and may borrow state.
///
/// # Errors
///
/// Returns the error from the event-specific handler unchanged.
pub async fn try_default_handle_event<H, E>(handler: &H, input: &Input) -> Result<Output, E>
where
    H: HookHandler<E> + ?Sized,
{
    match input {
        Input::PreToolUse(event) => Ok(Output::PreToolUse(handler.pre_tool_use(event).await?)),
        Input::PostToolUse(event) => Ok(Output::PostToolUse(handler.post_tool_use(event).await?)),
        Input::UserPromptSubmit(event) => Ok(Output::UserPromptSubmit(
            handler.user_prompt_submit(event).await?,
        )),
        Input::SessionStart(event) => Ok(Output::SessionStart(handler.session_start(event).await?)),
        Input::Stop(event) => Ok(Output::Stop(handler.stop(event).await?)),
    }
}

/// Implement the events a hook handles, selecting a concrete error type `E`.
///
/// Defaults return empty outputs. The default `anyhow::Error` parameter retains
/// existing SDK 0.1 implementations; new handlers should select their own error
/// type, or [`std::convert::Infallible`] when their decisions cannot fail.
#[allow(async_fn_in_trait)] // Hook handlers run on a single-threaded runtime; Send is not needed.
pub trait HookHandler<E = anyhow::Error> {
    /// Dispatch an input to its event-specific method, preserving `E`.
    async fn handle_event(&self, input: &Input) -> Result<Output, E> {
        try_default_handle_event(self, input).await
    }

    /// Called before the agent invokes a tool.
    async fn pre_tool_use(&self, _event: &PreToolUseInput) -> Result<PreToolUseOutput, E> {
        Ok(PreToolUseOutput::default())
    }

    /// Called after a tool completes.
    async fn post_tool_use(&self, _event: &PostToolUseInput) -> Result<PostToolUseOutput, E> {
        Ok(PostToolUseOutput::default())
    }

    /// Called when the user submits a prompt.
    async fn user_prompt_submit(
        &self,
        _event: &UserPromptSubmitInput,
    ) -> Result<UserPromptSubmitOutput, E> {
        Ok(UserPromptSubmitOutput::default())
    }

    /// Called when an agent session begins.
    async fn session_start(&self, _event: &SessionStartInput) -> Result<SessionStartOutput, E> {
        Ok(SessionStartOutput::default())
    }

    /// Called when an agent session/turn ends.
    async fn stop(&self, _event: &StopInput) -> Result<StopOutput, E> {
        Ok(StopOutput::default())
    }
}

// ── Harness ─────────────────────────────────────────────────────────────

/// Failure to initialize, execute, or deliver a hook response.
///
/// Display and Debug report only the failing operation. Sources remain available
/// for structured inspection and may contain private input or handler data.
/// The standard error implementation is available when `E` is a standard error;
/// harness execution itself does not require that capability.
#[derive(thiserror::Error)]
#[non_exhaustive]
pub enum HarnessError<E> {
    /// A synchronous harness cannot own a runtime inside an entered runtime.
    #[error("cannot run synchronous hook harness inside an active runtime")]
    RuntimeAlreadyActive,
    /// The current-thread runtime could not be created.
    #[error("could not create hook runtime")]
    Runtime {
        /// The runtime construction failure.
        #[source]
        source: io::Error,
    },
    /// Standard input could not be read as UTF-8.
    #[error("could not read hook input")]
    ReadInput {
        /// The input read failure.
        #[source]
        source: io::Error,
    },
    /// Input did not match the canonical hook protocol.
    #[error("could not parse hook input")]
    InputJson {
        /// The original JSON parse failure.
        #[source]
        source: serde_json::Error,
    },
    /// The selected handler returned an error.
    #[error("hook handler failed")]
    Handler {
        /// The unchanged handler error.
        #[source]
        source: E,
    },
    /// The handler output could not be serialized.
    #[error("could not serialize hook output")]
    OutputJson {
        /// The original JSON serialization failure.
        #[source]
        source: serde_json::Error,
    },
    /// Standard output rejected response bytes.
    #[error("could not write hook output")]
    WriteOutput {
        /// The output write failure.
        #[source]
        source: io::Error,
    },
    /// Standard output could not flush the response.
    #[error("could not flush hook output")]
    FlushOutput {
        /// The output flush failure.
        #[source]
        source: io::Error,
    },
}

impl<E> std::fmt::Debug for HarnessError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::RuntimeAlreadyActive => "HarnessError::RuntimeAlreadyActive",
            Self::Runtime { .. } => "HarnessError::Runtime",
            Self::ReadInput { .. } => "HarnessError::ReadInput",
            Self::InputJson { .. } => "HarnessError::InputJson",
            Self::Handler { .. } => "HarnessError::Handler",
            Self::OutputJson { .. } => "HarnessError::OutputJson",
            Self::WriteOutput { .. } => "HarnessError::WriteOutput",
            Self::FlushOutput { .. } => "HarnessError::FlushOutput",
        })
    }
}

/// Run an SDK 0.1 handler, reporting a fixed diagnostic on failure.
///
/// This supported adapter retains its single handler type parameter until SDK
/// 0.2. It returns exit 0 on success and exit 1 on failure. Diagnostic delivery
/// is best effort because [`ExitCode`] cannot retain a stderr error. Prefer
/// [`try_run`] to inspect concrete failures at the executable boundary.
pub fn run<H: HookHandler>(handler: H) -> ExitCode {
    match try_run(handler) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            if writeln!(stderr, "symposium-hook: {error}")
                .and_then(|()| stderr.flush())
                .is_err()
            {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
    }
}

/// Read one canonical input, dispatch it, and deliver the canonical response.
///
/// Runs the handler on a current-thread runtime, without `Send`, `Sync`, or
/// `'static` requirements. Empty allowing output writes nothing. A denial is
/// always serialized, including when it has no additional context. Other output
/// is one JSON line. This function emits no diagnostics; callers decide how to
/// report errors without exposing their sources.
///
/// # Errors
///
/// Returns a typed runtime, input, handler, serialization, write, or flush error.
/// An already entered Tokio runtime is rejected before reading input. A stream
/// error can leave a partial response; it never indicates successful delivery.
pub fn try_run<H, E>(handler: H) -> Result<ExitCode, HarnessError<E>>
where
    H: HookHandler<E>,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(HarnessError::RuntimeAlreadyActive);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| HarnessError::Runtime { source })?;
    runtime.block_on(try_run_with(
        &handler,
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
    ))
}

async fn try_run_with<H, E>(
    handler: &H,
    reader: &mut impl Read,
    writer: &mut impl Write,
) -> Result<ExitCode, HarnessError<E>>
where
    H: HookHandler<E> + ?Sized,
{
    let mut input_str = String::new();
    reader
        .read_to_string(&mut input_str)
        .map_err(|source| HarnessError::ReadInput { source })?;
    let input: Input =
        serde_json::from_str(&input_str).map_err(|source| HarnessError::InputJson { source })?;
    let output = handler
        .handle_event(&input)
        .await
        .map_err(|source| HarnessError::Handler { source })?;

    if is_empty_output(&output) {
        return Ok(ExitCode::SUCCESS);
    }

    // Complete serialization before writing any protocol bytes.
    let json = serde_json::to_vec(&output).map_err(|source| HarnessError::OutputJson { source })?;
    writer
        .write_all(&json)
        .and_then(|()| writer.write_all(b"\n"))
        .map_err(|source| HarnessError::WriteOutput { source })?;
    writer
        .flush()
        .map_err(|source| HarnessError::FlushOutput { source })?;
    Ok(ExitCode::SUCCESS)
}

fn is_empty_output(output: &Output) -> bool {
    match output {
        Output::PreToolUse(o) => {
            o.decision == Decision::Allow
                && o.additional_context.is_none()
                && o.updated_input.is_none()
        }
        Output::PostToolUse(o) => o.additional_context.is_none(),
        Output::UserPromptSubmit(o) => o.additional_context.is_none(),
        Output::SessionStart(o) => o.additional_context.is_none(),
        Output::Stop(o) => o.additional_context.is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::error::Error as _;
    use std::rc::Rc;

    fn input() -> &'static [u8] {
        br#"{"PreToolUse":{"tool_name":"Bash"}}"#
    }

    struct Allow;
    impl HookHandler<Infallible> for Allow {}

    struct Deny;
    impl HookHandler<Infallible> for Deny {
        async fn pre_tool_use(&self, _: &PreToolUseInput) -> Result<PreToolUseOutput, Infallible> {
            Ok(PreToolUseOutput::new(Decision::Deny, None, None))
        }
    }

    fn execute<H, E>(
        handler: &H,
        reader: &mut impl Read,
        writer: &mut impl Write,
    ) -> Result<ExitCode, HarnessError<E>>
    where
        H: HookHandler<E>,
    {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(try_run_with(handler, reader, writer))
    }

    #[test]
    fn denial_without_context_is_delivered_as_canonical_json() {
        let mut stdout = Vec::new();
        assert_eq!(
            execute(&Deny, &mut input(), &mut stdout).unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(stdout, b"{\"PreToolUse\":{\"decision\":\"deny\"}}\n");
    }

    #[test]
    fn empty_allowing_outputs_remain_silent_for_every_event() {
        for input in [
            r#"{"PreToolUse":{"tool_name":"Bash"}}"#,
            r#"{"PostToolUse":{"tool_name":"Bash"}}"#,
            r#"{"UserPromptSubmit":{}}"#,
            r#"{"SessionStart":{}}"#,
            r#"{"Stop":{}}"#,
        ] {
            let mut stdout = Vec::new();
            assert_eq!(
                execute(&Allow, &mut input.as_bytes(), &mut stdout).unwrap(),
                ExitCode::SUCCESS
            );
            assert!(stdout.is_empty());
        }
    }

    struct FailedRead;
    impl Read for FailedRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "PRIVATE_READ_CANARY",
            ))
        }
    }

    #[test]
    fn input_read_failure_preserves_source_and_writes_nothing() {
        let mut stdout = Vec::new();
        let error = execute(&Allow, &mut FailedRead, &mut stdout).unwrap_err();
        assert!(matches!(error, HarnessError::ReadInput { .. }));
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!format!("{error} {error:?}").contains("PRIVATE_READ_CANARY"));
        assert!(stdout.is_empty());
    }

    #[test]
    fn invalid_json_preserves_parser_source_without_rendering_input() {
        let mut stdout = Vec::new();
        let error = execute(
            &Allow,
            &mut br#"{"PRIVATE_JSON_CANARY":{}}"#.as_slice(),
            &mut stdout,
        )
        .unwrap_err();
        assert!(matches!(error, HarnessError::InputJson { .. }));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert!(!format!("{error} {error:?}").contains("PRIVATE_JSON_CANARY"));
        assert!(stdout.is_empty());
    }

    struct FailedWrite;
    impl Write for FailedWrite {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PRIVATE_WRITE_CANARY",
            ))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn rejected_response_is_a_typed_write_failure() {
        let error = execute(&Deny, &mut input(), &mut FailedWrite).unwrap_err();
        assert!(matches!(error, HarnessError::WriteOutput { .. }));
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(!format!("{error} {error:?}").contains("PRIVATE_WRITE_CANARY"));
    }

    #[derive(Default)]
    struct FailedFlush(Vec<u8>);
    impl Write for FailedFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PRIVATE_FLUSH_CANARY",
            ))
        }
    }

    #[test]
    fn flush_failure_does_not_claim_delivery_success() {
        let mut stdout = FailedFlush::default();
        let error = execute(&Deny, &mut input(), &mut stdout).unwrap_err();
        assert!(matches!(error, HarnessError::FlushOutput { .. }));
        assert_eq!(stdout.0, b"{\"PreToolUse\":{\"decision\":\"deny\"}}\n");
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(!format!("{error} {error:?}").contains("PRIVATE_FLUSH_CANARY"));
    }

    #[derive(Debug, thiserror::Error)]
    #[error("PRIVATE_HANDLER_CANARY")]
    struct LocalError(Rc<()>);

    struct LocalHandler<'a> {
        has_run: &'a Cell<bool>,
        marker: Rc<()>,
    }

    impl HookHandler<LocalError> for LocalHandler<'_> {
        async fn pre_tool_use(&self, _: &PreToolUseInput) -> Result<PreToolUseOutput, LocalError> {
            let marker = self.marker.clone();
            std::future::ready(()).await;
            self.has_run.set(true);
            Err(LocalError(marker))
        }
    }

    #[test]
    fn borrowed_non_send_handler_and_error_need_no_extra_bounds() {
        let has_run = Cell::new(false);
        let marker = Rc::new(());
        let handler = LocalHandler {
            has_run: &has_run,
            marker: marker.clone(),
        };
        let mut stdout = Vec::new();
        let error = execute(&handler, &mut input(), &mut stdout).unwrap_err();
        assert!(has_run.get());
        assert!(!format!("{error} {error:?}").contains("PRIVATE_HANDLER_CANARY"));
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<LocalError>()
            .unwrap();
        assert!(Rc::ptr_eq(&source.0, &marker));
        assert!(stdout.is_empty());
    }

    #[test]
    fn entered_runtime_is_rejected_before_touching_process_streams() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        assert!(matches!(
            try_run(Allow),
            Err(HarnessError::RuntimeAlreadyActive)
        ));
    }
}
