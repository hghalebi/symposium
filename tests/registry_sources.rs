//! Raw configuration stays compatible; operations require one complete source set.

use std::error::Error as _;
use std::path::Path;
use std::sync::Arc;

use symposium::config::{RegistryConfig, RegistryError, Symposium};
use symposium::pm::{GitPm, PmInstance, PmRegistry, RegistrySource, WorkspaceDeps};

fn isolated_symposium(root: &Path) -> Symposium {
    let mut sym = Symposium::try_from_dir(root).unwrap();
    sym.config.defaults.symposium_recommendations = false;
    sym.config.defaults.user_plugins = false;
    sym
}

fn registry(name: &str, git: Option<&str>, path: Option<&str>) -> RegistryConfig {
    RegistryConfig {
        name: name.to_string(),
        git: git.map(str::to_string),
        path: path.map(str::to_string),
        auto_update: false,
    }
}

fn resolution_error(sym: &Symposium) -> RegistryError {
    match sym.try_registry_instances() {
        Err(error) => error,
        Ok(_) => panic!("invalid registry configuration must not return a partial set"),
    }
}

#[test]
fn valid_sources_preserve_registry_order_and_resolve_relative_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sym = isolated_symposium(tmp.path());
    sym.config.defaults.symposium_recommendations = true;
    sym.config.defaults.user_plugins = true;
    std::fs::create_dir(tmp.path().join("plugins")).unwrap();
    sym.config.registries = vec![
        registry("remote", Some("https://github.com/example/plugins"), None),
        registry("local", None, Some("relative-registry")),
    ];

    let instances = sym.try_registry_instances().unwrap();
    let names: Vec<_> = instances
        .iter()
        .map(|instance| instance.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "symposium-recommendations",
            "user-plugins",
            "remote",
            "local"
        ]
    );
    assert!(instances.iter().all(|instance| instance.trusted));
    assert!(matches!(
        instances[2].pm.registry_source(),
        Some(RegistrySource::Git { url }) if url == "https://github.com/example/plugins"
    ));
    assert!(matches!(
        instances[3].pm.registry_source(),
        Some(RegistrySource::Path { dir }) if dir == tmp.path().join("relative-registry")
    ));

    let managers = sym.try_detached_managers().unwrap();
    let names: Vec<_> = managers
        .instances()
        .map(|instance| instance.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "cargo",
            "symposium-recommendations",
            "user-plugins",
            "remote",
            "local"
        ]
    );
}

#[test]
fn absolute_registry_paths_are_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sym = isolated_symposium(&tmp.path().join("config"));
    let path = tmp.path().join("external-registry");
    sym.config.registries = vec![registry("absolute", None, Some(path.to_str().unwrap()))];

    let instances = sym.try_registry_instances().unwrap();
    assert!(matches!(
        instances[0].pm.registry_source(),
        Some(RegistrySource::Path { dir }) if dir == path
    ));
}

#[test]
fn both_and_missing_sources_are_rejected_after_public_config_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sym = isolated_symposium(tmp.path());
    sym.config.registries = vec![registry("valid-first", None, Some("valid"))];
    assert_eq!(sym.try_registry_instances().unwrap().len(), 1);

    sym.config.registries.push(registry(
        "broken",
        Some("https://synthetic-secret-canary.invalid/token"),
        Some("local"),
    ));
    let error = resolution_error(&sym);
    assert!(matches!(&error, RegistryError::BothSources { name } if name == "broken"));
    assert!(!error.to_string().contains("synthetic-secret-canary"));
    assert!(
        sym.try_package_managers(&Arc::new(WorkspaceDeps::detached()))
            .is_err()
    );
    assert!(sym.try_detached_managers().is_err());

    sym.config.registries[1].git = None;
    sym.config.registries[1].path = None;
    assert!(matches!(
        resolution_error(&sym),
        RegistryError::MissingSource { name } if name == "broken"
    ));
}

#[test]
fn invalid_git_source_preserves_typed_error_without_echoing_url() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sym = isolated_symposium(tmp.path());
    sym.config.registries = vec![registry(
        "private-registry",
        Some("https://synthetic-secret-canary.invalid/token"),
        None,
    )];

    let error = resolution_error(&sym);
    assert!(matches!(&error, RegistryError::Git { name, .. } if name == "private-registry"));
    assert!(error.source().is_some());
    let error = anyhow::Error::from(error);
    assert!(!format!("{error:#}").contains("synthetic-secret-canary"));
}

#[test]
fn legacy_alias_and_valid_registry_fields_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    std::fs::write(
        &path,
        "[defaults]\nsymposium-recommendations = false\nuser-plugins = false\n\
         [[plugin-source]]\nname = 'remote'\ngit = 'https://github.com/example/plugins'\nauto-update = false\n\
         [[plugin-source]]\nname = 'local'\npath = 'relative-registry'\n",
    )
    .unwrap();
    let sym = Symposium::try_from_dir(tmp.path()).unwrap();
    assert_eq!(sym.try_registry_instances().unwrap().len(), 2);
    sym.save_config().unwrap();

    let saved = std::fs::read_to_string(&path).unwrap();
    let value: toml::Value = toml::from_str(&saved).unwrap();
    let registries = value["registry"].as_array().unwrap();
    assert_eq!(
        registries[0]["git"].as_str(),
        Some("https://github.com/example/plugins")
    );
    assert_eq!(registries[0]["auto-update"].as_bool(), Some(false));
    assert_eq!(registries[1]["path"].as_str(), Some("relative-registry"));
    assert!(registries[0].get("path").is_none());
    let reloaded = Symposium::try_from_dir(tmp.path()).unwrap();
    assert_eq!(reloaded.try_registry_instances().unwrap().len(), 2);
}

#[test]
fn invalid_registry_resolution_preserves_original_configuration_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config.toml");
    for fields in [
        "git = 'https://synthetic-secret-canary.invalid/token'\npath = 'local'\n",
        "",
    ] {
        let original = format!(
            "# user formatting survives validation\n[defaults]\nsymposium-recommendations = false\n\
             user-plugins = false\n[[registry]]\nname = 'broken'\n{fields}"
        );
        std::fs::write(&path, &original).unwrap();
        let sym = Symposium::try_from_dir(tmp.path()).unwrap();
        let error = resolution_error(&sym);
        assert!(matches!(
            error,
            RegistryError::BothSources { .. } | RegistryError::MissingSource { .. }
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}

#[test]
fn legacy_registry_and_manager_signatures_and_behavior_remain_available() {
    let _: fn(&Symposium) -> Vec<PmInstance> = Symposium::registry_instances;
    let _: fn(&Symposium, &Arc<WorkspaceDeps>) -> PmRegistry = Symposium::package_managers;
    let _: fn(&Symposium) -> PmRegistry = Symposium::detached_managers;
    let _: fn(String, String, bool, symposium_install::InstallContext) -> Option<GitPm> =
        GitPm::new;

    let tmp = tempfile::tempdir().unwrap();
    let mut sym = isolated_symposium(tmp.path());
    sym.config.registries = vec![
        registry(
            "legacy-both",
            Some("https://github.com/example/plugins"),
            Some("ignored"),
        ),
        registry("legacy-missing", None, None),
    ];
    let instances = sym.registry_instances();
    assert_eq!(instances.len(), 1);
    assert!(matches!(
        instances[0].pm.registry_source(),
        Some(RegistrySource::Git { .. })
    ));
}

#[test]
fn absent_optional_user_registry_does_not_make_a_manager_set_unreadable() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sym = isolated_symposium(tmp.path());
    sym.config.defaults.user_plugins = true;
    assert!(sym.try_registry_instances().unwrap().is_empty());
    assert_eq!(sym.try_detached_managers().unwrap().instances().count(), 1);
}

#[cfg(unix)]
#[test]
fn a_failed_optional_user_registry_probe_is_not_absence() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("config");
    let mut sym = isolated_symposium(&root);
    sym.config.defaults.user_plugins = true;
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::write(&root, b"not a directory").unwrap();

    let error = resolution_error(&sym);
    assert!(matches!(
        &error,
        RegistryError::ProbeUserPlugins { path, source }
            if path == &root.join("plugins") && source.kind() != std::io::ErrorKind::NotFound
    ));
    assert!(error.source().is_some());
    assert_eq!(std::fs::read(&root).unwrap(), b"not a directory");
}

#[cfg(unix)]
#[test]
fn broken_user_registry_symlink_is_retained_for_the_read_failure_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sym = isolated_symposium(tmp.path());
    sym.config.defaults.user_plugins = true;
    std::os::unix::fs::symlink(tmp.path().join("unavailable"), tmp.path().join("plugins")).unwrap();

    let instances = sym.try_registry_instances().unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0].name, "user-plugins");
}
