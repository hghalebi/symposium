//! Git-sourced plugin artifacts: URL parsing, API client, and cache management.

use std::path::{Path, PathBuf};

use crate::{InstallContext, UpdateLevel};

/// Minimum interval between freshness checks for cached sources.
const DEBOUNCE_DURATION: chrono::Duration = chrono::Duration::seconds(60);

/// Git-source validation, transport, and cache failures.
///
/// Diagnostics omit source URLs, response bodies, and parser input. Concrete
/// sources remain available through [`std::error::Error::source`].
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The URL is not one of the supported GitHub HTTPS forms.
    #[error("unsupported or malformed GitHub source URL")]
    InvalidUrl,
    /// A public source field is unsafe for a URL or cache path.
    #[error("invalid Git source component: {component}")]
    InvalidSource {
        /// Fixed name of the rejected component, without its value.
        component: &'static str,
    },
    /// HTTP-client construction, request, or response decoding failed.
    #[error("GitHub {operation} failed")]
    Request {
        /// Fixed classification of the HTTP operation.
        operation: &'static str,
        /// Concrete transport failure with its request URL removed.
        #[source]
        source: reqwest::Error,
    },
    /// GitHub rejected a request.
    #[error("GitHub {operation} returned HTTP {status}")]
    HttpStatus {
        /// Fixed classification of the HTTP operation.
        operation: &'static str,
        /// HTTP status code, without response content.
        status: u16,
    },
    /// GitHub returned no usable hexadecimal commit SHA.
    #[error("GitHub commit response is missing a valid SHA")]
    MissingCommitSha,
    /// The selected subtree is not a directory in the downloaded repository.
    #[error("selected Git repository subpath is not a directory")]
    MissingSubpath,
    /// A repository link escapes the extracted tree or forms a cycle.
    #[error("Git repository contains an escaping or cyclic path")]
    UnsafeRepositoryPath,
    /// A filesystem or archive operation failed.
    #[error("Git cache {operation} failed at {path}")]
    Io {
        /// Fixed classification of the filesystem or archive operation.
        operation: &'static str,
        /// Local path at which the operation failed.
        path: PathBuf,
        /// Original I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// A metadata document could not be decoded or encoded.
    #[error("Git cache metadata {operation} failed at {path}")]
    Metadata {
        /// Whether metadata encoding or decoding failed.
        operation: &'static str,
        /// Local metadata path.
        path: PathBuf,
        /// Original JSON failure, available for structured inspection.
        #[source]
        source: serde_json::Error,
    },
    /// Replacement and rollback both failed; the original cache is preserved.
    #[error("Git cache replacement and rollback failed; recover {cache_path} from {backup_path}")]
    Restore {
        /// Intended published cache path.
        cache_path: PathBuf,
        /// Preserved original cache; no automatic deletion is attempted.
        backup_path: PathBuf,
        /// Failure to publish the complete replacement.
        #[source]
        source: std::io::Error,
        /// Additional failure to restore the original cache.
        restore_source: std::io::Error,
    },
}

fn io_error(operation: &'static str, path: &Path, source: std::io::Error) -> GitError {
    GitError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn request_error(operation: &'static str, source: reqwest::Error) -> GitError {
    GitError::Request {
        operation,
        source: source.without_url(),
    }
}

// ── Public parsed-URL type ──────────────────────────────────────────────

/// A parsed git source URL.
///
/// Supported HTTPS URL forms that refer to the same logical repository parse
/// to the same fields, enabling deduplication. Public fields are validated again
/// before they are used to construct a cache path or request.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub enum GitSource {
    /// A GitHub-hosted repository.
    GitHub {
        /// Repository owner (e.g. `"symposium-dev"`).
        owner: String,
        /// Repository name (e.g. `"symposium"`).
        repo: String,
        /// Branch, tag, or commit ref. Empty string means default branch.
        git_ref: String,
        /// Path within the repo. Empty string means repo root.
        subpath: String,
    },
}

impl GitSource {
    /// A normalized identity string suitable for use in `SkillOrigin`.
    ///
    /// Two URLs that refer to the same logical repository produce the same
    /// `repo_id`, regardless of URL form.
    pub fn repo_id(&self) -> String {
        match self {
            GitSource::GitHub { owner, repo, .. } => format!("{owner}/{repo}"),
        }
    }

    /// The intra-repo subpath (the portion after `tree/<ref>/` in a GitHub URL).
    /// Empty string if the URL points at the repo root.
    pub fn subpath(&self) -> &str {
        match self {
            GitSource::GitHub { subpath, .. } => subpath,
        }
    }

    /// Filesystem-safe cache directory name.
    fn cache_key(&self) -> Result<String, GitError> {
        self.validate()?;
        match self {
            GitSource::GitHub {
                owner,
                repo,
                git_ref,
                subpath,
            } => {
                let mut key = format!("{owner}--{repo}");
                if !git_ref.is_empty() {
                    key.push_str(&format!("@{git_ref}"));
                }
                if !subpath.is_empty() {
                    let path_slug = subpath.replace('/', "--");
                    key.push_str(&format!("--{path_slug}"));
                }
                Ok(key)
            }
        }
    }

    fn validate(&self) -> Result<(), GitError> {
        let Self::GitHub {
            owner,
            repo,
            git_ref,
            subpath,
        } = self;
        for (component, value) in [("owner", owner), ("repository", repo)] {
            if value.is_empty()
                || value.ends_with('.')
                || matches!(value.as_str(), "." | "..")
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            {
                return Err(GitError::InvalidSource { component });
            }
        }
        if !git_ref.is_empty() && !is_safe_segment(git_ref) {
            return Err(GitError::InvalidSource { component: "ref" });
        }
        if !subpath.is_empty() && !subpath.split('/').all(is_safe_segment) {
            return Err(GitError::InvalidSource {
                component: "subpath",
            });
        }
        Ok(())
    }
}

fn is_safe_segment(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && !value.ends_with(' ')
        && !value.ends_with('.')
        && !value
            .chars()
            .any(|c| c.is_control() || "/\\:?#%<>\"|*".contains(c))
}

/// Parse a git URL into a structured [`GitSource`].
///
/// Currently recognizes GitHub URLs:
/// - `https://github.com/owner/repo`
/// - `https://github.com/owner/repo/tree/branch/path/to/dir`
///
/// Compatibility adapter retained through the installer 0.2 transition.
pub fn parse_git_url(url: &str) -> anyhow::Result<GitSource> {
    Ok(try_parse_git_url(url)?)
}

/// Parse and validate a supported GitHub HTTPS URL without retaining its input
/// in diagnostics. Host matching is case-insensitive; trailing slashes are ignored.
///
/// # Errors
/// Returns [`GitError::InvalidUrl`] for an unsupported shape and
/// [`GitError::InvalidSource`] for unsafe components.
pub fn try_parse_git_url(url: &str) -> Result<GitSource, GitError> {
    // Normalize: case-insensitive host matching
    let lower = url.to_lowercase();
    if lower.starts_with("https://github.com/") {
        return parse_github_https(url);
    }
    Err(GitError::InvalidUrl)
}

/// Parse an `https://github.com/...` URL.
fn parse_github_https(url: &str) -> Result<GitSource, GitError> {
    // Strip the prefix case-insensitively
    let stripped = &url["https://github.com/".len()..];

    // Remove trailing slash
    let stripped = stripped.trim_end_matches('/');

    let parts: Vec<&str> = stripped.splitn(4, '/').collect();

    let source = match parts.len() {
        // owner/repo
        2 => GitSource::GitHub {
            owner: parts[0].to_string(),
            repo: parts[1].to_string(),
            git_ref: String::new(),
            subpath: String::new(),
        },
        // owner/repo/tree/ref[/path...]
        n if n >= 4 && parts[2] == "tree" => {
            // rest is "branch/path/to/dir" — first segment is the ref, rest is path
            // But branches can contain slashes... we take the first segment as the ref.
            let rest = parts[3];
            let (git_ref, subpath) = match rest.split_once('/') {
                Some((r, p)) => (r.to_string(), p.to_string()),
                None => (rest.to_string(), String::new()),
            };
            if git_ref.is_empty() {
                return Err(GitError::InvalidSource { component: "ref" });
            }
            GitSource::GitHub {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
                git_ref,
                subpath,
            }
        }
        _ => return Err(GitError::InvalidUrl),
    };
    source.validate()?;
    Ok(source)
}

// --- GitHub API client ---

/// Client for GitHub REST API operations.
struct GitHubClient {
    client: reqwest::Client,
}

impl GitHubClient {
    // Compatibility constructor removed with the infallible public APIs in installer 0.2.
    fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("cargo-agents")
            .build()
            .expect("failed to build HTTP client");
        Self { client }
    }

    fn try_new() -> Result<Self, GitError> {
        reqwest::Client::builder()
            .user_agent("cargo-agents")
            .build()
            .map(|client| Self { client })
            .map_err(|source| request_error("client construction", source))
    }

    /// Resolve the commit SHA for a given ref (branch, tag, or "HEAD").
    async fn resolve_commit_sha(&self, source: &GitSource) -> Result<String, GitError> {
        source.validate()?;
        let GitSource::GitHub {
            owner,
            repo,
            git_ref,
            ..
        } = source;
        let ref_str = if git_ref.is_empty() { "HEAD" } else { git_ref };
        let url = format!(
            "https://api.github.com/repos/{}/{}/commits/{}",
            owner, repo, ref_str
        );

        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .map_err(|source| request_error("commit request", source))?;

        tracing::trace!(status = %resp.status(), "GitHub API resolve_commit_sha");

        if !resp.status().is_success() {
            return Err(GitError::HttpStatus {
                operation: "commit request",
                status: resp.status().as_u16(),
            });
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|source| request_error("commit response decoding", source))?;
        let sha = json["sha"]
            .as_str()
            .filter(|sha| !sha.is_empty() && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or(GitError::MissingCommitSha)?;
        Ok(sha.to_string())
    }

    /// Download the repository tarball at the already-resolved commit.
    async fn download_tarball(
        &self,
        source: &GitSource,
        sha: &str,
    ) -> Result<bytes::Bytes, GitError> {
        source.validate()?;
        let GitSource::GitHub { owner, repo, .. } = source;
        if sha.is_empty() || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(GitError::MissingCommitSha);
        }
        let url = format!("https://api.github.com/repos/{owner}/{repo}/tarball/{sha}");

        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .map_err(|source| request_error("tarball request", source))?;

        if !resp.status().is_success() {
            return Err(GitError::HttpStatus {
                operation: "tarball request",
                status: resp.status().as_u16(),
            });
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|source| request_error("tarball response body", source))?;
        tracing::trace!(bytes = bytes.len(), "tarball downloaded");
        Ok(bytes)
    }
}

// --- Plugin cache manager ---

/// Metadata stored alongside cached plugin artifacts.
#[non_exhaustive]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginCacheMeta {
    /// The resolved commit SHA that was fetched.
    pub commit_sha: String,
    /// RFC 3339 timestamp of when this cache entry was last fetched.
    pub fetched_at: String,
    /// The original URL that produced this cache entry.
    pub source_url: String,
}

const CACHE_META_FILENAME: &str = ".symposium-cache-meta.json";

/// Manages downloading and caching of git-sourced plugin artifacts.
pub struct GitCacheManager {
    cache_dir: PathBuf,
    client: GitHubClient,
}

impl GitCacheManager {
    /// Create a cache manager using the legacy infallible constructor.
    ///
    /// Retained through installer 0.2; maintained callers should use
    /// [`Self::try_new`] to report HTTP-client construction failures.
    pub fn new(ctx: &InstallContext, subdir: &str) -> Self {
        Self::from_cache_dir(&ctx.cache_dir().join(subdir))
    }

    /// Create a manager under a relative cache subdirectory.
    ///
    /// # Errors
    /// Returns a typed error for an unsafe subdirectory or HTTP-client failure.
    pub fn try_new(ctx: &InstallContext, subdir: &str) -> Result<Self, GitError> {
        if !subdir.split('/').all(is_safe_segment) {
            return Err(GitError::InvalidSource {
                component: "cache subdirectory",
            });
        }
        Self::try_from_cache_dir(&ctx.cache_dir().join(subdir))
    }

    /// Create a manager rooted at an already-resolved directory.
    ///
    /// This legacy constructor retains its historical client-construction panic
    /// through installer 0.2. Prefer [`Self::try_from_cache_dir`].
    pub fn from_cache_dir(cache_dir: &Path) -> Self {
        Self {
            cache_dir: cache_dir.to_path_buf(),
            client: GitHubClient::new(),
        }
    }

    /// Create a manager without creating or modifying its cache directory.
    ///
    /// # Errors
    /// Returns the concrete HTTP-client construction failure.
    pub fn try_from_cache_dir(cache_dir: &Path) -> Result<Self, GitError> {
        Ok(Self {
            cache_dir: cache_dir.to_path_buf(),
            client: GitHubClient::try_new()?,
        })
    }

    /// Compute the expected cache path, returning `None` for an invalid URL.
    ///
    /// Compatibility adapter retained through installer 0.2.
    pub fn cache_path_for_url(&self, url: &str) -> Option<PathBuf> {
        self.try_cache_path_for_url(url).ok()
    }

    /// Compute a validated cache path without creating it or fetching anything.
    ///
    /// # Errors
    /// Returns a typed error for unsupported URLs or unsafe source components.
    pub fn try_cache_path_for_url(&self, url: &str) -> Result<PathBuf, GitError> {
        let source = try_parse_git_url(url)?;
        Ok(self.cache_dir.join(source.cache_key()?))
    }

    /// Fetch a GitHub source using the legacy result type.
    ///
    /// Compatibility adapter retained through installer 0.2.
    pub async fn fetch_url(&self, url: &str, update: UpdateLevel) -> anyhow::Result<PathBuf> {
        Ok(self.try_fetch_url(url, update).await?)
    }

    /// Validate a GitHub source and fetch or refresh its cache.
    ///
    /// # Errors
    /// Returns a typed validation, transport, extraction, or publication error.
    /// A freshness-check failure keeps an existing matching cache with a warning.
    pub async fn try_fetch_url(&self, url: &str, update: UpdateLevel) -> Result<PathBuf, GitError> {
        let source = try_parse_git_url(url)?;
        self.fetch(&source, url, update).await
    }

    /// Fetch a source and return its parsed identity using the legacy result type.
    ///
    /// Compatibility adapter retained through installer 0.2.
    pub async fn fetch_url_parsed(
        &self,
        url: &str,
        update: UpdateLevel,
    ) -> anyhow::Result<(PathBuf, GitSource)> {
        Ok(self.try_fetch_url_parsed(url, update).await?)
    }

    /// Validate and fetch a source, also returning its structured identity.
    ///
    /// # Errors
    /// Returns the same failures as [`Self::try_fetch_url`].
    pub async fn try_fetch_url_parsed(
        &self,
        url: &str,
        update: UpdateLevel,
    ) -> Result<(PathBuf, GitSource), GitError> {
        let source = try_parse_git_url(url)?;
        let path = self.fetch(&source, url, update).await?;
        Ok((path, source))
    }

    async fn fetch(
        &self,
        source: &GitSource,
        source_url: &str,
        update: UpdateLevel,
    ) -> Result<PathBuf, GitError> {
        let cache_key = source.cache_key()?;
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| io_error("directory creation", &self.cache_dir, e))?;
        let plugin_dir = self.cache_dir.join(&cache_key);
        let meta_path = plugin_dir.join(CACHE_META_FILENAME);

        if plugin_dir.is_dir()
            && let Some(meta) = self.read_meta_best_effort(&meta_path)
        {
            // Legacy slugs can collide, so freshness never proves source identity.
            if metadata_matches_source(&meta, source) {
                if matches!(update, UpdateLevel::None) {
                    match chrono::DateTime::parse_from_rfc3339(&meta.fetched_at) {
                        Ok(fetched_at) => {
                            if chrono::Utc::now() - fetched_at.with_timezone(&chrono::Utc)
                                < DEBOUNCE_DURATION
                            {
                                tracing::debug!("plugin cache is recent, skipping check");
                                return Ok(plugin_dir);
                            }
                        }
                        Err(_) => tracing::warn!("invalid Git cache timestamp; checking freshness"),
                    }
                }
                if !matches!(update, UpdateLevel::Fetch) {
                    match self.client.resolve_commit_sha(source).await {
                        Ok(remote_sha) => {
                            if meta.commit_sha == remote_sha {
                                if let Err(error) = self.touch_meta(&meta_path, &meta) {
                                    tracing::warn!(%error, "Git cache timestamp update failed; keeping cached version");
                                }
                                return Ok(plugin_dir);
                            }
                            self.fetch_and_cache_with_sha(
                                source,
                                source_url,
                                &plugin_dir,
                                &remote_sha,
                            )
                            .await?;
                            return Ok(plugin_dir);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "failed to check freshness, using cached version");
                            return Ok(plugin_dir);
                        }
                    }
                }
            } else {
                tracing::warn!("Git cache source identity differs; fetching the requested source");
            }
        }

        let sha = self.client.resolve_commit_sha(source).await?;
        self.fetch_and_cache_with_sha(source, source_url, &plugin_dir, &sha)
            .await?;
        Ok(plugin_dir)
    }

    async fn fetch_and_cache_with_sha(
        &self,
        source: &GitSource,
        source_url: &str,
        plugin_dir: &Path,
        sha: &str,
    ) -> Result<(), GitError> {
        let tarball = self.client.download_tarball(source, sha).await?;
        self.cache_tarball(source, source_url, plugin_dir, sha, &tarball)
    }

    fn cache_tarball(
        &self,
        source: &GitSource,
        source_url: &str,
        plugin_dir: &Path,
        sha: &str,
        tarball: &[u8],
    ) -> Result<(), GitError> {
        source.validate()?;
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| io_error("directory creation", &self.cache_dir, e))?;
        let extracted = tempfile::tempdir_in(&self.cache_dir)
            .map_err(|e| io_error("extraction staging", &self.cache_dir, e))?;
        let root = extract_tarball(tarball, extracted.path())?;
        let source_dir = root.join(source.subpath());
        let resolved = std::fs::canonicalize(&source_dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                GitError::MissingSubpath
            } else {
                io_error("subpath resolution", &source_dir, error)
            }
        })?;
        if !resolved.starts_with(&root) {
            return Err(GitError::UnsafeRepositoryPath);
        }
        if !resolved.is_dir() {
            return Err(GitError::MissingSubpath);
        }

        let prepared = tempfile::tempdir_in(&self.cache_dir)
            .map_err(|e| io_error("publication staging", &self.cache_dir, e))?;
        copy_dir_recursive(&resolved, prepared.path(), &root, &mut Vec::new())?;
        let meta = PluginCacheMeta {
            commit_sha: sha.to_string(),
            fetched_at: chrono::Utc::now().to_rfc3339(),
            source_url: source_url.to_string(),
        };
        write_meta(&prepared.path().join(CACHE_META_FILENAME), &meta)?;
        publish_cache(prepared.path(), plugin_dir, &self.cache_dir)?;
        Ok(())
    }

    fn read_meta_best_effort(&self, path: &Path) -> Option<PluginCacheMeta> {
        match read_meta(path) {
            Ok(meta) => meta,
            Err(error) => {
                tracing::warn!(%error, "Git cache metadata unavailable; refreshing cache");
                None
            }
        }
    }

    /// Read optional metadata; malformed or unreadable metadata is diagnosed.
    ///
    /// Compatibility adapter retained through installer 0.2. Prefer
    /// [`Self::try_read_meta_for`] when the failure must be distinguished.
    pub fn read_meta_for(&self, plugin_dir: &Path) -> Option<PluginCacheMeta> {
        self.read_meta_best_effort(&plugin_dir.join(CACHE_META_FILENAME))
    }

    /// Read cache metadata, distinguishing absence from malformed/unreadable data.
    ///
    /// # Errors
    /// Returns a typed I/O or JSON error; a missing file returns `Ok(None)`.
    pub fn try_read_meta_for(
        &self,
        plugin_dir: &Path,
    ) -> Result<Option<PluginCacheMeta>, GitError> {
        read_meta(&plugin_dir.join(CACHE_META_FILENAME))
    }

    fn touch_meta(&self, path: &Path, meta: &PluginCacheMeta) -> Result<(), GitError> {
        let updated = PluginCacheMeta {
            commit_sha: meta.commit_sha.clone(),
            fetched_at: chrono::Utc::now().to_rfc3339(),
            source_url: meta.source_url.clone(),
        };
        write_meta(path, &updated)
    }
}

fn metadata_matches_source(meta: &PluginCacheMeta, source: &GitSource) -> bool {
    matches!(try_parse_git_url(&meta.source_url), Ok(cached) if &cached == source)
}

fn read_meta(path: &Path) -> Result<Option<PluginCacheMeta>, GitError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("metadata read", path, error)),
    };
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|source| GitError::Metadata {
            operation: "decode",
            path: path.to_path_buf(),
            source,
        })
}

fn write_meta(path: &Path, meta: &PluginCacheMeta) -> Result<(), GitError> {
    use std::io::Write;

    let content = serde_json::to_vec_pretty(meta).map_err(|source| GitError::Metadata {
        operation: "encode",
        path: path.to_path_buf(),
        source,
    })?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut prepared = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| io_error("metadata staging", path, e))?;
    prepared
        .write_all(&content)
        .map_err(|e| io_error("metadata write", path, e))?;
    prepared
        .persist(path)
        .map_err(|e| io_error("metadata publication", path, e.error))?;
    Ok(())
}

/// Unpack without moving the wrapper's children over the wrapper itself.
fn extract_tarball(tarball_bytes: &[u8], dest: &Path) -> Result<PathBuf, GitError> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    std::fs::create_dir_all(dest)
        .map_err(|e| io_error("extraction directory creation", dest, e))?;
    let gz = GzDecoder::new(std::io::Cursor::new(tarball_bytes));
    Archive::new(gz)
        .unpack(dest)
        .map_err(|e| io_error("archive extraction", dest, e))?;
    let entries = std::fs::read_dir(dest)
        .map_err(|e| io_error("extraction directory read", dest, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io_error("extraction entry read", dest, e))?;
    let root = if let [entry] = entries.as_slice()
        && entry
            .file_type()
            .map_err(|e| io_error("wrapper inspection", &entry.path(), e))?
            .is_dir()
    {
        entry.path()
    } else {
        dest.to_path_buf()
    };
    std::fs::canonicalize(&root).map_err(|e| io_error("extraction root resolution", &root, e))
}

fn publish_cache(prepared: &Path, plugin_dir: &Path, cache_dir: &Path) -> Result<(), GitError> {
    let backup =
        tempfile::tempdir_in(cache_dir).map_err(|e| io_error("backup staging", cache_dir, e))?;
    let previous = backup.path().join("previous");
    let had_previous = match std::fs::symlink_metadata(plugin_dir) {
        Ok(_) => {
            std::fs::rename(plugin_dir, &previous)
                .map_err(|e| io_error("previous cache backup", plugin_dir, e))?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(io_error("previous cache inspection", plugin_dir, error)),
    };
    if let Err(source) = std::fs::rename(prepared, plugin_dir) {
        if had_previous && let Err(restore_source) = std::fs::rename(&previous, plugin_dir) {
            let backup_root = backup.keep();
            return Err(GitError::Restore {
                cache_path: plugin_dir.to_path_buf(),
                backup_path: backup_root.join("previous"),
                source,
                restore_source,
            });
        }
        return Err(io_error("prepared cache publication", plugin_dir, source));
    }
    if let Err(error) = backup.close() {
        let error = io_error("previous cache cleanup", cache_dir, error);
        tracing::warn!(%error, "new Git cache published; previous backup cleanup failed");
    }
    Ok(())
}

fn copy_dir_recursive(
    src: &Path,
    dst: &Path,
    root: &Path,
    ancestors: &mut Vec<PathBuf>,
) -> Result<(), GitError> {
    let resolved =
        std::fs::canonicalize(src).map_err(|e| io_error("copy source resolution", src, e))?;
    if !resolved.starts_with(root) || ancestors.contains(&resolved) {
        return Err(GitError::UnsafeRepositoryPath);
    }
    ancestors.push(resolved.clone());
    let result = copy_dir_entries(&resolved, dst, root, ancestors);
    ancestors.pop();
    result
}

fn copy_dir_entries(
    src: &Path,
    dst: &Path,
    root: &Path,
    ancestors: &mut Vec<PathBuf>,
) -> Result<(), GitError> {
    std::fs::create_dir_all(dst).map_err(|e| io_error("copy directory creation", dst, e))?;
    for entry in std::fs::read_dir(src).map_err(|e| io_error("copy directory read", src, e))? {
        let entry = entry.map_err(|e| io_error("copy entry read", src, e))?;
        let path = entry.path();
        let resolved = std::fs::canonicalize(&path)
            .map_err(|e| io_error("copy entry resolution", &path, e))?;
        if !resolved.starts_with(root) {
            return Err(GitError::UnsafeRepositoryPath);
        }
        let dst_path = dst.join(entry.file_name());
        let metadata = std::fs::metadata(&resolved)
            .map_err(|e| io_error("copy entry inspection", &path, e))?;
        if metadata.is_dir() {
            copy_dir_recursive(&resolved, &dst_path, root, ancestors)?;
        } else if metadata.is_file() {
            std::fs::copy(&resolved, &dst_path).map_err(|e| io_error("file copy", &dst_path, e))?;
        } else {
            return Err(GitError::UnsafeRepositoryPath);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_dir_recursive_creates_a_missing_destination() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("SKILL.md"), "skill").unwrap();

        let parent = tempfile::tempdir().unwrap();
        let dst = parent.path().join("owner--repo@main--subpath");
        let root = std::fs::canonicalize(src.path()).unwrap();
        copy_dir_recursive(&root, &dst, &root, &mut Vec::new()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("SKILL.md")).unwrap(),
            "skill"
        );
    }

    fn source(subpath: &str) -> GitSource {
        GitSource::GitHub {
            owner: "owner".into(),
            repo: "repo".into(),
            git_ref: "main".into(),
            subpath: subpath.into(),
        }
    }

    fn archive_with_file(path: &str, content: &[u8]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, path, content).unwrap();
        archive.into_inner().unwrap().finish().unwrap()
    }

    fn old_cache(plugin_dir: &Path) -> Vec<u8> {
        std::fs::create_dir_all(plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("SKILL.md"), "old skill").unwrap();
        let metadata = b"exact old metadata bytes".to_vec();
        std::fs::write(plugin_dir.join(CACHE_META_FILENAME), &metadata).unwrap();
        metadata
    }

    fn assert_old_cache(plugin_dir: &Path, metadata: &[u8]) {
        assert_eq!(
            std::fs::read(plugin_dir.join("SKILL.md")).unwrap(),
            b"old skill"
        );
        assert_eq!(
            std::fs::read(plugin_dir.join(CACHE_META_FILENAME)).unwrap(),
            metadata
        );
    }

    #[test]
    fn supported_https_forms_keep_existing_fields_and_cache_keys() {
        let root = try_parse_git_url("HTTPS://GITHUB.COM/owner/repo///").unwrap();
        assert_eq!(root.repo_id(), "owner/repo");
        assert_eq!(root.subpath(), "");
        assert_eq!(root.cache_key().unwrap(), "owner--repo");
        let tree =
            try_parse_git_url("https://github.com/owner/repo/tree/main/skills/rust/").unwrap();
        assert_eq!(tree, source("skills/rust"));
        assert_eq!(tree.cache_key().unwrap(), "owner--repo@main--skills--rust");
        assert!(try_parse_git_url("https://github.com/owner/repo.").is_err());
        assert!(try_parse_git_url("https://github.com/owner/repo/tree/main*").is_err());
        let git_suffix = try_parse_git_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(git_suffix.repo_id(), "owner/repo.git");
    }

    #[test]
    fn invalid_urls_have_content_free_diagnostics() {
        for url in [
            "ssh://CANARY_PRIVATE_SOURCE",
            "https://github.com/owner",
            "https://github.com/owner/repo/blob/main/file",
            "https://github.com/owner/repo?CANARY_PRIVATE_SOURCE",
            "https://github.com/owner/repo/tree/main/../CANARY_PRIVATE_SOURCE",
            "https://github.com/owner/repo/tree/main/%2e%2e/CANARY_PRIVATE_SOURCE",
            "https://github.com/owner/repo/tree//subpath",
        ] {
            let error = try_parse_git_url(url).unwrap_err();
            assert!(!error.to_string().contains("CANARY_PRIVATE_SOURCE"));
        }
    }

    #[test]
    fn constructed_public_sources_are_validated_before_cache_paths() {
        for field in ["owner", "repository", "ref", "subpath"] {
            for value in ["", ".", "..", "../escape", r"..\escape", "C:escape", "a\0b"] {
                if value.is_empty() && matches!(field, "ref" | "subpath") {
                    continue;
                }
                let GitSource::GitHub {
                    mut owner,
                    mut repo,
                    mut git_ref,
                    mut subpath,
                } = source("skills");
                match field {
                    "owner" => owner = value.into(),
                    "repository" => repo = value.into(),
                    "ref" => git_ref = value.into(),
                    "subpath" => subpath = value.into(),
                    _ => unreachable!(),
                }
                let constructed = GitSource::GitHub {
                    owner,
                    repo,
                    git_ref,
                    subpath,
                };
                assert!(
                    matches!(constructed.cache_key(), Err(GitError::InvalidSource { component }) if component == field)
                );
            }
        }
        for subpath in [
            "/root",
            "a//b",
            "a/.. ",
            "a/b.",
            "a/b#fragment",
            "a/b*",
            "a/b|",
            "a/b<",
            "a/b>",
            "a/b\"",
        ] {
            assert!(source(subpath).cache_key().is_err());
        }
    }

    #[tokio::test]
    async fn invalid_public_source_does_not_create_cache_or_make_a_request() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("not-created");
        let manager = GitCacheManager::try_from_cache_dir(&cache).unwrap();
        let error = manager
            .fetch(&source("../escape"), "unused", UpdateLevel::Fetch)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            GitError::InvalidSource {
                component: "subpath"
            }
        ));
        assert!(!cache.exists());
    }

    #[test]
    fn metadata_distinguishes_absence_decode_and_io_failures() {
        use std::error::Error;
        let cache = tempfile::tempdir().unwrap();
        let manager = GitCacheManager::try_from_cache_dir(cache.path()).unwrap();
        assert!(manager.try_read_meta_for(cache.path()).unwrap().is_none());
        std::fs::write(
            cache.path().join(CACHE_META_FILENAME),
            "{\"CANARY_METADATA\": invalid}",
        )
        .unwrap();
        let error = manager.try_read_meta_for(cache.path()).unwrap_err();
        assert!(matches!(&error, GitError::Metadata { source, .. } if source.is_syntax()));
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<serde_json::Error>()
                .is_some()
        );
        assert!(!error.to_string().contains("CANARY_METADATA"));
        assert!(manager.read_meta_for(cache.path()).is_none());

        let conflict = cache.path().join("metadata-dir");
        std::fs::create_dir(&conflict).unwrap();
        std::fs::create_dir(conflict.join(CACHE_META_FILENAME)).unwrap();
        std::fs::write(conflict.join("keep"), "keep").unwrap();
        let error = manager.try_read_meta_for(&conflict).unwrap_err();
        assert!(matches!(
            &error,
            GitError::Io {
                operation: "metadata read",
                ..
            }
        ));
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .is_some()
        );
        assert_eq!(
            std::fs::read_to_string(conflict.join("keep")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn failed_metadata_update_preserves_context_and_source() {
        let cache = tempfile::tempdir().unwrap();
        let manager = GitCacheManager::try_from_cache_dir(cache.path()).unwrap();
        let path = cache.path().join(CACHE_META_FILENAME);
        std::fs::create_dir(&path).unwrap();
        let meta = PluginCacheMeta {
            commit_sha: "sha".into(),
            fetched_at: "time".into(),
            source_url: "CANARY_URL".into(),
        };
        let error = manager.touch_meta(&path, &meta).unwrap_err();
        assert!(
            matches!(&error, GitError::Io { operation: "metadata publication", path: actual, .. } if actual == &path)
        );
        assert!(!error.to_string().contains("CANARY_URL"));
        assert!(path.is_dir());
    }

    #[test]
    fn metadata_identity_prevents_legacy_slug_collision_reuse() {
        let nested = source("a/b");
        let slug = source("a--b");
        assert_eq!(nested.cache_key().unwrap(), slug.cache_key().unwrap());
        let meta = PluginCacheMeta {
            commit_sha: "sha".into(),
            fetched_at: chrono::Utc::now().to_rfc3339(),
            source_url: "HTTPS://GITHUB.COM/owner/repo/tree/main/a/b/".into(),
        };
        assert!(metadata_matches_source(&meta, &nested));
        assert!(!metadata_matches_source(&meta, &slug));
    }

    #[test]
    fn broken_archive_and_missing_subpath_preserve_old_cache_and_metadata() {
        let cache = tempfile::tempdir().unwrap();
        let manager = GitCacheManager::try_from_cache_dir(cache.path()).unwrap();
        let plugin_dir = cache.path().join("plugin");
        let old_metadata = old_cache(&plugin_dir);
        let error = manager
            .cache_tarball(&source(""), "url", &plugin_dir, "abc", b"invalid archive")
            .unwrap_err();
        assert!(matches!(
            error,
            GitError::Io {
                operation: "archive extraction",
                ..
            }
        ));
        assert_old_cache(&plugin_dir, &old_metadata);
        let archive = archive_with_file("wrapper/SKILL.md", b"new skill");
        let error = manager
            .cache_tarball(&source("missing"), "url", &plugin_dir, "abc", &archive)
            .unwrap_err();
        assert!(matches!(error, GitError::MissingSubpath));
        assert_old_cache(&plugin_dir, &old_metadata);
    }

    #[test]
    fn failed_publication_restores_old_cache_and_metadata() {
        let cache = tempfile::tempdir().unwrap();
        let plugin_dir = cache.path().join("plugin");
        let old_metadata = old_cache(&plugin_dir);
        let error = publish_cache(
            &cache.path().join("missing-prepared"),
            &plugin_dir,
            cache.path(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GitError::Io {
                operation: "prepared cache publication",
                ..
            }
        ));
        assert_old_cache(&plugin_dir, &old_metadata);
    }

    #[test]
    fn nested_wrapper_name_survives_complete_cache_publication() {
        let cache = tempfile::tempdir().unwrap();
        let manager = GitCacheManager::try_from_cache_dir(cache.path()).unwrap();
        let plugin_dir = cache.path().join("plugin");
        old_cache(&plugin_dir);
        let archive = archive_with_file("wrapper/wrapper/SKILL.md", b"new skill");
        let url = "https://github.com/owner/repo/tree/main";
        manager
            .cache_tarball(&source(""), url, &plugin_dir, "abc", &archive)
            .unwrap();
        assert_eq!(
            std::fs::read(plugin_dir.join("wrapper/SKILL.md")).unwrap(),
            b"new skill"
        );
        assert!(!plugin_dir.join("SKILL.md").exists());
        let meta = manager.try_read_meta_for(&plugin_dir).unwrap().unwrap();
        assert_eq!(meta.commit_sha, "abc");
        assert_eq!(meta.source_url, url);
    }

    #[test]
    fn request_diagnostics_strip_url_and_preserve_the_concrete_error() {
        use std::error::Error;
        let source = reqwest::Client::new()
            .get("https://user:CANARY_REQUEST@github.com/owner/repo")
            .header("invalid\\nheader", "value")
            .build()
            .unwrap_err();
        let error = request_error("test", source);
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<reqwest::Error>()
            .unwrap();
        assert!(source.url().is_none());
        assert!(!error.to_string().contains("CANARY_REQUEST"));
    }

    #[test]
    fn legacy_public_signatures_and_constructible_fields_remain_usable() {
        let _: fn(&str) -> anyhow::Result<GitSource> = parse_git_url;
        let _: fn(&InstallContext, &str) -> GitCacheManager = GitCacheManager::new;
        let _: fn(&Path) -> GitCacheManager = GitCacheManager::from_cache_dir;
        let _: fn(&GitCacheManager, &str) -> Option<PathBuf> = GitCacheManager::cache_path_for_url;
        let _: fn(&GitCacheManager, &Path) -> Option<PluginCacheMeta> =
            GitCacheManager::read_meta_for;
        fn accepts_legacy_fetch(_: impl std::future::Future<Output = anyhow::Result<PathBuf>>) {}
        fn accepts_legacy_parsed(
            _: impl std::future::Future<Output = anyhow::Result<(PathBuf, GitSource)>>,
        ) {
        }
        let cache = tempfile::tempdir().unwrap();
        let manager = GitCacheManager::from_cache_dir(cache.path());
        accepts_legacy_fetch(manager.fetch_url("https://github.com/owner/repo", UpdateLevel::None));
        accepts_legacy_parsed(
            manager.fetch_url_parsed("https://github.com/owner/repo", UpdateLevel::None),
        );
        assert_eq!(
            manager
                .cache_path_for_url("https://github.com/owner/repo")
                .unwrap(),
            cache.path().join("owner--repo")
        );
        let GitSource::GitHub {
            owner,
            repo,
            git_ref,
            subpath,
        } = source("skills");
        assert_eq!(
            (
                owner.as_str(),
                repo.as_str(),
                git_ref.as_str(),
                subpath.as_str()
            ),
            ("owner", "repo", "main", "skills")
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_rejects_external_links_and_cycles_but_keeps_internal_link_content() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        std::fs::write(root.path().join("real/SKILL.md"), "inside").unwrap();
        symlink("real", root.path().join("linked")).unwrap();
        symlink("real/SKILL.md", root.path().join("linked-file")).unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        copy_dir_recursive(
            &canonical,
            &dest.path().join("internal"),
            &canonical,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.path().join("internal/linked/SKILL.md")).unwrap(),
            "inside"
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("internal/linked-file")).unwrap(),
            "inside"
        );

        std::fs::write(outside.path().join("CANARY_FILE"), "CANARY_CONTENT").unwrap();
        symlink(
            outside.path().join("CANARY_FILE"),
            root.path().join("escape"),
        )
        .unwrap();
        assert!(matches!(
            copy_dir_recursive(
                &canonical,
                &dest.path().join("external-file"),
                &canonical,
                &mut Vec::new()
            ),
            Err(GitError::UnsafeRepositoryPath)
        ));
        std::fs::remove_file(root.path().join("escape")).unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(matches!(
            copy_dir_recursive(
                &canonical,
                &dest.path().join("external-dir"),
                &canonical,
                &mut Vec::new()
            ),
            Err(GitError::UnsafeRepositoryPath)
        ));
        std::fs::remove_file(root.path().join("escape")).unwrap();
        symlink("..", root.path().join("real/cycle")).unwrap();
        assert!(matches!(
            copy_dir_recursive(
                &canonical,
                &dest.path().join("cycle"),
                &canonical,
                &mut Vec::new()
            ),
            Err(GitError::UnsafeRepositoryPath)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn failed_archive_copy_preserves_the_exact_old_cache() {
        let cache = tempfile::tempdir().unwrap();
        let manager = GitCacheManager::try_from_cache_dir(cache.path()).unwrap();
        let plugin_dir = cache.path().join("plugin");
        let old_metadata = old_cache(&plugin_dir);
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        archive
            .append_link(&mut header, "wrapper/escape", "/")
            .unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        let error = manager
            .cache_tarball(&source(""), "url", &plugin_dir, "abc", &bytes)
            .unwrap_err();
        assert!(matches!(error, GitError::UnsafeRepositoryPath));
        assert_old_cache(&plugin_dir, &old_metadata);
    }
}
