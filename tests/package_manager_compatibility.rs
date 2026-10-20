//! Downstream managers may keep implementing only the original trait contract.

use std::error::Error;
use std::io;

use symposium::UpdateLevel;
use symposium::pm::{
    FetchedPackage, PackageId, PackageManager, PackageManagerError, PluginInfo, PluginKind,
    PmInstance, PmRegistry, UnvalidatedPlugin,
};

struct LegacyManager;

#[async_trait::async_trait]
impl PackageManager for LegacyManager {
    fn name(&self) -> &str {
        "legacy"
    }

    async fn active_plugins(&self, _deps: &[PackageId]) -> Vec<UnvalidatedPlugin> {
        Vec::new()
    }

    async fn load_plugin(&self, id: &PackageId) -> Vec<UnvalidatedPlugin> {
        vec![UnvalidatedPlugin::new(
            id.clone(),
            std::path::PathBuf::from("/synthetic/legacy-plugin"),
            symposium_sdk::manifest::RawPluginManifest {
                name: Some("legacy-plugin".into()),
                ..Default::default()
            },
        )]
    }

    async fn list_deps(&self) -> anyhow::Result<Vec<PackageId>> {
        Ok(vec![PackageId::any_version("legacy", "example")])
    }

    async fn search(&self, _query: &str) -> anyhow::Result<Vec<PluginInfo>> {
        Ok(Vec::new())
    }

    async fn fetch(&self, _id: &PackageId, _update: UpdateLevel) -> anyhow::Result<FetchedPackage> {
        Err(io::Error::new(io::ErrorKind::NotFound, "synthetic-secret-canary").into())
    }
}

#[tokio::test]
async fn typed_calls_accept_unchanged_external_managers_and_preserve_the_source() {
    let registry = PmRegistry::new(vec![PmInstance {
        name: "legacy".into(),
        trusted: false,
        kind: PluginKind::Package,
        pm: Box::new(LegacyManager),
    }]);
    let deps = registry.try_list_deps().await.unwrap();
    assert_eq!(deps, vec![PackageId::any_version("legacy", "example")]);
    let error = registry
        .try_fetch(&deps[0], UpdateLevel::None)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PackageManagerError::LegacyImplementation { .. }
    ));
    assert!(!error.to_string().contains("synthetic-secret-canary"));
    let source = error.source().unwrap();
    assert_eq!(
        source.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::NotFound
    );
    assert!(registry.list_deps().await.is_ok());
    assert!(registry.fetch(&deps[0], UpdateLevel::None).await.is_err());
}

#[tokio::test]
async fn legacy_plugin_contributions_survive_without_claiming_complete_discovery() {
    let id = PackageId::any_version("legacy", "legacy-plugin");
    let (plugins, errors, is_complete) = LegacyManager.load_plugin_outcome(&id).await.into_parts();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].manifest.name.as_deref(), Some("legacy-plugin"));
    assert!(errors.is_empty());
    assert!(!is_complete);
    assert_eq!(LegacyManager.try_load_plugin(&id).await.unwrap().len(), 1);

    let registry = PmRegistry::new(vec![PmInstance {
        name: "legacy".into(),
        trusted: false,
        kind: PluginKind::Package,
        pm: Box::new(LegacyManager),
    }]);
    assert_eq!(registry.try_load_plugin(&id).await.unwrap().len(), 1);
    assert_eq!(registry.load_plugin(&id).await.len(), 1);
}

#[tokio::test]
async fn unknown_manager_and_builtin_registry_failures_remain_concrete() {
    let id = PackageId::any_version("unknown", "example");
    let error = PmRegistry::new(Vec::new())
        .try_fetch(&id, UpdateLevel::None)
        .await
        .unwrap_err();
    assert!(matches!(error, PackageManagerError::UnknownManager { .. }));

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("missing-registry");
    let manager = symposium::pm::PathPm::new("local", &path);
    let error = manager.try_registry_plugins().await.unwrap_err();
    match error {
        PackageManagerError::ReadRegistry {
            source,
            path: failed,
            ..
        } => {
            assert_eq!(failed, path);
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        }
        other => panic!("expected a concrete registry read failure, got {other}"),
    }
}

#[tokio::test]
async fn boxed_builtin_plugin_failures_keep_the_original_parser_source() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("bad")).unwrap();
    std::fs::write(
        temp.path().join("bad/Symposium.toml"),
        "name = [synthetic-secret-canary",
    )
    .unwrap();
    let manager = symposium::pm::PathPm::new("local", temp.path());
    let error = manager.try_registry_plugins().await.unwrap_err();
    assert!(!error.to_string().contains("synthetic-secret-canary"));
    let plugin_source = error.source().unwrap();
    let parser_source = plugin_source.source().unwrap();
    assert!(parser_source.downcast_ref::<toml::de::Error>().is_some());
    match error {
        PackageManagerError::PluginLoad { source, .. } => assert!(matches!(
            source.as_ref(),
            symposium::plugins::PluginLoadError::Parse { .. }
        )),
        other => panic!("expected a typed plugin parse error, got {other}"),
    }
}
