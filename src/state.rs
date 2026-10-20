//! Persistent state: `state.toml` in the config directory.
//!
//! Tracks the version of the binary that last touched the config/cache
//! directories and the last time an update check was performed.
//! Future versions can use a version mismatch to trigger migrations.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const STATE_FILE: &str = "state.toml";
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Runtime version of this binary, baked in at compile time.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Failure to read, decode, or save operational state.
///
/// Display messages identify the operation and path without including TOML
/// input. The original parser or filesystem error remains available as a source.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateError {
    /// An existing state file could not be read as UTF-8.
    #[error("failed to read operational state at {path}")]
    Read {
        /// The state file being read.
        path: PathBuf,
        /// The original filesystem or UTF-8 decoding error.
        #[source]
        source: std::io::Error,
    },
    /// State contents did not match the supported TOML format.
    #[error("failed to parse operational state at {path}")]
    Parse {
        /// The state file being decoded.
        path: PathBuf,
        /// The original TOML parser error.
        #[source]
        source: toml::de::Error,
    },
    /// State could not be encoded as TOML.
    #[error("failed to serialize operational state for {path}")]
    Serialize {
        /// The destination state file.
        path: PathBuf,
        /// The original TOML serializer error.
        #[source]
        source: toml::ser::Error,
    },
    /// The encoded state could not be written.
    #[error("failed to write operational state at {path}")]
    Write {
        /// The destination state file.
        path: PathBuf,
        /// The original filesystem error.
        #[source]
        source: std::io::Error,
    },
}

/// Version and update-throttle information persisted for this installation.
#[derive(Debug, Serialize)]
pub struct State {
    /// Semver of the binary that last wrote this file.
    pub version: String,

    /// Last time we checked crates.io for a newer version.
    #[serde(default, rename = "last-update-check")]
    pub last_update_check: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct RawState {
    version: String,
    #[serde(default, rename = "last-update-check")]
    last_update_check: Option<DateTime<Utc>>,
}

impl RawState {
    fn validate(self) -> State {
        State {
            version: self.version,
            last_update_check: self.last_update_check,
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION.to_string(),
            last_update_check: None,
        }
    }
}

/// Load `state.toml`, distinguishing an absent file from a failed read or parse.
///
/// # Errors
///
/// Returns [`StateError::Read`] for non-`NotFound` filesystem or UTF-8 failures,
/// and [`StateError::Parse`] for malformed TOML.
pub fn try_load(config_dir: &Path) -> Result<Option<State>, StateError> {
    let path = state_path(config_dir);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(StateError::Read { path, source }),
    };
    toml::from_str::<RawState>(&contents)
        .map(|raw| Some(raw.validate()))
        .map_err(|source| StateError::Parse { path, source })
}

/// Load `state.toml`, returning `None` if absent or unreadable/unparseable.
///
/// Legacy adapter retained until the root crate's 0.6 release. Read and parse
/// failures produce a warning; use [`try_load`] to distinguish them from absence.
pub fn load(config_dir: &Path) -> Option<State> {
    match try_load(config_dir) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(%error, "operational state unavailable (continuing)");
            None
        }
    }
}

fn save(config_dir: &Path, state: &State) -> Result<(), StateError> {
    let path = state_path(config_dir);
    let contents = toml::to_string_pretty(state).map_err(|source| StateError::Serialize {
        path: path.clone(),
        source,
    })?;
    fs::write(&path, contents).map_err(|source| StateError::Write { path, source })
}

/// Write the current binary version, preserving other supported state fields.
///
/// # Errors
///
/// Returns the state read, parse, serialization, or write failure. Existing
/// unreadable or malformed state is not overwritten. `config_dir` must exist.
pub fn try_stamp(config_dir: &Path) -> Result<(), StateError> {
    let mut state = try_load(config_dir)?.unwrap_or_default();
    state.version = CURRENT_VERSION.to_string();
    save(config_dir, &state)
}

/// Write the current binary version, preserving other supported state fields.
///
/// Legacy adapter retained until the root crate's 0.6 release. Failed operations
/// produce a warning; use [`try_stamp`] to observe whether the write succeeded.
pub fn stamp(config_dir: &Path) {
    if let Err(error) = try_stamp(config_dir) {
        tracing::warn!(%error, "failed to stamp operational state (continuing)");
    }
}

/// Ensure `state.toml` exists and reflects the running binary version.
///
/// Returns the previously recorded version (if any) so callers can
/// decide whether a migration is needed.
///
/// # Errors
///
/// Returns a state read, parse, serialization, or write failure. Existing
/// unreadable or malformed state is not overwritten. `config_dir` must exist.
pub fn try_ensure_current(config_dir: &Path) -> Result<Option<String>, StateError> {
    let (previous, write_result) = ensure_loaded_current(config_dir, try_load(config_dir)?);
    write_result?;
    Ok(previous)
}

fn ensure_loaded_current(
    config_dir: &Path,
    prev: Option<State>,
) -> (Option<String>, Result<(), StateError>) {
    let prev_version = prev.as_ref().map(|s| s.version.clone());

    let needs_write = match &prev {
        None => true,
        Some(s) => s.version != CURRENT_VERSION,
    };

    let write_result = if needs_write {
        let mut state = prev.unwrap_or_default();
        state.version = CURRENT_VERSION.to_string();
        save(config_dir, &state)
    } else {
        Ok(())
    };

    (prev_version, write_result)
}

/// Ensure operational state reflects the running version, returning its old version.
///
/// Legacy adapter retained until the root crate's 0.6 release. Failures produce
/// a warning; a successfully read previous version is returned even when its
/// replacement fails. Use [`try_ensure_current`] to observe persistence failures.
pub fn ensure_current(config_dir: &Path) -> Option<String> {
    let previous = match try_load(config_dir) {
        Ok(previous) => previous,
        Err(error) => {
            tracing::warn!(%error, "failed to update operational version state (continuing)");
            return None;
        }
    };
    let (previous, write_result) = ensure_loaded_current(config_dir, previous);
    if let Err(error) = write_result {
        tracing::warn!(%error, "failed to update operational version state (continuing)");
    }
    previous
}

/// Whether enough time has elapsed since the last update check.
///
/// Missing state allows a check; a timestamp in the future keeps it throttled.
///
/// # Errors
///
/// Returns the state read or parse failure.
pub fn try_should_check_for_update(config_dir: &Path) -> Result<bool, StateError> {
    let Some(state) = try_load(config_dir)? else {
        return Ok(true);
    };
    let Some(last_check) = state.last_update_check else {
        return Ok(true);
    };
    let elapsed = Utc::now().signed_duration_since(last_check);
    Ok(elapsed.to_std().unwrap_or(Duration::ZERO) >= UPDATE_CHECK_INTERVAL)
}

/// Whether enough time has elapsed since the last update check.
///
/// Legacy adapter retained until the root crate's 0.6 release. Failed reads or
/// parses produce a warning and allow a check; use [`try_should_check_for_update`]
/// to choose a different failure policy.
pub fn should_check_for_update(config_dir: &Path) -> bool {
    match try_should_check_for_update(config_dir) {
        Ok(should_check) => should_check,
        Err(error) => {
            tracing::warn!(%error, "update throttle unavailable (allowing check)");
            true
        }
    }
}

/// Record that an update check just happened, preserving the version field.
///
/// # Errors
///
/// Returns a state read, parse, serialization, or write failure. Existing
/// unreadable or malformed state is not overwritten. `config_dir` must exist.
pub fn try_record_update_check(config_dir: &Path) -> Result<(), StateError> {
    let mut state = try_load(config_dir)?.unwrap_or_default();
    state.last_update_check = Some(Utc::now());
    save(config_dir, &state)
}

/// Record that an update check just happened.
///
/// Legacy adapter retained until the root crate's 0.6 release. Failed operations
/// produce a warning; use [`try_record_update_check`] to observe persistence.
pub fn record_update_check(config_dir: &Path) {
    if let Err(error) = try_record_update_check(config_dir) {
        tracing::warn!(%error, "failed to save update-check timestamp (continuing)");
    }
}

fn state_path(config_dir: &Path) -> PathBuf {
    config_dir.join(STATE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_is_optional_but_read_failures_have_sources() {
        use std::error::Error;

        let tmp = tempfile::tempdir().unwrap();
        assert!(try_load(tmp.path()).unwrap().is_none());
        assert!(try_should_check_for_update(tmp.path()).unwrap());

        let path = state_path(tmp.path());
        fs::create_dir(&path).unwrap();
        let error = try_load(tmp.path()).unwrap_err();
        assert!(matches!(&error, StateError::Read { path: failed, .. } if failed == &path));
        assert!(error.source().is_some());
        assert!(path.is_dir());
        assert!(load(tmp.path()).is_none());
    }

    #[test]
    fn malformed_state_is_preserved_and_not_exposed_in_display() {
        use std::error::Error;

        let tmp = tempfile::tempdir().unwrap();
        let path = state_path(tmp.path());
        let contents = b"version = ['state-value-canary']\n";
        fs::write(&path, contents).unwrap();

        let error = try_load(tmp.path()).unwrap_err();
        assert!(matches!(&error, StateError::Parse { path: failed, .. } if failed == &path));
        assert!(error.source().unwrap().is::<toml::de::Error>());
        assert!(!error.to_string().contains("state-value-canary"));
        assert!(try_stamp(tmp.path()).is_err());
        assert!(try_ensure_current(tmp.path()).is_err());
        assert!(try_record_update_check(tmp.path()).is_err());
        assert!(try_should_check_for_update(tmp.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), contents);

        stamp(tmp.path());
        assert!(ensure_current(tmp.path()).is_none());
        record_update_check(tmp.path());
        assert!(load(tmp.path()).is_none());
        assert!(should_check_for_update(tmp.path()));
        assert_eq!(fs::read(&path).unwrap(), contents);
    }

    #[test]
    fn invalid_utf8_state_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = state_path(tmp.path());
        let contents = b"version = '\xff'\n";
        fs::write(&path, contents).unwrap();

        let error = try_stamp(tmp.path()).unwrap_err();
        assert!(matches!(error, StateError::Read { source, .. }
            if source.kind() == std::io::ErrorKind::InvalidData));
        assert!(try_ensure_current(tmp.path()).is_err());
        assert!(try_record_update_check(tmp.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), contents);
    }

    #[test]
    fn failed_state_writes_are_reported_and_retryable() {
        use std::error::Error;

        let tmp = tempfile::tempdir().unwrap();
        let missing_dir = tmp.path().join("not-created-yet");
        let path = state_path(&missing_dir);

        let error = try_stamp(&missing_dir).unwrap_err();
        assert!(matches!(&error, StateError::Write { path: failed, source }
            if failed == &path && source.kind() == std::io::ErrorKind::NotFound));
        assert!(error.source().is_some());
        assert!(try_ensure_current(&missing_dir).is_err());
        assert!(try_record_update_check(&missing_dir).is_err());
        assert!(!missing_dir.exists());

        fs::create_dir(&missing_dir).unwrap();
        assert!(try_ensure_current(&missing_dir).unwrap().is_none());
        try_record_update_check(&missing_dir).unwrap();
        let state = try_load(&missing_dir).unwrap().unwrap();
        assert_eq!(state.version, CURRENT_VERSION);
        assert!(state.last_update_check.is_some());
        assert!(!try_should_check_for_update(&missing_dir).unwrap());
    }

    #[test]
    fn future_update_timestamp_remains_throttled() {
        let tmp = tempfile::tempdir().unwrap();
        let state = State {
            last_update_check: Some(Utc::now() + chrono::Duration::hours(25)),
            ..State::default()
        };
        save(tmp.path(), &state).unwrap();

        assert!(!try_should_check_for_update(tmp.path()).unwrap());
        assert!(!should_check_for_update(tmp.path()));
    }

    #[test]
    fn stamp_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load(tmp.path()).is_none());
        stamp(tmp.path());
        let state = load(tmp.path()).expect("should exist after stamp");
        assert_eq!(state.version, CURRENT_VERSION);
    }

    #[test]
    fn ensure_current_returns_none_on_first_run() {
        let tmp = tempfile::tempdir().unwrap();
        let prev = ensure_current(tmp.path());
        assert!(prev.is_none());
        let state = load(tmp.path()).expect("should be written");
        assert_eq!(state.version, CURRENT_VERSION);
    }

    #[test]
    fn ensure_current_returns_previous_version() {
        let tmp = tempfile::tempdir().unwrap();
        let old = State {
            version: "0.1.0".to_string(),
            last_update_check: None,
        };
        fs::write(
            tmp.path().join(STATE_FILE),
            toml::to_string_pretty(&old).unwrap(),
        )
        .unwrap();

        let prev = ensure_current(tmp.path());
        assert_eq!(prev.as_deref(), Some("0.1.0"));
        let state = load(tmp.path()).expect("should be updated");
        assert_eq!(state.version, CURRENT_VERSION);
    }

    #[test]
    fn loaded_previous_version_survives_a_failed_stamp() {
        use std::error::Error;

        let tmp = tempfile::tempdir().unwrap();
        let path = state_path(tmp.path());
        let old_contents = b"version = '0.1.0'\n";
        fs::write(&path, old_contents).unwrap();
        let previous = try_load(tmp.path()).unwrap();

        let preserved = tmp.path().join("previous-state.toml");
        fs::rename(&path, &preserved).unwrap();
        fs::create_dir(&path).unwrap();
        let sentinel = path.join("keep.txt");
        fs::write(&sentinel, b"preserve this content").unwrap();

        let (previous_version, write_result) = ensure_loaded_current(tmp.path(), previous);
        assert_eq!(previous_version.as_deref(), Some("0.1.0"));
        let error = write_result.unwrap_err();
        assert!(matches!(&error, StateError::Write { path: failed, .. } if failed == &path));
        assert!(error.source().is_some());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve this content");
        assert_eq!(fs::read(&preserved).unwrap(), old_contents);
        assert!(try_ensure_current(tmp.path()).is_err());
    }

    #[test]
    fn should_check_when_no_state() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(should_check_for_update(tmp.path()));
    }

    #[test]
    fn should_check_when_never_checked() {
        let tmp = tempfile::tempdir().unwrap();
        stamp(tmp.path());
        assert!(should_check_for_update(tmp.path()));
    }

    #[test]
    fn should_not_check_when_recently_checked() {
        let tmp = tempfile::tempdir().unwrap();
        record_update_check(tmp.path());
        assert!(!should_check_for_update(tmp.path()));
    }

    #[test]
    fn should_check_when_interval_elapsed() {
        let tmp = tempfile::tempdir().unwrap();
        let state = State {
            last_update_check: Some(Utc::now() - chrono::Duration::hours(25)),
            ..State::default()
        };
        save(tmp.path(), &state).unwrap();
        assert!(should_check_for_update(tmp.path()));
    }

    #[test]
    fn stamp_preserves_last_update_check() {
        let tmp = tempfile::tempdir().unwrap();
        record_update_check(tmp.path());
        let before = load(tmp.path()).unwrap().last_update_check;
        assert!(before.is_some());

        stamp(tmp.path());
        let after = load(tmp.path()).unwrap().last_update_check;
        assert_eq!(before, after);
    }
}
