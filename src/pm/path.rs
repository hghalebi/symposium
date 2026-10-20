//! The path package manager: a registry instance fronting one local
//! directory.
//!
//! This is the ordinary plugin-source case — `~/.symposium/plugins/`, a
//! `[[registry]]` entry with a `path`, or the git cache directory a
//! [`GitPm`](crate::pm::GitPm) fetched into (a `GitPm` delegates its reads to
//! an inner `PathPm` over that cache dir, so once the content is on disk a git
//! registry is just a directory).
//!
//! Ids look like `(<registry name>, <entry subpath>, *)`: the `pm` component
//! is the configured registry's name — the same name plugins from it are
//! attributed to — and the name component locates the entry within the
//! source. The instance resolves its own ids against its directory.

use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use symposium_install::UpdateLevel;

use super::{
    ANY_VERSION, FetchedPackage, PackageId, PackageManager, PackageManagerError, PluginCollection,
    PluginInfo, RegistrySource, UnvalidatedPlugin, layout,
};
use crate::report::ReportEvent;

/// A configured path registry: one local directory whose tree is a
/// collection of plugin entries.
pub struct PathPm {
    name: String,
    pub(super) dir: PathBuf,
}

impl PathPm {
    /// An instance named `name` fronting the registry in `dir`.
    pub fn new(name: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            dir: dir.into(),
        }
    }

    fn registry_package_path(
        &self,
        id: &PackageId,
    ) -> std::result::Result<PathBuf, PackageManagerError> {
        if id.pm != self.name {
            return Err(PackageManagerError::WrongManager {
                expected: self.name.clone(),
                actual: id.pm.clone(),
            });
        }
        let path = Path::new(&id.name);
        if id.name.is_empty()
            || path.components().any(|component| match component {
                Component::Normal(_) | Component::CurDir => false,
                Component::RootDir | Component::Prefix(_) | Component::ParentDir => true,
            })
        {
            return Err(PackageManagerError::InvalidPackagePath { id: id.clone() });
        }
        Ok(self.dir.join(path))
    }

    fn registry_entries(
        &self,
    ) -> std::result::Result<Vec<layout::RegistryEntry>, PackageManagerError> {
        layout::try_enumerate_existing(&self.dir).map_err(|error| match error {
            layout::LayoutError::ReadDirectory { path, source } if path == self.dir => {
                PackageManagerError::ReadRegistry {
                    name: self.name.clone(),
                    path,
                    source,
                }
            }
            source @ (layout::LayoutError::RootIsPlugin { .. }
            | layout::LayoutError::RootIsSkill { .. }
            | layout::LayoutError::ReadDirectory { .. }
            | layout::LayoutError::ReadEntry { .. }) => PackageManagerError::RegistryLayout {
                name: self.name.clone(),
                source,
            },
        })
    }
}

#[async_trait::async_trait]
impl PackageManager for PathPm {
    fn name(&self) -> &str {
        &self.name
    }

    /// Every entry in the registry, loaded as a plugin. `deps` is ignored — a
    /// local registry's contents don't vary with the workspace; whether each
    /// plugin applies is decided later by its own predicates. A registry is a
    /// trust root, so these activate without consent.
    async fn active_plugins(&self, _deps: &[PackageId]) -> Vec<UnvalidatedPlugin> {
        match self.registry_plugins_outcome().await {
            Ok(collection) => {
                for error in &collection.errors {
                    tracing::warn!(report = %ReportEvent::Warning { message: error.to_string() });
                }
                collection.plugins
            }
            Err(error) => {
                tracing::warn!(error = %error, "cannot list registry");
                Vec::new()
            }
        }
    }

    async fn registry_plugins(&self) -> Result<Vec<UnvalidatedPlugin>> {
        let collection = self.registry_plugins_outcome().await?;
        for error in &collection.errors {
            tracing::warn!(report = %ReportEvent::Warning { message: error.to_string() });
        }
        Ok(collection.plugins)
    }

    async fn try_registry_plugins(
        &self,
    ) -> std::result::Result<Vec<UnvalidatedPlugin>, PackageManagerError> {
        self.registry_plugins_outcome().await?.into_result()
    }

    async fn registry_plugins_outcome(
        &self,
    ) -> std::result::Result<PluginCollection<UnvalidatedPlugin>, PackageManagerError> {
        let entries = self.registry_entries()?;
        let mut out = Vec::new();
        let mut errors = Vec::new();
        for entry in entries {
            let id = PackageId::new(&self.name, layout::subpath_key(&entry.subpath), ANY_VERSION);
            match crate::plugins::entry_plugin(&self.dir, &entry.subpath, &self.name) {
                Some(Ok(p)) => out.push(p),
                Some(Err(source)) => errors.push(PackageManagerError::PluginLoad {
                    id,
                    source: Box::new(source),
                }),
                None => errors.push(PackageManagerError::PluginLoad {
                    id,
                    source: Box::new(crate::plugins::PluginLoadError::UnexpectedKind {
                        path: self.dir.join(&entry.subpath),
                        expected: "a discovered plugin or standalone skill",
                    }),
                }),
            }
        }
        Ok(if errors.is_empty() {
            PluginCollection::complete(out)
        } else {
            PluginCollection::incomplete(out, errors)
        })
    }

    /// The entry an id names (`id.name` is the entry's subpath key).
    async fn load_plugin(&self, id: &PackageId) -> Vec<UnvalidatedPlugin> {
        let collection = self.load_plugin_outcome(id).await;
        for error in &collection.errors {
            tracing::warn!(registry = %self.name, error = %error, "cannot load registry package");
        }
        collection.plugins
    }

    async fn try_load_plugin(
        &self,
        id: &PackageId,
    ) -> std::result::Result<Vec<UnvalidatedPlugin>, PackageManagerError> {
        self.load_plugin_outcome(id).await.into_result()
    }

    async fn load_plugin_outcome(&self, id: &PackageId) -> PluginCollection<UnvalidatedPlugin> {
        if id.pm != self.name {
            return PluginCollection::complete(Vec::new());
        }
        if let Err(error) = self.registry_package_path(id) {
            return PluginCollection::incomplete(Vec::new(), vec![error]);
        }
        if let Err(source) = std::fs::metadata(&self.dir) {
            return PluginCollection::incomplete(
                Vec::new(),
                vec![PackageManagerError::ReadRegistry {
                    name: self.name.clone(),
                    path: self.dir.clone(),
                    source,
                }],
            );
        }
        match crate::plugins::entry_plugin(&self.dir, Path::new(&id.name), &self.name) {
            Some(Ok(p)) => PluginCollection::complete(vec![p]),
            Some(Err(source)) => PluginCollection::incomplete(
                Vec::new(),
                vec![PackageManagerError::PluginLoad {
                    id: id.clone(),
                    source: Box::new(source),
                }],
            ),
            None => PluginCollection::complete(Vec::new()),
        }
    }

    /// A local directory contributes no workspace dependencies.
    async fn list_deps(&self) -> Result<Vec<PackageId>> {
        Ok(self.try_list_deps().await?)
    }

    async fn try_list_deps(&self) -> std::result::Result<Vec<PackageId>, PackageManagerError> {
        Ok(Vec::new())
    }

    /// Substring match over the entries' subpath keys. Manifest names are the
    /// plugin layer's to interpret, so this only sees directory names.
    async fn search(&self, query: &str) -> Result<Vec<PluginInfo>> {
        Ok(self.try_search(query).await?)
    }

    async fn try_search(
        &self,
        query: &str,
    ) -> std::result::Result<Vec<PluginInfo>, PackageManagerError> {
        let entries = self.registry_entries()?;
        Ok(entries
            .into_iter()
            .map(|entry| layout::subpath_key(&entry.subpath))
            .filter(|key| key.contains(query))
            .map(|key| PluginInfo::from_id(PackageId::new(&self.name, key, ANY_VERSION)))
            .collect())
    }

    /// The entry directory an id names — path entries are their own cache.
    async fn fetch(&self, id: &PackageId, update: UpdateLevel) -> Result<FetchedPackage> {
        Ok(self.try_fetch(id, update).await?)
    }

    async fn try_fetch(
        &self,
        id: &PackageId,
        _update: UpdateLevel,
    ) -> std::result::Result<FetchedPackage, PackageManagerError> {
        Ok(FetchedPackage {
            id: id.clone(),
            root: self.registry_package_path(id)?,
        })
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

    fn registry_source(&self) -> Option<RegistrySource> {
        Some(RegistrySource::Path {
            dir: self.dir.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loads_one_plugin_per_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("registry");
        std::fs::create_dir_all(source.join("tools")).unwrap();
        std::fs::write(source.join("tools/Symposium.toml"), "name = \"tools\"").unwrap();
        std::fs::create_dir_all(source.join("nested/style")).unwrap();
        std::fs::write(
            source.join("nested/style/SKILL.md"),
            "---\nname: style\ndescription: d\ndepends-on: serde\n---\nbody",
        )
        .unwrap();

        let pm = PathPm::new("user-plugins", &source);
        let active = pm.active_plugins(&[]).await;
        let mut names: Vec<&str> = active
            .iter()
            .map(|o| o.manifest.name.as_deref().unwrap_or_default())
            .collect();
        names.sort();
        assert_eq!(names, vec!["style", "tools"]);
        assert!(active.iter().all(|o| o.id.pm == "user-plugins"));

        // A bare SKILL.md is synthesized into a manifest with one group.
        let style = active
            .iter()
            .find(|o| o.manifest.name.as_deref() == Some("style"))
            .expect("the bare skill was offered");
        assert_eq!(style.manifest.skills.len(), 1);

        // load_plugin by the entry's subpath key.
        let one = pm
            .load_plugin(&PackageId::new("user-plugins", "tools", ANY_VERSION))
            .await;
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].manifest.name.as_deref(), Some("tools"));

        let hits = pm.search("too").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.name, "tools");
    }

    #[tokio::test]
    async fn fetch_returns_the_local_entry_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let pm = PathPm::new("local", tmp.path().join("registry"));
        let id = PackageId::new("local", "tools", ANY_VERSION);
        let fetched = pm.fetch(&id, UpdateLevel::None).await.unwrap();
        assert_eq!(fetched.root, tmp.path().join("registry/tools"));
        assert_eq!(fetched.id, id);
    }

    #[tokio::test]
    async fn inaccessible_registry_is_not_an_empty_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("missing");
        let pm = PathPm::new("local", &source);

        let error = pm.try_registry_plugins().await.err().unwrap();
        assert!(matches!(
            &error,
            PackageManagerError::ReadRegistry { name, path, source: error }
                if name == "local" && path == &source && error.kind() == std::io::ErrorKind::NotFound
        ));
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .is_some()
        );
        assert!(matches!(
            pm.try_search("").await,
            Err(PackageManagerError::ReadRegistry { .. })
        ));
        assert!(
            pm.registry_plugins()
                .await
                .unwrap_err()
                .downcast_ref::<PackageManagerError>()
                .is_some()
        );

        std::fs::create_dir(&source).unwrap();
        assert!(pm.try_registry_plugins().await.unwrap().is_empty());
        assert!(pm.try_search("").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_registry_layout_preserves_its_typed_source() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Symposium.toml"), "name = 'root'").unwrap();
        let pm = PathPm::new("local", tmp.path());

        let error = pm.try_registry_plugins().await.err().unwrap();
        assert!(matches!(error, PackageManagerError::RegistryLayout { .. }));
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<layout::LayoutError>()
                .is_some()
        );
    }

    #[tokio::test]
    async fn fetch_rejects_other_managers_but_preserves_nested_subpaths() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("group/tools")).unwrap();
        std::fs::write(
            tmp.path().join("group/tools/Symposium.toml"),
            "name = 'tools'",
        )
        .unwrap();
        let pm = PathPm::new("local", tmp.path());
        let wrong = PackageId::new("other", "tools", ANY_VERSION);
        assert!(matches!(
            pm.try_fetch(&wrong, UpdateLevel::None).await,
            Err(PackageManagerError::WrongManager { expected, actual })
                if expected == "local" && actual == "other"
        ));
        let nested = PackageId::new("local", "group/tools", ANY_VERSION);
        let fetched = pm.try_fetch(&nested, UpdateLevel::None).await.unwrap();
        assert_eq!(fetched.id, nested);
        assert_eq!(fetched.root, tmp.path().join("group/tools"));
        assert_eq!(pm.load_plugin(&nested).await.len(), 1);
        let dotted = PackageId::new("local", "./group/tools", ANY_VERSION);
        assert_eq!(pm.load_plugin(&dotted).await.len(), 1);
        assert_eq!(
            pm.try_fetch(&dotted, UpdateLevel::None).await.unwrap().root,
            tmp.path().join("./group/tools")
        );
    }

    #[tokio::test]
    async fn package_ids_cannot_escape_the_registry_source() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = tmp.path().join("registry");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&registry).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let manifest = outside.join("Symposium.toml");
        let original = b"name = 'outside'";
        std::fs::write(&manifest, original).unwrap();
        let pm = PathPm::new("local", &registry);

        for name in [
            "".to_owned(),
            "../outside".to_owned(),
            outside.to_string_lossy().into_owned(),
        ] {
            let id = PackageId::new("local", name, ANY_VERSION);
            let error = pm.try_fetch(&id, UpdateLevel::None).await.unwrap_err();
            assert!(matches!(
                error,
                PackageManagerError::InvalidPackagePath { .. }
            ));
            assert!(pm.load_plugin(&id).await.is_empty());
            assert!(
                pm.fetch(&id, UpdateLevel::None)
                    .await
                    .unwrap_err()
                    .downcast_ref::<PackageManagerError>()
                    .is_some()
            );
        }
        assert_eq!(std::fs::read(&manifest).unwrap(), original);
    }

    #[tokio::test]
    async fn malformed_entry_does_not_hide_valid_plugins() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("bad")).unwrap();
        std::fs::write(tmp.path().join("bad/Symposium.toml"), "name = [").unwrap();
        std::fs::create_dir(tmp.path().join("good")).unwrap();
        std::fs::write(tmp.path().join("good/Symposium.toml"), "name = 'good'").unwrap();
        let pm = PathPm::new("local", tmp.path());

        let collection = pm.registry_plugins_outcome().await.unwrap();
        assert!(!collection.is_complete());
        assert_eq!(collection.plugins.len(), 1);
        assert_eq!(collection.plugins[0].id.name, "good");
        assert_eq!(collection.errors.len(), 1);
        assert!(
            matches!(&collection.errors[0], PackageManagerError::PluginLoad { id, source }
            if id.name == "bad" && matches!(source.as_ref(), crate::plugins::PluginLoadError::Parse { .. }))
        );
        assert_eq!(pm.registry_plugins().await.unwrap().len(), 1);
        assert_eq!(pm.active_plugins(&[]).await.len(), 1);
        assert!(matches!(
            pm.try_registry_plugins().await,
            Err(PackageManagerError::PluginLoad { .. })
        ));
    }

    #[tokio::test]
    async fn explicit_load_distinguishes_failed_source_from_absent_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = tmp.path().join("registry");
        let pm = PathPm::new("local", &registry);
        let missing = PackageId::new("local", "missing", ANY_VERSION);

        let unavailable = pm.load_plugin_outcome(&missing).await;
        assert!(!unavailable.is_complete());
        assert!(matches!(
            &unavailable.errors[0],
            PackageManagerError::ReadRegistry { .. }
        ));

        std::fs::create_dir(&registry).unwrap();
        let absent = pm.load_plugin_outcome(&missing).await;
        assert!(absent.is_complete());
        assert!(absent.plugins.is_empty());
        assert!(absent.errors.is_empty());

        std::fs::create_dir(registry.join("bad")).unwrap();
        std::fs::write(registry.join("bad/Symposium.toml"), "name = [").unwrap();
        let bad = PackageId::new("local", "bad", ANY_VERSION);
        let failed = pm.load_plugin_outcome(&bad).await;
        assert!(!failed.is_complete());
        assert!(failed.plugins.is_empty());
        assert!(matches!(
            &failed.errors[0],
            PackageManagerError::PluginLoad { source, .. }
                if matches!(source.as_ref(), crate::plugins::PluginLoadError::Parse { .. })
        ));
        assert!(matches!(
            pm.try_load_plugin(&bad).await,
            Err(PackageManagerError::PluginLoad { .. })
        ));
        assert!(pm.load_plugin(&bad).await.is_empty());
    }
}
