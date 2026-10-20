#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable
    )
)]

use clap::Parser;
use std::env;
use std::io::Write;
use std::process::ExitCode;

use symposium::cli::{Cli, Commands, PluginCommand};
use symposium::config;
use symposium::help_render;
use symposium::hook;
use symposium::output::Output;
use symposium::plugins;
use symposium::report;
use symposium::self_update;
use symposium::state;
use symposium::subcommand_dispatch::try_dispatch_external;

#[tokio::main]
async fn main() -> ExitCode {
    // When invoked as `cargo agents`, cargo passes "agents" as the first arg.
    // Strip it so clap sees the real arguments.
    let args: Vec<_> = std::env::args_os().collect();
    let filtered: Vec<_> = if args.len() > 1 && args[1] == "agents" {
        std::iter::once(args[0].clone())
            .chain(args[2..].iter().cloned())
            .collect()
    } else {
        args
    };

    // Parse without exiting on error: a help request on a built-in with required args
    // (`crate-info --help`, `plugin --help`) surfaces a parse error that `help_text` recovers.
    // `args_str` feeds the subcommand-name walk.
    let args_str = filtered
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let parse = Cli::try_parse_from(filtered);

    // Parse the hook identity before fallible startup so permission failures can deny natively.
    let startup_hook = parse.as_ref().ok().and_then(|cli| match &cli.command {
        Some(Commands::Hook { agent, event }) => Some((*agent, *event)),
        _ => None,
    });
    let mut sym = match config::Symposium::try_from_environment() {
        Ok(sym) => sym,
        Err(error) => return startup_failure(startup_hook, &error),
    };
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(source) => {
            return startup_failure(startup_hook, &hook::HookError::CurrentDirectory { source });
        }
    };

    // `--help` / `-h` / `help` / no subcommand -> audience-grouped top-level help (or clap's
    // per-command help for `<built-in> --help`).
    // Plugin `<name> --help` returns `None` here and is forwarded to the child by dispatch below.
    let help = match help_render::try_help_text(parse.as_ref(), &args_str, &sym, &cwd).await {
        Ok(help) => help,
        Err(error) => return startup_failure(startup_hook, &error),
    };
    if let Some(text) = help {
        return match write_stdout(text.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => startup_failure(None, &error),
        };
    }

    let cli = match parse {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };

    let is_hook = matches!(cli.command, Some(Commands::Hook { .. }));

    // Install the report layer. Mode determines output format:
    // --json → accumulate JSON array; -v → stderr trace; default → stdout.
    let (mode, level) = if cli.json {
        let level = if cli.verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        };
        (report::ReportMode::Json, level)
    } else if cli.verbose {
        (report::ReportMode::Verbose, tracing::Level::DEBUG)
    } else {
        (report::ReportMode::Normal, tracing::Level::INFO)
    };
    let (report_layer, report_handle) = report::ReportLayer::new(mode, level);
    // A hook's stdout is the agent's protocol: any report line ahead of the
    // JSON makes the agent discard the whole output. The events still reach
    // the log file; `-v` prints them to stderr when debugging a hook by hand.
    let report_layer = (!is_hook || cli.verbose).then_some(report_layer);
    if let Err(error) = sym.try_init_logging(report_layer) {
        return startup_failure(startup_hook, &error);
    }

    // Log the command being invoked
    match &cli.command {
        Some(Commands::Init { .. }) => tracing::info!("cargo agents init"),
        Some(Commands::Sync) => tracing::info!("cargo agents sync"),
        Some(Commands::Search { .. }) => tracing::info!("cargo agents search"),
        Some(Commands::Use {
            name,
            global,
            remove,
        }) => tracing::info!(name_len = name.len(), global, remove, "cargo agents use"),
        Some(Commands::Status) => tracing::info!("cargo agents status"),
        Some(Commands::Plugin { .. }) => tracing::info!("cargo agents plugin"),
        Some(Commands::Hook { agent, event }) => {
            tracing::debug!(?agent, ?event, "cargo agents hook");
        }
        Some(Commands::SelfUpdate) => tracing::info!("cargo agents self-update"),
        Some(Commands::CrateInfo { .. }) => tracing::debug!("cargo agents crate-info"),
        Some(Commands::Telemetry { command }) => {
            tracing::info!(subcommand = ?command, "cargo agents telemetry");
        }
        Some(Commands::External(argv)) => {
            tracing::info!(argument_count = argv.len(), "cargo agents <external>");
        }
        None => {}
    }

    // Hook commands are quiet by default (they're invoked by the agent, not the user).
    // JSON mode also suppresses human output (only JSON goes to stdout).
    let out = if cli.quiet || is_hook || cli.json {
        Output::quiet()
    } else {
        Output::normal()
    };

    // Ensure git-based plugin sources are up to date (non-blocking on failure).
    // SessionStart runs once per session, so we force a real freshness check
    // there; other invocations use the `--update` level (debounced by default).
    let source_update = match &cli.command {
        Some(Commands::Hook { event, .. })
            if *event == symposium::hook::HookEvent::SessionStart =>
        {
            symposium_install::UpdateLevel::Check
        }
        _ => cli.update,
    };
    if let Err(error) = plugins::try_ensure_registries(&sym, source_update).await {
        return startup_failure(startup_hook, &error);
    }

    // A rejected registry configuration must not stamp operational state.
    if let Err(error) = state::try_ensure_current(sym.config_dir()) {
        tracing::warn!(error = %error, "operational state was not stamped (continuing)");
    }

    // Auto-update = "on": check for updates and re-exec if a new binary was
    // installed.  Skipped for self-update (which always checks explicitly)
    // and for hooks (session-start injects the warn nudge into hook output;
    // the "on" re-exec for hooks is handled here).
    if !matches!(cli.command, Some(Commands::SelfUpdate)) && !is_hook {
        if self_update::maybe_check_for_update(&sym, &out).await {
            self_update::re_exec();
        }
    } else if is_hook
        && sym.config.auto_update == config::AutoUpdate::On
        && self_update::maybe_check_for_update(&sym, &Output::quiet()).await
    {
        self_update::re_exec();
    }

    match cli.command {
        // Commands that need direct I/O (stdin/stdout) stay in the binary
        Some(Commands::Hook { agent, event }) => hook::run(&sym, agent, event).await,

        Some(Commands::Plugin { command }) => {
            let code = handle_plugin_command(&sym, command).await;
            match emit_events(&report_handle) {
                Ok(()) => code,
                Err(error) => startup_failure(None, &error),
            }
        }

        Some(Commands::External(argv)) => match try_dispatch_external(&sym, &cwd, argv).await {
            Ok(result) => {
                let stdout = write_stdout(&result.stdout);
                let stderr = write_stream(&mut std::io::stderr(), &result.stderr);
                match stdout.and(stderr) {
                    Ok(()) => ExitCode::from(result.exit_code),
                    Err(error) => startup_failure(None, &error),
                }
            }
            Err(error) => startup_failure(None, &error),
        },
        // No-subcommand and the `help` keyword are handled by the help branch
        // right after parsing, above.
        None => startup_failure(None, &"no command selected"),

        // Everything else delegates to the library
        Some(cmd) => match symposium::cli::try_run(&mut sym, cmd, &cwd, &out, cli.update).await {
            Ok(()) => match emit_events(&report_handle) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => startup_failure(None, &error),
            },
            Err(error) => startup_failure(None, &error),
        },
    }
}

fn startup_failure(
    request: Option<(hook::HookAgent, hook::HookEvent)>,
    error: &impl std::fmt::Display,
) -> ExitCode {
    // Deliver the protocol decision before best-effort diagnostics; stderr can be closed.
    let code = match request {
        Some((agent, event)) => hook::respond_to_failure(agent, event),
        None => ExitCode::FAILURE,
    };
    if let Err(source) = std::io::stderr().write_all(format!("cargo agents: {error}\n").as_bytes())
    {
        tracing::warn!(error_kind = ?source.kind(), "failed to write startup diagnostic");
    }
    code
}

fn write_stream(writer: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    writer.write_all(bytes).and_then(|()| writer.flush())
}

fn write_stdout(bytes: &[u8]) -> std::io::Result<()> {
    write_stream(&mut std::io::stdout(), bytes)
}

fn emit_events(handle: &report::ReportHandle) -> anyhow::Result<()> {
    let events = handle.drain();
    if !events.is_empty() {
        let mut bytes = serde_json::to_vec_pretty(&events)?;
        bytes.push(b'\n');
        write_stdout(&bytes)?;
    }
    Ok(())
}

async fn handle_plugin_command(sym: &config::Symposium, command: PluginCommand) -> ExitCode {
    match try_handle_plugin_command(sym, command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => startup_failure(None, &error),
    }
}

async fn try_handle_plugin_command(
    sym: &config::Symposium,
    command: PluginCommand,
) -> anyhow::Result<()> {
    match command {
        PluginCommand::Sync { provider } => {
            let synced = plugins::try_sync_registries(sym, provider.as_deref()).await?;
            if synced.is_empty() {
                let text = match provider {
                    Some(provider) => format!("No git source found for provider: {provider}\n"),
                    None => "No git sources to sync.\n".into(),
                };
                write_stdout(text.as_bytes())?;
            } else {
                for name in &synced {
                    write_stdout(format!("Synced: {name}\n").as_bytes())?;
                }
            }
        }
        PluginCommand::List => {
            for provider in plugins::try_list_plugins(sym).await? {
                tracing::info!(report = %report::ReportEvent::ProviderListed {
                    name: provider.name,
                    source_type: provider.source_type.to_string(),
                    url: provider.git_url,
                    path: provider.path,
                    plugins: provider.plugins.iter().map(|p| p.name.clone()).collect(),
                });
            }
        }
        PluginCommand::Validate {
            path,
            no_check_crates,
        } => {
            if path.is_dir() {
                let results = plugins::try_validate_source_dir(&path)?;
                if results.is_empty() {
                    anyhow::bail!("no plugins or skills found in source directory");
                }
                let mut errors = results.iter().map(emit_validation_results).sum::<usize>();
                if !no_check_crates {
                    let names = plugins::try_collect_crate_names_in_source_dir(&path)?;
                    for name in &names {
                        let exists = plugins::try_check_crate_exists(name).await?;
                        tracing::info!(report = %report::ReportEvent::Validated {
                            path: name.clone(), item_kind: "crate".into(), valid: exists,
                            error: if exists { None } else { Some("not found on crates.io".into()) },
                            warning: None,
                        });
                        errors += usize::from(!exists);
                    }
                }
                if errors > 0 {
                    anyhow::bail!("plugin validation failed ({errors} errors)");
                }
            } else {
                let parent = path.parent().unwrap_or(&path);
                plugins::try_load_plugin(&path, "", parent)?;
                // Validation reports success without copying potentially private manifest values.
                write_stdout(format!("Validated: {}\n", path.display()).as_bytes())?;
            }
        }
        PluginCommand::Show { plugin } => {
            let resolved = plugins::try_find_plugin(sym, &plugin)
                .await?
                .ok_or_else(|| anyhow::anyhow!("plugin not found"))?;
            let rendered = toml::to_string_pretty(&resolved.manifest)?;
            write_stdout(format!("# {}\n\n{rendered}", resolved.canonical).as_bytes())?;
        }
    }
    Ok(())
}

fn emit_validation_results(r: &plugins::TypedPluginValidationResult) -> usize {
    let mut errors = 0;
    let (valid, error, warning) = match &r.result {
        Ok(()) => (true, None, r.warning.clone()),
        Err(error) => {
            errors += 1;
            (false, Some(error.to_string()), None)
        }
    };
    tracing::info!(report = %report::ReportEvent::Validated {
        path: r.id.clone(), item_kind: r.kind.to_string(), valid, error, warning,
    });
    errors
        + r.children
            .iter()
            .map(emit_validation_results)
            .sum::<usize>()
}
