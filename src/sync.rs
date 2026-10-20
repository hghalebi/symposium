//! Sync command: `symposium sync`.
//!
//! Scans workspace dependencies, finds applicable skills from plugin sources,
//! installs them into each configured agent's skill directory, and cleans up
//! stale skills by looking for a `.symposium` marker file in each skill dir.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use symposium_install::UpdateLevel;
use thiserror::Error;

use crate::agents::{Agent, AgentConfigError};
use crate::config::{RegistryError, Symposium};
use crate::dir_walk::{self, EntryKind};
use crate::output::{Output, display_path};
use crate::plugins;
use crate::pm::WorkspaceDeps;
use crate::skills;
use std::sync::Arc;

/// Marker file written into every skill directory symposium installs.
///
/// Cleanup walks each agent's skills parent dir and removes any subdir
/// containing this marker that isn't in the freshly-installed set, leaving
/// user-managed skill directories (which lack the marker) untouched.
pub(crate) const MARKER_FILE: &str = ".symposium";

/// Failure while preparing or updating agent integrations and managed skills.
///
/// Individual skill and removed-agent cleanup failures remain visible best
/// effort. Errors returned here prevent the remaining workflow from running.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The configured registry sources could not be validated.
    #[error("failed to validate plugin registries")]
    Registry(#[from] RegistryError),
    /// Sync needs a resolved Rust workspace.
    #[error("not in a Rust workspace")]
    WorkspaceRequired,
    /// Workspace discovery failed instead of proving that no workspace exists.
    #[error("failed to resolve Rust workspace: {source}")]
    WorkspaceLookup {
        /// The shared concrete workspace failure from the memoized resolver.
        #[source]
        source: Arc<crate::pm::WorkspaceError>,
    },
    /// An agent name, configuration, or registration could not be processed.
    #[error("failed to update agent configuration: {0}")]
    Agent(#[from] AgentConfigError),
    /// A filesystem operation failed, retaining its path and original source.
    #[error("failed to {operation} at {}", path.display())]
    Io {
        /// The fixed operation that failed.
        operation: &'static str,
        /// The affected path.
        path: PathBuf,
        /// The original filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// A skill file could not be copied.
    #[error("failed to copy skill file from {} to {}", from.display(), to.display())]
    Copy {
        /// The source file.
        from: PathBuf,
        /// The destination file.
        to: PathBuf,
        /// The original copy failure.
        #[source]
        source: std::io::Error,
    },
    /// An agent skill path unexpectedly has no parent directory.
    #[error("agent skill path has no parent directory")]
    MissingSkillParent,
    /// A user-managed entry appeared at a skill's destination before publication.
    #[error("refusing to replace a user-managed skill at {}", path.display())]
    UnmanagedDestination {
        /// The protected user-managed path.
        path: PathBuf,
    },
    /// Publication and rollback both failed; the previous skill remains in backup.
    #[error("skill replacement and rollback failed; recover {} from {}", destination.display(), backup.display())]
    Restore {
        /// The intended installation directory.
        destination: PathBuf,
        /// The preserved previous directory, which needs manual recovery.
        backup: PathBuf,
        /// The original publication failure.
        #[source]
        source: std::io::Error,
        /// The additional failure restoring the previous installation.
        restore_source: std::io::Error,
    },
    /// The external MCP schema introduced an unsupported server variant.
    #[error("unsupported MCP server variant")]
    UnsupportedMcpServer,
}

impl SyncError {
    fn io(operation: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

type SyncResult<T> = std::result::Result<T, SyncError>;

/// Create `path` and any missing ancestors up to `boundary`.
///
/// `boundary` is the workspace root — we never walk above it.
pub(crate) fn create_managed_dir_all(path: &Path, boundary: &Path) -> SyncResult<()> {
    if path
        .try_exists()
        .map_err(|source| SyncError::io("inspect directory", path, source))?
    {
        return Ok(());
    }
    if path == boundary {
        fs::create_dir_all(path)
            .map_err(|source| SyncError::io("create directory", path, source))?;
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_managed_dir_all(parent, boundary)?;
    }
    fs::create_dir(path).map_err(|source| SyncError::io("create directory", path, source))?;
    Ok(())
}

/// Skills parent directory for an agent (e.g. `.claude/skills/` or
/// `.agents/skills/`), derived from `Agent::project_skill_dir`.
fn skills_parent_dir(agent: Agent, project_root: &Path) -> SyncResult<PathBuf> {
    let skill_dir = agent.try_project_skill_dir(project_root, "_")?;
    skill_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or(SyncError::MissingSkillParent)
}

/// Mark a directory as symposium-generated: drop the `.symposium` marker
/// and a `.gitignore` containing `*` so the directory is recognized on
/// future syncs and kept out of version control.
///
/// Idempotent — overwrites any pre-existing marker or `.gitignore` in
/// `dir`. Callers use this both for freshly-installed plugin skills and
/// for skills propagated by the agents-syncing feature.
fn mark_generated_skill_directory(dir: &Path) -> SyncResult<()> {
    let marker = dir.join(MARKER_FILE);
    fs::write(&marker, "").map_err(|source| SyncError::io("write marker", &marker, source))?;
    let ignore = dir.join(".gitignore");
    fs::write(&ignore, "*\n")
        .map_err(|source| SyncError::io("write ignore file", &ignore, source))?;
    Ok(())
}

/// Does `dir` contain the `.symposium` marker, i.e. is it a symposium-managed
/// skill directory? Returns `false` for user-authored skills and for any
/// directory symposium did not create.
fn has_symposium_marker(dir: &Path) -> SyncResult<bool> {
    let marker = dir.join(MARKER_FILE);
    match marker.try_exists() {
        Ok(exists) => Ok(exists),
        // A user-managed file can occupy the plain skill slot. It has no
        // marker, and the caller selects the existing collision suffix.
        Err(source) if source.kind() == std::io::ErrorKind::NotADirectory => Ok(false),
        Err(source) => Err(SyncError::io("inspect marker", &marker, source)),
    }
}

/// One entry of a source tree as the installer sees it: a path relative to the
/// tree root, with symlinks already resolved.
type TreeEntry = (PathBuf, EntryKind);

/// Walk `root` depth-first with symlinks followed, returning every directory
/// and regular file under it as a path relative to `root`, sorted, together
/// with one message per entry the walk refused to follow.
///
/// Copying and comparing both go through this one walk, because they have to
/// agree entry for entry. A comparison scan that omitted something the copier
/// writes would see the destination differ from the source on every run, and
/// every sync would become a full delete-and-recopy.
///
/// Sorting by relative path puts each directory before its children, so a
/// caller may create or read the entries in order.
fn walk_tree(root: &Path) -> SyncResult<(Vec<TreeEntry>, Vec<String>)> {
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let mut ancestors = dir_walk::Ancestors::rooted_at(root);
    walk_tree_inner(root, root, &mut ancestors, &mut entries, &mut skipped)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((entries, skipped))
}

fn walk_tree_inner(
    root: &Path,
    dir: &Path,
    ancestors: &mut dir_walk::Ancestors,
    entries: &mut Vec<TreeEntry>,
    skipped: &mut Vec<String>,
) -> SyncResult<()> {
    let read =
        fs::read_dir(dir).map_err(|source| SyncError::io("read skill directory", dir, source))?;
    for entry in read {
        let entry = entry.map_err(|source| SyncError::io("read directory entry", dir, source))?;
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let file_type = entry
            .file_type()
            .map_err(|source| SyncError::io("inspect directory entry", &path, source))?;
        match dir_walk::resolved_kind(&path, file_type) {
            Ok(Some(EntryKind::Dir)) => {
                if !ancestors.enter(&path) {
                    skipped.push(format!(
                        "skipping {}: symlink re-enters a directory it is inside",
                        display_path(&path)
                    ));
                    continue;
                }
                entries.push((rel, EntryKind::Dir));
                walk_tree_inner(root, &path, ancestors, entries, skipped)?;
                ancestors.leave();
            }
            Ok(Some(EntryKind::File)) => entries.push((rel, EntryKind::File)),
            // A socket or a fifo. Never skill content, so never worth a word.
            Ok(None) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => skipped.push(format!(
                "skipping {}: broken symlink ({source})",
                display_path(&path)
            )),
            Err(source) => return Err(SyncError::io("resolve skill symlink", &path, source)),
        }
    }
    Ok(())
}

/// Recursively copy the contents of `src` into `dst`. Creates `dst` if
/// missing. Symlinks are followed: a link to a file lands as a real file, a
/// link to a directory as a real directory holding a copy of its contents.
///
/// Entries the walk cannot follow are reported and skipped, so a single broken
/// link leaves the rest of the skill installed instead of failing the sync.
fn copy_dir_recursive(src: &Path, dst: &Path) -> SyncResult<()> {
    let (entries, skipped) = walk_tree(src)?;
    for message in skipped {
        tracing::info!(report = %crate::report::ReportEvent::Warning { message });
    }
    fs::create_dir_all(dst).map_err(|source| SyncError::io("create directory", dst, source))?;
    for (rel, kind) in entries {
        let dst_path = dst.join(&rel);
        match kind {
            EntryKind::Dir => {
                fs::create_dir_all(&dst_path)
                    .map_err(|source| SyncError::io("create directory", &dst_path, source))?;
            }
            EntryKind::File => {
                let src_path = src.join(&rel);
                fs::copy(&src_path, &dst_path).map_err(|source| SyncError::Copy {
                    from: src_path,
                    to: dst_path,
                    source,
                })?;
            }
        }
    }
    Ok(())
}

/// Collect `dir`'s tree for comparison: every directory and regular file under
/// it, as a path relative to `dir` paired with the file's contents (`None` for
/// a directory). Symlinks resolve exactly as [`copy_dir_recursive`] resolves
/// them, so the source of an installed skill and its destination compare equal.
///
/// Skips the `.symposium` marker and `.gitignore`, which are managed metadata
/// rather than skill content. Entries the walk refuses are dropped silently:
/// the copy this comparison guards reports them, and reporting them here too
/// would say the same thing twice per sync.
fn collect_dir_contents(dir: &Path) -> SyncResult<Vec<(PathBuf, Option<Vec<u8>>)>> {
    let (entries, _skipped) = walk_tree(dir)?;
    let mut out = Vec::with_capacity(entries.len());
    for (rel, kind) in entries {
        let name = rel.to_string_lossy();
        if name == MARKER_FILE || name == ".gitignore" {
            continue;
        }
        let contents = match kind {
            EntryKind::Dir => None,
            EntryKind::File => {
                let path = dir.join(&rel);
                Some(
                    fs::read(&path)
                        .map_err(|source| SyncError::io("read skill file", &path, source))?,
                )
            }
        };
        out.push((rel, contents));
    }
    Ok(out)
}

/// Returns true if the source directory's content differs from the
/// destination's content (ignoring managed metadata files).
fn dir_contents_differ(source_dir: &Path, dest_dir: &Path) -> SyncResult<bool> {
    let src = collect_dir_contents(source_dir)?;
    let dst = collect_dir_contents(dest_dir)?;
    Ok(src != dst)
}

/// Synchronize a skill directory from `source_dir` into `dest_dir`.
///
/// This is the single function used by both the plugin-skill and
/// user-authored-skill code paths. It:
/// 1. Checks whether `dest_dir` is debounce-fresh (marker mtime < `debounce`)
///    — if so, skips entirely.
/// 2. Compares source and dest content — if identical, touches the marker
///    to reset the debounce window and returns without modifying content.
/// 3. Otherwise copies and marks a temporary sibling completely, then publishes
///    it with a same-filesystem rename, retaining a backup until publication
///    succeeds. Publication failure restores the previous installation.
///
/// Returns `Ok(true)` if the destination was created or updated (callers
/// record it as installed). Returns `Ok(false)` if skipped (no-op).
fn sync_skill_dir(
    source_dir: &Path,
    dest_dir: &Path,
    project_root: &Path,
    debounce: Duration,
) -> SyncResult<bool> {
    if dest_dir == source_dir {
        return Ok(false);
    }

    // If the destination doesn't exist yet, do a fresh install.
    if !dest_dir
        .try_exists()
        .map_err(|source| SyncError::io("inspect skill destination", dest_dir, source))?
    {
        replace_skill_contents(source_dir, dest_dir, project_root)?;
        return Ok(true);
    }

    // Missing markers mean an ordinary full comparison. Other metadata
    // failures are real installation failures, rather than a fresh timestamp.
    let marker_path = dest_dir.join(MARKER_FILE);
    if !debounce.is_zero() {
        let metadata = match fs::metadata(&marker_path) {
            Ok(metadata) => Some(metadata),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(SyncError::io("inspect marker", &marker_path, source)),
        };
        if let Some(metadata) = metadata {
            let modified = metadata
                .modified()
                .map_err(|source| SyncError::io("read marker timestamp", &marker_path, source))?;
            // A future timestamp is a clock adjustment, not evidence of a
            // recent completed sync. Compare contents in that case.
            if SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|elapsed| elapsed < debounce)
            {
                tracing::debug!(dest = %dest_dir.display(), "skill sync debounced");
                return Ok(false);
            }
        }
    }

    // Compare content (excluding managed metadata).
    if !dir_contents_differ(source_dir, dest_dir)? {
        // Content is identical — just touch the marker to reset debounce.
        touch_marker(&marker_path)?;
        return Ok(false);
    }

    replace_skill_contents(source_dir, dest_dir, project_root)?;
    Ok(true)
}

/// Prepare all bytes and managed metadata before moving the installed artifact.
fn replace_skill_contents(
    source: &Path,
    destination: &Path,
    project_root: &Path,
) -> SyncResult<()> {
    let parent = destination.parent().ok_or(SyncError::MissingSkillParent)?;
    create_managed_dir_all(parent, project_root)?;
    let staging = tempfile::Builder::new()
        .prefix(".symposium-stage-")
        .tempdir_in(parent)
        .map_err(|source| SyncError::io("create skill staging directory", parent, source))?;
    // The wrapper itself has no managed marker. A retained recovery copy must
    // not look like a stale installation to a later cleanup pass.
    let prepared = staging.path().join("prepared");
    let previous = staging.path().join("previous");
    let outcome = (|| {
        copy_dir_recursive(source, &prepared)?;
        mark_generated_skill_directory(&prepared)?;
        publish_skill(&prepared, destination, &previous)
    })();
    if matches!(&outcome, Err(SyncError::Restore { .. })) {
        let retained = staging.keep();
        tracing::info!(report = %crate::report::ReportEvent::Warning {
            message: format!("skill rollback failed; previous installation preserved at {}", display_path(&retained.join("previous"))),
        });
    } else {
        close_skill_staging(staging);
    }
    outcome
}

/// Keep the previous installation until the completed replacement is published.
/// The two renames are recoverable on ordinary I/O failure, not a crash-safe
/// transaction or a lock against simultaneous sync processes.
fn publish_skill(prepared: &Path, destination: &Path, previous: &Path) -> SyncResult<()> {
    let had_previous = match fs::symlink_metadata(destination) {
        Ok(_) => {
            // Recheck after staging: an entry created by the user while
            // preparing must not be overwritten by a managed installation.
            if !has_symposium_marker(destination)? {
                return Err(SyncError::UnmanagedDestination {
                    path: destination.to_path_buf(),
                });
            }
            fs::rename(destination, previous)
                .map_err(|source| SyncError::io("back up installed skill", destination, source))?;
            true
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(SyncError::io(
                "inspect installed skill",
                destination,
                source,
            ));
        }
    };
    // Once the previous entry moves, handle both publication and compensation
    // explicitly. A relative symlink may appear broken in its temporary slot;
    // successful rename, rather than existence probing, decides rollback.
    if let Err(source) = fs::rename(prepared, destination) {
        if had_previous && let Err(restore_source) = fs::rename(previous, destination) {
            return Err(SyncError::Restore {
                destination: destination.to_path_buf(),
                backup: previous.to_path_buf(),
                source,
                restore_source,
            });
        }
        return Err(SyncError::io("publish prepared skill", destination, source));
    }
    Ok(())
}

/// Cleanup is best effort after publication, and every failure is reported.
fn close_skill_staging(directory: tempfile::TempDir) {
    let path = directory.path().to_path_buf();
    if let Err(source) = directory.close() {
        let error = SyncError::io("clean skill staging directory", &path, source);
        tracing::info!(report = %crate::report::ReportEvent::Warning { message: error.to_string() });
    }
}

/// Update the marker file's mtime to now without changing content.
fn touch_marker(marker_path: &Path) -> SyncResult<()> {
    fs::write(marker_path, "")
        .map_err(|source| SyncError::io("touch marker", marker_path, source))?;
    Ok(())
}

/// Inspect the entire parent before deleting anything. A failed read must not
/// turn a partial directory listing into evidence that an installation is stale.
fn cleanup_stale_skills(parent: &Path, installed: &BTreeSet<PathBuf>) -> SyncResult<()> {
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(SyncError::io("read skills parent", parent, source)),
    };
    let mut stale = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|source| SyncError::io("read skills parent entry", parent, source))?;
        let path = entry.path();
        if installed.contains(&path) {
            continue;
        }
        let metadata = fs::metadata(&path)
            .map_err(|source| SyncError::io("inspect installed skill", &path, source))?;
        if metadata.is_dir() && has_symposium_marker(&path)? {
            stale.push(path);
        }
    }
    for path in stale {
        match fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!(report = %crate::report::ReportEvent::SkillRemoved {
                path: display_path(&path),
            }),
            Err(source) => {
                let error = SyncError::io("remove stale skill", &path, source);
                tracing::info!(report = %crate::report::ReportEvent::Warning {
                    message: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

fn report_unregister_failure(
    agent: Agent,
    integration: &'static str,
    result: std::result::Result<(), AgentConfigError>,
) {
    if let Err(error) = result {
        tracing::info!(report = %crate::report::ReportEvent::Warning {
            message: format!("failed to unregister {integration} for {}: {error}", agent.display_name()),
        });
    }
}

fn mcp_server_names(servers: &[sacp::schema::McpServer]) -> SyncResult<Vec<&str>> {
    servers
        .iter()
        .map(|server| match server {
            sacp::schema::McpServer::Stdio(server) => Ok(server.name.as_str()),
            sacp::schema::McpServer::Http(server) => Ok(server.name.as_str()),
            sacp::schema::McpServer::Sse(server) => Ok(server.name.as_str()),
            _ => Err(SyncError::UnsupportedMcpServer),
        })
        .collect()
}

/// Compatibility adapter through root 0.6; prefer [`try_sync`].
///
/// # Errors
///
/// Returns a typed workflow failure through the legacy dynamic error boundary.
pub async fn sync(
    sym: &Symposium,
    deps: &Arc<WorkspaceDeps>,
    update: UpdateLevel,
) -> anyhow::Result<()> {
    try_sync(sym, deps, update).await.map_err(legacy_error)
}

/// Compatibility adapter through root 0.6; prefer [`try_register_hooks`].
///
/// # Errors
///
/// Returns a typed registration failure through the legacy dynamic error boundary.
pub async fn register_hooks(sym: &Symposium, out: &Output) -> anyhow::Result<()> {
    try_register_hooks(sym, out).await.map_err(legacy_error)
}

fn legacy_error(error: SyncError) -> anyhow::Error {
    match error {
        // Existing callers inspect invalid registry errors directly.
        SyncError::Registry(source) => anyhow::Error::new(source),
        other => anyhow::Error::new(other),
    }
}

/// Resolve custom predicate installations from the registry into entries
/// suitable for [`PredicateContext::with_custom_predicates`].
async fn resolve_custom_predicate_entries(
    sym: &Symposium,
    registry: &plugins::PluginRegistry,
    update: UpdateLevel,
) -> (
    std::collections::HashMap<String, crate::predicate::ResolvedPredicateEntry>,
    bool,
) {
    use crate::predicate::ResolvedPredicateEntry;

    let mut entries = std::collections::HashMap::new();
    let mut has_failures = false;

    for (name, resolved) in registry.custom_predicates.iter() {
        let plugin = &registry.plugins[resolved.plugin_index];
        let Some(install) = plugin.manifest.get_installation(&resolved.command) else {
            has_failures = true;
            tracing::warn!(
                predicate = name,
                command = &resolved.command,
                "custom predicate references unknown installation"
            );
            continue;
        };

        let acquired =
            match crate::installation::acquire_installation(sym, install, None, None, update).await
            {
                Ok(a) => a,
                Err(e) => {
                    has_failures = true;
                    tracing::warn!(
                        predicate = name,
                        error = %e,
                        "failed to acquire custom predicate installation"
                    );
                    continue;
                }
            };

        let runnable =
            match crate::installation::resolve_runnable(acquired, &format!("predicate `{name}`")) {
                Ok(r) => r,
                Err(e) => {
                    has_failures = true;
                    tracing::warn!(
                        predicate = name,
                        error = %e,
                        "failed to resolve custom predicate runnable"
                    );
                    continue;
                }
            };

        entries.insert(
            name.clone(),
            ResolvedPredicateEntry {
                runnable,
                args: resolved.args.clone(),
            },
        );
    }

    (entries, has_failures)
}

/// Run the full sync: discover applicable skills, install into agent dirs,
/// clean up stale installations.
///
/// Skill installation, stale cleanup, and removed-agent cleanup are visible
/// best effort. A failed skill install or unreadable registry suppresses stale
/// cleanup so failure cannot be mistaken for proof that a skill is obsolete.
///
/// # Errors
///
/// Returns registry, workspace, agent registration, or skill-path failures.
/// Registry sources are validated before installation or configuration effects.
pub async fn try_sync(
    sym: &Symposium,
    deps: &Arc<WorkspaceDeps>,
    update: UpdateLevel,
) -> SyncResult<()> {
    let pms = sym.try_package_managers(deps)?;
    let out = &Output::quiet();
    let loaded = deps
        .try_load()
        .map_err(|source| SyncError::WorkspaceLookup { source })?
        .ok_or(SyncError::WorkspaceRequired)?;
    let project_root = loaded.root.clone();
    let workspace: Vec<_> = loaded.crates.clone();
    let loaded = loaded.clone();
    let debounce = Duration::from_secs(sym.config.sync_debounce_secs);
    tracing::debug!(root = %project_root.display(), "resolved workspace root");

    // Load plugin registry (registry sources + workspace plugins)
    let registry = plugins::try_load_registry_with_workspace(sym, Some(&loaded)).await?;

    for warning in &registry.warnings {
        tracing::info!(
            report = %crate::report::ReportEvent::Warning {
                message: format!("skipping {}: {}", display_path(&warning.path), warning.message),
            },
        );
    }

    tracing::info!(
        report = %crate::report::ReportEvent::Info {
            message: format!("scanning {} workspace dependencies", workspace.len()),
        },
    );

    // Resolve custom predicate installations.
    let (custom_entries, has_failed_predicate_preparation) =
        resolve_custom_predicate_entries(sym, &registry, update).await;

    // Resolve the workspace once and build the predicate context shared by
    // skill resolution and MCP-server filtering. Attach the on-disk cache so
    // custom predicate results survive across sync runs; results are persisted
    // at the end of this evaluation pass.
    let (dep_ids, has_failed_dependency_listing) = match pms.try_list_deps().await {
        Ok(dependencies) => (dependencies, false),
        Err(error) => {
            tracing::warn!(error = %error, "failed to list workspace dependencies; continuing with no dependency matches");
            (Vec::new(), true)
        }
    };
    let used_names = sym.config.plugins.used_names_in(&project_root);
    let predicate_cache_path =
        crate::predicate_cache::PredicateCache::path_for_workspace(sym.cache_dir(), &project_root);
    let mut ctx =
        crate::predicate::PredicateContext::with_custom_predicates(&dep_ids, custom_entries)
            .with_used_names(&used_names)
            .with_disk_cache(&predicate_cache_path);

    // The active plugin set: registry plugins plus the crate-sourced plugins
    // reached through `[[plugins]]` chained references and dependency
    // enablement. Every facet resolves over this one set, so a crate plugin's
    // skills and MCP servers install exactly like a registry plugin's.
    let active_collection =
        plugins::active_plugins_outcome(sym, &registry, &pms, Some(&project_root), &mut ctx).await;
    let active = active_collection.plugins;

    // Find all applicable skills.
    let collection = skills::collect_skills(sym, &active, &mut ctx, update).await;
    let applicable = collection.skills;

    // Dedup by `(skill_name, origin_hash)`: two crate origins with the same
    // (name, version, skill-path-within-crate) collapse (the same skill bytes
    // reached through two plugins); skills from genuinely different locations —
    // including two skills at different paths within one crate — survive
    // independently. Skills that survive dedup are recorded with both their
    // plain name and their origin hash so we can decide later whether each one
    // needs an `<name>-<hash>` suffix to avoid collisions.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut to_install: Vec<(String, String, &std::path::Path)> = Vec::new();
    let mut name_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    for entry in &applicable {
        let name = entry.skill.name().to_string();
        if seen.insert((name.clone(), entry.origin_hash.clone())) {
            *name_counts.entry(name.clone()).or_default() += 1;
            to_install.push((name, entry.origin_hash.clone(), &entry.skill.path));
        }
    }

    // Collect MCP servers from the same active plugin set.
    let mut mcp_servers: Vec<sacp::schema::McpServer> = Vec::new();
    for p in &active {
        if p.applies(&mut ctx) {
            mcp_servers.extend(p.manifest.applicable_mcp_servers(&mut ctx));
        }
    }
    if let Err(e) = ctx.persist_disk_cache(&predicate_cache_path) {
        tracing::warn!(
            path = %predicate_cache_path.display(),
            error = %e,
            "failed to persist predicate cache"
        );
    }

    let server_names = mcp_server_names(&mcp_servers)?;

    // Sync each configured agent
    let agent_names: Vec<String> = sym.config.agents.iter().map(|a| a.name.clone()).collect();

    tracing::info!(
        workspace_deps = workspace.len(),
        agents = agent_names.len(),
        skills = to_install.len(),
        "sync started"
    );

    if agent_names.is_empty() {
        tracing::info!(
            report = %crate::report::ReportEvent::Info {
                message: "no agents configured, run `cargo agents init` to add one".into(),
            },
        );
        return Ok(());
    }

    // Track every skill directory we (re)install during this sync. Anything
    // we find later that has the marker file but isn't in this set is stale.
    let mut installed_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut has_failed_skill_install = false;

    let agents = agent_names
        .iter()
        .map(|name| Agent::try_from_config_name(name))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (agent_name, agent) in agent_names.iter().zip(agents) {
        // Hooks: the project and global locations are genuinely different files
        // for some agents - Antigravity writes `.agents/hooks.json` but
        // `~/.gemini/config/hooks.json`, and Copilot `.github/hooks/` but
        // `~/.copilot/settings.json` - so project scope cannot be produced by
        // rooting the global path at the workspace.
        match sym.config.hook_scope {
            crate::config::HookScope::Global => {
                agent.try_register_hooks(sym.home_dir(), sym, out)?
            }
            crate::config::HookScope::Project => {
                agent.try_register_project_hooks(&project_root, sym, out)?
            }
        }

        // MCP does not follow the hook scope: an agent's MCP file differs from
        // its hooks file, so the target is resolved per agent instead.
        let mcp_scope = match sym.config.hook_scope {
            crate::config::HookScope::Global => crate::agents::McpScope::User,
            crate::config::HookScope::Project => crate::agents::McpScope::Project,
        };
        // Reported, not logged: the user asked for project scope and is getting a
        // machine-wide entry.
        if mcp_scope == crate::agents::McpScope::Project
            && !agent.supports_project_mcp_scope()
            && !mcp_servers.is_empty()
        {
            tracing::info!(
                report = %crate::report::ReportEvent::Info {
                    message: format!(
                        "{} has no project-level MCP config; registering its servers at user level",
                        agent.display_name(),
                    ),
                },
            );
        }
        agent.try_register_mcp_servers(
            mcp_scope,
            &project_root,
            sym.home_dir(),
            &mcp_servers,
            out,
        )?;

        for (skill_name, origin_hash, skill_source) in &to_install {
            // `skill_source` is the path to the SKILL.md file; the skill
            // directory is its parent.
            let source_dir = match skill_source.parent() {
                Some(p) => p,
                None => {
                    has_failed_skill_install = true;
                    tracing::info!(report = %crate::report::ReportEvent::Warning {
                        message: format!("skill {skill_name}: cannot determine source directory"),
                    });
                    continue;
                }
            };

            // The skill's source already sits at this agent's install slot
            // (a workspace `.agents/skills/` skill, on an agent that reads
            // that same directory) — it is in place as user content, not
            // something to copy.
            let plain_dir = agent.try_project_skill_dir(&project_root, skill_name)?;
            let in_place = match (source_dir.canonicalize(), plain_dir.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            };
            if in_place {
                continue;
            }

            // Pick the install dir name for this skill on *this* agent:
            // - If exactly one origin claims the name and the un-suffixed
            //   slot is "available" (nonexistent or symposium-managed),
            //   use the plain `<skill-name>/`.
            // - Otherwise fall back to `<skill-name>-<origin-hash>/` so
            //   distinct origins coexist and we never clobber a
            //   user-managed directory.
            let unique_name = name_counts.get(skill_name).copied().unwrap_or(0) == 1;
            let plain_available = !plain_dir
                .try_exists()
                .map_err(|source| SyncError::io("inspect skill destination", &plain_dir, source))?
                || has_symposium_marker(&plain_dir)?;
            let dir_name = if unique_name && plain_available {
                skill_name.clone()
            } else {
                format!("{skill_name}-{}", origin_hash)
            };
            let dest_dir = agent.try_project_skill_dir(&project_root, &dir_name)?;

            // If the dest exists but is user-managed, skip it.
            if dest_dir
                .try_exists()
                .map_err(|source| SyncError::io("inspect skill destination", &dest_dir, source))?
                && !has_symposium_marker(&dest_dir)?
            {
                tracing::info!(
                    report = %crate::report::ReportEvent::Warning {
                        message: format!(
                            "skipping {}: user-managed skill already present",
                            display_path(&dest_dir)
                        ),
                    },
                );
                continue;
            }

            match sync_skill_dir(source_dir, &dest_dir, &project_root, debounce) {
                Ok(true) => {
                    installed_dirs.insert(dest_dir.clone());
                    tracing::info!(
                        report = %crate::report::ReportEvent::SkillInstalled {
                            skill: dir_name.clone(),
                            agent: agent_name.clone(),
                            dest: display_path(&dest_dir),
                        },
                    );
                }
                Ok(false) => {
                    // Debounced or unchanged — still record as installed
                    // so stale-cleanup doesn't remove it.
                    installed_dirs.insert(dest_dir.clone());
                }
                Err(e) => {
                    has_failed_skill_install = true;
                    tracing::info!(
                        report = %crate::report::ReportEvent::Warning {
                            message: format!("failed to install skill {dir_name}: {e}"),
                        },
                    );
                }
            }
        }
    }

    // Stale-skill cleanup: scan every agent's skills parent directory (across
    // all known agents, so we also clean up after agents removed from config)
    // and remove subdirs containing the marker that we didn't just install.
    let skip_cleanup = !registry.unreadable_registries.is_empty()
        || !registry.warnings.is_empty()
        || has_failed_predicate_preparation
        || has_failed_skill_install
        || collection.has_failures
        || !active_collection.is_complete
        || has_failed_dependency_listing;
    let mut scanned: BTreeSet<PathBuf> = BTreeSet::new();
    for &agent in Agent::all() {
        let parent = skills_parent_dir(agent, &project_root)?;
        if skip_cleanup || !scanned.insert(parent.clone()) {
            continue;
        }
        if let Err(error) = cleanup_stale_skills(&parent, &installed_dirs) {
            tracing::info!(report = %crate::report::ReportEvent::Warning {
                message: format!("skipped stale skill cleanup: {error}"),
            });
        }
    }
    if has_failed_skill_install
        || collection.has_failures
        || !active_collection.is_complete
        || has_failed_dependency_listing
        || has_failed_predicate_preparation
        || !registry.warnings.is_empty()
    {
        tracing::info!(report = %crate::report::ReportEvent::Warning {
            message: "skill discovery or installation was incomplete; skipped cleanup of installed skills".into(),
        });
    }
    for name in &registry.unreadable_registries {
        tracing::info!(
            report = %crate::report::ReportEvent::Warning {
                message: format!(
                    "registry `{name}` could not be read; skipped cleanup of installed skills"
                ),
            },
        );
    }

    // Hooks come off at whichever scope they would have been written to; MCP
    // entries come off at both, since a leftover one keeps pointing the agent
    // at a server this workspace no longer offers.
    for &agent in Agent::all() {
        if !agent_names.contains(&agent.config_name().to_string()) {
            let unregister = match sym.config.hook_scope {
                crate::config::HookScope::Global => {
                    agent.try_unregister_hooks(sym.home_dir(), sym, out)
                }
                crate::config::HookScope::Project => {
                    agent.try_unregister_project_hooks(&project_root, sym, out)
                }
            };
            report_unregister_failure(agent, "hooks", unregister);
            for scope in [
                crate::agents::McpScope::Project,
                crate::agents::McpScope::User,
            ] {
                report_unregister_failure(
                    agent,
                    "MCP servers",
                    agent.try_unregister_mcp_servers(
                        scope,
                        &project_root,
                        sym.home_dir(),
                        &server_names,
                        out,
                    ),
                );
            }
        }
    }

    if to_install.is_empty() {
        tracing::info!(
            report = %crate::report::ReportEvent::Info {
                message: "no applicable skills found for workspace dependencies".into(),
            },
        );
    }

    Ok(())
}

/// Register hooks and MCP servers for configured agents at user scope.
///
/// Initialization calls this after saving the user configuration. Removing
/// integrations for unconfigured agents remains visible best effort.
///
/// # Errors
///
/// Returns invalid registry sources, unsupported MCP variants, invalid agent
/// names, and agent registration failures, preserving their concrete sources.
pub async fn try_register_hooks(sym: &Symposium, out: &Output) -> SyncResult<()> {
    let registry = plugins::try_load_registry(sym).await?;
    let mcp_servers: Vec<sacp::schema::McpServer> = registry
        .plugins
        .iter()
        .flat_map(|p| p.manifest.mcp_servers.iter().map(|s| s.server.clone()))
        .collect();

    let server_names = mcp_server_names(&mcp_servers)?;

    let agent_names: Vec<String> = sym.config.agents.iter().map(|a| a.name.clone()).collect();

    // `init` has no workspace in hand, so this is the user-level pass only;
    // the project-level file is written by the first `sync` in a workspace.
    let home = sym.home_dir();
    let agents = agent_names
        .iter()
        .map(|name| Agent::try_from_config_name(name))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for agent in agents {
        agent.try_register_hooks(home, sym, out)?;
        agent.try_register_mcp_servers(
            crate::agents::McpScope::User,
            home,
            home,
            &mcp_servers,
            out,
        )?;
    }

    // Unregister hooks for agents no longer configured
    for &agent in Agent::all() {
        if !agent_names.contains(&agent.config_name().to_string()) {
            report_unregister_failure(agent, "hooks", agent.try_unregister_hooks(home, sym, out));
            report_unregister_failure(
                agent,
                "MCP servers",
                agent.try_unregister_mcp_servers(
                    crate::agents::McpScope::User,
                    home,
                    home,
                    &server_names,
                    out,
                ),
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source tree holding, in order: a plain file, a nested directory, an
    /// empty directory, a link to a file outside the tree, a link to a
    /// directory outside the tree, a broken link, and a link back to the root.
    #[cfg(unix)]
    fn tree_with_symlinks(root: &Path) -> PathBuf {
        use std::os::unix::fs::symlink;

        let outside = root.join("outside");
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::write(outside.join("lib.txt"), "shared").unwrap();
        fs::write(outside.join("nested/deep.txt"), "deep").unwrap();

        let src = root.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::create_dir(src.join("empty")).unwrap();
        fs::write(src.join("SKILL.md"), "skill").unwrap();
        fs::write(src.join("sub/plain.txt"), "plain").unwrap();
        symlink("../outside/lib.txt", src.join("lib.txt")).unwrap();
        // A chain: the first link resolves to a second one, itself relative to
        // a third directory. Whole chains are followed, not just one hop.
        symlink("../outside/hop.txt", src.join("chain.txt")).unwrap();
        symlink("nested/../lib.txt", outside.join("hop.txt")).unwrap();
        symlink("../outside", src.join("vendor")).unwrap();
        symlink("gone.txt", src.join("broken.txt")).unwrap();
        symlink(".", src.join("loop")).unwrap();
        src
    }

    /// The installed copy is the content behind the links, as real files, and
    /// the comparison that guards the next sync agrees with what was written.
    #[cfg(unix)]
    #[test]
    fn copy_follows_symlinks_and_compares_equal_afterwards() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tree_with_symlinks(tmp.path());
        let dst = tmp.path().join("dst");

        copy_dir_recursive(&src, &dst).unwrap();

        assert!(
            !fs::symlink_metadata(dst.join("lib.txt"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(dst.join("lib.txt")).unwrap(), "shared");
        assert_eq!(
            fs::read_to_string(dst.join("chain.txt")).unwrap(),
            "shared",
            "a link chain resolves to its final target"
        );
        assert_eq!(
            fs::read_to_string(dst.join("vendor/lib.txt")).unwrap(),
            "shared"
        );
        assert_eq!(
            fs::read_to_string(dst.join("vendor/nested/deep.txt")).unwrap(),
            "deep"
        );
        assert_eq!(
            fs::read_to_string(dst.join("sub/plain.txt")).unwrap(),
            "plain"
        );
        assert!(
            dst.join("empty").is_dir(),
            "an empty directory still copies"
        );
        assert!(!dst.join("broken.txt").exists(), "a broken link is skipped");
        assert!(
            !dst.join("loop").exists(),
            "a link back to the root is skipped"
        );

        assert!(
            !dir_contents_differ(&src, &dst).unwrap(),
            "the source scan must see exactly what the copier wrote, or every \
             sync turns into a full recopy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unfollowable_entries_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tree_with_symlinks(tmp.path());

        let (_, skipped) = walk_tree(&src).unwrap();

        assert!(
            skipped
                .iter()
                .any(|m| m.contains("broken.txt") && m.contains("broken symlink")),
            "{skipped:?}"
        );
        assert!(
            skipped
                .iter()
                .any(|m| m.contains("loop") && m.contains("re-enters")),
            "{skipped:?}"
        );
    }

    /// A change behind a link is a change to the skill.
    #[cfg(unix)]
    #[test]
    fn editing_a_linked_file_is_seen_as_a_difference() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tree_with_symlinks(tmp.path());
        let dst = tmp.path().join("dst");
        copy_dir_recursive(&src, &dst).unwrap();

        fs::write(tmp.path().join("outside/lib.txt"), "edited").unwrap();

        assert!(dir_contents_differ(&src, &dst).unwrap());
    }

    #[test]
    fn a_plain_tree_copies_and_compares_equal() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("SKILL.md"), "skill").unwrap();
        fs::write(src.join("sub/a.txt"), "a").unwrap();
        let dst = tmp.path().join("dst");

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("sub/a.txt")).unwrap(), "a");
        assert!(!dir_contents_differ(&src, &dst).unwrap());
    }

    /// Managed metadata is excluded from the comparison, so writing the marker
    /// and gitignore into the destination does not make it look changed.
    #[test]
    fn managed_metadata_is_not_skill_content() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("SKILL.md"), "skill").unwrap();
        let dst = tmp.path().join("dst");

        copy_dir_recursive(&src, &dst).unwrap();
        mark_generated_skill_directory(&dst).unwrap();

        assert!(!dir_contents_differ(&src, &dst).unwrap());
    }

    fn isolated_symposium(root: &Path) -> Symposium {
        let mut sym = Symposium::try_from_dir(root).unwrap();
        sym.config.defaults.symposium_recommendations = false;
        sym.config.defaults.user_plugins = false;
        sym.config.agents_syncing = false;
        sym.config.sync_debounce_secs = 0;
        sym.config.hook_scope = crate::config::HookScope::Project;
        sym.config.agents = vec![crate::config::AgentEntry {
            name: "claude".into(),
        }];
        sym
    }

    #[test]
    fn failed_source_read_preserves_the_installed_skill_and_error_source() {
        use std::error::Error;
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("installed");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("SKILL.md"), "previous contents").unwrap();
        mark_generated_skill_directory(&destination).unwrap();
        let missing = temp.path().join("missing");

        let error =
            sync_skill_dir(&missing, &destination, temp.path(), Duration::ZERO).unwrap_err();

        assert!(matches!(&error, SyncError::Io { path, source, .. }
            if path == &missing && source.kind() == std::io::ErrorKind::NotFound));
        assert!(error.source().unwrap().is::<std::io::Error>());
        assert_eq!(
            fs::read_to_string(destination.join("SKILL.md")).unwrap(),
            "previous contents"
        );
        assert!(destination.join(MARKER_FILE).is_file());
    }

    #[test]
    fn copying_a_missing_source_does_not_create_a_destination() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("installed");
        assert!(copy_dir_recursive(&temp.path().join("missing"), &destination).is_err());
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_cleanup_scan_does_not_remove_any_installed_skill() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let stale = temp.path().join("previous");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("SKILL.md"), "keep this artifact").unwrap();
        mark_generated_skill_directory(&stale).unwrap();
        let unreadable = temp.path().join("unresolvable");
        symlink("unresolvable", &unreadable).unwrap();

        let error = cleanup_stale_skills(temp.path(), &BTreeSet::new()).unwrap_err();

        assert!(matches!(error, SyncError::Io { path, .. } if path == unreadable));
        assert_eq!(
            fs::read_to_string(stale.join("SKILL.md")).unwrap(),
            "keep this artifact"
        );
    }

    #[test]
    fn complete_cleanup_removes_only_stale_managed_skills() {
        let temp = tempfile::tempdir().unwrap();
        let stale = temp.path().join("stale");
        let active = temp.path().join("active");
        let user = temp.path().join("user");
        for path in [&stale, &active, &user] {
            fs::create_dir(path).unwrap();
            fs::write(path.join("SKILL.md"), "skill").unwrap();
        }
        mark_generated_skill_directory(&stale).unwrap();
        mark_generated_skill_directory(&active).unwrap();

        cleanup_stale_skills(temp.path(), &BTreeSet::from([active.clone()])).unwrap();

        assert!(!stale.exists());
        assert!(active.join("SKILL.md").is_file());
        assert!(user.join("SKILL.md").is_file());
    }

    #[tokio::test]
    async fn typed_sync_checks_registry_sources_before_workspace_or_agent_effects() {
        let temp = tempfile::tempdir().unwrap();
        let mut sym = isolated_symposium(temp.path());
        sym.config.registries.push(crate::config::RegistryConfig {
            name: "ambiguous".into(),
            git: Some("https://synthetic-canary.invalid/private".into()),
            path: Some("local".into()),
            auto_update: false,
        });
        let deps = Arc::new(WorkspaceDeps::detached());

        let error = try_sync(&sym, &deps, UpdateLevel::None).await.unwrap_err();

        assert!(matches!(
            error,
            SyncError::Registry(RegistryError::BothSources { .. })
        ));
        assert!(!temp.path().join(".claude").exists());
        assert!(!temp.path().join("config.toml").exists());
    }

    #[tokio::test]
    async fn canonical_sync_reports_missing_workspace_without_mutating_agents() {
        let temp = tempfile::tempdir().unwrap();
        let sym = isolated_symposium(temp.path());
        let error = try_sync(
            &sym,
            &Arc::new(WorkspaceDeps::detached()),
            UpdateLevel::None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, SyncError::WorkspaceRequired));
        assert!(!temp.path().join(".claude").exists());
    }

    #[tokio::test]
    async fn failed_workspace_lookup_preserves_its_cause_before_agent_effects() {
        use std::error::Error;
        let temp = tempfile::tempdir().unwrap();
        let sym = isolated_symposium(temp.path());
        let missing_project = temp.path().join("missing-project");
        let deps = Arc::new(WorkspaceDeps::new(&missing_project, sym.dirs()));

        let error = try_sync(&sym, &deps, UpdateLevel::None).await.unwrap_err();

        let SyncError::WorkspaceLookup { source } = error else {
            panic!("workspace failure must remain distinct from optional absence");
        };
        assert!(
            matches!(source.as_ref(), crate::pm::WorkspaceError::Io { path, source, .. }
            if path == &missing_project && source.kind() == std::io::ErrorKind::NotFound)
        );
        assert!(source.source().unwrap().is::<std::io::Error>());
        assert!(!temp.path().join(".claude").exists());
        assert!(!temp.path().join("config.toml").exists());
    }

    #[tokio::test]
    async fn unreadable_skill_retains_previous_installation_and_skips_stale_cleanup() {
        use tracing_subscriber::prelude::*;
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let source = project.join("skills/stable");
        fs::create_dir_all(&source).unwrap();
        let original = "---\nname: stable\ndescription: stable fixture\n---\nPrevious skill body\n";
        fs::write(source.join("SKILL.md"), original).unwrap();
        let sym = isolated_symposium(&home);
        let deps = WorkspaceDeps::fixture(&project, Vec::new());
        try_sync(&sym, &deps, UpdateLevel::None).await.unwrap();
        let installed = project.join(".claude/skills/stable");
        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            original
        );
        let stale = project.join(".claude/skills/previous");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("SKILL.md"), "previous artifact").unwrap();
        mark_generated_skill_directory(&stale).unwrap();
        // This remains a discovered regular file but cannot be read as UTF-8.
        fs::write(source.join("SKILL.md"), [0xff]).unwrap();
        let (layer, handle) =
            crate::report::ReportLayer::new(crate::report::ReportMode::Json, tracing::Level::INFO);
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

        try_sync(&sym, &deps, UpdateLevel::None).await.unwrap();

        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            original
        );
        assert_eq!(
            fs::read_to_string(stale.join("SKILL.md")).unwrap(),
            "previous artifact"
        );
        let reports = handle.drain();
        assert!(
            reports.iter().any(|report| report["kind"] == "warning"
                && report["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("skipped cleanup"))),
            "{reports:?}"
        );
        assert!(
            !reports
                .iter()
                .any(|report| report["kind"] == "skill_removed")
        );
    }

    #[tokio::test]
    async fn failed_removed_agent_cleanup_is_visible_and_preserves_foreign_config() {
        use tracing_subscriber::prelude::*;
        let temp = tempfile::tempdir().unwrap();
        let mut sym = isolated_symposium(temp.path());
        sym.config.agents.clear();
        let settings = temp.path().join(".claude/settings.json");
        fs::create_dir(settings.parent().unwrap()).unwrap();
        let invalid = r#"{"foreign":"synthetic-canary" invalid}"#;
        fs::write(&settings, invalid).unwrap();
        let (layer, handle) =
            crate::report::ReportLayer::new(crate::report::ReportMode::Json, tracing::Level::INFO);
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

        try_register_hooks(&sym, &Output::capturing())
            .await
            .unwrap();

        assert_eq!(fs::read_to_string(settings).unwrap(), invalid);
        let reports = handle.drain();
        assert!(
            reports.iter().any(|report| report["kind"] == "warning"
                && report["message"].as_str().is_some_and(
                    |message| message.contains("failed to unregister hooks for Claude")
                )),
            "{reports:?}"
        );
        assert!(
            !reports
                .iter()
                .any(|report| report.to_string().contains("synthetic-canary"))
        );
    }

    #[tokio::test]
    async fn unreadable_enabled_crate_plugin_preserves_its_previous_skill() {
        use tracing_subscriber::prelude::*;
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let dependency = temp.path().join("dependency");
        fs::create_dir(&project).unwrap();
        fs::create_dir_all(dependency.join("skills/stable")).unwrap();
        let cargo_manifest = dependency.join("Cargo.toml");
        fs::write(
            &cargo_manifest,
            "[package]\nname = 'dep-fixture'\nversion = '1.0.0'\n",
        )
        .unwrap();
        let skill = "---\nname: dep-stable\ndescription: dependency fixture\n---\nPrevious dependency advice\n";
        fs::write(dependency.join("skills/stable/SKILL.md"), skill).unwrap();
        let mut sym = isolated_symposium(&home);
        sym.config.plugins.auto_enable.push("dep-fixture".into());
        let deps = WorkspaceDeps::fixture(
            &project,
            vec![crate::pm::WorkspaceCrate::new(
                "dep-fixture".into(),
                semver::Version::new(1, 0, 0),
                Some(dependency),
            )],
        );
        try_sync(&sym, &deps, UpdateLevel::None).await.unwrap();
        let installed = project.join(".claude/skills/dep-stable");
        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            skill
        );
        // The dependency remains resolved locally; reading its plugin metadata
        // now fails. The active-plugin outcome must retain that incompleteness.
        fs::write(cargo_manifest, [0xff]).unwrap();
        let (layer, handle) =
            crate::report::ReportLayer::new(crate::report::ReportMode::Json, tracing::Level::INFO);
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

        try_sync(&sym, &deps, UpdateLevel::None).await.unwrap();

        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            skill
        );
        let reports = handle.drain();
        assert!(
            reports.iter().any(|report| report["kind"] == "warning"
                && report["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("skipped cleanup"))),
            "{reports:?}"
        );
        assert!(
            !reports
                .iter()
                .any(|report| report["kind"] == "skill_removed")
        );
    }

    #[tokio::test]
    async fn invalid_registry_plugin_preserves_its_previous_skill() {
        use tracing_subscriber::prelude::*;
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let registry = temp.path().join("registry");
        let plugin = registry.join("entry");
        fs::create_dir(&project).unwrap();
        fs::create_dir_all(plugin.join("skills/stable")).unwrap();
        let manifest = plugin.join("Symposium.toml");
        fs::write(
            &manifest,
            "name = 'registry-fixture'\ndepends-on = ['*']\n[[skills]]\nsource.path = 'skills'\n",
        )
        .unwrap();
        let skill = "---\nname: registry-stable\ndescription: registry fixture\n---\nPrevious registry advice\n";
        fs::write(plugin.join("skills/stable/SKILL.md"), skill).unwrap();
        let mut sym = isolated_symposium(&temp.path().join("home"));
        sym.config.registries.push(crate::config::RegistryConfig {
            name: "fixture".into(),
            git: None,
            path: Some(registry.to_string_lossy().into_owned()),
            auto_update: false,
        });
        let deps = WorkspaceDeps::fixture(&project, Vec::new());
        try_sync(&sym, &deps, UpdateLevel::None).await.unwrap();
        let installed = project.join(".claude/skills/registry-stable");
        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            skill
        );
        // Valid TOML, but an invalid source sum: one group cannot be both a
        // local path and a Git source. Validation must mark the scan incomplete.
        fs::write(&manifest, "name = 'registry-fixture'\ndepends-on = ['*']\n[[skills]]\nsource.path = 'skills'\nsource.git = 'https://synthetic.invalid/repo'\n").unwrap();
        let (layer, handle) =
            crate::report::ReportLayer::new(crate::report::ReportMode::Json, tracing::Level::INFO);
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

        try_sync(&sym, &deps, UpdateLevel::None).await.unwrap();

        assert_eq!(
            fs::read_to_string(installed.join("SKILL.md")).unwrap(),
            skill
        );
        let reports = handle.drain();
        assert!(
            reports.iter().any(|report| report["kind"] == "warning"
                && report["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("skipped cleanup"))),
            "{reports:?}"
        );
        assert!(
            !reports
                .iter()
                .any(|report| report["kind"] == "skill_removed")
        );
    }

    fn previous_skill(destination: &Path) {
        fs::create_dir(destination).unwrap();
        fs::write(destination.join("SKILL.md"), "previous advice").unwrap();
        mark_generated_skill_directory(destination).unwrap();
        fs::write(destination.join(MARKER_FILE), "previous marker bytes").unwrap();
    }

    fn assert_previous_skill(destination: &Path) {
        assert_eq!(
            fs::read_to_string(destination.join("SKILL.md")).unwrap(),
            "previous advice"
        );
        assert_eq!(
            fs::read_to_string(destination.join(MARKER_FILE)).unwrap(),
            "previous marker bytes"
        );
        assert_eq!(
            fs::read_to_string(destination.join(".gitignore")).unwrap(),
            "*\n"
        );
    }

    #[test]
    fn failed_staged_marker_write_preserves_previous_artifact_and_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("installed");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("SKILL.md"), "new advice").unwrap();
        // Copy succeeds, but managed metadata cannot replace this directory.
        // This is a real deterministic I/O failure after content preparation.
        fs::create_dir(source.join(MARKER_FILE)).unwrap();
        previous_skill(&destination);

        let error = sync_skill_dir(&source, &destination, temp.path(), Duration::ZERO).unwrap_err();

        assert!(
            matches!(&error, SyncError::Io { operation: "write marker", path, .. }
            if path.file_name().unwrap() == MARKER_FILE)
        );
        assert_previous_skill(&destination);
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            2,
            "staging must be cleaned"
        );
    }

    #[test]
    fn failed_skill_publication_restores_previous_artifact_and_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("installed");
        previous_skill(&destination);

        let error = publish_skill(
            &temp.path().join("missing-prepared"),
            &destination,
            &temp.path().join("previous"),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            SyncError::Io {
                operation: "publish prepared skill",
                ..
            }
        ));
        assert_previous_skill(&destination);
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            1,
            "rollback must remove its empty backup"
        );
    }

    #[test]
    fn successful_staged_replacement_publishes_complete_skill_and_removes_backup() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("installed");
        fs::create_dir_all(source.join("empty")).unwrap();
        fs::write(source.join("SKILL.md"), "new advice").unwrap();
        previous_skill(&destination);

        assert!(sync_skill_dir(&source, &destination, temp.path(), Duration::ZERO).unwrap());

        assert_eq!(
            fs::read_to_string(destination.join("SKILL.md")).unwrap(),
            "new advice"
        );
        assert!(destination.join("empty").is_dir());
        assert_eq!(
            fs::read_to_string(destination.join(MARKER_FILE)).unwrap(),
            ""
        );
        assert_eq!(
            fs::read_to_string(destination.join(".gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
    }

    #[test]
    fn publication_preserves_a_user_managed_destination() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("installed");
        let prepared = temp.path().join("prepared");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("SKILL.md"), "user advice").unwrap();
        fs::create_dir(&prepared).unwrap();
        fs::write(prepared.join("SKILL.md"), "managed advice").unwrap();
        mark_generated_skill_directory(&prepared).unwrap();

        let error =
            publish_skill(&prepared, &destination, &temp.path().join("previous")).unwrap_err();

        assert!(matches!(error, SyncError::UnmanagedDestination { path } if path == destination));
        assert_eq!(
            fs::read_to_string(destination.join("SKILL.md")).unwrap(),
            "user advice"
        );
        assert!(!destination.join(MARKER_FILE).exists());
        assert!(prepared.join("SKILL.md").is_file());
    }

    #[test]
    fn failed_fresh_staging_does_not_publish_a_partial_installation() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("installed");

        assert!(
            sync_skill_dir(
                &temp.path().join("missing"),
                &destination,
                temp.path(),
                Duration::ZERO
            )
            .is_err()
        );

        assert!(!destination.exists());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}
