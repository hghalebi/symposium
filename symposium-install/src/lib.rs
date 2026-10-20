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

//! Acquisition and caching of tool binaries for symposium plugins.
//!
//! This crate provides the machinery to install cargo binaries and clone GitHub
//! repositories into a local cache. It is used by both the main `symposium`
//! binary and by hook handlers that need to invoke external tools.
#![deny(missing_docs)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod git;

/// A source acquisition or executable-resolution failure.
///
/// Display messages omit untrusted values, URLs, command arguments and stderr.
/// Sources and paths remain inspectable; do not log the debug representation or
/// recursively print source chains containing external response data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstallError {
    /// A public source field is invalid at the acquisition boundary.
    #[error("invalid installation field: {field}")]
    InvalidInput {
        /// The fixed field name, without its untrusted value.
        field: &'static str,
    },
    /// A source cannot infer the binary to install.
    #[error("{kind} acquisition requires an executable hint")]
    MissingExecutable {
        /// The fixed source kind requiring an explicit hint.
        kind: &'static str,
    },
    /// Public acquisition fields do not describe a possible location.
    #[error("invalid acquired location: {reason}")]
    InvalidLocation {
        /// A fixed description of the inconsistent field combination.
        reason: &'static str,
    },
    /// A global Cargo installation has no managed script directory.
    #[error("global Cargo installations cannot resolve scripts")]
    GlobalScript,
    /// The registry HTTP client could not be constructed.
    #[error("failed to construct registry HTTP client")]
    Client {
        /// The original HTTP-client error, with its URL removed.
        source: reqwest::Error,
    },
    /// A registry request or response decoding failed.
    #[error("failed to {operation}")]
    RegistryRequest {
        /// The fixed operation being attempted.
        operation: &'static str,
        /// The original request or decoding error, with its URL removed.
        source: reqwest::Error,
    },
    /// The registry returned an unsuccessful HTTP status.
    #[error("registry returned {status} while attempting to {operation}")]
    RegistryStatus {
        /// The fixed operation being attempted.
        operation: &'static str,
        /// The unsuccessful HTTP status.
        status: reqwest::StatusCode,
    },
    /// Binary inference found more than one possible executable.
    #[error("crate has {count} binaries; provide an executable hint")]
    MultipleBinaries {
        /// The number of binary candidates returned by the registry.
        count: usize,
    },
    /// A blocking Cargo installation task could not be joined.
    #[error("failed to join Cargo installation task")]
    JoinCargo {
        /// The original task error.
        source: tokio::task::JoinError,
    },
    /// Cargo or a Git probe could not be invoked.
    #[error("failed to {operation}")]
    InvokeCargo {
        /// The fixed process operation being attempted.
        operation: &'static str,
        /// The original process I/O failure.
        source: std::io::Error,
    },
    /// Cargo or a Git probe returned an unsuccessful process status.
    #[error("failed to {operation}: {status}")]
    CargoFailed {
        /// The fixed process operation being attempted.
        operation: &'static str,
        /// The unsuccessful process status, without command stderr.
        status: std::process::ExitStatus,
    },
    /// Cargo reported success without producing the requested managed binary.
    #[error("Cargo installation did not produce the requested binary")]
    MissingBinary {
        /// The expected binary path, retained for explicit inspection.
        path: PathBuf,
    },
    /// A filesystem operation failed.
    #[error("failed to {operation}")]
    Io {
        /// The fixed filesystem operation being attempted.
        operation: &'static str,
        /// The affected path, retained for explicit inspection.
        path: PathBuf,
        /// The original filesystem failure.
        source: std::io::Error,
    },
    /// A Git source could not be validated, fetched or cached.
    #[error(transparent)]
    Git(#[from] git::GitError),
}

fn install_io(operation: &'static str, path: &Path, source: std::io::Error) -> InstallError {
    InstallError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn safe_component(value: &str) -> bool {
    !value.is_empty() && !matches!(value, "." | "..") && !value.contains(['/', '\\', ':', '\0'])
}

fn validate_relative_path(value: &str) -> Result<(), InstallError> {
    if value.is_empty()
        || value.starts_with(['/', '\\'])
        || value.contains([':', '\0'])
        || value.split(['/', '\\']).any(|part| part == "..")
        || !value
            .split(['/', '\\'])
            .any(|part| !part.is_empty() && part != ".")
    {
        return Err(InstallError::InvalidInput {
            field: "executable or script",
        });
    }
    Ok(())
}

fn validate_source(source: &Source, executable_hint: Option<&str>) -> Result<(), InstallError> {
    match source {
        Source::Cargo(cargo) => {
            if !safe_component(&cargo.crate_name) || cargo.crate_name.starts_with('-') {
                return Err(InstallError::InvalidInput { field: "crate" });
            }
            if let Some(name) = executable_hint {
                validate_relative_path(name)?;
                if !safe_component(name) {
                    return Err(InstallError::InvalidInput {
                        field: "Cargo executable",
                    });
                }
            }
            if (cargo.global || cargo.git.is_some()) && executable_hint.is_none() {
                return Err(InstallError::MissingExecutable {
                    kind: if cargo.global {
                        "global Cargo"
                    } else {
                        "git Cargo"
                    },
                });
            }
            if cargo.git.as_deref().is_some_and(|url| {
                url.trim().is_empty() || url.contains('\0') || url.starts_with('-')
            }) {
                return Err(InstallError::InvalidInput { field: "git" });
            }
            if cargo
                .version
                .as_deref()
                .is_some_and(|version| version.contains('\0'))
            {
                return Err(InstallError::InvalidInput { field: "version" });
            }
        }
        Source::Github(github) => {
            git::try_parse_git_url(&github.url)?;
            if let Some(name) = executable_hint {
                validate_relative_path(name)?;
            }
        }
    }
    Ok(())
}

/// Validate a managed source and selected runnable before acquisition or setup.
///
/// Use this when a script selection is separate from the acquisition executable
/// hint. No-source installations retain their caller's PATH/absolute-path policy.
///
/// # Errors
/// Rejects conflicting choices, invalid managed relative paths, global scripts,
/// invalid source fields, and required Cargo hints before any effects.
pub fn validate_source_runnable(
    source: &Source,
    executable: Option<&str>,
    script: Option<&str>,
) -> Result<(), InstallError> {
    if executable.is_some() && script.is_some() {
        return Err(InstallError::InvalidInput {
            field: "executable and script",
        });
    }
    if let Some(script) = script {
        if matches!(source, Source::Cargo(cargo) if cargo.global) {
            return Err(InstallError::GlobalScript);
        }
        validate_relative_path(script)?;
    }
    validate_source(source, executable)
}

/// Records the crates.io version resolved by the last freshness check, so a
/// `None` acquire can reuse it without re-querying. Lives at
/// `<cache>/binaries/<crate>/current`.
const CURRENT_VERSION_FILENAME: &str = "current";

/// Records the git commit a `cargo + git` binary was last installed from, so a
/// freshness check can skip the reinstall when the branch hasn't moved. Lives
/// inside the binary's version-keyed cache dir.
const COMMIT_SHA_FILENAME: &str = ".commit-sha";

/// Minimal context needed for acquisition.
///
/// Replaces the full `Symposium` config struct — hook handlers construct this
/// directly from environment or hardcoded paths.
#[derive(Debug, Clone)]
pub struct InstallContext {
    cache_dir: PathBuf,
    cargo_bin: Option<PathBuf>,
}

impl InstallContext {
    /// Create a new context rooted at the given cache directory.
    ///
    /// Acquired binaries and cloned repositories are stored under this path.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            cargo_bin: None,
        }
    }

    /// Override the cargo binary used for `cargo install` / `cargo binstall`.
    ///
    /// If not set, the plain `"cargo"` from `$PATH` is used.
    pub fn with_cargo_bin(mut self, path: PathBuf) -> Self {
        self.cargo_bin = Some(path);
        self
    }

    /// The root directory where cached artifacts are stored.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Build a [`Command`](std::process::Command) for the configured cargo binary.
    pub fn cargo_command(&self) -> std::process::Command {
        match &self.cargo_bin {
            Some(path) => std::process::Command::new(path),
            None => std::process::Command::new("cargo"),
        }
    }
}

/// Controls how aggressively cached sources are updated.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
pub enum UpdateLevel {
    /// Debounced: skip the API check if fetched recently.
    #[default]
    None,
    /// Always check freshness via API, but only download if stale.
    Check,
    /// Always re-download regardless of staleness.
    Fetch,
}

/// How to acquire bits onto disk.
///
/// `Source` describes acquisition only: cargo install, github clone. The
/// "what to run" lives separately on the installation as `executable` or
/// `script` (which resolve relative to the acquired source). An installation
/// can omit `Source` entirely — in that case `executable` / `script` are
/// taken as paths on disk and `install_commands` does any setup.
#[non_exhaustive]
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum Source {
    /// Install via `cargo install` or `cargo binstall`.
    Cargo(CargoSource),
    /// Clone from a GitHub repository.
    Github(GithubSource),
}

/// A binary obtained by `cargo install` (with optional binstall fast-path).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CargoSource {
    /// The crate name (on crates.io, or as named in the git repo).
    #[serde(rename = "crate")]
    pub crate_name: String,
    /// Optional version (defaults to latest stable from crates.io; for git
    /// sources, used to derive a cache key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Install from a git URL (`cargo install --git`) instead of crates.io.
    /// When set, the user must specify `executable` on the installation since
    /// crates.io is not consulted to discover binary names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    /// Install into the user's global cargo location (`~/.cargo/bin`) instead
    /// of a symposium-managed cache. The default (`false`) uses
    /// `cargo install --root <symposium-cache>` so binaries don't pollute the
    /// global namespace; hook execution adds the cache `bin/` to `$PATH` so
    /// scripts can still invoke them by name.
    #[serde(default, skip_serializing_if = "is_false")]
    pub global: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl CargoSource {
    /// Create a source for the given crate name, defaulting to the latest
    /// stable version from crates.io.
    pub fn new(crate_name: impl Into<String>) -> Self {
        Self {
            crate_name: crate_name.into(),
            version: None,
            git: None,
            global: false,
        }
    }

    /// Pin to a specific version (e.g. `"1.2.3"`).
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// Install from a git repository URL instead of crates.io.
    ///
    /// When set, an `executable` hint is required since crates.io is not
    /// consulted to discover binary names.
    pub fn with_git(mut self, git_url: impl Into<String>) -> Self {
        self.git = Some(git_url.into());
        self
    }
}

/// A directory of files acquired from a GitHub repository (or subtree).
/// The file to run inside the cloned tree is picked by `executable` /
/// `script` on the installation or hook.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GithubSource {
    /// The GitHub URL to clone.
    #[serde(alias = "git")]
    pub url: String,
}

impl GithubSource {
    /// Create a source for the given GitHub URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

fn platform_binary_exe(binary_name: &str) -> String {
    if cfg!(windows) {
        format!("{}.exe", binary_name)
    } else {
        binary_name.to_string()
    }
}

/// Response from crates.io version endpoint
#[derive(Debug, Deserialize)]
struct CratesIoVersionResponse {
    version: CratesIoVersion,
}

#[derive(Debug, Deserialize)]
struct CratesIoVersion {
    bin_names: Vec<String>,
}

/// Response from crates.io crate endpoint (for getting latest version)
#[derive(Debug, Deserialize)]
struct CratesIoCrateResponse {
    #[serde(rename = "crate")]
    krate: CratesIoCrate,
}

#[derive(Debug, Deserialize)]
struct CratesIoCrate {
    max_stable_version: Option<String>,
    max_version: String,
}

/// Query crates.io for binary names of a crate
async fn query_crate_binaries(
    crate_name: &str,
    version: Option<&str>,
) -> Result<(String, Vec<String>), InstallError> {
    let client = reqwest::Client::builder()
        .user_agent("symposium (https://github.com/symposium-dev/symposium)")
        .build()
        .map_err(|source| InstallError::Client {
            source: source.without_url(),
        })?;
    let version = match version {
        Some(version) => version.to_string(),
        None => {
            let response = client
                .get(format!("https://crates.io/api/v1/crates/{crate_name}"))
                .send()
                .await
                .map_err(|source| InstallError::RegistryRequest {
                    operation: "query crate metadata",
                    source: source.without_url(),
                })?;
            if !response.status().is_success() {
                return Err(InstallError::RegistryStatus {
                    operation: "query crate metadata",
                    status: response.status(),
                });
            }
            let info: CratesIoCrateResponse =
                response
                    .json()
                    .await
                    .map_err(|source| InstallError::RegistryRequest {
                        operation: "decode crate metadata",
                        source: source.without_url(),
                    })?;
            info.krate
                .max_stable_version
                .unwrap_or(info.krate.max_version)
        }
    };
    let response = client
        .get(format!(
            "https://crates.io/api/v1/crates/{crate_name}/{version}"
        ))
        .send()
        .await
        .map_err(|source| InstallError::RegistryRequest {
            operation: "query crate version",
            source: source.without_url(),
        })?;
    if !response.status().is_success() {
        return Err(InstallError::RegistryStatus {
            operation: "query crate version",
            status: response.status(),
        });
    }
    let info: CratesIoVersionResponse =
        response
            .json()
            .await
            .map_err(|source| InstallError::RegistryRequest {
                operation: "decode crate version",
                source: source.without_url(),
            })?;
    Ok((version, info.version.bin_names))
}

/// Install a crate using Cargo binstall, then Cargo install if necessary.
async fn install_cargo_crate(
    ctx: &InstallContext,
    crate_name: &str,
    version: &str,
    binary_name: Option<String>,
    cache_dir: Option<PathBuf>,
    git: Option<String>,
    force: bool,
) -> Result<(), InstallError> {
    let ctx = ctx.clone();
    let crate_name = crate_name.to_string();
    let version = version.to_string();
    tokio::task::spawn_blocking(move || {
        install_cargo_crate_sync(
            &ctx,
            &crate_name,
            &version,
            binary_name,
            cache_dir.as_deref(),
            git,
            force,
        )
    })
    .await
    .map_err(|source| InstallError::JoinCargo { source })?
}

fn cached_path_exists(path: &Path) -> Result<bool, InstallError> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(install_io("inspect cached artifact", path, source)),
    }
}

fn cleanup_other_versions(cache_dir: Option<&Path>) {
    let Some(cache_dir) = cache_dir else { return };
    let Some(parent) = cache_dir.parent() else {
        return;
    };
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(source) => {
            tracing::warn!(error_kind = ?source.kind(), "could not inspect obsolete Cargo cache entries");
            return;
        }
    };
    for entry in entries {
        let cleanup = (|| -> std::io::Result<()> {
            let entry = entry?;
            if entry.path() != cache_dir && entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())?;
            }
            Ok(())
        })();
        if let Err(source) = cleanup {
            tracing::warn!(error_kind = ?source.kind(), "could not remove obsolete Cargo cache entry");
        }
    }
}

fn cargo_install_command(
    ctx: &InstallContext,
    prefix: &[&str],
    crate_spec: &str,
    cache_dir: Option<&Path>,
    git: Option<&str>,
    force: bool,
) -> std::process::Command {
    let mut command = ctx.cargo_command();
    command.args(prefix);
    if force {
        command.arg("--force");
    }
    if let Some(path) = cache_dir {
        command.arg("--root").arg(path);
    }
    if let Some(url) = git {
        command.arg("--git").arg(url);
    }
    command.arg(crate_spec);
    command
}

fn install_cargo_crate_sync(
    ctx: &InstallContext,
    crate_name: &str,
    version: &str,
    binary_name: Option<String>,
    cache_dir: Option<&Path>,
    git: Option<String>,
    force: bool,
) -> Result<(), InstallError> {
    if let Some(path) = cache_dir {
        std::fs::create_dir_all(path)
            .map_err(|source| install_io("create Cargo cache directory", path, source))?;
    }
    let crate_spec = if version.is_empty() {
        crate_name.to_string()
    } else {
        format!("{crate_name}@{version}")
    };
    let mut binstall = cargo_install_command(
        ctx,
        &["binstall", "--no-confirm"],
        &crate_spec,
        cache_dir,
        git.as_deref(),
        force,
    );
    let binary_path = match (cache_dir, binary_name.as_ref()) {
        (Some(path), Some(name)) => Some(path.join("bin").join(platform_binary_exe(name))),
        _ => None,
    };
    match binstall.output() {
        Ok(output) if output.status.success() => {
            let installed = match &binary_path {
                Some(path) => cached_path_exists(path)?,
                None => true,
            };
            if installed {
                cleanup_other_versions(cache_dir);
                return Ok(());
            }
            tracing::debug!(
                "Cargo binstall reported success without the requested binary; falling back"
            );
        }
        Ok(output) => {
            tracing::debug!(status = ?output.status.code(), "Cargo binstall failed; falling back")
        }
        Err(source) => {
            tracing::debug!(error_kind = ?source.kind(), "Cargo binstall unavailable; falling back")
        }
    }
    let mut install = cargo_install_command(
        ctx,
        &["install"],
        &crate_spec,
        cache_dir,
        git.as_deref(),
        force,
    );
    let output = install
        .output()
        .map_err(|source| InstallError::InvokeCargo {
            operation: "run Cargo install",
            source,
        })?;
    if !output.status.success() {
        return Err(InstallError::CargoFailed {
            operation: "run Cargo install",
            status: output.status,
        });
    }
    if let Some(path) = binary_path
        && !cached_path_exists(&path)?
    {
        return Err(InstallError::MissingBinary { path });
    }
    cleanup_other_versions(cache_dir);
    Ok(())
}

fn get_binary_cache_dir(
    ctx: &InstallContext,
    krate: &str,
    version: &str,
) -> Result<PathBuf, InstallError> {
    if !safe_component(krate) || !safe_component(version) {
        return Err(InstallError::InvalidInput {
            field: "Cargo cache component",
        });
    }
    Ok(ctx.cache_dir().join("binaries").join(krate).join(version))
}

/// Cache key for a git-sourced cargo install. The user-supplied `version` (or
/// the literal `"git"` if absent) folds in with the URL so re-installs are
/// triggered when either changes.
fn git_cache_version(git_url: &str, version: Option<&str>) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    git_url.hash(&mut hasher);
    version.unwrap_or("git").hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Outcome of acquiring a cargo source.
struct AcquiredCargo {
    /// Symposium-managed cache dir, or `None` for global installs (no --root).
    cache_dir: Option<PathBuf>,
    /// The resolved binary name, when known.
    resolved_executable: Option<String>,
}

/// Acquire a cargo installation: install if missing.
///
/// Three branches, in priority order:
/// - `global = true`: skip crates.io, install with no `--root` (binary lands
///   in the user's `$CARGO_HOME/bin`). Validation guarantees the caller has
///   set `executable`, so no inference needed. `cache_dir = None` is the
///   signal that the source is unmanaged.
/// - `git` source: skip crates.io, install with `--root <cache>` and
///   `--git <url>`. Validation guarantees `executable`.
/// - Plain crates.io: query for version + bin_names; auto-infer the binary
///   when the crate has exactly one.
async fn acquire_cargo(
    ctx: &InstallContext,
    cargo: &CargoSource,
    executable_hint: Option<&str>,
    update: UpdateLevel,
) -> Result<AcquiredCargo, InstallError> {
    if cargo.global {
        // Validation requires `executable` for global cargo.
        let resolved = executable_hint
            .ok_or(InstallError::MissingExecutable {
                kind: "global Cargo",
            })?
            .to_string();
        install_cargo_crate(
            ctx,
            &cargo.crate_name,
            cargo.version.as_deref().unwrap_or(""),
            Some(resolved.clone()),
            None,
            cargo.git.clone(),
            false,
        )
        .await?;
        return Ok(AcquiredCargo {
            cache_dir: None,
            resolved_executable: Some(resolved),
        });
    }

    if let Some(git_url) = cargo.git.as_deref() {
        let resolved = match executable_hint {
            Some(name) => name.to_string(),
            None => return Err(InstallError::MissingExecutable { kind: "git Cargo" }),
        };
        let cache_version = git_cache_version(git_url, cargo.version.as_deref());
        let cache_dir = get_binary_cache_dir(ctx, &cargo.crate_name, &cache_version)?;
        let probe = cache_dir.join("bin").join(platform_binary_exe(&resolved));
        let sha_file = cache_dir.join(COMMIT_SHA_FILENAME);

        // The cache key folds in only the URL + user version, not the resolved
        // commit, so a moved branch never invalidates it on its own. Under
        // `Check`/`Fetch` we resolve the remote `HEAD` with a cheap `git
        // ls-remote` and reinstall only when it differs from the commit we last
        // installed. `None` skips freshness probes for an existing install;
        // a fresh install still records HEAD on a best-effort basis below.
        let remote_sha = if matches!(update, UpdateLevel::Check | UpdateLevel::Fetch) {
            remote_git_head_sha(git_url).await
        } else {
            None
        };
        let stored_sha = read_optional_cache(&sha_file)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let sha_changed = match (&remote_sha, &stored_sha) {
            (Some(remote), Some(stored)) => remote != stored,
            // Remote resolved but nothing recorded yet → (re)install to capture it.
            (Some(_), None) => true,
            // Couldn't resolve the remote → don't reinstall on SHA grounds.
            (None, _) => false,
        };

        let present = cached_path_exists(&probe)?;
        if !present || matches!(update, UpdateLevel::Fetch) || sha_changed {
            let overwrite = present;
            install_cargo_crate(
                ctx,
                &cargo.crate_name,
                cargo.version.as_deref().unwrap_or(""),
                Some(resolved.clone()),
                Some(cache_dir.clone()),
                Some(git_url.to_string()),
                overwrite,
            )
            .await?;
            // Record the commit we just installed so the next check can compare.
            let installed_sha = match remote_sha {
                Some(sha) => Some(sha),
                None => remote_git_head_sha(git_url).await,
            };
            if let Some(sha) = installed_sha
                && let Err(source) = std::fs::write(&sha_file, sha)
            {
                tracing::warn!(error_kind = ?source.kind(), "could not record installed Git commit");
            }
        }
        return Ok(AcquiredCargo {
            cache_dir: Some(cache_dir),
            resolved_executable: Some(resolved),
        });
    }

    // Plain crates.io. Resolving the version means a crates.io query, so under
    // `None` we serve the version recorded by the last freshness check instead
    // — a dispatch never hits the network. `Check`/`Fetch` always re-resolve
    // (picking up newly published versions) and refresh the pointer.
    let registry_dir = ctx.cache_dir().join("binaries").join(&cargo.crate_name);
    let pointer = registry_dir.join(CURRENT_VERSION_FILENAME);
    if matches!(update, UpdateLevel::None)
        && let Some((cache_dir, resolved)) =
            cached_cargo_version(&registry_dir, &pointer, executable_hint)
    {
        return Ok(AcquiredCargo {
            cache_dir: Some(cache_dir),
            resolved_executable: resolved,
        });
    }

    let (version, bin_names) =
        query_crate_binaries(&cargo.crate_name, cargo.version.as_deref()).await?;

    let resolved = match executable_hint {
        Some(name) => Some(name.to_string()),
        None => match bin_names.as_slice() {
            [] => None,
            [single] => Some(single.clone()),
            multiple => {
                return Err(InstallError::MultipleBinaries {
                    count: multiple.len(),
                });
            }
        },
    };

    if let Some(name) = &resolved
        && !safe_component(name)
    {
        return Err(InstallError::InvalidInput {
            field: "resolved executable",
        });
    }
    let cache_dir = get_binary_cache_dir(ctx, &cargo.crate_name, &version)?;
    let probe_path = resolved
        .as_ref()
        .map(|n| cache_dir.join("bin").join(platform_binary_exe(n)));

    // The cache dir is keyed on the resolved version, so a newer published
    // version installs into a fresh dir on its own. Only `Fetch` forces a
    // reinstall of the same version.
    let force = matches!(update, UpdateLevel::Fetch);
    let already = match &probe_path {
        Some(path) => cached_path_exists(path)?,
        None => false,
    };
    if !already || force {
        install_cargo_crate(
            ctx,
            &cargo.crate_name,
            &version,
            resolved.clone(),
            Some(cache_dir.clone()),
            None,
            force,
        )
        .await?;
    }

    // Record the resolved version so future `None` acquires can skip the query.
    if let Err(e) =
        std::fs::create_dir_all(&registry_dir).and_then(|()| std::fs::write(&pointer, &version))
    {
        tracing::warn!(error_kind = ?e.kind(), "failed to record current Cargo version pointer");
    }

    Ok(AcquiredCargo {
        cache_dir: Some(cache_dir),
        resolved_executable: resolved,
    })
}

/// Resolve a cargo binary cache from the recorded `current` version pointer,
/// without consulting crates.io. Returns the version's cache dir and the binary
/// name to run, or `None` when there's no usable cached version (so the caller
/// falls back to a registry query).
fn read_optional_cache(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(value) => Some(value),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            tracing::warn!(error_kind = ?source.kind(), "could not read optional Cargo cache metadata");
            None
        }
    }
}

fn cached_cargo_version(
    registry_dir: &Path,
    pointer: &Path,
    executable_hint: Option<&str>,
) -> Option<(PathBuf, Option<String>)> {
    let version = read_optional_cache(pointer)?;
    let version = version.trim();
    if !safe_component(version) {
        tracing::debug!("optional Cargo cache pointer is invalid");
        return None;
    }
    let cache_dir = registry_dir.join(version);
    let bin_dir = cache_dir.join("bin");
    let resolved = match executable_hint {
        Some(name) => {
            if !safe_component(name) {
                return None;
            }
            match cached_path_exists(&bin_dir.join(platform_binary_exe(name))) {
                Ok(true) => Some(name.to_string()),
                Ok(false) => return None,
                Err(error) => {
                    tracing::warn!(error = %error, "could not inspect optional Cargo cache");
                    return None;
                }
            }
        }
        None => Some(single_cached_binary(&bin_dir)?),
    };
    Some((cache_dir, resolved))
}

fn single_cached_binary(bin_dir: &Path) -> Option<String> {
    let entries = match std::fs::read_dir(bin_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return None,
        Err(source) => {
            tracing::warn!(error_kind = ?source.kind(), "could not list optional Cargo binaries");
            return None;
        }
    };
    let mut found = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                tracing::warn!(error_kind = ?source.kind(), "could not read optional Cargo directory entry");
                return None;
            }
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(source) => {
                tracing::warn!(error_kind = ?source.kind(), "could not inspect optional Cargo directory entry");
                return None;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(name) = entry.file_name().into_string() else {
            tracing::debug!("optional Cargo binary name is not Unicode");
            return None;
        };
        if found.is_some() {
            return None;
        }
        let name = name
            .strip_suffix(".exe")
            .map(str::to_string)
            .unwrap_or(name);
        if !safe_component(&name) {
            return None;
        }
        found = Some(name);
    }
    found
}

/// Probe remote HEAD without making a failed optional freshness check fatal.
async fn remote_git_head_sha(url: &str) -> Option<String> {
    let url = url.to_string();
    let result = tokio::task::spawn_blocking(move || -> Result<Option<String>, InstallError> {
        let output = std::process::Command::new("git")
            .args(["ls-remote", &url, "HEAD"])
            .output()
            .map_err(|source| InstallError::InvokeCargo {
                operation: "inspect Git HEAD",
                source,
            })?;
        if !output.status.success() {
            return Err(InstallError::CargoFailed {
                operation: "inspect Git HEAD",
                status: output.status,
            });
        }
        let sha = parse_ls_remote_sha(&String::from_utf8_lossy(&output.stdout));
        if sha.is_none() {
            tracing::debug!("Git HEAD probe did not return a usable commit");
        }
        Ok(sha)
    })
    .await;
    match result {
        Ok(Ok(sha)) => sha,
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "optional Git HEAD probe failed");
            None
        }
        Err(source) => {
            tracing::warn!(
                cancelled = source.is_cancelled(),
                panicked = source.is_panic(),
                "optional Git HEAD probe task failed"
            );
            None
        }
    }
}

/// Parse the commit SHA from `git ls-remote <url> HEAD` output, whose first
/// field is the hash (`<sha>\tHEAD`).
fn parse_ls_remote_sha(output: &str) -> Option<String> {
    let sha = output.split_whitespace().next()?;
    (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())).then(|| sha.to_string())
}

/// Acquire a github source: returns the cache directory containing the repo
/// (or its requested subtree).
async fn acquire_github(
    ctx: &InstallContext,
    source: &GithubSource,
    update: UpdateLevel,
) -> Result<PathBuf, InstallError> {
    let manager = crate::git::GitCacheManager::try_new(ctx, "plugins")?;
    Ok(manager.try_fetch_url(&source.url, update).await?)
}

/// Intermediate result of acquiring a `Source`: where the bits landed and
/// the layout-specific hooks needed to turn an `executable` / `script` name
/// into a concrete path. Consumed by `hook::acquire_installation`, which
/// wraps this together with the installation's name, `install_commands`,
/// and (for no-source installs) absolute path defaults to produce the
/// fully-resolved `AcquiredInstallation`.
pub struct AcquiredSource {
    /// Where bits landed. `None` for global cargo (binary is in
    /// `~/.cargo/bin`, which we don't manage).
    pub base: Option<PathBuf>,
    /// For cargo, the binary name that was installed. Used as the fallback
    /// when neither the installation nor the hook supplies an explicit
    /// `executable`. `None` for github (which has no notion of "default
    /// binary").
    pub resolved_executable: Option<String>,
    /// Layout discriminator — cargo binaries live under `<base>/bin/`,
    /// github paths live under `<base>/` directly.
    pub kind: AcquiredKind,
}

/// How an [`AcquiredSource`] was obtained.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquiredKind {
    /// Installed via `cargo install` / `cargo binstall`.
    Cargo,
    /// Cloned from a GitHub repository.
    Github,
}

/// The validated location of acquired artifacts, borrowing the public result.
///
/// This view is recomputed because legacy [`AcquiredSource`] fields remain
/// publicly mutable until the installer's independent 0.2 breaking release.
#[derive(Debug)]
#[non_exhaustive]
pub enum AcquiredLocation<'a> {
    /// Artifacts reside in a managed cache directory.
    Managed {
        /// The managed cache directory.
        base: &'a Path,
        /// The layout used within the cache directory.
        kind: AcquiredKind,
        /// The optional inferred executable name.
        resolved_executable: Option<&'a str>,
    },
    /// Cargo installed into its own global location; commands resolve by name.
    GlobalCargo {
        /// The required bare command name.
        executable: &'a str,
    },
}

impl AcquiredSource {
    /// Validate the public fields and borrow their managed/global alternative.
    ///
    /// # Errors
    /// Returns [`InstallError::InvalidLocation`] for unmanaged GitHub results or
    /// global Cargo without an executable; invalid names return `InvalidInput`.
    pub fn try_location(&self) -> Result<AcquiredLocation<'_>, InstallError> {
        if let Some(name) = self.resolved_executable.as_deref() {
            validate_relative_path(name)?;
            if self.kind == AcquiredKind::Cargo && !safe_component(name) {
                return Err(InstallError::InvalidInput {
                    field: "resolved Cargo executable",
                });
            }
        }
        match (&self.base, self.kind, self.resolved_executable.as_deref()) {
            (Some(base), kind, resolved_executable) => Ok(AcquiredLocation::Managed {
                base,
                kind,
                resolved_executable,
            }),
            (None, AcquiredKind::Cargo, Some(executable)) => {
                Ok(AcquiredLocation::GlobalCargo { executable })
            }
            (None, AcquiredKind::Cargo, None) => Err(InstallError::InvalidLocation {
                reason: "global Cargo requires an executable",
            }),
            (None, AcquiredKind::Github, _) => Err(InstallError::InvalidLocation {
                reason: "GitHub acquisition requires a managed base",
            }),
        }
    }

    /// Resolve the optional default executable from a validated location.
    ///
    /// # Errors
    /// Returns a typed location/name error instead of assuming a managed base.
    pub fn try_executable(&self) -> Result<Option<PathBuf>, InstallError> {
        let name = match self.try_location()? {
            AcquiredLocation::Managed {
                resolved_executable,
                ..
            } => resolved_executable,
            AcquiredLocation::GlobalCargo { executable } => Some(executable),
        };
        name.map(|name| self.try_executable_named(name)).transpose()
    }

    /// Resolve a named executable within a managed layout or as a global name.
    ///
    /// # Errors
    /// Rejects invalid locations and absolute/parent-traversing names. Global
    /// Cargo accepts a bare command name and has no managed base to join.
    pub fn try_executable_named(&self, name: &str) -> Result<PathBuf, InstallError> {
        validate_relative_path(name)?;
        match self.try_location()? {
            AcquiredLocation::Managed {
                base,
                kind: AcquiredKind::Cargo,
                ..
            } => Ok(base.join("bin").join(platform_binary_exe(name))),
            AcquiredLocation::Managed {
                base,
                kind: AcquiredKind::Github,
                ..
            } => Ok(base.join(name.trim_start_matches("./"))),
            AcquiredLocation::GlobalCargo { .. } => {
                if !safe_component(name) {
                    return Err(InstallError::InvalidInput {
                        field: "global executable",
                    });
                }
                Ok(PathBuf::from(name))
            }
        }
    }

    /// Resolve a script within its managed layout.
    ///
    /// # Errors
    /// Rejects invalid locations or names and returns [`InstallError::GlobalScript`]
    /// for unmanaged global Cargo installations.
    pub fn try_script_named(&self, name: &str) -> Result<PathBuf, InstallError> {
        validate_relative_path(name)?;
        match self.try_location()? {
            AcquiredLocation::Managed { base, kind, .. } => {
                let base = match kind {
                    AcquiredKind::Cargo => base.join("bin"),
                    AcquiredKind::Github => base.to_path_buf(),
                };
                Ok(base.join(name.trim_start_matches("./")))
            }
            AcquiredLocation::GlobalCargo { .. } => Err(InstallError::GlobalScript),
        }
    }

    /// Path to the executable, using `resolved_executable` as the name.
    /// Use [`Self::executable_named`] to specify the executable name from the outside.
    ///
    /// Returns `None` if no executable name was resolved (e.g., a GitHub
    /// source with no hint).
    /// Legacy 0.1.x API; use [`Self::try_executable`] before installer 0.2.
    ///
    /// # Panics
    /// Panics when an executable name is present but `base` is `None`.
    pub fn executable(&self) -> Option<PathBuf> {
        let name = self.resolved_executable.as_deref()?;
        Some(self.executable_named(name))
    }

    /// Path of an `executable` declared on installation/hook.
    /// Legacy 0.1.x API; requires a managed base. Prefer [`Self::try_executable_named`].
    ///
    /// # Panics
    /// Panics when `base` is `None`, including global Cargo acquisitions.
    #[cfg_attr(
        not(test),
        expect(
            clippy::expect_used,
            reason = "Legacy infallible public resolver retained until installer 0.2; use try_executable_named"
        )
    )]
    pub fn executable_named(&self, name: &str) -> PathBuf {
        let base = self
            .base
            .as_ref()
            .expect("resolve_executable called on unmanaged source");

        match self.kind {
            AcquiredKind::Cargo => base.join("bin").join(platform_binary_exe(name)),
            AcquiredKind::Github => base.join(name.trim_start_matches("./")),
        }
    }

    /// Path of a `script` declared on installation/hook.
    /// Legacy 0.1.x API; requires a managed base. Prefer [`Self::try_script_named`].
    ///
    /// # Panics
    /// Panics when `base` is `None`, including global Cargo acquisitions.
    #[cfg_attr(
        not(test),
        expect(
            clippy::expect_used,
            reason = "Legacy infallible public resolver retained until installer 0.2; use try_script_named"
        )
    )]
    pub fn script_named(&self, name: &str) -> PathBuf {
        let base = self
            .base
            .as_ref()
            .expect("resolve_executable called on unmanaged source");

        match self.kind {
            AcquiredKind::Cargo => base.join("bin").join(name.trim_start_matches("./")),
            AcquiredKind::Github => base.join(name.trim_start_matches("./")),
        }
    }
}

/// Acquire a source, downloading / installing as needed.
///
/// `executable_hint` is only used for cargo (to pick which binary to install
/// for multi-binary crates, or as the binary name when using a git source).
///
/// `update` controls freshness: `None` serves the cache (git checks debounced),
/// while `Check`/`Fetch` force a re-pull. It's the lever that lets `SessionStart`
/// refresh a `cargo + git` binary whose branch moved (the version-keyed cache
/// otherwise never re-installs on its own).
pub async fn acquire_source(
    ctx: &InstallContext,
    source: &Source,
    executable_hint: Option<&str>,
    update: UpdateLevel,
) -> anyhow::Result<AcquiredSource> {
    Ok(try_acquire_source(ctx, source, executable_hint, update).await?)
}

/// Acquire a source after validating public source fields and executable hints.
///
/// # Errors
/// Returns typed input errors before any side effects, or contextual process,
/// registry, Git, and filesystem failures with their original sources.
/// [`acquire_source`] is the legacy 0.1.x adapter, retained until installer 0.2.
pub async fn try_acquire_source(
    ctx: &InstallContext,
    source: &Source,
    executable_hint: Option<&str>,
    update: UpdateLevel,
) -> Result<AcquiredSource, InstallError> {
    validate_source(source, executable_hint)?;
    let acquired = match source {
        Source::Cargo(cargo) => {
            let acquired = acquire_cargo(ctx, cargo, executable_hint, update).await?;
            AcquiredSource {
                base: acquired.cache_dir,
                resolved_executable: acquired.resolved_executable,
                kind: AcquiredKind::Cargo,
            }
        }
        Source::Github(github) => AcquiredSource {
            base: Some(acquire_github(ctx, github, update).await?),
            resolved_executable: None,
            kind: AcquiredKind::Github,
        },
    };
    acquired.try_location()?;
    Ok(acquired)
}

/// Refresh an already-acquired source in place, leaving an un-acquired source
/// untouched. Unlike [`acquire_source`], this never installs a missing source:
/// it only *updates* what is already cached (with `UpdateLevel::Check`).
/// Returns `true` when a refresh ran.
///
/// This is for the once-per-session `SessionStart` warm-up, which should keep
/// already-installed tools current but must not eagerly install a tool a hook
/// may never use — that install happens lazily on first dispatch.
pub async fn refresh_source_if_present(
    ctx: &InstallContext,
    source: &Source,
    executable_hint: Option<&str>,
) -> anyhow::Result<bool> {
    Ok(try_refresh_source_if_present(ctx, source, executable_hint).await?)
}

/// Refresh a validated source only when its managed artifacts are already cached.
///
/// # Errors
/// Returns input errors before effects, or a typed inspection/acquisition error.
/// Missing optional caches remain misses. The legacy adapter is retained until 0.2.
pub async fn try_refresh_source_if_present(
    ctx: &InstallContext,
    source: &Source,
    executable_hint: Option<&str>,
) -> Result<bool, InstallError> {
    validate_source(source, executable_hint)?;
    if !try_source_is_cached(ctx, source, executable_hint)? {
        return Ok(false);
    }
    try_acquire_source(ctx, source, executable_hint, UpdateLevel::Check).await?;
    Ok(true)
}

fn try_source_is_cached(
    ctx: &InstallContext,
    source: &Source,
    hint: Option<&str>,
) -> Result<bool, InstallError> {
    match source {
        Source::Cargo(cargo) if cargo.global => Ok(false),
        Source::Cargo(cargo) => match cargo.git.as_deref() {
            Some(url) => {
                let Some(name) = hint else { return Ok(false) };
                let version = git_cache_version(url, cargo.version.as_deref());
                let dir = get_binary_cache_dir(ctx, &cargo.crate_name, &version)?;
                cached_path_exists(&dir.join("bin").join(platform_binary_exe(name)))
            }
            None => {
                let dir = ctx.cache_dir().join("binaries").join(&cargo.crate_name);
                Ok(cached_cargo_version(&dir, &dir.join(CURRENT_VERSION_FILENAME), hint).is_some())
            }
        },
        Source::Github(source) => {
            let manager = crate::git::GitCacheManager::try_new(ctx, "plugins")?;
            cached_path_exists(&manager.try_cache_path_for_url(&source.url)?)
        }
    }
}

#[cfg(test)]
fn source_is_cached(ctx: &InstallContext, source: &Source, hint: Option<&str>) -> bool {
    try_source_is_cached(ctx, source, hint).unwrap()
}

/// What an installation resolves to once acquired.
#[derive(Debug)]
pub enum Runnable {
    /// Run as a binary: `path args...`.
    Exec(PathBuf),
    /// Run as a shell script: `sh path args...`.
    Script(PathBuf),
}

impl Runnable {
    /// Spawn this runnable with the given arguments, wait for it to finish,
    /// and return the captured output.
    pub fn spawn(
        &self,
        args: &[impl AsRef<std::ffi::OsStr>],
    ) -> std::io::Result<std::process::Output> {
        use std::process::Command;
        match self {
            Runnable::Exec(path) => Command::new(path).args(args).output(),
            Runnable::Script(path) => Command::new("sh").arg(path).args(args).output(),
        }
    }
}

/// Ensure a path is executable on Unix. No-op on other platforms.
pub fn make_executable(path: &Path) -> anyhow::Result<()> {
    Ok(try_make_executable(path)?)
}

/// Prepare Unix executable permissions without suppressing metadata failures.
///
/// Missing paths remain a no-op for compatibility with bare commands resolved
/// through PATH. Non-Unix platforms do not require executable mode changes.
///
/// # Errors
/// Returns [`InstallError::Io`] for metadata or permission failures other than
/// absence. The legacy [`make_executable`] adapter is retained until installer 0.2.
pub fn try_make_executable(path: &Path) -> Result<(), InstallError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(install_io("inspect executable permissions", path, source)),
        };
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)
            .map_err(|source| install_io("set executable permissions", path, source))?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_unmanaged_resolvers_keep_their_panicking_contract() {
        let source = AcquiredSource {
            base: None,
            resolved_executable: Some("tool".into()),
            kind: AcquiredKind::Cargo,
        };
        for resolve in [
            AcquiredSource::executable_named,
            AcquiredSource::script_named,
        ] {
            assert!(std::panic::catch_unwind(|| resolve(&source, "tool")).is_err());
        }
        assert!(std::panic::catch_unwind(|| source.executable()).is_err());
        assert_eq!(
            source.try_executable().unwrap(),
            Some(PathBuf::from("tool"))
        );
        assert!(matches!(
            source.try_script_named("tool"),
            Err(InstallError::GlobalScript)
        ));
    }

    #[test]
    fn invalid_paths_are_rejected_before_acquisition() {
        for name in [
            "",
            ".",
            "./",
            "../tool",
            "a/../../tool",
            "/tool",
            "C:\\tool",
            "..\\tool",
        ] {
            assert!(validate_relative_path(name).is_err());
        }
        for name in ["tool", "./scripts/run.sh", "scripts/run.sh"] {
            assert!(validate_relative_path(name).is_ok());
        }
        let source = Source::Cargo(CargoSource::new("../outside"));
        assert!(matches!(
            validate_source(&source, Some("tool")),
            Err(InstallError::InvalidInput { field: "crate" })
        ));
        let git = Source::Cargo(CargoSource::new("tool").with_git("--upload-pack=unexpected"));
        assert!(matches!(
            validate_source(&git, Some("tool")),
            Err(InstallError::InvalidInput { field: "git" })
        ));
        let local = Source::Cargo(CargoSource::new("tool").with_git("./-repository"));
        assert!(validate_source(&local, Some("tool")).is_ok());
        let github = Source::Github(GithubSource::new("https://github.com/example/tool"));
        for source in [&github, &Source::Cargo(CargoSource::new("tool"))] {
            assert!(validate_source_runnable(source, None, Some("../outside")).is_err());
            assert!(validate_source_runnable(source, None, Some("./scripts/run.sh")).is_ok());
        }
        assert!(validate_source_runnable(&github, Some("../outside"), None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cargo_command_preserves_native_root_bytes_without_creating_the_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let cargo = tmp.path().join("capture-cargo");
        std::fs::write(&cargo, "#!/bin/sh\nset -eu\nprevious=\nfor argument in \"$@\"; do\nif test \"$previous\" = --root; then printf '%s' \"$argument\" > \"$CAPTURE_ROOT\"; fi\nprevious=$argument\ndone\n").unwrap();
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = tmp.path().join(OsString::from_vec(b"native-\xff".to_vec()));
        let ctx = InstallContext::new(tmp.path().join("cache")).with_cargo_bin(cargo);
        for (index, prefix) in [&["binstall", "--no-confirm"][..], &["install"][..]]
            .into_iter()
            .enumerate()
        {
            let capture = tmp.path().join(format!("root-{index}"));
            let output = cargo_install_command(&ctx, prefix, "tool", Some(&root), None, false)
                .env("CAPTURE_ROOT", &capture)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(std::fs::read(capture).unwrap(), root.as_os_str().as_bytes());
        }
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_install_preserves_previous_versions_and_omits_process_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let cargo = tmp.path().join("fake-cargo");
        std::fs::write(
            &cargo,
            "#!/bin/sh\nprintf '%s' PRIVATE_CARGO_STDERR_CANARY >&2\nexit 37\n",
        )
        .unwrap();
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ctx = InstallContext::new(tmp.path().join("cache")).with_cargo_bin(cargo);
        let old = get_binary_cache_dir(&ctx, "tool", "1.0.0").unwrap();
        std::fs::create_dir_all(old.join("bin")).unwrap();
        std::fs::write(old.join("bin/tool"), "working old binary").unwrap();
        let new = get_binary_cache_dir(&ctx, "tool", "2.0.0").unwrap();
        let error = install_cargo_crate_sync(
            &ctx,
            "tool",
            "2.0.0",
            Some("tool".into()),
            Some(&new),
            None,
            false,
        )
        .unwrap_err();
        assert!(matches!(error, InstallError::CargoFailed { .. }));
        assert!(!error.to_string().contains("PRIVATE_CARGO_STDERR_CANARY"));
        assert_eq!(
            std::fs::read_to_string(old.join("bin/tool")).unwrap(),
            "working old binary"
        );
    }

    #[test]
    fn invocation_error_retains_its_io_source() {
        use std::error::Error as _;
        let tmp = tempfile::tempdir().unwrap();
        let ctx = InstallContext::new(tmp.path().join("cache"))
            .with_cargo_bin(tmp.path().join("missing-cargo"));
        let error = install_cargo_crate_sync(
            &ctx,
            "tool",
            "1.0.0",
            Some("tool".into()),
            None,
            None,
            false,
        )
        .unwrap_err();
        assert!(matches!(error, InstallError::InvokeCargo { .. }));
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .is_some()
        );
    }

    #[test]
    fn parse_ls_remote_typical() {
        let out = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678\tHEAD\n";
        assert_eq!(
            parse_ls_remote_sha(out).as_deref(),
            Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678")
        );
    }

    #[test]
    fn parse_ls_remote_takes_first_field() {
        // ls-remote separates the sha from the ref with a tab.
        let out = "deadbeefcafe1234\trefs/heads/main\n";
        assert_eq!(
            parse_ls_remote_sha(out).as_deref(),
            Some("deadbeefcafe1234")
        );
    }

    #[test]
    fn parse_ls_remote_rejects_empty_and_nonhex() {
        assert_eq!(parse_ls_remote_sha(""), None);
        assert_eq!(parse_ls_remote_sha("not-a-sha\tHEAD"), None);
        // Too short to be a real object id.
        assert_eq!(parse_ls_remote_sha("abc\tHEAD"), None);
    }

    fn write_cached_binary(registry_dir: &Path, version: &str, bin: &str) {
        let bin_dir = registry_dir.join(version).join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join(platform_binary_exe(bin)), b"").unwrap();
    }

    #[test]
    fn cached_cargo_version_serves_pointer_with_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_dir = tmp.path().join("ripgrep");
        write_cached_binary(&registry_dir, "14.1.0", "rg");
        let pointer = registry_dir.join(CURRENT_VERSION_FILENAME);
        std::fs::write(&pointer, "14.1.0").unwrap();

        let (cache_dir, resolved) =
            cached_cargo_version(&registry_dir, &pointer, Some("rg")).unwrap();
        assert_eq!(cache_dir, registry_dir.join("14.1.0"));
        assert_eq!(resolved.as_deref(), Some("rg"));
    }

    #[test]
    fn cached_cargo_version_infers_single_binary_without_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_dir = tmp.path().join("ripgrep");
        write_cached_binary(&registry_dir, "14.1.0", "rg");
        let pointer = registry_dir.join(CURRENT_VERSION_FILENAME);
        std::fs::write(&pointer, "14.1.0\n").unwrap();

        let (_, resolved) = cached_cargo_version(&registry_dir, &pointer, None).unwrap();
        assert_eq!(resolved.as_deref(), Some("rg"));
    }

    #[test]
    fn cached_cargo_version_none_without_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_dir = tmp.path().join("ripgrep");
        write_cached_binary(&registry_dir, "14.1.0", "rg");
        let pointer = registry_dir.join(CURRENT_VERSION_FILENAME);
        // No pointer written → must fall back to a query.
        assert!(cached_cargo_version(&registry_dir, &pointer, Some("rg")).is_none());
    }

    #[test]
    fn cached_cargo_version_none_when_binary_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_dir = tmp.path().join("ripgrep");
        write_cached_binary(&registry_dir, "14.1.0", "rg");
        let pointer = registry_dir.join(CURRENT_VERSION_FILENAME);
        std::fs::write(&pointer, "14.1.0").unwrap();

        // Pointer is valid but the requested binary isn't there.
        assert!(cached_cargo_version(&registry_dir, &pointer, Some("other")).is_none());
    }

    #[test]
    fn cached_cargo_version_ambiguous_without_hint_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_dir = tmp.path().join("multi");
        write_cached_binary(&registry_dir, "1.0.0", "a");
        write_cached_binary(&registry_dir, "1.0.0", "b");
        let pointer = registry_dir.join(CURRENT_VERSION_FILENAME);
        std::fs::write(&pointer, "1.0.0").unwrap();

        // Two binaries and no hint → can't disambiguate from cache.
        assert!(cached_cargo_version(&registry_dir, &pointer, None).is_none());
    }

    #[test]
    fn source_is_cached_crates_io_present_and_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = InstallContext::new(tmp.path().to_path_buf());
        let src = Source::Cargo(CargoSource::new("ripgrep"));

        // Absent → false: the prewarm must not eagerly install.
        assert!(!source_is_cached(&ctx, &src, Some("rg")));

        let registry_dir = ctx.cache_dir().join("binaries").join("ripgrep");
        write_cached_binary(&registry_dir, "14.1.0", "rg");
        std::fs::write(registry_dir.join(CURRENT_VERSION_FILENAME), "14.1.0").unwrap();
        assert!(source_is_cached(&ctx, &src, Some("rg")));
    }

    #[test]
    fn source_is_cached_global_is_never_managed() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = InstallContext::new(tmp.path().to_path_buf());
        let mut cargo = CargoSource::new("ripgrep");
        cargo.global = true;
        assert!(!source_is_cached(&ctx, &Source::Cargo(cargo), Some("rg")));
    }

    #[test]
    fn source_is_cached_cargo_git_probes_commit_keyed_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = InstallContext::new(tmp.path().to_path_buf());
        let cargo = CargoSource::new("tool").with_git("https://example.com/o/r");
        let src = Source::Cargo(cargo.clone());

        assert!(!source_is_cached(&ctx, &src, Some("tool")));

        let cache_version =
            git_cache_version(cargo.git.as_deref().unwrap(), cargo.version.as_deref());
        let bin_dir = get_binary_cache_dir(&ctx, &cargo.crate_name, &cache_version)
            .unwrap()
            .join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join(platform_binary_exe("tool")), b"").unwrap();
        assert!(source_is_cached(&ctx, &src, Some("tool")));

        // A git source can't be probed without knowing the binary name.
        assert!(!source_is_cached(&ctx, &src, None));
    }

    #[tokio::test]
    async fn refresh_source_if_present_skips_absent_source() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = InstallContext::new(tmp.path().to_path_buf());
        let src = Source::Cargo(CargoSource::new("ripgrep"));

        // Nothing cached → no refresh runs and nothing is installed (no network).
        let refreshed = refresh_source_if_present(&ctx, &src, Some("rg"))
            .await
            .unwrap();
        assert!(!refreshed);
        assert!(!ctx.cache_dir().join("binaries").join("ripgrep").exists());
    }
}
