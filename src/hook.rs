use crate::predicate::PredicateEval;
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{ExitCode, Stdio},
};

use symposium_install::Runnable;
use thiserror::Error;
use tokio::{io::AsyncWriteExt, process::Command};

use crate::installation::{
    AcquiredInstallation, AcquiredRunnable, acquire_installation, refresh_installation_if_present,
    resolve_runnable,
};
use crate::plugins::{HookFormat, Installation};
use crate::pm::WorkspaceDeps;
use crate::{
    config::Symposium,
    hook_schema::{AgentHookInput, ErasedAgentHookEvent, HookSchemaError, symposium},
    plugins::Plugin,
};
use crate::{
    help_render::{AGENTS_HEADING, HUMANS_HEADING},
    hook_schema::symposium::{OutputEvent, SessionStartInput},
    subcommand_dispatch::applicable_subcommands,
};
use std::sync::Arc;

/// A hook prepared for dispatch — installation names looked up to concrete
/// `Installation` entries, so the dispatch loop never has to scan the plugin's
/// installations list again.
struct ResolvedHook {
    hook_name: String,
    format: HookFormat,
    requirements: Vec<Installation>,
    command: Installation,
    /// Hook-level `executable` override. Validation guarantees that if set,
    /// the command installation does not also set executable/script.
    hook_executable: Option<String>,
    /// Hook-level `script` override.
    hook_script: Option<String>,
    args: Vec<String>,
}

impl ResolvedHook {
    fn build(parsed_plugin: &Plugin, hook: &crate::plugins::Hook) -> anyhow::Result<Self> {
        let plugin = &parsed_plugin.manifest;
        let lookup = |name: &str| -> anyhow::Result<Installation> {
            plugin
                .get_installation(name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("installation `{name}` not found in plugin"))
        };

        let command = lookup(&hook.command)?;
        let requirements = hook
            .requirements
            .iter()
            .map(|name| lookup(name))
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self {
            hook_name: hook.name.clone(),
            format: hook.format.clone(),
            requirements,
            command,
            hook_executable: hook.executable.clone(),
            hook_script: hook.script.clone(),
            args: hook.args.clone(),
        })
    }
}

/// Sanitize an installation name for use as part of an env var name.
/// Replaces non-alphanumeric chars with underscore.
fn env_safe(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Build the env vars (including augmented PATH) for the spawn.
///
/// Iterates `acquired` in order; the parent directory of each absolute
/// `runnable` is prepended to `$PATH` so later entries take precedence over
/// earlier ones (the command's own parent ends up first since it's pushed
/// last and prepended).
fn build_env(acquired: &[AcquiredInstallation]) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let mut path_prefix: Vec<String> = Vec::new();

    for a in acquired {
        let key = env_safe(&a.name);
        if let Some(base) = &a.base {
            env.push((format!("SYMPOSIUM_DIR_{key}"), base.display().to_string()));
        }
        if let Some(
            AcquiredRunnable::ResolvedScript { path, .. } | AcquiredRunnable::ResolvedExec { path },
        ) = &a.runnable
        {
            env.push((format!("SYMPOSIUM_{key}"), path.display().to_string()));
            if let Some(parent) = path.parent() {
                let parent_str = parent.display().to_string();
                if !parent_str.is_empty() {
                    path_prefix.push(parent_str);
                }
            }
        }
    }

    // Command was pushed last into `acquired`; reverse so its bin dir wins
    // PATH lookup over requirements' bin dirs.
    path_prefix.reverse();

    if let Some(joined) = prepend_to_path(&path_prefix) {
        env.push(("PATH".to_string(), joined));
    }

    env
}

/// Prepend `dirs` to the current `PATH`, using the platform path-list
/// separator (`:` on Unix, `;` on Windows). Returns `None` when `dirs` is
/// empty. `join_paths` picks the separator, so this stays correct on Windows.
fn prepend_to_path(dirs: &[String]) -> Option<String> {
    if dirs.is_empty() {
        return None;
    }
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut entries: Vec<PathBuf> = dirs.iter().map(PathBuf::from).collect();
    entries.extend(std::env::split_paths(&existing).filter(|dir| !dir.as_os_str().is_empty()));
    std::env::join_paths(entries)
        .ok()
        .map(|joined| joined.to_string_lossy().into_owned())
}

enum SpawnSpec {
    Exec {
        path: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Script {
        path: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
}

async fn build_spawn_spec(sym: &Symposium, hook: &ResolvedHook) -> anyhow::Result<SpawnSpec> {
    // Dispatch-time acquisition serves the cache (git checks debounced); the
    // `SessionStart` prewarm is what forces a freshness check once per session.
    let update = symposium_install::UpdateLevel::None;

    // Acquire requirements first so the command's PATH sees them.
    let mut acquired: Vec<AcquiredInstallation> = Vec::new();
    for requirement in &hook.requirements {
        match acquire_installation(sym, requirement, None, None, update).await {
            Ok(a) => acquired.push(a),
            Err(_error) => {
                tracing::error!("failed to install hook requirement");
            }
        }
    }

    let command_acquired = acquire_installation(
        sym,
        &hook.command,
        hook.hook_executable.as_deref(),
        hook.hook_script.as_deref(),
        update,
    )
    .await?;

    acquired.push(command_acquired.clone());
    let env = build_env(&acquired);

    let label = format!("hook `{}`", hook.hook_name);
    let runnable = resolve_runnable(command_acquired, &label)?;

    Ok(match runnable {
        Runnable::Script(path) => SpawnSpec::Script {
            path,
            args: hook.args.clone(),
            env,
        },
        Runnable::Exec(path) => SpawnSpec::Exec {
            path,
            args: hook.args.clone(),
            env,
        },
    })
}

fn spawn_from_spec(spec: SpawnSpec) -> std::io::Result<tokio::process::Child> {
    match spec {
        SpawnSpec::Script { path, args, env } => {
            let mut cmd = Command::new("sh");
            cmd.arg(path).args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
        }
        SpawnSpec::Exec { path, args, env } => {
            let mut cmd = Command::new(path);
            cmd.args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
        }
    }
}

// Re-export hook schema types for convenience.
pub use crate::hook_schema::{HookAgent, HookEvent, HookResponse};
/// A hook execution failure with content-free diagnostics and inspectable sources.
///
/// Never recursively format its sources or its `Debug` representation in logs:
/// parser, plugin, and I/O sources can carry private request or configuration data.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HookError {
    /// An agent does not support the selected event.
    #[error("agent {agent:?} does not support hook event {event:?}")]
    UnsupportedEvent { agent: HookAgent, event: HookEvent },
    /// Parsing, conversion, or serialization failed at the typed schema boundary.
    #[error(transparent)]
    Schema(#[from] HookSchemaError),
    /// No payload working directory was supplied and the process directory failed.
    #[error("failed to get current directory for hook")]
    CurrentDirectory {
        #[source]
        source: std::io::Error,
    },
    /// The incoming agent stream could not be read.
    #[error("failed to read hook stdin")]
    ReadInput {
        #[source]
        source: std::io::Error,
    },
    /// The response could not be delivered on stdout.
    #[error("failed to write hook stdout")]
    WriteStdout {
        #[source]
        source: std::io::Error,
        /// A simultaneous stderr failure, retained without formatting it.
        stderr_source: Option<std::io::Error>,
    },
    /// The response could not be delivered on stderr.
    #[error("failed to write hook stderr")]
    WriteStderr {
        #[source]
        source: std::io::Error,
    },
    /// A hook command could not be acquired or resolved.
    #[error("failed to prepare plugin hook command")]
    PluginPreparation {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A plugin process could not be started.
    #[error("failed to spawn plugin hook")]
    PluginSpawn {
        #[source]
        source: std::io::Error,
    },
    /// A plugin did not receive its complete input.
    #[error("failed to write plugin hook stdin")]
    PluginInput {
        #[source]
        source: std::io::Error,
    },
    /// A plugin process could not be collected.
    #[error("failed to wait for plugin hook")]
    PluginWait {
        #[source]
        source: std::io::Error,
        /// An input delivery failure observed before process collection failed.
        input_source: Option<std::io::Error>,
    },
    /// Portable plugin output was not valid JSON.
    #[error("failed to parse plugin hook JSON")]
    PluginOutput {
        #[source]
        source: serde_json::Error,
    },
    /// Portable or legacy JSON did not contain an object.
    #[error("plugin hook output must be a JSON object")]
    PluginOutputShape,
}

/// Execute the hook pipeline with typed parsing, transport and conversion errors.
///
/// # Errors
/// Returns [`HookError`] on unsupported events, invalid input/output, command
/// preparation, or transport failure. Explicit plugin denials remain responses.
/// CLI callers must render a native denial on a `PreToolUse` error; [`run`] does so.
pub async fn try_execute_hook(
    sym: &Symposium,
    agent: HookAgent,
    event: HookEvent,
    input: &str,
) -> Result<HookResponse, HookError> {
    let handler = crate::hook_schema::agent_event(agent, event)
        .ok_or(HookError::UnsupportedEvent { agent, event })?;
    let payload = handler.try_parse_input(input)?;
    if !payload.should_dispatch() {
        tracing::debug!(
            ?agent,
            ?event,
            "payload is not an occurrence of the event, skipping"
        );
        let neutral = handler.try_translate_output(&OutputEvent::empty_for(event))?;
        return Ok(handler.try_respond(&neutral.try_to_hook_output()?)?);
    }
    let sym_input = payload.to_symposium();
    if sym_input.event() != event {
        return Err(HookSchemaError::EventMismatch {
            expected: event,
            actual: Some(sym_input.event()),
        }
        .into());
    }
    let cwd = match sym_input.cwd() {
        Some(cwd) => PathBuf::from(cwd),
        None => std::env::current_dir().map_err(|source| HookError::CurrentDirectory { source })?,
    };
    let deps = sym.workspace_deps(&cwd);
    let session_start = event == HookEvent::SessionStart;
    let notify_about_possible_new_plugins = run_auto_sync(sym, &deps, session_start).await;
    if session_start && sym.config.auto_sync {
        prewarm_hook_sources(sym, &deps).await;
    }
    let builtin = dispatch_builtin(sym, &sym_input, &deps, notify_about_possible_new_plugins).await;
    let prior = handler
        .try_translate_output(&builtin)?
        .try_to_hook_output()?;
    let response = match try_dispatch_plugin_hooks(
        sym,
        agent,
        event,
        &sym_input,
        payload.as_ref(),
        prior,
        builtin.additional_context().map(String::from),
        &deps,
    )
    .await
    {
        Ok(output) => handler.try_respond(&output)?,
        Err(HookDispatchError::Blocked(stderr)) => {
            blocked_response(handler.as_ref(), event, &stderr)?
        }
        Err(HookDispatchError::Failed(error)) => return Err(error),
    };
    tracing::trace!(
        stdout_len = response.stdout.len(),
        exit_code = response.exit_code,
        "hook response ready"
    );
    Ok(response)
}

/// Compatible root-0.5 adapter for [`try_execute_hook`].
///
/// # Errors
/// Wraps the same typed [`HookError`] without formatting or losing its source.
/// Root 0.6 will use the typed signature directly.
pub async fn execute_hook(
    sym: &Symposium,
    agent: HookAgent,
    event: HookEvent,
    input: &str,
) -> anyhow::Result<HookResponse> {
    Ok(try_execute_hook(sym, agent, event, input).await?)
}

fn blocked_response(
    handler: &dyn ErasedAgentHookEvent,
    event: HookEvent,
    stderr: &[u8],
) -> Result<HookResponse, HookError> {
    let reason = String::from_utf8_lossy(stderr).trim().to_string();
    let reason = if reason.is_empty() {
        "blocked by a plugin hook".to_string()
    } else {
        reason
    };
    tracing::info!(?event, reason_len = reason.len(), "plugin hook blocked");
    if event == HookEvent::PreToolUse {
        let deny = OutputEvent::PreToolUse(symposium::PreToolUseOutput::deny(reason));
        let output = handler.try_translate_output(&deny)?.try_to_hook_output()?;
        Ok(handler.try_respond(&output)?)
    } else {
        Ok(HookResponse::blocked(&reason))
    }
}

/// Render a fixed host-native denial when a supported `PreToolUse` cannot run.
///
/// # Errors
/// Returns an unsupported-event or typed schema error if no native denial can
/// be rendered. Error details are never included in the protocol response.
pub fn failure_response(agent: HookAgent, event: HookEvent) -> Result<HookResponse, HookError> {
    if event != HookEvent::PreToolUse {
        return Err(HookError::UnsupportedEvent { agent, event });
    }
    let handler = crate::hook_schema::agent_event(agent, event)
        .ok_or(HookError::UnsupportedEvent { agent, event })?;
    blocked_response(
        handler.as_ref(),
        event,
        b"Symposium could not process this hook safely; tool execution is blocked.",
    )
}

/// Deliver a native denial after startup failure, preserving the host's exit policy.
///
/// Unsupported events and failed delivery return failure. A failed stream cannot
/// prove that an agent received the denial, even when an exit code is nonzero.
pub fn respond_to_failure(agent: HookAgent, event: HookEvent) -> ExitCode {
    match failure_response(agent, event) {
        Ok(response) => deliver_response(&response),
        Err(error) => {
            tracing::warn!(?agent, ?event, error = %error, "could not render hook failure");
            ExitCode::FAILURE
        }
    }
}

fn read_hook_input(reader: &mut impl Read) -> Result<String, HookError> {
    let mut input = String::new();
    reader
        .read_to_string(&mut input)
        .map_err(|source| HookError::ReadInput { source })?;
    Ok(input)
}

fn write_hook_response(
    response: &HookResponse,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<(), HookError> {
    let stdout_result = if response.stdout.is_empty() {
        Ok(())
    } else {
        stdout
            .write_all(&response.stdout)
            .and_then(|()| stdout.flush())
    };
    let stderr_result = if response.stderr.is_empty() {
        Ok(())
    } else {
        stderr
            .write_all(&response.stderr)
            .and_then(|()| stderr.flush())
    };
    if let Err(source) = stdout_result {
        return Err(HookError::WriteStdout {
            source,
            stderr_source: stderr_result.err(),
        });
    }
    stderr_result.map_err(|source| HookError::WriteStderr { source })?;
    Ok(())
}

fn failed_delivery_exit(response: &HookResponse) -> u8 {
    if response.exit_code == 2 { 2 } else { 1 }
}

fn deliver_response(response: &HookResponse) -> ExitCode {
    match write_hook_response(response, &mut std::io::stdout(), &mut std::io::stderr()) {
        Ok(()) => ExitCode::from(response.exit_code),
        Err(error) => {
            tracing::warn!(error = %error, "hook response delivery failed");
            ExitCode::from(failed_delivery_exit(response))
        }
    }
}

/// CLI entry point: read input, execute, and deliver the host's protocol response.
/// A supported `PreToolUse` boundary failure is a native denial, including on
/// agents that ignore exit status. Other failures return a nonzero status.
pub async fn run(sym: &Symposium, agent: HookAgent, event: HookEvent) -> ExitCode {
    tracing::debug!(?agent, ?event, "running hook listener");
    let input = match read_hook_input(&mut std::io::stdin()) {
        Ok(input) => input,
        Err(error) => {
            tracing::warn!(?agent, ?event, error = %error, "hook input failed");
            return respond_to_failure(agent, event);
        }
    };
    tracing::trace!(
        ?agent,
        ?event,
        input_len = input.len(),
        "hook stdin received"
    );
    let response = match try_execute_hook(sym, agent, event, &input).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(?agent, ?event, error = %error, "hook failed");
            match failure_response(agent, event) {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(?agent, ?event, error = %error, "could not render hook failure");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    write_hook_trace(agent, event, &input, &response);
    deliver_response(&response)
}

/// Append metadata-only JSONL when `SYMPOSIUM_HOOK_TRACE` names a file.
/// Payloads and stderr remain exclusively on the agent protocol streams.
fn write_hook_trace(agent: HookAgent, event: HookEvent, input: &str, response: &HookResponse) {
    let Some(path) = std::env::var_os("SYMPOSIUM_HOOK_TRACE") else {
        return;
    };

    let entry = serde_json::json!({
        "event": event,
        "agent": agent,
        "input": { "byte_len": input.len() },
        "output": { "byte_len": response.stdout.len() },
        "exit_code": response.exit_code,
        "stderr_len": response.stderr.len(),
    });

    use std::fs::OpenOptions;
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let mut line = match serde_json::to_string(&entry) {
                Ok(line) => line,
                Err(_error) => {
                    tracing::warn!("failed to serialize hook trace metadata");
                    return;
                }
            };
            line.push('\n');
            if let Err(e) = f.write_all(line.as_bytes()) {
                tracing::warn!(error_kind = ?e.kind(), "failed to write hook trace");
            }
        }
        Err(e) => tracing::warn!(error_kind = ?e.kind(), "failed to open hook trace file"),
    }
}

/// Run sync if we're in a workspace directory and auto-sync is enabled. Non-fatal.
///
/// On most events this uses per-workspace state to skip sync when `Cargo.lock`
/// hasn't changed since the last successful sync, avoiding expensive `cargo
/// metadata` calls on every hook invocation. If sync runs, `deps` gets
/// populated — later hook stages reuse the result.
///
/// `SessionStart` runs once per session, so there we do the real work: refresh
/// every source cache (`UpdateLevel::Check`) and sync unconditionally, ignoring
/// the `Cargo.lock` freshness gate — upstream skill changes land even when the
/// workspace's dependencies are unchanged.
///
/// Returns whether it re-synced because `Cargo.lock` changed mid-session.
async fn run_auto_sync(sym: &Symposium, deps: &Arc<WorkspaceDeps>, session_start: bool) -> bool {
    if !sym.config.auto_sync {
        tracing::debug!("auto-sync disabled, skipping");
        return false;
    }

    let cwd = deps.cwd().to_path_buf();

    // Find workspace root via `cargo locate-project` (fast, no dep resolution).
    // If we can't find one, fall through to full sync which will
    // use cargo metadata.
    let workspace_root = crate::workspace_state::find_workspace_root(sym, &cwd);

    // SessionStart always syncs (and refreshes sources); other events skip
    // when the workspace hasn't changed since the last sync.
    if !session_start && let Some(ref root) = workspace_root {
        let state = crate::workspace_state::WorkspaceState::load(sym, root);
        if state.sync_is_fresh(root) {
            tracing::debug!("auto-sync skipped: Cargo.lock unchanged since last sync");
            return false;
        }
    }
    let dependencies_changed = !session_start && workspace_root.is_some();

    let update = if session_start {
        symposium_install::UpdateLevel::Check
    } else {
        symposium_install::UpdateLevel::None
    };

    tracing::debug!("auto-sync running");
    if let Err(_error) = crate::sync::sync(sym, deps, update).await {
        tracing::warn!(session_start, "auto-sync during hook failed (continuing)");
        return false;
    }

    // Record successful sync. If we didn't find the root earlier,
    // try again now (sync may have created Cargo.lock).
    let root = workspace_root.or_else(|| crate::workspace_state::find_workspace_root(sym, &cwd));
    if let Some(ref root) = root {
        let mut state = crate::workspace_state::WorkspaceState::load(sym, root);
        state.record_sync(root);
        state.workspace_root = Some(root.clone());
        state.save(sym, root);
    }
    dependencies_changed
}

/// Whether the hook pipeline must resolve the workspace crate graph before
/// building the active plugin set. True when some plugin's hook gating names a
/// concrete crate, or when there is any crate-plugin expansion to perform — a
/// chained `[[plugins]]` edge, or an enablement entry that could pull a crate
/// plugin in — since expansion evaluates edge and plugin predicates against the
/// crate graph too. Registry plugins reached without any of these dispatch on a
/// crate-free context (the fast path for `PreToolUse`).
fn hook_dispatch_needs_deps(sym: &Symposium, registry_plugins: &[Plugin]) -> bool {
    registry_plugins
        .iter()
        .any(|p| p.manifest.hooks_need_dep_resolution() || !p.manifest.chained.is_empty())
        || sym.config.plugins.has_enablement_entries()
}

/// Refresh the source cache for every hook the workspace could fire this
/// session. Run once on `SessionStart` (where the per-session cost is
/// acceptable) so later events dispatch fresh binaries from cache — dispatch
/// itself acquires with `UpdateLevel::None`. This is also the only path that
/// re-pulls a `cargo + git` hook binary whose branch moved, since its
/// version-keyed cache never invalidates on its own.
///
/// Refresh-only: a source that was never acquired is left alone (it installs
/// lazily when the hook first fires) — `SessionStart` updates installed tools
/// but never installs eagerly. Best-effort: failures are logged and skipped.
async fn prewarm_hook_sources(sym: &Symposium, deps: &Arc<WorkspaceDeps>) {
    let workspace = deps.load().cloned();
    let registry = crate::plugins::load_registry_with_workspace(sym, workspace.as_deref()).await;

    // Resolving the workspace runs cargo, so only do it when some hook's
    // gating references a concrete crate, or there is crate-plugin expansion to
    // perform (mirrors dispatch).
    let dep_ids = if hook_dispatch_needs_deps(sym, &registry.plugins) {
        crate::pm::workspace_dep_ids(sym, deps).await
    } else {
        Vec::new()
    };
    let used_names = workspace
        .as_ref()
        .map(|ws| sym.config.plugins.used_names_in(&ws.root))
        .unwrap_or_default();
    let mut ctx = crate::predicate::PredicateContext::new(&dep_ids).with_used_names(&used_names);
    let pms = sym.package_managers(deps);
    let plugins = crate::plugins::active_plugins(
        sym,
        &registry,
        &pms,
        workspace.as_ref().map(|ws| ws.root.as_path()),
        &mut ctx,
    )
    .await;

    for parsed in &plugins {
        if !parsed.applies(&mut ctx) {
            continue;
        }
        for hook in &parsed.manifest.hooks {
            if !hook.predicates.evaluate(&mut ctx) {
                continue;
            }
            let resolved = match ResolvedHook::build(parsed, hook) {
                Ok(r) => r,
                Err(_error) => {
                    tracing::debug!("prewarm: skipping unbuildable hook");
                    continue;
                }
            };
            for req in &resolved.requirements {
                if let Err(_error) = refresh_installation_if_present(sym, req, None).await {
                    tracing::debug!("prewarm: requirement refresh failed");
                }
            }
            if let Err(_error) = refresh_installation_if_present(
                sym,
                &resolved.command,
                resolved.hook_executable.as_deref(),
            )
            .await
            {
                tracing::debug!("prewarm: command refresh failed");
            }
        }
    }
}

/// Built-in hook logic on canonical symposium types.
pub async fn dispatch_builtin(
    sym: &Symposium,
    input: &symposium::InputEvent,
    deps: &Arc<WorkspaceDeps>,
    notify_about_possible_new_plugins: bool,
) -> symposium::OutputEvent {
    match input {
        symposium::InputEvent::PreToolUse(_) => {
            symposium::OutputEvent::empty_for(HookEvent::PreToolUse)
        }
        symposium::InputEvent::PostToolUse(post) => {
            handle_post_tool_use(sym, post, notify_about_possible_new_plugins).await
        }
        symposium::InputEvent::UserPromptSubmit(prompt) => {
            handle_user_prompt_submit(sym, prompt, notify_about_possible_new_plugins).await
        }
        symposium::InputEvent::SessionStart(session) => {
            handle_session_start(sym, session, deps).await
        }
        symposium::InputEvent::Stop(_) => symposium::OutputEvent::empty_for(HookEvent::Stop),
        _ => symposium::OutputEvent::empty_for(input.event()),
    }
}

/// Handle SessionStart: orient the agent toward crate-aware tooling and, when due, nudge the
/// user to update. The two fragments are computed independently -- the discovery hint is never gated
/// behind the update-check throttle -- then joined into a single context block.
async fn handle_session_start(
    sym: &Symposium,
    _payload: &SessionStartInput,
    deps: &Arc<WorkspaceDeps>,
) -> OutputEvent {
    let fragments = [
        discovery_hint(sym, deps).await,
        consent_hint(sym, deps).await,
        update_nudge(sym),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<String>>();

    if fragments.is_empty() {
        OutputEvent::empty_for(HookEvent::SessionStart)
    } else {
        OutputEvent::with_context(HookEvent::SessionStart, fragments.join("\n\n"))
    }
}

/// Suggest `cargo agents --help` when the active workspace exposes crate-aware plugin subcommands.
/// Reuses the help renderer's `applicable_subcommands`, so the hint fires only when there is actually something to discover; `None` otherwise.
async fn discovery_hint(sym: &Symposium, deps: &Arc<WorkspaceDeps>) -> Option<String> {
    let workspace = deps.load().cloned();
    let registry = crate::plugins::load_registry_with_workspace(sym, workspace.as_deref()).await;
    let dep_ids = crate::pm::workspace_dep_ids(sym, deps).await;

    let used = workspace
        .as_ref()
        .map(|ws| sym.config.plugins.used_names_in(&ws.root))
        .unwrap_or_default();
    let mut ctx = crate::predicate::PredicateContext::new(&dep_ids).with_used_names(&used);
    let pms = sym.package_managers(deps);
    let active = crate::plugins::active_plugins(
        sym,
        &registry,
        &pms,
        workspace.as_ref().map(|ws| ws.root.as_path()),
        &mut ctx,
    )
    .await;
    let any_subcommand = !applicable_subcommands(&active, &dep_ids, &used).is_empty();

    any_subcommand.then(|| {
        format!(
            "This project has crate-aware tools available via `cargo agents`. \
             Run `cargo agents --help` to list them before working with the Rust code. \
             Only use tools under the '{AGENTS_HEADING}' section unless the user \
             explicitly asks you to run one from '{HUMANS_HEADING}'."
        )
    })
}

/// Surface dependency plugins awaiting consent. A hook runs on the agent's
/// behalf, so it must never block on stdin — the candidates are reported as
/// context, with a pointer at the interactive command that can actually ask.
/// `None` when nothing is pending.
async fn consent_hint(sym: &Symposium, deps: &Arc<WorkspaceDeps>) -> Option<String> {
    let names = crate::discovery::pending_candidates(sym, deps).await;
    if names.is_empty() {
        return None;
    }
    Some(format!(
        "These dependencies ship agent plugins that are not enabled yet: {}. \
         They stay off until the user consents — tell the user to run \
         `cargo agents sync` (which asks about each one) or \
         `cargo agents use <name>`. Do not enable them yourself.",
        names.join(", ")
    ))
}

/// Nudge the user to update. Gated by `auto-update = \"warn\"`, the 25h throttle,
/// and a newer published version on the registry; `None` otherwise.
fn update_nudge(sym: &Symposium) -> Option<String> {
    use crate::config::AutoUpdate;
    use crate::state::CURRENT_VERSION;

    if sym.config.auto_update != AutoUpdate::Warn {
        return None;
    }
    if !crate::state::should_check_for_update(sym.config_dir()) {
        return None;
    }
    crate::state::record_update_check(sym.config_dir());

    let latest = crate::self_update::check_upgrade(sym).ok()??;
    Some(format!(
        "symposium {latest} is available (current: {CURRENT_VERSION}). \
         Run `cargo agents self-update` to upgrade."
    ))
}

async fn handle_post_tool_use(
    _sym: &Symposium,
    _post: &symposium::PostToolUseInput,
    notify_about_possible_new_plugins: bool,
) -> symposium::OutputEvent {
    possible_new_plugins_notice(HookEvent::PostToolUse, notify_about_possible_new_plugins)
}

async fn handle_user_prompt_submit(
    _sym: &Symposium,
    _prompt_payload: &symposium::UserPromptSubmitInput,
    notify_about_possible_new_plugins: bool,
) -> symposium::OutputEvent {
    possible_new_plugins_notice(
        HookEvent::UserPromptSubmit,
        notify_about_possible_new_plugins,
    )
}

fn possible_new_plugins_notice(event: HookEvent, notify: bool) -> symposium::OutputEvent {
    if !notify {
        return symposium::OutputEvent::empty_for(event);
    }
    symposium::OutputEvent::with_context(
        event,
        "The workspace's dependencies just changed, so there may be new agent plugins \
         available from them. They stay off until the user consents: tell the user to run \
         `cargo agents sync` in their terminal to review them. Do not enable them yourself."
            .to_string(),
    )
}

pub enum PluginHookOutput {
    // The merged json from all plugin hooks
    Success(serde_json::Value),
    // The stderr from the first plugin hook that exited with failure
    Failure(Vec<u8>),
}

/// Compatible root-0.5 dispatch adapter; prefer [`try_execute_hook`] for typed failures.
///
/// Successful output and explicit plugin-block reasons retain their wire form.
/// Operational failures use a content-free reason in this legacy byte channel.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_plugin_hooks(
    sym: &Symposium,
    host_agent: HookAgent,
    event: HookEvent,
    sym_input: &symposium::InputEvent,
    original_input: &dyn AgentHookInput,
    prior_output: serde_json::Value,
    prior_context: Option<String>,
    deps: &Arc<WorkspaceDeps>,
) -> Result<serde_json::Value, Vec<u8>> {
    try_dispatch_plugin_hooks(
        sym,
        host_agent,
        event,
        sym_input,
        original_input,
        prior_output,
        prior_context,
        deps,
    )
    .await
    .map_err(|failure| match failure {
        HookDispatchError::Blocked(stderr) => stderr,
        HookDispatchError::Failed(error) => error.to_string().into_bytes(),
    })
}

#[derive(Debug)]
enum HookDispatchError {
    Blocked(Vec<u8>),
    Failed(HookError),
}

impl From<HookError> for HookDispatchError {
    fn from(error: HookError) -> Self {
        Self::Failed(error)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ChildDisposition {
    Blocked,
    Ready,
}

fn classify_child_outcome(
    exit_code: Option<i32>,
    input: std::io::Result<()>,
) -> Result<ChildDisposition, HookError> {
    if matches!(exit_code, None | Some(2)) {
        if let Err(error) = input {
            tracing::warn!(error_kind = ?error.kind(), "plugin block takes precedence over input delivery failure");
        }
        return Ok(ChildDisposition::Blocked);
    }
    match input {
        Ok(()) => Ok(ChildDisposition::Ready),
        Err(source) if exit_code == Some(0) && source.kind() == std::io::ErrorKind::BrokenPipe => {
            // Successful hooks may intentionally answer without consuming stdin.
            tracing::debug!("successful plugin closed its input before delivery completed");
            Ok(ChildDisposition::Ready)
        }
        Err(source) => Err(HookError::PluginInput { source }),
    }
}

async fn collect_plugin_output(
    mut child: tokio::process::Child,
    input: &[u8],
) -> Result<std::process::Output, HookDispatchError> {
    let stdin = child.stdin.take();
    // Write and drain concurrently: a plugin may fill stdout before reading stdin.
    let (input_result, output_result) = tokio::join!(
        async move {
            match stdin {
                Some(mut stdin) => stdin.write_all(input).await,
                None => Err(std::io::Error::other("plugin stdin was not piped")),
            }
        },
        child.wait_with_output()
    );
    let output = match output_result {
        Ok(output) => output,
        Err(source) => {
            return Err(HookError::PluginWait {
                source,
                input_source: input_result.err(),
            }
            .into());
        }
    };
    match classify_child_outcome(output.status.code(), input_result)? {
        ChildDisposition::Blocked => Err(HookDispatchError::Blocked(output.stderr)),
        ChildDisposition::Ready => Ok(output),
    }
}

fn selected_native_output(
    output: &dyn crate::hook_schema::AgentHookOutput,
    agent: HookAgent,
    event: HookEvent,
) -> OutputEvent {
    let canonical = output.to_symposium();
    // Antigravity reuses one context-only output type for both lifecycle events.
    if agent == HookAgent::Antigravity && event == HookEvent::UserPromptSubmit {
        match canonical.additional_context() {
            Some(context) => OutputEvent::with_context(event, context.to_string()),
            None => OutputEvent::empty_for(event),
        }
    } else {
        canonical
    }
}

fn decode_plugin_output(
    handler: &dyn ErasedAgentHookEvent,
    agent: HookAgent,
    event: HookEvent,
    native: bool,
    bytes: &[u8],
) -> Result<(serde_json::Value, OutputEvent), HookError> {
    if native {
        let output = handler.try_parse_output(bytes)?;
        let canonical = selected_native_output(output.as_ref(), agent, event);
        handler.try_translate_output(&canonical)?;
        return Ok((output.try_to_hook_output()?, canonical));
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| HookError::PluginOutput { source })?;
    let object = value.as_object().ok_or(HookError::PluginOutputShape)?;
    if object.keys().any(|key| {
        matches!(
            key.as_str(),
            "PreToolUse" | "PostToolUse" | "UserPromptSubmit" | "SessionStart" | "Stop"
        )
    }) {
        let canonical: OutputEvent =
            serde_json::from_value(value).map_err(|source| HookError::PluginOutput { source })?;
        let output = handler
            .try_translate_output(&canonical)?
            .try_to_hook_output()?;
        return Ok((output, canonical));
    }
    // Legacy untagged host JSON remains valid, but its native decision is checked.
    let parsed = handler.try_parse_output(bytes)?;
    let canonical = selected_native_output(parsed.as_ref(), agent, event);
    handler.try_translate_output(&canonical)?;
    let output = if agent == HookAgent::Kiro {
        parsed.try_to_hook_output()?
    } else {
        value
    };
    Ok((output, canonical))
}

#[allow(clippy::too_many_arguments)]
async fn try_dispatch_plugin_hooks(
    sym: &Symposium,
    host_agent: HookAgent,
    event: HookEvent,
    sym_input: &symposium::InputEvent,
    original_input: &dyn AgentHookInput,
    prior_output: serde_json::Value,
    prior_context: Option<String>,
    deps: &Arc<WorkspaceDeps>,
) -> Result<serde_json::Value, HookDispatchError> {
    let workspace = deps.load().cloned();
    let registry = crate::plugins::load_registry_with_workspace(sym, workspace.as_deref()).await;

    // Resolving the workspace means running cargo, so only do it when some
    // plugin's hook gating references a concrete crate (a `depends-on(*)`
    // wildcard or env/shell/path predicate never needs the crate graph), or
    // when there is crate-plugin expansion to perform — that too evaluates
    // predicates against the crate graph.
    let dep_ids = if hook_dispatch_needs_deps(sym, &registry.plugins) {
        crate::pm::workspace_dep_ids(sym, deps).await
    } else {
        Vec::new()
    };
    let used_names = workspace
        .as_ref()
        .map(|ws| sym.config.plugins.used_names_in(&ws.root))
        .unwrap_or_default();
    let mut ctx = crate::predicate::PredicateContext::new(&dep_ids).with_used_names(&used_names);
    // Dispatch over the active set — registry plugins plus crate-sourced ones —
    // so a crate plugin's hooks fire exactly like a registry plugin's.
    let pms = sym.package_managers(deps);
    let plugins = crate::plugins::active_plugins(
        sym,
        &registry,
        &pms,
        workspace.as_ref().map(|ws| ws.root.as_path()),
        &mut ctx,
    )
    .await;
    let hooks = dispatched_hooks_for_payload(&plugins, sym_input, host_agent, &mut ctx);

    let handler =
        crate::hook_schema::agent_event(host_agent, event).ok_or(HookError::UnsupportedEvent {
            agent: host_agent,
            event,
        })?;
    let mut output = prior_output;
    let mut context = JoinedContext::new(event, prior_context);
    for hook in hooks {
        tracing::info!(?event, format = ?hook.format, "running plugin hook");
        let native = hook.format.as_agent() == Some(host_agent);
        let input: &dyn AgentHookInput = if native { original_input } else { sym_input };
        let stdin = input.try_to_string().map_err(HookError::from)?;
        let spec =
            build_spawn_spec(sym, &hook)
                .await
                .map_err(|source| HookError::PluginPreparation {
                    source: source.into_boxed_dyn_error(),
                })?;
        let child = spawn_from_spec(spec).map_err(|source| HookError::PluginSpawn { source })?;
        let child_output = collect_plugin_output(child, stdin.as_bytes()).await?;
        let exit_code = child_output.status.code();
        tracing::trace!(
            ?event,
            stdout_len = child_output.stdout.len(),
            stderr_len = child_output.stderr.len(),
            exit_code,
            "hook finished"
        );
        match exit_code {
            Some(0) if child_output.stdout.is_empty() => continue,
            Some(0) => {
                let (host_json, canonical) = decode_plugin_output(
                    handler.as_ref(),
                    host_agent,
                    event,
                    native,
                    &child_output.stdout,
                )?;
                merge(&mut output, host_json);
                if denies(&canonical) {
                    tracing::info!(
                        ?event,
                        "plugin hook denied the tool call; later hooks do not run"
                    );
                    return Ok(output);
                }
                context.add(Some(&canonical), &mut output, handler.as_ref())?;
            }
            Some(code) => tracing::warn!(
                exit_code = code,
                "plugin hook exited with non-zero (continuing)"
            ),
            None => return Err(HookDispatchError::Blocked(child_output.stderr)),
        }
    }
    Ok(output)
}

/// The context every source contributed to one hook invocation (the builtin
/// output first, then each plugin hook in dispatch order), kept whole across
/// [`merge`], which would let each hook's context replace the one before it.
/// Agents do the same when several hooks answer one event.
///
/// Only for events whose context is pure context. On `PreToolUse` and `Stop`
/// some agents carry it in the field that also holds the decision (a denial's
/// reason, a `continue` reason), so joining there could change the decision.
struct JoinedContext {
    event: HookEvent,
    text: Option<String>,
}

impl JoinedContext {
    fn new(event: HookEvent, prior: Option<String>) -> Self {
        Self { event, text: prior }
    }

    /// Add `hook_output`'s context and write the joined text back into
    /// `output`, whose context the hook's own output has just replaced (or,
    /// for Antigravity's `injectSteps` list, emptied).
    fn add(
        &mut self,
        hook_output: Option<&symposium::OutputEvent>,
        output: &mut serde_json::Value,
        handler: &dyn crate::hook_schema::ErasedAgentHookEvent,
    ) -> Result<(), HookError> {
        if !matches!(
            self.event,
            HookEvent::SessionStart | HookEvent::UserPromptSubmit | HookEvent::PostToolUse
        ) {
            return Ok(());
        }
        if let Some(more) = hook_output.and_then(|o| o.additional_context()) {
            self.text = Some(match self.text.take() {
                Some(text) => format!("{text}\n\n{more}"),
                None => more.to_string(),
            });
        }
        if let Some(text) = &self.text {
            let joined = OutputEvent::with_context(self.event, text.clone());
            merge(
                output,
                handler
                    .try_translate_output(&joined)?
                    .try_to_hook_output()?,
            );
        }
        Ok(())
    }
}

/// Whether a hook's output denies the tool call. A denial is final, like exit
/// 2: otherwise a later hook's decision would overwrite it in [`merge`].
fn denies(output: &symposium::OutputEvent) -> bool {
    matches!(
        output,
        symposium::OutputEvent::PreToolUse(o) if matches!(o.decision, symposium_sdk::hook::Decision::Deny)
    )
}

/// Recursively merge two JSON objects, with `b` taking precedence over `a`.
/// Fields with null values in `b` will delete the corresponding field in `a`.
/// Fields not present in `b` will be left unchanged in `a`.
pub fn merge(a: &mut serde_json::Value, b: serde_json::Value) {
    if let serde_json::Value::Object(a) = a
        && let serde_json::Value::Object(b) = b
    {
        for (k, v) in b {
            if v.is_null() {
                a.remove(&k);
            } else {
                merge(a.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }

        return;
    }

    *a = b;
}

/// Match plugin hooks against the incoming event, selecting at most one hook
/// per plugin based on format priority:
/// 1. A hook whose format matches the host agent (native fidelity).
/// 2. A symposium-format hook (portable fallback).
/// 3. Otherwise, nothing fires for that plugin.
///
/// The resulting `ResolvedHook`s are ready to dispatch without further plugin
/// lookups.
fn dispatched_hooks_for_payload(
    plugins: &[Plugin],
    input: &symposium::InputEvent,
    host_agent: HookAgent,
    ctx: &mut crate::predicate::PredicateContext,
) -> Vec<ResolvedHook> {
    tracing::trace!(event = ?input.event(), plugin_count = plugins.len(), "matching hooks for payload");

    let mut out = Vec::new();

    for parsed_plugin in plugins {
        // Plugin-level predicates gate every hook in the plugin. Evaluated once
        // per plugin per dispatch — keep them cheap.
        if !parsed_plugin.applies(ctx) {
            tracing::debug!(event = ?input.event(), "plugin predicates failed, skipping hooks");
            continue;
        }

        let mut native_match: Option<&crate::plugins::Hook> = None;
        let mut symposium_match: Option<&crate::plugins::Hook> = None;

        for hook in &parsed_plugin.manifest.hooks {
            if hook.event != input.event() {
                continue;
            }
            if let Some(matcher) = &hook.matcher
                && !input.matches_matcher(matcher)
            {
                continue;
            }

            match hook.format.as_agent() {
                Some(agent) if agent == host_agent => {
                    native_match = Some(hook);
                }
                None => {
                    // format = "symposium"
                    symposium_match = Some(hook);
                }
                Some(_) => {
                    // Different agent format — does not fire on this agent.
                }
            }
        }

        let selected = native_match.or(symposium_match);
        if let Some(hook) = selected {
            // Hook-level predicates are evaluated at dispatch so they pick up
            // live state (file present, tool installed, crate present, …).
            if !hook.predicates.evaluate(ctx) {
                tracing::debug!(event = ?input.event(), selected = false, format = ?hook.format, "hook predicates not satisfied");
                continue;
            }
            tracing::debug!(event = ?input.event(), selected = true, format = ?hook.format, "hook selected");
            match ResolvedHook::build(parsed_plugin, hook) {
                Ok(dispatched) => out.push(dispatched),
                Err(_error) => {
                    tracing::warn!(event = ?input.event(), "failed to resolve hook for dispatch");
                }
            }
        } else if parsed_plugin
            .manifest
            .hooks
            .iter()
            .any(|h| h.event == input.event())
        {
            tracing::debug!(event = ?input.event(), selected = false, "no matching format for this agent");
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::pm::{ANY_VERSION, PackageId};

    use super::*;

    const STREAM_CANARY: &str = "HOOK_STREAM_PRIVATE_CANARY";

    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _bytes: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                STREAM_CANARY,
            ))
        }
    }

    #[derive(Default)]
    struct TestWriter {
        bytes: Vec<u8>,
        fail_write: bool,
        fail_flush: bool,
        flushes: usize,
    }
    impl Write for TestWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    STREAM_CANARY,
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            if self.fail_flush {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    STREAM_CANARY,
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn hook_stream_failures_preserve_sources_and_attempt_both_streams() {
        use std::error::Error as _;
        let error = read_hook_input(&mut FailingReader).unwrap_err();
        assert!(matches!(error, HookError::ReadInput { .. }));
        assert_eq!(error.source().unwrap().to_string(), STREAM_CANARY);
        assert!(!error.to_string().contains(STREAM_CANARY));
        let response = HookResponse {
            stdout: b"output".to_vec(),
            stderr: b"reason".to_vec(),
            exit_code: 2,
        };
        let mut stdout = TestWriter {
            fail_write: true,
            ..Default::default()
        };
        let mut stderr = TestWriter {
            fail_flush: true,
            ..Default::default()
        };
        let error = write_hook_response(&response, &mut stdout, &mut stderr).unwrap_err();
        assert!(matches!(
            error,
            HookError::WriteStdout {
                stderr_source: Some(_),
                ..
            }
        ));
        assert_eq!(error.source().unwrap().to_string(), STREAM_CANARY);
        assert!(!error.to_string().contains(STREAM_CANARY));
        assert_eq!(stderr.bytes, b"reason");
        assert_eq!(stderr.flushes, 1);
        assert_eq!(failed_delivery_exit(&response), 2);
    }

    #[test]
    fn response_flush_failures_are_not_reported_as_delivery() {
        let response = HookResponse {
            stdout: b"{}".to_vec(),
            stderr: b"reason".to_vec(),
            exit_code: 0,
        };
        let mut stdout = TestWriter {
            fail_flush: true,
            ..Default::default()
        };
        let mut stderr = TestWriter::default();
        let error = write_hook_response(&response, &mut stdout, &mut stderr).unwrap_err();
        assert!(matches!(error, HookError::WriteStdout { .. }));
        assert_eq!(stdout.bytes, b"{}");
        assert_eq!(stdout.flushes, 1);
        assert_eq!(stderr.bytes, b"reason");
        assert_eq!(stderr.flushes, 1);
        assert_eq!(failed_delivery_exit(&response), 1);

        let mut stdout = TestWriter::default();
        let mut stderr = TestWriter {
            fail_write: true,
            ..Default::default()
        };
        assert!(matches!(
            write_hook_response(&response, &mut stdout, &mut stderr),
            Err(HookError::WriteStderr { .. })
        ));
        assert_eq!(stdout.flushes, 1);
    }

    #[test]
    fn explicit_child_block_precedes_input_failure() {
        use std::error::Error as _;
        for exit in [None, Some(2)] {
            let input = Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                STREAM_CANARY,
            ));
            assert_eq!(
                classify_child_outcome(exit, input).unwrap(),
                ChildDisposition::Blocked
            );
        }
        for exit in [Some(0), Some(1)] {
            let input = Err(std::io::Error::other(STREAM_CANARY));
            let error = classify_child_outcome(exit, input).unwrap_err();
            assert!(matches!(error, HookError::PluginInput { .. }));
            assert_eq!(error.source().unwrap().to_string(), STREAM_CANARY);
            assert!(!error.to_string().contains(STREAM_CANARY));
            assert_eq!(
                classify_child_outcome(exit, Ok(())).unwrap(),
                ChildDisposition::Ready
            );
        }
        assert_eq!(
            classify_child_outcome(
                Some(0),
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            )
            .unwrap(),
            ChildDisposition::Ready,
        );
        assert!(matches!(
            classify_child_outcome(
                Some(1),
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            ),
            Err(HookError::PluginInput { .. }),
        ));
    }

    #[tokio::test]
    async fn successful_child_can_answer_without_consuming_input() {
        let child = Command::new("sh")
            .args(["-c", "exec 0<&-; printf '%s' '{\"PreToolUse\":{}}'"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = vec![b'x'; 2 * 1024 * 1024];
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            collect_plugin_output(child, &input),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, br#"{"PreToolUse":{}}"#);
    }

    #[tokio::test]
    async fn early_child_block_is_reaped_even_when_it_does_not_read_input() {
        let child = Command::new("sh")
            .args(["-c", "printf '%s' 'explicit block' >&2; exit 2"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = vec![b'x'; 2 * 1024 * 1024];
        match collect_plugin_output(child, &input).await {
            Err(HookDispatchError::Blocked(reason)) => assert_eq!(reason, b"explicit block"),
            result => panic!("expected explicit block, got {result:?}"),
        }
    }

    #[test]
    fn env_safe_sanitizes_punctuation() {
        assert_eq!(env_safe("rtk"), "rtk");
        assert_eq!(env_safe("rtk-hooks"), "rtk_hooks");
        assert_eq!(env_safe("a.b-c"), "a_b_c");
        assert_eq!(env_safe("name__req_0"), "name__req_0");
    }

    #[test]
    fn build_env_sets_dir_and_name_vars() {
        // [req (rtk), command (no-source)] order — command was pushed last,
        // so PATH should put its bin dir first.
        let acquired = vec![
            AcquiredInstallation {
                name: "rtk".to_string(),
                base: Some(PathBuf::from("/cache/rtk/1.0")),
                runnable: Some(AcquiredRunnable::ResolvedExec {
                    path: PathBuf::from("/cache/rtk/1.0/bin/rtk"),
                }),
            },
            AcquiredInstallation {
                name: "no-source".to_string(),
                base: None,
                runnable: Some(AcquiredRunnable::ResolvedExec {
                    path: PathBuf::from("/usr/local/bin/tool"),
                }),
            },
        ];
        let env: std::collections::HashMap<_, _> = build_env(&acquired).into_iter().collect();
        assert_eq!(
            env.get("SYMPOSIUM_DIR_rtk").map(String::as_str),
            Some("/cache/rtk/1.0")
        );
        assert_eq!(
            env.get("SYMPOSIUM_rtk").map(String::as_str),
            Some("/cache/rtk/1.0/bin/rtk")
        );
        // No source means no _DIR, but absolute runnable path → SYMPOSIUM_<name> set.
        assert_eq!(env.get("SYMPOSIUM_DIR_no_source"), None);
        assert_eq!(
            env.get("SYMPOSIUM_no_source").map(String::as_str),
            Some("/usr/local/bin/tool")
        );
        // Command (pushed last) wins PATH lookup, so its parent comes first.
        let path = env.get("PATH").expect("PATH set");
        assert!(path.starts_with("/usr/local/bin"), "PATH = {path}");
        assert!(path.contains("/cache/rtk/1.0/bin"), "PATH = {path}");
    }

    #[test]
    fn prepend_to_path_uses_platform_separator() {
        assert!(prepend_to_path(&[]).is_none(), "empty input yields None");

        let dirs = vec!["/first".to_string(), "/second".to_string()];
        let joined = prepend_to_path(&dirs).expect("non-empty dirs yield Some");

        let sep = if cfg!(windows) { ';' } else { ':' };
        assert!(
            joined.starts_with(&format!("/first{sep}/second")),
            "prepended dirs come first, joined by the platform separator: {joined}"
        );
    }

    #[test]
    fn build_env_no_runnable_no_vars() {
        // Pure-setup installation: no runnable means no SYMPOSIUM_<name>
        // and no PATH contribution. SYMPOSIUM_DIR_<name> still gets set
        // when there's a managed base dir.
        let acquired = vec![AcquiredInstallation {
            name: "setup".to_string(),
            base: Some(PathBuf::from("/cache/setup")),
            runnable: None,
        }];
        let env: std::collections::HashMap<_, _> = build_env(&acquired).into_iter().collect();
        assert_eq!(
            env.get("SYMPOSIUM_DIR_setup").map(String::as_str),
            Some("/cache/setup")
        );
        assert!(!env.contains_key("SYMPOSIUM_setup"));
        assert!(!env.contains_key("PATH"));
    }

    #[test]
    fn build_env_global_cargo_skips_env_and_path() {
        // Global cargo: PathLookup runnable. Nothing exposed.
        let acquired = vec![AcquiredInstallation {
            name: "rg".to_string(),
            base: None,
            runnable: Some(AcquiredRunnable::GlobalExec {
                path: PathBuf::from("rg".to_string()),
            }),
        }];
        let env: std::collections::HashMap<_, _> = build_env(&acquired).into_iter().collect();
        assert!(!env.contains_key("SYMPOSIUM_DIR_rg"));
        assert!(!env.contains_key("SYMPOSIUM_rg"));
        assert!(!env.contains_key("PATH"));
    }

    #[tokio::test]
    async fn builtin_pre_tool_use_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::from_dir(tmp.path());
        let deps = sym.workspace_deps(tmp.path());
        let input = symposium::InputEvent::PreToolUse(symposium::PreToolUseInput::new(
            "Bash".to_string(),
            serde_json::Value::default(),
            None,
            None,
        ));
        let output = dispatch_builtin(&sym, &input, &deps, false).await;
        assert!(output.additional_context().is_none());
    }

    #[tokio::test]
    async fn builtin_post_tool_use_returns_empty_when_dependencies_are_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::from_dir(tmp.path());
        let deps = sym.workspace_deps(tmp.path());
        let input = symposium::InputEvent::PostToolUse(symposium::PostToolUseInput::new(
            "Bash".to_string(),
            serde_json::json!({"command": "ls"}),
            serde_json::json!({"stdout": "file.rs"}),
            Some("test-session".to_string()),
            Some("/tmp".to_string()),
        ));
        let output = dispatch_builtin(&sym, &input, &deps, false).await;
        assert!(output.additional_context().is_none());
    }

    #[tokio::test]
    async fn builtin_user_prompt_submit_returns_empty_when_dependencies_are_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::from_dir(tmp.path());
        let deps = sym.workspace_deps(tmp.path());
        let input = symposium::InputEvent::UserPromptSubmit(symposium::UserPromptSubmitInput::new(
            "Use tokio for async".to_string(),
            Some("test-session".to_string()),
            Some("/tmp".to_string()),
        ));
        let output = dispatch_builtin(&sym, &input, &deps, false).await;
        assert!(output.additional_context().is_none());
    }

    #[test]
    fn symposium_output_serializes_with_additional_context() {
        let output = symposium::OutputEvent::with_context(
            HookEvent::UserPromptSubmit,
            "Load tokio guidance".to_string(),
        );
        let ctx = output.additional_context().unwrap();
        assert_eq!(ctx, "Load tokio guidance");
    }

    #[test]
    fn symposium_output_empty_has_no_context() {
        let output = symposium::OutputEvent::empty_for(HookEvent::PreToolUse);
        assert!(output.additional_context().is_none());
    }

    /// Helper: build a minimal plugin with a single PreToolUse hook backed
    /// by a no-op script installation (no `source`, just an on-disk script).
    fn plugin_with_hook(plugin_shell: Vec<&str>, hook_shell: Vec<&str>) -> crate::plugins::Plugin {
        use crate::plugins::{Hook, HookFormat, Installation, PluginManifest};

        let install = Installation {
            name: "no-op".into(),
            requirements: vec![],
            install_commands: vec![],
            source: None,
            executable: None,
            script: Some("/bin/true".into()),
            args: vec![],
        };
        let hook = Hook {
            name: "h".into(),
            event: HookEvent::PreToolUse,
            agent: None,
            matcher: None,
            requirements: vec![],
            command: "no-op".into(),
            executable: None,
            script: None,
            args: vec![],
            format: HookFormat::Symposium,
            predicates: crate::predicate::PredicateSet {
                predicates: hook_shell
                    .into_iter()
                    .map(|c| crate::predicate::Predicate::Shell(c.into()))
                    .collect(),
            },
        };
        // Plugin gate: `depends-on(*)` (always applies) plus the shell predicates.
        let plugin_predicates = std::iter::once(crate::predicate::Predicate::DependsOnWildcard)
            .chain(
                plugin_shell
                    .into_iter()
                    .map(|c| crate::predicate::Predicate::Shell(c.into())),
            )
            .collect();
        let plugin = PluginManifest {
            name: "test-plugin".into(),
            predicates: crate::predicate::PredicateSet {
                predicates: plugin_predicates,
            },
            installations: vec![install],
            hooks: vec![hook],
            skills: vec![],
            mcp_servers: vec![],
            subcommands: BTreeMap::new(),
            custom_predicates: vec![],
            chained: vec![],
            requires_use: false,
        };
        crate::plugins::Plugin {
            manifest: plugin,
            workspace_member: false,
            canonical: PackageId::new("test", "test-plugin", ANY_VERSION),
        }
    }

    fn pre_tool_use_input() -> symposium::InputEvent {
        symposium::InputEvent::PreToolUse(symposium::PreToolUseInput::new(
            "Bash".into(),
            serde_json::json!({}),
            None,
            None,
        ))
    }

    #[test]
    fn dispatch_skips_when_plugin_predicate_fails() {
        let plugin = plugin_with_hook(vec!["false"], vec![]);
        let hooks = dispatched_hooks_for_payload(
            &[plugin],
            &pre_tool_use_input(),
            HookAgent::Claude,
            &mut crate::predicate::PredicateContext::new(&[]),
        );
        assert!(hooks.is_empty(), "plugin-level false should drop all hooks");
    }

    #[test]
    fn dispatch_skips_when_hook_predicate_fails() {
        let plugin = plugin_with_hook(vec![], vec!["false"]);
        let hooks = dispatched_hooks_for_payload(
            &[plugin],
            &pre_tool_use_input(),
            HookAgent::Claude,
            &mut crate::predicate::PredicateContext::new(&[]),
        );
        assert!(hooks.is_empty(), "hook-level false should drop the hook");
    }

    #[test]
    fn dispatch_includes_when_predicates_pass() {
        let plugin = plugin_with_hook(vec!["true"], vec!["true"]);
        let hooks = dispatched_hooks_for_payload(
            &[plugin],
            &pre_tool_use_input(),
            HookAgent::Claude,
            &mut crate::predicate::PredicateContext::new(&[]),
        );
        assert_eq!(hooks.len(), 1);
    }

    /// A plugin gated on a concrete crate must not dispatch its hooks in a
    /// workspace that lacks the crate (regression: hooks used to fire for every
    /// plugin regardless of its `depends-on`).
    #[test]
    fn dispatch_respects_plugin_crate_gate() {
        // Replace the wildcard plugin gate with a concrete `depends-on(serde)`.
        let mut plugin = plugin_with_hook(vec![], vec![]);
        plugin.manifest.predicates = crate::predicate::PredicateSet {
            predicates: vec![crate::predicate::Predicate::DependsOn("serde".into(), None)],
        };

        // No serde in the workspace → the hook is skipped.
        let empty = dispatched_hooks_for_payload(
            &[plugin.clone()],
            &pre_tool_use_input(),
            HookAgent::Claude,
            &mut crate::predicate::PredicateContext::new(&[]),
        );
        assert!(
            empty.is_empty(),
            "crate-gated hook should not fire without the crate"
        );

        // serde present → the hook fires.
        let deps = vec![crate::pm::PackageId::new(
            crate::pm::CARGO_PM,
            "serde",
            "1.0.0",
        )];
        let matched = dispatched_hooks_for_payload(
            &[plugin],
            &pre_tool_use_input(),
            HookAgent::Claude,
            &mut crate::predicate::PredicateContext::new(&deps),
        );
        assert_eq!(
            matched.len(),
            1,
            "crate-gated hook should fire when the crate is present"
        );
    }
}
