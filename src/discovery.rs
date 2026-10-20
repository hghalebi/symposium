//! Dependency discovery: which plugins the workspace's dependencies bring
//! within reach, and which of them the user has consented to.
//!
//! Discovery is the read side of the enablement axis. It runs in two phases:
//!
//! 1. list the workspace's dependencies ([`pm::workspace_dep_ids`]);
//! 2. ask each *untrusted* instance — the cargo transport — for the plugins
//!    its dependencies embed ([`PackageManager::active_plugins`]).
//!
//! Each such offer is then classified against the `[plugins]` config:
//! already enabled, auto-enabled, declined, or a candidate still awaiting
//! consent. Discovery itself neither fetches nor writes.
//!
//! On top of that read side sits the consent write side:
//! [`prompt_for_consent`] asks about each candidate and [`apply_consent`]
//! records the answers. The prompt is inert unless its [`Output`] is
//! interactive, so hook dispatch — and anything else an agent triggers —
//! can never block on stdin; those contexts get [`pending_candidates`] as a
//! `SessionStart` hint instead.
//!
//! Enablement matters because a dependency is deliberately *not* a trust
//! root: depending on a crate means compiling its code, not letting its
//! author inject agent context. Registry instances are trust roots — a
//! registry exists to curate plugins — but their plugins are loaded and
//! gated directly ([`plugins::load_registry`], evaluated by
//! [`Plugin::applies`]), so they never reach discovery. What discovery
//! classifies is exactly the untrusted offers: the dependency-embedded
//! plugins an ecosystem transport surfaces, which run only with consent.
//!
//! [`plugins::load_registry`]: crate::plugins::load_registry
//! [`Plugin::applies`]: crate::plugins::Plugin::applies
//!
//! [`pm::workspace_dep_ids`]: crate::pm::workspace_dep_ids
//! [`PackageManager::active_plugins`]: crate::pm::PackageManager::active_plugins

use std::path::Path;

use crate::pm::WorkspaceDeps;
use std::sync::Arc;

use crate::config::{ConfigError, RegistryError, Symposium};
use crate::crate_sources::normalize_crate_name;
use crate::output::Output;
use crate::pm::{CARGO_PM, PackageId};
use crate::report::ReportEvent;

/// A failure to discover consent candidates, ask for a decision, or persist it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConsentError {
    /// Registry validation failed before any consent decision was recorded.
    #[error("invalid registry configuration for consent")]
    Registry {
        /// The invalid source configuration.
        #[source]
        source: RegistryError,
    },
    /// Decisions could not be saved; the in-memory decisions were restored.
    #[error("failed to write user config")]
    SaveConfig {
        /// The configuration serialization or filesystem failure.
        #[source]
        source: ConfigError,
    },
    /// Reading the user's consent decision failed.
    #[error("consent prompt failed")]
    Prompt {
        /// The terminal interaction failure.
        #[source]
        source: dialoguer::Error,
    },
}

impl ConsentError {
    fn into_legacy(self) -> anyhow::Error {
        match self {
            Self::Registry { source } => anyhow::Error::new(source),
            Self::SaveConfig { source } => {
                anyhow::Error::new(source).context("failed to write user config")
            }
            Self::Prompt { source } => anyhow::Error::new(source).context("consent prompt failed"),
        }
    }
}

/// Why a discovered offer is (or is not) enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enablement {
    /// Enabled by a `[plugins] use` entry naming it.
    Used,
    /// Enabled ahead of time by `[plugins] auto-enable`.
    AutoEnabled,
    /// Declined: `[plugins] disable` names it.
    Declined,
    /// Nobody has decided yet — this is what a consent prompt would ask about.
    Candidate,
}

impl Enablement {
    /// Does this decision let the plugin run?
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Used | Self::AutoEnabled)
    }
}

/// One plugin offer whose recommended dependency the workspace has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPlugin {
    /// The offering package-manager instance: an ecosystem transport
    /// (`cargo`) for a dependency-embedded plugin, or a registry's name.
    pub registry: String,
    /// The offered package.
    pub id: PackageId,
    /// The dependency this offer is a plugin for.
    pub recommends: String,
    /// A short human summary of what the plugin contributes, for the consent
    /// prompt.
    pub description: Option<String>,
    /// How the `[plugins]` config decided this offer.
    pub enablement: Enablement,
}

impl DiscoveredPlugin {
    /// The name the user would type to enable this plugin.
    pub fn name(&self) -> &str {
        &self.id.name
    }
}

/// Every dependency-matched offer, grouped by what the config decided.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// Already enabled: named by a `use` entry, or offered by a registry.
    pub active: Vec<DiscoveredPlugin>,
    /// Enabled by `[plugins] auto-enable`.
    pub auto_enabled: Vec<DiscoveredPlugin>,
    /// Awaiting consent — newly discovered and not yet decided.
    pub candidates: Vec<DiscoveredPlugin>,
    /// Declined previously, recorded in `[plugins] disable`.
    pub declined: Vec<DiscoveredPlugin>,
}

impl Discovery {
    /// Every offer that may run, whatever enabled it.
    pub fn enabled(&self) -> impl Iterator<Item = &DiscoveredPlugin> {
        self.active.iter().chain(&self.auto_enabled)
    }
}

/// Discover the plugins offered for this workspace's dependencies.
///
/// `workspace_root` scopes the `use` entries that count (an entry can be
/// recorded for one workspace only). `active_plugins` fetches each dependency
/// cache-only — for a workspace dependency, into the source `cargo metadata`
/// already extracted (no probe, no network) — so every dependency-embedded
/// plugin is discoverable, registry crates included.
/// This legacy best-effort adapter remains through root 0.6. New callers should
/// use [`try_discover`] so invalid registries cannot appear to be empty.
pub async fn discover(sym: &Symposium, deps: &Arc<WorkspaceDeps>) -> Discovery {
    match try_discover(sym, deps).await {
        Ok(discovery) => discovery,
        Err(_) => {
            tracing::warn!("discovery skipped: invalid registry configuration");
            Discovery::default()
        }
    }
}

/// Discover dependency-embedded plugins after validating all registry sources.
///
/// Dependency listing remains best-effort for compatibility: a manager failure
/// emits a content-free warning and discovers no offers for this pass.
///
/// # Errors
/// Returns [`RegistryError`] before discovery when a configured source is invalid.
pub async fn try_discover(
    sym: &Symposium,
    deps: &Arc<WorkspaceDeps>,
) -> Result<Discovery, RegistryError> {
    let pms = sym.try_package_managers(deps)?;
    let Some(workspace_root) = deps.workspace_root().map(Path::to_path_buf) else {
        return Ok(Discovery::default());
    };
    let dep_ids = match pms.try_list_deps().await {
        Ok(ids) => ids,
        Err(_) => {
            tracing::warn!("dependency discovery skipped: failed to list workspace dependencies");
            Vec::new()
        }
    };

    let mut discovery = Discovery::default();
    // Untrusted instances = the cargo transport: its `active_plugins` are the
    // plugins embedded in dependencies, which run only with consent. Classify
    // each against the `[plugins]` config.
    for inst in pms.instances().filter(|i| !i.trusted) {
        for plugin in inst.active_plugins(&dep_ids).await {
            let name = plugin.canonical.name.clone();
            let description = Some(describe_plugin(&plugin.manifest));
            let enablement = decide(sym, &name, &workspace_root);
            let discovered = DiscoveredPlugin {
                registry: inst.name.clone(),
                id: plugin.canonical,
                recommends: name,
                description,
                enablement,
            };
            match enablement {
                Enablement::Used => discovery.active.push(discovered),
                Enablement::AutoEnabled => discovery.auto_enabled.push(discovered),
                Enablement::Declined => discovery.declined.push(discovered),
                Enablement::Candidate => discovery.candidates.push(discovered),
            }
        }
    }
    Ok(discovery)
}

/// The crate names whose embedded plugins the user has enabled, to load at
/// sync time. Two sources:
///
/// 1. workspace **dependencies** covered by `[plugins] auto-enable` or an
///    applicable `use` entry, and
/// 2. crates named by a `use` entry that are **not** dependencies —
///    `cargo agents use <crate>` pulls a plugin in from its registry
///    (crates.io) whether or not the workspace depends on it.
///
/// `auto-enable` intentionally contributes only (1): it is consent for what a
/// dependency you already have carries, not a way to add crates. Declined
/// names are pruned. This reads the config rather than the offer list, so a
/// `use`d crate that isn't a dependency at all (source not resolved yet) still
/// works.
pub fn enabled_dependencies(
    sym: &Symposium,
    dep_ids: &[PackageId],
    workspace_root: &Path,
) -> Vec<String> {
    let plugins = &sym.config.plugins;
    let mut names: Vec<String> = dep_ids
        .iter()
        .filter(|id| id.pm == CARGO_PM)
        .filter(|id| !plugins.is_disabled(&id.name))
        .filter(|id| {
            plugins.is_auto_enabled(&id.name) || plugins.is_used_in(&id.name, workspace_root)
        })
        .map(|id| id.name.clone())
        .collect();

    for used in plugins.used_names_in(workspace_root) {
        let norm = normalize_crate_name(used);
        let known = plugins.is_disabled(used)
            || names.iter().any(|n| normalize_crate_name(n) == norm)
            || dep_ids
                .iter()
                .any(|id| normalize_crate_name(&id.name) == norm);
        if !known {
            names.push(used.to_string());
        }
    }

    names
}

/// Run [`discover`] for the workspace `deps` points at, or an empty
/// [`Discovery`] when there is no workspace.
/// Legacy best-effort alias for [`discover`], retained through root 0.6.
pub async fn discover_for(sym: &Symposium, deps: &Arc<WorkspaceDeps>) -> Discovery {
    discover(sym, deps).await
}

/// Checked discovery for the workspace `deps` points at.
///
/// # Errors
/// Returns [`RegistryError`] when a configured source is invalid.
pub async fn try_discover_for(
    sym: &Symposium,
    deps: &Arc<WorkspaceDeps>,
) -> Result<Discovery, RegistryError> {
    try_discover(sym, deps).await
}

/// The names of the discovered offers still awaiting consent, deduplicated
/// and sorted — what a consent prompt would ask about, and what the
/// non-interactive hint names.
/// Legacy best-effort adapter retained through root 0.6. New callers should use
/// [`try_pending_candidates`] to distinguish invalid sources from no candidates.
pub async fn pending_candidates(sym: &Symposium, deps: &Arc<WorkspaceDeps>) -> Vec<String> {
    match try_pending_candidates(sym, deps).await {
        Ok(names) => names,
        Err(_) => {
            tracing::warn!("candidate hints skipped: invalid registry configuration");
            Vec::new()
        }
    }
}

/// Names of undecided dependency plugins, after validating registry sources.
///
/// # Errors
/// Returns [`RegistryError`] when a configured source is invalid.
pub async fn try_pending_candidates(
    sym: &Symposium,
    deps: &Arc<WorkspaceDeps>,
) -> Result<Vec<String>, RegistryError> {
    let mut names: Vec<String> = try_discover_for(sym, deps)
        .await?
        .candidates
        .into_iter()
        .map(|c| c.id.name)
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Record consent decisions: approvals into `[plugins] auto-enable`,
/// declines into `[plugins] disable`, then save the config.
///
/// Split out from the prompt so the decision recording is testable without a
/// terminal, and so other entry points can record the same way.
/// Legacy adapter retained through root 0.6; prefer [`try_apply_consent`].
pub fn apply_consent(
    sym: &mut Symposium,
    approved: &[String],
    declined: &[String],
) -> anyhow::Result<()> {
    try_apply_consent(sym, approved, declined).map_err(ConsentError::into_legacy)
}

/// Record explicit consent decisions after validating all registry sources.
///
/// # Errors
/// Returns [`ConsentError::Registry`] before mutation for an invalid source, or
/// [`ConsentError::SaveConfig`] when persistence fails. In-memory plugin
/// decisions are restored on a failed save, and success is reported only after
/// persistence completes.
pub fn try_apply_consent(
    sym: &mut Symposium,
    approved: &[String],
    declined: &[String],
) -> Result<(), ConsentError> {
    if approved.is_empty() && declined.is_empty() {
        return Ok(());
    }
    sym.try_registry_instances()
        .map_err(|source| ConsentError::Registry { source })?;
    let previous_plugins = sym.config.plugins.clone();
    let plugins = &mut sym.config.plugins;
    for name in approved {
        if !plugins.is_auto_enabled(name) {
            plugins.auto_enable.push(name.clone());
        }
    }
    for name in declined {
        if !plugins.is_disabled(name) {
            plugins.disable.push(name.clone());
        }
    }
    if let Err(source) = sym.try_save_config() {
        sym.config.plugins = previous_plugins;
        return Err(ConsentError::SaveConfig { source });
    }

    if !approved.is_empty() {
        tracing::info!(
            report = %ReportEvent::Info {
                message: format!("enabled dependency plugins: {}", approved.join(", ")),
            },
        );
    }
    if !declined.is_empty() {
        tracing::info!(
            report = %ReportEvent::Info {
                message: format!(
                    "declined dependency plugins (recorded in `[plugins] disable`): {}",
                    declined.join(", ")
                ),
            },
        );
    }
    Ok(())
}

/// Ask the user about each undecided offer, then record the answers.
///
/// **Never prompts unless `out` is interactive** ([`Output::is_interactive`]):
/// hook dispatch and anything an agent triggers use a quiet or capturing
/// output, so they return here immediately without touching stdin. In those
/// contexts the candidates surface as a `SessionStart` hint instead (see
/// [`pending_candidates`]).
///
/// Only explicit answers are recorded — the default ("ask me later") leaves
/// the dependency undecided, so reflexively hitting Enter never permanently
/// declines anything, and Escape leaves the remaining offers undecided too.
pub async fn prompt_for_consent(
    sym: &mut Symposium,
    deps: &Arc<WorkspaceDeps>,
    out: &Output,
) -> anyhow::Result<()> {
    try_prompt_for_consent(sym, deps, out)
        .await
        .map_err(ConsentError::into_legacy)
}

/// Ask interactively about undecided offers and persist only explicit answers.
///
/// The gating, default "ask me later", and Escape behavior match
/// [`prompt_for_consent`]. Non-interactive outputs never touch stdin.
///
/// # Errors
/// Returns [`ConsentError`] for invalid registry sources, terminal interaction
/// failures, or a failed save. A failed save restores in-memory decisions.
pub async fn try_prompt_for_consent(
    sym: &mut Symposium,
    deps: &Arc<WorkspaceDeps>,
    out: &Output,
) -> Result<(), ConsentError> {
    if !out.is_interactive() {
        return Ok(());
    }
    let candidates = try_discover_for(sym, deps)
        .await
        .map_err(|source| ConsentError::Registry { source })?
        .candidates;
    if candidates.is_empty() {
        return Ok(());
    }

    let mut approved = Vec::new();
    let mut declined = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for candidate in candidates {
        let name = candidate.id.name;
        if !seen.insert(name.clone()) {
            continue;
        }
        let what = candidate
            .description
            .as_deref()
            .unwrap_or("agent extensions");
        let answer = dialoguer::Select::new()
            .with_prompt(format!("Dependency `{name}` provides {what}. Enable it?"))
            .items(["Ask me later", "Enable", "No — don't ask again"])
            .default(0)
            .interact_opt()
            .map_err(|source| ConsentError::Prompt { source })?;
        match answer {
            Some(1) => approved.push(name),
            Some(2) => declined.push(name),
            Some(_) => {}  // ask me later — record nothing
            None => break, // Esc — leave the rest undecided
        }
    }
    try_apply_consent(sym, &approved, &declined)
}

/// A short human summary of what a discovered plugin contributes, for the
/// consent prompt and status output. Emphasizes the facets that matter to a
/// trust decision — a plugin that only ships skills is lower-stakes than one
/// that runs a hook or an MCP server.
fn describe_plugin(plugin: &crate::plugins::PluginManifest) -> String {
    let parts: Vec<String> = [
        count_phrase(plugin.skills.len(), "skill group", "skill groups"),
        count_phrase(plugin.hooks.len(), "hook", "hooks"),
        count_phrase(plugin.mcp_servers.len(), "MCP server", "MCP servers"),
        count_phrase(plugin.subcommands.len(), "subcommand", "subcommands"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "agent extensions".to_string()
    } else {
        parts.join(", ")
    }
}

/// `"<n> <singular|plural>"`, or `None` when `n` is zero.
fn count_phrase(n: usize, singular: &str, plural: &str) -> Option<String> {
    (n > 0).then(|| format!("{n} {}", if n == 1 { singular } else { plural }))
}

/// Classify one offer against the `[plugins]` config. An explicit decision —
/// `use`, then `disable` — outranks the standing `auto-enable`, so a name the
/// user declined stays declined.
fn decide(sym: &Symposium, name: &str, workspace_root: &Path) -> Enablement {
    let plugins = &sym.config.plugins;
    // `disable` is tested first because it is the last word: every other
    // entry says a plugin *may* run, and this is the only one that says it
    // may not. Activation agrees (see `record_active`), so classifying a
    // disabled plugin as `Used` here would report it active while it stays
    // off.
    if plugins.is_disabled(name) {
        Enablement::Declined
    } else if plugins.is_used_in(name, workspace_root) {
        Enablement::Used
    } else if plugins.is_auto_enabled(name) {
        Enablement::AutoEnabled
    } else {
        Enablement::Candidate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm::ANY_VERSION;
    use crate::pm::WorkspaceCrate;
    use indoc::indoc;

    /// A workspace with `widget-lib` as a path dependency carrying skills,
    /// plus a plain registry dependency (an extracted source with no plugin
    /// content, as `cargo metadata` always yields a `source_dir`).
    fn workspace(root: &Path) -> Arc<WorkspaceDeps> {
        let widget = root.join("widget-lib");
        std::fs::create_dir_all(widget.join("skills/guidance")).unwrap();
        std::fs::write(widget.join("skills/guidance/SKILL.md"), "").unwrap();
        let serde = root.join("serde-src");
        std::fs::create_dir_all(&serde).unwrap();
        WorkspaceDeps::fixture(
            root.to_path_buf(),
            vec![
                WorkspaceCrate::new(
                    "widget-lib".to_string(),
                    semver::Version::new(1, 0, 0),
                    Some(widget),
                ),
                WorkspaceCrate::new("serde".to_string(), semver::Version::new(1, 0, 210), None)
                    .with_source_dir(Some(serde)),
            ],
        )
    }

    /// A `Symposium` over a fresh config dir with only the given config, and
    /// no built-in registries (so tests see only what they set up).
    fn sym_with(root: &Path, config: &str) -> Symposium {
        let config_dir = root.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            format!(
                "{}\n[defaults]\nsymposium-recommendations = false\nuser-plugins = false\n",
                config
            ),
        )
        .unwrap();
        Symposium::from_dir(&config_dir)
    }

    #[test]
    fn failed_consent_save_restores_decisions_and_preserves_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = sym_with(
            tmp.path(),
            "[plugins]\nauto-enable = [\"already-approved\"]\ndisable = [\"already-declined\"]",
        );
        let previous = sym.config.plugins.clone();
        let config_path = sym.config_dir().join("config.toml");
        let original = std::fs::read(&config_path).unwrap();
        let saved_path = sym.config_dir().join("original-config.toml");
        std::fs::rename(&config_path, &saved_path).unwrap();
        std::fs::create_dir(&config_path).unwrap();
        let sentinel = config_path.join("keep");
        std::fs::write(&sentinel, b"existing destination contents").unwrap();

        let error = try_apply_consent(&mut sym, &["new-approval".into()], &["new-decline".into()])
            .unwrap_err();

        assert_eq!(sym.config.plugins, previous);
        assert_eq!(std::fs::read(saved_path).unwrap(), original);
        assert_eq!(
            std::fs::read(sentinel).unwrap(),
            b"existing destination contents"
        );
        let ConsentError::SaveConfig { source } = error else {
            panic!("expected a typed save failure");
        };
        assert!(matches!(source, ConfigError::Write { source, .. }
            if source.kind() != std::io::ErrorKind::NotFound));
    }

    #[test]
    fn rejected_consent_keeps_registry_source_and_legacy_downcast() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = sym_with(tmp.path(), "");
        sym.config.registries.push(crate::config::RegistryConfig {
            name: "invalid".into(),
            git: Some("https://synthetic-consent-canary.invalid/token".into()),
            path: Some("local".into()),
            auto_update: false,
        });
        let original = std::fs::read(sym.config_dir().join("config.toml")).unwrap();

        let error = try_apply_consent(&mut sym, &["candidate".into()], &[]).unwrap_err();
        assert!(!error.to_string().contains("synthetic-consent-canary"));
        assert!(matches!(
            error,
            ConsentError::Registry {
                source: RegistryError::BothSources { .. }
            }
        ));
        let legacy = apply_consent(&mut sym, &["candidate".into()], &[]).unwrap_err();
        assert!(legacy.downcast_ref::<RegistryError>().is_some());
        assert!(sym.config.plugins.auto_enable.is_empty());
        assert_eq!(
            std::fs::read(sym.config_dir().join("config.toml")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn typed_consent_never_prompts_or_mutates_noninteractive_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = sym_with(tmp.path(), "");
        let deps = workspace(tmp.path());
        let original = std::fs::read(sym.config_dir().join("config.toml")).unwrap();

        for output in [Output::quiet(), Output::capturing()] {
            try_prompt_for_consent(&mut sym, &deps, &output)
                .await
                .unwrap();
        }

        assert_eq!(sym.config.plugins, crate::config::PluginsConfig::default());
        assert_eq!(
            std::fs::read(sym.config_dir().join("config.toml")).unwrap(),
            original
        );
    }

    #[test]
    fn consent_prompt_error_retains_the_concrete_terminal_source() {
        use std::error::Error;

        let error = ConsentError::Prompt {
            source: dialoguer::Error::IO(std::io::Error::other("synthetic-prompt-canary")),
        };
        assert!(!error.to_string().contains("synthetic-prompt-canary"));
        assert!(error.source().unwrap().is::<dialoguer::Error>());
        assert!(
            error
                .into_legacy()
                .downcast_ref::<dialoguer::Error>()
                .is_some()
        );
    }

    #[tokio::test]
    async fn undecided_dependency_plugin_is_a_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = workspace(tmp.path());
        let sym = sym_with(tmp.path(), "");

        let found = discover(&sym, &ws).await;
        assert!(found.active.is_empty());
        assert!(found.auto_enabled.is_empty());
        let names: Vec<&str> = found.candidates.iter().map(|c| c.name()).collect();
        assert_eq!(names, vec!["widget-lib"]);
        assert_eq!(found.candidates[0].recommends, "widget-lib");
    }

    #[tokio::test]
    async fn auto_enable_moves_a_candidate_to_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = workspace(tmp.path());
        let sym = sym_with(
            tmp.path(),
            indoc! {r#"
                [plugins]
                auto-enable = ["widget_lib"]
            "#},
        );

        let found = discover(&sym, &ws).await;
        assert!(found.candidates.is_empty());
        let names: Vec<&str> = found.auto_enabled.iter().map(|c| c.name()).collect();
        assert_eq!(names, vec!["widget-lib"]);
        assert!(found.enabled().count() == 1);
    }

    #[tokio::test]
    async fn use_entry_and_disable_outrank_the_standing_decisions() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = workspace(tmp.path());

        let sym = sym_with(
            tmp.path(),
            indoc! {r#"
                [plugins]
                use = ["widget-lib"]
            "#},
        );
        let found = discover(&sym, &ws).await;
        assert_eq!(found.active.len(), 1);
        assert_eq!(found.active[0].enablement, Enablement::Used);

        let sym = sym_with(
            tmp.path(),
            indoc! {r#"
                [plugins]
                auto-enable = ["*"]
                disable = ["widget-lib"]
            "#},
        );
        let found = discover(&sym, &ws).await;
        assert!(found.auto_enabled.is_empty());
        assert_eq!(found.declined.len(), 1);
    }

    /// A `use` entry recorded for another workspace does not enable anything
    /// here.
    #[tokio::test]
    async fn workspace_scoped_use_entries_only_count_in_their_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = workspace(tmp.path());
        let sym = sym_with(
            tmp.path(),
            indoc! {r#"
                [plugins]
                use = [{ name = "widget-lib", workspace = "/elsewhere" }]
            "#},
        );

        let found = discover(&sym, &ws).await;
        assert!(found.active.is_empty());
        assert_eq!(found.candidates.len(), 1);
    }

    #[test]
    fn enabled_dependencies_reads_config_not_offers() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = sym_with(
            tmp.path(),
            indoc! {r#"
                [plugins]
                auto-enable = ["serde"]
                use = ["tokio"]
                disable = ["clap"]
            "#},
        );
        let deps = [
            // A registry dependency, invisible to `active_plugins`, is still
            // enabled by name.
            PackageId::new(CARGO_PM, "serde", "1.0.210"),
            PackageId::new(CARGO_PM, "tokio", "1.0.0"),
            PackageId::new(CARGO_PM, "clap", "4.0.0"),
            PackageId::new(CARGO_PM, "anyhow", "1.0.0"),
            // Not a cargo package: not a crate to load.
            PackageId::new("npm", "serde", ANY_VERSION),
        ];

        assert_eq!(
            enabled_dependencies(&sym, &deps, tmp.path()),
            vec!["serde".to_string(), "tokio".to_string()]
        );
    }

    /// `use`-ing a crate that isn't a workspace dependency still enables it, so
    /// sync loads its plugin from the registry. `auto-enable` does not — it is
    /// consent for dependencies you already have.
    #[test]
    fn use_enables_a_non_dependency_crate_but_auto_enable_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = sym_with(
            tmp.path(),
            indoc! {r#"
                [plugins]
                auto-enable = ["not-a-dep-autoenable"]
                use = ["my-skills-crate", { name = "scoped-crate", workspace = "/elsewhere" }]
            "#},
        );
        // Only `anyhow` is an actual dependency; the rest are not.
        let deps = [PackageId::new(CARGO_PM, "anyhow", "1.0.0")];

        let enabled = enabled_dependencies(&sym, &deps, tmp.path());
        // The `use`d non-dependency crate is enabled; the workspace-scoped one
        // (for /elsewhere) and the auto-enabled non-dependency are not.
        assert_eq!(enabled, vec!["my-skills-crate".to_string()]);
    }

    /// A `use`d crate that *is* a dependency appears once, not twice.
    #[test]
    fn used_dependency_is_not_duplicated() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = sym_with(
            tmp.path(),
            indoc! {r#"
                [plugins]
                use = ["serde"]
            "#},
        );
        let deps = [PackageId::new(CARGO_PM, "serde", "1.0.210")];
        assert_eq!(
            enabled_dependencies(&sym, &deps, tmp.path()),
            vec!["serde".to_string()]
        );
    }
}
