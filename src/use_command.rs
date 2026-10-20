//! `cargo agents use` — explicit plugin enablement.
//!
//! Enablement is the consent axis: the workspace and the configured
//! registries are trust roots, but a dependency is not, so a plugin embedded
//! in a dependency runs only once the user says so. `use` is the durable,
//! by-name form of that decision — it writes a [`UseEntry`] into `[plugins]
//! use`, scoped to the current workspace by default or to every workspace
//! with `--global`.
//!
//! It is also what wakes a *dormant* registry plugin (one whose manifest
//! names no dependency, so nothing else would ever gate it on —
//! [`PluginManifest::requires_use`](crate::plugins::PluginManifest::requires_use)).
//!
//! `use` only adds to what *may* run; activation predicates still decide
//! when it applies. `--remove` is the inverse, and re-syncs so the plugin's
//! skills are reaped straight away.

use std::path::Path;

use anyhow::Result;
use symposium_install::UpdateLevel;

use crate::config::{ConfigError, RegistryError, Symposium, UseEntry};
use crate::crate_sources::normalize_crate_name;
use crate::report::ReportEvent;

/// Failure to change plugin consent or synchronize its effects.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UseError {
    /// An explicit disable decision takes precedence over enablement.
    #[error("plugin is disabled in `[plugins] disable`; remove that entry before enabling it")]
    Disabled,
    /// Workspace-scoped enablement needs a Rust workspace.
    #[error("not in a Rust workspace; pass --global to enable the plugin everywhere")]
    WorkspaceRequired,
    /// No dependency or searchable package matched the requested plugin.
    #[error("no crate or plugin named by the request was found; try `cargo agents search`")]
    NotFound,
    /// No recorded enablement matches the requested name and scope.
    #[error("no `use` entry matches the requested scope; see `cargo agents status`")]
    NoUseEntry,
    /// Workspace discovery or metadata resolution failed.
    #[error("failed to load Rust workspace")]
    Workspace {
        /// The shared discovery/metadata failure, including its concrete source.
        #[from]
        source: std::sync::Arc<crate::pm::WorkspaceError>,
    },
    /// Registry configuration could not be resolved completely.
    #[error("could not resolve configured registries")]
    Registry(#[from] RegistryError),
    /// The consent configuration could not be saved.
    #[error("failed to write user config")]
    Config(#[from] ConfigError),
    /// Consent was recorded, but synchronizing its effects failed.
    #[error("plugin consent changed, but synchronization failed")]
    Sync(#[from] crate::sync::SyncError),
}

/// Record an enablement using the legacy error boundary, retained through root 0.6.
/// New callers should use [`try_use_plugin`].
pub async fn use_plugin(
    sym: &mut Symposium,
    cwd: &Path,
    name: &str,
    global: bool,
    update: UpdateLevel,
) -> Result<()> {
    Ok(try_use_plugin(sym, cwd, name, global, update).await?)
}

/// Record an enablement for `name` and sync so its skills install now.
///
/// # Errors
/// Returns [`UseError`] for invalid registry configuration, disabled or unresolved
/// plugins, missing workspace scope, persistence failure, or synchronization failure.
pub async fn try_use_plugin(
    sym: &mut Symposium,
    cwd: &Path,
    name: &str,
    global: bool,
    update: UpdateLevel,
) -> std::result::Result<(), UseError> {
    // `disable` outranks every way of enabling a plugin, `use` included, so
    // recording an entry here would report success and then install nothing.
    // Re-enabling is dropping the `disable` entry, which is a deliberate edit
    // rather than something `use` undoes on the user's behalf.
    if sym.config.plugins.is_disabled(name) {
        return Err(UseError::Disabled);
    }

    // A configured registry is a trust root: what it offers is already
    // enabled by configuration, so there is nothing to record. The exception
    // is a dormant plugin, for which `use` is exactly the wake-up call.
    let registry = crate::plugins::try_load_registry(sym).await?;
    let normalized = normalize_crate_name(name);
    let registry_plugin = registry
        .plugins
        .iter()
        .find(|p| normalize_crate_name(&p.manifest.name) == normalized);
    let dormant = registry_plugin.is_some_and(|p| p.manifest.requires_use);
    let already_trusted = registry_plugin.is_some() && !dormant;
    if already_trusted {
        tracing::info!(
            report = %ReportEvent::Info {
                message: format!(
                    "`{name}` is already available from a configured registry; nothing to enable"
                ),
            },
        );
        return Ok(());
    }

    let deps = sym.workspace_deps(cwd);
    let workspace_root = deps.try_load()?.map(|ws| ws.root.clone());
    let entry = match (global, &workspace_root) {
        (true, _) => UseEntry::Global(name.to_string()),
        (false, Some(root)) => UseEntry::Workspace {
            name: name.to_string(),
            workspace: root.clone(),
        },
        (false, None) => return Err(UseError::WorkspaceRequired),
    };

    if !dormant {
        resolve_name(sym, &deps, name).await?;
    }

    if sym.config.plugins.used.contains(&entry) {
        tracing::info!(
            report = %ReportEvent::Info {
                message: format!("`{name}` is already enabled; nothing changed"),
            },
        );
    } else {
        sym.config.plugins.used.push(entry);
        sym.try_save_config()?;
        tracing::info!(
            report = %ReportEvent::PluginEnabled {
                name: name.to_string(),
                global,
            },
        );
    }

    // Install now rather than waiting for the next sync.
    if workspace_root.is_some() {
        crate::sync::try_sync(sym, &deps, update).await?;
    }
    Ok(())
}

/// Drop a previously recorded enablement and re-sync so the plugin's skills
/// are reaped now.
///
/// The scope must match: without `--global` this removes the entry recorded
/// for the current workspace, with it the unscoped entry. A scope mismatch is
/// an error rather than a silent success.
/// Legacy error adapter retained through root 0.6. New callers should use
/// [`try_remove_plugin`].
pub async fn remove_plugin(
    sym: &mut Symposium,
    cwd: &Path,
    name: &str,
    global: bool,
    update: UpdateLevel,
) -> Result<()> {
    Ok(try_remove_plugin(sym, cwd, name, global, update).await?)
}

/// Remove a scoped plugin enablement and synchronize its installed effects.
///
/// # Errors
/// Returns [`UseError`] for invalid registry configuration, no matching scoped
/// entry, a persistence failure, or a synchronization failure.
pub async fn try_remove_plugin(
    sym: &mut Symposium,
    cwd: &Path,
    name: &str,
    global: bool,
    update: UpdateLevel,
) -> std::result::Result<(), UseError> {
    let deps = sym.workspace_deps(cwd);
    let workspace_root = deps.try_load()?.map(|ws| ws.root.clone());

    sym.try_registry_instances()?;
    let used = &mut sym.config.plugins.used;
    let before = used.len();
    used.retain(|entry| {
        if normalize_crate_name(entry.name()) != normalize_crate_name(name) {
            return true;
        }
        let in_scope = match entry {
            UseEntry::Global(_) => global,
            UseEntry::Workspace { .. } => {
                !global
                    && workspace_root
                        .as_deref()
                        .is_some_and(|root| entry.applies_in(root))
            }
        };
        !in_scope
    });
    if used.len() == before {
        return Err(UseError::NoUseEntry);
    }
    sym.try_save_config()?;
    tracing::info!(
        report = %ReportEvent::PluginRemoved {
            name: name.to_string(),
            global,
        },
    );

    if workspace_root.is_some() {
        crate::sync::try_sync(sym, &deps, update).await?;
    }
    Ok(())
}

/// Check that `name` resolves to something before recording it: a workspace
/// dependency (offline-friendly) or a registry search hit.
async fn resolve_name(
    sym: &Symposium,
    deps: &std::sync::Arc<crate::pm::WorkspaceDeps>,
    name: &str,
) -> std::result::Result<(), UseError> {
    let normalized = normalize_crate_name(name);
    let is_workspace_dep = deps.try_load()?.is_some_and(|ws| {
        ws.crates
            .iter()
            .any(|c| normalize_crate_name(&c.name) == normalized)
    });
    if is_workspace_dep {
        return Ok(());
    }

    let found = sym
        .try_package_managers(deps)?
        .search(name)
        .await
        .iter()
        .any(|(_, info)| normalize_crate_name(&info.id.name) == normalized);
    if found {
        return Ok(());
    }
    Err(UseError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    fn isolated_symposium(root: &Path) -> Symposium {
        let mut sym = Symposium::try_from_dir(root).unwrap();
        sym.config.defaults.symposium_recommendations = false;
        sym.config.defaults.user_plugins = false;
        sym
    }

    #[tokio::test]
    async fn disabled_plugin_refuses_before_writes_without_echoing_input() {
        let temp = tempfile::tempdir().unwrap();
        let mut sym = isolated_symposium(temp.path());
        let canary = "synthetic-private-plugin-canary";
        sym.config.plugins.disable.push(canary.into());

        let error = try_use_plugin(&mut sym, temp.path(), canary, true, UpdateLevel::None)
            .await
            .unwrap_err();
        assert!(matches!(error, UseError::Disabled));
        assert!(!error.to_string().contains(canary));
        assert!(sym.config.plugins.used.is_empty());
        assert!(!temp.path().join("config.toml").exists());

        let legacy: Result<()> =
            use_plugin(&mut sym, temp.path(), canary, true, UpdateLevel::None).await;
        assert!(legacy.unwrap_err().is::<UseError>());
    }

    #[tokio::test]
    async fn removal_save_failure_retains_concrete_io_source() {
        let temp = tempfile::tempdir().unwrap();
        let mut sym = isolated_symposium(temp.path());
        sym.config
            .plugins
            .used
            .push(UseEntry::Global("example".into()));
        let destination = temp.path().join("config.toml");
        std::fs::create_dir(&destination).unwrap();
        let marker = destination.join("user-file");
        std::fs::write(&marker, "preserve").unwrap();

        let error = try_remove_plugin(&mut sym, temp.path(), "example", true, UpdateLevel::None)
            .await
            .unwrap_err();
        assert!(matches!(&error, UseError::Config(_)));
        let config_error = error.source().unwrap();
        assert!(config_error.is::<ConfigError>());
        assert!(config_error.source().unwrap().is::<std::io::Error>());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "preserve");
    }

    #[tokio::test]
    async fn malformed_workspace_remains_a_typed_failure_before_consent_or_dispatch() {
        use crate::status_command::{StatusError, try_workspace_status};
        use crate::subcommand_dispatch::{SubcommandError, try_dispatch_external};

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let manifest = project.join("Cargo.toml");
        let original = "[workspace\n# synthetic-private-workspace-canary\n";
        std::fs::write(&manifest, original).unwrap();
        let mut sym = isolated_symposium(&home);
        sym.config
            .plugins
            .used
            .push(UseEntry::Global("existing".into()));
        let before = sym.config.plugins.used.clone();

        let use_error = try_use_plugin(&mut sym, &project, "new", false, UpdateLevel::None)
            .await
            .unwrap_err();
        assert!(matches!(&use_error, UseError::Workspace { source }
            if matches!(source.as_ref(), crate::pm::WorkspaceError::CargoLocate { .. })));
        assert!(use_error.source().is_some());
        assert!(
            !use_error
                .to_string()
                .contains("synthetic-private-workspace-canary")
        );
        let remove_error =
            try_remove_plugin(&mut sym, &project, "existing", true, UpdateLevel::None)
                .await
                .unwrap_err();
        assert!(matches!(remove_error, UseError::Workspace { .. }));
        assert_eq!(sym.config.plugins.used, before);
        assert!(!home.join("config.toml").exists());

        let deps = sym.workspace_deps(&project);
        assert!(matches!(
            try_workspace_status(&sym, &deps).await,
            Err(StatusError::Workspace { .. })
        ));
        assert!(matches!(
            try_dispatch_external(&sym, &project, vec!["anything".into()]).await,
            Err(SubcommandError::Workspace { .. })
        ));
        assert_eq!(std::fs::read_to_string(manifest).unwrap(), original);
    }
}
