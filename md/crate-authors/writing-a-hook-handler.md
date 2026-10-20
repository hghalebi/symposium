# Writing a hook handler

This guide walks through writing a symposium hook handler in Rust using the `symposium-sdk` crate.

## Step 1. Create a new binary crate

Create your new crate. A separate crate next to your library (for example `my-crate-hooks`) keeps the SDK out of your library's dependencies:

```bash
cargo new my-crate-hooks
cd my-crate-hooks
```

Add `symposium-sdk` to your dependencies:

```bash
cargo add symposium-sdk
```

## Step 2. Write the handler

A hook handler is a program that reads a JSON event on stdin and writes a JSON response to stdout. The `symposium-sdk` crate provides a `HookHandler<E>` trait and a fallible `try_run()` harness that handles the plumbing while preserving the handler's error type.

Implement `HookHandler<E>` and override the methods for the events you care about. This handler makes a pure decision, so `Infallible` states that its event methods cannot fail:

```rust
// src/main.rs
use std::convert::Infallible;
use std::io::Write as _;
use std::process::ExitCode;
use symposium_sdk::hook::{HookHandler, PreToolUseInput, PreToolUseOutput, try_run};

// The shell tool's name in Claude Code and Codex, Copilot, Antigravity, and Kiro.
const SHELL_TOOLS: &[&str] = &["Bash", "bash", "run_command", "execute_bash"];

struct MyHook;

impl HookHandler<Infallible> for MyHook {
    async fn pre_tool_use(&self, event: &PreToolUseInput) -> Result<PreToolUseOutput, Infallible> {
        if SHELL_TOOLS.contains(&event.tool_name.as_str()) {
            Ok(PreToolUseOutput::context("Remember: prefer non-destructive commands"))
        } else {
            Ok(PreToolUseOutput::default())
        }
    }
}

fn main() -> ExitCode {
    match try_run(MyHook) {
        Ok(exit) => exit,
        Err(error) => {
            if writeln!(std::io::stderr().lock(), "symposium-hook: {error}").is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
    }
}
```

The `try_run()` function:

1. Creates a current-thread runtime and reads symposium canonical JSON from stdin.
2. Deserializes it into an `Input` event.
3. Calls `handler.handle_event()`, which dispatches to the appropriate method.
4. Serializes nonempty output to stdout and flushes it. A denial is always nonempty, even when it carries no context.

Runtime creation, input, handler, serialization, output, and flush failures return a `HarnessError<E>`. Keep stdout for the protocol; the example writes a safe diagnostic to stderr and returns failure if the hook or diagnostic cannot be delivered. `Display` identifies the failing operation without printing input or source-error contents. Inspect attached sources deliberately: parser and handler errors may contain private input, so avoid logging `Debug` output or unrestricted error chains.

`try_run()` owns its runtime and must be called outside an existing Tokio runtime. Code already running asynchronously can call the handler or `try_default_handle_event()` directly. Handlers and their futures do not need `Send`, `Sync`, or `'static` bounds for this current-thread harness.

You only need to override the methods you care about — unimplemented methods return the default (empty) output for their event type.

`tool_name` arrives exactly as the agent sent it, and `tool_input` keeps the agent's own argument names, so check every name you want to handle (Antigravity's shell, for example, takes `CommandLine` rather than `command`); see [Matcher](../reference/hook-events.md#matcher).

## Step 3. Register it in your plugin manifest

Publish the handler to crates.io, then reference it as a hook command in your plugin's `Symposium.toml`:

```toml
[[hooks]]
name = "check-usage"
event = "PreToolUse"
matcher = "^(Bash|bash|run_command|execute_bash)$"
command = { source = "cargo", crate = "my-crate-hooks", executable = "my-crate-hooks" }
```

Symposium installs the binary (with `cargo binstall` or `cargo install`) the first time the hook runs. A manifest shipped in your crate needs nothing else; a recommendations-repository entry also sets `name` and `depends-on` (see [Authoring a plugin](./authoring-a-plugin.md)).

## Output types

Each handler method returns its event-specific output type:

| Method | Return type | Key fields |
|--------|-------------|------------|
| `pre_tool_use` | `PreToolUseOutput` | `decision`, `additional_context`, `updated_input` |
| `post_tool_use` | `PostToolUseOutput` | `additional_context` |
| `user_prompt_submit` | `UserPromptSubmitOutput` | `additional_context` |
| `session_start` | `SessionStartOutput` | `additional_context` |
| `stop` | `StopOutput` | `additional_context` |

Each output type has convenience constructors:

- `::default()`: empty output, no-op.
- `::context("...")`: inject text into the agent's context. On `Stop`, agents treat this output differently; see [`Stop` output](../reference/hook-events.md#stop-output).
- `PreToolUseOutput::with_updated_input(value)`: replace the tool input. Not every agent can apply a rewrite; see [how each agent receives the decision](../reference/hook-events.md#pretooluse-output).
- `PreToolUseOutput::deny("reason")`: block the tool call with a reason.

Return `Err(...)` from a method to preserve its failure in `HarnessError::Handler`. The executable decides how to report it and returns failure, as in the example above. The context example below uses `std::io::Error`: absent `NOTES.md` is optional, while permission, decoding, and other read failures remain errors.

## The `HookHandler` trait

```rust
pub trait HookHandler<E = anyhow::Error> {
    async fn handle_event(&self, input: &Input) -> Result<Output, E> { /* dispatches */ }
    async fn pre_tool_use(&self, event: &PreToolUseInput) -> Result<PreToolUseOutput, E> { /* default */ }
    async fn post_tool_use(&self, event: &PostToolUseInput) -> Result<PostToolUseOutput, E> { /* default */ }
    async fn user_prompt_submit(&self, event: &UserPromptSubmitInput) -> Result<UserPromptSubmitOutput, E> { /* default */ }
    async fn session_start(&self, event: &SessionStartInput) -> Result<SessionStartOutput, E> { /* default */ }
    async fn stop(&self, event: &StopInput) -> Result<StopOutput, E> { /* default */ }
}
```

Override `handle_event` only if you need custom dispatch logic (e.g., shared state across events). Otherwise, just override the per-event methods. An override can delegate to `try_default_handle_event()` without erasing `E`.

For compatibility until SDK 0.2, `impl HookHandler` still selects `anyhow::Error`, the `hook::anyhow` re-export remains available, and `run()` and `default_handle_event()` keep their existing signatures. Existing handlers continue to compile. Use an explicit error type with `try_run()` and `try_default_handle_event()` for new code; choose a typed `thiserror` error when several failure cases need distinct meaning.

## Testing locally

You can test your handler by piping JSON directly:

```bash
cargo build
echo '{"PreToolUse":{"tool_name":"Bash","tool_input":{"command":"rm -rf /"},"session_id":null,"cwd":"/tmp"}}' \
  | ./target/debug/my-crate-hooks
```

To run it through Symposium the way an agent does, see [Try it in a project](./authoring-a-plugin.md#step-4-try-it-in-a-project).

## Example: blocking destructive commands

This example reads Claude Code's `Bash` tool; to cover other agents, check their tool names as in Step 2 and read Antigravity's `CommandLine` as well.

```rust
{{#include ../../symposium-sdk/examples/block_destructive.rs}}
```

## Example: injecting context on session start

```rust
{{#include ../../symposium-sdk/examples/inject_context.rs}}
```
