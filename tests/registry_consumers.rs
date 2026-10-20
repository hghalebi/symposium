//! Operational callers reject ambiguous registry sources before publishing results or consent.

use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use symposium::config::{RegistryConfig, RegistryError, Symposium, UseEntry};
use symposium::pm::WorkspaceDeps;

fn ambiguous_symposium(root: &Path) -> Symposium {
    let mut sym = Symposium::try_from_dir(root).unwrap();
    sym.config.defaults.symposium_recommendations = false;
    sym.config.defaults.user_plugins = false;
    sym.config.registries.push(RegistryConfig {
        name: "ambiguous".into(),
        git: Some("https://synthetic-secret-canary.invalid/token".into()),
        path: Some("local".into()),
        auto_update: false,
    });
    sym
}

fn assert_ambiguous(error: RegistryError) {
    assert!(matches!(&error, RegistryError::BothSources { name } if name == "ambiguous"));
    assert!(!error.to_string().contains("synthetic-secret-canary"));
}

#[tokio::test]
async fn checked_read_paths_refuse_ambiguous_sources_instead_of_empty_results() {
    let temp = tempfile::tempdir().unwrap();
    let sym = ambiguous_symposium(temp.path());
    let deps = Arc::new(WorkspaceDeps::detached());

    assert_ambiguous(
        symposium::discovery::try_discover(&sym, &deps)
            .await
            .unwrap_err(),
    );
    assert_ambiguous(
        symposium::discovery::try_pending_candidates(&sym, &deps)
            .await
            .unwrap_err(),
    );
    assert_ambiguous(
        symposium::pm::try_workspace_dep_ids(&sym, &deps)
            .await
            .unwrap_err(),
    );
    assert_ambiguous(
        symposium::search_command::try_find_matches(&sym, "anything")
            .await
            .unwrap_err(),
    );
    assert_ambiguous(
        symposium::help_render::try_render_help(&sym, temp.path())
            .await
            .unwrap_err(),
    );
    let args = vec!["cargo-agents".to_string()];
    let cli = symposium::cli::Cli::try_parse_from(&args).unwrap();
    assert_ambiguous(
        symposium::help_render::try_help_text(Ok(&cli), &args, &sym, temp.path())
            .await
            .unwrap_err(),
    );
}

#[tokio::test]
async fn invalid_sources_prevent_enablement_and_removal_config_writes() {
    let temp = tempfile::tempdir().unwrap();
    let mut sym = ambiguous_symposium(temp.path());
    sym.config
        .plugins
        .used
        .push(UseEntry::Global("example".into()));
    sym.save_config().unwrap();
    let config_path = temp.path().join("config.toml");
    let original = std::fs::read(&config_path).unwrap();
    let original_used = sym.config.plugins.used.clone();

    let error = symposium::discovery::apply_consent(&mut sym, &["other".into()], &[]).unwrap_err();
    assert!(error.downcast_ref::<RegistryError>().is_some());
    assert!(sym.config.plugins.auto_enable.is_empty());
    let error = symposium::use_command::use_plugin(
        &mut sym,
        temp.path(),
        "other",
        true,
        symposium::UpdateLevel::None,
    )
    .await
    .unwrap_err();
    assert!(error.downcast_ref::<RegistryError>().is_some());
    let error = symposium::use_command::remove_plugin(
        &mut sym,
        temp.path(),
        "example",
        true,
        symposium::UpdateLevel::None,
    )
    .await
    .unwrap_err();
    assert!(error.downcast_ref::<RegistryError>().is_some());
    assert_eq!(sym.config.plugins.used, original_used);
    assert_eq!(std::fs::read(config_path).unwrap(), original);
}

#[tokio::test]
async fn operational_dispatch_refuses_invalid_sources_before_resolving_commands() {
    let temp = tempfile::tempdir().unwrap();
    let sym = ambiguous_symposium(temp.path());
    let error = symposium::subcommand_dispatch::dispatch_external(
        &sym,
        temp.path(),
        vec!["anything".into()],
    )
    .await
    .err()
    .expect("invalid source must refuse dispatch");
    assert!(error.downcast_ref::<RegistryError>().is_some());
    assert!(matches!(
        symposium::crate_command::dispatch_crate(&sym, "anything", None, temp.path()).await,
        symposium::crate_command::DispatchResult::Err(message)
            if message.contains("ambiguous") && !message.contains("synthetic-secret-canary")
    ));
}

#[test]
fn legacy_non_result_call_signatures_remain_available() {
    let temp = tempfile::tempdir().unwrap();
    let sym = ambiguous_symposium(temp.path());
    let deps = Arc::new(WorkspaceDeps::detached());
    let args = vec!["cargo-agents".to_string()];
    let cli = symposium::cli::Cli::try_parse_from(&args).unwrap();

    // Constructing futures checks the public signatures without invoking search or agents.
    std::mem::drop(symposium::discovery::discover(&sym, &deps));
    std::mem::drop(symposium::discovery::discover_for(&sym, &deps));
    std::mem::drop(symposium::discovery::pending_candidates(&sym, &deps));
    std::mem::drop(symposium::pm::workspace_dep_ids(&sym, &deps));
    std::mem::drop(symposium::help_render::render_help(&sym, temp.path()));
    std::mem::drop(symposium::help_render::help_text(
        Ok(&cli),
        &args,
        &sym,
        temp.path(),
    ));
    std::mem::drop(symposium::search_command::find_matches(&sym, "anything"));
}
