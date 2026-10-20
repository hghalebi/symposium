# `cargo agents hook`

Entry point invoked by the agent's hook system on session events.

## Flow

The CLI parses the hook identity before fallible environment, working-directory,
and logging initialization. A recognized supported `PreToolUse` that cannot
initialize returns the same host-native denial as an execution failure; it does
not rely on exit status alone. Other startup failures retain failure status and
clean stdout. Diagnostics are written after the protocol decision and cannot
panic merely because stderr is closed.

The payload is then parsed, and the agent's parser can declare it not an occurrence of the symposium event (`AgentHookInput::should_dispatch`; false for an Antigravity `PreInvocation` after the first of a turn). Such a payload is answered with the agent's neutral output and skips every step below.

1. **Auto-sync** (if enabled) — when `auto-sync = true` in the user config, runs [`cargo agents sync`](./sync-agent-flow.md) to ensure skills are current. The workspace root is resolved from the payload's `cwd` field; if the payload does not include a working directory, the process's current working directory is used as a fallback. Runs quietly and non-fatally — failures are logged but don't block hook dispatch.

   **`SessionStart` is the refresh point.** Because it fires once per agent session, it does the expensive work that other events skip: it bypasses the `Cargo.lock` freshness gate (so skills re-sync even when the workspace's dependencies are unchanged) and passes `UpdateLevel::Check` so git registries and `source.git` skill groups are re-fetched if their upstream moved. Every other event keeps the cheap, `Cargo.lock`-gated path with `UpdateLevel::None` (debounced) to avoid per-event network and `cargo metadata` cost. The registry refresh on `SessionStart` (`ensure_registries` with `Check`, decided in the binary entry point from the event) still honors each registry's `auto-update` toggle. `SessionStart` also runs `prewarm_hook_sources`, which *refreshes already-installed* hook binaries/scripts (the `cargo`/`github` sources backing plugin hooks) — refresh-only, so it never eagerly installs a tool a hook may never use; first install still happens lazily at dispatch.

2. **Built-in dispatch** — symposium's own handling, before plugin hooks. `PreToolUse` produces nothing. On `SessionStart` three fragments are computed independently and, when present, joined into one `additionalContext`:
   - **Discovery hint** — when the active workspace exposes plugin-vended subcommands (the same workspace-filtered set listed by [`cargo agents --help`](./subcommands.md#help-text-grouping)), a line suggesting the agent run `cargo agents --help` to find them. Computed independently of the update-check throttle, so it fires whenever there is something to discover.
   - **Consent hint**: the dependency plugins awaiting consent, telling the agent to point the user at `cargo agents sync` or `cargo agents use`.
   - **Update nudge** — when `auto-update = "warn"`, the 24-hour check throttle has elapsed, and the registry reports a newer version: a line suggesting `cargo agents self-update`.

   On `PostToolUse` and `UserPromptSubmit`, when auto-sync just re-synced because `Cargo.lock` changed, a generic notice says there may be new plugins and points at `cargo agents sync`. That is right after a tool call that changed the dependencies, or the user's next prompt when they changed them. `run_auto_sync` returns that as a bool, so an event with an unchanged `Cargo.lock` does no extra work. The notice is lost when the agent ignores context on that event (Antigravity after a tool call), or when the change is first seen on `PreToolUse` or `Stop`, which sync but carry no notice.

   Agents without hook registration (OpenCode, Goose) never receive this; for them the only discovery surface is `cargo agents --help` itself.

3. **Dispatch to plugin hooks** — for each enabled plugin that defines a hook handler for the incoming event:
   - **Select format**: for each plugin, pick the best hook to deliver (see [Hooks](./hooks.md) for priority rules). If the plugin has a hook matching the current agent's format, deliver the input unmodified. Otherwise deliver in symposium canonical format (or convert to the declared format if only one non-symposium hook exists).
   - **Acquire and run**:
     - Acquire every declared `requirements` entry before the command. A required
       installation failure stops dispatch and follows the native failure policy;
       refresh-only prewarming remains visible best effort.
     - Resolve the hook's `command` (a named installation reference or inline declaration) into a runnable form:
       - If the installation declares a `source`, acquire it (install / cache / clone) and resolve the `executable` / `script` against the cached location. Dispatch acquires with `UpdateLevel::None` — it serves the cache (git checks debounced) rather than hitting the network on every event. Freshness comes from the `SessionStart` prewarm (step 1), which re-acquires every applicable hook's source with `Check` once per session.
       - If no source, the `executable` / `script` is taken as a path on disk (relative paths resolve against the plugin directory, so a refreshed plugin-source repo updates these for free).
       - Run the installation's `install_commands` (post-source) before invoking the runnable.
       - Spawn `path args…` directly for `Exec`, or `sh path args…` for `Script`.
     - Preserve installation directories and resolved runnable paths as native
       operating-system values in `SYMPOSIUM_DIR_<name>`, `SYMPOSIUM_<name>`, and
       `PATH`, including Unix paths that are not UTF-8. Sanitize names as before.
       The command's directory precedes requirement directories in reverse
       acquisition order, followed by the inherited nonempty `PATH` entries.
       An unrepresentable platform path-list entry returns a typed environment
       error with the original join error; it never silently drops the override.
       These environment values do not change the UTF-8 JSON protocol.
     - Pass the event JSON (in the selected format) on stdin to the plugin's hook.
       Input writing and stdout/stderr collection run concurrently: a plugin may
       produce more than a pipe buffer before reading its input. Close the input
       handle after writing and collect every spawned child even if writing fails.
     - Collect output from each handler. An explicit exit 2 or signal/no exit code
       is a block, taking precedence over a simultaneous stdin write failure.
       A successful hook may intentionally close stdin without consuming it;
       its `BrokenPipe` is diagnosed and its output is still parsed normally.
       Other input-write failures and failed process collection remain typed
       transport failures. A successful write only queues bytes; it cannot prove
       that a child consumed them.
     - Convert output back to the agent's wire format. A recognized canonical
       event envelope must parse and match the selected event; malformed envelopes
       never fall back to the native parser. Legacy untagged host JSON remains
       accepted through the native parser, with unknown fields preserved and its
       canonical denial checked before later hooks run. Kiro's native output is
       plain text, so untagged JSON there is parsed as text context rather than
       trusted as an internal decision object.
     - Merge results (e.g., allow/block decisions, output text) across all handlers. `merge` lets a later value replace an earlier one, so on `SessionStart`, `UserPromptSubmit` and `PostToolUse` a `JoinedContext` keeps every source's context (the builtin fragments first, then each hook) and writes the joined text back after each merge. `PreToolUse` and `Stop` are left out: there some agents keep context in the field that holds the decision.
     - Return the merged result to the agent.
   - **A hook that exits 2 blocks**: dispatch stops, and `blocked_response` turns its stderr into the reason. On `PreToolUse` that is a canonical deny, translated like any other so each agent receives it in the form it honors (Antigravity ignores exit codes, Copilot drops the reason of a failing hook). On other events the process exits 2 with the reason on stderr.

4. **Respond**: the agent's handler renders the final output as a `HookResponse`
   (stdout, stderr, exit code). Most agents read everything from stdout; Kiro,
   whose stdout is plain context, is told to deny a tool through exit 2. Each
   nonempty response stream is written and flushed; both streams are attempted
   even if one fails. A delivery error remains typed, and an existing exit 2 is
   preserved. A failed stdout pipe cannot prove that an agent received a JSON
   denial, regardless of the process exit status.

## Failure boundaries and compatibility

`try_execute_hook` returns a typed `HookError` whose display is safe for normal
diagnostics and whose structured sources remain inspectable. It separates an
explicit plugin block from preparation, parsing, translation and transport
failures. The CLI renders a fixed native denial on a supported `PreToolUse`
failure: Claude, Codex, Copilot and Antigravity receive denial JSON with their
normal success exit; Kiro receives a fixed reason with exit 2. Other event
failures return failure status. Raw parser or I/O source text never becomes the
failure reason sent to the model or persistent diagnostics.

The existing `execute_hook` and byte-error `dispatch_plugin_hooks` signatures
remain root-0.5 compatibility adapters until the root-0.6 transition. Maintained
runtime and test-library callers use the typed path. Valid native payloads,
context ordering, final denials and unknown native extension fields retain their
existing behavior. A plugin's ordinary nonzero exit other than 2 continues to be
a visible best-effort failure. Declared requirements must acquire successfully
before the command runs; optional refresh-only prewarming remains best effort.

Plugin hooks can respond to agent-specific events (e.g., `pre-tool-use`, `post-tool-use`, `user-prompt-submit` for Claude Code). The available events depend on which agent is in use.


## Diagnostic and trace privacy

Hook protocol stdout and stderr carry the agent's response, including intended
context or denial reasons. Persistent hook diagnostics do not copy those streams
or the request payload. The hook dispatcher logs fixed operation messages and
closed metadata such as agent/event/format variants, byte lengths, exit codes,
and I/O error kinds. It does not log raw parser messages, plugin names, hook
names, prompts, tool arguments/results, plugin output/stderr, or denial reasons.
Other subsystems retain their own diagnostic contracts; this is not a blanket
claim about arbitrary identifiers elsewhere in the application.

When `SYMPOSIUM_HOOK_TRACE` names a file, a prepared response after stdin was
read appends one metadata JSONL record with this shape:

```json
{"event":"PreToolUse","agent":"claude","input":{"byte_len":120},"output":{"byte_len":2},"exit_code":0,"stderr_len":0}
```

The numeric lengths describe the UTF-8 request and prepared response bytes.
The trace is written before final stream delivery: its `exit_code` and lengths
are not a receipt proving that bytes reached the agent or the process exited
with that status. Stream failures are reported separately. The
`input` and `output` keys remain objects for trace consumers, but contain only
sizes; raw input/output and the previous raw `stderr` field are not persisted.
There is no raw-payload tracing option. This trace can verify which events
occurred; inspect synthetic in-process responses in tests to assert context or
reason contents. The test library keeps its `HookTrace` value fields compatible
and documents the difference between process metadata and simulation payloads.

Configuration diagnostics similarly avoid full configuration dumps, invalid
log-level values, invalid registry URLs, and parser messages that can include
source text. Malformed TOML still warns and uses defaults. Errors returned by
library APIs retain their sources; persistent diagnostics do not format arbitrary
source errors. At the binary's library-command failure boundary, an
`AgentConfigError` wrapped in `anyhow` contexts is identified by type and rendered
with its own content-free `Display`; its parser source remains available to
callers but is not recursively printed. Other command errors retain their
existing detailed diagnostics. This formatting rule does not claim that every
error type or every subsystem is safe to render recursively.

`tests/hook_diagnostics.rs` runs the real binary with synthetic canaries to verify
protocol delivery and private diagnostics. Its additional Codex MCP registration
failure check runs on Unix with an isolated `HOME` and `SYMPOSIUM_HOME`, proves the
underlying TOML parser would display the canary, and verifies that the CLI does
not print it or rewrite the original file. Windows uses a known-folder API for
home discovery, so that home-mutating process check is Unix-only; isolated
adapter tests still cover configuration validation independently.

The runtime regressions cover malformed and wrong-event permission requests,
legacy native denials followed by a later rewrite, failing and buffering stream
writers, child input/block precedence, and bounded bidirectional pipe pressure.
Startup regressions cover configuration/logging failures for all five registered
hook agents, including a closed diagnostic stream. These tests use synthetic
input and local child processes; they do not qualify live-agent behavior.


Applicable tool matchers use the SDK's checked regex operation. An invalid
pattern is a typed hook failure and follows the existing native permission-denial
policy; its pattern is absent from diagnostics. A foreign agent format is
filtered before evaluating its tool matcher. Optional prewarming, hints and
cache maintenance diagnose failures without claiming successful persistence.
Workspace freshness uses checked input probes: failed reads cannot equal
missing timestamps and suppress a required sync.
