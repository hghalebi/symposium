//! Rust crate source fetching and management.
//!
//! Registry crate fetching delegates to `cargo fetch` via a dummy temporary
//! package (see [`probe`]) rather than hitting `crates.io` HTTP endpoints
//! directly. Local path dependencies short-circuit through the workspace's
//! path overrides and never touch the registry.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;

use crate::pm::{WorkspaceCrate, WorkspaceDeps, WorkspaceError};

mod probe;

/// A crate source could not be resolved or fetched.
///
/// Display messages omit input and parser/subprocess output. Operation details
/// and concrete sources remain available for structured inspection.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CrateSourceError {
    /// Workspace resolution failed before the registry probe could begin.
    #[error("failed to resolve workspace dependencies before fetching crate sources")]
    Workspace {
        /// The shared, concrete workspace-resolution failure.
        #[source]
        source: Arc<WorkspaceError>,
    },
    /// The blocking probe task did not complete.
    #[error("cargo probe task failed")]
    JoinProbe {
        /// The task failure.
        #[source]
        source: tokio::task::JoinError,
    },
    /// The temporary probe directory could not be created.
    #[error("failed to create the temporary cargo probe directory")]
    CreateProbe {
        /// The filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// A file or directory needed by the probe could not be written.
    #[error("failed to {operation} for the cargo probe")]
    WriteProbe {
        /// The attempted operation, selected by the implementation.
        operation: &'static str,
        /// The affected path.
        path: PathBuf,
        /// The filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// Cargo's home directory could not be located.
    #[error("failed to locate cargo home")]
    LocateCargoHome {
        /// The directory-resolution failure.
        #[source]
        source: std::io::Error,
    },
    /// The configured Cargo executable could not be invoked.
    #[error("failed to invoke cargo fetch")]
    InvokeCargo {
        /// The configured executable, preserved without lossy conversion.
        executable: OsString,
        /// The process-creation failure.
        #[source]
        source: std::io::Error,
    },
    /// Cargo ran but failed to fetch the dependency.
    #[error("cargo fetch failed ({status})")]
    CargoFetchFailed {
        /// The requested crate.
        crate_name: String,
        /// The requested version requirement.
        version_req: String,
        /// Cargo's unsuccessful exit status.
        status: ExitStatus,
    },
    /// Cargo metadata could not be produced or decoded.
    #[error("failed to read cargo metadata")]
    CargoMetadata {
        /// The concrete Cargo metadata failure.
        #[source]
        source: cargo_metadata::Error,
    },
    /// The manifest path returned by Cargo had no parent directory.
    #[error("cargo metadata returned a manifest path without a parent")]
    ManifestWithoutParent {
        /// The manifest path returned by Cargo.
        path: PathBuf,
    },
    /// The probe's lockfile could not be read.
    #[error("failed to read the cargo probe lockfile")]
    ReadLockfile {
        /// The affected lockfile.
        path: PathBuf,
        /// The filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The probe's lockfile was malformed.
    #[error("failed to parse the cargo probe lockfile")]
    ParseLockfile {
        /// The affected lockfile.
        path: PathBuf,
        /// The parser failure, which may contain source text.
        #[source]
        source: toml::de::Error,
    },
    /// Neither Cargo metadata nor the lockfile identified the requested crate.
    #[error("requested crate was absent from cargo metadata and the probe lockfile")]
    CrateNotLocked {
        /// The requested crate.
        crate_name: String,
    },
    /// Cargo's extracted-registry directory could not be read.
    #[error("failed to read cargo registry sources")]
    ReadRegistrySources {
        /// The affected registry-source directory.
        path: PathBuf,
        /// The filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// Reading an extracted-registry directory entry failed.
    #[error("failed to read a cargo registry source entry")]
    ReadRegistryEntry {
        /// The registry-source directory being traversed.
        path: PathBuf,
        /// The filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The locked crate had no completed extraction in Cargo's source cache.
    #[error("locked crate source was not found in the cargo registry cache")]
    SourceNotFound {
        /// The locked crate name.
        name: String,
        /// The locked crate version.
        version: String,
        /// The registry-source directory searched.
        registry_root: PathBuf,
    },
}

/// Normalize a crate name for hyphen/underscore-insensitive comparison.
///
/// Cargo treats `foo-bar` and `foo_bar` as the same crate name (published
/// name on crates.io vs. Rust module identifier), so any name-equality check
/// against a user-supplied query should go through this normalization.
pub(crate) fn normalize_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

/// Result of fetching a crate's sources.
#[derive(Debug, Clone)]
pub struct FetchResult {
    /// The canonical crate name (e.g. `serde_json` even if queried as `serde-json`).
    pub name: String,
    /// The exact version that was fetched.
    pub version: String,
    /// Path to the crate sources on disk.
    pub path: PathBuf,
}

/// Builder for accessing Rust crate source code.
pub struct RustCrateFetch<'a> {
    crate_name: String,
    version_spec: Option<String>,
    workspace: &'a WorkspaceDeps,
}

impl<'a> RustCrateFetch<'a> {
    /// Create a new fetch request for the given crate name.
    pub fn new(name: &str, workspace: &'a WorkspaceDeps) -> Self {
        Self {
            crate_name: name.to_string(),
            version_spec: None,
            workspace,
        }
    }

    /// Specify a version constraint (e.g. `"^1.0"`, `"=1.2.3"`).
    pub fn version(mut self, version: &str) -> Self {
        self.version_spec = Some(version.to_string());
        self
    }

    /// Fetch the crate sources, returning a path to the source directory.
    ///
    /// Resolution order:
    /// 1. If the crate is a workspace dependency with a known extracted source
    ///    (and no explicit `--version` was requested), return that directory
    ///    directly — the workspace's own `cargo metadata` already located it,
    ///    for both path *and* registry crates, so no probe is needed.
    /// 2. Otherwise, run `cargo fetch` in a temporary dummy package to
    ///    populate cargo's registry cache, then read `cargo metadata` to get
    ///    the extracted source path under `~/.cargo/registry/src/`.
    ///
    /// # Errors
    /// Workspace resolution failures propagate before any registry probe.
    /// Otherwise returns the probe's typed acquisition failure.
    pub async fn fetch(self) -> Result<FetchResult, CrateSourceError> {
        let workspace = self
            .workspace
            .try_crates()
            .map_err(|source| CrateSourceError::Workspace { source })?;
        // Serve a workspace dependency from its already-extracted source.
        if self.version_spec.is_none() {
            let normalized = normalize_crate_name(&self.crate_name);
            if let Some((wc, dir)) = workspace.iter().find_map(|wc| {
                (normalize_crate_name(&wc.name) == normalized)
                    .then(|| wc.source_dir.as_ref().map(|d| (wc, d)))
                    .flatten()
            }) {
                tracing::debug!(crate_name = %wc.name, path = %dir.display(), "resolved from workspace source");
                return Ok(FetchResult {
                    name: wc.name.clone(),
                    version: wc.version.to_string(),
                    path: dir.clone(),
                });
            }
        }

        let (name, version_req) = self.resolve_registry_spec(workspace);
        probe::fetch_via_cargo(&name, &version_req).await
    }

    /// Choose the `(name, version_req)` pair to put in the probe package's
    /// dependency entry when going through the registry.
    ///
    /// Precedence:
    /// 1. Explicit `--version` constraint from the caller.
    /// 2. If the crate is a direct dependency of the current workspace, pin
    ///    to that exact resolved version (`=x.y.z`).
    /// 3. Otherwise, `"*"` — cargo picks the latest compatible version.
    fn resolve_registry_spec(&self, workspace: &[WorkspaceCrate]) -> (String, String) {
        if let Some(spec) = &self.version_spec {
            return (self.crate_name.clone(), spec.clone());
        }

        let normalized = normalize_crate_name(&self.crate_name);
        if let Some(wc) = workspace
            .iter()
            .find(|wc| normalize_crate_name(&wc.name) == normalized)
        {
            return (wc.name.clone(), format!("={}", wc.version));
        }

        (self.crate_name.clone(), "*".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    fn wc(name: &str, version: &str, path: Option<PathBuf>) -> WorkspaceCrate {
        WorkspaceCrate::new(name.to_string(), v(version), path)
    }

    // -- Path override behaviour ---------------------------------------

    #[tokio::test]
    async fn fetch_uses_path_override_for_path_dep() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = WorkspaceDeps::fixture(
            tmp.path(),
            vec![wc("my-crate", "0.1.0", Some(tmp.path().to_path_buf()))],
        );

        let result = RustCrateFetch::new("my-crate", &workspace)
            .fetch()
            .await
            .unwrap();

        assert_eq!(result.name, "my-crate");
        assert_eq!(result.version, "0.1.0");
        assert_eq!(result.path, tmp.path());
    }

    #[tokio::test]
    async fn fetch_path_override_normalizes_hyphens() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = WorkspaceDeps::fixture(
            tmp.path(),
            vec![wc("my_crate", "0.1.0", Some(tmp.path().to_path_buf()))],
        );

        // Query with hyphen, workspace entry uses underscore.
        let result = RustCrateFetch::new("my-crate", &workspace)
            .fetch()
            .await
            .unwrap();

        assert_eq!(result.name, "my_crate");
        assert_eq!(result.path, tmp.path());
    }

    // -- Registry spec resolution (pure, no I/O) -----------------------

    #[test]
    fn registry_spec_prefers_explicit_version() {
        let workspace = WorkspaceDeps::fixture(PathBuf::new(), vec![wc("foo", "1.0.0", None)]);
        let fetch = RustCrateFetch::new("foo", &workspace).version("^2.0");
        let (name, req) = fetch.resolve_registry_spec(workspace.try_crates().unwrap());
        assert_eq!(name, "foo");
        assert_eq!(req, "^2.0");
    }

    #[test]
    fn registry_spec_pins_workspace_version_exactly() {
        let workspace = WorkspaceDeps::fixture(PathBuf::new(), vec![wc("foo", "1.2.3", None)]);
        let fetch = RustCrateFetch::new("foo", &workspace);
        let (name, req) = fetch.resolve_registry_spec(workspace.try_crates().unwrap());
        assert_eq!(name, "foo");
        assert_eq!(req, "=1.2.3");
    }

    #[test]
    fn registry_spec_normalizes_hyphens_against_workspace() {
        let workspace =
            WorkspaceDeps::fixture(PathBuf::new(), vec![wc("serde_json", "1.0.0", None)]);
        let fetch = RustCrateFetch::new("serde-json", &workspace);
        let (name, req) = fetch.resolve_registry_spec(workspace.try_crates().unwrap());
        // Canonical name from the workspace wins.
        assert_eq!(name, "serde_json");
        assert_eq!(req, "=1.0.0");
    }

    #[test]
    fn registry_spec_falls_back_to_wildcard() {
        let workspace = WorkspaceDeps::fixture(PathBuf::new(), Vec::new());
        let fetch = RustCrateFetch::new("foo", &workspace);
        let (name, req) = fetch.resolve_registry_spec(workspace.try_crates().unwrap());
        assert_eq!(name, "foo");
        assert_eq!(req, "*");
    }

    #[test]
    fn registry_spec_is_used_when_version_specified_even_with_path_dep() {
        // Explicit version → path override is skipped → registry spec uses
        // the explicit version.
        let tmp = tempfile::tempdir().unwrap();
        let workspace = WorkspaceDeps::fixture(
            tmp.path(),
            vec![wc("serde", "1.0.210", Some(tmp.path().to_path_buf()))],
        );
        let fetch = RustCrateFetch::new("serde", &workspace).version("=99.99.99");
        let (name, req) = fetch.resolve_registry_spec(workspace.try_crates().unwrap());
        assert_eq!(name, "serde");
        assert_eq!(req, "=99.99.99");
    }

    #[tokio::test]
    async fn missing_workspace_cargo_propagates_before_a_registry_probe() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let dirs = crate::dirs::SymposiumDirs::new(
            tmp.path().join("config"),
            tmp.path().join("cache"),
            Some(tmp.path().join("missing-cargo")),
        );
        let workspace = WorkspaceDeps::new(tmp.path(), &dirs);
        // An invalid requirement also prevents registry I/O if a regression reaches the probe.
        let error = RustCrateFetch::new("missing-crate", &workspace)
            .version("\n")
            .fetch()
            .await
            .unwrap_err();
        let CrateSourceError::Workspace { source } = &error else {
            panic!("workspace failure must propagate before the registry probe");
        };
        assert!(matches!(
            source.as_ref(),
            WorkspaceError::CargoInvoke { source, .. }
                if source.kind() == std::io::ErrorKind::NotFound
        ));
        assert!(Arc::ptr_eq(source, &workspace.try_load().unwrap_err()));
        assert_eq!(
            error.to_string(),
            "failed to resolve workspace dependencies before fetching crate sources"
        );
        assert!(!tmp.path().join("cache").exists());
    }
}
