//! Self-update: check for newer versions and install them via `cargo install`.

use std::io;
use std::process::{Command, ExitStatus};

use crate::config::{AutoUpdate, Symposium};
use crate::output::Output;
use crate::state;
use crate::state::CURRENT_VERSION;

const CRATE_NAME: &str = "symposium";

/// A failure to check for or install a Symposium update.
///
/// Display messages omit subprocess output and parser input. Original I/O and
/// version-parser errors remain available through [`std::error::Error::source`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UpdateError {
    /// Cargo could not be launched or its output could not be collected.
    #[error("failed to run cargo {operation}")]
    InvokeCargo {
        /// The Cargo operation being attempted.
        operation: &'static str,
        /// The original process I/O error.
        #[source]
        source: io::Error,
    },
    /// The registry query exited unsuccessfully.
    #[error("cargo search failed ({status})")]
    SearchFailed {
        /// The query's process exit status.
        status: ExitStatus,
    },
    /// The registry query did not return the expected crate/version record.
    #[error("unexpected cargo search output")]
    UnexpectedSearchOutput,
    /// A current or published version was not valid semantic versioning.
    #[error("failed to parse {operation} version")]
    Version {
        /// Whether the current or published version was being parsed.
        operation: &'static str,
        /// The original semantic-version parser error.
        #[source]
        source: semver::Error,
    },
    /// Cargo failed to install the update.
    #[error("cargo install failed ({status})")]
    InstallFailed {
        /// The installation's process exit status.
        status: ExitStatus,
    },
}

/// Query the registry for the latest published version of Symposium.
///
/// Runs `cargo search symposium --limit 1` using the configured Cargo binary.
///
/// # Errors
///
/// Returns a typed error if Cargo fails or its response has no valid version.
pub fn try_latest_version(sym: &Symposium) -> Result<semver::Version, UpdateError> {
    let output = sym
        .cargo_command()
        .args(["search", CRATE_NAME, "--limit", "1"])
        .output()
        .map_err(|source| UpdateError::InvokeCargo {
            operation: "search",
            source,
        })?;

    if !output.status.success() {
        return Err(UpdateError::SearchFailed {
            status: output.status,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_cargo_search_output(&stdout)
}

/// Query the registry for the latest published version of Symposium.
///
/// Legacy adapter retained until the root crate's 0.6 release. New callers
/// should use [`try_latest_version`] for structured failures.
pub fn latest_version(sym: &Symposium) -> anyhow::Result<semver::Version> {
    try_latest_version(sym).map_err(Into::into)
}

/// Parse the version from `cargo search` output.
///
/// Expected format: `symposium = "0.3.0"    # AI the Rust Way`
fn parse_cargo_search_output(output: &str) -> Result<semver::Version, UpdateError> {
    let version_str = output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix(CRATE_NAME))
        .and_then(|rest| rest.strip_prefix(" = \""))
        .and_then(|rest| rest.split_once('"'))
        .map(|(version, _)| version)
        .ok_or(UpdateError::UnexpectedSearchOutput)?;

    semver::Version::parse(version_str).map_err(|source| UpdateError::Version {
        operation: "published",
        source,
    })
}

/// Check whether a newer version is available.  Returns `Some(latest)`
/// if an upgrade is available, `None` if we're current or ahead.
///
/// # Errors
///
/// Returns a typed error if either version is invalid or the query fails.
pub fn try_check_upgrade(sym: &Symposium) -> Result<Option<semver::Version>, UpdateError> {
    let current =
        semver::Version::parse(CURRENT_VERSION).map_err(|source| UpdateError::Version {
            operation: "current",
            source,
        })?;
    let latest = try_latest_version(sym)?;
    if latest > current {
        Ok(Some(latest))
    } else {
        Ok(None)
    }
}

/// Check whether a newer version is available.
///
/// Legacy adapter retained until the root crate's 0.6 release. New callers
/// should use [`try_check_upgrade`] for structured failures.
pub fn check_upgrade(sym: &Symposium) -> anyhow::Result<Option<semver::Version>> {
    try_check_upgrade(sym).map_err(Into::into)
}

// Optional update checks may proceed when state is unreadable; never claim a failed stamp.
fn should_check_for_update(sym: &Symposium) -> bool {
    match state::try_should_check_for_update(sym.config_dir()) {
        Ok(should_check) => should_check,
        Err(error) => {
            tracing::warn!(error = %error, "update throttle state unavailable (continuing)");
            true
        }
    }
}

fn record_update_check(sym: &Symposium) {
    if let Err(error) = state::try_record_update_check(sym.config_dir()) {
        tracing::warn!(error = %error, "update check timestamp was not saved (continuing)");
    }
}

/// Warn-only update check for the library path (`cli::run`).
///
/// Respects config and the 24-hour throttle.  Prints a nudge when
/// `auto-update = "warn"` and a newer version exists.  Does nothing for
/// `off` or `on` — the binary handles `on` (which needs re-exec).
pub fn maybe_warn_for_update(sym: &Symposium, out: &Output) {
    if sym.config.auto_update != AutoUpdate::Warn {
        return;
    }
    if !should_check_for_update(sym) {
        return;
    }
    record_update_check(sym);

    match try_check_upgrade(sym) {
        Ok(Some(latest)) => out.warn(format!(
            "symposium {latest} is available (current: {CURRENT_VERSION}). \
             Run `cargo agents self-update` to upgrade.",
        )),
        Ok(None) => {}
        Err(error) => tracing::debug!(%error, "opportunistic update check failed"),
    }
}

/// Full update check for the binary startup path.
///
/// Respects config and the 24-hour throttle.  For `warn`, prints a
/// nudge.  For `on`, installs the update via `cargo install`, then
/// returns `true` so the caller can re-exec into the new binary.
pub async fn maybe_check_for_update(sym: &Symposium, out: &Output) -> bool {
    if sym.config.auto_update == AutoUpdate::Off {
        return false;
    }
    if !should_check_for_update(sym) {
        return false;
    }
    record_update_check(sym);

    let latest = match try_check_upgrade(sym) {
        Ok(Some(v)) => v,
        Ok(None) => return false,
        Err(error) => {
            tracing::debug!(%error, "opportunistic update check failed");
            return false;
        }
    };

    match sym.config.auto_update {
        AutoUpdate::Warn => {
            out.warn(format!(
                "symposium {latest} is available (current: {CURRENT_VERSION}). \
                 Run `cargo agents self-update` to upgrade.",
            ));
            false
        }
        AutoUpdate::On => {
            out.info(format!(
                "auto-updating symposium {CURRENT_VERSION} → {latest}..."
            ));
            match cargo_install(sym) {
                Ok(()) => {
                    out.done(format!("updated to {latest}"));
                    true
                }
                Err(e) => {
                    out.warn(format!("auto-update failed: {e}"));
                    false
                }
            }
        }
        AutoUpdate::Off => false,
    }
}

/// Check for a newer version and install it through Cargo if one is available.
///
/// # Errors
///
/// Returns a typed error if the version check or Cargo installation fails.
pub fn try_self_update(sym: &Symposium, out: &Output) -> Result<(), UpdateError> {
    let target_version = match try_check_upgrade(sym) {
        Ok(Some(latest)) => {
            out.info(format!("updating symposium {CURRENT_VERSION} → {latest}"));
            latest
        }
        Ok(None) => {
            out.already_ok(format!("symposium {CURRENT_VERSION} is up to date"));
            return Ok(());
        }
        Err(e) => {
            out.warn(format!("could not check for updates: {e}"));
            return Err(e);
        }
    };

    cargo_install(sym)?;
    out.done(format!("updated to {target_version}"));
    Ok(())
}

/// Run the self-update: check for a newer version and install it.
///
/// Legacy adapter retained until the root crate's 0.6 release. New callers
/// should use [`try_self_update`] for structured failures.
pub fn self_update(sym: &Symposium, out: &Output) -> anyhow::Result<()> {
    try_self_update(sym, out).map_err(Into::into)
}

fn cargo_install(sym: &Symposium) -> Result<(), UpdateError> {
    // Captured Cargo diagnostics may contain private registry or build-script data.
    let output = sym
        .cargo_command()
        .args(["install", CRATE_NAME, "--force"])
        .output()
        .map_err(|source| UpdateError::InvokeCargo {
            operation: "install",
            source,
        })?;

    if !output.status.success() {
        return Err(UpdateError::InstallFailed {
            status: output.status,
        });
    }
    Ok(())
}

/// Re-execute the current process (replaces the process via exec on Unix,
/// spawn-and-exit on Windows).
pub fn re_exec() -> ! {
    let mut args = std::env::args_os();
    let Some(program) = args.next() else {
        crate::output::write_line_best_effort(
            std::io::stderr(),
            "failed to re-exec: current program is unavailable",
            "stderr",
        );
        std::process::exit(1);
    };

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(program).args(args).exec();
        crate::output::write_line_best_effort(
            std::io::stderr(),
            &format!("failed to re-exec ({:?})", err.kind()),
            "stderr",
        );
        std::process::exit(1);
    }

    #[cfg(not(unix))]
    {
        let status = match Command::new(program).args(args).status() {
            Ok(status) => status,
            Err(error) => {
                crate::output::write_line_best_effort(
                    std::io::stderr(),
                    &format!("failed to re-exec ({:?})", error.kind()),
                    "stderr",
                );
                std::process::exit(1);
            }
        };
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;

    #[test]
    fn parse_search_output_typical() {
        let output = r#"symposium = "1.2.3"    # AI the Rust Way
... and 105 crates more (use --limit N to see more)
"#;
        let v = parse_cargo_search_output(output).unwrap();
        assert_eq!(v, semver::Version::new(1, 2, 3));
    }

    #[test]
    fn parse_search_output_prerelease() {
        let output = "symposium = \"0.4.0-beta.1\"    # AI the Rust Way\n";
        let v = parse_cargo_search_output(output).unwrap();
        assert_eq!(v.to_string(), "0.4.0-beta.1");
    }

    #[test]
    fn parse_search_output_no_description() {
        let output = "symposium = \"0.3.0\"\n";
        let v = parse_cargo_search_output(output).unwrap();
        assert_eq!(v, semver::Version::new(0, 3, 0));
    }

    #[test]
    fn parse_search_output_wrong_crate() {
        let output = "something-else = \"1.0.0\"    # Not symposium\n";
        assert!(parse_cargo_search_output(output).is_err());
    }

    #[test]
    fn parse_search_output_empty() {
        assert!(parse_cargo_search_output("").is_err());
    }

    #[test]
    fn parse_search_output_requires_a_complete_quoted_version() {
        for output in [
            "symposium = \"1.2.3",
            "symposium-other = \"1.2.3\"",
            "symposium = 1.2.3",
        ] {
            assert!(matches!(
                parse_cargo_search_output(output),
                Err(UpdateError::UnexpectedSearchOutput)
            ));
        }
    }

    #[test]
    fn malformed_version_preserves_the_parser_error_without_displaying_input() {
        let error = parse_cargo_search_output("symposium = \"version-secret-canary\"").unwrap_err();
        assert!(matches!(
            error,
            UpdateError::Version {
                operation: "published",
                ..
            }
        ));
        assert!(error.source().unwrap().is::<semver::Error>());
        assert_eq!(error.to_string(), "failed to parse published version");
        assert!(!error.to_string().contains("version-secret-canary"));
    }

    #[test]
    fn cargo_invocation_failure_preserves_its_io_source() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = Symposium::try_from_dir(tmp.path()).unwrap();
        sym.set_cargo_override(tmp.path().join("missing-cargo-secret-canary"));

        for (error, operation) in [
            (try_latest_version(&sym).unwrap_err(), "search"),
            (cargo_install(&sym).unwrap_err(), "install"),
        ] {
            assert!(matches!(
                error,
                UpdateError::InvokeCargo {
                    operation: actual,
                    ..
                } if actual == operation
            ));
            assert_eq!(
                error
                    .source()
                    .unwrap()
                    .downcast_ref::<io::Error>()
                    .unwrap()
                    .kind(),
                io::ErrorKind::NotFound
            );
            assert_eq!(
                error.to_string(),
                format!("failed to run cargo {operation}")
            );
        }
    }

    #[test]
    fn explicit_update_reports_safe_failure_and_retains_typed_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = Symposium::try_from_dir(tmp.path()).unwrap();
        sym.set_cargo_override(tmp.path().join("missing-cargo-secret-canary"));
        let out = Output::capturing();

        assert!(matches!(
            try_self_update(&sym, &out),
            Err(UpdateError::InvokeCargo {
                operation: "search",
                ..
            })
        ));
        let messages = out.captured().join("\n");
        assert!(messages.contains("could not check for updates: failed to run cargo search"));
        assert!(!messages.contains("missing-cargo-secret-canary"));
    }

    #[test]
    fn legacy_public_signatures_remain_available() {
        let _: fn(&Symposium) -> anyhow::Result<semver::Version> = latest_version;
        let _: fn(&Symposium) -> anyhow::Result<Option<semver::Version>> = check_upgrade;
        let _: fn(&Symposium, &Output) -> anyhow::Result<()> = self_update;
    }
}
