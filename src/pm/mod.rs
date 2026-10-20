//! Package managers: the in-process seam from the [registry-centric plugin
//! distribution RFD](../../md/rfds/registry-centric-plugins/README.md).
//!
//! A [`PackageId`] names a package as a `(pm, name, version)` tuple, and a
//! [`PackageManager`] resolves ids of its ecosystem to content on disk.
//!
//! A `PackageManager` value is an *instance*, not just an ecosystem. A
//! **transport** ([`CargoPm`]) can `fetch` any id of its ecosystem, because the
//! id carries the source; a **registry instance** ([`PathPm`]) fronts one
//! configured source and enumerates the packages it contains. [`PmRegistry`]
//! holds them as one flat set: an id is dispatched to the instance whose
//! [`PackageManager::name`] matches its [`PackageId::pm`], and plugin loading
//! ([`active_plugins`](PackageManager::active_plugins) /
//! [`load_plugin`](PackageManager::load_plugin) / `search`) iterates every
//! instance.
//!
//! A registry instance's [`PackageManager::name`] is the *configured registry
//! name* (`user-plugins`, `symposium-recommendations`, …), which is also the
//! `pm` component of every id it mints and the name its plugins are attributed
//! to. Registry instances resolve their own ids, so those ids are never routed
//! through the ecosystem transports.
//!
//! In-process for now — when PMs move out of process, [`PmRegistry`] becomes
//! the seam that spawns and talks to them.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use symposium_install::UpdateLevel;

use crate::plugins::Plugin;

/// A concrete failure while reading a package's Cargo manifest metadata.
pub use crate::crate_metadata::MetadataError;
/// A concrete failure while locating a Cargo package's source directory.
pub use crate::crate_sources::CrateSourceError;

mod cargo;
mod git;
pub mod layout;
mod path;
pub use cargo::{
    CargoPm, LoadedWorkspace, WorkspaceCrate, WorkspaceDeps, WorkspaceError, file_mtime,
    try_file_mtime, try_find_workspace_root, workspace_dir_name,
};
pub use git::GitPm;
pub use path::PathPm;

/// The identity types are the SDK's, since they cross the PM boundary: a
/// package-manager binary speaks in them too.
pub use symposium_sdk::pm::{ANY_VERSION, CARGO_PM, PackageId, PluginInfo, UnvalidatedPlugin};

/// A fetched package: the exact id it resolved to, plus the directory
/// holding its content.
#[derive(Debug, Clone)]
pub struct FetchedPackage {
    pub id: PackageId,
    pub root: PathBuf,
}

/// A package-manager failure with its original source and a safe diagnostic.
///
/// Source chains may contain registry response text or URLs. Display describes
/// the operation without rendering those sources; callers must not print an
/// unrestricted source chain for user-controlled package data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PackageManagerError {
    /// Required workspace metadata could not be resolved.
    #[error("failed to resolve workspace dependencies")]
    Workspace {
        /// The original shared workspace failure.
        source: Arc<WorkspaceError>,
    },
    /// No registered manager owns the package's manager tag.
    #[error("unknown package manager `{manager}`")]
    UnknownManager {
        /// The requested manager tag.
        manager: String,
        /// The unresolved package identity.
        id: PackageId,
    },
    /// A package was sent directly to a manager for a different ecosystem.
    #[error("package manager `{expected}` cannot handle manager `{actual}`")]
    WrongManager {
        /// The manager that received the request.
        expected: String,
        /// The package's manager tag.
        actual: String,
    },
    /// A registry package name could escape its configured source directory.
    #[error("registry package path must be nonempty and relative without parent components")]
    InvalidPackagePath {
        /// The rejected package identity, retained without rendering its input.
        id: PackageId,
    },
    /// A Git registry has no readable cached source yet.
    #[error("registry `{name}` has not been fetched yet; run `cargo agents plugin sync {name}`")]
    RegistryNotFetched {
        /// The configured registry name.
        name: String,
    },
    /// A local registry directory could not be inspected.
    #[error("cannot read registry `{name}` at {}", path.display())]
    ReadRegistry {
        /// The configured registry name.
        name: String,
        /// The registry directory.
        path: PathBuf,
        /// The original filesystem failure.
        source: std::io::Error,
    },
    /// A fetched package's source directory is absent, unreadable, or not a directory.
    #[error("cannot read package source directory at {}", path.display())]
    PackageSource {
        /// The source directory returned by the package fetcher.
        path: PathBuf,
        /// The original filesystem failure, or a rejected non-directory kind.
        source: std::io::Error,
    },
    /// A registry directory could not be enumerated as a complete source.
    #[error("cannot enumerate registry `{name}`")]
    RegistryLayout {
        /// The configured registry name.
        name: String,
        /// The original layout or filesystem failure.
        source: layout::LayoutError,
    },
    /// Cargo package-source discovery failed.
    #[error("failed to locate Cargo package sources")]
    FetchCrate {
        /// The requested package identity.
        id: PackageId,
        /// The original source-discovery failure.
        source: Box<CrateSourceError>,
    },
    /// The crates.io HTTP client could not be constructed.
    #[error("failed to build the crates.io search client")]
    Client {
        /// The original client-construction failure.
        source: reqwest::Error,
    },
    /// The crates.io search request or response failed.
    #[error("crates.io search failed")]
    SearchCrates {
        /// The original registry failure.
        source: crates_io_api::Error,
    },
    /// A Git registry could not be refreshed.
    #[error("failed to refresh Git registry `{name}`")]
    RefreshGitRegistry {
        /// The configured registry name.
        name: String,
        /// The original Git acquisition failure.
        source: symposium_install::git::GitError,
    },
    /// A package's plugin manifest could not be read or parsed.
    #[error("failed to load a plugin: {source}")]
    PluginLoad {
        /// The package whose plugin could not be loaded.
        id: PackageId,
        /// The safe outer failure with its original parser or I/O source.
        source: Box<crate::plugins::PluginLoadError>,
    },
    /// A package's parsed plugin violated the contributor contract.
    #[error("invalid plugin at `{}`", path.display())]
    PluginValidation {
        /// The package whose plugin was rejected.
        id: PackageId,
        /// The plugin's source directory.
        path: PathBuf,
        /// The semantic validation failure.
        source: Box<crate::plugins::PluginValidationError>,
    },
    /// A package's Cargo metadata could not be read or parsed.
    #[error("failed to read Cargo plugin metadata")]
    PluginMetadata {
        /// The package whose metadata could not be loaded.
        id: PackageId,
        /// The concrete metadata failure.
        source: Box<MetadataError>,
    },
    /// An external implementation supplies only the pre-0.6 dynamic contract.
    ///
    /// Built-in managers return the concrete variants above. This bridge keeps
    /// existing downstream trait implementations source-compatible through 0.6.
    #[error("legacy package manager `{manager}` failed to {operation}")]
    LegacyImplementation {
        /// The external manager's name.
        manager: String,
        /// The requested operation.
        operation: &'static str,
        /// The external implementation's original error chain.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl PackageManagerError {
    fn legacy(manager: &str, operation: &'static str, error: anyhow::Error) -> Self {
        Self::LegacyImplementation {
            manager: manager.to_owned(),
            operation,
            source: error.reallocate_into_boxed_dyn_error_without_backtrace(),
        }
    }
}

/// Plugins collected together with failures and knowledge of source completeness.
///
/// Legacy implementations may contribute valid plugins without exposing their
/// omitted failures. Their outcomes remain incomplete so that absence cannot
/// authorize deletion. A collection with any reported error is always incomplete.
#[derive(Debug)]
pub struct PluginCollection<T> {
    pub(crate) plugins: Vec<T>,
    pub(crate) errors: Vec<PackageManagerError>,
    pub(crate) is_complete: bool,
}

impl<T> PluginCollection<T> {
    /// A producer successfully enumerated every plugin in its source.
    pub fn complete(plugins: Vec<T>) -> Self {
        Self {
            plugins,
            errors: Vec::new(),
            is_complete: true,
        }
    }

    /// Retain valid contributions when failures or legacy limitations hide others.
    pub fn incomplete(plugins: Vec<T>, errors: Vec<PackageManagerError>) -> Self {
        Self {
            plugins,
            errors,
            is_complete: false,
        }
    }

    /// Whether the producer knows no source or plugin was omitted.
    pub fn is_complete(&self) -> bool {
        self.is_complete && self.errors.is_empty()
    }

    /// Consume the valid plugins, reported failures, and completeness evidence.
    pub fn into_parts(self) -> (Vec<T>, Vec<PackageManagerError>, bool) {
        let is_complete = self.is_complete();
        (self.plugins, self.errors, is_complete)
    }

    pub(crate) fn into_result(self) -> Result<Vec<T>, PackageManagerError> {
        match self.errors.into_iter().next() {
            Some(error) => Err(error),
            None => Ok(self.plugins),
        }
    }
}

/// The operations every package manager implements (per the registry-centric
/// plugin distribution RFD).
///
/// A PM is self-contained: it holds whatever it needs to resolve its own
/// ecosystem ([`CargoPm`] owns an [`Arc<WorkspaceDeps>`], [`PathPm`] owns its
/// directory), so operations take no ambient context. This mirrors the
/// out-of-process shape — a PM spawned for a workspace answers RPC calls from
/// its own state, with nothing workspace-shaped threaded per call.
///
/// Loading has two forms. [`active_plugins`](Self::active_plugins) is what the
/// PM activates for the workspace's dependency set (a registry lists its
/// entries; the cargo transport surfaces dependency-embedded plugins);
/// [`load_plugin`](Self::load_plugin) resolves a *specific* id named elsewhere
/// (a `[[plugins]]` chained reference, an explicitly enabled crate). Both
/// answer with [`UnvalidatedPlugin`]s: an id, a content directory, and an
/// *unvalidated* manifest, and are best-effort: failures are logged and
/// dropped, not surfaced, so one bad plugin never aborts a sync or hook.
///
/// Validating an [`UnvalidatedPlugin`] into a [`Plugin`] is [`PmInstance`]'s
/// job, not the PM's, because the policy depends on where the plugin came from rather than
/// on what it says. That is the whole trust boundary: see [`PluginKind`].
///
/// The original fallible methods retain their dynamic error contract through
/// root 0.6. New callers use the additive `try_*` methods; existing external
/// implementations need not add methods until they adopt the typed contract.
#[async_trait::async_trait]
pub trait PackageManager {
    /// The PM's registry name — the `pm` component of every id it owns. For
    /// an ecosystem transport this is the ecosystem (`cargo`); for a registry
    /// instance it is the configured registry's name.
    fn name(&self) -> &str;

    /// The plugins this PM activates for the workspace's dependency set. A
    /// registry lists its own entries (deps ignored); the cargo transport
    /// surfaces the plugins its dependencies embed. Whether a dependency-embedded
    /// plugin is *trusted* is the caller's decision — see [`PmInstance::trusted`].
    async fn active_plugins(&self, deps: &[PackageId]) -> Vec<UnvalidatedPlugin>;

    /// Fails instead of returning an empty list when the source cannot be
    /// read, since sync removes what a registry stops offering.
    async fn registry_plugins(&self) -> Result<Vec<UnvalidatedPlugin>> {
        Ok(self.active_plugins(&[]).await)
    }

    /// List a complete registry source with a typed failure.
    ///
    /// The default bridges a legacy external implementation. Built-in managers
    /// override this method to preserve their concrete error variants.
    async fn try_registry_plugins(&self) -> Result<Vec<UnvalidatedPlugin>, PackageManagerError> {
        self.registry_plugins().await.map_err(|error| {
            PackageManagerError::legacy(self.name(), "list registry plugins", error)
        })
    }

    /// Retain valid registry plugins and explicit completeness evidence.
    ///
    /// The default preserves a legacy implementation's contributions but cannot
    /// know which errors its old listing suppressed, so completeness is unknown.
    async fn registry_plugins_outcome(
        &self,
    ) -> Result<PluginCollection<UnvalidatedPlugin>, PackageManagerError> {
        self.registry_plugins()
            .await
            .map(|plugins| PluginCollection::incomplete(plugins, Vec::new()))
            .map_err(|error| {
                PackageManagerError::legacy(self.name(), "list registry plugins", error)
            })
    }

    /// The plugin(s) a specific id maps to — zero, one, or many. Used for
    /// `[[plugins]]` chained references and explicitly enabled crates.
    async fn load_plugin(&self, id: &PackageId) -> Vec<UnvalidatedPlugin>;

    /// Load a named plugin while propagating failures the producer can report.
    ///
    /// Legacy implementations keep their existing contributions. Callers that
    /// may remove missing content must inspect [`Self::load_plugin_outcome`].
    async fn try_load_plugin(
        &self,
        id: &PackageId,
    ) -> Result<Vec<UnvalidatedPlugin>, PackageManagerError> {
        Ok(self.load_plugin(id).await)
    }

    /// Load plugins with completeness evidence for absence-sensitive operations.
    ///
    /// Legacy producers keep their valid plugins; their suppressed failures are
    /// unknown, so this default never authorizes stale-content cleanup.
    async fn load_plugin_outcome(&self, id: &PackageId) -> PluginCollection<UnvalidatedPlugin> {
        PluginCollection::incomplete(self.load_plugin(id).await, Vec::new())
    }

    /// The package ids the current workspace depends on. Empty for PMs with no
    /// workspace notion.
    async fn list_deps(&self) -> Result<Vec<PackageId>>;

    /// List dependencies with a typed failure, preserving legacy implementors.
    async fn try_list_deps(&self) -> Result<Vec<PackageId>, PackageManagerError> {
        self.list_deps()
            .await
            .map_err(|error| PackageManagerError::legacy(self.name(), "list dependencies", error))
    }

    /// Find packages matching a partial query (backs `use` / `search`). PMs
    /// without a searchable registry return an empty list.
    async fn search(&self, query: &str) -> Result<Vec<PluginInfo>>;

    /// Search with a typed failure, preserving legacy implementors.
    async fn try_search(&self, query: &str) -> Result<Vec<PluginInfo>, PackageManagerError> {
        self.search(query)
            .await
            .map_err(|error| PackageManagerError::legacy(self.name(), "search packages", error))
    }

    /// Acquire a package's source content, canonicalizing the id's version.
    /// `update` controls how aggressively an already-cached package is refreshed.
    async fn fetch(&self, id: &PackageId, update: UpdateLevel) -> Result<FetchedPackage>;

    /// Fetch with a typed failure, preserving legacy implementors.
    async fn try_fetch(
        &self,
        id: &PackageId,
        update: UpdateLevel,
    ) -> Result<FetchedPackage, PackageManagerError> {
        self.fetch(id, update)
            .await
            .map_err(|error| PackageManagerError::legacy(self.name(), "fetch a package", error))
    }

    /// Refresh this PM's backing source — for a registry, pull the latest
    /// content onto disk. `force` ignores the source's auto-update opt-out
    /// (used by an explicit `plugin sync <name>`). Returns whether a remote
    /// source was actually refreshed, so callers can report what synced. The
    /// default is a no-op: a PM whose content is already local (the cargo
    /// transport, a path registry) has nothing to pull.
    async fn refresh(&self, _update: UpdateLevel, _force: bool) -> Result<bool> {
        Ok(false)
    }

    /// Refresh with a typed failure, preserving legacy implementors.
    async fn try_refresh(
        &self,
        update: UpdateLevel,
        force: bool,
    ) -> Result<bool, PackageManagerError> {
        self.refresh(update, force)
            .await
            .map_err(|error| PackageManagerError::legacy(self.name(), "refresh a source", error))
    }

    /// How a registry instance's content is sourced, for `plugin list`
    /// display. `None` for the cargo transport, which is not a configured
    /// registry.
    fn registry_source(&self) -> Option<RegistrySource> {
        None
    }
}

/// Where a registry instance's content comes from — the git-vs-path
/// distinction, made visible on the PM itself rather than inferred elsewhere.
pub enum RegistrySource {
    /// A git repository, fetched into the plugin-source cache.
    Git { url: String },
    /// A local directory.
    Path { dir: PathBuf },
}

/// Which validation policy an instance's plugins get.
///
/// Symposium's decision, keyed on the kind of instance the plugin came from:
/// never something a package manager states about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// A curated registry entry. It must name itself, `[defaults]` is
    /// rejected, and one that references no dependency anywhere loads
    /// [dormant](crate::plugins::PluginManifest::requires_use): there is nothing to
    /// infer a gate from, and "always on" would fire it in every workspace.
    Registry,
    /// A package in an ecosystem. Its id supplies the name, the reference that
    /// reached it supplies the gate (so dormancy does not apply), and it picks
    /// up the default `skills/` group.
    Package,
}

/// A package-manager instance: its attribution name (the config registry name,
/// or `cargo` for the transport: the `pm` component of every id it owns), the
/// policy its plugins are validated under, a trust marker, and the PM itself.
pub struct PmInstance {
    pub name: String,
    /// Whether this instance's [`active_plugins`](PackageManager::active_plugins)
    /// are trust roots. Registries and the workspace are trusted; the cargo
    /// transport is not, since its `active_plugins` are the plugins *embedded in
    /// dependencies*, which run only with the user's consent.
    pub trusted: bool,
    /// Validation policy for this instance's plugins.
    pub kind: PluginKind,
    pub pm: Box<dyn PackageManager + Send + Sync>,
}

impl PmInstance {
    /// This instance's active plugins, validated. A plugin that fails
    /// validation is logged and dropped: best-effort, like the PM layer above.
    pub async fn active_plugins(&self, deps: &[PackageId]) -> Vec<Plugin> {
        self.validate_all(self.pm.active_plugins(deps).await)
    }

    pub async fn registry_plugins(&self) -> Option<Vec<Plugin>> {
        match self.registry_plugins_outcome().await {
            Ok(collection) => {
                self.report_failures(&collection.errors);
                Some(collection.plugins)
            }
            Err(e) => {
                tracing::warn!(
                    report = %crate::report::ReportEvent::Warning {
                        message: e.to_string(),
                    },
                );
                None
            }
        }
    }

    /// Load and validate this registry, rejecting reported plugin failures.
    ///
    /// # Errors
    /// Returns the manager's concrete source or validation failure. Legacy
    /// producers cannot prove completeness; absence-sensitive callers must use
    /// the collection outcome rather than infer completeness from this result.
    pub async fn try_registry_plugins(&self) -> Result<Vec<Plugin>, PackageManagerError> {
        self.registry_plugins_outcome().await?.into_result()
    }

    pub(crate) async fn registry_plugins_outcome(
        &self,
    ) -> Result<PluginCollection<Plugin>, PackageManagerError> {
        Ok(self.validate_collection(self.pm.registry_plugins_outcome().await?))
    }

    /// The plugin(s) an id maps to, validated.
    pub async fn load_plugin(&self, id: &PackageId) -> Vec<Plugin> {
        let collection = self.load_plugin_outcome(id).await;
        self.report_failures(&collection.errors);
        collection.plugins
    }

    /// Load a named plugin while retaining its concrete reported failures.
    ///
    /// # Errors
    /// Returns a source or validation failure reported by the producer. Legacy
    /// producer completeness remains unknown even when this returns success.
    pub async fn try_load_plugin(
        &self,
        id: &PackageId,
    ) -> Result<Vec<Plugin>, PackageManagerError> {
        self.load_plugin_outcome(id).await.into_result()
    }

    async fn load_plugin_outcome(&self, id: &PackageId) -> PluginCollection<Plugin> {
        self.validate_collection(self.pm.load_plugin_outcome(id).await)
    }

    /// A registry entry that fails validation is the user's to fix, so it is
    /// reported. A package's is its author's, and the hook path must not print
    /// it next to the agent's JSON, so it is only logged.
    fn validate_all(&self, unvalidated: Vec<UnvalidatedPlugin>) -> Vec<Plugin> {
        let collection = self.validate_collection(PluginCollection::complete(unvalidated));
        self.report_failures(&collection.errors);
        collection.plugins
    }

    fn validate_collection(
        &self,
        unvalidated: PluginCollection<UnvalidatedPlugin>,
    ) -> PluginCollection<Plugin> {
        let mut plugins = Vec::new();
        let mut errors = unvalidated.errors;
        for plugin in unvalidated.plugins {
            let id = plugin.id.clone();
            let path = plugin.root.clone();
            match crate::plugins::validate_plugin(plugin, self.kind) {
                Ok(plugin) => plugins.push(plugin),
                Err(source) => errors.push(PackageManagerError::PluginValidation {
                    id,
                    path,
                    source: Box::new(source),
                }),
            }
        }
        if unvalidated.is_complete && errors.is_empty() {
            PluginCollection::complete(plugins)
        } else {
            PluginCollection::incomplete(plugins, errors)
        }
    }

    fn report_failures(&self, errors: &[PackageManagerError]) {
        for error in errors {
            match self.kind {
                PluginKind::Registry => tracing::warn!(
                    report = %crate::report::ReportEvent::Warning {
                        message: format!("skipping {error}"),
                    },
                ),
                PluginKind::Package => {
                    tracing::warn!(error = %error, "failed to load package plugin")
                }
            }
        }
    }
}

/// The active set of package-manager instances — one flat collection, the cargo
/// transport alongside one instance per configured registry. Ids are dispatched
/// by their `pm` component ([`PackageId::pm`]) to the instance that owns them.
pub struct PmRegistry {
    instances: Vec<PmInstance>,
}

impl PmRegistry {
    pub fn new(instances: Vec<PmInstance>) -> Self {
        Self { instances }
    }

    /// Every instance, in order (cargo transport first, then registries).
    pub fn instances(&self) -> impl Iterator<Item = &PmInstance> {
        self.instances.iter()
    }

    /// The instance owning the named ecosystem/registry.
    fn owner(
        &self,
        pm: &str,
        id: &PackageId,
    ) -> Result<&(dyn PackageManager + Send + Sync), PackageManagerError> {
        self.instances
            .iter()
            .find(|inst| inst.pm.name() == pm)
            .map(|inst| inst.pm.as_ref())
            .ok_or_else(|| PackageManagerError::UnknownManager {
                manager: pm.to_owned(),
                id: id.clone(),
            })
    }

    /// Fetch a package via the instance named in its id.
    ///
    /// Legacy adapter retained until root 0.6; use [`Self::try_fetch`].
    pub async fn fetch(&self, id: &PackageId, update: UpdateLevel) -> Result<FetchedPackage> {
        Ok(self.try_fetch(id, update).await?)
    }

    /// Fetch a package with a concrete manager failure.
    ///
    /// # Errors
    /// Returns an unknown-manager error or the owning manager's typed failure.
    pub async fn try_fetch(
        &self,
        id: &PackageId,
        update: UpdateLevel,
    ) -> Result<FetchedPackage, PackageManagerError> {
        self.owner(&id.pm, id)?.try_fetch(id, update).await
    }

    /// Union of `list_deps` across the instances — the workspace's full
    /// dependency set for discovery and `depends-on` predicate evaluation.
    ///
    /// Legacy adapter retained until root 0.6; use [`Self::try_list_deps`].
    pub async fn list_deps(&self) -> Result<Vec<PackageId>> {
        Ok(self.try_list_deps().await?)
    }

    /// Union every instance's dependencies without erasing its error source.
    ///
    /// # Errors
    /// Returns the first manager listing failure; no partial union is returned.
    pub async fn try_list_deps(&self) -> Result<Vec<PackageId>, PackageManagerError> {
        let mut deps = Vec::new();
        for inst in &self.instances {
            deps.extend(inst.pm.try_list_deps().await?);
        }
        Ok(deps)
    }

    /// Load the plugin(s) an id maps to, asking every instance. Any instance may
    /// contribute a plugin relevant to the id, so this can return several. Each
    /// instance's plugins are validated under its own [`PluginKind`].
    pub async fn load_plugin(&self, id: &PackageId) -> Vec<Plugin> {
        let mut out = Vec::new();
        for inst in &self.instances {
            out.extend(inst.load_plugin(id).await);
        }
        out
    }

    /// Load named plugins while rejecting failures reported by their producers.
    ///
    /// # Errors
    /// Returns the first concrete loading or validation failure. A successful
    /// legacy producer may still have unknown completeness; cleanup must inspect
    /// the collection outcome rather than treat this result as proof of absence.
    pub async fn try_load_plugin(
        &self,
        id: &PackageId,
    ) -> Result<Vec<Plugin>, PackageManagerError> {
        self.load_plugin_outcome(id).await.into_result()
    }

    pub(crate) async fn load_plugin_outcome(&self, id: &PackageId) -> PluginCollection<Plugin> {
        let mut plugins = Vec::new();
        let mut errors = Vec::new();
        let mut is_complete = true;
        for instance in &self.instances {
            let collection = instance.load_plugin_outcome(id).await;
            is_complete &= collection.is_complete();
            plugins.extend(collection.plugins);
            errors.extend(collection.errors);
        }
        if is_complete && errors.is_empty() {
            PluginCollection::complete(plugins)
        } else {
            PluginCollection::incomplete(plugins, errors)
        }
    }

    /// Search every instance for packages matching `query`, tagged with the
    /// instance's display name. A failing instance is skipped with a debug log
    /// rather than failing the union.
    pub async fn search(&self, query: &str) -> Vec<(String, PluginInfo)> {
        let mut out = Vec::new();
        for inst in self.instances() {
            match inst.pm.try_search(query).await {
                Ok(infos) => out.extend(infos.into_iter().map(|i| (inst.name.clone(), i))),
                Err(e) => {
                    tracing::debug!(instance = %inst.name, error = %e, "search failed, skipping");
                }
            }
        }
        out
    }
}

/// The workspace's dependency set as package ids — every PM's `list_deps`
/// unioned. This is what `depends-on` predicates evaluate against
/// (the internal predicate context). Failures are logged and yield an
/// empty list so predicate evaluation degrades to "no deps" rather than
/// aborting the caller.
/// Legacy best-effort adapter retained through root 0.6. Use
/// [`try_workspace_dep_ids`] before operations that rely on the configured set.
pub async fn workspace_dep_ids(
    sym: &crate::config::Symposium,
    deps: &Arc<WorkspaceDeps>,
) -> Vec<PackageId> {
    match try_workspace_dep_ids(sym, deps).await {
        Ok(ids) => ids,
        Err(_) => {
            tracing::warn!("dependency discovery skipped: invalid registry configuration");
            Vec::new()
        }
    }
}

/// The workspace dependency union, rejecting invalid configured registry sources.
///
/// Package-manager listing failures retain the existing visible best-effort
/// behavior; an invalid source configuration always returns an error.
///
/// # Errors
/// Returns [`crate::config::RegistryError`] when a configured source is invalid.
pub async fn try_workspace_dep_ids(
    sym: &crate::config::Symposium,
    deps: &Arc<WorkspaceDeps>,
) -> Result<Vec<PackageId>, crate::config::RegistryError> {
    match sym.try_package_managers(deps)?.try_list_deps().await {
        Ok(deps) => Ok(deps),
        Err(e) => {
            tracing::warn!(error = %e, "failed to list workspace dependencies");
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_id_display_is_colon_tuple() {
        let id = PackageId::new("cargo", "serde", "1.0.210");
        assert_eq!(id.to_string(), "cargo:serde:1.0.210");
    }

    /// A PM that answers with one plugin whose manifest fails validation.
    struct InvalidPluginPm;

    #[async_trait::async_trait]
    impl PackageManager for InvalidPluginPm {
        fn name(&self) -> &str {
            "stub"
        }
        async fn active_plugins(&self, _deps: &[PackageId]) -> Vec<UnvalidatedPlugin> {
            let manifest = symposium_sdk::manifest::RawPluginManifest::parse(
                "name = \"broken\"\n[[skills]]\n",
            )
            .unwrap();
            vec![UnvalidatedPlugin::new(
                PackageId::any_version("stub", "broken"),
                PathBuf::from("/nonexistent/broken"),
                manifest,
            )]
        }
        async fn load_plugin(&self, _id: &PackageId) -> Vec<UnvalidatedPlugin> {
            Vec::new()
        }
        async fn list_deps(&self) -> Result<Vec<PackageId>> {
            Ok(Vec::new())
        }
        async fn search(&self, _query: &str) -> Result<Vec<PluginInfo>> {
            Ok(Vec::new())
        }
        async fn fetch(&self, id: &PackageId, _update: UpdateLevel) -> Result<FetchedPackage> {
            anyhow::bail!("cannot fetch {id}")
        }
    }

    /// The report events emitted while validating the stub's plugin under `kind`.
    async fn reports_for_invalid_plugin(kind: PluginKind) -> Vec<serde_json::Value> {
        use tracing_subscriber::layer::SubscriberExt;
        symposium_testlib::install_tracing_baseline();
        let (layer, handle) =
            crate::report::ReportLayer::new(crate::report::ReportMode::Json, tracing::Level::DEBUG);
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
        let instance = PmInstance {
            name: "stub".into(),
            trusted: false,
            kind,
            pm: Box::new(InvalidPluginPm),
        };
        assert!(instance.active_plugins(&[]).await.is_empty());
        handle.drain()
    }

    #[tokio::test]
    async fn invalid_registry_plugin_is_reported_with_its_path() {
        let reports = reports_for_invalid_plugin(PluginKind::Registry).await;
        assert_eq!(reports.len(), 1, "{reports:?}");
        let message = reports[0]["message"].as_str().unwrap();
        assert!(message.contains("/nonexistent/broken"), "{message}");
    }

    /// A package's manifest is its author's problem, and the hook path prints
    /// reports next to the agent's JSON, so a package failure is only logged.
    #[tokio::test]
    async fn invalid_package_plugin_is_not_reported() {
        let reports = reports_for_invalid_plugin(PluginKind::Package).await;
        assert!(reports.is_empty(), "{reports:?}");
    }

    #[tokio::test]
    async fn registry_validation_preserves_good_siblings_without_claiming_completeness() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["good", "invalid"] {
            std::fs::create_dir(temp.path().join(name)).unwrap();
        }
        std::fs::write(temp.path().join("good/Symposium.toml"), "name = 'good'").unwrap();
        std::fs::write(
            temp.path().join("invalid/Symposium.toml"),
            "name = 'invalid'\n[[skills]]\n",
        )
        .unwrap();
        let instance = PmInstance {
            name: "local".into(),
            trusted: true,
            kind: PluginKind::Registry,
            pm: Box::new(PathPm::new("local", temp.path())),
        };
        let collection = instance.registry_plugins_outcome().await.unwrap();
        assert_eq!(collection.plugins.len(), 1);
        assert_eq!(collection.plugins[0].manifest.name, "good");
        assert!(!collection.is_complete());
        assert!(matches!(
            collection.errors.as_slice(),
            [PackageManagerError::PluginValidation { .. }]
        ));
        assert_eq!(instance.registry_plugins().await.unwrap().len(), 1);
        assert!(matches!(
            instance.try_registry_plugins().await,
            Err(PackageManagerError::PluginValidation { .. })
        ));
    }

    #[tokio::test]
    async fn validated_legacy_plugin_outcomes_preserve_unknown_completeness() {
        let registry = PmRegistry::new(vec![PmInstance {
            name: "stub".into(),
            trusted: false,
            kind: PluginKind::Package,
            pm: Box::new(InvalidPluginPm),
        }]);
        let collection = registry
            .load_plugin_outcome(&PackageId::any_version("stub", "missing"))
            .await;
        assert!(collection.plugins.is_empty());
        assert!(collection.errors.is_empty());
        assert!(!collection.is_complete());
    }

    #[tokio::test]
    async fn git_registry_without_its_copy_is_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = symposium_install::InstallContext::new(tmp.path().to_path_buf());
        let pm = GitPm::new("reg", "https://github.com/example/reg", false, ctx).unwrap();
        let err = pm.registry_plugins().await.unwrap_err();
        assert!(
            err.to_string().contains("cargo agents plugin sync reg"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn registry_rejects_unknown_pm() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = tmp;
        let id = PackageId::any_version("npm", "leftpad");
        let err = PmRegistry::new(vec![])
            .fetch(&id, UpdateLevel::None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown package manager `npm`"));
    }
}
