//! Per-workspace persistent state.
//!
//! Stored under `~/.symposium/cache/workspaces/<name>-<hash>/state.json`.
//! The directory name uses the workspace root's final component for
//! readability and an 8-hex-char SHA-256 prefix of the full path for
//! uniqueness.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Symposium;

/// A workspace-state failure with its original source and a safe diagnostic.
///
/// Normal displays never include parser input or unrestricted source messages.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkspaceStateError {
    /// The workspace root could not be resolved to a stable cache key.
    #[error("failed to resolve workspace root {}", path.display())]
    Canonicalize {
        /// The workspace root being resolved.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// An existing state file could not be read.
    #[error("failed to read workspace state {}", path.display())]
    Read {
        /// The state file being read.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The state file is not valid workspace-state JSON.
    #[error("failed to parse workspace state {}", path.display())]
    Parse {
        /// The rejected state file.
        path: PathBuf,
        /// The parser failure, retained without displaying the rejected input.
        #[source]
        source: serde_json::Error,
    },
    /// State could not be serialized before writing.
    #[error("failed to serialize workspace state")]
    Serialize {
        /// The serializer failure, including unsupported native path values.
        #[source]
        source: serde_json::Error,
    },
    /// The state directory could not be created.
    #[error("failed to create workspace state directory {}", path.display())]
    CreateDirectory {
        /// The required state directory.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// A sibling temporary file could not be prepared.
    #[error("failed to create temporary workspace state in {}", path.display())]
    CreateTemporary {
        /// The state directory.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The complete replacement could not be written.
    #[error("failed to write temporary workspace state for {}", path.display())]
    WriteTemporary {
        /// The intended destination.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The temporary replacement could not be synchronized before persistence.
    #[error("failed to synchronize temporary workspace state for {}", path.display())]
    SynchronizeTemporary {
        /// The intended destination.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The prepared replacement could not replace the destination.
    #[error("failed to replace workspace state {}", path.display())]
    Persist {
        /// The intended destination.
        path: PathBuf,
        /// The underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// An input timestamp could not be inspected.
    #[error("failed to inspect workspace state input {}", path.display())]
    Probe {
        /// The workspace input being inspected.
        path: PathBuf,
        /// The concrete timestamp failure.
        #[source]
        source: Box<crate::pm::WorkspaceError>,
    },
    /// Cargo workspace discovery failed.
    #[error("failed to discover Cargo workspace from {}", path.display())]
    Discover {
        /// The working directory used for discovery.
        path: PathBuf,
        /// The concrete workspace discovery failure.
        #[source]
        source: Box<crate::pm::WorkspaceError>,
    },
}

/// The last successfully observed workspace sync inputs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// The recorded Cargo lockfile timestamp, in seconds since the Unix epoch.
    #[serde(default, rename = "last-sync-lock-mtime")]
    pub last_sync_lock_mtime: Option<u64>,

    /// The recorded optional battery-pack timestamp, in seconds since the Unix epoch.
    #[serde(default, rename = "last-sync-battery-pack-mtime")]
    pub last_sync_battery_pack_mtime: Option<u64>,

    /// The root used by the last successful workspace synchronization.
    #[serde(default, rename = "workspace-root")]
    pub workspace_root: Option<PathBuf>,
}

impl WorkspaceState {
    /// Load state through the best-effort root 0.6 compatibility adapter.
    ///
    /// Failures are diagnosed and return defaults. If root canonicalization
    /// fails, retain the original path-derived cache key. Use [`Self::try_load`]
    /// to distinguish missing state from an operational or format failure.
    pub fn load(sym: &Symposium, workspace_root: &Path) -> Self {
        match load_state(&compatibility_state_file_path(sym, workspace_root)) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(error = %error, "workspace state unavailable; using defaults");
                Self::default()
            }
        }
    }

    /// Load state, treating a missing state file as an empty state.
    ///
    /// # Errors
    /// Returns root resolution, state read, or JSON parsing failures.
    pub fn try_load(sym: &Symposium, workspace_root: &Path) -> Result<Self, WorkspaceStateError> {
        load_state(&state_file_path(sym, workspace_root)?)
    }

    /// Save through the best-effort root 0.6 compatibility adapter.
    ///
    /// Failures are diagnosed. If root canonicalization fails, retain the
    /// original path-derived cache key. Use [`Self::try_save`] for typed failures.
    pub fn save(&self, sym: &Symposium, workspace_root: &Path) {
        if let Err(error) = self.save_state(&compatibility_state_file_path(sym, workspace_root)) {
            tracing::warn!(error = %error, "workspace state was not saved");
        }
    }

    /// Atomically replace state only after successful serialization and writing.
    /// Replacement commits at the cache path, replacing a destination symlink
    /// itself rather than writing to its target.
    ///
    /// # Errors
    /// Returns root resolution, serialization, directory, write, synchronization,
    /// or replacement failures. A failed replacement retains the previous file.
    pub fn try_save(
        &self,
        sym: &Symposium,
        workspace_root: &Path,
    ) -> Result<(), WorkspaceStateError> {
        self.save_state(&state_file_path(sym, workspace_root)?)
    }

    fn save_state(&self, path: &Path) -> Result<(), WorkspaceStateError> {
        let contents = serde_json::to_vec_pretty(self)
            .map_err(|source| WorkspaceStateError::Serialize { source })?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| WorkspaceStateError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|source| {
            WorkspaceStateError::CreateTemporary {
                path: parent.to_path_buf(),
                source,
            }
        })?;
        temporary
            .write_all(&contents)
            .map_err(|source| WorkspaceStateError::WriteTemporary {
                path: path.to_path_buf(),
                source,
            })?;
        temporary.as_file_mut().sync_all().map_err(|source| {
            WorkspaceStateError::SynchronizeTemporary {
                path: path.to_path_buf(),
                source,
            }
        })?;
        temporary
            .persist(path)
            .map_err(|error| WorkspaceStateError::Persist {
                path: path.to_path_buf(),
                source: error.error,
            })?;
        Ok(())
    }

    /// Check freshness through the root 0.6 compatibility adapter.
    ///
    /// A failed probe is diagnosed and always makes the state stale.
    pub fn sync_is_fresh(&self, workspace_root: &Path) -> bool {
        match self.try_sync_is_fresh(workspace_root) {
            Ok(fresh) => fresh,
            Err(error) => {
                tracing::warn!(error = %error, "workspace state freshness is unknown; treating as stale");
                false
            }
        }
    }

    /// Check whether both recorded input timestamps still match.
    ///
    /// # Errors
    /// Returns a timestamp probe failure rather than treating it as absence.
    pub fn try_sync_is_fresh(&self, workspace_root: &Path) -> Result<bool, WorkspaceStateError> {
        // Cargo.lock must have been seen at least once — if we've never
        // synced, the cache is unconditionally stale.
        if self.last_sync_lock_mtime.is_none() {
            return Ok(false);
        }
        if !mtime_matches(
            self.last_sync_lock_mtime,
            &workspace_root.join("Cargo.lock"),
        )? {
            return Ok(false);
        }
        if !mtime_matches(
            self.last_sync_battery_pack_mtime,
            &workspace_root.join("battery-pack.toml"),
        )? {
            return Ok(false);
        }
        Ok(true)
    }

    /// Record inputs through the best-effort root 0.6 compatibility adapter.
    ///
    /// Failed probes are diagnosed and preserve the previous snapshot.
    pub fn record_sync(&mut self, workspace_root: &Path) {
        if let Err(error) = self.try_record_sync(workspace_root) {
            tracing::warn!(error = %error, "workspace sync timestamps were not recorded");
        }
    }

    /// Record both input timestamps together after both probes succeed.
    ///
    /// # Errors
    /// Returns a timestamp probe failure without changing the saved snapshot.
    pub fn try_record_sync(&mut self, workspace_root: &Path) -> Result<(), WorkspaceStateError> {
        let lock_mtime = probe_mtime(&workspace_root.join("Cargo.lock"))?;
        let battery_pack_mtime = probe_mtime(&workspace_root.join("battery-pack.toml"))?;
        self.last_sync_lock_mtime = lock_mtime;
        self.last_sync_battery_pack_mtime = battery_pack_mtime;
        Ok(())
    }
}

/// Find the workspace root via `cargo locate-project --workspace`.
///
/// Root 0.6 compatibility adapter: failures are diagnosed and return `None`.
/// Use [`try_find_workspace_root`] to distinguish absence from discovery failure.
pub fn find_workspace_root(sym: &Symposium, cwd: &Path) -> Option<PathBuf> {
    match try_find_workspace_root(sym, cwd) {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(error = %error, "Cargo workspace discovery failed");
            None
        }
    }
}

/// Find the root using the configured Cargo executable and Cargo's discovery.
///
/// # Errors
/// Returns filesystem, process, or invalid discovery-output failures.
pub fn try_find_workspace_root(
    sym: &Symposium,
    cwd: &Path,
) -> Result<Option<PathBuf>, WorkspaceStateError> {
    crate::pm::try_find_workspace_root(cwd, sym.cargo_override()).map_err(|source| {
        WorkspaceStateError::Discover {
            path: cwd.to_path_buf(),
            source: Box::new(source),
        }
    })
}

/// Check whether the cached mtime matches the current file state.
/// A missing file matches a `None` cached mtime (both absent = fresh).
/// A missing file with a `Some` cached mtime means the file was deleted = stale.
fn mtime_matches(cached: Option<u64>, path: &Path) -> Result<bool, WorkspaceStateError> {
    Ok(cached == probe_mtime(path)?)
}

fn probe_mtime(path: &Path) -> Result<Option<u64>, WorkspaceStateError> {
    crate::pm::try_file_mtime(path).map_err(|source| WorkspaceStateError::Probe {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

fn load_state(path: &Path) -> Result<WorkspaceState, WorkspaceStateError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceState::default());
        }
        Err(source) => {
            return Err(WorkspaceStateError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_str(&contents).map_err(|source| WorkspaceStateError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn state_file_path(sym: &Symposium, workspace_root: &Path) -> Result<PathBuf, WorkspaceStateError> {
    // Canonicalize so that symlink variants (e.g. /var vs /private/var on
    // macOS) hash to the same cache directory.
    let canonical =
        fs::canonicalize(workspace_root).map_err(|source| WorkspaceStateError::Canonicalize {
            path: workspace_root.to_path_buf(),
            source,
        })?;
    Ok(state_file_path_for_root(sym, &canonical))
}

fn compatibility_state_file_path(sym: &Symposium, workspace_root: &Path) -> PathBuf {
    match state_file_path(sym, workspace_root) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(error = %error, "using unresolved workspace root for legacy state cache");
            state_file_path_for_root(sym, workspace_root)
        }
    }
}

fn state_file_path_for_root(sym: &Symposium, workspace_root: &Path) -> PathBuf {
    sym.cache_dir()
        .join("workspaces")
        .join(crate::pm::workspace_dir_name(workspace_root))
        .join("state.json")
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;
    use crate::pm::{file_mtime, workspace_dir_name};

    #[test]
    fn missing_state_is_default_but_invalid_and_unreadable_state_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::from_dir(tmp.path());
        let workspace = tmp.path().join("project");
        fs::create_dir(&workspace).unwrap();

        let missing = WorkspaceState::try_load(&sym, &workspace).unwrap();
        assert!(missing.last_sync_lock_mtime.is_none());
        assert!(missing.last_sync_battery_pack_mtime.is_none());
        assert!(missing.workspace_root.is_none());

        let path = state_file_path(&sym, &workspace).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let rejected = br#"{"last-sync-lock-mtime":"private-state-canary"}"#;
        fs::write(&path, rejected).unwrap();
        let error = WorkspaceState::try_load(&sym, &workspace).unwrap_err();
        assert!(matches!(error, WorkspaceStateError::Parse { .. }));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert!(!error.to_string().contains("private-state-canary"));
        assert_eq!(fs::read(&path).unwrap(), rejected);
        assert!(
            WorkspaceState::load(&sym, &workspace)
                .workspace_root
                .is_none()
        );
        assert_eq!(fs::read(&path).unwrap(), rejected);

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let error = WorkspaceState::try_load(&sym, &workspace).unwrap_err();
        assert!(matches!(error, WorkspaceStateError::Read { .. }));
        assert!(error.source().unwrap().is::<std::io::Error>());
    }

    #[test]
    fn failed_replacement_preserves_destination_and_can_be_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::from_dir(tmp.path());
        let workspace = tmp.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let path = state_file_path(&sym, &workspace).unwrap();
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("sentinel"), b"preserved").unwrap();
        let state = WorkspaceState {
            last_sync_lock_mtime: Some(123),
            workspace_root: Some(workspace.clone()),
            ..WorkspaceState::default()
        };

        let error = state.try_save(&sym, &workspace).unwrap_err();
        assert!(matches!(error, WorkspaceStateError::Persist { .. }));
        assert!(error.source().unwrap().is::<std::io::Error>());
        assert_eq!(fs::read(path.join("sentinel")).unwrap(), b"preserved");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

        fs::remove_dir_all(&path).unwrap();
        state.try_save(&sym, &workspace).unwrap();
        let loaded = WorkspaceState::try_load(&sym, &workspace).unwrap();
        assert_eq!(loaded.last_sync_lock_mtime, Some(123));
        assert_eq!(loaded.workspace_root, Some(workspace));
    }

    #[test]
    fn canonicalization_failure_is_typed_and_legacy_path_fallback_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::from_dir(tmp.path());
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let workspace = blocker.join("project");
        assert!(matches!(
            WorkspaceState::try_load(&sym, &workspace),
            Err(WorkspaceStateError::Canonicalize { .. })
        ));
        assert!(matches!(
            WorkspaceState::default().try_save(&sym, &workspace),
            Err(WorkspaceStateError::Canonicalize { .. })
        ));
        let fallback = state_file_path_for_root(&sym, &workspace);
        assert!(!fallback.exists());

        let state = WorkspaceState {
            last_sync_lock_mtime: Some(12),
            ..WorkspaceState::default()
        };
        state.save(&sym, &workspace);
        assert_eq!(
            WorkspaceState::load(&sym, &workspace).last_sync_lock_mtime,
            Some(12)
        );
    }

    #[test]
    fn failed_timestamp_probe_is_an_error_and_legacy_freshness_is_false() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("not-a-directory");
        fs::write(&blocked, "blocked").unwrap();
        for workspace in [blocked.clone(), blocked.join("missing/deeper")] {
            let mut state = WorkspaceState {
                last_sync_lock_mtime: Some(123),
                last_sync_battery_pack_mtime: Some(456),
                workspace_root: None,
            };
            let original = fs::metadata(workspace.join("Cargo.lock")).unwrap_err();
            let error = state.try_sync_is_fresh(&workspace).unwrap_err();
            let WorkspaceStateError::Probe { source, .. } = error else {
                panic!("expected a timestamp probe error");
            };
            let crate::pm::WorkspaceError::Io { source, .. } = *source else {
                panic!("expected an I/O error");
            };
            assert_eq!(source.kind(), original.kind());
            assert_eq!(source.raw_os_error(), original.raw_os_error());
            assert!(!state.sync_is_fresh(&workspace));
            assert!(matches!(
                state.try_record_sync(&workspace),
                Err(WorkspaceStateError::Probe { .. })
            ));
            state.record_sync(&workspace);
            assert_eq!(state.last_sync_lock_mtime, Some(123));
            assert_eq!(state.last_sync_battery_pack_mtime, Some(456));
        }
    }

    #[cfg(unix)]
    #[test]
    fn failed_second_probe_never_matches_absence_or_partially_updates_state() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();
        fs::write(workspace.join("Cargo.lock"), "# lock").unwrap();
        std::os::unix::fs::symlink("battery-pack.toml", workspace.join("battery-pack.toml"))
            .unwrap();
        let mut state = WorkspaceState {
            last_sync_lock_mtime: file_mtime(&workspace.join("Cargo.lock")),
            last_sync_battery_pack_mtime: None,
            workspace_root: None,
        };
        assert!(matches!(
            state.try_sync_is_fresh(workspace),
            Err(WorkspaceStateError::Probe { .. })
        ));
        assert!(!state.sync_is_fresh(workspace));

        state.last_sync_lock_mtime = Some(123);
        assert!(state.try_record_sync(workspace).is_err());
        assert_eq!(state.last_sync_lock_mtime, Some(123));
        assert_eq!(state.last_sync_battery_pack_mtime, None);
    }

    #[cfg(unix)]
    #[test]
    fn serialization_failure_preserves_existing_state_bytes() {
        use std::os::unix::ffi::OsStringExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::from_dir(tmp.path());
        let workspace = tmp.path().join("project");
        fs::create_dir(&workspace).unwrap();
        WorkspaceState::default()
            .try_save(&sym, &workspace)
            .unwrap();
        let path = state_file_path(&sym, &workspace).unwrap();
        let original = fs::read(&path).unwrap();
        let state = WorkspaceState {
            workspace_root: Some(std::ffi::OsString::from_vec(vec![0xff]).into()),
            ..WorkspaceState::default()
        };

        let error = state.try_save(&sym, &workspace).unwrap_err();
        assert!(matches!(error, WorkspaceStateError::Serialize { .. }));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn saving_replaces_a_destination_symlink_without_writing_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::from_dir(tmp.path());
        let workspace = tmp.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let path = state_file_path(&sym, &workspace).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = tmp.path().join("unrelated-state");
        fs::write(&target, b"preserved").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        WorkspaceState::default()
            .try_save(&sym, &workspace)
            .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"preserved");
        assert!(!fs::symlink_metadata(&path).unwrap().is_symlink());
        WorkspaceState::try_load(&sym, &workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_discovery_uses_cargo_override_and_keeps_rejection_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("Cargo.toml"), "[workspace]\n").unwrap();
        let cargo = tmp.path().join("cargo-stub");
        fs::write(&cargo, "#!/bin/sh\nprintf '%s/Cargo.toml\\n' \"$PWD\"\n").unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o700)).unwrap();
        let mut sym = Symposium::from_dir(tmp.path());
        sym.set_cargo_override(cargo.clone());
        let found = try_find_workspace_root(&sym, &workspace).unwrap().unwrap();
        assert_eq!(
            fs::canonicalize(found).unwrap(),
            fs::canonicalize(&workspace).unwrap()
        );

        fs::write(
            &cargo,
            "#!/bin/sh\nprintf 'private-process-canary' >&2\nexit 23\n",
        )
        .unwrap();
        let error = try_find_workspace_root(&sym, &workspace).unwrap_err();
        assert!(matches!(error, WorkspaceStateError::Discover { .. }));
        assert!(error.source().is_some());
        assert!(!error.to_string().contains("private-process-canary"));
        assert!(find_workspace_root(&sym, &workspace).is_none());
    }

    #[test]
    fn workspace_dir_name_uses_tail_and_hash() {
        let name = workspace_dir_name(Path::new("/users/dev/symposium"));
        assert!(name.starts_with("symposium-"));
        assert_eq!(name.len(), "symposium-".len() + 8);
    }

    #[test]
    fn different_paths_same_tail_get_different_hashes() {
        let a = workspace_dir_name(Path::new("/home/alice/myproject"));
        let b = workspace_dir_name(Path::new("/home/bob/myproject"));
        assert_ne!(a, b);
        assert!(a.starts_with("myproject-"));
        assert!(b.starts_with("myproject-"));
    }

    #[test]
    fn roundtrip_save_load() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::from_dir(tmp.path());
        let workspace = tmp.path().join("project");
        fs::create_dir_all(&workspace).unwrap();

        let state = WorkspaceState {
            last_sync_lock_mtime: Some(1234567890),
            last_sync_battery_pack_mtime: Some(9876543210),
            workspace_root: Some(workspace.clone()),
        };
        state.save(&sym, &workspace);

        let loaded = WorkspaceState::load(&sym, &workspace);
        assert_eq!(loaded.last_sync_lock_mtime, Some(1234567890));
        assert_eq!(loaded.last_sync_battery_pack_mtime, Some(9876543210));
        assert_eq!(loaded.workspace_root, Some(workspace));
    }

    #[test]
    fn sync_is_fresh_when_mtime_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("Cargo.lock"), "# lock").unwrap();

        let lock_mtime = file_mtime(&workspace.join("Cargo.lock")).unwrap();
        let state = WorkspaceState {
            last_sync_lock_mtime: Some(lock_mtime),
            last_sync_battery_pack_mtime: None,
            workspace_root: None,
        };
        assert!(state.sync_is_fresh(&workspace));
    }

    #[test]
    fn sync_is_fresh_with_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("Cargo.lock"), "# lock").unwrap();
        fs::write(workspace.join("battery-pack.toml"), "# bp").unwrap();

        let lock_mtime = file_mtime(&workspace.join("Cargo.lock")).unwrap();
        let bp_mtime = file_mtime(&workspace.join("battery-pack.toml")).unwrap();
        let state = WorkspaceState {
            last_sync_lock_mtime: Some(lock_mtime),
            last_sync_battery_pack_mtime: Some(bp_mtime),
            workspace_root: None,
        };
        assert!(state.sync_is_fresh(&workspace));
    }

    #[test]
    fn sync_is_stale_when_lock_mtime_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("Cargo.lock"), "# lock").unwrap();

        let state = WorkspaceState {
            last_sync_lock_mtime: Some(0),
            last_sync_battery_pack_mtime: None,
            workspace_root: None,
        };
        assert!(!state.sync_is_fresh(&workspace));
    }

    #[test]
    fn sync_is_stale_when_battery_pack_added() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("Cargo.lock"), "# lock").unwrap();
        fs::write(workspace.join("battery-pack.toml"), "# bp").unwrap();

        let lock_mtime = file_mtime(&workspace.join("Cargo.lock")).unwrap();
        // Cached state doesn't know about battery-pack.toml yet
        let state = WorkspaceState {
            last_sync_lock_mtime: Some(lock_mtime),
            last_sync_battery_pack_mtime: None,
            workspace_root: None,
        };
        assert!(!state.sync_is_fresh(&workspace));
    }

    #[test]
    fn sync_is_stale_when_battery_pack_mtime_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("Cargo.lock"), "# lock").unwrap();
        fs::write(workspace.join("battery-pack.toml"), "# bp").unwrap();

        let lock_mtime = file_mtime(&workspace.join("Cargo.lock")).unwrap();
        let state = WorkspaceState {
            last_sync_lock_mtime: Some(lock_mtime),
            last_sync_battery_pack_mtime: Some(0),
            workspace_root: None,
        };
        assert!(!state.sync_is_fresh(&workspace));
    }

    #[test]
    fn sync_is_stale_when_no_lock_file() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project");
        fs::create_dir_all(&workspace).unwrap();

        let state = WorkspaceState {
            last_sync_lock_mtime: Some(1234567890),
            last_sync_battery_pack_mtime: None,
            workspace_root: None,
        };
        assert!(!state.sync_is_fresh(&workspace));
    }

    #[test]
    fn sync_is_stale_when_no_cached_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("Cargo.lock"), "# lock").unwrap();

        let state = WorkspaceState::default();
        assert!(!state.sync_is_fresh(&workspace));
    }

    #[test]
    fn find_workspace_root_finds_workspace_from_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(src.join("lib.rs"), "").unwrap();

        let sym = Symposium::from_dir(tmp.path());
        // Canonicalize both sides: macOS resolves /var -> /private/var, and on Windows
        // canonicalization adds a `\\?\` prefix cargo's output lacks.
        let found = find_workspace_root(&sym, &src).map(|pth| fs::canonicalize(pth).unwrap());
        let expected = fs::canonicalize(&root).unwrap();
        assert_eq!(found, Some(expected));
    }

    #[test]
    fn find_workspace_root_returns_none_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let subdir = tmp.path().join("no-rust-here");
        fs::create_dir_all(&subdir).unwrap();

        let sym = Symposium::from_dir(tmp.path());
        let found = find_workspace_root(&sym, &subdir);
        assert!(found.is_none());
    }
}
