//! Skill model, frontmatter parsing, discovery, and crate advice output.
//!
//! Skills follow the [agentskills.io](https://agentskills.io/specification.md) format
//! and live inside plugin directories under `skills/*/SKILL.md`.

use crate::predicate::PredicateEval;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use symposium_install::UpdateLevel;
use symposium_sdk::predicate::PredicateSyntaxError;
use thiserror::Error;

use crate::config::Symposium;
use crate::plugins::{Plugin, PluginSource, SkillGroup};
use crate::predicate::{PredicateContext, PredicateSet};

/// A frontmatter failure whose display never includes source text or field values.
///
/// YAML parser diagnostics remain available through [`std::error::Error::source`]
/// and can contain private input. Do not recursively format those sources in logs.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FrontmatterError {
    /// The opening frontmatter delimiter is absent.
    #[error("SKILL.md must start with --- frontmatter fence")]
    MissingOpeningFence,
    /// The closing frontmatter delimiter is absent.
    #[error("no closing --- fence in frontmatter")]
    MissingClosingFence,
    /// The fenced text is malformed YAML.
    #[error("frontmatter is not valid YAML")]
    Yaml(#[source] serde_yaml_ng::Error),
    /// The YAML root is not a mapping.
    #[error("SKILL.md frontmatter must be a YAML mapping")]
    ExpectedMapping,
    /// A frontmatter key is not a string.
    #[error("SKILL.md frontmatter keys must be strings")]
    NonStringKey,
    /// A frontmatter value is not a string.
    #[error("SKILL.md frontmatter fields must be strings")]
    NonStringValue,
    /// The retired dependency field must be migrated.
    #[error("the `crates` frontmatter field has been renamed; use `depends-on` instead")]
    RetiredCrates,
}

/// A skill read, validation, discovery, or acquisition failure with safe display text.
///
/// Paths and concrete parser/I/O sources are retained for structured inspection.
/// Sources and `Debug` output can contain private data and must not be logged.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SkillError {
    /// The skill's Markdown file could not be read.
    #[error("failed to read SKILL.md")]
    Read {
        /// The affected skill file.
        path: PathBuf,
        /// The original filesystem failure.
        source: std::io::Error,
    },
    /// A directory or directory entry could not be inspected.
    #[error("failed to discover SKILL.md files")]
    Discover {
        /// The affected directory or entry.
        path: PathBuf,
        /// The original filesystem failure.
        source: std::io::Error,
    },
    /// A configured skill directory exists with a different filesystem type.
    #[error("skill source must be a directory")]
    NotDirectory {
        /// The configured source path.
        path: PathBuf,
    },
    /// A skill's frontmatter could not be parsed.
    #[error("failed to parse frontmatter in SKILL.md")]
    Frontmatter {
        /// The affected skill file.
        path: PathBuf,
        /// The concrete frontmatter failure.
        source: FrontmatterError,
    },
    /// Skill-level dependency or predicate syntax is invalid.
    #[error("invalid SKILL.md depends-on predicate or predicates")]
    Predicates {
        /// The affected skill file.
        path: PathBuf,
        /// The original predicate syntax failure.
        source: PredicateSyntaxError,
    },
    /// A required published-skill field is missing.
    #[error("SKILL.md frontmatter missing required `{field}` field")]
    MissingField {
        /// The fixed required field name.
        field: &'static str,
    },
    /// The skill name cannot safely name an installation directory.
    #[error("SKILL.md `name` must be a plain directory name")]
    UnsafeName,
    /// The description contains no non-whitespace text.
    #[error("SKILL.md `description` must not be empty")]
    EmptyDescription,
    /// The description exceeds the existing UTF-8 byte limit.
    #[error("SKILL.md `description` exceeds 1024 bytes ({byte_len} bytes)")]
    OversizeDescription {
        /// The observed UTF-8 byte length.
        byte_len: usize,
    },
    /// A Git skill source could not be validated, fetched, or inspected.
    #[error("failed to acquire Git skill source")]
    Git(#[source] symposium_install::git::GitError),
    /// The acquired Git source lacks metadata needed to pin its contents.
    #[error("skipping Git skill group: cache metadata missing; cannot pin commit SHA")]
    MissingGitMetadata {
        /// The acquired source directory.
        path: PathBuf,
    },
}

fn source_display(source: &PluginSource) -> String {
    match source {
        PluginSource::Path(p) => format!("path:{}", p.display()),
        PluginSource::Git(url) => match symposium_install::git::try_parse_git_url(url) {
            Ok(_) => format!("git:{url}"),
            Err(_) => "git:invalid source".to_string(),
        },
    }
}

/// The group's resolved source label: the string the package manager attached
/// to a `Path` source, or the derived form for a `Git` source.
fn group_source_label(group: &crate::plugins::SkillGroup) -> String {
    match &group.source {
        PluginSource::Git(_) => source_display(&group.source),
        PluginSource::Path(_) => group
            .source_label
            .clone()
            .unwrap_or_else(|| source_display(&group.source)),
    }
}

/// A parsed skill from a SKILL.md file.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Frontmatter fields as key-value pairs (name, description, license, etc.).
    pub frontmatter: BTreeMap<String, String>,
    /// Skill-level activation predicates: the frontmatter `depends-on` (lowered to
    /// `any(depends-on(...))`) merged with `predicates`. ANDed with the plugin- and
    /// group-level sets.
    pub predicates: PredicateSet,
    /// Path to the SKILL.md file on disk.
    pub path: PathBuf,
}

impl Skill {
    /// Return the skill name from frontmatter, or "unknown".
    pub fn name(&self) -> &str {
        self.frontmatter
            .get("name")
            .map_or("unknown", |s| s.as_str())
    }
}

// Install-path disambiguator identifying *where a skill's bytes live*.
//
// A skill's origin is the *canonical* on-disk path of its `SKILL.md`, hashed.
// Two references that resolve to the same file — the same crate reached through
// two chained plugins, or a `source.path` group and the standalone walk landing
// on the same bundle — produce the same hash and dedupe at sync time; skills at
// different paths stay distinct.
//
// Canonicalizing is what makes that hold. Group discovery walks a canonicalized
// scan dir, while the standalone registry walk uses the configured source path
// verbatim; on a platform whose temp prefix is a symlink (macOS `/var` ->
// `/private/var`) the same file would otherwise hash two ways and install twice.
//
// This is deliberately coarser than a structured `(pm, plugin, skill_path)`
// identity: the hash *is* both the dedup key and the disambiguating suffix, so
// only a string — not a structured origin — is carried to the sync layer.

/// 8-hex-char prefix of SHA-256 over the JSON-serialized origin key.
fn hash_origin_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let encoded = serde_json::Value::String(key.to_owned()).to_string();
    let digest = Sha256::digest(encoded.as_bytes());
    format!("{digest:x}").chars().take(8).collect()
}

/// Origin hash for a skill, keyed on its `SKILL.md`'s canonical path so that the
/// same file reached two ways hashes identically. Falls back to the given path
/// when it can't be canonicalized.
pub(crate) fn skill_origin_hash(skill_md: &Path) -> String {
    let canonical = skill_md.canonicalize();
    let path = canonical.as_deref().unwrap_or(skill_md);
    hash_origin_key(&path.to_string_lossy())
}

/// An applicable skill paired with the origin it was discovered through.
///
/// The plugin-, group-, and skill-level predicate sets are all evaluated during
/// collection; only skills whose every level holds end up here.
pub struct SkillWithGroupContext {
    pub skill: Skill,
    /// The hash of where the skill was discovered. Drives install-path disambiguation
    /// and dedup at sync time.
    pub origin_hash: String,
}

/// Successful skills and whether any source or item could not be inspected.
pub(crate) struct SkillCollection {
    /// Applicable skills successfully discovered from the available sources.
    pub skills: Vec<SkillWithGroupContext>,
    /// Cleanup must retain installed skills while a source or item is unresolved.
    pub has_failures: bool,
}

/// Resolve all applicable skills from the registry.
///
/// Resolve all skills applicable to the given crates.
///
/// `for_crates` is the set of crate name/version pairs to match against.
/// For `crate --list`, this is the full workspace deps.
/// For `crate <name>`, this is a single-element slice with the resolved crate.
///
/// `workspace_root` scopes the `[plugins] use` entries that apply, which
/// decide both which dormant plugins wake up and which dependency-embedded
/// plugins load. `None` (no workspace) means no entry applies.
/// Convenience wrapper over [`crate::plugins::active_plugins`] + [`collect_skills`] that builds
/// the predicate context itself. Production sync shares one context across the
/// skill and MCP passes, so it calls the two primitives directly; this keeps the
/// combined form for tests that only care about the resulting skills.
#[cfg(test)]
pub async fn skills_applicable_to(
    sym: &Symposium,
    registry: &crate::plugins::PluginRegistry,
    deps: &std::sync::Arc<crate::pm::WorkspaceDeps>,
    workspace_root: Option<&Path>,
    custom_predicate_entries: std::collections::HashMap<
        String,
        crate::predicate::ResolvedPredicateEntry,
    >,
    update: UpdateLevel,
) -> Vec<SkillWithGroupContext> {
    let for_crates = crate::pm::try_workspace_dep_ids(sym, deps)
        .await
        .expect("skill fixture has valid registry sources");
    let used_names = workspace_root
        .map(|root| sym.config.plugins.used_names_in(root))
        .unwrap_or_default();
    let mut ctx = PredicateContext::with_custom_predicates(&for_crates, custom_predicate_entries)
        .with_used_names(&used_names);

    let pms = sym
        .try_package_managers(deps)
        .expect("skill fixture has valid registry sources");
    let active =
        crate::plugins::active_plugins(sym, registry, &pms, workspace_root, &mut ctx).await;
    collect_skills(sym, &active, &mut ctx, update).await.skills
}

/// Extract the applicable skills from an already-resolved active plugin set.
///
/// Provenance is re-stamped per plugin so a `source.path` group's
/// `workspace-member()` predicate sees the plugin it belongs to. Skill git
/// sources are fetched here (hence `update`), which is why this is separate
/// from [`crate::plugins::active_plugins`] — the plugin walk never touches the network.
pub(crate) async fn collect_skills(
    sym: &Symposium,
    active: &[Plugin],
    ctx: &mut PredicateContext<'_>,
    update: UpdateLevel,
) -> SkillCollection {
    let mut collection = SkillCollection {
        skills: Vec::new(),
        has_failures: false,
    };
    for parsed in active {
        ctx.set_workspace_member(parsed.workspace_member);
        for group in &parsed.manifest.skills {
            let (skills, has_failures) =
                load_skills_for_group(sym, parsed, group, ctx, update).await;
            collection.has_failures |= has_failures;
            for (skill, origin_hash) in skills {
                collect_skill_applicable_to(
                    skill,
                    origin_hash,
                    &parsed.manifest.name,
                    ctx,
                    &mut collection.skills,
                );
            }
        }
    }
    collection
}

/// Discover and load skills for a group, applying pre-fetch filtering.
///
/// Checks group-level `depends-on` predicates against `for_crates` before
/// fetching git sources, to avoid unnecessary downloads. Each returned skill is
/// paired with the hash of its canonical on-disk SKILL.md path.
async fn load_skills_for_group(
    sym: &Symposium,
    parsed: &Plugin,
    group: &SkillGroup,
    ctx: &mut PredicateContext<'_>,
    update: UpdateLevel,
) -> (Vec<(Skill, String)>, bool) {
    let plugin = &parsed.manifest;

    // Pre-fetch filtering: skip groups whose predicates don't hold (crate
    // matching and runtime checks alike). Done before any git/crates fetch so
    // we don't pay network cost when the group doesn't apply.
    let predicates_display = group
        .predicates
        .predicates
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let predicates_display = (!predicates_display.is_empty()).then_some(predicates_display);

    if !group.predicates.evaluate(ctx) {
        tracing::debug!(plugin = %plugin.name, "skill group predicates failed, skipping");
        tracing::debug!(
            report = %crate::report::ReportEvent::SkillGroupConsidered {
                plugin: plugin.name.clone(),
                group_crates: predicates_display,
                source: Some(group_source_label(group)),
                matched: false,
                skills_found: None,
                reason: Some("group predicates not satisfied".into()),
            },
        );
        return (Vec::new(), false);
    }

    let resolved = match resolve_group_dirs(sym, parsed, group, update).await {
        Ok(resolved) => resolved,
        Err(error) => {
            tracing::warn!(error = %error, "failed to load skill group");
            return (Vec::new(), true);
        }
    };
    let (skills, has_failures) = collect_skills_from_dirs(resolved, group);

    tracing::debug!(
        report = %crate::report::ReportEvent::SkillGroupConsidered {
            plugin: plugin.name.clone(),
            group_crates: predicates_display,
            source: Some(group_source_label(group)),
            matched: true,
            skills_found: Some(skills.len()),
            reason: None,
        },
    );

    (skills, has_failures)
}

/// A base directory resolved for a skill group, with the report labels for the
/// skills discovered inside it. Both `source` variants reduce to this: `Path` is
/// already on disk; `Git` is fetched via the git cache. (A crate is not a group
/// source — it becomes a plugin through a `[[plugins]]` chained reference; see
/// [`expand_chained_plugins`].)
struct ResolvedSkillDir {
    dir: PathBuf,
    /// `SkillSourceSearched` report `plugin` label.
    plugin_label: String,
    /// `SkillSourceSearched` report `source` label.
    source_label: String,
}

/// Resolve a group's `source` to the base directories to scan. Both variants
/// land on a directory to walk; the only difference is whether the base is local
/// (`Path`) or fetched (`Git` via a git cache). Skill identity is not decided
/// here — every discovered skill's origin is the hash of its on-disk `SKILL.md`
/// path (see the module-level note above `skill_origin_hash`).
async fn resolve_group_dirs(
    sym: &Symposium,
    parsed: &Plugin,
    group: &SkillGroup,
    update: UpdateLevel,
) -> Result<Vec<ResolvedSkillDir>, SkillError> {
    let plugin = &parsed.manifest;

    match &group.source {
        // Resolved to an absolute directory by the package manager, so there is
        // nothing to join here.
        PluginSource::Path(dir) => Ok(vec![ResolvedSkillDir {
            dir: dir.clone(),
            plugin_label: plugin.name.clone(),
            source_label: group_source_label(group),
        }]),
        PluginSource::Git(url) => {
            let (cache_dir, source, _commit_sha) = fetch_git_skill_source(sym, url, update).await?;
            Ok(vec![ResolvedSkillDir {
                dir: cache_dir,
                plugin_label: source.repo_id(),
                source_label: format!("git:{url}"),
            }])
        }
    }
}

/// Discover skills in each resolved base dir and stamp origins. The single
/// path all group sources funnel through, replacing the former per-source
/// `load_*_skills` functions.
fn collect_skills_from_dirs(
    resolved: Vec<ResolvedSkillDir>,
    group: &SkillGroup,
) -> (Vec<(Skill, String)>, bool) {
    let mut skills = Vec::new();
    let mut has_failures = false;
    for entry in resolved {
        let discovered = discover_skills(&entry.dir, group.workspace_member, &group.predicates);
        tracing::debug!(
            report = %crate::report::ReportEvent::SkillSourceSearched {
                plugin: entry.plugin_label.clone(),
                source: entry.source_label.clone(),
                path: entry.dir.display().to_string(),
                skills_found: discovered.iter().filter(|r| r.is_ok()).count(),
            },
        );
        for result in discovered {
            match result {
                Ok(skill) => {
                    let origin_hash = skill_origin_hash(&skill.path);
                    skills.push((skill, origin_hash));
                }
                Err(error) => {
                    has_failures = true;
                    tracing::warn!(source = %entry.source_label, error = %error, "failed to load skill");
                }
            }
        }
    }
    (skills, has_failures)
}

/// Fetch a `source.git` group's tarball and look up the resolved commit
/// SHA from the cache meta. Returns `(cache_dir, parsed_source, commit_sha)`
/// or a typed failure; the group boundary reports failure and retains other groups.
async fn fetch_git_skill_source(
    sym: &Symposium,
    git_url: &str,
    update: UpdateLevel,
) -> Result<(PathBuf, symposium_install::git::GitSource, String), SkillError> {
    let cache_mgr =
        symposium_install::git::GitCacheManager::try_new(&sym.install_context(), "plugins")
            .map_err(SkillError::Git)?;
    let (cache_dir, source) = cache_mgr
        .try_fetch_url_parsed(git_url, update)
        .await
        .map_err(SkillError::Git)?;
    let meta = cache_mgr
        .try_read_meta_for(&cache_dir)
        .map_err(SkillError::Git)?
        .ok_or_else(|| SkillError::MissingGitMetadata {
            path: cache_dir.clone(),
        })?;
    Ok((cache_dir, source, meta.commit_sha))
}

/// Discover all skills found in a given directory.
///
/// Recursively searches for `SKILL.md` files, then prunes nested candidates
/// (if `A/SKILL.md` exists, `A/B/SKILL.md` is excluded — skills don't nest).
pub(crate) fn discover_skills(
    skills_dir: &Path,
    workspace_member: bool,
    group_predicates: &PredicateSet,
) -> Vec<Result<Skill, SkillError>> {
    match std::fs::metadata(skills_dir) {
        Ok(metadata) if !metadata.is_dir() => {
            return vec![Err(SkillError::NotDirectory {
                path: skills_dir.to_path_buf(),
            })];
        }
        Ok(_) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(source) => {
            return vec![Err(SkillError::Discover {
                path: skills_dir.to_path_buf(),
                source,
            })];
        }
    }

    let mut skill_files = Vec::new();
    let errors = find_skill_files_recursive(skills_dir, &mut skill_files);
    prune_nested_skills(&mut skill_files);

    errors
        .into_iter()
        .map(Err)
        .chain(
            skill_files
                .into_iter()
                .map(|skill_md| load_skill(&skill_md, workspace_member, group_predicates)),
        )
        .collect()
}

/// Recursively walk a directory collecting paths to `SKILL.md` files.
///
/// Directories carrying the `.symposium` marker are skipped: the marker means
/// symposium itself installed the directory, and installed copies are never
/// sources. This matters for `.agents/skills/`, which is both a workspace
/// skill-group source and the install destination for vendor-neutral agents.
pub(crate) fn find_skill_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Vec<SkillError> {
    let mut ancestors = crate::dir_walk::Ancestors::rooted_at(dir);
    let mut errors = Vec::new();
    find_skill_files_in(dir, &mut ancestors, out, &mut errors);
    errors
}

fn find_skill_files_in(
    dir: &Path,
    ancestors: &mut crate::dir_walk::Ancestors,
    out: &mut Vec<PathBuf>,
    errors: &mut Vec<SkillError>,
) {
    let marker = dir.join(crate::sync::MARKER_FILE);
    match std::fs::metadata(&marker) {
        Ok(_) => return,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            errors.push(SkillError::Discover {
                path: marker,
                source,
            });
            return;
        }
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(source) => {
            errors.push(SkillError::Discover {
                path: dir.to_path_buf(),
                source,
            });
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                errors.push(SkillError::Discover {
                    path: dir.to_path_buf(),
                    source,
                });
                continue;
            }
        };
        let path = entry.path();
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) => {
                errors.push(SkillError::Discover { path, source });
                continue;
            }
        };
        // `is_dir` follows symlinks, so a symlinked directory of skills is
        // discovered — and can name a directory this walk is already inside.
        if metadata.is_dir() {
            if ancestors.enter(&path) {
                find_skill_files_in(&path, ancestors, out, errors);
                ancestors.leave();
            }
        } else if path.file_name().is_some_and(|f| f == "SKILL.md") {
            out.push(path);
        }
    }
}

/// Remove nested skill candidates: if `A/SKILL.md` and `A/B/SKILL.md` both
/// exist, keep only the shallower `A/SKILL.md`.
pub(crate) fn prune_nested_skills(paths: &mut Vec<PathBuf>) {
    // Sort shallowest first so we encounter parents before children.
    paths.sort_by_key(|p| p.components().count());
    let mut kept: Vec<PathBuf> = Vec::new();
    for path in paths.drain(..) {
        let nested = path.parent().is_some_and(|skill_dir| {
            kept.iter()
                .filter_map(|path| path.parent())
                .any(|parent| skill_dir.starts_with(parent))
        });
        if !nested {
            kept.push(path);
        }
    }
    *paths = kept;
}

/// Merge a SKILL.md's frontmatter `depends-on` (dependency atoms) and
/// `predicates` (function-call syntax) into one predicate set.
fn frontmatter_predicates(
    depends_on: Option<&str>,
    predicates: Option<&str>,
) -> Result<PredicateSet, PredicateSyntaxError> {
    let depends_on = match depends_on {
        Some(s) => Some(crate::predicate::DependsOnList::try_parse(s)?),
        None => None,
    };
    let extra = match predicates {
        Some(s) => PredicateSet::try_parse(s)?,
        None => PredicateSet::default(),
    };
    Ok(PredicateSet::merged(depends_on, extra))
}

/// The frontmatter a synthesized plugin needs from a bare `SKILL.md` entry:
/// its declared `name` (the skill's identity, used as the plugin name) and
/// its `depends-on`/`predicates` hoisted into one set (so the plugin is gated
/// by them and the ordinary dormancy rule applies — a bare skill with no
/// dependency is dormant until `use`d). See
/// [`plugins::standalone_skill_manifest`](crate::plugins::standalone_skill_manifest).
pub(crate) fn standalone_skill_meta(
    skill_md: &Path,
) -> Result<(Option<String>, PredicateSet), SkillError> {
    let content = std::fs::read_to_string(skill_md).map_err(|source| SkillError::Read {
        path: skill_md.to_path_buf(),
        source,
    })?;
    let fm = parse_frontmatter(&content).map_err(|source| SkillError::Frontmatter {
        path: skill_md.to_path_buf(),
        source,
    })?;
    let name = fm.fields.get("name").map(|n| {
        n.strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(n)
            .to_string()
    });
    let predicates = frontmatter_predicates(fm.depends_on.as_deref(), fm.predicates.as_deref())
        .map_err(|source| SkillError::Predicates {
            path: skill_md.to_path_buf(),
            source,
        })?;
    Ok((name, predicates))
}

/// Load a single skill from a SKILL.md file.
///
/// A skill should have `depends-on` at either the skill level or
/// the group level (or both). If neither provides it, a warning is logged
/// but loading succeeds (the skill simply won't match any dependency query).
///
/// Workspace-member groups load leniently: the frontmatter (and its `name`
/// and `description` fields) is optional — `name` falls back to the skill
/// directory's name. Workspace skills are the maintainers' own informal
/// notes; the agentskills.io contract applies to published skills.
fn load_skill(
    skill_md_path: &Path,
    workspace_member: bool,
    group_predicates: &PredicateSet,
) -> Result<Skill, SkillError> {
    let content = std::fs::read_to_string(skill_md_path).map_err(|source| SkillError::Read {
        path: skill_md_path.to_path_buf(),
        source,
    })?;

    let fm = if workspace_member && !content.trim_start().starts_with("---") {
        RawFrontmatter {
            fields: BTreeMap::new(),
            depends_on: None,
            predicates: None,
        }
    } else {
        parse_frontmatter(&content).map_err(|source| SkillError::Frontmatter {
            path: skill_md_path.to_path_buf(),
            source,
        })?
    };

    let mut frontmatter = fm.fields;

    // Strip surrounding quotes from name if present (YAML scalars may be quoted)
    if let Some(name) = frontmatter.get_mut("name")
        && let Some(unquoted) = name.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
    {
        *name = unquoted.to_string();
    }

    if workspace_member
        && !frontmatter.contains_key("name")
        && let Some(dir_name) = skill_md_path
            .parent()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
    {
        frontmatter.insert("name".to_string(), dir_name.to_string());
    }

    let name = frontmatter
        .get("name")
        .ok_or(SkillError::MissingField { field: "name" })?;
    if !is_safe_dir_name(name) {
        return Err(SkillError::UnsafeName);
    }

    // Validate description per agentskills.io spec
    // (https://agentskills.io/specification.md): required, non-empty, max 1024 chars.
    match frontmatter.get("description") {
        None if workspace_member => {}
        None => {
            return Err(SkillError::MissingField {
                field: "description",
            });
        }
        Some(desc) => {
            let trimmed_desc = desc.trim();
            if trimmed_desc.is_empty() {
                return Err(SkillError::EmptyDescription);
            }
            if trimmed_desc.len() > 1024 {
                return Err(SkillError::OversizeDescription {
                    byte_len: trimmed_desc.len(),
                });
            }
        }
    }

    // Merge the skill-level `depends-on` (dependency atoms, OR-combined) with
    // the frontmatter `predicates` (function-call syntax) into one set, ANDed
    // with the plugin- and group-level sets at match time.
    let predicates = frontmatter_predicates(fm.depends_on.as_deref(), fm.predicates.as_deref())
        .map_err(|source| SkillError::Predicates {
            path: skill_md_path.to_path_buf(),
            source,
        })?;

    // Warn if no dependency is referenced at either level — the skill won't
    // match any dependency query, but we don't fail so a misconfigured plugin
    // can't bring down the tool.
    if !predicates.mentions_dep() && !group_predicates.mentions_dep() {
        tracing::warn!(
            skill = %name,
            "skill references no dependency in SKILL.md frontmatter or its plugin [[skills]] group"
        );
    }

    let skill = Skill {
        frontmatter,
        predicates,
        path: skill_md_path.to_path_buf(),
    };
    tracing::debug!(name = %skill.name(), path = %skill_md_path.display(), "skill loaded");
    Ok(skill)
}

pub(crate) fn is_safe_dir_name(name: &str) -> bool {
    // Windows trims trailing dots and spaces, so a name made only of them resolves to the parent.
    let only_dots_and_spaces = name.chars().all(|c| c == '.' || c == ' ');
    !only_dots_and_spaces && !name.contains(['/', '\\', ':'])
}

/// Evaluate the skill-level predicate set and collect the skill if it holds.
///
/// Plugin- and group-level predicates have already been evaluated by callers as
/// a pre-filter, so only the skill-level set is checked here.
fn collect_skill_applicable_to(
    skill: Skill,
    origin_hash: String,
    plugin_name: &str,
    ctx: &mut PredicateContext,
    results: &mut Vec<SkillWithGroupContext>,
) {
    if !skill.predicates.evaluate(ctx) {
        tracing::debug!(
            report = %crate::report::ReportEvent::SkillConsidered {
                skill: skill.name().to_string(),
                plugin: plugin_name.to_string(),
                matched: false,
                reason: Some("skill-level predicates not satisfied".into()),
            },
        );
        return;
    }

    tracing::debug!(
        report = %crate::report::ReportEvent::SkillConsidered {
            skill: skill.name().to_string(),
            plugin: plugin_name.to_string(),
            matched: true,
            reason: None,
        },
    );
    results.push(SkillWithGroupContext { skill, origin_hash });
}

/// Raw frontmatter fields extracted from a SKILL.md file.
/// `depends-on` is comma-separated on a single line.
#[derive(Debug)]
struct RawFrontmatter {
    fields: BTreeMap<String, String>,
    /// Raw `depends-on` value (comma-separated predicate string).
    depends_on: Option<String>,
    /// Raw `predicates` value (comma-separated predicate expressions).
    predicates: Option<String>,
}

/// Parse SKILL.md content: extract the `---`-fenced frontmatter. The body
/// after the frontmatter is not retained — skills install by copying the
/// file, not by re-emitting parsed content.
fn parse_frontmatter(content: &str) -> Result<RawFrontmatter, FrontmatterError> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(FrontmatterError::MissingOpeningFence);
    }

    let after_first_fence = &trimmed[3..];
    let after_first_fence = after_first_fence
        .strip_prefix('\n')
        .unwrap_or(after_first_fence);

    let end = after_first_fence
        .find("\n---")
        .ok_or(FrontmatterError::MissingClosingFence)?;

    let frontmatter_text = &after_first_fence[..end];

    let yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(frontmatter_text).map_err(FrontmatterError::Yaml)?;
    let mapping = yaml.as_mapping().ok_or(FrontmatterError::ExpectedMapping)?;

    let mut fields = BTreeMap::new();
    let mut depends_on = None;
    let mut predicates = None;

    for (key, value) in mapping {
        let Some(key) = key.as_str() else {
            return Err(FrontmatterError::NonStringKey);
        };

        let Some(value) = value.as_str() else {
            return Err(FrontmatterError::NonStringValue);
        };

        match key {
            "depends-on" => depends_on = Some(value.to_string()),
            "crates" => {
                return Err(FrontmatterError::RetiredCrates);
            }
            "predicates" => predicates = Some(value.to_string()),
            _ => {
                fields.insert(key.to_string(), value.to_string());
            }
        }
    }

    Ok(RawFrontmatter {
        fields,
        depends_on,
        predicates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::fs;

    use crate::{
        pm::{ANY_VERSION, PackageId},
        predicate::Predicate,
    };

    /// Build a predicate set from dependency atoms (the `depends-on` field form).
    fn pred_set(s: &str) -> PredicateSet {
        PredicateSet::from_depends_on(s).unwrap()
    }

    fn group(source: PluginSource) -> SkillGroup {
        SkillGroup {
            source,
            predicates: PredicateSet::default(),
            source_label: None,
            workspace_member: false,
        }
    }

    fn plugin_with_skill_groups(skills: Vec<SkillGroup>) -> Plugin {
        Plugin {
            canonical: PackageId::any_version("test", "typed-skills"),
            workspace_member: false,
            manifest: crate::plugins::PluginManifest {
                name: "typed-skills".into(),
                predicates: PredicateSet::default(),
                skills,
                hooks: vec![],
                mcp_servers: vec![],
                installations: vec![],
                subcommands: BTreeMap::new(),
                custom_predicates: vec![],
                chained: vec![],
                requires_use: false,
            },
        }
    }

    #[test]
    fn origin_hash_preserves_json_string_encoding_and_sha256_prefix() {
        for (key, expected) in [
            ("", "12ae32cb"),
            ("/workspace/skills/rust/SKILL.md", "ae1c6828"),
            (r"C:\Users\developer\skills\SKILL.md", "b4826630"),
            ("/workspace/\"quoted\"/SKILL.md", "e692dcfb"),
            ("/workspace/é/🦀/SKILL.md", "a8de7e98"),
            ("/workspace/skills/skill-534/SKILL.md", "007baaca"),
            ("tab\tline\nreturn\rback\u{8}form\u{c}null\0end", "85403ca1"),
        ] {
            assert_eq!(hash_origin_key(key), expected, "{key:?}");
        }
    }

    #[test]
    fn frontmatter_errors_classify_shapes_without_disclosing_contents() {
        const CANARY: &str = "PRIVATE_FRONTMATTER_CANARY";
        for (yaml, expected) in [
            (format!("description: [{CANARY}"), "yaml"),
            (format!("{CANARY}: [{CANARY}]"), "value"),
            (format!("- {CANARY}"), "mapping"),
            (format!("1: {CANARY}"), "key"),
        ] {
            let error = parse_frontmatter(&format!("---\n{yaml}\n---\n")).unwrap_err();
            assert!(!error.to_string().contains(CANARY));
            match (expected, &error) {
                ("yaml", FrontmatterError::Yaml(_)) => {
                    use std::error::Error as _;
                    assert!(error.source().unwrap().is::<serde_yaml_ng::Error>());
                }
                ("value", FrontmatterError::NonStringValue)
                | ("mapping", FrontmatterError::ExpectedMapping)
                | ("key", FrontmatterError::NonStringKey) => {}
                _ => panic!("wrong frontmatter classification: {error}"),
            }
        }
    }

    #[test]
    fn skill_boundaries_preserve_paths_and_concrete_read_and_yaml_sources() {
        use std::error::Error as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("SKILL.md");
        let error = load_skill(&path, false, &PredicateSet::default()).unwrap_err();
        assert!(matches!(&error, SkillError::Read { path: failed, .. } if failed == &path));
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::NotFound
        );

        fs::write(&path, "---\ndescription: [PRIVATE_YAML_CANARY\n---\n").unwrap();
        let loaded = load_skill(&path, false, &PredicateSet::default()).unwrap_err();
        let standalone = standalone_skill_meta(&path).unwrap_err();
        for error in [loaded, standalone] {
            assert!(!error.to_string().contains("PRIVATE_YAML_CANARY"));
            assert!(
                matches!(&error, SkillError::Frontmatter { path: failed, .. } if failed == &path)
            );
            let frontmatter = error
                .source()
                .unwrap()
                .downcast_ref::<FrontmatterError>()
                .unwrap();
            assert!(frontmatter.source().unwrap().is::<serde_yaml_ng::Error>());
        }
    }

    #[test]
    fn skill_validation_retains_typed_predicates_without_echoing_values() {
        use std::error::Error as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("SKILL.md");
        for field in [
            "depends-on: serde>=PRIVATE_PREDICATE_CANARY",
            "predicates: PRIVATE_PREDICATE_CANARY",
        ] {
            fs::write(
                &path,
                format!("---\nname: valid\ndescription: d\n{field}\n---\n"),
            )
            .unwrap();
            let loaded = load_skill(&path, false, &PredicateSet::default()).unwrap_err();
            let standalone = standalone_skill_meta(&path).unwrap_err();
            for error in [loaded, standalone] {
                assert!(!error.to_string().contains("PRIVATE_PREDICATE_CANARY"));
                assert!(matches!(error, SkillError::Predicates { .. }));
                assert!(error.source().unwrap().is::<PredicateSyntaxError>());
            }
        }
        fs::write(
            &path,
            "---\nname: PRIVATE_NAME_CANARY/escape\ndescription: d\n---\n",
        )
        .unwrap();
        let error = load_skill(&path, false, &PredicateSet::default()).unwrap_err();
        assert!(matches!(error, SkillError::UnsafeName));
        assert!(!error.to_string().contains("PRIVATE_NAME_CANARY"));
    }

    #[test]
    #[cfg(unix)]
    fn discovery_keeps_valid_skills_and_reports_broken_links() {
        use std::error::Error as _;
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("good")).unwrap();
        fs::write(
            tmp.path().join("good/SKILL.md"),
            "---\nname: good\ndescription: d\ndepends-on: serde\n---\n",
        )
        .unwrap();
        let broken = tmp.path().join("broken");
        std::os::unix::fs::symlink("missing", &broken).unwrap();
        let found = discover_skills(tmp.path(), false, &PredicateSet::default());
        assert_eq!(found.iter().filter(|skill| skill.is_ok()).count(), 1);
        let error = found.into_iter().find_map(Result::err).unwrap();
        assert!(matches!(&error, SkillError::Discover { path, .. } if path == &broken));
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(
            discover_skills(&tmp.path().join("absent"), false, &PredicateSet::default()).is_empty()
        );
    }

    #[test]
    fn pruning_paths_without_parents_does_not_panic() {
        let mut paths = vec![
            PathBuf::new(),
            "outer/SKILL.md".into(),
            "outer/child/SKILL.md".into(),
        ];
        prune_nested_skills(&mut paths);
        assert_eq!(paths, vec![PathBuf::new(), PathBuf::from("outer/SKILL.md")]);
    }

    #[tokio::test]
    async fn skill_collection_retains_siblings_and_signals_incomplete_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::try_from_dir(tmp.path()).unwrap();
        let source = tmp.path().join("skills");
        fs::create_dir_all(source.join("good")).unwrap();
        fs::create_dir(source.join("bad")).unwrap();
        fs::write(
            source.join("good/SKILL.md"),
            "---\nname: good\ndescription: d\ndepends-on: serde\n---\n",
        )
        .unwrap();
        fs::write(
            source.join("bad/SKILL.md"),
            "---\nname: [PRIVATE_BAD_SKILL_CANARY\n---\n",
        )
        .unwrap();
        let plugin = plugin_with_skill_groups(vec![group(PluginSource::Path(source))]);
        let deps = ws(&[("serde", "1.0.0")]);
        let collection = collect_skills(&sym, &[plugin], &mut ctx(&deps), UpdateLevel::None).await;
        assert!(collection.has_failures);
        assert_eq!(collection.skills.len(), 1);
        assert_eq!(collection.skills[0].skill.name(), "good");

        let missing = tmp.path().join("missing");
        let plugin = plugin_with_skill_groups(vec![group(PluginSource::Path(missing.clone()))]);
        let collection = collect_skills(&sym, &[plugin], &mut ctx(&deps), UpdateLevel::None).await;
        assert!(!collection.has_failures);
        assert!(collection.skills.is_empty());
        fs::write(&missing, b"not a directory").unwrap();
        let plugin = plugin_with_skill_groups(vec![group(PluginSource::Path(missing))]);
        let collection = collect_skills(&sym, &[plugin], &mut ctx(&deps), UpdateLevel::None).await;
        assert!(collection.has_failures);
        assert!(collection.skills.is_empty());
    }

    #[tokio::test]
    async fn invalid_git_sources_have_safe_reports_and_typed_failures_without_fetching() {
        use std::error::Error as _;
        const CANARY: &str = "PRIVATE_GIT_TOKEN_CANARY";
        symposium_testlib::install_tracing_baseline();
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::try_from_dir(tmp.path()).unwrap();
        let log = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(log.reopen().unwrap())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let url = format!("https://user:{CANARY}@github.com/owner/repo");
        let source = PluginSource::Git(url.clone());
        assert_eq!(source_display(&source), "git:invalid source");
        assert_eq!(
            source_display(&PluginSource::Git("https://github.com/owner/repo".into())),
            "git:https://github.com/owner/repo"
        );
        let error = fetch_git_skill_source(&sym, &url, UpdateLevel::None)
            .await
            .unwrap_err();
        assert!(!error.to_string().contains(CANARY));
        assert!(
            error
                .source()
                .unwrap()
                .is::<symposium_install::git::GitError>()
        );
        for skipped in [true, false] {
            let mut group = group(source.clone());
            group.source_label = Some(url.clone());
            if skipped {
                group.predicates = pred_set("serde");
            }
            let plugin = plugin_with_skill_groups(vec![group]);
            let collection =
                collect_skills(&sym, &[plugin], &mut ctx(&[]), UpdateLevel::None).await;
            assert_eq!(collection.has_failures, !skipped);
            assert!(collection.skills.is_empty());
        }
        drop(guard);
        let logs = fs::read_to_string(log.path()).unwrap();
        assert!(logs.contains("git:invalid source"));
        assert!(logs.contains("failed to load skill group"));
        assert!(!logs.contains(CANARY));
        assert!(!sym.cache_dir().join("plugins").exists());
    }

    /// Discovery follows directory links, so a skills tree that links back
    /// into itself must not be walked forever — and must not report the same
    /// `SKILL.md` once per turn around the loop.
    #[test]
    #[cfg(unix)]
    fn discovery_terminates_on_a_symlink_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        fs::create_dir_all(skills.join("my-skill")).unwrap();
        fs::write(skills.join("my-skill/SKILL.md"), "# skill").unwrap();
        std::os::unix::fs::symlink("..", skills.join("my-skill/back")).unwrap();

        let mut found = Vec::new();
        assert!(find_skill_files_recursive(&skills, &mut found).is_empty());

        assert_eq!(found, vec![skills.join("my-skill/SKILL.md")]);
    }

    /// Reaching one SKILL.md through a symlinked prefix must yield one origin.
    /// This is the macOS `/var` -> `/private/var` case: group discovery walks a
    /// canonicalized dir while the standalone walk uses the configured path
    /// verbatim, so without canonicalizing here the file installs twice.
    #[test]
    #[cfg(unix)]
    fn skill_origin_hash_is_stable_across_symlinked_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(real.join("my-skill")).unwrap();
        let skill_md = real.join("my-skill/SKILL.md");
        fs::write(&skill_md, "# skill").unwrap();

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let via_link = link.join("my-skill/SKILL.md");

        assert_ne!(skill_md, via_link, "paths should differ textually");
        assert_eq!(
            skill_origin_hash(&skill_md),
            skill_origin_hash(&via_link),
            "same file reached through a symlink must hash to one origin"
        );
    }

    fn ctx(deps: &[crate::pm::PackageId]) -> PredicateContext<'_> {
        PredicateContext::new(deps)
    }

    fn ws(pairs: &[(&str, &str)]) -> Vec<crate::pm::PackageId> {
        pairs
            .iter()
            .map(|(n, ver)| crate::pm::PackageId::new(crate::pm::CARGO_PM, *n, *ver))
            .collect()
    }

    // --- Frontmatter parsing ---

    #[test]
    fn parse_frontmatter_basic() {
        let content = indoc! {"
            ---
            name: my-skill
            description: A test skill
            depends-on: serde
            ---

            # Body content

            Some instructions here.
        "};
        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.fields.get("name").unwrap(), "my-skill");
        assert_eq!(fm.fields.get("description").unwrap(), "A test skill");
        assert_eq!(fm.depends_on.as_deref(), Some("serde"));
    }

    #[test]
    fn parse_frontmatter_comma_separated_depends_on() {
        let content = indoc! {"
            ---
            name: multi
            depends-on: serde, serde_json>=1.0, toml
            ---

            Body.
        "};
        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(
            fm.depends_on.as_deref(),
            Some("serde, serde_json>=1.0, toml")
        );
    }

    #[test]
    fn parse_frontmatter_rejects_renamed_crates_field() {
        let content = indoc! {"
            ---
            name: old-spelling
            description: Old spelling
            crates: serde
            ---

            Body.
        "};
        let err = parse_frontmatter(content).unwrap_err();
        assert!(
            err.to_string().contains("use `depends-on` instead"),
            "expected migration hint, got: {err}"
        );
    }

    #[test]
    fn parse_frontmatter_rejects_invalid_yaml_description() {
        let content = indoc! {"
            ---
            name: rust-best-practice
            description: [Critical] Best practice for Rust coding.
            ---

            Body.
        "};

        let err = parse_frontmatter(content).unwrap_err();
        assert!(
            err.to_string().contains("frontmatter is not valid YAML"),
            "expected YAML parse error, got: {err}"
        );
    }

    #[test]
    fn parse_frontmatter_rejects_non_string_description() {
        let content = indoc! {"
            ---
            name: rust-best-practice
            description: [Critical]
            ---

            Body.
        "};

        let err = parse_frontmatter(content).unwrap_err();
        assert!(matches!(err, FrontmatterError::NonStringValue));
    }

    #[test]
    fn parse_frontmatter_no_fence() {
        let content = "# Just markdown\n\nNo frontmatter here.\n";
        assert!(parse_frontmatter(content).is_err());
    }

    #[test]
    fn parse_frontmatter_no_closing_fence() {
        let content = "---\nname: broken\n";
        assert!(parse_frontmatter(content).is_err());
    }

    // --- Skill loading ---

    #[test]
    fn load_skill_from_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_md = tmp.path().join("SKILL.md");
        fs::write(
            &skill_md,
            indoc! {"
                ---
                name: test-skill
                description: Test
                depends-on: serde
                ---

                Use serde like this.
            "},
        )
        .unwrap();

        let skill = load_skill(&skill_md, false, &PredicateSet::default()).unwrap();

        assert_eq!(skill.frontmatter.get("name").unwrap(), "test-skill");
        assert!(skill.predicates.references_dep("serde"));
    }

    #[test]
    fn load_skill_comma_separated_crates() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_md = tmp.path().join("SKILL.md");
        fs::write(
            &skill_md,
            indoc! {"
                ---
                name: multi-crate
                description: Multi-crate skill
                depends-on: serde, tokio>=1.0
                ---

                Body.
            "},
        )
        .unwrap();

        let skill = load_skill(&skill_md, false, &PredicateSet::default()).unwrap();
        assert!(skill.predicates.references_dep("serde"));
        assert!(skill.predicates.references_dep("tokio"));
    }

    #[test]
    fn load_skill_inherits_toml_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_md = tmp.path().join("SKILL.md");
        fs::write(
            &skill_md,
            indoc! {"
                ---
                name: inherited
                description: Inherits crates from TOML
                ---

                Body.
            "},
        )
        .unwrap();

        let skill = load_skill(&skill_md, false, &pred_set("tokio")).unwrap();

        // Skill has no crates in frontmatter, so it's empty at skill level.
        // The plugin default provides the crates scope.
        assert!(skill.predicates.is_empty());
    }

    #[test]
    fn load_skill_frontmatter_specializes_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_md = tmp.path().join("SKILL.md");
        fs::write(
            &skill_md,
            indoc! {"
                ---
                name: override
                description: Override skill
                depends-on: serde
                ---

                Body.
            "},
        )
        .unwrap();

        let skill = load_skill(&skill_md, false, &pred_set("tokio")).unwrap();

        // Skill-level crates specializes (ANDs with) plugin defaults
        assert!(skill.predicates.references_dep("serde"));
        assert!(!skill.predicates.references_dep("tokio"));
    }

    #[test]
    fn load_skill_missing_crates_warns_but_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_md = tmp.path().join("SKILL.md");
        fs::write(
            &skill_md,
            indoc! {"
                ---
                name: no-depends-on
                description: Missing depends-on
                ---

                Body.
            "},
        )
        .unwrap();

        // No longer an error — just a warning. The skill loads but won't match anything.
        let skill = load_skill(&skill_md, false, &PredicateSet::default()).unwrap();
        assert!(skill.predicates.is_empty());
    }

    #[test]
    fn load_skill_ok_when_plugin_provides_crates() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_md = tmp.path().join("SKILL.md");
        fs::write(
            &skill_md,
            indoc! {"
                ---
                name: no-own-crates
                description: PluginManifest provides crates
                ---

                Body.
            "},
        )
        .unwrap();

        // Plugin defaults provide crates, so the skill doesn't need its own
        let skill = load_skill(&skill_md, false, &pred_set("serde")).unwrap();
        assert!(skill.predicates.is_empty()); // skill-level is empty
        assert_eq!(skill.frontmatter.get("name").unwrap(), "no-own-crates");
    }

    #[test]
    fn load_skill_rejects_names_that_are_not_plain_directory_names() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_md = tmp.path().join("SKILL.md");
        for name in [
            "",
            ".",
            "..",
            "...",
            ".. ",
            "../escaped",
            "nested/skill",
            "/tmp/escaped",
            "..\\escaped",
            "C:escaped",
        ] {
            fs::write(
                &skill_md,
                format!("---\nname: {name:?}\ndescription: d\ndepends-on: serde\n---\n"),
            )
            .unwrap();
            let err = load_skill(&skill_md, false, &PredicateSet::default()).unwrap_err();
            assert!(
                err.to_string().contains("plain directory name"),
                "{name:?}: {err}"
            );
        }
    }

    // --- Bare SKILL.md loading (see plugins::standalone_skill_manifest
    // for how a bare skill becomes a plugin) ---

    #[test]
    fn bare_skill_is_self_contained() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                name: my-standalone
                description: A standalone skill
                depends-on: serde
                ---

                Standalone body.
            "},
        )
        .unwrap();

        let skill =
            load_skill(&skill_dir.join("SKILL.md"), false, &PredicateSet::default()).unwrap();
        assert_eq!(skill.name(), "my-standalone");
        assert!(skill.predicates.references_dep("serde"));
    }

    #[test]
    fn workspace_group_skill_needs_no_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("release-notes");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "Plain maintainer notes.\n").unwrap();

        let skill =
            load_skill(&skill_dir.join("SKILL.md"), true, &PredicateSet::default()).unwrap();
        assert_eq!(skill.name(), "release-notes");
        assert!(!skill.frontmatter.contains_key("description"));

        // Registry groups keep the agentskills.io contract.
        let err =
            load_skill(&skill_dir.join("SKILL.md"), false, &PredicateSet::default()).unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "{err}");
    }

    #[test]
    fn workspace_group_skill_frontmatter_fields_stay_optional() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("style");
        fs::create_dir_all(&skill_dir).unwrap();
        // Frontmatter present, but neither name nor description: the name
        // falls back to the directory, other fields still parse.
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                depends-on: serde
                ---

                Body.
            "},
        )
        .unwrap();

        let skill =
            load_skill(&skill_dir.join("SKILL.md"), true, &PredicateSet::default()).unwrap();
        assert_eq!(skill.name(), "style");
        assert!(skill.predicates.references_dep("serde"));

        // An explicit name still wins over the directory fallback.
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                name: explicit
                ---

                Body.
            "},
        )
        .unwrap();
        let skill =
            load_skill(&skill_dir.join("SKILL.md"), true, &PredicateSet::default()).unwrap();
        assert_eq!(skill.name(), "explicit");
    }

    #[test]
    fn validate_standalone_skill_bad_depends_on() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("bad-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                name: bad
                description: Bad depends-on skill
                depends-on: \">=not_valid!!\"
                ---

                Body.
            "},
        )
        .unwrap();

        let err =
            load_skill(&skill_dir.join("SKILL.md"), false, &PredicateSet::default()).unwrap_err();
        assert!(
            err.to_string().contains("depends-on predicate"),
            "expected parse error, got: {err}"
        );
    }

    // --- Multi-level crate filtering tests ---

    #[tokio::test]
    async fn test_plugin_level_filtering_blocks_skills() {
        use crate::plugins::{Plugin, PluginManifest, PluginRegistry, PluginSource, SkillGroup};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let sym = crate::config::Symposium::from_dir(tmp.path());

        // Create a plugin that only applies to "other-crate"
        let plugin = PluginManifest {
            name: "other-crate-plugin".to_string(),
            predicates: pred_set("other-crate"),
            hooks: vec![],
            skills: vec![SkillGroup {
                predicates: pred_set("serde"), // Group targets serde
                source: PluginSource::Path(PathBuf::from("skills")),
                source_label: None,
                workspace_member: false,
            }],
            mcp_servers: vec![],
            installations: Vec::new(),
            subcommands: BTreeMap::new(),
            custom_predicates: vec![],
            chained: vec![],
            requires_use: false,
        };

        let registry = PluginRegistry {
            plugins: vec![Plugin {
                canonical: PackageId::new("test", &plugin.name, ANY_VERSION),
                manifest: plugin,
                workspace_member: false,
            }],
            warnings: vec![],
            unreadable_registries: vec![],
            custom_predicates: crate::plugins::CustomPredicateRegistry::default(),
        };

        // Query for serde - should find no skills because plugin doesn't apply
        let workspace_crates = vec![crate::pm::WorkspaceCrate::new(
            "serde".to_string(),
            semver::Version::new(1, 0, 0),
            None,
        )];
        let skills = skills_applicable_to(
            &sym,
            &registry,
            &crate::pm::WorkspaceDeps::fixture(std::path::PathBuf::new(), workspace_crates),
            None,
            std::collections::HashMap::new(),
            UpdateLevel::None,
        )
        .await;

        assert!(
            skills.is_empty(),
            "Plugin should be filtered out at plugin level"
        );
    }

    #[tokio::test]
    async fn test_group_level_filtering_blocks_skills() {
        use crate::plugins::{Plugin, PluginManifest, PluginRegistry, PluginSource, SkillGroup};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let sym = crate::config::Symposium::from_dir(tmp.path());

        // Create a plugin with wildcard that has a group targeting different crate
        let plugin = PluginManifest {
            name: "wildcard-plugin".to_string(),
            predicates: pred_set("*"), // Plugin applies to all
            hooks: vec![],
            skills: vec![SkillGroup {
                predicates: pred_set("other-crate"), // But group targets other-crate
                source: PluginSource::Path(PathBuf::from("skills")),
                source_label: None,
                workspace_member: false,
            }],
            mcp_servers: vec![],
            installations: Vec::new(),
            subcommands: BTreeMap::new(),
            custom_predicates: vec![],
            chained: vec![],
            requires_use: false,
        };

        let registry = PluginRegistry {
            plugins: vec![Plugin {
                canonical: PackageId::new("test", &plugin.name, ANY_VERSION),
                manifest: plugin,
                workspace_member: false,
            }],
            warnings: vec![],
            unreadable_registries: vec![],
            custom_predicates: crate::plugins::CustomPredicateRegistry::default(),
        };

        // Query for serde - should find no skills because group doesn't match
        let workspace_crates = vec![crate::pm::WorkspaceCrate::new(
            "serde".to_string(),
            semver::Version::new(1, 0, 0),
            None,
        )];
        let skills = skills_applicable_to(
            &sym,
            &registry,
            &crate::pm::WorkspaceDeps::fixture(std::path::PathBuf::new(), workspace_crates),
            None,
            std::collections::HashMap::new(),
            UpdateLevel::None,
        )
        .await;

        assert!(
            skills.is_empty(),
            "Skills should be filtered out at group level"
        );
    }

    #[tokio::test]
    async fn test_all_levels_match_allows_skills() {
        use crate::plugins::{Plugin, PluginManifest, PluginRegistry, PluginSource, SkillGroup};
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let sym = crate::config::Symposium::from_dir(tmp.path());

        // Create skill directory and file
        let skill_dir = tmp.path().join("serde-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                name: serde-basics
                description: Basic serde usage
                depends-on: serde
                ---

                Use derive macros.
            "},
        )
        .unwrap();

        // Create a plugin where all levels match serde
        let plugin = PluginManifest {
            name: "serde-plugin".to_string(),
            predicates: pred_set("serde"), // Plugin targets serde
            hooks: vec![],
            skills: vec![SkillGroup {
                predicates: pred_set("serde"), // Group also targets serde
                source: PluginSource::Path(skill_dir.to_path_buf()),
                source_label: None,
                workspace_member: false,
            }],
            mcp_servers: vec![],
            installations: Vec::new(),
            subcommands: BTreeMap::new(),
            custom_predicates: vec![],
            chained: vec![],
            requires_use: false,
        };

        let registry = PluginRegistry {
            plugins: vec![Plugin {
                canonical: PackageId::new("test", &plugin.name, ANY_VERSION),
                manifest: plugin,
                workspace_member: false,
            }],
            warnings: vec![],
            unreadable_registries: vec![],
            custom_predicates: crate::plugins::CustomPredicateRegistry::default(),
        };

        let workspace_crates = vec![crate::pm::WorkspaceCrate::new(
            "serde".to_string(),
            semver::Version::new(1, 0, 0),
            None,
        )];
        let skills = skills_applicable_to(
            &sym,
            &registry,
            &crate::pm::WorkspaceDeps::fixture(std::path::PathBuf::new(), workspace_crates),
            None,
            std::collections::HashMap::new(),
            UpdateLevel::None,
        )
        .await;

        assert_eq!(
            skills.len(),
            1,
            "Should find one skill when all levels match"
        );
        assert_eq!(skills[0].skill.name(), "serde-basics");
    }

    #[tokio::test]
    async fn predicate_failure_filters_skill() {
        use crate::plugins::{Plugin, PluginManifest, PluginRegistry, PluginSource, SkillGroup};
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let sym = crate::config::Symposium::from_dir(tmp.path());

        let skill_dir = tmp.path().join("serde-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                name: serde-basics
                description: Basic serde usage
                depends-on: serde
                ---

                Use derive macros.
            "},
        )
        .unwrap();

        // Plugin matches by crates, but its shell predicate fails.
        let plugin = PluginManifest {
            name: "p".into(),
            predicates: PredicateSet {
                predicates: vec![
                    Predicate::DependsOn("serde".into(), None),
                    Predicate::Shell("false".into()),
                ],
            },
            hooks: vec![],
            skills: vec![SkillGroup {
                predicates: pred_set("serde"),
                source: PluginSource::Path(skill_dir.to_path_buf()),
                source_label: None,
                workspace_member: false,
            }],
            mcp_servers: vec![],
            installations: Vec::new(),
            subcommands: Default::default(),
            custom_predicates: vec![],
            chained: vec![],
            requires_use: false,
        };

        let registry = PluginRegistry {
            plugins: vec![Plugin {
                canonical: PackageId::new("test", &plugin.name, ANY_VERSION),
                manifest: plugin,
                workspace_member: false,
            }],
            warnings: vec![],
            unreadable_registries: vec![],
            custom_predicates: crate::plugins::CustomPredicateRegistry::default(),
        };

        let workspace = vec![crate::pm::WorkspaceCrate::new(
            "serde".into(),
            semver::Version::new(1, 0, 0),
            None,
        )];
        let skills = skills_applicable_to(
            &sym,
            &registry,
            &crate::pm::WorkspaceDeps::fixture(std::path::PathBuf::new(), workspace),
            None,
            std::collections::HashMap::new(),
            UpdateLevel::None,
        )
        .await;
        assert!(
            skills.is_empty(),
            "plugin predicate=false should filter out skills"
        );
    }

    #[tokio::test]
    async fn predicate_pass_allows_skill() {
        use crate::plugins::{Plugin, PluginManifest, PluginRegistry, PluginSource, SkillGroup};
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let sym = crate::config::Symposium::from_dir(tmp.path());

        let skill_dir = tmp.path().join("serde-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                name: serde-basics
                description: Basic serde usage
                depends-on: serde
                ---

                Body.
            "},
        )
        .unwrap();

        let plugin = PluginManifest {
            name: "p".into(),
            predicates: PredicateSet {
                predicates: vec![
                    Predicate::DependsOn("serde".into(), None),
                    Predicate::Shell("true".into()),
                ],
            },
            hooks: vec![],
            skills: vec![SkillGroup {
                predicates: PredicateSet {
                    predicates: vec![
                        Predicate::DependsOn("serde".into(), None),
                        Predicate::Shell("true".into()),
                    ],
                },
                source: PluginSource::Path(skill_dir.to_path_buf()),
                source_label: None,
                workspace_member: false,
            }],
            mcp_servers: vec![],
            installations: Vec::new(),
            subcommands: Default::default(),
            custom_predicates: vec![],
            chained: vec![],
            requires_use: false,
        };

        let registry = PluginRegistry {
            plugins: vec![Plugin {
                canonical: PackageId::new("test", &plugin.name, ANY_VERSION),
                manifest: plugin,
                workspace_member: false,
            }],
            warnings: vec![],
            unreadable_registries: vec![],
            custom_predicates: crate::plugins::CustomPredicateRegistry::default(),
        };

        let workspace = vec![crate::pm::WorkspaceCrate::new(
            "serde".into(),
            semver::Version::new(1, 0, 0),
            None,
        )];
        let skills = skills_applicable_to(
            &sym,
            &registry,
            &crate::pm::WorkspaceDeps::fixture(std::path::PathBuf::new(), workspace),
            None,
            std::collections::HashMap::new(),
            UpdateLevel::None,
        )
        .await;
        assert_eq!(skills.len(), 1);
    }

    #[test]
    fn skill_frontmatter_parses_predicates() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_md = tmp.path().join("SKILL.md");
        std::fs::write(
            &skill_md,
            indoc! {"
                ---
                name: with-env
                description: A skill with runtime predicates
                depends-on: serde
                predicates: shell(command -v rg), path_exists(Cargo.toml)
                ---

                Body.
            "},
        )
        .unwrap();

        let skill = load_skill(&skill_md, false, &PredicateSet::default()).unwrap();
        // `depends-on: serde` lowers to a leading `depends-on(serde)`, then the two
        // function-call predicates.
        assert_eq!(
            skill.predicates.predicates,
            vec![
                Predicate::DependsOn("serde".into(), None),
                Predicate::Shell("command -v rg".into()),
                Predicate::PathExists("Cargo.toml".into()),
            ]
        );
    }

    #[test]
    fn validate_standalone_skill_missing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("bad-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                depends-on: serde
                ---

                Body.
            "},
        )
        .unwrap();

        let err =
            load_skill(&skill_dir.join("SKILL.md"), false, &PredicateSet::default()).unwrap_err();
        assert!(
            err.to_string().contains("missing required `name` field"),
            "expected missing name error, got: {err}"
        );
    }

    /// A bare `SKILL.md` directory, modelled as its synthesized plugin would be
    /// (name from the directory, one `source.path = "."` group gated on the
    /// skill's own `depends-on`), contributes its skill through the ordinary
    /// plugin path.
    #[tokio::test]
    async fn bare_skill_plugin_contributes_its_skill() {
        use crate::plugins::{Plugin, PluginManifest, PluginRegistry, PluginSource, SkillGroup};

        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                name: standalone-serde
                description: Standalone serde skill
                depends-on: serde
                ---

                Body.
            "},
        )
        .unwrap();

        let plugin = PluginManifest {
            name: "my-skill".to_string(),
            predicates: pred_set("serde"),
            installations: vec![],
            hooks: vec![],
            skills: vec![SkillGroup {
                predicates: PredicateSet::default(),
                // A PM returns absolute skill dirs; the bare-skill group's "."
                // resolves to the skill's own directory.
                source: PluginSource::Path(skill_dir.clone()),
                source_label: None,
                workspace_member: false,
            }],
            mcp_servers: vec![],
            subcommands: Default::default(),
            custom_predicates: vec![],
            chained: vec![],
            requires_use: false,
        };
        let registry = PluginRegistry {
            plugins: vec![Plugin {
                canonical: crate::pm::PackageId::any_version("recs", "my-skill"),
                manifest: plugin,
                workspace_member: false,
            }],
            warnings: vec![],
            unreadable_registries: vec![],
            custom_predicates: crate::plugins::CustomPredicateRegistry::default(),
        };

        let sym = crate::config::Symposium::from_dir(tmp.path());
        let workspace = vec![crate::pm::WorkspaceCrate::new(
            "serde".to_string(),
            semver::Version::new(1, 0, 0),
            None,
        )];
        let results = skills_applicable_to(
            &sym,
            &registry,
            &crate::pm::WorkspaceDeps::fixture(std::path::PathBuf::new(), workspace),
            None,
            std::collections::HashMap::new(),
            UpdateLevel::None,
        )
        .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].skill.name(), "standalone-serde");
        assert!(results[0].skill.predicates.references_dep("serde"));
    }

    // --- Discovery ---

    #[test]
    fn discover_skills_in_plugin_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path();

        // Create skills/my-skill/SKILL.md
        let skill_dir = plugin_dir.join("skills").join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                name: my-skill
                description: A discovered skill
                depends-on: serde
                ---

                Discovered body.
            "},
        )
        .unwrap();

        let skills = discover_skills(&plugin_dir.join("skills"), false, &PredicateSet::default());

        assert_eq!(skills.len(), 1);
        let skill = skills.into_iter().next().unwrap().unwrap();
        assert_eq!(skill.frontmatter.get("name").unwrap(), "my-skill");
    }

    #[test]
    fn discover_skills_in_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a skill nested two levels deep: group/sub/SKILL.md
        let skill_dir = root.join("group").join("sub");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            indoc! {"
                ---
                name: nested-skill
                description: Nested skill
                depends-on: tokio
                ---

                Nested body.
            "},
        )
        .unwrap();

        let skills = discover_skills(root, false, &PredicateSet::default());

        assert_eq!(skills.len(), 1);
        let skill = skills.into_iter().next().unwrap().unwrap();
        assert_eq!(skill.frontmatter.get("name").unwrap(), "nested-skill");
    }

    #[test]
    fn discover_skills_prunes_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a shallow skill: alpha/SKILL.md
        let shallow = root.join("alpha");
        fs::create_dir_all(&shallow).unwrap();
        fs::write(
            shallow.join("SKILL.md"),
            indoc! {"
                ---
                name: shallow
                description: Shallow skill
                depends-on: serde
                ---

                Shallow.
            "},
        )
        .unwrap();

        // Create a nested skill inside alpha: alpha/beta/SKILL.md
        let nested = root.join("alpha").join("beta");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("SKILL.md"),
            indoc! {"
                ---
                name: nested
                description: Nested skill
                depends-on: serde
                ---

                Nested.
            "},
        )
        .unwrap();

        // Also create a sibling skill: gamma/SKILL.md (should be kept)
        let sibling = root.join("gamma");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(
            sibling.join("SKILL.md"),
            indoc! {"
                ---
                name: sibling
                description: Sibling skill
                depends-on: tokio
                ---

                Sibling.
            "},
        )
        .unwrap();

        let skills = discover_skills(root, false, &PredicateSet::default());

        // Should find shallow + sibling, but NOT nested (pruned by shallow)
        let names: Vec<String> = skills
            .into_iter()
            .map(|r| r.unwrap().frontmatter.get("name").unwrap().clone())
            .collect();
        assert!(names.contains(&"shallow".to_string()));
        assert!(names.contains(&"sibling".to_string()));
        assert!(!names.contains(&"nested".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn discover_skills_no_skills_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = discover_skills(tmp.path(), false, &PredicateSet::default());
        assert!(skills.is_empty());
    }

    // --- AND composition across levels (plugin ∧ group ∧ skill) ---

    /// Each level's `depends-on` lowers to one predicate set; the skill applies when
    /// every level's set holds (AND across levels).
    fn applies(levels: &[&str], deps: &[crate::pm::PackageId]) -> bool {
        levels
            .iter()
            .all(|spec| pred_set(spec).evaluate(&mut ctx(deps)))
    }

    #[test]
    fn and_across_levels_all_satisfied() {
        let w = ws(&[("serde", "1.0.0"), ("tokio", "1.0.0")]);
        assert!(applies(&["serde", "tokio"], &w));
    }

    #[test]
    fn and_across_levels_one_missing() {
        let w = ws(&[("serde", "1.0.0")]);
        assert!(!applies(&["serde", "tokio"], &w));
    }

    #[test]
    fn and_across_levels_empty_is_vacuously_true() {
        let w = ws(&[("serde", "1.0.0")]);
        assert!(applies(&[], &w));
    }

    #[test]
    fn wildcard_level_matches_any() {
        let w = ws(&[("serde", "1.0.0")]);
        assert!(applies(&["*", "serde"], &w));
    }
}
