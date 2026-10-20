//! Environment-variable helpers that emit predicate cache events.
//!
//! Use [`try_var`] instead of [`std::env::var`] from a custom predicate so that
//! reading a variable also declares a dependency on its value. Symposium
//! fingerprints the value at read time and invalidates the cached predicate
//! result if the value changes.

use std::env;
use std::fmt;
use std::io::{self, Write};

use crate::predicate::PredicateEmitter;

/// Failure to declare or read an environment-variable dependency.
///
/// Display and Debug output omit the variable name, value, and source text.
/// The original failure remains available through [`std::error::Error::source`].
#[derive(thiserror::Error)]
#[non_exhaustive]
pub enum EnvReadError {
    /// The dependency record could not be written; the variable was not read.
    #[error("could not emit environment-variable dependency")]
    Watch {
        /// The underlying output failure.
        #[source]
        source: io::Error,
    },
    /// The dependency was declared, but the variable could not be read.
    #[error("could not read environment variable")]
    Environment {
        /// The underlying missing-variable or invalid-Unicode failure.
        #[source]
        source: env::VarError,
    },
}

impl fmt::Debug for EnvReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Watch { .. } => "EnvReadError::Watch",
            Self::Environment { .. } => "EnvReadError::Environment",
        })
    }
}

/// Declare a dependency on `name`, then read its value.
///
/// Symposium invalidates the cached predicate result when the value changes.
///
/// # Errors
///
/// Returns [`EnvReadError::Watch`] if the dependency record cannot be written
/// to stdout, without reading the variable. Returns [`EnvReadError::Environment`]
/// if the variable is absent or its value is not valid Unicode.
pub fn try_var(name: &str) -> Result<String, EnvReadError> {
    try_var_with(name, &mut PredicateEmitter::stdout(), |name| env::var(name))
}

fn try_var_with(
    name: &str,
    emitter: &mut PredicateEmitter<impl Write>,
    read: impl FnOnce(&str) -> Result<String, env::VarError>,
) -> Result<String, EnvReadError> {
    emitter
        .watch_env(name)
        .map_err(|source| EnvReadError::Watch { source })?;
    read(name).map_err(|source| EnvReadError::Environment { source })
}

/// Read the value of the environment variable `name` and emit a
/// [`CustomPredicateEvent::WatchEnv`] on stdout so Symposium invalidates the
/// cached predicate result when the value changes.
///
/// This compatibility helper retains best-effort watch delivery until SDK 0.2.
/// It reports a delivery failure to stderr, then still reads the variable.
/// Prefer [`try_var`] to propagate both dependency and environment failures.
///
/// # Errors
///
/// Returns [`env::VarError`] if the variable is absent or is not valid Unicode.
/// Watch failures cannot be represented by this legacy return type.
///
/// [`CustomPredicateEvent::WatchEnv`]: crate::predicate::CustomPredicateEvent::WatchEnv
pub fn var(name: &str) -> Result<String, env::VarError> {
    match try_var(name) {
        Ok(value) => Ok(value),
        Err(EnvReadError::Environment { source }) => Err(source),
        Err(EnvReadError::Watch { .. }) => {
            // Legacy VarError cannot carry output failures, including stderr.
            let _ = writeln!(
                io::stderr(),
                "symposium-sdk: could not emit environment-variable dependency; cached results may be stale"
            );
            env::var(name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::error::Error;

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "WATCH_CANARY"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn declares_dependency_and_returns_variable() {
        let mut output = Vec::new();
        let value = try_var_with(
            "SDK_TEST_VAR",
            &mut PredicateEmitter::new(&mut output),
            |name| {
                assert_eq!(name, "SDK_TEST_VAR");
                Ok("value".into())
            },
        )
        .unwrap();

        assert_eq!(value, "value");
        assert_eq!(output, b"{\"watchEnv\":\"SDK_TEST_VAR\"}\n");
    }

    #[test]
    fn watch_failure_prevents_read_and_preserves_source_without_displaying_it() {
        let was_read = Cell::new(false);
        let error = try_var_with(
            "NAME_CANARY",
            &mut PredicateEmitter::new(FailingWriter),
            |_| {
                was_read.set(true);
                Ok("VALUE_CANARY".into())
            },
        )
        .unwrap_err();

        assert!(!was_read.get());
        assert!(matches!(error, EnvReadError::Watch { .. }));
        assert_eq!(
            error.to_string(),
            "could not emit environment-variable dependency"
        );
        assert_eq!(format!("{error:?}"), "EnvReadError::Watch");
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn missing_variable_is_distinct_from_watch_failure() {
        let mut output = Vec::new();
        let error = try_var_with(
            "SDK_TEST_VAR",
            &mut PredicateEmitter::new(&mut output),
            |_| Err(env::VarError::NotPresent),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            EnvReadError::Environment {
                source: env::VarError::NotPresent
            }
        ));
        assert!(matches!(
            error.source().unwrap().downcast_ref::<env::VarError>(),
            Some(env::VarError::NotPresent)
        ));
        assert_eq!(output, b"{\"watchEnv\":\"SDK_TEST_VAR\"}\n");
    }

    #[test]
    fn variable_error_diagnostics_omit_values() {
        let error = try_var_with(
            "NAME_CANARY",
            &mut PredicateEmitter::new(Vec::new()),
            |_| Err(env::VarError::NotUnicode("VALUE_CANARY".into())),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "could not read environment variable");
        assert_eq!(format!("{error:?}"), "EnvReadError::Environment");
        assert!(matches!(
            error.source().unwrap().downcast_ref::<env::VarError>(),
            Some(env::VarError::NotUnicode(_))
        ));
    }

    #[test]
    fn legacy_signature_is_preserved() {
        let _: fn(&str) -> Result<String, env::VarError> = var;
    }
}
