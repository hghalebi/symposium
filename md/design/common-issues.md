# Common issues

## Known hook implementation gaps

The following issues were identified by auditing our hook implementations against the agent reference docs (`md/design/agent-details/`). They don't cause crashes (the fallback path handles events without agent-specific handlers) but mean some features are incomplete.

### A tool decision has to reach the agent in the form it honors

A deny or rewrite that the translation drops fails open: the tool runs, and since the reason still reaches the model as context, the agent can even report the call as blocked. That was the state of Claude, Codex and Copilot until each `from_symposium` mapped `decision` and `updatedInput`, so check the wire output per agent (`tests/hook_decisions.rs`) whenever a PreToolUse field is added. Two agents need more than a field:

- **Claude and Codex apply `updatedInput` only alongside a `permissionDecision`**, and `"allow"` skips the user's permission prompt. Claude gets `"ask"`; Codex has no `"ask"`, so a rewrite is not applied there.
- **Kiro reads no decision from stdout.** A denial becomes exit 2 in the Kiro handler's `respond`, which is why the pipeline returns a `HookResponse` rather than bytes.

### Copilot also runs Claude Code's hooks

Copilot CLI reads `.claude/settings.json` as well as its own hook files. In a project where both agents are configured, every Copilot event runs `cargo agents hook` twice: as `copilot` from `.github/hooks/symposium.json`, and as `claude` from Claude's settings. Plugin hooks with side effects run twice, and whatever symposium tells the agent once (such as a notice) is taken by whichever invocation runs first.

## Hook stdout belongs to the agent

An agent parses everything `cargo agents hook` writes to stdout. One line ahead of the JSON and Claude Code falls back to treating it all as plain text (which reaches the model only on `SessionStart` and `UserPromptSubmit`), while Copilot drops the output entirely. The in-process test pipeline cannot see this, because the leaks come from around it: the report layer, which the binary installs (and leaves out for hooks), and child processes that inherit stdout (plugin `install_commands` send theirs to stderr). Anything new that prints during a hook, or spawns a child that might, needs the same care; `tests/hook_stdout.rs` runs the real binary to catch it.

## Antigravity footguns

Two Antigravity behaviours fail silently rather than loudly, so they are worth
knowing before debugging a hook that "does nothing".

### `{}` on `PreToolUse` denies the tool call

Antigravity ignores hook exit codes entirely; only stdout decides. On
`PreToolUse`, an object without a valid `decision` — `{}` included, as well as
`{"decision": ""}` — is treated as a **denial**, while writing nothing at all
allows. Symposium's dispatcher would otherwise return `{}` whenever no plugin contributed, which
is the common case, so `AntigravityPreToolUseOutput` keeps `decision` as a plain
always-serialized field defaulting to `allow`. Making it `Option` or adding
`skip_serializing_if` would block every tool call.

### Unknown event names and shapes are accepted and never fire

An unrecognised event key in `hooks.json`, or the wrong structure for a known one
(a flat handler list where a `matcher` group is expected, or vice versa), produces
no error — the hook simply never runs. `ANTIGRAVITY_EVENTS` and
`antigravity_is_tool_event` in `agents/mod.rs` are the single source of truth for
both, and the unit tests assert the shape per event for exactly this reason.

## Symlinks in source trees

`DirEntry::file_type()` reports the *entry*, so a symlink is neither `is_dir()` nor `is_file()` and falls through the gap between the two branches — silently, with no error and no warning. That is how installing a skill used to drop every symlinked file in it: `sync`'s copier had exactly that shape, and so did the scan it compares against. `fs::metadata` follows the link; `DirEntry::metadata` does not. Reach for `dir_walk::resolved_kind` instead of either.

`Path::is_dir()` *does* follow links, which is the other half of the trap: a walk written that way already descends into linked directories, so it can already loop on a link naming a directory it is inside. Guard any such walk with `dir_walk::Ancestors`.

And when two walks over one tree feed each other — a copy, and the comparison that decides whether to copy — they have to be the same traversal. Fixing one and not the other does not show up as a wrong file; it shows up as a sync that reinstalls the same skill forever.

## Plugin-supplied names are untrusted input

Plugin content must not decide where Symposium writes. A name it supplies, such as a skill's `name`, can resolve outside the directory it is joined onto (`Path::join` does not confine it), so it never becomes part of a path unchecked. Validate it with `skills::is_safe_dir_name`, which also covers Windows-specific path forms, rather than with a check of your own.

## A source that reads as empty is not an empty source

A listing that swallows read errors returns the same empty list for "nothing there" and "could not look", and anything that removes what a source stopped offering turns the second into data loss: a `[[registry]]` path on an unmounted share would make `sync` delete every skill installed from it. So `PackageManager::registry_plugins` fails when a registry cannot be read, `PluginRegistry::unreadable_registries` carries that to `sync`, and `sync` skips its stale-skill cleanup while that list is not empty. Any new pass that removes things because a source stopped offering them belongs behind the same condition. The opposite mistake is as bad: a source that is legitimately absent must not count as unreadable, or cleanup never runs again, which is why the built-in `~/.symposium/plugins/` is left out when nothing exists at that path.

## Windows portability (tests)

The test suite runs on `windows-latest`, where CI passes `--no-fail-fast` so that one failing test binary does not hide the failures in every binary cargo would otherwise skip. A few patterns recur when writing tests that touch paths or scripts:

- **Paths in TOML/JSON string literals.** A Windows path like `C:\Users\...` is invalid inside a TOML or JSON string (the backslashes read as escapes). When substituting a real path into fixture text, convert to forward slashes first; Windows accepts `/` in paths. See `setup_fixture` in `symposium-testlib`.
- **Paths inside `sh` script bodies.** On Windows `sh` is git-bash's MSYS shell, which reads `C:\a\b` as escapes plus an illegal `:`. Rewrite to the `/c/a/b` form and quote the value. See `sh_path` in `predicate.rs` tests.
- **`.sh` files must use `script`, not `executable`.** A shell script cannot be spawned directly as a process on Windows (no shebang support). In fixtures, reference it via `script = "..."` so it is run through `sh`, never `executable = "..."`.
- **Canonicalized paths carry a `\\?\` prefix.** `fs::canonicalize` on Windows returns an extended-length path that `cargo`'s output lacks. Canonicalize both sides before comparing.
- **Paths inside asserted messages.** A report event or error that embeds a path renders it with backslashes on Windows, so an assertion matching a `/`-separated substring fails there. Normalize with `replace('\\', "/")` before matching, the way `normalize_paths` does for snapshots.
- **Snapshot tests and home-abbreviated paths.** `display_path` (in `output.rs`) abbreviates `$HOME` to `~/`. On Windows the test temp dir lives under `$HOME`, so printed config paths come out home-relative, not absolute. `normalize_paths` (in `symposium-testlib`) replaces both the absolute and the `~/` form; a snapshot leaking a random `.tmpXXXX/` path means one form was missed. Do not `UPDATE_EXPECT` your way past it: that bakes the volatile temp path into the snapshot and it fails on the next run.

## Capturing report events in tests

A test that asserts on `ReportEvent`s installs a `ReportLayer` with `tracing::subscriber::set_default`, which is thread-local. tracing caches callsite interest and the global max level process-wide, recomputing both from whichever dispatchers are alive the first time a callsite is reached. Tests run in parallel, so a thread with no subscriber can reach a reporting callsite first and pin it to `Interest::never()` for every thread; the capturing test then drains zero events, which looks like the command reporting nothing. The failure is timing-dependent, so it shows up as a platform-specific flake rather than a reproducible failure.

`install_tracing_baseline` (in `symposium-testlib`) installs a permissive global default subscriber once per test process to keep that cache open, and a thread-local `set_default` still takes precedence over it. `setup_fixture` calls it, and so must any helper that installs its own report layer.

## Invalid agent configuration must not become an empty configuration

Agent settings are user-owned data. A JSON root, `hooks`/MCP container, or
supported hook event with the wrong shape is an error, not a missing field.
Registration validates those shapes and preserves the original file bytes on
rejection. Missing and whitespace-only files still initialize normally. Unknown
settings and user hook entries are retained, including in Copilot's project
`symposium.json`; finding one registered event does not skip the other events.
MCP updates preserve unknown extension fields while reconciling managed
transport fields and retaining an existing `enabled: false` setting.

Codex MCP registration likewise rejects malformed TOML and non-table
`mcp_servers` values rather than replacing them with an empty document. Normal
and inline tables are supported. Configuration errors expose the path and
expected shape without embedding source values; parser and filesystem sources
remain attached for structured inspection.

When registration reports a shape error, inspect the indicated JSON pointer (or
TOML table) and repair it using the agent's supported configuration format.
Retry the original registration command, then verify the expected hooks or MCP
servers and unrelated settings. Do not delete the file as a recovery step.
The registration tests cover invalid input, exact byte preservation, extension
fields, and repeated registration; run `cargo test --lib agents::` with real-agent
testing disabled as described in the [test guide](./running-tests.md).

Hook unregistration validates every supported present event before removing
anything and reports rejected input without rewriting it. It removes Symposium
handlers individually, preserving other handlers in the same matcher group,
unknown settings, and unknown events. A dedicated Copilot or Kiro file is deleted
only when it contains generated defaults and no remaining foreign content;
otherwise the edited file remains. This also makes repeated removal a no-op.
