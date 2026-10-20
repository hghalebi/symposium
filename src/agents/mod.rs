//! Agent abstraction: hook registration and extension installation paths.
//!
//! Each supported agent has different conventions for where hooks are
//! configured and where skill files are placed. This module centralizes
//! that knowledge.

mod mcp_server_registration;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::config::Symposium;
use crate::output::{Output, display_path};

/// Which of an agent's two MCP configuration levels to write.
///
/// Distinct from [`crate::config::HookScope`]: an agent may support one level
/// and not the other, so this is a preference, not a guarantee (see
/// [`Agent::supports_project_mcp_scope`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpScope {
    /// Applies to one workspace.
    Project,
    /// Applies to every project this user opens.
    User,
}

/// Supported AI agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Antigravity,
    Claude,
    Codex,
    Copilot,
    Goose,
    Kiro,
    OpenCode,
}

impl Agent {
    /// Parse a supported agent name without including untrusted names in diagnostics.
    pub fn try_from_config_name(name: &str) -> std::result::Result<Self, AgentConfigError> {
        match name {
            "antigravity" => Ok(Agent::Antigravity),
            "claude" => Ok(Agent::Claude),
            "codex" => Ok(Agent::Codex),
            "copilot" => Ok(Agent::Copilot),
            "goose" => Ok(Agent::Goose),
            "kiro" => Ok(Agent::Kiro),
            "opencode" => Ok(Agent::OpenCode),
            _ => Err(AgentConfigError::UnknownAgent),
        }
    }

    /// Parse an agent name from a config string.
    ///
    /// Legacy error-erasing adapter retained until the root crate's 0.6 release.
    /// Prefer [`Self::try_from_config_name`] for structured failures.
    pub fn from_config_name(name: &str) -> Result<Self> {
        Self::try_from_config_name(name).map_err(Into::into)
    }

    /// Config name as stored in TOML.
    pub fn config_name(&self) -> &'static str {
        match self {
            Agent::Antigravity => "antigravity",
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Copilot => "copilot",
            Agent::Goose => "goose",
            Agent::Kiro => "kiro",
            Agent::OpenCode => "opencode",
        }
    }

    /// Human-readable display name.
    pub fn display_name(&self) -> &'static str {
        match self {
            Agent::Antigravity => "Antigravity CLI",
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex CLI",
            Agent::Copilot => "GitHub Copilot",
            Agent::Goose => "Goose",
            Agent::Kiro => "Kiro",
            Agent::OpenCode => "OpenCode",
        }
    }

    /// All supported agents for interactive prompts.
    pub fn all() -> &'static [Agent] {
        &[
            Agent::Antigravity,
            Agent::Claude,
            Agent::Codex,
            Agent::Copilot,
            Agent::Goose,
            Agent::Kiro,
            Agent::OpenCode,
        ]
    }

    // -----------------------------------------------------------------------
    // Skill installation paths
    // -----------------------------------------------------------------------

    /// Resolve a project skill directory after validating the single directory name.
    pub fn try_project_skill_dir(
        &self,
        project_root: &Path,
        skill_name: &str,
    ) -> std::result::Result<PathBuf, AgentConfigError> {
        if !crate::skills::is_safe_dir_name(skill_name) {
            return Err(AgentConfigError::UnsafeSkillName);
        }
        Ok(self.project_skill_path(project_root, skill_name))
    }

    /// Project-level skill directory for a given skill name.
    ///
    /// Claude Code requires `.claude/skills/`; Antigravity, Codex and Copilot
    /// support the vendor-neutral
    /// `.agents/skills/` path.
    ///
    /// Legacy infallible adapter retained until the root crate's 0.6 release.
    /// Prefer [`Self::try_project_skill_dir`] when the name is not already validated.
    ///
    /// # Panics
    /// Panics when `skill_name` is not a safe single directory name.
    pub fn project_skill_dir(&self, project_root: &Path, skill_name: &str) -> PathBuf {
        assert!(
            crate::skills::is_safe_dir_name(skill_name),
            "unsafe skill directory name {skill_name:?}"
        );
        self.project_skill_path(project_root, skill_name)
    }

    fn project_skill_path(&self, project_root: &Path, skill_name: &str) -> PathBuf {
        match self {
            Agent::Claude => project_root.join(".claude").join("skills").join(skill_name),
            Agent::Antigravity | Agent::Codex | Agent::Copilot => {
                project_root.join(".agents").join("skills").join(skill_name)
            }
            Agent::Goose => project_root.join(".agents").join("skills").join(skill_name),
            Agent::Kiro => project_root.join(".kiro").join("skills").join(skill_name),
            Agent::OpenCode => project_root.join(".agents").join("skills").join(skill_name),
        }
    }

    /// Resolve a supported global skill directory after validating its name.
    pub fn try_global_skill_dir(
        &self,
        home: &Path,
        skill_name: &str,
    ) -> std::result::Result<Option<PathBuf>, AgentConfigError> {
        if !crate::skills::is_safe_dir_name(skill_name) {
            return Err(AgentConfigError::UnsafeSkillName);
        }
        Ok(self.global_skill_path(home, skill_name))
    }

    /// Global skill directory for a given skill name, if supported.
    ///
    /// Legacy infallible adapter retained until the root crate's 0.6 release.
    /// Prefer [`Self::try_global_skill_dir`] when the name is not already validated.
    ///
    /// # Panics
    /// Panics when `skill_name` is not a safe single directory name.
    pub fn global_skill_dir(&self, home: &Path, skill_name: &str) -> Option<PathBuf> {
        assert!(
            crate::skills::is_safe_dir_name(skill_name),
            "unsafe skill directory name {skill_name:?}"
        );
        self.global_skill_path(home, skill_name)
    }

    fn global_skill_path(&self, home: &Path, skill_name: &str) -> Option<PathBuf> {
        match self {
            // Antigravity's shared config root, the one location all three of its
            // surfaces (CLI, IDE, web) read.
            Agent::Antigravity => Some(
                home.join(".gemini")
                    .join("config")
                    .join("skills")
                    .join(skill_name),
            ),
            Agent::Claude => Some(home.join(".claude").join("skills").join(skill_name)),
            Agent::Codex => Some(home.join(".agents").join("skills").join(skill_name)),
            Agent::Copilot => None, // no global skills path
            Agent::Goose => Some(home.join(".agents").join("skills").join(skill_name)),
            Agent::Kiro => Some(home.join(".kiro").join("skills").join(skill_name)),
            Agent::OpenCode => Some(home.join(".agents").join("skills").join(skill_name)),
        }
    }

    // -----------------------------------------------------------------------
    // Hook registration
    // -----------------------------------------------------------------------

    /// Register hooks in the project-level agent config.
    /// Legacy error-erasing adapter retained until the root crate's 0.6 release.
    /// Prefer [`Self::try_register_project_hooks`] for structured failures.
    pub fn register_project_hooks(
        &self,
        project_root: &Path,
        sym: &Symposium,
        out: &Output,
    ) -> Result<()> {
        self.try_register_project_hooks(project_root, sym, out)
            .map_err(Into::into)
    }

    /// Register project hooks while preserving malformed files and typed failures.
    pub fn try_register_project_hooks(
        &self,
        project_root: &Path,
        _sym: &Symposium,
        out: &Output,
    ) -> std::result::Result<(), AgentConfigError> {
        match self {
            Agent::Antigravity => {
                register_antigravity_hooks(&project_root.join(".agents").join("hooks.json"), out)
            }
            Agent::Claude => {
                register_claude_hooks(&project_root.join(".claude").join("settings.json"), out)
            }
            Agent::Codex => {
                register_codex_hooks(&project_root.join(".codex").join("hooks.json"), out)
            }
            Agent::Copilot => {
                register_copilot_hooks(&project_root.join(".github").join("hooks"), out)
            }
            Agent::Kiro => register_kiro_hooks(&project_root.join(".kiro").join("agents"), out),
            Agent::Goose => {
                out.info(
                    "Goose uses MCP extensions for hooks; skipping hook registration (skills only)",
                );
                Ok(())
            }
            Agent::OpenCode => {
                out.info("OpenCode uses JS/TS plugins for hooks; skipping hook registration (skills only)");
                Ok(())
            }
        }?;

        Ok(())
    }

    /// Register hooks in the global agent config.
    /// Legacy error-erasing adapter retained until the root crate's 0.6 release.
    /// Prefer [`Self::try_register_hooks`] for structured failures.
    pub fn register_hooks(&self, home: &Path, sym: &Symposium, out: &Output) -> Result<()> {
        self.try_register_hooks(home, sym, out).map_err(Into::into)
    }

    /// Register global hooks while preserving malformed files and typed failures.
    pub fn try_register_hooks(
        &self,
        home: &Path,
        _sym: &Symposium,
        out: &Output,
    ) -> std::result::Result<(), AgentConfigError> {
        tracing::debug!(agent = %self.config_name(), "registering hooks");
        // Register hooks
        match self {
            Agent::Antigravity => register_antigravity_hooks(
                &home.join(".gemini").join("config").join("hooks.json"),
                out,
            ),
            Agent::Claude => {
                register_claude_hooks(&home.join(".claude").join("settings.json"), out)
            }
            Agent::Codex => register_codex_hooks(&home.join(".codex").join("hooks.json"), out),
            Agent::Copilot => {
                register_copilot_hooks_global(&home.join(".copilot").join("settings.json"), out)
            }
            Agent::Kiro => register_kiro_hooks(&home.join(".kiro").join("agents"), out),
            Agent::Goose => {
                out.info(
                    "Goose uses MCP extensions for hooks; skipping hook registration (skills only)",
                );
                Ok(())
            }
            Agent::OpenCode => {
                out.info("OpenCode uses JS/TS plugins for hooks; skipping hook registration (skills only)");
                Ok(())
            }
        }?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // MCP server registration
    // -----------------------------------------------------------------------

    /// Where an agent reads MCP servers from.
    ///
    /// Deliberately not the file its *hooks* live in: several agents keep the
    /// two apart, and writing MCP entries into the hooks file means the agent
    /// never sees them.
    ///
    /// Honors each tool's relocation env var (`CLAUDE_CONFIG_DIR`,
    /// `XDG_CONFIG_HOME`), or a user who moved their config gets a file the
    /// agent never reads.
    pub fn mcp_config_path(&self, scope: McpScope, project_root: &Path, home: &Path) -> PathBuf {
        let env_dir = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let xdg_config = || env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));

        match (self, scope) {
            // Project MCP is `.mcp.json`; the user-level file is `.claude.json`.
            // Neither is `settings.json`, which holds hooks.
            (Agent::Claude, McpScope::Project) => project_root.join(".mcp.json"),
            (Agent::Claude, McpScope::User) => env_dir("CLAUDE_CONFIG_DIR")
                .unwrap_or_else(|| home.to_path_buf())
                .join(".claude.json"),

            // Antigravity keeps MCP out of its hooks file, unlike Gemini CLI:
            // `mcp_config.json`, not `hooks.json`.
            (Agent::Antigravity, McpScope::Project) => {
                project_root.join(".agents").join("mcp_config.json")
            }
            (Agent::Antigravity, McpScope::User) => {
                home.join(".gemini").join("config").join("mcp_config.json")
            }

            (Agent::OpenCode, McpScope::Project) => project_root.join("opencode.json"),
            (Agent::OpenCode, McpScope::User) => {
                xdg_config().join("opencode").join("opencode.json")
            }

            (Agent::Kiro, McpScope::Project) => {
                project_root.join(".kiro").join("settings").join("mcp.json")
            }
            (Agent::Kiro, McpScope::User) => home.join(".kiro").join("settings").join("mcp.json"),

            // No project-level MCP config exists for these: the CLI reads the
            // user-level file only (Codex and Copilot verified by asking them
            // from a project holding an entry: both reported none). Project
            // scope therefore resolves to the same user-level file rather than
            // to a file nobody reads.
            (Agent::Codex, _) => home.join(".codex").join("config.toml"),
            (Agent::Copilot, _) => home.join(".copilot").join("mcp-config.json"),
            (Agent::Goose, _) => xdg_config().join("goose").join("config.yaml"),
        }
    }

    /// Whether this agent honors project-scoped MCP registration at all.
    ///
    /// `false` means [`Self::mcp_config_path`] ignores the requested scope and
    /// answers with the user-level file, which callers may want to report.
    pub fn supports_project_mcp_scope(&self) -> bool {
        matches!(
            self,
            Agent::Antigravity | Agent::Claude | Agent::OpenCode | Agent::Kiro
        )
    }

    /// Register MCP servers in the agent's config for `scope`.
    /// Legacy error-erasing adapter retained until the root crate's 0.6 release.
    /// Prefer [`Self::try_register_mcp_servers`] for structured failures.
    pub fn register_mcp_servers(
        &self,
        scope: McpScope,
        project_root: &Path,
        home: &Path,
        servers: &[sacp::schema::McpServer],
        out: &Output,
    ) -> Result<()> {
        self.try_register_mcp_servers(scope, project_root, home, servers, out)
            .map_err(Into::into)
    }

    /// Register MCP servers for the requested scope with structured failures.
    pub fn try_register_mcp_servers(
        &self,
        scope: McpScope,
        project_root: &Path,
        home: &Path,
        servers: &[sacp::schema::McpServer],
        out: &Output,
    ) -> std::result::Result<(), AgentConfigError> {
        tracing::debug!(agent = %self.config_name(), count = servers.len(), ?scope, "registering MCP servers");
        let path = self.mcp_config_path(scope, project_root, home);
        match self {
            Agent::Claude => {
                mcp_server_registration::register_claude_mcp_servers(&path, servers, out)
            }
            Agent::Codex => {
                mcp_server_registration::register_codex_mcp_servers(&path, servers, out)
            }
            Agent::Copilot => {
                mcp_server_registration::register_copilot_mcp_servers(&path, servers, out)
            }
            Agent::Antigravity => {
                mcp_server_registration::register_antigravity_mcp_servers(&path, servers, out)
            }
            Agent::Kiro => mcp_server_registration::register_kiro_mcp_servers(&path, servers, out),
            Agent::Goose => {
                mcp_server_registration::register_goose_mcp_servers(&path, servers, out)
            }
            Agent::OpenCode => {
                mcp_server_registration::register_opencode_mcp_servers(&path, servers, out)
            }
        }
    }

    /// Remove MCP servers from the agent's config for `scope`.
    /// Legacy error-erasing adapter retained until the root crate's 0.6 release.
    /// Prefer [`Self::try_unregister_mcp_servers`] for structured failures.
    pub fn unregister_mcp_servers(
        &self,
        scope: McpScope,
        project_root: &Path,
        home: &Path,
        names: &[&str],
        out: &Output,
    ) -> Result<()> {
        self.try_unregister_mcp_servers(scope, project_root, home, names, out)
            .map_err(Into::into)
    }

    /// Remove only the named MCP servers, preserving unrelated agent settings.
    pub fn try_unregister_mcp_servers(
        &self,
        scope: McpScope,
        project_root: &Path,
        home: &Path,
        names: &[&str],
        out: &Output,
    ) -> std::result::Result<(), AgentConfigError> {
        let path = self.mcp_config_path(scope, project_root, home);
        match self {
            Agent::Claude => {
                mcp_server_registration::unregister_claude_mcp_servers(&path, names, out)
            }
            Agent::Codex => {
                mcp_server_registration::unregister_codex_mcp_servers(&path, names, out)
            }
            Agent::Copilot => {
                mcp_server_registration::unregister_copilot_mcp_servers(&path, names, out)
            }
            Agent::Antigravity => {
                mcp_server_registration::unregister_antigravity_mcp_servers(&path, names, out)
            }
            Agent::Kiro => mcp_server_registration::unregister_kiro_mcp_servers(&path, names, out),
            Agent::Goose => {
                mcp_server_registration::unregister_goose_mcp_servers(&path, names, out)
            }
            Agent::OpenCode => {
                mcp_server_registration::unregister_opencode_mcp_servers(&path, names, out)
            }
        }
    }

    /// Remove hooks from the project-level agent config.
    ///
    /// Legacy best-effort adapter retained until the root crate's 0.6 release.
    /// Rejected configuration remains unchanged and the failure is reported.
    pub fn unregister_project_hooks(&self, project_root: &Path, sym: &Symposium, out: &Output) {
        if let Err(error) = self.try_unregister_project_hooks(project_root, sym, out) {
            out.warn(error);
        }
    }

    /// Remove project hooks while preserving structured failures.
    pub fn try_unregister_project_hooks(
        &self,
        project_root: &Path,
        _sym: &Symposium,
        out: &Output,
    ) -> std::result::Result<(), AgentConfigError> {
        let (path, layout) = match self {
            Agent::Antigravity => (
                project_root.join(".agents/hooks.json"),
                HookConfigLayout::Antigravity,
            ),
            Agent::Claude => (
                project_root.join(".claude/settings.json"),
                HookConfigLayout::Grouped,
            ),
            Agent::Codex => (
                project_root.join(".codex/hooks.json"),
                HookConfigLayout::Grouped,
            ),
            Agent::Copilot => (
                project_root.join(".github/hooks/symposium.json"),
                HookConfigLayout::CopilotProject,
            ),
            Agent::Kiro => (
                project_root.join(".kiro/agents/symposium.json"),
                HookConfigLayout::Kiro,
            ),
            Agent::Goose | Agent::OpenCode => return Ok(()),
        };
        try_unregister_hook_file_with_output(&path, layout, out)
    }

    /// Remove hooks from the global agent config.
    ///
    /// Legacy best-effort adapter retained until the root crate's 0.6 release.
    /// Rejected configuration remains unchanged and the failure is reported.
    pub fn unregister_hooks(&self, home: &Path, sym: &Symposium, out: &Output) {
        if let Err(error) = self.try_unregister_hooks(home, sym, out) {
            out.warn(error);
        }
    }

    /// Remove global hooks while preserving structured failures.
    pub fn try_unregister_hooks(
        &self,
        home: &Path,
        _sym: &Symposium,
        out: &Output,
    ) -> std::result::Result<(), AgentConfigError> {
        let (path, layout) = match self {
            Agent::Antigravity => (
                home.join(".gemini/config/hooks.json"),
                HookConfigLayout::Antigravity,
            ),
            Agent::Claude => (
                home.join(".claude/settings.json"),
                HookConfigLayout::Grouped,
            ),
            Agent::Codex => (home.join(".codex/hooks.json"), HookConfigLayout::Grouped),
            Agent::Copilot => (
                home.join(".copilot/settings.json"),
                HookConfigLayout::CopilotSettings,
            ),
            Agent::Kiro => (
                home.join(".kiro/agents/symposium.json"),
                HookConfigLayout::Kiro,
            ),
            Agent::Goose | Agent::OpenCode => return Ok(()),
        };
        try_unregister_hook_file_with_output(&path, layout, out)
    }
}

// ---------------------------------------------------------------------------
// Claude Code hook registration
// ---------------------------------------------------------------------------

fn register_claude_hooks(
    settings_path: &Path,
    out: &Output,
) -> std::result::Result<(), AgentConfigError> {
    let mut settings = load_json_or_empty(settings_path)?;
    let display = display_path(settings_path);
    let root = json_object_mut(&mut settings, settings_path, "")?;
    let hooks = json_object_entry_mut(root, "hooks", settings_path, "/hooks")?;
    let mut added = Vec::new();

    for event in [
        "PreToolUse",
        "PostToolUse",
        "UserPromptSubmit",
        "SessionStart",
        "Stop",
    ] {
        let command = format!("cargo-agents hook claude {}", event_to_cli_arg(event));
        let entry = json!({
            "matcher": "*",
            "hooks": [{ "type": "command", "command": command }]
        });
        if ensure_hook_entry(
            hooks,
            event,
            entry,
            settings_path,
            "/hooks",
            HookEntryShape::Grouped,
        )? {
            added.push(event);
        }
    }

    if added.is_empty() {
        out.already_ok(format!("{display}: hooks already registered"));
    } else {
        save_json(settings_path, &settings)?;
        out.done(format!("{display}: added hooks ({})", added.join(", ")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Codex CLI hook registration
// ---------------------------------------------------------------------------

fn register_codex_hooks(
    hooks_path: &Path,
    out: &Output,
) -> std::result::Result<(), AgentConfigError> {
    let mut settings = load_json_or_empty(hooks_path)?;
    let display = display_path(hooks_path);
    let root = json_object_mut(&mut settings, hooks_path, "")?;
    let hooks = json_object_entry_mut(root, "hooks", hooks_path, "/hooks")?;
    let mut added = Vec::new();

    for event in [
        "PreToolUse",
        "PostToolUse",
        "UserPromptSubmit",
        "SessionStart",
    ] {
        let command = format!("cargo-agents hook codex {}", event_to_cli_arg(event));
        let entry = json!({
            "matcher": "",
            "hooks": [{ "type": "command", "command": command, "timeout": 10 }]
        });
        if ensure_hook_entry(
            hooks,
            event,
            entry,
            hooks_path,
            "/hooks",
            HookEntryShape::Grouped,
        )? {
            added.push(event);
        }
    }

    if added.is_empty() {
        out.already_ok(format!("{display}: hooks already registered"));
    } else {
        save_json(hooks_path, &settings)?;
        out.done(format!("{display}: added hooks ({})", added.join(", ")));
    }
    Ok(())
}

#[cfg(test)]
fn unregister_codex_hooks(hooks_path: &Path, out: &Output) {
    unregister_hook_file(hooks_path, HookConfigLayout::Grouped, out);
}

// ---------------------------------------------------------------------------
// GitHub Copilot hook registration
// ---------------------------------------------------------------------------

/// Register hooks in the global Copilot user settings file (`~/.copilot/settings.json`).
fn register_copilot_hooks_global(
    config_path: &Path,
    out: &Output,
) -> std::result::Result<(), AgentConfigError> {
    register_copilot_hooks_file(config_path, false, out)
}

/// Register hooks in a project-level Copilot hooks directory (`.github/hooks/`).
fn register_copilot_hooks(
    hooks_dir: &Path,
    out: &Output,
) -> std::result::Result<(), AgentConfigError> {
    register_copilot_hooks_file(&hooks_dir.join("symposium.json"), true, out)
}

fn register_copilot_hooks_file(
    config_path: &Path,
    project: bool,
    out: &Output,
) -> std::result::Result<(), AgentConfigError> {
    let display = display_path(config_path);
    let mut config = load_json_or_empty(config_path)?;
    let root = json_object_mut(&mut config, config_path, "")?;
    let mut changed = false;
    if project {
        match root.get("version") {
            Some(version) if version == &json!(1) => {}
            Some(_) => return Err(config_shape(config_path, "/version", "version 1")),
            None => {
                root.insert("version".into(), json!(1));
                changed = true;
            }
        }
    }
    let hooks = json_object_entry_mut(root, "hooks", config_path, "/hooks")?;
    for (event, entry) in copilot_hook_entries() {
        changed |= ensure_hook_entry(
            hooks,
            event,
            entry,
            config_path,
            "/hooks",
            HookEntryShape::Flat("bash"),
        )?;
    }
    if changed {
        save_json(config_path, &config)?;
        out.done(format!("{display}: added hooks"));
    } else {
        out.already_ok(format!("{display}: hooks already registered"));
    }
    Ok(())
}

/// Copilot hook entries shared by global and project registration.
fn copilot_hook_entries() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (
            "preToolUse",
            json!({
                "type": "command",
                "bash": "cargo-agents hook copilot pre-tool-use",
                "timeoutSec": 10
            }),
        ),
        (
            "postToolUse",
            json!({
                "type": "command",
                "bash": "cargo-agents hook copilot post-tool-use",
                "timeoutSec": 10
            }),
        ),
        (
            "userPromptSubmitted",
            json!({
                "type": "command",
                "bash": "cargo-agents hook copilot user-prompt-submit",
                "timeoutSec": 10
            }),
        ),
        (
            "sessionStart",
            json!({
                "type": "command",
                "bash": "cargo-agents hook copilot session-start",
                "timeoutSec": 10
            }),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Antigravity CLI hook registration
// ---------------------------------------------------------------------------

/// The key symposium owns in Antigravity's `hooks.json`. Registrations there are
/// keyed by a hook *name*, so several tools can share one file; symposium only
/// ever writes and reaps this one entry.
const ANTIGRAVITY_HOOK_NAME: &str = "symposium";

/// Antigravity's events, paired with the symposium event each maps to.
///
/// `PreInvocation` stands in for `user-prompt-submit`: Antigravity has no
/// prompt event, and `PreInvocation` fires before every model call, so only the
/// first invocation of a turn dispatches. `SessionStart` is undocumented but
/// real; it fires with the first turn of a new conversation and never on resume.
const ANTIGRAVITY_EVENTS: &[(&str, &str)] = &[
    ("PreToolUse", "pre-tool-use"),
    ("PostToolUse", "post-tool-use"),
    ("PreInvocation", "user-prompt-submit"),
    ("SessionStart", "session-start"),
    ("Stop", "stop"),
];

/// Tool events wrap their handlers in a `matcher` group; lifecycle events take a
/// flat list of handlers. Getting this wrong is silent — Antigravity accepts an
/// unrecognised shape and simply never fires it.
fn antigravity_is_tool_event(event: &str) -> bool {
    matches!(event, "PreToolUse" | "PostToolUse")
}

fn antigravity_handler(cli_arg: &str) -> serde_json::Value {
    json!({
        "type": "command",
        "command": format!("cargo-agents hook antigravity {cli_arg}"),
        "timeout": 30,
    })
}

fn register_antigravity_hooks(
    hooks_path: &Path,
    out: &Output,
) -> std::result::Result<(), AgentConfigError> {
    let mut config = load_json_or_empty(hooks_path)?;
    let display = display_path(hooks_path);
    let root = json_object_mut(&mut config, hooks_path, "")?;
    let hooks = json_object_entry_mut(root, ANTIGRAVITY_HOOK_NAME, hooks_path, "/symposium")?;
    let mut added = Vec::new();
    for (event, cli_arg) in ANTIGRAVITY_EVENTS {
        let handler = antigravity_handler(cli_arg);
        let (entry, shape) = if antigravity_is_tool_event(event) {
            (
                json!({ "matcher": "*", "hooks": [handler] }),
                HookEntryShape::Grouped,
            )
        } else {
            (handler, HookEntryShape::Flat("command"))
        };
        if ensure_hook_entry(hooks, event, entry, hooks_path, "/symposium", shape)? {
            added.push(*event);
        }
    }
    if added.is_empty() {
        out.already_ok(format!("{display}: hooks already registered"));
    } else {
        save_json(hooks_path, &config)?;
        out.done(format!("{display}: added hooks ({})", added.join(", ")));
    }
    Ok(())
}

/// Remove only symposium's own named entry, leaving hooks other tools or the
/// user registered in the same file untouched.
#[cfg(test)]
fn unregister_antigravity_hooks(hooks_path: &Path, out: &Output) {
    unregister_hook_file(hooks_path, HookConfigLayout::Antigravity, out);
}
// ---------------------------------------------------------------------------
// Kiro hook registration
// ---------------------------------------------------------------------------

/// Merge Kiro hook entries into a JSON config, returning the list of newly added events.
///
/// Also ensures the required `name` field is present (Kiro validates it on load).
/// Returns `(changed, added_events)` — `changed` is true if any field was
/// inserted (not just hooks), so the caller knows to save the file.
fn merge_kiro_hooks(
    config: &mut serde_json::Value,
    default_name: &str,
    path: &Path,
) -> std::result::Result<(bool, Vec<&'static str>), AgentConfigError> {
    let obj = json_object_mut(config, path, "")?;
    validate_kiro_fields(obj, path)?;
    let mut changed = false;

    // Use a helper to track insertions.
    let mut ensure = |key: &str, value: serde_json::Value| {
        if !obj.contains_key(key) {
            obj.insert(key.to_string(), value);
            changed = true;
        }
    };

    ensure("name", json!(default_name));

    // Without `tools`, the agent has zero tools available.
    ensure("tools", json!(["*"]));

    // Auto-discover skills from the standard locations.
    ensure("resources", json!(["skill://.kiro/skills/**/SKILL.md",]));

    changed |= !obj.contains_key("hooks");
    let hooks_obj = json_object_entry_mut(obj, "hooks", path, "/hooks")?;

    let mut added = Vec::new();
    for (event, entry) in kiro_hook_entries() {
        if ensure_hook_entry(
            hooks_obj,
            event,
            entry,
            path,
            "/hooks",
            HookEntryShape::Flat("command"),
        )? {
            added.push(event);
            changed = true;
        }
    }
    Ok((changed, added))
}

/// Register hooks by creating a Kiro agent file (`.kiro/agents/symposium.json`).
fn register_kiro_hooks(
    agents_dir: &Path,
    out: &Output,
) -> std::result::Result<(), AgentConfigError> {
    let hook_file = agents_dir.join("symposium.json");
    let display = display_path(&hook_file);

    let mut config = load_json_or_empty(&hook_file)?;
    let (changed, added) = merge_kiro_hooks(&mut config, "symposium", &hook_file)?;

    if !changed {
        out.already_ok(format!("{display}: hooks already registered"));
    } else {
        save_json(&hook_file, &config)?;
        if added.is_empty() {
            out.done(format!("{display}: updated agent config"));
        } else {
            out.done(format!("{display}: added hooks ({})", added.join(", ")));
        }
    }

    Ok(())
}

/// Kiro hook entries for all supported events.
fn kiro_hook_entries() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (
            "preToolUse",
            json!({
                "matcher": "*",
                "command": "cargo-agents hook kiro pre-tool-use"
            }),
        ),
        (
            "postToolUse",
            json!({
                "matcher": "*",
                "command": "cargo-agents hook kiro post-tool-use"
            }),
        ),
        (
            "userPromptSubmit",
            json!({
                "command": "cargo-agents hook kiro user-prompt-submit"
            }),
        ),
        (
            "agentSpawn",
            json!({
                "command": "cargo-agents hook kiro session-start"
            }),
        ),
    ]
}

/// Remove the symposium agent file from a Kiro agents directory.
#[cfg(test)]
fn unregister_kiro_hooks(agents_dir: &Path, out: &Output) {
    unregister_hook_file(
        &agents_dir.join("symposium.json"),
        HookConfigLayout::Kiro,
        out,
    );
}

// ---------------------------------------------------------------------------
// Hook unregistration
// ---------------------------------------------------------------------------

#[cfg(test)]
fn unregister_claude_hooks(settings_path: &Path, out: &Output) {
    unregister_hook_file(settings_path, HookConfigLayout::Grouped, out);
}

/// Remove symposium hooks from a Copilot project hooks directory.
#[cfg(test)]
fn unregister_copilot_hooks(hooks_dir: &Path, out: &Output) {
    unregister_hook_file(
        &hooks_dir.join("symposium.json"),
        HookConfigLayout::CopilotProject,
        out,
    );
}

/// Remove symposium hooks from the global Copilot config.
#[cfg(test)]
fn unregister_copilot_hooks_global(config_path: &Path, out: &Output) {
    unregister_hook_file(config_path, HookConfigLayout::CopilotSettings, out);
}

#[derive(Clone, Copy)]
enum HookConfigLayout {
    Grouped,
    CopilotSettings,
    CopilotProject,
    Kiro,
    Antigravity,
}

impl HookConfigLayout {
    fn container(self) -> &'static str {
        match self {
            Self::Antigravity => ANTIGRAVITY_HOOK_NAME,
            Self::Grouped | Self::CopilotSettings | Self::CopilotProject | Self::Kiro => "hooks",
        }
    }

    fn event_shape(self, event: &str) -> Option<HookEntryShape> {
        match self {
            Self::Grouped
                if matches!(
                    event,
                    "PreToolUse" | "PostToolUse" | "UserPromptSubmit" | "SessionStart" | "Stop"
                ) =>
            {
                Some(HookEntryShape::Grouped)
            }
            Self::CopilotSettings | Self::CopilotProject
                if matches!(
                    event,
                    "preToolUse" | "postToolUse" | "userPromptSubmitted" | "sessionStart"
                ) =>
            {
                Some(HookEntryShape::Flat("bash"))
            }
            Self::Kiro
                if matches!(
                    event,
                    "preToolUse" | "postToolUse" | "userPromptSubmit" | "agentSpawn"
                ) =>
            {
                Some(HookEntryShape::Flat("command"))
            }
            Self::Antigravity if ANTIGRAVITY_EVENTS.iter().any(|(known, _)| *known == event) => {
                Some(if antigravity_is_tool_event(event) {
                    HookEntryShape::Grouped
                } else {
                    HookEntryShape::Flat("command")
                })
            }
            Self::Grouped
            | Self::CopilotSettings
            | Self::CopilotProject
            | Self::Kiro
            | Self::Antigravity => None,
        }
    }
}

fn try_unregister_hook_file_with_output(
    path: &Path,
    layout: HookConfigLayout,
    out: &Output,
) -> std::result::Result<(), AgentConfigError> {
    if try_unregister_hook_file(path, layout)? {
        out.removed(format!("{}: removed hooks", display_path(path)));
    }
    Ok(())
}

#[cfg(test)]
fn unregister_hook_file(path: &Path, layout: HookConfigLayout, out: &Output) {
    match try_unregister_hook_file(path, layout) {
        Ok(true) => out.removed(format!("{}: removed hooks", display_path(path))),
        Ok(false) => {}
        Err(error) => out.warn(error),
    }
}

fn try_unregister_hook_file(
    path: &Path,
    layout: HookConfigLayout,
) -> std::result::Result<bool, AgentConfigError> {
    let mut config = load_json_or_empty(path)?;
    let root = json_object_mut(&mut config, path, "")?;
    if matches!(layout, HookConfigLayout::CopilotProject)
        && root
            .get("version")
            .is_some_and(|version| version != &json!(1))
    {
        return Err(config_shape(path, "/version", "version 1"));
    }
    if let HookConfigLayout::Kiro = layout {
        validate_kiro_fields(root, path)?;
    }
    let container = layout.container();
    let Some(value) = root.get_mut(container) else {
        return Ok(false);
    };
    let parent_pointer = format!("/{container}");
    let hooks = json_object_mut(value, path, &parent_pointer)?;
    for (event, value) in hooks.iter() {
        if let Some(shape) = layout.event_shape(event) {
            let pointer = format!("{parent_pointer}/{event}");
            let entries = value
                .as_array()
                .ok_or_else(|| config_shape(path, &pointer, "array"))?;
            validate_hook_entries(entries, path, &pointer, shape)?;
        }
    }

    let mut changed = false;
    for (event, value) in hooks.iter_mut() {
        let Some(shape) = layout.event_shape(event) else {
            continue;
        };
        let pointer = format!("{parent_pointer}/{event}");
        let entries = value
            .as_array_mut()
            .ok_or_else(|| config_shape(path, &pointer, "array"))?;
        match shape {
            HookEntryShape::Flat(key) => {
                let before = entries.len();
                entries.retain(|entry| !has_hook_command(entry, key));
                changed |= before != entries.len();
            }
            HookEntryShape::Grouped => {
                let mut remaining = Vec::with_capacity(entries.len());
                for (index, mut group) in std::mem::take(entries).into_iter().enumerate() {
                    let pointer = format!("{pointer}/{index}/hooks");
                    let nested = group
                        .get_mut("hooks")
                        .and_then(Value::as_array_mut)
                        .ok_or_else(|| config_shape(path, &pointer, "array"))?;
                    let before = nested.len();
                    nested.retain(|entry| !has_hook_command(entry, "command"));
                    let removed = before != nested.len();
                    changed |= removed;
                    if !removed || !nested.is_empty() {
                        remaining.push(group);
                    }
                }
                *entries = remaining;
            }
        }
    }
    if !changed {
        return Ok(false);
    }
    if let HookConfigLayout::Antigravity = layout {
        hooks.retain(|event, value| {
            !(layout.event_shape(event).is_some() && value.as_array().is_some_and(Vec::is_empty))
        });
        if hooks.is_empty() {
            root.remove(container);
        }
    }
    if contains_only_generated_settings(&config, layout) {
        fs::remove_file(path).map_err(|source| AgentConfigError::Remove {
            path: path.to_path_buf(),
            source,
        })?;
    } else {
        save_json(path, &config)?;
    }
    Ok(true)
}

fn contains_only_generated_settings(config: &Value, layout: HookConfigLayout) -> bool {
    let defaults = match layout {
        HookConfigLayout::CopilotProject => json!({"version": 1}),
        HookConfigLayout::Kiro => {
            json!({"name": "symposium", "tools": ["*"], "resources": ["skill://.kiro/skills/**/SKILL.md"]})
        }
        HookConfigLayout::Grouped
        | HookConfigLayout::CopilotSettings
        | HookConfigLayout::Antigravity => return false,
    };
    config.as_object().is_some_and(|root| {
        root.iter().all(|(key, value)| {
            if key == "hooks" {
                value.as_object().is_some_and(|hooks| {
                    hooks.iter().all(|(event, entries)| {
                        layout.event_shape(event).is_some()
                            && entries.as_array().is_some_and(Vec::is_empty)
                    })
                })
            } else {
                defaults.get(key) == Some(value)
            }
        })
    })
}

fn validate_kiro_fields(
    object: &Map<String, Value>,
    path: &Path,
) -> std::result::Result<(), AgentConfigError> {
    for (key, expected, matches) in [
        ("name", "string", Value::is_string as fn(&Value) -> bool),
        ("tools", "array", Value::is_array),
        ("resources", "array", Value::is_array),
    ] {
        if object.get(key).is_some_and(|value| !matches(value)) {
            return Err(config_shape(path, &format!("/{key}"), expected));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn event_to_cli_arg(event: &str) -> &str {
    match event {
        "PreToolUse" | "preToolUse" => "pre-tool-use",
        "PostToolUse" | "postToolUse" => "post-tool-use",
        "UserPromptSubmit" | "userPromptSubmit" => "user-prompt-submit",
        "SessionStart" | "sessionStart" | "agentSpawn" => "session-start",
        "Stop" | "stop" => "stop",
        other => other,
    }
}

/// Agent configuration failures with content-free diagnostics and structured sources.
///
/// Canonical `try_*` methods return this type directly. Legacy registration methods
/// retain their compatible `anyhow::Result` signatures through the root 0.6 release.
/// Format the top-level error for diagnostics: parser source displays can contain
/// configuration contents.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AgentConfigError {
    /// A skill name is empty, traverses directories, or is otherwise unsafe as one component.
    #[error("unsafe skill directory name")]
    UnsafeSkillName,
    /// The requested name is not one of the supported agents.
    #[error(
        "unknown agent (expected antigravity, claude, codex, copilot, goose, kiro, or opencode)"
    )]
    UnknownAgent,
    /// A newer transport variant cannot be represented safely by this adapter.
    #[error("unsupported MCP server transport")]
    UnsupportedMcpServer,
    /// The agent configuration could not be read.
    #[error("failed to read agent configuration {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Existing JSON could not be parsed.
    #[error("invalid JSON in agent configuration {path}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// Existing TOML could not be parsed.
    #[error("invalid TOML in agent configuration {path}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    /// The destination directory could not be created.
    #[error("failed to create agent configuration directory {path}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The proposed JSON could not be serialized.
    #[error("failed to serialize agent configuration {path}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// The replacement contents could not be written.
    #[error("failed to write agent configuration {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The atomic replacement could not be completed.
    #[error("failed to replace agent configuration {path}")]
    Replace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A generated configuration file could not be removed.
    #[error("failed to remove agent configuration {path}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// An existing value has an incompatible shape at the specified pointer.
    #[error("agent configuration {path} at {pointer:?}: expected {expected}")]
    Shape {
        path: PathBuf,
        pointer: String,
        expected: &'static str,
    },
}

fn config_shape(path: &Path, pointer: &str, expected: &'static str) -> AgentConfigError {
    AgentConfigError::Shape {
        path: path.to_path_buf(),
        pointer: pointer.into(),
        expected,
    }
}

fn json_object_mut<'a>(
    value: &'a mut Value,
    path: &Path,
    pointer: &str,
) -> std::result::Result<&'a mut Map<String, Value>, AgentConfigError> {
    value
        .as_object_mut()
        .ok_or_else(|| config_shape(path, pointer, "object"))
}

fn json_object_entry_mut<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
    pointer: &str,
) -> std::result::Result<&'a mut Map<String, Value>, AgentConfigError> {
    json_object_mut(
        object.entry(key).or_insert_with(|| json!({})),
        path,
        pointer,
    )
}

fn json_array_entry_mut<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
    pointer: &str,
) -> std::result::Result<&'a mut Vec<Value>, AgentConfigError> {
    object
        .entry(key)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| config_shape(path, pointer, "array"))
}

#[derive(Clone, Copy)]
enum HookEntryShape {
    Grouped,
    Flat(&'static str),
}

fn validate_hook_entries(
    entries: &[Value],
    path: &Path,
    pointer: &str,
    shape: HookEntryShape,
) -> std::result::Result<(), AgentConfigError> {
    for (index, entry) in entries.iter().enumerate() {
        let pointer = format!("{pointer}/{index}");
        let object = entry
            .as_object()
            .ok_or_else(|| config_shape(path, &pointer, "object"))?;
        if object
            .get("matcher")
            .is_some_and(|matcher| !matcher.is_string())
        {
            return Err(config_shape(path, &format!("{pointer}/matcher"), "string"));
        }
        if let HookEntryShape::Flat(key) = shape
            && object.get(key).is_some_and(|command| !command.is_string())
        {
            return Err(config_shape(path, &format!("{pointer}/{key}"), "string"));
        }
        if let HookEntryShape::Grouped = shape {
            let nested_pointer = format!("{pointer}/hooks");
            let hooks = object
                .get("hooks")
                .and_then(Value::as_array)
                .ok_or_else(|| config_shape(path, &nested_pointer, "array"))?;
            validate_hook_entries(
                hooks,
                path,
                &nested_pointer,
                HookEntryShape::Flat("command"),
            )?;
        }
    }
    Ok(())
}

fn has_hook_command(entry: &Value, key: &str) -> bool {
    entry
        .get(key)
        .and_then(Value::as_str)
        .and_then(|command| command.strip_prefix("cargo-agents hook"))
        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace))
}

fn ensure_hook_entry(
    hooks: &mut Map<String, Value>,
    event: &str,
    entry: Value,
    path: &Path,
    parent_pointer: &str,
    shape: HookEntryShape,
) -> std::result::Result<bool, AgentConfigError> {
    let pointer = format!("{parent_pointer}/{event}");
    let entries = json_array_entry_mut(hooks, event, path, &pointer)?;
    validate_hook_entries(entries, path, &pointer, shape)?;
    let already_registered = entries.iter().any(|entry| match shape {
        HookEntryShape::Grouped => entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| hooks.iter().any(|hook| has_hook_command(hook, "command"))),
        HookEntryShape::Flat(key) => has_hook_command(entry, key),
    });
    if already_registered {
        return Ok(false);
    }
    entries.push(entry);
    Ok(true)
}

fn load_json_or_empty(path: &Path) -> std::result::Result<Value, AgentConfigError> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(source) => {
            return Err(AgentConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if contents.trim().is_empty() {
        return Ok(json!({}));
    }
    let mut value = serde_json::from_str(&contents).map_err(|source| AgentConfigError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    json_object_mut(&mut value, path, "")?;
    Ok(value)
}

/// Write JSON config, replacing the file atomically.
///
/// Temp file plus rename, because some of these are live agent state (Claude
/// Code rewrites `~/.claude.json` throughout a session) and a truncating write
/// that loses a race leaves a document the agent cannot parse.
///
/// Bounds torn reads, not lost updates. What keeps that window from mattering is
/// that registration writes only when an entry actually differs.
fn save_json(path: &Path, value: &serde_json::Value) -> std::result::Result<(), AgentConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| AgentConfigError::CreateDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let contents =
        serde_json::to_string_pretty(value).map_err(|source| AgentConfigError::Serialize {
            path: path.to_path_buf(),
            source,
        })?;

    // Pid-suffixed so two concurrent syncs cannot share a temp path.
    let temp = path.with_extension(format!("symposium-tmp-{}", std::process::id()));
    fs::write(&temp, contents).map_err(|source| AgentConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    if let Err(source) = fs::rename(&temp, path) {
        if let Err(cleanup) = fs::remove_file(&temp) {
            tracing::warn!(error_kind = ?cleanup.kind(), "failed to remove temporary agent configuration");
        }
        return Err(AgentConfigError::Replace {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_agent_name_rejects_without_echoing_untrusted_input() {
        let error = Agent::try_from_config_name("synthetic-secret-canary").unwrap_err();
        assert!(matches!(error, AgentConfigError::UnknownAgent));
        assert!(!error.to_string().contains("synthetic-secret-canary"));
        let legacy = Agent::from_config_name("synthetic-secret-canary").unwrap_err();
        assert!(matches!(
            legacy.downcast_ref::<AgentConfigError>(),
            Some(AgentConfigError::UnknownAgent)
        ));
    }

    #[test]
    fn checked_skill_paths_validate_without_panicking_or_changing_valid_paths() {
        let base = Path::new("root");
        for agent in Agent::all() {
            for name in ["", ".", "..", "../outside", "synthetic-secret-canary/child"] {
                for error in [
                    agent.try_project_skill_dir(base, name).unwrap_err(),
                    agent.try_global_skill_dir(base, name).unwrap_err(),
                ] {
                    assert!(matches!(error, AgentConfigError::UnsafeSkillName));
                    assert!(!error.to_string().contains("synthetic-secret-canary"));
                }
            }
            assert_eq!(
                agent.try_project_skill_dir(base, "safe-skill").unwrap(),
                agent.project_skill_dir(base, "safe-skill"),
            );
            assert_eq!(
                agent.try_global_skill_dir(base, "safe-skill").unwrap(),
                agent.global_skill_dir(base, "safe-skill"),
            );
        }
    }

    #[test]
    fn checked_public_hook_changes_preserve_parser_sources_and_rejected_bytes() {
        use std::error::Error as _;

        for (agent, file) in [
            (Agent::Antigravity, ".agents/hooks.json"),
            (Agent::Claude, ".claude/settings.json"),
            (Agent::Codex, ".codex/hooks.json"),
            (Agent::Copilot, ".github/hooks/symposium.json"),
            (Agent::Kiro, ".kiro/agents/symposium.json"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let sym = Symposium::try_from_dir(&tmp.path().join("symposium")).unwrap();
            let path = tmp.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original = "{invalid-json synthetic-secret-canary";
            fs::write(&path, original).unwrap();
            for error in [
                agent
                    .try_register_project_hooks(tmp.path(), &sym, &Output::quiet())
                    .unwrap_err(),
                agent
                    .try_unregister_project_hooks(tmp.path(), &sym, &Output::quiet())
                    .unwrap_err(),
            ] {
                assert!(matches!(error, AgentConfigError::Json { .. }));
                assert!(error.source().unwrap().is::<serde_json::Error>());
                assert!(!error.to_string().contains("synthetic-secret-canary"));
                assert_eq!(fs::read_to_string(&path).unwrap(), original);
            }
            let legacy = agent
                .register_project_hooks(tmp.path(), &sym, &Output::quiet())
                .unwrap_err();
            assert!(matches!(
                legacy.downcast_ref::<AgentConfigError>(),
                Some(AgentConfigError::Json { .. })
            ));
        }
    }

    type HookRegistrar = fn(&Path, &Output) -> std::result::Result<(), AgentConfigError>;

    struct HookRegistration {
        register: HookRegistrar,
        unregister: fn(&Path, &Output),
        target: &'static str,
        file: &'static str,
        container: &'static str,
        event: &'static str,
        shape: HookEntryShape,
    }

    fn hook_registrations() -> [HookRegistration; 6] {
        [
            HookRegistration {
                register: register_claude_hooks,
                unregister: unregister_claude_hooks,
                target: "claude.json",
                file: "claude.json",
                container: "hooks",
                event: "PreToolUse",
                shape: HookEntryShape::Grouped,
            },
            HookRegistration {
                register: register_codex_hooks,
                unregister: unregister_codex_hooks,
                target: "codex.json",
                file: "codex.json",
                container: "hooks",
                event: "PreToolUse",
                shape: HookEntryShape::Grouped,
            },
            HookRegistration {
                register: register_copilot_hooks_global,
                unregister: unregister_copilot_hooks_global,
                target: "copilot-global.json",
                file: "copilot-global.json",
                container: "hooks",
                event: "preToolUse",
                shape: HookEntryShape::Flat("bash"),
            },
            HookRegistration {
                register: register_copilot_hooks,
                unregister: unregister_copilot_hooks,
                target: "copilot-project",
                file: "copilot-project/symposium.json",
                container: "hooks",
                event: "preToolUse",
                shape: HookEntryShape::Flat("bash"),
            },
            HookRegistration {
                register: register_antigravity_hooks,
                unregister: unregister_antigravity_hooks,
                target: "antigravity.json",
                file: "antigravity.json",
                container: "symposium",
                event: "PreToolUse",
                shape: HookEntryShape::Grouped,
            },
            HookRegistration {
                register: register_kiro_hooks,
                unregister: unregister_kiro_hooks,
                target: "kiro",
                file: "kiro/symposium.json",
                container: "hooks",
                event: "preToolUse",
                shape: HookEntryShape::Flat("command"),
            },
        ]
    }

    #[test]
    fn hook_unregistration_rejects_invalid_shapes_and_reports_without_rewriting() {
        let tmp = tempfile::tempdir().unwrap();
        for case in hook_registrations() {
            let file = tmp.path().join(case.file);
            let target = tmp.path().join(case.target);
            (case.register)(&target, &Output::quiet()).unwrap();
            let valid: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
            let mut invalid = vec![
                "null".to_string(),
                "[]".to_string(),
                "{synthetic-secret-canary".to_string(),
            ];
            for value in [json!(null), json!([]), json!("synthetic-secret-canary")] {
                let mut root = valid.clone();
                root[case.container] = value;
                invalid.push(root.to_string());
            }
            for value in [
                json!(null),
                json!({}),
                json!("synthetic-secret-canary"),
                json!([null]),
            ] {
                let mut root = valid.clone();
                root[case.container][case.event] = value;
                invalid.push(root.to_string());
            }
            let mut bad_entry = valid.clone();
            bad_entry[case.container][case.event] = match case.shape {
                HookEntryShape::Grouped => {
                    json!([{ "hooks": [{ "command": ["synthetic-secret-canary"] }] }])
                }
                HookEntryShape::Flat(key) => {
                    let mut entry = json!({});
                    entry[key] = json!(["synthetic-secret-canary"]);
                    json!([entry])
                }
            };
            invalid.push(bad_entry.to_string());
            bad_entry[case.container][case.event] = json!([{ "matcher": null, "hooks": [] }]);
            invalid.push(bad_entry.to_string());
            for original in invalid {
                fs::write(&file, &original).unwrap();
                let output = Output::capturing();
                (case.unregister)(&target, &output);
                assert_eq!(
                    original,
                    fs::read_to_string(&file).unwrap(),
                    "{}",
                    case.file
                );
                let diagnostics = output.captured();
                assert_eq!(diagnostics.len(), 1, "{}", case.file);
                assert!(!diagnostics[0].contains("synthetic-secret-canary"));
                assert!(diagnostics[0].starts_with("⚠️"));
            }
        }
    }

    #[test]
    fn hook_unregistration_preserves_foreign_command_prefix_and_unknown_fields() {
        let tmp = tempfile::tempdir().unwrap();
        for case in hook_registrations() {
            let target = tmp.path().join(case.target);
            (case.register)(&target, &Output::quiet()).unwrap();
            let file = tmp.path().join(case.file);
            let mut original: Value =
                serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
            original["unknown"] = json!({"preserve": true});
            original[case.container]["FutureEvent"] = json!({"preserve": true});
            let foreign = match case.shape {
                HookEntryShape::Grouped => {
                    json!({"type": "command", "command": "cargo-agents hook-custom --keep", "vendor": true})
                }
                HookEntryShape::Flat(key) => {
                    let mut entry = json!({"vendor": true});
                    entry[key] = json!("cargo-agents hook-custom --keep");
                    entry
                }
            };
            match case.shape {
                HookEntryShape::Grouped => {
                    original[case.container][case.event][0]["vendor"] = json!({"keep": true});
                    original[case.container][case.event][0]["hooks"]
                        .as_array_mut()
                        .unwrap()
                        .push(foreign.clone());
                }
                HookEntryShape::Flat(_) => original[case.container][case.event]
                    .as_array_mut()
                    .unwrap()
                    .push(foreign.clone()),
            }
            fs::write(&file, original.to_string()).unwrap();
            (case.unregister)(&target, &Output::quiet());
            let first = fs::read_to_string(&file).unwrap();
            let updated: Value = serde_json::from_str(&first).unwrap();
            assert_eq!(updated["unknown"], original["unknown"]);
            assert_eq!(
                updated[case.container]["FutureEvent"],
                original[case.container]["FutureEvent"]
            );
            match case.shape {
                HookEntryShape::Grouped => {
                    assert_eq!(
                        updated[case.container][case.event][0]["hooks"],
                        json!([foreign])
                    );
                    assert_eq!(
                        updated[case.container][case.event][0]["vendor"],
                        json!({"keep": true})
                    );
                }
                HookEntryShape::Flat(_) => {
                    assert_eq!(updated[case.container][case.event], json!([foreign]))
                }
            }
            (case.unregister)(&target, &Output::quiet());
            assert_eq!(first, fs::read_to_string(&file).unwrap());
        }
    }

    #[test]
    fn unregistration_deletes_only_generated_dedicated_files() {
        let tmp = tempfile::tempdir().unwrap();
        for case in hook_registrations() {
            let target = tmp.path().join(case.target);
            let file = tmp.path().join(case.file);
            (case.register)(&target, &Output::quiet()).unwrap();
            (case.unregister)(&target, &Output::quiet());
            let dedicated = case.target == "kiro" || case.target == "copilot-project";
            assert_eq!(file.exists(), !dedicated, "{}", case.file);
            let output = Output::capturing();
            (case.unregister)(&target, &output);
            assert!(output.captured().is_empty());
        }
    }

    #[test]
    fn hook_registration_rejects_invalid_json_without_rewriting() {
        let tmp = tempfile::tempdir().unwrap();
        for case in hook_registrations() {
            let file = tmp.path().join(case.file);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            let mut invalid = vec![
                "null".to_string(),
                "[]".to_string(),
                "12".to_string(),
                r#""synthetic-secret-canary""#.to_string(),
                "{broken-json".to_string(),
            ];
            for value in [json!(null), json!([]), json!("synthetic-secret-canary")] {
                let mut root = json!({"unrelated": {"keep": true}});
                root[case.container] = value.clone();
                invalid.push(root.to_string());
                if !value.is_array() {
                    root[case.container] = json!({});
                    root[case.container][case.event] = value;
                    invalid.push(root.to_string());
                }
            }
            let mut malformed_entry = json!({});
            malformed_entry[case.container] = json!({});
            malformed_entry[case.container][case.event] = json!([null]);
            invalid.push(malformed_entry.to_string());
            malformed_entry[case.container][case.event] = json!([{ "matcher": 12, "hooks": [] }]);
            invalid.push(malformed_entry.to_string());
            match case.shape {
                HookEntryShape::Grouped => {
                    malformed_entry[case.container][case.event] =
                        json!([{ "hooks": [{ "command": ["invalid"] }] }]);
                    invalid.push(malformed_entry.to_string());
                }
                HookEntryShape::Flat(key) => {
                    let mut entry = json!({});
                    entry[key] = json!(["invalid"]);
                    malformed_entry[case.container][case.event] = json!([entry]);
                    invalid.push(malformed_entry.to_string());
                }
            }
            if let HookEntryShape::Grouped = case.shape {
                for value in [json!(null), json!({}), json!([null])] {
                    malformed_entry[case.container][case.event] = json!([{ "hooks": value }]);
                    invalid.push(malformed_entry.to_string());
                }
            }
            for original in invalid {
                fs::write(&file, &original).unwrap();
                let error =
                    (case.register)(&tmp.path().join(case.target), &Output::quiet()).unwrap_err();
                assert!(
                    matches!(
                        error,
                        AgentConfigError::Json { .. } | AgentConfigError::Shape { .. }
                    ),
                    "{error}"
                );
                assert!(!error.to_string().contains("synthetic-secret-canary"));
                assert_eq!(
                    original,
                    fs::read_to_string(&file).unwrap(),
                    "{}",
                    case.file
                );
            }
        }
    }

    #[test]
    fn hook_registration_preserves_foreign_command_prefix_and_unknown_fields() {
        let tmp = tempfile::tempdir().unwrap();
        for case in hook_registrations() {
            let file = tmp.path().join(case.file);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            let entry = match case.shape {
                HookEntryShape::Grouped => {
                    json!({"matcher": "custom", "hooks": [{"type": "command", "command": "cargo-agents hook-custom --keep", "vendor": true}], "vendor": 1})
                }
                HookEntryShape::Flat(key) => {
                    let mut entry = json!({"vendor": 1});
                    entry[key] = json!("cargo-agents hook-custom --keep");
                    entry
                }
            };
            let mut original = json!({"unknown": {"preserve": [1, 2]}, "version": 1});
            original[case.container] = json!({"FutureEvent": {"vendor": true}});
            original[case.container][case.event] = json!([entry.clone()]);
            fs::write(&file, original.to_string()).unwrap();
            let target = tmp.path().join(case.target);
            (case.register)(&target, &Output::quiet()).unwrap();
            let first = fs::read_to_string(&file).unwrap();
            let result: Value = serde_json::from_str(&first).unwrap();
            assert_eq!(result["unknown"], original["unknown"]);
            assert_eq!(
                result[case.container]["FutureEvent"],
                original[case.container]["FutureEvent"]
            );
            assert_eq!(result[case.container][case.event][0], entry);
            assert_eq!(
                result[case.container][case.event].as_array().unwrap().len(),
                2,
                "foreign command must not suppress registration"
            );
            (case.register)(&target, &Output::quiet()).unwrap();
            assert_eq!(first, fs::read_to_string(&file).unwrap(), "{}", case.file);
        }
    }

    #[test]
    fn hook_registration_accepts_missing_and_whitespace_files() {
        for contents in [None, Some("  \n\t")] {
            let tmp = tempfile::tempdir().unwrap();
            for case in hook_registrations() {
                let file = tmp.path().join(case.file);
                if let Some(contents) = contents {
                    fs::create_dir_all(file.parent().unwrap()).unwrap();
                    fs::write(&file, contents).unwrap();
                }
                (case.register)(&tmp.path().join(case.target), &Output::quiet()).unwrap();
                let result: Value =
                    serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
                assert!(
                    result[case.container][case.event].is_array(),
                    "{}",
                    case.file
                );
            }
        }
    }

    #[test]
    fn json_read_failure_and_parse_failure_preserve_sources() {
        use std::error::Error as _;
        let tmp = tempfile::tempdir().unwrap();
        let read = load_json_or_empty(tmp.path()).unwrap_err();
        assert!(matches!(read, AgentConfigError::Read { .. }));
        assert!(read.source().is_some());
        let file = tmp.path().join("invalid.json");
        fs::write(&file, "{synthetic-secret-canary").unwrap();
        let parse = load_json_or_empty(&file).unwrap_err();
        assert!(matches!(parse, AgentConfigError::Json { .. }));
        assert!(parse.source().is_some());
        assert!(!parse.to_string().contains("synthetic-secret-canary"));
    }

    #[test]
    fn registration_validates_all_events_even_when_one_is_registered() {
        let tmp = tempfile::tempdir().unwrap();
        for case in hook_registrations() {
            let target = tmp.path().join(case.target);
            (case.register)(&target, &Output::quiet()).unwrap();
            let file = tmp.path().join(case.file);
            let mut original: Value =
                serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
            let next_event = if case.container == "symposium" || case.event == "PreToolUse" {
                "PostToolUse"
            } else {
                "postToolUse"
            };
            original[case.container][next_event] = json!(null);
            let bytes = original.to_string();
            fs::write(&file, &bytes).unwrap();
            assert!((case.register)(&target, &Output::quiet()).is_err());
            assert_eq!(bytes, fs::read_to_string(&file).unwrap());
        }
    }

    #[test]
    fn kiro_defaults_and_copilot_version_reject_wrong_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let kiro = tmp.path().join("kiro");
        fs::create_dir_all(&kiro).unwrap();
        for original in [
            r#"{"name":null}"#,
            r#"{"tools":"*"}"#,
            r#"{"resources":{}}"#,
        ] {
            fs::write(kiro.join("symposium.json"), original).unwrap();
            assert!(register_kiro_hooks(&kiro, &Output::quiet()).is_err());
            assert_eq!(
                original,
                fs::read_to_string(kiro.join("symposium.json")).unwrap()
            );
        }
        let copilot = tmp.path().join("copilot");
        fs::create_dir_all(&copilot).unwrap();
        for original in [r#"{"version":2}"#, r#"{"version":"1"}"#] {
            fs::write(copilot.join("symposium.json"), original).unwrap();
            assert!(register_copilot_hooks(&copilot, &Output::quiet()).is_err());
            assert_eq!(
                original,
                fs::read_to_string(copilot.join("symposium.json")).unwrap()
            );
        }
    }

    #[test]
    fn agent_from_config_name() {
        assert_eq!(Agent::from_config_name("claude").unwrap(), Agent::Claude);
        assert_eq!(Agent::from_config_name("codex").unwrap(), Agent::Codex);
        assert_eq!(Agent::from_config_name("copilot").unwrap(), Agent::Copilot);
        assert!(Agent::from_config_name("unknown").is_err());
    }

    /// Each path was confirmed by asking the tool itself, not read off docs: a
    /// wrong one fails silently, since symposium reports success for writing a
    /// file the agent never reads.
    #[test]
    fn mcp_config_paths_match_what_each_agent_reads() {
        let project = Path::new("/project");
        let home = Path::new("/home/user");
        let cases = [
            (
                Agent::Claude,
                "/project/.mcp.json",
                "/home/user/.claude.json",
            ),
            (
                Agent::Antigravity,
                "/project/.agents/mcp_config.json",
                "/home/user/.gemini/config/mcp_config.json",
            ),
            (
                Agent::OpenCode,
                "/project/opencode.json",
                "/home/user/.config/opencode/opencode.json",
            ),
            (
                Agent::Kiro,
                "/project/.kiro/settings/mcp.json",
                "/home/user/.kiro/settings/mcp.json",
            ),
        ];
        for (agent, project_path, user_path) in cases {
            assert_eq!(
                agent.mcp_config_path(McpScope::Project, project, home),
                PathBuf::from(project_path),
                "{agent:?} project scope"
            );
            // Env-relocatable on purpose; mutating env here would race other tests.
            if !relocated_by_env(agent) {
                assert_eq!(
                    agent.mcp_config_path(McpScope::User, project, home),
                    PathBuf::from(user_path),
                    "{agent:?} user scope"
                );
            }
            assert!(agent.supports_project_mcp_scope(), "{agent:?}");
        }
    }

    /// Is this agent's user-scope path redirected by an env var right now?
    fn relocated_by_env(agent: Agent) -> bool {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        match agent {
            Agent::Claude => set("CLAUDE_CONFIG_DIR"),
            Agent::OpenCode | Agent::Goose => set("XDG_CONFIG_HOME"),
            _ => false,
        }
    }

    #[test]
    fn claude_user_path_follows_claude_config_dir() {
        let path = Agent::Claude.mcp_config_path(
            McpScope::User,
            Path::new("/project"),
            Path::new("/home/user"),
        );
        match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => assert_eq!(path, PathBuf::from(dir).join(".claude.json")),
            None => assert_eq!(path, PathBuf::from("/home/user/.claude.json")),
        }
    }

    /// These CLIs read only their user-level file, so project scope resolves
    /// there rather than to a project file they would ignore.
    #[test]
    fn agents_without_project_mcp_scope_fall_back_to_the_user_file() {
        let project = Path::new("/project");
        let home = Path::new("/home/user");
        for (agent, expected) in [
            (Agent::Codex, "/home/user/.codex/config.toml"),
            (Agent::Copilot, "/home/user/.copilot/mcp-config.json"),
            (Agent::Goose, "/home/user/.config/goose/config.yaml"),
        ] {
            assert!(!agent.supports_project_mcp_scope(), "{agent:?}");
            for scope in [McpScope::Project, McpScope::User] {
                let path = agent.mcp_config_path(scope, project, home);
                if !relocated_by_env(agent) {
                    assert_eq!(path, PathBuf::from(expected), "{agent:?} {scope:?}");
                } else {
                    // Still the point of the test: both scopes agree.
                    assert_eq!(
                        path,
                        agent.mcp_config_path(McpScope::User, project, home),
                        "{agent:?} {scope:?}"
                    );
                }
            }
        }
    }

    /// Compared per agent against *its own* hook file: a global "never
    /// `.claude/settings.json`" check would pass while pointing Codex at
    /// `.codex/hooks.json`.
    ///
    #[test]
    fn mcp_config_is_never_the_agents_own_hooks_file() {
        let project = Path::new("/project");
        let home = Path::new("/home/user");
        for &agent in Agent::all() {
            for (scope, root) in [(McpScope::Project, project), (McpScope::User, home)] {
                let mcp = agent.mcp_config_path(scope, project, home);
                for hooks in hook_paths_for(agent, root) {
                    assert_ne!(
                        mcp, hooks,
                        "{agent:?} {scope:?} writes MCP into its hooks file"
                    );
                }
            }
        }
    }

    /// Mirrors the targets [`Agent::register_hooks`] writes.
    fn hook_paths_for(agent: Agent, root: &Path) -> Vec<PathBuf> {
        match agent {
            Agent::Claude => vec![root.join(".claude").join("settings.json")],
            Agent::Codex => vec![root.join(".codex").join("hooks.json")],
            Agent::Copilot => vec![root.join(".github").join("hooks")],
            Agent::Antigravity => vec![
                root.join(".agents").join("hooks.json"),
                root.join(".gemini").join("config").join("hooks.json"),
            ],
            Agent::Kiro => vec![root.join(".kiro").join("agents")],
            // Skills-only agents register no hooks at all.
            Agent::Goose | Agent::OpenCode => vec![],
        }
    }

    #[test]
    fn claude_project_skill_dir() {
        let root = Path::new("/project");
        assert_eq!(
            Agent::Claude.project_skill_dir(root, "tokio"),
            PathBuf::from("/project/.claude/skills/tokio")
        );
    }

    #[test]
    fn copilot_project_skill_dir_uses_vendor_neutral() {
        let root = Path::new("/project");
        assert_eq!(
            Agent::Copilot.project_skill_dir(root, "tokio"),
            PathBuf::from("/project/.agents/skills/tokio")
        );
    }

    #[test]
    fn register_claude_hooks_creates_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        register_claude_hooks(&settings_path, &Output::quiet()).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let hooks = settings.get("hooks").unwrap();

        assert!(hooks.get("PreToolUse").is_some());
        assert!(hooks.get("PostToolUse").is_some());
        assert!(hooks.get("UserPromptSubmit").is_some());
    }

    #[test]
    fn register_claude_hooks_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        register_claude_hooks(&settings_path, &Output::quiet()).unwrap();
        register_claude_hooks(&settings_path, &Output::quiet()).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let pre_tool = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
    }

    #[test]
    fn codex_project_skill_dir_uses_vendor_neutral() {
        let root = Path::new("/project");
        assert_eq!(
            Agent::Codex.project_skill_dir(root, "tokio"),
            PathBuf::from("/project/.agents/skills/tokio")
        );
    }

    #[test]
    fn register_codex_hooks_creates_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
        register_codex_hooks(&hooks_path, &Output::quiet()).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        let hooks = settings.get("hooks").unwrap();

        assert!(hooks.get("PreToolUse").is_some());
        assert!(hooks.get("PostToolUse").is_some());
        assert!(hooks.get("UserPromptSubmit").is_some());
        assert!(hooks.get("SessionStart").is_some());

        // Verify the structure uses empty matcher
        let pre_tool = hooks["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool[0]["matcher"], "");
    }

    #[test]
    fn register_codex_hooks_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_path = tmp.path().join("hooks.json");
        register_codex_hooks(&hooks_path, &Output::quiet()).unwrap();
        register_codex_hooks(&hooks_path, &Output::quiet()).unwrap();

        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        let pre_tool = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
    }

    #[test]
    fn register_copilot_hooks_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks_dir = tmp.path().join("hooks");
        register_copilot_hooks(&hooks_dir, &Output::quiet()).unwrap();

        let hook_file = hooks_dir.join("symposium.json");
        assert!(hook_file.exists());
        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&hook_file).unwrap()).unwrap();
        assert_eq!(content["version"], 1);
        assert!(content["hooks"]["preToolUse"].is_array());
    }

    #[test]
    fn agent_from_config_name_kiro() {
        assert_eq!(Agent::from_config_name("kiro").unwrap(), Agent::Kiro);
    }

    #[test]
    fn kiro_project_skill_dir() {
        let root = Path::new("/project");
        assert_eq!(
            Agent::Kiro.project_skill_dir(root, "tokio"),
            PathBuf::from("/project/.kiro/skills/tokio")
        );
    }

    #[test]
    fn kiro_global_skill_dir() {
        let home = Path::new("/home/user");
        assert_eq!(
            Agent::Kiro.global_skill_dir(home, "tokio"),
            Some(PathBuf::from("/home/user/.kiro/skills/tokio"))
        );
    }

    #[test]
    fn register_kiro_hooks_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        register_kiro_hooks(&agents_dir, &Output::quiet()).unwrap();

        let hook_file = agents_dir.join("symposium.json");
        assert!(hook_file.exists());
        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&hook_file).unwrap()).unwrap();
        assert_eq!(content["name"], "symposium");
        assert!(content["hooks"]["preToolUse"].is_array());
        assert!(content["hooks"]["postToolUse"].is_array());
        assert!(content["hooks"]["userPromptSubmit"].is_array());
        assert!(content["hooks"]["agentSpawn"].is_array());

        // Verify flat format (command directly on entry, no nested hooks array)
        let pre_tool = &content["hooks"]["preToolUse"][0];
        assert_eq!(pre_tool["command"], "cargo-agents hook kiro pre-tool-use");
        assert_eq!(pre_tool["matcher"], "*");
        assert!(pre_tool.get("hooks").is_none());
    }

    #[test]
    fn register_kiro_hooks_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        register_kiro_hooks(&agents_dir, &Output::quiet()).unwrap();
        register_kiro_hooks(&agents_dir, &Output::quiet()).unwrap();

        let hook_file = agents_dir.join("symposium.json");
        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&hook_file).unwrap()).unwrap();
        let pre_tool = content["hooks"]["preToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
    }

    #[test]
    fn unregister_kiro_hooks_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        register_kiro_hooks(&agents_dir, &Output::quiet()).unwrap();

        let hook_file = agents_dir.join("symposium.json");
        assert!(hook_file.exists());

        unregister_kiro_hooks(&agents_dir, &Output::quiet());
        assert!(!hook_file.exists());
    }

    #[test]
    fn agent_from_config_name_opencode() {
        assert_eq!(
            Agent::from_config_name("opencode").unwrap(),
            Agent::OpenCode
        );
    }

    #[test]
    fn opencode_project_skill_dir_uses_vendor_neutral() {
        let root = Path::new("/project");
        assert_eq!(
            Agent::OpenCode.project_skill_dir(root, "tokio"),
            PathBuf::from("/project/.agents/skills/tokio")
        );
    }

    #[test]
    fn opencode_global_skill_dir() {
        let home = Path::new("/home/user");
        assert_eq!(
            Agent::OpenCode.global_skill_dir(home, "tokio"),
            Some(PathBuf::from("/home/user/.agents/skills/tokio"))
        );
    }

    #[test]
    fn agent_from_config_name_goose() {
        assert_eq!(Agent::from_config_name("goose").unwrap(), Agent::Goose);
    }

    #[test]
    fn goose_project_skill_dir_uses_vendor_neutral() {
        let root = Path::new("/project");
        assert_eq!(
            Agent::Goose.project_skill_dir(root, "tokio"),
            PathBuf::from("/project/.agents/skills/tokio")
        );
    }

    #[test]
    fn goose_global_skill_dir() {
        let home = Path::new("/home/user");
        assert_eq!(
            Agent::Goose.global_skill_dir(home, "tokio"),
            Some(PathBuf::from("/home/user/.agents/skills/tokio"))
        );
    }
    // ── Antigravity ──────────────────────────────────────────────────────

    #[test]
    fn antigravity_hooks_use_the_right_shape_per_event() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.json");
        register_antigravity_hooks(&path, &Output::quiet()).unwrap();

        let cfg: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let entry = &cfg["symposium"];

        // Tool events wrap handlers in a matcher group.
        let pre_tool = &entry["PreToolUse"][0];
        assert_eq!(pre_tool["matcher"], "*");
        assert_eq!(
            pre_tool["hooks"][0]["command"],
            "cargo-agents hook antigravity pre-tool-use"
        );

        // Lifecycle events are flat handler lists — no matcher wrapper.
        let pre_invocation = &entry["PreInvocation"][0];
        assert!(pre_invocation.get("matcher").is_none());
        assert_eq!(
            pre_invocation["command"],
            "cargo-agents hook antigravity user-prompt-submit"
        );

        // SessionStart is undocumented but real; it fires with the first turn
        // of a new conversation and never on resume.
        assert_eq!(
            entry["SessionStart"][0]["command"],
            "cargo-agents hook antigravity session-start"
        );
        assert_eq!(
            entry["Stop"][0]["command"],
            "cargo-agents hook antigravity stop"
        );

        // Timeout is in seconds for this agent, not milliseconds.
        assert_eq!(pre_invocation["timeout"], 30);
    }

    #[test]
    fn antigravity_hooks_registration_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.json");
        register_antigravity_hooks(&path, &Output::quiet()).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        register_antigravity_hooks(&path, &Output::quiet()).unwrap();
        assert_eq!(first, fs::read_to_string(&path).unwrap());
    }

    /// The file is shared with whatever else the user registered, so reaping
    /// must take symposium's named entry and nothing else.
    #[test]
    fn antigravity_unregister_leaves_other_named_hooks_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.json");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "someone-elses-linter": { "PostToolUse": [{ "matcher": "*", "hooks": [] }] }
            }))
            .unwrap(),
        )
        .unwrap();

        register_antigravity_hooks(&path, &Output::quiet()).unwrap();
        unregister_antigravity_hooks(&path, &Output::quiet());

        let cfg: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(cfg.get("symposium").is_none(), "symposium entry removed");
        assert!(
            cfg.get("someone-elses-linter").is_some(),
            "unrelated hook preserved"
        );
    }

    /// Antigravity keeps MCP in its own file rather than sharing one with
    /// hooks, and the project and global locations are different shapes — so
    /// the scope dispatch is worth pinning down.
    #[test]
    fn antigravity_mcp_lands_in_its_own_file_at_both_scopes() {
        use sacp::schema::{McpServer, McpServerStdio};
        let servers = vec![McpServer::Stdio(McpServerStdio::new(
            "symposium",
            "/usr/local/bin/cargo-agents",
        ))];

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        Agent::Antigravity
            .register_mcp_servers(McpScope::Project, root, root, &servers, &Output::quiet())
            .unwrap();
        let project = root.join(".agents/mcp_config.json");
        assert!(
            project.is_file(),
            "project MCP goes to .agents/mcp_config.json"
        );
        let cfg: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&project).unwrap()).unwrap();
        assert!(cfg["mcpServers"]["symposium"].is_object());

        Agent::Antigravity
            .register_mcp_servers(McpScope::User, root, root, &servers, &Output::quiet())
            .unwrap();
        assert!(
            root.join(".gemini/config/mcp_config.json").is_file(),
            "global MCP goes to the shared config root"
        );
    }

    #[test]
    fn antigravity_skill_and_config_paths() {
        let root = Path::new("/project");
        let home = Path::new("/home/user");
        assert_eq!(
            Agent::Antigravity.project_skill_dir(root, "tokio"),
            PathBuf::from("/project/.agents/skills/tokio")
        );
        assert_eq!(
            Agent::Antigravity.global_skill_dir(home, "tokio"),
            Some(PathBuf::from("/home/user/.gemini/config/skills/tokio"))
        );
        assert_eq!(
            Agent::from_config_name("antigravity").unwrap(),
            Agent::Antigravity
        );
    }
}
