//! The git package manager: a registry instance backed by a git repository.
//!
//! A [`GitPm`] fronts one `[[registry]]` `git` entry. Once its repository is
//! fetched into the plugin-source cache it is just a directory, so the *reads*
//! ([`active_plugins`](PackageManager::active_plugins) / `load_plugin` /
//! `search` / `fetch`) delegate to an inner [`PathPm`] over that cache dir. The
//! git-specific part is [`refresh`](PackageManager::refresh): pulling the latest
//! content — which is exactly the operation a [`PathPm`] has no work to do for.

use anyhow::Result;
use symposium_install::UpdateLevel;
use symposium_install::git::GitCacheManager;

use super::{
    FetchedPackage, PackageId, PackageManager, PackageManagerError, PathPm, PluginCollection,
    PluginInfo, RegistrySource, UnvalidatedPlugin,
};
use crate::config::REGISTRY_CACHE_SUBDIR;

/// A git-backed registry: a repository cached on disk, read as a directory.
pub struct GitPm {
    name: String,
    git_url: String,
    /// Whether this registry participates in unforced auto-refresh.
    auto_update: bool,
    /// Install context, used to build a [`GitCacheManager`] for refreshing.
    ctx: symposium_install::InstallContext,
    /// Reads delegate here — the cache directory the repository unpacks into.
    inner: PathPm,
}

impl GitPm {
    /// A git registry named `name` for `git_url`, cached under
    /// [`REGISTRY_CACHE_SUBDIR`]. `None` if the URL can't be resolved to a
    /// cache path or its HTTP client cannot be built. Failures are reported
    /// with a warning and their source URL is omitted.
    ///
    /// Legacy entry point retained until the root crate's 0.6 release.
    /// New callers should use [`Self::try_new`].
    pub fn new(
        name: impl Into<String>,
        git_url: impl Into<String>,
        auto_update: bool,
        ctx: symposium_install::InstallContext,
    ) -> Option<Self> {
        match Self::try_new(name, git_url, auto_update, ctx) {
            Ok(manager) => Some(manager),
            Err(error) => {
                tracing::warn!(error = %error, "cannot configure Git registry");
                None
            }
        }
    }

    /// Resolve a Git registry's local cache path without fetching its contents.
    ///
    /// # Errors
    /// Returns a structured Git source or HTTP-client construction error.
    /// Source URL contents are not included in its diagnostic.
    pub fn try_new(
        name: impl Into<String>,
        git_url: impl Into<String>,
        auto_update: bool,
        ctx: symposium_install::InstallContext,
    ) -> std::result::Result<Self, symposium_install::git::GitError> {
        let name = name.into();
        let git_url = git_url.into();
        let content_dir = GitCacheManager::try_new(&ctx, REGISTRY_CACHE_SUBDIR)?
            .try_cache_path_for_url(&git_url)?;
        let inner = PathPm::new(name.clone(), content_dir);
        Ok(Self {
            name,
            git_url,
            auto_update,
            ctx,
            inner,
        })
    }

    fn check_registry_source(&self) -> std::result::Result<(), PackageManagerError> {
        match std::fs::metadata(&self.inner.dir) {
            Ok(_) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(PackageManagerError::RegistryNotFetched {
                    name: self.name.clone(),
                })
            }
            Err(source) => Err(PackageManagerError::ReadRegistry {
                name: self.name.clone(),
                path: self.inner.dir.clone(),
                source,
            }),
        }
    }
}

#[async_trait::async_trait]
impl PackageManager for GitPm {
    fn name(&self) -> &str {
        &self.name
    }

    async fn active_plugins(&self, deps: &[PackageId]) -> Vec<UnvalidatedPlugin> {
        self.inner.active_plugins(deps).await
    }

    async fn registry_plugins(&self) -> Result<Vec<UnvalidatedPlugin>> {
        let collection = self.registry_plugins_outcome().await?;
        for error in &collection.errors {
            tracing::warn!(report = %crate::report::ReportEvent::Warning { message: error.to_string() });
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
        self.check_registry_source()?;
        self.inner.registry_plugins_outcome().await
    }

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
        if let Err(error) = self.check_registry_source() {
            return PluginCollection::incomplete(Vec::new(), vec![error]);
        }
        self.inner.load_plugin_outcome(id).await
    }

    async fn list_deps(&self) -> Result<Vec<PackageId>> {
        Ok(self.try_list_deps().await?)
    }

    async fn try_list_deps(&self) -> std::result::Result<Vec<PackageId>, PackageManagerError> {
        self.inner.try_list_deps().await
    }

    async fn search(&self, query: &str) -> Result<Vec<PluginInfo>> {
        Ok(self.try_search(query).await?)
    }

    async fn try_search(
        &self,
        query: &str,
    ) -> std::result::Result<Vec<PluginInfo>, PackageManagerError> {
        self.inner.try_search(query).await
    }

    async fn fetch(&self, id: &PackageId, update: UpdateLevel) -> Result<FetchedPackage> {
        Ok(self.try_fetch(id, update).await?)
    }

    async fn try_fetch(
        &self,
        id: &PackageId,
        update: UpdateLevel,
    ) -> std::result::Result<FetchedPackage, PackageManagerError> {
        self.inner.try_fetch(id, update).await
    }

    /// Pull the repository. Skipped (returns `false`) when auto-update is off
    /// and the caller did not `force`; otherwise fetches at `update` and
    /// returns `true`.
    async fn refresh(&self, update: UpdateLevel, force: bool) -> Result<bool> {
        Ok(self.try_refresh(update, force).await?)
    }

    async fn try_refresh(
        &self,
        update: UpdateLevel,
        force: bool,
    ) -> std::result::Result<bool, PackageManagerError> {
        if !force && !self.auto_update {
            tracing::debug!(registry = %self.name, "skipping refresh (auto-update disabled)");
            return Ok(false);
        }
        GitCacheManager::try_new(&self.ctx, REGISTRY_CACHE_SUBDIR)
            .map_err(|source| PackageManagerError::RefreshGitRegistry {
                name: self.name.clone(),
                source,
            })?
            .try_fetch_url(&self.git_url, update)
            .await
            .map_err(|source| PackageManagerError::RefreshGitRegistry {
                name: self.name.clone(),
                source,
            })?;
        Ok(true)
    }

    fn registry_source(&self) -> Option<RegistrySource> {
        Some(RegistrySource::Git {
            url: self.git_url.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_resolves_a_cache_dir_and_reports_git_source() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = symposium_install::InstallContext::new(tmp.path().to_path_buf());
        let pm = GitPm::new(
            "symposium-recommendations",
            "https://github.com/symposium-dev/recommendations",
            true,
            ctx,
        )
        .expect("a well-formed URL resolves to a cache path");
        assert_eq!(pm.name(), "symposium-recommendations");
        match pm.registry_source() {
            Some(RegistrySource::Git { url }) => {
                assert_eq!(url, "https://github.com/symposium-dev/recommendations");
            }
            _ => panic!("expected a git registry source"),
        }
    }

    #[tokio::test]
    async fn unfetched_registry_is_typed_and_disabled_refresh_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = symposium_install::InstallContext::new(tmp.path().to_path_buf());
        let pm = GitPm::try_new(
            "recommendations",
            "https://github.com/symposium-dev/recommendations",
            false,
            ctx,
        )
        .unwrap();

        assert!(matches!(
            pm.try_registry_plugins().await,
            Err(PackageManagerError::RegistryNotFetched { name }) if name == "recommendations"
        ));
        let owned = PackageId::new("recommendations", "plugin", super::super::ANY_VERSION);
        let missing = pm.load_plugin_outcome(&owned).await;
        assert!(!missing.is_complete());
        assert!(matches!(
            &missing.errors[0],
            PackageManagerError::RegistryNotFetched { .. }
        ));
        let foreign = PackageId::new("other", "plugin", super::super::ANY_VERSION);
        let unrelated = pm.load_plugin_outcome(&foreign).await;
        assert!(unrelated.is_complete());
        assert!(unrelated.plugins.is_empty());
        assert!(unrelated.errors.is_empty());
        assert!(!pm.try_refresh(UpdateLevel::None, false).await.unwrap());
        assert!(!pm.inner.dir.exists());
    }

    #[tokio::test]
    async fn refresh_preserves_typed_git_source_without_rendering_url() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = symposium_install::InstallContext::new(tmp.path().to_path_buf());
        let mut pm = GitPm::try_new(
            "recommendations",
            "https://github.com/symposium-dev/recommendations",
            true,
            ctx,
        )
        .unwrap();
        // The refresh boundary must validate again before any network operation.
        pm.git_url = "https://user:synthetic-secret@example.com/repository".into();

        let error = pm.try_refresh(UpdateLevel::None, true).await.unwrap_err();
        assert!(matches!(
            error,
            PackageManagerError::RefreshGitRegistry { .. }
        ));
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<symposium_install::git::GitError>()
                .is_some()
        );
        assert!(!error.to_string().contains("synthetic-secret"));
        assert!(!pm.inner.dir.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_cache_is_not_reported_as_unfetched() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = symposium_install::InstallContext::new(tmp.path().to_path_buf());
        let pm = GitPm::try_new(
            "recommendations",
            "https://github.com/symposium-dev/recommendations",
            true,
            ctx,
        )
        .unwrap();
        std::fs::create_dir_all(pm.inner.dir.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(pm.inner.dir.file_name().unwrap(), &pm.inner.dir).unwrap();

        let error = pm.try_registry_plugins().await.err().unwrap();
        assert!(
            matches!(&error, PackageManagerError::ReadRegistry { source, .. }
            if source.kind() != std::io::ErrorKind::NotFound)
        );
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .is_some()
        );
    }

    #[tokio::test]
    async fn cached_registry_keeps_valid_siblings_and_reports_incompleteness() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = symposium_install::InstallContext::new(tmp.path().to_path_buf());
        let pm = GitPm::try_new(
            "recommendations",
            "https://github.com/symposium-dev/recommendations",
            false,
            ctx,
        )
        .unwrap();
        std::fs::create_dir_all(pm.inner.dir.join("good")).unwrap();
        std::fs::write(pm.inner.dir.join("good/Symposium.toml"), "name = 'good'").unwrap();
        std::fs::create_dir(pm.inner.dir.join("bad")).unwrap();
        std::fs::write(pm.inner.dir.join("bad/Symposium.toml"), "name = [").unwrap();

        let collection = pm.registry_plugins_outcome().await.unwrap();
        assert!(!collection.is_complete());
        assert_eq!(collection.plugins.len(), 1);
        assert_eq!(collection.plugins[0].id.name, "good");
        assert_eq!(collection.errors.len(), 1);
        assert!(matches!(
            &collection.errors[0],
            PackageManagerError::PluginLoad { .. }
        ));
        assert_eq!(pm.registry_plugins().await.unwrap().len(), 1);
        assert!(matches!(
            pm.try_registry_plugins().await,
            Err(PackageManagerError::PluginLoad { .. })
        ));
    }
}
