//! CLI argument parsing and dispatch.
//!
//! This module defines the argument types and the core `run()` function
//! so that both the binary and the test harness can invoke commands.

use std::ffi::OsString;
use std::path::Path;

use std::io::Write;

use clap::{Parser, Subcommand};
use thiserror::Error;

use crate::config::Symposium;
use crate::crate_command;
use crate::discovery;
use crate::hook;
use crate::init::{self, InitOpts};
use crate::output::Output;
use crate::plugins::Audience;
use crate::search_command;
use crate::self_update;
use crate::status_command;
use crate::subcommand_dispatch::try_dispatch_external;
use crate::sync;
use crate::use_command;

/// Parsed CLI arguments.
#[derive(Debug, Parser)]
#[command(
    name = "cargo-agents",
    bin_name = "cargo agents",
    version,
    about = "AI the Rust Way",
    allow_external_subcommands = true,
    disable_help_flag = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Control plugin source update behavior (none, check, fetch)
    #[arg(long, global = true, default_value = "none")]
    pub update: symposium_install::UpdateLevel,

    /// Suppress status output
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Print detailed information about decisions made
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Output structured JSON report
    #[arg(long, global = true)]
    pub json: bool,

    /// Print help
    #[arg(short = 'h', long = "help", global = true)]
    pub help: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Set up user-wide configuration
    Init {
        /// Agent to configure (e.g., antigravity, claude, copilot). Repeatable.
        /// Skips the interactive prompt.
        #[arg(long = "add-agent")]
        agents: Vec<String>,

        /// Remove an agent. Repeatable.
        #[arg(long = "remove-agent")]
        remove_agents: Vec<String>,

        /// Where to install agent hooks: global (~/) or project (./).
        #[arg(long = "hook-scope")]
        hook_scope: Option<crate::config::HookScope>,
    },

    /// Synchronize skills with workspace dependencies
    Sync,

    /// Search configured registries for plugins
    Search {
        /// Name (or name fragment) to look for
        query: String,
    },

    /// Enable a plugin by name and sync it into the workspace
    Use {
        /// Plugin (crate) name to enable
        name: String,

        /// Enable for every workspace instead of just the current one
        #[arg(long)]
        global: bool,

        /// Remove a previously recorded enablement instead of adding one
        #[arg(long)]
        remove: bool,
    },

    /// Show which plugins are enabled for this workspace, and why
    Status,

    /// Hook entry point invoked by your agent (internal)
    #[command(hide = true)]
    Hook {
        /// The agent (antigravity, claude, copilot, ...)
        agent: hook::HookAgent,

        /// The hook event (e.g., pre-tool-use, post-tool-use)
        event: hook::HookEvent,
    },

    /// Manage plugins
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },

    /// Update symposium to the latest version
    SelfUpdate,

    /// Find crate sources
    CrateInfo {
        /// Crate name
        name: String,

        /// Version constraint (e.g., "1.0.3", "^1.0"); defaults to workspace version or latest
        #[arg(long)]
        version: Option<String>,
    },

    /// Manage opt-in usage telemetry (status, enable, disable, show)
    Telemetry {
        #[command(subcommand)]
        command: Option<TelemetryCommand>,
    },

    /// Plugin-vended subcommand. `argv[0]` is the name; the rest forwards to the child.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Debug, Subcommand)]
pub enum TelemetryCommand {
    /// Show whether telemetry is enabled, where data lives, and how much is stored (default)
    Status,

    /// Turn on telemetry collection (writes `[telemetry] enabled = true`)
    Enable,

    /// Turn off telemetry collection
    Disable,

    /// Print recent recorded events as JSON lines, for inspection
    Show {
        /// Number of most-recent events to print
        #[arg(long, default_value_t = 50)]
        count: usize,
    },
}

/// Audience section a built-in subcommand belongs to in `--help`.
///
/// `None` means the subcommand is hidden (omitted from help entirely).
/// Plugin-vended subcommands carry their own audience on the manifest;
/// this only covers the static `Commands` variants above.
pub fn builtin_audience(name: &str) -> Option<Audience> {
    match name {
        "init" | "sync" | "search" | "use" | "status" | "self-update" | "plugin" | "telemetry" => {
            Some(Audience::Humans)
        }
        "crate-info" => Some(Audience::Agents),
        _ => None,
    }
}

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Sync plugin sources from git repositories
    Sync {
        /// Provider name to sync (omit to sync all)
        provider: Option<String>,
    },

    /// List all providers and their plugins
    List,

    /// Show details for a specific plugin
    Show {
        /// Plugin name
        plugin: String,
    },

    /// Validate a plugin source directory or a single TOML manifest
    Validate {
        /// Path to a directory or a single .toml file
        path: std::path::PathBuf,

        /// Skip checking that crate names in predicates exist on crates.io
        #[arg(long)]
        no_check_crates: bool,
    },
}

/// Typed initialization failures exposed through [`CliError`].
pub use crate::init::InitError;

/// A command failed before completing its requested operation.
///
/// Display is safe for diagnostics; source chains may contain private inputs.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CliError {
    /// Invalid registry alternatives.
    #[error(transparent)]
    Registry(#[from] crate::config::RegistryError),
    /// Initialization could not complete.
    #[error(transparent)]
    Init(#[from] InitError),
    /// Consent could not be recorded or requested.
    #[error(transparent)]
    Consent(#[from] discovery::ConsentError),
    /// Synchronization failed.
    #[error(transparent)]
    Sync(#[from] sync::SyncError),
    /// Plugin enablement failed.
    #[error(transparent)]
    Use(#[from] use_command::UseError),
    /// Workspace status could not be obtained.
    #[error(transparent)]
    Status(#[from] status_command::StatusError),
    /// An update could not complete.
    #[error(transparent)]
    Update(#[from] self_update::UpdateError),
    /// Crate source lookup failed.
    #[error(transparent)]
    Crate(#[from] crate_command::CrateError),
    /// A plugin subcommand could not execute.
    #[error(transparent)]
    Subcommand(#[from] crate::subcommand_dispatch::SubcommandError),
    /// The plugin child completed unsuccessfully.
    #[error("subcommand exited with status: {code}")]
    SubcommandExit { code: u8 },
    /// Configuration persistence failed.
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    /// Protocol or command output could not be delivered.
    #[error("failed to deliver command output")]
    Output {
        #[source]
        source: std::io::Error,
    },
    /// A telemetry event could not be encoded.
    #[error("failed to encode telemetry output")]
    TelemetryOutput {
        #[source]
        source: serde_json::Error,
    },
    /// This command requires the executable's direct I/O dispatch.
    #[error("command not supported in library dispatch (use binary)")]
    ExecutableOnly,
}

/// Run a parsed CLI command with concrete library error sources.
///
/// `cwd` is the working directory for workspace commands; `update` selects
/// source refresh policy. Hook and plugin management require binary dispatch.
///
/// # Errors
/// Returns [`CliError`] when validation, an operation, or output delivery fails.
pub async fn try_run(
    sym: &mut Symposium,
    cmd: Commands,
    cwd: &Path,
    out: &Output,
    update: symposium_install::UpdateLevel,
) -> Result<(), CliError> {
    sym.try_registry_instances()?;
    if !matches!(cmd, Commands::SelfUpdate) {
        self_update::maybe_warn_for_update(sym, out);
    }
    match cmd {
        Commands::Init {
            agents,
            remove_agents,
            hook_scope,
        } => {
            let opts = InitOpts {
                agents,
                remove_agents,
                hook_scope,
            };
            init::init(sym, out, &opts).await.map_err(Into::into)
        }
        Commands::Sync => {
            let deps = sym.workspace_deps(cwd);
            discovery::try_prompt_for_consent(sym, &deps, out).await?;
            sync::try_sync(sym, &deps, update).await.map_err(Into::into)
        }
        Commands::Search { query } => search_command::try_search(sym, &query)
            .await
            .map_err(Into::into),
        Commands::Use {
            name,
            global,
            remove,
        } => if remove {
            use_command::try_remove_plugin(sym, cwd, &name, global, update).await
        } else {
            use_command::try_use_plugin(sym, cwd, &name, global, update).await
        }
        .map_err(Into::into),
        Commands::Status => status_command::try_status(sym, cwd)
            .await
            .map_err(Into::into),
        Commands::SelfUpdate => self_update::try_self_update(sym, out).map_err(Into::into),
        Commands::CrateInfo { name, version } => {
            let output =
                crate_command::try_dispatch_crate(sym, &name, version.as_deref(), cwd).await?;
            write_command_output(&mut std::io::stdout(), output.as_bytes())
        }
        Commands::Telemetry { command } => {
            match command.unwrap_or(TelemetryCommand::Status) {
                TelemetryCommand::Status => {
                    let text = crate::telemetry::status_text(
                        sym.config_dir(),
                        sym.config.telemetry.enabled,
                    );
                    write_command_output(&mut std::io::stdout(), text.as_bytes())?;
                }
                TelemetryCommand::Enable => {
                    sym.config.telemetry.enabled = true;
                    sym.try_save_config()?;
                    out.println(
                        "Telemetry enabled. Events are stored locally under the telemetry \
                        directory and are never uploaded automatically — review them with \
                        `cargo agents telemetry show`.",
                    );
                }
                TelemetryCommand::Disable => {
                    sym.config.telemetry.enabled = false;
                    sym.try_save_config()?;
                    out.println("Telemetry disabled.");
                }
                TelemetryCommand::Show { count } => {
                    let events = crate::telemetry::recent_events(sym.config_dir(), count);
                    if events.is_empty() {
                        out.println("No telemetry events recorded.");
                    }
                    for event in &events {
                        let mut line = serde_json::to_vec(event)
                            .map_err(|source| CliError::TelemetryOutput { source })?;
                        line.push(b'\n');
                        write_command_output(&mut std::io::stdout(), &line)?;
                    }
                }
            }
            Ok(())
        }
        Commands::External(argv) => {
            let result = try_dispatch_external(sym, cwd, argv).await?;
            if !result.stdout.is_empty() {
                out.println(String::from_utf8_lossy(&result.stdout).trim_end());
            }
            write_command_output(&mut std::io::stderr(), &result.stderr)?;
            match result.exit_code {
                0 => Ok(()),
                code => Err(CliError::SubcommandExit { code }),
            }
        }
        Commands::Hook { .. } | Commands::Plugin { .. } => Err(CliError::ExecutableOnly),
    }
}

fn write_command_output(writer: &mut impl Write, bytes: &[u8]) -> Result<(), CliError> {
    writer
        .write_all(bytes)
        .and_then(|()| writer.flush())
        .map_err(|source| CliError::Output { source })
}

/// Compatible root-0.5 dispatch adapter for [`try_run`], retained until root 0.6.
///
/// # Errors
/// Returns the original typed [`CliError`] through the legacy error container.
pub async fn run(
    sym: &mut Symposium,
    cmd: Commands,
    cwd: &Path,
    out: &Output,
    update: symposium_install::UpdateLevel,
) -> anyhow::Result<()> {
    Ok(try_run(sym, cmd, cwd, out, update).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_delivery_preserves_source_without_rendering_it() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "CLI_PRIVATE_CANARY",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = write_command_output(&mut Broken, b"result").unwrap_err();
        assert!(!error.to_string().contains("CLI_PRIVATE_CANARY"));
        use std::error::Error;
        assert_eq!(error.source().unwrap().to_string(), "CLI_PRIVATE_CANARY");
    }
}
