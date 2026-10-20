//! Workspace dependency types and resolution.
//!
//! [`WorkspaceDeps`] resolves and caches the crate dependency graph for a Cargo
//! workspace. Plugin binaries obtain one via
//! [`SymposiumDirs::workspace_deps`](crate::dirs::SymposiumDirs::workspace_deps).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use cargo_metadata::{CargoOpt, MetadataCommand};
use serde::{Deserialize, Serialize};

/// A workspace, metadata, timestamp, or derived-cache operation failed.
///
/// Parser and subprocess sources may contain untrusted content. Display reports
/// only the operation and path; inspect sources structurally instead of printing
/// an unrestricted error chain.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// A required filesystem operation failed.
    #[error("failed to {operation} at {}", path.display())]
    Io {
        /// The fixed operation label.
        operation: &'static str,
        /// The affected filesystem path.
        path: PathBuf,
        /// The original filesystem error.
        source: std::io::Error,
    },
    /// A derived workspace cache was not valid JSON/schema input.
    #[error("failed to parse workspace cache at {}", path.display())]
    CacheParse {
        /// The rejected cache file.
        path: PathBuf,
        /// The original parser failure.
        source: serde_json::Error,
    },
    /// Cache data could not be serialized before touching its destination.
    #[error("failed to serialize workspace cache at {}", path.display())]
    CacheSerialize {
        /// The destination cache file.
        path: PathBuf,
        /// The original serializer failure.
        source: serde_json::Error,
    },
    /// A cache referred to a different workspace than the requested one.
    #[error("workspace cache refers to a different workspace")]
    CacheRootMismatch,
    /// Cargo could not be invoked.
    #[error("failed to invoke cargo workspace discovery")]
    CargoInvoke {
        /// The requested working directory.
        cwd: PathBuf,
        /// The original process-creation failure.
        source: std::io::Error,
    },
    /// Cargo rejected workspace discovery despite a present manifest.
    #[error("cargo workspace discovery failed ({status})")]
    CargoLocate {
        /// Cargo's unsuccessful process status.
        status: std::process::ExitStatus,
    },
    /// Cargo discovery returned non-UTF-8 text.
    #[error("cargo workspace discovery returned non-UTF-8 output")]
    CargoOutput {
        /// The original decoding failure.
        source: std::string::FromUtf8Error,
    },
    /// Cargo discovery did not return one manifest path.
    #[error("cargo workspace discovery returned an invalid manifest path")]
    InvalidCargoOutput,
    /// Cargo could not resolve or decode workspace metadata.
    #[error("failed to resolve cargo workspace metadata")]
    Metadata {
        /// The original metadata failure.
        source: Box<cargo_metadata::Error>,
    },
    /// Metadata omitted the dependency graph required for resolution.
    #[error("cargo workspace metadata omitted its dependency graph")]
    MissingResolve,
    /// A file timestamp precedes the supported Unix-epoch representation.
    #[error("file timestamp precedes the Unix epoch at {}", path.display())]
    Timestamp {
        /// The file whose timestamp could not be represented.
        path: PathBuf,
        /// The original timestamp-range failure.
        source: std::time::SystemTimeError,
    },
}

/// A crate in the workspace's direct dependency graph.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceCrate {
    /// The crate name as published (e.g. `"serde"`, `"tokio"`).
    pub name: String,
    /// The resolved version.
    pub version: semver::Version,
    /// Local source path for path dependencies (unpublished, so `fetch` must
    /// resolve them locally). `None` for registry crates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The crate's extracted source directory, from `cargo metadata`'s
    /// `manifest_path`. Populated for *every* resolved dependency — registry
    /// crates included, since `cargo metadata` already extracted them — so the
    /// source can be inspected or fetched without a fresh cargo probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_dir: Option<PathBuf>,
}

impl WorkspaceCrate {
    /// A crate whose source lives at `path` (a path dependency), or a registry
    /// crate when `path` is `None`. `source_dir` defaults to `path`; use
    /// [`with_source_dir`](Self::with_source_dir) for a registry crate whose
    /// extracted source is known.
    pub fn new(name: String, version: semver::Version, path: Option<PathBuf>) -> Self {
        Self {
            name,
            version,
            source_dir: path.clone(),
            path,
        }
    }

    /// Set the extracted source directory (from `cargo metadata`).
    pub fn with_source_dir(mut self, source_dir: Option<PathBuf>) -> Self {
        self.source_dir = source_dir;
        self
    }
}

/// The resolved workspace: root path + dependency list + member directories.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedWorkspace {
    /// Workspace root directory.
    pub root: PathBuf,
    /// Direct dependencies of all workspace members.
    pub crates: Vec<WorkspaceCrate>,
    /// Manifest directories of the workspace's member packages. Backs
    /// workspace-plugin discovery (a member directory may define a plugin).
    pub members: Vec<PathBuf>,
}

/// On-disk cache format. Adding a field is a compatible cache bump: an old
/// cache file fails to deserialize, reads as a miss, and is rebuilt.
#[derive(Serialize, Deserialize)]
struct DiskCache {
    lock_mtime: u64,
    root: PathBuf,
    crates: Vec<WorkspaceCrate>,
    members: Vec<PathBuf>,
}

/// Lazy, cached workspace dependency resolver.
///
/// The first `load()` checks the disk cache (keyed on `Cargo.lock` mtime); on
/// miss it runs `cargo metadata` (expensive) and writes through to disk. The
/// result is memoized in a [`OnceLock`], so resolution happens at most once per
/// instance and every method reads through a shared `&self` — which lets a
/// single resolver be shared (held as an [`Arc`] by a
/// [`CargoPm`](crate::pm::CargoPm), and read directly by core code that needs
/// the workspace root or members) rather than each caller resolving its own.
pub struct WorkspaceDeps {
    cwd: PathBuf,
    dirs: crate::dirs::SymposiumDirs,
    cached: OnceLock<Result<Option<Arc<LoadedWorkspace>>, Arc<WorkspaceError>>>,
}

impl WorkspaceDeps {
    pub fn new(cwd: impl Into<PathBuf>, dirs: &crate::dirs::SymposiumDirs) -> Self {
        Self {
            cwd: cwd.into(),
            dirs: dirs.clone(),
            cached: OnceLock::new(),
        }
    }

    /// A pre-resolved resolver for tests: skips `cargo metadata` and returns
    /// exactly `crates` (rooted at `root`, no members).
    #[cfg(test)]
    pub(crate) fn fixture(root: impl Into<PathBuf>, crates: Vec<WorkspaceCrate>) -> Arc<Self> {
        let root = root.into();
        let cached = OnceLock::from(Ok(Some(Arc::new(LoadedWorkspace {
            root: root.clone(),
            crates,
            members: Vec::new(),
        }))));
        Arc::new(Self {
            cwd: root,
            dirs: crate::dirs::SymposiumDirs::new(PathBuf::new(), PathBuf::new(), None),
            cached,
        })
    }

    /// A resolver pre-set to "no workspace" — it never runs `cargo metadata`.
    /// Backs [`detached_managers`](crate::config::Symposium::detached_managers)
    /// for workspace-independent operations (registry listing, crates.io
    /// search).
    pub fn detached() -> Self {
        Self {
            cwd: PathBuf::new(),
            dirs: crate::dirs::SymposiumDirs::new(PathBuf::new(), PathBuf::new(), None),
            cached: OnceLock::from(Ok(None)),
        }
    }

    /// The working directory this was constructed with.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Load (or return cached) workspace metadata.
    /// Returns `None` if not inside a Cargo workspace.
    ///
    /// Legacy best-effort adapter retained until root 0.6. Operational failures
    /// are diagnosed safely; new callers should use [`Self::try_load`].
    pub fn load(&self) -> Option<&Arc<LoadedWorkspace>> {
        match self.try_load() {
            Ok(workspace) => workspace,
            Err(error) => {
                tracing::warn!(%error, "workspace resolution failed");
                None
            }
        }
    }

    /// Resolve once, distinguishing no workspace from an operational failure.
    ///
    /// # Errors
    /// Returns the shared typed failure from the memoized resolution. Sharing
    /// preserves the original source without cloning filesystem errors or
    /// resolving the same instance concurrently. Optional cache failures are
    /// safely diagnosed and rebuilt from Cargo metadata.
    pub fn try_load(&self) -> Result<Option<&Arc<LoadedWorkspace>>, Arc<WorkspaceError>> {
        match self.cached.get_or_init(|| self.resolve().map_err(Arc::new)) {
            Ok(workspace) => Ok(workspace.as_ref()),
            Err(error) => Err(Arc::clone(error)),
        }
    }

    /// Convenience: workspace root, or `None` if not in a workspace.
    pub fn workspace_root(&self) -> Option<&Path> {
        self.load().map(|w| w.root.as_path())
    }

    /// Resolve the workspace root without hiding resolution failures.
    ///
    /// # Errors
    /// Returns the same shared failure as [`Self::try_load`].
    pub fn try_workspace_root(&self) -> Result<Option<&Path>, Arc<WorkspaceError>> {
        Ok(self.try_load()?.map(|workspace| workspace.root.as_path()))
    }

    /// Convenience: crate list (empty slice if not in a workspace).
    pub fn crates(&self) -> &[WorkspaceCrate] {
        match self.load() {
            Some(w) => &w.crates,
            None => &[],
        }
    }

    /// Resolve dependency crates, using an empty slice only outside a workspace.
    ///
    /// # Errors
    /// Returns the same shared failure as [`Self::try_load`].
    pub fn try_crates(&self) -> Result<&[WorkspaceCrate], Arc<WorkspaceError>> {
        Ok(match self.try_load()? {
            Some(workspace) => &workspace.crates,
            None => &[],
        })
    }

    /// The one-time resolution: disk cache, then `cargo metadata` on miss.
    fn resolve(&self) -> Result<Option<Arc<LoadedWorkspace>>, WorkspaceError> {
        let Some(root) = try_find_workspace_root(&self.cwd, self.dirs.cargo_override.as_deref())?
        else {
            return Ok(None);
        };
        let canonical = fs::canonicalize(&root).map_err(|source| WorkspaceError::Io {
            operation: "canonicalize workspace root",
            path: root,
            source,
        })?;
        let cache_dir = self
            .dirs
            .cache_dir
            .join("workspaces")
            .join(workspace_dir_name(&canonical));
        match try_disk_cache(&cache_dir, &canonical) {
            Ok(Some(loaded)) => return Ok(Some(Arc::new(loaded))),
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "workspace cache rejected; resolving metadata"),
        }
        let loaded = load_workspace(&self.cwd, self.dirs.cargo_override.as_deref())?;
        if let Err(error) = write_disk_cache(&cache_dir, &loaded) {
            tracing::warn!(%error, "failed to persist derived workspace cache");
        }
        Ok(Some(Arc::new(loaded)))
    }
}

fn try_disk_cache(
    ws_cache_dir: &Path,
    workspace_root: &Path,
) -> Result<Option<LoadedWorkspace>, WorkspaceError> {
    let cache_file = ws_cache_dir.join("workspace-deps.json");
    let contents = match fs::read_to_string(&cache_file) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(WorkspaceError::Io {
                operation: "read workspace cache",
                path: cache_file,
                source,
            });
        }
    };
    let cached: DiskCache =
        serde_json::from_str(&contents).map_err(|source| WorkspaceError::CacheParse {
            path: cache_file,
            source,
        })?;
    let cached_root = fs::canonicalize(&cached.root).map_err(|source| WorkspaceError::Io {
        operation: "canonicalize cached workspace root",
        path: cached.root.clone(),
        source,
    })?;
    if cached_root != workspace_root {
        return Err(WorkspaceError::CacheRootMismatch);
    }

    // Validate: Cargo.lock mtime must match.
    let lock_path = cached.root.join("Cargo.lock");
    let Some(current_mtime) = try_file_mtime(&lock_path)? else {
        return Ok(None);
    };
    if current_mtime != cached.lock_mtime {
        return Ok(None);
    }

    Ok(Some(LoadedWorkspace {
        root: cached.root,
        crates: cached.crates,
        members: cached.members,
    }))
}

fn write_disk_cache(ws_cache_dir: &Path, loaded: &LoadedWorkspace) -> Result<(), WorkspaceError> {
    let lock_path = loaded.root.join("Cargo.lock");
    let Some(mtime) = try_file_mtime(&lock_path)? else {
        return Ok(());
    };

    let disk = DiskCache {
        lock_mtime: mtime,
        root: loaded.root.clone(),
        crates: loaded.crates.clone(),
        members: loaded.members.clone(),
    };

    let path = ws_cache_dir.join("workspace-deps.json");
    let contents =
        serde_json::to_vec_pretty(&disk).map_err(|source| WorkspaceError::CacheSerialize {
            path: path.clone(),
            source,
        })?;
    fs::create_dir_all(ws_cache_dir).map_err(|source| WorkspaceError::Io {
        operation: "create workspace cache directory",
        path: ws_cache_dir.into(),
        source,
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(ws_cache_dir).map_err(|source| WorkspaceError::Io {
            operation: "create workspace cache temporary file",
            path: ws_cache_dir.into(),
            source,
        })?;
    temporary
        .write_all(&contents)
        .map_err(|source| WorkspaceError::Io {
            operation: "write workspace cache temporary file",
            path: path.clone(),
            source,
        })?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|source| WorkspaceError::Io {
            operation: "synchronize workspace cache temporary file",
            path: path.clone(),
            source,
        })?;
    temporary
        .persist(&path)
        .map_err(|error| WorkspaceError::Io {
            operation: "replace workspace cache",
            path,
            source: error.error,
        })?;
    Ok(())
}

/// Find workspace root via `cargo locate-project --workspace`.
/// Fast (~10-50ms), no dep resolution.
///
/// # Errors
/// Returns a typed failure for inaccessible ancestor manifests, Cargo invocation,
/// rejected discovery, or invalid output. A directory with no ancestor Cargo
/// manifest is the ordinary `Ok(None)` case.
pub fn try_find_workspace_root(
    cwd: &Path,
    cargo_path: Option<&Path>,
) -> Result<Option<PathBuf>, WorkspaceError> {
    let canonical = fs::canonicalize(cwd).map_err(|source| WorkspaceError::Io {
        operation: "canonicalize workspace discovery directory",
        path: cwd.into(),
        source,
    })?;
    let mut has_manifest = false;
    for ancestor in canonical.ancestors() {
        let path = ancestor.join("Cargo.toml");
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                has_manifest = true;
                break;
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(WorkspaceError::Io {
                    operation: "inspect ancestor Cargo manifest",
                    path,
                    source,
                });
            }
        }
    }
    if !has_manifest {
        return Ok(None);
    }
    let cargo = cargo_path.unwrap_or(Path::new("cargo"));
    let output = std::process::Command::new(cargo)
        .args(["locate-project", "--workspace", "--message-format=plain"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|source| WorkspaceError::CargoInvoke {
            cwd: cwd.into(),
            source,
        })?;

    if !output.status.success() {
        return Err(WorkspaceError::CargoLocate {
            status: output.status,
        });
    }

    let manifest = String::from_utf8(output.stdout)
        .map_err(|source| WorkspaceError::CargoOutput { source })?;
    let manifest = manifest.trim();
    if manifest.lines().count() != 1
        || Path::new(manifest).file_name() != Some(std::ffi::OsStr::new("Cargo.toml"))
    {
        return Err(WorkspaceError::InvalidCargoOutput);
    }
    let manifest = if Path::new(manifest).is_absolute() {
        PathBuf::from(manifest)
    } else {
        canonical.join(manifest)
    };
    manifest
        .parent()
        .map(|parent| Some(parent.to_path_buf()))
        .ok_or(WorkspaceError::InvalidCargoOutput)
}

/// Compute a human-readable, unique directory name for a workspace root.
///
/// Format: `<tail>-<8-hex-sha256-prefix>` where `tail` is the final path
/// component. Used to derive per-workspace cache directories.
pub fn workspace_dir_name(workspace_root: &Path) -> String {
    use sha2::{Digest, Sha256};

    let tail = workspace_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");

    let digest = Sha256::digest(workspace_root.as_os_str().as_encoded_bytes());
    let mut hash = String::with_capacity(8);
    for byte in &digest[..4] {
        hash.push_str(&format!("{byte:02x}"));
    }

    format!("{tail}-{hash}")
}

/// Get a file's mtime as seconds since the Unix epoch.
/// Returns `None` if the file doesn't exist or its metadata can't be read.
/// Legacy adapter retained until root 0.6; use [`try_file_mtime`] for freshness.
pub fn file_mtime(path: &Path) -> Option<u64> {
    match try_file_mtime(path) {
        Ok(mtime) => mtime,
        Err(error @ WorkspaceError::Timestamp { .. }) => {
            tracing::warn!(%error, "legacy file timestamp was rounded to the Unix epoch");
            Some(0)
        }
        Err(error) => {
            tracing::warn!(%error, "file timestamp probe failed");
            None
        }
    }
}

/// Probe a file timestamp while distinguishing absence from an operational error.
///
/// # Errors
/// Returns the original metadata/modification/range failure. A missing file is
/// represented as `Ok(None)` only when its nearest existing ancestor is an
/// accessible directory; an invalid parent or dangling entry remains an error.
pub fn try_file_mtime(path: &Path) -> Result<Option<u64>, WorkspaceError> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(source) => {
            // Windows also reports NotFound when a parent component is a file.
            // Confirm genuine absence before allowing a missing timestamp to
            // participate in a cache freshness comparison.
            if source.kind() == std::io::ErrorKind::NotFound
                && missing_file_is_absent(path).map_err(|source| WorkspaceError::Io {
                    operation: "inspect missing file path ancestors",
                    path: path.into(),
                    source,
                })?
            {
                return Ok(None);
            }
            return Err(WorkspaceError::Io {
                operation: "read file metadata",
                path: path.into(),
                source,
            });
        }
    };
    let mtime = meta.modified().map_err(|source| WorkspaceError::Io {
        operation: "read modification time",
        path: path.into(),
        source,
    })?;
    let duration = mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|source| WorkspaceError::Timestamp {
            path: path.into(),
            source,
        })?;
    Ok(Some(duration.as_secs()))
}

fn missing_file_is_absent(path: &Path) -> std::io::Result<bool> {
    for ancestor in path.ancestors() {
        let candidate = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor
        };
        match fs::symlink_metadata(candidate) {
            // An existing target whose metadata could not be followed, such as
            // a dangling symlink, is not an absent file.
            Ok(_) if ancestor == path => return Ok(false),
            Ok(_) => {
                return match fs::metadata(candidate) {
                    Ok(metadata) => Ok(metadata.is_dir()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(error),
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// Run `cargo metadata` and extract workspace root + direct deps.
fn load_workspace(
    cwd: &Path,
    cargo_path: Option<&Path>,
) -> Result<LoadedWorkspace, WorkspaceError> {
    let mut cmd = MetadataCommand::new();
    cmd.features(CargoOpt::AllFeatures).current_dir(cwd);
    if let Some(path) = cargo_path {
        cmd.cargo_path(path);
    }
    let metadata = cmd.exec().map_err(|source| WorkspaceError::Metadata {
        source: Box::new(source),
    })?;

    let root = metadata.workspace_root.clone().into_std_path_buf();

    let resolve = metadata
        .resolve
        .as_ref()
        .ok_or(WorkspaceError::MissingResolve)?;

    let ws_members: HashSet<_> = metadata.workspace_members.iter().collect();
    let mut direct_dep_ids: HashSet<&cargo_metadata::PackageId> = HashSet::new();

    for node in &resolve.nodes {
        if ws_members.contains(&node.id) {
            for dep in &node.deps {
                direct_dep_ids.insert(&dep.pkg);
            }
        }
    }

    let path_overrides: HashMap<String, PathBuf> = metadata
        .packages
        .iter()
        .filter(|p| p.source.is_none())
        .filter_map(|p| {
            p.manifest_path
                .parent()
                .map(|dir| (p.name.clone(), dir.into()))
        })
        .collect();

    let mut crates: Vec<_> = metadata
        .packages
        .iter()
        .filter(|p| direct_dep_ids.contains(&p.id) && !ws_members.contains(&p.id))
        .map(|p| WorkspaceCrate {
            path: path_overrides.get(&p.name).cloned(),
            source_dir: p.manifest_path.parent().map(|dir| dir.into()),
            name: p.name.to_string(),
            version: p.version.clone(),
        })
        .collect();

    crates.sort_by(|a, b| a.name.cmp(&b.name));
    crates.dedup_by(|a, b| a.name == b.name);

    let mut members: Vec<PathBuf> = metadata
        .packages
        .iter()
        .filter(|p| ws_members.contains(&p.id))
        .filter_map(|p| p.manifest_path.parent().map(|dir| dir.into()))
        .collect();
    members.sort();
    members.dedup();

    Ok(LoadedWorkspace {
        root,
        crates,
        members,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    fn loaded(root: &Path) -> LoadedWorkspace {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("Cargo.lock"), "# synthetic lock\n").unwrap();
        LoadedWorkspace {
            root: fs::canonicalize(root).unwrap(),
            crates: Vec::new(),
            members: Vec::new(),
        }
    }

    #[test]
    fn cache_roundtrip_distinguishes_missing_stale_and_wrong_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let loaded = loaded(&temp.path().join("workspace"));
        let cache = temp.path().join("cache");
        assert!(try_disk_cache(&cache, &loaded.root).unwrap().is_none());
        write_disk_cache(&cache, &loaded).unwrap();
        assert_eq!(
            try_disk_cache(&cache, &loaded.root).unwrap().unwrap().root,
            loaded.root
        );
        let other = temp.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(matches!(
            try_disk_cache(&cache, &other),
            Err(WorkspaceError::CacheRootMismatch)
        ));
        fs::remove_file(loaded.root.join("Cargo.lock")).unwrap();
        assert!(try_disk_cache(&cache, &loaded.root).unwrap().is_none());
    }

    #[test]
    fn invalid_cache_preserves_source_and_safe_display_without_rewriting_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("workspace-deps.json");
        let bytes = b"\"synthetic-secret-canary\"";
        fs::write(&path, bytes).unwrap();
        let error = try_disk_cache(temp.path(), temp.path()).unwrap_err();
        assert!(matches!(error, WorkspaceError::CacheParse { .. }));
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert!(!error.to_string().contains("synthetic-secret-canary"));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn failed_cache_replace_keeps_the_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let loaded = loaded(&temp.path().join("workspace"));
        let cache = temp.path().join("cache");
        let destination = cache.join("workspace-deps.json");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("keep"), b"existing bytes").unwrap();
        let error = write_disk_cache(&cache, &loaded).unwrap_err();
        assert!(matches!(
            error,
            WorkspaceError::Io {
                operation: "replace workspace cache",
                ..
            }
        ));
        assert_eq!(
            fs::read(destination.join("keep")).unwrap(),
            b"existing bytes"
        );
        assert_eq!(fs::read_dir(cache).unwrap().count(), 1);
    }

    #[test]
    fn file_mtime_distinguishes_absence_from_a_failed_probe() {
        let temp = tempfile::tempdir().unwrap();
        for path in [
            temp.path().join("missing"),
            temp.path().join("missing/deeper/Cargo.lock"),
        ] {
            assert!(try_file_mtime(&path).unwrap().is_none());
            assert!(missing_file_is_absent(&path).unwrap());
        }
        let file = temp.path().join("file");
        fs::write(&file, b"present").unwrap();
        assert!(try_file_mtime(&file).unwrap().is_some());
        for path in [file.join("child"), file.join("missing/deeper/Cargo.lock")] {
            let original = fs::metadata(&path).unwrap_err();
            let error = try_file_mtime(&path).unwrap_err();
            let WorkspaceError::Io {
                operation,
                path: error_path,
                source,
            } = &error
            else {
                panic!("expected an I/O error");
            };
            assert_eq!(*operation, "read file metadata");
            assert_eq!(error_path, &path);
            assert_eq!(source.kind(), original.kind());
            assert_eq!(source.raw_os_error(), original.raw_os_error());
            assert!(error.source().unwrap().is::<std::io::Error>());
            assert!(!missing_file_is_absent(&path).unwrap());
        }
    }

    #[test]
    fn detached_and_nonworkspace_are_normal_absence_without_running_cargo() {
        assert!(WorkspaceDeps::detached().try_load().unwrap().is_none());
        let temp = tempfile::tempdir().unwrap();
        assert!(
            try_find_workspace_root(temp.path(), Some(Path::new("missing-cargo")))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn required_package_dependencies_preserve_a_failed_workspace_lookup() {
        use crate::pm::{CargoPm, PackageManager, PackageManagerError};
        let temp = tempfile::tempdir().unwrap();
        let dirs =
            crate::dirs::SymposiumDirs::new(temp.path().into(), temp.path().join("cache"), None);
        let resolver = Arc::new(WorkspaceDeps::new(
            temp.path().join("missing-project"),
            &dirs,
        ));
        let manager = CargoPm::new(Arc::clone(&resolver));
        let error = manager.try_list_deps().await.unwrap_err();
        let PackageManagerError::Workspace { source } = error else {
            panic!("required lookup lost its concrete workspace failure");
        };
        assert!(matches!(source.as_ref(), WorkspaceError::Io { .. }));
        assert!(Arc::ptr_eq(&source, &resolver.try_load().unwrap_err()));
        assert!(!dirs.cache_dir.exists());
    }

    #[cfg(unix)]
    fn cargo_stub(root: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        let path = root.join("cargo-stub");
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn cargo_discovery_distinguishes_spawn_status_encoding_and_shape_failures() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let missing = temp.path().join("missing-cargo");
        let result = try_find_workspace_root(temp.path(), Some(&missing));
        assert!(
            matches!(&result, Err(WorkspaceError::CargoInvoke { .. })),
            "expected an invocation failure, got {result:?}"
        );
        let error = result.unwrap_err();
        assert!(error.source().unwrap().is::<std::io::Error>());
        let path = cargo_stub(temp.path(), "exit 37");
        let result = try_find_workspace_root(temp.path(), Some(&path));
        assert!(
            matches!(&result, Err(WorkspaceError::CargoLocate { .. })),
            "expected an unsuccessful discovery status, got {result:?}"
        );
        let path = cargo_stub(temp.path(), "printf '\\377'");
        let result = try_find_workspace_root(temp.path(), Some(&path));
        assert!(
            matches!(&result, Err(WorkspaceError::CargoOutput { .. })),
            "expected non-UTF-8 discovery output, got {result:?}"
        );
        let path = cargo_stub(temp.path(), "printf 'unexpected-output\\n'");
        let result = try_find_workspace_root(temp.path(), Some(&path));
        assert!(
            matches!(&result, Err(WorkspaceError::InvalidCargoOutput)),
            "expected an invalid discovery path, got {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn metadata_failure_remains_memoized_as_a_typed_error_instead_of_absence() {
        let temp = tempfile::tempdir().unwrap();
        let cargo = cargo_stub(
            temp.path(),
            "case \"$1\" in\nlocate-project) printf '%s/Cargo.toml\\n' \"$PWD\";;\nmetadata) printf 'synthetic-secret-canary\\n';;\nesac",
        );
        let dirs = crate::dirs::SymposiumDirs::new(
            temp.path().into(),
            temp.path().join("cache"),
            Some(cargo),
        );
        let resolver = WorkspaceDeps::new(temp.path(), &dirs);
        let first = resolver.try_load().unwrap_err();
        assert!(matches!(first.as_ref(), WorkspaceError::Metadata { .. }));
        assert!(!first.to_string().contains("synthetic-secret-canary"));
        let second = resolver.try_load().unwrap_err();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(resolver.load().is_none());
    }
}
