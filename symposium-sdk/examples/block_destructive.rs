#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable
    )
)]

//! A `PreToolUse` handler that refuses destructive shell commands.
//!
//! ```bash
//! echo '{"PreToolUse":{"tool_name":"Bash","tool_input":{"command":"rm -rf /"},"session_id":null,"cwd":"/tmp"}}' \
//!   | cargo run --example block_destructive
//! ```

use std::convert::Infallible;
use std::io::Write as _;
use std::process::ExitCode;

use symposium_sdk::hook::{HookHandler, PreToolUseInput, PreToolUseOutput, try_run};

/// Fragments that are never worth running, whatever the surrounding command.
const DESTRUCTIVE: &[&str] = &["rm -rf /", "mkfs", "dd if=", ":(){ :|:& };:"];

struct BlockDestructive;

impl HookHandler<Infallible> for BlockDestructive {
    async fn pre_tool_use(&self, event: &PreToolUseInput) -> Result<PreToolUseOutput, Infallible> {
        if event.tool_name != "Bash" {
            return Ok(PreToolUseOutput::default());
        }

        let command = event
            .tool_input
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or_default();

        match DESTRUCTIVE.iter().find(|needle| command.contains(**needle)) {
            Some(needle) => Ok(PreToolUseOutput::deny(format!(
                "refusing a command containing `{needle}`"
            ))),
            None => Ok(PreToolUseOutput::default()),
        }
    }
}

fn main() -> ExitCode {
    match try_run(BlockDestructive) {
        Ok(exit) => exit,
        Err(error) => {
            if writeln!(std::io::stderr().lock(), "symposium-hook: {error}").is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
    }
}
