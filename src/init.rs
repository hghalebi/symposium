//! Init command: `cargo agents init`.

use dialoguer::MultiSelect;

use crate::agents::{Agent, AgentConfigError};
use crate::config::{AgentEntry, ConfigError, RegistryError, Symposium};
use crate::output::{Output, display_path};
use crate::sync::SyncError;

/// A failure while validating, persisting, or applying initialization choices.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InitError {
    /// Registry configuration was rejected before changing initialization state.
    #[error("invalid registry configuration for initialization")]
    Registry {
        /// The invalid registry source.
        #[source]
        source: RegistryError,
    },
    /// A configured or requested agent name was invalid.
    #[error("failed to resolve initialization agents")]
    Agent {
        /// The agent-name validation failure.
        #[source]
        source: AgentConfigError,
    },
    /// An interactive choice could not be read.
    #[error("failed to prompt for {operation}")]
    Prompt {
        /// The setting being requested.
        operation: &'static str,
        /// The terminal interaction failure.
        #[source]
        source: dialoguer::Error,
    },
    /// The terminal returned a choice outside its offered agent list.
    #[error("agent selection was outside the offered choices")]
    InvalidAgentSelection,
    /// Initialization choices could not be saved; prior in-memory state remains.
    #[error("failed to write user config")]
    SaveConfig {
        /// The serialization or filesystem failure.
        #[source]
        source: ConfigError,
    },
    /// Applying hooks or MCP configuration failed after the config was saved.
    #[error("failed to {operation}: {source}")]
    Sync {
        /// The application operation that failed.
        operation: &'static str,
        /// The hook or MCP registration failure. Boxing keeps the cold error
        /// variant from enlarging every initialization result.
        #[source]
        source: Box<SyncError>,
    },
}

/// Options that can be provided on the command line to skip interactive prompts.
#[derive(Debug, Default)]
pub struct InitOpts {
    /// Agent names provided via `--add-agent`. If non-empty, skips the agent prompt.
    pub agents: Vec<String>,
    /// Agent names to remove via `--remove-agent`.
    pub remove_agents: Vec<String>,
    /// Explicit hook scope override.
    pub hook_scope: Option<crate::config::HookScope>,
}

/// Whether we can prompt the user interactively.
fn interactive(out: &Output) -> bool {
    out.is_interactive()
}

/// Resolve which agents to configure. Priority:
/// 1. Explicit `--add-agent` / `--remove-agent` flags (applied to existing set)
/// 2. Interactive multi-select (if `should_prompt`), pre-selecting existing agents
/// 3. Default to the first supported agent (Antigravity) in non-interactive mode
fn resolve_agents(
    opts: &InitOpts,
    existing: &[AgentEntry],
    should_prompt: bool,
) -> Result<Vec<Agent>, InitError> {
    if !opts.agents.is_empty() || !opts.remove_agents.is_empty() {
        let mut names: Vec<String> = existing.iter().map(|e| e.name.clone()).collect();
        for name in &opts.agents {
            Agent::try_from_config_name(name).map_err(|source| InitError::Agent { source })?;
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        for name in &opts.remove_agents {
            Agent::try_from_config_name(name).map_err(|source| InitError::Agent { source })?;
            names.retain(|n| n != name);
        }
        return names
            .iter()
            .map(|name| {
                Agent::try_from_config_name(name).map_err(|source| InitError::Agent { source })
            })
            .collect();
    }
    if should_prompt {
        return prompt_for_agents(existing);
    }
    if !existing.is_empty() {
        return existing
            .iter()
            .map(|entry| {
                Agent::try_from_config_name(&entry.name)
                    .map_err(|source| InitError::Agent { source })
            })
            .collect();
    }
    Ok(vec![Agent::Antigravity])
}

/// Run user-wide initialization.
///
/// Prompts for agents (unless provided), writes
/// `~/.symposium/config.toml`, and registers global hooks.
pub async fn init(sym: &mut Symposium, out: &Output, opts: &InitOpts) -> Result<(), InitError> {
    sym.try_registry_instances()
        .map_err(|source| InitError::Registry { source })?;
    tracing::info!("init started");
    out.println("Setting up symposium for your user account.\n");

    // CLI flags signal non-interactive intent — skip all prompts.
    let cli_driven = !opts.agents.is_empty() || !opts.remove_agents.is_empty();
    let should_prompt = !cli_driven && interactive(out);

    // Resolve each setting: CLI flag > interactive prompt > keep existing.
    let agents = resolve_agents(opts, &sym.config.agents, should_prompt)?;

    let mut config = sym.config.clone();
    config.agents = agents
        .iter()
        .map(|a| AgentEntry {
            name: a.config_name().to_string(),
        })
        .collect();

    config.hook_scope = match opts.hook_scope {
        Some(scope) => scope,
        None if should_prompt && !agents.is_empty() => {
            prompt_for_hook_scope(sym.config.hook_scope)?
        }
        None => sym.config.hook_scope,
    };

    if should_prompt && !agents.is_empty() {
        config.auto_update = prompt_for_auto_update(sym.config.auto_update)?;
        config.telemetry.enabled = prompt_for_telemetry(sym.config.telemetry.enabled)?;
    }

    tracing::debug!(
        agents = ?agents.iter().map(|a| a.config_name()).collect::<Vec<_>>(),
        scope = ?config.hook_scope,
        auto_update = ?config.auto_update,
        "resolved config"
    );

    // Persist and apply.
    let previous_config = std::mem::replace(&mut sym.config, config);
    if let Err(source) = sym.try_save_config() {
        sym.config = previous_config;
        return Err(InitError::SaveConfig { source });
    }

    let config_path = sym.config_dir().join("config.toml");

    if agents.is_empty() {
        // Uninstall: unregister all hooks and MCP servers for every agent.
        crate::sync::try_register_hooks(sym, out)
            .await
            .map_err(|source| InitError::Sync {
                operation: "unregister hooks",
                source: Box::new(source),
            })?;
        out.done(format!(
            "{}: wrote user config (no agents — symposium uninstalled)",
            display_path(&config_path),
        ));
        return Ok(());
    }

    let agent_names: Vec<_> = agents.iter().map(|a| a.display_name()).collect();
    out.done(format!(
        "{}: wrote user config (agents: {})",
        display_path(&config_path),
        agent_names.join(", ")
    ));

    if sym.config.hook_scope == crate::config::HookScope::Global {
        crate::sync::try_register_hooks(sym, out)
            .await
            .map_err(|source| InitError::Sync {
                operation: "register global hooks",
                source: Box::new(source),
            })?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Interactive prompts
// ---------------------------------------------------------------------------

fn prompt_for_hook_scope(
    current: crate::config::HookScope,
) -> Result<crate::config::HookScope, InitError> {
    use crate::config::HookScope;

    let items = ["Globally (recommended)", "Per project"];
    let default = match current {
        HookScope::Global => 0,
        HookScope::Project => 1,
    };

    let selection = dialoguer::Select::new()
        .with_prompt("Install hooks and agent configuration")
        .items(items)
        .default(default)
        .interact()
        .map_err(|source| InitError::Prompt {
            operation: "hook scope",
            source,
        })?;

    Ok(match selection {
        0 => HookScope::Global,
        _ => HookScope::Project,
    })
}

fn prompt_for_auto_update(
    current: crate::config::AutoUpdate,
) -> Result<crate::config::AutoUpdate, InitError> {
    use crate::config::AutoUpdate;

    let items = [
        "Auto-update (recommended)",
        "Warn when updates are available",
        "Off",
    ];
    let default = match current {
        AutoUpdate::On => 0,
        AutoUpdate::Warn => 1,
        AutoUpdate::Off => 2,
    };

    let selection = dialoguer::Select::new()
        .with_prompt("Automatic updates")
        .items(items)
        .default(default)
        .interact()
        .map_err(|source| InitError::Prompt {
            operation: "automatic updates",
            source,
        })?;

    Ok(match selection {
        0 => AutoUpdate::On,
        1 => AutoUpdate::Warn,
        _ => AutoUpdate::Off,
    })
}

fn prompt_for_telemetry(current: bool) -> Result<bool, InitError> {
    dialoguer::Confirm::new()
        .with_prompt(
            "Enable anonymous usage telemetry? It is stored locally under \
             ~/.symposium/telemetry/ and never uploaded automatically — you can review \
             it with `cargo agents telemetry show` and share it yourself",
        )
        .default(current)
        .interact()
        .map_err(|source| InitError::Prompt {
            operation: "telemetry consent",
            source,
        })
}

fn prompt_for_agents(existing: &[AgentEntry]) -> Result<Vec<Agent>, InitError> {
    let agents = Agent::all();
    let items: Vec<&str> = agents.iter().map(|a| a.display_name()).collect();

    let defaults: Vec<bool> = agents
        .iter()
        .map(|a| existing.iter().any(|e| e.name == a.config_name()))
        .collect();

    let selections = MultiSelect::new()
        .with_prompt("Which agents do you use? (space to select, enter to confirm)")
        .items(items)
        .defaults(&defaults)
        .interact()
        .map_err(|source| InitError::Prompt {
            operation: "agents",
            source,
        })?;

    selections
        .into_iter()
        .map(|index| {
            agents
                .get(index)
                .copied()
                .ok_or(InitError::InvalidAgentSelection)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HookScope, RegistryConfig};
    use std::error::Error;
    use std::path::Path;

    fn isolated_symposium(path: &Path) -> Symposium {
        let mut sym = Symposium::try_from_dir(path).unwrap();
        sym.config.defaults.symposium_recommendations = false;
        sym.config.defaults.user_plugins = false;
        sym.config.hook_scope = HookScope::Project;
        sym.config.agents = vec![AgentEntry {
            name: "codex".into(),
        }];
        sym.try_save_config().unwrap();
        sym
    }

    #[test]
    fn noninteractive_resolution_preserves_existing_default_and_explicit_removal() {
        assert_eq!(
            resolve_agents(&InitOpts::default(), &[], false).unwrap(),
            vec![Agent::Antigravity]
        );
        let existing = vec![AgentEntry {
            name: "codex".into(),
        }];
        assert_eq!(
            resolve_agents(&InitOpts::default(), &existing, false).unwrap(),
            vec![Agent::Codex]
        );
        let opts = InitOpts {
            remove_agents: vec!["codex".into()],
            ..InitOpts::default()
        };
        assert!(resolve_agents(&opts, &existing, false).unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_registry_is_rejected_before_config_changes_or_output() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = isolated_symposium(tmp.path());
        sym.config.registries.push(RegistryConfig {
            name: "invalid".into(),
            git: Some("https://synthetic-init-canary.invalid/token".into()),
            path: Some("local".into()),
            auto_update: false,
        });
        let previous = toml::to_string(&sym.config).unwrap();
        let config_path = tmp.path().join("config.toml");
        let original = std::fs::read(&config_path).unwrap();
        let output = Output::capturing();
        let opts = InitOpts {
            agents: vec!["claude".into()],
            ..InitOpts::default()
        };

        let error = init(&mut sym, &output, &opts).await.unwrap_err();

        assert!(!error.to_string().contains("synthetic-init-canary"));
        assert!(matches!(
            error,
            InitError::Registry {
                source: RegistryError::BothSources { .. }
            }
        ));
        assert!(output.captured().is_empty());
        assert_eq!(toml::to_string(&sym.config).unwrap(), previous);
        assert_eq!(std::fs::read(config_path).unwrap(), original);
    }

    #[tokio::test]
    async fn invalid_agent_retains_source_without_mutating_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = isolated_symposium(tmp.path());
        let previous = toml::to_string(&sym.config).unwrap();
        let original = std::fs::read(tmp.path().join("config.toml")).unwrap();
        let opts = InitOpts {
            agents: vec!["synthetic-unknown-agent-canary".into()],
            ..InitOpts::default()
        };

        let error = init(&mut sym, &Output::quiet(), &opts).await.unwrap_err();

        assert!(!error.to_string().contains("synthetic-unknown-agent-canary"));
        assert!(error.source().unwrap().is::<AgentConfigError>());
        assert_eq!(toml::to_string(&sym.config).unwrap(), previous);
        assert_eq!(
            std::fs::read(tmp.path().join("config.toml")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn failed_init_save_restores_config_and_reports_no_success() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = isolated_symposium(tmp.path());
        let previous = toml::to_string(&sym.config).unwrap();
        let config_path = tmp.path().join("config.toml");
        let original = std::fs::read(&config_path).unwrap();
        let saved_path = tmp.path().join("original-config.toml");
        std::fs::rename(&config_path, &saved_path).unwrap();
        std::fs::create_dir(&config_path).unwrap();
        let sentinel = config_path.join("keep");
        std::fs::write(&sentinel, b"existing destination contents").unwrap();
        let output = Output::capturing();
        let opts = InitOpts {
            agents: vec!["claude".into()],
            hook_scope: Some(HookScope::Global),
            ..InitOpts::default()
        };

        let error = init(&mut sym, &output, &opts).await.unwrap_err();

        let InitError::SaveConfig { source } = error else {
            panic!("expected a typed save failure");
        };
        assert!(matches!(source, ConfigError::Write { .. }));
        assert_eq!(toml::to_string(&sym.config).unwrap(), previous);
        assert_eq!(std::fs::read(saved_path).unwrap(), original);
        assert_eq!(
            std::fs::read(sentinel).unwrap(),
            b"existing destination contents"
        );
        assert!(
            !output
                .captured()
                .iter()
                .any(|line| line.contains("wrote user config"))
        );
    }

    #[test]
    fn init_prompt_error_retains_the_concrete_terminal_source() {
        let error = InitError::Prompt {
            operation: "agents",
            source: dialoguer::Error::IO(std::io::Error::other("synthetic-prompt-canary")),
        };
        assert!(!error.to_string().contains("synthetic-prompt-canary"));
        assert!(error.source().unwrap().is::<dialoguer::Error>());
    }
}
