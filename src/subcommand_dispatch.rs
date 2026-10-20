//! Dispatch for plugin-vended `cargo agents <name>` subcommands.
//!
//! The top-level CLI declares `allow_external_subcommands = true`, any
//! unknown subcommand is routed here. This module looks up the name across configured plugins
//! (filtered by workspace crates at the plugin and subcommand levels), resolves its `Installation` to a `Runnable`, and
//! spawns it with inherited stdio, propagating the child's exit code.
//!

use crate::predicate::PredicateEval;
use std::{ffi::OsString, path::Path, process::ExitStatus};

use crate::{
    config::Symposium,
    installation::{acquire_installation, resolve_runnable},
    plugins::{self, Plugin, PluginManifest, Subcommand},
    pm::PackageId,
};
use anyhow::Result;
use symposium_install::{Runnable, UpdateLevel};
use tokio::process::Command;

/// Collect every plugin subcommand whose plugin-level and subcommand-level predicates
/// apply to `deps`. `used` names the plugins the applicable `[plugins] use`
/// entries enable, which is what wakes a dormant plugin. Shared between
/// dispatch (name lookup) and help rendering (audience grouping).
///
/// `plugins` is the resolved active set from
/// [`plugins::active_plugins`](crate::plugins::active_plugins) — registry plugins
/// plus crate-sourced ones — so a crate plugin's subcommands are dispatchable
/// exactly like a registry plugin's.
pub fn applicable_subcommands<'a>(
    plugins: &'a [Plugin],
    deps: &[PackageId],
    used: &[&str],
) -> Vec<(&'a PluginManifest, &'a str, &'a Subcommand)> {
    let mut ctx = crate::predicate::PredicateContext::new(deps).with_used_names(used);
    let mut results = Vec::new();
    for parsed in plugins {
        let plugin = &parsed.manifest;
        if !parsed.applies(&mut ctx) {
            continue;
        }
        for (name, subcommand) in &plugin.subcommands {
            if subcommand.predicates.evaluate(&mut ctx) {
                results.push((plugin, name.as_str(), subcommand));
            }
        }
    }
    results
}

/// Failure to select, resolve, or run an external plugin subcommand.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubcommandError {
    /// No subcommand name was supplied.
    #[error("missing subcommand name")]
    MissingName,
    /// The supplied name is not valid protocol text.
    #[error("subcommand name must be valid UTF-8")]
    InvalidUtf8,
    /// More than one applicable plugin claims the same name.
    #[error("subcommand is defined by multiple applicable plugins")]
    Ambiguous {
        /// Manifest names that claim the subcommand, in discovery order.
        plugins: Vec<String>,
    },
    /// No active plugin claims the requested subcommand.
    #[error(
        "no active plugin defines the requested subcommand; run `cargo agents --help` for available commands"
    )]
    NotFound,
    /// A plugin references an installation absent from its manifest.
    #[error("subcommand references an unknown installation")]
    UnknownInstallation,
    /// The complete registry set could not be resolved.
    #[error("could not resolve configured registries for subcommand dispatch")]
    Registry(#[from] crate::config::RegistryError),
    /// Installation acquisition or runnable preparation failed.
    #[error("failed to prepare subcommand installation")]
    Installation(#[from] crate::hook::InstallationError),
    /// The resolved child process could not be started.
    #[error("failed to spawn subcommand process")]
    Spawn(#[source] std::io::Error),
}

/// Look up a subcommand by name across the active plugin set, filtered by
/// workspace crates at the plugin and subcommand levels.
///
/// - `Ok(None)` - no plugin claims the name, or every claim was filtered out.
/// - `Ok(Some(..))` - exactly one plugin claims the name and applies.
/// - `Err(..)` - two or more plugins claim the name and all apply.
///
/// Legacy error adapter retained through root 0.6; use [`try_find_subcommand`].
pub fn find_subcommand<'a>(
    plugins: &'a [Plugin],
    name: &str,
    deps: &[PackageId],
    used: &[&str],
) -> Result<Option<(&'a PluginManifest, &'a Subcommand)>> {
    Ok(try_find_subcommand(plugins, name, deps, used)?)
}

/// Look up an applicable subcommand while retaining a typed ambiguity error.
///
/// # Errors
/// Returns [`SubcommandError::Ambiguous`] when several active plugins claim it.
pub fn try_find_subcommand<'a>(
    plugins: &'a [Plugin],
    name: &str,
    deps: &[PackageId],
    used: &[&str],
) -> std::result::Result<Option<(&'a PluginManifest, &'a Subcommand)>, SubcommandError> {
    let matches: Vec<_> = applicable_subcommands(plugins, deps, used)
        .into_iter()
        .filter(|(_, n, _)| *n == name)
        .map(|(plugin, _, subcmd)| (plugin, subcmd))
        .collect();

    match matches.as_slice() {
        [] => Ok(None),
        [single] => Ok(Some(*single)),
        many => Err(SubcommandError::Ambiguous {
            plugins: many.iter().map(|(plugin, _)| plugin.name.clone()).collect(),
        }),
    }
}

/// Result of running an external subcommand.
pub struct ExternalOutput {
    pub exit_code: u8,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Dispatch through the legacy error boundary, retained through root 0.6.
/// New callers should use [`try_dispatch_external`].
pub async fn dispatch_external(
    sym: &Symposium,
    cwd: &Path,
    argv: Vec<OsString>,
) -> Result<ExternalOutput> {
    Ok(try_dispatch_external(sym, cwd, argv).await?)
}

/// Select and execute an applicable plugin subcommand.
///
/// # Errors
/// Returns [`SubcommandError`] for invalid input or registries, ambiguous/missing
/// commands, invalid installations, acquisition failure, or child-spawn failure.
pub async fn try_dispatch_external(
    sym: &Symposium,
    cwd: &Path,
    argv: Vec<OsString>,
) -> std::result::Result<ExternalOutput, SubcommandError> {
    let mut argv = argv.into_iter();
    let raw_name = argv.next().ok_or(SubcommandError::MissingName)?;

    let name = raw_name.to_str().ok_or(SubcommandError::InvalidUtf8)?;
    let forwarded = argv.collect::<Vec<_>>();

    let deps = sym.workspace_deps(cwd);
    let workspace = deps.load().cloned();
    let registry = plugins::try_load_registry_with_workspace(sym, workspace.as_deref()).await?;

    let dep_ids = crate::pm::try_workspace_dep_ids(sym, &deps).await?;
    let used = workspace
        .as_ref()
        .map(|ws| sym.config.plugins.used_names_in(&ws.root))
        .unwrap_or_default();

    // Resolve the active plugin set so crate-sourced subcommands are dispatchable.
    let mut ctx = crate::predicate::PredicateContext::new(&dep_ids).with_used_names(&used);
    let pms = sym.try_package_managers(&deps)?;
    let active = crate::plugins::active_plugins(
        sym,
        &registry,
        &pms,
        workspace.as_ref().map(|ws| ws.root.as_path()),
        &mut ctx,
    )
    .await;

    let (plugin, subcommand) =
        try_find_subcommand(&active, name, &dep_ids, &used)?.ok_or(SubcommandError::NotFound)?;

    let installation = plugin
        .get_installation(&subcommand.command)
        .ok_or(SubcommandError::UnknownInstallation)?;

    let runnable = resolve_runnable(
        acquire_installation(sym, installation, None, None, UpdateLevel::None).await?,
        "subcommand",
    )?;

    spawn(runnable, &installation.args, &forwarded).await
}

async fn spawn(
    runnable: Runnable,
    install_args: &[String],
    forwarded: &[OsString],
) -> std::result::Result<ExternalOutput, SubcommandError> {
    let mut cmd = match runnable {
        Runnable::Exec(path_buf) => Command::new(path_buf),
        Runnable::Script(path_buf) => {
            let mut cmd = Command::new("sh");
            cmd.arg(path_buf);
            cmd
        }
    };

    cmd.args(install_args).args(forwarded);

    let output = cmd.output().await.map_err(SubcommandError::Spawn)?;

    Ok(ExternalOutput {
        exit_code: exit_byte_from(output.status),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn exit_byte_from(status: ExitStatus) -> u8 {
    match status.code() {
        Some(0) => 0,
        Some(n) => u8::try_from(n).unwrap_or(1),
        None => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PluginRegistry;
    use crate::pm::ANY_VERSION;
    use crate::{plugins::Audience, predicate::PredicateSet};
    use std::collections::BTreeMap;

    fn ws_crate(name: &str, version: &str) -> PackageId {
        PackageId::new(crate::pm::CARGO_PM, name, version)
    }

    fn crate_set(spec: &str) -> PredicateSet {
        PredicateSet::from_depends_on(spec).unwrap()
    }

    fn plugin_with(
        name: &str,
        depends_on: &str,
        subcommands: BTreeMap<String, Subcommand>,
    ) -> Plugin {
        Plugin {
            canonical: PackageId::new("test", name, ANY_VERSION),
            manifest: PluginManifest {
                name: name.into(),
                predicates: crate_set(depends_on),
                installations: vec![],
                hooks: vec![],
                skills: vec![],
                mcp_servers: vec![],
                subcommands,
                custom_predicates: vec![],
                chained: vec![],
                requires_use: false,
            },
            workspace_member: false,
        }
    }

    fn subcommand(command: &str, depends_on: Option<&str>) -> Subcommand {
        Subcommand {
            description: "test".into(),
            audience: Audience::default(),
            command: command.into(),
            predicates: depends_on.map(crate_set).unwrap_or_default(),
        }
    }

    fn registry(plugins: Vec<Plugin>) -> PluginRegistry {
        PluginRegistry {
            plugins,
            warnings: vec![],
            unreadable_registries: vec![],
            custom_predicates: crate::plugins::CustomPredicateRegistry::default(),
        }
    }

    #[test]
    fn returns_single_match() {
        let mut subs = BTreeMap::new();
        subs.insert("greet".into(), subcommand("greet-install", None));
        let reg = registry(vec![plugin_with("example-plugin", "*", subs)]);

        let ws = [ws_crate("skill-tree", "1.0.0")];

        let (plugin, sub) = find_subcommand(&reg.plugins, "greet", &ws, &[])
            .unwrap()
            .unwrap();
        assert_eq!(plugin.name, "example-plugin");
        assert_eq!(sub.command, "greet-install");
    }

    #[test]
    fn unknown_name_returns_none() {
        let mut subs = BTreeMap::new();
        subs.insert("greet".into(), subcommand("greet-install", None));
        let reg = registry(vec![plugin_with("example-plugin", "*", subs)]);
        let ws = [ws_crate("skill-tree", "1.0.0")];

        assert!(
            find_subcommand(&reg.plugins, "nope", &ws, &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn plugin_predicate_excludes_plugin() {
        let mut subs = BTreeMap::new();
        subs.insert("greet".into(), subcommand("greet-install", Some("rayon")));
        let reg = registry(vec![plugin_with("example-plugin", "*", subs)]);
        let ws = [ws_crate("skill-tree", "1.0.0")];

        assert!(
            find_subcommand(&reg.plugins, "greet", &ws, &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn two_matching_plugins_conflict() {
        let mut a = BTreeMap::new();
        a.insert("greet".into(), subcommand("a-install", None));

        let mut b = BTreeMap::new();
        b.insert("greet".into(), subcommand("b-install", None));

        let reg = registry(vec![
            plugin_with("plugin-a", "*", a),
            plugin_with("plugin-b", "*", b),
        ]);
        let ws = [ws_crate("skill-tree", "1.0.0")];

        let error = try_find_subcommand(&reg.plugins, "greet", &ws, &[]).unwrap_err();
        assert!(matches!(
            &error,
            SubcommandError::Ambiguous { plugins } if plugins == &["plugin-a", "plugin-b"]
        ));
        assert!(!error.to_string().contains("plugin-a"));
    }

    #[tokio::test]
    async fn missing_name_and_spawn_failure_remain_typed() {
        use std::error::Error as _;

        let temp = tempfile::tempdir().unwrap();
        let sym = Symposium::try_from_dir(temp.path()).unwrap();
        let error = try_dispatch_external(&sym, temp.path(), Vec::new())
            .await
            .err()
            .expect("missing name must fail before registry lookup");
        assert!(matches!(error, SubcommandError::MissingName));

        let canary = "synthetic-private-command-canary";
        let absent = temp.path().join(canary);
        let error = spawn(Runnable::Exec(absent), &[], &[])
            .await
            .err()
            .expect("missing executable must fail");
        assert!(
            matches!(&error, SubcommandError::Spawn(source) if source.kind() == std::io::ErrorKind::NotFound)
        );
        assert!(error.source().unwrap().is::<std::io::Error>());
        assert!(!error.to_string().contains(canary));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_subcommand_name_is_rejected_before_registry_lookup() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let sym = Symposium::try_from_dir(temp.path()).unwrap();
        let error = try_dispatch_external(&sym, temp.path(), vec![OsString::from_vec(vec![0xff])])
            .await
            .err()
            .expect("non-UTF-8 name cannot be a subcommand");
        assert!(matches!(error, SubcommandError::InvalidUtf8));
    }
}
