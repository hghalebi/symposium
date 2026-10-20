//! User-facing output with emoji status indicators.
//!
//! All user-visible status messages go through `Output`, which can be
//! silenced with `--quiet` or when running from a hook.

use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// Captured messages were requested from a non-capturing output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("output is not in capturing mode")]
pub struct CaptureError;

/// Controls whether user-facing status messages are printed.
///
/// Status methods preserve their best-effort, void contract. Stream delivery
/// failures produce a content-free stderr warning when that channel is writable.
#[derive(Debug, Clone)]
pub struct Output {
    quiet: bool,
    capture: Option<Arc<Mutex<Vec<String>>>>,
}

impl Output {
    pub fn normal() -> Self {
        Self {
            quiet: false,
            capture: None,
        }
    }

    pub fn quiet() -> Self {
        Self {
            quiet: true,
            capture: None,
        }
    }

    /// Create an output that captures all messages into a buffer
    /// instead of printing them.
    pub fn capturing() -> Self {
        Self {
            quiet: false,
            capture: Some(Arc::new(Mutex::new(Vec::new()))),
        }
    }

    /// Return all captured messages through the legacy infallible adapter.
    ///
    /// Retained until root 0.6; use [`Self::try_captured`] for typed access.
    ///
    /// # Panics
    /// Panics when this output is not in capturing mode.
    #[cfg_attr(
        not(test),
        expect(
            clippy::expect_used,
            reason = "legacy infallible capture access retained until root 0.6; use try_captured"
        )
    )]
    pub fn captured(&self) -> Vec<String> {
        self.try_captured().expect("not in capturing mode")
    }

    /// Return a snapshot of all captured messages without consuming them.
    ///
    /// # Errors
    /// Returns [`CaptureError`] when this output is not in capturing mode.
    pub fn try_captured(&self) -> Result<Vec<String>, CaptureError> {
        let buffer = self.capture.as_ref().ok_or(CaptureError)?;
        Ok(lock_output_buffer(buffer).clone())
    }

    pub fn is_quiet(&self) -> bool {
        self.quiet
    }

    /// May we block on stdin to ask the user a question?
    ///
    /// Only a non-quiet, non-capturing output attached to a terminal on both
    /// ends qualifies. Hook dispatch and the library test harness both use
    /// quiet/capturing outputs, so neither can ever reach a prompt — the TTY
    /// check alone would not guarantee that, since `cargo test` inherits the
    /// developer's terminal.
    pub fn is_interactive(&self) -> bool {
        use std::io::IsTerminal;
        !self.quiet
            && self.capture.is_none()
            && std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
    }

    fn emit(&self, msg: String) {
        if self.quiet {
            return;
        }
        if let Some(buf) = &self.capture {
            lock_output_buffer(buf).push(msg);
        } else {
            write_line_best_effort(io::stdout().lock(), &msg, "stdout");
        }
    }

    fn emit_err(&self, msg: String) {
        if self.quiet {
            return;
        }
        if let Some(buf) = &self.capture {
            lock_output_buffer(buf).push(msg);
        } else {
            write_line_best_effort(io::stderr().lock(), &msg, "stderr");
        }
    }

    /// Something was already in place, no action needed.
    pub fn already_ok(&self, msg: impl std::fmt::Display) {
        self.emit(format!("🟢 {msg}"));
    }

    /// An action was taken successfully (created, added, wrote).
    pub fn done(&self, msg: impl std::fmt::Display) {
        self.emit(format!("✅ {msg}"));
    }

    /// A new item was discovered or added.
    pub fn added(&self, msg: impl std::fmt::Display) {
        self.emit(format!("➕ {msg}"));
    }

    /// An item was removed.
    pub fn removed(&self, msg: impl std::fmt::Display) {
        self.emit(format!("➖ {msg}"));
    }

    /// Informational status (not an action).
    pub fn info(&self, msg: impl std::fmt::Display) {
        self.emit(format!("ℹ️  {msg}"));
    }

    /// A warning (something went wrong but not fatal).
    pub fn warn(&self, msg: impl std::fmt::Display) {
        self.emit_err(format!("⚠️  {msg}"));
    }

    /// Print a blank line for spacing.
    pub fn blank(&self) {
        self.emit(String::new());
    }

    /// Print arbitrary text (for prompts, headers, etc.).
    pub fn println(&self, msg: impl std::fmt::Display) {
        self.emit(format!("{msg}"));
    }
}

// Unwinding cannot invalidate a Vec; preserve messages already collected.
pub(crate) fn lock_output_buffer<T>(buffer: &Mutex<Vec<T>>) -> MutexGuard<'_, Vec<T>> {
    buffer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn write_line(mut writer: impl Write, message: &str) -> io::Result<()> {
    writeln!(writer, "{message}")?;
    writer.flush()
}

pub(crate) fn write_line_best_effort(writer: impl Write, message: &str, stream: &'static str) {
    if let Err(error) = write_line(writer, message) {
        let diagnostic = format!(
            "warning: failed to write {stream} output ({:?})",
            error.kind()
        );
        // stderr is the final diagnostic channel; never recurse if it is also closed.
        let _diagnostic_delivery = write_line(io::stderr().lock(), &diagnostic);
    }
}

/// Replace the home directory prefix with `~` for display.
pub fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_access_is_typed_and_snapshots_do_not_consume_messages() {
        assert_eq!(Output::normal().try_captured(), Err(CaptureError));
        assert_eq!(Output::quiet().try_captured(), Err(CaptureError));
        let output = Output::capturing();
        output.info("first");
        output.warn("second");
        let expected = vec!["ℹ️  first".to_owned(), "⚠️  second".to_owned()];
        assert_eq!(output.try_captured().unwrap(), expected);
        assert_eq!(output.captured(), expected);
    }

    #[test]
    fn poisoned_capture_preserves_existing_and_later_messages() {
        let output = Output::capturing();
        output.info("before");
        let buffer = Arc::clone(output.capture.as_ref().unwrap());
        assert!(
            std::panic::catch_unwind(move || {
                let _guard = buffer.lock().unwrap();
                panic!("poison capture for recovery test");
            })
            .is_err()
        );
        output.warn("after");
        assert_eq!(
            output.try_captured().unwrap(),
            vec!["ℹ️  before".to_owned(), "⚠️  after".to_owned()]
        );
    }

    #[test]
    fn line_writer_preserves_bytes_and_reports_delivery_failures() {
        let mut bytes = Vec::new();
        write_line(&mut bytes, "✅ done").unwrap();
        assert_eq!(bytes, "✅ done\n".as_bytes());
        let error = write_line(ClosedWriter, "not delivered").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    struct ClosedWriter;

    impl Write for ClosedWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed output"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
