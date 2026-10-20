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

//! A `SessionStart` handler that puts a project's notes in front of the agent.
//!
//! ```bash
//! echo '{"SessionStart":{"session_id":null,"cwd":"/path/to/project"}}' \
//!   | cargo run --example inject_context
//! ```

use std::io::{self, Write as _};
use std::path::Path;
use std::process::ExitCode;

use symposium_sdk::hook::{HookHandler, SessionStartInput, SessionStartOutput, try_run};

struct InjectContext;

impl HookHandler<io::Error> for InjectContext {
    async fn session_start(&self, event: &SessionStartInput) -> io::Result<SessionStartOutput> {
        let Some(cwd) = event.cwd.as_deref() else {
            return Ok(SessionStartOutput::default());
        };

        match std::fs::read_to_string(Path::new(cwd).join("NOTES.md")) {
            Ok(notes) => Ok(SessionStartOutput::context(notes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(SessionStartOutput::default())
            }
            Err(error) => Err(error),
        }
    }
}

fn main() -> ExitCode {
    match try_run(InjectContext) {
        Ok(exit) => exit,
        Err(error) => {
            if writeln!(std::io::stderr().lock(), "symposium-hook: {error}").is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absent_notes_add_no_context() -> io::Result<()> {
        let event = SessionStartInput::new(None, Some(env!("CARGO_MANIFEST_DIR").to_owned()));
        let output = InjectContext.session_start(&event).await?;
        assert!(output.additional_context.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn unreadable_notes_propagate_the_failure() {
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let event = SessionStartInput::new(None, Some(cwd.to_string_lossy().into_owned()));
        assert!(InjectContext.session_start(&event).await.is_err());
    }
}
