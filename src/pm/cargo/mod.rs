//! The cargo package manager: crates from the active workspace's dependency
//! graph, resolved by [`RustCrateFetch`] (path-dependency override, then the
//! cargo registry cache, then crates.io).
//!
//! The [`workspace`] submodule owns the cargo-workspace resolution — the
//! `cargo metadata` invocation, its cache, and the [`WorkspaceCrate`] /
//! [`WorkspaceDeps`] types — since that is cargo's ecosystem, not a generic
//! concern. A [`CargoPm`] *holds* its [`WorkspaceDeps`] resolver (as an
//! [`Arc`], so several instances share one lazily-run, cached `cargo metadata`)
//! and drives it through the checked workspace resolver.

use std::sync::Arc;

use anyhow::Result;
use symposium_install::UpdateLevel;

use crate::crate_sources::RustCrateFetch;

pub mod workspace;
pub use workspace::{
    LoadedWorkspace, WorkspaceCrate, WorkspaceDeps, WorkspaceError, file_mtime, try_file_mtime,
    try_find_workspace_root, workspace_dir_name,
};

use super::{
    ANY_VERSION, CARGO_PM, FetchedPackage, PackageId, PackageManager, PackageManagerError,
    PluginCollection, PluginInfo, UnvalidatedPlugin,
};

/// How many crates.io hits a search returns — enough to surface the crate a
/// user is looking for without flooding the report.
const SEARCH_PAGE_SIZE: u64 = 10;

/// The cargo transport, bound to one workspace's [`WorkspaceDeps`] resolver.
///
/// Holds the resolver as an [`Arc`] so the transport in a [`PmRegistry`] and any
/// ad-hoc [`CargoPm`] built for crate loading share one lazily-run, cached
/// `cargo metadata` — the in-process stand-in for a per-workspace PM process.
pub struct CargoPm {
    workspace: Arc<WorkspaceDeps>,
}

impl CargoPm {
    /// A transport resolving against `workspace`.
    pub fn new(workspace: Arc<WorkspaceDeps>) -> Self {
        Self { workspace }
    }

    /// Cargo id for a crate name and optional version requirement.
    pub fn id_for(name: &str, version: Option<&str>) -> PackageId {
        PackageId::new(CARGO_PM, name, version.unwrap_or(ANY_VERSION))
    }

    /// Build the [`UnvalidatedPlugin`] for an already-fetched crate, layering its
    /// manifest sources: `[package.metadata.symposium]` in `Cargo.toml` and a
    /// `Symposium.toml` at the root: via
    /// [`merge_crate_manifest_outcome`](crate::plugins::merge_crate_manifest_outcome).
    ///
    /// A crate with no manifest sources still yields a plugin: an empty
    /// manifest, which validation turns into a plugin whose only content is the
    /// default `skills/` group.
    /// Invalid layers are reported independently while valid layers survive.
    fn plugin_from_fetched(&self, fetched: FetchedPackage) -> PluginCollection<UnvalidatedPlugin> {
        let source = match std::fs::metadata(&fetched.root) {
            Ok(metadata) if metadata.is_dir() => None,
            Ok(_) => Some(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "package source must be a directory",
            )),
            Err(source) => Some(source),
        };
        if let Some(source) = source {
            return PluginCollection::incomplete(
                Vec::new(),
                vec![PackageManagerError::PackageSource {
                    path: fetched.root,
                    source,
                }],
            );
        }
        let name = &fetched.id.name;
        let mut errors = Vec::new();
        let metadata =
            match crate::crate_metadata::symposium_metadata(&fetched.root.join("Cargo.toml")) {
                Ok(metadata) => metadata,
                // Source-only fixtures and non-Cargo packages can still embed skills.
                Err(crate::crate_metadata::MetadataError::ReadManifest { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    None
                }
                Err(source) => {
                    errors.push(PackageManagerError::PluginMetadata {
                        id: fetched.id.clone(),
                        source: Box::new(source),
                    });
                    None
                }
            };

        let manifest_path = fetched.root.join(crate::pm::layout::MANIFEST_FILE);
        let file = match std::fs::read_to_string(&manifest_path) {
            Ok(content) => Some(content),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                errors.push(PackageManagerError::PluginLoad {
                    id: fetched.id.clone(),
                    source: Box::new(crate::plugins::PluginLoadError::Read {
                        path: manifest_path,
                        source,
                    }),
                });
                None
            }
        };

        let (manifest, layer_errors) = crate::plugins::merge_crate_manifest_outcome(
            metadata,
            file.as_deref(),
            name,
            &fetched.root,
        );
        errors.extend(
            layer_errors
                .into_iter()
                .map(|source| PackageManagerError::PluginLoad {
                    id: fetched.id.clone(),
                    source: Box::new(source),
                }),
        );
        let plugins = vec![UnvalidatedPlugin::new(fetched.id, fetched.root, manifest)];
        if errors.is_empty() {
            PluginCollection::complete(plugins)
        } else {
            PluginCollection::incomplete(plugins, errors)
        }
    }
}

/// What plugin content a crate source tree at `dir` embeds, as a short
/// human-readable phrase — or `None` when it embeds none. Mirrors what
/// [`CargoPm::load_plugin`] would build a plugin from: a `Symposium.toml`,
/// `[package.metadata.symposium]`, or the default `skills/` directory.
fn embedded_plugin_kind(dir: &std::path::Path) -> Option<&'static str> {
    if dir.join(crate::pm::layout::MANIFEST_FILE).is_file() {
        return Some("plugin manifest (Symposium.toml)");
    }
    match crate::crate_metadata::symposium_metadata(&dir.join("Cargo.toml")) {
        Ok(Some(_)) => return Some("embedded plugin ([package.metadata.symposium])"),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(error = %error, "failed to inspect crate plugin metadata");
        }
    }
    contains_skill_md(&dir.join(crate::plugins::CRATE_DEFAULT_SKILLS_PATH))
        .then_some("embedded skills (skills/)")
}

/// Is there a `SKILL.md` anywhere under `dir`?
fn contains_skill_md(dir: &std::path::Path) -> bool {
    let mut ancestors = crate::dir_walk::Ancestors::rooted_at(dir);
    contains_skill_md_in(dir, &mut ancestors)
}

fn contains_skill_md_in(dir: &std::path::Path, ancestors: &mut crate::dir_walk::Ancestors) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) => {
            if source.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %dir.display(), error_kind = ?source.kind(), "failed to inspect embedded skills");
            }
            return false;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                tracing::warn!(path = %dir.display(), error_kind = ?source.kind(), "failed to read embedded skill directory entry");
                continue;
            }
        };
        let path = entry.path();
        // `is_dir` follows symlinks, so a linked directory is searched — and
        // can name a directory this walk is already inside.
        if path.is_dir() {
            if !ancestors.enter(&path) {
                continue;
            }
            let found = contains_skill_md_in(&path, ancestors);
            ancestors.leave();
            if found {
                return true;
            }
        } else if path.file_name().is_some_and(|f| f == "SKILL.md") {
            return true;
        }
    }
    false
}

#[async_trait::async_trait]
impl PackageManager for CargoPm {
    fn name(&self) -> &str {
        CARGO_PM
    }

    /// The plugins embedded in the workspace's dependencies: every dep in
    /// `deps` whose source tree embeds plugin content, built into a full
    /// `Plugin`. Whether each is *trusted* (may activate without consent)
    /// is the caller's decision — the cargo transport is marked untrusted.
    ///
    /// Each dependency is fetched cache-only ([`UpdateLevel::None`]) to locate
    /// its source, then inspected. A workspace dependency resolves into the
    /// source `cargo metadata` already extracted — no probe, no network — so
    /// registry dependencies are surfaced exactly like path ones. A dependency
    /// whose source can't be served from cache is skipped.
    async fn active_plugins(&self, deps: &[PackageId]) -> Vec<UnvalidatedPlugin> {
        let mut out = Vec::new();
        for id in deps.iter().filter(|id| id.pm == CARGO_PM) {
            // Fetch by name only: the concrete version in `id` would make
            // `fetch` treat it as an explicit `--version` and probe, bypassing
            // the workspace-source shortcut.
            let fetched = match self
                .try_fetch(&Self::id_for(&id.name, None), UpdateLevel::None)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!(error = %e, "cannot serve dependency source from cache; skipping");
                    continue;
                }
            };
            // Only surface dependencies that actually embed plugin content.
            if embedded_plugin_kind(&fetched.root).is_some() {
                let outcome = self.plugin_from_fetched(fetched);
                for error in outcome.errors {
                    tracing::warn!(%error, "crate plugin loading was incomplete");
                }
                out.extend(outcome.plugins);
            }
        }
        out
    }

    /// Resolve a specific crate id to its plugin (a chained reference or an
    /// enabled crate). Unlike `active_plugins`, this loads the named crate
    /// whatever it embeds — any fetchable crate yields at least the default
    /// `skills/` plugin. Fetched cache-only.
    async fn load_plugin(&self, id: &PackageId) -> Vec<UnvalidatedPlugin> {
        let outcome = self.load_plugin_outcome(id).await;
        for error in outcome.errors {
            tracing::warn!(%error, "crate plugin loading was incomplete");
        }
        outcome.plugins
    }

    async fn try_load_plugin(
        &self,
        id: &PackageId,
    ) -> std::result::Result<Vec<UnvalidatedPlugin>, PackageManagerError> {
        self.load_plugin_outcome(id).await.into_result()
    }

    async fn load_plugin_outcome(&self, id: &PackageId) -> PluginCollection<UnvalidatedPlugin> {
        if id.pm != CARGO_PM {
            return PluginCollection::complete(Vec::new());
        }
        let fetched = match self.try_fetch(id, UpdateLevel::None).await {
            Ok(f) => f,
            Err(error) => {
                return PluginCollection::incomplete(Vec::new(), vec![error]);
            }
        };
        self.plugin_from_fetched(fetched)
    }

    /// Search crates.io for crates matching `query`.
    ///
    /// Name-based, like `cargo search`: the results are *candidate* crates —
    /// whether one actually carries plugin content is only known once it is
    /// fetched (any fetchable crate yields at least a default `skills/` plugin).
    /// So this lets `cargo agents use <crate>` name a crate the workspace
    /// doesn't depend on; the fetch/load step decides what it contributes.
    async fn search(&self, query: &str) -> Result<Vec<PluginInfo>> {
        Ok(self.try_search(query).await?)
    }

    async fn try_search(
        &self,
        query: &str,
    ) -> std::result::Result<Vec<PluginInfo>, PackageManagerError> {
        let http = reqwest::Client::builder()
            .user_agent("symposium (https://github.com/symposium-dev/symposium)")
            .build()
            .map_err(|source| PackageManagerError::Client { source })?;
        let client = crates_io_api::AsyncClient::with_http_client(
            http,
            std::time::Duration::from_millis(1000),
        );
        let cq = crates_io_api::CratesQuery::builder()
            .search(query)
            .page_size(SEARCH_PAGE_SIZE)
            .build();
        let page = client
            .crates(cq)
            .await
            .map_err(|source| PackageManagerError::SearchCrates { source })?;
        Ok(page
            .crates
            .into_iter()
            .map(|c| PluginInfo {
                id: PackageId::new(CARGO_PM, c.name, c.max_version),
                description: c.description,
            })
            .collect())
    }

    async fn fetch(&self, id: &PackageId, update: UpdateLevel) -> Result<FetchedPackage> {
        Ok(self.try_fetch(id, update).await?)
    }

    async fn try_fetch(
        &self,
        id: &PackageId,
        _update: UpdateLevel,
    ) -> std::result::Result<FetchedPackage, PackageManagerError> {
        if id.pm != CARGO_PM {
            return Err(PackageManagerError::WrongManager {
                expected: CARGO_PM.to_owned(),
                actual: id.pm.clone(),
            });
        }
        let mut fetch = RustCrateFetch::new(&id.name, self.workspace.as_ref());
        if id.version != ANY_VERSION {
            fetch = fetch.version(&id.version);
        }
        let result = fetch
            .fetch()
            .await
            .map_err(|source| PackageManagerError::FetchCrate {
                id: id.clone(),
                source: Box::new(source),
            })?;
        Ok(FetchedPackage {
            id: PackageId::new(CARGO_PM, result.name, result.version),
            root: result.path,
        })
    }

    async fn list_deps(&self) -> Result<Vec<PackageId>> {
        Ok(self.try_list_deps().await?)
    }

    async fn try_list_deps(&self) -> std::result::Result<Vec<PackageId>, PackageManagerError> {
        Ok(self
            .workspace
            .try_crates()
            .map_err(|source| PackageManagerError::Workspace { source })?
            .iter()
            .map(|c| PackageId::new(CARGO_PM, c.name.clone(), c.version.to_string()))
            .collect())
    }

    async fn registry_plugins(&self) -> Result<Vec<UnvalidatedPlugin>> {
        Ok(self.try_registry_plugins().await?)
    }

    async fn try_registry_plugins(
        &self,
    ) -> std::result::Result<Vec<UnvalidatedPlugin>, PackageManagerError> {
        self.registry_plugins_outcome().await?.into_result()
    }

    async fn registry_plugins_outcome(
        &self,
    ) -> std::result::Result<PluginCollection<UnvalidatedPlugin>, PackageManagerError> {
        Ok(PluginCollection::complete(Vec::new()))
    }

    async fn refresh(&self, update: UpdateLevel, force: bool) -> Result<bool> {
        Ok(self.try_refresh(update, force).await?)
    }

    async fn try_refresh(
        &self,
        _update: UpdateLevel,
        _force: bool,
    ) -> std::result::Result<bool, PackageManagerError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pm::WorkspaceCrate;
    use std::path::PathBuf;

    /// A path dependency: `source_dir` defaults to its local path.
    fn path_dep(name: &str, dir: PathBuf) -> WorkspaceCrate {
        WorkspaceCrate::new(name.to_string(), semver::Version::new(1, 0, 0), Some(dir))
    }

    /// A registry dependency whose extracted source `cargo metadata` located —
    /// no local `path`, but a known `source_dir` (as populated in production).
    fn registry_dep(name: &str, source_dir: PathBuf) -> WorkspaceCrate {
        WorkspaceCrate::new(name.to_string(), semver::Version::new(1, 0, 0), None)
            .with_source_dir(Some(source_dir))
    }

    #[test]
    fn missing_or_non_directory_package_sources_are_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        let file = tmp.path().join("file");
        std::fs::write(&file, "preserved source bytes").unwrap();
        let pm = CargoPm::new(Arc::new(WorkspaceDeps::detached()));

        for (root, expected_kind) in [
            (missing, std::io::ErrorKind::NotFound),
            (file.clone(), std::io::ErrorKind::NotADirectory),
        ] {
            let outcome = pm.plugin_from_fetched(FetchedPackage {
                id: CargoPm::id_for("example", None),
                root: root.clone(),
            });
            assert!(!outcome.is_complete);
            assert!(outcome.plugins.is_empty());
            assert!(matches!(
                outcome.errors.as_slice(),
                [PackageManagerError::PackageSource { path, source }]
                    if *path == root && source.kind() == expected_kind
            ));
        }
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "preserved source bytes"
        );
    }

    #[test]
    fn malformed_manifest_layer_preserves_the_other_layer_and_source() {
        for (cargo, symposium, expected_name, failed_manifest) in [
            (
                "[package.metadata.symposium]\nname = 'from-metadata'\n",
                "name = [PRIVATE_CONTENT_CANARY",
                "from-metadata",
                "Symposium.toml",
            ),
            (
                "[package.metadata.symposium]\nname = 42\n",
                "name = 'from-file'\n",
                "from-file",
                "Cargo.toml",
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("Cargo.toml"), cargo).unwrap();
            std::fs::write(tmp.path().join("Symposium.toml"), symposium).unwrap();
            let pm = CargoPm::new(Arc::new(WorkspaceDeps::detached()));

            let outcome = pm.plugin_from_fetched(FetchedPackage {
                id: CargoPm::id_for("example", None),
                root: tmp.path().into(),
            });

            assert!(!outcome.is_complete);
            assert_eq!(outcome.plugins.len(), 1);
            assert_eq!(
                outcome.plugins[0].manifest.name.as_deref(),
                Some(expected_name)
            );
            assert_eq!(outcome.errors.len(), 1);
            assert!(matches!(
                &outcome.errors[0],
                PackageManagerError::PluginLoad { source, .. }
                    if matches!(source.as_ref(), crate::plugins::PluginLoadError::Parse { path, source }
                        if *path == tmp.path().join(failed_manifest) && !source.message().is_empty())
                        && std::error::Error::source(&outcome.errors[0]).is_some()
            ));
            assert!(
                !outcome.errors[0]
                    .to_string()
                    .contains("PRIVATE_CONTENT_CANARY")
            );
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap(),
                cargo
            );
            assert_eq!(
                std::fs::read_to_string(tmp.path().join("Symposium.toml")).unwrap(),
                symposium
            );
        }
    }

    #[test]
    fn malformed_cargo_manifest_preserves_file_layer_and_metadata_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[PRIVATE_CONTENT_CANARY").unwrap();
        std::fs::write(tmp.path().join("Symposium.toml"), "name = 'from-file'").unwrap();
        let pm = CargoPm::new(Arc::new(WorkspaceDeps::detached()));

        let outcome = pm.plugin_from_fetched(FetchedPackage {
            id: CargoPm::id_for("example", None),
            root: tmp.path().into(),
        });

        assert!(!outcome.is_complete);
        assert_eq!(
            outcome.plugins[0].manifest.name.as_deref(),
            Some("from-file")
        );
        assert!(matches!(
            outcome.errors.as_slice(),
            [PackageManagerError::PluginMetadata { source, .. }]
                if matches!(source.as_ref(), crate::crate_metadata::MetadataError::ParseManifest { path: Some(path), .. }
                    if *path == tmp.path().join("Cargo.toml"))
        ));
        assert!(
            !outcome.errors[0]
                .to_string()
                .contains("PRIVATE_CONTENT_CANARY")
        );
    }

    #[test]
    fn unreadable_manifest_is_incomplete_instead_of_optional_absence() {
        for failed_manifest in ["Cargo.toml", "Symposium.toml"] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir(tmp.path().join(failed_manifest)).unwrap();
            let pm = CargoPm::new(Arc::new(WorkspaceDeps::detached()));

            let outcome = pm.plugin_from_fetched(FetchedPackage {
                id: CargoPm::id_for("example", None),
                root: tmp.path().into(),
            });

            assert!(!outcome.is_complete);
            assert_eq!(outcome.plugins.len(), 1);
            assert_eq!(outcome.errors.len(), 1);
            match &outcome.errors[0] {
                PackageManagerError::PluginMetadata { source, .. } => {
                    let crate::crate_metadata::MetadataError::ReadManifest { path, source } =
                        source.as_ref()
                    else {
                        panic!("expected manifest read failure");
                    };
                    assert_eq!(path, &tmp.path().join("Cargo.toml"));
                    assert_ne!(source.kind(), std::io::ErrorKind::NotFound);
                }
                PackageManagerError::PluginLoad { source, .. } => {
                    let crate::plugins::PluginLoadError::Read { path, source } = source.as_ref()
                    else {
                        panic!("expected plugin manifest read failure");
                    };
                    assert_eq!(path, &tmp.path().join("Symposium.toml"));
                    assert_ne!(source.kind(), std::io::ErrorKind::NotFound);
                }
                error => panic!("unexpected failure category: {error}"),
            }
        }
    }

    #[test]
    fn optional_manifests_and_valid_layering_are_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let pm = CargoPm::new(Arc::new(WorkspaceDeps::detached()));
        let empty = pm.plugin_from_fetched(FetchedPackage {
            id: CargoPm::id_for("example", None),
            root: tmp.path().into(),
        });
        assert!(empty.is_complete);
        assert!(empty.errors.is_empty());
        assert_eq!(empty.plugins.len(), 1);
        assert!(empty.plugins[0].manifest.name.is_none());

        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package.metadata.symposium]\nname = 'base'\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("Symposium.toml"), "name = 'over'").unwrap();
        let layered = pm.plugin_from_fetched(FetchedPackage {
            id: CargoPm::id_for("example", None),
            root: tmp.path().into(),
        });
        assert!(layered.is_complete);
        assert!(layered.errors.is_empty());
        assert_eq!(layered.plugins[0].manifest.name.as_deref(), Some("over"));
    }

    #[tokio::test]
    async fn strict_loading_rejects_partial_content_while_legacy_and_active_loading_keep_it() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package.metadata.symposium]\nname = 42\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("Symposium.toml"), "name = 'from-file'").unwrap();
        let pm = CargoPm::new(WorkspaceDeps::fixture(
            tmp.path().to_path_buf(),
            vec![path_dep("example", tmp.path().to_path_buf())],
        ));
        let id = CargoPm::id_for("example", None);

        let outcome = pm.load_plugin_outcome(&id).await;
        assert!(!outcome.is_complete);
        assert_eq!(
            outcome.plugins[0].manifest.name.as_deref(),
            Some("from-file")
        );
        assert!(matches!(
            pm.try_load_plugin(&id).await,
            Err(PackageManagerError::PluginLoad { .. })
        ));
        let legacy = pm.load_plugin(&id).await;
        assert_eq!(legacy[0].manifest.name.as_deref(), Some("from-file"));
        let active = pm.active_plugins(&[id]).await;
        assert_eq!(active[0].manifest.name.as_deref(), Some("from-file"));
    }

    #[tokio::test]
    async fn foreign_ids_and_cargo_registry_are_complete_empty_results() {
        let pm = CargoPm::new(Arc::new(WorkspaceDeps::detached()));
        let foreign = PackageId::new("other", "PRIVATE_CONTENT_CANARY", ANY_VERSION);
        let outcome = pm.load_plugin_outcome(&foreign).await;
        assert!(outcome.is_complete);
        assert!(outcome.plugins.is_empty());
        assert!(outcome.errors.is_empty());
        assert!(pm.try_load_plugin(&foreign).await.unwrap().is_empty());
        let registry = pm.registry_plugins_outcome().await.unwrap();
        assert!(registry.is_complete);
        assert!(registry.plugins.is_empty());
        assert!(registry.errors.is_empty());
    }

    #[tokio::test]
    async fn offers_dependencies_whose_sources_embed_plugin_content() {
        let tmp = tempfile::tempdir().unwrap();

        let with_skills = tmp.path().join("with-skills");
        std::fs::create_dir_all(with_skills.join("skills/guidance")).unwrap();
        std::fs::write(with_skills.join("skills/guidance/SKILL.md"), "").unwrap();

        // A *registry* dependency (no path) with an extracted source that
        // embeds a manifest — surfaced now that `active_plugins` fetches.
        let registry_embedded = tmp.path().join("registry-embedded");
        std::fs::create_dir_all(&registry_embedded).unwrap();
        std::fs::write(registry_embedded.join("Symposium.toml"), "").unwrap();

        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(plain.join("src")).unwrap();

        let crates = vec![
            path_dep("with-skills", with_skills),
            registry_dep("registry-embedded", registry_embedded),
            path_dep("plain", plain),
        ];
        let deps: Vec<PackageId> = crates
            .iter()
            .map(|c| PackageId::new(CARGO_PM, &c.name, c.version.to_string()))
            .collect();
        let pm = CargoPm::new(crate::pm::WorkspaceDeps::fixture(
            tmp.path().to_path_buf(),
            crates,
        ));

        let active = pm.active_plugins(&deps).await;
        let got: Vec<&str> = active.iter().map(|o| o.id.name.as_str()).collect();
        // `plain` embeds nothing, so it is not surfaced.
        assert_eq!(got, vec!["with-skills", "registry-embedded"]);
        assert!(active.iter().all(|o| o.id.pm == CARGO_PM));
        // The plugin points at the crate source; the default `skills/` group is
        // added by validation, not by the PM.
        assert!(active[0].root.join("skills").is_dir());
    }

    #[tokio::test]
    async fn typed_fetch_rejects_wrong_manager_before_source_resolution() {
        let pm = CargoPm::new(Arc::new(WorkspaceDeps::detached()));
        let id = PackageId::new("other", "irrelevant", ANY_VERSION);

        assert!(matches!(
            pm.try_fetch(&id, UpdateLevel::None).await,
            Err(PackageManagerError::WrongManager { expected, actual })
                if expected == CARGO_PM && actual == "other"
        ));
        let legacy = pm.fetch(&id, UpdateLevel::None).await.unwrap_err();
        assert!(legacy.downcast_ref::<PackageManagerError>().is_some());
    }

    #[tokio::test]
    async fn typed_fetch_uses_workspace_source_without_a_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("crate-source");
        let pm = CargoPm::new(WorkspaceDeps::fixture(
            tmp.path().to_path_buf(),
            vec![path_dep("local_crate", source.clone())],
        ));

        let fetched = pm
            .try_fetch(&CargoPm::id_for("local-crate", None), UpdateLevel::None)
            .await
            .unwrap();
        assert_eq!(fetched.id, PackageId::new(CARGO_PM, "local_crate", "1.0.0"));
        assert_eq!(fetched.root, source);
        assert_eq!(
            pm.try_list_deps().await.unwrap(),
            vec![PackageId::new(CARGO_PM, "local_crate", "1.0.0")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn embedded_skill_scan_preserves_cycle_guard_and_finds_nested_content() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("group/nested")).unwrap();
        std::fs::write(tmp.path().join("group/nested/SKILL.md"), "").unwrap();
        std::os::unix::fs::symlink("..", tmp.path().join("group/back")).unwrap();

        assert!(contains_skill_md(tmp.path()));
        assert!(!contains_skill_md(&tmp.path().join("missing")));
    }
}
